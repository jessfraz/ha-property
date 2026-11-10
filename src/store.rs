use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::tax::{
    ledger::{
        Coverage, ImportInput, Observation, PropertyBinding, Snapshot, TaxError, validate_coverage,
    },
    reconcile::reconcile,
};

const STATE_SQL: &str =
    "SELECT revision, property_json, coverage_json FROM ledger_state WHERE singleton = 1";
const HEALTH_SQL: &str = "SELECT revision FROM ledger_state WHERE singleton = 1";
const OBSERVATIONS_SQL: &str = "SELECT body FROM observations ORDER BY id LIMIT ?";
const HISTORY_OBSERVATIONS_SQL: &str = "SELECT body FROM observations WHERE (kind = 'assessment' AND CAST(json_extract(body, '$.observation.data.fiscal_year') AS INTEGER) >= ?) OR (kind != 'assessment' AND unixepoch(json_extract(body, '$.evidence.effective_at')) >= ?) ORDER BY id LIMIT ?";
const OBSERVATION_PAGE_SQL: &str =
    "SELECT body FROM observations WHERE kind = ? AND id > ? ORDER BY id LIMIT ?";
const HISTORY_STATE_SQL: &str =
    "SELECT revision, property_json FROM ledger_state WHERE singleton = 1";
const ASSESSMENT_PAGE_SQL: &str = "SELECT body FROM observations WHERE kind = 'assessment' AND id > ? AND CAST(json_extract(body, '$.observation.data.fiscal_year') AS INTEGER) >= ? ORDER BY id LIMIT ?";
const PAYMENT_PAGE_SQL: &str = "SELECT body FROM observations WHERE kind = 'payment' AND id > ? AND unixepoch(json_extract(body, '$.evidence.effective_at')) >= ? ORDER BY id LIMIT ?";
const RECEIPT_SQL: &str =
    "SELECT preview_hash, result_json FROM import_receipts WHERE idempotency_key = ?";
const INSERT_OBSERVATION_SQL: &str = "INSERT INTO observations(id, kind, body) VALUES (?, ?, ?)";
const UPDATE_STATE_SQL: &str = "UPDATE ledger_state SET revision = ?, property_json = ?, coverage_json = ? WHERE singleton = 1 AND revision = ?";
const UPDATE_COVERAGE_SQL: &str = "UPDATE ledger_state SET revision = revision + 1, coverage_json = ? WHERE singleton = 1 AND revision = ?";
const UPDATE_PROPERTY_SQL: &str = "UPDATE ledger_state SET revision = revision + 1, property_json = ? WHERE singleton = 1 AND revision = ?";
const INSERT_RECEIPT_SQL: &str =
    "INSERT INTO import_receipts(idempotency_key, preview_hash, result_json) VALUES (?, ?, ?)";
const MAX_OBSERVATIONS: usize = 20_000;
pub const MAX_PROJECTION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

enum ObservationScope {
    All,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    Assessment,
    Payment,
}

impl ObservationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Assessment => "assessment",
            Self::Payment => "payment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRows {
    pub revision: i64,
    pub items: Vec<Observation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PropertyState {
    pub revision: i64,
    pub property: Option<PropertyBinding>,
    pub coverage: Vec<Coverage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub hash: String,
    pub revision: i64,
    pub input: ImportInput,
    pub added: usize,
    pub duplicates: usize,
    /// Exact IDs already present before this import. File-internal duplicates
    /// are still counted in duplicates, but their first occurrence is new.
    #[serde(default)]
    pub duplicate_ids: Vec<String>,
    pub summary: PreviewSummary,
}

/// A preview carries its exact import once, not repeated bill/evidence trees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewSummary {
    pub known_balance_cents: Option<i64>,
    pub needs_attention: usize,
    pub bill_count: usize,
    pub installment_count: usize,
    pub issue_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub preview: Preview,
    pub if_match: i64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyResult {
    pub revision: i64,
    pub added: usize,
    pub duplicates: usize,
    pub replayed: bool,
}

impl Store {
    /// One database owns all evidence and receipts. WAL is runtime state, not a backup.
    /// Stop the app before copying the entire data directory for a cold backup.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaxError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn health(&self) -> Result<(), TaxError> {
        sqlx::query_scalar::<_, i64>(HEALTH_SQL)
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    /// Call after listeners and accepted mutations have drained. A successful
    /// checkpoint plus pool shutdown makes the database file cold-copyable.
    pub async fn close(&self) -> Result<(), TaxError> {
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

    pub async fn snapshot(&self) -> Result<Snapshot, TaxError> {
        let mut transaction = self.pool.begin().await?;
        let snapshot = load_snapshot(&mut transaction, ObservationScope::All).await?;
        transaction.commit().await?;
        Ok(snapshot)
    }

    pub async fn history_snapshot(&self) -> Result<Snapshot, TaxError> {
        let mut transaction = self.pool.begin().await?;
        let snapshot = load_snapshot(&mut transaction, ObservationScope::History).await?;
        transaction.commit().await?;
        Ok(snapshot)
    }

    pub async fn property_state(&self) -> Result<PropertyState, TaxError> {
        let row = sqlx::query(STATE_SQL).fetch_one(&self.pool).await?;
        decode_property_state(&row)
    }

    pub async fn update_bought_on(
        &self,
        bought_on: Option<NaiveDate>,
        expected_revision: i64,
        now: DateTime<Utc>,
    ) -> Result<PropertyState, TaxError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(STATE_SQL).fetch_one(&mut *transaction).await?;
        let mut state = decode_property_state(&row)?;
        if state.revision != expected_revision {
            return Err(TaxError::RevisionConflict);
        }
        let property = state.property.as_mut().ok_or(TaxError::Invalid(
            "confirm a property before changing its history window",
        ))?;
        if property.bought_on == bought_on {
            transaction.commit().await?;
            return Ok(state);
        }
        property.bought_on = bought_on;
        property.validate_history(now)?;
        let next = state.revision.checked_add(1).ok_or(TaxError::Capacity)?;
        let changed = sqlx::query(UPDATE_PROPERTY_SQL)
            .bind(serde_json::to_string(property)?)
            .bind(expected_revision)
            .execute(&mut *transaction)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(TaxError::RevisionConflict);
        }
        state.revision = next;
        transaction.commit().await?;
        Ok(state)
    }

    pub async fn observation_page(
        &self,
        kind: ObservationKind,
        after: Option<&str>,
        limit: u32,
        expected_revision: Option<i64>,
    ) -> Result<ObservationRows, TaxError> {
        if !(1..=201).contains(&limit) || after.is_some_and(|value| value.len() > 200) {
            return Err(TaxError::Invalid(
                "observation page limit or cursor is invalid",
            ));
        }
        // Keep the revision and bounded evidence rows in one read snapshot, so
        // an import cannot silently mix ledger versions between pages.
        let mut transaction = self.pool.begin().await?;
        let state = sqlx::query(HISTORY_STATE_SQL)
            .fetch_one(&mut *transaction)
            .await?;
        let revision: i64 = state.try_get("revision")?;
        let property: Option<PropertyBinding> = state
            .try_get::<Option<&str>, _>("property_json")?
            .map(serde_json::from_str)
            .transpose()?;
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(TaxError::RevisionConflict);
        }
        let rows = match property.as_ref().filter(|p| p.bought_on.is_some()) {
            Some(property) => match kind {
                ObservationKind::Assessment => {
                    sqlx::query(ASSESSMENT_PAGE_SQL)
                        .bind(after.unwrap_or_default())
                        .bind(property.first_roll_year())
                        .bind(i64::from(limit))
                        .fetch_all(&mut *transaction)
                        .await?
                }
                ObservationKind::Payment => {
                    sqlx::query(PAYMENT_PAGE_SQL)
                        .bind(after.unwrap_or_default())
                        .bind(property.history_start()?.map(|date| date.timestamp()))
                        .bind(i64::from(limit))
                        .fetch_all(&mut *transaction)
                        .await?
                }
            },
            None => {
                sqlx::query(OBSERVATION_PAGE_SQL)
                    .bind(kind.as_str())
                    .bind(after.unwrap_or_default())
                    .bind(i64::from(limit))
                    .fetch_all(&mut *transaction)
                    .await?
            }
        };
        let items = rows
            .iter()
            .map(|row| {
                let body: &str = row.try_get("body")?;
                Ok(serde_json::from_str(body)?)
            })
            .collect::<Result<Vec<Observation>, TaxError>>()?;
        transaction.commit().await?;
        Ok(ObservationRows { revision, items })
    }

    pub async fn preview(&self, input: ImportInput) -> Result<Preview, TaxError> {
        let snapshot = self.snapshot().await?;
        build_preview(&snapshot, input)
    }

    pub async fn apply(&self, request: ApplyRequest) -> Result<ApplyResult, TaxError> {
        if request.idempotency_key.trim().is_empty()
            || request.idempotency_key.len() > 200
            || request.idempotency_key.chars().any(char::is_control)
        {
            return Err(TaxError::Invalid("a bounded idempotency key is required"));
        }
        if preview_hash(request.preview.revision, &request.preview.input)? != request.preview.hash {
            return Err(TaxError::PreviewMismatch);
        }
        if request.if_match != request.preview.revision {
            return Err(TaxError::RevisionConflict);
        }
        // IMMEDIATE acquires the writer reservation before the read: two concurrent
        // applies cannot both validate the same revision and then overwrite each other.
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(receipt) = sqlx::query(RECEIPT_SQL)
            .bind(&request.idempotency_key)
            .fetch_optional(&mut *transaction)
            .await?
        {
            let hash: String = receipt.try_get("preview_hash")?;
            if hash != request.preview.hash {
                return Err(TaxError::IdempotencyConflict);
            }
            let mut result: ApplyResult = serde_json::from_str(receipt.try_get("result_json")?)?;
            result.replayed = true;
            transaction.commit().await?;
            return Ok(result);
        }
        let snapshot = load_snapshot(&mut transaction, ObservationScope::All).await?;
        if snapshot.revision != request.if_match || request.preview.revision != request.if_match {
            return Err(TaxError::RevisionConflict);
        }
        let preview = build_preview(&snapshot, request.preview.input)?;
        if preview.hash != request.preview.hash {
            return Err(TaxError::PreviewMismatch);
        }
        let existing: BTreeMap<_, _> = snapshot
            .observations
            .iter()
            .map(|value| (&value.id, value))
            .collect();
        let mut unique = BTreeMap::new();
        for observation in &preview.input.observations {
            if !existing.contains_key(&observation.id) {
                unique.insert(&observation.id, observation);
            }
        }
        for observation in unique.values() {
            sqlx::query(INSERT_OBSERVATION_SQL)
                .bind(&observation.id)
                .bind(observation.observation.kind())
                .bind(serde_json::to_string(observation)?)
                .execute(&mut *transaction)
                .await?;
        }
        let property = snapshot
            .property
            .as_ref()
            .unwrap_or(&preview.input.property);
        let coverage = merged_coverage(&snapshot.coverage, &preview.input.coverage);
        let revision = snapshot.revision.checked_add(1).ok_or(TaxError::Capacity)?;
        let changed = sqlx::query(UPDATE_STATE_SQL)
            .bind(revision)
            .bind(serde_json::to_string(property)?)
            .bind(serde_json::to_string(&coverage)?)
            .bind(snapshot.revision)
            .execute(&mut *transaction)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(TaxError::RevisionConflict);
        }
        let result = ApplyResult {
            revision,
            added: preview.added,
            duplicates: preview.duplicates,
            replayed: false,
        };
        sqlx::query(INSERT_RECEIPT_SQL)
            .bind(&request.idempotency_key)
            .bind(&preview.hash)
            .bind(serde_json::to_string(&result)?)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(result)
    }

    pub async fn record_coverage(
        &self,
        coverage: Vec<Coverage>,
        expected_revision: i64,
    ) -> Result<i64, TaxError> {
        validate_coverage(&coverage)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let state = sqlx::query(STATE_SQL).fetch_one(&mut *transaction).await?;
        let revision: i64 = state.try_get("revision")?;
        if revision != expected_revision {
            return Err(TaxError::RevisionConflict);
        }
        let next_revision = revision.checked_add(1).ok_or(TaxError::Capacity)?;
        let prior: Vec<Coverage> = serde_json::from_str(state.try_get("coverage_json")?)?;
        let merged = merged_coverage(&prior, &coverage);
        let changed = sqlx::query(UPDATE_COVERAGE_SQL)
            .bind(serde_json::to_string(&merged)?)
            .bind(expected_revision)
            .execute(&mut *transaction)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(TaxError::RevisionConflict);
        }
        transaction.commit().await?;
        Ok(next_revision)
    }
}

fn decode_property_state(row: &sqlx::sqlite::SqliteRow) -> Result<PropertyState, TaxError> {
    Ok(PropertyState {
        revision: row.try_get("revision")?,
        property: row
            .try_get::<Option<&str>, _>("property_json")?
            .map(serde_json::from_str)
            .transpose()?,
        coverage: serde_json::from_str(row.try_get("coverage_json")?)?,
    })
}

async fn load_snapshot(
    connection: &mut SqliteConnection,
    scope: ObservationScope,
) -> Result<Snapshot, TaxError> {
    let state = sqlx::query(STATE_SQL).fetch_one(&mut *connection).await?;
    let property_json: Option<&str> = state.try_get("property_json")?;
    let property: Option<PropertyBinding> = property_json.map(serde_json::from_str).transpose()?;
    let limit = i64::try_from(MAX_OBSERVATIONS + 1).map_err(|_| TaxError::Capacity)?;
    let observations = match (scope, property.as_ref().filter(|p| p.bought_on.is_some())) {
        (ObservationScope::History, Some(property)) => {
            sqlx::query(HISTORY_OBSERVATIONS_SQL)
                .bind(property.first_roll_year())
                .bind(property.history_start()?.map(|at| at.timestamp()))
                .bind(limit)
                .fetch_all(&mut *connection)
                .await?
        }
        _ => {
            sqlx::query(OBSERVATIONS_SQL)
                .bind(limit)
                .fetch_all(&mut *connection)
                .await?
        }
    };
    if observations.len() > MAX_OBSERVATIONS {
        return Err(TaxError::Capacity);
    }
    let observations: Vec<Observation> = observations
        .iter()
        .map(|row| {
            let body: &str = row.try_get("body")?;
            Ok(serde_json::from_str(body)?)
        })
        .collect::<Result<_, TaxError>>()?;
    Ok(Snapshot {
        revision: state.try_get("revision")?,
        property,
        observations,
        coverage: serde_json::from_str(state.try_get("coverage_json")?)?,
    })
}

fn merged_coverage(prior: &[Coverage], incoming: &[Coverage]) -> Vec<Coverage> {
    let mut merged = prior.to_vec();
    for value in incoming {
        match merged
            .iter_mut()
            .find(|item| item.capability == value.capability)
        {
            Some(existing) if existing.checked_at <= value.checked_at => {
                *existing = value.clone();
            }
            None => merged.push(value.clone()),
            Some(_) => {}
        }
    }
    merged
}

fn preview_hash(revision: i64, input: &ImportInput) -> Result<String, TaxError> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        revision, input,
    ))?)))
}

fn build_preview(snapshot: &Snapshot, input: ImportInput) -> Result<Preview, TaxError> {
    input.validate()?;
    input.property.validate_history(Utc::now())?;
    if snapshot
        .property
        .as_ref()
        .is_some_and(|property| property.key != input.property.key)
    {
        return Err(TaxError::PropertyChanged);
    }
    let mut existing: BTreeMap<_, _> = snapshot
        .observations
        .iter()
        .map(|observation| (observation.id.clone(), observation.clone()))
        .collect();
    let duplicate_ids = input
        .observations
        .iter()
        .filter(|observation| {
            existing
                .get(&observation.id)
                .is_some_and(|prior| prior == *observation)
        })
        .map(|observation| observation.id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut added = 0;
    let mut duplicates = 0;
    for observation in &input.observations {
        if let Some(prior) = existing.get(&observation.id) {
            if prior != observation {
                return Err(TaxError::ObservationConflict);
            }
            duplicates += 1;
        } else {
            existing.insert(observation.id.clone(), observation.clone());
            added += 1;
        }
    }
    if existing.len() > MAX_OBSERVATIONS {
        return Err(TaxError::Capacity);
    }
    let candidate = Snapshot {
        revision: snapshot.revision.checked_add(1).ok_or(TaxError::Capacity)?,
        property: Some(
            snapshot
                .property
                .as_ref()
                .unwrap_or(&input.property)
                .clone(),
        ),
        observations: existing.into_values().collect(),
        coverage: merged_coverage(&snapshot.coverage, &input.coverage),
    };
    let projection = reconcile(&candidate, Utc::now());
    if serde_json::to_vec(&projection)?.len() > MAX_PROJECTION_BYTES {
        return Err(TaxError::Capacity);
    }
    Ok(Preview {
        hash: preview_hash(snapshot.revision, &input)?,
        revision: snapshot.revision,
        input,
        added,
        duplicates,
        duplicate_ids,
        summary: PreviewSummary {
            known_balance_cents: projection.known_balance_cents,
            needs_attention: projection.needs_attention,
            bill_count: projection.bills.len(),
            installment_count: projection.installments.len(),
            issue_count: projection.issues.len(),
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        store::{ApplyRequest, ObservationKind, Store},
        tax::ledger::{
            Assessment, ImportInput, Observation, ObservationData, Payment, PaymentStatus,
        },
    };
    use chrono::{NaiveDate, Utc};
    use sqlx::Row;

    #[tokio::test]
    async fn history_keyset_queries_preserve_pages_and_use_real_database_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ledger.sqlite3"))
            .await
            .unwrap();
        let mut input: ImportInput =
            serde_json::from_str(include_str!("../tests/fixtures/import.example.json")).unwrap();
        input.observations.clear();
        let binding = input.property.clone();
        let preview = store.preview(input).await.unwrap();
        store
            .apply(ApplyRequest {
                preview,
                if_match: 0,
                idempotency_key: "representative-ledger".into(),
            })
            .await
            .unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        for index in 0..4000 {
            let recent = index >= 3000;
            let kind = if index % 2 == 0 {
                ObservationData::Assessment(Assessment {
                    fiscal_year: if recent { 2021 } else { 2019 },
                    assessed_value_cents: 100_000,
                    description: "Fictional roll".into(),
                })
            } else {
                ObservationData::Payment(Payment {
                    reference: format!("fictional-payment-{index}"),
                    status: PaymentStatus::CountyPosted,
                    amount_cents: 100,
                    allocations: Vec::new(),
                })
            };
            let mut evidence = binding.confirmation.clone();
            evidence.effective_at = if recent {
                "2020-01-01T08:00:00Z"
            } else {
                "2019-12-31T23:59:59Z"
            }
            .parse()
            .unwrap();
            let observation = Observation {
                id: format!("row-{index:04}"),
                binding: binding.key.clone(),
                evidence,
                observation: kind,
            };
            sqlx::query(crate::store::INSERT_OBSERVATION_SQL)
                .bind(&observation.id)
                .bind(observation.observation.kind())
                .bind(serde_json::to_string(&observation).unwrap())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let state = store
            .update_bought_on(
                Some(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()),
                1,
                Utc::now(),
            )
            .await
            .unwrap();
        for kind in [ObservationKind::Assessment, ObservationKind::Payment] {
            let first = store
                .observation_page(kind, None, 201, Some(state.revision))
                .await
                .unwrap();
            assert_eq!(first.items.len(), 201);
            assert!(first.items.iter().all(|row| row.id.as_str() >= "row-3000"));
            let second = store
                .observation_page(kind, Some(&first.items[200].id), 201, Some(state.revision))
                .await
                .unwrap();
            assert_eq!(second.items.len(), 201);
            assert!(second.items.iter().all(|row| row.id > first.items[200].id));
        }
        let statements = [
            (
                "before-summary",
                crate::store::OBSERVATIONS_SQL,
                vec!["20001".to_owned()],
            ),
            (
                "after-summary",
                crate::store::HISTORY_OBSERVATIONS_SQL,
                vec![
                    "2020".to_owned(),
                    "1577865600".to_owned(),
                    "20001".to_owned(),
                ],
            ),
            (
                "before-unfiltered",
                crate::store::OBSERVATION_PAGE_SQL,
                vec![
                    "payment".to_owned(),
                    "row-1000".to_owned(),
                    "201".to_owned(),
                ],
            ),
            (
                "after-assessments",
                crate::store::ASSESSMENT_PAGE_SQL,
                vec!["row-1000".to_owned(), "2020".to_owned(), "201".to_owned()],
            ),
            (
                "after-payments",
                crate::store::PAYMENT_PAGE_SQL,
                vec![
                    "row-1000".to_owned(),
                    "1577865600".to_owned(),
                    "201".to_owned(),
                ],
            ),
            ("settings-read", crate::store::STATE_SQL, vec![]),
            (
                "history-state-read",
                crate::store::HISTORY_STATE_SQL,
                vec![],
            ),
            (
                "settings-write",
                crate::store::UPDATE_PROPERTY_SQL,
                vec![serde_json::to_string(&binding).unwrap(), "1".to_owned()],
            ),
        ];
        let started = std::time::Instant::now();
        let history = store.history_snapshot().await.unwrap();
        assert_eq!(history.observations.len(), 1000);
        eprintln!(
            "Filtered summary read: {}ms;1000 returned from4000 retained rows",
            started.elapsed().as_millis()
        );
        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        eprintln!("SQLite {version}; representative observations=4000;75% outside history window");
        for (name, statement, params) in statements {
            let sql = format!("EXPLAIN QUERY PLAN {statement}");
            // EXPLAIN receives only the fixed statements above, never user SQL.
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
            for param in params {
                query = query.bind(param);
            }
            let plan = query
                .fetch_all(&store.pool)
                .await
                .unwrap()
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            assert!(
                plan.iter().all(|detail| (name.ends_with("summary")
                    || !detail.contains("SCAN observations"))
                    && !detail.contains("TEMP B-TREE")),
                "{name}: {plan:?}"
            );
            eprintln!("{name}: {plan:?}");
        }
        store.close().await.unwrap();
    }
}
