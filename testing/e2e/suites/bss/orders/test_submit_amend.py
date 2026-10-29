"""Live HTTP/PostgreSQL E2E for amendments over the development delivery (Orders Stage 4,
S4-03/S4-04).

A buyer submits an order (version 2, ``test_submit.py``'s path) and amends it through
``POST /bss-orders/v1/orders/{orderId}/amendments`` (``api/rest/amend.rs``), mounted with the
submit route (D-231). The amendment re-runs the gate over every proposed line, reserves the next
candidate M and commits it through the engine: ``order_version`` M supersedes N with reason
``amendment`` and the caller's explanation, and one ``OrderAmended`` is enqueued with the
transition. The Orders producer's Event Broker grant is the family prefix in
``gears/bss/orders/config/e2e-orders.yaml``, so the event is delivered and acknowledged.

The file name sorts after ``test_submit.py`` on purpose: that file's throttle test needs its
buyer to have made no submit earlier in the session (its module docstring). The claim release
after each test is ``test_submit.py``'s autouse fixture, imported here.

Reason tokens and statuses are read from ``orders-sdk/contracts/catalog.json`` through
``test_submit.py``'s ``_refused``, never written as literals here.
"""

from __future__ import annotations

import pytest

from .conftest import API, Buyer, Catalog, OrdersEnv, new_key, submit, write_headers
from .test_submit import (  # noqa: F401 (`_released_claims` is an autouse fixture)
    CREATED_VERSION,
    DEAD_LETTERS,
    PROCESSED,
    SUBMITTED_VERSION,
    _await_acknowledged,
    _draft_with_line,
    _refused,
    _released_claims,
)

REASON = "the customer asked for a bigger plan"


def _amend(buyer: Buyer, order_id: str, version: int, body: dict, key: str | None = None):
    """``POST /orders/{id}/amendments`` as ``buyer`` with ``If-Match: "<version>"``."""
    return buyer.client.post(
        f"{API}/orders/{order_id}/amendments", json=body, headers=write_headers(version, key)
    )


def _quantity(catalog: Catalog, line_id: str, quantity: str) -> dict:
    """A delta changing ``line_id``'s one selected item to ``quantity``."""
    return {
        "reason": REASON,
        "lines": [
            {
                "op": "change",
                "line_id": line_id,
                "patch": {"selected_items": [{"item_id": catalog.item_id, "quantity": quantity}]},
            }
        ],
    }


def _submitted(env: OrdersEnv, buyer: Buyer, catalog: Catalog) -> tuple[str, str]:
    """A submitted order of ``buyer`` with one line: committed version 2."""
    order_id, line_id = _draft_with_line(env, buyer, catalog)
    r = submit(buyer, order_id, CREATED_VERSION, 1)
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    assert r.json()["version"] == SUBMITTED_VERSION, r.json()
    return order_id, line_id


@pytest.mark.timeout(45, func_only=True)
def test_amend_submitted_order(env: OrdersEnv, buyer: Buyer, catalog: Catalog):
    """An amendment of a submitted order commits M > N superseding N, stays ``submitted``,
    counts once, re-pins every line at M and delivers exactly one more message
    (``OrderAmended``) after the submit's ``OrderSubmitted``."""
    processed = env.count(PROCESSED)
    dead = env.count(DEAD_LETTERS)
    order_id, line_id = _submitted(env, buyer, catalog)

    key = new_key()
    r = _amend(buyer, order_id, SUBMITTED_VERSION, _quantity(catalog, line_id, "2"), key)
    assert r.status_code == 200, f"{r.status_code} {r.text}"
    m = r.json()["version"]
    assert m > SUBMITTED_VERSION, r.json()
    assert r.json()["order_id"] == order_id and r.json()["state"] == "submitted", r.json()
    assert r.headers["etag"] == f'"{m}"'
    assert "idempotency-replayed" not in r.headers

    assert env.psql(
        "SELECT state || '|' || current_version || '|' || amendment_count"
        f" FROM bss_orders__order WHERE order_id = '{order_id}'"
    ) == f"submitted|{m}|1"
    assert env.psql(
        "SELECT supersedes_version || '|' || reason || '|' || amendment_reason || '|' || actor"
        f" FROM bss_orders__order_version WHERE order_id = '{order_id}' AND version = {m}"
    ) == f"{SUBMITTED_VERSION}|amendment|{REASON}|{buyer.subject_id}"
    # M's line: the same line id, the new quantity, pinned at M by the amendment's attempt.
    assert env.count(
        "SELECT count(*) FROM bss_orders__order_line l"
        " JOIN bss_orders__commercial_attempt a ON a.attempt_id = l.commercial_attempt_id"
        f" WHERE l.order_id = '{order_id}' AND l.version = {m} AND l.line_id = '{line_id}'"
        " AND l.selected_items->0->>'quantity' = '2' AND l.order_pin IS NOT NULL"
        f" AND a.operation = 'amendment' AND a.status = 'committed' AND a.candidate_version = {m}"
    ) == 1
    # N is untouched: its line keeps the submitted quantity.
    assert env.psql(
        "SELECT selected_items->0->>'quantity' FROM bss_orders__order_line"
        f" WHERE order_id = '{order_id}' AND version = {SUBMITTED_VERSION}"
    ) == "1"
    assert env.psql(
        "SELECT outcome || '|' || from_state || '|' || to_state || '|' || version || '|'"
        " || caller_reason FROM bss_orders__transition_audit"
        f" WHERE order_id = '{order_id}' AND trigger = 'amendment'"
    ) == f"committed|submitted|submitted|{m}|{REASON}"

    # OrderSubmitted, then OrderAmended: both delivered and acknowledged, none dead-lettered.
    _await_acknowledged(env, processed + 2)
    assert env.count(DEAD_LETTERS) == dead

    # The same key replays the stored answer.
    replay = _amend(buyer, order_id, SUBMITTED_VERSION, _quantity(catalog, line_id, "2"), key)
    assert replay.status_code == 200, f"{replay.status_code} {replay.text}"
    assert replay.content == r.content
    assert replay.headers["idempotency-replayed"] == "true"


@pytest.mark.timeout(45, func_only=True)
def test_amend_stale_if_match_412(env: OrdersEnv, buyer: Buyer, catalog: Catalog):
    """An ``If-Match`` naming a version that is not current is 412 ``precondition-failed``
    naming the current version, before any external call: no attempt, no candidate, no new
    version, no message."""
    processed = env.count(PROCESSED)
    order_id, line_id = _submitted(env, buyer, catalog)

    r = _amend(buyer, order_id, CREATED_VERSION, _quantity(catalog, line_id, "2"))
    body = _refused(r, "precondition-failed")
    assert body["context"]["data"] == {"current_version": SUBMITTED_VERSION}, body

    assert env.psql(
        "SELECT state || '|' || current_version || '|' || version_allocation_high_water || '|'"
        f" || amendment_count FROM bss_orders__order WHERE order_id = '{order_id}'"
    ) == f"submitted|{SUBMITTED_VERSION}|{SUBMITTED_VERSION}|0"
    assert env.count(
        "SELECT count(*) FROM bss_orders__commercial_attempt"
        f" WHERE order_id = '{order_id}' AND operation = 'amendment'"
    ) == 0
    assert env.count(
        "SELECT count(*) FROM bss_orders__transition_audit"
        f" WHERE order_id = '{order_id}' AND trigger = 'amendment' AND outcome = 'refused'"
    ) == 1
    # The submit's message only; the refusal enqueues nothing.
    _await_acknowledged(env, processed + 1)
