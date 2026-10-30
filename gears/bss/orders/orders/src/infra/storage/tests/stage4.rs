//! The Stage 4 integrated scenario on real PostgreSQL (S4-12; Stage 4 DESIGN §8): one order
//! driven through every Stage 4 step with the services and the engine, over the S4-08b world
//! (`acceptance.rs`): a partner places a contract-bound two-line order, the resource tenant's
//! customer records consent through the record-acceptance service, Workflow reflects approval,
//! the partner amends, and Workflow begins fulfillment. Refused attempts burn real candidates,
//! so the committed history is the sparse 1 → 4 → 7. Every step asserts the order's whole
//! footprint (versions, consent rows, attempts, claims, audit chain, idempotency keys, outbox),
//! and every committed step is replayed under its key. Compiled only with `gate-dev-stubs`.
//!
//! TEST DOUBLES, each labelled where it is used:
//! - Pricing's acceptance: the development double (`stubs::SellabilityStub`) behind the scripted
//!   wrapper (`submit.rs` `Scripted`): per-line refusals, lapsed receipts and lost replies.
//! - Contracts, payer profiles, occupancy and the other gate ports: the development stubs
//!   (`gate::stubs`) over the S3 example fixture; the outages are `gate::unavailable`.
//! - Workflow's approval reflections (rows 7 and 9, S5-03): `amend.rs` [`reflect`], the row's
//!   guards bound passing; the verdict is Workflow's to decide.
//! - Workflow's payment outcome on row 11 (Payments has no provider): the request body of
//!   `fulfillment.rs` [`begin`], whose S5-04 `gate.binding-deadline` guard is bound passing.
use super::acceptance::{CUSTOMER, PARTNER, World, customer, meta, partner, policy, record, world};
use super::amend::{change_quantity, live_claims, reflect};
use super::capture::{create_meta, view, write};
use super::fulfillment::{begin, resolver};
use super::submit::{
    acceptances, admit_two, contract_fixture, lapse_leases, last_event, line, submit_with,
    transition_instant,
};
use super::*;
use crate::authz::test_pdp::RulesProvider;
use crate::infra::capture::{Settled, SettledOutcome};
use crate::infra::engine::{EngineOutcome, OutcomeKind};
use crate::infra::events::payload::{
    EventKind, OrderAcceptanceRecorded, OrderAmended, OrderApproved, OrderSubmitted,
};
use crate::infra::gate::budget::Budget;
use crate::infra::gate::stubs::StubFixture;
use crate::infra::gate::submit::SubmitService;
use crate::infra::gate::tests::orchestrate::{CONTRACT, PAYER, harness_with};
use crate::infra::gate::unavailable::ContractsUnavailable;
use crate::infra::throttle::{CallerLimitSettings, CallerLimiter, PerCallerLimiter};
use arc_swap::ArcSwapOption;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::authoring::{Category, CreateOrder, OrderView};
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::fulfillment::Authorization;
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::{AcceptanceList, RequirementSource, VersionList};
use bss_pricing_sdk::acceptance::CommercialReason;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

/// A payer the AM double knows but outside the resource relationship (D-215/D-232).
const UNRELATED_PAYER: u128 = 31;

/// The S3 example fixture with the contract requiring consent, two lines admitted on its one
/// SKU and [`UNRELATED_PAYER`] known without a resource relationship.
fn stubs() -> StubFixture {
    let mut stubs = admit_two(contract_fixture(Some(true)));
    let mut profile = stubs.payer_profiles[&PAYER].clone();
    profile.in_relationship_with_resource = false;
    stubs.payer_profiles.insert(u(UNRELATED_PAYER), profile);
    stubs
}

/// The partner's two-line draft on the contract, as the partner reads it.
async fn draft(w: &World) -> OrderView {
    let order = CreateOrder {
        resource_tenant_id: u(super::submit::RESOURCE),
        payer_tenant_id: PAYER,
        seller_tenant_id: u(super::submit::SELLER),
        category: Category::NewSale,
        contract_id: Some(CONTRACT),
    };
    let created = view(
        w.capture
            .create(&partner(), order, &create_meta("s4-12-create"))
            .await
            .unwrap(),
    );
    let id = created.order.order_id;
    for n in 0..2 {
        w.capture
            .add_line(
                &partner(),
                id,
                line(),
                &write(&format!("s4-12-line-{n}"), 1, Some(n)),
            )
            .await
            .unwrap();
    }
    w.reads.get(&partner(), id).await.unwrap()
}

/// The world's submit service over a gate whose contract port is down (`ContractsUnavailable`,
/// the missing-provider answer), sharing the world's engine, dates, authority and Pricing double.
fn submit_without_contracts(w: &World) -> SubmitService {
    let h = harness_with(stubs(), false, Arc::new(Budget::baseline()), |ports| {
        ports.contracts = Arc::new(ContractsUnavailable);
    });
    let pdp = RulesProvider::from_policy(policy());
    let port: Arc<dyn bss_pricing_sdk::acceptance::SellabilityV1> = w.f.acceptance.clone();
    SubmitService::new(
        Arc::new(ArcSwapOption::from(Some(Arc::clone(&w.engine)))),
        Arc::new(w.f.t.reads_with(pdp)),
        h.orchestrator,
        w.f.dates.clone(),
        w.f.authority.clone(),
        PerCallerLimiter::new(
            CallerLimiter::Submit,
            CallerLimitSettings {
                per_minute: NonZeroU32::new(600).unwrap(),
                max_keys: 100,
            },
        ),
        200,
    )
    .with_policy_store(w.f.t.env.db.clone())
    .with_acceptance(Some(port))
}

async fn amend(
    w: &World,
    order: Uuid,
    version: i64,
    delta: Value,
    text: &str,
) -> Result<Settled, OrdersError> {
    let request: AmendOrder = serde_json::from_value(delta).unwrap();
    w.amender()
        .amend(
            &partner(),
            order,
            request,
            &meta(text, version),
            &CancellationToken::new(),
        )
        .await
}

/// The order's whole Stage 4 footprint: aggregate columns, committed versions with their
/// predecessor and reason, consent rows by version, attempts by candidate and status, live
/// claim ids, audit entries (all and committed), idempotency keys and the outbox.
async fn footprint(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_object( \
             'state', o.state, 'current_version', o.current_version, \
             'high_water', o.version_allocation_high_water, 'amendments', o.amendment_count, \
             'tolerated', o.authorization_failure_tolerated_at IS NOT NULL, \
             'audit_sequence', o.audit_sequence, \
             'versions', (SELECT jsonb_agg(jsonb_build_array(v.version, v.supersedes_version, \
                 v.reason) ORDER BY v.version) FROM bss_orders__order_version v \
                 WHERE v.order_id=o.order_id), \
             'consents', (SELECT coalesce(jsonb_agg(a.accepted_version ORDER BY \
                 a.accepted_version), '[]') FROM bss_orders__acceptance a WHERE a.order_id=o.order_id), \
             'attempts', (SELECT coalesce(jsonb_agg(jsonb_build_array(c.candidate_version, \
                 c.status) ORDER BY c.candidate_version), '[]') \
                 FROM bss_orders__commercial_attempt c WHERE c.order_id=o.order_id), \
             'claims', (SELECT coalesce(jsonb_agg(c.claim_id ORDER BY c.claim_id), '[]') \
                 FROM bss_orders__inflight_overlap_claim c \
                 WHERE c.order_id=o.order_id AND c.released_at IS NULL), \
             'audits', (SELECT count(*) FROM bss_orders__transition_audit t \
                 WHERE t.order_id=o.order_id), \
             'committed', (SELECT count(*) FROM bss_orders__transition_audit t \
                 WHERE t.order_id=o.order_id AND t.outcome='committed'), \
             'keys', (SELECT count(*) FROM bss_orders__idempotency i WHERE i.order_id=o.order_id), \
             'outbox', (SELECT count(*) FROM toolkit_outbox_body)) AS j \
             FROM bss_orders__order o WHERE o.order_id='{order}'"
        ))
        .await
}

/// `before` with `changes` applied: what a step must leave.
fn with(before: &Value, changes: &Value) -> Value {
    let mut after = before.clone();
    for (k, v) in changes.as_object().unwrap() {
        assert!(after.get(k).is_some(), "unknown footprint key {k}");
        after[k] = v.clone();
    }
    after
}

/// What a settled refusal must leave: `before` plus its one audited entry (outside the chain:
/// no sequence) and its settled key, with `changes` (a burned candidate) applied; no version,
/// consent, claim or event.
fn refusal(before: &Value, changes: &Value) -> Value {
    let mut all = json!({"audits": plus(before, "audits", 1), "keys": plus(before, "keys", 1)});
    for (k, v) in changes.as_object().unwrap() {
        all[k] = v.clone();
    }
    with(before, &all)
}

/// `before`'s counter `name` plus `by`.
fn plus(before: &Value, name: &str, by: i64) -> Value {
    json!(before[name].as_i64().unwrap() + by)
}

/// The audit hash chain of `order` is intact: sequenced entries are 1..=k without gaps, each
/// `prev_hash` is its predecessor's `entry_hash`, the first is the order's genesis, and the
/// aggregate's `audit_sequence` is k. Returns k.
async fn chain(w: &World, order: Uuid) -> i64 {
    let broken = w
        .f
        .t
        .n(&format!(
            "SELECT count(*) AS n FROM (SELECT sequence, prev_hash, \
             lag(entry_hash) OVER (ORDER BY sequence) AS before, \
             lag(sequence) OVER (ORDER BY sequence) AS previous \
             FROM bss_orders__transition_audit WHERE order_id='{order}' AND sequence IS NOT NULL) c \
             WHERE previous IS NOT NULL AND (prev_hash <> before OR sequence <> previous + 1)"
        ))
        .await;
    assert_eq!(broken, 0, "every link names its predecessor");
    let first =
        w.f.t
            .json(&format!(
                "SELECT to_jsonb(encode(prev_hash, 'hex')) AS j FROM bss_orders__transition_audit \
             WHERE order_id='{order}' AND sequence = 1"
            ))
            .await;
    let tenant: Uuid =
        serde_json::from_value(w.f.t.order_row(order).await["audit_tenant_id"].clone()).unwrap();
    let genesis = crate::domain::audit::genesis(tenant, order);
    let hex = genesis.iter().fold(String::new(), |mut hex, byte| {
        write!(hex, "{byte:02x}").unwrap();
        hex
    });
    assert_eq!(first, json!(hex), "the chain starts at the order's genesis");
    let k =
        w.f.t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__transition_audit \
             WHERE order_id='{order}' AND sequence IS NOT NULL"
            ))
            .await;
    assert_eq!(
        w.f.t.order_row(order).await["audit_sequence"],
        json!(k),
        "the aggregate's sequence is the chain head"
    );
    k
}

/// The type of the newest outbox event.
async fn last_type(w: &World) -> Value {
    let envelope =
        w.f.t
            .json(
                "SELECT convert_from(payload, 'UTF8')::jsonb AS j FROM toolkit_outbox_body \
                 ORDER BY id DESC LIMIT 1",
            )
            .await;
    envelope["type"].clone()
}

/// The newest committed audit entry of `order`: `[trigger, from, to, version]`.
async fn last_commit(w: &World, order: Uuid) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(trigger, from_state, to_state, version) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' AND outcome='committed' \
             ORDER BY sequence DESC LIMIT 1"
        ))
        .await
}

fn committed(settled: &Settled) {
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert!(!settled.replayed);
}

fn refused(settled: &Settled, reason: Reason, status: u16) {
    assert_eq!(
        settled.outcome,
        SettledOutcome::Refused(Some(reason)),
        "{settled:?}"
    );
    assert_eq!(settled.response.status(), status, "{settled:?}");
}

fn engine_refused(out: &EngineOutcome, reason: Reason, status: u16) {
    assert!(
        matches!(out.kind, OutcomeKind::Refused { reason: r, .. } if r == reason),
        "{out:?}"
    );
    assert_eq!(out.response.status(), status, "{out:?}");
}

fn engine_committed(out: &EngineOutcome) {
    assert!(matches!(out.kind, OutcomeKind::Committed { .. }), "{out:?}");
}

/// A same-key replay of a committed service step: the stored response, nothing new written.
async fn replays(w: &World, order: Uuid, first: &Settled, again: Result<Settled, OrdersError>) {
    let before = footprint(w, order).await;
    let again = again.unwrap();
    assert!(again.replayed, "{again:?}");
    assert_eq!(again.response, first.response, "identical stored response");
    assert_eq!(footprint(w, order).await, before, "a replay writes nothing");
}

/// A same-key replay of a committed engine step (the Workflow seams): the stored response,
/// nothing new written.
async fn engine_replays(
    w: &World,
    order: Uuid,
    first: &EngineOutcome,
    again: impl std::future::Future<Output = EngineOutcome>,
) {
    let before = footprint(w, order).await;
    let again = again.await;
    assert!(
        matches!(again.kind, OutcomeKind::Replayed { .. }),
        "{again:?}"
    );
    assert_eq!(again.response, first.response, "identical stored response");
    assert_eq!(footprint(w, order).await, before, "a replay writes nothing");
}

/// Burned candidate `candidate` is a receipt-only attempt: it holds Pricing's accepted receipt
/// for `line` and nothing else of it exists — no version, no consent, no pinned line.
async fn receipt_only(w: &World, order: Uuid, candidate: i64, line: Uuid) {
    assert_eq!(
        w.f.t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__commercial_attempt WHERE order_id='{order}' \
                 AND candidate_version={candidate} AND status='refused' \
                 AND receipt_results->'{line}' ? 'accepted'"
            ))
            .await,
        1,
        "candidate {candidate} holds an accepted receipt"
    );
    for table in ["order_version", "order_line", "resolved_total"] {
        assert_eq!(
            w.f.t
                .n(&format!(
                    "SELECT count(*) AS n FROM bss_orders__{table} \
                     WHERE order_id='{order}' AND version={candidate}"
                ))
                .await,
            0,
            "{table} at burned candidate {candidate}"
        );
    }
    assert_eq!(
        w.f.t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__acceptance \
                 WHERE order_id='{order}' AND accepted_version={candidate}"
            ))
            .await,
        0,
        "a receipt is never consent"
    );
}

/// The committed lines of `version` are pinned by that candidate's own committed attempt.
async fn pinned_by_own_attempt(w: &World, order: Uuid, version: i64) {
    assert_eq!(
        w.f.t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__order_line l \
                 JOIN bss_orders__commercial_attempt a ON a.attempt_id = l.commercial_attempt_id \
                 WHERE l.order_id='{order}' AND l.version={version} AND l.order_pin IS NOT NULL \
                 AND a.candidate_version={version} AND a.status='committed'"
            ))
            .await,
        2
    );
}

/// The consent record of `version`: `[recorded_by, path, source, accepted_at]`.
async fn consent_row(w: &World, order: Uuid, version: i64) -> Value {
    w.f.t
        .json(&format!(
            "SELECT jsonb_build_array(recorded_by, recording_path, requirement_source, \
             accepted_at) AS j FROM bss_orders__acceptance \
             WHERE order_id='{order}' AND accepted_version={version}"
        ))
        .await
}

/// The engine's time of the committed record-acceptance entry for `version`.
async fn recorded_at(w: &World, order: Uuid, version: i64) -> Value {
    w.f.t
        .json(&format!(
            "SELECT to_jsonb(created_at) AS j FROM bss_orders__transition_audit \
             WHERE order_id='{order}' AND trigger='record-acceptance' AND outcome='committed' \
             AND version={version}"
        ))
        .await
}

fn ver(n: i64) -> OrderVersion {
    OrderVersion::try_from(n).unwrap()
}

/// S4-12 happy path with its refusals, on one order (Stage 4 DESIGN §8):
///
/// 1. submit with the referenced contract's provider down: 503, nothing reserved;
/// 2. two refused submits burn candidates 2 (Pricing accepts line A, refuses line B: a
///    receipt-only attempt) and 3 (lapsed receipts); the third submit commits 4, partner-placed,
///    without consent;
/// 3. consent with the contract down: 503; the customer records consent on 4 through the
///    record-acceptance service;
/// 4. Workflow reflects `reflect-approval-required` then `-granted` on 4;
/// 5. an amendment naming an unrelated payer refuses `payer-relationship-unconfirmed`; two
///    refused amendments burn 5 (receipt-only) and 6; the amendment whose Pricing reply is lost
///    is recovered from its frozen inputs and commits 7 superseding 4, back to `submitted`;
/// 6. consent and verdict for 4 no longer count: a late approval naming 4 is `version-conflict`
///    naming 7, a recording naming 4 is 412, begin on 7 after a fresh approval refuses
///    `acceptance-required-not-recorded`;
/// 7. fresh consent on 7; begin-fulfillment `pending` refused, `failed` (nothing tolerates it)
///    refused `authorization-failed`, `authorized` admitted;
/// 8. every committed step replays under its key with the identical stored response and no new
///    rows; the history reads show 1 → 4 → 7 and the burned numbers are `version-not-found`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario, asserted step by step; splitting it would hide the order of events"
)]
async fn stage4_integrated_scenario() {
    let w = Box::pin(world(stubs())).await;
    let view = draft(&w).await;
    let order = view.order.order_id;
    let mut ids: Vec<Uuid> = view.lines.iter().map(|l| l.line_id).collect();
    ids.sort();
    // Phase B commands lines in line-id order: A is commanded first, B second.
    let (line_a, line_b) = (ids[0], ids[1]);
    let partner_id = u(PARTNER.0).to_string();
    let customer_id = u(CUSTOMER.0).to_string();
    let initial = footprint(&w, order).await;
    assert_eq!(initial["versions"], json!([[1, null, "create"]]));
    assert_eq!(initial["high_water"], json!(1));

    // ---- 1. Submit with the referenced contract unavailable: 503, nothing reserved. ----
    let down = submit_with(
        &submit_without_contracts(&w),
        &partner(),
        &view,
        "s4-12-submit-down",
    )
    .await
    .unwrap();
    refused(&down, Reason::ContractResolutionUnavailable, 503);
    let now = footprint(&w, order).await;
    assert_eq!(
        now,
        with(
            &initial,
            &json!({"audits": plus(&initial, "audits", 1), "keys": plus(&initial, "keys", 1)})
        ),
        "an unavailable contract reserves no candidate and writes nothing commercial; the \
         audited refusal takes no chain sequence"
    );

    // ---- 2. Two burned submit candidates, then the commit on 4. ----
    w.f.acceptance
        .refuse
        .lock()
        .insert(line_b, CommercialReason::NotSellable);
    let before = footprint(&w, order).await;
    let burned = submit_with(&w.submit, &partner(), &view, "s4-12-submit-1")
        .await
        .unwrap();
    refused(&burned, Reason::CatalogPredicateFailed, 400);
    w.f.acceptance.refuse.lock().clear();
    assert_eq!(
        footprint(&w, order).await,
        refusal(
            &before,
            &json!({"high_water": 2, "attempts": [[2, "refused"]]})
        ),
        "candidate 2 burned"
    );
    receipt_only(&w, order, 2, line_a).await;

    // Every receipt Pricing issues for candidate 3 is already past its activation deadline.
    w.f.acceptance.lapsed.store(true, Ordering::SeqCst);
    let before = footprint(&w, order).await;
    let burned = submit_with(&w.submit, &partner(), &view, "s4-12-submit-2")
        .await
        .unwrap();
    refused(&burned, Reason::OrderBindingExpired, 400);
    w.f.acceptance.lapsed.store(false, Ordering::SeqCst);
    assert_eq!(
        footprint(&w, order).await,
        refusal(
            &before,
            &json!({"high_water": 3, "attempts": [[2, "refused"], [3, "refused"]]})
        ),
        "candidate 3 burned"
    );

    let before = footprint(&w, order).await;
    let k = chain(&w, order).await;
    let submitted = submit_with(&w.submit, &partner(), &view, "s4-12-submit-3")
        .await
        .unwrap();
    committed(&submitted);
    assert_eq!(submitted.response.body()["version"], json!(4));
    let now = footprint(&w, order).await;
    assert_eq!(
        now["claims"].as_array().unwrap().len(),
        1,
        "the order claims its tuple"
    );
    assert_eq!(
        now,
        with(
            &before,
            &json!({"state": "submitted", "current_version": 4, "high_water": 4,
                   "versions": [[1, null, "create"], [4, 1, "submit"]],
                   "attempts": [[2, "refused"], [3, "refused"], [4, "committed"]],
                   "claims": now["claims"], "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1), "outbox": plus(&before, "outbox", 1)})
        )
    );
    assert_eq!(chain(&w, order).await, k + 1);
    assert_eq!(last_type(&w).await, json!(OrderSubmitted::TYPE_ID));
    assert!(
        last_event(&w.f).await.get("acceptance").is_none(),
        "a partner submit never consents"
    );
    assert_eq!(
        last_commit(&w, order).await,
        json!(["submit", "draft", "submitted", 4])
    );
    pinned_by_own_attempt(&w, order, 4).await;
    let claims_4 = live_claims(&w.f, order).await;
    replays(
        &w,
        order,
        &submitted,
        submit_with(&w.submit, &partner(), &view, "s4-12-submit-3").await,
    )
    .await;

    // ---- 3. Consent on 4: the contract down is 503; the customer records it. ----
    let before = footprint(&w, order).await;
    let contracts_down = w.recorder_with(Arc::clone(&w.engine), Arc::new(ContractsUnavailable));
    let unevaluable = record(&contracts_down, &customer(), order, 4, "s4-12-consent-down")
        .await
        .unwrap();
    refused(&unevaluable, Reason::AcceptanceRequirementUnevaluable, 503);
    assert_eq!(
        footprint(&w, order).await,
        refusal(&before, &json!({})),
        "no consent without the requirement, never a default"
    );

    let before = footprint(&w, order).await;
    let consent_4 = record(&w.recorder(), &customer(), order, 4, "s4-12-consent-4")
        .await
        .unwrap();
    committed(&consent_4);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"consents": [4], "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1), "outbox": plus(&before, "outbox", 1)})
        ),
        "consent changes no state or version"
    );
    assert_eq!(
        consent_row(&w, order, 4).await,
        json!([
            customer_id,
            "partner_placed",
            "contract",
            recorded_at(&w, order, 4).await
        ])
    );
    assert_eq!(last_type(&w).await, json!(OrderAcceptanceRecorded::TYPE_ID));
    assert_eq!(last_event(&w.f).await["acceptedVersion"], json!(4));
    assert_eq!(
        last_commit(&w, order).await,
        json!(["record-acceptance", "submitted", "submitted", 4])
    );
    chain(&w, order).await;
    replays(
        &w,
        order,
        &consent_4,
        record(&w.recorder(), &customer(), order, 4, "s4-12-consent-4").await,
    )
    .await;

    // ---- 4. Workflow reflects approval on 4 (rows 7 and 9). ----
    let before = footprint(&w, order).await;
    let required_4 = reflect(
        &w.f,
        order,
        7,
        Trigger::ReflectApprovalRequired,
        4,
        "s4-12-req-4",
    )
    .await;
    engine_committed(&required_4);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"state": "pending_approval", "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1)})
        ),
        "row 7 is eventless (no catalog event reflects approval-required)"
    );
    assert_eq!(
        last_commit(&w, order).await,
        json!([
            "reflect-approval-required",
            "submitted",
            "pending_approval",
            4
        ])
    );
    let before = footprint(&w, order).await;
    let granted_4 = reflect(
        &w.f,
        order,
        9,
        Trigger::ReflectApprovalGranted,
        4,
        "s4-12-grant-4",
    )
    .await;
    engine_committed(&granted_4);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"state": "approved", "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1), "outbox": plus(&before, "outbox", 1)})
        )
    );
    assert_eq!(last_type(&w).await, json!(OrderApproved::TYPE_ID));
    assert_eq!(last_event(&w.f).await["approvedVersion"], json!(4));
    assert_eq!(
        last_commit(&w, order).await,
        json!([
            "reflect-approval-granted",
            "pending_approval",
            "approved",
            4
        ])
    );
    chain(&w, order).await;
    engine_replays(
        &w,
        order,
        &required_4,
        reflect(
            &w.f,
            order,
            7,
            Trigger::ReflectApprovalRequired,
            4,
            "s4-12-req-4",
        ),
    )
    .await;
    engine_replays(
        &w,
        order,
        &granted_4,
        reflect(
            &w.f,
            order,
            9,
            Trigger::ReflectApprovalGranted,
            4,
            "s4-12-grant-4",
        ),
    )
    .await;

    // ---- 5. Amendments: an unrelated payer, two burned candidates, a lost Pricing reply. ----
    let before = footprint(&w, order).await;
    let payer = amend(
        &w,
        order,
        4,
        json!({"reason": "new payer", "header": {"payer_tenant_id": u(UNRELATED_PAYER)}}),
        "s4-12-amend-payer",
    )
    .await
    .unwrap();
    refused(&payer, Reason::PayerRelationshipUnconfirmed, 400);
    assert_eq!(
        footprint(&w, order).await,
        refusal(&before, &json!({})),
        "the gate refuses before any candidate is reserved"
    );
    assert_eq!(
        w.f.t.order_row(order).await["payer_tenant_id"],
        json!(PAYER)
    );

    let delta = change_quantity(line_a, "2");
    w.f.acceptance
        .refuse
        .lock()
        .insert(line_b, CommercialReason::NotSellable);
    let before = footprint(&w, order).await;
    let burned = amend(&w, order, 4, delta.clone(), "s4-12-amend-1")
        .await
        .unwrap();
    refused(&burned, Reason::CatalogPredicateFailed, 400);
    w.f.acceptance.refuse.lock().clear();
    assert_eq!(
        footprint(&w, order).await,
        refusal(
            &before,
            &json!({"high_water": 5,
                   "attempts": [[2, "refused"], [3, "refused"], [4, "committed"], [5, "refused"]]})
        ),
        "candidate 5 burned; N stays current, approved, uncounted"
    );
    receipt_only(&w, order, 5, line_a).await;
    w.f.acceptance.lapsed.store(true, Ordering::SeqCst);
    let before = footprint(&w, order).await;
    let burned = amend(&w, order, 4, delta.clone(), "s4-12-amend-2")
        .await
        .unwrap();
    refused(&burned, Reason::OrderBindingExpired, 400);
    w.f.acceptance.lapsed.store(false, Ordering::SeqCst);
    assert_eq!(
        footprint(&w, order).await,
        refusal(
            &before,
            &json!({"high_water": 6,
                   "attempts": [[2, "refused"], [3, "refused"], [4, "committed"], [5, "refused"],
                                [6, "refused"]]})
        ),
        "candidate 6 burned"
    );
    assert_eq!(live_claims(&w.f, order).await, claims_4);

    // The first command's reply and its one retry are lost: unavailable, M reserved.
    w.f.acceptance.lose_replies.store(2, Ordering::SeqCst);
    let before = footprint(&w, order).await;
    let lost = amend(&w, order, 4, delta.clone(), "s4-12-amend-3")
        .await
        .unwrap_err();
    assert!(matches!(lost, OrdersError::Unavailable), "{lost:?}");
    let now = footprint(&w, order).await;
    assert_eq!(
        now,
        with(
            &before,
            &json!({"high_water": 7, "keys": plus(&before, "keys", 1), "attempts": now["attempts"]})
        ),
        "M is reserved, nothing settled or committed"
    );
    assert_eq!(now["attempts"][5][0], json!(7));
    assert_ne!(now["attempts"][5][1], json!("committed"));
    let reads = *w.f.h.calls.read_lines.lock();
    let prepared = w.f.dates.prepared.load(Ordering::SeqCst);
    lapse_leases(&w.f, order).await;
    let before = footprint(&w, order).await;
    let amended = amend(&w, order, 4, delta.clone(), "s4-12-amend-3")
        .await
        .unwrap();
    committed(&amended);
    assert_eq!(amended.response.body()["version"], json!(7));
    assert_eq!(amended.response.body()["state"], json!("submitted"));
    assert_eq!(
        *w.f.h.calls.read_lines.lock(),
        reads,
        "recovered from frozen inputs: no gate run"
    );
    assert_eq!(w.f.dates.prepared.load(Ordering::SeqCst), prepared);
    let now = footprint(&w, order).await;
    assert_eq!(
        now,
        with(
            &before,
            &json!({"state": "submitted", "current_version": 7, "amendments": 1,
                   "versions": [[1, null, "create"], [4, 1, "submit"], [7, 4, "amendment"]],
                   "attempts": [[2, "refused"], [3, "refused"], [4, "committed"],
                                [5, "refused"], [6, "refused"], [7, "committed"]],
                   "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "outbox": plus(&before, "outbox", 1)})
        ),
        "M commits; N's consent stays N's"
    );
    assert_eq!(
        live_claims(&w.f, order).await,
        claims_4,
        "the same tuple is retained, never released and re-taken"
    );
    assert_eq!(last_type(&w).await, json!(OrderAmended::TYPE_ID));
    let event = last_event(&w.f).await;
    assert_eq!(event["supersedesVersion"], json!(4), "{event}");
    assert_eq!(event["orderVersion"], json!(7));
    assert_eq!(
        last_commit(&w, order).await,
        json!(["amendment", "approved", "submitted", 7])
    );
    transition_instant(&w.f, order, 7).await;
    pinned_by_own_attempt(&w, order, 7).await;
    chain(&w, order).await;
    replays(
        &w,
        order,
        &amended,
        amend(&w, order, 4, delta, "s4-12-amend-3").await,
    )
    .await;

    // ---- 6. Consent and verdict for 4 no longer count. ----
    let page = w
        .reads
        .get_acceptance(&customer(), order, &AcceptanceList::default())
        .await
        .unwrap();
    assert_eq!(page.current_version, Some(ver(7)));
    assert_eq!(page.current_acceptance, None, "M starts without consent");
    assert_eq!(
        page.items
            .iter()
            .map(|r| i64::from(r.accepted_version))
            .collect::<Vec<_>>(),
        vec![4],
        "4's consent is a prior record"
    );
    let before = footprint(&w, order).await;
    let late = reflect(
        &w.f,
        order,
        9,
        Trigger::ReflectApprovalGranted,
        4,
        "s4-12-grant-late",
    )
    .await;
    engine_refused(&late, Reason::VersionConflict, 409);
    assert_eq!(
        late.response.body()["context"]["data"]["current_version"],
        json!(7)
    );
    for (version, text) in [(4, "s4-12-consent-stale-4"), (5, "s4-12-consent-stale-5")] {
        let stale = record(&w.recorder(), &customer(), order, version, text)
            .await
            .unwrap();
        refused(&stale, Reason::PreconditionFailed, 412);
        assert_eq!(
            stale.response.body()["context"]["data"]["current_version"],
            json!(7)
        );
    }
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"audits": plus(&before, "audits", 3), "keys": plus(&before, "keys", 3)})
        ),
        "three audited refusals: the order stays submitted on 7, 4's consent stays 4's"
    );

    // ---- 7. Approval on 7; consent on 4 cannot carry begin-fulfillment. ----
    let before = footprint(&w, order).await;
    let required_7 = reflect(
        &w.f,
        order,
        7,
        Trigger::ReflectApprovalRequired,
        7,
        "s4-12-req-7",
    )
    .await;
    engine_committed(&required_7);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"state": "pending_approval", "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1)})
        )
    );
    assert_eq!(
        last_commit(&w, order).await,
        json!([
            "reflect-approval-required",
            "submitted",
            "pending_approval",
            7
        ])
    );
    let before = footprint(&w, order).await;
    let granted_7 = reflect(
        &w.f,
        order,
        9,
        Trigger::ReflectApprovalGranted,
        7,
        "s4-12-grant-7",
    )
    .await;
    engine_committed(&granted_7);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"state": "approved", "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1), "outbox": plus(&before, "outbox", 1)})
        )
    );
    assert_eq!(last_type(&w).await, json!(OrderApproved::TYPE_ID));
    assert_eq!(last_event(&w.f).await["approvedVersion"], json!(7));
    assert_eq!(
        last_commit(&w, order).await,
        json!([
            "reflect-approval-granted",
            "pending_approval",
            "approved",
            7
        ])
    );
    chain(&w, order).await;
    engine_replays(
        &w,
        order,
        &required_7,
        reflect(
            &w.f,
            order,
            7,
            Trigger::ReflectApprovalRequired,
            7,
            "s4-12-req-7",
        ),
    )
    .await;
    engine_replays(
        &w,
        order,
        &granted_7,
        reflect(
            &w.f,
            order,
            9,
            Trigger::ReflectApprovalGranted,
            7,
            "s4-12-grant-7",
        ),
    )
    .await;

    let r = resolver(&w, Some(true), true);
    let before = footprint(&w, order).await;
    for (version, text) in [(4, "s4-12-begin-4"), (6, "s4-12-begin-6")] {
        let stale = begin(&w, &r, order, version, Authorization::Authorized, text)
            .await
            .unwrap();
        engine_refused(&stale, Reason::VersionConflict, 409);
    }
    let unconsented = begin(
        &w,
        &r,
        order,
        7,
        Authorization::Authorized,
        "s4-12-begin-7-0",
    )
    .await
    .unwrap();
    engine_refused(&unconsented, Reason::AcceptanceRequiredNotRecorded, 400);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"audits": plus(&before, "audits", 3), "keys": plus(&before, "keys", 3)})
        ),
        "each refusal is audited and settled; the order stays approved on 7"
    );

    // ---- 8. Fresh consent on 7. ----
    let before = footprint(&w, order).await;
    let consent_7 = record(&w.recorder(), &customer(), order, 7, "s4-12-consent-7")
        .await
        .unwrap();
    committed(&consent_7);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"consents": [4, 7], "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1), "outbox": plus(&before, "outbox", 1)})
        )
    );
    assert_eq!(
        consent_row(&w, order, 7).await,
        json!([
            customer_id,
            "partner_placed",
            "contract",
            recorded_at(&w, order, 7).await
        ])
    );
    assert_eq!(last_type(&w).await, json!(OrderAcceptanceRecorded::TYPE_ID));
    assert_eq!(last_event(&w.f).await["acceptedVersion"], json!(7));
    assert_eq!(
        last_commit(&w, order).await,
        json!(["record-acceptance", "approved", "approved", 7])
    );
    replays(
        &w,
        order,
        &consent_7,
        record(&w.recorder(), &customer(), order, 7, "s4-12-consent-7").await,
    )
    .await;
    assert_eq!(
        acceptances(&w.f, order).await.as_array().unwrap().len(),
        2,
        "one consent row per version, bound to it"
    );

    // ---- 9. Begin-fulfillment on 7: pending, failed-not-tolerated, then authorized. ----
    let before = footprint(&w, order).await;
    let pending = begin(&w, &r, order, 7, Authorization::Pending, "s4-12-begin-7-1")
        .await
        .unwrap();
    engine_refused(&pending, Reason::AuthorizationPending, 400);
    let failed = begin(&w, &r, order, 7, Authorization::Failed, "s4-12-begin-7-2")
        .await
        .unwrap();
    engine_refused(&failed, Reason::AuthorizationFailed, 400);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"audits": plus(&before, "audits", 2), "keys": plus(&before, "keys", 2)})
        ),
        "refusals leave the order approved, untolerated and eventless"
    );
    let before = footprint(&w, order).await;
    let admitted = begin(
        &w,
        &r,
        order,
        7,
        Authorization::Authorized,
        "s4-12-begin-7-3",
    )
    .await
    .unwrap();
    engine_committed(&admitted);
    assert_eq!(
        footprint(&w, order).await,
        with(
            &before,
            &json!({"state": "in_fulfillment", "audits": plus(&before, "audits", 1),
                   "committed": plus(&before, "committed", 1),
                   "audit_sequence": plus(&before, "audit_sequence", 1),
                   "keys": plus(&before, "keys", 1)})
        ),
        "row 11 is eventless and flags no tolerance"
    );
    assert_eq!(
        last_commit(&w, order).await,
        json!(["begin-fulfillment", "approved", "in_fulfillment", 7])
    );
    chain(&w, order).await;
    engine_replays(&w, order, &admitted, async {
        begin(
            &w,
            &r,
            order,
            7,
            Authorization::Authorized,
            "s4-12-begin-7-3",
        )
        .await
        .unwrap()
    })
    .await;

    // ---- 10. The history reads. ----
    let history = w
        .reads
        .list_versions(&customer(), order, &VersionList::default())
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|r| (
                i64::from(r.version),
                r.supersedes_version.map(i64::from),
                r.actor.clone()
            ))
            .collect::<Vec<_>>(),
        vec![
            (1, None, partner_id.clone()),
            (4, Some(1), partner_id.clone()),
            (7, Some(4), partner_id.clone()),
        ]
    );
    assert_eq!(history.current_version, Some(ver(7)));
    for burned in [2, 3, 5, 6] {
        let missing = w
            .reads
            .get_version(&customer(), order, ver(burned))
            .await
            .unwrap_err();
        assert!(
            matches!(missing, OrdersError::Refused(Reason::VersionNotFound)),
            "{burned}: {missing:?}"
        );
    }
    let page = w
        .reads
        .get_acceptance(&customer(), order, &AcceptanceList::default())
        .await
        .unwrap();
    let current = page.current_acceptance.unwrap();
    assert_eq!(current.accepted_version, ver(7));
    assert_eq!(current.recorded_by, customer_id);
    assert_eq!(current.requirement_source, RequirementSource::Contract);
    assert_eq!(
        page.items
            .iter()
            .map(|r| i64::from(r.accepted_version))
            .collect::<Vec<_>>(),
        vec![4]
    );
}
