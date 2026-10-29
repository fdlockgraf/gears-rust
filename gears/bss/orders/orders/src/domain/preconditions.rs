//! Acceptance preconditions (05 §4.1–4.2; S4-08; D-146, D-251, D-255): who consents automatically
//! on a submit, whether consent is required, the source a recorded consent carries and who is
//! barred from recording the customer's consent after submit.
//!
//! Pure rules over already-resolved facts. The caller supplies the live contract's
//! `acceptance_required` and the seller and platform elections as read (`None` = no row); a read
//! that failed never reaches these rules (it answers `unavailable`, never a default).
use bss_orders_sdk::authoring::SalesPath;
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

/// Who appended one stored role version (`orders_order_version.actor`, `actor_tenant_id`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleActor {
    /// The appending actor's reference (D-103: the lowercase hyphenated subject UUID).
    pub actor: String,
    /// The appending actor's trusted subject tenant.
    pub actor_tenant_id: Uuid,
}

/// The role versions the recording-party bar reads (05 §3.2 step 1): the creator (version 1,
/// reason `create`), the actual submit version and, when the current version was appended by an
/// amendment, its amender.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleVersions {
    /// Version 1's appender.
    pub creator: RoleActor,
    /// The submit version's appender.
    pub submitter: RoleActor,
    /// The current version's appender when its reason is `amendment`; `None` otherwise.
    pub amender: Option<RoleActor>,
}

/// The recording-party bar (05 §3.2 step 2, *Record Acceptance* step 4; D-130, D-146): `caller`
/// (the trusted actor's reference) is barred when, for any one role version, it is that version's
/// actor **and** either the order is `partner_placed` or that version's actor tenant is not the
/// resource tenant. Each role is decided on its own, so a resource-tenant creator is not barred
/// because a delegate submitted; `self_service` alone never admits a role actor of another
/// tenant.
#[must_use]
pub fn recording_party_barred(
    caller: &str,
    sales_path: SalesPath,
    resource_tenant_id: Uuid,
    roles: &RoleVersions,
) -> bool {
    [
        Some(&roles.creator),
        Some(&roles.submitter),
        roles.amender.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|role| {
        role.actor == caller
            && (sales_path == SalesPath::PartnerPlaced
                || role.actor_tenant_id != resource_tenant_id)
    })
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
