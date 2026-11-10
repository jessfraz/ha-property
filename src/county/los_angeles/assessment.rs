//! Public, county-published annual assessment rolls. This is the documented
//! ArcGIS query service, not a private Assessor-dashboard backend.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, Utc};
use reqwest::{Client, Url};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    county::{CountyError, CountySyncRequest},
    tax::ledger::{
        Assessment, BindingKey, CountyFips, Coverage, CoverageCapability, CoverageStatus, Evidence,
        Observation, ObservationData, ParcelId, SourceKind, TaxError,
    },
};

pub const LAYER_URL: &str = "https://services.arcgis.com/RmCCgQtiZLDCtblq/arcgis/rest/services/Parcel_Data_2021_Table/FeatureServer/0";
const ROLL_FIELDS: &str = "AIN,RollYear,Roll_LandValue,Roll_ImpValue,Roll_FixtureValue,Roll_PersPropValue,Roll_TotalValue,Roll_TotalExemption,netTaxableValue";
const ADDRESS_FIELDS: &str = "AIN,RollYear,PropertyLocation";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ROLL_ROWS: usize = 100;
const MAX_ADDRESS_ROWS: usize = 100;

#[derive(Debug)]
pub struct AssessmentRead {
    pub observations: Vec<Observation>,
    pub coverage: Coverage,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddressCandidate {
    pub key: BindingKey,
    pub address: String,
    pub roll_year: u16,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddressCandidates {
    pub candidates: Vec<AddressCandidate>,
    pub coverage: Coverage,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GisError {
    code: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum GisResponse<T> {
    Error {
        error: GisError,
    },
    Records {
        features: Vec<Feature<T>>,
        #[serde(rename = "exceededTransferLimit", default)]
        exceeded_transfer_limit: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Feature<T> {
    attributes: T,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LayerMetadata {
    #[serde(rename = "editingInfo")]
    editing_info: EditingInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct EditingInfo {
    #[serde(rename = "dataLastEditDate")]
    data_last_edit_date: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct RollAttributes {
    #[serde(rename = "AIN")]
    ain: Option<String>,
    #[serde(rename = "RollYear")]
    roll_year: Option<String>,
    #[serde(rename = "Roll_LandValue")]
    land: Option<i64>,
    #[serde(rename = "Roll_ImpValue")]
    improvements: Option<i64>,
    #[serde(rename = "Roll_FixtureValue")]
    fixtures: Option<i64>,
    #[serde(rename = "Roll_PersPropValue")]
    personal_property: Option<i64>,
    #[serde(rename = "Roll_TotalValue")]
    total: Option<i64>,
    #[serde(rename = "Roll_TotalExemption")]
    exemptions: Option<i64>,
    #[serde(rename = "netTaxableValue")]
    net_taxable: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct AddressAttributes {
    #[serde(rename = "AIN")]
    ain: Option<String>,
    #[serde(rename = "RollYear")]
    roll_year: Option<String>,
    #[serde(rename = "PropertyLocation")]
    address: Option<String>,
}

/// Read all bounded annual-roll rows for the configured, confirmed AIN.
/// Request/response bodies and identifiers never enter error messages.
pub async fn collect(
    request: &CountySyncRequest,
    client: &Client,
) -> Result<AssessmentRead, CountyError> {
    let layer = Url::parse(LAYER_URL).map_err(|_| CountyError::Unavailable)?;
    collect_from(request, client, &layer).await
}

async fn collect_from(
    request: &CountySyncRequest,
    client: &Client,
    layer: &Url,
) -> Result<AssessmentRead, CountyError> {
    if request.property.key.jurisdiction != CountyFips::los_angeles() {
        return Err(CountyError::BindingMismatch);
    }
    let ain = canonical_ain(request.property.key.parcel.as_str())?;
    let published_at = publication_time(client, layer, request.requested_at).await?;
    let mut query_url = layer.clone();
    query_url.set_path(&format!("{}/query", layer.path().trim_end_matches('/')));
    let where_clause = match request.property.first_roll_year() {
        Some(year) => format!("AIN = '{ain}' AND RollYear >= '{year:04}'"),
        None => format!("AIN = '{ain}'"),
    };
    let query = [
        ("f", "json".to_owned()),
        ("where", where_clause),
        ("outFields", ROLL_FIELDS.to_owned()),
        ("returnGeometry", "false".to_owned()),
        ("orderByFields", "RollYear DESC".to_owned()),
        ("resultRecordCount", MAX_ROLL_ROWS.to_string()),
    ];
    let response: GisResponse<RollAttributes> = get_json(client, &query_url, &query).await?;
    parse_rolls(response, request, &ain, published_at, layer)
}

/// Return a bounded, explicit candidate set from the county's situs-address
/// field. Matching a prefix does not select a parcel or confirm a binding.
/// This county-owned dataset establishes county FIPS; ZIP codes do not.
pub async fn address_candidates(
    client: &Client,
    address: &str,
    observed_at: DateTime<Utc>,
) -> Result<AddressCandidates, CountyError> {
    let layer = Url::parse(LAYER_URL).map_err(|_| CountyError::Unavailable)?;
    address_candidates_from(client, address, observed_at, &layer).await
}

async fn address_candidates_from(
    client: &Client,
    address: &str,
    observed_at: DateTime<Utc>,
    layer: &Url,
) -> Result<AddressCandidates, CountyError> {
    if address.len() > 250 {
        return Err(invalid("enter a bounded street address"));
    }
    let address = StreetLookup::parse(address)?;
    // Query a bounded superset without a street suffix or mailing locality.
    // Exact normalized street matching below admits RD/ROAD, never another road.
    let literal = address
        .query_words
        .iter()
        .map(|word| word.replace('\'', "''"))
        .collect::<Vec<_>>()
        .join("%");
    let published_at = publication_time(client, layer, observed_at).await?;
    let mut query_url = layer.clone();
    query_url.set_path(&format!("{}/query", layer.path().trim_end_matches('/')));
    let query = [
        ("f", "json".to_owned()),
        (
            "where",
            format!(
                "SitusHouseNo = {} AND UPPER(PropertyLocation) LIKE '{literal}%'",
                address.house
            ),
        ),
        ("outFields", ADDRESS_FIELDS.to_owned()),
        ("returnGeometry", "false".to_owned()),
        ("orderByFields", "AIN ASC,RollYear DESC".to_owned()),
        ("resultRecordCount", MAX_ADDRESS_ROWS.to_string()),
    ];
    let response: GisResponse<AddressAttributes> = get_json(client, &query_url, &query).await?;
    parse_candidates(
        response,
        &CandidateLookup::Address(address),
        observed_at,
        published_at,
        layer,
    )
}

pub async fn parcel_candidates(
    client: &Client,
    parcel: &str,
    observed_at: DateTime<Utc>,
) -> Result<AddressCandidates, CountyError> {
    let layer = Url::parse(LAYER_URL).map_err(|_| CountyError::Unavailable)?;
    parcel_candidates_from(client, parcel, observed_at, &layer).await
}

async fn parcel_candidates_from(
    client: &Client,
    parcel: &str,
    observed_at: DateTime<Utc>,
    layer: &Url,
) -> Result<AddressCandidates, CountyError> {
    let ain = canonical_ain(parcel)?;
    let published_at = publication_time(client, layer, observed_at).await?;
    let mut query_url = layer.clone();
    query_url.set_path(&format!("{}/query", layer.path().trim_end_matches('/')));
    let query = [
        ("f", "json".to_owned()),
        ("where", format!("AIN = '{ain}'")),
        ("outFields", ADDRESS_FIELDS.to_owned()),
        ("returnGeometry", "false".to_owned()),
        ("orderByFields", "AIN ASC,RollYear DESC".to_owned()),
        ("resultRecordCount", MAX_ADDRESS_ROWS.to_string()),
    ];
    let response: GisResponse<AddressAttributes> = get_json(client, &query_url, &query).await?;
    parse_candidates(
        response,
        &CandidateLookup::Parcel(ain),
        observed_at,
        published_at,
        layer,
    )
}

enum CandidateLookup {
    Address(StreetLookup),
    Parcel(String),
}

struct StreetLookup {
    house: u32,
    street: Vec<String>,
    unit: Option<String>,
    complete_street: bool,
    query_words: Vec<String>,
}

fn suffix(word: &str) -> Option<&'static str> {
    match word {
        "ROAD" | "RD" => Some("RD"),
        "STREET" | "ST" => Some("ST"),
        "AVENUE" | "AVE" => Some("AVE"),
        "DRIVE" | "DR" => Some("DR"),
        "LANE" | "LN" => Some("LN"),
        "COURT" | "CT" => Some("CT"),
        "PLACE" | "PL" => Some("PL"),
        "BOULEVARD" | "BLVD" => Some("BLVD"),
        "CIRCLE" | "CIR" => Some("CIR"),
        "TERRACE" | "TER" => Some("TER"),
        "WAY" => Some("WAY"),
        "HIGHWAY" | "HWY" => Some("HWY"),
        "PARKWAY" | "PKWY" => Some("PKWY"),
        "TRAIL" | "TRL" => Some("TRL"),
        _ => None,
    }
}
fn direction(word: &str) -> Option<&'static str> {
    match word {
        "NORTH" | "N" => Some("N"),
        "SOUTH" | "S" => Some("S"),
        "EAST" | "E" => Some("E"),
        "WEST" | "W" => Some("W"),
        "NORTHEAST" | "NE" => Some("NE"),
        "NORTHWEST" | "NW" => Some("NW"),
        "SOUTHEAST" | "SE" => Some("SE"),
        "SOUTHWEST" | "SW" => Some("SW"),
        _ => None,
    }
}
impl StreetLookup {
    fn parse(address: &str) -> Result<Self, CountyError> {
        if address.len() < 5
            || address.len() > 500
            || address.chars().any(char::is_control)
            || address.contains(['%', '_'])
        {
            return Err(invalid(
                "enter a bounded street address without query wildcards",
            ));
        }
        let first_line = address
            .split(',')
            .next()
            .ok_or_else(|| invalid("a street address is required"))?;
        let mut words = first_line
            .split_whitespace()
            .map(|word| word.trim_matches('.').to_uppercase())
            .collect::<Vec<_>>();
        let house = words
            .first()
            .and_then(|word| word.parse::<u32>().ok())
            .filter(|n| *n > 0 && *n <= i32::MAX.unsigned_abs())
            .ok_or_else(|| invalid("begin the street address with its house number"))?;
        words[0] = house.to_string();
        let suffix_at = words
            .iter()
            .rposition(|word| suffix(word).is_some())
            .filter(|index| *index >= 2);
        let mut end = suffix_at.map_or(words.len(), |i| i + 1);
        if suffix_at.is_some() && words.get(end).is_some_and(|word| direction(word).is_some()) {
            end += 1;
        }
        let unit = words.get(end).and_then(|word| {
            if matches!(
                word.as_str(),
                "UNIT" | "APT" | "APARTMENT" | "SUITE" | "STE" | "#"
            ) {
                words.get(end + 1).cloned()
            } else {
                word.strip_prefix('#')
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            }
        });
        let street = words[..end]
            .iter()
            .map(|word| {
                suffix(word)
                    .or_else(|| direction(word))
                    .unwrap_or(word)
                    .to_owned()
            })
            .collect();
        let name_start = if words.get(1).is_some_and(|word| direction(word).is_some()) {
            2
        } else {
            1
        };
        let name_end = suffix_at.unwrap_or(end);
        if name_start >= name_end {
            return Err(invalid("include the street name"));
        }
        let query_words = std::iter::once(house.to_string())
            .chain(words[name_start..name_end].iter().cloned())
            .collect();
        Ok(Self {
            house,
            street,
            unit,
            complete_street: suffix_at.is_some(),
            query_words,
        })
    }
    fn matches(&self, address: &str) -> Result<bool, CountyError> {
        let other = Self::parse(address)?;
        if self.house != other.house {
            return Err(CountyError::BindingMismatch);
        }
        Ok((if self.complete_street {
            self.street == other.street
        } else {
            other.street.starts_with(&self.street)
        }) && self
            .unit
            .as_ref()
            .is_none_or(|unit| other.unit.as_ref() == Some(unit)))
    }
}

async fn publication_time(
    client: &Client,
    layer: &Url,
    observed_at: DateTime<Utc>,
) -> Result<DateTime<Utc>, CountyError> {
    let metadata: LayerMetadata = get_json(client, layer, &[("f", "json".to_owned())]).await?;
    let timestamp = DateTime::from_timestamp_millis(metadata.editing_info.data_last_edit_date)
        .ok_or_else(|| invalid("county assessment publication time is invalid"))?;
    if timestamp > observed_at + Duration::minutes(5) || timestamp.year() < 1900 {
        return Err(invalid("county assessment publication time is invalid"));
    }
    Ok(timestamp)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &Url,
    query: &[(&str, String)],
) -> Result<T, CountyError> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut response = client
            .get(url.clone())
            .query(query)
            .send()
            .await
            .map_err(|_| CountyError::Unavailable)?;
        if !response.status().is_success() || response.url().origin() != url.origin() {
            return Err(CountyError::Unavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| CountyError::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err(CountyError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| CountyError::Unavailable)
    })
    .await
    .map_err(|_| CountyError::Unavailable)?
}

fn parse_rolls(
    response: GisResponse<RollAttributes>,
    request: &CountySyncRequest,
    expected_ain: &str,
    published_at: DateTime<Utc>,
    layer: &Url,
) -> Result<AssessmentRead, CountyError> {
    let GisResponse::Records {
        features,
        exceeded_transfer_limit,
    } = response
    else {
        return Err(CountyError::Unavailable);
    };
    if features.is_empty() {
        if request.property.bought_on.is_some() {
            return Ok(AssessmentRead {
                observations:Vec::new(),
                coverage:Coverage {capability:CoverageCapability::Assessments,status:CoverageStatus::Partial,
                    checked_at:request.requested_at,
                    detail:"The county returned no annual roll within the configured history window. The source supplies roll years, not precise assessment event dates; this is not proof that no assessment or bill exists.".into()},
            });
        }
        return Err(invalid(
            "the county returned no annual assessment record for the confirmed parcel",
        ));
    }
    if features.len() > MAX_ROLL_ROWS {
        return Err(invalid(
            "county assessment response exceeded its requested row bound",
        ));
    }
    let mut by_year: BTreeMap<u16, RollAttributes> = BTreeMap::new();
    let mut conflicting = BTreeSet::new();
    let mut rejected = 0;
    for feature in features {
        let attributes = feature.attributes;
        if let Some(ain) = &attributes.ain {
            if canonical_ain(ain)? != expected_ain {
                return Err(CountyError::BindingMismatch);
            }
        } else {
            rejected += 1;
            continue;
        }
        let year = match roll_year(attributes.roll_year.as_deref()) {
            Ok(year) => year,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        if validated_values(&attributes).is_err() {
            rejected += 1;
            continue;
        }
        if let Some(prior) = by_year.get(&year) {
            if prior != &attributes {
                conflicting.insert(year);
            }
        } else {
            by_year.insert(year, attributes);
        }
    }
    for year in &conflicting {
        by_year.remove(year);
    }
    rejected += conflicting.len();
    if by_year.is_empty() {
        return Err(invalid(
            "county assessment records are incomplete or contradictory",
        ));
    }
    let mut observations = Vec::new();
    let mut incomplete = 0;
    let latest_year = by_year
        .keys()
        .next_back()
        .copied()
        .ok_or(CountyError::Unavailable)?;
    for (year, attributes) in by_year {
        let values = validated_values(&attributes)?;
        if values.is_incomplete() {
            incomplete += 1;
        }
        let encoded =
            serde_json::to_vec(&(layer.as_str(), expected_ain, published_at, &attributes))
                .map_err(|_| invalid("county assessment evidence could not be encoded"))?;
        let id = format!("la-assessment-{}", hex::encode(Sha256::digest(encoded)));
        let observation = Observation {
            id,
            binding: request.property.key.clone(),
            evidence: evidence(year, published_at, request.requested_at, layer),
            observation: ObservationData::Assessment(Assessment {
                fiscal_year: year,
                assessed_value_cents: values
                    .total
                    .checked_mul(100)
                    .ok_or_else(|| invalid("county assessed value overflow"))?,
                description: format!(
                    "Published annual roll {year}: land {}; improvements {}; fixtures {}; personal property {}; exemptions {}; net taxable value {}. GIS publication {}. Not a tax bill or live balance.",
                    dollars(values.land),
                    dollars(values.improvements),
                    dollars(values.fixtures),
                    dollars(values.personal_property),
                    dollars(values.exemptions),
                    dollars(values.net_taxable),
                    published_at.to_rfc3339(),
                ),
            }),
        };
        if request.property.includes_observation(&observation) {
            observations.push(observation);
        }
    }
    Ok(AssessmentRead {
        observations,
        coverage: annual_coverage(
            request.requested_at,
            published_at,
            latest_year,
            exceeded_transfer_limit,
            rejected,
            incomplete,
        ),
    })
}

struct Values {
    land: Option<i64>,
    improvements: Option<i64>,
    fixtures: Option<i64>,
    personal_property: Option<i64>,
    total: i64,
    exemptions: Option<i64>,
    net_taxable: Option<i64>,
}
impl Values {
    fn is_incomplete(&self) -> bool {
        [
            self.land,
            self.improvements,
            self.fixtures,
            self.personal_property,
            self.exemptions,
            self.net_taxable,
        ]
        .iter()
        .any(Option::is_none)
    }
}
fn validated_values(attributes: &RollAttributes) -> Result<Values, CountyError> {
    let values = Values {
        land: component(attributes.land)?,
        improvements: component(attributes.improvements)?,
        fixtures: component(attributes.fixtures)?,
        personal_property: component(attributes.personal_property)?,
        total: component(attributes.total)?
            .ok_or_else(|| invalid("county gross assessed value is unknown"))?,
        exemptions: component(attributes.exemptions)?,
        net_taxable: component(attributes.net_taxable)?,
    };
    let components = [
        values.land,
        values.improvements,
        values.fixtures,
        values.personal_property,
    ];
    if components.iter().all(Option::is_some) {
        let sum = components
            .into_iter()
            .flatten()
            .try_fold(0_i64, |sum, value| sum.checked_add(value))
            .ok_or_else(|| invalid("county assessment components overflow"))?;
        if sum != values.total {
            return Err(invalid("county assessment components disagree"));
        }
    }
    if let (Some(exemptions), Some(net_taxable)) = (values.exemptions, values.net_taxable)
        && values.total.checked_sub(exemptions) != Some(net_taxable)
    {
        return Err(invalid("county assessment components disagree"));
    }
    values
        .total
        .checked_mul(100)
        .ok_or_else(|| invalid("county assessed value overflow"))?;
    Ok(values)
}
fn component(value: Option<i64>) -> Result<Option<i64>, CountyError> {
    if value.is_some_and(|value| value < 0) {
        return Err(invalid("county assessment component is invalid"));
    }
    Ok(value)
}
fn dollars(value: Option<i64>) -> String {
    value
        .map(|value| format!("USD {value}.00"))
        .unwrap_or_else(|| "unknown".into())
}
fn canonical_ain(value: &str) -> Result<String, CountyError> {
    let ain: String = value
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    if ain.len() != 10
        || value
            .chars()
            .any(|character| !character.is_ascii_digit() && character != '-' && character != ' ')
    {
        return Err(invalid(
            "Los Angeles parcel lookup requires a ten-digit AIN",
        ));
    }
    Ok(ain)
}
fn roll_year(value: Option<&str>) -> Result<u16, CountyError> {
    value
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|year| (1900..=2200).contains(year))
        .ok_or_else(|| invalid("county assessment roll year is missing or invalid"))
}
fn evidence(
    year: u16,
    published_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    layer: &Url,
) -> Evidence {
    Evidence {
        source: SourceKind::County,
        document: format!(
            "LA County Assessor annual roll {year}; GIS data publication {}",
            published_at.to_rfc3339()
        ),
        url: Some(layer.to_string()),
        observed_at,
        effective_at: published_at,
    }
}
fn annual_coverage(
    observed_at: DateTime<Utc>,
    published_at: DateTime<Utc>,
    latest_year: u16,
    truncated: bool,
    rejected: usize,
    incomplete: usize,
) -> Coverage {
    let stale = i32::from(latest_year) < observed_at.year();
    Coverage {
        capability: CoverageCapability::Assessments,
        status: CoverageStatus::Partial,
        checked_at: observed_at,
        detail: format!(
            "Official annual assessment rolls; latest returned roll {latest_year}, GIS data publication {}. {} Supplemental assessments and live tax balances are not covered. {}{}{}",
            published_at.to_rfc3339(),
            if stale {
                "The latest returned roll predates the current calendar year."
            } else {
                "Annual-roll coverage is not complete assessment history."
            },
            if truncated {
                "The source result was truncated. "
            } else {
                ""
            },
            if rejected > 0 {
                "Some source rows were incomplete or contradictory and were not imported. "
            } else {
                ""
            },
            if incomplete > 0 {
                "Some published components are unknown; no missing value was treated as zero."
            } else {
                ""
            }
        ),
    }
}
fn parse_candidates(
    response: GisResponse<AddressAttributes>,
    expected: &CandidateLookup,
    observed_at: DateTime<Utc>,
    published_at: DateTime<Utc>,
    layer: &Url,
) -> Result<AddressCandidates, CountyError> {
    let GisResponse::Records {
        features,
        exceeded_transfer_limit,
    } = response
    else {
        return Err(CountyError::Unavailable);
    };
    if features.len() > MAX_ADDRESS_ROWS {
        return Err(invalid(
            "county address response exceeded its requested row bound",
        ));
    }
    let source_had_rows = !features.is_empty();
    let mut valid_rows = 0;
    let mut candidates: BTreeMap<String, AddressCandidate> = BTreeMap::new();
    let mut rejected = 0;
    for feature in features {
        let attributes = feature.attributes;
        let candidate = (|| -> Result<AddressCandidate, CountyError> {
            let ain = canonical_ain(
                attributes
                    .ain
                    .as_deref()
                    .ok_or_else(|| invalid("county address row has no parcel identity"))?,
            )?;
            let year = roll_year(attributes.roll_year.as_deref())?;
            let address = attributes
                .address
                .filter(|value| {
                    !value.trim().is_empty()
                        && value.len() <= 500
                        && !value.chars().any(char::is_control)
                })
                .ok_or_else(|| invalid("county address row has no valid situs address"))?;
            Ok(AddressCandidate {
                key: BindingKey {
                    jurisdiction: CountyFips::los_angeles(),
                    parcel: ParcelId::try_from(ain)?,
                },
                address,
                roll_year: year,
                evidence: evidence(year, published_at, observed_at, layer),
            })
        })();
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(CountyError::BindingMismatch) => return Err(CountyError::BindingMismatch),
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        valid_rows += 1;
        match expected {
            CandidateLookup::Address(address) if !address.matches(&candidate.address)? => continue,
            CandidateLookup::Parcel(ain) if candidate.key.parcel.as_str() != ain => {
                return Err(CountyError::BindingMismatch);
            }
            _ => {}
        }
        let key = candidate.key.parcel.as_str().to_owned();
        if let Some(prior) = candidates.get(&key) {
            if prior.roll_year == candidate.roll_year && prior.address != candidate.address {
                return Err(invalid(
                    "county address records conflict for the same parcel and roll",
                ));
            }
            if prior.roll_year >= candidate.roll_year {
                continue;
            }
        }
        candidates.insert(key, candidate);
    }
    if source_had_rows && valid_rows == 0 {
        return Err(invalid(
            "county address records were invalid, not an empty lookup",
        ));
    }
    let latest_year = candidates
        .values()
        .map(|candidate| candidate.roll_year)
        .max()
        .unwrap_or_default();
    let mut coverage = annual_coverage(
        observed_at,
        published_at,
        latest_year,
        exceeded_transfer_limit,
        rejected,
        0,
    );
    if candidates.is_empty() {
        coverage.detail="No address candidate was established by the published annual-roll table. This does not establish that the property is absent; verify the exact situs address and current county records.".into();
    }
    Ok(AddressCandidates {
        candidates: candidates.into_values().collect(),
        coverage,
        truncated: exceeded_transfer_limit,
    })
}
fn invalid(message: &'static str) -> CountyError {
    CountyError::InvalidEvidence(TaxError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use chrono::{DateTime, Utc};
    use reqwest::{Client, Url};

    use crate::county::los_angeles::assessment::{
        AddressAttributes, EditingInfo, Feature, GisError, GisResponse, LayerMetadata,
        RollAttributes, address_candidates_from, collect_from,
    };
    use crate::{
        county::{CountyError, CountySyncRequest},
        tax::ledger::{
            BindingKey, CountyFips, CoverageStatus, Evidence, ImportInput, ObservationData,
            ParcelId, PropertyBinding, SourceKind,
        },
    };

    fn timestamp(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }
    fn request() -> CountySyncRequest {
        let observed = timestamp("2030-11-10T12:00:00Z");
        CountySyncRequest {
            property: PropertyBinding {
                bought_on: None,
                key: BindingKey {
                    jurisdiction: CountyFips::los_angeles(),
                    parcel: ParcelId::try_from("0000-000-001".to_owned()).unwrap(),
                },
                label: "Fictional example property".into(),
                confirmed_at: observed,
                confirmation: Evidence {
                    source: SourceKind::County,
                    document: "Fictional parcel confirmation".into(),
                    url: Some("https://example.invalid/source".into()),
                    observed_at: observed,
                    effective_at: observed,
                },
            },
            cursor: None,
            requested_at: observed,
        }
    }
    fn roll(year: u16) -> RollAttributes {
        RollAttributes {
            ain: Some("0000000001".into()),
            roll_year: Some(year.to_string()),
            land: Some(200_000),
            improvements: Some(300_000),
            fixtures: Some(1_000),
            personal_property: Some(2_000),
            total: Some(503_000),
            exemptions: Some(7_000),
            net_taxable: Some(496_000),
        }
    }
    fn metadata() -> Vec<u8> {
        serde_json::to_vec(&LayerMetadata {
            editing_info: EditingInfo {
                data_last_edit_date: timestamp("2030-03-01T10:00:00Z").timestamp_millis(),
            },
        })
        .unwrap()
    }
    fn records<T: serde::Serialize>(rows: Vec<T>, truncated: bool) -> Vec<u8> {
        serde_json::to_vec(&GisResponse::Records {
            features: rows
                .into_iter()
                .map(|attributes| Feature { attributes })
                .collect(),
            exceeded_transfer_limit: truncated,
        })
        .unwrap()
    }
    struct Server {
        layer: Url,
        requests: Arc<Mutex<Vec<String>>>,
        worker: thread::JoinHandle<()>,
    }
    impl Server {
        fn new(responses: Vec<(u16, Vec<u8>)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let layer = Url::parse(&format!(
                "http://{}/FeatureServer/0",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            let worker = thread::spawn(move || {
                for (status, body) in responses {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 2048];
                    while !bytes.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let count = socket.read(&mut buffer).unwrap();
                        if count == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        assert!(bytes.len() < 16384);
                    }
                    captured
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(bytes).unwrap());
                    let header = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes());
                    let _ = socket.write_all(&body);
                }
            });
            Self {
                layer,
                requests,
                worker,
            }
        }
        fn finish(self) -> Vec<String> {
            self.worker.join().unwrap();
            Arc::try_unwrap(self.requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }
    fn client() -> Client {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }
    fn params(request: &str) -> BTreeMap<String, String> {
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        Url::parse(&format!("http://example.invalid{target}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[tokio::test]
    async fn actual_http_query_collects_bound_annual_rolls_and_preserves_source_publication() {
        let fixture = request();
        let server = Server::new(vec![
            (200, metadata()),
            (200, records(vec![roll(2029), roll(2028)], false)),
        ]);
        let result = collect_from(&fixture, &client(), &server.layer)
            .await
            .unwrap();
        assert_eq!(result.observations.len(), 2);
        assert_eq!(result.coverage.status, CoverageStatus::Partial);
        assert!(result.coverage.detail.contains("predates"));
        for observation in &result.observations {
            assert_eq!(observation.binding, fixture.property.key);
            assert_eq!(observation.evidence.observed_at, fixture.requested_at);
            assert_eq!(
                observation.evidence.effective_at,
                timestamp("2030-03-01T10:00:00Z")
            );
            let ObservationData::Assessment(assessment) = &observation.observation else {
                panic!("expected assessment")
            };
            assert_eq!(assessment.assessed_value_cents, 50_300_000);
            assert!(assessment.description.contains("land USD 200000.00"));
            assert!(
                assessment
                    .description
                    .contains("net taxable value USD 496000.00")
            );
        }
        ImportInput {
            property: fixture.property.clone(),
            observations: result.observations,
            coverage: vec![result.coverage],
        }
        .validate()
        .unwrap();
        let captured = server.finish();
        assert_eq!(captured.len(), 2);
        let query = params(&captured[1]);
        assert_eq!(query["where"], "AIN = '0000000001'");
        assert_eq!(query["returnGeometry"], "false");
        assert_eq!(query["resultRecordCount"], "100");
        assert!(!query["outFields"].contains("PropertyLocation"));
        assert_eq!(query["orderByFields"], "RollYear DESC");
    }
    #[tokio::test]
    async fn collection_identity_is_stable_but_read_freshness_advances() {
        let original = request();
        let mut later = original.clone();
        later.requested_at += chrono::Duration::days(1);
        let first = Server::new(vec![
            (200, metadata()),
            (200, records(vec![roll(2030)], false)),
            (200, metadata()),
            (200, records(vec![roll(2030)], false)),
        ]);
        let one = collect_from(&original, &client(), &first.layer)
            .await
            .unwrap();
        let two = collect_from(&later, &client(), &first.layer).await.unwrap();
        assert_eq!(one.observations[0].id, two.observations[0].id);
        assert_ne!(
            one.observations[0].evidence.observed_at,
            two.observations[0].evidence.observed_at
        );
        assert_eq!(
            one.observations[0].evidence.effective_at,
            two.observations[0].evidence.effective_at
        );
        assert!(two.coverage.checked_at > one.coverage.checked_at);
        first.finish();
    }
    #[tokio::test]
    async fn unknown_components_do_not_erase_a_published_total_or_become_zero() {
        let mut row = roll(2030);
        row.fixtures = None;
        row.personal_property = None;
        let server = Server::new(vec![(200, metadata()), (200, records(vec![row], false))]);
        let result = collect_from(&request(), &client(), &server.layer)
            .await
            .unwrap();
        let ObservationData::Assessment(assessment) = &result.observations[0].observation else {
            panic!("expected assessment")
        };
        assert_eq!(assessment.assessed_value_cents, 50_300_000);
        assert!(assessment.description.contains("fixtures unknown"));
        assert!(result.coverage.detail.contains("unknown"));
        server.finish();
    }
    #[tokio::test]
    async fn malformed_year_or_total_keeps_other_verified_years_and_reports_partial() {
        let mut bad = roll(2030);
        bad.total = None;
        let server = Server::new(vec![
            (200, metadata()),
            (200, records(vec![bad, roll(2029)], true)),
        ]);
        let result = collect_from(&request(), &client(), &server.layer)
            .await
            .unwrap();
        assert_eq!(result.observations.len(), 1);
        assert!(result.coverage.detail.contains("truncated"));
        assert!(result.coverage.detail.contains("not imported"));
        server.finish();
    }
    #[tokio::test]
    async fn conflicting_duplicate_years_are_not_selected_by_response_order() {
        let mut changed = roll(2030);
        changed.land = Some(201_000);
        changed.total = Some(504_000);
        changed.net_taxable = Some(497_000);
        let server = Server::new(vec![
            (200, metadata()),
            (200, records(vec![roll(2030), changed, roll(2029)], false)),
        ]);
        let result = collect_from(&request(), &client(), &server.layer)
            .await
            .unwrap();
        assert_eq!(result.observations.len(), 1);
        let ObservationData::Assessment(assessment) = &result.observations[0].observation else {
            panic!("expected assessment")
        };
        assert_eq!(assessment.fiscal_year, 2029);
        server.finish();
    }
    #[tokio::test]
    async fn no_record_wrong_parcel_component_conflict_and_overflow_are_not_empty_successes() {
        let mut wrong = roll(2030);
        wrong.ain = Some("0000000002".into());
        let mut conflict = roll(2030);
        conflict.total = Some(999_000);
        let mut overflow = roll(2030);
        overflow.land = Some(i64::MAX);
        overflow.improvements = Some(i64::MAX);
        let cases = [
            records(Vec::<RollAttributes>::new(), false),
            records(vec![wrong], false),
            records(vec![conflict], false),
            records(vec![overflow], false),
        ];
        for (index, body) in cases.into_iter().enumerate() {
            let server = Server::new(vec![(200, metadata()), (200, body)]);
            let result = collect_from(&request(), &client(), &server.layer).await;
            if index == 1 {
                assert!(matches!(result, Err(CountyError::BindingMismatch)));
            } else {
                assert!(result.is_err());
            }
            server.finish();
        }
    }
    #[tokio::test]
    async fn county_errors_html_and_http_outages_never_decode_as_missing_assessments() {
        let error = serde_json::to_vec(&GisResponse::<RollAttributes>::Error {
            error: GisError { code: 400 },
        })
        .unwrap();
        for (status, body) in [
            (200, error),
            (200, b"<html>maintenance</html>".to_vec()),
            (503, b"unavailable".to_vec()),
        ] {
            let server = Server::new(vec![(200, metadata()), (status, body)]);
            assert!(matches!(
                collect_from(&request(), &client(), &server.layer).await,
                Err(CountyError::Unavailable)
            ));
            server.finish();
        }
    }
    #[tokio::test]
    async fn oversized_responses_and_future_publication_dates_are_rejected() {
        let large = Server::new(vec![(200, vec![b'x'; 1024 * 1024 + 1])]);
        assert!(matches!(
            collect_from(&request(), &client(), &large.layer).await,
            Err(CountyError::Unavailable)
        ));
        large.finish();
        let future = serde_json::to_vec(&LayerMetadata {
            editing_info: EditingInfo {
                data_last_edit_date: timestamp("2040-01-01T00:00:00Z").timestamp_millis(),
            },
        })
        .unwrap();
        let server = Server::new(vec![(200, future)]);
        assert!(
            collect_from(&request(), &client(), &server.layer)
                .await
                .is_err()
        );
        server.finish();
    }
    fn address(ain: &str, year: u16, situs: &str) -> AddressAttributes {
        AddressAttributes {
            ain: Some(ain.into()),
            roll_year: Some(year.to_string()),
            address: Some(situs.into()),
        }
    }
    #[tokio::test]
    async fn address_lookup_returns_every_distinct_candidate_without_guessing_or_confirming() {
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![
                        address("0000000002", 2030, "42 EXAMPLE AVE UNIT 2 EXAMPLE CITY CA"),
                        address("0000000001", 2030, "42 EXAMPLE AVE UNIT 1 EXAMPLE CITY CA"),
                        address("0000000001", 2029, "42 EXAMPLE AVE UNIT 1 EXAMPLE CITY CA"),
                    ],
                    true,
                ),
            ),
        ]);
        let result = address_candidates_from(
            &client(),
            " 42  Example Ave ",
            request().requested_at,
            &server.layer,
        )
        .await
        .unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.coverage.status, CoverageStatus::Partial);
        assert!(
            result
                .candidates
                .iter()
                .all(
                    |candidate| candidate.key.jurisdiction == CountyFips::los_angeles()
                        && candidate.roll_year == 2030
                )
        );
        assert_ne!(result.candidates[0].key, result.candidates[1].key);
        let captured = server.finish();
        let query = params(&captured[1]);
        assert_eq!(
            query["where"],
            "SitusHouseNo = 42 AND UPPER(PropertyLocation) LIKE '42%EXAMPLE%'"
        );
        assert_eq!(query["outFields"], "AIN,RollYear,PropertyLocation");
        assert_eq!(query["resultRecordCount"], "100");
    }
    #[tokio::test]
    async fn address_query_escapes_quotes_and_refuses_user_wildcards_before_network() {
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![address(
                        "0000000001",
                        2030,
                        "42 O'EXAMPLE AVE EXAMPLE CITY CA",
                    )],
                    false,
                ),
            ),
        ]);
        let result = address_candidates_from(
            &client(),
            "42 O'Example Ave",
            request().requested_at,
            &server.layer,
        )
        .await
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        let captured = server.finish();
        assert_eq!(
            params(&captured[1])["where"],
            "SitusHouseNo = 42 AND UPPER(PropertyLocation) LIKE '42%O''EXAMPLE%'"
        );
        let unreachable = Url::parse("http://127.0.0.1:1/FeatureServer/0").unwrap();
        for input in ["%", "42 EXAMPLE%", "42 EXAMPLE_", "42\nExample"] {
            assert!(matches!(
                address_candidates_from(&client(), input, request().requested_at, &unreachable)
                    .await,
                Err(CountyError::InvalidEvidence(_))
            ));
        }
    }
    #[tokio::test]
    async fn wrong_address_invalid_candidates_and_county_errors_are_not_valid_empty_searches() {
        let sources = [
            records(
                vec![address("0000000001", 2030, "99 OTHER ROAD EXAMPLE CITY CA")],
                false,
            ),
            records(
                vec![AddressAttributes {
                    ain: None,
                    roll_year: Some("2030".into()),
                    address: Some("42 EXAMPLE AVE".into()),
                }],
                false,
            ),
            serde_json::to_vec(&GisResponse::<AddressAttributes>::Error {
                error: GisError { code: 500 },
            })
            .unwrap(),
        ];
        for body in sources {
            let server = Server::new(vec![(200, metadata()), (200, body)]);
            assert!(
                address_candidates_from(
                    &client(),
                    "42 Example Ave",
                    request().requested_at,
                    &server.layer
                )
                .await
                .is_err()
            );
            server.finish();
        }
        let empty = Server::new(vec![
            (200, metadata()),
            (200, records(Vec::<AddressAttributes>::new(), false)),
        ]);
        let result = address_candidates_from(
            &client(),
            "42 Example Ave",
            request().requested_at,
            &empty.layer,
        )
        .await
        .unwrap();
        assert!(result.candidates.is_empty());
        assert!(result.coverage.detail.contains("does not establish"));
        empty.finish();
    }
    #[tokio::test]
    async fn county_situs_spacing_is_normalized_without_offering_other_street_candidates() {
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![
                        address(
                            "0000000001",
                            2030,
                            "42  EXAMPLE  AVE UNIT 1 EXAMPLE CITY CA",
                        ),
                        address("0000000003", 2030, "42 EXAMPLE TERRACE AVE EXAMPLE CITY CA"),
                        address("0000000002", 2030, "42 EXAMPLE AVE UNIT 2 EXAMPLE CITY CA"),
                    ],
                    false,
                ),
            ),
        ]);
        let result = address_candidates_from(
            &client(),
            "42 Example Ave",
            request().requested_at,
            &server.layer,
        )
        .await
        .unwrap();
        let identities: Vec<&str> = result
            .candidates
            .iter()
            .map(|candidate| candidate.key.parcel.as_str())
            .collect();
        assert_eq!(identities, ["0000000001", "0000000002"]);
        assert!(result.candidates[0].address.contains("  "));
        server.finish();
    }

    #[tokio::test]
    async fn full_mailing_address_matches_county_abbreviated_street() {
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![address(
                        "0000000001",
                        2030,
                        "42 EXAMPLE RD EXAMPLE CITY CA 90000",
                    )],
                    false,
                ),
            ),
        ]);
        let result = address_candidates_from(
            &client(),
            "42 Example Road, Example City, CA 90000",
            request().requested_at,
            &server.layer,
        )
        .await
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].key.parcel.as_str(), "0000000001");
        assert_eq!(result.coverage.status, CoverageStatus::Partial);
        server.finish();
    }

    #[tokio::test]
    async fn parcel_lookup_canonicalizes_ain_and_rejects_foreign_candidates() {
        use crate::county::los_angeles::assessment::parcel_candidates_from;
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![
                        address("0000000001", 2029, "42 EXAMPLE RD EXAMPLE CITY CA"),
                        address("0000000001", 2030, "42 EXAMPLE RD EXAMPLE CITY CA"),
                    ],
                    false,
                ),
            ),
        ]);
        let result = parcel_candidates_from(
            &client(),
            "0000-000-001",
            request().requested_at,
            &server.layer,
        )
        .await
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].key.parcel.as_str(), "0000000001");
        assert_eq!(result.candidates[0].roll_year, 2030);
        let calls = server.finish();
        assert_eq!(params(&calls[1])["where"], "AIN = '0000000001'");
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(
                    vec![address("0000000002", 2030, "42 EXAMPLE RD EXAMPLE CITY CA")],
                    false,
                ),
            ),
        ]);
        assert!(matches!(
            parcel_candidates_from(
                &client(),
                "0000000001",
                request().requested_at,
                &server.layer
            )
            .await,
            Err(CountyError::BindingMismatch)
        ));
        server.finish();
        let unreachable = Url::parse("http://127.0.0.1:1/FeatureServer/0").unwrap();
        for parcel in [
            "000000001",
            "00000000001",
            "0000000001' OR 1=1",
            "AIN:0000000001",
        ] {
            assert!(matches!(
                parcel_candidates_from(&client(), parcel, request().requested_at, &unreachable)
                    .await,
                Err(CountyError::InvalidEvidence(_))
            ));
        }
    }

    #[tokio::test]
    async fn suffix_direction_and_mailing_locality_never_select_another_street() {
        for query in [
            "42 North Example Road, Example City, CA 90000",
            "42 N Example Rd. Example City CA 90000",
        ] {
            let server = Server::new(vec![
                (200, metadata()),
                (
                    200,
                    records(
                        vec![
                            address("0000000001", 2030, "42 N EXAMPLE RD UNIT 1 EXAMPLE CITY CA"),
                            address(
                                "0000000002",
                                2030,
                                "42 NORTH EXAMPLE ROAD UNIT 2 EXAMPLE CITY CA",
                            ),
                            address(
                                "0000000003",
                                2030,
                                "42 N EXAMPLE TERRACE RD EXAMPLE CITY CA",
                            ),
                            address("0000000004", 2030, "42 S EXAMPLE RD EXAMPLE CITY CA"),
                        ],
                        true,
                    ),
                ),
            ]);
            let result =
                address_candidates_from(&client(), query, request().requested_at, &server.layer)
                    .await
                    .unwrap();
            let ids: Vec<_> = result
                .candidates
                .iter()
                .map(|c| c.key.parcel.as_str())
                .collect();
            assert_eq!(ids, ["0000000001", "0000000002"]);
            assert!(result.truncated);
            server.finish();
        }
    }

    #[tokio::test]
    async fn bought_on_filters_roll_year_without_substituting_new_publication_time() {
        let server = Server::new(vec![
            (200, metadata()),
            (
                200,
                records(vec![roll(2023), roll(2024), roll(2025)], false),
            ),
        ]);
        let mut request = request();
        request.property.bought_on = Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 1).unwrap());
        let result = collect_from(&request, &client(), &server.layer)
            .await
            .unwrap();
        assert_eq!(result.observations.len(), 1);
        assert!(
            matches!(&result.observations[0].observation,ObservationData::Assessment(value) if value.fiscal_year==2025)
        );
        let calls = server.finish();
        assert_eq!(
            params(&calls[1])["where"],
            "AIN = '0000000001' AND RollYear >= '2025'"
        );
        let empty = Server::new(vec![
            (200, metadata()),
            (200, records(Vec::<RollAttributes>::new(), false)),
        ]);
        let result = collect_from(&request, &client(), &empty.layer)
            .await
            .unwrap();
        assert!(result.observations.is_empty());
        assert_eq!(result.coverage.status, CoverageStatus::Partial);
        empty.finish();
    }
}
