//! The consent requirement, resolved once (S4-08, D-251): submit, recording an acceptance and
//! begin-fulfillment all ask this resolver, so the precedence cannot drift between them. The
//! record-acceptance service ([`AcceptanceService`], S4-08b, D-255) lives here too.
//!
//! Requirement: the contract the order references, then the seller's `acceptance_required`
//! election, then the platform's, then unset = required. Tolerance of a failed authorization
//! (S4-10): the seller's election, then the platform's, then unset = false. An election is read
//! only when the earlier term is silent. A missing or failed read is `Unavailable`, never a
//! default.
use crate::authz::Caller;
use crate::domain::contributions::AggregateContribution;
use crate::domain::gate::answers::ContractQuery;
use crate::domain::gate::ports::PortAnswer;
use crate::domain::guards::InputPort;
use crate::domain::preconditions::{
    Requirement, RequirementSource, RoleActor, RoleVersions, acceptance_requirement,
    recorded_source, recording_party_barred, tolerate_failure,
};
use crate::domain::state_table::StateTable;
use crate::domain::transition::{GuardBindings, GuardSubject, GuardVerdict, InputState, Inputs};
use crate::infra::capture::{Settled, fingerprint_document};
use crate::infra::engine::documents::EventEnqueue;
use crate::infra::engine::signals::ConsentObservation;
use crate::infra::engine::{
    ChildDocuments, DocumentWriter, Engine, EventSpec, Preparation, PreparationView, PrepareError,
    Prepared, Reached, TransitionRequest,
};
use crate::infra::events::EventEnqueueError;
use crate::infra::events::payload::OrderAcceptanceRecorded;
use crate::infra::gate::budget::RunBudget;
use crate::infra::gate::ports::ContractReads;
use crate::infra::gate::unavailable::ContractsUnavailable;
use crate::infra::policy::{self, Election, PolicyScope};
use crate::infra::storage::entity::{acceptance, order_version};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::acceptance::Acceptance;
use bss_orders_sdk::authoring::SalesPath;
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::models::CallMeta;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use toolkit_db::Db;
use toolkit_db::secure::ScopeError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The parties and contract of the order whose requirement is resolved.
#[expect(
    clippy::struct_field_names,
    reason = "the names are the Contracts port's `ContractQuery` fields"
)]
#[derive(Debug, Clone, Copy)]
pub struct RequirementSubject {
    /// The contract the order references, if any.
    pub contract_id: Option<Uuid>,
    /// The paying tenant.
    pub payer_tenant_id: Uuid,
    /// The selling tenant.
    pub seller_tenant_id: Uuid,
}

/// Resolves the consent requirement and the authorization-failure tolerance.
pub struct RequirementResolver {
    contracts: Arc<dyn ContractReads>,
    policy: Option<Db>,
}

impl RequirementResolver {
    /// A resolver over the Contracts port and the policy election store; `None` answers
    /// unavailable whenever an election is read.
    #[must_use]
    pub fn new(contracts: Arc<dyn ContractReads>, policy: Option<Db>) -> Self {
        Self { contracts, policy }
    }

    /// A resolver for a caller that always holds the contract fact (submit's frozen one):
    /// the Contracts port is the fail-closed adapter and is never reached.
    #[must_use]
    pub fn from_fact(policy: Option<Db>) -> Self {
        Self::new(Arc::new(ContractsUnavailable), policy)
    }

    /// The live consent requirement. `contract` is `Some(fact)` when the caller already holds
    /// the contract's term (submit's frozen fact); `None` reads it live, and only when the order
    /// references a contract.
    ///
    /// # Errors
    /// `Unavailable`: the contract answer is missing, unavailable or lacks the contract, or an
    /// election read failed or has no store.
    #[expect(
        clippy::option_option,
        reason = "the Task 10 contract: `None` reads live, `Some(None)` is a silent held fact"
    )]
    pub async fn requirement(
        &self,
        ctx: &SecurityContext,
        order: &RequirementSubject,
        contract: Option<Option<bool>>,
        run: &RunBudget,
        cancel: &CancellationToken,
    ) -> Result<Requirement, PrepareError> {
        let fact = match contract {
            Some(fact) => fact,
            None => self.contract_term(ctx, order, run, cancel).await?,
        };
        self.requirement_for(order.seller_tenant_id, fact).await
    }

    /// The requirement given the contract's term already in hand.
    ///
    /// # Errors
    /// `Unavailable`: an election read failed or has no store.
    pub async fn requirement_for(
        &self,
        seller: Uuid,
        contract: Option<bool>,
    ) -> Result<Requirement, PrepareError> {
        if contract.is_some() {
            return Ok(acceptance_requirement(contract, None, None));
        }
        let (seller, platform) = self.elections(Election::AcceptanceRequired, seller).await?;
        Ok(acceptance_requirement(None, seller, platform))
    }

    /// Whether a failed payment authorization is tolerated: seller, then platform, unset false.
    ///
    /// # Errors
    /// `Unavailable`: an election read failed or has no store.
    pub async fn tolerate_failure(&self, seller: Uuid) -> Result<bool, PrepareError> {
        let (seller, platform) = self
            .elections(Election::TolerateAuthorizationFailure, seller)
            .await?;
        Ok(tolerate_failure(seller, platform))
    }

    async fn contract_term(
        &self,
        ctx: &SecurityContext,
        order: &RequirementSubject,
        run: &RunBudget,
        cancel: &CancellationToken,
    ) -> Result<Option<bool>, PrepareError> {
        let Some(contract_id) = order.contract_id else {
            return Ok(None);
        };
        let query = ContractQuery {
            contract_id,
            payer_tenant_id: order.payer_tenant_id,
            seller_tenant_id: order.seller_tenant_id,
        };
        match self.contracts.resolve(ctx, &[query], run, cancel).await {
            PortAnswer::Answered(facts) => facts
                .get(&contract_id)
                .map(|facts| facts.acceptance_required)
                .ok_or(PrepareError::Unavailable),
            PortAnswer::Skipped(_) | PortAnswer::Unavailable(_) => Err(PrepareError::Unavailable),
        }
    }

    /// The seller's election, and the platform's only when the seller's is silent.
    async fn elections(
        &self,
        election: Election,
        seller: Uuid,
    ) -> Result<(Option<bool>, Option<bool>), PrepareError> {
        let seller = self.election(election, PolicyScope::Seller(seller)).await?;
        let platform = match seller {
            Some(_) => None,
            None => self.election(election, PolicyScope::Platform).await?,
        };
        Ok((seller, platform))
    }

    /// One election; a failed read is `Unavailable` (never "no row").
    async fn election(
        &self,
        election: Election,
        scope: PolicyScope,
    ) -> Result<Option<bool>, PrepareError> {
        let Some(db) = &self.policy else {
            tracing::warn!(
                target: "orders.acceptance",
                "bss-orders: no policy election store is bound; the requirement is unavailable"
            );
            return Err(PrepareError::Unavailable);
        };
        policy::election(db, election, scope).await.map_err(|error| {
            tracing::warn!(
                target: "orders.acceptance",
                election = election.token(),
                scope = scope.token(),
                error = %error,
                "bss-orders: a policy election could not be read; the requirement is unavailable"
            );
            PrepareError::Unavailable
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Recording the customer's consent after submit (S4-08b; 05 *Record Acceptance*; D-255)

/// The public catalog operation a recording fingerprints under.
const OPERATION: &str = "record_acceptance";
/// Row 25's guards, in registration order.
const NOT_RECORDED: &str = "preconditions.acceptance-not-recorded";
const RECORDING_PARTY: &str = "preconditions.recording-party";

/// The record-acceptance service: the one service REST and the local SDK share.
///
/// [`Self::record`] builds the row-25 [`TransitionRequest`] from the call alone (operation
/// `record_acceptance`, the caller's `If-Match` as the expected version, the empty body as the
/// fingerprint document) and hands the engine a [`RecordPreparation`]. The engine authorizes
/// `acceptance × record` on the target (resource-tenant membership is the PDP's), replays a
/// settled key, takes the authorized snapshot with the
/// [`RecordingFacts`](crate::infra::engine::RecordingFacts) and the trusted actor, then decides
/// state, version and the two guards under the aggregate lock. When the snapshot could not be
/// admitted but the locked order is (the read-to-lock race), the engine aborts unsettled and the
/// service runs once more from a fresh snapshot, once (D-253 item 10, D-255 item 9).
pub struct AcceptanceService {
    engine: Arc<ArcSwapOption<Engine>>,
    resolver: RequirementResolver,
}

impl AcceptanceService {
    /// A service over the engine slot and the consent-requirement resolver (the gate's
    /// contract port for the live contract read, the policy store for the elections).
    #[must_use]
    pub fn new(engine: Arc<ArcSwapOption<Engine>>, resolver: RequirementResolver) -> Self {
        Self { engine, resolver }
    }

    /// Record the caller's consent to the committed version `meta.expected_version` of
    /// `order_id`. The boundary has already refused a body with any member and a missing
    /// `If-Match`.
    ///
    /// # Errors
    /// The engine's unsettled refusals (denial, `precondition-failed` race evidence,
    /// `idempotency-mismatch`, `still-processing`); `Unavailable` when the engine is not bound;
    /// `Integration` for a broken contract. A business refusal is settled and returned as `Ok`.
    pub async fn record(
        &self,
        caller: &Caller,
        order_id: Uuid,
        request: Acceptance,
        meta: &CallMeta,
        cancel: &CancellationToken,
    ) -> Result<Settled, OrdersError> {
        let engine = self.engine.load_full().ok_or(OrdersError::Unavailable)?;
        // One pass, and a second one only when the engine reached the guards of a preparation
        // whose snapshot it could not admit; a second race answers what the engine answered.
        let reached = Reached::new();
        match self
            .pass(&engine, caller, order_id, &request, meta, cancel, &reached)
            .await
        {
            Err(_) if reached.reached() => {
                tracing::debug!(
                    target: "orders.acceptance",
                    order_id = %order_id,
                    "bss-orders: the order became admissible between the snapshot and the lock; \
                     deciding again"
                );
                self.pass(
                    &engine,
                    caller,
                    order_id,
                    &request,
                    meta,
                    cancel,
                    &Reached::new(),
                )
                .await
            }
            answer => answer,
        }
    }

    /// One engine attempt of row 25.
    #[expect(
        clippy::too_many_arguments,
        reason = "one attempt reads the engine, the caller, the target, the body, the call \
                  metadata, the cancellation and the race witness"
    )]
    async fn pass(
        &self,
        engine: &Engine,
        caller: &Caller,
        order_id: Uuid,
        request: &Acceptance,
        meta: &CallMeta,
        cancel: &CancellationToken,
        reached: &Reached,
    ) -> Result<Settled, OrdersError> {
        let transition = TransitionRequest {
            order_id,
            trigger: Trigger::RecordAcceptance,
            operation: OPERATION,
            expected_version: meta.expected_version,
            expected_draft_revision: None,
            document: fingerprint_document(request)?,
            proposed: None,
            caller_reason: None,
            idempotency_key: meta.idempotency_key.clone(),
            correlation_id: meta.correlation_id,
            execution: None,
        };
        let preparation = RecordPreparation {
            resolver: &self.resolver,
            table: &engine.registries().table,
            ctx: caller.ctx(),
            expected_version: i32::from(meta.expected_version),
            cancel,
            reached,
        };
        let outcome = engine.transition(caller, transition, &preparation).await?;
        Ok(Settled::from_outcome(outcome))
    }
}

/// Row 25's preparation (05 *Record Acceptance* steps 2-6, D-255), from the engine's authorized
/// snapshot:
///
/// 1. admissibility of the snapshot: a state outside row 25 or a current version other than the
///    expected one reads nothing external, and the engine refuses by precedence before any guard
///    (`not-admissible`, `precondition-failed`). Should the guards be reached after all (the
///    order became admissible between the snapshot and the lock), they are the [`Reached`]
///    witness's and the preparation is [`Prepared::unread`]: the engine aborts unsettled, never
///    records on unread facts, and the service runs once more (D-253 item 10, D-255 item 9).
///    Then the role versions of the expected version ([`role_versions`]): version 1 (`create`),
///    the actual submit version and, when the expected version was appended by an amendment,
///    that version. An admissible snapshot without them is a broken store contract
///    (integration), never a settled refusal;
/// 2. the recording-party bar over the trusted actor's reference
///    ([`AuditActor::reference`](crate::domain::audit::AuditActor::reference), the spelling of
///    `order_version.actor`) and the stored sales path. A barred caller records
///    nothing, so its requirement is not read: the bar is decided from stored facts and is never
///    masked by an outage (D-113; the guard registry declares the input on this guard for that
///    order);
/// 3. otherwise the live requirement ([`RequirementResolver::requirement`], the live contract
///    for a contract-bound order, then the elections); unavailable is the unresolvable input
///    `acceptance-requirement-unevaluable` (503), never a default. Not required records
///    `volunteered`;
/// 4. the guards: already-recorded over the engine's locked read, then the bar; the documents
///    (one consent row at the locked current version, the engine's transition time, the trusted
///    actor, the stored sales path and the source) and exactly one `OrderAcceptanceRecorded`.
///    The row is state-only: no version, no state change.
struct RecordPreparation<'a> {
    resolver: &'a RequirementResolver,
    table: &'a StateTable,
    ctx: &'a SecurityContext,
    expected_version: i32,
    cancel: &'a CancellationToken,
    reached: &'a Reached,
}

/// The role versions of `expected` among `versions` (05 §3.2 step 1); `None` when version 1,
/// the submit version or the expected version itself is not among them.
fn role_versions(versions: &[order_version::Model], expected: i32) -> Option<RoleVersions> {
    let actor = |row: &order_version::Model| RoleActor {
        actor: row.actor.clone(),
        actor_tenant_id: row.actor_tenant_id,
    };
    let creator = versions
        .iter()
        .find(|v| v.version == 1 && v.reason == Trigger::Create.token())?;
    let submitter = versions
        .iter()
        .find(|v| v.reason == Trigger::Submit.token())?;
    let current = versions.iter().find(|v| v.version == expected)?;
    Some(RoleVersions {
        creator: actor(creator),
        submitter: actor(submitter),
        amender: (current.reason == Trigger::Amendment.token()).then(|| actor(current)),
    })
}

#[async_trait]
impl Preparation for RecordPreparation<'_> {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let recording = view.recording.as_ref().ok_or(PrepareError::Integration)?;
        let facts = &view.facts;
        let mut prepared = Prepared::new(AggregateContribution::None);
        let admissible = facts.current_version == self.expected_version
            && self
                .table
                .lookup(facts.state, Trigger::RecordAcceptance)
                .is_some();
        // 1. Not admissible from this snapshot: the engine refuses by precedence.
        if !admissible {
            prepared.unread = true;
            prepared.inputs =
                Inputs::new().with(InputPort::AcceptanceRequirement, InputState::Resolved);
            prepared.guards = self.reached.passing([NOT_RECORDED, RECORDING_PARTY]);
            return Ok(prepared);
        }
        let Some(roles) = role_versions(&recording.versions, self.expected_version) else {
            tracing::error!(
                target: "orders.acceptance",
                order_id = %facts.order_id,
                version = self.expected_version,
                "bss-orders: an admissible order lacks its create or submit version row"
            );
            return Err(PrepareError::Integration);
        };
        let caller = view.actor.reference();
        let barred = recording_party_barred(
            &caller,
            recording.sales_path,
            facts.resource_tenant_id,
            &roles,
        );
        prepared.guards = GuardBindings::new()
            .bind(NOT_RECORDED, |subject: &GuardSubject<'_>| {
                if subject.acceptance_recorded {
                    GuardVerdict::Fail(Reason::AcceptanceAlreadyRecorded)
                } else {
                    GuardVerdict::Pass
                }
            })
            .bind(RECORDING_PARTY, move |_: &GuardSubject<'_>| {
                if barred {
                    GuardVerdict::Fail(Reason::AcceptanceRecordingPartyBarred)
                } else {
                    GuardVerdict::Pass
                }
            });
        // 2. A barred caller: decided from stored facts, nothing is recorded or read.
        if barred {
            prepared.inputs =
                Inputs::new().with(InputPort::AcceptanceRequirement, InputState::Resolved);
            return Ok(prepared);
        }
        // 3. The live requirement.
        let subject = RequirementSubject {
            contract_id: facts.contract_id,
            payer_tenant_id: facts.payer_tenant_id,
            seller_tenant_id: facts.seller_tenant_id,
        };
        let requirement = match self
            .resolver
            .requirement(self.ctx, &subject, None, &RunBudget::new(), self.cancel)
            .await
        {
            Ok(requirement) => requirement,
            Err(PrepareError::Unavailable) => {
                prepared.inputs = Inputs::new().with(
                    InputPort::AcceptanceRequirement,
                    InputState::Unresolvable(Reason::AcceptanceRequirementUnevaluable),
                );
                return Ok(prepared);
            }
            Err(PrepareError::Integration) => return Err(PrepareError::Integration),
        };
        let source = recorded_source(requirement);
        // 4. The consent row and its one event.
        prepared.inputs =
            Inputs::new().with(InputPort::AcceptanceRequirement, InputState::Resolved);
        // Reported with the commit's decision signal (D-257).
        prepared.consent = Some(ConsentObservation {
            source,
            path: recording.sales_path,
            due: recording.acceptance_due,
        });
        prepared.documents = Some(Arc::new(ConsentDocument {
            recorded_by: caller.clone(),
            recording_path: recording.sales_path,
            source,
        }));
        prepared.event = Some(Arc::new(AcceptanceRecordedEvent {
            recording_actor: caller,
            source,
        }));
        Ok(prepared)
    }
}

/// The one consent row, at the locked current version and the engine's transition time.
struct ConsentDocument {
    recorded_by: String,
    recording_path: SalesPath,
    source: RequirementSource,
}

#[async_trait]
impl DocumentWriter for ConsentDocument {
    async fn write(&self, documents: &ChildDocuments<'_, '_>) -> Result<(), ScopeError> {
        let (accepted_version, _) = documents.locked_version_and_generation();
        documents
            .insert_acceptance(acceptance::Model {
                order_id: documents.order_id(),
                accepted_version,
                accepted_at: documents.transition_time(),
                recorded_by: self.recorded_by.clone(),
                recording_path: self.recording_path.token().to_owned(),
                requirement_source: self.source.token().to_owned(),
            })
            .await?;
        Ok(())
    }
}

/// Row 25's `OrderAcceptanceRecorded`: the version and instant from the summary the engine hands
/// the enqueue, so they equal the stored row.
struct AcceptanceRecordedEvent {
    recording_actor: String,
    source: RequirementSource,
}

#[async_trait]
impl EventSpec for AcceptanceRecordedEvent {
    fn kind(&self) -> bss_orders_sdk::catalog::EventKind {
        bss_orders_sdk::catalog::EventKind::OrderAcceptanceRecorded
    }
    async fn enqueue(&self, enqueue: EventEnqueue<'_, '_>) -> Result<(), EventEnqueueError> {
        let detail = OrderAcceptanceRecorded {
            accepted_version: enqueue.order_version(),
            accepted_at: enqueue.occurred_at(),
            recording_actor: self.recording_actor.clone(),
            requirement_source: self.source,
        };
        enqueue.enqueue(detail).await
    }
}

#[cfg(test)]
#[path = "acceptance_tests.rs"]
mod tests;
