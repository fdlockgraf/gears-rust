//! S4-05 on real PostgreSQL: administrative order and line edits through the capture service and
//! the S2-04 engine (04 §3.2 steps 2-3, §4.5-4.6): last write wins per field, no version, no
//! event, one audit entry per changed field. Built on the submit fixture, so the committed
//! lines are the ones a real submit wrote. Compiled only with `gate-dev-stubs`.
use super::capture::{buyer, refused, write};
use super::submit::{Fixture, draft, setup_with, submit, two_line_fixture};
use super::*;
use crate::infra::capture::SettledOutcome;
use bss_orders_sdk::authoring::{HeaderPatch, LinePatch};
use bss_orders_sdk::catalog::Reason;
use serde_json::{Value, json};

/// A submitted order with one committed line: its id, the line id and the committed version.
async fn submitted(f: &Fixture) -> (Uuid, Uuid, i64) {
    let v = draft(f, 1).await;
    let order = v.order.order_id;
    let line = v.lines[0].line_id;
    let settled = submit(f, &buyer(), &v, "adm-submit").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let version = f.t.order_row(order).await["current_version"]
        .as_i64()
        .unwrap();
    (order, line, version)
}

fn header(reference: Option<&str>, label: Option<&str>, notes: Option<&str>) -> HeaderPatch {
    HeaderPatch {
        external_reference: reference.map(|v| Some(v.to_owned())),
        display_label: label.map(|v| Some(v.to_owned())),
        internal_notes: notes.map(|v| Some(v.to_owned())),
        ..HeaderPatch::default()
    }
}

async fn committed_fields(f: &Fixture, order: Uuid) -> Value {
    f.t.json(&format!(
        "SELECT coalesce(jsonb_agg(changed_field ORDER BY sequence), '[]'::jsonb) AS j \
         FROM bss_orders__transition_audit \
         WHERE order_id='{order}' AND trigger='administrative-edit' AND outcome='committed'"
    ))
    .await
}

async fn audit_rows(f: &Fixture, order: Uuid) -> i64 {
    f.t.audits(order).await
}

/// The aggregate counters a no-version write must leave alone.
async fn counters(f: &Fixture, order: Uuid) -> Value {
    f.t.json(&format!(
        "SELECT jsonb_build_array(current_version, version_allocation_high_water, draft_revision, amendment_count, state) AS j \
         FROM bss_orders__order WHERE order_id='{order}'"
    ))
    .await
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn administrative_header_edit_writes_one_audit_entry_per_changed_field() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    // The stored notes the later edit repeats.
    let seeded = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(None, None, Some("n")),
            &write("a-0", version, None),
        )
        .await
        .unwrap();
    assert_eq!(seeded.outcome, SettledOutcome::Committed, "{seeded:?}");
    let before = counters(&f, order).await;
    let outbox = f.t.outbox().await;
    let audits = audit_rows(&f, order).await;
    let r = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("PO-1"), Some("L"), Some("n")),
            &write("a-1", version, None),
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, SettledOutcome::Committed, "{r:?}");
    assert_eq!(r.response.status(), 200);
    assert_eq!(
        committed_fields(&f, order).await,
        json!(["internal_notes", "external_reference", "display_label"])
    );
    assert_eq!(audit_rows(&f, order).await, audits + 2);
    assert_eq!(counters(&f, order).await, before);
    assert_eq!(f.t.outbox().await, outbox, "no event");
    assert_eq!(
        f.t.json(&format!(
            "SELECT to_jsonb(external_reference) AS j FROM bss_orders__order_admin WHERE order_id='{order}'"
        ))
        .await,
        json!("PO-1")
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn administrative_line_edit_checks_membership_against_committed_lines() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, line, version) = submitted(&f).await;
    let patch = |label: &str| LinePatch {
        display_label: Some(Some(label.to_owned())),
        ..LinePatch::default()
    };
    let r = f
        .capture
        .patch_line(
            &buyer(),
            order,
            line,
            patch("l"),
            &write("l-1", version, None),
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, SettledOutcome::Committed, "{r:?}");
    assert_eq!(
        committed_fields(&f, order).await,
        json!([format!("lines/{}/display_label", line.hyphenated())])
    );
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_line_admin WHERE order_id='{order}' AND line_id='{line}'"
        ))
        .await,
        1
    );
    let unknown = f
        .capture
        .patch_line(
            &buyer(),
            order,
            u(4242),
            patch("l"),
            &write("l-2", version, None),
        )
        .await
        .unwrap();
    assert_eq!(refused(&unknown), Reason::LineNotFound);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn all_unchanged_admin_edit_is_an_audited_refusal() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    let first = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("PO-1"), None, None),
            &write("u-1", version, None),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, SettledOutcome::Committed);
    let audits = audit_rows(&f, order).await;
    let stored =
        f.t.json(&format!(
            "SELECT to_jsonb(a) AS j FROM bss_orders__order_admin a WHERE order_id='{order}'"
        ))
        .await;
    let r = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("PO-1"), None, None),
            &write("u-2", version, None),
        )
        .await
        .unwrap();
    assert_eq!(refused(&r), Reason::AdministrativeEditUnchanged);
    assert_eq!(
        audit_rows(&f, order).await,
        audits + 1,
        "one refused audit row"
    );
    assert_eq!(
        committed_fields(&f, order).await,
        json!(["external_reference"])
    );
    assert_eq!(
        f.t.json(&format!(
            "SELECT to_jsonb(a) AS j FROM bss_orders__order_admin a WHERE order_id='{order}'"
        ))
        .await,
        stored,
        "no administrative write"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn same_key_admin_replay_adds_no_audit() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    let (caller, patch, meta) = (
        buyer(),
        header(Some("PO-9"), None, None),
        write("r-1", version, None),
    );
    let call = || f.capture.patch_order(&caller, order, patch.clone(), &meta);
    let first = call().await.unwrap();
    assert!(!first.replayed);
    let audits = audit_rows(&f, order).await;
    let again = call().await.unwrap();
    assert!(again.replayed, "{again:?}");
    assert_eq!(again.response.body(), first.response.body());
    assert_eq!(audit_rows(&f, order).await, audits);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn stale_expected_version_refuses_admin_edit_despite_lww() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    let r = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("PO-1"), None, None),
            &write("s-1", version - 1, None),
        )
        .await
        .unwrap();
    assert_eq!(r.response.status(), 412, "{r:?}");
    assert_eq!(refused(&r), Reason::PreconditionFailed);
    assert_eq!(
        f.t.n(&format!(
            "SELECT count(*) AS n FROM bss_orders__order_admin WHERE order_id='{order}'"
        ))
        .await,
        0
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn terminal_order_refuses_admin_edit() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    f.t.env
        .pg
        .sql(&format!(
            "UPDATE bss_orders__order SET state='cancelled' WHERE order_id='{order}'"
        ))
        .await
        .unwrap();
    let r = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("PO-1"), None, None),
            &write("t-1", version, None),
        )
        .await
        .unwrap();
    assert_eq!(refused(&r), Reason::NotAdmissible);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_same_field_admin_edits_reconstruct_the_value_chain() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    let seed = f
        .capture
        .patch_order(
            &buyer(),
            order,
            header(Some("A"), None, None),
            &write("c-0", version, None),
        )
        .await
        .unwrap();
    assert_eq!(seed.outcome, SettledOutcome::Committed);
    let caller = buyer();
    let (pb, pc) = (header(Some("B"), None, None), header(Some("C"), None, None));
    let (mb, mc) = (write("c-b", version, None), write("c-c", version, None));
    let (b, c) = tokio::join!(
        f.capture.patch_order(&caller, order, pb, &mb),
        f.capture.patch_order(&caller, order, pc, &mc),
    );
    for r in [b.unwrap(), c.unwrap()] {
        assert_eq!(r.outcome, SettledOutcome::Committed, "{r:?}");
    }
    let chain =
        f.t.json(&format!(
            "SELECT jsonb_agg(jsonb_build_array(prior_value, new_value) ORDER BY sequence) AS j \
             FROM bss_orders__transition_audit WHERE order_id='{order}' \
             AND trigger='administrative-edit' AND outcome='committed'"
        ))
        .await;
    let rows = chain.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{chain}");
    assert_eq!(rows[0][0], Value::Null);
    assert_eq!(rows[1][0], rows[0][1], "{chain}");
    assert_eq!(rows[2][0], rows[1][1], "{chain}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn mixed_admin_and_commercial_patch_routes_to_draft_mutate() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (order, _, version) = submitted(&f).await;
    let r = f
        .capture
        .patch_order(
            &buyer(),
            order,
            HeaderPatch {
                contract_id: Some(None),
                display_label: Some(Some("x".into())),
                ..HeaderPatch::default()
            },
            &write("m-1", version, None),
        )
        .await
        .unwrap();
    assert_eq!(refused(&r), Reason::NotAdmissible);
    assert_eq!(committed_fields(&f, order).await, json!([]));
}
