"""Bounded HTTP transport to the configured Property service."""

import json
from dataclasses import dataclass
from urllib.parse import urlsplit

from aiohttp import ClientSession, ClientTimeout

from .const import MAX_RESPONSE_BYTES, REQUEST_TIMEOUT_SECONDS


class InvalidBackendURL(ValueError):
    """The backend must be an HTTP origin without embedded credentials."""


class ResponseTooLarge(ValueError):
    """The service exceeded the adapter response limit."""


class InvalidCredential(ValueError):
    """A backend credential is malformed; its value is never included."""


def normalize_backend_url(value: str) -> str:
    """Accept one explicit origin; requests never select a destination."""
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError as err:
        raise InvalidBackendURL from err
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
        or any(character.isspace() for character in value)
        or "\\" in value
        or port == 0
    ):
        raise InvalidBackendURL
    return value.rstrip("/")


@dataclass(frozen=True, slots=True)
class BackendResponse:
    """The status, payload, and public protocol headers from the service."""

    status: int
    body: bytes
    headers: dict[str, str]

    def json(self) -> object:
        """Decode an upstream JSON document."""
        return json.loads(self.body)


class PropertyClient:
    """Forward only requests constructed by the adapter."""

    def __init__(self, session: ClientSession, backend_url: str, token: str) -> None:
        self._session = session
        self._backend_url = normalize_backend_url(backend_url)
        if (
            not isinstance(token, str)
            or not 32 <= len(token) <= 256
            or not all("!" <= character <= "~" for character in token)
        ):
            raise InvalidCredential
        self._token = token

    async def request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
        admin: bool = False,
    ) -> BackendResponse:
        """Contact a fixed backend without following redirects or leaking auth."""
        upstream_headers = {
            "Authorization": f"Bearer {self._token}",
            "Accept": "application/json",
        }
        if body:
            upstream_headers["Content-Type"] = "application/json"
        if admin:
            # Only the HA bridge sets this after checking the authenticated user.
            # Incoming request headers cannot supply or override the claim.
            upstream_headers["X-Property-Admin"] = "true"
        if headers:
            upstream_headers.update(
                (key, value)
                for key, value in headers.items()
                if key in {"If-Match", "If-None-Match", "Idempotency-Key"}
            )

        async with self._session.request(
            method,
            f"{self._backend_url}{path}",
            data=body,
            headers=upstream_headers,
            allow_redirects=False,
            timeout=ClientTimeout(total=REQUEST_TIMEOUT_SECONDS),
        ) as response:
            response_limit = MAX_RESPONSE_BYTES
            payload = bytearray()
            async for chunk in response.content.iter_chunked(16 * 1024):
                payload.extend(chunk)
                if len(payload) > response_limit:
                    raise ResponseTooLarge
            public_headers = {
                header: response.headers[header]
                for header in ("Content-Type", "ETag", "Retry-After", "Cache-Control")
                if header in response.headers
            }
            return BackendResponse(response.status, bytes(payload), public_headers)
