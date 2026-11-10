use chrono::Utc;
use property::{
    county::{CountySyncRequest, los_angeles::tax_collector},
    tax::ledger::{BindingKey, CountyFips, Evidence, ParcelId, PropertyBinding, SourceKind},
};

#[tokio::test]
async fn malformed_ain_is_rejected_before_any_county_request_instead_of_empty_success() {
    let at = Utc::now();
    let request = CountySyncRequest {
        property: PropertyBinding {
            bought_on: None,
            key: BindingKey {
                jurisdiction: CountyFips::los_angeles(),
                parcel: ParcelId::try_from("FICTIONAL-NOT-AN-AIN".to_owned()).unwrap(),
            },
            label: "Fictional property".into(),
            confirmed_at: at,
            confirmation: Evidence {
                source: SourceKind::County,
                document: "Fictional confirmation".into(),
                url: None,
                observed_at: at,
                effective_at: at,
            },
        },
        cursor: None,
        requested_at: at,
    };
    let result = tax_collector::collect(&request, &reqwest::Client::new()).await;
    assert!(result.is_err());
}
