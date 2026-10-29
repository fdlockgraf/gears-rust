//! The consent requirement, resolved once (S4-08, D-251): submit, recording an acceptance and
//! begin-fulfillment all ask this resolver, so the precedence cannot drift between them.
//!
//! Requirement: the contract the order references, then the seller's `acceptance_required`
//! election, then the platform's, then unset = required. Tolerance of a failed authorization
//! (S4-10): the seller's election, then the platform's, then unset = false. An election is read
//! only when the earlier term is silent. A missing or failed read is `Unavailable`, never a
//! default.
use crate::domain::gate::answers::ContractQuery;
use crate::domain::gate::ports::PortAnswer;
use crate::domain::preconditions::{Requirement, acceptance_requirement, tolerate_failure};
use crate::infra::engine::PrepareError;
use crate::infra::gate::budget::RunBudget;
use crate::infra::gate::ports::ContractReads;
use crate::infra::gate::unavailable::ContractsUnavailable;
use crate::infra::policy::{self, Election, PolicyScope};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use toolkit_db::Db;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The parties and contract of the order whose requirement is resolved.
#[allow(
    clippy::struct_field_names,
    reason = "the names are the Contracts port's `ContractQuery` fields"
)]
#[derive(Debug, Clone, Copy)]
pub struct RequirementSubject {
    /// The contract the order references, if any.
    pub contract_id: Option<Uuid>,
    /// The paying tenant.
    pub payer_tenant_id: Uuid,
    /// The selling tenant.
    pub seller_tenant_id: Uuid,
}

/// Resolves the consent requirement and the authorization-failure tolerance.
pub struct RequirementResolver {
    contracts: Arc<dyn ContractReads>,
    policy: Option<Db>,
}

impl RequirementResolver {
    /// A resolver over the Contracts port and the policy election store; `None` answers
    /// unavailable whenever an election is read.
    #[must_use]
    pub fn new(contracts: Arc<dyn ContractReads>, policy: Option<Db>) -> Self {
        Self { contracts, policy }
    }

    /// A resolver for a caller that always holds the contract fact (submit's frozen one):
    /// the Contracts port is the fail-closed adapter and is never reached.
    #[must_use]
    pub fn from_fact(policy: Option<Db>) -> Self {
        Self::new(Arc::new(ContractsUnavailable), policy)
    }

    /// The live consent requirement. `contract` is `Some(fact)` when the caller already holds
    /// the contract's term (submit's frozen fact); `None` reads it live, and only when the order
    /// references a contract.
    ///
    /// # Errors
    /// `Unavailable`: the contract answer is missing, unavailable or lacks the contract, or an
    /// election read failed or has no store.
    #[allow(
        clippy::option_option,
        reason = "the Task 10 contract: `None` reads live, `Some(None)` is a silent held fact"
    )]
    pub async fn requirement(
        &self,
        ctx: &SecurityContext,
        order: &RequirementSubject,
        contract: Option<Option<bool>>,
        run: &RunBudget,
        cancel: &CancellationToken,
    ) -> Result<Requirement, PrepareError> {
        let fact = match contract {
            Some(fact) => fact,
            None => self.contract_term(ctx, order, run, cancel).await?,
        };
        self.requirement_for(order.seller_tenant_id, fact).await
    }

    /// The requirement given the contract's term already in hand.
    ///
    /// # Errors
    /// `Unavailable`: an election read failed or has no store.
    pub async fn requirement_for(
        &self,
        seller: Uuid,
        contract: Option<bool>,
    ) -> Result<Requirement, PrepareError> {
        if contract.is_some() {
            return Ok(acceptance_requirement(contract, None, None));
        }
        let (seller, platform) = self.elections(Election::AcceptanceRequired, seller).await?;
        Ok(acceptance_requirement(None, seller, platform))
    }

    /// Whether a failed payment authorization is tolerated: seller, then platform, unset false.
    ///
    /// # Errors
    /// `Unavailable`: an election read failed or has no store.
    pub async fn tolerate_failure(&self, seller: Uuid) -> Result<bool, PrepareError> {
        let (seller, platform) = self
            .elections(Election::TolerateAuthorizationFailure, seller)
            .await?;
        Ok(tolerate_failure(seller, platform))
    }

    async fn contract_term(
        &self,
        ctx: &SecurityContext,
        order: &RequirementSubject,
        run: &RunBudget,
        cancel: &CancellationToken,
    ) -> Result<Option<bool>, PrepareError> {
        let Some(contract_id) = order.contract_id else {
            return Ok(None);
        };
        let query = ContractQuery {
            contract_id,
            payer_tenant_id: order.payer_tenant_id,
            seller_tenant_id: order.seller_tenant_id,
        };
        match self.contracts.resolve(ctx, &[query], run, cancel).await {
            PortAnswer::Answered(facts) => facts
                .get(&contract_id)
                .map(|facts| facts.acceptance_required)
                .ok_or(PrepareError::Unavailable),
            PortAnswer::Skipped(_) | PortAnswer::Unavailable(_) => Err(PrepareError::Unavailable),
        }
    }

    /// The seller's election, and the platform's only when the seller's is silent.
    async fn elections(
        &self,
        election: Election,
        seller: Uuid,
    ) -> Result<(Option<bool>, Option<bool>), PrepareError> {
        let seller = self.election(election, PolicyScope::Seller(seller)).await?;
        let platform = match seller {
            Some(_) => None,
            None => self.election(election, PolicyScope::Platform).await?,
        };
        Ok((seller, platform))
    }

    /// One election; a failed read is `Unavailable` (never "no row").
    async fn election(
        &self,
        election: Election,
        scope: PolicyScope,
    ) -> Result<Option<bool>, PrepareError> {
        let Some(db) = &self.policy else {
            tracing::warn!(
                target: "orders.acceptance",
                "bss-orders: no policy election store is bound; the requirement is unavailable"
            );
            return Err(PrepareError::Unavailable);
        };
        policy::election(db, election, scope).await.map_err(|error| {
            tracing::warn!(
                target: "orders.acceptance",
                election = election.token(),
                scope = scope.token(),
                error = %error,
                "bss-orders: a policy election could not be read; the requirement is unavailable"
            );
            PrepareError::Unavailable
        })
    }
}

#[cfg(test)]
#[path = "acceptance_tests.rs"]
mod tests;
