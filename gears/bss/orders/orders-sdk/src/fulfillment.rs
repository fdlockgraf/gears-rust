//! The begin-fulfillment payment outcome (S4-10; 05 §2.3 *Attempt fulfillment with a payment
//! outcome*, §3.3; Stage 4 DESIGN §6; D-256).
//!
//! `models.json` `AuthorizationOutcome`: a transient Workflow fact handed to begin-fulfillment,
//! never persisted as an order-owned payment status. The operation is not yet on any route (the
//! Workflow seam is Stage 5); the type exists so the guard's input has one closed shape. An
//! unknown outcome, a missing one or any other member (an instrument, a token) is refused at the
//! wire and never reaches the tolerance decision.
use serde::{Deserialize, Serialize};

/// What the payment authorization concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authorization {
    /// The payment was authorized: begin-fulfillment admits.
    Authorized,
    /// Defensive: Workflow keeps a pending authorization itself (05 §3.4); Orders refuses
    /// `authorization-pending` without a state change.
    Pending,
    /// A conclusive failure: admitted only when the seller (then the platform) elected to
    /// tolerate it, otherwise `authorization-failed`.
    Failed,
}

/// The begin-fulfillment request body: exactly `{ "outcome": … }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationOutcome {
    /// The authorization's conclusion.
    pub outcome: Authorization,
}
