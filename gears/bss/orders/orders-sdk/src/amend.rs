//! The amendment request (S4-03; 04 §3.6 *Append Amendment*; Stage 4 DESIGN §3).
//!
//! An amendment of a submitted, pending-approval or approved order is a delta onto committed
//! version N: the explanation, the order-level fields it names and add/change/remove operations
//! on lines by stable id. Everything the delta does not name carries forward from N. The wire is
//! `snake_case` like every other authoring body (D-206) and every level refuses unknown keys. The
//! header and line patches are the capture authoring types, so presence names a field and the
//! one field classification decides what each name means. `If-Match` is not a body member: it is
//! `CallMeta.expected_version`.
use crate::authoring::{AddLine, HeaderPatch, LinePatch};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An amendment request. The reason is not validated here: a missing, empty or oversized
/// explanation is the audited engine refusal `amendment-reason-invalid` (D-129), not a malformed
/// body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmendOrder {
    /// Explanation, 1-4096 characters (engine guard `amendment-reason-invalid`); an absent
    /// member decodes as the empty text the guard then refuses.
    #[serde(default)]
    pub reason: String,
    /// Order-level fields to change; absent names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderPatch>,
    /// Line operations; a line id is named at most once (boundary `request-invalid`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<AmendLine>,
}

/// One line operation of an amendment, by stable line id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum AmendLine {
    /// Add a line; the amendment allocates its id.
    Add {
        /// The new line.
        line: AddLine,
    },
    /// Change the named fields of a retained line.
    Change {
        /// The retained line.
        line_id: Uuid,
        /// The fields to change.
        patch: LinePatch,
    },
    /// Remove a line of the committed version.
    Remove {
        /// The line to remove.
        line_id: Uuid,
    },
}
