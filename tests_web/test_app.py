"""Tax evidence remains exact and inspectable through the real browser UI."""

import json
import re
from typing import Literal

import pytest
from aiohttp import web
from playwright.async_api import Page, Route, expect
from pytest_aiohttp.plugin import AiohttpServer

from tests_web.conftest import PropertyBackend
from tests_web.fixtures import evidence_input


async def login(
    page: Page, backend: PropertyBackend, *, integration: bool = False
) -> None:
    await page.goto(backend.origin + "/")
    await expect(
        page.get_by_role("heading", name="Connect to Property")
    ).to_be_visible()
    await page.get_by_label("API token", exact=True).fill(
        backend.integration_token if integration else backend.token
    )
    await page.get_by_role("button", name="Connect", exact=True).click()
    await expect(page.locator("#workspace")).to_be_visible()
    await expect(page.locator("#app-footer").get_by_role("status")).to_have_text(
        "Connected"
    )


async def paste_import(page: Page, evidence: dict[str, object]) -> None:
    await page.get_by_role("button", name="Import evidence", exact=True).first.click()
    await page.get_by_text("Paste structured evidence instead", exact=True).click()
    await page.get_by_label("Evidence JSON", exact=True).fill(json.dumps(evidence))
    await page.get_by_role("button", name="Preview import", exact=True).click()


async def test_empty_setup_then_reviewed_import_updates_all_views(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await expect(
        page.get_by_role("heading", name="Add a property", exact=True)
    ).to_be_visible()
    await expect(page.locator("#summary")).to_contain_text("Unknown")
    await paste_import(page, evidence_input())
    preview = page.locator("#import-preview")
    await expect(preview).to_be_visible()
    await expect(preview).to_contain_text("6 new observations")
    await expect(preview).to_contain_text("EXAMPLE-PARCEL-001")
    await expect(preview).to_contain_text("County posted")
    await expect(preview).to_contain_text("installment 1: $600.00")
    apply = page.get_by_role("button", name="Apply reviewed import")
    await expect(apply).to_be_disabled()
    await page.get_by_label("The county FIPS and parcel match").check()
    await apply.click()
    await expect(page.locator("#import-notice")).to_contain_text(
        "Import applied at revision 1"
    )
    await page.get_by_role("button", name="Close import", exact=True).click()
    await expect(page.locator("#property-label")).to_have_text("Example property")
    await expect(page.locator("#summary")).to_contain_text("$900.00")
    await expect(page.locator("#summary")).to_contain_text("2 installments due")
    await expect(page.locator("#attention-items > article")).to_have_count(4)
    missing = page.locator("#attention-items > article").filter(
        has=page.get_by_role("heading", name="1 bill needs a notice", exact=True)
    )
    await expect(missing).to_contain_text("FY 2030 · MISSING · version 1")
    for capability in ("Notices", "Assessments", "Balances", "Payments"):
        await expect(page.locator("#attention-items")).to_contain_text(
            capability + ": Partial"
        )
    await missing.get_by_role("button", name="View affected bill").click()
    await expect(
        page.get_by_role("dialog", name="Bill awaiting notice")
    ).to_contain_text("FY 2030 · MISSING · version 1")
    await page.keyboard.press("Escape")
    await page.get_by_role("button", name="View county coverage").first.click()
    await expect(page.locator("#property-view")).to_be_visible()
    await expect(page.locator("#sources-title")).to_be_focused()
    await page.get_by_role("button", name="Needs attention", exact=True).click()
    await expect(page.locator("#deadline-section")).not_to_be_visible()
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator("#installments article")).to_have_count(4)
    second = page.locator("#installments article").filter(
        has=page.get_by_role("heading", name="Installment 2", exact=True)
    )
    await expect(second).to_have_count(2)
    for card in await second.all():
        await expect(card).not_to_contain_text("Paid")
        await expect(card).to_contain_text("Unknown")
    await expect(page.locator("#bills article")).to_have_count(3)
    await expect(
        page.get_by_role("heading", name="Bill awaiting notice", exact=True)
    ).to_be_visible()
    await page.get_by_role(
        "button", name="View bill FY 2030 · ANNUAL · version 1"
    ).click()
    dialog = page.get_by_role("dialog", name="Annual bill", exact=True)
    await expect(
        dialog.get_by_role("heading", name="Evidence", exact=True)
    ).to_be_visible()
    await expect(dialog).to_contain_text("Payment total: $900.00")
    await expect(dialog).to_contain_text("This bill · installment 1: $600.00")
    await expect(dialog).not_to_contain_text("This bill · installment 2")
    await expect(dialog.get_by_text("Notice", exact=True)).to_be_visible()
    await expect(dialog.get_by_text("Payment", exact=True)).to_be_visible()
    await page.keyboard.press("Escape")
    await expect(dialog).not_to_be_visible()
    await page.get_by_role("button", name="Assessments", exact=True).click()
    await expect(page.locator("#assessments")).to_contain_text("$420,000.00")
    await expect(page.locator("#assessments")).to_contain_text(
        "Fictional assessment <script>not instructions</script>"
    )
    await expect(page.locator("#assessments script")).to_have_count(0)
    await page.get_by_role("button", name="Property", exact=True).click()
    await expect(page.locator("#binding")).to_contain_text("06037")
    await expect(page.locator("#coverage article")).to_have_count(4)
    await page.get_by_role("button", name="Check county source", exact=True).click()
    await expect(page.locator("#sync-status")).to_contain_text("unavailable")
    await expect(page.locator("#coverage")).to_contain_text("Unavailable")
    status, summary = await property_backend.request("GET", "/v1/summary")
    assert status == 200 and summary["payment_count"] == 1
    assert summary["known_balance_cents"] == 90000


async def test_input_changes_invalidate_preview_and_stale_apply_is_not_retried(
    property_backend: PropertyBackend, page: Page
) -> None:
    evidence = evidence_input()
    await property_backend.seed(evidence)
    await login(page, property_backend)
    await paste_import(page, evidence)
    await expect(page.locator("#import-preview")).to_be_visible()
    await expect(page.locator("#import-preview")).to_contain_text(
        re.compile(r"Preview hash: [a-f0-9]{64}")
    )
    await expect(
        page.locator("#import-preview").get_by_text(
            "Duplicate · already recorded or repeated in this file", exact=True
        )
    ).to_have_count(6)
    await expect(page.locator("#import-preview")).to_contain_text(
        "No new observations will be added"
    )
    await expect(page.locator("#import-preview")).to_contain_text(
        "FY 2030 · MISSING · version 1"
    )
    await page.get_by_label("Evidence JSON", exact=True).fill(
        json.dumps(evidence, indent=2)
    )
    await expect(page.locator("#import-preview")).not_to_be_visible()
    await page.get_by_role("button", name="Preview import", exact=True).click()
    await expect(page.locator("#import-preview")).to_be_visible()
    # A real independent import advances SQLite while the reviewed UI is open.
    await property_backend.seed(evidence, key="fictional-concurrent-editor")
    await page.get_by_role(
        "button", name="Apply reviewed import (no new observations)", exact=True
    ).click()
    await expect(page.locator("#import-notice")).to_contain_text("The ledger changed")
    _, property_state = await property_backend.request("GET", "/v1/property")
    assert property_state["revision"] == 2


async def test_invalid_json_and_unsafe_numbers_never_reach_preview(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await page.get_by_role("button", name="Import evidence", exact=True).first.click()
    await page.get_by_text("Paste structured evidence instead", exact=True).click()
    await page.get_by_label("Evidence JSON", exact=True).fill("{broken OCR evidence")
    await page.get_by_role("button", name="Preview import", exact=True).click()
    await expect(page.locator("#import-notice")).to_contain_text("not valid JSON")
    await page.get_by_label("Evidence JSON", exact=True).fill(
        '{"amount_cents":9007199254740993}'
    )
    await page.get_by_role("button", name="Preview import", exact=True).click()
    await expect(page.locator("#import-notice")).to_contain_text("exact whole integers")
    _, property_state = await property_backend.request("GET", "/v1/property")
    assert property_state["revision"] == 0


async def test_unknown_balance_and_missing_notice_are_not_zero_or_paid(
    property_backend: PropertyBackend, page: Page
) -> None:
    evidence = evidence_input()
    observations = evidence["observations"]
    assert isinstance(observations, list)
    evidence["observations"] = [
        observation
        for observation in observations
        if observation["id"] not in {"property-balance", "annual-notice"}
    ]
    await property_backend.seed(evidence)
    await login(page, property_backend)
    balance = page.locator(".metric").filter(has_text="Latest property balance")
    await expect(balance).to_contain_text("Unknown")
    await expect(balance).not_to_contain_text("$0.00")
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(
        page.locator("#bills").get_by_text("Missing notice", exact=True)
    ).to_have_count(2)
    await page.get_by_role(
        "button", name="View bill FY 2030 · ANNUAL · version 1"
    ).click()
    await expect(
        page.get_by_role("dialog", name="Bill awaiting notice")
    ).to_contain_text("does not establish the full bill")


@pytest.mark.parametrize(
    "status", ["submitted", "bank_settled", "reversed", "refunded"]
)
async def test_payment_lifecycle_is_visible_without_closing_installments(
    property_backend: PropertyBackend, page: Page, status: str
) -> None:
    evidence = evidence_input()
    observations = evidence["observations"]
    assert isinstance(observations, list)
    for observation in observations:
        assert isinstance(observation, dict)
        if observation["id"] != "split-payment":
            continue
        source = observation["evidence"]
        assert isinstance(source, dict)
        observation["evidence"] = {
            **source,
            "source": "bank" if status == "bank_settled" else "owner_document",
            "document": "Fictional payment lifecycle receipt",
        }
        tagged = observation["observation"]
        assert isinstance(tagged, dict)
        data = tagged["data"]
        assert isinstance(data, dict)
        data["status"] = status
    await property_backend.seed(evidence)
    await login(page, property_backend)
    await expect(
        page.locator("#installments .tag").get_by_text("Paid", exact=True)
    ).to_have_count(0)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator("#payments").get_by_role("heading")).to_have_text(
        status.replace("_", " ").capitalize()
    )
    await expect(
        page.get_by_role("button", name=re.compile(r"mark paid|pay now", re.IGNORECASE))
    ).to_have_count(0)


async def test_network_failure_keeps_the_last_verified_snapshot(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend)

    async def offline(route: Route) -> None:
        await route.abort("failed")

    await page.route("**/v1/**", offline)
    await page.get_by_role("button", name="Refresh Property", exact=True).click()
    await expect(page.locator("#notice")).to_contain_text(
        "previous snapshot is still shown"
    )
    await expect(page.locator("#summary")).to_contain_text("$900.00")
    await expect(page.locator("#app-footer").get_by_role("status")).to_contain_text(
        "Refresh failed"
    )
    await page.unroute("**/v1/**", offline)
    await page.get_by_role("button", name="Refresh Property", exact=True).click()
    await expect(page.locator("#app-footer").get_by_role("status")).to_have_text(
        "Connected"
    )
    await expect(page.locator("#notice")).to_be_empty()


async def test_uncertain_apply_reuses_the_reviewed_idempotency_key(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await paste_import(page, evidence_input())
    await expect(page.locator("#import-preview")).to_be_visible()
    await page.get_by_label("The county FIPS and parcel match").check()

    async def lose_response(route: Route) -> None:
        response = await route.fetch()
        assert response.ok
        await route.abort("failed")

    await page.route("**/v1/imports", lose_response, times=1)
    await page.get_by_role("button", name="Apply reviewed import").click()
    await expect(page.locator("#import-notice")).to_contain_text(
        "Check the ledger before retrying"
    )
    _, before_retry = await property_backend.request("GET", "/v1/property")
    assert before_retry["revision"] == 1
    await page.get_by_role("button", name="Apply reviewed import").click()
    await expect(page.locator("#import-notice")).to_contain_text(
        "Import applied at revision 1"
    )
    _, after_retry = await property_backend.request("GET", "/v1/property")
    assert after_retry["revision"] == 1


async def test_integration_credential_has_no_owner_controls(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend, integration=True)
    await expect(
        page.get_by_role("button", name="Import evidence", exact=True)
    ).not_to_be_visible()
    await page.get_by_role("button", name="Property", exact=True).click()
    await expect(
        page.get_by_role("button", name="Check county source", exact=True)
    ).to_be_disabled()
    # Token storage remains opt-in, in-memory only across a full reload.
    await page.reload()
    await expect(
        page.get_by_role("heading", name="Connect to Property")
    ).to_be_visible()


async def test_ingress_nontrailing_frame_path_uses_the_real_peer_listener(
    property_backend: PropertyBackend,
    page: Page,
    aiohttp_server: AiohttpServer,
) -> None:
    await property_backend.seed(evidence_input())
    prefix = "/api/hassio_ingress/fictional-session"
    paths: list[str] = []

    async def proxy(request: web.Request) -> web.Response:
        paths.append(request.path)
        assert request.path == prefix or request.path.startswith(prefix + "/")
        target = request.path.removeprefix(prefix) or "/"
        headers = {
            "X-Ingress-Path": prefix,
            "X-Remote-User-Id": "fictional-owner",
            **{
                name: request.headers[name]
                for name in ("Content-Type", "X-Property-Request", "If-Match")
                if name in request.headers
            },
        }
        async with property_backend.session.request(
            request.method,
            property_backend.ingress_origin + target,
            params=request.query,
            data=await request.read(),
            headers=headers,
        ) as upstream:
            return web.Response(
                status=upstream.status,
                body=await upstream.read(),
                headers={
                    name: upstream.headers[name]
                    for name in ("Content-Type", "Content-Security-Policy")
                    if name in upstream.headers
                },
            )

    app = web.Application()
    app.router.add_route("*", "/{path:.*}", proxy)
    server = await aiohttp_server(app)
    await page.goto(str(server.make_url(prefix)))
    await expect(page.locator("#app-footer").get_by_role("status")).to_have_text(
        "Connected"
    )
    await expect(page.locator("#property-label")).to_have_text("Example property")
    await expect(page.locator("#login")).not_to_be_visible()
    await page.get_by_role("button", name="Property", exact=True).click()
    await page.get_by_role("button", name="Check county source", exact=True).click()
    await expect(page.locator("#sync-status")).to_contain_text("unavailable")
    assert prefix + "/app.js" in paths
    assert prefix + "/kit.js" in paths
    assert prefix + "/v1/sync" in paths


@pytest.mark.parametrize(
    "width,theme", [(320, "light"), (390, "dark"), (1180, "light")]
)
async def test_responsive_navigation_keyboard_and_dialog_focus(
    property_backend: PropertyBackend,
    page: Page,
    width: int,
    theme: Literal["light", "dark"],
) -> None:
    await property_backend.seed(evidence_input())
    await page.set_viewport_size({"width": width, "height": 900})
    await page.emulate_media(color_scheme=theme)
    await login(page, property_backend)
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    bills = page.get_by_role("button", name="Bills", exact=True)
    await bills.focus()
    await page.keyboard.press("Enter")
    await expect(bills).to_have_attribute("aria-pressed", "true")
    await expect(
        page.get_by_role("heading", name="Bills", exact=True).first
    ).to_be_visible()
    detail = page.get_by_role("button", name="View bill FY 2030 · ANNUAL · version 1")
    await detail.focus()
    await page.keyboard.press("Enter")
    await expect(page.get_by_role("dialog", name="Annual bill")).to_be_visible()
    assert await page.evaluate("document.activeElement.closest('dialog') !== null")
    await page.keyboard.press("Escape")
    await expect(detail).to_be_focused()
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
