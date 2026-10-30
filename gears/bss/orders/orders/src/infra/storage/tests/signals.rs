//! The Stage 4 fault-injection matrix (S4-12, D-257 item 3; the D-237 item 6 pattern): one test
//! per injected condition, each proving that the observation behind its alert is emitted by the
//! engine at the decision point, on real PostgreSQL. Every engine the harness builds reports to
//! the recording sink `T::transitions`. Row 1 (amendment assessment failure) is a gate row:
//! `infra/gate/tests/faults.rs` `alert_9_amendment_assessment_failure_is_its_own_flow`.
//! Compiled only with `gate-dev-stubs`.
use super::acceptance::{customer, partner, record, refusal, world};
use super::amend::{amend, change_quantity, force, reflect, submitted};
use super::capture::{buyer, write};
use super::fulfillment::{approved, begin, resolver, tolerate};
use super::submit::{SELLER, contract_fixture, setup_with, two_line_fixture};
use super::*;
use crate::domain::preconditions::RequirementSource;
use crate::infra::capture::SettledOutcome;
use crate::infra::engine::OutcomeKind;
use crate::infra::engine::signals::{ConsentObservation, Decision, TransitionObservation};
use crate::infra::gate::tests::orchestrate::CONTRACT;
use crate::infra::gate::unavailable::ContractsUnavailable;
use bss_orders_sdk::authoring::{HeaderPatch, SalesPath};
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::fulfillment::Authorization;
use std::sync::Arc;

/// The earliest acceptance due date of `order`'s consented version and the consent's margin
/// against it in whole days (positive: recorded before it), from the stored rows.
async fn due_and_margin(t: &super::capture::T, order: Uuid) -> (time::Date, i64) {
    let row = t
        .json(&format!(
            "SELECT jsonb_build_array(\
             (SELECT min(l.acceptance_due_date) FROM bss_orders__order_line l \
              WHERE l.order_id=a.order_id AND l.version=a.accepted_version), \
             (a.accepted_at AT TIME ZONE 'UTC')::date) AS j \
             FROM bss_orders__acceptance a WHERE a.order_id='{order}'"
        ))
        .await;
    let date = |v: &serde_json::Value| {
        time::Date::parse(
            v.as_str().unwrap(),
            time::macros::format_description!("[year]-[month]-[day]"),
        )
        .unwrap()
    };
    let (due, accepted) = (date(&row[0]), date(&row[1]));
    (due, (due - accepted).whole_days())
}

/// The observations of `trigger` recorded by `t`'s engines, in order.
fn of(t: &super::capture::T, trigger: Trigger) -> Vec<TransitionObservation> {
    t.transitions
        .observations()
        .into_iter()
        .filter(|o| o.trigger == trigger)
        .collect()
}

/// Row 2 (alert: amendment cap reached) and the depth signal. A committed amendment reports the
/// order's amendment count after it; an order whose count is forced to the cap is refused
/// `amendment-cap-exhausted`, observed as a refused amendment under that reason. The self-service
/// submit before it reports its consent (`self_service`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn alert_s2_amendment_cap_refusal_and_depth_are_observed() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "sig-cap").await;
    let order = v.order.order_id;
    let submits = of(&f.t, Trigger::Submit);
    assert_eq!(submits.len(), 1, "{submits:?}");
    assert_eq!(submits[0].decision, Decision::Committed);
    assert_eq!(
        submits[0].consent.map(|c| c.path),
        Some(SalesPath::SelfService),
        "{submits:?}"
    );
    // The submit's consent is measured against its version's earliest acceptance due date.
    let (due, margin) = due_and_margin(&f.t, v.order.order_id).await;
    assert_eq!(submits[0].consent.and_then(|c| c.due), Some(due));
    assert_eq!(submits[0].consent_margin_days, Some(margin));

    let line = v.lines[0].line_id;
    let settled = amend(&f, order, n, change_quantity(line, "2"), "sig-cap-1")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let m = settled.response.body()["version"].as_i64().unwrap();
    let committed = TransitionObservation {
        amendment_count: Some(1),
        ..TransitionObservation::new(Trigger::Amendment, Decision::Committed)
    };
    assert_eq!(of(&f.t, Trigger::Amendment), vec![committed.clone()]);

    force(&f, order, "amendment_count = 20").await;
    let capped = amend(&f, order, m, change_quantity(line, "3"), "sig-cap-2")
        .await
        .unwrap();
    assert_eq!(
        capped.outcome,
        SettledOutcome::Refused(Some(Reason::AmendmentCapExhausted))
    );
    assert_eq!(
        of(&f.t, Trigger::Amendment),
        vec![
            committed,
            TransitionObservation::new(
                Trigger::Amendment,
                Decision::Refused(Reason::AmendmentCapExhausted)
            )
        ]
    );
    // A same-key replay is not a decision: nothing more is observed.
    amend(&f, order, m, change_quantity(line, "3"), "sig-cap-2")
        .await
        .unwrap();
    assert_eq!(of(&f.t, Trigger::Amendment).len(), 2);
}

/// Row 3 (alert: stale Workflow results). A Workflow verdict naming N after the amendment
/// committed M is refused `version-conflict`, observed as a stale Workflow result; the earlier
/// verdict naming the current version is not one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn alert_s3_stale_workflow_result_is_observed() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, n) = submitted(&f, 1, "sig-wf").await;
    let order = v.order.order_id;
    let required = reflect(
        &f,
        order,
        7,
        Trigger::ReflectApprovalRequired,
        n,
        "sig-wf-r",
    )
    .await;
    assert!(matches!(required.kind, OutcomeKind::Committed { .. }));
    let settled = amend(
        &f,
        order,
        n,
        change_quantity(v.lines[0].line_id, "2"),
        "sig-wf-1",
    )
    .await
    .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    reflect(&f, order, 9, Trigger::ReflectApprovalGranted, n, "sig-wf-g").await;

    let current = of(&f.t, Trigger::ReflectApprovalRequired);
    assert_eq!(current.len(), 1);
    assert!(!current[0].stale_workflow_result());
    let stale = of(&f.t, Trigger::ReflectApprovalGranted);
    assert_eq!(
        stale,
        vec![TransitionObservation::new(
            Trigger::ReflectApprovalGranted,
            Decision::Refused(Reason::VersionConflict)
        )]
    );
    assert!(stale[0].stale_workflow_result());
}

/// Rows 4 and 5 (alerts: party-barred rate, requirement unevaluable). On a partner-placed order
/// the partner is barred; with the Contracts port down the customer's recording is
/// `acceptance-requirement-unevaluable`; once the port answers the customer's consent is observed
/// with its source (`contract`) and path (`partner_placed`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn alert_s4_s5_party_bar_and_requirement_outage_are_observed() {
    let w = world(contract_fixture(Some(true))).await;
    let (order, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), "sig-rec")
        .await;
    let down = w.recorder_with(Arc::clone(&w.engine), Arc::new(ContractsUnavailable));
    let barred = record(&down, &partner(), order, n, "sig-rec-1").await;
    assert_eq!(refusal(&barred), Reason::AcceptanceRecordingPartyBarred);
    let outage = record(&down, &customer(), order, n, "sig-rec-2").await;
    assert_eq!(refusal(&outage), Reason::AcceptanceRequirementUnevaluable);
    let settled = record(&w.recorder(), &customer(), order, n, "sig-rec-3")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let (due, margin) = due_and_margin(&w.f.t, order).await;
    // A same-key replay goes through `Engine::transition`'s `Replayed` branch and is not a
    // decision: the observation count does not change.
    let before = of(&w.f.t, Trigger::RecordAcceptance).len();
    let replay = record(&w.recorder(), &customer(), order, n, "sig-rec-3")
        .await
        .unwrap();
    assert_eq!(replay.outcome, SettledOutcome::Committed, "{replay:?}");
    assert_eq!(of(&w.f.t, Trigger::RecordAcceptance).len(), before);

    assert_eq!(
        of(&w.f.t, Trigger::RecordAcceptance),
        vec![
            TransitionObservation::new(
                Trigger::RecordAcceptance,
                Decision::Refused(Reason::AcceptanceRecordingPartyBarred)
            ),
            TransitionObservation::new(
                Trigger::RecordAcceptance,
                Decision::Refused(Reason::AcceptanceRequirementUnevaluable)
            ),
            TransitionObservation {
                consent: Some(ConsentObservation {
                    source: RequirementSource::Contract,
                    path: SalesPath::PartnerPlaced,
                    due: Some(due),
                }),
                consent_margin_days: Some(margin),
                ..TransitionObservation::new(Trigger::RecordAcceptance, Decision::Committed)
            },
        ]
    );
}

/// Row 6 (alert: tolerated-failure admission) and the authorization observations. With the
/// seller tolerating a failed authorization, `pending` is refused and observed as `pending`; a
/// conclusive `failed` is admitted and observed as `failed` with the tolerated flag, the risk an
/// operator must remediate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn alert_s6_tolerated_failure_admission_is_observed() {
    let w = world(contract_fixture(Some(true))).await;
    tolerate(&w, Some(u(SELLER)), true).await;
    let (order, n) = approved(&w, "sig-bf", true).await;
    let r = resolver(&w, Some(true), true);
    let pending = begin(&w, &r, order, n, Authorization::Pending, "sig-bf-1")
        .await
        .unwrap();
    assert!(
        matches!(
            pending.kind,
            OutcomeKind::Refused {
                reason: Reason::AuthorizationPending,
                ..
            }
        ),
        "{pending:?}"
    );
    let admitted = begin(&w, &r, order, n, Authorization::Failed, "sig-bf-2")
        .await
        .unwrap();
    assert!(
        matches!(admitted.kind, OutcomeKind::Committed { .. }),
        "{admitted:?}"
    );
    assert_eq!(
        of(&w.f.t, Trigger::BeginFulfillment),
        vec![
            TransitionObservation {
                authorization: Some(Authorization::Pending),
                ..TransitionObservation::new(
                    Trigger::BeginFulfillment,
                    Decision::Refused(Reason::AuthorizationPending)
                )
            },
            TransitionObservation {
                authorization: Some(Authorization::Failed),
                tolerated: true,
                ..TransitionObservation::new(Trigger::BeginFulfillment, Decision::Committed)
            },
        ]
    );
}

/// Diagnostic (no alert): a committed administrative edit reports the audit entries it appended,
/// one per changed field; a repeated value appends none and is not counted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn administrative_edit_audits_are_observed_per_changed_field() {
    let f = setup_with(two_line_fixture(), 10, true).await;
    let (v, version) = submitted(&f, 1, "sig-adm").await;
    let order = v.order.order_id;
    let patch = |reference: &str, notes: &str| HeaderPatch {
        external_reference: Some(Some(reference.to_owned())),
        internal_notes: Some(Some(notes.to_owned())),
        ..HeaderPatch::default()
    };
    for (text, reference) in [("sig-adm-1", "PO-1"), ("sig-adm-2", "PO-2")] {
        let r = f
            .capture
            .patch_order(
                &buyer(),
                order,
                patch(reference, "n"),
                &write(text, version, None),
            )
            .await
            .unwrap();
        assert_eq!(r.outcome, SettledOutcome::Committed, "{r:?}");
    }
    let audits: Vec<usize> = of(&f.t, Trigger::AdministrativeEdit)
        .iter()
        .map(|o| o.admin_audits)
        .collect();
    // The first edit changes both fields, the second only the reference.
    assert_eq!(audits, vec![2, 1]);
}

/// Row 7 (alert: consent recorded after its due date). The consented version's acceptance due
/// date is put in the past by owner SQL (a calendar field the fixture authors in the future);
/// the customer's later consent is observed with a negative margin, the overdue signal.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn alert_s7_consent_after_its_due_date_is_observed() {
    let w = world(contract_fixture(Some(true))).await;
    let (order, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), "sig-due")
        .await;
    w.f.t
        .env
        .pg
        .sql(&format!(
            "ALTER TABLE bss_orders__order_line DISABLE TRIGGER USER; \
             UPDATE bss_orders__order_line SET acceptance_due_date = DATE '2020-01-01' \
             WHERE order_id='{order}'; \
             ALTER TABLE bss_orders__order_line ENABLE TRIGGER USER;"
        ))
        .await
        .unwrap();
    let settled = record(&w.recorder(), &customer(), order, n, "sig-due-1")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let (due, margin) = due_and_margin(&w.f.t, order).await;
    assert_eq!(due, time::macros::date!(2020 - 01 - 01));
    assert!(margin < 0, "{margin}");
    let recorded = of(&w.f.t, Trigger::RecordAcceptance);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].consent.and_then(|c| c.due), Some(due));
    assert_eq!(recorded[0].consent_margin_days, Some(margin));
    assert!(recorded[0].consent_overdue(), "{recorded:?}");
}
