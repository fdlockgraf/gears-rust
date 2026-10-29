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
use bss_orders_sdk::catalog::{OrderState, Reason};
use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

/// Row 11's S4-10 guards, in registration order.
const ACCEPTANCE_RECORDED: &str = "preconditions.acceptance-recorded";
const PAYMENT_AUTHORIZATION: &str = "preconditions.payment-authorization";

/// Row 11's preparation (05 §3.3, D-256). Decision order:
///
/// 0. a snapshot outside `approved` cannot be admitted: nothing external is read and the engine
///    refuses by precedence (both inputs unresolvable should the guards be reached);
/// 1. before the transaction, the live consent requirement through the one resolver (the
///    referenced contract's `acceptance_required` read live, then the seller and platform
///    elections). Unavailable is the unresolvable input `acceptance-requirement-unevaluable`
///    (503, settled), never a default; nothing else is read then;
/// 2. before the transaction, and only for a `failed` outcome, the tolerance election (seller,
///    then platform, unset `false`): the input [`InputPort::AuthorizationTolerance`] of the
///    payment guard. A failed read makes it unresolvable with `acceptance-requirement-unevaluable`
///    (05 §3.3 `inst-bg-resolve-requirement`), never "not tolerated" and never "tolerated"; being
///    the second guard's input, it answers only after the consent guard (D-113);
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

/// Both inputs unresolvable and both predicates bound fail-closed (consent as required, payment
/// as undecided): evaluation stops at the consent guard's input (D-113), so neither predicate is
/// reached and nothing is admitted on unread facts.
fn unread() -> Prepared {
    let unevaluable = InputState::Unresolvable(Reason::AcceptanceRequirementUnevaluable);
    let mut prepared = Prepared::new(AggregateContribution::BeginFulfillment { tolerated: false });
    prepared.inputs = Inputs::new()
        .with(InputPort::AcceptanceRequirement, unevaluable)
        .with(InputPort::AuthorizationTolerance, unevaluable);
    prepared.guards = GuardBindings::new()
        .bind(
            ACCEPTANCE_RECORDED,
            consent_guard(Requirement::Required(RequirementSource::PlatformDefault)),
        )
        .bind(PAYMENT_AUTHORIZATION, undecided);
    prepared
}

/// The payment predicate when the tolerance input is unresolvable: never reached (D-113), and
/// bound as "undecided" so it can never read as failed or tolerated.
fn undecided(_: &GuardSubject<'_>) -> GuardVerdict {
    GuardVerdict::Fail(Reason::AuthorizationPending)
}

#[async_trait]
impl Preparation for BeginFulfillmentPreparation<'_> {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let mut prepared = self.decide(view).await?;
        // The presented outcome is reported with the decision, whatever it is (D-257).
        prepared.authorization = Some(self.outcome);
        Ok(prepared)
    }
}

impl BeginFulfillmentPreparation<'_> {
    /// Steps 0-4 (type doc).
    async fn decide(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let facts = &view.facts;
        // 0. Not admissible from this snapshot (row 11 starts only at `approved`): the engine
        //    refuses by precedence before any guard, so nothing external is read. Should the
        //    guards be reached after all (the order became `approved` between the snapshot and
        //    the lock) the inputs are unresolvable, never admitted on unread facts. Mirrors
        //    record-acceptance (`infra/acceptance.rs` `RecordPreparation` step 1).
        if facts.state != OrderState::Approved {
            return Ok(unread());
        }
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
            Err(PrepareError::Unavailable) => return Ok(unread()),
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
        let mut prepared = Prepared::new(AggregateContribution::BeginFulfillment {
            tolerated: decision == PaymentDecision::AdmitTolerated,
        });
        let (tolerance, payment) = match decision {
            PaymentDecision::Admit | PaymentDecision::AdmitTolerated => {
                (InputState::Resolved, GuardVerdict::Pass)
            }
            PaymentDecision::Refuse(reason) => (InputState::Resolved, GuardVerdict::Fail(reason)),
            PaymentDecision::Unavailable => {
                tracing::warn!(
                    target: "orders.acceptance",
                    order_id = %facts.order_id,
                    "bss-orders: the authorization-failure tolerance could not be read; \
                     begin-fulfillment is unevaluable after the consent guard"
                );
                (
                    InputState::Unresolvable(Reason::AcceptanceRequirementUnevaluable),
                    GuardVerdict::Fail(Reason::AuthorizationPending),
                )
            }
        };
        // 3-4. The guards, under the lock, in registration order: consent, then payment.
        prepared.inputs = Inputs::new()
            .with(InputPort::AcceptanceRequirement, InputState::Resolved)
            .with(InputPort::AuthorizationTolerance, tolerance);
        prepared.guards = GuardBindings::new()
            .bind(ACCEPTANCE_RECORDED, consent_guard(requirement))
            .bind(PAYMENT_AUTHORIZATION, move |_: &GuardSubject<'_>| payment);
        Ok(prepared)
    }
}
