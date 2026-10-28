//! Amendment domain rules (04 §3.6 *Append Amendment* steps 1-4, §4.1; Stage 4 DESIGN §3; S4-03).
//!
//! Pure and local: the authored delta, its merge onto the committed content of version N, the
//! facts the five versioning guards decide on, and their predicates bound to the registered
//! names (`domain::guards`, rows 18-20). Carry-forward is total: every unnamed field and every
//! retained line the delta does not list is N's, byte for byte; retained line ids are stable and
//! added lines get new ids. The merged [`ProposedVersion`] is shaped as the SDK [`DraftLine`]s
//! Submit's basket builder already consumes, so the shared gate runs over it unchanged.
//!
//! Every named field is classified through Capture's one declaration
//! ([`FieldClassification::class`], `FIELD_CLASSES`; 02 §4.3 "neither maintains its own list").
//! There is no payer guard (D-248): a payer change passes these guards and gate predicate 1
//! decides it. The shared structural and gate guards of rows 18-20 are bound by their owners.
use std::collections::BTreeSet;
use std::sync::Arc;
use toolkit_macros::domain_model;

use bss_orders_sdk::authoring::{AddLine, Category, DraftLine, HeaderPatch, LinePatch};
use bss_orders_sdk::catalog::Reason;
use uuid::Uuid;

use super::audit::validate_caller_reason;
use super::contributions::{FieldClass, FieldClassification, FieldScope};
use super::transition::{GuardBindings, GuardSubject, GuardVerdict};

/// The amendment cap: baseline 20 amendments per order, never unlimited (04 §4.1).
pub const AMENDMENT_CAP: i32 = 20;

/// Registered versioning guard names (`domain::guards`, rows 18-20), in registration order.
/// Refuses `amendment-cap-exhausted` once the locked `amendment_count` reaches the cap.
pub const CAP: &str = "versioning.amendment-cap";
/// Refuses `amendment-empty` for a delta naming no field.
pub const NONEMPTY: &str = "versioning.amendment-nonempty";
/// Refuses `amendment-reason-invalid` for an absent or invalid explanation (D-129).
pub const REASON: &str = "versioning.amendment-reason";
/// Refuses `administrative-field-in-amendment` for a delta naming any administrative field.
pub const NO_ADMIN: &str = "versioning.no-administrative-field";
/// Refuses `tenant-axis-immutable` for a delta naming a commercial-frozen axis.
pub const NO_FROZEN: &str = "versioning.no-frozen-field";

/// The line-membership pseudo-field: adding or removing a line is commercial content
/// (`AUTHORED_FIELDS`), classified like any named field.
const MEMBERSHIP: &str = "membership";

/// One line operation of an amendment delta, by stable line id.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineDelta {
    /// A new line; the merge allocates its id.
    Add(AddLine),
    /// The named fields of a retained line of N.
    Change(Uuid, LinePatch),
    /// Drop a line of N.
    Remove(Uuid),
}

/// The authored amendment delta (SDK `AmendOrder.delta` decoded): order-level fields and line
/// operations. Presence names a field, as on the capture `PATCH` bodies it reuses.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmendmentDelta {
    /// Order-level fields.
    pub header: HeaderPatch,
    /// Line operations; a line id appears at most once.
    pub lines: Vec<LineDelta>,
}

/// The authored commercial header of committed version N: `order_version` (payer, category,
/// contract) and the order's fixed axes. Gate outputs (market, totals) are never carried: the
/// amendment re-runs the complete gate (04 §3.6 step 5, "reuse no old gate outputs").
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedHeader {
    /// The order category.
    pub category: Category,
    /// Resource tenant; commercial-frozen from `submitted`.
    pub resource_tenant_id: Uuid,
    /// Seller tenant; commercial-frozen from creation.
    pub seller_tenant_id: Uuid,
    /// Payer tenant; amendable, decided by gate predicate 1 (D-248).
    pub payer_tenant_id: Uuid,
    /// The order contract, if any.
    pub contract_id: Option<Uuid>,
}

/// The committed content of version N, as the read service composes it: the header and every
/// line in identity order (SDK [`DraftLine`], committed evidence optional and never carried).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedVersion {
    /// N.
    pub version: i32,
    /// N's commercial header.
    pub header: CommittedHeader,
    /// N's lines in identity order.
    pub lines: Vec<DraftLine>,
}

/// Where a proposed line comes from.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    /// A line of N the delta does not list: identical to N.
    Retained,
    /// A line of N with named fields applied; same id.
    Changed,
    /// A new line with a new id.
    Added,
}

/// One line of the proposed version.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedLine {
    /// Whether the line is retained, changed or added.
    pub origin: LineOrigin,
    /// The line's authored content (no committed evidence: nothing of N's evidence carries).
    pub line: DraftLine,
}

/// The carried-forward content of N with the delta applied; retained line ids are stable.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedVersion {
    /// The version the delta was merged onto.
    pub base_version: i32,
    /// The proposed header.
    pub header: CommittedHeader,
    /// N's retained lines in N's order, then added lines in delta order.
    pub lines: Vec<ProposedLine>,
}

impl ProposedVersion {
    /// The proposed lines as the SDK draft lines Submit's basket and date basis consume
    /// (synthetic: never written to the working table).
    #[must_use]
    pub fn as_draft_lines(&self) -> Vec<DraftLine> {
        self.lines.iter().map(|l| l.line.clone()).collect()
    }
}

/// Why a delta cannot be merged onto N.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    /// A changed or removed line is not a line of N.
    #[error("line {0} is not a line of the committed version")]
    LineNotFound(Uuid),
    /// The delta names one line more than once.
    #[error("line {0} is named more than once in the delta")]
    LineNamedTwice(Uuid),
}

impl MergeError {
    /// The registered refusal reason, when there is one: `line-not-found` (404). A line named
    /// twice is a malformed request (`request-invalid` at the boundary).
    #[must_use]
    pub fn reason(self) -> Option<Reason> {
        match self {
            Self::LineNotFound(_) => Some(Reason::LineNotFound),
            Self::LineNamedTwice(_) => None,
        }
    }
}

/// Merge `delta` onto N, allocating a time-ordered id for every added line.
///
/// # Errors
/// [`MergeError`].
pub fn merge(n: &CommittedVersion, delta: &AmendmentDelta) -> Result<ProposedVersion, MergeError> {
    merge_with(n, delta, Uuid::now_v7)
}

/// [`merge`] with the caller's line-id allocator.
///
/// # Errors
/// [`MergeError`].
pub fn merge_with(
    n: &CommittedVersion,
    delta: &AmendmentDelta,
    mut new_id: impl FnMut() -> Uuid,
) -> Result<ProposedVersion, MergeError> {
    let mut named = BTreeSet::new();
    for op in &delta.lines {
        let target = match op {
            LineDelta::Add(_) => continue,
            LineDelta::Change(id, _) | LineDelta::Remove(id) => *id,
        };
        if !named.insert(target) {
            return Err(MergeError::LineNamedTwice(target));
        }
        if !n.lines.iter().any(|l| l.line_id == target) {
            return Err(MergeError::LineNotFound(target));
        }
    }
    let mut lines = Vec::with_capacity(n.lines.len() + delta.lines.len());
    for carried in &n.lines {
        let op = delta.lines.iter().find(|op| match op {
            LineDelta::Change(id, _) | LineDelta::Remove(id) => *id == carried.line_id,
            LineDelta::Add(_) => false,
        });
        let mut line = DraftLine {
            committed: None,
            ..carried.clone()
        };
        let origin = match op {
            Some(LineDelta::Remove(_)) => continue,
            Some(LineDelta::Change(_, patch)) => {
                apply_line(&mut line, patch);
                LineOrigin::Changed
            }
            Some(LineDelta::Add(_)) | None => LineOrigin::Retained,
        };
        lines.push(ProposedLine { origin, line });
    }
    for op in &delta.lines {
        if let LineDelta::Add(add) = op {
            lines.push(ProposedLine {
                origin: LineOrigin::Added,
                line: added_line(new_id(), add),
            });
        }
    }
    Ok(ProposedVersion {
        base_version: n.version,
        header: apply_header(n.header, &delta.header),
        lines,
    })
}

/// Apply the commercial header fields. Commercial-frozen axes are never applied (their guard
/// refuses the delta) and administrative fields have no place on a version (04 §3.7).
fn apply_header(mut header: CommittedHeader, patch: &HeaderPatch) -> CommittedHeader {
    // Exhaustive on purpose: a new header field fails to compile until it is decided here.
    let HeaderPatch {
        category,
        payer_tenant_id,
        contract_id,
        resource_tenant_id: _,
        seller_tenant_id: _,
        external_reference: _,
        display_label: _,
        internal_notes: _,
    } = patch;
    if let Some(category) = category {
        header.category = *category;
    }
    if let Some(payer) = payer_tenant_id {
        header.payer_tenant_id = *payer;
    }
    if let Some(contract) = contract_id {
        header.contract_id = *contract;
    }
    header
}

/// Apply the named commercial line fields; `Some(None)` clears a nullable one.
fn apply_line(line: &mut DraftLine, patch: &LinePatch) {
    // Exhaustive on purpose: a new line field fails to compile until it is decided here.
    let LinePatch {
        plan_id,
        plan_revision_id,
        selected_items,
        currency,
        contract_effective_date,
        service_activation_date,
        acceptance_due_date,
        term_duration,
        billing_cycle,
        external_reference: _,
        display_label: _,
        internal_notes: _,
    } = patch;
    if let Some(v) = plan_id {
        line.plan_id = *v;
    }
    if let Some(v) = plan_revision_id {
        line.plan_revision_id = *v;
    }
    if let Some(v) = selected_items {
        line.selected_items.clone_from(v);
    }
    if let Some(v) = currency {
        line.currency = v.clone();
    }
    if let Some(v) = contract_effective_date {
        line.contract_effective_date = *v;
    }
    if let Some(v) = service_activation_date {
        line.service_activation_date = *v;
    }
    if let Some(v) = acceptance_due_date {
        line.acceptance_due_date = *v;
    }
    if let Some(v) = term_duration {
        line.term_duration = *v;
    }
    if let Some(v) = billing_cycle {
        line.billing_cycle = *v;
    }
}

fn added_line(line_id: Uuid, add: &AddLine) -> DraftLine {
    let AddLine {
        plan_id,
        plan_revision_id,
        selected_items,
        currency,
        contract_effective_date,
        service_activation_date,
        acceptance_due_date,
        term_duration,
        billing_cycle,
    } = add;
    DraftLine {
        committed: None,
        line_id,
        plan_id: *plan_id,
        plan_revision_id: *plan_revision_id,
        selected_items: selected_items.clone(),
        currency: currency.clone(),
        contract_effective_date: *contract_effective_date,
        service_activation_date: *service_activation_date,
        acceptance_due_date: *acceptance_due_date,
        term_duration: *term_duration,
        billing_cycle: *billing_cycle,
    }
}

/// A delta field the shared declaration does not classify. Startup refuses an unclassified
/// authoring field (`capture::check_wire_classification`), so this is an integration failure.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("delta field {scope:?}/{name} is not classified")]
pub struct UnclassifiedField {
    /// The field's scope.
    pub scope: FieldScope,
    /// The field's wire name.
    pub name: &'static str,
}

/// The request facts the versioning guards decide on. The cap is not one of them: its guard
/// reads the `amendment_count` the engine locked (`GuardSubject::locked`), never a preparation
/// snapshot (the S4-05 lock rule for line membership).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one independent request fact per registered versioning guard"
)]
pub struct AmendmentFacts {
    /// The delta names no field.
    pub delta_empty: bool,
    /// The caller's explanation as received.
    pub reason: Option<String>,
    /// The delta names an administrative field (a mixed delta included).
    pub names_admin: bool,
    /// The delta names a commercial-frozen field (resource or seller tenant).
    pub names_frozen: bool,
}

impl AmendmentFacts {
    /// Classify every field `delta` names through the shared declaration.
    ///
    /// # Errors
    /// [`UnclassifiedField`] (startup proves none exists).
    pub fn of(
        fields: &FieldClassification,
        delta: &AmendmentDelta,
        reason: Option<String>,
    ) -> Result<Self, UnclassifiedField> {
        let named = delta
            .header
            .named_fields()
            .into_iter()
            .map(|name| (FieldScope::Order, name))
            .chain(delta.lines.iter().flat_map(|op| {
                match op {
                    LineDelta::Add(_) | LineDelta::Remove(_) => {
                        vec![(FieldScope::Line, MEMBERSHIP)]
                    }
                    LineDelta::Change(_, patch) => patch
                        .named_fields()
                        .into_iter()
                        .map(|name| (FieldScope::Line, name))
                        .collect(),
                }
            }));
        let mut facts = Self {
            delta_empty: true,
            reason,
            names_admin: false,
            names_frozen: false,
        };
        for (scope, name) in named {
            facts.delta_empty = false;
            match fields.class(scope, name) {
                None => return Err(UnclassifiedField { scope, name }),
                Some(FieldClass::Administrative) => facts.names_admin = true,
                Some(FieldClass::CommercialFrozen) => facts.names_frozen = true,
                Some(FieldClass::Commercial) => {}
            }
        }
        Ok(facts)
    }

    fn reason_verdict(&self) -> GuardVerdict {
        // The stored `amendment_reason` and the audit `caller_reason` share one rule (D-129).
        match self.reason.as_deref().map(validate_caller_reason) {
            Some(Ok(())) => GuardVerdict::Pass,
            None | Some(Err(_)) => GuardVerdict::Fail(Reason::AmendmentReasonInvalid),
        }
    }

    /// Bind the five registered versioning guards of rows 18-20 to these facts. The caller adds
    /// the shared structural and gate bindings.
    #[must_use]
    pub fn bindings(self) -> GuardBindings {
        let facts = Arc::new(self);
        let bind = |f: fn(&Self) -> GuardVerdict| {
            let facts = Arc::clone(&facts);
            move |_: &GuardSubject<'_>| f(&facts)
        };
        GuardBindings::new()
            .bind(CAP, |subject: &GuardSubject<'_>| {
                cap_verdict(subject.locked.amendment_count)
            })
            .bind(
                NONEMPTY,
                bind(|f| fail_if(f.delta_empty, Reason::AmendmentEmpty)),
            )
            .bind(REASON, bind(Self::reason_verdict))
            .bind(
                NO_ADMIN,
                bind(|f| fail_if(f.names_admin, Reason::AdministrativeFieldInAmendment)),
            )
            .bind(
                NO_FROZEN,
                bind(|f| fail_if(f.names_frozen, Reason::TenantAxisImmutable)),
            )
    }
}

/// `amendment-cap-exhausted` once the locked count reaches [`AMENDMENT_CAP`] (04 §4.1).
#[must_use]
pub fn cap_verdict(amendment_count: i32) -> GuardVerdict {
    fail_if(
        amendment_count >= AMENDMENT_CAP,
        Reason::AmendmentCapExhausted,
    )
}

fn fail_if(failed: bool, reason: Reason) -> GuardVerdict {
    if failed {
        GuardVerdict::Fail(reason)
    } else {
        GuardVerdict::Pass
    }
}

#[cfg(test)]
#[path = "versioning_tests.rs"]
mod tests;
