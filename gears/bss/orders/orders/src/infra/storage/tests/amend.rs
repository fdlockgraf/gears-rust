//! Amendment on real PostgreSQL (S4-03/S4-04; 04 §3.6 *Append Amendment*; Stage 4 DESIGN §3):
//! the amendment service over the submit fixture (rules PDP, authorized reads, the gate
//! orchestrator over the dev stubs, Pricing's development double behind the scripted wrapper).
//! A submitted order at version N is amended by a delta: the shared assessment runs over every
//! proposed line, the D-188 attempt reserves candidate M and phase C appends M through the
//! engine (`supersedes_version = N`, reason `amendment`, one `OrderAmended`). Compiled only with
//! `gate-dev-stubs`.
use super::capture::{buyer, key, s};
use super::engine::fixed;
use super::submit::{
    FailAt, FixedBindings, Fixture, ORDER_RT, RESOURCE, SELLER, WORKFLOW, acceptances, attempts,
    committed, draft_tagged, draft_tagged_items, high_water, lapse_leases, last_event,
    multikey_fixture, rebind, service_over, setup_with, setup_with_bindings, submit, submit_with,
    transition_instant, two_line_fixture, versions,
};
use super::*;
use crate::authz::Caller;
use crate::authz::test_pdp::{RulesProvider, service};
use crate::domain::contributions::AggregateContribution;
use crate::domain::gate::ports::GatePort;
use crate::domain::state_table::RowId;
use crate::domain::transition::{GuardBindings, GuardSubject, GuardVerdict, InputState, Inputs};
use crate::infra::amend::AmendService;
use crate::infra::capture::{Settled, SettledOutcome};
use crate::infra::engine::FaultPoint;
use crate::infra::engine::documents::EventDetail;
use crate::infra::engine::{EngineOutcome, OutcomeKind, Prepared, Registries, TransitionRequest};
use crate::infra::events::payload::OrderApproved;
use crate::infra::gate::tests::orchestrate::PAYER;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, EvaluationRequest, EvaluationResponse};
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::authoring::OrderView;
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::models::{CallMeta, OrderVersion};
use bss_pricing_sdk::acceptance::CommercialReason;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::PlatformSecurityContext;

/// The amendment service over the fixture's engine, gate, dates, authority and Pricing double.
fn amend_service(f: &Fixture) -> AmendService {
    AmendService::new(Arc::new(rebind(f)))
}

fn amend_meta(text: &str, version: i64) -> CallMeta {
    CallMeta {
        expected_version: OrderVersion::try_from(version).unwrap(),
        idempotency_key: key(text),
        correlation_id: None,
        delegation_proof_ref: None,
    }
}

async fn amend_as(
    svc: &AmendService,
    caller: &Caller,
    order: Uuid,
    version: i64,
    request: Value,
    text: &str,
) -> Result<Settled, OrdersError> {
    let request: AmendOrder = serde_json::from_value(request).unwrap();
    svc.amend(
        caller,
        order,
        request,
        &amend_meta(text, version),
        &CancellationToken::new(),
    )
    .await
}

async fn amend(
    f: &Fixture,
    order: Uuid,
    version: i64,
    request: Value,
    text: &str,
) -> Result<Settled, OrdersError> {
    amend_as(&amend_service(f), &buyer(), order, version, request, text).await
}

/// A submitted order of `lines` lines: the draft view (its line ids are N's) and N.
async fn submitted(f: &Fixture, lines: usize, tag: &str) -> (OrderView, i64) {
    let v = draft_tagged(f, lines, tag).await;
    let settled = submit(f, &buyer(), &v, &format!("{tag}-submit"))
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let n = settled.response.body()["version"].as_i64().unwrap();
    (v, n)
}

/// A delta changing line `line`'s quantity to `quantity`.
fn change_quantity(line: Uuid, quantity: &str) -> Value {
    json!({"reason": "bigger plan", "lines": [{"op": "change", "line_id": line,
        "patch": {"selected_items": [{"item_id": crate::infra::gate::tests::orchestrate::ITEM,
            "quantity": quantity}]}}]})
}

/// Every external call the fixture counts: gate ports, catalog reads, date preparations,
/// principal lookups and Pricing commands.
fn external_calls(f: &Fixture) -> usize {
    GatePort::ALL
        .iter()
        .map(|p| f.h.calls.of(*p))
        .sum::<usize>()
        + *f.h.calls.read_lines.lock()
        + f.dates.prepared.load(Ordering::SeqCst)
        + f.authority.calls.load(Ordering::SeqCst)
        + f.acceptance.calls()
}

async fn order_row(f: &Fixture, order: Uuid) -> Value {
    f.t.json(&format!(
        "SELECT to_jsonb(o) AS j FROM bss_orders__order o WHERE order_id='{order}'"
    ))
    .await
}

/// Every row that belongs to committed version `version` of `order`: the version row, its
/// lines and totals, ordered, as JSON (byte-identical comparisons).
async fn version_rows(f: &Fixture, order: Uuid, version: i64) -> Value {
    f.t.json(&format!(
        "SELECT jsonb_build_array( \
           (SELECT to_jsonb(v) FROM bss_orders__order_version v WHERE order_id='{order}' AND version={version}), \
           (SELECT coalesce(jsonb_agg(to_jsonb(l) ORDER BY line_id), '[]') FROM bss_orders__order_line l WHERE order_id='{order}' AND version={version}), \
           (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY scope, line_id, charge_kind), '[]') FROM bss_orders__resolved_total t WHERE order_id='{order}' AND version={version}), \
           (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY accepted_version), '[]') FROM bss_orders__acceptance a WHERE order_id='{order}')) AS j"
    ))
    .await
}

/// Live claims of `order` (the claim set N holds).
async fn live_claims(f: &Fixture, order: Uuid) -> Value {
    f.t.json(&format!(
        "SELECT coalesce(jsonb_agg(to_jsonb(c) ORDER BY claim_id), '[]') AS j \
         FROM bss_orders__inflight_overlap_claim c WHERE order_id='{order}' AND released_at IS NULL"
    ))
    .await
}

/// Set an aggregate column directly (a state the fixture cannot reach through delivered paths).
async fn force(f: &Fixture, order: Uuid, assignment: &str) {
    f.t.env
        .pg
        .sql(&format!(
            "ALTER TABLE bss_orders__order DISABLE TRIGGER USER; \
             UPDATE bss_orders__order SET {assignment} WHERE order_id='{order}'; \
             ALTER TABLE bss_orders__order ENABLE TRIGGER USER;"
        ))
        .await
        .unwrap();
}

fn workflow() -> Caller {
    service(WORKFLOW.0, WORKFLOW.1)
}

/// A Workflow approval reflection of `order` at `version` (row 7, 8 or 9) with the row's guards
/// bound passing (the S5-03 owner's predicates are not this package's).
async fn reflect(
    f: &Fixture,
    order: Uuid,
    row: u8,
    trigger: Trigger,
    version: i64,
    text: &str,
) -> EngineOutcome {
    let registries = Registries::compile().unwrap();
    let guards = registries.guards.row_guards(RowId::of(row));
    let mut p = Prepared::new(AggregateContribution::None);
    p.guards = guards.iter().fold(GuardBindings::new(), |b, g| {
        b.bind(g.name, |_: &GuardSubject<'_>| GuardVerdict::Pass)
    });
    p.inputs = registries
        .guards
        .row_inputs(RowId::of(row))
        .into_iter()
        .fold(Inputs::new(), |i, port| i.with(port, InputState::Resolved));
    if matches!(row, 8 | 9) {
        p.event = Some(Arc::new(EventDetail(OrderApproved {
            deciding_authority: "approver-1".into(),
            approved_version: i32::try_from(version).unwrap(),
        })));
    }
    // Workflow decisions carry a finite order-ID path restriction (D-182 seam policy).
    let policy = json!({"vendor": "constructorfabric", "priority": 10, "policy_revision": "s4-03",
    "rules": [{
        "id": "workflow", "subject": {"id": s(Uuid::from_u128(WORKFLOW.0))},
        "resource_type": ORDER_RT, "actions": ["read", "approval_reflection"],
        "paths": [{"predicates": [
            {"property": "id", "values": [s(order)]},
            {"property": "seller_tenant_id", "values": [s(Uuid::from_u128(SELLER))]}]}],
    }]});
    f.t.engine(RulesProvider::from_policy(policy))
        .transition(
            &workflow(),
            TransitionRequest {
                order_id: order,
                trigger,
                operation: "reflect_approval",
                expected_version: OrderVersion::try_from(version).unwrap(),
                expected_draft_revision: None,
                document: json!({"verdict": text}),
                proposed: None,
                caller_reason: None,
                idempotency_key: key(text),
                correlation_id: None,
                execution: None,
            },
            &fixed(p),
        )
        .await
        .unwrap()
}

/// S4-04: an amendment of a `submitted` order commits M superseding N with the explanation
/// stored on the version and the audit entry, counts once, keeps the state and its entry time,
/// moves the claims to M and emits exactly one `OrderAmended { supersedesVersion: N }`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amend_submitted_order_commits_m_superseding_n() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 2, "am-commit").await;
    let order = v.order.order_id;
    let before = order_row(&f, order).await;
    let claims_n = live_claims(&f, order).await;
    let m = high_water(&f, order).await + 1;
    let outbox = f.t.outbox().await;
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-commit-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert!(m > n);
    assert_eq!(settled.response.body()["version"], json!(m));
    assert_eq!(settled.response.body()["state"], json!("submitted"));
    let after = order_row(&f, order).await;
    assert_eq!(after["current_version"], json!(m));
    assert_eq!(after["state"], json!("submitted"));
    assert_eq!(after["amendment_count"], json!(1));
    assert_eq!(after["state_entered_at"], before["state_entered_at"]);
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_build_array(supersedes_version, reason, amendment_reason) AS j \
             FROM bss_orders__order_version WHERE order_id='{order}' AND version={m}"
        ))
        .await,
        json!([n, "amendment", "bigger plan"])
    );
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_agg(jsonb_build_array(caller_reason, version)) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' AND trigger='amendment'"
        ))
        .await,
        json!([["bigger plan", m]])
    );
    // M's lines: both proposed lines, pinned at M; the changed quantity is M's.
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_line WHERE order_id='{order}' \
             AND version={m} AND order_pin IS NOT NULL"
        ))
        .await,
        2
    );
    assert_eq!(
        f.t.json(&format!(
            "SELECT selected_items->0->'quantity' AS j FROM bss_orders__order_line \
             WHERE order_id='{order}' AND version={m} AND line_id='{}'",
            v.lines[0].line_id
        ))
        .await,
        json!("2")
    );
    // The complete proposed claim set is live: the same (payer, resource, key) tuple is
    // retained, never released and re-taken.
    assert_eq!(live_claims(&f, order).await, claims_n);
    assert_eq!(claims_n.as_array().unwrap().len(), 1);
    assert_eq!(f.t.outbox().await, outbox + 1, "exactly one OrderAmended");
    let event = last_event(&f).await;
    assert_eq!(event["supersedesVersion"], json!(n), "{event}");
    assert_eq!(event["orderVersion"], json!(m));
    assert_eq!(event["state"], json!("submitted"));
    assert_eq!(attempts(&f, order).await, 2);
    // The reads compose M.
    let read = f.t.reads().get(&buyer(), order).await.unwrap();
    assert_eq!(i64::from(read.version.version), m);
    assert_eq!(read.version.supersedes_version.map(i64::from), Some(n));
}

/// Rows 19/20: an amendment of an `approved` order targets `submitted`, and the state change
/// moves `state_entered_at` to the engine's transition time.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amend_from_approved_targets_submitted_and_updates_state_entered_at() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-approved").await;
    let order = v.order.order_id;
    let approved = reflect(
        &f,
        order,
        8,
        Trigger::ReflectApprovalNotRequired,
        n,
        "am-approved-r",
    )
    .await;
    assert!(
        matches!(approved.kind, OutcomeKind::Committed { .. }),
        "{approved:?}"
    );
    assert_eq!(order_row(&f, order).await["state"], json!("approved"));
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "3"),
        "am-approved-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let after = order_row(&f, order).await;
    assert_eq!(after["state"], json!("submitted"));
    // `transition_instant` asserts the order's entry time is M's creation instant.
    transition_instant(&f, order, m).await;
    let event = last_event(&f).await;
    assert_eq!(event["supersedesVersion"], json!(n), "{event}");
    assert_eq!(event["orderVersion"], json!(m));
}

/// 04 §3.6 step 5: the shared assessment runs over every proposed line, the untouched one
/// included: each line is commanded at M under its own key.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amend_reassesses_untouched_lines() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 2, "am-all").await;
    let order = v.order.order_id;
    let commands = f.acceptance.calls();
    let reads = *f.h.calls.read_lines.lock();
    let m = high_water(&f, order).await + 1;
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-all-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(f.acceptance.calls(), commands + 2);
    let untouched = v.lines[1].line_id;
    assert!(
        f.acceptance
            .keys()
            .contains(&format!("{order}/{m}/{untouched}/submit")),
        "{:?}",
        f.acceptance.keys()
    );
    assert_eq!(*f.h.calls.read_lines.lock(), reads + 1, "one catalog call");
    // M's diagnostics name both lines.
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(DISTINCT line_id) AS n FROM bss_orders__gate_outcome \
             WHERE order_id='{order}' AND version={m} AND line_id IS NOT NULL"
        ))
        .await,
        2
    );
}

/// Nothing from N is evidence for M: N's consent stays N's, and M has none.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn new_version_has_no_consent() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-consent").await;
    let order = v.order.order_id;
    let consent_n = acceptances(&f, order).await;
    assert_eq!(consent_n[0]["version"], json!(n), "{consent_n}");
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-consent-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(acceptances(&f, order).await, consent_n);
}

/// D-215/D-228: a payer change is assessed by gate predicate 1 like a submit; a payer outside
/// the resource relationship refuses `payer-relationship-unconfirmed`, audited and settled, and
/// N stays current.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn payer_change_without_resource_relationship_refuses_payer_relationship_unconfirmed() {
    let other = Uuid::from_u128(31);
    let mut stubs = two_line_fixture();
    let mut profile = stubs.payer_profiles[&PAYER].clone();
    profile.in_relationship_with_resource = false;
    stubs.payer_profiles.insert(other, profile);
    let f = setup_with(stubs, 10, true).await;
    let (v, n) = submitted(&f, 1, "am-payer").await;
    let order = v.order.order_id;
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "new payer", "header": {"payer_tenant_id": other}}),
        "am-payer-1",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::PayerRelationshipUnconfirmed)),
        "{settled:?}"
    );
    let row = order_row(&f, order).await;
    assert_eq!(row["current_version"], json!(n));
    assert_eq!(row["payer_tenant_id"], json!(PAYER));
    assert_eq!(row["amendment_count"], json!(0));
}

/// 04 §4.1, D-113: at the cap the amendment refuses `amendment-cap-exhausted` before any
/// external call: the gate inputs are precluded, never unavailable.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cap_exhausted_makes_no_external_calls() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-cap").await;
    let order = v.order.order_id;
    force(&f, order, "amendment_count = 20").await;
    let calls = external_calls(&f);
    let high = high_water(&f, order).await;
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-cap-1",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentCapExhausted)),
        "{settled:?}"
    );
    assert_eq!(external_calls(&f), calls);
    assert_eq!(high_water(&f, order).await, high, "no candidate reserved");
    // Every other early local guard precludes the gate the same way (an empty delta here).
    let settled = amend(&f, order, n, json!({"reason": "nothing"}), "am-cap-2")
        .await
        .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentCapExhausted))
    );
    force(&f, order, "amendment_count = 0").await;
    let settled = amend(&f, order, n, json!({"reason": "nothing"}), "am-cap-3")
        .await
        .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentEmpty))
    );
    let unknown = Uuid::from_u128(0xdead);
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "r", "lines": [{"op": "remove", "line_id": unknown}]}),
        "am-cap-4",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::LineNotFound))
    );
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "r", "lines": [{"op": "remove", "line_id": v.lines[0].line_id}]}),
        "am-cap-5",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::NoLines))
    );
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2")
            .as_object()
            .map(|o| {
                let mut o = o.clone();
                o.insert("reason".into(), json!(""));
                Value::Object(o)
            })
            .unwrap(),
        "am-cap-6",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentReasonInvalid))
    );
    assert_eq!(external_calls(&f), calls);
    assert_eq!(high_water(&f, order).await, high);
}

/// D-212: a stale well-formed `If-Match` is the audited, settled `precondition-failed` naming
/// the current version, with no external call.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stale_if_match_is_412() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-stale").await;
    let order = v.order.order_id;
    let calls = external_calls(&f);
    let settled = amend(
        &f,
        order,
        n - 1,
        change_quantity(v.lines[0].line_id, "2"),
        "am-stale-1",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::PreconditionFailed)),
        "{settled:?}"
    );
    assert_eq!(settled.response.status(), 412);
    assert_eq!(
        settled.response.body()["context"]["data"]["current_version"],
        json!(n)
    );
    assert_eq!(external_calls(&f), calls);
}

/// D-188 E2: the same key replays the settled amendment before any read, date, gate or Pricing
/// work.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn exact_retry_makes_no_fresh_resolution() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-retry").await;
    let order = v.order.order_id;
    let request = change_quantity(v.lines[0].line_id, "2");
    let first = amend(&f, order, n, request.clone(), "am-retry-1")
        .await
        .unwrap();
    assert_eq!(first.outcome, SettledOutcome::Committed, "{first:?}");
    let calls = external_calls(&f);
    let outbox = f.t.outbox().await;
    let replay = amend(&f, order, n, request, "am-retry-1").await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.response, first.response);
    assert_eq!(external_calls(&f), calls);
    assert_eq!(f.t.outbox().await, outbox);
}

/// U8 / 04 §3.6 step 1: amendment from `in_fulfillment` is a state-admissibility refusal, even
/// with the cap exhausted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amend_in_fulfillment_is_not_admissible() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-fulfil").await;
    let order = v.order.order_id;
    force(&f, order, "state = 'in_fulfillment', amendment_count = 20").await;
    let calls = external_calls(&f);
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-fulfil-1",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::NotAdmissible)),
        "{settled:?}"
    );
    assert_eq!(
        settled.response.body()["context"]["data"]["state"],
        json!("in_fulfillment")
    );
    assert_eq!(external_calls(&f), calls);
}

/// Sparse versions (D-188): two burned submit candidates and two burned amendment candidates
/// leave the committed history 1 → 4 → 7.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sparse_versions_1_4_7() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let v = draft_tagged(&f, 1, "am-sparse").await;
    let order = v.order.order_id;
    let line = v.lines[0].line_id;
    f.acceptance
        .refuse
        .lock()
        .insert(line, CommercialReason::NotSellable);
    for k in ["am-sparse-s1", "am-sparse-s2"] {
        let refused = submit(&f, &buyer(), &v, k).await.unwrap();
        assert_eq!(
            refused.outcome,
            SettledOutcome::Refused(Some(Reason::CatalogPredicateFailed))
        );
    }
    f.acceptance.refuse.lock().clear();
    let settled = submit(&f, &buyer(), &v, "am-sparse-s3").await.unwrap();
    assert_eq!(settled.response.body()["version"], json!(4));
    f.acceptance
        .refuse
        .lock()
        .insert(line, CommercialReason::NotSellable);
    for k in ["am-sparse-a1", "am-sparse-a2"] {
        let refused = amend(&f, order, 4, change_quantity(line, "2"), k)
            .await
            .unwrap();
        assert_eq!(
            refused.outcome,
            SettledOutcome::Refused(Some(Reason::CatalogPredicateFailed)),
            "{refused:?}"
        );
    }
    f.acceptance.refuse.lock().clear();
    let settled = amend(&f, order, 4, change_quantity(line, "2"), "am-sparse-a3")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(settled.response.body()["version"], json!(7));
    assert_eq!(
        versions(&f, order).await,
        json!([[1, null], [4, 1], [7, 4]])
    );
    // Failed attempts do not count toward the cap.
    assert_eq!(order_row(&f, order).await["amendment_count"], json!(1));
}

/// A failed amendment attempt burns its candidate and leaves every row of N, N's claims and the
/// aggregate's commercial facts exactly as they were.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn failed_attempt_leaves_n_byte_identical_and_burns_the_candidate() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 2, "am-burn").await;
    let order = v.order.order_id;
    let rows_n = version_rows(&f, order, n).await;
    let claims_n = live_claims(&f, order).await;
    let before = order_row(&f, order).await;
    let high = high_water(&f, order).await;
    let (versions_n, ..) = committed(&f, order).await;
    f.acceptance
        .refuse
        .lock()
        .insert(v.lines[1].line_id, CommercialReason::NotSellable);
    let refused = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-burn-1",
    )
    .await
    .unwrap();
    assert_eq!(
        refused.outcome,
        SettledOutcome::Refused(Some(Reason::CatalogPredicateFailed)),
        "{refused:?}"
    );
    assert_eq!(version_rows(&f, order, n).await, rows_n);
    assert_eq!(live_claims(&f, order).await, claims_n);
    assert_eq!(committed(&f, order).await.0, versions_n);
    let after = order_row(&f, order).await;
    assert_eq!(
        high_water(&f, order).await,
        high + 1,
        "the candidate burned"
    );
    for column in [
        "current_version",
        "state",
        "state_entered_at",
        "amendment_count",
        "payer_tenant_id",
        "category",
        "contract_id",
    ] {
        assert_eq!(after[column], before[column], "{column}");
    }
}

/// A lost Pricing reply leaves the attempt unsettled; the same-key retry continues under the
/// frozen inputs (no second gate run) and commits; a lost final response replays exactly.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lost_response_replays_exactly() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-lost").await;
    let order = v.order.order_id;
    let request = change_quantity(v.lines[0].line_id, "2");
    // The first command and its one retry both lose their reply.
    f.acceptance.lose_replies.store(2, Ordering::SeqCst);
    let lost = amend(&f, order, n, request.clone(), "am-lost-1")
        .await
        .unwrap_err();
    assert!(matches!(lost, OrdersError::Unavailable), "{lost:?}");
    let reads = *f.h.calls.read_lines.lock();
    let prepared = f.dates.prepared.load(Ordering::SeqCst);
    lapse_leases(&f, order).await;
    let settled = amend(&f, order, n, request.clone(), "am-lost-1")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(*f.h.calls.read_lines.lock(), reads, "no fresh gate run");
    assert_eq!(f.dates.prepared.load(Ordering::SeqCst), prepared);
    let calls = external_calls(&f);
    let replay = amend(&f, order, n, request, "am-lost-1").await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.response, settled.response);
    assert_eq!(external_calls(&f), calls);
}

/// Stage 4 DESIGN §3: a Workflow result naming N after M exists is `version-conflict` naming
/// M, decided before Workflow-class admissibility and never converted to a denial.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stale_workflow_result_naming_n_is_version_conflict_with_m() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-wf").await;
    let order = v.order.order_id;
    let required = reflect(&f, order, 7, Trigger::ReflectApprovalRequired, n, "am-wf-r").await;
    assert!(
        matches!(required.kind, OutcomeKind::Committed { .. }),
        "{required:?}"
    );
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-wf-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    // The verdict on N arrives after M exists (the order is `submitted` again).
    let stale = reflect(&f, order, 9, Trigger::ReflectApprovalGranted, n, "am-wf-g").await;
    let OutcomeKind::Refused { reason, .. } = stale.kind else {
        panic!("{stale:?}")
    };
    assert_eq!(reason, Reason::VersionConflict);
    assert_eq!(stale.response.status(), 409);
    assert_eq!(
        stale.response.body()["context"]["data"]["current_version"],
        json!(m)
    );
    let row = order_row(&f, order).await;
    assert_eq!(row["state"], json!("submitted"));
    assert_eq!(row["current_version"], json!(m));
}

/// E1 (D-188 step 2): a new amendment execution rechecks, under the reservation lock, that the
/// amendment is admissible from the locked state and that the locked version is the caller's
/// expected one; a stale or inadmissible request reserves no candidate and creates no attempt.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amendment_reservation_rechecks_state_and_version_under_the_lock() {
    use crate::domain::error::EngineError;
    use crate::infra::storage::repo::idempotency::AttemptInputs;
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-e1").await;
    let order = v.order.order_id;
    let request = |version: i64, text: &str| TransitionRequest {
        order_id: order,
        trigger: Trigger::Amendment,
        operation: "amend",
        expected_version: OrderVersion::try_from(version).unwrap(),
        expected_draft_revision: None,
        document: json!({"e1": text}),
        proposed: None,
        caller_reason: Some("r".into()),
        idempotency_key: key(text),
        correlation_id: None,
        execution: None,
    };
    let inputs = || AttemptInputs {
        prepared_draft_revision: None,
        proposed_arrangement: json!({}),
        authorization_fact_fingerprint: "facts".into(),
        original_principal: json!({}),
        proof_reference: None,
        date_policy_basis: json!({}),
        commercial_subject_id: Uuid::from_u128(1),
        commercial_subject_type: "user".into(),
        commercial_subject_tenant_id: Uuid::from_u128(2),
    };
    let attempts_before = attempts(&f, order).await;
    let high = high_water(&f, order).await;
    let stale = f
        .engine
        .prepare_commercial_attempt(&buyer(), &request(n - 1, "am-e1-1"), inputs(), |_| {
            Ok(json!({}))
        })
        .await;
    assert!(
        matches!(stale, Err(EngineError::Refused(Reason::PreconditionFailed))),
        "{stale:?}"
    );
    force(&f, order, "state = 'in_fulfillment'").await;
    let inadmissible = f
        .engine
        .prepare_commercial_attempt(&buyer(), &request(n, "am-e1-2"), inputs(), |_| {
            Ok(json!({}))
        })
        .await;
    assert!(
        matches!(
            inadmissible,
            Err(EngineError::Refused(Reason::NotAdmissible))
        ),
        "{inadmissible:?}"
    );
    assert_eq!(high_water(&f, order).await, high);
    assert_eq!(attempts(&f, order).await, attempts_before);
}

/// A payer the resource relationship confirms: M is committed under the proposed arrangement
/// (the PEP-authorized payer on the aggregate and the version), and the complete claim set moves
/// to the new payer's tuple while N's tuple becomes released history.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn amend_changes_the_payer_and_replaces_the_claim_tuple() {
    let other = Uuid::from_u128(31);
    let mut stubs = two_line_fixture();
    let profile = stubs.payer_profiles[&PAYER].clone();
    stubs.payer_profiles.insert(other, profile);
    let f = setup_with(stubs, 10, true).await;
    let (v, n) = submitted(&f, 1, "am-payer-ok").await;
    let order = v.order.order_id;
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "new payer", "header": {"payer_tenant_id": other}}),
        "am-payer-ok-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    assert_eq!(order_row(&f, order).await["payer_tenant_id"], json!(other));
    assert_eq!(
        f.t.json(&format!(
            "SELECT to_jsonb(payer_tenant_id) AS j FROM bss_orders__order_version \
             WHERE order_id='{order}' AND version={m}"
        ))
        .await,
        json!(other)
    );
    let live = live_claims(&f, order).await;
    assert_eq!(live.as_array().unwrap().len(), 1, "{live}");
    assert_eq!(live[0]["payer_tenant_id"], json!(other));
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__inflight_overlap_claim WHERE order_id='{order}' \
             AND payer_tenant_id='{PAYER}' AND released_at IS NOT NULL"
        ))
        .await,
        1
    );
    // The reads compose M under the new payer.
    let read = f.t.reads().get(&buyer(), order).await.unwrap();
    assert_eq!(read.order.payer_tenant_id, other);
}

/// An added line gets a new identity reserved at M's transition, and a recovered attempt
/// re-merges the same delta with the attempt's own added id (never a fresh one), so the commit
/// admits exactly the frozen lines; the retained line keeps its id.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn added_line_identity_survives_recovery() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-add").await;
    let order = v.order.order_id;
    let retained = v.lines[0].line_id;
    let add = serde_json::to_value(super::submit::line()).unwrap();
    let request = json!({"reason": "one more", "lines": [{"op": "add", "line": add}]});
    f.acceptance.lose_replies.store(2, Ordering::SeqCst);
    let lost = amend(&f, order, n, request.clone(), "am-add-1")
        .await
        .unwrap_err();
    assert!(matches!(lost, OrdersError::Unavailable), "{lost:?}");
    let frozen: Vec<Uuid> =
        f.t.json(&format!(
            "SELECT to_jsonb(array(SELECT k FROM jsonb_object_keys(line_requests) k \
             WHERE k <> 'order' ORDER BY k)) AS j FROM bss_orders__commercial_attempt \
             WHERE order_id='{order}' AND status IN ('prepared','running')"
        ))
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(frozen.len(), 2);
    assert!(frozen.contains(&retained));
    let added = *frozen.iter().find(|id| **id != retained).unwrap();
    lapse_leases(&f, order).await;
    let settled = amend(&f, order, n, request, "am-add-1").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let mut lines: Vec<Uuid> =
        f.t.json(&format!(
            "SELECT jsonb_agg(line_id) AS j FROM bss_orders__order_line \
             WHERE order_id='{order}' AND version={m}"
        ))
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().parse().unwrap())
        .collect();
    lines.sort_unstable();
    let mut expected = vec![retained, added];
    expected.sort_unstable();
    assert_eq!(lines, expected);
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_line_identity \
             WHERE order_id='{order}' AND line_id='{added}'"
        ))
        .await,
        1
    );
    // N keeps its one line.
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_line WHERE order_id='{order}' AND version={n}"
        ))
        .await,
        1
    );
}

// ---------------------------------------------------------------------------------------------
// Races, stress and the remaining preclusion proofs (Task 9b)

/// A concurrent writer's work, run by a test hook.
type Work = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The fixture's rules PDP, running `work` once on the `nth` `amend` decision of the engine it
/// is bound to. The service's committed read uses the fixture's own PDP, so the `nth = 2`
/// decision (after the recovery probe's) is the engine's target authorization of the
/// transition: a concurrent writer between the service's read and the engine's lock, as the
/// engine tests' `side_effect` preparation is.
struct RaceAt {
    inner: Arc<dyn AuthZResolverApi>,
    nth: usize,
    seen: AtomicUsize,
    fired: AtomicUsize,
    work: Work,
}
#[async_trait]
impl AuthZResolverApi for RaceAt {
    async fn evaluate(
        &self,
        ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        if request.action.name == "amend"
            && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth
        {
            self.fired.fetch_add(1, Ordering::SeqCst);
            (self.work)().await;
        }
        self.inner.evaluate(ctx, request).await
    }
}

/// The amendment service over an engine whose second `amend` decision runs `work` first.
fn racing(f: &Fixture, work: Work) -> (AmendService, Arc<RaceAt>) {
    let pdp = Arc::new(RaceAt {
        inner: f.pdp(),
        nth: 2,
        seen: AtomicUsize::new(0),
        fired: AtomicUsize::new(0),
        work,
    });
    let api: Arc<dyn AuthZResolverApi> = pdp.clone();
    let engine = f.t.engine(api);
    (AmendService::new(Arc::new(service_over(f, engine))), pdp)
}

/// Owner SQL on the raw connection (triggers off, as [`force`] does), as racing work.
fn sql_work(f: &Fixture, assignment: String, order: Uuid) -> Work {
    let raw = f.t.env.pg.raw.clone();
    Arc::new(move || {
        let raw = raw.clone();
        let sql = format!(
            "ALTER TABLE bss_orders__order DISABLE TRIGGER USER; \
             UPDATE bss_orders__order SET {assignment} WHERE order_id='{order}'; \
             ALTER TABLE bss_orders__order ENABLE TRIGGER USER;"
        );
        Box::pin(async move {
            raw.execute_unprepared(&sql).await.unwrap();
        })
    })
}

/// Holds the `nth` Pricing command (counted from now) on its worker thread until released, so
/// the test can run a competing writer while that attempt is between reservation and commit.
struct Hold {
    reached: Arc<AtomicBool>,
    go: Arc<AtomicBool>,
}
impl Hold {
    fn on(f: &Fixture, nth: usize) -> Self {
        let base = f.acceptance.calls();
        let reached = Arc::new(AtomicBool::new(false));
        let go = Arc::new(AtomicBool::new(false));
        let (r, g) = (Arc::clone(&reached), Arc::clone(&go));
        *f.acceptance.on_call.lock() = Some(Arc::new(move |n| {
            if n != base + nth {
                return;
            }
            r.store(true, Ordering::SeqCst);
            // The worker hands its core off while it waits (multi-thread runtime only).
            tokio::task::block_in_place(|| {
                let start = Instant::now();
                while !g.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(60) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }));
        Self { reached, go }
    }
    async fn reached(&self) {
        let start = Instant::now();
        while !self.reached.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the held command was never reached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn release(&self) {
        self.go.store(true, Ordering::SeqCst);
    }
}

/// An amendment on its own task (its own pool connections), for a race.
fn spawn_amend(
    svc: &Arc<AmendService>,
    order: Uuid,
    version: i64,
    request: Value,
    text: &str,
) -> tokio::task::JoinHandle<Result<Settled, OrdersError>> {
    let (svc, text) = (Arc::clone(svc), text.to_owned());
    tokio::spawn(async move { amend_as(&svc, &buyer(), order, version, request, &text).await })
}

/// The race fix (D-253): an order read `on_hold` and resumed (row 22, same version) before the
/// engine locks it is decided from the locked state. The engine finds the amendment admissible
/// at N; the refusal-only preparation does not admit it, so the service runs once more from the
/// recovery probe and the amendment commits M, never an abort.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn on_hold_resumed_between_read_and_lock_is_decided_from_the_locked_state() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-hold").await;
    let order = v.order.order_id;
    force(&f, order, "state = 'on_hold', pre_hold_state = 'submitted'").await;
    let resume = sql_work(
        &f,
        "state = 'submitted', pre_hold_state = NULL".to_owned(),
        order,
    );
    let (svc, race) = racing(&f, resume);
    let outbox = f.t.outbox().await;
    let settled = amend_as(
        &svc,
        &buyer(),
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-hold-1",
    )
    .await
    .unwrap();
    assert_eq!(race.fired.load(Ordering::SeqCst), 1, "the resume raced");
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let row = order_row(&f, order).await;
    assert_eq!(row["state"], json!("submitted"));
    assert_eq!(row["current_version"], json!(m));
    assert_eq!(row["amendment_count"], json!(1));
    assert_eq!(f.t.outbox().await, outbox + 1, "exactly one OrderAmended");
    assert_eq!(last_event(&f).await["supersedesVersion"], json!(n));
    // An order still on hold at the lock is the settled state refusal, with no external call.
    force(&f, order, "state = 'on_hold', pre_hold_state = 'submitted'").await;
    let calls = external_calls(&f);
    let settled = amend(
        &f,
        order,
        m,
        change_quantity(v.lines[0].line_id, "3"),
        "am-hold-2",
    )
    .await
    .unwrap();
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(Reason::NotAdmissible)),
        "{settled:?}"
    );
    assert_eq!(
        settled.response.body()["context"]["data"]["state"],
        json!("on_hold")
    );
    assert_eq!(external_calls(&f), calls);
}

/// Two amendments from N, each on its own connection: the first reserves M1 and is held in its
/// Pricing command while the second reserves M2 and commits. The first's commit then finds the
/// order at M2 and settles `precondition-failed` (412 naming M2), burning M1. One winner: the
/// pointer, the predecessor chain and the claims are the winner's alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn amendment_races_amendment_one_winner() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-win").await;
    let order = v.order.order_id;
    let line = v.lines[0].line_id;
    let claims_n = live_claims(&f, order).await;
    let outbox = f.t.outbox().await;
    let svc = Arc::new(amend_service(&f));
    let hold = Hold::on(&f, 1);
    let first = spawn_amend(&svc, order, n, change_quantity(line, "2"), "am-win-1");
    hold.reached().await;
    let m1 = high_water(&f, order).await;
    assert_eq!(m1, n + 1, "the first reserved its candidate");
    let second = amend_as(
        &rebind_amend(&f),
        &buyer(),
        order,
        n,
        change_quantity(line, "3"),
        "am-win-2",
    )
    .await
    .unwrap();
    assert_eq!(second.outcome, SettledOutcome::Committed, "{second:?}");
    let m2 = second.response.body()["version"].as_i64().unwrap();
    assert_eq!(m2, n + 2);
    hold.release();
    let first = first.await.unwrap().unwrap();
    assert_eq!(
        first.outcome,
        SettledOutcome::Refused(Some(Reason::PreconditionFailed)),
        "{first:?}"
    );
    assert_eq!(first.response.status(), 412);
    assert_eq!(
        first.response.body()["context"]["data"]["current_version"],
        json!(m2)
    );
    let row = order_row(&f, order).await;
    assert_eq!(row["current_version"], json!(m2));
    assert_eq!(row["amendment_count"], json!(1));
    assert_eq!(high_water(&f, order).await, m2);
    // M2 supersedes N; M1 never became a version.
    let history = versions(&f, order).await;
    let history = history.as_array().unwrap();
    assert_eq!(history.last().unwrap(), &json!([m2, n]), "{history:?}");
    assert!(!history.iter().any(|v| v[0] == json!(m1)), "{history:?}");
    assert_eq!(
        f.t.json(&format!(
            "SELECT selected_items->0->'quantity' AS j FROM bss_orders__order_line \
             WHERE order_id='{order}' AND version={m2}"
        ))
        .await,
        json!("3")
    );
    // The one claim tuple stays the one live claim (retained by the winner).
    assert_eq!(live_claims(&f, order).await, claims_n);
    assert_eq!(
        f.t.outbox().await,
        outbox + 1,
        "only the winner's OrderAmended"
    );
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_agg(status ORDER BY candidate_version) AS j \
             FROM bss_orders__commercial_attempt WHERE order_id='{order}' AND operation='amendment'"
        ))
        .await,
        json!(["refused", "committed"])
    );
}

/// A second amendment service over the fixture (another process).
fn rebind_amend(f: &Fixture) -> AmendService {
    amend_service(f)
}

/// The consent path that exists (Task 6, S4-08a: a self-service submit records the customer's
/// consent on the version it commits) raced by an amendment naming that version, both ways:
/// (a) the amendment arrives while the consented submit is in its Pricing phase (the order is
/// still a draft at the lock): the settled `not-admissible`, and the submit's consent names its
/// version; (b) the amendment reads the draft, then the consented submit commits N before the
/// amendment's lock: the amendment is decided from the locked state (D-253 race fix) and
/// commits M, N's consent stays N's and M has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn amendment_races_consent() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    // (a) The amendment during the submit's Pricing command.
    let v = draft_tagged(&f, 1, "am-cons-a").await;
    let order = v.order.order_id;
    let n = high_water(&f, order).await + 1;
    let hold = Hold::on(&f, 1);
    let submitter = Arc::new(rebind(&f));
    let submitting = {
        let (svc, v) = (Arc::clone(&submitter), v.clone());
        tokio::spawn(async move { submit_with(&svc, &buyer(), &v, "am-cons-a-submit").await })
    };
    hold.reached().await;
    let refused = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-cons-a-1",
    )
    .await
    .unwrap();
    assert_eq!(
        refused.outcome,
        SettledOutcome::Refused(Some(Reason::NotAdmissible)),
        "{refused:?}"
    );
    assert_eq!(
        refused.response.body()["context"]["data"]["state"],
        json!("draft")
    );
    hold.release();
    let submitted = submitting.await.unwrap().unwrap();
    assert_eq!(
        submitted.outcome,
        SettledOutcome::Committed,
        "{submitted:?}"
    );
    let consent = acceptances(&f, order).await;
    assert_eq!(consent.as_array().unwrap().len(), 1, "{consent}");
    assert_eq!(consent[0]["version"], json!(n));
    assert_eq!(order_row(&f, order).await["current_version"], json!(n));
    *f.acceptance.on_call.lock() = None;

    // (b) The consented submit commits N between the amendment's read and its lock.
    // Leg (a)'s order leaves flight first: its claim would block this order's one tuple.
    f.t.env
        .pg
        .sql(&format!(
            "UPDATE bss_orders__inflight_overlap_claim SET released_at = now() \
             WHERE order_id='{order}' AND released_at IS NULL"
        ))
        .await
        .unwrap();
    let v = draft_tagged(&f, 1, "am-cons-b").await;
    let order = v.order.order_id;
    let n = high_water(&f, order).await + 1;
    let committed_n = Arc::new(AtomicBool::new(false));
    let work: Work = {
        let (svc, v, done) = (Arc::clone(&submitter), v.clone(), Arc::clone(&committed_n));
        Arc::new(move || {
            let (svc, v, done) = (Arc::clone(&svc), v.clone(), Arc::clone(&done));
            Box::pin(async move {
                let settled = submit_with(&svc, &buyer(), &v, "am-cons-b-submit")
                    .await
                    .unwrap();
                done.store(
                    settled.outcome == SettledOutcome::Committed,
                    Ordering::SeqCst,
                );
            })
        })
    };
    let (svc, race) = racing(&f, work);
    let outbox = f.t.outbox().await;
    let settled = amend_as(
        &svc,
        &buyer(),
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-cons-b-1",
    )
    .await
    .unwrap();
    assert_eq!(race.fired.load(Ordering::SeqCst), 1);
    assert!(committed_n.load(Ordering::SeqCst), "the submit committed N");
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    assert!(m > n);
    let consent = acceptances(&f, order).await;
    assert_eq!(consent.as_array().unwrap().len(), 1, "{consent}");
    assert_eq!(consent[0]["version"], json!(n), "N's consent stays N's");
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_build_array(supersedes_version, reason) AS j \
             FROM bss_orders__order_version WHERE order_id='{order}' AND version={m}"
        ))
        .await,
        json!([n, "amendment"])
    );
    assert_eq!(
        f.t.outbox().await,
        outbox + 2,
        "OrderSubmitted, OrderAmended"
    );
}

/// A Workflow approval reflection on N lands while an amendment from N is between reservation
/// and commit: the reflection commits (`approved` at N), and the amendment's commit is decided
/// from the locked `approved` state (row 20): M supersedes N, the order returns to `submitted`
/// with its entry time moved to M's transition, and both events are emitted in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn amendment_races_approval_reflection() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-refl").await;
    let order = v.order.order_id;
    let outbox = f.t.outbox().await;
    let svc = Arc::new(amend_service(&f));
    let hold = Hold::on(&f, 1);
    let amending = spawn_amend(
        &svc,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "am-refl-1",
    );
    hold.reached().await;
    let approved = reflect(
        &f,
        order,
        8,
        Trigger::ReflectApprovalNotRequired,
        n,
        "am-refl-r",
    )
    .await;
    assert!(
        matches!(approved.kind, OutcomeKind::Committed { .. }),
        "{approved:?}"
    );
    assert_eq!(order_row(&f, order).await["state"], json!("approved"));
    hold.release();
    let settled = amending.await.unwrap().unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let row = order_row(&f, order).await;
    assert_eq!(row["state"], json!("submitted"));
    assert_eq!(row["current_version"], json!(m));
    transition_instant(&f, order, m).await;
    assert_eq!(
        f.t.outbox().await,
        outbox + 2,
        "OrderApproved, OrderAmended"
    );
    let event = last_event(&f).await;
    assert_eq!(event["supersedesVersion"], json!(n), "{event}");
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_agg(jsonb_build_array(trigger, from_state, to_state) ORDER BY sequence) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' AND outcome='committed' \
             AND trigger IN ('reflect-approval-not-required','amendment')"
        ))
        .await,
        json!([
            ["reflect-approval-not-required", "submitted", "approved"],
            ["amendment", "approved", "submitted"]
        ])
    );
}

/// 04 §4.1: twenty committed amendments among failed attempts reach the cap; failed attempts
/// burn candidates but never count. The twenty-first is `amendment-cap-exhausted` with no
/// external call and no candidate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn twenty_successes_among_failed_attempts_hit_the_cap() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-twenty").await;
    let order = v.order.order_id;
    let line = v.lines[0].line_id;
    let svc = amend_service(&f);
    let mut current = n;
    let mut failed = 0;
    for k in 0..20 {
        if k % 4 == 3 {
            // A failed attempt between successes: Pricing refuses, the candidate burns.
            f.acceptance
                .refuse
                .lock()
                .insert(line, CommercialReason::NotSellable);
            let refused = amend_as(
                &svc,
                &buyer(),
                order,
                current,
                change_quantity(line, "9"),
                &format!("am-twenty-f{k}"),
            )
            .await
            .unwrap();
            assert_eq!(
                refused.outcome,
                SettledOutcome::Refused(Some(Reason::CatalogPredicateFailed)),
                "{refused:?}"
            );
            f.acceptance.refuse.lock().clear();
            failed += 1;
        }
        let settled = amend_as(
            &svc,
            &buyer(),
            order,
            current,
            change_quantity(line, &format!("{}", k + 2)),
            &format!("am-twenty-{k}"),
        )
        .await
        .unwrap();
        assert_eq!(
            settled.outcome,
            SettledOutcome::Committed,
            "{k}: {settled:?}"
        );
        let m = settled.response.body()["version"].as_i64().unwrap();
        assert!(m > current);
        current = m;
    }
    assert_eq!(failed, 5);
    let row = order_row(&f, order).await;
    assert_eq!(row["amendment_count"], json!(20));
    assert_eq!(row["current_version"], json!(current));
    // 20 committed amendments plus the created and submitted versions.
    assert_eq!(committed(&f, order).await.0, 22);
    let calls = external_calls(&f);
    let high = high_water(&f, order).await;
    let capped = amend_as(
        &svc,
        &buyer(),
        order,
        current,
        change_quantity(line, "2"),
        "am-twenty-cap",
    )
    .await
    .unwrap();
    assert_eq!(
        capped.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentCapExhausted)),
        "{capped:?}"
    );
    assert_eq!(external_calls(&f), calls);
    assert_eq!(high_water(&f, order).await, high, "no candidate reserved");
}

/// Step 17 for an amendment that moves the payer: the proposed tuples are offered in the shared
/// total order, and when the second collides with a live claim of another order the one this
/// attempt just acquired is released, N's claims were never released (acquire new before
/// releasing old), and the attempt settles `order-in-flight-for-key` with N current.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn claim_collision_after_one_new_reservation_releases_only_new_claims() {
    let other = Uuid::from_u128(31);
    let mut stubs = multikey_fixture();
    let profile = stubs.payer_profiles[&PAYER].clone();
    stubs.payer_profiles.insert(other, profile);
    let f = setup_with_bindings(stubs, 10, true, Some(FixedBindings::multikey())).await;
    let v = draft_tagged_items(&f, 1, "am-collide", true).await;
    let order = v.order.order_id;
    let settled = submit(&f, &buyer(), &v, "am-collide-submit").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let n = settled.response.body()["version"].as_i64().unwrap();
    let claims_n = live_claims(&f, order).await;
    assert_eq!(claims_n.as_array().unwrap().len(), 2, "{claims_n}");
    // Another order holds the later of the two proposed tuples (`dev-stub-scope` sorts after
    // `dev-second-scope`), so the first is acquired before the collision.
    let holder = 0x9877;
    super::claims::create_for(&f.t.env.db, holder, 31, RESOURCE)
        .await
        .unwrap();
    f.t.env
        .pg
        .sql(&format!(
            "INSERT INTO bss_orders__inflight_overlap_claim VALUES(gen_random_uuid(),'{other}','{}',\
             'dev-stub-scope','{}',1,clock_timestamp(),NULL)",
            Uuid::from_u128(RESOURCE),
            Uuid::from_u128(holder)
        ))
        .await
        .unwrap();
    let high = high_water(&f, order).await;
    let refused = amend(
        &f,
        order,
        n,
        json!({"reason": "new payer", "header": {"payer_tenant_id": other}}),
        "am-collide-1",
    )
    .await
    .unwrap();
    assert_eq!(
        refused.outcome,
        SettledOutcome::Refused(Some(Reason::OrderInFlightForKey)),
        "{refused:?}"
    );
    // N's claims: untouched and live.
    assert_eq!(live_claims(&f, order).await, claims_n);
    // The new tuple acquired before the collision: inserted, then released by this attempt.
    assert_eq!(
        f.t.json(&format!(
            "SELECT jsonb_agg(jsonb_build_array(overlap_scope_key, released_at IS NOT NULL)) AS j \
             FROM bss_orders__inflight_overlap_claim WHERE order_id='{order}' \
             AND payer_tenant_id='{other}'"
        ))
        .await,
        json!([["dev-second-scope", true]])
    );
    let row = order_row(&f, order).await;
    assert_eq!(row["current_version"], json!(n));
    assert_eq!(row["payer_tenant_id"], json!(PAYER));
    assert_eq!(
        high_water(&f, order).await,
        high + 1,
        "the candidate burned"
    );
}

/// D-188 step 5 atomicity for an amendment's claim cleanup: a payer move releases N's tuple
/// after acquiring the new one; a failure injected right after that step rolls the whole commit
/// back (no M, no new claim, N's claim live, no event, no settlement, the receipt kept), and the
/// same-key retry commits from the recorded receipt without commanding Pricing again.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn injected_cleanup_failure_rolls_back() {
    let other = Uuid::from_u128(31);
    let mut stubs = two_line_fixture();
    let profile = stubs.payer_profiles[&PAYER].clone();
    stubs.payer_profiles.insert(other, profile);
    let f = setup_with(stubs, 10, true).await;
    let (v, n) = submitted(&f, 1, "am-fault").await;
    let order = v.order.order_id;
    let claims_n = live_claims(&f, order).await;
    let rows_n = version_rows(&f, order, n).await;
    let candidate = high_water(&f, order).await + 1;
    let hook = Arc::new(FailAt {
        point: FaultPoint::Claims,
        hits: AtomicUsize::new(0),
    });
    let faulty = AmendService::new(Arc::new(service_over(
        &f,
        f.engine.with_fault(hook.clone()),
    )));
    let request = json!({"reason": "new payer", "header": {"payer_tenant_id": other}});
    let outbox = f.t.outbox().await;
    let calls = f.acceptance.calls();
    let answer = amend_as(&faulty, &buyer(), order, n, request.clone(), "am-fault-1")
        .await
        .unwrap_err();
    assert!(matches!(answer, OrdersError::Internal), "{answer:?}");
    assert!(hook.hits.load(Ordering::SeqCst) >= 1, "the cleanup ran");
    assert_eq!(f.acceptance.calls(), calls + 1);
    // Rolled back completely.
    assert_eq!(live_claims(&f, order).await, claims_n);
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__inflight_overlap_claim WHERE order_id='{order}' \
             AND payer_tenant_id='{other}'"
        ))
        .await,
        0
    );
    assert_eq!(version_rows(&f, order, n).await, rows_n);
    let row = order_row(&f, order).await;
    assert_eq!(row["current_version"], json!(n));
    assert_eq!(row["payer_tenant_id"], json!(PAYER));
    assert_eq!(f.t.outbox().await, outbox);
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__commercial_attempt WHERE order_id='{order}' \
             AND operation='amendment' AND status='running' AND receipt_results->'{}' ? 'accepted'",
            v.lines[0].line_id
        ))
        .await,
        1
    );
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__idempotency WHERE order_id='{order}' \
             AND operation='amendment' AND status='settled'"
        ))
        .await,
        0
    );
    // The same-key retry commits from the recorded receipt.
    lapse_leases(&f, order).await;
    let settled = amend(&f, order, n, request, "am-fault-1").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(settled.response.body()["version"], json!(candidate));
    assert_eq!(f.acceptance.calls(), calls + 1, "no second command");
    assert_eq!(f.t.outbox().await, outbox + 1);
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__idempotency WHERE order_id='{order}' \
             AND operation='amendment' AND status='settled'"
        ))
        .await,
        1
    );
    let live = live_claims(&f, order).await;
    assert_eq!(live.as_array().unwrap().len(), 1, "{live}");
    assert_eq!(live[0]["payer_tenant_id"], json!(other));
}

/// D-113 for the remaining local refusals: a delta naming an administrative field, a delta
/// naming a commercial-frozen axis, and a structural refusal over the proposed version (an added
/// line in another currency) each settle before any date, gate, authority or Pricing call and
/// reserve no candidate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn local_refusals_make_no_external_calls() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-local").await;
    let order = v.order.order_id;
    let calls = external_calls(&f);
    let high = high_water(&f, order).await;
    let mut eur = serde_json::to_value(super::submit::line()).unwrap();
    eur["currency"] = json!("EUR");
    for (request, reason) in [
        (
            json!({"reason": "r", "header": {"external_reference": "po-7"}}),
            Reason::AdministrativeFieldInAmendment,
        ),
        (
            json!({"reason": "r", "header": {"seller_tenant_id": Uuid::from_u128(77)}}),
            Reason::TenantAxisImmutable,
        ),
        (
            json!({"reason": "r", "lines": [{"op": "add", "line": eur}]}),
            Reason::CurrencyMixed,
        ),
    ] {
        let settled = amend(
            &f,
            order,
            n,
            request.clone(),
            &format!("am-local-{reason:?}"),
        )
        .await
        .unwrap();
        assert_eq!(
            settled.outcome,
            SettledOutcome::Refused(Some(reason)),
            "{request}: {settled:?}"
        );
        assert_eq!(external_calls(&f), calls, "{request}");
        assert_eq!(high_water(&f, order).await, high, "{request}");
    }
    assert_eq!(order_row(&f, order).await["current_version"], json!(n));
}

/// Three lines on the fixture's one overlap scope.
fn three_line_fixture() -> crate::infra::gate::stubs::StubFixture {
    let mut f = two_line_fixture();
    for occupancy in f.occupancy.values_mut() {
        let crate::domain::gate::answers::OccupancyObservation::LegacyActiveV1(occupancy) =
            occupancy
        else {
            panic!("legacy fixture expected")
        };
        occupancy.max_concurrent_active =
            crate::domain::gate::answers::MaxConcurrentActive::Limit(3);
    }
    f
}

/// Two added lines through a lost reply: fresh ids are handed out in ascending order in delta
/// order, and the recovery re-merges the same delta with the attempt's own added ids, sorted,
/// so each added line keeps both its id and its content (the first addition, quantity 1, the
/// smaller id; the second, quantity 3, the larger).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_added_lines_keep_their_ids_and_content_through_recovery() {
    let f = setup_with(three_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "am-add2").await;
    let order = v.order.order_id;
    let retained = v.lines[0].line_id;
    let first = serde_json::to_value(super::submit::line()).unwrap();
    let mut second = first.clone();
    second["selected_items"][0]["quantity"] = json!("3");
    let request = json!({"reason": "two more", "lines": [
        {"op": "add", "line": first}, {"op": "add", "line": second}]});
    f.acceptance.lose_replies.store(2, Ordering::SeqCst);
    let lost = amend(&f, order, n, request.clone(), "am-add2-1")
        .await
        .unwrap_err();
    assert!(matches!(lost, OrdersError::Unavailable), "{lost:?}");
    let mut added: Vec<Uuid> =
        f.t.json(&format!(
            "SELECT to_jsonb(array(SELECT k FROM jsonb_object_keys(line_requests) k \
             WHERE k <> 'order' ORDER BY k)) AS j FROM bss_orders__commercial_attempt \
             WHERE order_id='{order}' AND status IN ('prepared','running')"
        ))
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().parse().unwrap())
        .filter(|id| *id != retained)
        .collect();
    added.sort_unstable();
    assert_eq!(added.len(), 2);
    let reads = *f.h.calls.read_lines.lock();
    lapse_leases(&f, order).await;
    let settled = amend(&f, order, n, request, "am-add2-1").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(*f.h.calls.read_lines.lock(), reads, "no fresh gate run");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let quantities =
        f.t.json(&format!(
            "SELECT jsonb_object_agg(line_id, selected_items->0->'quantity') AS j \
             FROM bss_orders__order_line WHERE order_id='{order}' AND version={m}"
        ))
        .await;
    assert_eq!(
        quantities,
        json!({
            retained.to_string(): "1",
            added[0].to_string(): "1",
            added[1].to_string(): "3",
        })
    );
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_line_identity \
             WHERE order_id='{order}' AND line_id IN ('{}','{}')",
            added[0], added[1]
        ))
        .await,
        2
    );
}
