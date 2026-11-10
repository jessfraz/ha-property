use property::{
    api::{self, Context, Page, PropertyView},
    store::{ApplyResult, Preview, Store},
    sync::SyncStore,
    tax::{
        ledger::{ImportInput, Observation},
        reconcile::BillView,
    },
};
use serde::Deserialize;
use std::{
    process::{Command, Output},
    sync::Arc,
};
const TOKEN: &str = "example-cli-owner-credential-at-least-32-characters";
#[derive(Deserialize)]
struct Envelope<T> {
    status: u16,
    etag: Option<String>,
    body: T,
}
#[derive(Deserialize)]
struct Diagnostic {
    status: Option<u16>,
    error_code: String,
}
async fn call(origin: String, args: Vec<String>, token: bool) -> Output {
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ha-property"));
        command.args(["--url", &origin]).args(args);
        for name in [
            "PROPERTY_API_TOKEN",
            "HOME_ASSISTANT_TOKEN",
            "PROPERTY_API_URL",
            "HOME_ASSISTANT_URL",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ] {
            command.env_remove(name);
        }
        if token {
            command.env("PROPERTY_API_TOKEN", TOKEN);
        }
        command.output().unwrap()
    })
    .await
    .unwrap()
}
fn success<T: for<'de> Deserialize<'de>>(output: Output) -> Envelope<T> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_edits_history_and_validates_resolution_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let syncs = SyncStore::open(dir.path().join("sync.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(Context::new(store.clone(), syncs.clone(), TOKEN)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let input: ImportInput =
        serde_json::from_str(include_str!("fixtures/import.example.json")).unwrap();
    let preview = store.preview(input).await.unwrap();
    store
        .apply(property::store::ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "cli-confirm-history".into(),
        })
        .await
        .unwrap();
    let before = store.snapshot().await.unwrap().observations;
    let changed: Envelope<PropertyView> = success(
        call(
            origin.clone(),
            vec![
                "history".into(),
                "from".into(),
                "2020-06-01".into(),
                "--if-match".into(),
                "\"1\"".into(),
            ],
            true,
        )
        .await,
    );
    assert_eq!(changed.status, 200);
    assert_eq!(changed.etag.as_deref(), Some("\"2\""));
    assert_eq!(
        changed.body.property.unwrap().bought_on,
        Some(chrono::NaiveDate::from_ymd_opt(2020, 6, 1).unwrap())
    );
    let cleared: Envelope<PropertyView> = success(
        call(
            origin.clone(),
            vec![
                "history".into(),
                "all".into(),
                "--if-match".into(),
                "\"2\"".into(),
            ],
            true,
        )
        .await,
    );
    assert!(cleared.body.property.unwrap().bought_on.is_none());
    assert_eq!(store.snapshot().await.unwrap().observations, before);
    let invalid = call(
        origin.clone(),
        vec!["resolve".into(), "--parcel".into(), "invalid".into()],
        true,
    )
    .await;
    assert!(!invalid.status.success());
    let error: Diagnostic = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(error.status, Some(400));
    assert_eq!(error.error_code, "invalid_input");
    let both = call(
        origin,
        vec![
            "resolve".into(),
            "--parcel".into(),
            "0000000001".into(),
            "--address".into(),
            "42 Example Road".into(),
        ],
        true,
    )
    .await;
    assert_eq!(both.status.code(), Some(2));
    server.close().await.unwrap();
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_preview_apply_replay_list_detail_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let syncs = SyncStore::open(dir.path().join("sync.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(Context::new(store.clone(), syncs.clone(), TOKEN)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let sample = dir.path().join("input.json");
    std::fs::write(&sample, include_str!("fixtures/import.example.json")).unwrap();
    let preview = call(
        origin.clone(),
        vec![
            "imports".into(),
            "preview".into(),
            "--input".into(),
            sample.display().to_string(),
        ],
        true,
    )
    .await;
    let saved = dir.path().join("preview.json");
    std::fs::write(&saved, &preview.stdout).unwrap();
    let result: Envelope<Preview> = success(preview);
    assert_eq!(result.status, 200);
    assert_eq!(result.etag.as_deref(), Some("\"0\""));
    let args = vec![
        "imports".into(),
        "apply".into(),
        "--preview".into(),
        saved.display().to_string(),
        "--if-match".into(),
        "\"0\"".into(),
        "--idempotency-key".into(),
        "example-cli-import".into(),
    ];
    let applied: Envelope<ApplyResult> = success(call(origin.clone(), args.clone(), true).await);
    assert_eq!(applied.body.revision, 1);
    let replay: Envelope<ApplyResult> = success(call(origin.clone(), args, true).await);
    assert!(replay.body.replayed);
    let show: Envelope<PropertyView> =
        success(call(origin.clone(), vec!["show".into()], true).await);
    assert!(show.body.property.is_some());
    let bills: Envelope<Page<BillView>> = success(
        call(
            origin.clone(),
            vec![
                "tax".into(),
                "bills".into(),
                "list".into(),
                "--limit".into(),
                "1".into(),
            ],
            true,
        )
        .await,
    );
    let id = bills.body.items[0].id.clone();
    assert!(bills.body.next_page.is_some());
    let detail: Envelope<BillView> = success(
        call(
            origin.clone(),
            vec!["tax".into(), "bills".into(), "get".into(), id.clone()],
            true,
        )
        .await,
    );
    assert_eq!(detail.body.id, id);
    let payments: Envelope<Page<Observation>> = success(
        call(
            origin.clone(),
            vec!["tax".into(), "payments".into(), "list".into()],
            true,
        )
        .await,
    );
    assert!(!payments.body.items.is_empty());
    let denied = call(origin.clone(), vec!["summary".into()], false).await;
    assert!(!denied.status.success());
    let error: Diagnostic = serde_json::from_slice(&denied.stderr).unwrap();
    assert_eq!(error.status, None);
    assert_eq!(error.error_code, "token_required");
    let malformed = dir.path().join("malformed.json");
    std::fs::write(&malformed, "private-source-text-is-not-json").unwrap();
    let error = call(
        origin,
        vec![
            "imports".into(),
            "preview".into(),
            "--input".into(),
            malformed.display().to_string(),
        ],
        true,
    )
    .await;
    assert!(!error.status.success());
    assert!(!String::from_utf8_lossy(&error.stderr).contains("private-source-text"));
    let _: ImportInput =
        serde_json::from_str(include_str!("fixtures/import.example.json")).unwrap();
    server.close().await.unwrap();
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_accepted_boundary_preview_round_trips_and_excess_capacity_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let syncs = SyncStore::open(dir.path().join("sync.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(Context::new(store.clone(), syncs.clone(), TOKEN)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let mut source: ImportInput =
        serde_json::from_str(include_str!("fixtures/import.example.json")).unwrap();
    let template = source.observations[0].clone();
    source.observations.clear();
    for index in 0..1000 {
        let mut observation = template.clone();
        observation.id = format!("fictional-notice-{index:04}");
        observation.evidence.document = format!("Fictional document {}", "x".repeat(450));
        if let property::tax::ledger::ObservationData::Notice(notice) = &mut observation.observation
        {
            notice.identity.sequence = format!("FICTIONAL-{index:04}");
            let installment = notice.installments[0].clone();
            notice.installments = (1..=12)
                .map(|number| property::tax::ledger::Installment {
                    number,
                    ..installment.clone()
                })
                .collect();
        }
        source.observations.push(observation);
    }
    let file = dir.path().join("boundary.json");
    let bytes = serde_json::to_vec(&source).unwrap();
    assert!(bytes.len() < 4 * 1024 * 1024);
    std::fs::write(&file, bytes).unwrap();
    // This formerly accepted input expanded to a 22MB preview and broke both
    // apply and HA refresh. Capacity is now rejected before a preview is issued.
    let rejected = call(
        origin.clone(),
        vec![
            "imports".into(),
            "preview".into(),
            "--input".into(),
            file.display().to_string(),
        ],
        true,
    )
    .await;
    assert!(!rejected.status.success());
    let error: Diagnostic = serde_json::from_slice(&rejected.stderr).unwrap();
    assert_eq!(error.error_code, "capacity");
    assert!(store.snapshot().await.unwrap().observations.is_empty());
    source.observations.truncate(100);
    std::fs::write(&file, serde_json::to_vec(&source).unwrap()).unwrap();
    let output = call(
        origin.clone(),
        vec![
            "imports".into(),
            "preview".into(),
            "--input".into(),
            file.display().to_string(),
        ],
        true,
    )
    .await;
    assert!(output.stdout.len() < 8 * 1024 * 1024);
    let saved = dir.path().join("boundary-preview.json");
    std::fs::write(&saved, &output.stdout).unwrap();
    let preview: Envelope<Preview> = success(output);
    assert_eq!(preview.body.added, 100);
    let applied: Envelope<ApplyResult> = success(
        call(
            origin,
            vec![
                "imports".into(),
                "apply".into(),
                "--preview".into(),
                saved.display().to_string(),
                "--if-match".into(),
                "\"0\"".into(),
                "--idempotency-key".into(),
                "boundary-import".into(),
            ],
            true,
        )
        .await,
    );
    assert_eq!(applied.body.added, 100);
    server.close().await.unwrap();
    syncs.close().await.unwrap();
    store.close().await.unwrap();
}
