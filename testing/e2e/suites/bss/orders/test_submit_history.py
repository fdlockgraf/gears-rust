"""Live HTTP/PostgreSQL E2E for the history reads and consent recording (Orders Stage 4,
S4-06 and S4-08b).

The three history reads are production routes (``GET /orders/{id}/versions``,
``.../versions/{version}``, ``.../acceptance``, D-254); ``POST /orders/{id}/acceptance`` is the
development delivery's (D-255, D-235). The file name sorts after ``test_submit.py`` and
``test_submit_amend.py``: both depend on the session's earlier throttle state of their buyers.

Reason tokens and statuses come from ``orders-sdk/contracts/catalog.json`` through
``test_submit.py``'s ``_refused``.
"""

from __future__ import annotations

import pytest

from .conftest import (
    ACCEPTED_PROOF,
    API,
    RESOURCE_TENANT,
    Buyer,
    Catalog,
    OrdersEnv,
    create_body,
    line_from_catalog,
    new_key,
    submit,
    write_headers,
)
from .test_submit import (  # noqa: F401 (`_released_claims` is an autouse fixture)
    CREATED_VERSION,
    DEAD_LETTERS,
    PROCESSED,
    SUBMITTED_VERSION,
    _await_acknowledged,
    _refused,
    _released_claims,
)
from .test_submit_amend import REASON, _amend, _quantity, _submitted

PROOF = {"X-Delegation-Proof-Ref": ACCEPTED_PROOF}


def _acceptance(buyer: Buyer, order_id: str, version: int, key: str | None = None, **extra: str):
    """``POST /orders/{id}/acceptance`` as ``buyer`` (an empty body) at ``version``."""
    return buyer.client.post(
        f"{API}/orders/{order_id}/acceptance", json={}, headers=write_headers(version, key, **extra)
    )


def _partner_submitted(env: OrdersEnv, buyer: Buyer, partner: Buyer, catalog: Catalog) -> str:
    """An order the partner created and submitted for resource tenant A under an accepted proof
    reference (sales path ``partner_placed``); ``buyer`` is the customer. Returns its id."""
    r = partner.client.post(
        f"{API}/orders",
        json=create_body(),
        headers={"Idempotency-Key": new_key(), **PROOF},
    )
    assert r.status_code == 201, f"{r.status_code} {r.text}"
    order_id = r.json()["order"]["order_id"]
    r = partner.client.post(
        f"{API}/orders/{order_id}/lines",
        json=line_from_catalog(catalog, 0),
        headers=write_headers(CREATED_VERSION, None, **PROOF),
    )
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    r = partner.client.post(
        f"{API}/orders/{order_id}/submit",
        json={"expected_draft_revision": 1},
        headers=write_headers(CREATED_VERSION, None, **PROOF),
    )
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    assert r.json()["version"] == SUBMITTED_VERSION, r.json()
    return order_id


@pytest.mark.timeout(60, func_only=True)
def test_versions_after_amendment(env: OrdersEnv, buyer: Buyer, catalog: Catalog):
    """After submit and an amendment the version list shows the committed versions in order with
    their reasons and predecessors, each version reads back at its own content, and a number the
    order never committed is 404 ``version-not-found``."""
    order_id, line_id = _submitted(env, buyer, catalog)
    r = _amend(buyer, order_id, SUBMITTED_VERSION, _quantity(catalog, line_id, "2"))
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    m = r.json()["version"]

    page = buyer.client.get(f"{API}/orders/{order_id}/versions")
    assert page.status_code == 200, f"{page.status_code} {page.text}"
    body = page.json()
    items = body["items"]
    assert [(i["version"], i["reason"]) for i in items] == [
        (CREATED_VERSION, "create"),
        (SUBMITTED_VERSION, "submit"),
        (m, "amendment"),
    ], items
    assert "supersedes_version" not in items[0]
    assert items[1]["supersedes_version"] == CREATED_VERSION
    assert items[2]["supersedes_version"] == SUBMITTED_VERSION
    assert items[2]["amendment_reason"] == REASON
    assert all(i["actor"] == buyer.subject_id and i["actor_tenant_id"] == RESOURCE_TENANT for i in items)
    assert body["current_version"] == m

    for version in (CREATED_VERSION, SUBMITTED_VERSION, m):
        one = buyer.client.get(f"{API}/orders/{order_id}/versions/{version}")
        assert one.status_code == 200, f"{version}: {one.status_code} {one.text}"
        assert one.json()["version"] == version and one.json()["order_id"] == order_id
    # Submitted content is the original quantity; the amendment's is the new one.
    qty = lambda v: buyer.client.get(f"{API}/orders/{order_id}/versions/{v}").json()["lines"][0][
        "selected_items"
    ][0]["quantity"]
    assert (qty(SUBMITTED_VERSION), qty(m)) == ("1", "2")
    assert buyer.client.get(f"{API}/orders/{order_id}/versions/{CREATED_VERSION}").json()["lines"] == []

    missing = buyer.client.get(f"{API}/orders/{order_id}/versions/{m + 50}")
    _refused(missing, "version-not-found")


@pytest.mark.timeout(60, func_only=True)
def test_acceptance_page_shows_self_service_consent(env: OrdersEnv, buyer: Buyer, catalog: Catalog):
    """A self-service submit records the customer's consent at the submitted version (the order
    names no contract and no election exists, so the platform default requires it); the
    acceptance read shows it as the current one."""
    order_id, _ = _submitted(env, buyer, catalog)
    r = buyer.client.get(f"{API}/orders/{order_id}/acceptance")
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    body = r.json()
    assert body["current_version"] == SUBMITTED_VERSION
    current = body["current_acceptance"]
    assert current["accepted_version"] == SUBMITTED_VERSION, body
    assert current["recorded_by"] == buyer.subject_id
    assert current["recording_path"] == "self_service"
    assert current["requirement_source"] == "platform_default"
    assert body["items"] == []


@pytest.mark.timeout(60, func_only=True)
def test_partner_placed_consent_is_recorded_by_the_customer_only(
    env: OrdersEnv, buyer: Buyer, catalog: Catalog
):
    """On a partner-placed submitted order the partner is barred from recording consent (403
    ``acceptance-recording-party-barred``); the resource-tenant customer records it (200,
    ``OrderAcceptanceRecorded`` delivered), the acceptance read then shows it, and a second
    record with a different key is 409 ``acceptance-already-recorded``."""
    partner = Buyer(name="partner", subject_id="dddddddd-6a88-4768-9dfc-6bcd5187d9ed", client=env.client("partner"))
    processed = env.count(PROCESSED)
    dead = env.count(DEAD_LETTERS)
    order_id = _partner_submitted(env, buyer, partner, catalog)

    # Self-service consent is not recorded at a partner-placed submit.
    page = buyer.client.get(f"{API}/orders/{order_id}/acceptance").json()
    assert page["current_version"] == SUBMITTED_VERSION and "current_acceptance" not in page, page

    barred = _acceptance(partner, order_id, SUBMITTED_VERSION, **PROOF)
    _refused(barred, "acceptance-recording-party-barred")
    assert env.count(
        f"SELECT count(*) FROM bss_orders__acceptance WHERE order_id = '{order_id}'"
    ) == 0

    ok = _acceptance(buyer, order_id, SUBMITTED_VERSION)
    assert ok.status_code == 200, f"{ok.status_code} {ok.text}"
    assert ok.json()["version"] == SUBMITTED_VERSION and ok.json()["state"] == "submitted", ok.json()

    # OrderSubmitted, then OrderAcceptanceRecorded: delivered, none dead-lettered.
    _await_acknowledged(env, processed + 2)
    assert env.count(DEAD_LETTERS) == dead

    page = buyer.client.get(f"{API}/orders/{order_id}/acceptance").json()
    current = page["current_acceptance"]
    assert current["accepted_version"] == SUBMITTED_VERSION, page
    assert current["recorded_by"] == buyer.subject_id
    assert current["recording_path"] == "partner_placed"

    again = _acceptance(buyer, order_id, SUBMITTED_VERSION, new_key())
    _refused(again, "acceptance-already-recorded")
