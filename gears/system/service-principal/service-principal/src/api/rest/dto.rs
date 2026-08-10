//! REST DTOs for the service-principal gear.

use secrecy::ExposeSecret as _;
use service_principal_sdk::{ServicePrincipalCredentials, ServicePrincipalSummary};
use uuid::Uuid;

/// Request body for `POST …/service-principals`. `tenant_id` comes from the path.
///
/// `name`/`scopes` are not secret, so `Debug` is plain-derived (unlike the
/// credential DTOs below, which hand-write a redacting `Debug`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateServicePrincipalRequestDto {
    /// Short caller-chosen suffix (lowercase alnum + '-', max 40 chars).
    /// Adapters commonly derive the client id as `svc-<tenant_id>-<name>`, but
    /// that format is a convention, not a contract; the listing echoes this
    /// `name` back so callers correlate without parsing client ids.
    pub name: String,
    /// Client scopes to attach; validated against the adapter allowlist.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Live credentials returned by create + rotate. The secret is returned ONLY here —
/// hand-written `Debug` redacts it, and the handler sets `Cache-Control: no-store`.
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ServicePrincipalCredentialsDto {
    /// The adapter-assigned client id. Opaque to callers: `svc-<tenant_id>-<name>`
    /// is a common adapter convention, not a format callers may rely on.
    pub client_id: String,
    /// The `client_credentials` secret. Returned exactly once by create/rotate —
    /// persist it immediately; recovery from loss is `rotate_secret`.
    pub client_secret: String,
    /// OAuth token endpoint for the `client_credentials` grant.
    pub token_url: String,
    /// The principal's subject id (`sub` of issued tokens) — use it for RBAC bindings.
    pub subject_id: Uuid,
}

impl std::fmt::Debug for ServicePrincipalCredentialsDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServicePrincipalCredentialsDto")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("token_url", &self.token_url)
            .field("subject_id", &self.subject_id)
            .finish()
    }
}

impl From<ServicePrincipalCredentials> for ServicePrincipalCredentialsDto {
    fn from(c: ServicePrincipalCredentials) -> Self {
        Self {
            client_id: c.client_id,
            client_secret: c.client_secret.expose_secret().to_owned(),
            token_url: c.token_url,
            subject_id: c.subject_id,
        }
    }
}

/// A listing entry (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ServicePrincipalSummaryDto {
    /// The adapter-assigned client id, used to address rotate-secret and revoke.
    pub client_id: String,
    /// The caller-supplied `name` this principal was created with, reported
    /// verbatim by the adapter. This is how a caller correlates a name it
    /// submitted with an opaque `client_id` — notably to reconcile a create
    /// that returned an ambiguous outcome.
    pub name: String,
    /// Whether the client can currently authenticate.
    pub enabled: bool,
    /// Attached client scopes as reported by the `IdP` — includes realm-default
    /// scopes, not only consumer-requested ones.
    pub scopes: Vec<String>,
}

impl From<ServicePrincipalSummary> for ServicePrincipalSummaryDto {
    fn from(s: ServicePrincipalSummary) -> Self {
        Self {
            client_id: s.client_id,
            name: s.name,
            enabled: s.enabled,
            scopes: s.scopes,
        }
    }
}

/// Response body for `GET …/service-principals`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ListServicePrincipalsResponseDto {
    /// The tenant's service principals, in upstream `IdP` order (no pagination).
    pub service_principals: Vec<ServicePrincipalSummaryDto>,
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
