//! Submit phase C (S3-12; 03 §3.6 step 15–27; D-188 steps 4–6, D-192, D-216): the commit of a
//! reserved commercial attempt through the one engine transaction. The preparation reads only
//! the attempt the engine loaded under the presented owner ([`PreparationView::frozen`]), its
//! frozen requests and evaluation and its recorded receipts; it never re-runs the gate.
//!
//! [`SubmitPreparation::prepare`], in order:
//! 1. the view's frozen execution must be this attempt (identity, candidate and frozen
//!    requests), else `Integration`; the frozen date basis is restored exactly
//!    ([`DateBasis::from_frozen`]);
//! 2. a draft revision other than the attempt's prepared one is left to the engine's precedence
//!    refusal (`version-conflict`, decided before any guard): nothing commercial is built;
//! 3. every line's recorded result is decoded. A recorded Pricing refusal settles the attempt
//!    refused through its registered guard ([`refusal_disposition`]); a line without a result
//!    when nothing refused is an integration failure;
//! 4. every receipt becomes the line's schema-2 pin ([`OrderPin::new`], the only constructor),
//!    bound to the committed row `(order, candidate, line)` and verified against the frozen
//!    query, the frozen request digest and the terms digest recomputed over the receipt's
//!    bindings ([`OrderPin::verify_selection`]) and, when the attempt froze the Rating request,
//!    against that request ([`verify_evaluated_query`], [`verify_evaluated_bindings`], D-197);
//!    any mismatch fails closed (`Integration`, D-192);
//! 5. the admitted line rows ([`admitted_line`] plus the commercial columns, the encoded pin and
//!    the claimed overlap key) and the `resolved_total` rows ([`total_rows`]) are built; an
//!    evaluation that cannot be stored refuses `evaluation-unavailable`, and one not in the
//!    frozen market currency is an integration failure (D-216);
//! 6. the guards: `gate.has-lines`, the shared structural guards, `gate.binding-admission`
//!    (a recorded lapsed hold, a single pin or the whole version over the byte limit, then
//!    every receipt's exclusive `hold_until` against the transition time),
//!    `capture.date-basis` and the composite gate. A single pin's size is judged when step 4
//!    builds it, before that receipt's selection verification: an oversized receipt settles
//!    `purchase-capacity-exceeded` even if it would also fail D-192/D-197 verification, and
//!    nothing commits either way (D-246 item 15);
//!    the version contribution names the reserved candidate, the claims are the frozen keys,
//!    the event is `OrderSubmitted`; the settlement carries the frozen gate report and the
//!    engine writes the frozen outcome vector as the run's diagnostics, whether this call
//!    assessed or recovered the attempt (D-231);
//! 7. automatic consent (S4-08a, D-146, D-251), only when nothing refused: decided from the
//!    frozen attempt (its original principal's tenant and proof reference) against the resource
//!    tenant the engine locked, so a recovered attempt decides as the fresh one did. A
//!    self-service commit writes the `acceptance` row of the committed candidate
//!    (`self_service`, the frozen subject's reference, the engine's transition time) with the
//!    source resolved from the frozen contract fact, then the seller and platform elections
//!    (a failed election read is `Unavailable`, never a default); `OrderSubmitted` carries
//!    `acceptance { acceptedVersion, acceptedAt }` from the engine's summary.
//!
//! Steps 1 and 3–6 are [`ReservedAttempt`]'s and shared with the amendment commit
//! (`super::amend_commit`), which differs only in its contribution, content rows, request
//! guards, documents and event. The engine does the rest: claims, version, documents,
//! diagnostics, aggregate, audit, event, settlement and `finish_execution`.
use super::failure::{Ids, submit_integration};
use super::writer::GateRun;
use crate::domain::capture::{self, category_verdict, currency_verdict, line_cap_verdict};
use crate::domain::contributions::{AggregateContribution, VersionContribution};
use crate::domain::dates::DateBasis;
use crate::domain::gate::commercial::{
    FrozenLine, FrozenRequests, ReceiptResult, RefusalDisposition, decode_receipt_result,
    digest_hex, persisted, refusal_disposition, verify_evaluated_bindings, verify_evaluated_query,
    version_capacity, version_row_bytes,
};
use crate::domain::gate::totals::{TotalRow, total_rows};
use crate::domain::guards::{GATE_COMPOSITE, GUARDS, InputPort};
use crate::domain::overlap::{ClaimParties, OverlapScopeKey, ProposedClaims};
use crate::domain::preconditions::{
    self, Requirement, RequirementSource, acceptance_requirement, recorded_source,
};
use crate::domain::transition::{GuardBindings, GuardSubject, GuardVerdict, InputState, Inputs};
use crate::infra::dates::admitted_line;
use crate::infra::engine::documents::{EventEnqueue, EventSpec};
use crate::infra::engine::{
    ChildDocuments, DocumentWriter, Preparation, PreparationView, PrepareError, Prepared,
};
use crate::infra::events::EventEnqueueError;
use crate::infra::events::payload::{LineRef, OrderSubmitted, SubmitAcceptance};
use crate::infra::policy::{self, Election, PolicyScope};
use crate::infra::storage::entity::{
    acceptance, commercial_attempt, draft_content, order_line, resolved_total,
};
use crate::infra::storage::repo::idempotency::Frozen;
use async_trait::async_trait;
use bss_orders_sdk::authoring::Category;
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::commercial::{
    AcceptedVersionRef, AssessmentRef, CodecError, OrderPin, SelectionEvidence,
};
use bss_orders_sdk::models::OrderVersion;
use bss_pricing_sdk::acceptance::Market;
use bss_pricing_sdk::digest::terms_digest;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use time::OffsetDateTime;
use toolkit_db::secure::ScopeError;
use uuid::Uuid;

/// The transition-table row submit runs (`draft → submitted`).
pub(super) const SUBMIT_ROW: u8 = 4;
/// The guard submit's at-least-one-line rule is registered under (03 step 1).
pub(super) const HAS_LINES: &str = "gate.has-lines";
/// The accepted-binding admission guard (03 step 15).
pub(super) const BINDING_ADMISSION: &str = "gate.binding-admission";
/// The date-basis guard (S2-10).
pub(super) const DATE_BASIS: &str = "capture.date-basis";

/// The reserved attempt phase C commits, as the service reserved or recovered it: shared by
/// submit ([`SubmitPreparation`]) and amendment (`super::amend_commit::AmendPreparation`).
pub struct ReservedAttempt {
    /// The reserved attempt.
    pub attempt_id: Uuid,
    /// Its candidate version.
    pub candidate_version: i32,
    /// Its stored `line_requests`, compared with the attempt the engine loaded.
    pub line_requests: Value,
    /// The decoded frozen requests and order facts.
    pub requests: FrozenRequests,
    /// The configured line cap.
    pub line_cap: usize,
}

/// Submit's commit preparation (module doc).
pub struct SubmitPreparation {
    /// The reserved attempt.
    pub(super) reserved: ReservedAttempt,
    /// The policy election store (step 7); `None` answers unavailable when an election is read.
    pub(super) policy: Option<toolkit_db::Db>,
}

/// The consent a self-service commit records (module doc, step 7).
#[derive(Debug, Clone)]
struct Consent {
    /// The trusted subject's reference (D-103: the lowercase hyphenated subject UUID).
    recorded_by: String,
    /// Where the requirement came from, or `volunteered`.
    source: RequirementSource,
}

/// The identity members of a frozen `original_principal` (`submit::attempt_inputs`).
#[derive(serde::Deserialize)]
struct FrozenPrincipal {
    subject_id: Uuid,
    subject_tenant_id: Uuid,
}

/// What one accepted line contributes.
struct AdmittedLine {
    row: order_line::Model,
    /// The persisted bytes of the line row ([`line_row_bytes`]), its encoded pin included.
    bytes: usize,
    hold_until: OffsetDateTime,
}

/// One accepted line's admission: its row, or a pin no version can hold.
enum Admission {
    Admitted(Box<AdmittedLine>),
    /// The line's schema-2 pin alone exceeds the version limit (`CodecError::TooLarge`).
    OverCapacity,
}

/// A recorded refusal or a commercial input the commit cannot admit, with where it settles.
#[derive(Clone, Copy)]
pub(super) enum Blocked {
    Pricing(RefusalDisposition),
    /// The evaluation cannot be stored (D-216).
    Totals,
    /// One line's schema-2 pin alone exceeds the version byte limit (D-235 item 7).
    Capacity,
}

fn integration(ids: Ids, what: &'static str) -> PrepareError {
    submit_integration(what, ids, None);
    PrepareError::Integration
}

/// [`integration`] keeping the cause's `Display` (never stored content, see [`super::failure`]).
fn integration_with(ids: Ids, what: &'static str, cause: &dyn std::fmt::Display) -> PrepareError {
    submit_integration(what, ids, Some(cause));
    PrepareError::Integration
}

/// What a reserved attempt's frozen evidence admits (module doc, steps 1 and 3–5), before the
/// row's own guards, contribution, documents and event are added.
pub(super) struct Evidence<'v> {
    /// The attempt the engine loaded under the presented owner.
    pub(super) attempt: &'v commercial_attempt::Model,
    /// A recorded refusal or unstorable totals; nothing commercial is admitted then.
    pub(super) blocked: Option<Blocked>,
    lines: Vec<AdmittedLine>,
    totals: Vec<TotalRow>,
    basis: DateBasis,
}

impl Evidence<'_> {
    /// The admitted line rows (empty when blocked).
    pub(super) fn line_rows(&self) -> Vec<order_line::Model> {
        self.lines.iter().map(|l| l.row.clone()).collect()
    }
}

impl ReservedAttempt {
    /// The reserved attempt of `attempt`, its frozen requests decoded.
    pub(super) fn of(
        attempt: &commercial_attempt::Model,
        requests: FrozenRequests,
        line_cap: usize,
    ) -> Self {
        Self {
            attempt_id: attempt.attempt_id,
            candidate_version: attempt.candidate_version,
            line_requests: attempt.line_requests.clone(),
            requests,
            line_cap,
        }
    }

    /// The identities a failure of this attempt names: the order its frozen requests are for
    /// and the attempt.
    fn ids(&self) -> Ids {
        Ids {
            order_id: self.requests.lines.first().map(|l| l.query.order_id),
            attempt_id: Some(self.attempt_id),
            line_id: None,
        }
    }

    /// Step 1: the attempt the engine loaded under the presented owner must be this one.
    pub(super) fn loaded<'v>(
        &self,
        view: &'v PreparationView,
    ) -> Result<&'v commercial_attempt::Model, PrepareError> {
        let ids = Ids::attempt(view.facts.order_id, self.attempt_id);
        let Some(Frozen::Commercial(attempt)) = &view.frozen else {
            return Err(integration(
                ids,
                "commit without the reserved commercial attempt",
            ));
        };
        if attempt.attempt_id != self.attempt_id
            || attempt.candidate_version != self.candidate_version
            || attempt.line_requests != self.line_requests
        {
            return Err(integration(
                ids,
                "the loaded attempt is not the reserved one",
            ));
        }
        Ok(attempt)
    }

    /// The one market every frozen line was assessed in.
    pub(super) fn market(&self) -> Result<&Market, PrepareError> {
        let market = &self
            .requests
            .lines
            .first()
            .ok_or_else(|| integration(self.ids(), "an attempt without lines"))?
            .query
            .market;
        if self
            .requests
            .lines
            .iter()
            .any(|l| l.query.market != *market)
        {
            return Err(integration(
                self.ids(),
                "frozen lines disagree on the market",
            ));
        }
        Ok(market)
    }

    /// The version contribution naming the reserved candidate, in the frozen market.
    pub(super) fn contribution(
        &self,
        payer_tenant_id: Uuid,
        category: String,
        contract_id: Option<Uuid>,
        amendment_reason: Option<String>,
    ) -> Result<VersionContribution, PrepareError> {
        let market = self.market()?;
        Ok(VersionContribution {
            candidate: self.candidate_version,
            payer_tenant_id,
            category,
            contract_id,
            market_currency: Some(market.currency.clone()),
            market_region: market.region.clone(),
            amendment_reason,
        })
    }

    /// Steps 1 and 3–5: the loaded attempt, its frozen basis, every line's recorded result and,
    /// when nothing refused, the verified pins, the admitted rows built from `content` (the
    /// content row of every frozen line, nothing else) and the totals rows.
    pub(super) fn evidence<'v>(
        &self,
        view: &'v PreparationView,
        content: &BTreeMap<Uuid, &draft_content::Model>,
    ) -> Result<Evidence<'v>, PrepareError> {
        let attempt = self.loaded(view)?;
        let ids = Ids::attempt(view.facts.order_id, self.attempt_id);
        let basis = DateBasis::from_frozen(&attempt.date_policy_basis)
            .map_err(|e| integration_with(ids, "the frozen date basis does not restore", &e))?;
        let market = self.market()?;
        if content.len() != self.requests.lines.len()
            || self
                .requests
                .lines
                .iter()
                .any(|l| !content.contains_key(&l.line_id))
        {
            return Err(integration(
                ids,
                "the content lines are not the frozen lines",
            ));
        }
        // 3. The recorded results.
        let mut blocked = None;
        let mut receipts = Vec::with_capacity(self.requests.lines.len());
        for line in &self.requests.lines {
            match attempt.receipt_results.get(line.line_id.to_string()) {
                Some(value) => match decode_receipt_result(value).map_err(|e| {
                    integration_with(
                        ids.line(line.line_id),
                        "a recorded receipt result does not decode",
                        &e,
                    )
                })? {
                    ReceiptResult::Accepted(receipt) => receipts.push((line, *receipt)),
                    ReceiptResult::Refused(reason) => {
                        blocked.get_or_insert(Blocked::Pricing(refusal_disposition(reason)));
                    }
                },
                None if blocked.is_some() => {}
                None => return Err(integration(ids, "a line without a recorded result")),
            }
        }
        // 4–5. Pins, line rows and totals, only when nothing refused.
        let mut lines = Vec::new();
        let mut totals = Vec::new();
        if blocked.is_none() {
            for (line, receipt) in receipts {
                let row = content.get(&line.line_id).ok_or_else(|| {
                    integration(ids.line(line.line_id), "a frozen line without its content")
                })?;
                match self.admitted(line, row, receipt, view.facts.resource_tenant_id, &basis)? {
                    Admission::Admitted(admitted) => lines.push(*admitted),
                    Admission::OverCapacity => {
                        lines.clear();
                        blocked = Some(Blocked::Capacity);
                        break;
                    }
                }
            }
            if blocked.is_none() {
                let line_ids: Vec<Uuid> = self.requests.lines.iter().map(|l| l.line_id).collect();
                match total_rows(&self.requests.order.evaluation, &line_ids, &market.currency) {
                    Ok(rows) => totals = rows,
                    Err(error) if error.is_contract_defect() => {
                        return Err(integration_with(
                            ids,
                            "the frozen evaluation is not in the market currency (D-216)",
                            &error,
                        ));
                    }
                    Err(error) => {
                        // D-216: a fraction without a terminating decimal, or a display finer
                        // than the currency scale.
                        tracing::warn!(
                            target: "orders.gate.submit",
                            error = %error,
                            "bss-orders: the frozen evaluation cannot be stored as submit totals"
                        );
                        blocked = Some(Blocked::Totals);
                    }
                }
            }
        }
        Ok(Evidence {
            attempt,
            blocked,
            lines,
            totals,
            basis,
        })
    }

    /// Step 6's shared part over `evidence`: the row's resolved inputs (a recorded unevaluable
    /// refusal or unstorable totals as the input that settles it), the commercial guards after
    /// the row's own (`structural`, binding admission, date basis, composite gate), the claims
    /// (the frozen keys of every line) and the frozen run (D-231). `own` are the row's request
    /// guards; `version_row` the persisted bytes of the version row; `item_capacity` the
    /// authored-selection capacity verdict over the committed content.
    #[expect(
        clippy::too_many_arguments,
        reason = "step 6 reads the evidence, the row's own guards, its structural facts, the \
                  version row bytes, the item capacity and the claim parties"
    )]
    pub(super) fn prepare_commercial(
        &self,
        prepared: &mut Prepared,
        evidence: &Evidence<'_>,
        own: GuardBindings,
        structural: &Structural,
        version_row: usize,
        item_capacity: GuardVerdict,
        parties: ClaimParties,
    ) -> Result<(), PrepareError> {
        // The version limit counts every persisted member: the line rows (pins included), the
        // totals rows and the version row.
        let persisted: Vec<usize> = evidence
            .lines
            .iter()
            .map(|l| l.bytes)
            .chain(evidence.totals.iter().map(TotalRow::persisted_bytes))
            .chain([version_row])
            .collect();
        let capacity = match item_capacity {
            GuardVerdict::Pass => version_capacity(persisted),
            failure @ GuardVerdict::Fail(_) => failure,
        };
        let earliest_hold = evidence.lines.iter().map(|l| l.hold_until).min();
        prepared.inputs = row_inputs_resolved(structural.row);
        if let Some(Blocked::Pricing(RefusalDisposition::Unevaluable(reason))) = evidence.blocked {
            prepared.inputs = prepared.inputs.clone().with(
                InputPort::CatalogPredicates,
                InputState::Unresolvable(reason),
            );
        }
        if let Some(Blocked::Totals) = evidence.blocked {
            prepared.inputs = prepared.inputs.clone().with(
                InputPort::Evaluation,
                InputState::Unresolvable(Reason::EvaluationUnavailable),
            );
        }
        prepared.guards = commercial_guards(
            structural.bind(own),
            evidence.blocked,
            capacity,
            earliest_hold,
            evidence.basis.clone(),
        );
        let keys = frozen_scope_keys(&self.requests.lines, self.ids())?;
        prepared.claims = Some(ProposedClaims::new(parties, keys));
        // The frozen run: a fresh and a recovered commit settle the same report and write the
        // same diagnostics (D-235).
        let order = &self.requests.order;
        let axes = &self
            .requests
            .lines
            .first()
            .ok_or_else(|| integration(self.ids(), "an attempt without lines"))?
            .query
            .tenant_axes;
        // The engine settles the report as its wire value (`Prepared::assessment`).
        prepared.assessment = Some(
            serde_json::to_value(&order.report)
                .map_err(|_| integration(self.ids(), "frozen report"))?,
        );
        prepared.assessment_id = Some(order.assessment_id);
        let run: Arc<GateRun> = Arc::new(GateRun {
            overlap_keys: self
                .requests
                .lines
                .iter()
                .map(|l| {
                    (
                        l.line_id,
                        l.scopes().into_iter().map(str::to_owned).collect(),
                    )
                })
                .collect(),
            run_id: order.assessment_id,
            resource_tenant_id: axes.resource_tenant_id,
            seller_tenant_id: axes.seller_tenant_id,
            payer_tenant_id: axes.payer_tenant_id,
            correlation_id: order.correlation_id,
            evaluated_at: order.assessed_at,
            vector: order.vector.clone(),
        });
        prepared.diagnostics = Some(run);
        Ok(())
    }

    /// The `resolved_total` rows of the admitted version (empty when blocked).
    pub(super) fn total_models(
        &self,
        order_id: Uuid,
        evidence: &Evidence<'_>,
    ) -> Vec<resolved_total::Model> {
        evidence
            .totals
            .iter()
            .map(|row| {
                total_model(
                    order_id,
                    self.candidate_version,
                    self.requests.order.assessment_id,
                    row,
                )
            })
            .collect()
    }

    /// One accepted line: its verified pin and admitted row (module doc, steps 4–5).
    fn admitted(
        &self,
        line: &FrozenLine,
        draft: &draft_content::Model,
        receipt: bss_pricing_sdk::acceptance::AcceptanceReceipt,
        resource_tenant_id: Uuid,
        basis: &DateBasis,
    ) -> Result<Admission, PrepareError> {
        let order = &self.requests.order;
        let ids = Ids {
            order_id: Some(line.query.order_id),
            attempt_id: Some(self.attempt_id),
            line_id: Some(line.line_id),
        };
        let version = OrderVersion::try_from(i64::from(self.candidate_version))
            .map_err(|_| integration(ids, "the candidate is not an order version"))?;
        // A NULL key is an exception only for a receipt without paid recurring bindings.
        // Recovery trusts the verified receipt, never a nullable field by itself.
        if !line.matches_bindings(&receipt.bindings) {
            return Err(integration(
                ids,
                "the frozen overlap evidence does not cover the receipt",
            ));
        }
        let terms = terms_digest(&line.query, &receipt.bindings);
        let (acceptance_id, hold_until) = (receipt.acceptance_id, receipt.hold_until);
        let bindings = receipt.bindings.clone();
        let pin = match OrderPin::new(
            AssessmentRef {
                assessment_id: order.assessment_id,
                assessed_at: order.assessed_at,
                resolve_date: order.resolve_date,
            },
            AcceptedVersionRef {
                order_id: line.query.order_id,
                order_version: version,
                line_id: line.line_id,
            },
            receipt,
        ) {
            Ok(pin) => pin,
            Err(CodecError::TooLarge) => {
                tracing::warn!(
                    target: "orders.gate.submit",
                    order_id = %line.query.order_id,
                    attempt_id = %self.attempt_id,
                    line_id = %line.line_id,
                    "bss-orders: a single pin exceeds the version limit"
                );
                return Ok(Admission::OverCapacity);
            }
            Err(error) => {
                return Err(integration_with(
                    ids,
                    "the receipt is not a valid schema-2 pin (D-192)",
                    &error,
                ));
            }
        };
        pin.verify_selection(
            &line.query,
            SelectionEvidence {
                acceptance_id,
                request_digest: line.request_digest,
                terms_digest: terms,
                evaluated_bindings: &bindings,
            },
        )
        .map_err(|_| integration(ids, "the receipt does not match the frozen request (D-192)"))?;
        // D-197: the receipt equals what Rating evaluated (selection, quantity, market, terms,
        // bindings); a legacy attempt without the frozen request is not compared.
        if let Some(evaluated) = &order.evaluation_request {
            verify_evaluated_query(&line.query, evaluated)
                .and_then(|()| verify_evaluated_bindings(&line.query, &bindings, evaluated))
                .map_err(|what| {
                    integration_with(
                        ids,
                        "the receipt does not match the evaluated request (D-197)",
                        &what,
                    )
                })?;
        }
        let encoded = pin
            .encode()
            .map_err(|_| integration(ids, "the pin does not encode"))?;
        let order_pin: Value = serde_json::from_slice(&encoded)
            .map_err(|_| integration(ids, "the encoded pin is not JSON"))?;
        let mut row = admitted_line(draft, resource_tenant_id, self.candidate_version, basis)
            .map_err(|e| {
                integration_with(ids, "the frozen basis does not admit the content line", &e)
            })?;
        let items: Vec<bss_orders_sdk::authoring::SelectedItem> =
            serde_json::from_value(row.selected_items.clone())
                .map_err(|_| integration(ids, "the admitted selections do not decode"))?;
        let selected = super::preview::gate_line(
            row.line_id,
            row.plan_id,
            row.plan_revision_id,
            &items,
            &row.currency,
        );
        if !crate::domain::gate::commercial::verify_commit_line(
            &line.query,
            row.order_id,
            u64::try_from(row.version).map_err(|_| integration(ids, "invalid admitted version"))?,
            &selected,
        ) {
            return Err(integration(
                ids,
                "the admitted row differs from the frozen query",
            ));
        }
        row.commercial_attempt_id = Some(self.attempt_id);
        row.pricing_acceptance_id = Some(acceptance_id);
        row.pricing_request_digest = Some(digest_hex(&line.request_digest));
        row.pricing_terms_digest = Some(digest_hex(&terms));
        row.order_pin = Some(order_pin);
        row.overlap_item_keys = line
            .overlap_item_keys
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| integration(ids, "item key encoding"))?;
        row.overlap_scope_key.clone_from(&line.overlap_scope_key);
        row.catalog_subscription_product_key
            .clone_from(&line.catalog_subscription_product_key);
        let bytes = line_row_bytes(&row);
        Ok(Admission::Admitted(Box::new(AdmittedLine {
            row,
            bytes,
            hold_until,
        })))
    }
}

#[async_trait]
impl Preparation for SubmitPreparation {
    async fn prepare(&self, view: &PreparationView) -> Result<Prepared, PrepareError> {
        let reserved = &self.reserved;
        // 1. The attempt the engine loaded under the presented owner.
        let attempt = reserved.loaded(view)?;
        let facts = &view.facts;
        let mut prepared = Prepared::new(AggregateContribution::Version(reserved.contribution(
            facts.payer_tenant_id,
            facts.category.clone(),
            facts.contract_id,
            None,
        )?));
        // 2. A changed draft: the engine refuses by precedence before any guard.
        if view.prepared_draft_revision != attempt.prepared_draft_revision {
            return Ok(prepared);
        }
        let drafts: BTreeMap<Uuid, &draft_content::Model> =
            view.draft_lines.iter().map(|l| (l.line_id, l)).collect();
        // 3–5. The recorded results, pins, line rows and totals.
        let evidence = reserved.evidence(view, &drafts)?;
        // 6. Guards, inputs, claims and the frozen run, over the locked working draft.
        let version_row = match &prepared.aggregate {
            AggregateContribution::Version(version) => {
                version_row_bytes(version, Trigger::Submit.token())
            }
            _ => {
                return Err(integration(
                    Ids::attempt(facts.order_id, reserved.attempt_id),
                    "submit without a version contribution",
                ));
            }
        };
        let structural = Structural::draft(view, reserved.line_cap);
        let lines = structural.lines;
        let own = GuardBindings::new().bind(HAS_LINES, move |_: &GuardSubject<'_>| {
            if lines == 0 {
                GuardVerdict::Fail(Reason::NoLines)
            } else {
                GuardVerdict::Pass
            }
        });
        reserved.prepare_commercial(
            &mut prepared,
            &evidence,
            own,
            &structural,
            version_row,
            super::submit::locked_consumed_item_capacity(view)?,
            ClaimParties {
                payer_tenant_id: facts.payer_tenant_id,
                resource_tenant_id: facts.resource_tenant_id,
            },
        )?;
        // 7. Consent, only for a commit (a refusal records nothing and emits no event).
        let consent = match evidence.blocked {
            None => self.consent(evidence.attempt, view).await?,
            Some(_) => None,
        };
        if evidence.blocked.is_none() {
            prepared.documents = Some(Arc::new(SubmitDocuments {
                lines: evidence.line_rows(),
                totals: reserved.total_models(facts.order_id, &evidence),
                version: reserved.candidate_version,
                consent: consent.clone(),
            }));
        }
        prepared.event = Some(Arc::new(SubmittedEvent {
            lines: reserved
                .requests
                .lines
                .iter()
                .map(|l| LineRef { line_id: l.line_id })
                .collect(),
            accepted: consent.is_some(),
        }));
        Ok(prepared)
    }
}

fn frozen_scope_keys(lines: &[FrozenLine], ids: Ids) -> Result<Vec<OverlapScopeKey>, PrepareError> {
    lines
        .iter()
        .flat_map(|line| line.scopes().into_iter().map(move |key| (line, key)))
        .map(|(line, key)| {
            OverlapScopeKey::try_from(key.to_owned()).map_err(|e| {
                integration_with(
                    ids.line(line.line_id),
                    "a frozen overlap key is invalid",
                    &e,
                )
            })
        })
        .collect()
}

impl SubmitPreparation {
    /// The commit preparation of `attempt` as phase C builds it (storage tests drive phase C
    /// for a chosen owner).
    #[cfg(test)]
    #[cfg_attr(
        not(feature = "gate-dev-stubs"),
        expect(
            dead_code,
            reason = "only the `gate-dev-stubs` storage and orchestrate tests use it"
        )
    )]
    pub fn for_attempt(
        attempt: &commercial_attempt::Model,
        line_cap: usize,
        policy: Option<toolkit_db::Db>,
    ) -> Option<Self> {
        Some(Self {
            reserved: ReservedAttempt::of(
                attempt,
                crate::domain::gate::commercial::decode_line_requests(&attempt.line_requests)
                    .ok()?,
                line_cap,
            ),
            policy,
        })
    }

    /// Step 7 (module doc): the consent of a self-service commit, `None` otherwise.
    async fn consent(
        &self,
        attempt: &commercial_attempt::Model,
        view: &PreparationView,
    ) -> Result<Option<Consent>, PrepareError> {
        let principal: FrozenPrincipal = serde_json::from_value(attempt.original_principal.clone())
            .map_err(|_| {
                integration(
                    Ids::attempt(view.facts.order_id, self.reserved.attempt_id),
                    "the frozen original principal does not decode",
                )
            })?;
        if !preconditions::auto_consent(
            principal.subject_tenant_id,
            view.facts.resource_tenant_id,
            attempt.proof_reference.is_some(),
        ) {
            return Ok(None);
        }
        let requirement = self.requirement(view.facts.seller_tenant_id).await?;
        Ok(Some(Consent {
            recorded_by: principal.subject_id.hyphenated().to_string(),
            source: recorded_source(requirement),
        }))
    }

    /// The requirement's precedence (D-251): the frozen contract fact, then the seller's
    /// election, then the platform's; each election is read only when the earlier term is
    /// silent.
    async fn requirement(&self, seller: Uuid) -> Result<Requirement, PrepareError> {
        let contract = self.reserved.requests.order.contract_acceptance_required;
        if contract.is_some() {
            return Ok(acceptance_requirement(contract, None, None));
        }
        let seller = self.election(PolicyScope::Seller(seller)).await?;
        let platform = match seller {
            Some(_) => None,
            None => self.election(PolicyScope::Platform).await?,
        };
        Ok(acceptance_requirement(None, seller, platform))
    }

    /// One `acceptance_required` election; a failed read is `Unavailable` (never "no row").
    async fn election(&self, scope: PolicyScope) -> Result<Option<bool>, PrepareError> {
        let Some(db) = &self.policy else {
            tracing::warn!(
                target: "orders.gate.submit",
                "bss-orders: no policy election store is bound; a self-service submit answers \
                 unavailable"
            );
            return Err(PrepareError::Unavailable);
        };
        policy::election(db, Election::AcceptanceRequired, scope)
            .await
            .map_err(|error| {
                tracing::warn!(
                    target: "orders.gate.submit",
                    scope = scope.token(),
                    error = %error,
                    "bss-orders: the acceptance election could not be read; submit answers \
                     unavailable"
                );
                PrepareError::Unavailable
            })
    }
}

/// The shared structural guards of rows 2, 4 and 18–20 over one line set (01 §3.6):
/// `capture.category-admitted` (the named category, else the locked one), `currency-mixed` and
/// `line-cap-exceeded` over the complete line set. Submit evaluates them over the engine's
/// locked working draft; an amendment over its proposed version.
pub struct Structural {
    /// The transition-table row whose declared inputs the preparation resolves.
    pub row: u8,
    /// A category the request names; `None` reads the locked one.
    pub named_category: Option<Category>,
    /// The line count.
    pub lines: usize,
    /// Every line's currency.
    pub currencies: Vec<String>,
    /// The configured line cap.
    pub cap: usize,
}

impl Structural {
    /// Row 4 over the engine's locked working draft.
    pub(super) fn draft(view: &PreparationView, cap: usize) -> Self {
        Self {
            row: SUBMIT_ROW,
            named_category: None,
            lines: view.draft_lines.len(),
            currencies: view
                .draft_lines
                .iter()
                .map(|l| l.currency.clone())
                .collect(),
            cap,
        }
    }

    /// The first refusal these guards decide, in registration order (an amendment's pre-call
    /// check, D-113): category, currency, then the line cap.
    pub fn first_failure(&self, locked_category: &str) -> Option<Reason> {
        [
            category_verdict(self.named_category, locked_category),
            currency_verdict(self.currencies.iter().map(String::as_str)),
            line_cap_verdict(self.lines, self.cap),
        ]
        .into_iter()
        .find_map(|verdict| match verdict {
            GuardVerdict::Fail(reason) => Some(reason),
            GuardVerdict::Pass => None,
        })
    }

    /// `bindings` with the three structural guards bound.
    pub(super) fn bind(&self, bindings: GuardBindings) -> GuardBindings {
        let named = self.named_category;
        let currencies = self.currencies.clone();
        let (lines, cap) = (self.lines, self.cap);
        bindings
            .bind(capture::CATEGORY, move |subject: &GuardSubject<'_>| {
                category_verdict(named, &subject.locked.category)
            })
            .bind(capture::CURRENCY, move |_: &GuardSubject<'_>| {
                currency_verdict(currencies.iter().map(String::as_str))
            })
            .bind(capture::LINE_CAP, move |_: &GuardSubject<'_>| {
                line_cap_verdict(lines, cap)
            })
    }
}

/// The commit's guards after the structural ones (module doc, step 6): a recorded lapsed hold,
/// the version limit, then each receipt's exclusive activation deadline against the engine's
/// transition time (03 step 15, D-152); the frozen date basis; the composite gate's recorded
/// Pricing refusal.
fn commercial_guards(
    bindings: GuardBindings,
    blocked: Option<Blocked>,
    capacity: GuardVerdict,
    earliest_hold: Option<OffsetDateTime>,
    basis: DateBasis,
) -> GuardBindings {
    let admission = match blocked {
        Some(Blocked::Pricing(RefusalDisposition::BindingAdmission(reason))) => {
            GuardVerdict::Fail(reason)
        }
        Some(Blocked::Capacity) => GuardVerdict::Fail(Reason::PurchaseCapacityExceeded),
        Some(
            Blocked::Pricing(RefusalDisposition::Composite(_) | RefusalDisposition::Unevaluable(_))
            | Blocked::Totals,
        )
        | None => capacity,
    };
    let composite = match blocked {
        Some(Blocked::Pricing(RefusalDisposition::Composite(reason))) => GuardVerdict::Fail(reason),
        Some(
            Blocked::Pricing(
                RefusalDisposition::BindingAdmission(_) | RefusalDisposition::Unevaluable(_),
            )
            | Blocked::Totals
            | Blocked::Capacity,
        )
        | None => GuardVerdict::Pass,
    };
    bindings
        .bind(
            BINDING_ADMISSION,
            move |subject: &GuardSubject<'_>| match (admission, earliest_hold) {
                (GuardVerdict::Fail(reason), _) => GuardVerdict::Fail(reason),
                (GuardVerdict::Pass, Some(hold)) if hold <= subject.transition_time => {
                    GuardVerdict::Fail(Reason::OrderBindingExpired)
                }
                (GuardVerdict::Pass, _) => GuardVerdict::Pass,
            },
        )
        .bind(DATE_BASIS, Arc::new(basis).guard())
        .bind(GATE_COMPOSITE, move |_: &GuardSubject<'_>| composite)
}

/// Every input `row` declares, resolved: the receipts are the evidence (module doc).
pub(super) fn row_inputs_resolved(row: u8) -> Inputs {
    row_inputs(row, InputState::Resolved)
}

/// Every input `row` declares, in `state`.
pub(super) fn row_inputs(row: u8, state: InputState) -> Inputs {
    GUARDS
        .iter()
        .filter(|g| g.rows.contains(&row))
        .flat_map(|g| g.inputs.iter().copied())
        .fold(Inputs::new(), |inputs, port| inputs.with(port, state))
}

/// The persisted bytes of one admitted `order_line` row, every column counted ([`persisted`]),
/// the encoded schema-2 pin included.
fn line_row_bytes(row: &order_line::Model) -> usize {
    persisted::UUID
        + persisted::INT
        + persisted::UUID
        + persisted::opt(row.commercial_attempt_id.as_ref(), persisted::UUID)
        + persisted::opt(row.pricing_acceptance_id.as_ref(), persisted::UUID)
        + persisted::opt_text(row.pricing_request_digest.as_deref())
        + persisted::opt_text(row.pricing_terms_digest.as_deref())
        + persisted::UUID
        + persisted::UUID
        + persisted::json(&row.selected_items)
        + persisted::text(&row.currency)
        + persisted::DATE
        + persisted::opt(row.service_activation_date.as_ref(), persisted::DATE)
        + persisted::opt(row.acceptance_due_date.as_ref(), persisted::DATE)
        + row
            .term_duration
            .as_ref()
            .map_or(0, |_| persisted::INTERVAL)
        + persisted::text(&row.term_kind)
        + persisted::json(&row.authored_term)
        + persisted::text(&row.billing_cycle)
        + row.order_pin.as_ref().map_or(0, persisted::json)
        + persisted::opt_text(row.overlap_scope_key.as_deref())
        + row.overlap_item_keys.as_ref().map_or(0, persisted::json)
        + persisted::opt_text(row.catalog_subscription_product_key.as_deref())
        + persisted::json(&row.date_policy_switch_state)
}

/// The admitted version's child documents: line rows, totals rows, then the self-service
/// consent of that version (step 19).
struct SubmitDocuments {
    lines: Vec<order_line::Model>,
    totals: Vec<resolved_total::Model>,
    version: i32,
    consent: Option<Consent>,
}

/// Insert the admitted version's line rows, then its totals rows (step 19).
pub(super) async fn write_version_rows(
    documents: &ChildDocuments<'_, '_>,
    lines: &[order_line::Model],
    totals: &[resolved_total::Model],
) -> Result<(), ScopeError> {
    documents.insert_order_lines(lines.to_vec()).await?;
    documents.insert_resolved_totals(totals.to_vec()).await
}

#[async_trait]
impl DocumentWriter for SubmitDocuments {
    async fn write(&self, documents: &ChildDocuments<'_, '_>) -> Result<(), ScopeError> {
        write_version_rows(documents, &self.lines, &self.totals).await?;
        if let Some(consent) = &self.consent {
            documents
                .insert_acceptance(acceptance::Model {
                    order_id: documents.order_id(),
                    accepted_version: self.version,
                    accepted_at: documents.transition_time(),
                    recorded_by: consent.recorded_by.clone(),
                    recording_path: preconditions::SELF_SERVICE.to_owned(),
                    requirement_source: consent.source.token().to_owned(),
                })
                .await?;
        }
        Ok(())
    }
}

/// Row 4's `OrderSubmitted`. Its acceptance names the committed version and the engine's
/// transition time, both from the summary the engine hands the enqueue, so they equal what
/// [`SubmitDocuments`] stored (module doc, step 7).
struct SubmittedEvent {
    lines: Vec<LineRef>,
    /// Whether this commit recorded the self-service consent.
    accepted: bool,
}

#[async_trait]
impl EventSpec for SubmittedEvent {
    fn kind(&self) -> bss_orders_sdk::catalog::EventKind {
        bss_orders_sdk::catalog::EventKind::OrderSubmitted
    }
    async fn enqueue(&self, enqueue: EventEnqueue<'_, '_>) -> Result<(), EventEnqueueError> {
        let acceptance = self.accepted.then(|| SubmitAcceptance {
            accepted_version: enqueue.order_version(),
            accepted_at: enqueue.occurred_at(),
        });
        enqueue
            .enqueue(OrderSubmitted {
                lines: self.lines.clone(),
                acceptance,
            })
            .await
    }
}

/// One `resolved_total` row from its content (`domain::gate::totals`).
fn total_model(
    order_id: Uuid,
    version: i32,
    assessment_id: Uuid,
    row: &TotalRow,
) -> resolved_total::Model {
    let (discount_exact, discount_display) = row
        .discount
        .clone()
        .map_or((None, None), |(e, d)| (Some(e), Some(d)));
    let (tcv_exact, tcv_display, tcv_basis, tcv_evidence) = row
        .tcv
        .clone()
        .map_or((None, None, None, None), |(e, d, b, ev)| {
            (Some(e), Some(d), Some(b.token().to_owned()), Some(ev))
        });
    resolved_total::Model {
        order_id,
        version,
        scope: row.scope.token().to_owned(),
        line_id: row.line_id,
        currency: row.currency.clone(),
        assessment_id,
        currency_scale: i32::from(row.currency_scale),
        exclusions: row.exclusions.clone(),
        item_breakdown: row.item_breakdown.clone(),
        recurring_by_cycle: row.recurring_by_cycle.clone(),
        amount_status: row.amount_status.token().to_owned(),
        amount_basis: row.amount_basis.clone(),
        period_evidence: row.period_evidence.clone(),
        gross_exact: row.gross.as_ref().map(|g| g.0.clone()),
        gross_display: row.gross.as_ref().map(|g| g.1.clone()),
        net_exact: row.net.as_ref().map(|n| n.0.clone()),
        net_display: row.net.as_ref().map(|n| n.1.clone()),
        discount_status: if row.discount.is_some() {
            "available"
        } else {
            "unavailable"
        }
        .to_owned(),
        discount_exact,
        discount_display,
        promotion_ref: None,
        charge_kind: row.charge_kind.token().to_owned(),
        tcv_exact,
        tcv_display,
        tcv_basis,
        tcv_evidence,
    }
}
