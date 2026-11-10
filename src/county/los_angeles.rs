use std::time::Duration;

use crate::{
    county::{
        CollectionMode, County, CountyCapability, CountyError, CountySyncPage, CountySyncRequest,
    },
    tax::ledger::{
        CountyFips, Coverage, CoverageCapability, CoverageStatus, ImportInput, SourceKind,
    },
};

pub mod assessment;
pub mod tax_collector;

/// Each ordinary county flow is independent. A CAPTCHA or changed dashboard
/// cannot erase working assessment/history reads or become a zero balance.
#[derive(Debug, Clone, Copy, Default)]
pub struct LosAngeles;

impl LosAngeles {
    pub fn new() -> Self {
        Self
    }
}

impl County for LosAngeles {
    fn jurisdiction(&self) -> CountyFips {
        CountyFips::los_angeles()
    }

    fn capabilities(&self) -> Vec<CountyCapability> {
        vec![
            CountyCapability {capability: CoverageCapability::Notices, mode: CollectionMode::HttpForm,
                official_url: "https://ttc.lacounty.gov/request-duplicate-bill/".into(),
                detail: "Read the ordinary bill-viewer form. A no-record or unrecognized result is unavailable evidence, not proof that no bill exists.".into()},
            CountyCapability {capability: CoverageCapability::Assessments, mode: CollectionMode::HttpApi,
                official_url: assessment::LAYER_URL.into(),
                detail: "Read county-published annual assessment rolls through documented ArcGIS queries. Historical publication coverage remains explicit.".into()},
            CountyCapability {capability: CoverageCapability::Balances, mode: CollectionMode::ManualImport,
                official_url: "https://vcheck.ttc.lacounty.gov/".into(),
                detail: "Check the ordinary balance viewer. Its current terms form requires human reCAPTCHA verification; no challenge is bypassed or balance inferred.".into()},
            CountyCapability {capability: CoverageCapability::Payments, mode: CollectionMode::HttpForm,
                official_url: "https://ttc.lacounty.gov/property-tax-payment-history/".into(),
                detail: "Read the public payment-history viewer's current nonce, session and paginated form flow. This is a web adapter, not a promised stable API.".into()},
        ]
    }

    async fn sync(&self, request: CountySyncRequest) -> Result<CountySyncPage, CountyError> {
        if request.property.key.jurisdiction != self.jurisdiction() {
            return Err(CountyError::BindingMismatch);
        }
        if request.cursor.is_some() {
            return Err(CountyError::InvalidCursor);
        }
        // The cookie jar belongs to this read session. It is neither an account
        // enrollment nor a shared global browser/profile with another parcel.
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(12))
            .user_agent(concat!(
                "ha-property/",
                env!("CARGO_PKG_VERSION"),
                " (read-only county collection)"
            ))
            .build()
            .map_err(|_| CountyError::Unavailable)?;
        let (assessments, taxes) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(25),
                assessment::collect(&request, &client)
            ),
            tokio::time::timeout(
                Duration::from_secs(60),
                tax_collector::collect(&request, &client)
            ),
        );
        let mut observations = Vec::new();
        let mut coverage = Vec::new();
        match assessments {
            Ok(Ok(read)) => {observations.extend(read.observations); coverage.push(read.coverage);}
            _ => coverage.push(Coverage {capability: CoverageCapability::Assessments,
                status: CoverageStatus::Unavailable, checked_at: request.requested_at,
                detail: "The county assessment source could not be read and bound to the confirmed parcel. Existing assessments are unchanged.".into()}),
        }
        match taxes {
            Ok(Ok(read)) => {
                observations.extend(read.observations);
                coverage.extend(read.coverage);
            }
            _ => {
                for capability in [
                    CoverageCapability::Notices,
                    CoverageCapability::Payments,
                    CoverageCapability::Balances,
                ] {
                    coverage.push(Coverage {capability, status: CoverageStatus::Unavailable, checked_at: request.requested_at,
                        detail: "The ordinary county tax flow did not return verified evidence. Existing notices, balances and payments are unchanged.".into()});
                }
            }
        }
        if observations
            .iter()
            .any(|value| value.evidence.source != SourceKind::County)
        {
            return Err(CountyError::Unavailable);
        }
        observations.retain(|value| request.property.includes_observation(value));
        ImportInput {
            property: request.property.clone(),
            observations: observations.clone(),
            coverage: coverage.clone(),
        }
        .validate()?;
        let page = CountySyncPage {
            binding: request.property.key.clone(),
            observations,
            coverage,
            next_cursor: None,
        };
        page.validate_binding(&request.property.key)?;
        Ok(page)
    }
}
