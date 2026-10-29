// Internal capability and scoped adapters are consumed by the engine, draft service, reads and
// workers (S2-04/09/11, S6-01). Business operations remain unavailable until they are delivered.
/// The one consent-requirement resolver (S4-08) and the record-acceptance service (S4-08b).
pub mod acceptance;
/// The amendment service (S4-03, S4-04, D-253): development-delivered like submit (D-231).
pub mod amend;
/// The managed producer binding and its readiness (S2-08).
pub mod broker;
/// Draft authoring (S2-09): the service REST and the local SDK share.
pub mod capture;
/// Date policy channel, startup default check and admission-time date preparation (S2-10).
/// Preview, submit (S3-12) and amendment (S4) consume the preparer and admitted-line builder.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the date preparer and admitted-line builder have no caller until Preview, \
                  submit (S3-12) and amendment (S4) are delivered"
    )
)]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "the unit tests reach only part of the undelivered ports, so whether the lint \
                  fires differs per test target; `expect` cannot be verified there"
    )
)]
pub mod dates;
/// Log-safe decode diagnostics shared by the boundary and the storage mapping.
pub mod diagnostics;
/// The transition engine's transaction adapter (S2-04): the single writer of the aggregate,
/// versions, audit, registry settlement and transition events.
pub mod engine;
/// Typed events, their GTS contract and the transition-transaction enqueue (S2-08). The S2-04
/// engine is the production caller; no business operation emits yet.
pub mod error_mapping;
pub mod events;
pub mod execution;
/// The begin-fulfillment consent and payment guard (S4-10, D-256): row 11's preparation and the
/// Stage 5 seam's driver. The operation is an undelivered Workflow seam until Stage 5, so only
/// the engine tests call the driver.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "begin_fulfillment is an undelivered Workflow seam (Stage 5); only the engine \
                  tests call its driver until then (D-256)"
    )
)]
#[cfg_attr(
    all(test, not(feature = "gate-dev-stubs")),
    expect(
        dead_code,
        reason = "the PostgreSQL tests that call the driver need `gate-dev-stubs`"
    )
)]
pub mod fulfillment;
/// Bounded gate port calls: per-port deadline, single retry, bulkheads and run caps (S3-09).
/// Preview (S3-11) and submit (S3-12) are the callers.
pub mod gate;
/// Full jitter for retry pauses (the producer binding and the gate retry).
pub mod jitter;
pub mod maintenance;
/// Policy-election channel: startup promotion and the election read port (S4-07). Consent
/// (S4-08) and the begin-fulfillment money guard (S4-10) read the elections.
pub mod policy;
/// Authorized reads with access logging (early S6-01/S6-04).
pub mod read;
pub mod storage;
/// The five Orders-owned workers: scheduling, locking, class connections, retention purge,
/// idempotency cleanup with the D-188/D-198 continuation port, and audit verification/
/// checkpointing (S2-11). Expiry/auto-void bodies are Stage 5 ports.
pub mod throttle;
pub mod workers;
