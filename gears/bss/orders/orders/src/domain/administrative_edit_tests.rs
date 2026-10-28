#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once"
)]
use super::*;
use crate::domain::audit::AdminField;
use crate::domain::contributions::reconcile_administrative;
use crate::domain::contributions::tests::facts;
use crate::domain::guards::GuardRegistry;
use crate::domain::state_table::{RowId, StateTable};
use crate::domain::transition::{Decision, DraftRevisions, decide_with};
use bss_orders_sdk::authoring::{HeaderPatch, LinePatch};
use bss_orders_sdk::catalog::{OrderState as S, Reason, Trigger};
use time::OffsetDateTime;

fn line() -> Uuid {
    Uuid::from_u128(77)
}

#[test]
fn unnamed_fields_produce_no_change() {
    let edit = AdminEdit::from_header(&HeaderPatch {
        display_label: Some(Some("x".into())),
        ..HeaderPatch::default()
    });
    assert_eq!(edit.line_id, None);
    assert_eq!(
        edit.values,
        vec![(AdminAttribute::DisplayLabel, Some("x".to_owned()))]
    );
    assert_eq!(edit.named_changes().len(), 1);
}

#[test]
fn changes_drop_equal_values() {
    let edit = AdminEdit::from_header(&HeaderPatch {
        external_reference: Some(Some("PO-1".into())),
        display_label: Some(Some("y".into())),
        ..HeaderPatch::default()
    });
    let stored = |field: AdminField| match field {
        AdminField::Order(AdminAttribute::ExternalReference) => Some("PO-1".to_owned()),
        AdminField::Order(AdminAttribute::DisplayLabel) => Some("x".to_owned()),
        _ => None,
    };
    let changes = reconcile_administrative(&edit.named_changes(), stored);
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].field,
        AdminField::Order(AdminAttribute::DisplayLabel)
    );
    assert_eq!(changes[0].prior.as_deref(), Some("x"));
}

#[test]
fn clearing_a_value_is_a_change() {
    let edit = AdminEdit::from_header(&HeaderPatch {
        internal_notes: Some(None),
        ..HeaderPatch::default()
    });
    let changes = reconcile_administrative(&edit.named_changes(), |_| Some("a".to_owned()));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].new, None);
    assert_eq!(changes[0].prior.as_deref(), Some("a"));
}

#[test]
fn line_edit_names_line_fields() {
    let edit = AdminEdit::from_line(
        line(),
        &LinePatch {
            display_label: Some(Some("l".into())),
            ..LinePatch::default()
        },
    );
    assert_eq!(edit.line_id, Some(line()));
    let changes = edit.named_changes();
    assert_eq!(
        changes[0].field,
        AdminField::Line(line(), AdminAttribute::DisplayLabel)
    );
}

#[test]
fn guards_refuse_in_contract_order() {
    let table = StateTable::registered().unwrap();
    let registry = GuardRegistry::registered(&table).unwrap();
    let f = facts(S::Approved);
    let change = AdminEdit::from_header(&HeaderPatch {
        external_reference: Some(Some("PO".into())),
        ..HeaderPatch::default()
    })
    .named_changes();
    let run = |guards: AdminGuardFacts, administrative: &[AdminChange]| {
        let d = decide_with(
            &table,
            &registry,
            Trigger::AdministrativeEdit,
            &f,
            f.current_version,
            DraftRevisions::NotApplicable,
            &guards.bindings(),
            &crate::domain::transition::Inputs::new(),
            OffsetDateTime::from_unix_timestamp(1_790_000_100).unwrap(),
            administrative,
        )
        .unwrap();
        match d {
            Decision::Admitted(_) => None,
            Decision::Guard { reason, .. } => Some(reason),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(registry.row_guards(RowId::of(3)).len(), 3);
    let ok = AdminGuardFacts {
        line_member: true,
        all_administrative: true,
    };
    assert_eq!(run(ok, &change), None);
    // Membership precedes classification and the change guard.
    let none = AdminGuardFacts {
        line_member: false,
        all_administrative: false,
    };
    assert_eq!(run(none, &[]), Some(Reason::LineNotFound));
    let commercial = AdminGuardFacts {
        line_member: true,
        all_administrative: false,
    };
    assert_eq!(run(commercial, &[]), Some(Reason::CommercialFieldImmutable));
    assert_eq!(run(ok, &[]), Some(Reason::AdministrativeEditUnchanged));
}
