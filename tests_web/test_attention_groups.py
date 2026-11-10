"""Group repeated gaps while keeping each real ledger record reachable."""

from copy import deepcopy

from playwright.async_api import Page, expect

from tests_web.conftest import PropertyBackend
from tests_web.fixtures import evidence_input
from tests_web.test_app import login


def populated_history() -> dict[str, object]:
    """Several fictional years expose the repetition hidden by a tiny fixture."""
    data = evidence_input()
    observations = data["observations"]
    assert isinstance(observations, list)
    missing = next(row for row in observations if row["id"] == "missing-notice-balance")
    assessment = next(row for row in observations if row["id"] == "assessment")
    observations[:] = [row for row in observations if row["id"] != "property-balance"]
    for year in range(2021, 2026):
        balance = deepcopy(missing)
        balance["id"] = f"fictional-missing-{year}"
        balance["observation"]["data"]["scope"]["bill"]["fiscal_year"] = year
        balance["observation"]["data"]["scope"]["bill"]["source_version"] = (
            "unversioned"
        )
        annual = deepcopy(assessment)
        annual["id"] = f"fictional-assessment-{year}"
        annual["observation"]["data"]["fiscal_year"] = year
        observations.extend([balance, annual])
    return data


async def test_populated_ledger_groups_causes_without_losing_bill_actions(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(populated_history())
    await login(page, property_backend)
    groups = page.locator("#attention-items > .issue-group")
    await expect(groups).to_have_count(3)
    notices = groups.filter(
        has=page.get_by_role("heading", name="6 bills need notices", exact=True)
    )
    await expect(
        notices.get_by_role("button", name="View affected bill")
    ).to_have_count(6)
    await expect(notices.locator("li > span")).to_have_text(
        ["FY 2030 · MISSING · version 1"]
        + [f"FY {year} · MISSING" for year in range(2025, 2020, -1)]
    )
    await expect(notices).not_to_contain_text("Assessments without a notice:")
    await expect(
        page.get_by_role("button", name="View county coverage", exact=True)
    ).to_have_count(1)
    await expect(page.locator("#attention-items")).not_to_contain_text(
        "version unversioned"
    )
    _, summary = await property_backend.request("GET", "/v1/summary")
    await expect(page.locator("#summary")).to_contain_text(
        str(summary["needs_attention"])
    )
    await expect(page.locator("#summary")).to_contain_text("Unknown")
    row = notices.locator("li").filter(has_text="FY 2025")
    await row.get_by_role("button", name="View affected bill").click()
    await expect(
        page.get_by_role("dialog", name="Bill awaiting notice")
    ).to_contain_text("FY 2025 · MISSING")
    await page.keyboard.press("Escape")
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator("#summary")).not_to_be_visible()
    await expect(page.locator("#bills .bill-year > h2")).to_have_text(
        ["FY 2030", "FY 2025", "FY 2024", "FY 2023", "FY 2022", "FY 2021"]
    )
    await page.get_by_role("button", name="Assessments", exact=True).click()
    await expect(page.locator("#assessments .record-heading h3")).to_have_text(
        [f"FY {year} assessment" for year in [2030, 2025, 2024, 2023, 2022, 2021]]
    )
    await expect(page.locator("#assessments details[open]")).to_have_count(0)
    await page.get_by_role("button", name="Property", exact=True).click()
    await expect(page.locator("#coverage article")).to_have_count(4)
    await expect(page.locator("#coverage details[open]")).to_have_count(0)
    await page.get_by_text("Source details", exact=True).first.click()
    await expect(page.locator("#coverage")).to_contain_text(
        "Fictional imported evidence"
    )


async def test_partial_balance_coverage_does_not_erase_a_known_imported_balance(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend)
    await expect(page.locator("#summary")).to_contain_text("$900.00")
    await expect(
        page.get_by_role("heading", name="County balance not confirmed", exact=True)
    ).to_have_count(0)
    await expect(page.locator("#attention-items")).to_contain_text("Balances: Partial")


async def test_attention_shows_open_installments_once_and_keeps_history_on_bills(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(evidence_input())
    await login(page, property_backend)
    await expect(
        page.get_by_role("heading", name="Installment 2", exact=True)
    ).to_have_count(2)
    await expect(page.locator("#deadline-section")).not_to_be_visible()
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator("#deadline-section")).to_be_visible()
    await expect(page.locator("#installments article")).to_have_count(4)


async def test_optional_purchase_date_persists_and_can_restore_retained_history(
    property_backend: PropertyBackend, page: Page
) -> None:
    await property_backend.seed(populated_history())
    _, before = await property_backend.request("GET", "/v1/property")
    original = before["property"]
    assert isinstance(original, dict)
    revision = before["revision"]
    assert isinstance(revision, int)
    await login(page, property_backend)
    await page.get_by_role("button", name="Property", exact=True).click()
    bought = page.locator("#binding").get_by_label("Bought on (optional)", exact=True)
    await expect(bought).to_have_value("")
    await bought.fill("2024-06-30")
    await page.get_by_role("button", name="Save date", exact=True).click()
    await expect(page.locator("#notice")).to_have_text("History date saved.")
    _, saved = await property_backend.request("GET", "/v1/property")
    binding = saved["property"]
    assert isinstance(binding, dict)
    assert binding["key"] == original["key"]
    assert binding["confirmation"] == original["confirmation"]
    assert binding["bought_on"] == "2024-06-30"
    assert saved["revision"] == revision + 1
    await page.reload()
    await login(page, property_backend)
    await page.get_by_role("button", name="Property", exact=True).click()
    await expect(bought).to_have_value("2024-06-30")
    await bought.fill("")
    await page.get_by_role("button", name="Save date", exact=True).click()
    await expect(page.locator("#notice")).to_have_text("History date saved.")
    _, restored = await property_backend.request("GET", "/v1/property")
    binding = restored["property"]
    assert isinstance(binding, dict) and binding["bought_on"] is None
    await page.get_by_role("button", name="Assessments", exact=True).click()
    await expect(page.locator("#assessments .record")).to_have_count(6)
