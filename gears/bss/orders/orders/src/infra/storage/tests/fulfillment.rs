//! The begin-fulfillment consent and payment guard on real PostgreSQL (S4-10; 05 §3.3; Stage 4
//! DESIGN §6; D-256). The public operation is an undelivered Workflow seam (Stage 5), so row 11
//! is driven through its Stage 5 seam driver [`begin_fulfillment`] by the configured Workflow
//! service, over the S4-08b world (`acceptance.rs`): partner-placed orders, consent
//! recorded by the customer through the real record-acceptance service, the live contract read
//! through the development contract stub and the tolerance elections from the policy store.
//! Compiled only with `gate-dev-stubs`.
//!
//! Two doubles, each labelled where it is used: S5-04's `gate.binding-deadline` is bound to pass
//! ([`deadline_double`]), and Workflow's approval reflection (row 8, S5-03) is owner SQL
//! ([`approve`]), as `engine.rs` `seed` does.
use super::acceptance::{
    Before, BeforeLock, Hooked, World, customer, meta, partner, policy, record, world,
};
use super::amend::{force, sql_work};
use super::capture::key;
use super::submit::{ORDER_RT, RESOURCE, SELLER, WORKFLOW, contract_fixture, elect, submit_with};
use super::*;
use crate::authz::Caller;
use crate::authz::test_pdp::{RulesProvider, service, user};
use crate::domain::preconditions::{Requirement, RequirementSource as Source};
use crate::domain::transition::{GuardSubject, GuardVerdict};
use crate::infra::acceptance::{RequirementResolver, RequirementSubject};
use crate::infra::amend::AmendService;
use crate::infra::capture::SettledOutcome;
use crate::infra::engine::{
    Engine, EngineError, EngineOutcome, OutcomeKind, PrepareError, TransitionRequest,
};
use crate::infra::fulfillment::{Deadline, begin_fulfillment};
use crate::infra::gate::budget::RunBudget;
use crate::infra::gate::ports::ContractReads;
use crate::infra::gate::stubs::ContractsStub;
use crate::infra::gate::tests::orchestrate::CONTRACT;
use crate::infra::gate::unavailable::ContractsUnavailable;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::catalog::{OrderState as S, Reason, Trigger};
use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::RequirementSource;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

fn workflow() -> Caller {
    service(WORKFLOW.0, WORKFLOW.1)
}

/// TEST DOUBLE for S5-04's `gate.binding-deadline`, row 11's third guard: a predicate that
/// passes, so these tests observe only the two S4-10 guards. The binding deadline is S5-04's to
/// implement; its owner hands the predicate to the driver.
fn deadline_double() -> Deadline {
    Arc::new(|_: &GuardSubject<'_>| GuardVerdict::Pass)
}

/// A resolver reading the contract's `acceptance_required` as `stated` (the development contract
/// stub), with the world's policy store or, for an outage, none.
pub(super) fn resolver(w: &World, stated: Option<bool>, store: bool) -> RequirementResolver {
    resolver_over(
        w,
        Arc::new(ContractsStub::new(Arc::new(contract_fixture(stated)))),
        store,
    )
}

/// A resolver over `contracts`, with the world's policy store or, for an outage, none.
fn resolver_over(w: &World, contracts: Arc<dyn ContractReads>, store: bool) -> RequirementResolver {
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
pub(super) async fn begin(
    w: &World,
    resolver: &RequirementResolver,
    order: Uuid,
    version: i64,
    outcome: Authorization,
    text: &str,
) -> Result<EngineOutcome, EngineError> {
    let engine = workflow_engine(w, order);
    begin_on(&engine, resolver, order, version, outcome, text).await
}

/// As [`begin`], on `engine` (the world's Workflow engine, possibly with a test hook).
async fn begin_on(
    engine: &Engine,
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
    begin_fulfillment(
        engine,
        &caller,
        &request,
        resolver,
        body,
        &deadline_double(),
        &cancel,
    )
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

/// Set the `tolerate_authorization_failure` election of `seller` (`None` = platform).
pub(super) async fn tolerate(w: &World, seller: Option<Uuid>, elected: bool) {
    upsert(w, "tolerate_authorization_failure", seller, elected).await;
}

/// Set `election` of `seller` (`None` = platform) to `elected`, as the policy channel's promotion
/// would (D-250): update the row, or insert it. Election rows are never deleted (the evidence
/// trigger refuses it), so a cell that needs no row runs before any row of that scope is stored.
async fn upsert(w: &World, election: &str, seller: Option<Uuid>, elected: bool) {
    let (scope, scope_id) = match seller {
        Some(id) => ("seller", format!("'{id}'")),
        None => ("platform", "NULL".to_owned()),
    };
    w.f.t
        .env
        .pg
        .sql(&format!(
            "INSERT INTO bss_orders__policy_election VALUES('{}',\
             '{election}','{scope}',{scope_id},{elected},'test',\
             clock_timestamp()) ON CONFLICT (election, scope, scope_id) \
             DO UPDATE SET elected=EXCLUDED.elected, elected_at=EXCLUDED.elected_at",
            Uuid::new_v4()
        ))
        .await
        .unwrap();
}

/// Everything a refusal must leave alone: state, version, dwell clock, the tolerance flag, the
/// fulfillment generation, version rows, the order's *active* overlap claims, grants and consent
/// rows. The claim term is meaningful only for an order that still holds its claim
/// ([`approved_holding`]); the matrix releases every claim (each order of a world claims the same
/// tuple), so there it is `0` before and after and checks nothing — the refusal tests that keep
/// the claim are where a claim change would show.
async fn footprint(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(o.state, o.current_version, o.state_entered_at, \
             o.authorization_failure_tolerated_at, o.fulfillment_control_generation, \
             (SELECT count(*) FROM bss_orders__order_version v WHERE v.order_id=o.order_id), \
             (SELECT count(*) FROM bss_orders__inflight_overlap_claim c WHERE c.order_id=o.order_id \
             AND c.released_at IS NULL), \
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
pub(super) async fn approved(w: &World, tag: &str, consent: bool) -> (Uuid, i64) {
    let (order, n) = approved_holding(w, tag, consent).await;
    w.release(order).await;
    (order, n)
}

/// As [`approved`], but the order keeps its active overlap claim: the caller releases it
/// before the world's next order submits.
async fn approved_holding(w: &World, tag: &str, consent: bool) -> (Uuid, i64) {
    let (order, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), tag)
        .await;
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
    Admitted {
        tolerated: bool,
    },
    Refused(Reason),
    /// The settled 503 `acceptance-requirement-unevaluable` of an unreadable tolerance election.
    Unevaluable,
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
        (Authorization::Failed, Tolerance::Outage) => Expect::Unevaluable,
        (Authorization::Failed, _) => Expect::Refused(Reason::AuthorizationFailed),
    }
}

fn expected(consent: Consent, outcome: Authorization, tolerance: Tolerance) -> Expect {
    match (consent, outcome, tolerance) {
        // A tolerance outage is the payment guard's unresolvable input, so the consent guard,
        // registered first, answers before it (D-113, D-256 item 4).
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
/// refuses `authorization-failed` otherwise. An outage is the payment guard's unresolvable input:
/// the settled, audited 503 `acceptance-requirement-unevaluable` after the consent guard, never
/// "not tolerated" and never "tolerated"; authorized and pending never read the election, so the
/// outage cannot touch them. Every refusal leaves the order `approved` with no grant, version,
/// consent or event change, and is audited.
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
            Expect::Unevaluable => {
                let out = answer.unwrap();
                assert!(
                    matches!(
                        out.kind,
                        OutcomeKind::Refused {
                            reason: Reason::AcceptanceRequirementUnevaluable,
                            ..
                        }
                    ),
                    "{cell}: {out:?}"
                );
                assert_eq!(out.response.status(), 503, "{cell}");
                assert_eq!(
                    footprint(&w, order).await,
                    before,
                    "{cell}: nothing changes"
                );
                assert_eq!(w.f.t.outbox().await, outbox, "{cell}");
                assert_eq!(w.f.t.audits(order).await, audits + 1, "{cell}: audited");
            }
        }
    }
}

/// An unavailable requirement is the unresolvable input `acceptance-requirement-unevaluable`
/// (503, settled), for the contract port and for the policy store, and the tolerance election
/// is never read. The order stays `approved` and the same attempt admits once the input
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

/// The last audit entry of `order`'s begin-fulfillment attempts: `[outcome, reason, to_state]`.
async fn last_begin(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(outcome, reason, to_state) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' \
             AND trigger='begin-fulfillment' ORDER BY sequence DESC LIMIT 1"
        ))
        .await
}

/// D-256 item 4 (05 §3.3 `inst-bg-resolve-requirement`): the tolerance election of a `failed`
/// outcome is resolved before the transaction, and its outage reaches the engine as the payment
/// guard's unresolvable input. Consent required and missing is therefore refused
/// `acceptance-required-not-recorded` first; with consent in place the outage is the settled,
/// audited 503 `acceptance-requirement-unevaluable` with no state change. `authorized` and
/// `pending` never read the election, so the outage leaves them unaffected. Each order keeps its
/// overlap claim while it is refused, so the footprint's claim term is live here.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_tolerance_outage_is_unevaluable_after_the_consent_guard() {
    let w = world(contract_fixture(Some(true))).await;
    // No policy store: any election read fails. The contract is stated, so the requirement
    // itself never reads an election.
    let down = resolver(&w, Some(true), false);

    // (a) Consent required and missing: the consent guard answers first.
    let (missing, n) = approved_holding(&w, "bf-tol-a", false).await;
    let before = footprint(&w, missing).await;
    let audits = w.f.t.audits(missing).await;
    let out = begin(&w, &down, missing, n, Authorization::Failed, "bf-tol-a-1")
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
    assert_eq!(out.response.status(), 400);
    assert_eq!(footprint(&w, missing).await, before);
    assert_eq!(w.f.t.audits(missing).await, audits + 1);
    w.release(missing).await;

    // (b) Consent in place: the settled, audited 503.
    let (fine, n) = approved_holding(&w, "bf-tol-b", true).await;
    let before = footprint(&w, fine).await;
    let (outbox, audits) = (w.f.t.outbox().await, w.f.t.audits(fine).await);
    let out = begin(&w, &down, fine, n, Authorization::Failed, "bf-tol-b-1")
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
    assert_eq!(
        footprint(&w, fine).await,
        before,
        "no state or claim change"
    );
    assert_eq!(w.f.t.outbox().await, outbox);
    assert_eq!(w.f.t.audits(fine).await, audits + 1, "audited");
    assert_eq!(
        last_begin(&w, fine).await,
        json!(["refused", "acceptance-requirement-unevaluable", "approved"])
    );

    // (c) Authorized and pending never read the election.
    let out = begin(&w, &down, fine, n, Authorization::Pending, "bf-tol-b-2")
        .await
        .unwrap();
    assert!(
        matches!(
            out.kind,
            OutcomeKind::Refused {
                reason: Reason::AuthorizationPending,
                ..
            }
        ),
        "{out:?}"
    );
    let out = begin(&w, &down, fine, n, Authorization::Authorized, "bf-tol-b-3")
        .await
        .unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
}

/// The requirement-source axis of row 11 on orders that reference no contract, so the
/// elections decide: the unset default (required, `platform_default`), an election-read failure
/// (the settled 503 `acceptance-requirement-unevaluable`, whatever the consent), the platform's
/// election, the seller's `false` over the platform's `true` (not required) and the seller's
/// `true`; each with consent missing and present. The outcome is `authorized`, so only the
/// consent guard decides. Cells run in election-row order (rows are never deleted).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_requirement_source_axis() {
    #[derive(Debug, Clone, Copy)]
    enum Axis {
        Unset,
        ReadFailure,
        Platform,
        SellerFalse,
        Seller,
    }
    let w = world(contract_fixture(Some(true))).await;
    for (i, source) in [
        Axis::Unset,
        Axis::ReadFailure,
        Axis::Platform,
        Axis::SellerFalse,
        Axis::Seller,
    ]
    .into_iter()
    .enumerate()
    {
        match source {
            Axis::Unset | Axis::ReadFailure => {}
            Axis::Platform => upsert(&w, "acceptance_required", None, true).await,
            Axis::SellerFalse => {
                upsert(&w, "acceptance_required", Some(u(SELLER)), false).await;
            }
            Axis::Seller => upsert(&w, "acceptance_required", Some(u(SELLER)), true).await,
        }
        let r = resolver_over(
            &w,
            Arc::new(ContractsUnavailable),
            !matches!(source, Axis::ReadFailure),
        );
        let resolved = r
            .requirement(
                workflow().ctx(),
                &RequirementSubject {
                    contract_id: None,
                    payer_tenant_id: u(RESOURCE),
                    seller_tenant_id: u(SELLER),
                },
                None,
                &RunBudget::new(),
                &CancellationToken::new(),
            )
            .await;
        let required = match (source, resolved) {
            (Axis::Unset, Ok(Requirement::Required(Source::PlatformDefault)))
            | (Axis::Platform, Ok(Requirement::Required(Source::PlatformDefault)))
            | (Axis::Seller, Ok(Requirement::Required(Source::Seller))) => Some(true),
            (Axis::SellerFalse, Ok(Requirement::NotRequired)) => Some(false),
            (Axis::ReadFailure, Err(PrepareError::Unavailable)) => None,
            other => panic!("{source:?}: {other:?}"),
        };
        for consent in [false, true] {
            let cell = format!("{source:?}/consent={consent}");
            let tag = format!("bf-src{i}-{consent}");
            let (order, n) = w.submitted(&partner(), &partner(), None, &tag).await;
            if consent {
                let settled = record(&w.recorder(), &customer(), order, n, &format!("{tag}-c"))
                    .await
                    .unwrap();
                assert_eq!(
                    settled.outcome,
                    SettledOutcome::Committed,
                    "{cell}: {settled:?}"
                );
            }
            approve(&w, order).await;
            let before = footprint(&w, order).await;
            let out = begin(
                &w,
                &r,
                order,
                n,
                Authorization::Authorized,
                &format!("{tag}-b"),
            )
            .await
            .unwrap();
            match (required, consent) {
                (None, _) => {
                    assert!(
                        matches!(
                            out.kind,
                            OutcomeKind::Refused {
                                reason: Reason::AcceptanceRequirementUnevaluable,
                                ..
                            }
                        ),
                        "{cell}: {out:?}"
                    );
                    assert_eq!(out.response.status(), 503, "{cell}");
                    assert_eq!(footprint(&w, order).await, before, "{cell}");
                }
                (Some(true), false) => {
                    assert!(
                        matches!(
                            out.kind,
                            OutcomeKind::Refused {
                                reason: Reason::AcceptanceRequiredNotRecorded,
                                ..
                            }
                        ),
                        "{cell}: {out:?}"
                    );
                    assert_eq!(footprint(&w, order).await, before, "{cell}");
                }
                (Some(_), _) => {
                    assert!(
                        matches!(out.kind, OutcomeKind::Committed { .. }),
                        "{cell}: {out:?}"
                    );
                }
            }
            w.release(order).await;
        }
    }
}

/// A consent committed between begin-fulfillment's preparation and the engine's lock is seen:
/// the consent guard reads the locked current version (D-256 item 3), not a snapshot. The
/// customer records through the real record-acceptance service from inside the preparation's
/// live contract read (the `acceptance.rs` race hook), and begin then admits.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn consent_recorded_after_the_preparation_admits() {
    let w = Arc::new(world(contract_fixture(Some(true))).await);
    let (order, n) = approved(&w, "bf-race", false).await;
    let fired = Arc::new(AtomicBool::new(false));
    let before: Before = {
        let (w, fired) = (Arc::clone(&w), Arc::clone(&fired));
        Arc::new(move || {
            let (w, fired) = (Arc::clone(&w), Arc::clone(&fired));
            Box::pin(async move {
                if fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                let settled = record(&w.recorder(), &customer(), order, n, "bf-race-consent")
                    .await
                    .unwrap();
                assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
            })
        })
    };
    let hooked = Arc::new(Hooked {
        inner: Arc::new(ContractsStub::new(Arc::new(w.stubs.clone()))),
        before,
        calls: AtomicUsize::new(0),
    });
    let r = resolver_over(&w, hooked.clone(), true);
    let out = begin(&w, &r, order, n, Authorization::Authorized, "bf-race-begin")
        .await
        .unwrap();
    assert!(
        fired.load(Ordering::SeqCst),
        "the consent was recorded inside the preparation"
    );
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
    assert_eq!(flag(&w, order).await, json!(["in_fulfillment", null]));
}

/// A snapshot outside `approved` cannot be admitted, so the preparation reads nothing external
/// (as record-acceptance short-circuits): the engine refuses by precedence and the contract port
/// is never called, nor the tolerance election (no store is bound, and a failed outcome would
/// otherwise read it).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_snapshot_outside_approved_reads_nothing() {
    let w = world(contract_fixture(Some(true))).await;
    let (order, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), "bf-na")
        .await;
    let counting = Arc::new(Hooked {
        inner: Arc::new(ContractsStub::new(Arc::new(w.stubs.clone()))),
        before: Arc::new(|| Box::pin(async {})),
        calls: AtomicUsize::new(0),
    });
    let r = resolver_over(&w, counting.clone(), false);
    let before = footprint(&w, order).await;
    let out = begin(&w, &r, order, n, Authorization::Failed, "bf-na-1")
        .await
        .unwrap();
    assert!(
        matches!(
            out.kind,
            OutcomeKind::Refused {
                reason: Reason::NotAdmissible,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(counting.calls.load(Ordering::SeqCst), 0, "no external call");
    assert_eq!(footprint(&w, order).await, before);
}

/// The read-to-lock race (D-256 item 9, D-253 item 10): the snapshot is `pending_approval`, so
/// the preparation reads nothing, and Workflow's approval reflection (row 9, owner SQL as the
/// labelled double) commits `approved` at the same version before the lock. The engine reaches
/// the guards of a preparation that read nothing and aborts unsettled; the driver runs once
/// more from a fresh snapshot, which reads the requirement and admits on the recorded consent.
/// Never a settled `acceptance-requirement-unevaluable`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn approval_reflected_between_snapshot_and_lock_begins_after_one_more_pass() {
    let w = world(contract_fixture(Some(true))).await;
    let (order, n) = approved(&w, "bf-lock", true).await;
    force(&w.f, order, "state = 'pending_approval'").await;
    let hook = BeforeLock::new(sql_work(&w.f, "state = 'approved'".to_owned(), order));
    let engine = workflow_engine(&w, order).with_fault(hook.clone());
    let r = resolver(&w, Some(true), true);
    let out = begin_on(
        &engine,
        &r,
        order,
        n,
        Authorization::Authorized,
        "bf-lock-1",
    )
    .await;
    assert_eq!(hook.passes(), 2, "one more pass after the race: {out:?}");
    let out = out.unwrap();
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
    assert_eq!(flag(&w, order).await, json!(["in_fulfillment", null]));
    let refused =
        w.f.t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__transition_audit WHERE order_id='{order}' \
             AND trigger='begin-fulfillment' AND outcome <> 'committed'"
            ))
            .await;
    assert_eq!(refused, 0, "no refusal was settled or audited");
}
