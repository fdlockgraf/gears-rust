//! Read contracts (08 §3.3; S1-02 catalog `get`, `list`, `list_lines`, and the S4-06 history
//! reads `list_versions`, `get_version`, `get_acceptance`; `models.json` `ListOrders`,
//! `OrderPage`, `LineList`, `LinePage`, `VersionList`, `VersionPage`, `OrderVersionView`,
//! `AcceptanceList`, `AcceptancePage`). Draft and submitted reads with S6-04 access logging.
//!
//! Every collection is paged (08 §2.2) in the shared list format (DNA REST `PAGINATION.md`,
//! D-212): `limit` defaults to 50 and is bounded 1–200; the continuation `cursor` is an opaque
//! versioned token that is position only, never authority (D-139); a page is `items` plus
//! `page_info`. Over REST the order-list filters are an `OData` `$filter` lowered to
//! [`OrderFilters`]; clients cannot select a sort. A read carries no expected version or idempotency key; the only call metadata
//! is the supplied delegation proof reference, forwarded unvalidated to the PDP and recorded as
//! *supplied* on the access log (08 §4.4, D-202).
use crate::authoring::{DraftLine, OrderHeader, SalesPath};
use crate::catalog::OrderState;
use crate::models::{DelegationProofRef, DraftRevision, InvalidBoundaryValue, OrderVersion};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Default page size (08 §4.5).
pub const PAGE_DEFAULT: u16 = 50;
/// Smallest accepted page size.
pub const PAGE_MIN: u16 = 1;
/// Largest accepted page size; a larger request is refused `page-size-exceeded`, never truncated.
pub const PAGE_MAX: u16 = 200;
/// Largest accepted cursor token length in bytes (structural bound before decoding).
pub const CURSOR_MAX_LEN: usize = 4_096;

/// A validated page size in `1..=200`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct PageSize(u16);
impl PageSize {
    /// The design default of 50.
    pub const DEFAULT: Self = Self(PAGE_DEFAULT);
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}
impl Default for PageSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}
impl TryFrom<u64> for PageSize {
    type Error = InvalidBoundaryValue;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        let value = u16::try_from(value).map_err(|_| InvalidBoundaryValue)?;
        if (PAGE_MIN..=PAGE_MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidBoundaryValue)
        }
    }
}
impl From<PageSize> for u64 {
    fn from(value: PageSize) -> Self {
        Self::from(value.0)
    }
}

/// An opaque continuation token. Structure, version, precision and request binding are
/// validated by the server before the access decision; any failure is `cursor-invalid`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cursor(String);
impl Cursor {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cursor([opaque])")
    }
}
impl TryFrom<String> for Cursor {
    type Error = InvalidBoundaryValue;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > CURSOR_MAX_LEN
            || !value.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(InvalidBoundaryValue);
        }
        Ok(Self(value))
    }
}
impl From<Cursor> for String {
    fn from(value: Cursor) -> Self {
        value.0
    }
}

/// Trusted read call metadata: no version, no idempotency key.
#[derive(Debug, Clone, Default)]
pub struct ReadMeta {
    pub correlation_id: Option<Uuid>,
    /// Supplied delegation proof reference, forwarded to the PDP decision of the call and
    /// recorded as supplied on any access-log row (08 §4.4).
    pub delegation_proof_ref: Option<DelegationProofRef>,
}

/// Order-list filters: `state` exact, `created_from` inclusive, `created_to` exclusive,
/// `state_entered_before` inclusive (continuously in the current state since that instant or
/// earlier), `contract_id` exact. Over REST they are the `OData` `$filter` terms `state eq`,
/// `created_at ge`, `created_at lt`, `state_entered_at le` and `contract_id eq` joined by `and`
/// (D-212). Timestamps are RFC 3339 instants; stored microseconds are compared exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<OrderState>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_from: Option<OffsetDateTime>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_to: Option<OffsetDateTime>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_entered_before: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_id: Option<Uuid>,
}

/// The order-list request (`models.json` `ListOrders`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOrders {
    pub filters: OrderFilters,
    /// Absent means the default of 50.
    pub limit: Option<PageSize>,
    pub cursor: Option<Cursor>,
}

/// The per-order line-list request (`models.json` `LineList`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineList {
    /// Absent means the default of 50.
    pub limit: Option<PageSize>,
    pub cursor: Option<Cursor>,
}

/// One order-list element: the aggregate header at its current commercial version, with the
/// draft revision while the order is a draft. The composed detail is the point read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderSummary {
    pub order: OrderHeader,
    pub current_version: OrderVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_revision: Option<DraftRevision>,
}

/// Page metadata in the shared list format. `next_cursor` is present only when another
/// authorized row followed the page; reads are forward-only, so `prev_cursor` is always absent;
/// `limit` is the effective page size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<Cursor>,
    pub limit: PageSize,
}
impl PageInfo {
    /// A forward page of `limit` rows, continued by `next_cursor` when more rows follow.
    #[must_use]
    pub fn forward(next_cursor: Option<Cursor>, limit: PageSize) -> Self {
        Self {
            next_cursor,
            prev_cursor: None,
            limit,
        }
    }
}

/// One authorized keyset page of orders in `(created_at, order_id)` order (`models.json`
/// `OrderPage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderPage {
    pub items: Vec<OrderSummary>,
    pub page_info: PageInfo,
}

/// One authorized keyset page of an order's current lines in `(created_at, line_id)` order of
/// the line identity (`models.json` `LinePage`). While the order is a draft the members are the
/// working set as authored; `current_version` and `draft_revision` come from the same
/// consistent snapshot as the lines, and the response `ETag` carries `current_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinePage {
    pub items: Vec<DraftLine>,
    pub page_info: PageInfo,
    pub current_version: OrderVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_revision: Option<DraftRevision>,
}

/// The version-history request (`models.json` `VersionList`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionList {
    /// Absent means the default of 50.
    pub limit: Option<PageSize>,
    pub cursor: Option<Cursor>,
}

/// The consent-history request (`models.json` `AcceptanceList`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptanceList {
    /// Absent means the default of 50.
    pub limit: Option<PageSize>,
    pub cursor: Option<Cursor>,
}

/// Why a committed version was appended (`orders_order_version.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionReason {
    /// Version 1, the empty draft.
    Create,
    /// The version a submit committed.
    Submit,
    /// A version an amendment committed.
    Amendment,
}
impl VersionReason {
    /// The reason a stored token names; `None` for any other spelling.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "create" => Some(Self::Create),
            "submit" => Some(Self::Submit),
            "amendment" => Some(Self::Amendment),
            _ => None,
        }
    }
}

/// One committed version of the order's history, read from the stored version row (never a
/// chain or event reconstruction). Reserved or burned candidate numbers are never items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionRecord {
    pub version: OrderVersion,
    pub reason: VersionReason,
    /// The trusted subject that appended the version.
    pub actor: String,
    /// The appending subject's tenant.
    pub actor_tenant_id: Uuid,
    /// The engine instant the version was appended.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The caller's explanation; present exactly on an amendment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amendment_reason: Option<String>,
    /// The predecessor; absent exactly on version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_version: Option<OrderVersion>,
}

/// Why consent was required when it was recorded (`orders_acceptance.requirement_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementSource {
    Contract,
    Seller,
    PlatformDefault,
    /// Recorded although no requirement applied.
    Volunteered,
}
impl RequirementSource {
    /// The source a stored token names; `None` for any other spelling.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "contract" => Some(Self::Contract),
            "seller" => Some(Self::Seller),
            "platform_default" => Some(Self::PlatformDefault),
            "volunteered" => Some(Self::Volunteered),
            _ => None,
        }
    }
}

/// One recorded consent: the committed version it accepts, the engine instant, the trusted
/// recording subject, the recording path copied from the order's sales path and the source of
/// the requirement. Internal recovery evidence is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceRecord {
    pub accepted_version: OrderVersion,
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
    pub recorded_by: String,
    pub recording_path: SalesPath,
    pub requirement_source: RequirementSource,
}

/// One authorized keyset page of the committed version history in `version` order
/// (`models.json` `VersionPage`). `current_version` and `current_acceptance` come from the same
/// consistent snapshot as the items; an absent `current_acceptance` means the current version has
/// no recorded consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionPage {
    pub items: Vec<VersionRecord>,
    pub page_info: PageInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<OrderVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_acceptance: Option<AcceptanceRecord>,
}

/// The consent read (`models.json` `AcceptancePage`): the current version, its consent (absent
/// when none is recorded) and one keyset page of the prior versions' consent records in
/// `accepted_version` order, all from one consistent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptancePage {
    /// Consent records of versions before the current one.
    pub items: Vec<AcceptanceRecord>,
    pub page_info: PageInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<OrderVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_acceptance: Option<AcceptanceRecord>,
}

/// One exact committed version (`models.json` `OrderVersionView`): its lines with their
/// immutable commercial evidence and its persisted producer totals, composed at that version.
/// Version 1 (the empty draft) has no lines and no totals. The mutable administrative projection
/// (`admin`) is not delivered (D-254).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderVersionView {
    pub order_id: Uuid,
    pub version: OrderVersion,
    pub lines: Vec<DraftLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_total: Option<Vec<CommittedTotal>>,
}

/// Immutable commercial evidence selected by the committed version, never a live quotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedLine {
    pub commercial_attempt_id: Uuid,
    pub order_pin: crate::commercial::OrderPin,
}

/// Producer-resolved amounts copied from the committed version. Decimal amounts retain their
/// exact producer spelling; evidence documents are the producer's versioned JSON contracts.
///
/// The token fields (`scope`, `amount_status`, `amount_basis`, `discount_status`, `charge_kind`,
/// `tcv_basis`) are open strings by design, never enums: a stored body written by a newer build
/// may carry a token this build does not know, and replay must still decode it (D-233 item 3).
/// Consumers match the tokens they know and treat any other as unrecognized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedTotal {
    /// Which roll-up the row is: `line` (one line's total) or `order` (the order roll-up).
    pub scope: String,
    /// The line the row prices; the nil UUID for the `order` roll-up.
    pub line_id: Uuid,
    /// The evaluation currency code.
    pub currency: String,
    /// The pricing assessment the row was resolved under.
    pub assessment_id: Uuid,
    /// The evaluation currency's minor-unit scale (0..=28).
    pub currency_scale: u8,
    /// Producer exclusions, verbatim JSON.
    pub exclusions: serde_json::Value,
    /// Producer item breakdown, verbatim JSON.
    pub item_breakdown: serde_json::Value,
    /// Producer recurring amounts labelled by cycle, verbatim JSON.
    pub recurring_by_cycle: serde_json::Value,
    /// `committed`, or `uncommitted_usage` for a usage row that has no committed amount.
    pub amount_status: String,
    /// The producer-declared monetary horizon of the amounts.
    pub amount_basis: String,
    /// The producer's line, term and cycle evidence, verbatim JSON.
    pub period_evidence: serde_json::Value,
    /// The gross amount as an exact decimal string; absent exactly for a usage row.
    pub gross_exact: Option<String>,
    /// The gross amount's display text.
    pub gross_display: Option<String>,
    /// The net amount as an exact decimal string; absent exactly for a usage row.
    pub net_exact: Option<String>,
    /// The net amount's display text.
    pub net_display: Option<String>,
    /// `available` when the producer stated a discount, else `unavailable`.
    pub discount_status: String,
    /// The discount as an exact decimal string, when available.
    pub discount_exact: Option<String>,
    /// The discount's display text, when available.
    pub discount_display: Option<String>,
    /// The promotion the discount came from, when the producer named one.
    pub promotion_ref: Option<String>,
    /// `recurring`, `one_time` or `usage`.
    pub charge_kind: String,
    /// The total contract value as an exact decimal string (order roll-up of recurring charges).
    pub tcv_exact: Option<String>,
    /// The total contract value's display text.
    pub tcv_display: Option<String>,
    /// The valuation horizon the TCV declares (`finite_term`, `rolling_annualized`, `mixed`).
    pub tcv_basis: Option<String>,
    /// The producer's per-line TCV basis and policy version, verbatim JSON.
    pub tcv_evidence: Option<serde_json::Value>,
}
