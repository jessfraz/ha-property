//! Durable collection receipts. Failed reads never erase authoritative evidence.
use std::{path::Path, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    county::{County, CountySyncPage, CountySyncRequest, los_angeles::LosAngeles},
    store::Store,
    tax::ledger::{Coverage, CoverageCapability, CoverageStatus, PropertyBinding, TaxError},
};

const GET: &str = "SELECT id,status,started_at,completed_at,error_code,coverage_json,page_json FROM sync_runs WHERE id=?";
const LATEST: &str = "SELECT id,status,started_at,completed_at,error_code,coverage_json,page_json FROM sync_runs ORDER BY id DESC LIMIT 1";
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
#[derive(Clone)]
pub struct SyncStore {
    pool: SqlitePool,
    worker: Arc<Mutex<()>>,
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
            worker: Arc::new(Mutex::new(())),
        })
    }
    pub async fn get(&self, id: Uuid) -> Result<Option<SyncRun>, TaxError> {
        sqlx::query(GET)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .map(decode)
            .transpose()
    }
    pub async fn latest(&self) -> Result<Option<SyncRun>, TaxError> {
        sqlx::query(LATEST)
            .fetch_optional(&self.pool)
            .await?
            .map(decode)
            .transpose()
    }
    pub async fn request(
        &self,
        store: &Store,
        property: PropertyBinding,
    ) -> Result<SyncRun, TaxError> {
        let _worker = self.worker.lock().await;
        let mut run = SyncRun {
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
        let county = LosAngeles::new();
        if property.key.jurisdiction != county.jurisdiction() {
            run.status = SyncStatus::Unsupported;
            run.error_code = Some("county_unsupported".into());
            run.coverage = coverage(
                CoverageStatus::Unsupported,
                "No adapter is available for this confirmed county.",
                run.started_at,
            );
        } else {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                county.sync(CountySyncRequest {
                    property: property.clone(),
                    cursor: None,
                    requested_at: run.started_at,
                }),
            )
            .await;
            match result {
                Ok(Ok(page)) => {
                    if page.validate_binding(&property.key).is_err() {
                        run.status = SyncStatus::Unavailable;
                        run.error_code = Some("binding_mismatch".into());
                        run.coverage = coverage(
                            CoverageStatus::Unavailable,
                            "The county result did not match the confirmed property.",
                            run.started_at,
                        );
                    } else {
                        // The current adapter is manual-import only. Future adapters
                        // must atomically validate/import evidence before success.
                        if !page.observations.is_empty() || page.next_cursor.is_some() {
                            run.status = SyncStatus::Unavailable;
                            run.error_code = Some("collection_incomplete".into());
                            run.coverage = coverage(
                                CoverageStatus::Partial,
                                "Collection did not complete. Existing evidence is unchanged.",
                                run.started_at,
                            );
                        } else {
                            run.coverage = page.coverage.clone();
                            run.status = if run
                                .coverage
                                .iter()
                                .all(|v| v.status == CoverageStatus::Complete)
                            {
                                SyncStatus::Succeeded
                            } else if run
                                .coverage
                                .iter()
                                .all(|v| v.status == CoverageStatus::Unavailable)
                            {
                                SyncStatus::Unavailable
                            } else {
                                SyncStatus::Partial
                            };
                            run.page = Some(page);
                        }
                    }
                }
                _ => {
                    run.status = SyncStatus::Unavailable;
                    run.error_code = Some("county_unavailable".into());
                    run.coverage = coverage(
                        CoverageStatus::Unavailable,
                        "County collection failed. Existing evidence is unchanged.",
                        run.started_at,
                    );
                }
            }
        }
        let current = store.snapshot().await?;
        let revision = store
            .record_coverage(run.coverage.clone(), current.revision)
            .await;
        let applied = match revision {
            Err(TaxError::RevisionConflict) => {
                let latest = store.snapshot().await?;
                store
                    .record_coverage(run.coverage.clone(), latest.revision)
                    .await
            }
            result => result,
        };
        if applied.is_err() {
            run.error_code = Some("coverage_not_saved".into());
            run.status = SyncStatus::Unavailable;
        }
        run.completed_at = Some(Utc::now());
        sqlx::query(FINISH)
            .bind(status_name(&run.status))
            .bind(run.completed_at.map(|v| v.to_rfc3339()))
            .bind(&run.error_code)
            .bind(serde_json::to_string(&run.coverage)?)
            .bind(run.page.as_ref().map(serde_json::to_string).transpose()?)
            .bind(run.id.to_string())
            .execute(&self.pool)
            .await?;
        sqlx::query(PRUNE).execute(&self.pool).await?;
        Ok(run)
    }
    pub async fn close(&self) -> Result<(), TaxError> {
        let _worker = self.worker.lock().await;
        let checkpoint = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&self.pool)
            .await?;
        if checkpoint.try_get::<i64, _>(0)? != 0 {
            return Err(TaxError::Database(sqlx::Error::Protocol(
                "database checkpoint remained busy".to_owned(),
            )));
        }
        self.pool.close().await;
        Ok(())
    }
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
    use crate::sync::{FINISH, GET, INSERT, INTERRUPT, LATEST, PRUNE, SyncStatus, SyncStore};
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
            ("latest", LATEST, vec![]),
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
}
