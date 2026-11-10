"""Read-only tools through Core's real Assist API and fixed HTTP transport."""

import asyncio

import probatio
import pytest
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import llm
from homeassistant.setup import async_setup_component
from pytest_homeassistant.common import MockUser

from custom_components.property import PropertyConfigEntry
from custom_components.property.const import CONF_ALLOW_ASSIST, DOMAIN

from .conftest import Backend

ENTITY = "sensor.property_needs_attention"
SUMMARY = "property__summary"


async def api(
    hass: HomeAssistant, user_id: str | None, *, assistant: str = "conversation"
) -> llm.APIInstance:
    assert await async_setup_component(hass, "llm", {})
    return await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test",
            context=Context(user_id=user_id) if user_id is not None else None,
            language="en",
            assistant=assistant,
            device_id=None,
        ),
    )


async def call(caller: llm.APIInstance, name: str = SUMMARY) -> llm.ToolResult:
    return await caller.async_call_tool(llm.ToolInput(tool_name=name, tool_args={}))


@pytest.fixture
async def assist_api(
    hass: HomeAssistant, entry: PropertyConfigEntry, hass_user: MockUser
) -> llm.APIInstance:
    hass.config_entries.async_update_entry(entry, options={CONF_ALLOW_ASSIST: True})
    await hass.async_block_till_done()
    async_expose_entity(hass, "conversation", ENTITY, True)
    return await api(hass, hass_user.id)


async def test_assist_requires_explicit_option_and_actual_assistant_exposure(
    hass: HomeAssistant, entry: PropertyConfigEntry, hass_user: MockUser
) -> None:
    async_expose_entity(hass, "conversation", ENTITY, True)
    caller = await api(hass, hass_user.id)
    assert not any(tool.integration == DOMAIN for tool in caller.tools)
    hass.config_entries.async_update_entry(entry, options={CONF_ALLOW_ASSIST: True})
    await hass.async_block_till_done()
    hidden = await api(hass, hass_user.id, assistant="example_assistant")
    assert not any(tool.integration == DOMAIN for tool in hidden.tools)
    async_expose_entity(hass, "example_assistant", ENTITY, True)
    exposed = await api(hass, hass_user.id, assistant="example_assistant")
    assert not (await call(exposed)).error


async def test_native_tool_discovery_schema_and_unknown_balance(
    assist_api: llm.APIInstance, backend: Backend
) -> None:
    property_tools = [tool for tool in assist_api.tools if tool.integration == DOMAIN]
    assert {tool.name for tool in property_tools} == {
        "property__summary",
        "property__tax_bills",
        "property__tax_balance",
        "property__assessments",
        "property__health",
    }
    for tool in property_tools:
        assert tool.annotations is not None and tool.annotations.read_only
        schema = probatio.to_openapi(
            tool.parameters,
            custom_serializer=assist_api.custom_serializer,
            openapi_version="3.1.0",
        )
        assert schema["type"] == "object"
        assert schema["additionalProperties"] is False
    backend.document["known_balance_cents"] = None
    result = await call(assist_api, "property__tax_balance")
    assert not result.error and result.data["known_balance_cents"] is None
    page = await call(assist_api, "property__tax_bills")
    assert not page.error and page.data["next_page"] == "example-next-page"
    assert backend.requests[-1][1] == "/v1/tax/bills?limit=20"
    assert "untrusted data" in assist_api.api_prompt
    assert all(method == "GET" for method, _, _ in backend.requests)


async def test_tool_requires_known_active_calling_user_with_read_permission(
    hass: HomeAssistant, assist_api: llm.APIInstance, backend: Backend
) -> None:
    denied = MockUser().add_to_hass(hass)
    inactive = MockUser(is_active=False).add_to_hass(hass)
    for user_id in (None, "removed-example-user", denied.id, inactive.id):
        caller = await api(hass, user_id)
        before = len(backend.requests)
        result = await call(caller)
        assert result.error and result.data["error"] == "permission_denied"
        assert len(backend.requests) == before
    reader = MockUser().add_to_hass(hass)
    reader.mock_policy({"entities": {"entity_ids": {ENTITY: {"read": True}}}})
    assert not (await call(await api(hass, reader.id))).error


@pytest.mark.parametrize("change", ["exposure", "unload", "permission", "option"])
async def test_access_revoked_during_backend_io_is_not_disclosed(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    entry: PropertyConfigEntry,
    backend: Backend,
    hass_user: MockUser,
    change: str,
) -> None:
    backend.blocked_path = "/v1/tax/bills"
    backend.received.clear()
    task = hass.async_create_task(call(assist_api, "property__tax_bills"))
    async with asyncio.timeout(5):
        await backend.received.wait()
    if change == "exposure":
        async_expose_entity(hass, "conversation", ENTITY, False)
    elif change == "unload":
        assert await hass.config_entries.async_unload(entry.entry_id)
    elif change == "permission":
        hass_user.mock_policy({})
    else:
        hass.config_entries.async_update_entry(
            entry, options={CONF_ALLOW_ASSIST: False}
        )
    backend.release.set()
    result = await task
    assert result.error and result.data == {"error": "permission_denied"}
    await hass.async_block_till_done()


async def test_cached_tool_cannot_query_replacement_installation(
    hass: HomeAssistant,
    assist_api: llm.APIInstance,
    entry: PropertyConfigEntry,
    backend: Backend,
) -> None:
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    before = len(backend.requests)
    assert (await call(assist_api)).error
    assert len(backend.requests) == before
    fresh = await llm.async_get_api(hass, "assist", assist_api.llm_context)
    assert not (await call(fresh)).error


async def test_unknown_tool_arguments_and_error_bodies_are_not_forwarded(
    assist_api: llm.APIInstance, backend: Backend
) -> None:
    before = len(backend.requests)
    result = await assist_api.async_call_tool(
        llm.ToolInput(
            tool_name=SUMMARY, tool_args={"backend_url": "http://example.invalid"}
        )
    )
    assert result.error and len(backend.requests) == before
    backend.status = 503
    backend.document = {"private_example": "do not expose this backend error body"}
    result = await call(assist_api)
    assert result.error and result.data == {"error": "unavailable"}
