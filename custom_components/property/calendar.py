"""Read-only jurisdiction-local due dates and exact delinquency deadlines."""

from datetime import datetime, time, timedelta

from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util

from .const import DOMAIN
from .coordinator import PropertyCoordinator


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry[PropertyCoordinator],
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities([PropertyCalendar(entry.runtime_data, entry.entry_id)])


class PropertyCalendar(CoordinatorEntity[PropertyCoordinator], CalendarEntity):
    """Payments and supersession are reconciled by Rust, never by HA or Assist."""

    _attr_has_entity_name = True
    _attr_name = "Tax deadlines"
    _attr_supported_features = 0

    def __init__(self, coordinator: PropertyCoordinator, entry_id: str) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry_id}_deadlines"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)},
            name="Property",
            manufacturer="Property",
            model="Tax ledger",
        )

    def _events(self) -> list[CalendarEvent]:
        events: list[CalendarEvent] = []
        for installment in self.coordinator.data.installments:
            if installment.state == "paid":
                continue
            due_start = datetime.combine(
                installment.due_date, time.min, installment.timezone
            )
            due_end = datetime.combine(
                installment.due_date + timedelta(days=1), time.min, installment.timezone
            )
            description = (
                f"Ledger state: {installment.state}. Jurisdiction timezone: "
                f"{installment.timezone.key}. Review Property for exact bill evidence. "
                "A submitted or bank-settled payment is not county posting."
            )
            events.extend(
                [
                    CalendarEvent(
                        start=due_start,
                        end=due_end,
                        summary=f"Tax installment {installment.number} due",
                        description=description,
                        uid=f"{installment.id}:due",
                    ),
                    CalendarEvent(
                        start=installment.delinquent_at,
                        end=installment.delinquent_at + timedelta(minutes=1),
                        summary=(
                            f"Tax installment {installment.number} becomes delinquent"
                        ),
                        description=description,
                        uid=f"{installment.id}:delinquent",
                    ),
                ]
            )
        return sorted(
            events, key=lambda item: (item.start_datetime_local, item.uid or "")
        )

    @property
    def event(self) -> CalendarEvent | None:
        return next(
            (
                event
                for event in self._events()
                if event.end_datetime_local > dt_util.now()
            ),
            None,
        )

    async def async_get_events(
        self, hass: HomeAssistant, start_date: datetime, end_date: datetime
    ) -> list[CalendarEvent]:
        if not self.available:
            raise HomeAssistantError("Property is unavailable")
        if start_date >= end_date:
            return []
        return [
            event
            for event in self._events()
            if event.start_datetime_local < end_date
            and event.end_datetime_local > start_date
        ]
