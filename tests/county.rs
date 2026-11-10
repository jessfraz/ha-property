use chrono::Utc;
use property::{
    county::{CollectionMode, County, CountyError, CountySyncRequest, los_angeles::LosAngeles},
    tax::ledger::{
        Balance, BalanceScope, BillIdentity, BindingKey, CountyFips, CoverageStatus, Evidence,
        Observation, ObservationData, ParcelId, PropertyBinding, SourceKind,
    },
};

fn property() -> PropertyBinding {
    let now = Utc::now();
    PropertyBinding {
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

#[tokio::test]
async fn la_collection_keeps_unavailable_coverage_explicit_and_never_claims_no_tax_due() {
    let county = LosAngeles::new();
    let property = property();
    assert_eq!(county.jurisdiction(), property.key.jurisdiction);
    assert_eq!(county.capabilities().len(), 4);
    assert!(
        county
            .capabilities()
            .iter()
            .all(|capability| capability.mode == CollectionMode::ManualImport)
    );
    let page = county
        .sync(CountySyncRequest {
            property: property.clone(),
            cursor: None,
            requested_at: Utc::now(),
        })
        .await
        .unwrap();
    page.validate_binding(&property.key).unwrap();
    assert!(page.observations.is_empty());
    assert_eq!(page.coverage.len(), 4);
    assert!(
        page.coverage
            .iter()
            .all(|coverage| coverage.status == CoverageStatus::Unavailable
                && !coverage.detail.is_empty())
    );
    assert!(page.next_cursor.is_none());
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
    let mut page = county
        .sync(CountySyncRequest {
            property: confirmed.clone(),
            cursor: None,
            requested_at: Utc::now(),
        })
        .await
        .unwrap();
    page.binding.parcel = ParcelId::try_from("FICTIONAL-OTHER-PARCEL".to_owned()).unwrap();
    assert!(matches!(
        page.validate_binding(&confirmed.key),
        Err(CountyError::BindingMismatch)
    ));
}

#[tokio::test]
async fn a_correct_page_envelope_cannot_hide_a_foreign_embedded_bill() {
    let county = LosAngeles::new();
    let confirmed = property();
    let mut page = county
        .sync(CountySyncRequest {
            property: confirmed.clone(),
            cursor: None,
            requested_at: Utc::now(),
        })
        .await
        .unwrap();
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
