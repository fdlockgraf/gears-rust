//! Recording the customer's consent after submit on real PostgreSQL (S4-08b; 05 *Record
//! Acceptance*; Stage 4 DESIGN §5 "Standalone consent"; D-255): the record-acceptance service
//! over the submit fixture's engine, gate stubs and Pricing double, under a rules PDP naming a
//! buyer, a customer, a partner, a plain member and a seller operator. Every recorded consent is
//! read back through `ReadService::get_acceptance` (S4-06), never by SQL. Compiled only with
//! `gate-dev-stubs`.
use super::amend::change_quantity;
use super::capture::{BUYER, create_meta, key, s, view, write};
use super::submit::{
    FailAt, Fixture, ORDER_RT, RESOURCE, SELLER, contract_fixture, elect, last_event, line,
    setup_with, submit_with,
};
use super::*;
use crate::api::rest::acceptance::acceptance_body;
use crate::authz::Caller;
use crate::authz::test_pdp::{RulesProvider, user, with_proof};
use crate::domain::gate::answers::{ContractFacts, ContractQuery};
use crate::domain::gate::ports::PortAnswer;
use crate::infra::acceptance::{AcceptanceService, RequirementResolver};
use crate::infra::amend::AmendService;
use crate::infra::capture::{CaptureService, CaptureSettings, Settled, SettledOutcome};
use crate::infra::engine::{Engine, FaultPoint};
use crate::infra::gate::budget::RunBudget;
use crate::infra::gate::ports::ContractReads;
use crate::infra::gate::stubs::{ContractsStub, StubFixture};
use crate::infra::gate::submit::SubmitService;
use crate::infra::gate::tests::orchestrate::{CONTRACT, PAYER, fixture};
use crate::infra::gate::unavailable::ContractsUnavailable;
use crate::infra::read::ReadService;
use crate::infra::throttle::{CallerLimitSettings, CallerLimiter, PerCallerLimiter};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use bss_orders_sdk::OrdersError;
use bss_orders_sdk::acceptance::Acceptance;
use bss_orders_sdk::amend::AmendOrder;
use bss_orders_sdk::authoring::{Category, CreateOrder, OrderView, SalesPath};
use bss_orders_sdk::catalog::Reason;
use bss_orders_sdk::models::{CallMeta, OrderVersion};
use bss_orders_sdk::reads::{AcceptanceList, AcceptancePage, AcceptanceRecord, RequirementSource};
use bss_pricing_sdk::acceptance::SellabilityV1;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

const ACCEPTANCE_RT: &str = "gts.cf.bss.orders.acceptance.v1~";
/// The resource tenant's customer: may read and record consent, never authored the order.
const CUSTOMER: (u128, u128) = (110, 10);
/// A partner of another tenant placing orders for the resource tenant.
const PARTNER: (u128, u128) = (111, 11);
/// A resource-tenant member with `order × read` only: no `acceptance × record` grant.
const MEMBER: (u128, u128) = (112, 10);
/// A seller operator: reads the seller's orders, never records the customer's consent.
const SELLER_OPERATOR: (u128, u128) = (113, SELLER);

fn rule(id: &str, subject: u128, rt: &str, actions: &[&str], property: &str, value: Uuid) -> Value {
    json!({"id": id, "subject": {"id": s(u(subject))}, "resource_type": rt, "actions": actions,
        "paths": [{"predicates": [{"property": property, "values": [s(value)]}]}]})
}

/// The buyer authors, submits, amends and records on resource tenant 10; the partner does the
/// same for that tenant (the bar, not the PDP, refuses its recording); the customer records and
/// reads; the member and the seller operator only read.
fn policy() -> Value {
    let author = ["create", "write", "edit", "read", "submit", "amend"];
    let resource = u(RESOURCE);
    json!({"vendor": "constructorfabric", "priority": 10, "policy_revision": "s4-08b",
    "rules": [
        rule("buyer", BUYER.0, ORDER_RT, &author, "resource_tenant_id", resource),
        rule("buyer-consent", BUYER.0, ACCEPTANCE_RT, &["record"], "resource_tenant_id", resource),
        rule("partner", PARTNER.0, ORDER_RT, &author, "resource_tenant_id", resource),
        rule("partner-consent", PARTNER.0, ACCEPTANCE_RT, &["record"], "resource_tenant_id",
            resource),
        rule("customer", CUSTOMER.0, ORDER_RT, &["read"], "resource_tenant_id", resource),
        rule("customer-consent", CUSTOMER.0, ACCEPTANCE_RT, &["record"], "resource_tenant_id",
            resource),
        rule("member", MEMBER.0, ORDER_RT, &["read"], "resource_tenant_id", resource),
        rule("seller", SELLER_OPERATOR.0, ORDER_RT, &["read"], "seller_tenant_id", u(SELLER)),
    ]})
}

fn caller(who: (u128, u128)) -> Caller {
    user(who.0, who.1)
}
fn buyer() -> Caller {
    caller(BUYER)
}
fn customer() -> Caller {
    caller(CUSTOMER)
}
fn partner() -> Caller {
    with_proof(&caller(PARTNER), "proof-partner")
}

/// The fixture's engine, gate, dates, authority and Pricing double under [`policy`].
struct World {
    f: Fixture,
    stubs: StubFixture,
    engine: Arc<Engine>,
    capture: CaptureService,
    submit: Arc<SubmitService>,
    reads: ReadService,
}

fn slot(engine: &Arc<Engine>) -> Arc<ArcSwapOption<Engine>> {
    Arc::new(ArcSwapOption::from(Some(Arc::clone(engine))))
}

async fn world(stubs: StubFixture) -> World {
    let f = Box::pin(setup_with(stubs.clone(), 600, true)).await;
    let pdp: Arc<dyn AuthZResolverApi> = RulesProvider::from_policy(policy());
    let engine = Arc::new(f.t.engine(Arc::clone(&pdp)));
    let capture = CaptureService::new(slot(&engine), CaptureSettings { line_cap: 200 });
    let reads = f.t.reads_with(Arc::clone(&pdp));
    let port: Arc<dyn SellabilityV1> = f.acceptance.clone();
    let submit = Arc::new(
        SubmitService::new(
            slot(&engine),
            Arc::new(f.t.reads_with(pdp)),
            f.h.orchestrator.clone(),
            f.dates.clone(),
            f.authority.clone(),
            PerCallerLimiter::new(
                CallerLimiter::Submit,
                CallerLimitSettings {
                    per_minute: NonZeroU32::new(600).unwrap(),
                    max_keys: 100,
                },
            ),
            200,
        )
        .with_policy_store(f.t.env.db.clone())
        .with_acceptance(Some(port)),
    );
    World {
        f,
        stubs,
        engine,
        capture,
        submit,
        reads,
    }
}

impl World {
    /// The record-acceptance service over this world's engine and `contracts`.
    fn recorder_with(
        &self,
        engine: Arc<Engine>,
        contracts: Arc<dyn ContractReads>,
    ) -> AcceptanceService {
        AcceptanceService::new(
            slot(&engine),
            RequirementResolver::new(contracts, Some(self.f.t.env.db.clone())),
        )
    }
    /// The record-acceptance service reading the world's stubbed contracts.
    fn recorder(&self) -> AcceptanceService {
        self.recorder_with(
            Arc::clone(&self.engine),
            Arc::new(ContractsStub::new(Arc::new(self.stubs.clone()))),
        )
    }
    fn amender(&self) -> AmendService {
        AmendService::new(Arc::clone(&self.submit))
    }

    /// A one-line draft created by `author` (on `contract`), as `author` reads it.
    async fn draft(&self, author: &Caller, contract: Option<Uuid>, tag: &str) -> OrderView {
        let order = CreateOrder {
            resource_tenant_id: u(RESOURCE),
            payer_tenant_id: PAYER,
            seller_tenant_id: u(SELLER),
            category: Category::NewSale,
            contract_id: contract,
        };
        let created = view(
            self.capture
                .create(author, order, &create_meta(&format!("{tag}-create")))
                .await
                .unwrap(),
        );
        let id = created.order.order_id;
        self.capture
            .add_line(
                author,
                id,
                line(),
                &write(&format!("{tag}-line"), 1, Some(0)),
            )
            .await
            .unwrap();
        self.reads.get(author, id).await.unwrap()
    }

    /// A submitted order: drafted by `author`, submitted by `submitter`; its id and version N.
    async fn submitted(
        &self,
        author: &Caller,
        submitter: &Caller,
        contract: Option<Uuid>,
        tag: &str,
    ) -> (Uuid, i64) {
        let v = self.draft(author, contract, tag).await;
        let settled = submit_with(&self.submit, submitter, &v, &format!("{tag}-submit"))
            .await
            .unwrap();
        assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
        (
            v.order.order_id,
            settled.response.body()["version"].as_i64().unwrap(),
        )
    }

    /// Release `order`'s overlap claims: every order of the fixture claims the same tuple.
    async fn release(&self, order: Uuid) {
        self.f
            .t
            .env
            .pg
            .sql(&format!(
                "UPDATE bss_orders__inflight_overlap_claim SET released_at = now() \
                 WHERE order_id='{order}' AND released_at IS NULL"
            ))
            .await
            .unwrap();
    }

    async fn consent(&self, who: &Caller, order: Uuid) -> AcceptancePage {
        self.reads
            .get_acceptance(who, order, &AcceptanceList::default())
            .await
            .unwrap()
    }

    /// `[state, current_version, state_entered_at, versions]` of `order`: row 25 changes none.
    async fn shape(&self, order: Uuid) -> Value {
        self.f
            .t
            .json(&format!(
                "SELECT jsonb_build_array(o.state, o.current_version, o.state_entered_at, \
                 (SELECT count(*) FROM bss_orders__order_version v WHERE v.order_id=o.order_id)) \
                 AS j FROM bss_orders__order o WHERE order_id='{order}'"
            ))
            .await
    }

    /// The engine's transition time of the committed record-acceptance audit entry.
    async fn recorded_at(&self, order: Uuid) -> OffsetDateTime {
        let t = self
            .f
            .t
            .json(&format!(
                "SELECT to_jsonb(created_at) AS j FROM bss_orders__transition_audit \
                 WHERE order_id='{order}' AND trigger='record-acceptance' AND outcome='committed'"
            ))
            .await;
        parse(&t)
    }

    async fn rows(&self, order: Uuid) -> i64 {
        self.f
            .t
            .n(&format!(
                "SELECT count(*) AS n FROM bss_orders__acceptance WHERE order_id='{order}'"
            ))
            .await
    }
}

fn parse(value: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(
        value.as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
}

fn meta(text: &str, version: i64) -> CallMeta {
    CallMeta {
        expected_version: OrderVersion::try_from(version).unwrap(),
        idempotency_key: key(text),
        correlation_id: None,
        delegation_proof_ref: None,
    }
}

async fn record(
    svc: &AcceptanceService,
    who: &Caller,
    order: Uuid,
    version: i64,
    text: &str,
) -> Result<Settled, OrdersError> {
    svc.record(
        who,
        order,
        Acceptance,
        &meta(text, version),
        &CancellationToken::new(),
    )
    .await
}

/// The refusal of an attempt, settled or not.
fn refusal(answer: &Result<Settled, OrdersError>) -> Reason {
    match answer {
        Ok(Settled {
            outcome: SettledOutcome::Refused(Some(reason)),
            ..
        })
        | Err(OrdersError::Refused(reason)) => *reason,
        other => panic!("not a refusal: {other:?}"),
    }
}

fn record_of(
    version: i64,
    at: OffsetDateTime,
    by: (u128, u128),
    path: SalesPath,
    source: RequirementSource,
) -> AcceptanceRecord {
    AcceptanceRecord {
        accepted_version: OrderVersion::try_from(version).unwrap(),
        accepted_at: at,
        recorded_by: u(by.0).to_string(),
        recording_path: path,
        requirement_source: source,
    }
}

/// A contract port that runs `before` ahead of every read (a race between the preparation's
/// live requirement read and the engine's lock), then answers from `inner`.
type Before = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
struct Hooked {
    inner: Arc<dyn ContractReads>,
    before: Before,
    calls: AtomicUsize,
}
#[async_trait]
impl ContractReads for Hooked {
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        queries: &[ContractQuery],
        run: &RunBudget,
        cancel: &CancellationToken,
    ) -> PortAnswer<BTreeMap<Uuid, ContractFacts>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.before)().await;
        self.inner.resolve(ctx, queries, run, cancel).await
    }
}

/// D-255, the S4-08 matrix head: on a partner-placed order the partner who placed it is barred
/// (403, nothing recorded), the eligible customer records the current version (`partner_placed`,
/// the customer's reference, the engine's transition time, the requirement's source) in one
/// transaction with its audit entry and exactly one `OrderAcceptanceRecorded`, and nothing else
/// changes. A same-key replay returns the stored result; afterwards an independent attempt is
/// `acceptance-already-recorded`, even by the barred partner (already-recorded beats barred).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn eligible_customer_records_on_a_partner_placed_order_and_the_partner_is_barred() {
    let w = world(fixture()).await;
    let svc = w.recorder();
    let (order, n) = w.submitted(&partner(), &partner(), None, "rec-pp").await;
    assert_eq!(w.consent(&customer(), order).await.current_acceptance, None);
    let shape = w.shape(order).await;
    let (outbox, audits) = (w.f.t.outbox().await, w.f.t.audits(order).await);

    let barred = record(&svc, &partner(), order, n, "rec-pp-partner").await;
    assert_eq!(refusal(&barred), Reason::AcceptanceRecordingPartyBarred);
    assert_eq!(barred.unwrap().response.status(), 403);
    assert_eq!(w.rows(order).await, 0);
    assert_eq!(w.f.t.outbox().await, outbox, "a refusal emits nothing");
    assert_eq!(
        w.f.t.audits(order).await,
        audits + 1,
        "the refusal is audited"
    );

    let settled = record(&svc, &customer(), order, n, "rec-pp-customer")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert_eq!(settled.response.body()["version"], json!(n));
    assert_eq!(w.shape(order).await, shape, "no state or version change");
    assert_eq!(w.f.t.outbox().await, outbox + 1, "exactly one event");
    assert_eq!(w.f.t.audits(order).await, audits + 2);
    let t = w.recorded_at(order).await;
    let page = w.consent(&customer(), order).await;
    assert_eq!(
        page.current_acceptance,
        Some(record_of(
            n,
            t,
            CUSTOMER,
            SalesPath::PartnerPlaced,
            RequirementSource::PlatformDefault
        ))
    );
    let event = last_event(&w.f).await;
    assert_eq!(event["acceptedVersion"], json!(n), "{event}");
    assert_eq!(
        parse(&event["acceptedAt"]),
        t,
        "the event carries the stored instant"
    );
    assert_eq!(event["recordingActor"], json!(u(CUSTOMER.0).to_string()));
    assert_eq!(event["requirementSource"], json!("platform_default"));

    let replay = record(&svc, &customer(), order, n, "rec-pp-customer")
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.response, settled.response);
    assert_eq!(w.f.t.outbox().await, outbox + 1, "a replay emits nothing");

    for (who, text) in [
        (partner(), "rec-pp-partner-2"),
        (customer(), "rec-pp-customer-2"),
        (buyer(), "rec-pp-buyer"),
    ] {
        let again = record(&svc, &who, order, n, text).await;
        assert_eq!(refusal(&again), Reason::AcceptanceAlreadyRecorded, "{text}");
        assert_eq!(again.unwrap().response.status(), 409);
    }
    assert_eq!(w.rows(order).await, 1);
    assert_eq!(w.f.t.outbox().await, outbox + 1);
}

/// Delegation introduced after creation: a self-service order created by the customer and
/// submitted by a delegate of another tenant. The delegate is barred (its submit role is from
/// another tenant); the creator is not barred because a delegate submitted, and records it as
/// `self_service` (the stored sales path).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delegation_introduced_after_creation_bars_the_delegate_only() {
    let w = world(fixture()).await;
    let svc = w.recorder();
    let (order, n) = w.submitted(&buyer(), &partner(), None, "rec-dl").await;
    assert_eq!(w.consent(&buyer(), order).await.current_acceptance, None);
    let barred = record(&svc, &partner(), order, n, "rec-dl-partner").await;
    assert_eq!(refusal(&barred), Reason::AcceptanceRecordingPartyBarred);
    let settled = record(&svc, &buyer(), order, n, "rec-dl-buyer")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let t = w.recorded_at(order).await;
    assert_eq!(
        w.consent(&buyer(), order).await.current_acceptance,
        Some(record_of(
            n,
            t,
            BUYER,
            SalesPath::SelfService,
            RequirementSource::PlatformDefault
        ))
    );
}

/// A delegation proof on an own-tenant submit suppresses the automatic consent (D-146), so the
/// customer records it by this POST; its own creator and submitter roles are on the resource
/// tenant of a self-service order, so it is not barred.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn proof_on_an_own_tenant_submit_leaves_consent_to_the_post() {
    let w = world(fixture()).await;
    let svc = w.recorder();
    let proof = with_proof(&buyer(), "proof-own");
    let (order, n) = w.submitted(&buyer(), &proof, None, "rec-proof").await;
    assert_eq!(w.consent(&buyer(), order).await.current_acceptance, None);
    let settled = record(&svc, &buyer(), order, n, "rec-proof-1")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let t = w.recorded_at(order).await;
    assert_eq!(
        w.consent(&buyer(), order).await.current_acceptance,
        Some(record_of(
            n,
            t,
            BUYER,
            SalesPath::SelfService,
            RequirementSource::PlatformDefault
        ))
    );
}

/// Consent is per version: after an amendment the old consent is a prior record that cannot
/// satisfy M; a recording naming N is a stale `If-Match` (412); the original customer, who is
/// also M's amender on its own tenant, re-accepts M.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_original_customer_re_accepts_m_and_consent_for_n_cannot_satisfy_m() {
    let w = world(fixture()).await;
    let svc = w.recorder();
    let v = w.draft(&buyer(), None, "rec-am").await;
    let order = v.order.order_id;
    let submitted = submit_with(&w.submit, &buyer(), &v, "rec-am-submit")
        .await
        .unwrap();
    let n = submitted.response.body()["version"].as_i64().unwrap();
    let auto = w.consent(&buyer(), order).await.current_acceptance.unwrap();
    assert_eq!(auto.accepted_version, OrderVersion::try_from(n).unwrap());
    let request: AmendOrder =
        serde_json::from_value(change_quantity(v.lines[0].line_id, "2")).unwrap();
    let amended = w
        .amender()
        .amend(
            &buyer(),
            order,
            request,
            &meta("rec-am-amend", n),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(amended.outcome, SettledOutcome::Committed, "{amended:?}");
    let m = amended.response.body()["version"].as_i64().unwrap();
    let page = w.consent(&buyer(), order).await;
    assert_eq!(
        page.current_version,
        Some(OrderVersion::try_from(m).unwrap())
    );
    assert_eq!(page.current_acceptance, None, "M starts without consent");
    assert_eq!(
        page.items,
        vec![auto.clone()],
        "N's consent is a prior record"
    );

    let stale = record(&svc, &buyer(), order, n, "rec-am-n").await.unwrap();
    assert_eq!(
        stale.outcome,
        SettledOutcome::Refused(Some(Reason::PreconditionFailed)),
        "{stale:?}"
    );
    assert_eq!(
        stale.response.status(),
        412,
        "a stale If-Match is 412 (D-212)"
    );
    assert_eq!(
        stale.response.body()["context"]["data"]["current_version"],
        json!(m)
    );
    assert_eq!(w.rows(order).await, 1);

    let settled = record(&svc, &buyer(), order, m, "rec-am-m").await.unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let t = w.recorded_at(order).await;
    let page = w.consent(&buyer(), order).await;
    assert_eq!(
        page.current_acceptance,
        Some(record_of(
            m,
            t,
            BUYER,
            SalesPath::SelfService,
            RequirementSource::PlatformDefault
        ))
    );
    assert_eq!(page.items, vec![auto]);
}

/// An amendment that commits M between the recording's snapshot (its live contract read) and
/// the engine's lock: the consent naming N is refused by the final version check and binds to
/// no version; a fresh recording of M commits on M only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn an_amendment_racing_the_recording_never_receives_consent_meant_for_n() {
    let w = Arc::new(world(contract_fixture(Some(true))).await);
    let proof = with_proof(&buyer(), "proof-race");
    let v = w.draft(&buyer(), Some(CONTRACT), "rec-race").await;
    let order = v.order.order_id;
    let submitted = submit_with(&w.submit, &proof, &v, "rec-race-submit")
        .await
        .unwrap();
    let n = submitted.response.body()["version"].as_i64().unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let m = Arc::new(parking_lot::Mutex::new(None));
    let before: Before = {
        let (w, v, fired, m) = (
            Arc::clone(&w),
            v.clone(),
            Arc::clone(&fired),
            Arc::clone(&m),
        );
        Arc::new(move || {
            let (w, v, fired, m) = (
                Arc::clone(&w),
                v.clone(),
                Arc::clone(&fired),
                Arc::clone(&m),
            );
            Box::pin(async move {
                if fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                let request: AmendOrder =
                    serde_json::from_value(change_quantity(v.lines[0].line_id, "2")).unwrap();
                let amended = w
                    .amender()
                    .amend(
                        &buyer(),
                        v.order.order_id,
                        request,
                        &meta("rec-race-amend", n),
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(amended.outcome, SettledOutcome::Committed, "{amended:?}");
                *m.lock() = amended.response.body()["version"].as_i64();
            })
        })
    };
    let hooked = Arc::new(Hooked {
        inner: Arc::new(ContractsStub::new(Arc::new(w.stubs.clone()))),
        before,
        calls: AtomicUsize::new(0),
    });
    let svc = w.recorder_with(Arc::clone(&w.engine), hooked.clone());
    let raced = record(&svc, &buyer(), order, n, "rec-race-n").await;
    assert!(
        fired.load(Ordering::SeqCst),
        "the amendment ran inside the preparation"
    );
    assert_eq!(refusal(&raced), Reason::PreconditionFailed);
    assert_eq!(w.rows(order).await, 0, "no version received the consent");
    let m = m.lock().unwrap();
    assert!(m > n);
    let settled = record(&svc, &buyer(), order, m, "rec-race-m")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    let page = w.consent(&buyer(), order).await;
    let current = page.current_acceptance.unwrap();
    assert_eq!(current.accepted_version, OrderVersion::try_from(m).unwrap());
    assert_eq!(current.requirement_source, RequirementSource::Contract);
    assert!(page.items.is_empty(), "N has no consent");
}

/// Two independent recordings of one version, both prepared before either takes the lock: the
/// engine's locked read admits the first and refuses the second `acceptance-already-recorded`
/// (never a key violation); one row, one event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn duplicate_recordings_have_one_winner() {
    let w = world(contract_fixture(Some(false))).await;
    let proof = with_proof(&buyer(), "proof-dup");
    let (order, n) = w
        .submitted(&buyer(), &proof, Some(CONTRACT), "rec-dup")
        .await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let before: Before = {
        let barrier = Arc::clone(&barrier);
        Arc::new(move || {
            let barrier = Arc::clone(&barrier);
            Box::pin(async move {
                barrier.wait().await;
            })
        })
    };
    let hooked = Arc::new(Hooked {
        inner: Arc::new(ContractsStub::new(Arc::new(w.stubs.clone()))),
        before,
        calls: AtomicUsize::new(0),
    });
    let svc = Arc::new(w.recorder_with(Arc::clone(&w.engine), hooked.clone()));
    let outbox = w.f.t.outbox().await;
    let spawn = |who: Caller, text: &'static str| {
        let svc = Arc::clone(&svc);
        tokio::spawn(async move { record(&svc, &who, order, n, text).await })
    };
    let (a, b) = (spawn(buyer(), "rec-dup-a"), spawn(customer(), "rec-dup-b"));
    let answers = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(
        hooked.calls.load(Ordering::SeqCst),
        2,
        "both prepared before either locked"
    );
    let committed = answers
        .iter()
        .filter(|a| matches!(a, Ok(s) if s.outcome == SettledOutcome::Committed))
        .count();
    assert_eq!(committed, 1, "{answers:?}");
    let refused: Vec<Reason> = answers
        .iter()
        .filter(|a| !matches!(a, Ok(s) if s.outcome == SettledOutcome::Committed))
        .map(refusal)
        .collect();
    assert_eq!(refused, [Reason::AcceptanceAlreadyRecorded]);
    assert_eq!(w.rows(order).await, 1);
    assert_eq!(w.f.t.outbox().await, outbox + 1);
    assert_eq!(
        w.consent(&buyer(), order)
            .await
            .current_acceptance
            .unwrap()
            .requirement_source,
        RequirementSource::Volunteered,
        "a contract not requiring consent records it as volunteered"
    );
}

/// The consent row, its audit entry, the settlement and the event commit together: a failure
/// after the event enqueue (and one before settlement) rolls everything back, so nothing is
/// recorded or emitted; the same-key retry then records exactly one consent and one event.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_event_is_unique_under_rollback() {
    let w = world(fixture()).await;
    let (order, n) = w.submitted(&buyer(), &partner(), None, "rec-rb").await;
    let (outbox, audits) = (w.f.t.outbox().await, w.f.t.audits(order).await);
    for point in [
        FaultPoint::Documents,
        FaultPoint::Enqueue,
        FaultPoint::Settle,
    ] {
        let hook = Arc::new(FailAt {
            point,
            hits: AtomicUsize::new(0),
        });
        let faulty = Arc::new(w.engine.with_fault(hook.clone()));
        let svc = w.recorder_with(faulty, Arc::new(ContractsUnavailable));
        let answer = record(&svc, &buyer(), order, n, "rec-rb-1").await;
        assert!(answer.is_err(), "{point:?}: {answer:?}");
        assert_eq!(hook.hits.load(Ordering::SeqCst), 1, "{point:?}");
        assert_eq!(w.rows(order).await, 0, "{point:?}");
        assert_eq!(w.f.t.outbox().await, outbox, "{point:?}");
        assert_eq!(w.f.t.audits(order).await, audits, "{point:?}");
    }
    let settled = record(&w.recorder(), &buyer(), order, n, "rec-rb-1")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    assert!(!settled.replayed);
    assert_eq!(w.rows(order).await, 1);
    assert_eq!(w.f.t.outbox().await, outbox + 1);
}

/// The instant and the recording actor are server facts: a body naming a timestamp, an actor or
/// a version is refused at the boundary (`request-invalid`) before authorization, writing no
/// audit or idempotency row; the recorded instant is the engine's.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_caller_timestamp_is_rejected_at_the_boundary() {
    let w = world(fixture()).await;
    let (order, _) = w.submitted(&buyer(), &partner(), None, "rec-ts").await;
    let (audits, keys) = (
        w.f.t.audits(order).await,
        w.f.t
            .n("SELECT count(*) AS n FROM bss_orders__idempotency")
            .await,
    );
    for body in [
        r#"{"accepted_at":"2020-01-01T00:00:00Z"}"#,
        r#"{"recorded_by":"00000000-0000-0000-0000-00000000006e"}"#,
        r#"{"accepted_version":2}"#,
        r#"{"recording_path":"self_service"}"#,
    ] {
        assert!(
            matches!(
                acceptance_body(body.as_bytes()),
                Err(OrdersError::Refused(Reason::RequestInvalid))
            ),
            "{body}"
        );
    }
    assert!(acceptance_body(b"{}").is_ok());
    assert_eq!(w.f.t.audits(order).await, audits);
    assert_eq!(
        w.f.t
            .n("SELECT count(*) AS n FROM bss_orders__idempotency")
            .await,
        keys
    );
    assert_eq!(w.rows(order).await, 0);
}

/// Resource-tenant membership without the `acceptance × record` grant is refused by the PEP
/// (`operation-not-permitted-for-actor`, the caller can read the order), and so is a seller
/// operator: seller authority alone never suffices. Nothing is recorded.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn membership_without_the_endpoint_grant_is_refused() {
    let w = world(fixture()).await;
    let svc = w.recorder();
    let (order, n) = w.submitted(&buyer(), &partner(), None, "rec-grant").await;
    for (who, text) in [
        (MEMBER, "rec-grant-member"),
        (SELLER_OPERATOR, "rec-grant-seller"),
    ] {
        let answer = record(&svc, &caller(who), order, n, text).await;
        assert_eq!(
            refusal(&answer),
            Reason::OperationNotPermittedForActor,
            "{text}"
        );
    }
    assert_eq!(w.rows(order).await, 0);
}

/// An unavailable requirement is `acceptance-requirement-unevaluable` (503), never a default:
/// the live contract port down for a contract-bound order, the policy store unreadable for an
/// order without a contract. A barred caller is still barred during the outage (the bar is
/// decided from stored facts, D-113). A new key records once the input answers.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unavailable_requirement_is_503() {
    let w = world(contract_fixture(Some(true))).await;
    // (a) The contract port is down.
    let (bound, n) = w
        .submitted(&partner(), &partner(), Some(CONTRACT), "rec-503a")
        .await;
    let down = w.recorder_with(Arc::clone(&w.engine), Arc::new(ContractsUnavailable));
    let answer = record(&down, &customer(), bound, n, "rec-503a-1").await;
    assert_eq!(refusal(&answer), Reason::AcceptanceRequirementUnevaluable);
    assert_eq!(answer.unwrap().response.status(), 503);
    let barred = record(&down, &partner(), bound, n, "rec-503a-2").await;
    assert_eq!(refusal(&barred), Reason::AcceptanceRecordingPartyBarred);
    assert_eq!(w.rows(bound).await, 0);
    let settled = record(&w.recorder(), &customer(), bound, n, "rec-503a-3")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
    w.release(bound).await;
    // (b) The policy store is unreadable.
    let (free, n) = w.submitted(&partner(), &partner(), None, "rec-503b").await;
    w.f.t
        .env
        .pg
        .sql("REVOKE SELECT ON bss_orders__policy_election FROM bss_orders_runtime")
        .await
        .unwrap();
    let svc = w.recorder();
    let answer = record(&svc, &customer(), free, n, "rec-503b-1").await;
    assert_eq!(refusal(&answer), Reason::AcceptanceRequirementUnevaluable);
    let barred = record(&svc, &partner(), free, n, "rec-503b-2").await;
    assert_eq!(refusal(&barred), Reason::AcceptanceRecordingPartyBarred);
    w.f.t
        .env
        .pg
        .sql("GRANT SELECT ON bss_orders__policy_election TO bss_orders_runtime")
        .await
        .unwrap();
    assert_eq!(w.rows(free).await, 0);
    let settled = record(&svc, &customer(), free, n, "rec-503b-3")
        .await
        .unwrap();
    assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
}

/// The requirement is read live at recording, not taken from submit: the referenced contract's
/// `acceptance_required` decides (`true` → `contract`, `false` → `volunteered`, recorded, not
/// refused); a contract that states nothing falls through to the seller's election.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_live_contract_then_the_elections_decide_the_source() {
    let w = world(contract_fixture(Some(true))).await;
    elect(&w.f, Some(u(SELLER)), true).await;
    let mut previous = None;
    for (stated, source) in [
        (Some(true), RequirementSource::Contract),
        (Some(false), RequirementSource::Volunteered),
        (None, RequirementSource::Seller),
    ] {
        if let Some(order) = previous {
            w.release(order).await;
        }
        let tag = format!("rec-src-{stated:?}");
        let (order, n) = w
            .submitted(&partner(), &partner(), Some(CONTRACT), &tag)
            .await;
        previous = Some(order);
        let svc = w.recorder_with(
            Arc::clone(&w.engine),
            Arc::new(ContractsStub::new(Arc::new(contract_fixture(stated)))),
        );
        let settled = record(&svc, &customer(), order, n, &format!("{tag}-1"))
            .await
            .unwrap();
        assert_eq!(settled.outcome, SettledOutcome::Committed, "{settled:?}");
        let current = w
            .consent(&customer(), order)
            .await
            .current_acceptance
            .unwrap();
        assert_eq!(current.requirement_source, source, "{stated:?}");
        assert_eq!(last_event(&w.f).await["requirementSource"], json!(source));
    }
}
