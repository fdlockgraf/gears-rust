//! The transition signal sink (S4-12, D-257): the recording double keeps every observation and
//! the `OTel` sink emits the D-257 instruments with bounded labels only (trigger, outcome, reason,
//! requirement source, recording path, authorization outcome), never an order, tenant or subject.
#![allow(
    clippy::unwrap_used,
    reason = "test code: a failed metrics flush or collection must stop the test at once"
)]
use super::signals::{
    ConsentObservation, Decision, NoTransitionSignals, OtelTransitionSignals,
    RecordingTransitionSignals, TransitionObservation, TransitionSignals,
};
use crate::domain::preconditions::RequirementSource;
use crate::infra::gate::tests::signals::{Point, exported, install_meter, meter_lock, point};
use bss_orders_sdk::authoring::SalesPath;
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::fulfillment::Authorization;
use std::collections::BTreeSet;

/// The D-257 instruments [`OtelTransitionSignals::new`] builds.
const D250_INSTRUMENTS: [&str; 10] = [
    "orders_transitions_total",
    "orders_transition_refusals_total",
    "orders_workflow_stale_results_total",
    "orders_amendment_depth",
    "orders_admin_edit_audits_total",
    "orders_fulfillment_authorizations_total",
    "orders_fulfillment_tolerated_admissions_total",
    "orders_consent_recorded_total",
    "orders_consent_overdue_total",
    "orders_consent_overdue_days",
];

fn d250(points: BTreeSet<Point>) -> BTreeSet<Point> {
    points
        .into_iter()
        .filter(|(name, ..)| D250_INSTRUMENTS.contains(&name.as_str()))
        .collect()
}

fn refused(trigger: Trigger, reason: Reason) -> TransitionObservation {
    TransitionObservation::new(trigger, Decision::Refused(reason))
}

#[test]
fn the_recording_sink_keeps_each_observation_in_order() {
    let sink = RecordingTransitionSignals::default();
    let first = refused(Trigger::Amendment, Reason::AmendmentCapExhausted);
    let second = TransitionObservation {
        amendment_count: Some(3),
        ..TransitionObservation::new(Trigger::Amendment, Decision::Committed)
    };
    sink.settled(&first);
    sink.settled(&second);
    assert_eq!(sink.observations(), vec![first, second]);
    // The no-op sink answers too.
    NoTransitionSignals.settled(&refused(Trigger::Submit, Reason::NoLines));
}

/// Every D-257 instrument, its name and its labels: the generic outcome and refusal counters, the
/// Workflow stale-result counter (only a Workflow-class trigger refused `version-conflict`; a
/// submit's stale draft is not one), the amendment depth, the administrative audit entries, the
/// authorization observations, the tolerated admissions and the consents by source and path.
#[test]
fn the_otel_sink_emits_the_d250_instruments() {
    let guard = meter_lock().blocking_lock();
    let (provider, exporter) = install_meter(&guard);
    let sink = OtelTransitionSignals::new();

    sink.settled(&refused(Trigger::Amendment, Reason::AmendmentCapExhausted));
    sink.settled(&refused(
        Trigger::ReflectApprovalGranted,
        Reason::VersionConflict,
    ));
    sink.settled(&refused(Trigger::Submit, Reason::VersionConflict));
    sink.settled(&TransitionObservation {
        amendment_count: Some(2),
        ..TransitionObservation::new(Trigger::Amendment, Decision::Committed)
    });
    sink.settled(&TransitionObservation {
        admin_audits: 3,
        ..TransitionObservation::new(Trigger::AdministrativeEdit, Decision::Committed)
    });
    sink.settled(&TransitionObservation {
        authorization: Some(Authorization::Failed),
        tolerated: true,
        ..TransitionObservation::new(Trigger::BeginFulfillment, Decision::Committed)
    });
    sink.settled(&TransitionObservation {
        authorization: Some(Authorization::Pending),
        ..refused(Trigger::BeginFulfillment, Reason::AuthorizationPending)
    });
    // Recorded three days after the version's earliest acceptance due date.
    sink.settled(&TransitionObservation {
        consent: Some(ConsentObservation {
            source: RequirementSource::Seller,
            path: SalesPath::PartnerPlaced,
            due: Some(time::macros::date!(2026 - 10 - 01)),
        }),
        consent_margin_days: Some(-3),
        ..TransitionObservation::new(Trigger::RecordAcceptance, Decision::Committed)
    });
    // Recorded before its due date: counted as recorded, not overdue.
    sink.settled(&TransitionObservation {
        consent: Some(ConsentObservation {
            source: RequirementSource::Seller,
            path: SalesPath::PartnerPlaced,
            due: Some(time::macros::date!(2026 - 12 - 01)),
        }),
        consent_margin_days: Some(30),
        ..TransitionObservation::new(Trigger::RecordAcceptance, Decision::Committed)
    });

    let expected: BTreeSet<Point> = [
        point(
            "orders_transitions_total",
            &[("trigger", "amendment"), ("outcome", "refused")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "amendment"), ("outcome", "committed")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[
                ("trigger", "reflect-approval-granted"),
                ("outcome", "refused"),
            ],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "submit"), ("outcome", "refused")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "administrative-edit"), ("outcome", "committed")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "begin-fulfillment"), ("outcome", "committed")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "begin-fulfillment"), ("outcome", "refused")],
            1,
        ),
        point(
            "orders_transitions_total",
            &[("trigger", "record-acceptance"), ("outcome", "committed")],
            2,
        ),
        point(
            "orders_transition_refusals_total",
            &[
                ("trigger", "amendment"),
                ("reason", "amendment-cap-exhausted"),
            ],
            1,
        ),
        point(
            "orders_transition_refusals_total",
            &[
                ("trigger", "reflect-approval-granted"),
                ("reason", "version-conflict"),
            ],
            1,
        ),
        point(
            "orders_transition_refusals_total",
            &[("trigger", "submit"), ("reason", "version-conflict")],
            1,
        ),
        point(
            "orders_transition_refusals_total",
            &[
                ("trigger", "begin-fulfillment"),
                ("reason", "authorization-pending"),
            ],
            1,
        ),
        point(
            "orders_workflow_stale_results_total",
            &[("trigger", "reflect-approval-granted")],
            1,
        ),
        point("orders_amendment_depth", &[], 1),
        point("orders_admin_edit_audits_total", &[], 3),
        point(
            "orders_fulfillment_authorizations_total",
            &[("outcome", "failed")],
            1,
        ),
        point(
            "orders_fulfillment_authorizations_total",
            &[("outcome", "pending")],
            1,
        ),
        point("orders_fulfillment_tolerated_admissions_total", &[], 1),
        point(
            "orders_consent_recorded_total",
            &[
                ("requirement_source", "seller"),
                ("recording_path", "partner_placed"),
            ],
            2,
        ),
        point(
            "orders_consent_overdue_total",
            &[("recording_path", "partner_placed")],
            1,
        ),
        point("orders_consent_overdue_days", &[], 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(d250(exported(&provider, &exporter)), expected);
}

#[test]
fn the_authorization_tokens_are_the_wire_values() {
    use super::signals::authorization_token;
    assert_eq!(
        [
            Authorization::Authorized,
            Authorization::Pending,
            Authorization::Failed
        ]
        .map(authorization_token),
        ["authorized", "pending", "failed"]
    );
}

/// An idempotency answer (mismatch, still processing) is not a decision; a business refusal is.
#[test]
fn idempotency_answers_are_not_decisions() {
    use super::{EngineError, decided};
    for reason in [Reason::IdempotencyMismatch, Reason::StillProcessing] {
        assert_eq!(decided(&Err(EngineError::Refused(reason))), None);
    }
    assert_eq!(
        decided(&Err(EngineError::Refused(Reason::VersionConflict))),
        Some(Decision::Refused(Reason::VersionConflict))
    );
    assert_eq!(decided(&Err(EngineError::Integration)), None);
}
