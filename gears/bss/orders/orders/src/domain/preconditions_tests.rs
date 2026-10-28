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
