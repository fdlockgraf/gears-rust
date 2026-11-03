//! The `amend` body at the REST boundary (S4-03): the wire is `snake_case` and equals the SDK
//! serialization; unknown keys, malformed scalars and a line named twice are `request-invalid`
//! before authorization, the engine, audit and idempotency; a bad or missing reason is not a
//! boundary failure (the engine guard `amendment-reason-invalid` decides it).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once"
)]
use bss_orders::api::rest::amend::amend_body;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::{AmendLine, AmendOrder};
use bss_orders_sdk::catalog::Reason;
use serde_json::{Value, json};

fn add() -> Value {
    json!({
        "op": "add",
        "line": {
            "plan_id": "00000000-0000-0000-0000-000000000001",
            "plan_revision_id": "00000000-0000-0000-0000-000000000002",
            "selected_items": [{"item_id": "00000000-0000-0000-0000-000000000003"}],
            "currency": "EUR",
            "billing_cycle": "month",
            "term_duration": {"kind": "rolling"}
        }
    })
}

fn decode(body: &Value) -> Result<AmendOrder, OrdersError> {
    amend_body(&serde_json::to_vec(body).unwrap())
}

fn refused(body: &Value) -> bool {
    matches!(
        decode(body),
        Err(OrdersError::Refused(Reason::RequestInvalid))
    )
}

const LINE_A: &str = "00000000-0000-0000-0000-00000000000a";

#[test]
fn a_well_formed_body_decodes_into_the_sdk_request() {
    let body = json!({
        "reason": "bigger plan",
        "header": {"display_label": "renewal", "internal_notes": null},
        "lines": [
            add(),
            {"op": "change", "line_id": LINE_A, "patch": {"currency": "USD", "term_duration": null}},
            {"op": "remove", "line_id": "00000000-0000-0000-0000-00000000000b"}
        ]
    });
    let request = decode(&body).unwrap();
    assert_eq!(request.reason, "bigger plan");
    let header = request.header.as_ref().unwrap();
    assert_eq!(header.display_label, Some(Some("renewal".into())));
    assert_eq!(header.internal_notes, Some(None));
    assert!(matches!(request.lines[0], AmendLine::Add { .. }));
    assert!(matches!(request.lines[1], AmendLine::Change { .. }));
    assert!(matches!(request.lines[2], AmendLine::Remove { .. }));
    // The REST wire is the SDK serialization (one wire, D-206).
    let sdk = serde_json::to_value(&request).unwrap();
    assert_eq!(sdk["lines"][1]["op"], "change");
    assert_eq!(sdk["lines"][2]["op"], "remove");
}

#[test]
fn unknown_keys_malformed_scalars_and_non_objects_are_request_invalid() {
    let base = json!({"reason": "r", "lines": [add()]});
    assert!(decode(&base).is_ok());
    let mut top = base.clone();
    top["extra"] = json!(1);
    assert!(refused(&top));
    let mut header = base.clone();
    header["header"] = json!({"extra": 1});
    assert!(refused(&header));
    let mut op = base.clone();
    op["lines"][0]["extra"] = json!(1);
    assert!(refused(&op));
    let mut line = base.clone();
    line["lines"][0]["line"]["extra"] = json!(1);
    assert!(refused(&line));
    // An amendment line is not a draft write: no draft revision.
    let mut revision = base.clone();
    revision["lines"][0]["line"]["expected_draft_revision"] = json!(0);
    assert!(refused(&revision));
    let mut currency = base.clone();
    currency["lines"][0]["line"]["currency"] = json!("euro");
    assert!(refused(&currency));
    let mut unknown_op = base;
    unknown_op["lines"][0]["op"] = json!("replace");
    assert!(refused(&unknown_op));
    assert!(refused(
        &json!({"reason": "r", "lines": [{"op": "remove"}]})
    ));
    assert!(refused(&json!([1])));
    assert!(matches!(
        amend_body(b"not json"),
        Err(OrdersError::Refused(Reason::RequestInvalid))
    ));
    assert!(matches!(
        amend_body(b""),
        Err(OrdersError::Refused(Reason::RequestInvalid))
    ));
}

#[test]
fn a_line_id_named_twice_is_request_invalid() {
    let patch = json!({"currency": "USD"});
    for lines in [
        json!([{"op": "remove", "line_id": LINE_A}, {"op": "remove", "line_id": LINE_A}]),
        json!([
            {"op": "change", "line_id": LINE_A, "patch": patch},
            {"op": "remove", "line_id": LINE_A}
        ]),
        json!([
            {"op": "change", "line_id": LINE_A, "patch": patch},
            {"op": "change", "line_id": LINE_A, "patch": patch}
        ]),
    ] {
        assert!(refused(&json!({"reason": "r", "lines": lines})), "{lines}");
    }
    // Two additions name no existing line and are legal.
    assert!(decode(&json!({"reason": "r", "lines": [add(), add()]})).is_ok());
}

#[test]
fn a_bad_or_missing_reason_passes_the_boundary_for_the_engine_guard() {
    for body in [
        json!({"lines": [add()]}),
        json!({"reason": "", "lines": [add()]}),
        json!({"reason": "x".repeat(5000), "lines": [add()]}),
        json!({"reason": "   ", "lines": [add()]}),
    ] {
        assert!(decode(&body).is_ok(), "{body}");
    }
    // A reason of the wrong type is a malformed body.
    assert!(refused(&json!({"reason": 7})));
    // An empty delta is the engine's `amendment-empty`, not a boundary failure.
    assert!(decode(&json!({"reason": "r"})).is_ok());
}
