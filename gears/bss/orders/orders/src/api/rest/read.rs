//! The read routes (08 §3.3; S1-02 catalog): the early S6-01 `get`, `list`, `list_lines`
//! (`GET /orders/{orderId}`, `GET /orders`, `GET /orders/{orderId}/lines`) and the S4-06 history
//! reads `list_versions`, `get_version`, `get_acceptance` (`GET /orders/{orderId}/versions`,
//! `GET /orders/{orderId}/versions/{version}`, `GET /orders/{orderId}/acceptance`; D-254).
//!
//! The query is the shared list format (DNA REST `PAGINATION.md`, D-212): `limit`, `cursor` and,
//! on the order list, an `OData` `$filter`; a page answers `items` and `page_info`.
//!
//! Boundary order: `X-Delegation-Proof-Ref` (`request-invalid`, D-202) → `limit`
//! (`page-size-exceeded`) → `$filter` and unknown query names (`filter-invalid`) → `cursor`
//! (`cursor-invalid`, D-139). All of these precede the access decision and append no
//! access-log row (08 §3.6 common read wrapper item 1). Each handler then enters the shared
//! [`ReadService`], which the local SDK uses too. Successful per-order reads carry the current
//! commercial version as the strong `ETag` (the value `If-Match` sends back on a write); the
//! bodies are the [`super::dto`] response types converted from the SDK read results, so the
//! served bytes and the `OpenAPI` schema have one source (`snake_case`, D-206). Refusals are
//! RFC 9457 problems from the Orders registry.
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Router};
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::catalog::{OrderState, Reason};
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::{
    AcceptanceList, Cursor, LineList, ListOrders, OrderFilters, PageSize, VersionList,
};
use serde::Serialize;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};
use toolkit::api::rest::extract::Path;
use toolkit_odata::filter::{FilterNode, FilterOp, ODataValue, parse_odata_filter};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    OrdersAcceptancePage, OrdersLinePage, OrdersOrderPage, OrdersOrderVersionView, OrdersOrderView,
    OrdersVersionPage, ResponseUnencodable,
};
use super::error::ApiResult;
use crate::authz::Caller;
use crate::infra::read::{self, ReadService};

const TAG: &str = "Orders";
const LIMIT: &str = "limit";
const CURSOR: &str = "cursor";
const FILTER: &str = "$filter";

/// The order list's `$filter` fields (D-212). The supported terms are exactly the existing design
/// filters: `state eq`, `created_at ge` (inclusive) and `created_at lt` (exclusive),
/// `state_entered_at le` (in the current state since that instant or earlier) and
/// `contract_id eq`, each at most once and joined only by `and`.
#[derive(toolkit_odata_macros::ODataFilterable)]
#[expect(
    dead_code,
    reason = "never constructed: the ODataFilterable derive reads only the fields' filter kinds"
)]
struct OrderList {
    #[odata(filter(kind = "String"))]
    state: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    created_at: String,
    #[odata(filter(kind = "DateTimeUtc"))]
    state_entered_at: String,
    #[odata(filter(kind = "Uuid"))]
    contract_id: Uuid,
}

fn refused(reason: Reason) -> OrdersError {
    OrdersError::Refused(reason)
}

/// The decoded query, with repeated names remembered so each step refuses its own.
struct Query {
    values: BTreeMap<String, String>,
    repeated: Vec<String>,
}

/// Decode the query string; a string that is not `application/x-www-form-urlencoded` is a
/// filter failure (no name can be examined).
fn query(uri: &Uri) -> Result<Query, OrdersError> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(uri.query().unwrap_or(""))
        .map_err(|_| refused(Reason::FilterInvalid))?;
    let mut values = BTreeMap::new();
    let mut repeated = Vec::new();
    for (name, value) in pairs {
        if values.insert(name.clone(), value).is_some() {
            repeated.push(name);
        }
    }
    Ok(Query { values, repeated })
}

/// Step 1: `limit` is an unsigned decimal integer in 1–200; anything else, including a repeated
/// name, is `page-size-exceeded` (the S1-02 oracle's rule for a non-integer value).
fn limit(query: &Query) -> Result<Option<PageSize>, OrdersError> {
    if query.repeated.iter().any(|name| name == LIMIT) {
        return Err(refused(Reason::PageSizeExceeded));
    }
    let Some(text) = query.values.get(LIMIT) else {
        return Ok(None);
    };
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refused(Reason::PageSizeExceeded));
    }
    let value = text
        .parse::<u64>()
        .map_err(|_| refused(Reason::PageSizeExceeded))?;
    read::page_size(Some(value))
}

/// The `and`-joined terms of a filter; any other connective (`or`, `not`) is unsupported.
fn conjuncts(
    node: FilterNode<OrderListFilterField>,
    out: &mut Vec<FilterNode<OrderListFilterField>>,
) -> Result<(), OrdersError> {
    match node {
        FilterNode::Composite {
            op: FilterOp::And,
            children,
        } => children
            .into_iter()
            .try_for_each(|child| conjuncts(child, out)),
        FilterNode::Binary { .. } => {
            out.push(node);
            Ok(())
        }
        _ => Err(refused(Reason::FilterInvalid)),
    }
}

/// An `OData` `DateTimeOffset` literal as the exact instant (nanoseconds are kept).
fn instant(value: &ODataValue) -> Result<time::OffsetDateTime, OrdersError> {
    let ODataValue::DateTime(at) = value else {
        return Err(refused(Reason::FilterInvalid));
    };
    let nanos =
        i128::from(at.timestamp()) * 1_000_000_000 + i128::from(at.timestamp_subsec_nanos());
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|_| refused(Reason::FilterInvalid))
}

/// Set a filter once; a second term for the same filter is `filter-invalid`.
fn once<T>(slot: &mut Option<T>, value: T) -> Result<(), OrdersError> {
    if slot.replace(value).is_some() {
        return Err(refused(Reason::FilterInvalid));
    }
    Ok(())
}

/// Step 2 (order list): every query name is `limit`, `cursor` or `$filter`, each at most once;
/// the filter parses with the platform `OData` parser and lowers term by term to the design
/// filters. An unsupported field, operator, connective or value is `filter-invalid`.
fn order_filters(query: &Query) -> Result<OrderFilters, OrdersError> {
    let invalid = || refused(Reason::FilterInvalid);
    if query
        .values
        .keys()
        .any(|name| name != LIMIT && name != CURSOR && name != FILTER)
        || query.repeated.iter().any(|name| name == FILTER)
    {
        return Err(invalid());
    }
    let mut filters = OrderFilters::default();
    let Some(raw) = query.values.get(FILTER) else {
        return Ok(filters);
    };
    if raw.len() > toolkit::api::odata::MAX_FILTER_LEN {
        return Err(invalid());
    }
    let node = parse_odata_filter::<OrderListFilterField>(raw).map_err(|_| invalid())?;
    let mut terms = Vec::new();
    conjuncts(node, &mut terms)?;
    for term in terms {
        let FilterNode::Binary { field, op, value } = term else {
            return Err(invalid());
        };
        match (field, op, &value) {
            (OrderListFilterField::State, FilterOp::Eq, ODataValue::String(token)) => {
                let state =
                    serde_json::from_value::<OrderState>(serde_json::Value::String(token.clone()))
                        .map_err(|_| invalid())?;
                once(&mut filters.state, state)?;
            }
            (OrderListFilterField::CreatedAt, FilterOp::Ge, _) => {
                once(&mut filters.created_from, instant(&value)?)?;
            }
            (OrderListFilterField::CreatedAt, FilterOp::Lt, _) => {
                once(&mut filters.created_to, instant(&value)?)?;
            }
            (OrderListFilterField::StateEnteredAt, FilterOp::Le, _) => {
                once(&mut filters.state_entered_before, instant(&value)?)?;
            }
            (OrderListFilterField::ContractId, FilterOp::Eq, ODataValue::Uuid(id)) => {
                once(&mut filters.contract_id, *id)?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(filters)
}

/// Step 2 (line list): the collection takes no filters, so any other name is `filter-invalid`.
fn no_filters(query: &Query) -> Result<(), OrdersError> {
    if query
        .values
        .keys()
        .any(|name| name != LIMIT && name != CURSOR)
    {
        return Err(refused(Reason::FilterInvalid));
    }
    Ok(())
}

/// Step 3: a present `cursor` is a bounded opaque token; repeated or malformed is
/// `cursor-invalid`. Its binding is validated by the service before the access decision.
fn cursor(query: &Query) -> Result<Option<Cursor>, OrdersError> {
    if query.repeated.iter().any(|name| name == CURSOR) {
        return Err(refused(Reason::CursorInvalid));
    }
    query
        .values
        .get(CURSOR)
        .map(|text| Cursor::try_from(text.clone()).map_err(|_| refused(Reason::CursorInvalid)))
        .transpose()
}

/// The validated order-list request, in the flow's order.
fn list_request(uri: &Uri) -> Result<ListOrders, OrdersError> {
    let query = query(uri)?;
    let limit = limit(&query)?;
    let filters = order_filters(&query)?;
    let cursor = cursor(&query)?;
    Ok(ListOrders {
        filters,
        limit,
        cursor,
    })
}

/// The validated line-list request, in the flow's order.
fn line_request(uri: &Uri) -> Result<LineList, OrdersError> {
    let query = query(uri)?;
    let limit = limit(&query)?;
    no_filters(&query)?;
    let cursor = cursor(&query)?;
    Ok(LineList { limit, cursor })
}

/// The validated query of a per-order history page (no filters), in the flow's order.
fn history_request(uri: &Uri) -> Result<(Option<PageSize>, Option<Cursor>), OrdersError> {
    let query = query(uri)?;
    let limit = limit(&query)?;
    no_filters(&query)?;
    let cursor = cursor(&query)?;
    Ok((limit, cursor))
}

/// The `{version}` path segment: a committed version is a positive SQL integer written in
/// decimal digits; anything else is `request-invalid` before the access decision.
fn version_segment(text: &str) -> Result<OrderVersion, OrdersError> {
    let invalid = || refused(Reason::RequestInvalid);
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let number = text.parse::<i32>().map_err(|_| invalid())?;
    OrderVersion::try_from(i64::from(number)).map_err(|_| invalid())
}

fn etag_value(version: OrderVersion) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{}\"", i64::from(version)))
        .unwrap_or_else(|_| HeaderValue::from_static("\"0\""))
}

/// The read result as its REST response DTO. A member the SDK serializer refuses is the
/// sanitized internal error, logged with the member name only.
fn wire<S, D>(result: S) -> Result<D, OrdersError>
where
    D: TryFrom<S, Error = ResponseUnencodable>,
{
    D::try_from(result).map_err(|error| {
        tracing::error!(
            target: "orders.rest.read",
            member = error.0,
            "bss-orders: read response has no wire spelling"
        );
        OrdersError::Internal
    })
}

fn served<T: Serialize>(etag: Option<OrderVersion>, body: &T) -> Response {
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    if let Some(version) = etag {
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, etag_value(version));
    }
    response
}

async fn get_order(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let caller = super::caller(ctx, &headers)?;
    let view = service.get(&caller, order_id).await?;
    let etag = view.version.version;
    Ok(served(Some(etag), &wire::<_, OrdersOrderView>(view)?))
}

async fn list_orders(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let caller: Caller = super::caller(ctx, &headers)?;
    let request = list_request(&uri)?;
    let page = service.list(&caller, &request).await?;
    Ok(served(None, &wire::<_, OrdersOrderPage>(page)?))
}

async fn list_lines(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let caller = super::caller(ctx, &headers)?;
    let request = line_request(&uri)?;
    let page = service.list_lines(&caller, order_id, &request).await?;
    let etag = page.current_version;
    Ok(served(Some(etag), &wire::<_, OrdersLinePage>(page)?))
}

async fn list_versions(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let caller = super::caller(ctx, &headers)?;
    let (limit, cursor) = history_request(&uri)?;
    let page = service
        .list_versions(&caller, order_id, &VersionList { limit, cursor })
        .await?;
    let etag = page.current_version;
    Ok(served(etag, &wire::<_, OrdersVersionPage>(page)?))
}

async fn get_version(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((order_id, version)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let caller = super::caller(ctx, &headers)?;
    let version = version_segment(&version)?;
    let view = service.get_version(&caller, order_id, version).await?;
    // No ETag: the read version is not necessarily the current one `If-Match` must name.
    Ok(served(None, &wire::<_, OrdersOrderVersionView>(view)?))
}

async fn get_acceptance(
    Extension(service): Extension<Arc<ReadService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(order_id): Path<Uuid>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let caller = super::caller(ctx, &headers)?;
    let (limit, cursor) = history_request(&uri)?;
    let page = service
        .get_acceptance(&caller, order_id, &AcceptanceList { limit, cursor })
        .await?;
    let etag = page.current_version;
    Ok(served(etag, &wire::<_, OrdersAcceptancePage>(page)?))
}

fn etag() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "ETag",
        "The current commercial version, the value a write sends back as If-Match",
        ResponseHeaderType::String,
    )
}

const LIMIT_DOC: &str = "Page size 1-200 (default 50); outside the range is 400 PAGE_SIZE_EXCEEDED";
const FILTER_DOC: &str = "OData filter joined by `and` only, each term at most once: `state eq \
                          'draft'` (a registered state token), `created_at ge <instant>` \
                          (inclusive), `created_at lt <instant>` (exclusive), `state_entered_at \
                          le <instant>` (in the current state since that instant or earlier), \
                          `contract_id eq <uuid>`. Anything else is 400 FILTER_INVALID";
const CURSOR_DOC: &str = "Opaque continuation from the previous page's next_cursor; a token \
                          failing structure, version, precision or its binding to this request \
                          is 400 CURSOR_INVALID";

/// Register the read routes and attach the shared service once (`ToolKit` REST rule).
pub fn router(service: Arc<ReadService>, openapi: &dyn OpenApiRegistry) -> Router {
    let router = Router::new();
    let router = OperationBuilder::get("/bss-orders/v1/orders/{orderId}")
        .operation_id("bss_orders.get")
        .summary("Read the composed current order")
        .description(
            "The order at its current commercial version with its current lines, read from the \
             stored aggregate under the caller's PDP scope (never a chain walk, cache or \
             replica). Draft reads carry draft_revision; the ETag is the current version. A \
             missing or hidden order is 404 ORDER_NOT_FOUND on both arms. Committed-version \
             content (pins, totals, fulfillment projection, acceptance, administrative values) \
             is not delivered yet: an order outside draft answers 503. 503 READ_STORE_UNAVAILABLE \
             when a required access log cannot be committed; nothing is disclosed then.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .param(super::delegation_proof_param())
        .handler(get_order)
        .json_response_with_schema::<OrdersOrderView>(openapi, StatusCode::OK, "Current order")
        .response_header(etag())
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/bss-orders/v1/orders")
        .operation_id("bss_orders.list")
        .summary("List orders")
        .description(
            "One authorized keyset page in (created_at, order_id) order, scoped by the \
                 caller's PDP relationship (customer resource tenant, seller, current payer or an \
                 explicit service order set) in SQL, answered as items and page_info. $filter \
                 supports the terms listed on the parameter; an unknown query name, sort or \
                 malformed value is 400 FILTER_INVALID. Untargeted denials are 403 \
                 DELEGATION_PROOF_REQUIRED / DELEGATION_PROOF_INVALID / \
                 OPERATION_NOT_PERMITTED_FOR_ACTOR.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(LIMIT, false, LIMIT_DOC, "integer")
        .query_param(CURSOR, false, CURSOR_DOC)
        // Not `.with_odata_filter::<OrderListFilterField>()`: it publishes every operator each
        // field kind admits (`ne`, `contains`, `in`, … for a `String`), a superset of the five
        // D-212 terms `order_filters` accepts. The terms are listed here and parsed by the
        // platform parser over the derive's field table (D-225 item 3, #166).
        .query_param(FILTER, false, FILTER_DOC)
        .param(super::delegation_proof_param())
        .handler(list_orders)
        .json_response_with_schema::<OrdersOrderPage>(openapi, StatusCode::OK, "Authorized page")
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/bss-orders/v1/orders/{orderId}/lines")
        .operation_id("bss_orders.list_lines")
        .summary("List the order's current lines")
        .description(
            "One authorized keyset page of the current lines in (created_at, line_id) \
                 identity order, through the current parent's PDP scope. While the order is a \
                 draft the members are the working set as authored; a removed draft line is not \
                 a member. current_version and draft_revision come from the same snapshot as the \
                 lines; the ETag is the current version. Per-line fulfillment status and \
                 subscription linkage are not delivered yet.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .query_param_typed(LIMIT, false, LIMIT_DOC, "integer")
        .query_param(CURSOR, false, CURSOR_DOC)
        .param(super::delegation_proof_param())
        .handler(list_lines)
        .json_response_with_schema::<OrdersLinePage>(openapi, StatusCode::OK, "Authorized page")
        .response_header(etag())
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    let router = history_routes(router, openapi);
    router.layer(Extension(service))
}

/// The S4-06 history routes (D-254): `order × read` on the current parent, in every state.
fn history_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get("/bss-orders/v1/orders/{orderId}/versions")
        .operation_id("bss_orders.list_versions")
        .summary("List the order's committed versions")
        .description(
            "One authorized keyset page of the committed version history in version order, read \
            from the stored version rows (never a chain walk or event replay): version, reason \
            (create, submit, amendment), actor, actor tenant, engine timestamp, \
            amendment_reason and supersedes_version. Reserved or burned candidate numbers are \
            gaps, never placeholder items. current_version and current_acceptance (absent: no \
            consent recorded) come from the same snapshot; the ETag is the current version. \
            Authorization is decided on the current order first: a reader without access gets \
            404 ORDER_NOT_FOUND, exactly as for a missing order. History permission is order \
            read; it does not grant audit-trail access.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .query_param_typed(LIMIT, false, LIMIT_DOC, "integer")
        .query_param(CURSOR, false, CURSOR_DOC)
        .param(super::delegation_proof_param())
        .handler(list_versions)
        .json_response_with_schema::<OrdersVersionPage>(openapi, StatusCode::OK, "Authorized page")
        .response_header(etag())
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/bss-orders/v1/orders/{orderId}/versions/{version}")
        .operation_id("bss_orders.get_version")
        .summary("Read one committed version")
        .description(
            "The exact committed version composed at that version from its stored rows: its \
            lines with the complete immutable commercial evidence and its persisted producer \
            totals (version 1, the empty draft, has neither). 404 VERSION_NOT_FOUND for an \
            authorized order that never committed the version, a reserved or burned candidate \
            included; 404 ORDER_NOT_FOUND for a missing or hidden order. A version that is not \
            a positive integer is 400 REQUEST_INVALID. No ETag: the version read is not \
            necessarily the current one. The administrative projection is not delivered.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .path_param("version", "Committed version number (positive integer)")
        .param(super::delegation_proof_param())
        .handler(get_version)
        .json_response_with_schema::<OrdersOrderVersionView>(
            openapi,
            StatusCode::OK,
            "Committed version",
        )
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);
    OperationBuilder::get("/bss-orders/v1/orders/{orderId}/acceptance")
        .operation_id("bss_orders.get_acceptance")
        .summary("Read the order's consent")
        .description(
            "The current version, its recorded consent (absent: none recorded) and one keyset \
            page of the prior versions' consent records in version order: accepted_version, \
            accepted_at, recorded_by, recording_path and requirement_source. Consent is per \
            version: an amended version starts without one. Internal recovery evidence is \
            never returned. The ETag is the current version.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("orderId", "Order identity")
        .query_param_typed(LIMIT, false, LIMIT_DOC, "integer")
        .query_param(CURSOR, false, CURSOR_DOC)
        .param(super::delegation_proof_param())
        .handler(get_acceptance)
        .json_response_with_schema::<OrdersAcceptancePage>(
            openapi,
            StatusCode::OK,
            "Current consent and prior records",
        )
        .response_header(etag())
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
