"""Poll a single atomic projection; source outages never become empty data."""

import logging
from datetime import datetime, timedelta

from aiohttp import ClientError
from homeassistant.components import persistent_notification
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from .client import PropertyClient, ResponseTooLarge
from .const import CONF_NOTIFICATIONS, DOMAIN
from .models import PropertyData

_LOGGER = logging.getLogger(__name__)


class PropertyCoordinator(DataUpdateCoordinator[PropertyData]):
    """Native HA state is derived only from the canonical ledger projection."""

    def __init__(
        self, hass: HomeAssistant, entry: ConfigEntry, client: PropertyClient
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(seconds=60),
            always_update=False,
        )
        self.client = client
        self.entry = entry
        self.last_success_at: datetime | None = None
        self._stopped = False
        self._last_notified: int | None = None

    @callback
    def async_stop(self) -> None:
        """Ignore late IO and notification publication once unload starts."""
        self._stopped = True

    @callback
    def async_resume(self) -> None:
        """Retain native polling if HA could not unload its platforms."""
        self._stopped = False

    @callback
    def async_cleanup(self) -> None:
        """Remove installation-owned notices only after a successful unload."""
        for suffix in ("backend", "coverage"):
            ir.async_delete_issue(self.hass, DOMAIN, f"{self.entry.entry_id}_{suffix}")
        persistent_notification.async_dismiss(
            self.hass, f"property_{self.entry.entry_id}_attention"
        )

    @property
    def stopped(self) -> bool:
        """Whether the owning installation has begun unloading."""
        return self._stopped

    async def _async_update_data(self) -> PropertyData:
        try:
            response = await self.client.request("GET", "/v1/summary")
            if self._stopped:
                raise UpdateFailed("Property was unloaded")
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed(
                    "Property rejected its integration credential"
                )
            if response.status != 200:
                raise UpdateFailed("Property could not return its ledger projection")
            data = PropertyData.parse(response.json())
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            self._backend_issue(True)
            raise UpdateFailed(
                "Property could not return a valid ledger projection"
            ) from err
        except ConfigEntryAuthFailed:
            self._backend_issue(False)
            raise
        except UpdateFailed:
            self._backend_issue(True)
            raise
        self.last_success_at = dt_util.utcnow()
        self._backend_issue(False)
        if not self._stopped:
            issue_id = f"{self.entry.entry_id}_coverage"
            if data.coverage_gap:
                ir.async_create_issue(
                    self.hass,
                    DOMAIN,
                    issue_id,
                    config_entry_id=self.entry.entry_id,
                    is_fixable=False,
                    is_persistent=True,
                    severity=ir.IssueSeverity.WARNING,
                    translation_key="coverage_gap",
                )
            else:
                ir.async_delete_issue(self.hass, DOMAIN, issue_id)
            self._notify(data)
        return data

    @callback
    def _backend_issue(self, failed: bool) -> None:
        if self._stopped:
            return
        issue_id = f"{self.entry.entry_id}_backend"
        if failed:
            ir.async_create_issue(
                self.hass,
                DOMAIN,
                issue_id,
                config_entry_id=self.entry.entry_id,
                is_fixable=False,
                is_persistent=True,
                severity=ir.IssueSeverity.WARNING,
                translation_key="backend_unavailable",
            )
        else:
            ir.async_delete_issue(self.hass, DOMAIN, issue_id)

    @callback
    def _notify(self, data: PropertyData) -> None:
        notification_id = f"property_{self.entry.entry_id}_attention"
        if not self.entry.options.get(CONF_NOTIFICATIONS, True):
            persistent_notification.async_dismiss(self.hass, notification_id)
            self._last_notified = None
        elif data.needs_attention == 0:
            persistent_notification.async_dismiss(self.hass, notification_id)
            self._last_notified = 0
        elif self._last_notified != data.needs_attention:
            persistent_notification.async_create(
                self.hass,
                "Property tax evidence needs attention. Review Property for "
                "missing evidence, balances, and deadlines. No payment has been made.",
                title="Property needs attention",
                notification_id=notification_id,
            )
            self._last_notified = data.needs_attention
