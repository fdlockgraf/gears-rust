//! Storage row; never a public SDK model.
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;
/// Deployment-promoted policy election at platform or seller scope.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "bss_orders__policy_election")]
// `election` addresses the rows the policy channel governs without knowing their fresh
// `policy_id`s: promotion reads both elections whole (it must see rows the configuration no
// longer names), and the read port one election (D-250; D-207's `resource_tenant_id` analogue).
#[secure(
    no_tenant,
    resource_col = "policy_id",
    no_owner,
    no_type,
    pep_prop(election = "election")
)]
pub struct Model {
    /// Policy identity.
    #[sea_orm(primary_key, auto_increment = false)]
    pub policy_id: Uuid,
    /// `tolerate_authorization_failure` or `acceptance_required`.
    pub election: String,
    /// `platform` or `seller`.
    pub scope: String,
    /// Seller tenant for seller scope; NULL for platform scope.
    pub scope_id: Option<Uuid>,
    /// Whether the election is made.
    pub elected: bool,
    /// Change identity of the promotion that set the row.
    pub elected_by: String,
    /// Instant of that promotion.
    pub elected_at: TimeDateTimeWithTimeZone,
}
/// No `SeaORM` relations are declared for this table.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
