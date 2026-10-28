//! S4-07 on real PostgreSQL: the policy-election channel. The deployment's `policy_elections`
//! block is promoted at startup through the policy role (D-207's pattern, D-250): an unchanged
//! promotion only reads, a changed one writes in one transaction or not at all, and a stored
//! seller row the configuration no longer names is kept and reported (rows are undeletable).
use super::*;
use crate::config::{PolicyElectionsConfig, PolicySwitches, SellerPolicyConfig};
use crate::infra::dates::MAX_PROMOTED_ROWS;
use crate::infra::policy::{
    Election, PolicyPlan, PolicyScope, PolicyStoreError, election, install, promote,
};
use std::collections::BTreeMap;

const SELLER: u128 = 20;

fn switches(acceptance: Option<bool>, tolerate: Option<bool>) -> PolicySwitches {
    PolicySwitches {
        acceptance_required: acceptance,
        tolerate_authorization_failure: tolerate,
    }
}
fn config(platform: PolicySwitches, sellers: &[(u128, PolicySwitches)]) -> PolicyElectionsConfig {
    PolicyElectionsConfig {
        elected_by: "deploy-2026-10-28".into(),
        platform,
        sellers: sellers
            .iter()
            .map(|(s, sw)| SellerPolicyConfig {
                seller_tenant_id: u(*s),
                switches: *sw,
            })
            .collect(),
    }
}
fn plan(platform: PolicySwitches, sellers: &[(u128, PolicySwitches)]) -> PolicyPlan {
    PolicyPlan::from_config(&config(platform, sellers)).unwrap()
}

/// `(election, scope_id or 0 for platform, elected, elected_by, policy_id)`, platform first.
async fn rows(pg: &Pg) -> Vec<(String, u128, bool, String, Uuid)> {
    let rows = pg
        .raw
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT election, scope_id, elected, elected_by, policy_id FROM bss_orders__policy_election ORDER BY scope_id NULLS FIRST, election",
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            let scope_id: Option<Uuid> = r.try_get("", "scope_id").unwrap();
            (
                r.try_get("", "election").unwrap(),
                scope_id.map_or(0, |s| s.as_u128()),
                r.try_get("", "elected").unwrap(),
                r.try_get("", "elected_by").unwrap(),
                r.try_get("", "policy_id").unwrap(),
            )
        })
        .collect()
}

#[test]
fn policy_config_is_explicit_and_validated() {
    let yaml = |s: &str| serde_saphyr::from_str::<PolicyElectionsConfig>(s);
    // Unknown keys fail, at every level; the change identity is required.
    assert!(yaml("elected_by: x\nplatform: {acceptance_required: true, extra: 1}").is_err());
    assert!(yaml("elected_by: x\nextra: 1").is_err());
    assert!(yaml("platform: {acceptance_required: true}").is_err());
    assert!(
        yaml("elected_by: x\nsellers:\n  - {seller_tenant_id: 00000000-0000-0000-0000-000000000014, acceptance_required: true, extra: 1}")
            .is_err()
    );
    let ok = yaml(
        "elected_by: deploy-1\nplatform: {acceptance_required: true}\nsellers:\n  - {seller_tenant_id: 00000000-0000-0000-0000-000000000014, tolerate_authorization_failure: false}\n  - {seller_tenant_id: 00000000-0000-0000-0000-000000000015, acceptance_required: false, tolerate_authorization_failure: true}",
    )
    .unwrap();
    let valid = PolicyPlan::from_config(&ok).unwrap();
    let wanted: BTreeMap<_, _> = [
        ((Election::AcceptanceRequired, PolicyScope::Platform), true),
        (
            (
                Election::TolerateAuthorizationFailure,
                PolicyScope::Seller(u(20)),
            ),
            false,
        ),
        (
            (Election::AcceptanceRequired, PolicyScope::Seller(u(21))),
            false,
        ),
        (
            (
                Election::TolerateAuthorizationFailure,
                PolicyScope::Seller(u(21)),
            ),
            true,
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(valid.wanted(), &wanted);
    assert_eq!(valid.elected_by(), "deploy-1");
    assert_eq!(Election::AcceptanceRequired.token(), "acceptance_required");
    assert_eq!(
        Election::TolerateAuthorizationFailure.token(),
        "tolerate_authorization_failure"
    );

    let on = switches(Some(true), None);
    // A nil seller, a duplicated seller, an empty change identity.
    assert!(PolicyPlan::from_config(&config(on, &[(0, on)])).is_err());
    assert!(PolicyPlan::from_config(&config(on, &[(SELLER, on), (SELLER, on)])).is_err());
    let blank = PolicyElectionsConfig {
        elected_by: "  ".into(),
        ..config(on, &[])
    };
    assert!(PolicyPlan::from_config(&blank).is_err());
    // A seller entry that names neither election.
    let nothing = PolicyPlan::from_config(&config(on, &[(SELLER, PolicySwitches::default())]));
    assert!(
        nothing
            .unwrap_err()
            .to_string()
            .contains("names no election")
    );
    // More rows than one promotion may carry.
    let many: Vec<(u128, PolicySwitches)> = (1..=MAX_PROMOTED_ROWS as u128 + 1)
        .map(|s| (s, on))
        .collect();
    assert!(PolicyPlan::from_config(&config(on, &many)).is_err());
}

/// The policy channel (U1-U5): a changed promotion through the runtime role is refused with
/// nothing written; through the policy role it lands in one transaction; the same promotion again
/// only reads, so a runtime-role boot succeeds; a change keeps the row's identity; a seller the
/// configuration no longer names is kept and reported. The read port separates "no row" from a
/// failed read.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_policy_channel_promotes_elections_atomically() -> anyhow::Result<()> {
    let pg = Pg::new().await?;
    let (policy, _) = pg.role("policy").await?;
    let (runtime, _) = pg.role("runtime").await?;
    let first = plan(
        switches(Some(true), None),
        &[(SELLER, switches(None, Some(true)))],
    );

    // 1. The runtime role cannot write policy: refused, nothing written.
    assert!(matches!(
        promote(&runtime, &first).await,
        Err(PolicyStoreError::Scope(_) | PolicyStoreError::Db(_))
    ));
    assert!(install(&runtime, Some(&first)).await.is_err());
    assert!(rows(&pg).await.is_empty());

    // 2. The policy role inserts both rows in one promotion.
    let report = promote(&policy, &first).await?;
    assert_eq!((report.inserted, report.updated), (2, 0));
    assert!(report.kept_unlisted.is_empty());
    let after = rows(&pg).await;
    assert_eq!(
        after
            .iter()
            .map(|r| (r.0.as_str(), r.1, r.2, r.3.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("acceptance_required", 0, true, "deploy-2026-10-28"),
            (
                "tolerate_authorization_failure",
                SELLER,
                true,
                "deploy-2026-10-28"
            ),
        ]
    );
    assert_eq!(
        pg.scalar("SELECT count(*) AS n FROM bss_orders__policy_election WHERE elected_at > now() - interval '1 minute'")
            .await?,
        2
    );

    // 3. The same promotion again only reads: the runtime role succeeds and nothing changes.
    let again = promote(&runtime, &first).await?;
    assert!(!again.changed(), "{again:?}");
    install(&runtime, Some(&first)).await?;
    assert_eq!(rows(&pg).await, after);

    // 4. A change keeps the row's identity and records the new change identity.
    let changed = PolicyPlan::from_config(&PolicyElectionsConfig {
        elected_by: "deploy-2".into(),
        ..config(
            switches(Some(true), None),
            &[(SELLER, switches(None, Some(false)))],
        )
    })?;
    let report = promote(&policy, &changed).await?;
    assert_eq!((report.inserted, report.updated), (0, 1));
    let later = rows(&pg).await;
    assert_eq!((later[1].2, later[1].3.as_str()), (false, "deploy-2"));
    assert_eq!(later[1].4, after[1].4);
    // The unchanged platform row keeps its change identity.
    assert_eq!(later[0], after[0]);

    // 5. A seller the configuration no longer names is kept and reported (warn-and-keep).
    let without = plan(switches(Some(true), None), &[]);
    for db in [&runtime, &policy] {
        let report = promote(db, &without).await?;
        assert!(!report.changed());
        assert_eq!(
            report.kept_unlisted,
            vec![(
                Election::TolerateAuthorizationFailure,
                PolicyScope::Seller(u(SELLER))
            )]
        );
    }
    install(&runtime, Some(&without)).await?;
    assert_eq!(rows(&pg).await, later);

    // 6. The read port: a stored row, no row, and a failed read.
    assert_eq!(
        election(
            &runtime,
            Election::AcceptanceRequired,
            PolicyScope::Platform
        )
        .await?,
        Some(true)
    );
    assert_eq!(
        election(
            &runtime,
            Election::TolerateAuthorizationFailure,
            PolicyScope::Seller(u(SELLER))
        )
        .await?,
        Some(false)
    );
    assert_eq!(
        election(
            &runtime,
            Election::TolerateAuthorizationFailure,
            PolicyScope::Platform
        )
        .await?,
        None
    );
    assert_eq!(
        election(
            &runtime,
            Election::TolerateAuthorizationFailure,
            PolicyScope::Seller(u(99))
        )
        .await?,
        None
    );
    pg.sql("REVOKE SELECT ON bss_orders__policy_election FROM bss_orders_runtime")
        .await?;
    assert!(matches!(
        election(
            &runtime,
            Election::AcceptanceRequired,
            PolicyScope::Platform
        )
        .await,
        Err(PolicyStoreError::Scope(_) | PolicyStoreError::Db(_))
    ));
    Ok(())
}

/// Two replicas booting with the same configuration both start: a promotion whose insert meets a
/// concurrent one's uncommitted row waits for it, then re-reads the committed row instead of
/// failing on the unique key (the platform row's NULL `scope_id` included: the key is
/// `NULLS NOT DISTINCT`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_promotion_waiting_behind_a_concurrent_one_sees_its_committed_election()
-> anyhow::Result<()> {
    use sea_orm::TransactionTrait;
    let pg = Pg::new().await?;
    let (policy, policy_raw) = pg.role("policy").await?;
    // Replica A: inserts the platform row, uncommitted.
    let a = policy_raw.begin().await?;
    a.execute_unprepared(&format!(
        "INSERT INTO bss_orders__policy_election VALUES('{}','acceptance_required','platform',NULL,true,'replica-a',clock_timestamp())",
        u(700)
    ))
    .await?;
    // Replica B: the same plan plus a seller row. Its reads cannot see A's row, so its offer
    // waits on A's key.
    let same = plan(
        switches(Some(true), None),
        &[(SELLER, switches(Some(false), None))],
    );
    let b = tokio::spawn({
        let policy = policy.clone();
        async move { promote(&policy, &same).await }
    });
    let mut waited = false;
    for _ in 0..200 {
        if pg
            .scalar("SELECT count(*) AS n FROM pg_stat_activity WHERE wait_event_type='Lock' AND usename='test_policy'")
            .await?
            > 0
        {
            waited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(waited, "replica B never waited on replica A's key");
    a.commit().await?;
    let report = b.await??;
    assert_eq!((report.inserted, report.updated), (1, 0), "{report:?}");
    let stored = rows(&pg).await;
    assert_eq!(stored.len(), 2);
    assert_eq!((stored[0].3.as_str(), stored[0].4), ("replica-a", u(700)));

    // A waiting promotion whose wanted value differs from the committed row changes it in place.
    let a = policy_raw.begin().await?;
    a.execute_unprepared(&format!(
        "INSERT INTO bss_orders__policy_election VALUES('{}','tolerate_authorization_failure','platform',NULL,true,'replica-a',clock_timestamp())",
        u(701)
    ))
    .await?;
    let differing = plan(
        switches(Some(true), Some(false)),
        &[(SELLER, switches(Some(false), None))],
    );
    let b = tokio::spawn({
        let policy = policy.clone();
        async move { promote(&policy, &differing).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    a.commit().await?;
    let report = b.await??;
    assert_eq!((report.inserted, report.updated), (0, 1), "{report:?}");
    assert_eq!(
        pg.scalar("SELECT count(*) AS n FROM bss_orders__policy_election WHERE policy_id = '00000000-0000-0000-0000-0000000002bd' AND NOT elected")
            .await?,
        1
    );
    Ok(())
}

/// Rows are undeletable and their key is fixed, even for the policy role.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn policy_rows_cannot_be_deleted_or_rekeyed() -> anyhow::Result<()> {
    let pg = Pg::new().await?;
    let (policy, policy_raw) = pg.role("policy").await?;
    promote(
        &policy,
        &plan(
            switches(Some(true), None),
            &[(SELLER, switches(None, Some(true)))],
        ),
    )
    .await?;
    let before = rows(&pg).await;
    for statement in [
        "DELETE FROM bss_orders__policy_election".to_owned(),
        "UPDATE bss_orders__policy_election SET election = 'tolerate_authorization_failure' WHERE scope = 'platform'".to_owned(),
        "UPDATE bss_orders__policy_election SET scope = 'seller', scope_id = gen_random_uuid() WHERE scope = 'platform'".to_owned(),
        format!(
            "UPDATE bss_orders__policy_election SET scope_id = '{}' WHERE scope = 'seller'",
            u(99)
        ),
        "UPDATE bss_orders__policy_election SET policy_id = gen_random_uuid()".to_owned(),
    ] {
        assert!(
            policy_raw.execute_unprepared(&statement).await.is_err(),
            "{statement}"
        );
    }
    assert_eq!(rows(&pg).await, before);
    Ok(())
}
