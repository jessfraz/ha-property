use crate::{
    county::{
        CollectionMode, County, CountyCapability, CountyError, CountySyncPage, CountySyncRequest,
    },
    tax::ledger::{CountyFips, Coverage, CoverageCapability, CoverageStatus},
};

/// The county publishes separate bill, balance and payment-history web flows.
/// No supported public API is established. Manual evidence import is useful now;
/// a web form must not be relabeled a stable API or return success with no data.
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
        [
            (CoverageCapability::Notices, "https://ttc.lacounty.gov/request-duplicate-bill/", "Official bill viewer uses an AIN form. Import the county notice; automated form collection is not enabled."),
            (CoverageCapability::Assessments, "https://assessor.lacounty.gov/", "Confirm the exact parcel with the Assessor and import authoritative assessment evidence."),
            (CoverageCapability::Balances, "https://vcheck.ttc.lacounty.gov/", "Balance lookup is separate from bill viewing. No supported public balance API is established."),
            (CoverageCapability::Payments, "https://ttc.lacounty.gov/", "County payment history is separate evidence. A bank-settled receipt is not county posting."),
        ].into_iter().map(|(capability, url, detail)| CountyCapability {
            capability, mode: CollectionMode::ManualImport, official_url: url.into(), detail: detail.into(),
        }).collect()
    }

    async fn sync(&self, request: CountySyncRequest) -> Result<CountySyncPage, CountyError> {
        if request.property.key.jurisdiction != self.jurisdiction() {
            return Err(CountyError::BindingMismatch);
        }
        if request.cursor.is_some() {
            return Err(CountyError::InvalidCursor);
        }
        // Deliberately no network request: collection requires an established,
        // authorized normal web flow. Empty observations retain explicit gaps.
        let page = CountySyncPage {
            binding: request.property.key.clone(),
            observations: Vec::new(),
            next_cursor: None,
            coverage: self
                .capabilities()
                .into_iter()
                .map(|capability| Coverage {
                    capability: capability.capability,
                    status: CoverageStatus::Unavailable,
                    checked_at: request.requested_at,
                    detail: capability.detail,
                })
                .collect(),
        };
        page.validate_binding(&request.property.key)?;
        Ok(page)
    }
}
