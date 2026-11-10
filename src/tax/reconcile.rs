use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tax::ledger::{
    BalanceScope, BillIdentity, BillKind, Coverage, CoverageCapability, CoverageStatus,
    Observation, ObservationData, PaymentStatus, Snapshot, SourceKind,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Issue {
    pub code: String,
    pub message: String,
    pub observation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<IssueScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueScope {
    Property,
    Bill { bill: BillIdentity },
    Installment { bill: BillIdentity, number: u8 },
    Capability { capability: CoverageCapability },
}

impl Issue {
    fn scoped(mut self, scope: IssueScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallmentState {
    Unknown,
    Open,
    PendingPayment,
    Paid,
    Delinquent,
    Contradiction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InstallmentView {
    pub id: String,
    pub bill_id: String,
    pub identity: BillIdentity,
    pub number: u8,
    pub amount_cents: i64,
    pub due_date: NaiveDate,
    pub delinquent_at: DateTime<Utc>,
    #[schemars(with = "String")]
    pub timezone: Tz,
    pub state: InstallmentState,
    pub county_balance_cents: Option<i64>,
    pub estimated_remaining_cents: Option<i64>,
    pub evidence: Vec<String>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BillView {
    pub id: String,
    pub identity: BillIdentity,
    pub bill_kind: Option<BillKind>,
    pub superseded: bool,
    pub missing_notice: bool,
    pub notice: Option<Observation>,
    pub latest_balance: Option<Observation>,
    pub installments: Vec<InstallmentView>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Reconciliation {
    pub revision: i64,
    pub needs_attention: usize,
    pub known_balance_cents: Option<i64>,
    pub latest_balance: Option<Observation>,
    pub bills: Vec<BillView>,
    pub installments: Vec<InstallmentView>,
    pub issues: Vec<Issue>,
    pub coverage: Vec<Coverage>,
    pub last_observed_at: Option<DateTime<Utc>>,
    pub assessment_count: usize,
    pub payment_count: usize,
}

pub fn bill_id(identity: &BillIdentity) -> String {
    // Length delimiters prevent ambiguous concatenations without depending on JSON encoding.
    let mut hash = Sha256::new();
    for value in [
        identity.jurisdiction.as_str(),
        identity.parcel.as_str(),
        &identity.fiscal_year.to_string(),
        &identity.sequence,
        &identity.source_version,
    ] {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    }
    hex::encode(hash.finalize())
}

fn issue(code: &str, message: &str, ids: Vec<String>) -> Issue {
    Issue {
        code: code.into(),
        message: message.into(),
        observation_ids: ids,
        scope: None,
    }
}

fn fresh(observation: &Observation, now: DateTime<Utc>) -> bool {
    observation.evidence.observed_at <= now + Duration::minutes(5)
        && observation.evidence.effective_at <= now + Duration::minutes(5)
        && observation.evidence.effective_at >= now - Duration::hours(48)
}

fn order(observation: &Observation) -> (DateTime<Utc>, DateTime<Utc>, &str) {
    (
        observation.evidence.effective_at,
        observation.evidence.observed_at,
        &observation.id,
    )
}

fn latest_balance<'a>(
    observations: &'a [Observation],
    scope: &BalanceScope,
) -> Option<&'a Observation> {
    observations.iter().filter(|observation| {
        observation.evidence.source == SourceKind::County
            && matches!(&observation.observation, ObservationData::Balance(balance) if &balance.scope == scope)
    }).max_by_key(|observation| order(observation))
}

fn balance_amount(observation: Option<&Observation>, now: DateTime<Utc>) -> Option<i64> {
    observation
        .filter(|value| fresh(value, now))
        .and_then(|value| match &value.observation {
            ObservationData::Balance(balance) => balance.amount_cents,
            _ => None,
        })
}

fn contains_scope(ancestor: &BalanceScope, descendant: &BalanceScope) -> bool {
    match (ancestor, descendant) {
        (BalanceScope::Property, _) => true,
        (BalanceScope::Bill { bill: parent }, BalanceScope::Installment { bill: child, .. }) => {
            parent == child
        }
        _ => false,
    }
}

fn conflicting_balance_ids(
    observations: &[Observation],
    selected: Option<&Observation>,
    superseded: &BTreeSet<BillIdentity>,
) -> Vec<String> {
    let Some(selected) = selected else {
        return Vec::new();
    };
    let ObservationData::Balance(selected_balance) = &selected.observation else {
        return Vec::new();
    };
    let mut conflicts = Vec::new();
    for other in observations {
        if other.evidence.source != SourceKind::County
            || other.evidence.effective_at != selected.evidence.effective_at
        {
            continue;
        }
        let ObservationData::Balance(balance) = &other.observation else {
            continue;
        };
        if matches!(&balance.scope, BalanceScope::Bill {bill} | BalanceScope::Installment {bill, ..} if superseded.contains(bill))
        {
            continue;
        }
        if balance.scope == selected_balance.scope {
            if balance.amount_cents != selected_balance.amount_cents {
                conflicts.push(other.id.clone());
            }
            continue;
        }
        let (Some(selected_amount), Some(other_amount)) =
            (selected_balance.amount_cents, balance.amount_cents)
        else {
            continue;
        };
        // A scoped total cannot be smaller than an included bill/installment at
        // the same effective time. Different times do not describe one snapshot.
        if contains_scope(&selected_balance.scope, &balance.scope) && selected_amount < other_amount
            || contains_scope(&balance.scope, &selected_balance.scope)
                && other_amount < selected_amount
        {
            conflicts.push(other.id.clone());
        }
    }
    conflicts
}

fn notice_conflicts(history: &[&Observation]) -> bool {
    history.first().is_some_and(|first| {
        history
            .iter()
            .any(|other| other.observation != first.observation)
    })
}

/// Pure projection. No network calls, wall-clock reads, provider guessing or model inference.
pub fn reconcile(snapshot: &Snapshot, now: DateTime<Utc>) -> Reconciliation {
    let scoped;
    let snapshot =
        if let Some(property) = snapshot.property.as_ref().filter(|p| p.bought_on.is_some()) {
            scoped = Snapshot {
                revision: snapshot.revision,
                property: snapshot.property.clone(),
                coverage: property.history_coverage(&snapshot.coverage),
                observations: snapshot
                    .observations
                    .iter()
                    .filter(|value| property.includes_observation(value))
                    .cloned()
                    .collect(),
            };
            &scoped
        } else {
            snapshot
        };
    let mut issues = Vec::new();
    let mut identities = BTreeSet::new();
    let mut notices: BTreeMap<&BillIdentity, Vec<&Observation>> = BTreeMap::new();
    let mut payments: BTreeMap<&str, Vec<&Observation>> = BTreeMap::new();
    let mut assessment_years = BTreeSet::new();
    for observation in &snapshot.observations {
        match &observation.observation {
            ObservationData::Notice(notice) => {
                identities.insert(notice.identity.clone());
                notices
                    .entry(&notice.identity)
                    .or_default()
                    .push(observation);
            }
            ObservationData::Payment(payment) => {
                for allocation in &payment.allocations {
                    identities.insert(allocation.bill.clone());
                }
                payments
                    .entry(&payment.reference)
                    .or_default()
                    .push(observation);
            }
            ObservationData::Balance(balance) => match &balance.scope {
                BalanceScope::Bill { bill } | BalanceScope::Installment { bill, .. } => {
                    identities.insert(bill.clone());
                }
                BalanceScope::Property => {}
            },
            ObservationData::Assessment(assessment) => {
                assessment_years.insert(assessment.fiscal_year);
            }
        }
    }
    // A replaced bill must have one unambiguous replacement with the same bill kind.
    let mut replacements: BTreeMap<BillIdentity, BTreeSet<BillIdentity>> = BTreeMap::new();
    for observation in &snapshot.observations {
        if let ObservationData::Notice(notice) = &observation.observation
            && let Some(prior) = &notice.supersedes
        {
            identities.insert(prior.clone());
            replacements
                .entry(prior.clone())
                .or_default()
                .insert(notice.identity.clone());
        }
    }
    let mut superseded = BTreeSet::new();
    for (prior, successors) in &replacements {
        let mut seen = BTreeSet::new();
        let mut current = prior;
        let mut cycle = false;
        while let Some(next) = replacements.get(current).and_then(|values| values.first()) {
            if !seen.insert(current) {
                cycle = true;
                break;
            }
            current = next;
        }
        let prior_kind = notices
            .get(prior)
            .and_then(|values| values.first())
            .and_then(|observation| match &observation.observation {
                ObservationData::Notice(notice) => Some(&notice.bill_kind),
                _ => None,
            });
        let kind_conflict = prior_kind.is_some_and(|kind| successors.iter().any(|successor| {
            notices.get(successor).is_some_and(|values| values.iter().any(|observation| {
                matches!(&observation.observation, ObservationData::Notice(notice) if &notice.bill_kind != kind)
            }))
        }));
        let successor_conflict = successors.iter().any(|successor| {
            notices
                .get(successor)
                .is_none_or(|history| notice_conflicts(history))
        });
        if successors.len() != 1 || cycle || kind_conflict || successor_conflict {
            issues.push(issue(
                "conflicting_supersession",
                "Bill replacements conflict; no affected version is silently discarded.",
                Vec::new(),
            ));
        } else {
            superseded.insert(prior.clone());
        }
    }
    let mut selected_payments = Vec::new();
    let mut conflicted_payments = BTreeSet::new();
    for history in payments.values() {
        let Some(latest) = history
            .iter()
            .max_by_key(|observation| order(observation))
            .copied()
        else {
            continue;
        };
        let ObservationData::Payment(latest_payment) = &latest.observation else {
            continue;
        };
        let conflicting = history.iter().any(|observation| {
            let ObservationData::Payment(payment) = &observation.observation else {
                return false;
            };
            payment.amount_cents != latest_payment.amount_cents
                || (observation.evidence.effective_at == latest.evidence.effective_at
                    && payment != latest_payment)
                || (observation.evidence.effective_at < latest.evidence.effective_at
                    && matches!(
                        payment.status,
                        PaymentStatus::CountyPosted
                            | PaymentStatus::Reversed
                            | PaymentStatus::Refunded
                    )
                    && matches!(
                        latest_payment.status,
                        PaymentStatus::Submitted | PaymentStatus::BankSettled
                    ))
        });
        if conflicting {
            conflicted_payments.insert(latest_payment.reference.as_str());
            issues.push(issue(
                "payment_conflict",
                "Payment lifecycle observations disagree; the payment cannot close an installment.",
                history.iter().map(|value| value.id.clone()).collect(),
            ));
        }
        if latest_payment
            .allocations
            .iter()
            .map(|value| i128::from(value.amount_cents))
            .sum::<i128>()
            < i128::from(latest_payment.amount_cents)
        {
            issues.push(issue(
                "unallocated_payment",
                "Part of a payment has no explicit bill and installment allocation.",
                vec![latest.id.clone()],
            ));
        }
        if matches!(
            latest_payment.status,
            PaymentStatus::Reversed | PaymentStatus::Refunded
        ) {
            issues.push(issue(
                "payment_returned",
                "A reversed or refunded payment provides no settled tax credit.",
                vec![latest.id.clone()],
            ));
        }
        selected_payments.push(latest);
    }
    let mut bills = Vec::new();
    let mut all_installments = Vec::new();
    for identity in identities {
        let mut bill_issues = Vec::new();
        let history = notices.get(&identity);
        let notice_observation =
            history.and_then(|values| values.iter().max_by_key(|value| order(value)).copied());
        let notice = notice_observation.and_then(|value| match &value.observation {
            ObservationData::Notice(notice) => Some(notice),
            _ => None,
        });
        let conflicting_notice = history.is_some_and(|values| notice_conflicts(values));
        if conflicting_notice {
            bill_issues.push(issue("notice_conflict", "The same bill version has conflicting notices; confirm the corrected source version.", history.map(|values| values.iter().map(|value| value.id.clone()).collect()).unwrap_or_default()));
        }
        if notice.is_none() {
            bill_issues.push(issue(
                "missing_notice",
                "Payment or balance evidence references a bill whose notice is missing.",
                Vec::new(),
            ));
        }
        for issue in &mut bill_issues {
            issue.scope = Some(IssueScope::Bill {
                bill: identity.clone(),
            });
        }
        let id = bill_id(&identity);
        let mut installments = Vec::new();
        if let Some(notice) = notice {
            for installment in &notice.installments {
                let scope = BalanceScope::Installment {
                    bill: identity.clone(),
                    number: installment.number,
                };
                let balance = latest_balance(&snapshot.observations, &scope);
                let county_balance = balance_amount(balance, now);
                let mut installment_issues = Vec::new();
                let mut evidence = notice_observation
                    .iter()
                    .map(|value| value.id.clone())
                    .collect::<Vec<_>>();
                if let Some(balance) = balance {
                    evidence.push(balance.id.clone());
                }
                let conflicting_balances =
                    conflicting_balance_ids(&snapshot.observations, balance, &superseded);
                evidence.extend(conflicting_balances.iter().cloned());
                let mut credited = 0_i128;
                let mut pending = 0_i128;
                let mut payment_conflict = false;
                let mut zero_before_return = false;
                for observation in &selected_payments {
                    let ObservationData::Payment(payment) = &observation.observation else {
                        continue;
                    };
                    if matches!(
                        payment.status,
                        PaymentStatus::Reversed | PaymentStatus::Refunded
                    ) {
                        // A return can omit allocations, but it cannot erase the
                        // installment linkage already established by this same
                        // payment's history. It creates no new allocation.
                        let mut linked = false;
                        if let Some(history) = payments.get(payment.reference.as_str()) {
                            for prior in history {
                                let ObservationData::Payment(prior_payment) = &prior.observation
                                else {
                                    continue;
                                };
                                if prior_payment.allocations.iter().any(|allocation| {
                                    allocation.bill == identity
                                        && allocation.installment == installment.number
                                }) {
                                    linked = true;
                                    if prior.id != observation.id {
                                        evidence.push(prior.id.clone());
                                    }
                                }
                            }
                        }
                        if linked {
                            evidence.push(observation.id.clone());
                            // County history can identify a return separately
                            // from the original posting. Preserve both records;
                            // do not guess which older credit it reverses or
                            // leave that credit closing this same installment.
                            let later_county_zero = county_balance == Some(0)
                                && balance.is_some_and(|balance| {
                                    balance.evidence.effective_at
                                        > observation.evidence.effective_at
                                });
                            if !later_county_zero {
                                for posted in &selected_payments {
                                    if posted.evidence.effective_at
                                        > observation.evidence.effective_at
                                    {
                                        continue;
                                    }
                                    if let ObservationData::Payment(prior) = &posted.observation
                                        && prior.status == PaymentStatus::CountyPosted
                                        && prior.reference != payment.reference
                                        && prior.allocations.iter().any(|allocation| {
                                            allocation.bill == identity
                                                && allocation.installment == installment.number
                                        })
                                    {
                                        payment_conflict = true;
                                        evidence.push(posted.id.clone());
                                        break;
                                    }
                                }
                            }
                            if conflicted_payments.contains(payment.reference.as_str())
                                || observation.evidence.effective_at > now
                            {
                                payment_conflict = true;
                            } else {
                                zero_before_return |= county_balance == Some(0)
                                    && balance.is_some_and(|balance| {
                                        balance.evidence.effective_at
                                            <= observation.evidence.effective_at
                                    });
                            }
                        }
                        continue;
                    }
                    for allocation in payment.allocations.iter().filter(|value| {
                        value.bill == identity && value.installment == installment.number
                    }) {
                        evidence.push(observation.id.clone());
                        if conflicted_payments.contains(payment.reference.as_str())
                            || observation.evidence.effective_at > now
                        {
                            payment_conflict = true;
                        } else {
                            match payment.status {
                                PaymentStatus::CountyPosted => {
                                    credited += i128::from(allocation.amount_cents);
                                }
                                PaymentStatus::Submitted | PaymentStatus::BankSettled => {
                                    pending += i128::from(allocation.amount_cents);
                                }
                                PaymentStatus::Reversed | PaymentStatus::Refunded => {}
                            }
                        }
                    }
                }
                let remaining = i128::from(installment.amount_cents) - credited;
                let estimated_remaining_cents = i64::try_from(remaining.max(0)).ok();
                if credited > i128::from(installment.amount_cents) {
                    installment_issues.push(issue(
                        "overallocated_payment",
                        "County-posted allocations exceed this installment's notice amount.",
                        evidence.clone(),
                    ));
                }
                let contradictory = conflicting_notice
                    || payment_conflict
                    || zero_before_return
                    || !conflicting_balances.is_empty()
                    || (county_balance.is_some_and(|amount| amount > 0) && remaining <= 0)
                    || credited > i128::from(installment.amount_cents);
                if county_balance.is_none() {
                    installment_issues.push(issue(
                        "unknown_balance",
                        "No fresh authoritative balance is available for this installment.",
                        balance.iter().map(|value| value.id.clone()).collect(),
                    ));
                }
                let state = if contradictory {
                    installment_issues.push(issue("contradiction", "Evidence disagrees; verify the county record before treating this installment as paid.", evidence.clone()));
                    InstallmentState::Contradiction
                } else if county_balance == Some(0) || remaining == 0 {
                    InstallmentState::Paid
                } else if pending > 0 {
                    InstallmentState::PendingPayment
                } else if now >= installment.delinquent_at {
                    InstallmentState::Delinquent
                } else {
                    InstallmentState::Open
                };
                for issue in &mut installment_issues {
                    issue.scope = Some(IssueScope::Installment {
                        bill: identity.clone(),
                        number: installment.number,
                    });
                }
                installments.push(InstallmentView {
                    id: format!("{id}-{}", installment.number),
                    bill_id: id.clone(),
                    identity: identity.clone(),
                    number: installment.number,
                    amount_cents: installment.amount_cents,
                    due_date: installment.due_date,
                    delinquent_at: installment.delinquent_at,
                    timezone: installment.timezone,
                    state,
                    county_balance_cents: county_balance,
                    estimated_remaining_cents: if contradictory {
                        None
                    } else {
                        estimated_remaining_cents
                    },
                    evidence,
                    issues: installment_issues,
                });
            }
        }
        let is_superseded = superseded.contains(&identity);
        if !is_superseded {
            all_installments.extend(installments.iter().cloned());
            issues.extend(bill_issues.iter().cloned());
            for installment in &installments {
                issues.extend(installment.issues.iter().cloned());
            }
        }
        let bill_balance = latest_balance(
            &snapshot.observations,
            &BalanceScope::Bill {
                bill: identity.clone(),
            },
        )
        .cloned();
        bills.push(BillView {
            id,
            identity,
            bill_kind: notice.map(|value| value.bill_kind.clone()),
            superseded: is_superseded,
            missing_notice: notice.is_none(),
            notice: notice_observation.cloned(),
            latest_balance: bill_balance,
            installments,
            issues: bill_issues,
        });
    }
    for year in assessment_years {
        if !bills.iter().any(|bill| {
            !bill.superseded && !bill.missing_notice && bill.identity.fiscal_year == year
        }) {
            issues.push(issue(
                "assessment_without_notice",
                "Assessment evidence exists for a fiscal year with no imported bill notice.",
                Vec::new(),
            ));
        }
    }
    if snapshot.property.is_none() {
        issues.push(issue("property_unconfigured", "Confirm the county FIPS and exact parcel against county evidence before importing tax records.", Vec::new()));
    }
    if snapshot.coverage.is_empty() {
        issues.push(issue(
            "coverage_unknown",
            "County collection coverage has not been established.",
            Vec::new(),
        ));
    } else if [
        CoverageCapability::Notices,
        CoverageCapability::Assessments,
        CoverageCapability::Balances,
        CoverageCapability::Payments,
    ]
    .iter()
    .any(|capability| {
        !snapshot
            .coverage
            .iter()
            .any(|coverage| &coverage.capability == capability)
    }) {
        issues.push(issue(
            "coverage_incomplete",
            "Not every county capability has a recorded coverage result.",
            Vec::new(),
        ));
    }
    for coverage in &snapshot.coverage {
        if coverage.status != CoverageStatus::Complete
            || coverage.checked_at < now - Duration::hours(48)
            || coverage.checked_at > now + Duration::minutes(5)
        {
            issues.push(issue("coverage_gap", "County evidence is partial, unavailable, unsupported or stale; an empty result is not proof of no tax due.", Vec::new()).scoped(IssueScope::Capability {capability: coverage.capability.clone()}));
        }
    }
    let property_balance = latest_balance(&snapshot.observations, &BalanceScope::Property);
    let conflicting_property_balances =
        conflicting_balance_ids(&snapshot.observations, property_balance, &superseded);
    let outdated_property_balance = property_balance.is_some_and(|balance| selected_payments.iter().any(|observation| {
        observation.evidence.effective_at > balance.evidence.effective_at
            && observation.evidence.effective_at <= now
            && matches!(&observation.observation, ObservationData::Payment(payment)
                if matches!(payment.status, PaymentStatus::CountyPosted | PaymentStatus::Reversed | PaymentStatus::Refunded))
    }));
    let known_balance_cents =
        if !conflicting_property_balances.is_empty() || outdated_property_balance {
            None
        } else {
            balance_amount(property_balance, now)
        };
    if known_balance_cents.is_none() {
        issues.push(issue(
            "unknown_property_balance",
            "No fresh, unambiguous county balance covers the property. Unknown is not zero.",
            property_balance
                .iter()
                .map(|value| value.id.clone())
                .collect(),
        ));
    }
    if !conflicting_property_balances.is_empty() {
        let mut evidence = conflicting_property_balances;
        evidence.extend(
            property_balance
                .iter()
                .map(|observation| observation.id.clone()),
        );
        issues.push(
            issue(
                "balance_conflict",
                "County balances for overlapping scopes at the same effective time disagree.",
                evidence,
            )
            .scoped(IssueScope::Property),
        );
    }
    if outdated_property_balance {
        issues.push(issue("balance_outdated", "A later county posting, reversal or refund makes the earlier property balance historical. Refresh the county balance.", property_balance.iter().map(|value| value.id.clone()).collect()).scoped(IssueScope::Property));
    }
    all_installments.sort_by(|a, b| (a.delinquent_at, &a.id).cmp(&(b.delinquent_at, &b.id)));
    let needs_attention = all_installments
        .iter()
        .filter(|item| item.state != InstallmentState::Paid)
        .count()
        + issues
            .iter()
            .filter(|item| !matches!(item.code.as_str(), "unknown_balance"))
            .count();
    Reconciliation {
        revision: snapshot.revision,
        needs_attention,
        known_balance_cents,
        latest_balance: property_balance.cloned(),
        bills,
        installments: all_installments,
        issues,
        coverage: snapshot.coverage.clone(),
        last_observed_at: snapshot
            .observations
            .iter()
            .map(|value| value.evidence.observed_at)
            .max(),
        assessment_count: snapshot
            .observations
            .iter()
            .filter(|value| matches!(value.observation, ObservationData::Assessment(_)))
            .count(),
        payment_count: payments.len(),
    }
}
