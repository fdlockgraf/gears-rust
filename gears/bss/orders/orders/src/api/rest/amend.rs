//! The amendment route (S4-03): `POST /bss-orders/v1/orders/{orderId}/amendments`.
//!
//! Development delivery only (D-231), exactly as [`super::gate::submit_router`]: the gear mounts
//! [`amend_router`] only with the `gate-dev-stubs` feature **and** `gate.dev_stubs = true`, outside
//! [`super::DELIVERED_OPERATIONS`]. Boundary order is the existing-order writes' (D-142, D-149):
//! the per-(caller, order) edge limiter, the required `If-Match` (428), `Idempotency-Key`,
//! `X-Delegation-Proof-Ref`, then the body ([`amend_body`]: unknown keys, a malformed value or a
//! line id named twice are `request-invalid`), and only then the [`AmendService`]. A boundary
//! refusal therefore writes no audit row, no access-log row and no idempotency record. A bad or
//! missing `reason` is not a boundary failure: it reaches the engine guard
//! `amendment-reason-invalid`. The settled outcome is rendered exactly as stored.
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::{Extension, Router};
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::catalog::Reason;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{OrdersAmendOrder, OrdersTransitionResult};
use super::error::ApiResult;
use super::throttle::{PerOrderLimiter, caller_write_throttling};
use crate::authz::Caller;
use crate::domain::versioning::AmendmentDelta;
use crate::infra::amend::AmendService;
use crate::infra::diagnostics::serde_category;

const TAG: &str = "Orders";

/// `request-invalid` for a body the wire refuses; the debug record names the operation, the
/// refused member where it is static and the category, never the serde message (it quotes the
/// input).
fn refused_body(member: Option<&'static str>, category: Option<&'static str>) -> OrdersError {
    tracing::debug!(
        target: "orders.rest.amend",
        operation = "amend",
        member,
        category,
        "request body refused request-invalid"
    );
    OrdersError::Refused(Reason::RequestInvalid)
}

/// The `amend` body: an object decoded as [`OrdersAmendOrder`] and converted by the SDK's own
/// scalar parsers, then checked well-formed (no line id named twice).
///
/// # Errors
/// `request-invalid` for any body the wire refuses.
#[doc(hidden)]
pub fn amend_body(body: &[u8]) -> Result<AmendOrder, OrdersError> {
    let wire: OrdersAmendOrder = serde_json::from_slice(body)
        .map_err(|error| refused_body(None, Some(serde_category(&error))))?;
    let request = AmendOrder::try_from(wire).map_err(
        |bss_orders_sdk::authoring::AuthoringInvalid(member)| refused_body(Some(member), None),
    )?;
    AmendmentDelta::from_request(&request)
        .check_well_formed()
        .map_err(|_| refused_body(Some("lines"), None))?;
    Ok(request)
}

async fn amend(
    Extension(service): Extension<Arc<AmendService>>,
    Extension(limiter): Extension<Arc<PerOrderLimiter>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    limiter.admit(ctx.subject_id(), order_id)?;
    let call = super::capture::write_meta(&headers)?;
    let request = amend_body(&body)?;
    let caller = Caller::from_meta(&ctx, call.delegation_proof_ref.as_ref())?;
    // A fresh token, never cancelled: the dropped future is the cancellation.
    let settled = service
        .amend(&caller, order_id, request, &call, &CancellationToken::new())
        .await?;
    Ok(super::capture::render(settled))
}

/// Register the amendment route and attach the service and the edge limiter once (`ToolKit` REST
/// rule). Development delivery only, outside [`super::DELIVERED_OPERATIONS`].
pub fn amend_router(
    service: Arc<AmendService>,
    limiter: Arc<PerOrderLimiter>,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    let router = Router::new();
    let router = OperationBuilder::post("/bss-orders/v1/orders/{orderId}/amendments")
        .operation_id("bss_orders.amend")
        .summary("Amend an order")
        .description(
            "Appends an amendment: a delta onto the committed version named by If-Match (order \
             fields and line add/change/remove by stable line id) under an explanation of 1-4096 \
             characters, re-assessed over every line by the shared gate. Unnamed fields and \
             unlisted lines carry forward. Development delivery only. Refusals: 400 \
             REQUEST_INVALID for a malformed body or a line named twice; 400 \
             AMENDMENT_REASON_INVALID, AMENDMENT_EMPTY, AMENDMENT_CAP_EXHAUSTED, \
             ADMINISTRATIVE_FIELD_IN_AMENDMENT and the registered gate refusals; 403/404 per the \
             shared authorization contract; 409 VERSION_CONFLICT, IDEMPOTENCY_MISMATCH, \
             STILL_PROCESSING; 412 for a stale If-Match; 428 without If-Match; 429 when a \
             per-caller or per-order limit is exhausted; 503 when authorization, the store, the \
             gate run or Pricing is unavailable.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .with_throttling(caller_write_throttling())
        .json_request::<OrdersAmendOrder>(openapi, "Amendment")
        .param(super::capture::if_match())
        .param(super::capture::key_param())
        .param(super::delegation_proof_param())
        .handler(amend)
        .json_response_with_schema::<OrdersTransitionResult>(openapi, StatusCode::OK, "Amended")
        .response_header(super::capture::etag())
        .standard_errors(openapi)
        .problem_response(
            openapi,
            StatusCode::PRECONDITION_FAILED,
            super::capture::STALE_IF_MATCH,
        )
        .error_429(openapi)
        .error_503(openapi)
        .register(router, openapi);
    router.layer(Extension(service)).layer(Extension(limiter))
}
