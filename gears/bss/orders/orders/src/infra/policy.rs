//! Policy-election channel (S4-07; DESIGN 02 §3.7 `orders_policy_election`, §3.8 policy
//! channel; D-207's pattern, D-250).
//!
//! - **Promotion** applies the deployment's `policy_elections` block at startup, in one
//!   transaction, through the secure repositories. It runs only when a stored election differs,
//!   so an unchanged promotion needs no write privilege. No endpoint writes the table.
//! - **Unlisted rows** are kept: rows are undeletable (database guard), so a stored election the
//!   configuration no longer names stands and is reported. Undoing an election means configuring
//!   its default value explicitly.
//! - **Reads** distinguish "no row" from a failed read: a caller never receives a default for a
//!   failed read.
use std::collections::BTreeMap;

use time::OffsetDateTime;
use toolkit_db::secure::ScopeError;
use toolkit_db::{Db, DbError};
use toolkit_security::access_scope::{AccessScope, ScopeConstraint, ScopeFilter};
use uuid::Uuid;

use crate::config::{PolicyElectionsConfig, PolicySwitches};
use crate::infra::dates::MAX_PROMOTED_ROWS;
use crate::infra::storage::entity::policy_election;
use crate::infra::storage::repo::{TransactionRunner, mutable, private};

/// One promotable election.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Election {
    /// The seller must accept before fulfillment (S4-08 consent).
    AcceptanceRequired,
    /// A failed payment authorization is tolerated at begin-fulfillment (S4-10 money guard).
    TolerateAuthorizationFailure,
}
impl Election {
    /// Every election the channel governs.
    pub const ALL: [Self; 2] = [Self::AcceptanceRequired, Self::TolerateAuthorizationFailure];

    /// The stored `election` token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AcceptanceRequired => "acceptance_required",
            Self::TolerateAuthorizationFailure => "tolerate_authorization_failure",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.token() == token)
    }
}

/// The scope one election row governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PolicyScope {
    /// The platform-wide election.
    Platform,
    /// One seller tenant's election.
    Seller(Uuid),
}
impl PolicyScope {
    /// The stored `scope` token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Seller(_) => "seller",
        }
    }

    /// The stored `scope_id`: the seller tenant, NULL for the platform.
    #[must_use]
    pub const fn scope_id(self) -> Option<Uuid> {
        match self {
            Self::Platform => None,
            Self::Seller(seller) => Some(seller),
        }
    }
}

/// A validated promotion: the change identity and every named election's wanted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyPlan {
    elected_by: String,
    wanted: BTreeMap<(Election, PolicyScope), bool>,
}
impl PolicyPlan {
    /// Validate the configured promotion.
    ///
    /// # Errors
    /// An empty change identity, a nil or duplicated seller, a seller naming no election, or
    /// more than [`MAX_PROMOTED_ROWS`] rows.
    pub fn from_config(config: &PolicyElectionsConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !config.elected_by.trim().is_empty(),
            "bss-orders: policy_elections.elected_by must name the change"
        );
        anyhow::ensure!(
            config.sellers.len() <= MAX_PROMOTED_ROWS,
            "bss-orders: policy_elections carries more than {MAX_PROMOTED_ROWS} rows"
        );
        let mut wanted = BTreeMap::new();
        add(&mut wanted, PolicyScope::Platform, config.platform);
        let mut sellers = std::collections::BTreeSet::new();
        for seller in &config.sellers {
            let id = seller.seller_tenant_id;
            anyhow::ensure!(
                !id.is_nil(),
                "bss-orders: policy_elections seller needs a seller tenant"
            );
            anyhow::ensure!(
                sellers.insert(id),
                "bss-orders: policy_elections names seller {id} twice"
            );
            anyhow::ensure!(
                add(&mut wanted, PolicyScope::Seller(id), seller.switches),
                "bss-orders: policy_elections seller {id} names no election"
            );
        }
        anyhow::ensure!(
            wanted.len() <= MAX_PROMOTED_ROWS,
            "bss-orders: policy_elections carries more than {MAX_PROMOTED_ROWS} rows"
        );
        Ok(Self {
            elected_by: config.elected_by.clone(),
            wanted,
        })
    }

    /// The change identity recorded as `elected_by`.
    #[must_use]
    pub fn elected_by(&self) -> &str {
        &self.elected_by
    }

    /// Every named election and its wanted value.
    #[must_use]
    pub const fn wanted(&self) -> &BTreeMap<(Election, PolicyScope), bool> {
        &self.wanted
    }
}

/// Add the switches `scope` names; `false` when it names none.
fn add(
    wanted: &mut BTreeMap<(Election, PolicyScope), bool>,
    scope: PolicyScope,
    switches: PolicySwitches,
) -> bool {
    let named = [
        (Election::AcceptanceRequired, switches.acceptance_required),
        (
            Election::TolerateAuthorizationFailure,
            switches.tolerate_authorization_failure,
        ),
    ];
    let mut any = false;
    for (election, value) in named {
        if let Some(value) = value {
            wanted.insert((election, scope), value);
            any = true;
        }
    }
    any
}

/// The rows of `elections`, addressed by the `election` PEP property (D-250).
fn channel_scope(elections: &[Election]) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::r#in(
        "election",
        elections.iter().map(|e| e.token().into()).collect(),
    )]))
}

/// What one promotion changed, and the stored rows it left unnamed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotionReport {
    /// Elections inserted.
    pub inserted: usize,
    /// Existing elections whose value changed.
    pub updated: usize,
    /// Stored elections the configuration does not name: kept, never deleted.
    pub kept_unlisted: Vec<(Election, PolicyScope)>,
}
impl PromotionReport {
    /// Whether the promotion wrote anything.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.inserted + self.updated > 0
    }
}

/// Policy-election store failure.
#[derive(Debug, thiserror::Error)]
pub enum PolicyStoreError {
    /// A stored row outside the channel's vocabulary (the database checks forbid it).
    #[error("policy election store: unrecognised stored row")]
    Invalid,
    /// A scoped store operation failed.
    #[error("policy election store: {0}")]
    Scope(#[from] ScopeError),
    /// A database failure.
    #[error("policy election store: {0}")]
    Db(#[from] DbError),
}

fn key(row: &policy_election::Model) -> Result<(Election, PolicyScope), PolicyStoreError> {
    let election = Election::from_token(&row.election).ok_or(PolicyStoreError::Invalid)?;
    let scope = match (row.scope.as_str(), row.scope_id) {
        ("platform", None) => PolicyScope::Platform,
        ("seller", Some(seller)) => PolicyScope::Seller(seller),
        _ => return Err(PolicyStoreError::Invalid),
    };
    Ok((election, scope))
}

type Stored = BTreeMap<(Election, PolicyScope), policy_election::Model>;

fn index(rows: Vec<policy_election::Model>) -> Result<Stored, PolicyStoreError> {
    rows.into_iter().map(|r| Ok((key(&r)?, r))).collect()
}

fn unlisted(plan: &PolicyPlan, stored: &Stored) -> Vec<(Election, PolicyScope)> {
    stored
        .keys()
        .filter(|k| !plan.wanted().contains_key(k))
        .copied()
        .collect()
}

fn differs(plan: &PolicyPlan, stored: &Stored) -> bool {
    plan.wanted()
        .iter()
        .any(|(k, v)| stored.get(k).is_none_or(|r| r.elected != *v))
}

/// Apply `plan` through the policy channel. An unchanged plan reads only, so a connection with
/// the runtime role's read grant suffices. Otherwise one transaction locks the stored rows,
/// inserts each missing election with a fresh identity and changes each differing one in
/// place, stamping `elected_by` and `elected_at`. A failure writes nothing. Stored rows the plan
/// does not name are kept and reported.
///
/// Two replicas promoting at once both start: a missing election is offered with
/// `ON CONFLICT DO NOTHING` on its unique key, which waits for a concurrent promotion's insert
/// and then re-reads the committed row instead of failing on a duplicate.
///
/// # Errors
/// A store failure or a refused write (for example a connection without the policy role's
/// privileges).
pub async fn promote(db: &Db, plan: &PolicyPlan) -> Result<PromotionReport, PolicyStoreError> {
    let scope = channel_scope(&Election::ALL);
    let stored = index(private::policy_election_rows(&db.conn()?, &scope, false).await?)?;
    if !differs(plan, &stored) {
        return Ok(PromotionReport {
            kept_unlisted: unlisted(plan, &stored),
            ..PromotionReport::default()
        });
    }
    let plan = plan.clone();
    db.transaction_ref_mapped(move |tx| Box::pin(async move { apply(tx, &scope, &plan).await }))
        .await
}

async fn apply(
    tx: &impl TransactionRunner,
    scope: &AccessScope,
    plan: &PolicyPlan,
) -> Result<PromotionReport, PolicyStoreError> {
    let landed = OffsetDateTime::now_utc();
    let stored = index(private::policy_election_rows(tx, scope, true).await?)?;
    let mut report = PromotionReport {
        kept_unlisted: unlisted(plan, &stored),
        ..PromotionReport::default()
    };
    for (&(election, at), &elected) in plan.wanted() {
        let current = if let Some(row) = stored.get(&(election, at)) {
            row.clone()
        } else {
            let row = policy_election::Model {
                policy_id: Uuid::new_v4(),
                election: election.token().to_owned(),
                scope: at.token().to_owned(),
                scope_id: at.scope_id(),
                elected,
                elected_by: plan.elected_by.clone(),
                elected_at: landed,
            };
            if private::offer_policy_election(tx, scope, row).await? {
                report.inserted += 1;
                continue;
            }
            // A concurrent promotion committed this key while the offer waited on it; this
            // statement sees its row.
            private::policy_election_row(
                tx,
                scope,
                election.token(),
                at.token(),
                at.scope_id(),
                true,
            )
            .await?
            .ok_or(ScopeError::Denied("hidden record"))?
        };
        if current.elected == elected {
            continue;
        }
        let proposed = policy_election::Model {
            elected,
            elected_by: plan.elected_by.clone(),
            elected_at: landed,
            ..current.clone()
        };
        mutable::replace_policy_election(tx, scope, &current, proposed).await?;
        report.updated += 1;
    }
    Ok(report)
}

/// Install the configured promotion at startup. A stored election the configuration does not
/// name is kept and warned about.
///
/// # Errors
/// Any promotion failure: the gear refuses to start.
pub async fn install(db: &Db, plan: Option<&PolicyPlan>) -> anyhow::Result<()> {
    let Some(plan) = plan else {
        return Ok(());
    };
    let report = promote(db, plan)
        .await
        .map_err(|e| anyhow::anyhow!("bss-orders: policy election promotion: {e}"))?;
    if report.changed() {
        tracing::info!(
            inserted = report.inserted,
            updated = report.updated,
            elected_by = plan.elected_by(),
            "bss-orders: policy elections promoted"
        );
    }
    for (election, scope) in &report.kept_unlisted {
        tracing::warn!(
            election = election.token(),
            scope = scope.token(),
            seller_tenant_id = ?scope.scope_id(),
            "bss-orders: a stored policy election is not in policy_elections; it is kept (rows \
             are undeletable): configure its default value to undo it"
        );
    }
    Ok(())
}

/// The stored value of one election at one scope.
///
/// `Ok(None)` means no row: the election is unset at that scope. A failed read is an error,
/// never a default.
///
/// # Errors
/// No connection, or a failed scoped read.
pub async fn election(
    db: &Db,
    election: Election,
    scope: PolicyScope,
) -> Result<Option<bool>, PolicyStoreError> {
    let row = private::policy_election_row(
        &db.conn()?,
        &channel_scope(&[election]),
        election.token(),
        scope.token(),
        scope.scope_id(),
        false,
    )
    .await?;
    Ok(row.map(|r| r.elected))
}
