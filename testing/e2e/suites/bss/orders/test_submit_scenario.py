"""Live HTTP/PostgreSQL E2E for the Stage 4 integrated scenario (Orders S4-12, Stage 4 DESIGN §8),
the part reachable over HTTP in the development build: a partner places and submits an order,
the resource tenant's customer records consent on it, another resource-tenant buyer amends it,
the customer consents again on the amendment, and the history reads show both versions and
both consents.

Approval reflection and begin-fulfillment are undelivered Workflow seams (no route); the
in-crate PostgreSQL scenario (``orders/src/infra/storage/tests/stage4.rs``) drives them through
the engine. Pricing's receipts come from the development ``SellabilityStub`` and the undelivered
gate ports from the development stubs (``test_submit.py``'s module docstring): every provider
behind this scenario is a development double, so it is evidence of "locally complete with
doubles", never of production readiness.

The file name sorts after ``test_submit.py``, ``test_submit_amend.py`` and
``test_submit_history.py``: those depend on the session's earlier throttle state of their
buyers, and this file draws one extra buyer from the shared rotation.

Reason tokens and statuses come from ``orders-sdk/contracts/catalog.json`` through
``test_submit.py``'s ``_refused``.
"""

from __future__ import annotations

import pytest

from . import conftest
from .conftest import API, RESOURCE_TENANT, Buyer, Catalog, OrdersEnv, new_key
from .test_submit import (  # noqa: F401 (`_released_claims` is an autouse fixture)
    CREATED_VERSION,
    DEAD_LETTERS,
    PROCESSED,
    SUBMITTED_VERSION,
    _await_acknowledged,
    _refused,
    _released_claims,
)
from .test_submit_amend import REASON, _amend, _quantity
from .test_submit_history import _acceptance, _partner_submitted

PARTNER_SUBJECT = "dddddddd-6a88-4768-9dfc-6bcd5187d9ed"


@pytest.fixture
def amender(orders_env: OrdersEnv) -> Buyer:
    """A second resource-tenant buyer, drawn from the shared rotation like ``buyer``: the
    amendment's actor. The customer who consents is not the amender of a partner-placed order
    (the recording-party bar, D-255)."""
    names = list(conftest.BUYERS)
    name = names[conftest._buyer_cursor["next"] % len(names)]
    conftest._buyer_cursor["next"] += 1
    return Buyer(name=name, subject_id=conftest.BUYERS[name], client=orders_env.client(name))


def _line_id(env: OrdersEnv, order_id: str) -> str:
    """The order's one line id (the partner's draft has exactly one)."""
    return env.psql(
        f"SELECT DISTINCT line_id FROM bss_orders__order_line WHERE order_id = '{order_id}'"
    )


def _consents(env: OrdersEnv, order_id: str) -> str:
    """``version|recorded_by|path|source`` of every consent row, by version."""
    return env.psql(
        "SELECT string_agg(accepted_version || '|' || recorded_by || '|' || recording_path"
        " || '|' || requirement_source, ',' ORDER BY accepted_version)"
        f" FROM bss_orders__acceptance WHERE order_id = '{order_id}'"
    )


@pytest.mark.timeout(90, func_only=True)
def test_submit_consent_amend_consent_history(
    env: OrdersEnv, buyer: Buyer, amender: Buyer, catalog: Catalog
):
    """Submit (partner-placed, no consent) → the customer's consent on N → an amendment to M by
    another buyer → N's consent no longer current, a recording naming N is 412 naming M → the
    customer's consent on M → the version list shows 1, N, M with their reasons and
    predecessors, and the acceptance read shows M's consent current with N's as a prior record.
    Each committed step delivers exactly one message, and a same-key replay of each returns the
    stored bytes without writing."""
    partner = Buyer(name="partner", subject_id=PARTNER_SUBJECT, client=env.client("partner"))
    processed = env.count(PROCESSED)
    dead = env.count(DEAD_LETTERS)

    # Submit: version N, partner-placed, no consent (OrderSubmitted).
    order_id = _partner_submitted(env, buyer, partner, catalog)
    n = SUBMITTED_VERSION
    line_id = _line_id(env, order_id)
    assert _consents(env, order_id) == ""

    # Consent on N by the customer (OrderAcceptanceRecorded), and its exact replay.
    key_n = new_key()
    consent_n = _acceptance(buyer, order_id, n, key_n)
    assert consent_n.status_code == 200, f"{consent_n.status_code} {consent_n.text}"
    assert consent_n.json()["version"] == n and consent_n.json()["state"] == "submitted"
    assert _consents(env, order_id) == f"{n}|{buyer.subject_id}|partner_placed|platform_default"
    replay = _acceptance(buyer, order_id, n, key_n)
    assert replay.status_code == 200 and replay.content == consent_n.content
    assert replay.headers["idempotency-replayed"] == "true"

    # The amendment: M supersedes N (OrderAmended), and its exact replay.
    key_m = new_key()
    amended = _amend(amender, order_id, n, _quantity(catalog, line_id, "2"), key_m)
    assert amended.status_code == 200, f"{amended.status_code} {amended.text}"
    m = amended.json()["version"]
    assert m > n and amended.json()["state"] == "submitted", amended.json()
    replay = _amend(amender, order_id, n, _quantity(catalog, line_id, "2"), key_m)
    assert replay.status_code == 200 and replay.content == amended.content
    assert replay.headers["idempotency-replayed"] == "true"

    # N's consent does not carry over: M has none, and a recording naming N is a stale If-Match.
    page = buyer.client.get(f"{API}/orders/{order_id}/acceptance").json()
    assert page["current_version"] == m and "current_acceptance" not in page, page
    assert [r["accepted_version"] for r in page["items"]] == [n], page
    stale = _acceptance(buyer, order_id, n)
    body = _refused(stale, "precondition-failed")
    assert body["context"]["data"] == {"current_version": m}, body
    assert _consents(env, order_id) == f"{n}|{buyer.subject_id}|partner_placed|platform_default"

    # Consent on M by the customer (OrderAcceptanceRecorded), and its exact replay.
    key_c = new_key()
    consent_m = _acceptance(buyer, order_id, m, key_c)
    assert consent_m.status_code == 200, f"{consent_m.status_code} {consent_m.text}"
    assert consent_m.json()["version"] == m and consent_m.json()["state"] == "submitted"
    replay = _acceptance(buyer, order_id, m, key_c)
    assert replay.status_code == 200 and replay.content == consent_m.content
    assert replay.headers["idempotency-replayed"] == "true"
    assert _consents(env, order_id) == ",".join(
        f"{v}|{buyer.subject_id}|partner_placed|platform_default" for v in (n, m)
    )

    # Four committed steps, four messages; replays and the refusal produced none.
    _await_acknowledged(env, processed + 4)
    assert env.count(DEAD_LETTERS) == dead
    assert env.psql(
        "SELECT string_agg(trigger || ':' || version, ',' ORDER BY sequence)"
        f" FROM bss_orders__transition_audit WHERE order_id = '{order_id}'"
        " AND outcome = 'committed'"
    ) == (
        f"create:{CREATED_VERSION},draft-mutate:{CREATED_VERSION},submit:{n},"
        f"record-acceptance:{n},amendment:{m},record-acceptance:{m}"
    )

    # History: the version list and the acceptance read show both versions and both consents.
    versions = buyer.client.get(f"{API}/orders/{order_id}/versions")
    assert versions.status_code == 200, f"{versions.status_code} {versions.text}"
    items = versions.json()["items"]
    assert [
        (i["version"], i["reason"], i.get("supersedes_version"), i["actor"]) for i in items
    ] == [
        (CREATED_VERSION, "create", None, PARTNER_SUBJECT),
        (n, "submit", CREATED_VERSION, PARTNER_SUBJECT),
        (m, "amendment", n, amender.subject_id),
    ], items
    assert items[2]["amendment_reason"] == REASON
    assert items[2]["actor_tenant_id"] == RESOURCE_TENANT
    assert versions.json()["current_version"] == m

    page = buyer.client.get(f"{API}/orders/{order_id}/acceptance")
    assert page.status_code == 200, f"{page.status_code} {page.text}"
    page = page.json()
    assert page["current_version"] == m
    current = page["current_acceptance"]
    assert (current["accepted_version"], current["recorded_by"], current["recording_path"]) == (
        m,
        buyer.subject_id,
        "partner_placed",
    ), page
    assert [(r["accepted_version"], r["recorded_by"]) for r in page["items"]] == [
        (n, buyer.subject_id)
    ], page
