"""Configuration and discovery through the real HA flow manager and HTTP."""

import pytest
from homeassistant.config_entries import (
    SOURCE_HASSIO,
    SOURCE_REAUTH,
    SOURCE_USER,
    ConfigFlowResult,
)
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from custom_components.property import PropertyConfigEntry
from custom_components.property.const import (
    CONF_ADDON_SLUG,
    CONF_ALLOW_ASSIST,
    CONF_BACKEND_URL,
    CONF_NOTIFICATIONS,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import Backend

TOKEN = "fictional-integration-credential-at-least-32-characters"


async def configure(hass: HomeAssistant, values: dict[str, str]) -> ConfigFlowResult:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    return await hass.config_entries.flow.async_configure(result["flow_id"], values)


async def test_standalone_flow_validates_and_normalizes_one_origin(
    hass: HomeAssistant, backend: Backend
) -> None:
    result = await configure(
        hass, {CONF_BACKEND_URL: backend.url + "/", CONF_TOKEN: TOKEN}
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["data"] == {CONF_BACKEND_URL: backend.url, CONF_TOKEN: TOKEN}
    await hass.async_block_till_done()
    assert hass.states.get("sensor.property_needs_attention") is not None


@pytest.mark.parametrize(
    "origin",
    [
        "ftp://example.invalid",
        "http://example.invalid/v1",
        "http://user:fictional@example.invalid",
        "http://example.invalid?destination=other",
        "http://example.invalid#fragment",
        "http://example.invalid:0",
        "http://[malformed",
        "http://example.invalid\\@other.invalid",
    ],
)
async def test_invalid_origin_is_rejected_before_http(
    hass: HomeAssistant, backend: Backend, origin: str
) -> None:
    before = len(backend.requests)
    result = await configure(hass, {CONF_BACKEND_URL: origin, CONF_TOKEN: TOKEN})
    assert result["errors"] == {CONF_BACKEND_URL: "invalid_url"}
    assert len(backend.requests) == before


@pytest.mark.parametrize("credential", ["", "too-short", "x" * 257, "x" * 31 + "\n"])
async def test_invalid_credentials_are_not_stored_or_sent(
    hass: HomeAssistant, backend: Backend, credential: str
) -> None:
    result = await configure(
        hass, {CONF_BACKEND_URL: backend.url, CONF_TOKEN: credential}
    )
    assert result["errors"] == {"base": "invalid_auth"}
    assert not backend.requests


@pytest.mark.parametrize("role", ["admin", "owner", "agent"])
async def test_owner_or_unrecognized_credential_cannot_be_paired_as_an_adapter(
    hass: HomeAssistant, backend: Backend, role: str
) -> None:
    backend.role = role
    result = await configure(hass, {CONF_BACKEND_URL: backend.url, CONF_TOKEN: TOKEN})
    assert result["errors"] == {"base": "integration_required"}


async def test_hassio_discovery_requires_confirmation_before_probing(
    hass: HomeAssistant, backend: Backend
) -> None:
    discovery = HassioServiceInfo(
        config={CONF_BACKEND_URL: backend.url, CONF_TOKEN: TOKEN},
        name="Property",
        slug="example_property",
        uuid="d7a21472-a600-40c1-89b2-18d852878136",
    )
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=discovery
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert not backend.requests
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["data"][CONF_ADDON_SLUG] == "example_property"
    assert result["result"].unique_id == "d7a21472a60040c189b218d852878136"
    await hass.async_block_till_done()
    again = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=discovery
    )
    assert again["type"] is FlowResultType.ABORT
    assert again["reason"] == "single_instance_allowed"


async def test_reauth_replaces_only_the_configured_service_credential(
    hass: HomeAssistant, entry: PropertyConfigEntry, backend: Backend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_REAUTH, "entry_id": entry.entry_id},
        data=entry.data,
    )
    assert result["step_id"] == "reauth_confirm"
    replacement = "fictional-replacement-integration-at-least-32-characters"
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.url, CONF_TOKEN: replacement},
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "reauth_successful"
    await hass.async_block_till_done()
    assert entry.data[CONF_TOKEN] == replacement


async def test_options_default_to_readonly_assist_disabled(
    hass: HomeAssistant, entry: PropertyConfigEntry
) -> None:
    result = await hass.config_entries.options.async_init(entry.entry_id)
    assert result["type"] is FlowResultType.FORM
    result = await hass.config_entries.options.async_configure(
        result["flow_id"], {CONF_ALLOW_ASSIST: False, CONF_NOTIFICATIONS: False}
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    assert entry.options == {CONF_ALLOW_ASSIST: False, CONF_NOTIFICATIONS: False}
