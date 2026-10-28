//! Acceptance preconditions (05 §4.1–4.2; S4-08; D-146, D-251): who consents automatically on a
//! submit, whether consent is required and the source a recorded consent carries.
//!
//! Pure rules over already-resolved facts. The caller supplies the live contract's
//! `acceptance_required` and the seller and platform elections as read (`None` = no row); a read
//! that failed never reaches these rules (it answers `unavailable`, never a default).
use serde::{Deserialize, Serialize};
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The `recording_path` of a consent the customer gave on its own submit
/// (`ck_bss_orders__acceptance__0`).
pub const SELF_SERVICE: &str = "self_service";

/// Where the acceptance requirement came from (05 §4.2; `orders_acceptance.requirement_source`,
/// the `OrderAcceptanceRecorded` payload).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementSource {
    /// A contract requires acceptance.
    Contract,
    /// A seller-scope election requires acceptance.
    Seller,
    /// The platform-scope election, or its unset fallback, requires acceptance.
    PlatformDefault,
    /// Acceptance was recorded although none was required.
    Volunteered,
}
impl RequirementSource {
    /// Every value, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Contract,
        Self::Seller,
        Self::PlatformDefault,
        Self::Volunteered,
    ];
    /// The stored token (`ck_bss_orders__acceptance__1`), equal to the serialized spelling.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Contract => "contract",
            Self::Seller => "seller",
            Self::PlatformDefault => "platform_default",
            Self::Volunteered => "volunteered",
        }
    }
}

/// Whether an order version requires the customer's acceptance, and why.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Acceptance is required by `source` (never [`RequirementSource::Volunteered`]).
    Required(RequirementSource),
    /// Nothing requires acceptance.
    NotRequired,
}

/// D-146/S4-08 rule 1: automatic consent only for a self-service submit: no delegation proof and
/// the trusted subject tenant is the order's current resource tenant (never inferred from the
/// creation-time `sales_path`).
#[must_use]
pub fn auto_consent(
    subject_tenant_id: Uuid,
    resource_tenant_id: Uuid,
    proof_supplied: bool,
) -> bool {
    !proof_supplied && subject_tenant_id == resource_tenant_id
}

/// Precedence: the live referenced contract, then the seller election, then the platform
/// election; truly unset is required with source [`RequirementSource::PlatformDefault`].
#[must_use]
pub fn acceptance_requirement(
    contract: Option<bool>,
    seller: Option<bool>,
    platform: Option<bool>,
) -> Requirement {
    let (required, source) = match (contract, seller, platform) {
        (Some(required), _, _) => (required, RequirementSource::Contract),
        (None, Some(required), _) => (required, RequirementSource::Seller),
        (None, None, Some(required)) => (required, RequirementSource::PlatformDefault),
        (None, None, None) => (true, RequirementSource::PlatformDefault),
    };
    if required {
        Requirement::Required(source)
    } else {
        Requirement::NotRequired
    }
}

/// The source recorded with a consent: the requirement's source, or
/// [`RequirementSource::Volunteered`] when nothing required it (not a refusal).
#[must_use]
pub fn recorded_source(requirement: Requirement) -> RequirementSource {
    match requirement {
        Requirement::Required(source) => source,
        Requirement::NotRequired => RequirementSource::Volunteered,
    }
}

/// Tolerating a failed authorization (S4-10, its first caller): seller election, then
/// platform; unset is `false`.
#[must_use]
pub fn tolerate_failure(seller: Option<bool>, platform: Option<bool>) -> bool {
    seller.or(platform).unwrap_or(false)
}

#[cfg(test)]
#[path = "preconditions_tests.rs"]
mod tests;
