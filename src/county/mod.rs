//! County adapters collect evidence; they do not reconcile or initiate payments.

use std::future::Future;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tax::ledger::{
    BindingKey, CountyFips, Coverage, CoverageCapability, Observation, PropertyBinding, TaxError,
};

pub mod los_angeles;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollectionMode {
    ManualImport,
    HttpForm,
    BrowserWorker,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CountyCapability {
    pub capability: CoverageCapability,
    pub mode: CollectionMode,
    pub official_url: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CountySyncRequest {
    pub property: PropertyBinding,
    pub cursor: Option<String>,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CountySyncPage {
    pub binding: BindingKey,
    pub observations: Vec<Observation>,
    pub coverage: Vec<Coverage>,
    pub next_cursor: Option<String>,
}

impl CountySyncPage {
    pub fn validate_binding(&self, expected: &BindingKey) -> Result<(), CountyError> {
        if &self.binding != expected {
            return Err(CountyError::BindingMismatch);
        }
        for observation in &self.observations {
            match observation.validate_binding(expected) {
                Err(TaxError::BindingMismatch) => return Err(CountyError::BindingMismatch),
                result => result?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CountyError {
    #[error("the county result does not match the confirmed property")]
    BindingMismatch,
    #[error("this county does not support the requested operation")]
    Unsupported,
    #[error("county collection is unavailable; existing evidence is unchanged")]
    Unavailable,
    #[error("the county cursor is invalid")]
    InvalidCursor,
    #[error("county evidence is invalid")]
    InvalidEvidence(#[from] TaxError),
}

pub trait County {
    fn jurisdiction(&self) -> CountyFips;
    fn capabilities(&self) -> Vec<CountyCapability>;
    fn sync(
        &self,
        request: CountySyncRequest,
    ) -> impl Future<Output = Result<CountySyncPage, CountyError>> + Send;
}
