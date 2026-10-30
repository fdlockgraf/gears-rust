//! The record-acceptance route (S4-08b; 05 §2.2; D-255):
//! `POST /bss-orders/v1/orders/{orderId}/acceptance`.
//!
//! Development delivery only (D-235), exactly as [`super::amend::amend_router`]: the gear mounts
//! [`acceptance_router`] only with the `gate-dev-stubs` feature **and** `gate.dev_stubs = true`,
//! outside [`super::DELIVERED_OPERATIONS`]. Boundary order is the existing-order writes' (D-142,
//! D-149): the per-(caller, order) edge limiter, the required `If-Match` (428),
//! `Idempotency-Key`, `X-Delegation-Proof-Ref`, then the body, which must be exactly `{}`
//! ([`acceptance_body`]): the instant and the recording actor are server facts, so a
//! caller-supplied timestamp or actor is `request-invalid`. A boundary refusal writes no audit
//! row, no access-log row and no idempotency record. The settled outcome is rendered exactly as
//! stored.
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Extension, Router};
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::acceptance::Acceptance;
use bss_orders_sdk::catalog::Reason;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{OrdersAcceptance, OrdersTransitionResult};
use super::error::ApiResult;
use super::throttle::{PerOrderLimiter, caller_write_throttling};
use crate::authz::Caller;
use crate::infra::acceptance::AcceptanceService;
use crate::infra::diagnostics::serde_category;

const TAG: &str = "Orders";

/// The `record_acceptance` body: exactly an empty object, decoded by the SDK's [`Acceptance`]
/// (any member, an array or `null` is refused). The debug record names the operation and the
/// serde category, never the serde message (it quotes the input).
///
/// # Errors
/// `request-invalid` for any other body.
#[doc(hidden)]
pub fn acceptance_body(body: &[u8]) -> Result<Acceptance, OrdersError> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::debug!(
            target: "orders.rest.acceptance",
            operation = "record_acceptance",
            category = serde_category(&error),
            "request body refused request-invalid"
        );
        OrdersError::Refused(Reason::RequestInvalid)
    })
}

async fn record_acceptance(
    Extension(service): Extension<Arc<AcceptanceService>>,
    Extension(limiter): Extension<Arc<PerOrderLimiter>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    limiter.admit(ctx.subject_id(), order_id)?;
    let call = super::capture::write_meta(&headers)?;
    let request = acceptance_body(&body)?;
    let caller = Caller::from_meta(&ctx, call.delegation_proof_ref.as_ref())?;
    // A fresh token, never cancelled: the dropped future is the cancellation.
    let settled = service
        .record(&caller, order_id, request, &call, &CancellationToken::new())
        .await?;
    Ok(super::capture::render(settled))
}

/// Register the record-acceptance route and attach the service and the edge limiter once
/// (`ToolKit` REST rule). Development delivery only, outside [`super::DELIVERED_OPERATIONS`].
pub fn acceptance_router(
    service: Arc<AcceptanceService>,
    limiter: Arc<PerOrderLimiter>,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    let router = Router::new();
    let router = OperationBuilder::post("/bss-orders/v1/orders/{orderId}/acceptance")
        .operation_id("bss_orders.record_acceptance")
        .summary("Record acceptance of the current version")
        .description(
            "Records the customer's consent to the committed version named by If-Match, after \
             submit. The body is an empty object: the instant is the server's transition time \
             and the recording actor the authenticated caller. No state or version changes. \
             Development delivery only. Refusals: 400 REQUEST_INVALID for any body member; 403 \
             ACCEPTANCE_RECORDING_PARTY_BARRED when the caller created, submitted or amended the \
             order on the partner path or from another tenant; 403/404 per the shared \
             authorization contract; 409 ACCEPTANCE_ALREADY_RECORDED, NOT_ADMISSIBLE, \
             IDEMPOTENCY_MISMATCH, STILL_PROCESSING; 412 for a stale If-Match; 428 without \
             If-Match; 429 when a per-caller or per-order limit is exhausted; 503 \
             ACCEPTANCE_REQUIREMENT_UNEVALUABLE when the contract or policy cannot be read, or \
             when authorization or the store is unavailable.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .with_throttling(caller_write_throttling())
        .json_request::<OrdersAcceptance>(openapi, "Acceptance (an empty object)")
        .param(super::capture::if_match())
        .param(super::capture::key_param())
        .param(super::delegation_proof_param())
        .handler(record_acceptance)
        .json_response_with_schema::<OrdersTransitionResult>(openapi, StatusCode::OK, "Recorded")
        .response_header(super::capture::etag())
        .standard_errors(openapi)
        .problem_response(
            openapi,
            StatusCode::PRECONDITION_FAILED,
            super::capture::STALE_IF_MATCH,
        )
        .problem_response(
            openapi,
            StatusCode::PRECONDITION_REQUIRED,
            super::capture::MISSING_IF_MATCH,
        )
        .error_429(openapi)
        .error_503(openapi)
        .register(router, openapi);
    router.layer(Extension(service)).layer(Extension(limiter))
}
