"""Use the native version repair for this Property entry."""

from datetime import datetime, timedelta
from typing import TYPE_CHECKING

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.event import async_track_time_interval

from ._app_kit.repairs import VersionRepairs, VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .const import CONF_ADDON_SLUG, DOMAIN

if TYPE_CHECKING:
    from .coordinator import PropertyCoordinator


async def _async_probe(hass: HomeAssistant, entry: ConfigEntry) -> ReleaseVersion:
    coordinator: PropertyCoordinator = entry.runtime_data
    response = await coordinator.client.request("GET", "/v1/system")
    if response.status != 200:
        raise HomeAssistantError("Property version is unavailable")
    data = response.json()
    if (
        not isinstance(data, dict)
        or data.get("service") != DOMAIN
        or data.get("api_version") != 1
        or data.get("role") != "integration"
    ):
        raise HomeAssistantError("Property version is unavailable")
    return ReleaseVersion.parse(data.get("version"))


def _app_slug(entry: ConfigEntry) -> str | None:
    slug = entry.data.get(CONF_ADDON_SLUG)
    if slug is not None and not isinstance(slug, str):
        raise HomeAssistantError("Property app identity is invalid")
    return slug


VERSION_REPAIR = VersionRepairSpec(
    domain=DOMAIN,
    name="Property",
    repository_id="1452505203",
    loaded_version=ReleaseVersion.parse(LOADED_VERSION),
    probe=_async_probe,
    app_slug=_app_slug,
)


class VersionChecks(VersionRepairs):
    """Check version parity independently of the normal data refresh."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        super().__init__(hass, entry, VERSION_REPAIR)
        self._checking = False

    @callback
    def async_start(self) -> None:
        super().async_start()
        self.entry.async_on_unload(
            async_track_time_interval(self.hass, self._async_tick, timedelta(minutes=1))
        )

    @callback
    def _async_tick(self, _now: datetime) -> None:
        if self.entry.state is ConfigEntryState.LOADED and not self._checking:
            self.entry.async_create_background_task(
                self.hass, self.async_check(), "Check backend version"
            )

    async def async_check(self) -> None:
        # The loaded check and timer can meet. Only one may read the backend.
        if self._checking:
            return
        self._checking = True
        try:
            await super().async_check()
        finally:
            self._checking = False


async def async_create_fix_flow(
    hass: HomeAssistant, issue_id: str, data: dict[str, object] | None
) -> RepairsFlow:
    """Resolve the entry from the native issue registry."""
    return create_version_fix_flow(hass, issue_id, VERSION_REPAIR)
