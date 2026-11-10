use chrono::Utc;
use property::{
    county::{
        CollectionMode, County, CountyError, CountySyncPage, CountySyncRequest,
        los_angeles::LosAngeles,
    },
    tax::ledger::{
        Balance, BalanceScope, BillIdentity, BindingKey, CountyFips, Evidence, Observation,
        ObservationData, ParcelId, PropertyBinding, SourceKind,
    },
};

fn property() -> PropertyBinding {
    let now = Utc::now();
    PropertyBinding {
        bought_on: None,
        key: BindingKey {
            jurisdiction: CountyFips::los_angeles(),
            parcel: ParcelId::try_from("FICTIONAL-PARCEL-A".to_owned()).unwrap(),
        },
        label: "Example property".to_owned(),
        confirmed_at: now,
        confirmation: Evidence {
            source: SourceKind::County,
            document: "Fictional county confirmation".to_owned(),
            url: None,
            observed_at: now,
            effective_at: now,
        },
    }
}

#[test]
fn la_capabilities_expose_real_reads_and_keep_the_human_balance_boundary_explicit() {
    let county = LosAngeles::new();
    let capabilities = county.capabilities();
    assert_eq!(capabilities.len(), 4);
    assert!(
        capabilities
            .iter()
            .any(|value| value.mode == CollectionMode::HttpApi)
    );
    assert_eq!(
        capabilities
            .iter()
            .filter(|value| value.mode == CollectionMode::HttpForm)
            .count(),
        2
    );
    assert_eq!(
        capabilities
            .iter()
            .filter(|value| value.mode == CollectionMode::ManualImport)
            .count(),
        1
    );
}
fn empty_page(property: &PropertyBinding) -> CountySyncPage {
    CountySyncPage {
        binding: property.key.clone(),
        observations: Vec::new(),
        coverage: Vec::new(),
        next_cursor: None,
    }
}

#[tokio::test]
async fn wrong_county_wrong_parcel_or_cursor_is_not_an_empty_success() {
    let county = LosAngeles::new();
    let mut other = property();
    other.key.jurisdiction = CountyFips::try_from("06059".to_owned()).unwrap();
    assert!(matches!(
        county
            .sync(CountySyncRequest {
                property: other,
                cursor: None,
                requested_at: Utc::now()
            })
            .await,
        Err(CountyError::BindingMismatch)
    ));
    assert!(matches!(
        county
            .sync(CountySyncRequest {
                property: property(),
                cursor: Some("unexpected".to_owned()),
                requested_at: Utc::now()
            })
            .await,
        Err(CountyError::InvalidCursor)
    ));
    let confirmed = property();
    let mut page = empty_page(&confirmed);
    page.binding.parcel = ParcelId::try_from("FICTIONAL-OTHER-PARCEL".to_owned()).unwrap();
    assert!(matches!(
        page.validate_binding(&confirmed.key),
        Err(CountyError::BindingMismatch)
    ));
}

#[tokio::test]
async fn a_correct_page_envelope_cannot_hide_a_foreign_embedded_bill() {
    let confirmed = property();
    let mut page = empty_page(&confirmed);
    page.observations.push(Observation {
        id: "fictional-foreign-bill".to_owned(),
        binding: confirmed.key.clone(),
        evidence: confirmed.confirmation.clone(),
        observation: ObservationData::Balance(Balance {
            scope: BalanceScope::Bill {
                bill: BillIdentity {
                    jurisdiction: CountyFips::los_angeles(),
                    parcel: ParcelId::try_from("FICTIONAL-OTHER-PARCEL".to_owned()).unwrap(),
                    fiscal_year: 2030,
                    sequence: "annual".to_owned(),
                    source_version: "original".to_owned(),
                },
            },
            amount_cents: Some(0),
        }),
    });
    assert!(matches!(
        page.validate_binding(&confirmed.key),
        Err(CountyError::BindingMismatch)
    ));
}
