//! Amendment phase C (S4-04; 04 §3.6 *Append Amendment* steps 5–11; Stage 4 DESIGN §3
//! *Commit*): the commit of a reserved amendment attempt through the one engine transaction.
//! Like submit's [`super::commit::SubmitPreparation`] it reads only the attempt the engine loaded
//! under the presented owner, its frozen requests and recorded receipts, through the same
//! [`ReservedAttempt`] steps; it never re-runs the gate. What differs:
//!
//! 1. the version contribution names the proposed header (payer, category, contract) and the
//!    request's explanation as `amendment_reason`; the engine appends it with
//!    `supersedes_version = N` and reason `amendment`, writes the same explanation as the audit
//!    `caller_reason`, counts the amendment once and moves the pointer and the claims;
//! 2. when the locked current version is no longer N (or the service could not merge onto N
//!    because a later version exists), nothing commercial is built: the engine refuses by
//!    precedence before any guard (`precondition-failed`), never rebasing the attempt, and burns
//!    the candidate;
//! 3. the admitted rows are built from the proposed version merged onto N (every proposed line,
//!    retained lines included), never from N's evidence: N's receipts, consent and verdict are
//!    not M's;
//! 4. the guards are rows 18–20's: the seven versioning guards over the request facts (the cap
//!    over the locked count), the structural guards over the proposed version, then the commit's
//!    binding admission, date basis and composite gate;
//! 5. the claims are the complete proposed set under the proposed payer; the documents reserve
//!    every added line's identity, then insert M's line and totals rows; no consent is recorded
//!    for M; the event is exactly one `OrderAmended { supersedesVersion: N }`.
use super::commit::{ReservedAttempt, Structural, write_version_rows};
use crate::domain::contributions::AggregateContribution;
use crate::domain::gate::commercial::{consumed_item_capacity, version_row_bytes};
use crate::domain::overlap::ClaimParties;
use crate::domain::versioning::{AmendmentFacts, ProposedVersion};
use crate::infra::engine::documents::EventDetail;
use crate::infra::engine::{
    ChildDocuments, DocumentWriter, Preparation, PreparationView, PrepareError, Prepared,
};
use crate::infra::events::payload::OrderAmended;
use crate::infra::storage::entity::{draft_content, order_line, resolved_total};
use async_trait::async_trait;
use bss_orders_sdk::catalog::Trigger;
use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_db::secure::ScopeError;
use uuid::Uuid;

/// The transition-table row whose declared inputs an amendment resolves (rows 18–20 declare the
/// same guards and inputs; the engine picks the row from the locked state).
pub const AMEND_ROW: u8 = 18;

/// The proposed version merged onto N, with what the commit admits it from.
pub struct Proposal {
    /// The merge of the delta onto N.
    pub version: ProposedVersion,
    /// The request facts of the seven versioning guards.
    pub facts: AmendmentFacts,
    /// Every proposed line as its content row, in proposed order.
    pub content: Vec<draft_content::Model>,
    /// The configured line cap.
    pub line_cap: usize,
}

impl Proposal {
    /// The structural guards of rows 18–20 over the proposed version.
    pub fn structural(&self) -> Structural {
        Structural {
            row: AMEND_ROW,
            named_category: Some(self.version.header.category),
            lines: self.version.lines.len(),
            currencies: self
                .version
                .lines
                .iter()
                .map(|l| l.line.currency.as_str().to_owned())
                .collect(),
            cap: self.line_cap,
        }
    }

    /// The authored-selection capacity verdict over the proposed version.
    pub fn item_capacity(&self) -> crate::domain::transition::GuardVerdict {
        consumed_item_capacity(
            self.version
                .lines
                .iter()
                .map(|l| l.line.selected_items.len()),
        )
    }
}

/// An amendment's commit preparation (module doc).
pub struct AmendPreparation {
    /// The reserved attempt.
    pub reserved: ReservedAttempt,
    /// N: the committed version the request names (`If-Match`).
    pub base_version: i32,
    /// The proposal merged onto N; `None` when N is no longer current (module doc, step 2).
    pub proposal: Option<Proposal>,
    /// The request's explanation (`amendment_reason`, audit `caller_reason`).
    pub reason: String,
}

#[async_trait]
impl Preparation for AmendPreparation {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let reserved = &self.reserved;
        reserved.loaded(view)?;
        let facts = &view.facts;
        let (payer, category, contract) = match &self.proposal {
            Some(proposal) => (
                proposal.version.header.payer_tenant_id,
                proposal.version.header.category.gts_id().to_owned(),
                proposal.version.header.contract_id,
            ),
            None => (
                facts.payer_tenant_id,
                facts.category.clone(),
                facts.contract_id,
            ),
        };
        let contribution =
            reserved.contribution(payer, category, contract, Some(self.reason.clone()))?;
        let version_row = version_row_bytes(&contribution, Trigger::Amendment.token());
        let mut prepared = Prepared::new(AggregateContribution::Version(contribution));
        // 2. N is no longer current: the engine refuses by precedence before any guard.
        let Some(proposal) = &self.proposal else {
            return Ok(prepared);
        };
        if facts.current_version != self.base_version {
            return Ok(prepared);
        }
        // 3. Every proposed line's content row, and the recorded evidence over it.
        let content: BTreeMap<Uuid, &draft_content::Model> =
            proposal.content.iter().map(|l| (l.line_id, l)).collect();
        let evidence = reserved.evidence(view, &content)?;
        // 4–5. Rows 18–20's guards, inputs, the complete proposed claim set and the frozen run.
        reserved.prepare_commercial(
            &mut prepared,
            &evidence,
            proposal.facts.clone().bindings(),
            &proposal.structural(),
            version_row,
            proposal.item_capacity(),
            ClaimParties {
                payer_tenant_id: payer,
                resource_tenant_id: facts.resource_tenant_id,
            },
        )?;
        if evidence.blocked.is_none() {
            let added = proposal
                .version
                .lines
                .iter()
                .filter(|l| l.origin == crate::domain::versioning::LineOrigin::Added)
                .map(|l| l.line.line_id)
                .collect();
            prepared.documents = Some(Arc::new(AmendDocuments {
                added,
                lines: evidence.line_rows(),
                totals: reserved.total_models(facts.order_id, &evidence),
            }));
        }
        prepared.event = Some(Arc::new(EventDetail(OrderAmended {
            supersedes_version: self.base_version,
        })));
        Ok(prepared)
    }
}

/// M's child documents: every added line's identity, then the line and totals rows. No consent:
/// nothing from N is evidence for M.
struct AmendDocuments {
    added: Vec<Uuid>,
    lines: Vec<order_line::Model>,
    totals: Vec<resolved_total::Model>,
}

#[async_trait]
impl DocumentWriter for AmendDocuments {
    async fn write(&self, documents: &ChildDocuments<'_, '_>) -> Result<(), ScopeError> {
        for line_id in &self.added {
            documents.reserve_line_identity(*line_id).await?;
        }
        write_version_rows(documents, &self.lines, &self.totals).await
    }
}
