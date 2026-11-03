//! S4-06 history reads on real PostgreSQL (Stage 4 DESIGN §7; D-255): `list_versions`,
//! `get_version` and `get_acceptance` through the shared read wrapper over submitted and amended
//! orders of the submit fixture. Authorization is decided on the current parent before any
//! existence signal; pages hold committed rows only; a burned candidate is `version-not-found`;
//! a version is composed at that version; every read follows the access-log decision table under
//! its catalog operation id. Compiled only with `gate-dev-stubs`.
use super::acceptance::{PARTNER, meta, partner, world};
use super::amend::{amend, change_quantity, order_row, submitted};
use super::capture::{BUYER, buyer, http, write};
use super::submit::{
    Fixture, RESOURCE, SELLER, acceptances, draft_tagged, setup_with, submit, two_line_fixture,
    versions,
};
use super::*;
use crate::api::local_client::LocalOrdersClient;
use crate::authz::test_pdp::{FakePdp, Grant, user};
use crate::authz::{Action, Caller};
use crate::infra::capture::SettledOutcome;
use crate::infra::gate::tests::orchestrate::{PAYER, fixture};
use crate::infra::read::ReadService;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::OrdersV1;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::authoring::{Category, HeaderPatch, LinePatch, SalesPath};
use bss_orders_sdk::catalog::Reason;
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::{
    AcceptanceList, AdministrativeValues, AdministrativeView, Cursor, LineAdministrativeValues,
    PageSize, ReadMeta, RequirementSource, VersionActor, VersionActors, VersionList, VersionReason,
    VersionRecord,
};
use bss_pricing_sdk::acceptance::CommercialReason;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Seller operator on the fixture's seller tenant: a cross-tenant reader (logged).
const SELLER_READER: (u128, u128) = (103, SELLER);
/// Payer reader: reads the orders whose *current* payer is the fixture payer.
const PAYER_READER: (u128, u128) = (105, 0xa1);
/// A principal no rule names.
const STRANGER: (u128, u128) = (104, 10);
/// The amendment's new payer (confirmed by the resource relationship stub).
const OTHER_PAYER: u128 = 31;

fn read(subject: u128, property: &str, value: Uuid) -> Grant {
    Grant::new(u(subject), &[Action::OrderRead]).path([(property, vec![value])])
}

/// The policy fake (D-227, D-248), `order × read` only: history needs no audit permission.
fn history_policy() -> Arc<FakePdp> {
    FakePdp::new(
        vec![
            read(BUYER.0, "resource_tenant_id", u(RESOURCE)),
            read(SELLER_READER.0, "seller_tenant_id", u(SELLER)),
            read(PAYER_READER.0, "payer_tenant_id", PAYER),
        ],
        &[],
    )
}
fn reads(f: &Fixture) -> ReadService {
    f.t.reads_with(history_policy())
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

/// D-188/D-255: the history page holds committed rows only, in version order, without
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
        history_policy(),
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
    // Every authorized `version-not-found` is counted as a refusal (D-255 item 4).
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
    assert!(
        one.2.get("admin").is_none(),
        "no administrative row is stored (D-263)"
    );
    // D-260, D-262: the route serves the version metadata, header and role actors.
    assert_eq!(one.2["reason"], json!("submit"));
    assert_eq!(one.2["supersedes_version"], json!(1));
    assert_eq!(one.2["header"]["payer_tenant_id"], json!(PAYER));
    assert_eq!(
        one.2["actors"],
        json!({
            "creator": {"actor": u(BUYER.0).to_string(), "actor_tenant_id": u(BUYER.1)},
            "submitter": {"actor": u(BUYER.0).to_string(), "actor_tenant_id": u(BUYER.1)}
        })
    );
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

// -------------------------------------------------------------------------------------------
// D-260, D-262, D-263: the version read carries its metadata, its header at that version, its
// role actors, and both reads carry the current administrative projection.

/// A stored role pair as the version read spells it.
fn pair(who: (u128, u128)) -> VersionActor {
    VersionActor {
        actor: u(who.0).to_string(),
        actor_tenant_id: u(who.1),
    }
}

/// D-260 items 3-4, 04 §4.5: each version kind names exactly its role actors. Version 1 names
/// only the creator; the submit version adds the submitter; an amendment adds its own amender
/// (here a partner of another tenant), while the creator and submitter stay those of version 1
/// and the submit version.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn version_read_names_its_role_actors_for_each_version_kind() {
    let w = world(fixture()).await;
    let (order, n) = w.submitted(&buyer(), &buyer(), None, "va-kinds").await;
    let draft = w.reads.get(&buyer(), order).await.unwrap();
    let request: AmendOrder =
        serde_json::from_value(change_quantity(draft.lines[0].line_id, "2")).unwrap();
    let amended = w
        .amender()
        .amend(
            &partner(),
            order,
            request,
            &meta("va-kinds-amend", n),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(amended.outcome, SettledOutcome::Committed, "{amended:?}");
    let m = amended.response.body()["version"].as_i64().unwrap();
    let actors = |version: i64| {
        let reads = &w.reads;
        async move {
            reads
                .get_version(&buyer(), order, v(version))
                .await
                .unwrap()
                .actors
        }
    };
    assert_eq!(
        actors(1).await,
        VersionActors {
            creator: pair(BUYER),
            submitter: None,
            amender: None,
        },
        "version 1: the creator only"
    );
    assert_eq!(
        actors(n).await,
        VersionActors {
            creator: pair(BUYER),
            submitter: Some(pair(BUYER)),
            amender: None,
        },
        "the submit version: no amender"
    );
    assert_eq!(
        actors(m).await,
        VersionActors {
            creator: pair(BUYER),
            submitter: Some(pair(BUYER)),
            amender: Some(pair(PARTNER)),
        },
        "the amendment: its own appender is the amender"
    );
}

/// D-260 item 2 (delegation introduced after creation): a delegate of another tenant who submits
/// is the submitter with its own tenant, the creator stays the customer, and version 1 has no
/// submitter. The pairs are the stored rows the recording-party bar reads (D-256).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_delegated_submit_names_the_delegate_as_submitter() {
    let w = world(fixture()).await;
    let (order, n) = w.submitted(&buyer(), &partner(), None, "va-dl").await;
    let at_n = w.reads.get_version(&buyer(), order, v(n)).await.unwrap();
    assert_eq!(
        at_n.actors,
        VersionActors {
            creator: pair(BUYER),
            submitter: Some(pair(PARTNER)),
            amender: None,
        }
    );
    assert_eq!(
        (at_n.actor.clone(), at_n.actor_tenant_id),
        (u(PARTNER.0).to_string(), u(PARTNER.1)),
        "the submit version's own appender is the delegate"
    );
    let at_1 = w.reads.get_version(&buyer(), order, v(1)).await.unwrap();
    assert_eq!(at_1.actors.submitter, None);
    assert_eq!(at_1.actors.creator, pair(BUYER));
}

/// D-260 item 3: a role row the requested version needs and the store lacks (a submitted order
/// whose submit row is gone) is an integration failure, never an absent member. Version 1 needs
/// no submitter and still reads.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_version_read_without_its_submit_row_is_an_integration_failure() {
    let w = world(fixture()).await;
    let (order, n) = w.submitted(&buyer(), &buyer(), None, "va-missing").await;
    w.f.t
        .env
        .pg
        .sql(&format!(
            "ALTER TABLE bss_orders__order_version DISABLE TRIGGER USER; \
             UPDATE bss_orders__order_version SET reason='amendment', \
             amendment_reason='stand-in' WHERE order_id='{order}' AND reason='submit'; \
             ALTER TABLE bss_orders__order_version ENABLE TRIGGER USER;"
        ))
        .await
        .unwrap();
    let answer = w.reads.get_version(&buyer(), order, v(n)).await;
    assert!(
        matches!(answer, Err(OrdersError::Integration)),
        "{answer:?}"
    );
    let at_1 = w.reads.get_version(&buyer(), order, v(1)).await.unwrap();
    assert_eq!(at_1.actors.submitter, None);
}

/// D-260 item 3, the creator half: every version read needs version 1's `create` row, so a store
/// whose version 1 is missing that reason (here mis-reasoned to `amendment`, past the shape
/// constraint this test drops) fails every version read as an integration failure, version 1's
/// own included, and never names an absent or wrong creator.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_version_read_without_its_creator_row_is_an_integration_failure() {
    let w = world(fixture()).await;
    let (order, n) = w.submitted(&buyer(), &buyer(), None, "va-no-creator").await;
    w.f.t
        .env
        .pg
        .sql(&format!(
            "ALTER TABLE bss_orders__order_version \
             DROP CONSTRAINT ck_bss_orders__order_version__1; \
             ALTER TABLE bss_orders__order_version DISABLE TRIGGER USER; \
             UPDATE bss_orders__order_version SET reason='amendment', \
             amendment_reason='stand-in' WHERE order_id='{order}' AND version=1; \
             ALTER TABLE bss_orders__order_version ENABLE TRIGGER USER;"
        ))
        .await
        .unwrap();
    for number in [1, n] {
        let answer = w.reads.get_version(&buyer(), order, v(number)).await;
        assert!(
            matches!(answer, Err(OrdersError::Integration)),
            "version {number}: {answer:?}"
        );
    }
}

/// D-262, 04 flow 2.2 step 4 and §4.5: the version read returns the version row's actor,
/// timestamp, machine reason, explanation and predecessor (exactly its `VersionRecord` on the
/// list page) and the commercial header at that version: after a payer amendment N names N's
/// payer and M names M's; the market is the stored one; version 1, never gated, has none.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn version_read_carries_the_version_metadata_and_header_at_that_version() {
    let f = Box::pin(payer_fixture()).await;
    let reads = reads(&f);
    let (view, n) = submitted(&f, 1, "va-meta").await;
    let order = view.order.order_id;
    let settled = amend(
        &f,
        order,
        n,
        json!({"reason": "new payer", "header": {"payer_tenant_id": u(OTHER_PAYER)}}),
        "va-meta-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let history = reads
        .list_versions(&buyer(), order, &VersionList::default())
        .await
        .unwrap();
    for record in &history.items {
        let read = reads
            .get_version(&buyer(), order, record.version)
            .await
            .unwrap();
        let metadata = VersionRecord {
            version: read.version,
            reason: read.reason,
            actor: read.actor.clone(),
            actor_tenant_id: read.actor_tenant_id,
            created_at: read.created_at,
            amendment_reason: read.amendment_reason.clone(),
            supersedes_version: read.supersedes_version,
        };
        assert_eq!(
            &metadata, record,
            "the point read carries the list's metadata"
        );
        let stored =
            f.t.json(&format!(
                "SELECT jsonb_build_array(market_currency, market_region) AS j \
                 FROM bss_orders__order_version WHERE order_id='{order}' \
                 AND version={}",
                i64::from(record.version)
            ))
            .await;
        let market = read
            .header
            .market
            .as_ref()
            .map(|market| json!([market.currency.as_str(), market.region]));
        assert_eq!(market.unwrap_or(json!([null, null])), stored);
        assert_eq!(read.header.category, Category::NewSale);
        assert_eq!(read.header.resource_tenant_id, u(RESOURCE));
        assert_eq!(read.header.seller_tenant_id, u(SELLER));
        assert_eq!(read.header.contract_id, None);
    }
    let at = |number: i64| {
        let reads = &reads;
        async move { reads.get_version(&buyer(), order, v(number)).await.unwrap() }
    };
    let (at_1, at_n, at_m) = (at(1).await, at(n).await, at(m).await);
    assert_eq!(at_1.reason, VersionReason::Create);
    assert_eq!(at_1.header.market, None, "version 1 was never gated");
    assert_eq!(at_1.header.payer_tenant_id, PAYER);
    assert_eq!(at_n.reason, VersionReason::Submit);
    assert_eq!(at_n.supersedes_version, Some(v(1)));
    assert!(at_n.header.market.is_some());
    assert_eq!(at_n.header.payer_tenant_id, PAYER, "N as it was");
    assert_eq!(at_m.reason, VersionReason::Amendment);
    assert_eq!(at_m.amendment_reason.as_deref(), Some("new payer"));
    assert_eq!(at_m.supersedes_version, Some(v(n)));
    assert_eq!(at_m.header.payer_tenant_id, u(OTHER_PAYER), "M's own payer");
}

/// D-263, 04 flow 2.2 step 2 and §4.6: the order read and the version read carry the order's and
/// the composed lines' **current** administrative values (`admin`); absent before any
/// administrative row exists. The projection is current on every version: an older version shows
/// today's values, never the values at that version (those are the audit trail). Every
/// `order × read` principal sees it alike: no per-principal redaction (D-263 item 3).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn order_and_version_reads_carry_the_current_administrative_projection() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let reads = reads(&f);
    let (draft, n) = submitted(&f, 2, "va-admin").await;
    let order = draft.order.order_id;
    let (first, second) = (draft.lines[0].line_id, draft.lines[1].line_id);
    assert_eq!(reads.get(&buyer(), order).await.unwrap().admin, None);
    assert_eq!(
        reads
            .get_version(&buyer(), order, v(n))
            .await
            .unwrap()
            .admin,
        None
    );
    let header = |reference: &str| HeaderPatch {
        external_reference: Some(Some(reference.to_owned())),
        ..HeaderPatch::default()
    };
    let edited = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header("PO-1"),
            &write("va-admin-1", n, None),
        )
        .await
        .unwrap();
    assert_eq!(edited.outcome, SettledOutcome::Committed, "{edited:?}");
    let label = LinePatch {
        display_label: Some(Some("rack 7".to_owned())),
        internal_notes: Some(Some("call first".to_owned())),
        ..LinePatch::default()
    };
    let edited = f
        .capture
        .patch_line(
            &buyer(),
            order,
            second,
            label,
            &write("va-admin-2", n, None),
        )
        .await
        .unwrap();
    assert_eq!(edited.outcome, SettledOutcome::Committed, "{edited:?}");
    let expected = |reference: &str| AdministrativeView {
        order: Some(AdministrativeValues {
            external_reference: Some(reference.to_owned()),
            display_label: None,
            internal_notes: None,
        }),
        lines: vec![LineAdministrativeValues {
            line_id: second,
            external_reference: None,
            display_label: Some("rack 7".to_owned()),
            internal_notes: Some("call first".to_owned()),
        }],
    };
    let current = reads.get(&buyer(), order).await.unwrap();
    assert_eq!(current.admin, Some(expected("PO-1")));
    assert!(current.lines.iter().any(|line| line.line_id == first));
    let at_n = reads.get_version(&buyer(), order, v(n)).await.unwrap();
    assert_eq!(at_n.admin, Some(expected("PO-1")));
    let at_1 = reads.get_version(&buyer(), order, v(1)).await.unwrap();
    assert_eq!(
        at_1.admin,
        Some(AdministrativeView {
            lines: Vec::new(),
            ..expected("PO-1")
        }),
        "version 1 has no lines, so no line values"
    );
    // A later edit shows on the older version too: the projection is current, not historical.
    let settled = amend(&f, order, n, change_quantity(first, "2"), "va-admin-3")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let edited = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header("PO-2"),
            &write("va-admin-4", m, None),
        )
        .await
        .unwrap();
    assert_eq!(edited.outcome, SettledOutcome::Committed, "{edited:?}");
    for who in [buyer(), caller(SELLER_READER), caller(PAYER_READER)] {
        assert_eq!(
            reads.get(&who, order).await.unwrap().admin,
            Some(expected("PO-2"))
        );
        assert_eq!(
            reads.get_version(&who, order, v(n)).await.unwrap().admin,
            Some(expected("PO-2")),
            "N shows today's values"
        );
    }
}

/// D-263 on a draft: the order read composes the working lines' administrative values; a line
/// value set before submit is carried by the line identity, so it stays on the submitted read.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_draft_read_carries_its_working_lines_administrative_values() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let reads = reads(&f);
    let draft = draft_tagged(&f, 1, "va-draft").await;
    let order = draft.order.order_id;
    let line = draft.lines[0].line_id;
    let reference = LinePatch {
        external_reference: Some(Some("ext-1".to_owned())),
        ..LinePatch::default()
    };
    let edited = f
        .capture
        .patch_line(
            &buyer(),
            order,
            line,
            reference,
            &write("va-draft-1", 1, None),
        )
        .await
        .unwrap();
    assert_eq!(edited.outcome, SettledOutcome::Committed, "{edited:?}");
    let expected = AdministrativeView {
        order: None,
        lines: vec![LineAdministrativeValues {
            line_id: line,
            external_reference: Some("ext-1".to_owned()),
            display_label: None,
            internal_notes: None,
        }],
    };
    let read = reads.get(&buyer(), order).await.unwrap();
    assert_eq!(read.admin, Some(expected.clone()));
    assert_eq!(
        read.draft_revision, draft.draft_revision,
        "no draft revision bump"
    );
    let settled = submit(&f, &buyer(), &read, "va-draft-submit")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(
        reads.get(&buyer(), order).await.unwrap().admin,
        Some(expected)
    );
}
