//! Incremental local SDK surface: draft authoring (S2-09), the authorized draft reads
//! (early S6-01/S6-04), the version and consent history reads (S4-06) and the typed submit entry
//! point.
use crate::{
    OrdersError,
    amend::AmendOrder,
    authoring::{AddLine, CreateMeta, CreateOrder, HeaderPatch, LinePatch, OrderView},
    models::OrderVersion,
    models::{CallMeta, DraftRevision, TransitionResult},
    preview::{PreviewBasket, PreviewResult},
    reads::{
        AcceptanceList, AcceptancePage, LineList, LinePage, ListOrders, OrderPage,
        OrderVersionView, ReadMeta, VersionList, VersionPage,
    },
};
use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Orders application entry point. Resolve through the toolkit `ClientHub`.
///
/// Every method runs the same shared service as REST under the real caller context: the shared
/// PEP authorizes, the transition engine commits. Methods of packages not yet delivered return
/// [`OrdersError::Unavailable`]; no commercial success is possible. Failures are the
/// transport-neutral [`OrdersError`] projection (ADR-0005, D-219): see [`crate::errors`] for the
/// dispatch table.
#[async_trait]
pub trait OrdersV1: Send + Sync {
    /// Create an empty draft: version 1, draft revision 0, seller-unique number (02 §2.1).
    ///
    /// # Errors
    /// An Orders refusal or an infrastructure/unavailable failure.
    async fn create(
        &self,
        ctx: &SecurityContext,
        request: CreateOrder,
        meta: CreateMeta,
    ) -> Result<OrderView, OrdersError>;

    /// Edit the order header; the named fields' classes alone select the trigger (02 §3.1).
    /// `expected_draft_revision` is optional here and compared by the engine after admissibility
    /// (D-147).
    ///
    /// # Errors
    /// An Orders refusal or an infrastructure/unavailable failure.
    async fn patch_order(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        patch: HeaderPatch,
        expected_draft_revision: Option<DraftRevision>,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// Add one draft working-set line; the result carries its server-reserved `line_id`.
    ///
    /// # Errors
    /// An Orders refusal or an infrastructure/unavailable failure.
    async fn add_line(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        line: AddLine,
        expected_draft_revision: Option<DraftRevision>,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// Edit a line; the named fields' classes alone select the trigger (02 §3.1).
    ///
    /// # Errors
    /// An Orders refusal or an infrastructure/unavailable failure.
    async fn patch_line(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        line_id: Uuid,
        patch: LinePatch,
        expected_draft_revision: Option<DraftRevision>,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// Remove a line from the draft working set; its identity stays reserved.
    ///
    /// # Errors
    /// An Orders refusal or an infrastructure/unavailable failure.
    async fn remove_line(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        line_id: Uuid,
        expected_draft_revision: Option<DraftRevision>,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// The composed current-order read (08 §3.6 *Read One Order*): the authorized aggregate at
    /// its current version with its lines, under the common read wrapper and S6-04 logging.
    /// The S1-02 catalog lists `get` on the Workflow trait as well; one application entry serves
    /// every caller class under the same PEP decision.
    ///
    /// # Errors
    /// `order-not-found` for a missing or hidden target, a sanitized unavailable/integration
    /// failure, or `read-store-unavailable` when a required access log could not be committed.
    async fn get(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        meta: ReadMeta,
    ) -> Result<OrderView, OrdersError>;

    /// One authorized page of orders (08 §3.6 *List Orders*): validated filters, page size and
    /// cursor, then `order × read` without a target, PDP scope applied in SQL.
    ///
    /// # Errors
    /// `page-size-exceeded`, `filter-invalid`, `cursor-invalid`, the untargeted denial reasons,
    /// a sanitized unavailable/integration failure, or `read-store-unavailable`.
    async fn list(
        &self,
        ctx: &SecurityContext,
        request: ListOrders,
        meta: ReadMeta,
    ) -> Result<OrderPage, OrdersError>;

    /// One authorized page of an order's current lines through the current parent.
    ///
    /// # Errors
    /// As [`Self::get`] plus `page-size-exceeded` and `cursor-invalid`.
    async fn list_lines(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        request: LineList,
        meta: ReadMeta,
    ) -> Result<LinePage, OrdersError>;

    /// One authorized page of the order's committed version history in version order (S4-06):
    /// committed rows only, never a placeholder for a burned candidate, with the current version
    /// and its consent from the same snapshot. Authorized on the current parent first.
    ///
    /// # Errors
    /// As [`Self::get`] plus `page-size-exceeded` and `cursor-invalid`.
    async fn list_versions(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        request: VersionList,
        meta: ReadMeta,
    ) -> Result<VersionPage, OrdersError>;

    /// One exact committed version, composed at that version (S4-06). The S1-02 catalog lists
    /// this operation on `OrdersWorkflowSeamV1`, which the SDK does not define; it is served here
    /// for every `order × read` caller, as `get` is (D-254).
    ///
    /// # Errors
    /// As [`Self::get`]; `version-not-found` for an authorized order that never committed
    /// `version` (a reserved or burned candidate included).
    async fn get_version(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        version: OrderVersion,
        meta: ReadMeta,
    ) -> Result<OrderVersionView, OrdersError>;

    /// The consent read (S4-06): the current version, its consent or its absence, and one page of
    /// the prior versions' consent records.
    ///
    /// # Errors
    /// As [`Self::get`] plus `page-size-exceeded` and `cursor-invalid`.
    async fn get_acceptance(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        request: AcceptanceList,
        meta: ReadMeta,
    ) -> Result<AcceptancePage, OrdersError>;

    /// Submit exactly the observed draft revision under the real caller context.
    ///
    /// # Errors
    /// Returns an Orders refusal or an infrastructure/unavailable failure.
    async fn submit(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        expected_draft_revision: DraftRevision,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// Amend a submitted, pending-approval or approved order: a delta onto the committed version
    /// named by `meta.expected_version` (`If-Match`), under an explanation (S4-03). A stale
    /// version is the audited `precondition-failed`; the engine decides every other refusal.
    ///
    /// # Errors
    /// `expected-version-required` or `request-invalid` for a malformed request, an Orders
    /// refusal, or an infrastructure/unavailable failure.
    async fn amend(
        &self,
        ctx: &SecurityContext,
        order_id: Uuid,
        request: AmendOrder,
        meta: CallMeta,
    ) -> Result<TransitionResult, OrdersError>;

    /// Assess a basket without storing anything (F03 §2.2): the nonbinding Pricing assessment,
    /// gate report, indicative total and tax. No idempotency key (catalog: none), no receipt,
    /// hold or attempt, and never the submit phase-B check.
    ///
    /// # Errors
    /// `request-invalid` for a basket over the line limit or a line breaking the draft-line
    /// rules, the untargeted denial reasons, or a sanitized unavailable/integration failure.
    async fn preview(
        &self,
        ctx: &SecurityContext,
        basket: PreviewBasket,
        meta: ReadMeta,
    ) -> Result<PreviewResult, OrdersError>;
}
