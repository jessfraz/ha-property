use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// JSON clients must preserve every cent exactly, including the browser UI.
const MAX_CENTS: i64 = 9_007_199_254_740_991;
pub const MAX_IMPORT_BYTES: usize = 4 * 1024 * 1024;

/// Five digits: two state digits followed by three county digits. Never a ZIP.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct CountyFips(String);

impl CountyFips {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn los_angeles() -> Self {
        Self("06037".to_owned())
    }
}

impl TryFrom<String> for CountyFips {
    type Error = TaxError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 5 && value.bytes().all(|c| c.is_ascii_digit()) {
            Ok(Self(value))
        } else {
            Err(TaxError::Invalid("county_fips must contain five digits"))
        }
    }
}

impl From<CountyFips> for String {
    fn from(value: CountyFips) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct ParcelId(String);

impl ParcelId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ParcelId {
    type Error = TaxError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if valid_text(&value, 100) {
            Ok(Self(value))
        } else {
            Err(TaxError::Invalid(
                "parcel must be nonempty and at most 100 characters",
            ))
        }
    }
}
impl From<ParcelId> for String {
    fn from(value: ParcelId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingKey {
    pub jurisdiction: CountyFips,
    pub parcel: ParcelId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PropertyBinding {
    pub key: BindingKey,
    pub label: String,
    /// Confirmed by the user against authoritative county parcel evidence.
    pub confirmed_at: DateTime<Utc>,
    pub confirmation: Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    County,
    Bank,
    OwnerDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub source: SourceKind,
    pub document: String,
    pub url: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub effective_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BillIdentity {
    pub jurisdiction: CountyFips,
    pub parcel: ParcelId,
    pub fiscal_year: u16,
    pub sequence: String,
    pub source_version: String,
}

impl BillIdentity {
    pub fn binding(&self) -> BindingKey {
        BindingKey {
            jurisdiction: self.jurisdiction.clone(),
            parcel: self.parcel.clone(),
        }
    }
    pub fn same_bill(&self, other: &Self) -> bool {
        self.jurisdiction == other.jurisdiction
            && self.parcel == other.parcel
            && self.fiscal_year == other.fiscal_year
            && self.sequence == other.sequence
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillKind {
    Annual,
    Supplemental,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Installment {
    pub number: u8,
    pub amount_cents: i64,
    pub due_date: NaiveDate,
    pub delinquent_at: DateTime<Utc>,
    #[schemars(with = "String")]
    pub timezone: Tz,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub identity: BillIdentity,
    pub bill_kind: BillKind,
    pub supersedes: Option<BillIdentity>,
    pub installments: Vec<Installment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub fiscal_year: u16,
    pub assessed_value_cents: i64,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum BalanceScope {
    Property,
    Bill { bill: BillIdentity },
    Installment { bill: BillIdentity, number: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Balance {
    pub scope: BalanceScope,
    /// None records an explicitly unknown balance, never a zero.
    pub amount_cents: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Submitted,
    BankSettled,
    CountyPosted,
    Reversed,
    Refunded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub bill: BillIdentity,
    pub installment: u8,
    pub amount_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Payment {
    /// Stable across lifecycle events. Different observations retain the history.
    pub reference: String,
    pub status: PaymentStatus,
    pub amount_cents: i64,
    pub allocations: Vec<Allocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationData {
    Notice(Notice),
    Assessment(Assessment),
    Balance(Balance),
    Payment(Payment),
}

impl ObservationData {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Notice(_) => "notice",
            Self::Assessment(_) => "assessment",
            Self::Balance(_) => "balance",
            Self::Payment(_) => "payment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub id: String,
    pub binding: BindingKey,
    pub evidence: Evidence,
    pub observation: ObservationData,
}

impl Observation {
    /// Check both the envelope and every embedded bill identity. A provider's
    /// correctly labeled page cannot smuggle evidence for a different parcel.
    pub fn validate_binding(&self, expected: &BindingKey) -> Result<(), TaxError> {
        if &self.binding != expected {
            return Err(TaxError::BindingMismatch);
        }
        match &self.observation {
            ObservationData::Notice(notice) => {
                validate_identity(&notice.identity, expected)?;
                if let Some(prior) = &notice.supersedes {
                    validate_identity(prior, expected)?;
                }
            }
            ObservationData::Balance(balance) => match &balance.scope {
                BalanceScope::Bill { bill } | BalanceScope::Installment { bill, .. } => {
                    validate_identity(bill, expected)?
                }
                BalanceScope::Property => {}
            },
            ObservationData::Payment(payment) => {
                for allocation in &payment.allocations {
                    validate_identity(&allocation.bill, expected)?;
                }
            }
            ObservationData::Assessment(_) => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageCapability {
    Notices,
    Assessments,
    Balances,
    Payments,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    Complete,
    Partial,
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub capability: CoverageCapability,
    pub status: CoverageStatus,
    pub checked_at: DateTime<Utc>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportInput {
    pub property: PropertyBinding,
    pub observations: Vec<Observation>,
    pub coverage: Vec<Coverage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub revision: i64,
    pub property: Option<PropertyBinding>,
    pub observations: Vec<Observation>,
    pub coverage: Vec<Coverage>,
}

#[derive(Debug, Error)]
pub enum TaxError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("observation does not match the confirmed county and parcel")]
    BindingMismatch,
    #[error("the confirmed property cannot be replaced by an import")]
    PropertyChanged,
    #[error("an immutable observation id already has different evidence")]
    ObservationConflict,
    #[error("the ledger changed; create a new preview")]
    RevisionConflict,
    #[error("preview hash does not match this import")]
    PreviewMismatch,
    #[error("idempotency key was already used for another import")]
    IdempotencyConflict,
    #[error("the ledger has reached its observation limit")]
    Capacity,
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("stored evidence could not be decoded")]
    Encoding(#[from] serde_json::Error),
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn validate_evidence(evidence: &Evidence) -> Result<(), TaxError> {
    if !valid_text(&evidence.document, 500) {
        return Err(TaxError::Invalid(
            "evidence requires a bounded document reference",
        ));
    }
    if let Some(url) = &evidence.url {
        let parsed = url::Url::parse(url)
            .map_err(|_| TaxError::Invalid("evidence URL must use HTTP(S) without credentials"))?;
        if url.len() > 2000
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(TaxError::Invalid(
                "evidence URL must use HTTP(S) without credentials",
            ));
        }
    }
    Ok(())
}

fn validate_identity(identity: &BillIdentity, binding: &BindingKey) -> Result<(), TaxError> {
    if &identity.binding() != binding {
        return Err(TaxError::BindingMismatch);
    }
    if !(1900..=2200).contains(&identity.fiscal_year)
        || !valid_text(&identity.sequence, 100)
        || !valid_text(&identity.source_version, 100)
    {
        return Err(TaxError::Invalid(
            "bill identity requires a valid fiscal year, sequence and source version",
        ));
    }
    Ok(())
}

pub(crate) fn validate_coverage(coverage: &[Coverage]) -> Result<(), TaxError> {
    if coverage.len() > 4 {
        return Err(TaxError::Capacity);
    }
    let mut capabilities = BTreeSet::new();
    for coverage in coverage {
        if !capabilities.insert(&coverage.capability) || !valid_text(&coverage.detail, 1000) {
            return Err(TaxError::Invalid(
                "coverage must be unique and explain its scope",
            ));
        }
    }
    Ok(())
}

impl ImportInput {
    pub fn validate(&self) -> Result<(), TaxError> {
        if serde_json::to_vec(self)?.len() > MAX_IMPORT_BYTES {
            return Err(TaxError::Capacity);
        }
        if !valid_text(&self.property.label, 200) {
            return Err(TaxError::Invalid("property label is required"));
        }
        validate_evidence(&self.property.confirmation)?;
        if self.property.confirmation.source != SourceKind::County {
            return Err(TaxError::Invalid(
                "property confirmation requires county parcel evidence",
            ));
        }
        if self.observations.len() > 1000 || self.coverage.len() > 4 {
            return Err(TaxError::Capacity);
        }
        let mut ids = BTreeMap::new();
        for observation in &self.observations {
            if !valid_text(&observation.id, 200) {
                return Err(TaxError::Invalid("observation id is required"));
            }
            observation.validate_binding(&self.property.key)?;
            if let Some(prior) = ids.insert(&observation.id, observation)
                && prior != observation
            {
                return Err(TaxError::ObservationConflict);
            }
            validate_evidence(&observation.evidence)?;
            match &observation.observation {
                ObservationData::Notice(notice) => {
                    if let Some(prior) = &notice.supersedes
                        && (!prior.same_bill(&notice.identity) || prior == &notice.identity)
                    {
                        return Err(TaxError::Invalid(
                            "a superseding notice must identify another version of the same bill",
                        ));
                    }
                    if notice.installments.is_empty() || notice.installments.len() > 12 {
                        return Err(TaxError::Invalid("notice must have bounded installments"));
                    }
                    let mut numbers = BTreeSet::new();
                    for item in &notice.installments {
                        if item.number == 0
                            || !(0..=MAX_CENTS).contains(&item.amount_cents)
                            || !numbers.insert(item.number)
                            || item
                                .delinquent_at
                                .with_timezone(&item.timezone)
                                .date_naive()
                                < item.due_date
                        {
                            return Err(TaxError::Invalid(
                                "installment amounts, numbers or deadlines are invalid",
                            ));
                        }
                    }
                }
                ObservationData::Assessment(item) => {
                    if !(1900..=2200).contains(&item.fiscal_year)
                        || !(0..=MAX_CENTS).contains(&item.assessed_value_cents)
                        || !valid_text(&item.description, 1000)
                    {
                        return Err(TaxError::Invalid("assessment is invalid"));
                    }
                }
                ObservationData::Balance(item) => {
                    if item
                        .amount_cents
                        .is_some_and(|amount| !(0..=MAX_CENTS).contains(&amount))
                    {
                        return Err(TaxError::Invalid(
                            "balance must be a nonnegative exact JSON amount",
                        ));
                    }
                    match &item.scope {
                        BalanceScope::Property => {}
                        BalanceScope::Bill { .. } => {}
                        BalanceScope::Installment { number, .. } => {
                            if *number == 0 {
                                return Err(TaxError::Invalid(
                                    "installment number must be positive",
                                ));
                            }
                        }
                    }
                }
                ObservationData::Payment(item) => {
                    if !valid_text(&item.reference, 200)
                        || !(1..=MAX_CENTS).contains(&item.amount_cents)
                        || item.allocations.len() > 100
                    {
                        return Err(TaxError::Invalid("payment reference or amount is invalid"));
                    }
                    let mut total = 0_i64;
                    let mut targets = BTreeSet::new();
                    for allocation in &item.allocations {
                        if allocation.installment == 0
                            || !(1..=MAX_CENTS).contains(&allocation.amount_cents)
                            || !targets.insert((&allocation.bill, allocation.installment))
                        {
                            return Err(TaxError::Invalid("payment allocations are invalid"));
                        }
                        total = total
                            .checked_add(allocation.amount_cents)
                            .ok_or(TaxError::Invalid("payment allocation overflow"))?;
                    }
                    if total > item.amount_cents {
                        return Err(TaxError::Invalid(
                            "payment allocations exceed the payment amount",
                        ));
                    }
                    if item.status == PaymentStatus::CountyPosted
                        && observation.evidence.source != SourceKind::County
                    {
                        return Err(TaxError::Invalid("county posting requires county evidence"));
                    }
                }
            }
        }
        validate_coverage(&self.coverage)
    }
}
