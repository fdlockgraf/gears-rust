//! S4-06 history reads on real PostgreSQL (Stage 4 DESIGN §7; D-254): `list_versions`,
//! `get_version` and `get_acceptance` through the shared read wrapper over submitted and amended
//! orders of the submit fixture. Authorization is decided on the current parent before any
//! existence signal; pages hold committed rows only; a burned candidate is `version-not-found`;
//! a version is composed at that version; every read follows the access-log decision table under
//! its catalog operation id. Compiled only with `gate-dev-stubs`.
use super::amend::{amend, change_quantity, order_row, submitted};
use super::capture::{BUYER, buyer, http, s};
use super::submit::{
    Fixture, ORDER_RT, RESOURCE, SELLER, acceptances, draft_tagged, setup_with, submit,
    two_line_fixture, versions,
};
use super::*;
use crate::api::local_client::LocalOrdersClient;
use crate::authz::Caller;
use crate::authz::test_pdp::{RulesProvider, user};
use crate::infra::capture::SettledOutcome;
use crate::infra::gate::tests::orchestrate::PAYER;
use crate::infra::read::ReadService;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::OrdersV1;
use bss_orders_sdk::authoring::SalesPath;
use bss_orders_sdk::catalog::Reason;
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::{
    AcceptanceList, Cursor, PageSize, ReadMeta, RequirementSource, VersionList, VersionReason,
};
use bss_pricing_sdk::acceptance::CommercialReason;
use serde_json::{Value, json};
use std::sync::Arc;

/// Seller operator on the fixture's seller tenant: a cross-tenant reader (logged).
const SELLER_READER: (u128, u128) = (103, SELLER);
/// Payer reader: reads the orders whose *current* payer is the fixture payer.
const PAYER_READER: (u128, u128) = (105, 0xa1);
/// A principal no rule names.
const STRANGER: (u128, u128) = (104, 10);
/// The amendment's new payer (confirmed by the resource relationship stub).
const OTHER_PAYER: u128 = 31;

fn rule(id: &str, subject: u128, property: &str, value: Uuid) -> Value {
    json!({"id": id, "subject": {"id": s(u(subject))}, "resource_type": ORDER_RT,
        "actions": ["read"],
        "paths": [{"predicates": [{"property": property, "values": [s(value)]}]}]})
}

/// `order × read` only: history needs no audit permission.
fn history_policy() -> Value {
    json!({"vendor": "constructorfabric", "priority": 10, "policy_revision": "s4-06",
    "rules": [
        rule("buyer", BUYER.0, "resource_tenant_id", u(RESOURCE)),
        rule("seller", SELLER_READER.0, "seller_tenant_id", u(SELLER)),
        rule("payer", PAYER_READER.0, "payer_tenant_id", PAYER),
    ]})
}
fn reads(f: &Fixture) -> ReadService {
    f.t.reads_with(RulesProvider::from_policy(history_policy()))
}

fn caller(who: (u128, u128)) -> Caller {
    user(who.0, who.1)
}
fn v(n: i64) -> OrderVersion {
    OrderVersion::try_from(n).unwrap()
}
fn page(limit: u64, cursor: Option<Cursor>) -> VersionList {
    VersionList {
        limit: Some(PageSize::try_from(limit).unwrap()),
        cursor,
    }
}
fn refusal(error: &OrdersError) -> Reason {
    match error {
        OrdersError::Refused(reason) => *reason,
        other => panic!("not a refusal: {other:?}"),
    }
}

/// The access-log rows naming `order`, oldest first: `[operation, outcome, reason, order_id]`.
async fn logs(f: &Fixture, order: Uuid) -> Value {
    f.t.json(&format!(
        "SELECT coalesce(jsonb_agg(jsonb_build_array(operation, outcome, refusal_reason, order_id) \
         ORDER BY accessed_at, access_id), '[]'::jsonb) AS j FROM bss_orders__read_access_log \
         WHERE requested_order_ref='{order}'"
    ))
    .await
}

/// A fixture whose stubs confirm [`OTHER_PAYER`] as a payer of the resource tenant.
async fn payer_fixture() -> Fixture {
    let mut stubs = two_line_fixture();
    let profile = stubs.payer_profiles[&PAYER].clone();
    stubs.payer_profiles.insert(u(OTHER_PAYER), profile);
    setup_with(stubs, 10, true).await
}

/// 08 §3.6 and DESIGN §7: the decision is taken on the current parent before anything about
/// the history is read. A payer reader who loses the order to a payer amendment sees no versions,
/// no version and no consent: the same `order-not-found` a missing order answers, logged
/// refused under each operation. The resource tenant still reads N, composed under N's payer.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn history_reads_authorize_before_disclosing_existence() {
    let f = Box::pin(payer_fixture()).await;
    let reads = reads(&f);
    let payer = caller(PAYER_READER);
    let (view, n) = submitted(&f, 1, "h-auth").await;
    let order = view.order.order_id;
    // Before the amendment the payer reads the history.
    let before = reads
        .list_versions(&payer, order, &VersionList::default())
        .await
        .unwrap();
    assert_eq!(before.items.len(), 2);
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "new payer", "header": {"payer_tenant_id": u(OTHER_PAYER)}}),
        "h-auth-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let missing = Uuid::now_v7();
    for target in [order, missing] {
        for who in [payer.clone(), caller(STRANGER)] {
            let list = reads
                .list_versions(&who, target, &VersionList::default())
                .await
                .unwrap_err();
            let one = reads.get_version(&who, target, v(n)).await.unwrap_err();
            let consent = reads
                .get_acceptance(&who, target, &AcceptanceList::default())
                .await
                .unwrap_err();
            let burned = reads.get_version(&who, target, v(999)).await.unwrap_err();
            for error in [list, one, consent, burned] {
                assert_eq!(refusal(&error), Reason::OrderNotFound, "{target} {error:?}");
            }
        }
    }
    let refused = |target: Uuid, exists: bool| {
        let order_id = if exists { json!(target) } else { Value::Null };
        json!([
            ["list_versions", "refused", "order-not-found", order_id],
            ["get_version", "refused", "order-not-found", order_id],
            ["get_acceptance", "refused", "order-not-found", order_id],
            ["get_version", "refused", "order-not-found", order_id],
        ])
    };
    let mut expected = refused(order, true).as_array().unwrap().clone();
    expected.extend(refused(order, true).as_array().unwrap().clone());
    // The payer's earlier served read was cross-tenant, so it is logged first.
    let mut all = vec![json!(["list_versions", "served", null, order])];
    all.extend(expected);
    assert_eq!(logs(&f, order).await, json!(all));
    let mut missing_rows = refused(missing, false).as_array().unwrap().clone();
    missing_rows.extend(refused(missing, false).as_array().unwrap().clone());
    assert_eq!(logs(&f, missing).await, json!(missing_rows));
    // The resource tenant keeps reading, and N composes under N's payer, M under M's.
    let history = reads
        .list_versions(&buyer(), order, &VersionList::default())
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|r| i64::from(r.version))
            .collect::<Vec<_>>(),
        vec![1, n, m]
    );
    let at_n = reads.get_version(&buyer(), order, v(n)).await.unwrap();
    let at_m = reads.get_version(&buyer(), order, v(m)).await.unwrap();
    let payer_of = |line: &bss_orders_sdk::authoring::DraftLine| {
        line.committed
            .as_ref()
            .unwrap()
            .order_pin
            .receipt()
            .query
            .tenant_axes
            .payer_tenant_id
    };
    assert_eq!(payer_of(&at_n.lines[0]), PAYER);
    assert_eq!(payer_of(&at_m.lines[0]), u(OTHER_PAYER));
}

/// D-188/D-254: the history page holds committed rows only, in version order, without
/// placeholders for burned candidates; the keyset continues after the last number.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn sparse_versions_page_without_placeholders() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let order = sparse_1_4_7(&f, "h-sparse").await;
    let reads = reads(&f);
    let first = reads
        .list_versions(&buyer(), order, &page(2, None))
        .await
        .unwrap();
    assert_eq!(
        first
            .items
            .iter()
            .map(|r| i64::from(r.version))
            .collect::<Vec<_>>(),
        vec![1, 4]
    );
    assert_eq!(first.current_version, Some(v(7)));
    assert_eq!(first.page_info.limit.get(), 2);
    let create = &first.items[0];
    assert_eq!(create.reason, VersionReason::Create);
    assert_eq!(create.supersedes_version, None);
    assert_eq!(create.amendment_reason, None);
    assert_eq!(create.actor, u(BUYER.0).to_string());
    assert_eq!(create.actor_tenant_id, u(BUYER.1));
    assert_eq!(first.items[1].reason, VersionReason::Submit);
    assert_eq!(first.items[1].supersedes_version, Some(v(1)));
    let stored =
        f.t.json(&format!(
            "SELECT to_jsonb(created_at) AS j FROM bss_orders__order_version \
             WHERE order_id='{order}' AND version=4"
        ))
        .await;
    assert_eq!(
        json!(
            first.items[1]
                .created_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        ),
        json!(
            time::OffsetDateTime::parse(
                stored.as_str().unwrap(),
                &time::format_description::well_known::Rfc3339
            )
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
        ),
        "the engine timestamp of the stored row"
    );
    let cursor = first
        .page_info
        .next_cursor
        .clone()
        .expect("a third version follows");
    let second = reads
        .list_versions(&buyer(), order, &page(2, Some(cursor.clone())))
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    let amended = &second.items[0];
    assert_eq!(i64::from(amended.version), 7);
    assert_eq!(amended.reason, VersionReason::Amendment);
    assert_eq!(amended.supersedes_version, Some(v(4)));
    assert_eq!(amended.amendment_reason.as_deref(), Some("bigger plan"));
    assert!(second.page_info.next_cursor.is_none());
    // Exactly N rows: no cursor is issued.
    let all = reads
        .list_versions(&buyer(), order, &page(3, None))
        .await
        .unwrap();
    assert_eq!(all.items.len(), 3);
    assert!(all.page_info.next_cursor.is_none());
    // The token is bound to its collection, parent and principal (read rules, D-139).
    let other = draft_tagged(&f, 1, "h-sparse-other").await.order.order_id;
    let wrong_parent = reads
        .list_versions(&buyer(), other, &page(2, Some(cursor.clone())))
        .await
        .unwrap_err();
    let wrong_collection = reads
        .get_acceptance(
            &buyer(),
            order,
            &AcceptanceList {
                limit: None,
                cursor: Some(cursor.clone()),
            },
        )
        .await
        .unwrap_err();
    let wrong_subject = reads
        .list_versions(&caller(SELLER_READER), order, &page(2, Some(cursor)))
        .await
        .unwrap_err();
    let garbage = reads
        .list_versions(
            &buyer(),
            order,
            &page(
                2,
                Some(Cursor::try_from("bm90LWEtdG9rZW4".to_owned()).unwrap()),
            ),
        )
        .await
        .unwrap_err();
    for error in [wrong_parent, wrong_collection, wrong_subject, garbage] {
        assert_eq!(refusal(&error), Reason::CursorInvalid);
    }
    assert_eq!(
        logs(&f, order).await,
        json!([]),
        "input validation precedes the decision and appends nothing; own-tenant reads log nothing"
    );
}

/// Two burned submit candidates and two burned amendment candidates leave 1 → 4 → 7.
async fn sparse_1_4_7(f: &Fixture, tag: &str) -> Uuid {
    let draft = draft_tagged(f, 1, tag).await;
    let order = draft.order.order_id;
    let line = draft.lines[0].line_id;
    f.acceptance
        .refuse
        .lock()
        .insert(line, CommercialReason::NotSellable);
    for k in ["s1", "s2"] {
        let refused = submit(f, &buyer(), &draft, &format!("{tag}-{k}"))
            .await
            .unwrap();
        assert_ne!(refused.outcome, SettledOutcome::Committed);
    }
    f.acceptance.refuse.lock().clear();
    let settled = submit(f, &buyer(), &draft, &format!("{tag}-s3"))
        .await
        .unwrap();
    assert_eq!(settled.response.body()["version"], json!(4));
    f.acceptance
        .refuse
        .lock()
        .insert(line, CommercialReason::NotSellable);
    for k in ["a1", "a2"] {
        let refused = amend(
            f,
            order,
            4,
            change_quantity(line, "2"),
            &format!("{tag}-{k}"),
        )
        .await
        .unwrap();
        assert_ne!(refused.outcome, SettledOutcome::Committed);
    }
    f.acceptance.refuse.lock().clear();
    let settled = amend(
        f,
        order,
        4,
        change_quantity(line, "2"),
        &format!("{tag}-a3"),
    )
    .await
    .unwrap();
    assert_eq!(settled.response.body()["version"], json!(7));
    assert_eq!(versions(f, order).await, json!([[1, null], [4, 1], [7, 4]]));
    order
}

/// A reserved-then-burned candidate (and any number never committed) is an authorized
/// `version-not-found`; an unauthorized caller never learns it (`order-not-found`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn burned_candidate_is_version_not_found() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let order = sparse_1_4_7(&f, "h-burn").await;
    let signals = Arc::new(super::reads::RecordingSignals::default());
    let reads = f.t.reads_with_signals(
        RulesProvider::from_policy(history_policy()),
        Arc::clone(&signals) as Arc<dyn crate::infra::read::ReadSignals>,
    );
    for number in [2, 3, 5, 6, 8, 2_147_483_647] {
        let error = reads
            .get_version(&buyer(), order, v(number))
            .await
            .unwrap_err();
        assert_eq!(refusal(&error), Reason::VersionNotFound, "{number}");
        let hidden = reads
            .get_version(&caller(STRANGER), order, v(number))
            .await
            .unwrap_err();
        assert_eq!(refusal(&hidden), Reason::OrderNotFound, "{number}");
    }
    // Every authorized `version-not-found` is counted as a refusal (D-254 item 4).
    let counted = signals.refusals();
    assert_eq!(
        counted
            .iter()
            .filter(|(r, _)| *r == Reason::VersionNotFound)
            .count(),
        6
    );
    for number in [1, 4, 7] {
        let read = reads.get_version(&buyer(), order, v(number)).await.unwrap();
        assert_eq!(i64::from(read.version), number);
        assert_eq!(read.order_id, order);
    }
}

/// DESIGN §7: a version read composes the stored rows of exactly that version (no chain walk):
/// after an amendment N and M are both readable with their own lines, pins and totals; M equals
/// the current read; version 1 is the empty draft with no totals.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn version_view_reads_m_and_n_after_an_amendment() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let reads = reads(&f);
    let (draft, n) = submitted(&f, 2, "h-view").await;
    let order = draft.order.order_id;
    let current_n = reads.get(&buyer(), order).await.unwrap();
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(draft.lines[0].line_id, "2"),
        "h-view-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let at_n = reads.get_version(&buyer(), order, v(n)).await.unwrap();
    let at_m = reads.get_version(&buyer(), order, v(m)).await.unwrap();
    let current_m = reads.get(&buyer(), order).await.unwrap();
    assert_eq!(at_n.lines, current_n.lines, "N as it was current");
    assert_eq!(at_n.resolved_total, current_n.resolved_totals);
    assert_eq!(at_m.lines, current_m.lines, "M is the current version");
    assert_eq!(at_m.resolved_total, current_m.resolved_totals);
    let quantity = |view: &bss_orders_sdk::reads::OrderVersionView| {
        serde_json::to_value(view.lines[0].selected_items[0].quantity.as_ref()).unwrap()
    };
    assert_ne!(quantity(&at_n), quantity(&at_m));
    assert_eq!(quantity(&at_m), json!("2"));
    let assessment = |view: &bss_orders_sdk::reads::OrderVersionView| {
        view.lines[0]
            .committed
            .as_ref()
            .unwrap()
            .order_pin
            .assessment_id()
    };
    assert_ne!(assessment(&at_n), assessment(&at_m));
    assert!(
        at_m.resolved_total
            .as_ref()
            .unwrap()
            .iter()
            .all(|t| t.assessment_id == assessment(&at_m))
    );
    let empty = reads.get_version(&buyer(), order, v(1)).await.unwrap();
    assert!(empty.lines.is_empty());
    assert_eq!(empty.resolved_total, None);
    // The local SDK serves the same view under the same wrapper.
    let sdk = LocalOrdersClient::new(Arc::new(f.t.service()), Arc::new(reads));
    let via_sdk = sdk
        .get_version(buyer().ctx(), order, v(n), ReadMeta::default())
        .await
        .unwrap();
    assert_eq!(via_sdk, at_n);
}

/// DESIGN §7 acceptance read: the current version, its consent or absence, and the bounded
/// prior records in version order; no recovery evidence. Consent is per version: an amended M
/// starts without it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn acceptance_page_shows_current_and_prior_consents() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let reads = reads(&f);
    let (draft, n) = submitted(&f, 1, "h-consent").await;
    let order = draft.order.order_id;
    let line = draft.lines[0].line_id;
    let stored = acceptances(&f, order).await;
    assert_eq!(stored[0]["version"], json!(n), "{stored}");
    let at_n = reads
        .get_acceptance(&buyer(), order, &AcceptanceList::default())
        .await
        .unwrap();
    assert_eq!(at_n.current_version, Some(v(n)));
    assert!(at_n.items.is_empty(), "no prior version has consent");
    let current = at_n.current_acceptance.clone().expect("N's consent");
    assert_eq!(current.accepted_version, v(n));
    assert_eq!(json!(current.recorded_by), stored[0]["recorded_by"]);
    assert_eq!(json!(current.recording_path), stored[0]["path"]);
    assert_eq!(json!(current.requirement_source), stored[0]["source"]);
    // Amend to M: M has no consent; N's is a prior record.
    let m = amend(&f, order, n, change_quantity(line, "2"), "h-consent-1")
        .await
        .unwrap()
        .response
        .body()["version"]
        .as_i64()
        .unwrap();
    let at_m = reads
        .get_acceptance(&buyer(), order, &AcceptanceList::default())
        .await
        .unwrap();
    assert_eq!(at_m.current_version, Some(v(m)));
    assert_eq!(at_m.current_acceptance, None, "absence, not N's consent");
    assert_eq!(at_m.items, vec![current.clone()]);
    // M's consent as Task 12 will record it, then a further amendment K.
    f.t.env
        .pg
        .sql(&format!(
            "INSERT INTO bss_orders__acceptance VALUES ('{order}', {m}, clock_timestamp(), \
             '{}', 'self_service', 'volunteered')",
            u(BUYER.0)
        ))
        .await
        .unwrap();
    let k = amend(&f, order, m, change_quantity(line, "3"), "h-consent-2")
        .await
        .unwrap()
        .response
        .body()["version"]
        .as_i64()
        .unwrap();
    let first = reads
        .get_acceptance(
            &buyer(),
            order,
            &AcceptanceList {
                limit: Some(PageSize::try_from(1).unwrap()),
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.current_version, Some(v(k)));
    assert_eq!(first.current_acceptance, None);
    assert_eq!(first.items, vec![current]);
    let second = reads
        .get_acceptance(
            &buyer(),
            order,
            &AcceptanceList {
                limit: Some(PageSize::try_from(1).unwrap()),
                cursor: first.page_info.next_cursor.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].accepted_version, v(m));
    assert_eq!(second.items[0].recording_path, SalesPath::SelfService);
    assert_eq!(
        second.items[0].requirement_source,
        RequirementSource::Volunteered
    );
    assert!(second.page_info.next_cursor.is_none());
    // The wire record is exactly the five consent members: no recovery evidence.
    let wire = serde_json::to_value(&second).unwrap();
    let mut keys = wire["items"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        [
            "accepted_at",
            "accepted_version",
            "recorded_by",
            "recording_path",
            "requirement_source"
        ]
    );
    // The amendments kept the aggregate's count, as the history shows.
    assert_eq!(order_row(&f, order).await["amendment_count"], json!(2));
}

/// S6-04 §4.4 on the history reads: a cross-tenant reader's every read appends a served row
/// under the catalog operation id (an authorized `version-not-found` included: it disclosed the
/// order); an own-tenant read appends nothing; a refusal appends its refused row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn history_reads_are_access_logged() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let reads = reads(&f);
    let (draft, n) = submitted(&f, 1, "h-log").await;
    let order = draft.order.order_id;
    reads
        .list_versions(&buyer(), order, &VersionList::default())
        .await
        .unwrap();
    reads.get_version(&buyer(), order, v(n)).await.unwrap();
    reads
        .get_acceptance(&buyer(), order, &AcceptanceList::default())
        .await
        .unwrap();
    assert_eq!(
        logs(&f, order).await,
        json!([]),
        "own resource tenant, no proof"
    );
    let seller = caller(SELLER_READER);
    reads
        .list_versions(&seller, order, &VersionList::default())
        .await
        .unwrap();
    reads.get_version(&seller, order, v(n)).await.unwrap();
    reads
        .get_acceptance(&seller, order, &AcceptanceList::default())
        .await
        .unwrap();
    let burned = reads
        .get_version(&seller, order, v(n + 1))
        .await
        .unwrap_err();
    assert_eq!(refusal(&burned), Reason::VersionNotFound);
    let hidden = reads
        .list_versions(&caller(STRANGER), order, &VersionList::default())
        .await
        .unwrap_err();
    assert_eq!(refusal(&hidden), Reason::OrderNotFound);
    assert_eq!(
        logs(&f, order).await,
        json!([
            ["list_versions", "served", null, order],
            ["get_version", "served", null, order],
            ["get_acceptance", "served", null, order],
            ["get_version", "served", null, order],
            ["list_versions", "refused", "order-not-found", order],
        ])
    );
}

/// The mounted history routes (in-process router over real PostgreSQL): `OpenAPI` operations,
/// the current-version `ETag` on the pages, `snake_case` bodies, the page walk, boundary
/// refusals with no access-log row, and the 404 tokens of the two not-found arms.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_mounted_history_routes_carry_etag_pages_and_404_tokens() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = crate::api::rest::read::router(Arc::new(reads(&f)), &openapi)
        .layer(axum::Extension(buyer().ctx().clone()));
    let registered = openapi
        .operation_specs
        .iter()
        .map(|spec| {
            let spec = spec.value();
            (
                spec.method.as_str().to_owned(),
                spec.path.clone(),
                spec.operation_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (path, id) in [
        (
            "/bss-orders/v1/orders/{orderId}/versions",
            "bss_orders.list_versions",
        ),
        (
            "/bss-orders/v1/orders/{orderId}/versions/{version}",
            "bss_orders.get_version",
        ),
        (
            "/bss-orders/v1/orders/{orderId}/acceptance",
            "bss_orders.get_acceptance",
        ),
    ] {
        assert!(
            registered
                .iter()
                .any(|(m, p, o)| m == "GET" && p == path && o.as_deref() == Some(id)),
            "{path}: {registered:?}"
        );
    }
    let (draft, n) = submitted(&f, 1, "h-wire").await;
    let order = draft.order.order_id;
    let base = format!("/bss-orders/v1/orders/{order}");
    let first = http(
        &router,
        "GET",
        &format!("{base}/versions?limit=1"),
        &[],
        None,
    )
    .await;
    assert_eq!(first.0, 200, "{:?}", first.2);
    assert_eq!(first.1["etag"], format!("\"{n}\"").as_str());
    assert_eq!(first.2["current_version"], json!(n));
    assert_eq!(first.2["items"][0]["version"], json!(1));
    assert_eq!(first.2["items"][0]["reason"], json!("create"));
    assert_eq!(first.2["items"][0]["actor_tenant_id"], json!(u(BUYER.1)));
    assert_eq!(first.2["page_info"]["limit"], json!(1));
    let token = first.2["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = http(
        &router,
        "GET",
        &format!("{base}/versions?limit=1&cursor={token}"),
        &[],
        None,
    )
    .await;
    assert_eq!(second.0, 200, "{:?}", second.2);
    assert_eq!(second.2["items"][0]["version"], json!(n));
    assert_eq!(second.2["items"][0]["reason"], json!("submit"));
    assert_eq!(second.2["items"][0]["supersedes_version"], json!(1));
    assert!(second.2["page_info"].get("next_cursor").is_none());
    assert_eq!(second.2["current_acceptance"]["accepted_version"], json!(n));
    let one = http(&router, "GET", &format!("{base}/versions/{n}"), &[], None).await;
    assert_eq!(one.0, 200, "{:?}", one.2);
    assert!(
        one.1.get("etag").is_none(),
        "a version is not the If-Match value"
    );
    assert_eq!(one.2["order_id"], json!(order));
    assert_eq!(one.2["version"], json!(n));
    assert_eq!(one.2["lines"][0]["line_id"], json!(draft.lines[0].line_id));
    assert!(one.2["lines"][0]["committed"]["order_pin"].is_object());
    assert!(one.2["resolved_total"].is_array());
    assert!(one.2.get("admin").is_none());
    let consent = http(&router, "GET", &format!("{base}/acceptance"), &[], None).await;
    assert_eq!(consent.0, 200, "{:?}", consent.2);
    assert_eq!(consent.1["etag"], format!("\"{n}\"").as_str());
    assert_eq!(consent.2["current_version"], json!(n));
    assert_eq!(
        consent.2["current_acceptance"]["accepted_version"],
        json!(n)
    );
    assert_eq!(consent.2["items"], json!([]));
    // The two not-found arms answer their own registered codes.
    let expect = |path: String, status: u16, code: &'static str| {
        let router = router.clone();
        async move {
            let out = http(&router, "GET", &path, &[], None).await;
            assert_eq!(out.0, status, "{path}: {:?}", out.2);
            assert_eq!(out.1["content-type"], "application/problem+json", "{path}");
            assert_eq!(out.2["error_code"], json!(code), "{path}");
        }
    };
    expect(
        format!("{base}/versions/{}", n + 1),
        404,
        "VERSION_NOT_FOUND",
    )
    .await;
    let missing = format!("/bss-orders/v1/orders/{}", Uuid::now_v7());
    expect(format!("{missing}/versions"), 404, "ORDER_NOT_FOUND").await;
    expect(format!("{missing}/versions/1"), 404, "ORDER_NOT_FOUND").await;
    expect(format!("{missing}/acceptance"), 404, "ORDER_NOT_FOUND").await;
    let logged = logs(&f, order).await;
    // Boundary refusals precede the decision and append nothing.
    for (query, code) in [
        ("limit=0", "PAGE_SIZE_EXCEEDED"),
        ("limit=201", "PAGE_SIZE_EXCEEDED"),
        ("limit=1&bogus=1", "FILTER_INVALID"),
        ("%24filter=state%20eq%20%27draft%27", "FILTER_INVALID"),
        ("cursor=a%20b", "CURSOR_INVALID"),
        ("cursor=bm90LWEtdG9rZW4", "CURSOR_INVALID"),
    ] {
        expect(format!("{base}/versions?{query}"), 400, code).await;
        expect(format!("{base}/acceptance?{query}"), 400, code).await;
    }
    for bad in ["0", "-1", "abc", "1.5", "2147483648"] {
        expect(format!("{base}/versions/{bad}"), 400, "REQUEST_INVALID").await;
    }
    assert_eq!(logs(&f, order).await, logged);
}
