//! The consent-requirement resolver without a database: the contract term and the failure
//! rules. The election precedence on real PostgreSQL is in `storage/tests/policy.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::domain::gate::answers::{ContractFacts, ContractStatus};
use crate::domain::gate::ports::Unavailable;
use crate::domain::preconditions::RequirementSource;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// A double of the Contracts port answering a fixed outcome; it records every query set.
struct ContractsDouble {
    answer: fn() -> PortAnswer<BTreeMap<Uuid, ContractFacts>>,
    calls: Mutex<Vec<Vec<ContractQuery>>>,
}

#[async_trait::async_trait]
impl ContractReads for ContractsDouble {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        queries: &[ContractQuery],
        _run: &RunBudget,
        _cancel: &CancellationToken,
    ) -> PortAnswer<BTreeMap<Uuid, ContractFacts>> {
        self.calls.lock().unwrap().push(queries.to_vec());
        (self.answer)()
    }
}

const CONTRACT: Uuid = Uuid::from_u128(0xC);

fn facts(required: Option<bool>) -> ContractFacts {
    ContractFacts {
        status: ContractStatus::Active,
        payer_eligible: true,
        effective_at: None,
        acceptance_required: required,
        version: "v1".into(),
    }
}
fn answered(required: Option<bool>) -> PortAnswer<BTreeMap<Uuid, ContractFacts>> {
    PortAnswer::Answered(BTreeMap::from([(CONTRACT, facts(required))]))
}
fn resolver(
    answer: fn() -> PortAnswer<BTreeMap<Uuid, ContractFacts>>,
) -> (RequirementResolver, Arc<ContractsDouble>) {
    let double = Arc::new(ContractsDouble {
        answer,
        calls: Mutex::new(Vec::new()),
    });
    (RequirementResolver::new(double.clone(), None), double)
}
fn subject(contract: bool) -> RequirementSubject {
    RequirementSubject {
        contract_id: contract.then_some(CONTRACT),
        payer_tenant_id: Uuid::from_u128(1),
        seller_tenant_id: Uuid::from_u128(2),
    }
}
fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(3))
        .subject_tenant_id(Uuid::from_u128(1))
        .build()
        .unwrap()
}
async fn live(
    r: &RequirementResolver,
    s: &RequirementSubject,
) -> Result<Requirement, PrepareError> {
    r.requirement(
        &ctx(),
        s,
        None,
        &RunBudget::new(),
        &CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn a_live_contract_term_decides_without_any_election() {
    let (r, double) = resolver(|| answered(Some(false)));
    // No policy store is bound: a stated contract term never reads one.
    assert_eq!(live(&r, &subject(true)).await, Ok(Requirement::NotRequired));
    assert_eq!(double.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_contract_answer_that_is_not_an_answer_is_unavailable() {
    for answer in [
        (|| PortAnswer::Unavailable(Unavailable::Missing)) as fn() -> _,
        || PortAnswer::Unavailable(Unavailable::Outage),
        || PortAnswer::Answered(BTreeMap::new()),
    ] {
        let (r, _) = resolver(answer);
        assert_eq!(
            live(&r, &subject(true)).await,
            Err(PrepareError::Unavailable)
        );
    }
}

#[tokio::test]
async fn a_contract_less_order_never_reads_the_contracts_port() {
    let (r, double) = resolver(|| answered(Some(true)));
    // Silent contract term, absent store: the election read fails closed.
    assert_eq!(
        live(&r, &subject(false)).await,
        Err(PrepareError::Unavailable)
    );
    assert!(double.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_supplied_contract_fact_is_never_read_live() {
    let (r, double) = resolver(|| PortAnswer::Unavailable(Unavailable::Missing));
    let got = r
        .requirement(
            &ctx(),
            &subject(true),
            Some(Some(true)),
            &RunBudget::new(),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(got, Ok(Requirement::Required(RequirementSource::Contract)));
    assert!(double.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_absent_policy_store_is_unavailable_never_a_default() {
    let (r, _) = resolver(|| answered(None));
    assert_eq!(
        live(&r, &subject(true)).await,
        Err(PrepareError::Unavailable)
    );
    assert_eq!(
        r.tolerate_failure(Uuid::from_u128(2)).await,
        Err(PrepareError::Unavailable)
    );
}
