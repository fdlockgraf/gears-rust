//! Administrative order and line edits (04 §3.2 steps 2-3, §4.5-4.6; S4-05).
//!
//! An administrative-only `PATCH` selects `administrative-edit` (row 3): last write wins per
//! field, no version, no event, one audit entry per changed field (D-117). This module is pure:
//! the named values of one request and the three row-3 guards. Membership is decided over the
//! lines the engine reads under the aggregate lock, never over a preparation snapshot.
//! The prior values are never trusted from here: the engine re-reads them under the aggregate
//! lock and reconciles (`contributions::reconcile_administrative`, D-142, D-149).
use std::sync::Arc;
use toolkit_macros::domain_model;

use bss_orders_sdk::authoring::{HeaderPatch, LinePatch};
use bss_orders_sdk::catalog::Reason;
use uuid::Uuid;

use super::audit::{AdminAttribute, AdminChange, AdminField};
use super::transition::{GuardBindings, GuardVerdict};

/// The line-membership guard (`line-not-found`).
pub const MEMBERSHIP: &str = "versioning.admin-line-membership";
/// The field-classification guard (`commercial-field-immutable`).
pub const CLASSIFICATION: &str = "versioning.admin-field-classification";
/// The change guard (`administrative-edit-unchanged`).
pub const CHANGED: &str = "versioning.admin-changed";

/// The named administrative values of one `PATCH` (`None` = not named; `Some(None)` = clear).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminEdit {
    /// The target line; `None` for the order itself.
    pub line_id: Option<Uuid>,
    /// Each named attribute with its new value, in the declared attribute order.
    pub values: Vec<(AdminAttribute, Option<String>)>,
}

#[expect(
    clippy::option_option,
    reason = "the SDK's patch member: outer `None` is not named, `Some(None)` clears the value"
)]
fn named(
    members: [(AdminAttribute, Option<Option<String>>); 3],
) -> Vec<(AdminAttribute, Option<String>)> {
    members
        .into_iter()
        .filter_map(|(attribute, value)| value.map(|v| (attribute, v)))
        .collect()
}

impl AdminEdit {
    /// The administrative members a header patch names.
    #[must_use]
    pub fn from_header(patch: &HeaderPatch) -> Self {
        Self {
            line_id: None,
            values: named([
                (
                    AdminAttribute::ExternalReference,
                    patch.external_reference.clone(),
                ),
                (AdminAttribute::DisplayLabel, patch.display_label.clone()),
                (AdminAttribute::InternalNotes, patch.internal_notes.clone()),
            ]),
        }
    }

    /// The administrative members a line patch names.
    #[must_use]
    pub fn from_line(line_id: Uuid, patch: &LinePatch) -> Self {
        Self {
            line_id: Some(line_id),
            values: named([
                (
                    AdminAttribute::ExternalReference,
                    patch.external_reference.clone(),
                ),
                (AdminAttribute::DisplayLabel, patch.display_label.clone()),
                (AdminAttribute::InternalNotes, patch.internal_notes.clone()),
            ]),
        }
    }

    fn field(&self, attribute: AdminAttribute) -> AdminField {
        match self.line_id {
            None => AdminField::Order(attribute),
            Some(line) => AdminField::Line(line, attribute),
        }
    }

    /// Every named value as a change with no prior value. The engine replaces the prior value
    /// with the one stored under the lock and drops the unchanged ones, so nothing here reads
    /// storage and a concurrent edit cannot falsify the audit.
    #[must_use]
    pub fn named_changes(&self) -> Vec<AdminChange> {
        self.values
            .iter()
            .map(|(attribute, new)| AdminChange {
                field: self.field(*attribute),
                prior: None,
                new: new.clone(),
            })
            .collect()
    }
}

/// The facts the three row-3 guards decide over. Line membership is not one of them: the
/// membership guard compares the target with the lines the engine read under the aggregate lock
/// (`GuardSubject::admin_lines`), never with a preparation snapshot.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct AdminGuardFacts {
    /// The target line; `None` for an order edit (always a member).
    pub line: Option<Uuid>,
    /// Every named field is administrative (boundary classification already refused unknown
    /// fields; a commercial field never reaches row 3).
    pub all_administrative: bool,
}

impl AdminGuardFacts {
    /// Bind the registered row-3 guards, in the contract's refusal order.
    #[must_use]
    pub fn bindings(self) -> GuardBindings {
        let facts = Arc::new(self);
        let membership = Arc::clone(&facts);
        GuardBindings::new()
            .bind(
                MEMBERSHIP,
                move |subject: &super::transition::GuardSubject<'_>| {
                    if membership
                        .line
                        .is_none_or(|line| subject.admin_lines.contains(&line))
                    {
                        GuardVerdict::Pass
                    } else {
                        GuardVerdict::Fail(Reason::LineNotFound)
                    }
                },
            )
            .bind(
                CLASSIFICATION,
                move |_: &super::transition::GuardSubject<'_>| {
                    if facts.all_administrative {
                        GuardVerdict::Pass
                    } else {
                        GuardVerdict::Fail(Reason::CommercialFieldImmutable)
                    }
                },
            )
            .bind(CHANGED, |subject: &super::transition::GuardSubject<'_>| {
                if subject.administrative.is_empty() {
                    GuardVerdict::Fail(Reason::AdministrativeEditUnchanged)
                } else {
                    GuardVerdict::Pass
                }
            })
    }
}

#[cfg(test)]
#[path = "administrative_edit_tests.rs"]
mod tests;
