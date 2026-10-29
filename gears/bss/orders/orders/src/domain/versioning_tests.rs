#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once"
)]
use super::*;
use crate::domain::contributions::tests::facts;
use crate::domain::guards::{GUARDS, GuardRegistry};
use crate::domain::state_table::{RowId, StateTable};
use crate::domain::transition::{
    Decision, DraftRevisions, EngineRefusal, InputState, Inputs, decide,
};
use bss_orders_sdk::authoring::{BillingCycle, CalendarDate, Currency, SelectedItem};
use bss_orders_sdk::catalog::{OrderState as S, Reason, Trigger};
use bss_orders_sdk::commercial::OrderPin;
use bss_orders_sdk::reads::CommittedLine;
use time::OffsetDateTime;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn items(quantity: &str) -> Vec<SelectedItem> {
    vec![SelectedItem {
        item_id: id(500),
        quantity: Some(serde_json::from_value(serde_json::json!(quantity)).unwrap()),
        selected_dim_value: None,
    }]
}

fn line(line_id: Uuid, quantity: &str) -> DraftLine {
    DraftLine {
        committed: None,
        line_id,
        plan_id: id(300),
        plan_revision_id: id(301),
        selected_items: items(quantity),
        currency: Currency::try_from("EUR".to_owned()).unwrap(),
        contract_effective_date: Some(CalendarDate(
            time::Date::from_calendar_date(2026, time::Month::November, 1).unwrap(),
        )),
        service_activation_date: None,
        acceptance_due_date: None,
        term_duration: None,
        billing_cycle: Some(BillingCycle::Month),
    }
}

/// The SDK's golden schema-2 pin: real committed evidence the merge must never carry.
fn evidence() -> CommittedLine {
    CommittedLine {
        commercial_attempt_id: id(800),
        order_pin: OrderPin::decode(
            include_str!("../../../orders-sdk/tests/fixtures/commercial-pin-v2.json").as_bytes(),
        )
        .unwrap(),
    }
}

fn header() -> CommittedHeader {
    CommittedHeader {
        category: Category::NewSale,
        resource_tenant_id: id(10),
        seller_tenant_id: id(20),
        payer_tenant_id: id(30),
        contract_id: None,
    }
}

fn committed() -> CommittedVersion {
    CommittedVersion {
        version: 2,
        header: header(),
        lines: vec![line(id(1), "1"), line(id(2), "5")],
    }
}

fn add_line() -> AddLine {
    AddLine {
        plan_id: id(400),
        plan_revision_id: id(401),
        selected_items: items("3"),
        currency: Currency::try_from("EUR".to_owned()).unwrap(),
        contract_effective_date: None,
        service_activation_date: None,
        acceptance_due_date: None,
        term_duration: None,
        billing_cycle: Some(BillingCycle::Year),
    }
}

fn quantity_change(line_id: Uuid, quantity: &str) -> LineDelta {
    LineDelta::Change(
        line_id,
        LinePatch {
            selected_items: Some(items(quantity)),
            ..LinePatch::default()
        },
    )
}

#[test]
fn merge_inherits_unnamed_fields_and_keeps_retained_lines() {
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![quantity_change(id(1), "7"), LineDelta::Add(add_line())],
    };
    let proposed = merge_with(&n, &delta, || id(99)).unwrap();
    assert_eq!(
        proposed.header, n.header,
        "an unnamed header carries forward"
    );
    assert_eq!(proposed.lines.len(), 3);

    let l1 = &proposed.lines[0];
    assert_eq!(l1.origin, LineOrigin::Changed);
    assert_eq!(l1.line.line_id, id(1), "a retained line keeps its id");
    assert_eq!(l1.line.selected_items, items("7"));
    let unchanged = DraftLine {
        selected_items: items("7"),
        ..n.lines[0].clone()
    };
    assert_eq!(l1.line, unchanged, "only the named field changes");

    let l2 = &proposed.lines[1];
    assert_eq!(l2.origin, LineOrigin::Retained);
    assert_eq!(l2.line, n.lines[1], "an unlisted line is identical");

    let l3 = &proposed.lines[2];
    assert_eq!(l3.origin, LineOrigin::Added);
    assert_eq!(l3.line.line_id, id(99));
    assert!(n.lines.iter().all(|l| l.line_id != l3.line.line_id));
    assert_eq!(l3.line.plan_id, id(400));
    assert_eq!(l3.line.billing_cycle, Some(BillingCycle::Year));
    assert!(l3.line.committed.is_none());

    let rows = proposed.as_draft_lines();
    assert_eq!(
        rows.iter().map(|l| l.line_id).collect::<Vec<_>>(),
        vec![id(1), id(2), id(99)]
    );
}

#[test]
fn merge_default_allocates_fresh_line_ids() {
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Add(add_line()), LineDelta::Add(add_line())],
    };
    let proposed = merge(&n, &delta).unwrap();
    let added: Vec<Uuid> = proposed.lines[2..].iter().map(|l| l.line.line_id).collect();
    assert_eq!(added.len(), 2);
    assert_ne!(added[0], added[1]);
    assert!(
        added
            .iter()
            .all(|a| n.lines.iter().all(|l| l.line_id != *a))
    );
}

#[test]
fn merge_records_unknown_lines_as_a_fact_and_changes_nothing_for_them() {
    // D-252: an unknown line is the membership guard's refusal, never a merge error.
    let n = committed();
    let remove = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Remove(id(42)), quantity_change(id(1), "9")],
    };
    let proposed = merge(&n, &remove).unwrap();
    assert_eq!(proposed.unknown_lines, vec![id(42)]);
    assert_eq!(
        proposed
            .as_draft_lines()
            .iter()
            .map(|l| l.line_id)
            .collect::<Vec<_>>(),
        vec![id(1), id(2)]
    );
    let change = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![quantity_change(id(43), "2"), LineDelta::Remove(id(44))],
    };
    let proposed = merge(&n, &change).unwrap();
    assert_eq!(proposed.unknown_lines, vec![id(43), id(44)]);
    assert_eq!(proposed.as_draft_lines(), n.lines);
    assert!(
        merge(&n, &AmendmentDelta::default())
            .unwrap()
            .unknown_lines
            .is_empty()
    );
}

#[test]
fn merge_refuses_a_line_named_twice() {
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![quantity_change(id(1), "2"), LineDelta::Remove(id(1))],
    };
    assert_eq!(merge(&n, &delta), Err(MergeError::LineNamedTwice(id(1))));
    assert_eq!(
        delta.check_well_formed(),
        Err(MergeError::LineNamedTwice(id(1)))
    );
    // Named twice outranks unknown: the boundary refuses it before any guard runs (D-252).
    let unknown_twice = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Remove(id(42)), LineDelta::Remove(id(42))],
    };
    assert_eq!(
        merge(&n, &unknown_twice),
        Err(MergeError::LineNamedTwice(id(42)))
    );
    assert_eq!(valid_delta().check_well_formed(), Ok(()));
}

fn valid_delta() -> AmendmentDelta {
    AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![quantity_change(id(1), "2"), LineDelta::Remove(id(2))],
    }
}

#[test]
fn merge_refuses_an_allocated_id_that_collides() {
    let n = committed();
    let one_add = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Add(add_line())],
    };
    assert_eq!(
        merge_with(&n, &one_add, || id(2)),
        Err(MergeError::AllocatedIdCollision(id(2)))
    );
    let two_adds = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Add(add_line()), LineDelta::Add(add_line())],
    };
    assert_eq!(
        merge_with(&n, &two_adds, || id(99)),
        Err(MergeError::AllocatedIdCollision(id(99)))
    );
}

#[test]
fn merge_strips_committed_evidence_from_retained_and_changed_lines() {
    let mut n = committed();
    for l in &mut n.lines {
        l.committed = Some(evidence());
    }
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![quantity_change(id(1), "4")],
    };
    let proposed = merge(&n, &delta).unwrap();
    assert_eq!(proposed.lines[0].origin, LineOrigin::Changed);
    assert!(proposed.lines[0].line.committed.is_none());
    assert_eq!(proposed.lines[1].origin, LineOrigin::Retained);
    assert!(proposed.lines[1].line.committed.is_none());
    assert_eq!(
        proposed.lines[1].line,
        DraftLine {
            committed: None,
            ..n.lines[1].clone()
        },
        "a retained line is N's authored content without N's evidence"
    );
}

#[test]
fn a_change_naming_no_field_keeps_the_line_retained() {
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Change(id(1), LinePatch::default())],
    };
    let proposed = merge(&n, &delta).unwrap();
    assert_eq!(proposed.lines[0].origin, LineOrigin::Retained);
    assert_eq!(proposed.lines[0].line, n.lines[0]);
    // It is still no delta for the nonempty guard.
    assert!(classify(&delta, Some("r")).delta_empty);
}

#[test]
fn merge_never_drops_a_line_absent_from_the_delta() {
    let n = CommittedVersion {
        lines: (1..=5).map(|i| line(id(i), "1")).collect(),
        ..committed()
    };
    let delta = AmendmentDelta {
        header: HeaderPatch::default(),
        lines: vec![LineDelta::Remove(id(3))],
    };
    let proposed = merge(&n, &delta).unwrap();
    let ids: Vec<Uuid> = proposed.lines.iter().map(|l| l.line.line_id).collect();
    assert_eq!(ids, vec![id(1), id(2), id(4), id(5)]);
    for kept in &proposed.lines {
        assert_eq!(kept.origin, LineOrigin::Retained);
        let original = n.lines.iter().find(|l| l.line_id == kept.line.line_id);
        assert_eq!(Some(&kept.line), original);
    }
}

#[test]
fn merge_applies_commercial_header_fields() {
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch {
            payer_tenant_id: Some(id(31)),
            contract_id: Some(Some(id(700))),
            ..HeaderPatch::default()
        },
        lines: vec![],
    };
    let proposed = merge(&n, &delta).unwrap();
    assert_eq!(proposed.header.payer_tenant_id, id(31));
    assert_eq!(proposed.header.contract_id, Some(id(700)));
    assert_eq!(proposed.header.resource_tenant_id, id(10));
    assert_eq!(proposed.header.seller_tenant_id, id(20));

    let cleared = AmendmentDelta {
        header: HeaderPatch {
            contract_id: Some(None),
            ..HeaderPatch::default()
        },
        lines: vec![],
    };
    let n = CommittedVersion {
        header: proposed.header,
        ..committed()
    };
    assert_eq!(merge(&n, &cleared).unwrap().header.contract_id, None);
}

#[test]
fn merge_never_applies_a_frozen_axis() {
    // The no-frozen-field guard refuses the delta; the merge never rebinds an axis either.
    let n = committed();
    let delta = AmendmentDelta {
        header: HeaderPatch {
            resource_tenant_id: Some(id(11)),
            seller_tenant_id: Some(id(21)),
            ..HeaderPatch::default()
        },
        lines: vec![],
    };
    assert_eq!(merge(&n, &delta).unwrap().header, n.header);
}

// --- Facts from the shared classifier ---------------------------------------------------------

/// Facts of `delta` merged onto [`committed`] (lines 1 and 2).
fn classify(delta: &AmendmentDelta, reason: Option<&str>) -> AmendmentFacts {
    let fields = FieldClassification::registered().unwrap();
    let proposed = merge(&committed(), delta).unwrap();
    AmendmentFacts::of(&fields, delta, &proposed, reason.map(str::to_owned)).unwrap()
}

fn header_delta(patch: HeaderPatch) -> AmendmentDelta {
    AmendmentDelta {
        header: patch,
        lines: vec![],
    }
}

#[test]
fn facts_classify_every_named_field_with_the_shared_declaration() {
    let empty = classify(&header_delta(HeaderPatch::default()), Some("r"));
    assert!(empty.delta_empty);
    assert!(!empty.names_admin && !empty.names_frozen);

    let commercial = classify(
        &AmendmentDelta {
            header: HeaderPatch::default(),
            lines: vec![quantity_change(id(1), "2")],
        },
        Some("r"),
    );
    assert!(!commercial.delta_empty && !commercial.names_admin && !commercial.names_frozen);

    // Membership alone (add or remove) is a commercial delta.
    for lines in [
        vec![LineDelta::Add(add_line())],
        vec![LineDelta::Remove(id(1))],
    ] {
        let facts = classify(
            &AmendmentDelta {
                header: HeaderPatch::default(),
                lines,
            },
            None,
        );
        assert!(!facts.delta_empty && !facts.names_admin && !facts.names_frozen);
    }

    // Mixed administrative + commercial still names an administrative field.
    let mixed = classify(
        &AmendmentDelta {
            header: HeaderPatch {
                payer_tenant_id: Some(id(31)),
                display_label: Some(Some("x".into())),
                ..HeaderPatch::default()
            },
            lines: vec![],
        },
        Some("r"),
    );
    assert!(mixed.names_admin && !mixed.delta_empty);
    let line_admin = classify(
        &AmendmentDelta {
            header: HeaderPatch::default(),
            lines: vec![LineDelta::Change(
                id(1),
                LinePatch {
                    internal_notes: Some(None),
                    ..LinePatch::default()
                },
            )],
        },
        Some("r"),
    );
    assert!(line_admin.names_admin);

    for patch in [
        HeaderPatch {
            resource_tenant_id: Some(id(11)),
            ..HeaderPatch::default()
        },
        HeaderPatch {
            seller_tenant_id: Some(id(21)),
            ..HeaderPatch::default()
        },
    ] {
        let frozen = classify(&header_delta(patch), Some("r"));
        assert!(frozen.names_frozen && !frozen.names_admin);
    }

    // A payer change is commercial: no versioning guard refuses it (D-248).
    let payer = classify(
        &header_delta(HeaderPatch {
            payer_tenant_id: Some(id(31)),
            ..HeaderPatch::default()
        }),
        Some("r"),
    );
    assert!(!payer.delta_empty && !payer.names_admin && !payer.names_frozen);

    // A line change naming no field is no delta.
    let hollow = classify(
        &AmendmentDelta {
            header: HeaderPatch::default(),
            lines: vec![LineDelta::Change(id(1), LinePatch::default())],
        },
        Some("r"),
    );
    assert!(hollow.delta_empty);
}

// --- Guard order on row 18 via the engine's decision -------------------------------------------

/// Row 18 through `decide`: the seven versioning bindings under test, every other registered
/// guard of the row bound to a passing stub (labelled double: the structural and gate guards are
/// the existing bindings' responsibility).
fn run(amendment: AmendmentFacts, amendment_count: i32) -> Option<Reason> {
    outcome(S::Submitted, amendment, amendment_count).unwrap()
}

/// [`run`] from any state: an engine refusal is `Err`.
fn outcome(
    state: S,
    amendment: AmendmentFacts,
    amendment_count: i32,
) -> Result<Option<Reason>, EngineRefusal> {
    let table = StateTable::registered().unwrap();
    let registry = GuardRegistry::registered(&table).unwrap();
    let mut locked = facts(state);
    locked.amendment_count = amendment_count;
    let mut bindings = amendment.bindings();
    let ours = [
        CAP,
        NONEMPTY,
        REASON,
        NO_ADMIN,
        NO_FROZEN,
        LINE_MEMBERSHIP,
        HAS_LINES,
    ];
    for guard in GUARDS.iter().filter(|g| g.rows.contains(&18)) {
        if !ours.contains(&guard.name) {
            bindings = bindings.bind(guard.name, |_: &GuardSubject<'_>| GuardVerdict::Pass);
        }
    }
    let inputs = registry
        .row_inputs(RowId::of(18))
        .into_iter()
        .fold(Inputs::new(), |i, port| i.with(port, InputState::Resolved));
    let decision = decide(
        &table,
        &registry,
        Trigger::Amendment,
        &locked,
        locked.current_version,
        DraftRevisions::NotApplicable,
        &bindings,
        &inputs,
        OffsetDateTime::from_unix_timestamp(1_790_000_100).unwrap(),
    )
    .unwrap();
    match decision {
        Decision::Admitted(row) => {
            assert_eq!(row.id, RowId::of(18));
            Ok(None)
        }
        Decision::Guard { reason, .. } => Ok(Some(reason)),
        Decision::Engine(refusal) => Err(refusal),
        other => panic!("{other:?}"),
    }
}

fn ok() -> AmendmentFacts {
    AmendmentFacts {
        delta_empty: false,
        reason: Some("price correction".into()),
        names_admin: false,
        names_frozen: false,
        unknown_lines: vec![],
        no_lines: false,
    }
}

/// Every versioning fact failing at once.
fn worst() -> AmendmentFacts {
    AmendmentFacts {
        delta_empty: true,
        reason: None,
        names_admin: true,
        names_frozen: true,
        unknown_lines: vec![id(42)],
        no_lines: true,
    }
}

#[test]
fn guard_names_are_the_registered_row_18_to_20_order() {
    let table = StateTable::registered().unwrap();
    let registry = GuardRegistry::registered(&table).unwrap();
    for row in [18, 19, 20] {
        let names: Vec<&str> = registry
            .row_guards(RowId::of(row))
            .iter()
            .map(|g| g.name)
            .take(7)
            .collect();
        assert_eq!(
            names,
            vec![
                CAP,
                NONEMPTY,
                REASON,
                NO_ADMIN,
                NO_FROZEN,
                LINE_MEMBERSHIP,
                HAS_LINES
            ]
        );
    }
}

#[test]
fn row_18_refuses_in_contract_order() {
    assert_eq!(
        run(worst(), AMENDMENT_CAP),
        Some(Reason::AmendmentCapExhausted)
    );
    assert_eq!(
        run(worst(), AMENDMENT_CAP - 1),
        Some(Reason::AmendmentEmpty)
    );
    let bad_reason = AmendmentFacts {
        delta_empty: false,
        ..worst()
    };
    assert_eq!(run(bad_reason, 0), Some(Reason::AmendmentReasonInvalid));
    let admin = AmendmentFacts {
        delta_empty: false,
        reason: Some("r".into()),
        ..worst()
    };
    assert_eq!(run(admin, 0), Some(Reason::AdministrativeFieldInAmendment));
    let frozen = AmendmentFacts {
        names_frozen: true,
        unknown_lines: vec![id(42)],
        no_lines: true,
        ..ok()
    };
    assert_eq!(run(frozen, 0), Some(Reason::TenantAxisImmutable));
    let unknown = AmendmentFacts {
        unknown_lines: vec![id(42)],
        no_lines: true,
        ..ok()
    };
    assert_eq!(run(unknown, 0), Some(Reason::LineNotFound));
    let empty = AmendmentFacts {
        no_lines: true,
        ..ok()
    };
    assert_eq!(run(empty, 0), Some(Reason::NoLines));
    assert_eq!(run(ok(), AMENDMENT_CAP - 1), None);
    assert_eq!(
        run(ok(), AMENDMENT_CAP + 3),
        Some(Reason::AmendmentCapExhausted)
    );
}

#[test]
fn amendment_reason_is_one_to_4096_characters() {
    let with = |text: Option<String>| AmendmentFacts {
        reason: text,
        ..ok()
    };
    assert_eq!(
        run(with(Some(String::new())), 0),
        Some(Reason::AmendmentReasonInvalid)
    );
    assert_eq!(
        run(with(Some("x".repeat(4097))), 0),
        Some(Reason::AmendmentReasonInvalid)
    );
    // Characters, not bytes: 4096 two-byte characters pass.
    assert_eq!(run(with(Some("\u{e9}".repeat(4096))), 0), None);
    assert_eq!(run(with(Some("x".into())), 0), None);
    assert_eq!(run(with(Some("x".repeat(4096))), 0), None);
    // The same rule as the stored audit caller_reason: no control characters.
    assert_eq!(
        run(with(Some("a\u{0}b".into())), 0),
        Some(Reason::AmendmentReasonInvalid)
    );
}

#[test]
fn mixed_admin_and_frozen_and_payer_deltas_through_the_guards() {
    let fields = FieldClassification::registered().unwrap();
    let facts_of = |patch: HeaderPatch| {
        let delta = header_delta(patch);
        let proposed = merge(&committed(), &delta).unwrap();
        AmendmentFacts::of(&fields, &delta, &proposed, Some("why".into())).unwrap()
    };
    let mixed = facts_of(HeaderPatch {
        category: Some(Category::NewSale),
        external_reference: Some(Some("PO".into())),
        ..HeaderPatch::default()
    });
    assert_eq!(run(mixed, 0), Some(Reason::AdministrativeFieldInAmendment));
    let resource = facts_of(HeaderPatch {
        resource_tenant_id: Some(id(11)),
        ..HeaderPatch::default()
    });
    assert_eq!(run(resource, 0), Some(Reason::TenantAxisImmutable));
    let seller = facts_of(HeaderPatch {
        seller_tenant_id: Some(id(21)),
        ..HeaderPatch::default()
    });
    assert_eq!(run(seller, 0), Some(Reason::TenantAxisImmutable));
    // D-248: no payer guard; gate predicate 1 decides a payer change.
    let payer = facts_of(HeaderPatch {
        payer_tenant_id: Some(id(31)),
        ..HeaderPatch::default()
    });
    assert_eq!(run(payer, 0), None);
}

#[test]
fn an_inadmissible_state_refuses_before_the_cap() {
    // 04 §3.1 step 2: admissibility is the engine's, ahead of every slice guard.
    for state in [S::Draft, S::InFulfillment] {
        assert!(matches!(
            outcome(state, worst(), AMENDMENT_CAP),
            Err(EngineRefusal::NotAdmissible {
                trigger: Trigger::Amendment,
                ..
            })
        ));
    }
}

#[test]
fn row_18_refuses_unknown_lines_and_an_empty_result_from_real_deltas() {
    let delta = |lines| AmendmentDelta {
        header: HeaderPatch::default(),
        lines,
    };
    // Removing every line of N leaves no line: `no-lines` (D-252 (a)).
    let all_removed = classify(
        &delta(vec![LineDelta::Remove(id(1)), LineDelta::Remove(id(2))]),
        Some("r"),
    );
    assert!(all_removed.no_lines && all_removed.unknown_lines.is_empty());
    assert_eq!(run(all_removed, 0), Some(Reason::NoLines));
    // Removing all and adding one is a proposed version with a line.
    let replaced = classify(
        &delta(vec![
            LineDelta::Remove(id(1)),
            LineDelta::Remove(id(2)),
            LineDelta::Add(add_line()),
        ]),
        Some("r"),
    );
    assert_eq!(run(replaced, 0), None);
    // Removing or changing a line that is not N's: `line-not-found` (D-252 (b)).
    let removed_unknown = classify(&delta(vec![LineDelta::Remove(id(42))]), Some("r"));
    assert_eq!(removed_unknown.unknown_lines, vec![id(42)]);
    assert_eq!(run(removed_unknown, 0), Some(Reason::LineNotFound));
    let changed_unknown = classify(&delta(vec![quantity_change(id(43), "2")]), Some("r"));
    assert_eq!(run(changed_unknown.clone(), 0), Some(Reason::LineNotFound));
    // The cap outranks an unknown line.
    assert_eq!(
        run(changed_unknown, AMENDMENT_CAP),
        Some(Reason::AmendmentCapExhausted)
    );
    // An unknown line outranks an empty result.
    let both = classify(
        &delta(vec![
            LineDelta::Remove(id(1)),
            LineDelta::Remove(id(2)),
            LineDelta::Remove(id(42)),
        ]),
        Some("r"),
    );
    assert!(both.no_lines);
    assert_eq!(run(both, 0), Some(Reason::LineNotFound));
}

/// The service's pre-call check (04 §3.6 step 2, D-113) is the engine's decision: for every
/// combination of failing facts and counts, `first_failure` names what `decide` refuses with.
#[test]
fn first_failure_is_the_engine_decision_over_the_same_facts() {
    let cases = [
        worst(),
        ok(),
        AmendmentFacts {
            delta_empty: false,
            ..worst()
        },
        AmendmentFacts {
            names_admin: true,
            names_frozen: true,
            ..ok()
        },
        AmendmentFacts {
            names_frozen: true,
            ..ok()
        },
        AmendmentFacts {
            unknown_lines: vec![id(42)],
            no_lines: true,
            ..ok()
        },
        AmendmentFacts {
            no_lines: true,
            ..ok()
        },
        AmendmentFacts {
            reason: None,
            ..ok()
        },
    ];
    for facts in cases {
        for count in [0, AMENDMENT_CAP - 1, AMENDMENT_CAP, AMENDMENT_CAP + 1] {
            assert_eq!(
                facts.first_failure(count),
                run(facts.clone(), count),
                "{facts:?} at {count}"
            );
        }
    }
}
