"""Cross-language contract through the shipped Rust server, CLI, and native HA."""

import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from pathlib import Path

import pytest
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import llm
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.setup import async_setup_component
from homeassistant.util.json import JsonObjectType
from pytest_homeassistant.common import MockConfigEntry, MockUser
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.property import PropertyConfigEntry
from custom_components.property.const import (
    CONF_ALLOW_ASSIST,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)
from tests_web.fixtures import evidence_input

OWNER = "fictional-owner-credential-at-least-32-characters"
INTEGRATION = "fictional-integration-credential-at-least-32-characters"


@dataclass(frozen=True)
class RustBackend:
    origin: str
    owner: str = field(default=OWNER, repr=False)
    integration: str = field(default=INTEGRATION, repr=False)


@pytest.fixture
async def rust_backend(
    tmp_path: Path, socket_enabled: None
) -> AsyncIterator[RustBackend]:
    """Start an isolated real database, and close it before fixture removal."""
    binary = Path(os.environ.get("PROPERTY_TEST_BINARY", "target/debug/property"))
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the actual binaries first: cargo build --locked --bins")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("PROPERTY_", "SUPERVISOR"))
    }
    environment.update(PROPERTY_API_TOKEN=OWNER, PROPERTY_INTEGRATION_TOKEN=INTEGRATION)
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        "--bind",
        "127.0.0.1:0",
        "--database",
        str(tmp_path / "property.sqlite3"),
        env=environment,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        assert process.stdout is not None
        async with asyncio.timeout(10):
            line = (await process.stdout.readline()).decode().strip()
        if not line.startswith("property_listening=127.0.0.1:"):
            pytest.fail("The real Property service did not start")
        yield RustBackend("http://" + line.removeprefix("property_listening="))
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                async with asyncio.timeout(10):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()


async def connect(
    hass: HomeAssistant, rust_backend: RustBackend, *, assist: bool = False
) -> PropertyConfigEntry:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration,
        },
        options={CONF_ALLOW_ASSIST: assist},
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    return entry


async def seed(
    hass: HomeAssistant,
    backend: RustBackend,
    source: dict[str, object] | None = None,
) -> None:
    session = async_get_clientsession(hass)
    headers = {"Authorization": f"Bearer {backend.owner}"}
    async with session.post(
        backend.origin + "/v1/imports/preview",
        headers=headers,
        json=source if source is not None else evidence_input(),
    ) as response:
        assert response.status == 200
        preview = await response.json()
        assert isinstance(preview, dict) and preview["revision"] == 0
    async with session.post(
        backend.origin + "/v1/imports",
        headers={**headers, "If-Match": '"0"'},
        json={
            "preview": preview,
            "if_match": 0,
            "idempotency_key": "fictional-ha-seed",
        },
    ) as response:
        assert response.status == 200


async def test_real_ledger_projects_exact_installments_and_outage_coverage_to_ha(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    await seed(hass, rust_backend)
    entry = await connect(hass, rust_backend)
    assert hass.states.is_state("sensor.property_latest_observed_balance", "900")
    assert hass.states.is_state("sensor.property_county_coverage", "incomplete")
    client = await hass_client()
    response = await client.get("/api/property/v1/summary")
    assert response.status == 200
    summary = await response.json()
    assert isinstance(summary, dict)
    installments = summary["installments"]
    assert len(installments) == 4
    assert all(item["state"] == "paid" for item in installments if item["number"] == 1)
    assert all(item["state"] != "paid" for item in installments if item["number"] == 2)
    assert any(bill["missing_notice"] for bill in summary["bills"])
    response = await client.get(
        "/api/calendars/calendar.property_tax_deadlines",
        params={"start": "2031-02-01T00:00:00Z", "end": "2031-02-03T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 2
    assert all("installment 2" in event["summary"] for event in events)
    response = await client.post("/api/property/v1/sync")
    assert response.status == 202
    run = await response.json()
    assert run["status"] == "running"
    async with asyncio.timeout(5):
        while run["status"] == "running":
            await asyncio.sleep(0.01)
            response = await client.get(f"/api/property/v1/sync/{run['id']}")
            assert response.status == 200
            run = await response.json()
    assert run["status"] == "unavailable"
    response = await client.get(f"/api/property/v1/sync/{run['id']}")
    assert response.status == 200 and (await response.json())["id"] == run["id"]
    await entry.runtime_data.async_refresh()
    assert len(entry.runtime_data.data.installments) == 4
    assert hass.states.is_state("sensor.property_latest_observed_balance", "900")
    assert hass.states.is_state("sensor.property_county_coverage", "incomplete")


async def test_real_assist_reads_canonical_unknown_balance_without_an_llm(
    hass: HomeAssistant, hass_user: MockUser, rust_backend: RustBackend
) -> None:
    await connect(hass, rust_backend, assist=True)
    async_expose_entity(hass, "conversation", "sensor.property_needs_attention", True)
    assert await async_setup_component(hass, "llm", {})
    caller = await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test",
            context=Context(user_id=hass_user.id),
            language="en",
            assistant="conversation",
            device_id=None,
        ),
    )
    result = await caller.async_call_tool(
        llm.ToolInput(tool_name="property__tax_balance", tool_args={})
    )
    assert not result.error and result.data["known_balance_cents"] is None
    attention = result.data["needs_attention"]
    assert isinstance(attention, int) and attention > 0


async def test_real_assist_consumes_long_observation_cursors_without_gaps(
    hass: HomeAssistant, hass_user: MockUser, rust_backend: RustBackend
) -> None:
    source = evidence_input()
    observations = source["observations"]
    assert isinstance(observations, list)
    assessment: dict[str, object] | None = None
    for item in observations:
        assert isinstance(item, dict)
        observation = item.get("observation")
        if isinstance(observation, dict) and observation.get("kind") == "assessment":
            assessment = item
            break
    assert assessment is not None
    expected_ids = [
        f"fictional-assessment-{index:03d}-".ljust(199, "x") for index in range(21)
    ]
    source["observations"] = [
        {**assessment, "id": identity} for identity in reversed(expected_ids)
    ]
    await seed(hass, rust_backend, source)
    await connect(hass, rust_backend, assist=True)
    async_expose_entity(hass, "conversation", "sensor.property_needs_attention", True)
    assert await async_setup_component(hass, "llm", {})
    caller = await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test",
            context=Context(user_id=hass_user.id),
            language="en",
            assistant="conversation",
            device_id=None,
        ),
    )
    cursor: str | None = None
    seen: list[str] = []
    pages = 0
    while True:
        arguments: JsonObjectType = {"limit": 7}
        if cursor is not None:
            arguments["page_token"] = cursor
        result = await caller.async_call_tool(
            llm.ToolInput(tool_name="property__assessments", tool_args=arguments)
        )
        assert not result.error, result.data
        records = result.data["items"]
        assert isinstance(records, list) and 1 <= len(records) <= 7
        for record in records:
            assert isinstance(record, dict)
            identity = record["id"]
            assert isinstance(identity, str) and len(identity.encode()) == 199
            assert identity not in seen
            seen.append(identity)
        pages += 1
        next_page = result.data["next_page"]
        if next_page is None:
            break
        assert isinstance(next_page, str) and 256 < len(next_page) <= 820
        cursor = next_page
    assert pages == 3
    assert seen == expected_ids


async def test_real_cli_uses_authenticated_ha_transport_and_reports_pagination(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_access_token: str,
    rust_backend: RustBackend,
    tmp_path: Path,
) -> None:
    await seed(hass, rust_backend)
    await connect(hass, rust_backend)
    client = await hass_client()
    binary = Path(
        os.environ.get("PROPERTY_TEST_CLI_BINARY", "target/debug/ha-property")
    )
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the actual CLI first: cargo build --locked --bins")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("PROPERTY_", "HOME_ASSISTANT_", "SUPERVISOR"))
    }
    environment["HOME_ASSISTANT_TOKEN"] = hass_access_token
    token_file = tmp_path / "fixture-ha-token"
    await asyncio.to_thread(token_file.write_text, hass_access_token)
    await asyncio.to_thread(token_file.chmod, 0o600)
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        "--transport",
        "home-assistant",
        "--url",
        str(client.make_url("/")),
        "--token-file",
        str(token_file),
        "tax",
        "bills",
        "list",
        "--limit",
        "1",
        env=environment,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(10):
            stdout, _ = await process.communicate()
    finally:
        if process.returncode is None:
            process.terminate()
            await process.wait()
    assert process.returncode == 0
    output = json.loads(stdout)
    assert output["status"] == 200
    assert output["etag"] == '"1"'
    assert len(output["body"]["items"]) == 1
    assert output["body"]["next_page"] is not None
