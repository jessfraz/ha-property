"""Bounded authenticated bridge to one configured Property service."""

import re
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import PropertyCoordinator

_ROUTES = (
    (
        re.compile(
            r"system|health|summary|tax/bills|tax/installments|tax/assessments|tax/payments"
        ),
        frozenset({"GET"}),
    ),
    (re.compile(r"property"), frozenset({"GET", "PATCH"})),
    (
        re.compile(r"tax/bills/[A-Za-z0-9_-]{1,128}|sync/[A-Za-z0-9_-]{1,128}"),
        frozenset({"GET"}),
    ),
    (re.compile(r"property/resolve|imports/preview|imports|sync"), frozenset({"POST"})),
)
_LIST_PATHS = {"tax/bills", "tax/installments", "tax/assessments", "tax/payments"}


def loaded_coordinator(hass: HomeAssistant) -> PropertyCoordinator | None:
    """Do not retain a coordinator after its entry starts unloading."""
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED:
            return entry.runtime_data
    return None


class PropertyView(HomeAssistantView):
    """Tax evidence is administrator-only through the REST bridge."""

    url = "/api/property/v1/{path:.*}"
    name = "api:property:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        user = request["hass_user"]
        if not user.is_active or not user.is_admin:
            raise web.HTTPForbidden
        route = next((item for item in _ROUTES if item[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        allowed = {"limit", "page_token"} if path in _LIST_PATHS else set()
        if (
            len(request.query_string) > 4096
            or request.query.keys() - allowed
            or any(len(request.query.getall(key)) != 1 for key in request.query)
        ):
            raise web.HTTPBadRequest(reason="Unsupported query parameters")
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable
        if (request.content_length or 0) > MAX_REQUEST_BYTES:
            raise web.HTTPRequestEntityTooLarge(
                max_size=MAX_REQUEST_BYTES, actual_size=request.content_length or 0
            )
        payload = bytearray()
        async for chunk in request.content.iter_chunked(16 * 1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        # Reading the request body is an IO boundary too.
        if loaded_coordinator(self._hass) is not coordinator:
            raise web.HTTPServiceUnavailable
        if not user.is_active or not user.is_admin:
            raise web.HTTPForbidden
        headers = {
            key: request.headers[key]
            for key in ("If-Match", "Idempotency-Key")
            if key in request.headers
        }
        backend_path = "/health" if path == "health" else f"/v1/{path}"
        if request.query:
            backend_path += "?" + urlencode(list(request.query.items()))
        try:
            response = await coordinator.client.request(
                request.method,
                backend_path,
                body=bytes(payload) or None,
                headers=headers,
                admin=True,
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if loaded_coordinator(self._hass) is not coordinator:
            # Never disclose a late read from an unloaded/replaced installation.
            raise web.HTTPServiceUnavailable
        if not user.is_active or not user.is_admin:
            raise web.HTTPForbidden
        if request.method in {"POST", "PATCH"} and response.status < 300:
            entry = coordinator.config_entry
            if entry is not None:
                entry.async_create_background_task(
                    self._hass, coordinator.async_request_refresh(), "Property refresh"
                )
        response_headers = {**response.headers, "Cache-Control": "no-store"}
        return web.Response(
            status=response.status, body=response.body, headers=response_headers
        )

    get = _proxy
    post = _proxy
    put = _proxy
    patch = _proxy
    delete = _proxy
