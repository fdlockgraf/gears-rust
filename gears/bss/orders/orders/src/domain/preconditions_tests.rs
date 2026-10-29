#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once"
)]
use super::*;

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[test]
fn auto_consent_only_for_an_own_tenant_request_without_proof() {
    let (own, other) = (tenant(10), tenant(11));
    assert!(auto_consent(own, own, false));
    assert!(
        !auto_consent(own, own, true),
        "a delegation proof suppresses it"
    );
    assert!(!auto_consent(other, own, false), "a partner never consents");
    assert!(!auto_consent(other, own, true));
}

#[test]
fn the_live_contract_decides_before_any_election() {
    assert_eq!(
        acceptance_requirement(Some(false), Some(true), Some(true)),
        Requirement::NotRequired
    );
    assert_eq!(
        acceptance_requirement(Some(true), Some(false), Some(false)),
        Requirement::Required(RequirementSource::Contract)
    );
}

#[test]
fn the_seller_election_decides_before_the_platform() {
    assert_eq!(
        acceptance_requirement(None, Some(true), Some(false)),
        Requirement::Required(RequirementSource::Seller)
    );
    assert_eq!(
        acceptance_requirement(None, Some(false), Some(true)),
        Requirement::NotRequired
    );
}

#[test]
fn the_platform_election_and_its_unset_fallback_are_the_platform_default() {
    assert_eq!(
        acceptance_requirement(None, None, Some(true)),
        Requirement::Required(RequirementSource::PlatformDefault)
    );
    assert_eq!(
        acceptance_requirement(None, None, Some(false)),
        Requirement::NotRequired
    );
    assert_eq!(
        acceptance_requirement(None, None, None),
        Requirement::Required(RequirementSource::PlatformDefault)
    );
}

#[test]
fn a_consent_nothing_required_is_volunteered() {
    assert_eq!(
        recorded_source(Requirement::NotRequired),
        RequirementSource::Volunteered
    );
    for source in [
        RequirementSource::Contract,
        RequirementSource::Seller,
        RequirementSource::PlatformDefault,
    ] {
        assert_eq!(recorded_source(Requirement::Required(source)), source);
    }
}

#[test]
fn tolerance_is_seller_then_platform_then_off() {
    assert!(!tolerate_failure(None, None));
    assert!(tolerate_failure(None, Some(true)));
    assert!(!tolerate_failure(Some(false), Some(true)));
    assert!(tolerate_failure(Some(true), Some(false)));
}

#[test]
fn source_tokens_are_the_acceptance_check_vocabulary() {
    // `ck_bss_orders__acceptance__1` and the event's snake_case spelling.
    let tokens: Vec<&str> = RequirementSource::ALL.iter().map(|s| s.token()).collect();
    assert_eq!(
        tokens,
        ["contract", "seller", "platform_default", "volunteered"]
    );
    for source in RequirementSource::ALL {
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            serde_json::Value::String(source.token().to_owned())
        );
    }
    assert_eq!(SELF_SERVICE, "self_service");
}

// ---- S4-08b: the recording-party bar (05 §3.2 steps 1-2, *Record Acceptance* step 4) ----

/// A role version appended by `actor` from `tenant`.
fn role(actor: &str, tenant: Uuid) -> RoleActor {
    RoleActor {
        actor: actor.to_owned(),
        actor_tenant_id: tenant,
    }
}

/// Every role taken by someone other than the caller, on the resource tenant.
fn others(resource: Uuid) -> RoleVersions {
    RoleVersions {
        creator: role("creator", resource),
        submitter: role("submitter", resource),
        amender: Some(role("amender", resource)),
    }
}

/// The full truth table: creator, submitter and amender, each with the caller's identity or
/// another one, on both sales paths, appended from the resource tenant or another tenant. The
/// caller is barred exactly when that role's identity matches AND (the order is partner-placed
/// OR that role's actor tenant differs from the resource tenant).
#[test]
fn the_recording_party_bar_truth_table() {
    let (resource, partner) = (tenant(10), tenant(11));
    let caller = "caller";
    for which in ["creator", "submitter", "amender"] {
        for same_identity in [true, false] {
            for path in [SalesPath::SelfService, SalesPath::PartnerPlaced] {
                for actor_tenant in [resource, partner] {
                    let mut roles = others(resource);
                    let taken = role(if same_identity { caller } else { "someone" }, actor_tenant);
                    match which {
                        "creator" => roles.creator = taken,
                        "submitter" => roles.submitter = taken,
                        _ => roles.amender = Some(taken),
                    }
                    let expected = same_identity
                        && (path == SalesPath::PartnerPlaced || actor_tenant != resource);
                    assert_eq!(
                        recording_party_barred(caller, path, resource, &roles),
                        expected,
                        "{which} same={same_identity} {path:?} tenant={actor_tenant}"
                    );
                }
            }
        }
    }
}

/// The bar is per role: a resource-tenant creator of a self-service order is not barred because
/// a delegate of another tenant submitted it; the delegate is.
#[test]
fn a_resource_tenant_creator_is_not_barred_by_a_delegated_submit() {
    let (resource, partner) = (tenant(10), tenant(11));
    let roles = RoleVersions {
        creator: role("customer", resource),
        submitter: role("delegate", partner),
        amender: None,
    };
    assert!(!recording_party_barred(
        "customer",
        SalesPath::SelfService,
        resource,
        &roles
    ));
    assert!(recording_party_barred(
        "delegate",
        SalesPath::SelfService,
        resource,
        &roles
    ));
    // On a partner-placed order every role actor is barred, even one on the resource tenant.
    assert!(recording_party_barred(
        "customer",
        SalesPath::PartnerPlaced,
        resource,
        &roles
    ));
    // Nobody who took no role is barred, on either path.
    for path in [SalesPath::SelfService, SalesPath::PartnerPlaced] {
        assert!(!recording_party_barred("bystander", path, resource, &roles));
    }
}

/// Only the current version's amendment counts: an order without one is decided on the creator
/// and the submitter alone.
#[test]
fn without_an_amendment_only_creator_and_submitter_count() {
    let resource = tenant(10);
    let roles = RoleVersions {
        creator: role("a", resource),
        submitter: role("b", resource),
        amender: None,
    };
    assert!(!recording_party_barred(
        "c",
        SalesPath::PartnerPlaced,
        resource,
        &roles
    ));
}
