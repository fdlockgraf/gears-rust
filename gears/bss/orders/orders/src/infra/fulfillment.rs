//! The begin-fulfillment consent and payment guard (S4-10; 05 §3.3; Stage 4 DESIGN §6; D-256):
//! row 11, `approved → in_fulfillment`, trigger `begin-fulfillment`.
//!
//! The public operation is a Workflow seam that Stage 5 delivers; until then [`begin_fulfillment`]
//! is its driver and only the engine tests call it. The driver runs the preparation through
//! [`Engine::transition`] with the read-to-lock retry (D-253 item 10, D-256 item 9). It binds row
//! 11's two S4-10 guards; the third, `gate.binding-deadline`, is S5-04's, and its owner hands the
//! driver the predicate (a labelled double in the tests).
use std::sync::Arc;

use crate::authz::Caller;
use crate::domain::contributions::AggregateContribution;
use crate::domain::guards::InputPort;
use crate::domain::preconditions::{
    PaymentDecision, Requirement, RequirementSource, consent_missing, payment_decision,
    reads_tolerance,
};
use crate::domain::transition::{
    GuardBindings, GuardPredicate, GuardSubject, GuardVerdict, InputState, Inputs,
};
use crate::infra::acceptance::{RequirementResolver, RequirementSubject};
use crate::infra::engine::{
    Engine, EngineError, EngineOutcome, Preparation, PreparationView, PrepareError, Prepared,
    Reached, TransitionRequest,
};
use crate::infra::gate::budget::RunBudget;
use async_trait::async_trait;
use bss_orders_sdk::catalog::{OrderState, Reason};
use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

/// Row 11's S4-10 guards, in registration order.
const ACCEPTANCE_RECORDED: &str = "preconditions.acceptance-recorded";
const PAYMENT_AUTHORIZATION: &str = "preconditions.payment-authorization";
/// Row 11's third guard, S5-04's: its owner hands [`begin_fulfillment`] the predicate.
const BINDING_DEADLINE: &str = "gate.binding-deadline";

/// S5-04's `gate.binding-deadline` predicate, bound by its owner.
pub type Deadline = Arc<dyn GuardPredicate>;

/// Row 11 through the engine: the Stage 5 seam's driver (D-256 item 9). One attempt over
/// [`BeginFulfillmentPreparation`] and, only when the engine reached the guards of a preparation
/// whose snapshot it could not admit (the order became `approved` between the snapshot and the
/// lock), one more from a fresh snapshot (D-253 item 10). A second race answers what the engine
/// answered (`unavailable`); nothing was written by either aborted attempt.
///
/// # Errors
/// As [`Engine::transition`].
pub async fn begin_fulfillment(
    engine: &Engine,
    caller: &Caller,
    request: &TransitionRequest,
    resolver: &RequirementResolver,
    outcome: AuthorizationOutcome,
    deadline: &Deadline,
    cancel: &CancellationToken,
) -> Result<EngineOutcome, EngineError> {
    let attempt = |reached: Reached| {
        let preparation = BeginFulfillmentPreparation {
            resolver,
            ctx: caller.ctx(),
            outcome: outcome.outcome,
            deadline: Arc::clone(deadline),
            reached,
            cancel,
        };
        async move {
            engine
                .transition(caller, request.clone(), &preparation)
                .await
        }
    };
    let reached = Reached::new();
    match attempt(reached.clone()).await {
        Err(_) if reached.reached() => {
            tracing::debug!(
                target: "orders.acceptance",
                order_id = %request.order_id,
                "bss-orders: the order became approved between the snapshot and the lock; \
                 deciding again"
            );
            attempt(Reached::new()).await
        }
        answer => answer,
    }
}

/// Row 11's preparation (05 §3.3, D-256). Decision order:
///
/// 0. a snapshot outside `approved` cannot be admitted: nothing external is read and the engine
///    refuses by precedence. Should the guards be reached after all, they are the [`Reached`]
///    witness's and the preparation is [`Prepared::unread`]: the engine aborts unsettled and
///    [`begin_fulfillment`] runs once more (D-253 item 10, D-256 item 9);
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
struct BeginFulfillmentPreparation<'a> {
    resolver: &'a RequirementResolver,
    ctx: &'a SecurityContext,
    outcome: Authorization,
    /// S5-04's predicate, bound as row 11's third guard.
    deadline: Deadline,
    reached: Reached,
    cancel: &'a CancellationToken,
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

/// The requirement could not be read: both inputs unresolvable and both predicates bound
/// fail-closed (consent as required, payment as undecided). Evaluation stops at the consent
/// guard's input (D-113), so neither predicate is reached and the engine settles
/// `acceptance-requirement-unevaluable`.
fn unevaluable() -> Prepared {
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
        let deadline = Arc::clone(&self.deadline);
        prepared.guards = std::mem::take(&mut prepared.guards)
            .bind(BINDING_DEADLINE, move |subject: &GuardSubject<'_>| {
                deadline.evaluate(subject)
            });
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
        //    the lock), the engine aborts unsettled and the driver runs once more. Mirrors
        //    record-acceptance (`infra/acceptance.rs` `RecordPreparation` step 1).
        if facts.state != OrderState::Approved {
            let mut prepared =
                Prepared::new(AggregateContribution::BeginFulfillment { tolerated: false });
            prepared.unread = true;
            prepared.inputs = Inputs::new()
                .with(InputPort::AcceptanceRequirement, InputState::Resolved)
                .with(InputPort::AuthorizationTolerance, InputState::Resolved);
            prepared.guards = self
                .reached
                .passing([ACCEPTANCE_RECORDED, PAYMENT_AUTHORIZATION]);
            return Ok(prepared);
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
            Err(PrepareError::Unavailable) => return Ok(unevaluable()),
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
