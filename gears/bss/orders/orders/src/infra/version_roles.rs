//! The role rows of a version chain: the creator (version 1, `create`), the submitter (the
//! `submit` row) and the amender (an `amendment` row). One lookup for the version read's role
//! actors (D-260 item 3, `infra::read`) and record-acceptance's recording-party bar (05 §3.2
//! step 1, D-256, `infra::acceptance`); each caller states which submit row it accepts.
use bss_orders_sdk::catalog::Trigger;

use crate::infra::storage::entity::order_version;

/// Which `submit` row a caller accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitRow {
    /// Any `submit` row among the rows read. Record-acceptance reads the role rows of an
    /// admissible order at its expected version, which therefore has its one submit row.
    Any,
    /// A `submit` row whose version is at or before the requested one. The version read may
    /// name any version, and a later submit row (none exists today; history is sparse, D-188)
    /// must never name a submitter of an earlier version.
    AtOrBefore(i32),
}

/// Version 1's `create` row.
pub fn creator(rows: &[order_version::Model]) -> Option<&order_version::Model> {
    rows.iter()
        .find(|row| row.version == 1 && row.reason == Trigger::Create.token())
}

/// The `submit` row the caller accepts.
pub fn submitter(
    rows: &[order_version::Model],
    accept: SubmitRow,
) -> Option<&order_version::Model> {
    rows.iter().find(|row| {
        row.reason == Trigger::Submit.token()
            && match accept {
                SubmitRow::Any => true,
                SubmitRow::AtOrBefore(version) => row.version <= version,
            }
    })
}

/// Whether `row` was appended by an amendment, so its actor is the amender.
pub fn is_amendment(row: &order_version::Model) -> bool {
    row.reason == Trigger::Amendment.token()
}
