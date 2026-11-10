"""Native HA authentication, administrative claims and bridge lifecycle."""

import asyncio
import json

from homeassistant.core import HomeAssistant
from pytest_homeassistant.common import MockUser
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.property import PropertyConfigEntry
from custom_components.property.const import MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES

from .conftest import Backend

PREFIX = "/api/property/v1"


async def test_tax_rest_bridge_requires_an_active_admin(
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    anonymous = await hass_client(None)
    assert (await anonymous.get(f"{PREFIX}/summary")).status == 401
    user = await hass_client(hass_user_token)
    before = len(backend.requests)
    for method, path in (
        ("get", "summary"),
        ("post", "sync"),
        ("post", "imports"),
        ("patch", "property"),
    ):
        response = await getattr(user, method)(
            f"{PREFIX}/{path}", headers={"X-Property-Admin": "true"}
        )
        assert response.status == 403
    assert len(backend.requests) == before


async def test_admin_claim_and_preconditions_come_from_the_bridge(
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    admin = await hass_client()
    response = await admin.post(
        f"{PREFIX}/imports",
        json={"preview": {"hash": "fictional-hash"}, "if_match": 4},
        headers={
            "If-Match": '"4"',
            "Idempotency-Key": "fictional-request",
            "X-Property-Admin": "false",
            "X-Property-Actor-Id": "forged-user",
        },
    )
    assert response.status == 200
    assert response.headers["Cache-Control"] == "no-store"
    index = next(
        index
        for index, (method, path, _) in enumerate(backend.requests)
        if method == "POST" and path == "/v1/imports"
    )
    forwarded = backend.headers[index]
    assert forwarded["Authorization"] == (
        "Bearer fictional-integration-token-at-least-32-characters"
    )
    assert forwarded["X-Property-Admin"] == "true"
    assert "X-Property-Actor-Id" not in forwarded
    assert forwarded["If-Match"] == '"4"'
    assert forwarded["Idempotency-Key"] == "fictional-request"
    assert json.loads(backend.requests[index][2])["if_match"] == 4


async def test_history_setting_uses_native_admin_claim_etag_and_refresh(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    admin = await hass_client()
    backend.document["revision"] = 2
    response = await admin.patch(
        f"{PREFIX}/property",
        json={"bought_on": "2020-06-01"},
        headers={"If-Match": '"1"', "X-Property-Admin": "false"},
    )
    assert response.status == 200
    index = next(
        index
        for index, (method, path, _) in enumerate(backend.requests)
        if method == "PATCH" and path == "/v1/property"
    )
    assert backend.headers[index]["If-Match"] == '"1"'
    assert backend.headers[index]["X-Property-Admin"] == "true"
    assert json.loads(backend.requests[index][2]) == {"bought_on": "2020-06-01"}
    await hass.async_block_till_done()
    assert entry.runtime_data.data.revision == 2
    assert response.headers["Cache-Control"] == "no-store"


async def test_invalid_queries_and_oversized_inputs_do_not_reach_backend(
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client()
    before = len(backend.requests)
    assert (await client.get(f"{PREFIX}/tax/bills?limit=1&limit=2")).status == 400
    response = await client.post(
        f"{PREFIX}/imports/preview",
        data=b" " * (MAX_REQUEST_BYTES + 1),
        headers={"Content-Type": "application/json"},
    )
    assert response.status == 413
    assert len(backend.requests) == before


async def test_redirects_and_oversized_responses_never_escape_the_fixed_origin(
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client()
    backend.responses["/v1/property"] = (
        302,
        b"",
        {"Location": "http://example.invalid/collect-credential"},
    )
    response = await client.get(f"{PREFIX}/property", allow_redirects=False)
    assert response.status == 302 and "Location" not in response.headers
    assert backend.requests[-1][1] == "/v1/property"
    backend.responses["/v1/property"] = (
        200,
        b" " * (MAX_RESPONSE_BYTES + 1),
        {"Content-Type": "application/json"},
    )
    response = await client.get(f"{PREFIX}/property")
    assert response.status == 502


async def test_late_bridge_read_is_not_disclosed_after_unload(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client()
    backend.blocked_path = "/v1/property"
    backend.received.clear()
    pending = asyncio.create_task(client.get(f"{PREFIX}/property"))
    async with asyncio.timeout(5):
        await backend.received.wait()
    assert await hass.config_entries.async_unload(entry.entry_id)
    backend.release.set()
    assert (await pending).status == 503


async def test_admin_revocation_during_io_blocks_the_result(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_admin_user: MockUser,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    client = await hass_client()
    backend.blocked_path = "/v1/property"
    backend.received.clear()
    pending = asyncio.create_task(client.get(f"{PREFIX}/property"))
    async with asyncio.timeout(5):
        await backend.received.wait()
    await hass.auth.async_update_user(hass_admin_user, is_active=False)
    backend.release.set()
    assert (await pending).status == 403
