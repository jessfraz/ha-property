"""Exercise native sensors, county-local calendar, Repairs and real HTTP."""

import asyncio
from datetime import UTC, datetime, timedelta

import pytest
from freezegun.api import FrozenDateTimeFactory
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from homeassistant.setup import async_setup_component
from homeassistant.util import dt as dt_util
from pytest_homeassistant.common import async_fire_time_changed
from pytest_homeassistant.typing import ClientSessionGenerator, WebSocketGenerator

from custom_components.property import PropertyConfigEntry
from custom_components.property._app_kit.version import LOADED_VERSION
from custom_components.property.const import DOMAIN
from custom_components.property.models import PropertyData

from .conftest import Backend, installment, summary


async def test_version_repair_keeps_ledger_and_requires_fresh_parity(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
    freezer: FrozenDateTimeFactory,
) -> None:
    assert await async_setup_component(hass, "repairs", {})
    await hass.async_block_till_done(wait_background_tasks=True)
    issue_id = f"{entry.entry_id}_version_mismatch"
    registry = ir.async_get(hass)

    async def observe_next_version() -> None:
        freezer.tick(timedelta(minutes=1))
        async_fire_time_changed(hass, dt_util.utcnow())
        await hass.async_block_till_done(wait_background_tasks=True)

    backend.version = "0.0.1"
    await observe_next_version()
    issue = registry.async_get_issue(DOMAIN, issue_id)
    assert issue is not None and issue.is_fixable and issue.is_persistent
    assert issue.config_entry_id == entry.entry_id
    assert issue.translation_placeholders == {
        "app_version": "0.0.1",
        "integration_version": LOADED_VERSION,
    }
    client = await hass_client()
    result = await client.post(
        "/api/repairs/issues/fix", json={"handler": DOMAIN, "issue_id": issue_id}
    )
    assert result.status == 200
    retry = await result.json()
    assert retry["type"] == "form" and retry["step_id"] == "retry"
    assert retry["errors"] == {"base": "managed_update_unavailable"}
    closed = await client.delete(f"/api/repairs/issues/fix/{retry['flow_id']}")
    assert closed.status == 200
    assert registry.async_get_issue(DOMAIN, issue_id) is not None
    backend.responses["/v1/system"] = (503, b"", {})
    await observe_next_version()
    assert registry.async_get_issue(DOMAIN, issue_id) is not None
    assert hass.states.is_state("sensor.property_needs_attention", "2")
    backend.responses.pop("/v1/system")
    backend.version = LOADED_VERSION
    await observe_next_version()
    assert registry.async_get_issue(DOMAIN, issue_id) is None


async def test_projection_publishes_while_version_endpoint_is_blocked(
    hass: HomeAssistant,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    await hass.async_block_till_done(wait_background_tasks=True)
    assert hass.states.is_state("sensor.property_needs_attention", "2")
    request_start = len(backend.requests)
    backend.blocked_path = "/v1/system"
    backend.document["needs_attention"] = 3
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(minutes=1))
    refresh = asyncio.create_task(entry.runtime_data.async_refresh())
    try:
        async with asyncio.timeout(2):
            await backend.received.wait()
            await asyncio.shield(refresh)
        assert hass.states.is_state("sensor.property_needs_attention", "3")

        async with asyncio.timeout(2):
            await entry.runtime_data.async_refresh()
        assert (
            sum(path == "/v1/system" for _, path, _ in backend.requests[request_start:])
            == 1
        )
        async with asyncio.timeout(2):
            assert await hass.config_entries.async_unload(entry.entry_id)
    finally:
        backend.release.set()
        await refresh
        await hass.async_block_till_done(wait_background_tasks=True)


async def test_native_entities_preserve_unknown_zero_outage_and_recovery(
    hass: HomeAssistant,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    state = hass.states.get("sensor.property_latest_observed_balance")
    assert state is not None and state.state == "unknown"
    assert hass.states.is_state("sensor.property_needs_attention", "2")
    registry = ir.async_get(hass)
    assert (DOMAIN, f"{entry.entry_id}_coverage") in registry.issues
    backend.document["known_balance_cents"] = 0
    backend.document["needs_attention"] = 0
    backend.document["coverage"] = [
        {"capability": capability, "status": "complete"}
        for capability in ("notices", "assessments", "balances", "payments")
    ]
    await entry.runtime_data.async_refresh()
    assert hass.states.is_state("sensor.property_latest_observed_balance", "0")
    assert (DOMAIN, f"{entry.entry_id}_coverage") not in registry.issues
    backend.status = 503
    await entry.runtime_data.async_refresh()
    assert hass.states.is_state(
        "sensor.property_latest_observed_balance", "unavailable"
    )
    assert entry.runtime_data.data.known_balance_cents == 0
    assert (DOMAIN, f"{entry.entry_id}_backend") in registry.issues
    backend.status = 200
    backend.document["known_balance_cents"] = None
    await entry.runtime_data.async_refresh()
    assert hass.states.is_state("sensor.property_latest_observed_balance", "unknown")
    assert (DOMAIN, f"{entry.entry_id}_backend") not in registry.issues


async def test_calendar_uses_county_timezone_and_never_hides_pending_payment(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    await hass.config.async_set_time_zone("America/New_York")
    backend.document["installments"] = [
        installment(state="pending_payment"),
        installment(state="paid", identity="example-paid-bill"),
    ]
    await entry.runtime_data.async_refresh()
    client = await hass_client()
    response = await client.get(
        "/api/calendars/calendar.property_tax_deadlines",
        params={"start": "2026-11-01T00:00:00Z", "end": "2026-11-03T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 1
    event = events[0]
    start = datetime.fromisoformat(event["start"]["dateTime"])
    end = datetime.fromisoformat(event["end"]["dateTime"])
    assert start.astimezone(UTC).isoformat() == "2026-11-01T07:00:00+00:00"
    assert end.astimezone(UTC).isoformat() == "2026-11-02T08:00:00+00:00"
    assert (end.astimezone(UTC) - start.astimezone(UTC)).total_seconds() == 25 * 3600
    assert "pending_payment" in event["description"]
    assert event["uid"] == "example-bill-1:due"
    response = await client.get(
        "/api/calendars/calendar.property_tax_deadlines",
        params={"start": "2026-12-11T00:00:00Z", "end": "2026-12-12T00:00:00Z"},
    )
    assert response.status == 200
    assert (await response.json())[0]["uid"] == "example-bill-1:delinquent"
    state = hass.states.get("calendar.property_tax_deadlines")
    assert state is not None and state.attributes["supported_features"] == 0


@pytest.mark.parametrize(
    "bad",
    [
        {"known_balance_cents": False},
        {"known_balance_cents": -1},
        {"installments": [installment(state="fictional-unknown-state")]},
        {"installments": [installment(), installment()]},
        {"coverage": [{"capability": {}, "status": "complete"}]},
        {"installments": [{**installment(), "state": {}}]},
        {"installments": [{**installment(), "timezone": "Missing/Zone"}]},
        {"last_observed_at": "2026-11-01T00:00:00"},
    ],
)
def test_malformed_evidence_is_not_an_empty_or_zero_snapshot(
    bad: dict[str, object],
) -> None:
    with pytest.raises(ValueError):
        PropertyData.parse({**summary(), **bad})


async def test_repairs_and_notifications_follow_the_ledger_and_unload_cleanly(
    hass: HomeAssistant,
    hass_ws_client: WebSocketGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    websocket = await hass_ws_client(hass)
    await websocket.send_json({"id": 1, "type": "persistent_notification/get"})
    result = await websocket.receive_json()
    assert result["success"]
    notifications = result["result"]
    assert len(notifications) == 1
    assert notifications[0]["notification_id"] == f"property_{entry.entry_id}_attention"
    assert "No payment has been made" in notifications[0]["message"]
    backend.document["needs_attention"] = 0
    backend.document["coverage"] = [
        {"capability": capability, "status": "complete"}
        for capability in ("notices", "assessments", "balances", "payments")
    ]
    # Staleness comes from the Rust projection, not a second HA tax rule.
    backend.document["issues"] = [{"code": "coverage_gap"}]
    await entry.runtime_data.async_refresh()
    assert hass.states.is_state("sensor.property_county_coverage", "incomplete")
    await websocket.send_json({"id": 2, "type": "persistent_notification/get"})
    assert (await websocket.receive_json())["result"] == []
    assert await hass.config_entries.async_unload(entry.entry_id)
    registry = ir.async_get(hass)
    assert (DOMAIN, f"{entry.entry_id}_coverage") not in registry.issues
    assert (DOMAIN, f"{entry.entry_id}_backend") not in registry.issues


async def test_http_bridge_has_no_payment_route_and_blocks_destination_selection(
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client()
    before = len(backend.requests)
    for path in (
        "tax/payments/submit",
        "tax/bills/example/mark_paid",
        "http://example.invalid",
        "../system",
    ):
        response = await client.post(f"/api/property/v1/{path}", json={})
        assert response.status in {404, 405}
    response = await client.get(
        "/api/property/v1/tax/bills?backend_url=http://example.invalid"
    )
    assert response.status == 400
    assert len(backend.requests) == before
    response = await client.get("/api/property/v1/tax/bills?limit=1&page_token=example")
    assert response.status == 200
    assert (await response.json())["next_page"] == "example-next-page"
