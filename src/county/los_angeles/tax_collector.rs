//! Readers for the county's ordinary public web forms, not a supported API.
//!
//! Form actions and response shapes are checked on every request. A changed
//! dashboard is a coverage gap, never an empty ledger or a zero balance.
use std::{collections::BTreeMap, future::Future, time::Duration};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use reqwest::{Client, Response, Url};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    county::{CountyError, CountySyncRequest},
    tax::ledger::{
        Allocation, BillIdentity, CountyFips, Coverage, CoverageCapability, CoverageStatus,
        Evidence, ImportInput, Observation, ObservationData, Payment, PaymentStatus, SourceKind,
        TaxError,
    },
};

const BILL_VIEWER: &str = "https://ttc.lacounty.gov/request-duplicate-bill/";
const BILL_RESULT: &str = "https://ttc.lacounty.gov/secured-property-tax-results";
const HISTORY_VIEWER: &str = "https://ttc.lacounty.gov/property-tax-payment-history/";
const BALANCE_VIEWER: &str = "https://vcheck.ttc.lacounty.gov/";
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_PAGES: u32 = 20;

#[derive(Debug)]
pub struct TaxRead {
    pub observations: Vec<Observation>,
    pub coverage: Vec<Coverage>,
}

struct Sources {
    bill: Url,
    result: Url,
    history: Url,
    balance: Url,
}
impl Sources {
    fn official() -> Result<Self, CountyError> {
        Ok(Self {
            bill: parse_url(BILL_VIEWER)?,
            result: parse_url(BILL_RESULT)?,
            history: parse_url(HISTORY_VIEWER)?,
            balance: parse_url(BALANCE_VIEWER)?,
        })
    }
}
fn parse_url(value: &str) -> Result<Url, CountyError> {
    Url::parse(value).map_err(|_| CountyError::Unavailable)
}
fn invalid(message: &'static str) -> CountyError {
    CountyError::InvalidEvidence(TaxError::Invalid(message))
}
fn selector(value: &'static str) -> Result<Selector, CountyError> {
    Selector::parse(value).map_err(|_| invalid("county form selector is invalid"))
}
fn ain(value: &str) -> Result<String, CountyError> {
    if value.len() > 20
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b' '))
    {
        return Err(invalid("Los Angeles parcel must be a ten-digit AIN"));
    }
    let digits: String = value
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    if digits.len() != 10 {
        return Err(invalid("Los Angeles parcel must be a ten-digit AIN"));
    }
    Ok(digits)
}
async fn body(mut response: Response) -> Result<String, CountyError> {
    if !response.status().is_success() {
        return Err(CountyError::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CountyError::Unavailable)?
    {
        if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
            return Err(invalid("county response exceeded the collection limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| invalid("county response was not UTF-8"))
}
fn coverage(
    capability: CoverageCapability,
    status: CoverageStatus,
    detail: impl Into<String>,
    at: DateTime<Utc>,
) -> Coverage {
    Coverage {
        capability,
        status,
        detail: detail.into(),
        checked_at: at,
    }
}

/// The client is session-local, with cookie support and redirects disabled.
/// Only the current viewer's allowlisted lookup actions are submitted.
pub async fn collect(request: &CountySyncRequest, client: &Client) -> Result<TaxRead, CountyError> {
    // Finish each capability before the county adapter's overall deadline so
    // a stalled dashboard cannot discard another dashboard's completed read.
    collect_sources(
        request,
        client,
        &Sources::official()?,
        Duration::from_secs(45),
    )
    .await
}
async fn collect_sources(
    request: &CountySyncRequest,
    client: &Client,
    sources: &Sources,
    deadline: Duration,
) -> Result<TaxRead, CountyError> {
    if request.property.key.jurisdiction != CountyFips::los_angeles() {
        return Err(CountyError::BindingMismatch);
    }
    if request.cursor.is_some() {
        return Err(CountyError::InvalidCursor);
    }
    let requested_ain = ain(request.property.key.parcel.as_str())?;
    let (notice, history, balance) = tokio::join!(
        before_deadline(
            deadline,
            read_notice(request, client, sources, &requested_ain)
        ),
        before_deadline(
            deadline,
            read_history(request, client, sources, &requested_ain)
        ),
        before_deadline(deadline, read_balance(request, client, sources)),
    );
    let notices = match notice {
        Ok(value) => value,
        Err(CountyError::BindingMismatch) => coverage(
            CoverageCapability::Notices,
            CoverageStatus::Unavailable,
            "The bill viewer returned evidence for another parcel; no notice was imported.",
            request.requested_at,
        ),
        Err(_) => coverage(
            CoverageCapability::Notices,
            CoverageStatus::Unavailable,
            "The ordinary county bill lookup could not be verified. Existing notices are unchanged.",
            request.requested_at,
        ),
    };
    let (observations, payments) = match history {
        Ok(value) => value,
        Err(CountyError::BindingMismatch) => (
            Vec::new(),
            coverage(
                CoverageCapability::Payments,
                CoverageStatus::Unavailable,
                "The payment-history viewer returned another parcel; no payment evidence was imported.",
                request.requested_at,
            ),
        ),
        Err(_) => (
            Vec::new(),
            coverage(
                CoverageCapability::Payments,
                CoverageStatus::Unavailable,
                "The county payment-history lookup failed or changed shape. Existing history is unchanged.",
                request.requested_at,
            ),
        ),
    };
    let balances = balance.unwrap_or_else(|_| {
        coverage(
            CoverageCapability::Balances,
            CoverageStatus::Unavailable,
            "The ordinary county balance viewer could not be verified. No balance is inferred.",
            request.requested_at,
        )
    });
    let result = TaxRead {
        observations,
        coverage: vec![notices, payments, balances],
    };
    ImportInput {
        property: request.property.clone(),
        observations: result.observations.clone(),
        coverage: result.coverage.clone(),
    }
    .validate()?;
    Ok(result)
}

async fn before_deadline<T>(
    deadline: Duration,
    read: impl Future<Output = Result<T, CountyError>>,
) -> Result<T, CountyError> {
    tokio::time::timeout(deadline, read)
        .await
        .map_err(|_| CountyError::Unavailable)?
}

fn bill_form(html: &str, sources: &Sources) -> Result<Vec<(String, String)>, CountyError> {
    let document = Html::parse_document(html);
    let forms: Vec<_> = document.select(&selector("form#ainform")?).collect();
    if forms.len() != 1 {
        return Err(invalid("county bill lookup form changed"));
    }
    let form = forms[0];
    if form
        .value()
        .attr("method")
        .is_none_or(|value| !value.eq_ignore_ascii_case("post"))
    {
        return Err(invalid("county bill lookup method changed"));
    }
    let action = sources
        .bill
        .join(
            form.value()
                .attr("action")
                .ok_or_else(|| invalid("county bill form action is missing"))?,
        )
        .map_err(|_| invalid("county bill lookup action is invalid"))?;
    if action.origin() != sources.result.origin()
        || action.path().trim_end_matches('/') != sources.result.path().trim_end_matches('/')
        || action.query().is_some()
        || action.fragment().is_some()
    {
        return Err(invalid(
            "county bill lookup action is not the read-only viewer",
        ));
    }
    let inputs: Vec<_> = form.select(&selector("input[name=ain]")?).collect();
    if inputs.len() != 1 {
        return Err(invalid("county bill AIN control changed"));
    }
    let mut fields = Vec::new();
    for input in form.select(&selector("input[type=hidden]")?) {
        let name = input
            .value()
            .attr("name")
            .ok_or_else(|| invalid("county hidden form field has no name"))?;
        if !matches!(
            name,
            "timestamp" | "__VIEWSTATE" | "__EVENTVALIDATION" | "__VIEWSTATEGENERATOR" | "_wpnonce"
        ) {
            return Err(invalid(
                "county bill lookup has an unrecognized hidden control",
            ));
        }
        let value = input.value().attr("value").unwrap_or_default();
        if value.len() > 64 * 1024 {
            return Err(invalid("county form state exceeded its limit"));
        }
        if name != "timestamp" {
            fields.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(fields)
}
async fn read_notice(
    request: &CountySyncRequest,
    client: &Client,
    sources: &Sources,
    requested_ain: &str,
) -> Result<Coverage, CountyError> {
    let initial = body(
        client
            .get(sources.bill.clone())
            .send()
            .await
            .map_err(|_| CountyError::Unavailable)?,
    )
    .await?;
    let mut fields = bill_form(&initial, sources)?;
    fields.push(("ain".into(), requested_ain.into()));
    // This is the current viewer's ordinary submit handler: floor(Date.now()/1000).
    fields.push(("timestamp".into(), Utc::now().timestamp().to_string()));
    let response = body(
        client
            .post(sources.result.clone())
            .header("Origin", sources.bill.origin().ascii_serialization())
            .header("Referer", sources.bill.as_str())
            .form(&fields)
            .send()
            .await
            .map_err(|_| CountyError::Unavailable)?,
    )
    .await?;
    let document = Html::parse_document(&response);
    let no_record = document.select(&selector("h2, p")?).any(|item| {
        item.text()
            .collect::<String>()
            .trim()
            .to_ascii_lowercase()
            .starts_with("record not found")
    });
    if no_record {
        return Ok(coverage(
            CoverageCapability::Notices,
            CoverageStatus::Unavailable,
            "The ordinary bill viewer reported no record for this lookup. This is not proof that no notice exists; existing notices are unchanged.",
            request.requested_at,
        ));
    }
    // The verified current response has no bill for the configured parcel. A
    // future document response is not silently treated as parsed tax evidence.
    Err(invalid(
        "county bill viewer returned an unrecognized document response",
    ))
}
async fn read_balance(
    request: &CountySyncRequest,
    client: &Client,
    sources: &Sources,
) -> Result<Coverage, CountyError> {
    let html = body(
        client
            .get(sources.balance.clone())
            .send()
            .await
            .map_err(|_| CountyError::Unavailable)?,
    )
    .await?;
    let document = Html::parse_document(&html);
    let forms: Vec<_> = document.select(&selector("form#termsForm")?).collect();
    if forms.len() == 1
        && forms[0]
            .select(&selector("button.g-recaptcha")?)
            .next()
            .is_some()
    {
        return Ok(coverage(
            CoverageCapability::Balances,
            CoverageStatus::Unavailable,
            "The ordinary balance viewer requires Google reCAPTCHA verification at its terms form. Human verification is required; no challenge is bypassed and no balance is inferred.",
            request.requested_at,
        ));
    }
    Err(invalid(
        "county balance viewer did not expose a verified read-only lookup",
    ))
}

#[derive(Deserialize)]
struct AjaxConfig {
    ajax_url: String,
    nonce: String,
}
fn ajax_config(html: &str, source: &Url) -> Result<AjaxConfig, CountyError> {
    let document = Html::parse_document(html);
    let mut found = None;
    for script in document.select(&selector("script")?) {
        let text = script.text().collect::<String>();
        let Some((_, tail)) = text.split_once("var phf_ajax") else {
            continue;
        };
        if found.is_some() {
            return Err(invalid("county payment form state is ambiguous"));
        }
        let (_, json) = tail
            .split_once('=')
            .ok_or_else(|| invalid("county payment form state changed"))?;
        let json = json.split_once(';').map_or(json, |(value, _)| value).trim();
        if json.len() > 16 * 1024 {
            return Err(invalid("county payment form state exceeded its limit"));
        }
        let config: AjaxConfig =
            serde_json::from_str(json).map_err(|_| invalid("county payment form state changed"))?;
        let endpoint = parse_url(&config.ajax_url)?;
        if endpoint.origin() != source.origin()
            || endpoint.path() != "/wp-admin/admin-ajax.php"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || config.nonce.is_empty()
            || config.nonce.len() > 256
            || !config.nonce.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(invalid(
                "county payment lookup action is not the read-only viewer",
            ));
        }
        found = Some(config);
    }
    found.ok_or_else(|| invalid("county payment lookup form state is missing"))
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct HistoryRecord {
    payment_id: u64,
    ain: String,
    effective_date: String,
    installment_key: String,
    group_number: String,
    tax_paid: String,
    penalty_paid: String,
    cost_paid: String,
    total_paid: String,
    tax_year: u16,
    sequence: String,
    group_description: String,
}
#[derive(Deserialize, Serialize)]
struct HistoryEnvelope {
    success: bool,
    data: HistoryData,
}
#[derive(Deserialize, Serialize)]
struct HistoryData {
    status: u16,
    data: Option<Vec<HistoryRecord>>,
    meta: Option<HistoryMeta>,
}
#[derive(Deserialize, Serialize)]
struct HistoryMeta {
    #[serde(rename = "lastUpdated")]
    updated: String,
    #[serde(rename = "totalPages")]
    pages: u32,
    #[serde(rename = "totalRecords")]
    records: u32,
}
struct ParsedPage {
    records: Vec<HistoryRecord>,
    updated: NaiveDate,
    pages: u32,
    total: u32,
}
fn parse_page(bytes: &str, requested_ain: &str) -> Result<ParsedPage, CountyError> {
    let envelope: HistoryEnvelope = serde_json::from_str(bytes)
        .map_err(|_| invalid("county payment history response changed"))?;
    if !envelope.success || envelope.data.status != 200 {
        return Err(CountyError::Unavailable);
    }
    let meta = envelope
        .data
        .meta
        .ok_or_else(|| invalid("county payment history metadata is missing"))?;
    let records = envelope
        .data
        .data
        .ok_or_else(|| invalid("county payment history records are missing"))?;
    if records.is_empty()
        || meta.pages == 0
        || meta.pages > MAX_PAGES
        || meta.records == 0
        || meta.records > 1000
    {
        return Err(invalid(
            "county payment history coverage is missing or exceeds its limit",
        ));
    }
    for record in &records {
        if ain(&record.ain)? != requested_ain {
            return Err(CountyError::BindingMismatch);
        }
    }
    let updated = NaiveDate::parse_from_str(&meta.updated, "%Y-%m-%d")
        .map_err(|_| invalid("county history source date is invalid"))?;
    Ok(ParsedPage {
        records,
        updated,
        pages: meta.pages,
        total: meta.records,
    })
}
fn cents(value: &str) -> Result<i64, CountyError> {
    let mut value = value.trim();
    let negative = value.starts_with('-');
    if negative {
        value = &value[1..];
    }
    if let Some(rest) = value.strip_prefix('$') {
        value = rest;
    }
    let (whole, fraction) = value
        .split_once('.')
        .ok_or_else(|| invalid("county money requires two decimal places"))?;
    if fraction.len() != 2 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("county money has malformed cents"));
    }
    let groups: Vec<_> = whole.split(',').collect();
    if groups
        .iter()
        .any(|group| group.is_empty() || !group.bytes().all(|b| b.is_ascii_digit()))
        || (groups.len() > 1
            && (groups[0].len() > 3 || groups.iter().skip(1).any(|group| group.len() != 3)))
    {
        return Err(invalid("county money has malformed digit grouping"));
    }
    let whole = groups
        .concat()
        .parse::<i64>()
        .map_err(|_| invalid("county money is outside the supported range"))?;
    let fraction = fraction
        .parse::<i64>()
        .map_err(|_| invalid("county money is invalid"))?;
    let value = whole
        .checked_mul(100)
        .and_then(|v| v.checked_add(fraction))
        .ok_or_else(|| invalid("county money overflow"))?;
    if negative {
        value
            .checked_neg()
            .ok_or_else(|| invalid("county money overflow"))
    } else {
        Ok(value)
    }
}
fn local_midnight(date: NaiveDate) -> Result<DateTime<Utc>, CountyError> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| invalid("county date is invalid"))?;
    Los_Angeles
        .from_local_datetime(&midnight)
        .single()
        .map(|value| value.with_timezone(&Utc))
        .ok_or_else(|| invalid("county date is ambiguous"))
}
fn observation(
    record: &HistoryRecord,
    updated: NaiveDate,
    request: &CountySyncRequest,
) -> Result<Observation, CountyError> {
    if record.payment_id == 0
        || record.sequence.len() != 3
        || !record.sequence.bytes().all(|b| b.is_ascii_digit())
        || record.group_number.len() > 100
        || record.group_description.len() > 200
        || record.group_description.chars().any(char::is_control)
    {
        return Err(invalid(
            "county payment identity or classification is invalid",
        ));
    }
    let payment_date = NaiveDate::parse_from_str(&record.effective_date, "%m/%d/%Y")
        .map_err(|_| invalid("county payment date is invalid"))?;
    if payment_date > updated {
        return Err(invalid("county payment is newer than its source snapshot"));
    }
    let total = cents(&record.total_paid)?;
    let tax = cents(&record.tax_paid)?;
    let penalty = cents(&record.penalty_paid)?;
    let cost = cents(&record.cost_paid)?;
    if total == 0 || tax.checked_add(penalty).and_then(|v| v.checked_add(cost)) != Some(total) {
        return Err(invalid("county payment components do not match its total"));
    }
    let classification = record.group_description.to_ascii_uppercase();
    let words: Vec<_> = classification
        .split(|c: char| !c.is_ascii_alphanumeric())
        .collect();
    let refund = words
        .iter()
        .any(|word| matches!(*word, "REFUND" | "REFUNDED"));
    let returned = words
        .iter()
        .any(|word| matches!(*word, "RETURN" | "RETURNED" | "REVERSAL" | "REVERSED"));
    let status = match (refund, returned, total < 0) {
        (true, false, _) => PaymentStatus::Refunded,
        (false, true, _) => PaymentStatus::Reversed,
        (false, false, false) => PaymentStatus::CountyPosted,
        _ => {
            return Err(invalid(
                "county return classification is missing or ambiguous",
            ));
        }
    };
    let number = match record.installment_key.as_str() {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    };
    let amount = total
        .checked_abs()
        .ok_or_else(|| invalid("county payment overflow"))?;
    let tax = tax
        .checked_abs()
        .ok_or_else(|| invalid("county payment overflow"))?;
    let allocations = if let Some(installment) = number.filter(|_| tax > 0) {
        vec![Allocation {
            bill: BillIdentity {
                jurisdiction: request.property.key.jurisdiction.clone(),
                parcel: request.property.key.parcel.clone(),
                fiscal_year: record.tax_year,
                sequence: record.sequence.clone(),
                source_version: "unversioned".into(),
            },
            installment,
            amount_cents: tax,
        }]
    } else {
        Vec::new()
    };
    let payment = Payment {
        reference: format!("la-payment:{}", record.payment_id),
        status,
        amount_cents: amount,
        allocations,
    };
    let id = hex::encode(Sha256::digest(
        serde_json::to_vec(&(record, updated, &request.property.key))
            .map_err(|_| invalid("county payment could not be encoded"))?,
    ));
    Ok(Observation {
        id: format!("la-history:{id}"),
        binding: request.property.key.clone(),
        evidence: Evidence {
            source: SourceKind::County,
            document: format!(
                "LA county payment history; source snapshot {updated}; {}",
                record.group_description
            ),
            url: Some(HISTORY_VIEWER.into()),
            observed_at: request.requested_at,
            effective_at: local_midnight(payment_date)?,
        },
        observation: ObservationData::Payment(payment),
    })
}
async fn read_history(
    request: &CountySyncRequest,
    client: &Client,
    sources: &Sources,
    requested_ain: &str,
) -> Result<(Vec<Observation>, Coverage), CountyError> {
    let initial = body(
        client
            .get(sources.history.clone())
            .send()
            .await
            .map_err(|_| CountyError::Unavailable)?,
    )
    .await?;
    let config = ajax_config(&initial, &sources.history)?;
    let mut records = BTreeMap::new();
    let mut expected = None;
    let mut page = 1;
    loop {
        let form = [
            ("action", "phf_fetch_data".to_owned()),
            ("ain", requested_ain.to_owned()),
            ("page", page.to_string()),
            ("nonce", config.nonce.clone()),
        ];
        let html = body(
            client
                .post(&config.ajax_url)
                .header("Referer", sources.history.as_str())
                .header("X-Requested-With", "XMLHttpRequest")
                .form(&form)
                .send()
                .await
                .map_err(|_| CountyError::Unavailable)?,
        )
        .await?;
        let parsed = parse_page(&html, requested_ain)?;
        let metadata = (parsed.updated, parsed.pages, parsed.total);
        if expected.is_some_and(|prior| prior != metadata) {
            return Err(invalid("county history changed during pagination"));
        }
        expected = Some(metadata);
        for record in parsed.records {
            if let Some(prior) = records.insert(record.payment_id, record.clone())
                && prior != record
            {
                return Err(invalid(
                    "county history has conflicting duplicate payment identifiers",
                ));
            }
        }
        if records.len() > 1000 {
            return Err(invalid("county payment history exceeded its limit"));
        }
        if page >= parsed.pages {
            break;
        }
        page += 1;
    }
    let (updated, _, total) = expected.ok_or_else(|| invalid("county history returned no page"))?;
    if usize::try_from(total).ok() != Some(records.len()) {
        return Err(invalid("county history page count did not reconcile"));
    }
    if updated
        > request
            .requested_at
            .with_timezone(&Los_Angeles)
            .date_naive()
    {
        return Err(invalid("county history source snapshot is in the future"));
    }
    let observations = records
        .values()
        .map(|record| observation(record, updated, request))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|value| request.property.includes_observation(value))
        .collect();
    let stale = request
        .requested_at
        .with_timezone(&Los_Angeles)
        .date_naive()
        .signed_duration_since(updated)
        .num_days()
        > 7;
    let detail = format!(
        "County payment-history viewer, source snapshot {updated}; published three-fiscal-year window only. {} County-posted rows are not bank-submission receipts; fees are not credited as tax principal.",
        if stale {
            "The source snapshot is stale."
        } else {
            "Lifetime coverage is not established."
        }
    );
    Ok((
        observations,
        coverage(
            CoverageCapability::Payments,
            CoverageStatus::Partial,
            detail,
            request.requested_at,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use crate::county::los_angeles::tax_collector::{
        HistoryEnvelope, Sources, cents, collect_sources, observation, parse_page,
    };
    use crate::{
        county::{CountyError, CountySyncRequest},
        tax::ledger::{
            BindingKey, CountyFips, CoverageCapability, CoverageStatus, Evidence, ObservationData,
            ParcelId, PaymentStatus, PropertyBinding, SourceKind,
        },
    };
    use chrono::{TimeZone, Utc};
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    #[derive(Clone, Copy)]
    enum Case {
        Normal,
        BillOutage,
        BillStall,
        WrongBinding,
        MalformedMoney,
        OverlappingPage,
        ChangedSnapshot,
        TooManyPages,
        UnsafeBillAction,
        UnknownInstallment,
        Returned,
        PositiveRefund,
        MixedReturn,
        FutureSnapshot,
        Fees,
        ConflictingDuplicate,
        NoHistory,
    }
    struct Site {
        sources: Sources,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<String>>>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Site {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    impl Site {
        fn new(case: Case) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let sources = Sources {
                bill: format!("{origin}/request-duplicate-bill/").parse().unwrap(),
                result: format!("{origin}/secured-property-tax-results")
                    .parse()
                    .unwrap(),
                history: format!("{origin}/property-tax-payment-history/")
                    .parse()
                    .unwrap(),
                balance: format!("{origin}/balance").parse().unwrap(),
            };
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let thread = thread::spawn(move || {
                let mut workers = Vec::new();
                while !stopped.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    let origin = origin.clone();
                    let seen = seen.clone();
                    let stopped = stopped.clone();
                    workers.push(thread::spawn(move || {
                    let Some((method, path, headers, body)) = read_request(&mut stream) else { return; };
                    let form: BTreeMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    seen.lock().unwrap().push(format!("{method} {path}"));
                    if matches!(case, Case::BillStall) && path == "/secured-property-tax-results" {
                        // Keep this socket open without blocking other dashboard reads.
                        while !stopped.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(2));
                        }
                        return;
                    }
                    let(status,content,cookie)=match path.as_str(){
                        "/request-duplicate-bill/"=>{
                            let action=if matches!(case,Case::UnsafeBillAction){"/request-duplicate-bill-mail/"}else{"/secured-property-tax-results"};
                            (200,format!("<form id='ainform' method='post' action='{action}' target='_blank'><input name='ain' type='number'><input type='hidden' name='timestamp'><input type='hidden' name='__VIEWSTATE' value='fictional-state'></form>"),true)
                        },
                        "/secured-property-tax-results"=>{
                            assert_eq!(method,"POST");assert_eq!(form.get("ain").map(String::as_str),Some("0000000001"));
                            assert_eq!(form.get("__VIEWSTATE").map(String::as_str),Some("fictional-state"));
                            assert!(form.get("timestamp").unwrap().parse::<i64>().unwrap()>1_000_000_000);
                            assert!(headers.get("cookie").is_some_and(|v|v.contains("lookup_session=fictional")));
                            assert!(headers.get("referer").unwrap().ends_with("/request-duplicate-bill/"));
                            if matches!(case,Case::BillOutage){(503,"unavailable".into(),false)}else{(200,"<h2>Record not found, make sure the AIN is valid!</h2>".into(),false)}
                        },
                        "/property-tax-payment-history/"=>(200,format!("<script id='phf-script-js-extra'>var phf_ajax = {{\"ajax_url\":\"{origin}/wp-admin/admin-ajax.php\",\"nonce\":\"fictional-nonce\"}};</script>"),true),
                        "/wp-admin/admin-ajax.php"=>{
                            assert_eq!(method,"POST");assert_eq!(form.get("action").map(String::as_str),Some("phf_fetch_data"));assert_eq!(form.get("ain").map(String::as_str),Some("0000000001"));
                            assert_eq!(form.get("nonce").map(String::as_str),Some("fictional-nonce"));assert!(headers.get("cookie").is_some_and(|v|v.contains("lookup_session=fictional")));
                            let page=form.get("page").unwrap();
                            let mut envelope:HistoryEnvelope=serde_json::from_str(if page=="1"{include_str!("../../../tests/fixtures/county/tax_history_page_1.json")}else{include_str!("../../../tests/fixtures/county/tax_history_page_2.json")}).unwrap();
                            let records=envelope.data.data.as_mut().unwrap();
                            match case{
                                Case::WrongBinding if page=="2"=>records[0].ain="0000000002".into(),
                                Case::MalformedMoney=>records[0].tax_paid="1,2X.00".into(),
                                Case::OverlappingPage if page=="2"=>{
                                    let first:HistoryEnvelope=serde_json::from_str(include_str!("../../../tests/fixtures/county/tax_history_page_1.json")).unwrap();records.insert(0,first.data.data.unwrap().remove(0));
                                },
                                Case::ChangedSnapshot if page=="2"=>envelope.data.meta.as_mut().unwrap().updated="2030-11-07".into(),
                                Case::TooManyPages=>envelope.data.meta.as_mut().unwrap().pages=21,
                                Case::FutureSnapshot=>envelope.data.meta.as_mut().unwrap().updated="2030-11-08".into(),
                                Case::ConflictingDuplicate if page=="2"=>records[0].payment_id=1001,
                                Case::NoHistory=>{envelope.data.status=404;envelope.data.data=None;envelope.data.meta=None;},
                                Case::PositiveRefund=>records[0].group_description="Fictional refund".into(),
                                Case::Fees=>{let record=&mut records[0];record.penalty_paid="60.00".into();record.cost_paid="10.00".into();record.total_paid=if page=="1"{"670.00"}else{"370.00"}.into();},
                                Case::MixedReturn if page=="2"=>{let record=&mut records[0];record.group_description="Fictional refund".into();record.sequence="000".into();record.effective_date="11/06/2030".into();record.tax_paid="-600.00".into();record.total_paid="-600.00".into();},
                                Case::UnknownInstallment=>records[0].installment_key="0".into(),
                                Case::Returned=>{let record=&mut records[0];record.group_description="Fictional returned check".into();record.tax_paid=format!("-{}",record.tax_paid);record.total_paid=format!("-{}",record.total_paid);},
                                _=>{},
                            }
                            (200,serde_json::to_string(&envelope).unwrap(),false)
                        },
                        "/balance"=>(200,"<form id='termsForm' method='post' action='index.php?page=terms'><button class='g-recaptcha'>Submit</button></form>".into(),false),
                        _=>(500,"unexpected request".into(),false),
                    };
                    let cookie = if cookie {
                        "Set-Cookie: lookup_session=fictional; Path=/\r\n"
                    } else {
                        ""
                    };
                    write!(stream,"HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nContent-Type: text/html\r\n{cookie}Connection: close\r\n\r\n{content}",content.len()).unwrap();
                    }));
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            });
            Self {
                sources,
                stop,
                requests,
                thread: Some(thread),
            }
        }
    }
    fn read_request(
        stream: &mut TcpStream,
    ) -> Option<(String, String, BTreeMap<String, String>, String)> {
        // On macOS, accepted sockets inherit the listener's nonblocking flag.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end;
        loop {
            let mut part = [0; 4096];
            let len = stream.read(&mut part).ok()?;
            if len == 0 {
                return None;
            }
            bytes.extend_from_slice(&part[..len]);
            if let Some(position) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
            assert!(bytes.len() < 65536);
        }
        let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = head.split("\r\n");
        let request: Vec<_> = lines.next().unwrap().split_whitespace().collect();
        let method = request[0].to_owned();
        let path = request[1].to_owned();
        let headers: BTreeMap<String, String> = lines
            .filter_map(|v| v.split_once(':'))
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().into()))
            .collect();
        let length = headers
            .get("content-length")
            .map(|v| v.parse::<usize>().unwrap())
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let mut part = [0; 4096];
            let len = stream.read(&mut part).ok()?;
            if len == 0 {
                return None;
            }
            bytes.extend_from_slice(&part[..len]);
        }
        Some((
            method,
            path,
            headers,
            String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap(),
        ))
    }
    fn request() -> CountySyncRequest {
        let at = Utc.with_ymd_and_hms(2030, 11, 7, 12, 0, 0).unwrap();
        let key = BindingKey {
            jurisdiction: CountyFips::los_angeles(),
            parcel: ParcelId::try_from("0000000001".to_owned()).unwrap(),
        };
        CountySyncRequest {
            property: PropertyBinding {
                bought_on: None,
                key,
                label: "Fictional property".into(),
                confirmed_at: at,
                confirmation: Evidence {
                    source: SourceKind::County,
                    document: "Fictional county parcel confirmation".into(),
                    url: None,
                    observed_at: at,
                    effective_at: at,
                },
            },
            cursor: None,
            requested_at: at,
        }
    }
    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }
    #[tokio::test]
    async fn normal_forms_cookies_and_pages_collect_postings_despite_other_source_gaps() {
        let site = Site::new(Case::Normal);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.observations.len(), 2);
        for value in &read.observations {
            let ObservationData::Payment(payment) = &value.observation else {
                panic!("expected payment")
            };
            assert_eq!(payment.status, PaymentStatus::CountyPosted);
            assert_eq!(payment.allocations.len(), 1);
            assert_eq!(payment.allocations[0].installment, 1);
            assert_eq!(payment.allocations[0].bill.source_version, "unversioned");
            assert_eq!(value.binding, request().property.key);
        }
        assert!(
            read.coverage
                .iter()
                .any(|v| v.capability == CoverageCapability::Payments
                    && v.status == CoverageStatus::Partial)
        );
        assert!(
            read.coverage
                .iter()
                .any(|v| v.capability == CoverageCapability::Notices
                    && v.status == CoverageStatus::Unavailable
                    && v.detail.contains("no record"))
        );
        assert!(read.coverage.iter().any(
            |v| v.capability == CoverageCapability::Balances && v.detail.contains("reCAPTCHA")
        ));
        assert_eq!(site.requests.lock().unwrap().len(), 6);
    }
    #[tokio::test]
    async fn notice_outage_does_not_discard_working_payment_reads() {
        let site = Site::new(Case::BillOutage);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.observations.len(), 2);
    }
    #[tokio::test]
    async fn stalled_bill_read_cannot_discard_completed_payment_history() {
        let site = Site::new(Case::BillStall);
        let read = tokio::time::timeout(
            Duration::from_secs(3),
            collect_sources(&request(), &client(), &site.sources, Duration::from_secs(1)),
        )
        .await
        .expect("each capability must finish before the outer county deadline")
        .unwrap();
        assert_eq!(read.observations.len(), 2);
        assert!(
            read.coverage
                .iter()
                .any(|value| value.capability == CoverageCapability::Notices
                    && value.status == CoverageStatus::Unavailable)
        );
        assert!(
            read.coverage
                .iter()
                .any(|value| value.capability == CoverageCapability::Payments
                    && value.status == CoverageStatus::Partial)
        );
    }
    #[tokio::test]
    async fn mismatched_or_malformed_or_inconsistent_sources_are_not_empty_success() {
        for case in [
            Case::WrongBinding,
            Case::MalformedMoney,
            Case::ChangedSnapshot,
            Case::TooManyPages,
            Case::FutureSnapshot,
            Case::ConflictingDuplicate,
            Case::NoHistory,
        ] {
            let site = Site::new(case);
            let read =
                collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
                    .await
                    .unwrap();
            assert!(read.observations.is_empty());
            assert!(
                read.coverage
                    .iter()
                    .any(|v| v.capability == CoverageCapability::Payments
                        && v.status == CoverageStatus::Unavailable)
            );
        }
    }
    #[tokio::test]
    async fn identical_overlapping_rows_are_deduplicated_but_every_expected_record_is_retained() {
        let site = Site::new(Case::OverlappingPage);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.observations.len(), 2);
        assert_ne!(read.observations[0].id, read.observations[1].id);
    }
    #[tokio::test]
    async fn changed_form_cannot_submit_a_support_or_duplicate_notice_request() {
        let site = Site::new(Case::UnsafeBillAction);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(read.observations.len(), 2);
        assert!(
            !site
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path == "POST /request-duplicate-bill-mail/"
                    || path == "POST /secured-property-tax-results")
        );
    }
    #[tokio::test]
    async fn unsupported_installment_never_guesses_an_allocation() {
        let site = Site::new(Case::UnknownInstallment);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(read.observations.iter().all(
            |v| matches!(&v.observation,ObservationData::Payment(p)if p.allocations.is_empty())
        ));
    }
    #[tokio::test]
    async fn explicit_return_retains_source_reference_and_does_not_create_posted_credit() {
        let site = Site::new(Case::Returned);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(read.observations.iter().all(|v|matches!(&v.observation,ObservationData::Payment(p)if p.status==PaymentStatus::Reversed&&p.amount_cents>0&&p.allocations[0].installment==1)));
    }
    #[test]
    fn decimal_money_never_rounds_or_fills_missing_values() {
        assert_eq!(cents("1,234.56").unwrap(), 123456);
        assert_eq!(cents("-0.25").unwrap(), -25);
        for value in [
            "",
            "1.234",
            "1,2.34",
            "NaN",
            "1e3",
            "1.2",
            "1.00oops",
            "92233720368547758.08",
        ] {
            assert!(cents(value).is_err());
        }
    }
    #[test]
    fn actual_typed_parser_rejects_foreign_binding_and_preserves_source_event_dates() {
        let raw = include_str!("../../../tests/fixtures/county/tax_history_page_1.json");
        assert!(matches!(
            parse_page(raw, "0000000002"),
            Err(CountyError::BindingMismatch)
        ));
        let page = parse_page(raw, "0000000001").unwrap();
        let value = observation(&page.records[0], page.updated, &request()).unwrap();
        assert_eq!(
            value.evidence.effective_at,
            Utc.with_ymd_and_hms(2030, 11, 5, 8, 0, 0).unwrap()
        );
        assert!(value.evidence.document.contains("2030-11-06"));
    }
    #[tokio::test]
    async fn explicit_positive_refund_is_never_a_county_posted_credit() {
        let site = Site::new(Case::PositiveRefund);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(read.observations.iter().all(|value| matches!(&value.observation, ObservationData::Payment(payment) if payment.status == PaymentStatus::Refunded)));
    }
    #[tokio::test]
    async fn penalty_and_cost_are_retained_in_total_but_not_credited_as_principal() {
        let site = Site::new(Case::Fees);
        let read = collect_sources(&request(), &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        let ObservationData::Payment(payment) = &read.observations[0].observation else {
            panic!("expected payment")
        };
        assert_eq!(payment.amount_cents, 67000);
        assert_eq!(payment.allocations[0].amount_cents, 60000);
    }
    #[tokio::test]
    async fn real_producer_does_not_leave_prior_credit_paid_after_differently_identified_refund() {
        use crate::tax::{
            ledger::{BillIdentity, BillKind, Installment, Notice, Observation, Snapshot},
            reconcile::{InstallmentState, reconcile},
        };
        let site = Site::new(Case::MixedReturn);
        let request = request();
        let read = collect_sources(&request, &client(), &site.sources, Duration::from_secs(5))
            .await
            .unwrap();
        let references: Vec<_> = read
            .observations
            .iter()
            .filter_map(|value| match &value.observation {
                ObservationData::Payment(payment) => Some(payment.reference.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(references.len(), 2);
        assert_ne!(references[0], references[1]);
        let identity = BillIdentity {
            jurisdiction: request.property.key.jurisdiction.clone(),
            parcel: request.property.key.parcel.clone(),
            fiscal_year: 2030,
            sequence: "000".into(),
            source_version: "unversioned".into(),
        };
        let notice = Observation {
            id: "fictional-notice".into(),
            binding: request.property.key.clone(),
            evidence: request.property.confirmation.clone(),
            observation: ObservationData::Notice(Notice {
                identity,
                bill_kind: BillKind::Annual,
                supersedes: None,
                installments: vec![
                    Installment {
                        number: 1,
                        amount_cents: 60000,
                        due_date: chrono::NaiveDate::from_ymd_opt(2030, 11, 1).unwrap(),
                        delinquent_at: Utc.with_ymd_and_hms(2030, 12, 11, 1, 0, 0).unwrap(),
                        timezone: chrono_tz::America::Los_Angeles,
                    },
                    Installment {
                        number: 2,
                        amount_cents: 60000,
                        due_date: chrono::NaiveDate::from_ymd_opt(2031, 2, 1).unwrap(),
                        delinquent_at: Utc.with_ymd_and_hms(2031, 4, 11, 0, 0, 0).unwrap(),
                        timezone: chrono_tz::America::Los_Angeles,
                    },
                ],
            }),
        };
        let mut observations = vec![notice];
        observations.extend(read.observations);
        let snapshot = Snapshot {
            revision: 1,
            property: Some(request.property),
            observations,
            coverage: read.coverage,
        };
        let result = reconcile(&snapshot, request.requested_at);
        let first = result
            .installments
            .iter()
            .find(|value| value.number == 1)
            .unwrap();
        let second = result
            .installments
            .iter()
            .find(|value| value.number == 2)
            .unwrap();
        assert_ne!(first.state, InstallmentState::Paid);
        assert_eq!(second.state, InstallmentState::Open);
    }
    #[tokio::test]
    async fn wrong_jurisdiction_and_unknown_cursor_never_start_a_lookup() {
        let site = Site::new(Case::Normal);
        let mut request = request();
        request.property.key.jurisdiction = CountyFips::try_from("06001".to_owned()).unwrap();
        assert!(matches!(
            collect_sources(&request, &client(), &site.sources, Duration::from_secs(5)).await,
            Err(CountyError::BindingMismatch)
        ));
        request.property.key.jurisdiction = CountyFips::los_angeles();
        request.cursor = Some("fictional-unknown-cursor".into());
        assert!(matches!(
            collect_sources(&request, &client(), &site.sources, Duration::from_secs(5)).await,
            Err(CountyError::InvalidCursor)
        ));
        assert!(site.requests.lock().unwrap().is_empty());
    }
}
