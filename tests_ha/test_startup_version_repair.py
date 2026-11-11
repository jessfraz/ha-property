"""Startup failures retain a native repair without a loaded coordinator."""

import pytest
from aiohttp import web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from homeassistant.setup import async_setup_component
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant.common import MockConfigEntry
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.property._app_kit.version import LOADED_VERSION
from custom_components.property.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN


@pytest.mark.parametrize("version", ["0.0.1", LOADED_VERSION, "development"])
async def test_failed_first_refresh_has_a_fresh_version_repair(
    hass: HomeAssistant,
    aiohttp_server: AiohttpServer,
    socket_enabled: None,
    hass_client: ClientSessionGenerator,
    version: str,
) -> None:
    async def handle(request: web.Request) -> web.Response:
        if request.path == "/v1/system":
            return web.json_response(
                {
                    "version": version,
                    "service": "property",
                    "api_version": 1,
                    "role": "integration",
                }
            )
        return web.Response(status=503)

    app = web.Application()
    app.router.add_route("*", "/{path:.*}", handle)
    server = await aiohttp_server(app)
    assert await async_setup_component(hass, "repairs", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: str(server.make_url("")).rstrip("/"),
            CONF_TOKEN: "fictional-startup-integration-token-at-least-32-characters",
        },
    )
    entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.state is ConfigEntryState.SETUP_RETRY
    issue_id = f"{entry.entry_id}_version_mismatch"
    issue = ir.async_get(hass).async_get_issue(DOMAIN, issue_id)
    if version != "0.0.1":
        assert issue is None
        return
    assert issue is not None and issue.is_fixable and issue.is_persistent
    assert issue.config_entry_id == entry.entry_id
    client = await hass_client()
    response = await client.post(
        "/api/repairs/issues/fix", json={"handler": DOMAIN, "issue_id": issue_id}
    )
    assert response.status == 200
    flow = await response.json()
    assert flow["type"] == "form" and flow["step_id"] == "retry"
    assert flow["errors"] == {"base": "managed_update_unavailable"}
    assert (
        await client.delete(f"/api/repairs/issues/fix/{flow['flow_id']}")
    ).status == 200
    assert ir.async_get(hass).async_get_issue(DOMAIN, issue_id) is not None
