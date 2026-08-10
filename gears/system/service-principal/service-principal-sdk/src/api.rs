//! The service-principal SPI trait, resolved via `ClientHub`.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::error::ServicePrincipalFailure;
use crate::models::{
    CreateServicePrincipalRequest, ServicePrincipalCredentials, ServicePrincipalSummary, TenantId,
};

/// Lifecycle of tenant-scoped machine identities (confidential OAuth
/// `client_credentials` clients).
///
/// Contract:
/// - Callers are trusted platform modules; caller authorization
///   (including parent→child tenant subtree semantics) happens in the
///   consumer's RBAC/PDP BEFORE calling. `ctx` is for audit.
/// - `(tenant_id, client_id)` is the scoped resource address; an
///   address that does not resolve within the tenant yields `NotFound`.
/// - The caller-supplied `name` is the correlation key: `list` reports it
///   verbatim per entry, so a caller can map a name it submitted back to
///   the adapter-assigned `client_id` without parsing that id. Client-id
///   formats are adapter conventions, never contract.
/// - `(tenant_id, name)` MUST identify at most one live principal:
///   implementors reject a `create` for a name already live in the tenant
///   (see `create`). Without that uniqueness the correlation key could
///   match several entries and a caller could act on the wrong identity.
/// - The secret is returned only by `create`/`rotate_secret`. Persist
///   it immediately (credstore); a lost secret is recovered by rotate.
/// - Calls run outside any DB transaction. The adapter owns transport
///   resilience and reports transport uncertainty as `Ambiguous`,
///   never as success.
/// - Deployments without an adapter simply have no `ClientHub`
///   registration — `get::<dyn ServicePrincipalClientV1>()` fails.
#[async_trait]
pub trait ServicePrincipalClientV1: Send + Sync + 'static {
    /// Create a confidential `client_credentials`-only client owned by
    /// `req.tenant_id`. Tokens carry `tenant_id` and a service-subject
    /// `user_type`.
    ///
    /// Implementors **MUST** reject a `create` whose `req.name` is already
    /// live in `req.tenant_id` with `InvalidInput`, and **MUST NOT** resume,
    /// reveal, or modify the existing principal — including when that
    /// principal is a half-created one left behind by an earlier `Ambiguous`
    /// failure. So `(tenant_id, name)` identifies at most one live principal,
    /// which is precisely what makes the correlation step below unambiguous:
    /// a name matches one listing entry or none, never several. See
    /// [`ServicePrincipalSummary::name`].
    ///
    /// This check MUST be atomic with respect to concurrent calls: two
    /// concurrent `create` calls for the same `(tenant_id, name)` MUST NOT
    /// both succeed.
    ///
    /// Recovering from `Ambiguous` therefore starts with correlation, not
    /// with a blind retry: `list` the tenant and look for the entry whose
    /// [`ServicePrincipalSummary::name`] equals the submitted `name`. If it
    /// is present the create did land, and the caller resolves it by that
    /// entry's `client_id` — either `rotate_secret` to obtain usable
    /// credentials without deleting, or `revoke` followed by a fresh
    /// `create`. If it is absent, the create did not land and a plain
    /// `create` retry is safe. Principals are deleted when their owning
    /// tenant is deprovisioned.
    async fn create(
        &self,
        ctx: &SecurityContext,
        req: &CreateServicePrincipalRequest,
    ) -> Result<ServicePrincipalCredentials, ServicePrincipalFailure>;

    /// Regenerate the secret; the old one stops working.
    async fn rotate_secret(
        &self,
        ctx: &SecurityContext,
        tenant_id: TenantId,
        client_id: &str,
    ) -> Result<ServicePrincipalCredentials, ServicePrincipalFailure>;

    /// Delete the client. Repeat revokes yield `NotFound`, which callers treat as success-equivalent.
    async fn revoke(
        &self,
        ctx: &SecurityContext,
        tenant_id: TenantId,
        client_id: &str,
    ) -> Result<(), ServicePrincipalFailure>;

    /// List the tenant's service principals (no secrets). Backs audit
    /// and future management surfaces.
    ///
    /// Each entry carries the caller-supplied `name`, which implementors
    /// MUST report verbatim: it is what lets a caller correlate an
    /// `Ambiguous` create with the principal it may have produced.
    async fn list(
        &self,
        ctx: &SecurityContext,
        tenant_id: TenantId,
    ) -> Result<Vec<ServicePrincipalSummary>, ServicePrincipalFailure>;
}
