"""Native counts, observed balance, and evidence freshness."""

from datetime import datetime
from decimal import Decimal
from typing import override

from homeassistant.components.sensor import SensorDeviceClass, SensorEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo, EntityCategory
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import PropertyCoordinator


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[PropertyCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities(
        PropertySensor(entry.runtime_data, entry.entry_id, key)
        for key in ("needs_attention", "known_balance", "last_observed", "coverage")
    )


class PropertySensor(CoordinatorEntity[PropertyCoordinator], SensorEntity):
    """Unknown balances stay unknown, even with an otherwise healthy backend."""

    _attr_has_entity_name = True

    def __init__(
        self, coordinator: PropertyCoordinator, entry_id: str, key: str
    ) -> None:
        super().__init__(coordinator)
        self.key = key
        self._attr_unique_id = f"{entry_id}_{key}"
        self._attr_translation_key = key
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Property",
            manufacturer="Property",
            model="Tax ledger",
        )
        if key == "known_balance":
            self._attr_device_class = SensorDeviceClass.MONETARY
            self._attr_native_unit_of_measurement = "USD"
            self._attr_suggested_display_precision = 2
        elif key == "last_observed":
            self._attr_device_class = SensorDeviceClass.TIMESTAMP
            self._attr_entity_category = EntityCategory.DIAGNOSTIC
        elif key == "coverage":
            self._attr_entity_category = EntityCategory.DIAGNOSTIC
        self._attr_native_value = self._value()

    def _value(self) -> int | Decimal | str | datetime | None:
        data = self.coordinator.data
        match self.key:
            case "needs_attention":
                return data.needs_attention
            case "known_balance":
                return (
                    Decimal(data.known_balance_cents) / 100
                    if data.known_balance_cents is not None
                    else None
                )
            case "last_observed":
                return data.last_observed_at
            case "coverage":
                return "incomplete" if data.coverage_gap else "complete"
        return None

    @callback
    @override
    def _handle_coordinator_update(self) -> None:
        """Publish the current Rust projection with no local ledger changes."""
        self._attr_native_value = self._value()
        super()._handle_coordinator_update()
