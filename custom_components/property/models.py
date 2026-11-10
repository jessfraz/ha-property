"""Validate the deterministic backend projection used by native entities."""

from dataclasses import dataclass
from datetime import date, datetime, timedelta
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError


@dataclass(frozen=True, slots=True)
class Installment:
    """One exact active bill-version installment, never a guessed payment."""

    id: str
    number: int
    due_date: date
    delinquent_at: datetime
    timezone: ZoneInfo
    state: str

    @classmethod
    def parse(cls, value: object) -> Installment:
        if not isinstance(value, dict):
            raise ValueError("Invalid installment")
        identity = value.get("id")
        number = value.get("number")
        due = value.get("due_date")
        delinquent = value.get("delinquent_at")
        timezone = value.get("timezone")
        state = value.get("state")
        if (
            not isinstance(identity, str)
            or not identity
            or len(identity) > 200
            or type(number) is not int
            or not 1 <= number <= 255
            or not isinstance(due, str)
            or not isinstance(delinquent, str)
            or not isinstance(timezone, str)
            or not isinstance(state, str)
            or state
            not in {
                "unknown",
                "open",
                "pending_payment",
                "paid",
                "delinquent",
                "contradiction",
            }
        ):
            raise ValueError("Invalid installment fields")
        try:
            due_date = date.fromisoformat(due)
            delinquent_at = datetime.fromisoformat(delinquent)
            zone = ZoneInfo(timezone)
        except (ValueError, ZoneInfoNotFoundError) as err:
            raise ValueError("Invalid installment date or timezone") from err
        if (
            due_date == date.max
            or delinquent_at.year == 9999
            or delinquent_at.utcoffset() != timedelta(0)
        ):
            raise ValueError("Delinquency timestamp must be UTC")
        return cls(identity, number, due_date, delinquent_at, zone, state)


@dataclass(frozen=True, slots=True)
class PropertyData:
    """A complete, single-revision backend summary for all native projections."""

    revision: int
    needs_attention: int
    known_balance_cents: int | None
    last_observed_at: datetime | None
    coverage_gap: bool
    installments: tuple[Installment, ...]

    @classmethod
    def parse(cls, value: object) -> PropertyData:
        if not isinstance(value, dict):
            raise ValueError("Invalid summary")
        revision = value.get("revision")
        attention = value.get("needs_attention")
        balance = value.get("known_balance_cents")
        last_observed = value.get("last_observed_at")
        installments = value.get("installments")
        coverage = value.get("coverage")
        issues = value.get("issues")
        if (
            type(revision) is not int
            or revision < 0
            or type(attention) is not int
            or attention < 0
            or (balance is not None and (type(balance) is not int or balance < 0))
            or (last_observed is not None and not isinstance(last_observed, str))
            or not isinstance(installments, list)
            or len(installments) > 12000
            or not isinstance(coverage, list)
            or len(coverage) > 4
            or not isinstance(issues, list)
            or len(issues) > 20000
        ):
            raise ValueError("Invalid summary fields")
        last = (
            datetime.fromisoformat(last_observed) if last_observed is not None else None
        )
        if last is not None and last.utcoffset() != timedelta(0):
            raise ValueError("Observation timestamp must be UTC")
        complete: set[str] = set()
        capabilities: set[str] = set()
        for item in coverage:
            if not isinstance(item, dict):
                raise ValueError("Invalid coverage")
            capability, status = item.get("capability"), item.get("status")
            if (
                not isinstance(capability, str)
                or capability not in {"notices", "assessments", "balances", "payments"}
                or capability in capabilities
                or not isinstance(status, str)
                or status not in {"complete", "partial", "unavailable", "unsupported"}
            ):
                raise ValueError("Invalid coverage status")
            capabilities.add(capability)
            if status == "complete":
                complete.add(capability)
        records = tuple(Installment.parse(item) for item in installments)
        if len({item.id for item in records}) != len(records):
            raise ValueError("Duplicate installment identities")
        coverage_gap = len(complete) < 4
        for issue in issues:
            if not isinstance(issue, dict) or not isinstance(issue.get("code"), str):
                raise ValueError("Invalid ledger issue")
            # The Rust ledger owns the freshness rule. Native HA entities only
            # project its result rather than maintaining a second tax rule set.
            if issue["code"] in {"coverage_gap", "coverage_unknown"}:
                coverage_gap = True
        return cls(revision, attention, balance, last, coverage_gap, records)
