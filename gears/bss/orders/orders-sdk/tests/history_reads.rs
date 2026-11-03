//! S4-06 history read DTOs (`models.json` `VersionList`, `VersionPage`, `OrderVersionView`,
//! `AcceptanceList`, `AcceptancePage`; D-255, D-260, D-262, D-263): `snake_case` wire names, optional members
//! omitted when absent, round trips and unknown-member refusal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a failed setup or fixture must stop the test at once"
)]
use bss_orders_sdk::authoring::{Category, Currency, SalesPath};
use bss_orders_sdk::models::OrderVersion;
use bss_orders_sdk::reads::{
    AcceptanceList, AcceptancePage, AcceptanceRecord, AdministrativeValues, AdministrativeView,
    LineAdministrativeValues, OrderVersionView, PageInfo, PageSize, RequirementSource,
    VersionActor, VersionActors, VersionHeader, VersionList, VersionMarket, VersionPage,
    VersionReason, VersionRecord,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

fn v(n: i64) -> OrderVersion {
    OrderVersion::try_from(n).unwrap()
}
fn at() -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-10-29T10:00:00.123456Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
}
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let wire = serde_json::to_value(value).unwrap();
    assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), value);
}
fn consent(version: i64, source: RequirementSource) -> AcceptanceRecord {
    AcceptanceRecord {
        accepted_version: v(version),
        accepted_at: at(),
        recorded_by: Uuid::from_u128(102).to_string(),
        recording_path: SalesPath::PartnerPlaced,
        requirement_source: source,
    }
}

#[test]
fn version_page_spells_snake_case_and_omits_absent_members() {
    let page = VersionPage {
        items: vec![
            VersionRecord {
                version: v(1),
                reason: VersionReason::Create,
                actor: Uuid::from_u128(102).to_string(),
                actor_tenant_id: Uuid::from_u128(10),
                created_at: at(),
                amendment_reason: None,
                supersedes_version: None,
            },
            VersionRecord {
                version: v(7),
                reason: VersionReason::Amendment,
                actor: Uuid::from_u128(102).to_string(),
                actor_tenant_id: Uuid::from_u128(10),
                created_at: at(),
                amendment_reason: Some("bigger plan".into()),
                supersedes_version: Some(v(4)),
            },
        ],
        page_info: PageInfo::forward(None, PageSize::try_from(2).unwrap()),
        current_version: Some(v(7)),
        current_acceptance: None,
    };
    let wire = serde_json::to_value(&page).unwrap();
    assert_eq!(
        wire,
        json!({
            "items": [
                {"version": 1, "reason": "create", "actor": Uuid::from_u128(102),
                 "actor_tenant_id": Uuid::from_u128(10), "created_at": "2026-10-29T10:00:00.123456Z"},
                {"version": 7, "reason": "amendment", "actor": Uuid::from_u128(102),
                 "actor_tenant_id": Uuid::from_u128(10), "created_at": "2026-10-29T10:00:00.123456Z",
                 "amendment_reason": "bigger plan", "supersedes_version": 4}
            ],
            "page_info": {"limit": 2},
            "current_version": 7
        })
    );
    round_trip(&page);
    let mut extra = wire;
    extra["items"][0]["explanation"] = json!("x");
    assert!(serde_json::from_value::<VersionPage>(extra).is_err());
    for (reason, token) in [
        (VersionReason::Create, "create"),
        (VersionReason::Submit, "submit"),
        (VersionReason::Amendment, "amendment"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(token));
    }
}

#[test]
fn acceptance_page_carries_current_consent_or_its_absence() {
    let page = AcceptancePage {
        items: vec![consent(4, RequirementSource::Seller)],
        page_info: PageInfo::forward(None, PageSize::default()),
        current_version: Some(v(7)),
        current_acceptance: Some(consent(7, RequirementSource::Volunteered)),
    };
    let wire = serde_json::to_value(&page).unwrap();
    assert_eq!(wire["current_version"], json!(7));
    assert_eq!(
        wire["current_acceptance"],
        json!({"accepted_version": 7, "accepted_at": "2026-10-29T10:00:00.123456Z",
               "recorded_by": Uuid::from_u128(102), "recording_path": "partner_placed",
               "requirement_source": "volunteered"})
    );
    assert_eq!(wire["items"][0]["requirement_source"], json!("seller"));
    round_trip(&page);
    let absent = AcceptancePage {
        current_acceptance: None,
        ..page
    };
    assert!(
        serde_json::to_value(&absent)
            .unwrap()
            .get("current_acceptance")
            .is_none()
    );
    round_trip(&absent);
    for (source, token) in [
        (RequirementSource::Contract, "contract"),
        (RequirementSource::Seller, "seller"),
        (RequirementSource::PlatformDefault, "platform_default"),
        (RequirementSource::Volunteered, "volunteered"),
    ] {
        assert_eq!(serde_json::to_value(source).unwrap(), json!(token));
    }
}

fn pair(subject: u128, tenant: u128) -> VersionActor {
    VersionActor {
        actor: Uuid::from_u128(subject).to_string(),
        actor_tenant_id: Uuid::from_u128(tenant),
    }
}

/// Version 1 of an order: the empty draft, created by subject 102 of tenant 10.
fn create_view() -> OrderVersionView {
    OrderVersionView {
        order_id: Uuid::from_u128(5),
        version: v(1),
        reason: VersionReason::Create,
        actor: Uuid::from_u128(102).to_string(),
        actor_tenant_id: Uuid::from_u128(10),
        created_at: at(),
        amendment_reason: None,
        supersedes_version: None,
        header: VersionHeader {
            category: Category::NewSale,
            resource_tenant_id: Uuid::from_u128(10),
            seller_tenant_id: Uuid::from_u128(20),
            payer_tenant_id: Uuid::from_u128(30),
            contract_id: None,
            market: None,
        },
        actors: VersionActors {
            creator: pair(102, 10),
            submitter: None,
            amender: None,
        },
        lines: Vec::new(),
        resolved_total: None,
        admin: None,
    }
}

/// `models.json` `OrderVersionView` (D-260, D-262, D-263): the version row's metadata, its
/// commercial header, the role actors, the lines, the totals and the current administrative
/// projection; optional members are omitted when absent and an unknown member is refused.
#[test]
fn version_view_names_the_models_json_members() {
    let view = create_view();
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(
        wire,
        json!({
            "order_id": Uuid::from_u128(5),
            "version": 1,
            "reason": "create",
            "actor": Uuid::from_u128(102),
            "actor_tenant_id": Uuid::from_u128(10),
            "created_at": "2026-10-29T10:00:00.123456Z",
            "header": {
                "category": Category::NEW_SALE,
                "resource_tenant_id": Uuid::from_u128(10),
                "seller_tenant_id": Uuid::from_u128(20),
                "payer_tenant_id": Uuid::from_u128(30)
            },
            "actors": {"creator": {"actor": Uuid::from_u128(102), "actor_tenant_id": Uuid::from_u128(10)}},
            "lines": []
        })
    );
    round_trip(&view);
    let amended = OrderVersionView {
        version: v(7),
        reason: VersionReason::Amendment,
        actor: Uuid::from_u128(104).to_string(),
        actor_tenant_id: Uuid::from_u128(11),
        amendment_reason: Some("bigger plan".into()),
        supersedes_version: Some(v(4)),
        header: VersionHeader {
            contract_id: Some(Uuid::from_u128(40)),
            market: Some(VersionMarket {
                currency: Currency::try_from("EUR".to_owned()).unwrap(),
                region: Some("eu".into()),
            }),
            ..create_view().header
        },
        actors: VersionActors {
            creator: pair(102, 10),
            submitter: Some(pair(103, 11)),
            amender: Some(pair(104, 11)),
        },
        resolved_total: Some(Vec::new()),
        admin: Some(AdministrativeView {
            order: Some(AdministrativeValues {
                external_reference: Some("PO-1".into()),
                display_label: None,
                internal_notes: None,
            }),
            lines: vec![LineAdministrativeValues {
                line_id: Uuid::from_u128(9),
                external_reference: None,
                display_label: Some("L".into()),
                internal_notes: None,
            }],
        }),
        ..create_view()
    };
    let wire = serde_json::to_value(&amended).unwrap();
    assert_eq!(wire["amendment_reason"], json!("bigger plan"));
    assert_eq!(wire["supersedes_version"], json!(4));
    assert_eq!(wire["header"]["contract_id"], json!(Uuid::from_u128(40)));
    assert_eq!(
        wire["header"]["market"],
        json!({"currency": "EUR", "region": "eu"})
    );
    assert_eq!(
        wire["actors"],
        json!({
            "creator": {"actor": Uuid::from_u128(102), "actor_tenant_id": Uuid::from_u128(10)},
            "submitter": {"actor": Uuid::from_u128(103), "actor_tenant_id": Uuid::from_u128(11)},
            "amender": {"actor": Uuid::from_u128(104), "actor_tenant_id": Uuid::from_u128(11)}
        })
    );
    assert_eq!(wire["resolved_total"], json!([]));
    assert_eq!(
        wire["admin"],
        json!({
            "order": {"external_reference": "PO-1"},
            "lines": [{"line_id": Uuid::from_u128(9), "display_label": "L"}]
        })
    );
    round_trip(&amended);
    let mut extra = wire;
    extra["explanation"] = json!("x");
    // A response type tolerates members a newer server adds (rolling deploy, as `OrderView`).
    assert_eq!(
        serde_json::from_value::<OrderVersionView>(extra).expect("unknown member tolerated"),
        amended
    );
}

#[test]
fn history_requests_default_to_the_shared_page_size() {
    assert_eq!(VersionList::default().limit, None);
    assert_eq!(AcceptanceList::default().cursor, None);
}
