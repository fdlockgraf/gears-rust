//! DTO tests: secret redaction + request deserialization.

use secrecy::SecretString;
use service_principal_sdk::{ServicePrincipalCredentials, ServicePrincipalSummary};
use uuid::Uuid;

use super::*;

#[test]
fn credentials_debug_redacts_secret() {
    let dto = ServicePrincipalCredentialsDto::from(ServicePrincipalCredentials {
        client_id: "svc-abc".to_owned(),
        client_secret: SecretString::from("super-secret".to_owned()),
        token_url: "https://idp/token".to_owned(),
        subject_id: Uuid::nil(),
    });
    let rendered = format!("{dto:?}");
    assert!(
        !rendered.contains("super-secret"),
        "secret must not appear in Debug"
    );
    // The value still round-trips into JSON (that is the whole point of the DTO).
    assert_eq!(dto.client_secret, "super-secret");
}

/// The summary DTO must carry the caller-supplied name verbatim onto the wire:
/// with adapter-assigned (possibly opaque) client ids, that name is the only
/// contractual way a caller correlates a principal it asked for.
#[test]
fn summary_dto_serializes_caller_supplied_name_verbatim() {
    let dto = ServicePrincipalSummaryDto::from(ServicePrincipalSummary {
        client_id: "9f1c0b3e-object-id".to_owned(),
        name: "ci".to_owned(),
        enabled: true,
        scopes: vec!["openid".to_owned()],
    });
    assert_eq!(dto.name, "ci");
    let json = serde_json::to_value(&dto).expect("summary serializes");
    assert_eq!(json["name"], "ci");
}

#[test]
fn create_request_rejects_unknown_fields() {
    let ok: Result<CreateServicePrincipalRequestDto, _> =
        serde_json::from_str(r#"{"name":"ci","scopes":["openid"]}"#);
    assert!(ok.is_ok());
    let bad: Result<CreateServicePrincipalRequestDto, _> =
        serde_json::from_str(r#"{"name":"ci","unexpected":true}"#);
    assert!(bad.is_err(), "deny_unknown_fields must reject extra keys");
}

#[test]
fn create_request_scopes_default_to_empty() {
    let dto: CreateServicePrincipalRequestDto =
        serde_json::from_str(r#"{"name":"ci"}"#).expect("scopes optional");
    assert!(dto.scopes.is_empty());
}
