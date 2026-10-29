//! The begin-fulfillment consent and payment guard on real PostgreSQL (S4-10; 05 §3.3; Stage 4
//! DESIGN §6; D-256). The public operation is an undelivered Workflow seam (Stage 5), so row 11
//! is driven through `Engine::transition` with [`BeginFulfillmentPreparation`] by the configured
//! Workflow service, over the S4-08b world (`acceptance.rs`): partner-placed orders, consent
//! recorded by the customer through the real record-acceptance service, the live contract read
//! through the development contract stub and the tolerance elections from the policy store.
//! Compiled only with `gate-dev-stubs`.
//!
//! Two doubles, each labelled where it is used: S5-04's `gate.binding-deadline` is bound to pass
//! ([`DeadlineDouble`]), and Workflow's approval reflection (row 8, S5-03) is owner SQL
//! ([`approve`]), as `engine.rs` `seed` does.
use super::acceptance::{World, customer, meta, partner, policy, record, world};
use super::capture::key;
use super::submit::{ORDER_RT, SELLER, WORKFLOW, contract_fixture, elect, submit_with};
use super::*;
use crate::authz::Caller;
use crate::authz::test_pdp::{RulesProvider, service, user};
use crate::domain::transition::{GuardSubject, GuardVerdict};
use crate::infra::acceptance::RequirementResolver;
use crate::infra::amend::AmendService;
use crate::infra::capture::SettledOutcome;
use crate::infra::engine::{
    Engine, EngineError, EngineOutcome, OutcomeKind, Preparation, PreparationView, PrepareError,
    Prepared, TransitionRequest,
};
use crate::infra::fulfillment::BeginFulfillmentPreparation;
use crate::infra::gate::ports::ContractReads;
use crate::infra::gate::stubs::ContractsStub;
use crate::infra::gate::tests::orchestrate::CONTRACT;
use crate::infra::gate::unavailable::ContractsUnavailable;
use async_trait::async_trait;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::catalog::{OrderState as S, Reason, Trigger};
use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::RequirementSource;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn workflow() -> Caller {
    service(WORKFLOW.0, WORKFLOW.1)
}

/// TEST DOUBLE for S5-04's `gate.binding-deadline`, row 11's third guard: bound to pass so these
/// tests observe only the two S4-10 guards. The binding deadline is S5-04's to implement.
struct DeadlineDouble<'a>(BeginFulfillmentPreparation<'a>);
#[async_trait]
impl Preparation for DeadlineDouble<'_> {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let mut prepared = self.0.prepare(view).await?;
        prepared.guards = std::mem::take(&mut prepared.guards)
            .bind("gate.binding-deadline", |_: &GuardSubject<'_>| {
                GuardVerdict::Pass
            });
        Ok(prepared)
    }
}

/// A resolver reading the contract's `acceptance_required` as `stated` (the development contract
/// stub), with the world's policy store or, for an outage, none.
fn resolver(w: &World, stated: Option<bool>, store: bool) -> RequirementResolver {
    let contracts: Arc<dyn ContractReads> =
        Arc::new(ContractsStub::new(Arc::new(contract_fixture(stated))));
    RequirementResolver::new(contracts, store.then(|| w.f.t.env.db.clone()))
}

/// The world's engine under its policy plus a Workflow rule for exactly `order` of the seller:
/// a service decision must restrict to finite order ids (D-226).
fn workflow_engine(w: &World, order: Uuid) -> Engine {
    let mut policy = policy();
    policy["rules"].as_array_mut().unwrap().push(json!({
        "id": "workflow", "subject": {"id": u(WORKFLOW.0).to_string()}, "resource_type": ORDER_RT,
        "actions": ["read", "begin_fulfillment"],
        "paths": [{"predicates": [
            {"property": "id", "values": [order.to_string()]},
            {"property": "seller_tenant_id", "values": [u(SELLER).to_string()]},
        ]}],
    }));
    w.f.t.engine(RulesProvider::from_policy(policy))
}

/// Row 11 by the Workflow service, with `outcome` as the request body.
async fn begin(
    w: &World,
    resolver: &RequirementResolver,
    order: Uuid,
    version: i64,
    outcome: Authorization,
    text: &str,
) -> Result<EngineOutcome, EngineError> {
    let caller = workflow();
    let cancel = CancellationToken::new();
    let body = AuthorizationOutcome { outcome };
    let request = TransitionRequest {
        order_id: order,
        trigger: Trigger::BeginFulfillment,
        operation: "begin_fulfillment",
        expected_version: OrderVersion::try_from(version).unwrap(),
        expected_draft_revision: None,
        document: serde_json::to_value(body).unwrap(),
        proposed: None,
        caller_reason: None,
        idempotency_key: key(text),
        correlation_id: None,
        execution: None,
    };
    let preparation = DeadlineDouble(BeginFulfillmentPreparation::new(
        resolver,
        caller.ctx(),
        body,
        &cancel,
    ));
    workflow_engine(w, order)
        .transition(&caller, request, &preparation)
        .await
}

/// TEST DOUBLE for Workflow's approval reflection (row 8, S5-03): owner SQL moves a submitted
/// order to `approved`, as `engine.rs` `seed` does.
async fn approve(w: &World, order: Uuid) {
    w.f.t
        .env
        .pg
        .sql(&format!(
            "UPDATE bss_orders__order SET state='approved' WHERE order_id='{order}' \
             AND state='submitted'"
        ))
        .await
        .unwrap();
}

/// Set the `tolerate_authorization_failure` election of `seller` (`None` = platform) to
/// `elected`, as the policy channel's promotion would (D-250): update the row, or insert it.
/// Election rows are never deleted (the evidence trigger refuses it), so a cell that needs no
/// row runs before any row of that scope is stored.
async fn tolerate(w: &World, seller: Option<Uuid>, elected: bool) {
    let (scope, scope_id) = match seller {
        Some(id) => ("seller", format!("'{id}'")),
        None => ("platform", "NULL".to_owned()),
    };
    w.f.t
        .env
        .pg
        .sql(&format!(
            "INSERT INTO bss_orders__policy_election VALUES('{}',\
             'tolerate_authorization_failure','{scope}',{scope_id},{elected},'test',\
             clock_timestamp()) ON CONFLICT (election, scope, scope_id) \
             DO UPDATE SET elected=EXCLUDED.elected, elected_at=EXCLUDED.elected_at",
            Uuid::new_v4()
        ))
        .await
        .unwrap();
}

/// Everything a refusal must leave alone: state, version, dwell clock, the tolerance flag, the
/// fulfillment generation, version rows, overlap claims, grants and consent rows.
async fn footprint(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(o.state, o.current_version, o.state_entered_at, \
             o.authorization_failure_tolerated_at, o.fulfillment_control_generation, \
             (SELECT count(*) FROM bss_orders__order_version v WHERE v.order_id=o.order_id), \
             (SELECT count(*) FROM bss_orders__inflight_overlap_claim c WHERE c.order_id=o.order_id), \
             (SELECT count(*) FROM bss_orders__fulfillment_grant g WHERE g.order_id=o.order_id), \
             (SELECT count(*) FROM bss_orders__acceptance a WHERE a.order_id=o.order_id)) \
             AS j FROM bss_orders__order o WHERE order_id='{order}'"
        ))
        .await
}

/// `[state, authorization_failure_tolerated_at]` of `order`.
async fn flag(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(state, authorization_failure_tolerated_at) AS j \
             FROM bss_orders__order WHERE order_id='{order}'"
        ))
        .await
}

/// The `created_at` of `order`'s committed audit entry for `trigger`.
async fn committed_at(w: &World, order: Uuid, trigger: &str) -> Value {
    w.f.t
        .json(&format!(
            "SELECT to_jsonb(created_at) AS j FROM bss_orders__transition_audit \
             WHERE order_id='{order}' AND trigger='{trigger}' AND outcome='committed'"
        ))
        .await
}

/// A partner-placed, contract-bound `approved` order at version N, with the customer's consent
/// on N when `consent` (the world's contract requires it, source `contract`). Its overlap claim
/// is released so the next order of the world can submit.
async fn approved(w: &World, tag: &str, consent: bool) -> (Uuid, i64) {
    let (order, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), tag)
        .await;
    w.release(order).await;
    if consent {
        let settled = record(
            &w.recorder(),
            &customer(),
            order,
            n,
            &format!("{tag}-consent"),
        )
        .await
        .unwrap();
        assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    }
    approve(w, order).await;
    (order, n)
}

/// The tolerance column of the matrix.
#[derive(Debug, Clone, Copy)]
enum Tolerance {
    /// Seller elects `true`; the platform's `false` is never read.
    SellerTrue,
    /// Seller elects `false`; the platform's `true` is never read.
    SellerFalse,
    /// No seller row; the platform elects `true`.
    PlatformTrue,
    /// No seller row; the platform elects `false`.
    PlatformFalse,
    /// No row at either scope: `false`.
    Unset,
    /// The policy store is unavailable.
    Outage,
}
impl Tolerance {
    /// In matrix order: no row at either scope first (rows are never deleted), then the
    /// platform's row, then the seller's over the opposite platform value.
    const ORDER: [Self; 6] = [
        Self::Unset,
        Self::Outage,
        Self::PlatformFalse,
        Self::PlatformTrue,
        Self::SellerTrue,
        Self::SellerFalse,
    ];
    async fn arrange(self, w: &World) {
        let (seller, platform) = match self {
            Self::SellerTrue => (Some(true), Some(false)),
            Self::SellerFalse => (Some(false), Some(true)),
            Self::PlatformTrue => (None, Some(true)),
            Self::PlatformFalse => (None, Some(false)),
            Self::Unset | Self::Outage => (None, None),
        };
        if let Some(elected) = platform {
            tolerate(w, None, elected).await;
        }
        if let Some(elected) = seller {
            tolerate(w, Some(u(SELLER)), elected).await;
        }
    }
    fn store(self) -> bool {
        !matches!(self, Self::Outage)
    }
}

/// What one matrix cell must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Admitted { tolerated: bool },
    Refused(Reason),
    Unavailable,
}

/// The consent column of the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consent {
    /// The contract requires consent and the current version has none.
    RequiredMissing,
    /// The contract requires consent and the customer recorded it on the current version.
    RequiredRecorded,
    /// The contract does not require consent and none is recorded.
    Optional,
}

/// The payment answer when the consent guard passes.
fn payment(outcome: Authorization, tolerance: Tolerance) -> Expect {
    match (outcome, tolerance) {
        (Authorization::Authorized, _) => Expect::Admitted { tolerated: false },
        (Authorization::Pending, _) => Expect::Refused(Reason::AuthorizationPending),
        (Authorization::Failed, Tolerance::SellerTrue | Tolerance::PlatformTrue) => {
            Expect::Admitted { tolerated: true }
        }
        (Authorization::Failed, Tolerance::Outage) => Expect::Unavailable,
        (Authorization::Failed, _) => Expect::Refused(Reason::AuthorizationFailed),
    }
}

fn expected(consent: Consent, outcome: Authorization, tolerance: Tolerance) -> Expect {
    match (consent, outcome, tolerance) {
        // A tolerance outage is a pre-transaction `unavailable` (nothing is settled), so it
        // answers before the lock-time consent guard is reached (D-256 item 4).
        (Consent::RequiredMissing, Authorization::Failed, Tolerance::Outage) => Expect::Unavailable,
        (Consent::RequiredMissing, ..) => Expect::Refused(Reason::AcceptanceRequiredNotRecorded),
        _ => payment(outcome, tolerance),
    }
}

/// D-256, the S4-10 matrix: required consent missing / required and recorded / optional ×
/// authorized / pending / failed × the tolerance election at seller, platform, unset and an
/// outage of the policy store. Required-and-missing refuses `acceptance-required-not-recorded`
/// before any payment branch; `authorized` admits; `pending` refuses `authorization-pending`;
/// `failed` admits only on a `true` election (seller first, then platform; unset is `false`),
/// writing `authorization_failure_tolerated_at` at the committed audit entry's instant, and
/// refuses `authorization-failed` otherwise. An outage is `unavailable`, never "not tolerated"
/// and never "tolerated"; authorized and pending never read the election, so the outage cannot
/// touch them. Every refusal leaves the order `approved` with no claim, grant, version, consent
/// or event change, and is audited; `unavailable` settles nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_consent_and_payment_matrix() {
    let w = world(contract_fixture(Some(true))).await;
    let mut cells = Vec::new();
    for tolerance in Tolerance::ORDER {
        for consent in [
            Consent::RequiredMissing,
            Consent::RequiredRecorded,
            Consent::Optional,
        ] {
            for outcome in [
                Authorization::Authorized,
                Authorization::Pending,
                Authorization::Failed,
            ] {
                cells.push((consent, outcome, tolerance));
            }
        }
    }
    for (i, (consent, outcome, tolerance)) in cells.into_iter().enumerate() {
        let cell = format!("{consent:?}/{outcome:?}/{tolerance:?}");
        let tag = format!("bf-m{i}");
        let (order, n) = approved(&w, &tag, consent == Consent::RequiredRecorded).await;
        tolerance.arrange(&w).await;
        let required = consent != Consent::Optional;
        let r = resolver(&w, Some(required), tolerance.store());
        let before = footprint(&w, order).await;
        let (outbox, audits) = (w.f.t.outbox().await, w.f.t.audits(order).await);

        let answer = begin(&w, &r, order, n, outcome, &format!("{tag}-begin")).await;
        match expected(consent, outcome, tolerance) {
            Expect::Admitted { tolerated } => {
                let out = answer.unwrap();
                assert!(
                    matches!(out.kind, OutcomeKind::Committed { .. }),
                    "{cell}: {out:?}"
                );
                let [state, at] =
                    <[Value; 2]>::try_from(flag(&w, order).await.as_array().unwrap().clone())
                        .unwrap();
                assert_eq!(state, json!("in_fulfillment"), "{cell}");
                if tolerated {
                    assert_eq!(
                        at,
                        committed_at(&w, order, "begin-fulfillment").await,
                        "{cell}: the flag is the admission's transition time"
                    );
                } else {
                    assert_eq!(at, Value::Null, "{cell}: only a tolerated failure flags");
                }
                assert_eq!(w.f.t.audits(order).await, audits + 1, "{cell}");
                assert_eq!(w.f.t.outbox().await, outbox, "{cell}: row 11 is eventless");
            }
            Expect::Refused(reason) => {
                let out = answer.unwrap();
                assert!(
                    matches!(out.kind, OutcomeKind::Refused { reason: r, .. } if r == reason),
                    "{cell}: {out:?}"
                );
                assert_eq!(out.response.status(), 400, "{cell}");
                assert_eq!(
                    footprint(&w, order).await,
                    before,
                    "{cell}: nothing changes"
                );
                assert_eq!(w.f.t.outbox().await, outbox, "{cell}");
                assert_eq!(w.f.t.audits(order).await, audits + 1, "{cell}: audited");
            }
            Expect::Unavailable => {
                assert_eq!(answer.unwrap_err(), EngineError::Unavailable, "{cell}");
                assert_eq!(
                    footprint(&w, order).await,
                    before,
                    "{cell}: nothing changes"
                );
                assert_eq!(w.f.t.outbox().await, outbox, "{cell}");
                assert_eq!(w.f.t.audits(order).await, audits, "{cell}: nothing settles");
                // The same key is retryable once the store answers (no election row: `false`),
                // and the lock-time consent guard then answers first.
                let healthy = resolver(&w, Some(required), true);
                let retry = begin(&w, &healthy, order, n, outcome, &format!("{tag}-begin"))
                    .await
                    .unwrap();
                let reason = if consent == Consent::RequiredMissing {
                    Reason::AcceptanceRequiredNotRecorded
                } else {
                    Reason::AuthorizationFailed
                };
                assert!(
                    matches!(retry.kind, OutcomeKind::Refused { reason: r, .. } if r == reason),
                    "{cell}: {retry:?}"
                );
            }
        }
    }
}

/// An unavailable requirement is the unresolvable input `acceptance-requirement-unevaluable`
/// (503, settled), for the contract port and for the policy store, and the tolerance election
/// is never read: a failed outcome with no store would otherwise answer unsettled
/// `unavailable`. The order stays `approved` and the same attempt admits once the input
/// resolves.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unavailable_requirement_is_unevaluable_and_never_reads_tolerance() {
    let w = world(contract_fixture(Some(true))).await;
    // (a) The contract port is down on a contract-bound order.
    let (bound, n) = approved(&w, "bf-503a", true).await;
    let before = footprint(&w, bound).await;
    let down = RequirementResolver::new(Arc::new(ContractsUnavailable), None);
    let out = begin(&w, &down, bound, n, Authorization::Failed, "bf-503a-1")
        .await
        .unwrap();
    assert!(
        matches!(
            out.kind,
            OutcomeKind::Refused {
                reason: Reason::AcceptanceRequirementUnevaluable,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(out.response.status(), 503);
    assert_eq!(footprint(&w, bound).await, before);
    let out = begin(
        &w,
        &resolver(&w, Some(true), true),
        bound,
        n,
        Authorization::Authorized,
        "bf-503a-2",
    )
    .await
    .unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");

    // (b) No policy store on an order without a contract: the elections decide its requirement.
    let (free, n) = w.submitted(&partner(), &partner(), None, "bf-503b").await;
    w.release(free).await;
    approve(&w, free).await;
    let before = footprint(&w, free).await;
    let no_store = RequirementResolver::new(Arc::new(ContractsUnavailable), None);
    let out = begin(&w, &no_store, free, n, Authorization::Failed, "bf-503b-1")
        .await
        .unwrap();
    assert!(
        matches!(
            out.kind,
            OutcomeKind::Refused {
                reason: Reason::AcceptanceRequirementUnevaluable,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(footprint(&w, free).await, before);
}

/// Consent volunteered while nothing required it satisfies the guard once the requirement
/// becomes true: the guard asks whether the current version has a consent, not which source it
/// recorded. The policy change does not rewrite the recorded source (05 §3.1 step 5).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn volunteered_consent_counts_and_keeps_its_source_when_the_policy_changes() {
    let w = world(contract_fixture(Some(true))).await;
    elect(&w.f, Some(u(SELLER)), false).await;
    let (order, n) = w.submitted(&partner(), &partner(), None, "bf-vol").await;
    w.release(order).await;
    let settled = record(&w.recorder(), &customer(), order, n, "bf-vol-consent")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let recorded = w
        .consent(&customer(), order)
        .await
        .current_acceptance
        .unwrap();
    assert_eq!(recorded.requirement_source, RequirementSource::Volunteered);
    approve(&w, order).await;
    // The seller now requires consent.
    w.f.t
        .env
        .pg
        .sql(
            "UPDATE bss_orders__policy_election SET elected=true \
             WHERE election='acceptance_required' AND scope='seller'",
        )
        .await
        .unwrap();
    let r = RequirementResolver::new(Arc::new(ContractsUnavailable), Some(w.f.t.env.db.clone()));
    let out = begin(&w, &r, order, n, Authorization::Authorized, "bf-vol-begin")
        .await
        .unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
    assert_eq!(
        w.consent(&customer(), order).await.current_acceptance,
        Some(recorded),
        "the recorded source stays volunteered"
    );
}

/// Consent recorded on N does not satisfy M: after an amendment of the approved order, begin on
/// M refuses `acceptance-required-not-recorded` although N's consent exists; once the customer
/// re-accepts M it admits.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn consent_on_n_does_not_satisfy_m() {
    let w = world(contract_fixture(Some(true))).await;
    let v = w.draft(&partner(), Some(CONTRACT), "bf-nm").await;
    let order = v.order.order_id;
    let submitted = submit_with(&w.submit, &partner(), &v, "bf-nm-submit")
        .await
        .unwrap();
    let n = submitted.response.body()["version"].as_i64().unwrap();
    let settled = record(&w.recorder(), &customer(), order, n, "bf-nm-consent-n")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    approve(&w, order).await;
    let request: AmendOrder =
        serde_json::from_value(super::amend::change_quantity(v.lines[0].line_id, "2")).unwrap();
    let amended = AmendService::new(Arc::clone(&w.submit))
        .amend(
            &partner(),
            order,
            request,
            &meta("bf-nm-amend", n),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(amended.outcome, SettledOutcome::Committed, "{amended:?}");
    let m = amended.response.body()["version"].as_i64().unwrap();
    approve(&w, order).await;
    let r = resolver(&w, Some(true), true);
    let before = footprint(&w, order).await;
    let out = begin(&w, &r, order, m, Authorization::Authorized, "bf-nm-begin-1")
        .await
        .unwrap();
    assert!(
        matches!(
            out.kind,
            OutcomeKind::Refused {
                reason: Reason::AcceptanceRequiredNotRecorded,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(footprint(&w, order).await, before);
    let settled = record(&w.recorder(), &customer(), order, m, "bf-nm-consent-m")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let out = begin(&w, &r, order, m, Authorization::Authorized, "bf-nm-begin-2")
        .await
        .unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
}

/// `authorization_failure_tolerated_at` is written in the admitting transaction at its
/// transition time and never cleared or moved by a later transition (here a hold, row 21, whose
/// preparation is the `engine.rs` test binding). The committed audit entry of row 11 carries no
/// tolerance member, so the column is the evidence (D-256 item 5).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_tolerance_flag_is_written_once_and_never_cleared() {
    let w = world(contract_fixture(Some(true))).await;
    let (order, n) = approved(&w, "bf-flag", true).await;
    tolerate(&w, Some(u(SELLER)), true).await;
    let out = begin(
        &w,
        &resolver(&w, Some(true), true),
        order,
        n,
        Authorization::Failed,
        "bf-flag-begin",
    )
    .await
    .unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
    let at = committed_at(&w, order, "begin-fulfillment").await;
    assert_eq!(flag(&w, order).await, json!(["in_fulfillment", at]));
    let audit =
        w.f.t
            .json(&format!(
                "SELECT jsonb_build_array(reason, from_state, to_state) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' \
             AND trigger='begin-fulfillment' AND outcome='committed'"
            ))
            .await;
    assert_eq!(
        audit,
        json!(["begin-fulfillment", "approved", "in_fulfillment"])
    );

    let held = w
        .engine
        .transition(
            &user(super::capture::BUYER.0, super::capture::BUYER.1),
            super::engine::request(
                order,
                Trigger::Hold,
                "bf-flag-hold",
                i32::try_from(n).unwrap(),
            ),
            &super::engine::fixed(super::engine::prepared(21, order, S::InFulfillment, None)),
        )
        .await
        .unwrap();
    assert!(
        matches!(held.kind, OutcomeKind::Committed { .. }),
        "{held:?}"
    );
    assert_eq!(
        flag(&w, order).await,
        json!(["on_hold", at]),
        "never cleared"
    );
}
