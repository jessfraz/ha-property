"""Home Assistant adapter for the deterministic Property tax ledger."""

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType

from ._app_kit.repairs import async_check_startup_version
from .client import InvalidCredential, PropertyClient, ResponseTooLarge
from .const import CONF_BACKEND_URL, CONF_TOKEN
from .coordinator import PropertyCoordinator
from .http import PropertyView
from .repairs import VERSION_REPAIR, VersionChecks

PLATFORMS = [Platform.SENSOR, Platform.CALENDAR]
type PropertyConfigEntry = ConfigEntry[PropertyCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Register the authenticated API once, independent of entry reloads."""
    hass.http.register_view(PropertyView(hass))
    return True


async def async_setup_entry(hass: HomeAssistant, entry: PropertyConfigEntry) -> bool:
    """Connect the backend without requiring any conversation provider."""
    try:
        try:
            client = PropertyClient(
                async_get_clientsession(hass),
                entry.data[CONF_BACKEND_URL],
                entry.data[CONF_TOKEN],
            )
        except InvalidCredential as err:
            raise ConfigEntryAuthFailed(
                "Property needs a valid integration credential"
            ) from err
        try:
            system = await client.request("GET", "/v1/system")
        except (ClientError, TimeoutError, ResponseTooLarge) as err:
            raise ConfigEntryNotReady("Property is unavailable") from err

        if system.status in {401, 403}:
            raise ConfigEntryAuthFailed("Property rejected its integration credential")
        if system.status != 200:
            raise ConfigEntryNotReady("Property is unavailable")
        try:
            data = system.json()
            if (
                not isinstance(data, dict)
                or data.get("service") != "property"
                or data.get("api_version") != 1
                or data.get("role") != "integration"
            ):
                raise ConfigEntryAuthFailed(
                    "Property requires an integration credential"
                )
        except ValueError as err:
            raise ConfigEntryNotReady("Property returned invalid discovery") from err
        coordinator = PropertyCoordinator(hass, entry, client)
        await coordinator.async_config_entry_first_refresh()
    except ConfigEntryNotReady:
        await async_check_startup_version(hass, entry, VERSION_REPAIR)
        raise
    entry.runtime_data = coordinator
    VersionChecks(hass, entry).async_start()
    entry.async_on_unload(coordinator.async_stop)
    entry.async_on_unload(entry.add_update_listener(async_options_updated))
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_options_updated(
    hass: HomeAssistant, entry: PropertyConfigEntry
) -> None:
    """Reload so cached Assist tools cannot retain changed options."""
    await hass.config_entries.async_reload(entry.entry_id)


async def async_unload_entry(hass: HomeAssistant, entry: PropertyConfigEntry) -> bool:
    """Release native state without closing HA's shared HTTP session."""
    entry.runtime_data.async_stop()
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if not unloaded:
        entry.runtime_data.async_resume()
    else:
        entry.runtime_data.async_cleanup()
    return unloaded
