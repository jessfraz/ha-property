"""Fictional backend over real HTTP and native Home Assistant fixtures."""

import asyncio
import json
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.auth.providers import AuthProvider
from homeassistant.core import HomeAssistant
from homeassistant.setup import async_setup_component
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant.common import MockConfigEntry, MockUser

from custom_components.property import PropertyConfigEntry
from custom_components.property.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN


def installment(
    *, state: str = "open", identity: str = "example-bill-1"
) -> dict[str, object]:
    """No real parcel, amounts, or source documents are used."""
    return {
        "id": identity,
        "number": 1,
        "due_date": "2026-11-01",
        "delinquent_at": "2026-12-11T01:00:00Z",
        "timezone": "America/Los_Angeles",
        "state": state,
    }


def summary() -> dict[str, object]:
    return {
        "revision": 1,
        "needs_attention": 2,
        "known_balance_cents": None,
        "last_observed_at": "2026-10-30T18:00:00Z",
        "coverage": [],
        "issues": [],
        "installments": [installment()],
    }


@dataclass
class Backend:
    url: str = ""
    document: dict[str, object] = field(default_factory=summary)
    requests: list[tuple[str, str, bytes]] = field(default_factory=list)
    status: int = 200
    blocked_path: str | None = None
    received: asyncio.Event = field(default_factory=asyncio.Event)
    release: asyncio.Event = field(default_factory=asyncio.Event)
    role: str = "integration"
    headers: list[dict[str, str]] = field(default_factory=list)
    responses: dict[str, tuple[int, bytes, dict[str, str]]] = field(
        default_factory=dict
    )

    async def handle(self, request: web.Request) -> web.Response:
        self.requests.append((request.method, request.path_qs, await request.read()))
        self.headers.append(dict(request.headers))
        if request.path == self.blocked_path:
            self.received.set()
            await self.release.wait()
        if request.path in self.responses:
            status, body, headers = self.responses[request.path]
            return web.Response(status=status, body=body, headers=headers)
        if request.path == "/v1/system":
            return web.json_response(
                {"service": "property", "api_version": 1, "role": self.role}
            )
        if request.path == "/v1/summary":
            return web.Response(
                status=self.status,
                body=json.dumps(self.document).encode(),
                content_type="application/json",
            )
        if request.path in {"/v1/tax/bills", "/v1/tax/assessments"}:
            return web.json_response({"items": [], "next_page": "example-next-page"})
        return web.json_response({"status": "ok"})


@pytest.fixture(autouse=True)
def custom_integrations(enable_custom_integrations: None) -> None:
    """Enable only integration code in this checkout."""


@pytest.fixture(autouse=True)
async def core_component(hass: HomeAssistant) -> None:
    assert await async_setup_component(hass, "homeassistant", {})


@pytest.fixture
async def backend(
    aiohttp_server: AiohttpServer, socket_enabled: None
) -> AsyncIterator[Backend]:
    upstream = Backend()
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", upstream.handle)
    server = await aiohttp_server(app)
    upstream.url = str(server.make_url("")).rstrip("/")
    yield upstream


@pytest.fixture
async def entry(hass: HomeAssistant, backend: Backend) -> PropertyConfigEntry:
    config_entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "fictional-integration-token-at-least-32-characters",
        },
    )
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    return config_entry


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth: AuthProvider) -> MockUser:
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert group is not None
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh_token)
