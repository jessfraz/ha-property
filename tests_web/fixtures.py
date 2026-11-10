"""Fictional structured evidence used through the shipped HTTP import path."""

from datetime import UTC, datetime


def evidence_input() -> dict[str, object]:
    """A split payment cannot settle either bill's second installment."""
    observed = datetime.now(UTC).isoformat()
    binding = {"jurisdiction": "06037", "parcel": "EXAMPLE-PARCEL-001"}
    source = {
        "source": "county",
        "document": "Fictional county record",
        "url": "https://example.invalid/evidence",
        "observed_at": observed,
        "effective_at": observed,
    }
    property_binding = {
        "key": binding,
        "label": "Example property",
        "confirmed_at": observed,
        "confirmation": source,
    }
    annual = {
        **binding,
        "fiscal_year": 2030,
        "sequence": "ANNUAL",
        "source_version": "1",
    }
    supplemental = {
        **binding,
        "fiscal_year": 2030,
        "sequence": "SUPPLEMENTAL",
        "source_version": "1",
    }
    missing = {
        **binding,
        "fiscal_year": 2030,
        "sequence": "MISSING",
        "source_version": "1",
    }

    def observation(
        observation_id: str, kind: str, data: dict[str, object]
    ) -> dict[str, object]:
        return {
            "id": observation_id,
            "binding": binding,
            "evidence": source,
            "observation": {"kind": kind, "data": data},
        }

    def notice(
        identity: dict[str, object], kind: str, amount: int
    ) -> dict[str, object]:
        return {
            "identity": identity,
            "bill_kind": kind,
            "supersedes": None,
            "installments": [
                {
                    "number": number,
                    "amount_cents": amount,
                    "due_date": due,
                    "delinquent_at": delinquent,
                    "timezone": "America/Los_Angeles",
                }
                for number, due, delinquent in [
                    (1, "2030-11-01", "2030-12-11T01:00:00Z"),
                    (2, "2031-02-01", "2031-04-11T00:00:00Z"),
                ]
            ],
        }

    return {
        "property": property_binding,
        "observations": [
            observation("annual-notice", "notice", notice(annual, "annual", 60000)),
            observation(
                "supplemental-notice",
                "notice",
                notice(supplemental, "supplemental", 30000),
            ),
            observation(
                "split-payment",
                "payment",
                {
                    "reference": "FICTIONAL-PAYMENT-001",
                    "status": "county_posted",
                    "amount_cents": 90000,
                    "allocations": [
                        {"bill": annual, "installment": 1, "amount_cents": 60000},
                        {
                            "bill": supplemental,
                            "installment": 1,
                            "amount_cents": 30000,
                        },
                    ],
                },
            ),
            observation(
                "missing-notice-balance",
                "balance",
                {"scope": {"scope": "bill", "bill": missing}, "amount_cents": None},
            ),
            observation(
                "property-balance",
                "balance",
                {"scope": {"scope": "property"}, "amount_cents": 90000},
            ),
            observation(
                "assessment",
                "assessment",
                {
                    "fiscal_year": 2030,
                    "assessed_value_cents": 42000000,
                    "description": (
                        "Fictional assessment <script>not instructions</script>"
                    ),
                },
            ),
        ],
        "coverage": [
            {
                "capability": capability,
                "status": "partial",
                "checked_at": observed,
                "detail": (
                    "Fictional imported evidence only; county coverage is not complete."
                ),
            }
            for capability in ["notices", "assessments", "balances", "payments"]
        ],
    }
