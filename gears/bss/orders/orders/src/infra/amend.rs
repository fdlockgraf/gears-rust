//! The amendment service (S4-03, S4-04): the one service REST and the local SDK share.
//!
//! This is the S4-03 seam. The service is a development-delivery stub ([`AmendService::stub`]): it
//! answers `NotDelivered` (unavailable) before any effect, so the amendment route and the SDK
//! method exist end to end while the engine path is undelivered. S4-04 replaces the body of
//! [`AmendService::amend`] (and the `stub` constructor) with the real pipeline: authorize, recover,
//! snapshot N, merge, guards, shared assessment, D-188 attempt, engine handoff.
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::models::CallMeta;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::capture::Settled;
use super::engine::errors::not_delivered;
use crate::authz::Caller;

/// The amendment service. Holds nothing yet: a stub.
#[derive(Debug)]
pub struct AmendService {
    _stub: (),
}

impl AmendService {
    /// The undelivered stub (a double): every call answers `NotDelivered`.
    #[must_use]
    pub fn stub() -> Self {
        Self { _stub: () }
    }

    /// Amend `order_id` from committed version `meta.expected_version` by `request`.
    ///
    /// The boundary has already refused malformed bodies, a missing `If-Match` and a delta naming
    /// one line twice; everything here runs under the caller's authority.
    ///
    /// # Errors
    /// Stub: always `NotDelivered`, which the SDK projects as unavailable.
    #[expect(
        clippy::unused_async,
        reason = "the stub has the signature of the real service the S4-04 pipeline replaces"
    )]
    pub async fn amend(
        &self,
        _caller: &Caller,
        _order_id: Uuid,
        _request: AmendOrder,
        _meta: &CallMeta,
        _cancel: &CancellationToken,
    ) -> Result<Settled, OrdersError> {
        Err(not_delivered("amend (S4-04)").into())
    }
}
