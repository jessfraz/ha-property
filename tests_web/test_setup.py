"""One-time county binding and resumable sync use the shipped ledger paths."""

import asyncio
import re
from datetime import UTC, datetime

import pytest
from playwright.async_api import Page, Request, Route, expect

from tests_web.conftest import PropertyBackend
from tests_web.fixtures import evidence_input
from tests_web.test_app import login


def parcel_matches(count: int = 2, *, truncated: bool = False) -> dict[str, object]:
    """Only the source lookup response is fictional; admission stays real."""
    observed = datetime.now(UTC).isoformat()
    evidence = {
        "source": "county",
        "document": "Fictional published county parcel roll",
        "url": "https://example.invalid/county-parcel-record",
        "observed_at": observed,
        "effective_at": "2021-07-01T00:00:00Z",
    }
    return {
        "candidates": [
            {
                "key": {
                    "jurisdiction": "06037",
                    "parcel": f"EXAMPLE-PARCEL-{letter}",
                },
                "address": f"100 Example Street Unit {letter}",
                "roll_year": 2021,
                "evidence": evidence,
            }
            for letter in ("A", "B")[:count]
        ],
        "coverage": {
            "capability": "assessments",
            "status": "partial",
            "checked_at": observed,
            "detail": "Fictional published roll is older than the current year.",
        },
        "truncated": truncated,
    }


async def lookup(page: Page) -> None:
    await page.get_by_role("button", name="Find county parcel", exact=True).click()
    await page.get_by_role("radio", name="Street address", exact=True).check()
    await page.get_by_role("textbox", name="Street address", exact=True).fill(
        "100 Example Street"
    )
    await page.get_by_role("button", name="Search county records", exact=True).click()


async def reviewed_binding(page: Page) -> None:
    async def source(route: Route) -> None:
        assert route.request.method == "POST"
        assert route.request.post_data_json == {"address": "100 Example Street"}
        await route.fulfill(json=parcel_matches())

    await page.route("**/v1/property/resolve", source)
    await lookup(page)
    await page.get_by_role("radio", name=re.compile("EXAMPLE-PARCEL-B")).check()
    await page.get_by_label(
        "The county and selected parcel match this property"
    ).check()
    await page.get_by_label("Property label", exact=True).fill("Example property")
    await page.get_by_role(
        "button", name="Review property confirmation", exact=True
    ).click()
    await expect(page.locator("#property-preview")).to_be_visible()


async def test_parcel_number_is_the_default_and_keeps_explicit_confirmation(
    property_backend: PropertyBackend, page: Page
) -> None:
    calls: list[object] = []
    response = parcel_matches(1)
    candidates = response["candidates"]
    assert isinstance(candidates, list)
    candidates[0]["key"]["parcel"] = "0000000001"

    async def source(route: Route) -> None:
        calls.append(route.request.post_data_json)
        await route.fulfill(json=response)

    await page.route("**/v1/property/resolve", source)
    await login(page, property_backend)
    await page.get_by_role("button", name="Find county parcel", exact=True).click()
    await expect(
        page.get_by_role("radio", name="Parcel number (APN)", exact=True)
    ).to_be_checked()
    await expect(
        page.get_by_role("textbox", name="Street address", exact=True)
    ).not_to_be_visible()
    await page.get_by_role("textbox", name="Parcel number (APN)", exact=True).fill(
        "0000-000-001"
    )
    await page.get_by_role("button", name="Search county records", exact=True).click()
    await expect(
        page.get_by_role("radio", name=re.compile("0000000001"))
    ).to_be_visible()
    await expect(page.locator("#parcel-options input:checked")).to_have_count(0)
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["property"] is None and state["revision"] == 0
    assert calls == [{"parcel": "0000-000-001"}]
    await page.get_by_role("radio", name=re.compile("0000000001")).check()
    await page.get_by_label("Bought on (optional)", exact=True).fill("2024-06-30")
    await page.get_by_label(
        "The county and selected parcel match this property"
    ).check()
    await page.get_by_role(
        "button", name="Review property confirmation", exact=True
    ).click()
    await expect(page.locator("#property-preview")).to_contain_text("Jun 30, 2024")
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#notice")).to_contain_text("Property confirmed")
    _, confirmed = await property_backend.request("GET", "/v1/property")
    binding = confirmed["property"]
    assert isinstance(binding, dict)
    assert binding["bought_on"] == "2024-06-30"
    assert confirmed["revision"] == 1


@pytest.mark.parametrize("count", [1, 2])
async def test_county_candidates_require_explicit_selection_review_and_real_apply(
    property_backend: PropertyBackend, page: Page, count: int
) -> None:
    if count == 2:
        await page.set_viewport_size({"width": 390, "height": 900})
        await page.emulate_media(color_scheme="dark")
    calls: list[object] = []

    async def source(route: Route) -> None:
        calls.append(route.request.post_data_json)
        await route.fulfill(json=parcel_matches(count))

    await page.route("**/v1/property/resolve", source)
    await login(page, property_backend)
    await lookup(page)
    await expect(page.get_by_label("County", exact=True)).to_have_value("06037")
    await expect(page.get_by_label("County", exact=True)).to_be_disabled()
    await expect(page.locator("#parcel-options input:checked")).to_have_count(0)
    await expect(page.locator("#parcel-options").get_by_role("radio")).to_have_count(
        count
    )
    await expect(page.locator("#lookup-coverage")).to_contain_text("may be stale")
    await expect(page.locator("#parcel-options")).to_contain_text("2021")
    await expect(page.locator("#parcel-options")).to_contain_text("06037")
    await expect(
        page.get_by_role("button", name="Review property confirmation", exact=True)
    ).to_be_disabled()
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0 and state["property"] is None
    letter = "B" if count == 2 else "A"
    await page.get_by_role("radio", name=re.compile(f"EXAMPLE-PARCEL-{letter}")).check()
    await expect(
        page.get_by_role("button", name="Review property confirmation", exact=True)
    ).to_be_disabled()
    await page.get_by_label(
        "The county and selected parcel match this property"
    ).check()
    await page.get_by_label("Property label", exact=True).fill("Example property")
    await page.get_by_role(
        "button", name="Review property confirmation", exact=True
    ).click()
    review = page.locator("#property-preview")
    await expect(review).to_be_visible()
    await expect(review).to_contain_text(f"EXAMPLE-PARCEL-{letter}")
    await expect(review).to_contain_text(f"100 Example Street Unit {letter}")
    await expect(review).to_contain_text("Published roll")
    await expect(review).to_contain_text("2021")
    await expect(review).to_contain_text("may be stale")
    await expect(review).to_contain_text(re.compile(r"Preview hash: [a-f0-9]{64}"))
    await expect(review).to_contain_text("does not establish a tax balance")
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0 and state["property"] is None
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#binding")).to_contain_text(f"EXAMPLE-PARCEL-{letter}")
    await expect(page.locator("#property-setup")).not_to_be_visible()
    await expect(page.locator("#notice")).to_contain_text("Property confirmed")
    await expect(page.locator("#binding")).to_be_focused()
    await expect(page.locator("#overview-note")).to_have_text(
        "No current county balance or installment dates are on file."
    )
    await expect(page.locator("#summary")).not_to_contain_text("$0.00")
    _, state = await property_backend.request("GET", "/v1/property")
    binding = state["property"]
    assert isinstance(binding, dict)
    key = binding["key"]
    assert isinstance(key, dict) and key["parcel"] == f"EXAMPLE-PARCEL-{letter}"
    assert state["revision"] == 1
    assert calls == [{"address": "100 Example Street"}]


async def test_truncated_or_empty_matches_never_confirm_a_property(
    property_backend: PropertyBackend, page: Page
) -> None:
    response = parcel_matches(truncated=True)

    async def source(route: Route) -> None:
        await route.fulfill(json=response)

    await page.route("**/v1/property/resolve", source)
    await login(page, property_backend)
    await lookup(page)
    await expect(page.locator("#setup-notice")).to_contain_text(
        "Narrow the street address"
    )
    for radio in await page.locator("#parcel-options").get_by_role("radio").all():
        await expect(radio).to_be_disabled()
    await expect(page.locator("#property-confirmation")).not_to_be_visible()
    response = parcel_matches(0)
    await page.get_by_role("textbox", name="Street address", exact=True).fill(
        "200 Example Street"
    )
    await page.get_by_role("button", name="Search county records", exact=True).click()
    await expect(page.locator("#setup-notice")).to_contain_text(
        "property remains unconfirmed"
    )
    await expect(page.locator("#setup-notice")).to_contain_text("does not mean")
    await expect(page.locator("#parcel-options").get_by_role("radio")).to_have_count(0)
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0 and state["property"] is None


async def test_lookup_error_preserves_address_without_ledger_writes(
    property_backend: PropertyBackend, page: Page
) -> None:
    async def unavailable(route: Route) -> None:
        await route.fulfill(status=503, body="untrusted upstream body")

    await page.route("**/v1/property/resolve", unavailable)
    await login(page, property_backend)
    await lookup(page)
    await expect(page.locator("#setup-notice")).to_contain_text("HTTP 503")
    await expect(page.locator("#setup-notice")).not_to_contain_text(
        "untrusted upstream"
    )
    await expect(
        page.get_by_role("textbox", name="Street address", exact=True)
    ).to_have_value("100 Example Street")
    await expect(page.locator("#parcel-options").get_by_role("radio")).to_have_count(0)
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0 and state["property"] is None


async def test_a_foreign_county_response_is_not_a_confirmable_candidate(
    property_backend: PropertyBackend, page: Page
) -> None:
    response = parcel_matches(1)
    candidates = response["candidates"]
    assert isinstance(candidates, list)
    key = candidates[0]["key"]
    assert isinstance(key, dict)
    key["jurisdiction"] = "06059"

    async def source(route: Route) -> None:
        await route.fulfill(json=response)

    await page.route("**/v1/property/resolve", source)
    await login(page, property_backend)
    await lookup(page)
    await expect(page.locator("#setup-notice")).to_contain_text("county-bound parcel")
    await expect(page.locator("#parcel-options").get_by_role("radio")).to_have_count(0)
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0 and state["property"] is None


async def test_a_changed_property_label_invalidates_the_exact_setup_preview(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await reviewed_binding(page)
    await page.get_by_label("Property label", exact=True).fill("Updated example label")
    await expect(page.locator("#property-preview")).not_to_be_visible()
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 0


async def test_confirmation_works_on_a_normal_insecure_ha_origin(
    property_backend: PropertyBackend, page: Page
) -> None:
    origin = "http://example.invalid"

    async def proxy(route: Route) -> None:
        path = route.request.url.removeprefix(origin)
        response = await route.fetch(url=property_backend.origin + path)
        await route.fulfill(response=response)

    await page.route(origin + "/**", proxy)
    await page.goto(origin + "/")
    assert not await page.evaluate("isSecureContext")
    await page.get_by_label("API token", exact=True).fill(property_backend.token)
    await page.get_by_role("button", name="Connect", exact=True).click()
    await expect(page.locator("#workspace")).to_be_visible()
    await reviewed_binding(page)
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#notice")).to_contain_text("Property confirmed")
    _, state = await property_backend.request("GET", "/v1/property")
    assert state["revision"] == 1


async def test_concurrent_binding_cannot_be_overwritten_by_reviewed_setup(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await reviewed_binding(page)
    await property_backend.seed(evidence_input())
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#setup-notice")).to_contain_text("The ledger changed")
    _, state = await property_backend.request("GET", "/v1/property")
    binding = state["property"]
    assert isinstance(binding, dict)
    key = binding["key"]
    assert isinstance(key, dict) and key["parcel"] == "EXAMPLE-PARCEL-001"
    assert state["revision"] == 1


async def test_uncertain_confirmation_retries_only_the_same_preview_and_key(
    property_backend: PropertyBackend, page: Page
) -> None:
    await login(page, property_backend)
    await reviewed_binding(page)

    async def lose_response(route: Route) -> None:
        response = await route.fetch()
        assert response.ok
        await route.abort("failed")

    await page.route("**/v1/imports", lose_response, times=1)
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#setup-notice")).to_contain_text("before retrying")
    _, before = await property_backend.request("GET", "/v1/property")
    assert before["revision"] == 1
    await page.get_by_role("button", name="Confirm this property", exact=True).click()
    await expect(page.locator("#notice")).to_contain_text("Property confirmed")
    _, after = await property_backend.request("GET", "/v1/property")
    assert after["revision"] == 1


async def test_sync_poll_failure_resumes_the_existing_receipt_without_resubmission(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend)
    await page.get_by_role("button", name="Property", exact=True).click()
    posts: list[str] = []
    reads: list[str] = []

    def record(request: Request) -> None:
        if request.method == "POST" and request.url.endswith("/v1/sync"):
            posts.append(request.url)
        if request.method == "GET" and "/v1/sync/" in request.url:
            reads.append(request.url)

    page.on("request", record)

    async def offline(route: Route) -> None:
        await route.abort("failed")

    await page.route("**/v1/sync/*", offline)
    async with page.expect_response("**/v1/sync") as receipt:
        await page.get_by_role("button", name="Check county source", exact=True).click()
    response = await receipt.value
    assert response.status == 202
    run = await response.json()
    assert isinstance(run, dict) and run["status"] == "running"
    await expect(page.locator("#sync-status")).to_contain_text(
        "No new check has been submitted"
    )
    await expect(page.locator("#sync-status")).to_contain_text(run["id"])
    await page.unroute("**/v1/sync/*", offline)
    await page.get_by_role("button", name="Resume county check", exact=True).click()
    await expect(page.locator("#sync-status")).to_contain_text("unavailable")
    assert len(posts) == 1
    assert reads and all(url.endswith("/" + run["id"]) for url in reads)


async def test_sync_display_is_bounded_and_never_calls_running_work_completed(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend)
    await page.get_by_role("button", name="Property", exact=True).click()
    await page.clock.install()
    polling = asyncio.Event()
    release = asyncio.Event()

    async def delayed(route: Route) -> None:
        polling.set()
        await release.wait()
        await route.continue_()

    await page.route("**/v1/sync/*", delayed)
    try:
        async with page.expect_response("**/v1/sync") as receipt:
            await page.get_by_role(
                "button", name="Check county source", exact=True
            ).click()
        response = await receipt.value
        assert response.status == 202
        await expect(page.locator("#sync-status")).to_contain_text("still running")
        await page.clock.fast_forward(750)
        await asyncio.wait_for(polling.wait(), 5)
        await expect(page.locator("#sync-status")).not_to_contain_text("completed")
        await page.clock.fast_forward(120001)
        await expect(
            page.get_by_role("button", name="Resume county check", exact=True)
        ).to_be_enabled()
        await expect(page.locator("#sync-status")).to_contain_text(
            "no new check will be submitted"
        )
    finally:
        release.set()
        await page.unroute_all(behavior="wait")
