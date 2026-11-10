"""Run browser contracts against the shipped Rust service and SQLite."""

import asyncio
import os
import socket
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from aiohttp import ClientSession
from playwright.async_api import Page, async_playwright


@dataclass
class PropertyBackend:
    """A fresh local backend, never a configured provider or live HA service."""

    origin: str
    ingress_origin: str
    session: ClientSession
    token: str = "fictional-property-owner-token-00000000000000000000"
    integration_token: str = "fictional-property-integration-token-000000000000"

    async def request(
        self,
        method: str,
        path: str,
        body: object = None,
        headers: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, object]]:
        """Read actual API output without fixtures replacing the implementation."""
        async with self.session.request(
            method,
            self.origin + path,
            json=body,
            headers={"Authorization": f"Bearer {self.token}", **(headers or {})},
        ) as response:
            result = await response.json()
            assert isinstance(result, dict)
            return response.status, result

    async def seed(
        self, evidence: dict[str, object], key: str = "fictional-ui-seed"
    ) -> None:
        """Every seed follows preview, revision and idempotent apply."""
        status, preview = await self.request("POST", "/v1/imports/preview", evidence)
        assert status == 200, preview
        status, result = await self.request(
            "POST",
            "/v1/imports",
            {
                "preview": preview,
                "if_match": preview["revision"],
                "idempotency_key": key,
            },
            {"If-Match": f'"{preview["revision"]}"'},
        )
        assert status in {200, 201}, result


@pytest.fixture
async def property_backend(
    tmp_path: Path, socket_enabled: None
) -> AsyncIterator[PropertyBackend]:
    """Keep native service state and credentials isolated per test."""
    binary = Path(os.environ.get("PROPERTY_TEST_BINARY", "target/debug/property"))
    assert await asyncio.to_thread(binary.is_file), (
        "Build the actual Property binary before browser tests."
    )
    binary = await asyncio.to_thread(binary.resolve)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        ingress_port = reservation.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    ingress_origin = f"http://127.0.0.1:{ingress_port}"
    token = "fictional-property-owner-token-00000000000000000000"
    integration_token = "fictional-property-integration-token-000000000000"
    environment = {
        **{
            name: value
            for name, value in os.environ.items()
            if not name.startswith("PROPERTY_") and name != "SUPERVISOR_TOKEN"
        },
        "PROPERTY_API_TOKEN": token,
        "PROPERTY_INTEGRATION_TOKEN": integration_token,
    }
    process = await asyncio.create_subprocess_exec(
        str(binary),
        "--bind",
        f"127.0.0.1:{port}",
        "--database",
        str(tmp_path / "property.sqlite3"),
        "--ingress-bind",
        f"127.0.0.1:{ingress_port}",
        "--ingress-peer",
        "127.0.0.1",
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        async with ClientSession() as session:
            backend = PropertyBackend(
                origin, ingress_origin, session, token, integration_token
            )
            for _ in range(100):
                if process.returncode is not None:
                    pytest.fail(
                        "The actual Property backend exited before becoming ready."
                    )
                try:
                    async with session.get(origin + "/health") as response:
                        if response.status == 200:
                            break
                except OSError:
                    pass
                await asyncio.sleep(0.05)
            else:
                pytest.fail("The actual Property backend did not become ready.")
            yield backend
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                await asyncio.wait_for(process.wait(), 10)
            except TimeoutError:
                process.kill()
                await process.wait()


@pytest.fixture
async def page(socket_enabled: None) -> AsyncIterator[Page]:
    """Each case owns and closes a disposable browser, not a user's Chrome tab."""
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            context = await browser.new_context(viewport={"width": 1180, "height": 900})
            yield await context.new_page()
        finally:
            await browser.close()
