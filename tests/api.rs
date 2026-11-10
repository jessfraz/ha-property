use property::{
    api::{self, ApiError, Context, Page, PropertyView, System},
    store::{ApplyRequest, ApplyResult, Preview, Store},
    sync::{SyncRun, SyncStatus, SyncStore},
    tax::{
        ledger::{ImportInput, Snapshot},
        reconcile::{BillView, Reconciliation},
    },
};
use std::{net::SocketAddr, sync::Arc};

const OWNER: &str = "example-owner-credential-at-least-32-characters";
const INTEGRATION: &str = "example-integration-credential-at-least-32-characters";
fn input() -> ImportInput {
    serde_json::from_str(include_str!("fixtures/import.example.json")).unwrap()
}
async fn setup() -> (
    tempfile::TempDir,
    Store,
    SyncStore,
    Arc<Context>,
    dropshot::HttpServer<Arc<Context>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let syncs = SyncStore::open(dir.path().join("ledger.sync.sqlite3"))
        .await
        .unwrap();
    let context =
        Arc::new(Context::new(store.clone(), syncs.clone(), OWNER).with_integration(INTEGRATION));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
    (dir, store, syncs, context, server)
}
#[tokio::test]
async fn actual_http_auth_import_pagination_sync_and_cold_restore() {
    let (dir, store, syncs, context, server) = setup().await;
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{origin}/v1/property"))
        .header("x-remote-user-id", "example-user")
        .header("x-forwarded-for", "172.30.32.2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let before: PropertyView = client
        .get(format!("{origin}/v1/property"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before.revision, 0);
    assert!(before.property.is_none());
    let system: System = client
        .get(format!("{origin}/v1/system"))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(system.role, "integration");
    let untrusted = client
        .post(format!("{origin}/v1/imports/preview"))
        .bearer_auth(INTEGRATION)
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(untrusted.status(), 403);
    let response = client
        .post(format!("{origin}/v1/imports/preview"))
        .bearer_auth(OWNER)
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["etag"], "\"0\"");
    let preview: Preview = response.json().await.unwrap();
    assert_eq!(preview.revision, 0);
    assert!(store.snapshot().await.unwrap().observations.is_empty());
    let request = ApplyRequest {
        preview: preview.clone(),
        if_match: 0,
        idempotency_key: "example-import-001".into(),
    };
    let response = client
        .post(format!("{origin}/v1/imports"))
        .bearer_auth(OWNER)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 428);
    let applied: ApplyResult = client
        .post(format!("{origin}/v1/imports"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"0\"")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(applied.revision, 1);
    let replayed: ApplyResult = client
        .post(format!("{origin}/v1/imports"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"0\"")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(replayed.replayed);
    let summary: Reconciliation = client
        .get(format!("{origin}/v1/summary"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.known_balance_cents, Some(90000));
    assert!(summary.bills.iter().any(|v| v.missing_notice));
    assert!(
        summary
            .installments
            .iter()
            .filter(|v| v.number == 2)
            .all(|v| v.state != property::tax::reconcile::InstallmentState::Paid)
    );
    let first: Page<BillView> = client
        .get(format!("{origin}/v1/tax/bills?limit=1"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_page.unwrap();
    let next: Page<BillView> = client
        .get(format!("{origin}/v1/tax/bills"))
        .query(&[("limit", "1"), ("page_token", &cursor)])
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(first.items[0].id, next.items[0].id);
    let before_sync = store.snapshot().await.unwrap();
    let response = client
        .post(format!("{origin}/v1/sync"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let accepted: SyncRun = response.json().await.unwrap();
    assert_eq!(accepted.status, SyncStatus::Running);
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let run: SyncRun = client
                .get(format!("{origin}/v1/sync/{}", accepted.id))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if run.status != SyncStatus::Running {
                break run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(run.status, SyncStatus::Unavailable);
    assert_eq!(run.coverage.len(), 4);
    let receipt: SyncRun = client
        .get(format!("{origin}/v1/sync/{}", run.id))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt.id, run.id);
    let after_sync = store.snapshot().await.unwrap();
    assert_eq!(before_sync.observations, after_sync.observations);
    let stale = client
        .get(format!("{origin}/v1/tax/bills"))
        .query(&[("page_token", cursor)])
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let error: ApiError = stale.json().await.unwrap();
    assert_eq!(error.error_code, "revision_conflict");
    // Missing routes cannot be used to pay a bill or override its state.
    for route in ["/v1/tax/payments", "/v1/tax/bills/example/mark_paid"] {
        let response = client
            .post(format!("{origin}{route}"))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap();
        assert!(matches!(response.status().as_u16(), 404 | 405));
    }
    let source = store.snapshot().await.unwrap();
    server.close().await.unwrap();
    drop(context);
    syncs.close().await.unwrap();
    store.close().await.unwrap();
    let restored = tempfile::tempdir().unwrap();
    std::fs::copy(
        dir.path().join("ledger.sqlite3"),
        restored.path().join("ledger.sqlite3"),
    )
    .unwrap();
    std::fs::copy(
        dir.path().join("ledger.sync.sqlite3"),
        restored.path().join("ledger.sync.sqlite3"),
    )
    .unwrap();
    let ledger = Store::open(restored.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let receipts = SyncStore::open(restored.path().join("ledger.sync.sqlite3"))
        .await
        .unwrap();
    let snapshot: Snapshot = ledger.snapshot().await.unwrap();
    assert_eq!(snapshot, source);
    assert_eq!(
        receipts.get(run.id).await.unwrap().unwrap().status,
        SyncStatus::Unavailable
    );
    assert!(ledger.apply(request).await.unwrap().replayed);
    receipts.close().await.unwrap();
    ledger.close().await.unwrap();
}
#[tokio::test]
async fn ingress_requires_actual_peer_identity_and_nonambient_write_header() {
    let (_dir, store, syncs, context, server) = setup().await;
    server.close().await.unwrap();
    let wrong = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(
            context
                .as_ref()
                .clone()
                .for_ingress("192.0.2.1".parse().unwrap()),
        ),
    )
    .unwrap();
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{}/", wrong.local_addr()))
        .header("x-forwarded-for", "192.0.2.1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    wrong.close().await.unwrap();
    let ingress = api::start(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        Arc::new(
            context
                .as_ref()
                .clone()
                .for_ingress("127.0.0.1".parse().unwrap()),
        ),
    )
    .unwrap();
    let origin = format!("http://{}", ingress.local_addr());
    assert_eq!(
        client
            .get(format!("{origin}/v1/property"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let page = client
        .get(format!("{origin}/"))
        .header("x-ingress-path", "/api/hassio_ingress/example")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("<base href=\"/api/hassio_ingress/example/\""));
    let request = client
        .post(format!("{origin}/v1/imports/preview"))
        .header("x-remote-user-id", "fictional-user")
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(request.status(), 403);
    let request = client
        .post(format!("{origin}/v1/imports/preview"))
        .header("x-remote-user-id", "fictional-user")
        .header("x-property-request", "1")
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(request.status(), 200);
    ingress.close().await.unwrap();
    drop(context);
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn payment_pages_remain_readable_over_the_bounded_ha_transport() {
    use property::tax::ledger::{
        Allocation, BillIdentity, Observation, ObservationData, ParcelId, Payment, PaymentStatus,
    };
    let (_dir, store, syncs, context, server) = setup().await;
    let mut source = input();
    source.property.key.parcel = ParcelId::try_from(format!("EXAMPLE-{}", "P".repeat(92))).unwrap();
    let mut evidence = source.property.confirmation.clone();
    evidence.document = format!("Fictional {}", "d".repeat(490));
    evidence.url = Some(format!("https://example.invalid/{}", "e".repeat(1976)));
    let allocations: Vec<Allocation> = (0..100)
        .map(|n| Allocation {
            bill: BillIdentity {
                jurisdiction: source.property.key.jurisdiction.clone(),
                parcel: source.property.key.parcel.clone(),
                fiscal_year: 2030,
                sequence: format!("EXAMPLE-{n:03}-{}", "s".repeat(88)),
                source_version: format!("EXAMPLE-{}", "v".repeat(92)),
            },
            installment: 1,
            amount_cents: 100,
        })
        .collect();
    for batch in 0..3 {
        source.observations = (0..80)
            .map(|n| {
                let index = batch * 80 + n;
                Observation {
                    id: format!("example-payment-{index:03}-{}", "i".repeat(180)),
                    binding: source.property.key.clone(),
                    evidence: evidence.clone(),
                    observation: ObservationData::Payment(Payment {
                        reference: format!("EXAMPLE-REFERENCE-{index:03}-{}", "r".repeat(170)),
                        status: PaymentStatus::CountyPosted,
                        amount_cents: 10000,
                        allocations: allocations.clone(),
                    }),
                }
            })
            .collect();
        assert!(serde_json::to_vec(&source).unwrap().len() < 4 * 1024 * 1024);
        let preview = store.preview(source.clone()).await.unwrap();
        store
            .apply(ApplyRequest {
                if_match: preview.revision,
                preview,
                idempotency_key: format!("example-page-import-{batch}"),
            })
            .await
            .unwrap();
    }
    let client = reqwest::Client::new();
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let mut request = client
            .get(format!("http://{}/v1/tax/payments", server.local_addr()))
            .bearer_auth(OWNER)
            .query(&[("limit", 200)]);
        if let Some(token) = cursor {
            request = request.query(&[("page_token", token)]);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        assert!(
            bytes.len() <= 6 * 1024 * 1024 + 1024,
            "page cannot cross HA's 8MiB response bound: {}",
            bytes.len()
        );
        let page: Page<Observation> = serde_json::from_slice(&bytes).unwrap();
        for observation in page.items {
            assert!(seen.insert(observation.id));
        }
        match page.next_page {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 240);
    server.close().await.unwrap();
    drop(context);
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn existing_property_history_window_preserves_binding_and_saved_evidence() {
    use chrono::NaiveDate;
    use property::tax::ledger::{Assessment, ObservationData};
    #[derive(serde::Serialize)]
    struct HistoryUpdate {
        bought_on: Option<NaiveDate>,
    }
    #[derive(serde::Deserialize)]
    struct CurrentProperty {
        property: CurrentBinding,
    }
    #[derive(serde::Deserialize)]
    struct CurrentBinding {
        bought_on: Option<NaiveDate>,
    }

    let (_dir, store, syncs, _context, server) = setup().await;
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let mut sample = input();
    let payment = sample
        .observations
        .iter()
        .find(|value| matches!(value.observation, ObservationData::Payment(_)))
        .unwrap()
        .clone();
    sample.observations.clear();
    for year in [2019, 2020, 2021] {
        let mut row = payment.clone();
        row.id = format!("assessment-{year}");
        row.observation = ObservationData::Assessment(Assessment {
            fiscal_year: year,
            assessed_value_cents: 100_000,
            description: "Fictional annual roll".into(),
        });
        sample.observations.push(row);
    }
    for (id, at) in [
        ("payment-before", "2020-06-01T06:59:59Z"),
        ("payment-on", "2020-06-01T07:00:00Z"),
    ] {
        let mut row = payment.clone();
        row.id = id.into();
        if let ObservationData::Payment(value) = &mut row.observation {
            value.reference = id.into();
        }
        row.evidence.effective_at = at.parse().unwrap();
        sample.observations.push(row);
    }
    let preview = store.preview(sample.clone()).await.unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "confirm-example".into(),
        })
        .await
        .unwrap();
    let before = store.snapshot().await.unwrap();
    let date = NaiveDate::from_ymd_opt(2020, 6, 1).unwrap();
    let response = client
        .patch(format!("{origin}/v1/property"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .json(&HistoryUpdate {
            bought_on: Some(date),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["etag"], "\"2\"");
    let current: CurrentProperty = response.json().await.unwrap();
    assert_eq!(current.property.bought_on, Some(date));
    let after = store.snapshot().await.unwrap();
    assert_eq!(before.observations, after.observations);
    assert_eq!(
        before.property.as_ref().unwrap().key,
        after.property.as_ref().unwrap().key
    );
    assert_eq!(
        before.property.as_ref().unwrap().confirmation,
        after.property.as_ref().unwrap().confirmation
    );
    for (path, expected) in [
        ("assessments", "assessment-2021"),
        ("payments", "payment-on"),
    ] {
        let page: Page<property::tax::ledger::Observation> = client
            .get(format!("{origin}/v1/tax/{path}?limit=1"))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, expected);
        assert!(page.next_page.is_none());
    }
    let summary: Reconciliation = client
        .get(format!("{origin}/v1/summary"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.assessment_count, 1);
    assert_eq!(summary.payment_count, 1);
    let stale = client
        .patch(format!("{origin}/v1/property"))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .json(&HistoryUpdate { bought_on: None })
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);

    // An ordinary evidence import must retain the independently edited setting.
    let preview = store.preview(sample).await.unwrap();
    let revision = preview.revision;
    let result = store
        .apply(ApplyRequest {
            preview,
            if_match: revision,
            idempotency_key: "import-keeps-history".into(),
        })
        .await
        .unwrap();
    let current: CurrentProperty = client
        .get(format!("{origin}/v1/property"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current.property.bought_on, Some(date));
    let response = client
        .patch(format!("{origin}/v1/property"))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", result.revision))
        .json(&HistoryUpdate { bought_on: None })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let summary: Reconciliation = client
        .get(format!("{origin}/v1/summary"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.assessment_count, 3);
    assert_eq!(summary.payment_count, 2);
    assert_eq!(
        store.snapshot().await.unwrap().observations,
        before.observations
    );
    server.close().await.unwrap();
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn history_edits_require_admin_valid_dates_and_current_revision() {
    let (_dir, store, syncs, _context, server) = setup().await;
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let preview = store.preview(input()).await.unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "confirm-history-validation".into(),
        })
        .await
        .unwrap();
    let endpoint = format!("{origin}/v1/property");
    let response = client
        .patch(&endpoint)
        .bearer_auth(INTEGRATION)
        .header("If-Match", "\"1\"")
        .header("Content-Type", "application/json")
        .body(r#"{"bought_on":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = client
        .patch(&endpoint)
        .bearer_auth(OWNER)
        .header("Content-Type", "application/json")
        .body(r#"{"bought_on":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 428);
    for body in [
        r#"{"bought_on":"2999-01-01"}"#,
        r#"{"bought_on":"2020-02-30"}"#,
        r#"{"bought_on":""}"#,
        r#"{}"#,
        r#"{"bought_on":null,"label":"replace"}"#,
    ] {
        let response = client
            .patch(&endpoint)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{body}");
    }
    let update = |date: &'static str| {
        client
            .patch(&endpoint)
            .bearer_auth(OWNER)
            .header("If-Match", "\"1\"")
            .header("Content-Type", "application/json")
            .body(date)
            .send()
    };
    let (a, b) = tokio::join!(
        update(r#"{"bought_on":"2020-01-01"}"#),
        update(r#"{"bought_on":"2021-01-01"}"#)
    );
    let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    assert_eq!(store.snapshot().await.unwrap().revision, 2);
    for body in [
        r#"{"address":"42 Example Road","parcel":"0000000001"}"#,
        r#"{}"#,
        r#"{"parcel":"0000000001","confirm":true}"#,
    ] {
        let response = client
            .post(format!("{origin}/v1/property/resolve"))
            .bearer_auth(OWNER)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    server.close().await.unwrap();
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}
