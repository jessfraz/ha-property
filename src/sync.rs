//! Durable collection receipts. Failed reads never erase authoritative evidence.
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tokio::{sync::Mutex, task::JoinHandle};
use uuid::Uuid;

use crate::{
    county::{County, CountySyncPage, CountySyncRequest, los_angeles::LosAngeles},
    store::{ApplyRequest, Store},
    tax::ledger::{
        Coverage, CoverageCapability, CoverageStatus, ImportInput, Observation, PropertyBinding,
        TaxError,
    },
};

const GET: &str = "SELECT id,status,started_at,completed_at,error_code,coverage_json,page_json FROM sync_runs WHERE id=?";
const INSERT: &str =
    "INSERT INTO sync_runs(id,status,started_at,coverage_json) VALUES (?,'running',?,'[]')";
const FINISH: &str = "UPDATE sync_runs SET status=?,completed_at=?,error_code=?,coverage_json=?,page_json=? WHERE id=?";
const INTERRUPT: &str = "UPDATE sync_runs SET status='interrupted',completed_at=?,error_code='interrupted' WHERE status='running'";
const PRUNE: &str = "DELETE FROM sync_runs WHERE id IN (SELECT id FROM sync_runs ORDER BY id DESC LIMIT -1 OFFSET 1000)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Running,
    Succeeded,
    Partial,
    Unavailable,
    Unsupported,
    Interrupted,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SyncRun {
    pub id: Uuid,
    pub status: SyncStatus,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub coverage: Vec<Coverage>,
    pub page: Option<CountySyncPage>,
}
type SyncJob = JoinHandle<Result<(), TaxError>>;

#[derive(Clone)]
pub struct SyncStore {
    pool: SqlitePool,
    jobs: Arc<Mutex<Vec<SyncJob>>>,
    active: Arc<Mutex<Option<Uuid>>>,
    stopping: Arc<AtomicBool>,
}
impl SyncStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaxError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./sync_migrations").run(&pool).await?;
        // An accepted request whose task did not reach durable completion is not
        // silently called successful after a process restart.
        sqlx::query(INTERRUPT)
            .bind(Utc::now().to_rfc3339())
            .execute(&pool)
            .await?;
        Ok(Self {
            pool,
            jobs: Arc::new(Mutex::new(Vec::new())),
            active: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }
    pub async fn get(&self, id: Uuid) -> Result<Option<SyncRun>, TaxError> {
        let Some(run) = self.read_run(id).await? else {
            return Ok(None);
        };
        if run.status != SyncStatus::Running {
            return Ok(Some(run));
        }
        let active = self.active.lock().await;
        if *active == Some(id) {
            return Ok(Some(run));
        }
        // A terminal audit write can fail after the atomic ledger import has
        // committed. Without an owner this is interrupted, not still running
        // and not proof that nothing changed. Recheck under the ownership gate
        // before repairing the audit once storage is writable again.
        let Some(mut run) = self.read_run(id).await? else {
            return Ok(None);
        };
        if run.status == SyncStatus::Running {
            run.status = SyncStatus::Interrupted;
            run.completed_at = Some(Utc::now());
            run.error_code = Some("audit_interrupted".into());
            run.coverage = coverage(
                CoverageStatus::Unavailable,
                "Sync completion could not be confirmed. Imported evidence may already be saved; review the ledger before retrying.",
                Utc::now(),
            );
            self.save_run(&run).await?;
        }
        Ok(Some(run))
    }
    async fn read_run(&self, id: Uuid) -> Result<Option<SyncRun>, TaxError> {
        sqlx::query(GET)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .map(decode)
            .transpose()
    }
    /// Accept one owned job. Repeated requests while it runs return the same
    /// durable identity rather than queueing duplicate county lookups.
    pub async fn request(
        &self,
        store: &Store,
        property: PropertyBinding,
    ) -> Result<SyncRun, TaxError> {
        self.request_from(store, property, LosAngeles::new()).await
    }

    async fn request_from<C: County + Send + Sync + 'static>(
        &self,
        store: &Store,
        property: PropertyBinding,
        county: C,
    ) -> Result<SyncRun, TaxError> {
        let mut jobs = self.jobs.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return Err(TaxError::Invalid("county sync is shutting down"));
        }
        let mut index = 0;
        while index < jobs.len() {
            if jobs[index].is_finished() {
                jobs.swap_remove(index)
                    .await
                    .map_err(|_| worker_error())??;
            } else {
                index += 1;
            }
        }
        let mut active = self.active.lock().await;
        if let Some(id) = *active {
            let existing = self
                .read_run(id)
                .await?
                .ok_or(TaxError::Invalid("active sync receipt is unavailable"))?;
            if existing.status == SyncStatus::Running {
                return Ok(existing);
            }
            *active = None;
        }
        let run = SyncRun {
            id: Uuid::now_v7(),
            status: SyncStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            error_code: None,
            coverage: Vec::new(),
            page: None,
        };
        sqlx::query(INSERT)
            .bind(run.id.to_string())
            .bind(run.started_at.to_rfc3339())
            .execute(&self.pool)
            .await?;
        *active = Some(run.id);
        let owned = self.clone();
        let ledger = store.clone();
        let pending = run.clone();
        jobs.push(tokio::spawn(async move {
            let result = owned
                .perform(&ledger, property, &county, pending.clone())
                .await;
            let mut active = owned.active.lock().await;
            if *active == Some(pending.id) {
                *active = None;
            }
            result
        }));
        Ok(run)
    }

    async fn perform<C: County + Sync>(
        &self,
        store: &Store,
        property: PropertyBinding,
        county: &C,
        mut run: SyncRun,
    ) -> Result<(), TaxError> {
        if property.key.jurisdiction != county.jurisdiction() {
            run.status = SyncStatus::Unsupported;
            run.error_code = Some("county_unsupported".into());
            run.coverage = coverage(
                CoverageStatus::Unsupported,
                "No adapter is available for this confirmed county.",
                run.started_at,
            );
            if self.save_coverage(store, &run.coverage).await.is_err() {
                run.error_code = Some("coverage_not_saved".into());
                run.status = SyncStatus::Unavailable;
            }
        } else {
            let collection = tokio::time::timeout(Duration::from_secs(90), async {
                let mut cursor = None;
                for page_number in 0..16 {
                    let page = county.sync(CountySyncRequest {property: property.clone(), cursor: cursor.clone(), requested_at: Utc::now()}).await
                        .map_err(|_| TaxError::Invalid("county collection did not return valid evidence"))?;
                    page.validate_binding(&property.key).map_err(|_| TaxError::BindingMismatch)?;
                    let mut page = page;
                    page.observations.retain(|value|property.includes_observation(value));
                    if page.coverage.is_empty() {
                        return Err(TaxError::Invalid("county collection omitted source coverage"));
                    }
                    if page.next_cursor.is_some() {
                        for source in &mut page.coverage {
                            if source.status == CoverageStatus::Complete {
                                source.status = CoverageStatus::Partial;
                                source.detail = "County collection has additional pages; coverage is not complete.".into();
                            }
                        }
                    }
                    apply_collected(store, &property, &page, format!("county-sync-{}-{page_number}", run.id)).await?;
                    run.coverage = page.coverage.clone();
                    cursor = page.next_cursor.clone();
                    run.page = Some(page);
                    if cursor.is_none() { return Ok::<(), TaxError>(()); }
                }
                Err(TaxError::Capacity)
            }).await;
            match collection {
                Ok(Ok(())) => {
                    run.status = if run
                        .coverage
                        .iter()
                        .all(|item| item.status == CoverageStatus::Complete)
                    {
                        SyncStatus::Succeeded
                    } else if run.coverage.iter().all(|item| {
                        matches!(
                            item.status,
                            CoverageStatus::Unavailable | CoverageStatus::Unsupported
                        )
                    }) {
                        SyncStatus::Unavailable
                    } else {
                        SyncStatus::Partial
                    };
                }
                result => {
                    run.error_code = Some(
                        match result {
                            Err(_) => "county_timeout",
                            Ok(Err(TaxError::BindingMismatch)) => "binding_mismatch",
                            Ok(Err(TaxError::RevisionConflict)) => "revision_conflict",
                            Ok(Err(TaxError::ObservationConflict)) => "observation_conflict",
                            _ => "county_unavailable",
                        }
                        .into(),
                    );
                    run.status = if run.page.is_some() {
                        SyncStatus::Partial
                    } else {
                        SyncStatus::Unavailable
                    };
                    run.coverage = coverage(
                        CoverageStatus::Unavailable,
                        "County collection did not complete. Previously saved evidence is unchanged; a missing result is not a zero balance.",
                        Utc::now(),
                    );
                    if self.save_coverage(store, &run.coverage).await.is_err() {
                        run.error_code = Some("coverage_not_saved".into());
                        run.status = if run.page.is_some() {
                            SyncStatus::Partial
                        } else {
                            SyncStatus::Unavailable
                        };
                    }
                }
            }
        }
        run.completed_at = Some(Utc::now());
        // Ledger evidence and its import receipt commit together. The audit's
        // terminal state follows; restart marks any interrupted audit honestly.
        self.save_run(&run).await?;
        sqlx::query(PRUNE).execute(&self.pool).await?;
        Ok(())
    }

    async fn save_run(&self, run: &SyncRun) -> Result<(), TaxError> {
        sqlx::query(FINISH)
            .bind(status_name(&run.status))
            .bind(run.completed_at.map(|value| value.to_rfc3339()))
            .bind(&run.error_code)
            .bind(serde_json::to_string(&run.coverage)?)
            .bind(run.page.as_ref().map(serde_json::to_string).transpose()?)
            .bind(run.id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn save_coverage(&self, store: &Store, coverage: &[Coverage]) -> Result<(), TaxError> {
        for attempt in 0..2 {
            let current = store.snapshot().await?;
            match store
                .record_coverage(coverage.to_vec(), current.revision)
                .await
            {
                Err(TaxError::RevisionConflict) if attempt == 0 => continue,
                result => return result.map(|_| ()),
            }
        }
        Err(TaxError::RevisionConflict)
    }

    pub async fn close(&self) -> Result<(), TaxError> {
        self.stopping.store(true, Ordering::Release);
        let jobs: Vec<_> = self.jobs.lock().await.drain(..).collect();
        let mut failure = None;
        for job in jobs {
            if let Err(error) = job
                .await
                .map_err(|_| worker_error())
                .and_then(|result| result)
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        let checkpoint = async {
            let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&self.pool)
                .await?;
            if row.try_get::<i64, _>(0)? != 0 {
                return Err(worker_error());
            }
            Ok::<(), TaxError>(())
        }
        .await;
        // Worker failure must not skip checkpoint/connection cleanup.
        self.pool.close().await;
        match failure {
            Some(error) => Err(error),
            None => checkpoint,
        }
    }
}

fn worker_error() -> TaxError {
    TaxError::Database(sqlx::Error::Protocol(
        "county sync worker did not finish".into(),
    ))
}

async fn apply_collected(
    store: &Store,
    property: &PropertyBinding,
    page: &CountySyncPage,
    key: String,
) -> Result<(), TaxError> {
    for attempt in 0..2 {
        let snapshot = store.snapshot().await?;
        if snapshot.property.as_ref().is_some_and(|current| {
            current.key != property.key || current.bought_on != property.bought_on
        }) {
            return Err(TaxError::RevisionConflict);
        }
        let existing: BTreeMap<_, _> = snapshot
            .observations
            .iter()
            .map(|value| (value.id.as_str(), value))
            .collect();
        let observations = page
            .observations
            .iter()
            .map(|incoming| {
                if let Some(existing) = existing.get(incoming.id.as_str()) {
                    let mut comparison = incoming.clone();
                    comparison.evidence.observed_at = existing.evidence.observed_at;
                    if comparison == **existing {
                        return (*existing).clone();
                    }
                }
                incoming.clone()
            })
            .collect::<Vec<Observation>>();
        let input = ImportInput {
            property: property.clone(),
            observations,
            coverage: page.coverage.clone(),
        };
        let preview = match store.preview(input).await {
            Err(TaxError::ObservationConflict | TaxError::RevisionConflict) if attempt == 0 => {
                continue;
            }
            result => result?,
        };
        if preview.revision != snapshot.revision {
            continue;
        }
        let request = ApplyRequest {
            if_match: preview.revision,
            preview,
            idempotency_key: key.clone(),
        };
        match store.apply(request).await {
            Err(TaxError::RevisionConflict) if attempt == 0 => continue,
            result => return result.map(|_| ()),
        }
    }
    Err(TaxError::RevisionConflict)
}

fn status_name(status: &SyncStatus) -> &'static str {
    match status {
        SyncStatus::Running => "running",
        SyncStatus::Succeeded => "succeeded",
        SyncStatus::Partial => "partial",
        SyncStatus::Unavailable => "unavailable",
        SyncStatus::Unsupported => "unsupported",
        SyncStatus::Interrupted => "interrupted",
    }
}
fn coverage(status: CoverageStatus, detail: &str, checked_at: DateTime<Utc>) -> Vec<Coverage> {
    [
        CoverageCapability::Notices,
        CoverageCapability::Assessments,
        CoverageCapability::Balances,
        CoverageCapability::Payments,
    ]
    .into_iter()
    .map(|capability| Coverage {
        capability,
        status: status.clone(),
        checked_at,
        detail: detail.into(),
    })
    .collect()
}
fn decode(row: sqlx::sqlite::SqliteRow) -> Result<SyncRun, TaxError> {
    let id: String = row.try_get("id")?;
    let status: String = row.try_get("status")?;
    let started: String = row.try_get("started_at")?;
    let completed: Option<String> = row.try_get("completed_at")?;
    let coverage_json: String = row.try_get("coverage_json")?;
    let page_json: Option<String> = row.try_get("page_json")?;
    Ok(SyncRun {
        id: Uuid::parse_str(&id)
            .map_err(|_| TaxError::Invalid("stored sync identity is invalid"))?,
        status: match status.as_str() {
            "running" => SyncStatus::Running,
            "succeeded" => SyncStatus::Succeeded,
            "partial" => SyncStatus::Partial,
            "unavailable" => SyncStatus::Unavailable,
            "unsupported" => SyncStatus::Unsupported,
            "interrupted" => SyncStatus::Interrupted,
            _ => return Err(TaxError::Invalid("stored sync state is invalid")),
        },
        started_at: DateTime::parse_from_rfc3339(&started)
            .map_err(|_| TaxError::Invalid("stored sync time is invalid"))?
            .with_timezone(&Utc),
        completed_at: completed
            .map(|v| DateTime::parse_from_rfc3339(&v).map(|v| v.with_timezone(&Utc)))
            .transpose()
            .map_err(|_| TaxError::Invalid("stored sync time is invalid"))?,
        error_code: row.try_get("error_code")?,
        coverage: serde_json::from_str(&coverage_json)?,
        page: page_json.map(|v| serde_json::from_str(&v)).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use crate::sync::{FINISH, GET, INSERT, INTERRUPT, PRUNE, SyncStatus, SyncStore};
    use sqlx::Row;
    #[tokio::test]
    async fn interrupted_receipt_survives_restart_and_actual_queries_have_bounded_plans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.sqlite3");
        let store = SyncStore::open(&path).await.unwrap();
        let id = uuid::Uuid::now_v7();
        sqlx::query(INSERT)
            .bind(id.to_string())
            .bind("2030-01-01T00:00:00Z")
            .execute(&store.pool)
            .await
            .unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        for n in 0..2000 {
            sqlx::query(INSERT)
                .bind(format!("00000000-0000-7000-8000-{n:012}"))
                .bind("2030-01-01T00:00:00Z")
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let mut plans = Vec::new();
        for (name, sql, args) in [
            ("get", GET, vec![id.to_string()]),
            (
                "insert",
                INSERT,
                vec![
                    uuid::Uuid::now_v7().to_string(),
                    "2030-01-01T00:00:00Z".into(),
                ],
            ),
            (
                "finish",
                FINISH,
                vec![
                    "unavailable".into(),
                    "2030-01-01T00:00:00Z".into(),
                    "example".into(),
                    "[]".into(),
                    "null".into(),
                    id.to_string(),
                ],
            ),
            ("interrupt", INTERRUPT, vec!["2030-01-01T00:00:00Z".into()]),
            ("prune", PRUNE, vec![]),
        ] {
            // Only the literal statements enumerated above enter EXPLAIN.
            let explain = format!("EXPLAIN QUERY PLAN {sql}");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(explain.as_str()));
            for arg in args {
                query = query.bind(arg);
            }
            let rows = query.fetch_all(&store.pool).await.unwrap();
            let details: Vec<String> = rows.iter().map(|v| v.get("detail")).collect();
            if name == "get" || name == "finish" {
                assert!(details.iter().any(|v| v.contains("INDEX")));
            }
            plans.push(format!("{name}: {details:?}"));
        }
        eprintln!("{}", plans.join("\n"));
        store.close().await.unwrap();
        let reopened = SyncStore::open(&path).await.unwrap();
        assert_eq!(
            reopened.get(id).await.unwrap().unwrap().status,
            SyncStatus::Interrupted
        );
        sqlx::query(PRUNE).execute(&reopened.pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_runs")
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
        assert_eq!(count, 1000);
        reopened.close().await.unwrap();
    }
    struct FixtureCounty {
        page: crate::county::CountySyncPage,
        started: Option<std::sync::Arc<tokio::sync::Notify>>,
        release: Option<std::sync::Arc<tokio::sync::Notify>>,
    }

    #[tokio::test]
    async fn changing_history_fences_an_inflight_collection_and_filters_the_next_job() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, syncs, property, mut page) = fixture(dir.path()).await;
        if let crate::tax::ledger::ObservationData::Assessment(value) =
            &mut page.observations[0].observation
        {
            value.fiscal_year = 2019;
        }
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let run = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: page.clone(),
                    started: Some(started.clone()),
                    release: Some(release.clone()),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let state = ledger
            .update_bought_on(
                Some(chrono::NaiveDate::from_ymd_opt(2020, 6, 1).unwrap()),
                1,
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        release.notify_one();
        let stale = terminal(&syncs, run.id).await;
        assert_eq!(stale.error_code.as_deref(), Some("revision_conflict"));
        assert!(ledger.snapshot().await.unwrap().observations.is_empty());
        let current = state.property.unwrap();
        let run = syncs
            .request_from(
                &ledger,
                current,
                FixtureCounty {
                    page,
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        let refreshed = terminal(&syncs, run.id).await;
        assert!(refreshed.page.unwrap().observations.is_empty());
        let saved = ledger.snapshot().await.unwrap();
        assert!(saved.observations.is_empty());
        let saved = saved.property.unwrap();
        assert_eq!(saved.key, property.key);
        assert_eq!(saved.confirmation, property.confirmation);
        assert!(saved.bought_on.is_some());
        syncs.close().await.unwrap();
        ledger.close().await.unwrap();
    }
    impl crate::county::County for FixtureCounty {
        fn jurisdiction(&self) -> crate::tax::ledger::CountyFips {
            crate::tax::ledger::CountyFips::los_angeles()
        }
        fn capabilities(&self) -> Vec<crate::county::CountyCapability> {
            Vec::new()
        }
        async fn sync(
            &self,
            _request: crate::county::CountySyncRequest,
        ) -> Result<crate::county::CountySyncPage, crate::county::CountyError> {
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(self.page.clone())
        }
    }
    async fn fixture(
        dir: &std::path::Path,
    ) -> (
        crate::store::Store,
        SyncStore,
        crate::tax::ledger::PropertyBinding,
        crate::county::CountySyncPage,
    ) {
        use crate::tax::ledger::{
            Assessment, CoverageCapability, CoverageStatus, ImportInput, Observation,
            ObservationData,
        };
        let ledger = crate::store::Store::open(dir.join("ledger.sqlite3"))
            .await
            .unwrap();
        let syncs = SyncStore::open(dir.join("sync.sqlite3")).await.unwrap();
        let mut input: ImportInput =
            serde_json::from_str(include_str!("../tests/fixtures/import.example.json")).unwrap();
        input.observations.clear();
        input.coverage.clear();
        let property = input.property.clone();
        let preview = ledger.preview(input).await.unwrap();
        ledger
            .apply(crate::store::ApplyRequest {
                if_match: preview.revision,
                preview,
                idempotency_key: "fictional-binding".into(),
            })
            .await
            .unwrap();
        let mut sources = crate::sync::coverage(
            CoverageStatus::Unavailable,
            "Fictional unavailable tax source",
            chrono::Utc::now(),
        );
        for source in &mut sources {
            if source.capability == CoverageCapability::Assessments {
                source.status = CoverageStatus::Partial;
            }
        }
        let page = crate::county::CountySyncPage {
            binding: property.key.clone(),
            next_cursor: None,
            coverage: sources,
            observations: vec![Observation {
                id: "fictional-county-assessment".into(),
                binding: property.key.clone(),
                evidence: property.confirmation.clone(),
                observation: ObservationData::Assessment(Assessment {
                    fiscal_year: 2030,
                    assessed_value_cents: 100_000,
                    description: "Fictional county roll".into(),
                }),
            }],
        };
        (ledger, syncs, property, page)
    }
    async fn terminal(syncs: &SyncStore, id: uuid::Uuid) -> crate::sync::SyncRun {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let run = syncs.get(id).await.unwrap().unwrap();
                if run.status != SyncStatus::Running {
                    return run;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn accepted_job_is_owned_coalesced_and_partial_reads_are_committed() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, syncs, property, page) = fixture(dir.path()).await;
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let run = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: page.clone(),
                    started: Some(started.clone()),
                    release: Some(release.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(run.status, SyncStatus::Running);
        started.notified().await;
        let repeated = syncs
            .request_from(
                &ledger,
                property,
                FixtureCounty {
                    page: page.clone(),
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(repeated.id, run.id);
        assert!(ledger.snapshot().await.unwrap().observations.is_empty());
        release.notify_one();
        assert_eq!(terminal(&syncs, run.id).await.status, SyncStatus::Partial);
        let snapshot = ledger.snapshot().await.unwrap();
        assert_eq!(snapshot.observations, page.observations);
        assert_eq!(snapshot.coverage.len(), 4);
        syncs.close().await.unwrap();
        ledger.close().await.unwrap();
    }
    #[tokio::test]
    async fn unchanged_source_refresh_deduplicates_but_changed_immutable_content_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, syncs, property, page) = fixture(dir.path()).await;
        let first = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: page.clone(),
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(terminal(&syncs, first.id).await.status, SyncStatus::Partial);
        let original = ledger.snapshot().await.unwrap().observations;
        let mut refreshed = page.clone();
        refreshed.observations[0].evidence.observed_at += chrono::Duration::minutes(1);
        refreshed.coverage[0].checked_at += chrono::Duration::minutes(1);
        let repeated = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: refreshed.clone(),
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        assert_ne!(first.id, repeated.id);
        assert_eq!(
            terminal(&syncs, repeated.id).await.status,
            SyncStatus::Partial
        );
        assert_eq!(ledger.snapshot().await.unwrap().observations, original);
        if let crate::tax::ledger::ObservationData::Assessment(value) =
            &mut refreshed.observations[0].observation
        {
            value.assessed_value_cents += 1;
        }
        let conflicting = syncs
            .request_from(
                &ledger,
                property,
                FixtureCounty {
                    page: refreshed,
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        let run = terminal(&syncs, conflicting.id).await;
        assert_eq!(run.status, SyncStatus::Unavailable);
        assert_eq!(run.error_code.as_deref(), Some("observation_conflict"));
        assert_eq!(ledger.snapshot().await.unwrap().observations, original);
        syncs.close().await.unwrap();
        ledger.close().await.unwrap();
    }
    #[tokio::test]
    async fn foreign_evidence_never_crosses_the_binding_and_shutdown_waits_for_accepted_work() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, syncs, property, mut page) = fixture(dir.path()).await;
        page.observations[0].binding.parcel =
            crate::tax::ledger::ParcelId::try_from("FICTIONAL-OTHER".to_owned()).unwrap();
        let bad = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: page.clone(),
                    started: None,
                    release: None,
                },
            )
            .await
            .unwrap();
        let bad = terminal(&syncs, bad.id).await;
        assert_eq!(bad.error_code.as_deref(), Some("binding_mismatch"));
        assert!(ledger.snapshot().await.unwrap().observations.is_empty());
        page.observations[0].binding = property.key.clone();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let accepted = syncs
            .request_from(
                &ledger,
                property.clone(),
                FixtureCounty {
                    page: page.clone(),
                    started: Some(started.clone()),
                    release: Some(release.clone()),
                },
            )
            .await
            .unwrap();
        started.notified().await;
        let closing_store = syncs.clone();
        let closing = tokio::spawn(async move { closing_store.close().await });
        while !syncs.stopping.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished());
        assert!(
            syncs
                .request_from(
                    &ledger,
                    property,
                    FixtureCounty {
                        page: page.clone(),
                        started: None,
                        release: None
                    }
                )
                .await
                .is_err()
        );
        release.notify_one();
        closing.await.unwrap().unwrap();
        assert_eq!(
            ledger.snapshot().await.unwrap().observations,
            page.observations
        );
        let reopened = SyncStore::open(dir.path().join("sync.sqlite3"))
            .await
            .unwrap();
        assert_eq!(
            reopened.get(accepted.id).await.unwrap().unwrap().status,
            SyncStatus::Partial
        );
        reopened.close().await.unwrap();
        ledger.close().await.unwrap();
    }
    #[tokio::test]
    async fn review_temporary_audit_write_failure_does_not_leave_unowned_running_receipt() {
        use sqlx::{Connection, Executor};
        let dir = tempfile::tempdir().unwrap();
        let (ledger, syncs, property, page) = fixture(dir.path()).await;
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let run = syncs
            .request_from(
                &ledger,
                property,
                FixtureCounty {
                    page: page.clone(),
                    started: Some(started.clone()),
                    release: Some(release.clone()),
                },
            )
            .await
            .unwrap();
        started.notified().await;
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("sync.sqlite3"));
        let mut blocker = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        blocker.execute("BEGIN IMMEDIATE").await.unwrap();
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if syncs.jobs.lock().await.iter().all(|job| job.is_finished()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        blocker.execute("ROLLBACK").await.unwrap();
        blocker.close().await.unwrap();
        let receipt = syncs.get(run.id).await.unwrap().unwrap();
        let active = syncs.active.lock().await.is_some();
        let count = ledger.snapshot().await.unwrap().observations.len();
        println!(
            "receipt_status={:?} active_worker={} imported_observations={}",
            receipt.status, active, count
        );
        assert_ne!(
            receipt.status,
            SyncStatus::Running,
            "a completed failed job must not remain running after audit storage becomes writable"
        );
        assert_eq!(count, 1);
        assert!(syncs.close().await.is_err());
        assert!(syncs.pool.is_closed());
        let reopened = SyncStore::open(dir.path().join("sync.sqlite3"))
            .await
            .unwrap();
        assert_eq!(
            reopened.get(run.id).await.unwrap().unwrap().status,
            SyncStatus::Interrupted
        );
        reopened.close().await.unwrap();
        ledger.close().await.unwrap();
    }
}
