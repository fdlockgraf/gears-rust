#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once; `allow`, not \
              `expect`, because not every test in the file unwraps or expects"
)]
use bss_orders_sdk::authoring::{
    AddLine, AuthoredTerm, AuthoringInvalid, CalendarDate, Currency, LinePatch, SelectedItem,
};
use time::{Date, Month};
use uuid::Uuid;

/// `time` without `large-dates` stops at year 9999, so the out-of-range side is a negative year.
fn date(year: i32) -> CalendarDate {
    CalendarDate(Date::from_calendar_date(year, Month::January, 1).unwrap())
}

fn line() -> AddLine {
    AddLine {
        plan_id: Uuid::from_u128(1),
        plan_revision_id: Uuid::from_u128(2),
        selected_items: vec![SelectedItem {
            item_id: Uuid::from_u128(3),
            quantity: None,
            selected_dim_value: None,
        }],
        currency: Currency::try_from("USD".to_owned()).unwrap(),
        contract_effective_date: None,
        service_activation_date: None,
        acceptance_due_date: None,
        term_duration: None,
        billing_cycle: None,
    }
}

#[test]
fn add_line_refuses_dates_outside_the_wire_encoding() {
    // Both ends of the four-digit encoding are valid and serialize.
    for year in [0, 9999] {
        let mut ok = line();
        ok.contract_effective_date = Some(date(year));
        ok.validate().unwrap();
        serde_json::to_value(&ok).unwrap();
    }
    for year in [-1, -9999] {
        for (field, expected) in [
            (0, "contract_effective_date"),
            (1, "service_activation_date"),
            (2, "acceptance_due_date"),
        ] {
            let mut bad = line();
            let slot = match field {
                0 => &mut bad.contract_effective_date,
                1 => &mut bad.service_activation_date,
                _ => &mut bad.acceptance_due_date,
            };
            *slot = Some(date(year));
            // The validator names the field, before anything tries to serialize the value.
            assert_eq!(bad.validate(), Err(AuthoringInvalid(expected)));
            assert!(serde_json::to_value(&bad).is_err());
        }
    }
}

#[test]
fn line_patch_refuses_dates_outside_the_wire_encoding_and_still_clears_with_null() {
    let cleared = LinePatch {
        contract_effective_date: Some(None),
        ..LinePatch::default()
    };
    cleared.validate().unwrap();
    let edge = LinePatch {
        acceptance_due_date: Some(Some(date(9999))),
        ..LinePatch::default()
    };
    edge.validate().unwrap();
    for year in [-1, -9999] {
        let cases = [
            (
                LinePatch {
                    contract_effective_date: Some(Some(date(year))),
                    ..LinePatch::default()
                },
                "contract_effective_date",
            ),
            (
                LinePatch {
                    service_activation_date: Some(Some(date(year))),
                    ..LinePatch::default()
                },
                "service_activation_date",
            ),
            (
                LinePatch {
                    acceptance_due_date: Some(Some(date(year))),
                    ..LinePatch::default()
                },
                "acceptance_due_date",
            ),
        ];
        for (patch, expected) in cases {
            assert_eq!(patch.validate(), Err(AuthoringInvalid(expected)));
            assert!(serde_json::to_value(&patch).is_err());
        }
    }
}

/// The stored interval stays inside PostgreSQL's `interval` fields: whole months and days in
/// `i32`, microseconds in `i64`, and a period count whose month total fits `i32` under either
/// cycle. Each bound is pinned just inside and just over.
#[test]
fn authored_term_bounds_are_exact() {
    const MAX_MONTHS: u32 = i32::MAX.unsigned_abs();
    let invalid = Err(AuthoringInvalid("term_duration"));
    let calendar = |years, months, days, microseconds| AuthoredTerm::Calendar {
        years,
        months,
        days,
        microseconds,
    };
    // The largest count whose month total fits `i32`.
    let max_count = 178_956_970_u32;
    assert!(max_count * 12 <= MAX_MONTHS && (max_count + 1) * 12 > MAX_MONTHS);
    let max_micros = i64::MAX.unsigned_abs();
    for (term, expected) in [
        (AuthoredTerm::Rolling, Ok(())),
        (AuthoredTerm::Periods { count: 1 }, Ok(())),
        (AuthoredTerm::Periods { count: 0 }, invalid),
        (AuthoredTerm::Periods { count: max_count }, Ok(())),
        (
            AuthoredTerm::Periods {
                count: max_count + 1,
            },
            invalid,
        ),
        (AuthoredTerm::Periods { count: u32::MAX }, invalid),
        (calendar(0, 0, 0, 0), invalid),
        (calendar(0, 0, 0, 1), Ok(())),
        (calendar(0, MAX_MONTHS, 0, 0), Ok(())),
        (calendar(0, MAX_MONTHS + 1, 0, 0), invalid),
        // Years count twelve months towards the same bound.
        (calendar(max_count, MAX_MONTHS % 12, 0, 0), Ok(())),
        (calendar(max_count, MAX_MONTHS % 12 + 1, 0, 0), invalid),
        (calendar(u32::MAX, 0, 0, 0), invalid),
        (calendar(0, 0, MAX_MONTHS, 0), Ok(())),
        (calendar(0, 0, MAX_MONTHS + 1, 0), invalid),
        (calendar(0, 0, u32::MAX, 0), invalid),
        (calendar(0, 0, 0, max_micros), Ok(())),
        (calendar(0, 0, 0, max_micros + 1), invalid),
        (calendar(0, 0, 0, u64::MAX), invalid),
    ] {
        assert_eq!(term.validate(), expected, "{term:?}");
    }
}

/// D-192, D-235 item 12 (OQ-C2): a line has one exact quantity; every stated item quantity
/// must agree, on add and on a patch that names the items.
#[test]
fn a_line_states_one_quantity_on_every_item() {
    let item = |n: u128, q: Option<&str>| SelectedItem {
        item_id: Uuid::from_u128(n),
        quantity: q.map(|q| serde_json::from_value(serde_json::json!(q)).unwrap()),
        selected_dim_value: None,
    };
    let mut same = line();
    same.selected_items = vec![item(3, Some("2")), item(4, Some("2")), item(5, None)];
    same.validate().unwrap();
    let mut mixed = line();
    mixed.selected_items = vec![item(3, Some("2")), item(4, Some("3"))];
    assert_eq!(mixed.validate(), Err(AuthoringInvalid("quantity")));
    let patch = LinePatch {
        selected_items: Some(mixed.selected_items),
        ..LinePatch::default()
    };
    assert_eq!(patch.validate(), Err(AuthoringInvalid("quantity")));
}

/// The amendment request (S4-03): `snake_case` (D-206), `op`-tagged line operations, every
/// level `deny_unknown_fields`; the header patch and line patch are the capture authoring types.
#[test]
fn amend_request_round_trips_and_rejects_unknown_keys() {
    use bss_orders_sdk::amend::{AmendLine, AmendOrder};
    use bss_orders_sdk::authoring::HeaderPatch;
    let removed = Uuid::from_u128(7);
    let changed = Uuid::from_u128(8);
    let request = AmendOrder {
        reason: "customer asked for a larger plan".to_owned(),
        header: Some(HeaderPatch {
            display_label: Some(Some("renewal".to_owned())),
            ..HeaderPatch::default()
        }),
        lines: vec![
            AmendLine::Add { line: line() },
            AmendLine::Change {
                line_id: changed,
                patch: LinePatch {
                    currency: Some(Currency::try_from("EUR".to_owned()).unwrap()),
                    ..LinePatch::default()
                },
            },
            AmendLine::Remove { line_id: removed },
        ],
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["lines"][0]["op"], "add");
    assert_eq!(wire["lines"][1]["op"], "change");
    assert_eq!(wire["lines"][1]["line_id"], changed.to_string());
    assert_eq!(wire["lines"][2]["op"], "remove");
    assert_eq!(wire["header"]["display_label"], "renewal");
    let back: AmendOrder = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(back, request);

    // Omitted header and lines default; the reason alone decodes (the engine guards it).
    let bare: AmendOrder = serde_json::from_value(serde_json::json!({"reason": "r"})).unwrap();
    assert_eq!(bare.header, None);
    assert!(bare.lines.is_empty());

    // Unknown keys are refused at the request, the header, and every line operation.
    let mut top = wire.clone();
    top["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<AmendOrder>(top).is_err());
    let mut header = wire.clone();
    header["header"]["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<AmendOrder>(header).is_err());
    for index in 0..3 {
        let mut op = wire.clone();
        op["lines"][index]["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<AmendOrder>(op).is_err(), "{index}");
    }
    let mut unknown_op = wire;
    unknown_op["lines"][2]["op"] = serde_json::json!("replace");
    assert!(serde_json::from_value::<AmendOrder>(unknown_op).is_err());
}

/// S4-08b (`models.json` `Acceptance`, D-255): the record-acceptance body has no member. The
/// instant and the recording actor are server facts, so a caller-supplied timestamp or actor is
/// an unknown member the wire refuses.
#[test]
fn the_acceptance_body_carries_no_member() {
    use bss_orders_sdk::acceptance::Acceptance;
    let body: Acceptance = serde_json::from_str("{}").unwrap();
    assert_eq!(serde_json::to_value(body).unwrap(), serde_json::json!({}));
    for refused in [
        r#"{"accepted_at":"2026-10-29T10:00:00Z"}"#,
        r#"{"recorded_by":"00000000-0000-0000-0000-000000000066"}"#,
        r#"{"accepted_version":2}"#,
        "[]",
        "null",
    ] {
        assert!(
            serde_json::from_str::<Acceptance>(refused).is_err(),
            "{refused}"
        );
    }
}

/// S4-10 (`models.json` `AuthorizationOutcome`, D-256): the begin-fulfillment payment outcome is
/// exactly `{ "outcome": "authorized" | "pending" | "failed" }`. An unknown outcome, a missing
/// one or any other member is refused at the wire, so it can never reach the tolerance decision.
#[test]
fn the_authorization_outcome_is_closed() {
    use bss_orders_sdk::fulfillment::{Authorization, AuthorizationOutcome};
    for (token, outcome) in [
        ("authorized", Authorization::Authorized),
        ("pending", Authorization::Pending),
        ("failed", Authorization::Failed),
    ] {
        let wire = serde_json::json!({ "outcome": token });
        let parsed: AuthorizationOutcome = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(parsed, AuthorizationOutcome { outcome });
        assert_eq!(serde_json::to_value(parsed).unwrap(), wire);
    }
    for refused in [
        r#"{"outcome":"unknown"}"#,
        r#"{"outcome":"declined"}"#,
        r#"{"outcome":null}"#,
        r#"{}"#,
        r#"{"outcome":"failed","token":"tok_1"}"#,
        "[]",
        "null",
    ] {
        assert!(
            serde_json::from_str::<AuthorizationOutcome>(refused).is_err(),
            "{refused}"
        );
    }
}
