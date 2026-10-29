//! The begin-fulfillment consent and payment guard (S4-10; 05 §3.3; Stage 4 DESIGN §6; D-256):
//! row 11, `approved → in_fulfillment`, trigger `begin-fulfillment`.
//!
//! The public operation is a Workflow seam that Stage 5 delivers; until then this preparation is
//! driven through [`Engine::transition`](crate::infra::engine::Engine::transition) by the
//! engine tests only. It binds row 11's two S4-10 guards; the third, `gate.binding-deadline`, is
//! S5-04's and is bound by its owner (a labelled double in the tests).
use crate::domain::contributions::AggregateContribution;
use crate::domain::guards::InputPort;
use crate::domain::preconditions::{
    PaymentDecision, Requirement, RequirementSource, consent_missing, payment_decision,
    reads_tolerance,
};
use crate::domain::transition::{GuardBindings, GuardSubject, GuardVerdict, InputState, Inputs};
use crate::infra::acceptance::{RequirementResolver, RequirementSubject};
use crate::infra::engine::{Preparation, PreparationView, PrepareError, Prepared};
use crate::infra::gate::budget::RunBudget;
use async_trait::async_trait;
use bss_orders_sdk::catalog::Reason;
use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

/// Row 11's S4-10 guards, in registration order.
const ACCEPTANCE_RECORDED: &str = "preconditions.acceptance-recorded";
const PAYMENT_AUTHORIZATION: &str = "preconditions.payment-authorization";

/// Row 11's preparation (05 §3.3, D-256). Decision order:
///
/// 1. before the transaction, the live consent requirement through the one resolver (the
///    referenced contract's `acceptance_required` read live, then the seller and platform
///    elections). Unavailable is the unresolvable input `acceptance-requirement-unevaluable`
///    (503, settled), never a default; nothing else is read then;
/// 2. before the transaction, and only for a `failed` outcome, the tolerance election (seller,
///    then platform, unset `false`). A failed read is `unavailable` (the attempt aborts before
///    any transaction and settles nothing), never "not tolerated" and never "tolerated";
/// 3. under the aggregate lock, `preconditions.acceptance-recorded`: required and no consent on
///    the locked current version ([`GuardSubject::acceptance_recorded`], read by the engine
///    under the lock) refuses `acceptance-required-not-recorded`, before any payment branch. Any
///    consent counts, `volunteered` included;
/// 4. `preconditions.payment-authorization`: `authorized` passes; `pending` refuses
///    `authorization-pending`; `failed` passes when tolerated and refuses `authorization-failed`
///    otherwise.
///
/// Only a tolerated failure contributes `BeginFulfillment { tolerated: true }`; the engine then
/// writes `authorization_failure_tolerated_at = t` once, never clearing it. No payment column,
/// token or state is stored.
pub struct BeginFulfillmentPreparation<'a> {
    resolver: &'a RequirementResolver,
    ctx: &'a SecurityContext,
    outcome: Authorization,
    cancel: &'a CancellationToken,
}

impl<'a> BeginFulfillmentPreparation<'a> {
    /// A preparation for one begin-fulfillment attempt carrying Workflow's `outcome`.
    pub fn new(
        resolver: &'a RequirementResolver,
        ctx: &'a SecurityContext,
        outcome: AuthorizationOutcome,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            resolver,
            ctx,
            outcome: outcome.outcome,
            cancel,
        }
    }
}

/// The consent guard over the resolved requirement and the engine's locked consent fact.
fn consent_guard(requirement: Requirement) -> impl Fn(&GuardSubject<'_>) -> GuardVerdict {
    move |subject: &GuardSubject<'_>| {
        if consent_missing(requirement, subject.acceptance_recorded) {
            GuardVerdict::Fail(Reason::AcceptanceRequiredNotRecorded)
        } else {
            GuardVerdict::Pass
        }
    }
}

#[async_trait]
impl Preparation for BeginFulfillmentPreparation<'_> {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let facts = &view.facts;
        let subject = RequirementSubject {
            contract_id: facts.contract_id,
            payer_tenant_id: facts.payer_tenant_id,
            seller_tenant_id: facts.seller_tenant_id,
        };
        // 1. The live requirement.
        let requirement = match self
            .resolver
            .requirement(self.ctx, &subject, None, &RunBudget::new(), self.cancel)
            .await
        {
            Ok(requirement) => requirement,
            Err(PrepareError::Unavailable) => {
                // The consent guard's input is unresolvable, so evaluation stops there (D-113)
                // and neither predicate below is reached; the tolerance is not read. Both are
                // bound fail-closed: the consent as required, the payment as undecided.
                let mut prepared =
                    Prepared::new(AggregateContribution::BeginFulfillment { tolerated: false });
                prepared.inputs = Inputs::new().with(
                    InputPort::AcceptanceRequirement,
                    InputState::Unresolvable(Reason::AcceptanceRequirementUnevaluable),
                );
                prepared.guards = GuardBindings::new()
                    .bind(
                        ACCEPTANCE_RECORDED,
                        consent_guard(Requirement::Required(RequirementSource::PlatformDefault)),
                    )
                    .bind(PAYMENT_AUTHORIZATION, |_: &GuardSubject<'_>| {
                        GuardVerdict::Fail(Reason::AuthorizationPending)
                    });
                return Ok(prepared);
            }
            Err(PrepareError::Integration) => return Err(PrepareError::Integration),
        };
        // 2. The tolerance election, only for a conclusive failure.
        let tolerate = if reads_tolerance(self.outcome) {
            match self.resolver.tolerate_failure(facts.seller_tenant_id).await {
                Ok(tolerate) => Some(tolerate),
                Err(PrepareError::Unavailable) => None,
                Err(PrepareError::Integration) => return Err(PrepareError::Integration),
            }
        } else {
            None
        };
        let decision = payment_decision(self.outcome, tolerate);
        let payment = match decision {
            PaymentDecision::Admit | PaymentDecision::AdmitTolerated => GuardVerdict::Pass,
            PaymentDecision::Refuse(reason) => GuardVerdict::Fail(reason),
            PaymentDecision::Unavailable => {
                tracing::warn!(
                    target: "orders.acceptance",
                    order_id = %view.facts.order_id,
                    "bss-orders: the authorization-failure tolerance could not be read; \
                     begin-fulfillment is unavailable"
                );
                return Err(PrepareError::Unavailable);
            }
        };
        // 3-4. The guards, under the lock.
        let mut prepared = Prepared::new(AggregateContribution::BeginFulfillment {
            tolerated: decision == PaymentDecision::AdmitTolerated,
        });
        prepared.inputs =
            Inputs::new().with(InputPort::AcceptanceRequirement, InputState::Resolved);
        prepared.guards = GuardBindings::new()
            .bind(ACCEPTANCE_RECORDED, consent_guard(requirement))
            .bind(PAYMENT_AUTHORIZATION, move |_: &GuardSubject<'_>| payment);
        Ok(prepared)
    }
}
