//! The amendment service (S4-03, S4-04; 04 §3.6 *Append Amendment*; Stage 4 DESIGN §3): the one
//! service REST and the local SDK share. It runs the submit pipeline over the proposed version,
//! through the submit service's own steps (one gate, date, authority, Pricing and budget
//! wiring): [`SubmitService::assess`], [`SubmitService::reserve`] and
//! [`SubmitService::continue_with`]; the commit is [`AmendPreparation`].
//!
//! Flow, in order:
//! 1. the [`TransitionRequest`] (`amendment`, operation `amend`, the caller's expected version N,
//!    the request body as the fingerprint document, the explanation as `caller_reason`, and a
//!    payer the delta names as the proposed arrangement), built from the call alone;
//! 2. recovery before any read or assessment ([`Engine::recover_commercial_attempt`], D-188
//!    step 1, E2): the engine authorizes the target for `amend` first (no payer permission,
//!    D-215), then a settled key is replayed exactly, a reserved attempt resumes from its frozen
//!    inputs (step 7) and only a miss continues;
//! 3. the authorized committed read ([`ReadService::get_committed`]). A draft, a state other
//!    than `submitted`, `pending_approval` or `approved`, or a current version other than N
//!    makes no external call: the engine settles the precedence refusal (`not-admissible`,
//!    `precondition-failed`) through the refusal preparation, every guard presented passing only
//!    to satisfy the binding contract. The engine decides from the locked state: when a
//!    concurrent writer made the amendment admissible at N between the read and the lock (a
//!    resume of an order on hold, a submit committing N), the engine reaches the guards, which
//!    the refusal preparation never admits; the service then runs once more from step 2 and the
//!    amendment proceeds (D-253);
//! 4. the merge of the delta onto N ([`merge_with`]; added lines get fresh time-ordered ids,
//!    handed out in ascending order so a recovery reproduces them from the frozen requests) and
//!    the request facts of the seven versioning guards ([`AmendmentFacts`], Capture's one field
//!    classification). When a versioning guard (the cap over the read count) or a structural
//!    guard (category, currency, line cap, authored-selection capacity) already fails, the gate
//!    is precluded (D-113): no date, gate, authority or Pricing call; the engine decides the
//!    earlier guard under the lock and settles it;
//! 5. the shared Stage 3 assessment over every proposed line ([`SubmitService::assess`],
//!    [`GateTarget::Amendment`], the proposed payer on the axes): a blocking assessment refuses
//!    through the engine without reserving (D-230's primary refusal);
//! 6. the D-188 attempt: the inputs are frozen and the candidate M reserved under the engine's
//!    E1 recheck (state and version under the reservation lock); phase B commands Pricing for
//!    every line at M; phase C commits M through [`AmendPreparation`]. Nothing from N is evidence
//!    for M;
//! 7. a recovered attempt re-reads N and re-merges the same delta with the attempt's own added
//!    line ids (the frozen line ids that are not N's), so the commit admits exactly the frozen
//!    lines; when N is no longer current the commit names no content and the engine refuses by
//!    precedence, burning the candidate (stale work is never rebased).
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::catalog::{OrderState, Trigger};
use bss_orders_sdk::models::CallMeta;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::capture::{Settled, content_row, fingerprint_document};
use super::engine::{Engine, PreparedExecution, Recovery, TransitionRequest};
use super::gate::amend_commit::{AMEND_ROW, AmendPreparation, Proposal};
use super::gate::submit::{
    Assessing, RefusalGate, RefusalPreparation, RefusalSubject, Reservation, SubmitService,
};
use super::read::{CommittedSnapshot, ReadService};
use crate::authz::{ArrangementDelta, Caller};
use crate::domain::gate::orchestrate::GateTarget;
use crate::domain::gate::orchestrate::TenantAxes;
use crate::domain::state_table::RowId;
use crate::domain::transition::{GuardBindings, GuardSubject, GuardVerdict};
use crate::domain::versioning::{
    AmendmentDelta, AmendmentFacts, CommittedVersion, LineDelta, merge_with,
};
use crate::infra::storage::repo::idempotency::{DurableExecution, Frozen};

/// The public catalog operation an amendment fingerprints under.
const OPERATION: &str = "amend";

/// The amendment service (module doc).
#[derive(Debug)]
pub struct AmendService {
    /// The submit pipeline the amendment runs (its gate, dates, authority, Pricing and budget).
    submit: Arc<SubmitService>,
}

fn integration(what: &'static str) -> OrdersError {
    tracing::error!(target: "orders.amend", what, "bss-orders: amendment integration failure");
    OrdersError::Integration
}

/// The states an amendment is admissible from (rows 18–20).
fn amendable(state: OrderState) -> bool {
    matches!(
        state,
        OrderState::Submitted | OrderState::PendingApproval | OrderState::Approved
    )
}

impl AmendService {
    /// The amendment service over the submit pipeline.
    #[must_use]
    pub fn new(submit: Arc<SubmitService>) -> Self {
        Self { submit }
    }

    fn reads(&self) -> &ReadService {
        self.submit.reads()
    }

    /// Amend `order_id` from committed version `meta.expected_version` by `request` (module doc).
    ///
    /// The boundary has already refused malformed bodies, a missing `If-Match` (the typed
    /// expected version) and a delta naming one line twice; everything here runs under the
    /// caller's authority.
    ///
    /// # Errors
    /// The engine's unsettled refusals and the read's denial and not-found answers;
    /// `Unavailable` when the engine is not bound or a date, gate, principal or Pricing answer
    /// cannot be had (a same-key retry continues); `Integration` for a broken frozen attempt or
    /// contract. A business refusal is settled and returned as `Ok`.
    pub async fn amend(
        &self,
        caller: &Caller,
        order_id: Uuid,
        request: AmendOrder,
        meta: &CallMeta,
        cancel: &CancellationToken,
    ) -> Result<Settled, OrdersError> {
        // One pass, and a second one only when the first found the order amendable under the
        // engine's lock although its read did not (module doc, step 3); a second race answers
        // what the engine answered. Each pass is boxed so callers hold a small future.
        if let Some(settled) =
            Box::pin(self.pass(caller, order_id, &request, meta, cancel, true)).await?
        {
            return Ok(settled);
        }
        tracing::debug!(
            target: "orders.amend",
            order_id = %order_id,
            "bss-orders: the order became amendable between the read and the lock; deciding again"
        );
        Box::pin(self.pass(caller, order_id, &request, meta, cancel, false))
            .await?
            .ok_or(OrdersError::Unavailable)
    }

    /// Steps 1–7 (module doc) once. `None` only when `retry` is set and the engine reached the
    /// guards of a refusal-only preparation, i.e. the locked order was amendable at N.
    async fn pass(
        &self,
        caller: &Caller,
        order_id: Uuid,
        request: &AmendOrder,
        meta: &CallMeta,
        cancel: &CancellationToken,
        retry: bool,
    ) -> Result<Option<Settled>, OrdersError> {
        let engine = self.submit.engine()?;
        let delta = AmendmentDelta::from_request(request);
        let base_version = i32::from(meta.expected_version);
        // 1. The request, from the call alone.
        let transition = TransitionRequest {
            order_id,
            trigger: Trigger::Amendment,
            operation: OPERATION,
            expected_version: meta.expected_version,
            expected_draft_revision: None,
            document: fingerprint_document(request)?,
            // Only the payer may move on an amendment; a named resource or seller is the
            // `tenant-axis-immutable` guard's, never a proposal.
            proposed: delta.header.payer_tenant_id.map(|payer| ArrangementDelta {
                resource_tenant_id: None,
                payer_tenant_id: Some(payer),
            }),
            caller_reason: Some(request.reason.clone()),
            idempotency_key: meta.idempotency_key.clone(),
            correlation_id: meta.correlation_id,
            execution: None,
        };
        // 2. Recovery before any read or assessment.
        match engine
            .recover_commercial_attempt(caller, &transition)
            .await?
        {
            Recovery::Replay(outcome) => return Ok(Some(Settled::from_outcome(*outcome))),
            Recovery::Existing(existing) => {
                let (execution, frozen) = *existing;
                return self
                    .resume(
                        caller,
                        &engine,
                        transition,
                        &delta,
                        &request.reason,
                        &execution,
                        frozen,
                        cancel,
                    )
                    .await
                    .map(Some);
            }
            Recovery::Miss => {}
        }
        // 3. The authorized committed read of N.
        let Some(snapshot) = self
            .reads()
            .get_committed(caller, order_id)
            .await?
            .filter(|s| s.version.version == base_version && amendable(s.state))
        else {
            // The engine decides from the locked state; reaching any guard means the locked
            // order is amendable at N, which this preparation never admits (module doc, step 3).
            let reached = Arc::new(AtomicBool::new(false));
            let preparation = RefusalPreparation::proposed(
                RefusalGate::NotRun,
                RefusalSubject::Unread {
                    row: AMEND_ROW,
                    passing: passing(&engine, &reached),
                },
            );
            let refused = refuse(caller, &engine, transition, &preparation).await;
            if retry && refused.is_err() && reached.load(Ordering::SeqCst) {
                return Ok(None);
            }
            return refused.map(Some);
        };
        // 4. The merge, the request facts and the local guards before any external call.
        let CommittedSnapshot {
            category,
            amendment_count,
            version: n,
            ..
        } = snapshot;
        let header = n.header;
        let proposal = self.propose(
            &engine,
            order_id,
            &n,
            &delta,
            &request.reason,
            Allocation::Fresh,
        )?;
        let structural = proposal.structural();
        let capacity = proposal.item_capacity();
        let local = proposal
            .facts
            .first_failure(amendment_count)
            .or_else(|| structural.first_failure(&category))
            .or(match capacity {
                GuardVerdict::Fail(reason) => Some(reason),
                GuardVerdict::Pass => None,
            });
        let subject = RefusalSubject::Proposed {
            own: proposal.facts.clone().bindings(),
            structural,
            capacity,
        };
        if local.is_some() {
            let preparation = RefusalPreparation::proposed(RefusalGate::Precluded, subject);
            return refuse(caller, &engine, transition, &preparation)
                .await
                .map(Some);
        }
        // 5. The shared assessment over every proposed line.
        let lines = proposal.version.as_draft_lines();
        let proposed = &proposal.version.header;
        let assessing = Assessing {
            order_id,
            axes: TenantAxes {
                resource: header.resource_tenant_id,
                payer: proposed.payer_tenant_id,
                seller: header.seller_tenant_id,
            },
            contract_id: proposed.contract_id,
            lines: &lines,
            target: GateTarget::Amendment { order_id },
        };
        let assessed = self.submit.assess(caller, &assessing, meta, cancel).await?;
        if let Some(gate) = assessed.refusal(&assessing)? {
            let preparation = RefusalPreparation::proposed(gate, subject);
            return refuse(caller, &engine, transition, &preparation)
                .await
                .map(Some);
        }
        // 6. The D-188 attempt: reserve M, then phases B and C.
        let reserved = Box::pin(self.submit.reserve(
            caller,
            &engine,
            &transition,
            &assessing,
            assessed,
            Reservation {
                prepared_draft_revision: None,
                current_version: base_version,
                correlation_id: meta.correlation_id,
            },
        ))
        .await?;
        let (execution, frozen) = match reserved {
            PreparedExecution::Settled(outcome) => {
                return Ok(Some(Settled::from_outcome(*outcome)));
            }
            PreparedExecution::Execution(execution) => *execution,
        };
        let reason = request.reason.clone();
        self.submit
            .continue_with(
                caller,
                &engine,
                transition,
                &execution,
                frozen,
                cancel,
                |reserved| {
                    Ok(AmendPreparation {
                        reserved,
                        base_version,
                        proposal: Some(proposal),
                        reason,
                    })
                },
            )
            .await
            .map(Some)
    }

    /// Step 7 (module doc): phases B and C of a recovered attempt over the same delta re-merged
    /// onto N with the attempt's own added line ids, or over no content when N is no longer
    /// current.
    #[expect(
        clippy::too_many_arguments,
        reason = "the resume reads the caller, the engine, the request, the delta, the \
                  explanation, the owner, the frozen attempt and the cancellation"
    )]
    async fn resume(
        &self,
        caller: &Caller,
        engine: &Engine,
        transition: TransitionRequest,
        delta: &AmendmentDelta,
        reason: &str,
        execution: &DurableExecution,
        frozen: Frozen,
        cancel: &CancellationToken,
    ) -> Result<Settled, OrdersError> {
        let base_version = i32::from(transition.expected_version);
        let n = self
            .reads()
            .get_committed(caller, transition.order_id)
            .await?
            .filter(|s| s.version.version == base_version)
            .map(|s| s.version);
        let order_id = transition.order_id;
        self.submit
            .continue_with(
                caller,
                engine,
                transition,
                execution,
                frozen,
                cancel,
                |reserved| {
                    let proposal = match &n {
                        Some(n) => {
                            let added: Vec<Uuid> = reserved
                                .requests
                                .lines
                                .iter()
                                .map(|l| l.line_id)
                                .filter(|id| !n.lines.iter().any(|l| l.line_id == *id))
                                .collect();
                            Some(self.propose(
                                engine,
                                order_id,
                                n,
                                delta,
                                reason,
                                Allocation::Frozen(added),
                            )?)
                        }
                        None => None,
                    };
                    Ok(AmendPreparation {
                        reserved,
                        base_version,
                        proposal,
                        reason: reason.to_owned(),
                    })
                },
            )
            .await
    }

    /// The merge of `delta` onto N with the added lines' ids from `allocation`, ascending in
    /// delta order, its guard facts and every proposed line's content row (module doc, step 4).
    fn propose(
        &self,
        engine: &Engine,
        order_id: Uuid,
        n: &CommittedVersion,
        delta: &AmendmentDelta,
        reason: &str,
        allocation: Allocation,
    ) -> Result<Proposal, OrdersError> {
        let adds = delta
            .lines
            .iter()
            .filter(|op| matches!(op, LineDelta::Add(_)))
            .count();
        let mut added = match allocation {
            Allocation::Fresh => (0..adds).map(|_| Uuid::now_v7()).collect(),
            Allocation::Frozen(added) if added.len() == adds => added,
            Allocation::Frozen(_) => {
                return Err(integration(
                    "the added line ids are not the delta's additions",
                ));
            }
        };
        added.sort_unstable();
        let mut ids = added.into_iter();
        // The count is checked above, so the allocator never runs dry; a nil id would collide
        // with nothing and is refused by the identity's primary key at worst.
        let version = merge_with(n, delta, || ids.next().unwrap_or_default())
            .map_err(|_| integration("the delta does not merge onto N"))?;
        let facts = AmendmentFacts::of(
            &engine.registries().fields,
            delta,
            &version,
            Some(reason.to_owned()),
        )
        .map_err(|_| integration("an amendment field is not classified"))?;
        let content = version
            .lines
            .iter()
            .map(|l| content_row(order_id, &l.line))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| OrdersError::Integration)?;
        Ok(Proposal {
            version,
            facts,
            content,
            line_cap: self.submit.line_cap(),
        })
    }
}

/// Where an amendment's added line ids come from.
enum Allocation {
    /// A fresh attempt: new time-ordered ids.
    Fresh,
    /// A recovered attempt: its own added ids (the frozen line ids that are not N's).
    Frozen(Vec<Uuid>),
}

/// Every guard of rows 18–20 bound passing: the binding contract of a refusal the engine settles
/// by precedence before any guard (module doc, step 3). Each records in `reached` that the engine
/// evaluated it, i.e. found the locked order amendable at N.
fn passing(engine: &Engine, reached: &Arc<AtomicBool>) -> GuardBindings {
    engine
        .registries()
        .guards
        .row_guards(RowId::of(AMEND_ROW))
        .iter()
        .fold(GuardBindings::new(), |bindings, guard| {
            let reached = Arc::clone(reached);
            bindings.bind(guard.name, move |_: &GuardSubject<'_>| {
                reached.store(true, Ordering::SeqCst);
                GuardVerdict::Pass
            })
        })
}

/// Settle a refusal preparation through the engine.
async fn refuse(
    caller: &Caller,
    engine: &Engine,
    transition: TransitionRequest,
    preparation: &RefusalPreparation,
) -> Result<Settled, OrdersError> {
    let outcome = engine.transition(caller, transition, preparation).await?;
    Ok(Settled::from_outcome(outcome))
}
