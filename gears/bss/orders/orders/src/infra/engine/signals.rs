//! What the engine reports when an attempt is decided (S4-12, D-257; D-223 naming; the D-237
//! `GateSignals` and the read path's `ReadSignals` patterns). The engine is the decision point of
//! every Stage 4 operation: amendment, administrative edit, consent recording, begin-fulfillment
//! and the Workflow reflections all settle through [`Engine::transition`](super::Engine::transition),
//! so one observation per decided attempt carries the trigger, the outcome and the few facts the
//! preparation and the commit know (the amendment count after an amendment, the administrative
//! audit entries, the authorization outcome Workflow presented, a tolerated admission and a
//! recorded consent's source and path).
//!
//! Only decided attempts are observed: a committed transition or a business refusal, settled or
//! unsettled. A same-key replay, an outage and an integration failure are not decisions and emit
//! nothing. Labels are bounded tokens (trigger, outcome, reason, requirement source, recording
//! path, authorization outcome); no instrument carries an order, tenant or subject attribute.
use bss_orders_sdk::authoring::SalesPath;
use bss_orders_sdk::catalog::{Reason, Trigger};
use bss_orders_sdk::fulfillment::Authorization;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use crate::domain::preconditions::RequirementSource;
use crate::domain::state_table::is_workflow_class;

/// How a decided attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The transition committed.
    Committed,
    /// A business refusal (settled or unsettled) under its reason.
    Refused(Reason),
}

impl Decision {
    /// The `outcome` label value.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Refused(_) => "refused",
        }
    }
}

/// A consent the committed transition recorded (submit's self-service consent, D-251, or a
/// recording after submit, D-255).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsentObservation {
    /// Why consent was recorded (`requirement_source`).
    pub source: RequirementSource,
    /// Who recorded it (`recording_path`).
    pub path: SalesPath,
    /// The consented version's earliest line acceptance due date (a calendar field on every
    /// admitted line, D-60); `None` only when no line states one.
    pub due: Option<time::Date>,
}

/// One decided attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionObservation {
    /// The attempted trigger.
    pub trigger: Trigger,
    /// Committed or refused.
    pub decision: Decision,
    /// A committed amendment: the order's amendment count after it (its depth).
    pub amendment_count: Option<i32>,
    /// A committed administrative edit: the audit entries appended (one per changed field).
    pub admin_audits: usize,
    /// Begin-fulfillment: the authorization outcome Workflow presented, whatever the decision.
    pub authorization: Option<Authorization>,
    /// A committed begin-fulfillment that admitted a tolerated failed authorization.
    pub tolerated: bool,
    /// A committed transition that recorded a consent.
    pub consent: Option<ConsentObservation>,
    /// That consent's margin against its due date in whole days: the due date minus the UTC date
    /// of the engine's transition time (negative: recorded after the due date).
    pub consent_margin_days: Option<i64>,
}

impl TransitionObservation {
    /// An observation of `trigger` decided as `decision`, with no further facts.
    #[must_use]
    pub const fn new(trigger: Trigger, decision: Decision) -> Self {
        Self {
            trigger,
            decision,
            amendment_count: None,
            admin_audits: 0,
            authorization: None,
            tolerated: false,
            consent: None,
            consent_margin_days: None,
        }
    }

    /// A consent recorded after its version's acceptance due date.
    #[must_use]
    pub fn consent_overdue(&self) -> bool {
        self.consent_margin_days.is_some_and(|margin| margin < 0)
    }

    /// A Workflow result naming a superseded version: a Workflow-class trigger refused
    /// `version-conflict` (D-110; Stage 4 DESIGN §3).
    #[must_use]
    pub fn stale_workflow_result(&self) -> bool {
        self.decision == Decision::Refused(Reason::VersionConflict)
            && is_workflow_class(self.trigger)
    }
}

/// The `outcome` label value of an authorization observation (the wire spelling).
#[must_use]
pub const fn authorization_token(outcome: Authorization) -> &'static str {
    match outcome {
        Authorization::Authorized => "authorized",
        Authorization::Pending => "pending",
        Authorization::Failed => "failed",
    }
}

/// The engine's decision sink.
pub trait TransitionSignals: Send + Sync + std::fmt::Debug {
    /// One attempt was decided (module doc).
    fn settled(&self, observation: &TransitionObservation);
}

/// The sink that reports nothing.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTransitionSignals;

#[cfg(test)]
impl TransitionSignals for NoTransitionSignals {
    fn settled(&self, _observation: &TransitionObservation) {}
}

/// OpenTelemetry instruments on the process meter.
pub struct OtelTransitionSignals {
    transitions: Counter<u64>,
    refusals: Counter<u64>,
    stale: Counter<u64>,
    depth: Histogram<u64>,
    admin_audits: Counter<u64>,
    authorizations: Counter<u64>,
    tolerated: Counter<u64>,
    consents: Counter<u64>,
    overdue: Counter<u64>,
    overdue_days: Histogram<u64>,
}

impl std::fmt::Debug for OtelTransitionSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelTransitionSignals")
            .finish_non_exhaustive()
    }
}

impl Default for OtelTransitionSignals {
    fn default() -> Self {
        Self::new()
    }
}

impl OtelTransitionSignals {
    /// Build the instruments on the `bss-orders` process meter.
    #[must_use]
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("bss-orders");
        Self {
            transitions: meter
                .u64_counter("orders_transitions_total")
                .with_description("Decided transition attempts by trigger and outcome")
                .build(),
            refusals: meter
                .u64_counter("orders_transition_refusals_total")
                .with_description("Refused transition attempts by trigger and reason")
                .build(),
            stale: meter
                .u64_counter("orders_workflow_stale_results_total")
                .with_description(
                    "Workflow results naming a superseded version (version-conflict) by trigger",
                )
                .build(),
            depth: meter
                .u64_histogram("orders_amendment_depth")
                .with_description("The order's amendment count after a committed amendment")
                .build(),
            admin_audits: meter
                .u64_counter("orders_admin_edit_audits_total")
                .with_description("Audit entries appended by committed administrative edits")
                .build(),
            authorizations: meter
                .u64_counter("orders_fulfillment_authorizations_total")
                .with_description(
                    "Authorization outcomes presented to begin-fulfillment by outcome",
                )
                .build(),
            tolerated: meter
                .u64_counter("orders_fulfillment_tolerated_admissions_total")
                .with_description(
                    "Begin-fulfillment admissions of a tolerated failed authorization",
                )
                .build(),
            consents: meter
                .u64_counter("orders_consent_recorded_total")
                .with_description("Recorded consents by requirement_source and recording_path")
                .build(),
            overdue: meter
                .u64_counter("orders_consent_overdue_total")
                .with_description(
                    "Consents recorded after their version's acceptance due date by recording_path",
                )
                .build(),
            overdue_days: meter
                .u64_histogram("orders_consent_overdue_days")
                .with_description("Whole days a consent was recorded after its due date")
                .build(),
        }
    }
}

impl TransitionSignals for OtelTransitionSignals {
    fn settled(&self, o: &TransitionObservation) {
        let trigger = KeyValue::new("trigger", o.trigger.token());
        self.transitions.add(
            1,
            &[
                trigger.clone(),
                KeyValue::new("outcome", o.decision.token()),
            ],
        );
        if let Decision::Refused(reason) = o.decision {
            self.refusals.add(
                1,
                &[trigger.clone(), KeyValue::new("reason", reason.token())],
            );
        }
        if o.stale_workflow_result() {
            self.stale.add(1, &[trigger]);
        }
        if let Some(outcome) = o.authorization {
            self.authorizations
                .add(1, &[KeyValue::new("outcome", authorization_token(outcome))]);
        }
        if o.decision != Decision::Committed {
            return;
        }
        if let Some(count) = o.amendment_count {
            self.depth
                .record(u64::try_from(count).unwrap_or_default(), &[]);
        }
        if o.admin_audits > 0 {
            self.admin_audits
                .add(u64::try_from(o.admin_audits).unwrap_or(u64::MAX), &[]);
        }
        if o.tolerated {
            self.tolerated.add(1, &[]);
        }
        if let Some(consent) = o.consent {
            self.consents.add(
                1,
                &[
                    KeyValue::new("requirement_source", consent.source.token()),
                    KeyValue::new("recording_path", consent.path.token()),
                ],
            );
            if let Some(margin) = o.consent_margin_days.filter(|m| *m < 0) {
                self.overdue
                    .add(1, &[KeyValue::new("recording_path", consent.path.token())]);
                self.overdue_days.record(margin.unsigned_abs(), &[]);
            }
        }
    }
}

/// A sink that records every observation (tests only), the `RecordingGateSignals` shape.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingTransitionSignals {
    observations: parking_lot::Mutex<Vec<TransitionObservation>>,
}

#[cfg(test)]
impl RecordingTransitionSignals {
    /// Everything observed so far, in call order.
    #[must_use]
    pub fn observations(&self) -> Vec<TransitionObservation> {
        self.observations.lock().clone()
    }
}

#[cfg(test)]
impl TransitionSignals for RecordingTransitionSignals {
    fn settled(&self, observation: &TransitionObservation) {
        self.observations.lock().push(observation.clone());
    }
}
