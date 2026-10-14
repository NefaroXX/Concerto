//! H09 — scoped database checkpoint/restore for ONE concrete consumer: the
//! working-memory tables of [`MemoryDb`](crate::storage::MemoryDb)
//! (`memory.db`).
//!
//! # Why this consumer, and why this method
//!
//! `MemoryDb` is a real, already-present store whose schema this crate owns
//! (migrations `001_working_memory`, `002_links`), and whose file is separate
//! from the checkpoint sidecar itself (which lives in `sessions.db`) — so a
//! restore can never clobber the checkpoint record that authorized it. The
//! FTS (`fts_store`), vector (`vector_store`), global-memory, and watcher
//! stores are explicitly OUT of scope (they live in other databases or are
//! derived indexes — see the support matrix below).
//!
//! **Consistency is guaranteed with database-supported means, never a blind
//! file copy.** Capture runs every read on ONE pooled connection inside
//! `BEGIN IMMEDIATE … COMMIT`: the reserved lock blocks concurrent writers
//! for the snapshot window, so the exported rows are a single point-in-time
//! image, never torn mid-write. Restore runs `DELETE + INSERT` for the
//! scoped tables inside one `BEGIN IMMEDIATE … COMMIT` write transaction, so
//! concurrent readers never see a half-restored scope. The live database file
//! is never copied, moved, or opened outside the SQL interface.
//!
//! # Scope (declared, exact)
//!
//! [`MEMORY_DB_CHECKPOINT_TABLES`] = `decisions`, `task_nodes`,
//! `memory_links` — the three tables `MemoryDb` migrations own. Nothing else
//! in the file is read or written by this adapter.
//!
//! # Support matrix (honest limits)
//!
//! | Change | Supported? |
//! |---|
//! | Data rows (`INSERT`/`UPDATE`/`DELETE` within the same schema) | **Yes** — checkpoint → mutate → restore round-trips, validated (row counts + content hash + `PRAGMA quick_check` + `PRAGMA foreign_key_check`). |
//! | Schema / DDL (`ALTER`/`CREATE`/`DROP`, column changes) | **No** — capture records each scoped table's exact `CREATE` statement; restore compares and **refuses** (`SchemaChanged`) when the live schema differs. Reported, never silently mishandled. |
//! | Migration-version changes (`_sqlx_migrations` drift) | **No** — same refusal path: any `CREATE`-statement difference refuses. |
//! | FTS / vector / global-memory / reindex-queue state | **No** — out of scope; a rollback guarantee covering them is refused at preflight (unclaimed effect). |
//! | Concurrent-writer linearizability beyond the txn window | Point-in-time only: the snapshot is consistent at capture; writes landing after `COMMIT` are not included (stated, not hidden). |
//!
//! # Authorization posture
//!
//! These are pool-level primitives. A restore is an authorized mutation ONLY
//! when driven through the orchestrator rollback facade
//! (`concerto-orchestrator::managed_rollback`), which runs the coverage
//! preflight first and executes the restore as a host-owned operation through
//! the single `ToolExecutor` chokepoint (policy-gated, audited), with the
//! scope-ownership check below enforced fail-closed before any write.

use std::collections::BTreeMap;
use std::time::Duration;

use concerto_core::checkpoint::{EffectCoverage, EffectKind, EffectSupport};
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Scope, bounds, support declarations
// ---------------------------------------------------------------------------

/// Envelope schema version of [`MemoryDbCheckpoint`]'s serialized form.
/// Named-version policy (per the `CheckpointRecord` precedent): this exact
/// version loads as-is; anything else is rejected.
pub const MEMORY_DB_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// The exact table scope this adapter checkpoints and restores. Fixed and
/// declared: every capture and restore names these tables, in this order.
pub const MEMORY_DB_CHECKPOINT_TABLES: &[&str] = &["decisions", "task_nodes", "memory_links"];

/// Ownership scope string for the working-memory tables (the oracle key).
pub const SCOPE_WORKING_MEMORY: &str = "memory_db:working_memory";

/// Fail-closed row ceiling per scoped table (bounded I/O: capture refuses
/// rather than banking an unbounded snapshot).
pub const MEMORY_DB_CHECKPOINT_MAX_ROWS_PER_TABLE: usize = 50_000;

/// Fail-closed ceiling on the whole checkpoint's serialized rows, in bytes.
pub const MEMORY_DB_CHECKPOINT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Wall-clock bound on one capture or restore call (statement-level hangs
/// fail as `Timeout`, never park forever).
pub const MEMORY_DB_CHECKPOINT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How capture guarantees a non-torn snapshot (operator-readable, also used
/// as the `Supported.method` string in [`memory_db_adapter_coverage`]).
pub const MEMORY_DB_CHECKPOINT_METHOD: &str =
    "single-connection BEGIN IMMEDIATE snapshot of the scoped tables (no file copy); restore is one BEGIN IMMEDIATE DELETE+INSERT transaction, validated after commit";

/// The schema-change limit (operator-readable, also used as the `Qualified`
/// limitation — data-only support).
pub const MEMORY_DB_CHECKPOINT_DATA_ONLY_LIMITATION: &str =
    "data rows only for decisions/task_nodes/memory_links; schema/DDL/migration changes unsupported — restore refuses when the live CREATE statements differ; FTS/vector/global-memory/reindex state out of scope";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the memory-DB checkpoint adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MemoryDbCheckpointError {
    /// The operation was cancelled.
    #[error("memory db checkpoint operation cancelled")]
    Cancelled,
    /// The I/O bound was exceeded.
    #[error("memory db checkpoint I/O timed out")]
    Timeout,
    /// The database backend failed.
    #[error("memory db checkpoint database failed: {0}")]
    Database(String),
    /// A bound refused the operation (fail-closed, never partial).
    #[error("memory db checkpoint budget refused: {0}")]
    Budget(String),
    /// The live schema differs from the checkpoint's: DDL is unsupported, so
    /// the restore is refused BEFORE any write (reported, never silently
    /// mishandled).
    #[error("memory db checkpoint restore refused: live schema changed: {detail}")]
    SchemaChanged {
        /// What differs (operator-readable).
        detail: String,
    },
    /// A foreign-owned scope aborted the restore before any write.
    #[error("memory db checkpoint restore refused: scope {scope} owned by {owner}")]
    OwnershipConflict {
        /// The contested scope.
        scope: String,
        /// The holding agent.
        owner: String,
    },
    /// The checkpoint payload is malformed (a stored row that does not match
    /// the pinned schema — never guessed into shape).
    #[error("memory db checkpoint payload corrupt: {0}")]
    CorruptCheckpoint(String),
    /// Post-restore validation failed (counts/hash/`quick_check`/FK check).
    /// The attempted rows were already committed — the report says so; the
    /// caller must treat the scope as needing operator review, never as
    /// restored.
    #[error("memory db checkpoint post-restore validation failed: {0}")]
    ValidationFailed(String),
    /// JSON encoding failed.
    #[error("memory db checkpoint serialization failed: {0}")]
    Serialization(String),
}

impl From<sqlx::Error> for MemoryDbCheckpointError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err.to_string())
    }
}

impl From<serde_json::Error> for MemoryDbCheckpointError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Ownership oracle (scope-level, mirroring the core file oracle)
// ---------------------------------------------------------------------------

/// Read side of the scope-ownership table for database restore: `None` means
/// unowned (the restore may proceed); `Some` names the holding agent. A
/// foreign owner aborts the whole restore before any write (fail-closed).
pub trait DbScopeOwnership: Send + Sync + std::fmt::Debug {
    /// The agent holding `scope`, if any.
    fn owner_of(&self, scope: &str) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Checkpoint record
// ---------------------------------------------------------------------------

/// A scoped, validated snapshot of the working-memory tables.
///
/// Runtime-authored fact (ADR-65): constructed by [`checkpoint_memory_db`],
/// never by agent prose. The `rows` are the durable pre-image (JSON objects
/// with the pinned per-table columns); `table_sql` is the schema marker that
/// makes DDL drift a loud refusal instead of a silent misshape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDbCheckpoint {
    /// Envelope schema version ([`MEMORY_DB_CHECKPOINT_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Checkpoint identity (ULID).
    pub checkpoint_id: String,
    /// Capture time, Unix epoch millis (UTC).
    pub captured_at_ms: i64,
    /// Scoped tables, in [`MEMORY_DB_CHECKPOINT_TABLES`] order.
    pub tables: Vec<String>,
    /// Exact `CREATE` statement per scoped table at capture (the DDL marker).
    pub table_sql: BTreeMap<String, String>,
    /// Pre-image rows per table (JSON objects, primary-key ordered).
    pub rows: BTreeMap<String, Vec<serde_json::Value>>,
    /// Row counts per table at capture (validation input).
    pub row_counts: BTreeMap<String, usize>,
    /// Blake3 hex over the canonical row serialization (validation input).
    pub content_hash: String,
}

impl MemoryDbCheckpoint {
    /// Serialize the checkpoint envelope.
    pub fn to_json(&self) -> Result<String, MemoryDbCheckpointError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Deserialize with the named-version policy: the current version loads
    /// as-is; anything else is rejected (greenfield record — no legacy
    /// versions to migrate).
    pub fn from_json(json: &str) -> Result<Self, MemoryDbCheckpointError> {
        let record: Self = serde_json::from_str(json)?;
        if record.schema_version != MEMORY_DB_CHECKPOINT_SCHEMA_VERSION {
            return Err(MemoryDbCheckpointError::Serialization(format!(
                "unsupported memory db checkpoint schema version {}: this runtime supports v{MEMORY_DB_CHECKPOINT_SCHEMA_VERSION} only",
                record.schema_version
            )));
        }
        Ok(record)
    }
}

/// What one restore did (returned by value — the durable effects record for
/// the scope).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDbRestoreReport {
    /// The checkpoint restored.
    pub checkpoint_id: String,
    /// Tables rewritten, in scope order.
    pub tables: Vec<String>,
    /// Row counts rewritten per table.
    pub row_counts: BTreeMap<String, usize>,
    /// Post-restore validation observations (counts/hash/`quick_check`/FK).
    pub validation: Vec<String>,
}

/// Unix epoch milliseconds (UTC).
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// This adapter's share of a preflight's `offered` list: `Database` is
/// `Qualified` (data-only, validated); every other kind is `Unsupported`
/// (this adapter claims nothing outside its scope).
#[must_use]
pub fn memory_db_adapter_coverage() -> Vec<EffectCoverage> {
    let mut offered = vec![EffectCoverage {
        kind: EffectKind::Database,
        support: EffectSupport::Qualified {
            method: MEMORY_DB_CHECKPOINT_METHOD.to_string(),
            limitation: MEMORY_DB_CHECKPOINT_DATA_ONLY_LIMITATION.to_string(),
        },
    }];
    for kind in [EffectKind::File, EffectKind::Process, EffectKind::Shell, EffectKind::Network] {
        offered.push(EffectCoverage {
            kind,
            support: EffectSupport::Unsupported {
                reason: format!(
                    "the memory-db adapter claims no {} effects (scope: {})",
                    kind.as_str(),
                    MEMORY_DB_CHECKPOINT_TABLES.join(", ")
                ),
            },
        });
    }
    offered
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

fn check_cancel(cancel: &CancellationToken) -> Result<(), MemoryDbCheckpointError> {
    if cancel.is_cancelled() {
        Err(MemoryDbCheckpointError::Cancelled)
    } else {
        Ok(())
    }
}

/// Capture a scoped checkpoint of the working-memory tables over `pool`.
///
/// Every read runs on ONE connection inside `BEGIN IMMEDIATE … COMMIT`, so
/// concurrent writers are locked out for the snapshot window and the image
/// is point-in-time consistent — never torn mid-write, never a file copy.
/// Row and byte ceilings fail closed before anything is banked.
///
/// `pool` is borrowed, never owned: connection lifetime stays with the
/// caller (no pool reconfiguration, no side effects on the live store).
pub async fn checkpoint_memory_db(
    pool: &SqlitePool,
    cancel: CancellationToken,
) -> Result<MemoryDbCheckpoint, MemoryDbCheckpointError> {
    if cancel.is_cancelled() {
        return Err(MemoryDbCheckpointError::Cancelled);
    }
    let work = capture_inner(pool, cancel.clone());
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(MemoryDbCheckpointError::Cancelled),
        outcome = tokio::time::timeout(MEMORY_DB_CHECKPOINT_IO_TIMEOUT, work) => {
            match outcome {
                Ok(result) => result,
                Err(_) => Err(MemoryDbCheckpointError::Timeout),
            }
        }
    }
}

async fn capture_inner(
    pool: &SqlitePool,
    cancel: CancellationToken,
) -> Result<MemoryDbCheckpoint, MemoryDbCheckpointError> {
    let mut conn = pool.acquire().await.map_err(MemoryDbCheckpointError::from)?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let snapshot = capture_snapshot(&mut conn, &cancel).await;
    // The connection must return to the pool transaction-free on every path:
    // commit the read snapshot, roll back on capture failure.
    match snapshot {
        Ok((table_sql, rows)) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            let mut row_counts = BTreeMap::new();
            let mut total_bytes: usize = 0;
            for (table, table_rows) in &rows {
                row_counts.insert(table.clone(), table_rows.len());
                total_bytes = total_bytes.saturating_add(
                    serde_json::to_string(table_rows).map(|json| json.len()).unwrap_or(usize::MAX),
                );
            }
            if total_bytes > MEMORY_DB_CHECKPOINT_MAX_TOTAL_BYTES {
                return Err(MemoryDbCheckpointError::Budget(format!(
                    "snapshot holds {total_bytes} bytes, bound is {MEMORY_DB_CHECKPOINT_MAX_TOTAL_BYTES}"
                )));
            }
            let content_hash = content_hash_for(&rows);
            Ok(MemoryDbCheckpoint {
                schema_version: MEMORY_DB_CHECKPOINT_SCHEMA_VERSION,
                checkpoint_id: concerto_core::ids::Ulid::new().to_string(),
                captured_at_ms: now_millis(),
                tables: MEMORY_DB_CHECKPOINT_TABLES.iter().map(|table| table.to_string()).collect(),
                table_sql,
                rows,
                row_counts,
                content_hash,
            })
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

async fn capture_snapshot(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    cancel: &CancellationToken,
) -> Result<
    (BTreeMap<String, String>, BTreeMap<String, Vec<serde_json::Value>>),
    MemoryDbCheckpointError,
> {
    let mut table_sql = BTreeMap::new();
    let mut rows = BTreeMap::new();
    for table in MEMORY_DB_CHECKPOINT_TABLES {
        check_cancel(cancel)?;
        let sql: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(*table)
                .fetch_optional(&mut **conn)
                .await?
                .flatten();
        let Some(sql) = sql else {
            return Err(MemoryDbCheckpointError::Database(format!(
                "scoped table {table} is absent from the live database"
            )));
        };
        // AUDITED (sqlx 0.9 `AssertSqlSafe`): `{table}` iterates the
        // hard-coded [`MEMORY_DB_CHECKPOINT_TABLES`] allow-list — no caller
        // input is interpolated; values are bound.
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&mut **conn)
                .await?;
        let count = usize::try_from(count).map_err(|_| {
            MemoryDbCheckpointError::Database(format!(
                "scoped table {table} has an unrepresentable row count"
            ))
        })?;
        if count > MEMORY_DB_CHECKPOINT_MAX_ROWS_PER_TABLE {
            return Err(MemoryDbCheckpointError::Budget(format!(
                "table {table} holds {count} rows, bound is {MEMORY_DB_CHECKPOINT_MAX_ROWS_PER_TABLE}"
            )));
        }
        let table_rows = export_table(&mut *conn, table).await?;
        table_sql.insert(table.to_string(), sql);
        rows.insert(table.to_string(), table_rows);
    }
    Ok((table_sql, rows))
}

/// Export one scoped table's rows as JSON objects (primary-key ordered, so
/// the content hash is deterministic). Columns are pinned to the migration
/// schema — a schema change surfaces as a decode failure here, never a guess.
async fn export_table(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<serde_json::Value>, MemoryDbCheckpointError> {
    match table {
        "decisions" => {
            let rows = sqlx::query(
                "SELECT id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at FROM decisions ORDER BY id",
            )
            .fetch_all(conn)
            .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(serde_json::json!({
                    "id": row.try_get::<String, _>("id")?,
                    "session_id": row.try_get::<String, _>("session_id")?,
                    "task_id": row.try_get::<Option<String>, _>("task_id")?,
                    "what": row.try_get::<String, _>("what")?,
                    "why": row.try_get::<String, _>("why")?,
                    "outcome": row.try_get::<Option<String>, _>("outcome")?,
                    "category": row.try_get::<String, _>("category")?,
                    "confidence": row.try_get::<f64, _>("confidence")?,
                    "superseded_by": row.try_get::<Option<String>, _>("superseded_by")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                }));
            }
            Ok(out)
        }
        "task_nodes" => {
            let rows = sqlx::query(
                "SELECT id, session_id, parent_id, description, status, blocking, created_at FROM task_nodes ORDER BY id",
            )
            .fetch_all(conn)
            .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(serde_json::json!({
                    "id": row.try_get::<String, _>("id")?,
                    "session_id": row.try_get::<String, _>("session_id")?,
                    "parent_id": row.try_get::<Option<String>, _>("parent_id")?,
                    "description": row.try_get::<String, _>("description")?,
                    "status": row.try_get::<String, _>("status")?,
                    "blocking": row.try_get::<String, _>("blocking")?,
                    "created_at": row.try_get::<i64, _>("created_at")?,
                }));
            }
            Ok(out)
        }
        "memory_links" => {
            let rows = sqlx::query(
                "SELECT source_id, target_id, link_type, weight, created_at, expires_at FROM memory_links ORDER BY source_id, target_id, link_type",
            )
            .fetch_all(conn)
            .await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(serde_json::json!({
                    "source_id": row.try_get::<String, _>("source_id")?,
                    "target_id": row.try_get::<String, _>("target_id")?,
                    "link_type": row.try_get::<String, _>("link_type")?,
                    "weight": row.try_get::<f64, _>("weight")?,
                    "created_at": row.try_get::<String, _>("created_at")?,
                    "expires_at": row.try_get::<Option<String>, _>("expires_at")?,
                }));
            }
            Ok(out)
        }
        other => Err(MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint scope names an unknown table: {other}"
        ))),
    }
}

/// Deterministic content identity over the exported rows.
fn content_hash_for(rows: &BTreeMap<String, Vec<serde_json::Value>>) -> String {
    let canonical = serde_json::to_string(rows).unwrap_or_default();
    blake3::hash(canonical.as_bytes()).to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Restore a scoped checkpoint over `pool`, validated after commit.
///
/// Fail-closed ordering, before ANY write: cancellation → schema-marker
/// comparison (DDL drift refuses as [`MemoryDbCheckpointError::SchemaChanged`])
/// → scope-ownership check → write transaction → post-commit validation
/// (counts + content hash + `PRAGMA quick_check` + `PRAGMA foreign_key_check`).
#[allow(clippy::too_many_arguments)]
pub async fn restore_memory_db(
    pool: &SqlitePool,
    checkpoint: &MemoryDbCheckpoint,
    ownership: &dyn DbScopeOwnership,
    by_agent: &str,
    cancel: CancellationToken,
) -> Result<MemoryDbRestoreReport, MemoryDbCheckpointError> {
    if cancel.is_cancelled() {
        return Err(MemoryDbCheckpointError::Cancelled);
    }
    if checkpoint.schema_version != MEMORY_DB_CHECKPOINT_SCHEMA_VERSION {
        return Err(MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "unsupported memory db checkpoint schema version {}",
            checkpoint.schema_version
        )));
    }
    let work = restore_inner(pool, checkpoint, ownership, by_agent, cancel.clone());
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(MemoryDbCheckpointError::Cancelled),
        outcome = tokio::time::timeout(MEMORY_DB_CHECKPOINT_IO_TIMEOUT, work) => {
            match outcome {
                Ok(result) => result,
                Err(_) => Err(MemoryDbCheckpointError::Timeout),
            }
        }
    }
}

async fn restore_inner(
    pool: &SqlitePool,
    checkpoint: &MemoryDbCheckpoint,
    ownership: &dyn DbScopeOwnership,
    by_agent: &str,
    cancel: CancellationToken,
) -> Result<MemoryDbRestoreReport, MemoryDbCheckpointError> {
    check_cancel(&cancel)?;
    // Schema marker first: a DDL change since capture refuses BEFORE any
    // write — the declared-unsupported case, reported loudly.
    let live_sql = live_table_sql(pool).await?;
    for table in MEMORY_DB_CHECKPOINT_TABLES {
        let wanted = checkpoint.table_sql.get(*table).ok_or_else(|| {
            MemoryDbCheckpointError::CorruptCheckpoint(format!(
                "checkpoint has no schema marker for scoped table {table}"
            ))
        })?;
        let live = live_sql.get(*table).ok_or_else(|| MemoryDbCheckpointError::SchemaChanged {
            detail: format!("scoped table {table} is absent from the live database"),
        })?;
        if live != wanted {
            return Err(MemoryDbCheckpointError::SchemaChanged {
                detail: format!(
                    "scoped table {table} changed since capture (CREATE statement differs): schema/DDL restore unsupported"
                ),
            });
        }
    }
    check_cancel(&cancel)?;
    // Ownership before any write: a foreign-owned scope aborts fail-closed.
    if let Some(owner) = ownership.owner_of(SCOPE_WORKING_MEMORY) {
        if owner != by_agent {
            return Err(MemoryDbCheckpointError::OwnershipConflict {
                scope: SCOPE_WORKING_MEMORY.to_string(),
                owner,
            });
        }
    }
    check_cancel(&cancel)?;
    // Payload sanity before the transaction: absurd row counts refuse rather
    // than filling the write txn with unbounded inserts.
    for table in MEMORY_DB_CHECKPOINT_TABLES {
        let rows = checkpoint.rows.get(*table).ok_or_else(|| {
            MemoryDbCheckpointError::CorruptCheckpoint(format!(
                "checkpoint has no rows for scoped table {table}"
            ))
        })?;
        if rows.len() > MEMORY_DB_CHECKPOINT_MAX_ROWS_PER_TABLE {
            return Err(MemoryDbCheckpointError::Budget(format!(
                "checkpoint holds {} rows for {table}, bound is {MEMORY_DB_CHECKPOINT_MAX_ROWS_PER_TABLE}",
                rows.len()
            )));
        }
    }

    let mut conn = pool.acquire().await.map_err(MemoryDbCheckpointError::from)?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let rewrite = rewrite_scope(&mut conn, checkpoint).await;
    match rewrite {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            return Err(error);
        }
    }
    validate_restore(pool, checkpoint).await
}

async fn live_table_sql(
    pool: &SqlitePool,
) -> Result<BTreeMap<String, String>, MemoryDbCheckpointError> {
    let mut out = BTreeMap::new();
    for table in MEMORY_DB_CHECKPOINT_TABLES {
        let sql: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(*table)
                .fetch_optional(pool)
                .await?
                .flatten();
        if let Some(sql) = sql {
            out.insert(table.to_string(), sql);
        }
    }
    Ok(out)
}

async fn rewrite_scope(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    checkpoint: &MemoryDbCheckpoint,
) -> Result<(), MemoryDbCheckpointError> {
    // No foreign keys among the scoped tables: delete order is irrelevant, but
    // keep scope order for determinism.
    for table in MEMORY_DB_CHECKPOINT_TABLES {
        let rows = checkpoint.rows.get(*table).ok_or_else(|| {
            MemoryDbCheckpointError::CorruptCheckpoint(format!(
                "checkpoint has no rows for scoped table {table}"
            ))
        })?;
        // AUDITED (sqlx 0.9 `AssertSqlSafe`): `{table}` iterates the
        // hard-coded [`MEMORY_DB_CHECKPOINT_TABLES`] allow-list — no caller
        // input is interpolated; the checkpoint id is never interpolated.
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&mut **conn)
            .await?;
        for row in rows {
            insert_row(&mut *conn, table, row).await?;
        }
    }
    Ok(())
}

fn field<'a>(
    row: &'a serde_json::Value,
    table: &str,
    name: &str,
) -> Result<&'a serde_json::Value, MemoryDbCheckpointError> {
    row.get(name).ok_or_else(|| {
        MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint row for {table} is missing field {name}"
        ))
    })
}

fn as_str<'a>(
    row: &'a serde_json::Value,
    table: &str,
    name: &str,
) -> Result<&'a str, MemoryDbCheckpointError> {
    field(row, table, name)?.as_str().ok_or_else(|| {
        MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint row for {table} has a non-string field {name}"
        ))
    })
}

fn as_opt_str(
    row: &serde_json::Value,
    table: &str,
    name: &str,
) -> Result<Option<String>, MemoryDbCheckpointError> {
    match field(row, table, name)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(text) => Ok(Some(text.clone())),
        _ => Err(MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint row for {table} has a non-string-or-null field {name}"
        ))),
    }
}

fn as_f64(
    row: &serde_json::Value,
    table: &str,
    name: &str,
) -> Result<f64, MemoryDbCheckpointError> {
    field(row, table, name)?.as_f64().ok_or_else(|| {
        MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint row for {table} has a non-numeric field {name}"
        ))
    })
}

fn as_i64(
    row: &serde_json::Value,
    table: &str,
    name: &str,
) -> Result<i64, MemoryDbCheckpointError> {
    field(row, table, name)?.as_i64().ok_or_else(|| {
        MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint row for {table} has a non-integer field {name}"
        ))
    })
}

async fn insert_row(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
    row: &serde_json::Value,
) -> Result<(), MemoryDbCheckpointError> {
    match table {
        "decisions" => {
            sqlx::query(
                "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(as_str(row, table, "id")?)
            .bind(as_str(row, table, "session_id")?)
            .bind(as_opt_str(row, table, "task_id")?)
            .bind(as_str(row, table, "what")?)
            .bind(as_str(row, table, "why")?)
            .bind(as_opt_str(row, table, "outcome")?)
            .bind(as_str(row, table, "category")?)
            .bind(as_f64(row, table, "confidence")?)
            .bind(as_opt_str(row, table, "superseded_by")?)
            .bind(as_i64(row, table, "created_at")?)
            .execute(conn)
            .await?;
            Ok(())
        }
        "task_nodes" => {
            sqlx::query(
                "INSERT INTO task_nodes (id, session_id, parent_id, description, status, blocking, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(as_str(row, table, "id")?)
            .bind(as_str(row, table, "session_id")?)
            .bind(as_opt_str(row, table, "parent_id")?)
            .bind(as_str(row, table, "description")?)
            .bind(as_str(row, table, "status")?)
            .bind(as_str(row, table, "blocking")?)
            .bind(as_i64(row, table, "created_at")?)
            .execute(conn)
            .await?;
            Ok(())
        }
        "memory_links" => {
            sqlx::query(
                "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(as_str(row, table, "source_id")?)
            .bind(as_str(row, table, "target_id")?)
            .bind(as_str(row, table, "link_type")?)
            .bind(as_f64(row, table, "weight")?)
            .bind(as_str(row, table, "created_at")?)
            .bind(as_opt_str(row, table, "expires_at")?)
            .execute(conn)
            .await?;
            Ok(())
        }
        other => Err(MemoryDbCheckpointError::CorruptCheckpoint(format!(
            "checkpoint scope names an unknown table: {other}"
        ))),
    }
}

/// Post-commit validation: the scope must read back EXACTLY the checkpoint
/// (counts + content hash), with a clean `quick_check` and no FK violations.
/// Anything else is [`MemoryDbCheckpointError::ValidationFailed`] — the rows
/// were already committed, so the report states that plainly.
async fn validate_restore(
    pool: &SqlitePool,
    checkpoint: &MemoryDbCheckpoint,
) -> Result<MemoryDbRestoreReport, MemoryDbCheckpointError> {
    let mut validation = Vec::new();
    let mut row_counts = BTreeMap::new();
    let mut reread: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    // A bounded read transaction for the validation image (same consistency
    // shape as capture, without mutating anything).
    let mut conn = pool.acquire().await.map_err(MemoryDbCheckpointError::from)?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let reread_outcome = async {
        for table in MEMORY_DB_CHECKPOINT_TABLES {
            let table_rows = export_table(&mut conn, table).await?;
            row_counts.insert(table.to_string(), table_rows.len());
            reread.insert(table.to_string(), table_rows);
        }
        Ok::<(), MemoryDbCheckpointError>(())
    }
    .await;
    // Validation reads never leave the connection transactional.
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
    reread_outcome?;
    drop(conn);

    for table in MEMORY_DB_CHECKPOINT_TABLES {
        let want = checkpoint.row_counts.get(*table).copied().unwrap_or(usize::MAX);
        let got = row_counts.get(*table).copied().unwrap_or(usize::MAX);
        if want != got {
            return Err(MemoryDbCheckpointError::ValidationFailed(format!(
                "table {table} holds {got} rows after restore, checkpoint holds {want} (attempted rows already committed — operator review required)"
            )));
        }
        validation.push(format!("{table}: {got} rows (matches checkpoint)"));
    }
    let reread_hash = content_hash_for(&reread);
    if reread_hash != checkpoint.content_hash {
        return Err(MemoryDbCheckpointError::ValidationFailed(format!(
            "scope content hash {reread_hash} differs from checkpoint {} (attempted rows already committed — operator review required)",
            checkpoint.content_hash
        )));
    }
    validation.push(format!("content hash {} verified", checkpoint.content_hash));
    let quick: String = sqlx::query_scalar("PRAGMA quick_check").fetch_one(pool).await?;
    if quick != "ok" {
        return Err(MemoryDbCheckpointError::ValidationFailed(format!(
            "PRAGMA quick_check reported {quick:?} after restore (attempted rows already committed — operator review required)"
        )));
    }
    validation.push("PRAGMA quick_check: ok".to_string());
    let violations: Vec<(String, i64, String, i64)> =
        sqlx::query_as("PRAGMA foreign_key_check").fetch_all(pool).await?;
    if !violations.is_empty() {
        return Err(MemoryDbCheckpointError::ValidationFailed(format!(
            "{} foreign-key violation(s) after restore (attempted rows already committed — operator review required)",
            violations.len()
        )));
    }
    validation.push("PRAGMA foreign_key_check: no violations".to_string());
    Ok(MemoryDbRestoreReport {
        checkpoint_id: checkpoint.checkpoint_id.clone(),
        tables: MEMORY_DB_CHECKPOINT_TABLES.iter().map(|table| table.to_string()).collect(),
        row_counts,
        validation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryDb;

    #[derive(Debug, Default)]
    struct UnownedScopes;

    impl DbScopeOwnership for UnownedScopes {
        fn owner_of(&self, _scope: &str) -> Option<String> {
            None
        }
    }

    #[derive(Debug)]
    struct ForeignScope {
        owner: String,
    }

    impl DbScopeOwnership for ForeignScope {
        fn owner_of(&self, _scope: &str) -> Option<String> {
            Some(self.owner.clone())
        }
    }

    async fn temp_db() -> (tempfile::TempDir, MemoryDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            camino::Utf8PathBuf::from_path_buf(dir.path().join("memory.db")).expect("utf8 path");
        let db = MemoryDb::connect(&path).await.expect("temp memory db");
        (dir, db)
    }

    async fn seed_decision(db: &MemoryDb) {
        sqlx::query(
            "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("d1")
        .bind("s1")
        .bind(Option::<String>::None)
        .bind("ship it")
        .bind("because")
        .bind(Option::<String>::None)
        .bind("\"other\"")
        .bind(0.5_f64)
        .bind(Option::<String>::None)
        .bind(1_700_000_000_i64)
        .execute(db.pool())
        .await
        .expect("seed");
    }

    #[tokio::test]
    async fn checkpoint_mutate_restore_round_trips_on_a_temp_store() {
        let (_dir, db) = temp_db().await;
        seed_decision(&db).await;
        let checkpoint =
            checkpoint_memory_db(db.pool(), CancellationToken::new()).await.expect("capture");
        assert_eq!(checkpoint.tables, MEMORY_DB_CHECKPOINT_TABLES);
        assert_eq!(checkpoint.row_counts.get("decisions"), Some(&1));

        // Mutate after capture: delete the row, add another + a task node.
        sqlx::query("DELETE FROM decisions WHERE id = 'd1'")
            .execute(db.pool())
            .await
            .expect("mutate");
        sqlx::query(
            "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at) VALUES ('d2', 's9', NULL, 'other', 'x', NULL, '\"other\"', 0.1, NULL, 2)",
        )
        .execute(db.pool())
        .await
        .expect("mutate");
        sqlx::query(
            "INSERT INTO task_nodes (id, session_id, parent_id, description, status, blocking, created_at) VALUES ('t1', 's9', NULL, 'node', 'pending', '[]', 3)",
        )
        .execute(db.pool())
        .await
        .expect("mutate");

        let report = restore_memory_db(
            db.pool(),
            &checkpoint,
            &UnownedScopes,
            "agent-a",
            CancellationToken::new(),
        )
        .await
        .expect("restore");
        assert_eq!(report.checkpoint_id, checkpoint.checkpoint_id);
        assert_eq!(report.row_counts.get("decisions"), Some(&1));
        assert_eq!(report.row_counts.get("task_nodes"), Some(&0));
        assert!(report.validation.iter().any(|line| line.contains("quick_check: ok")));

        // The scope reads back EXACTLY the checkpoint: d1 back, d2/t1 gone.
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM decisions ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("reread");
        assert_eq!(ids, vec!["d1".to_string()]);
        let nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_nodes")
            .fetch_one(db.pool())
            .await
            .expect("reread");
        assert_eq!(nodes, 0);
        // The envelope round-trips under the named-version policy.
        let back = MemoryDbCheckpoint::from_json(&checkpoint.to_json().expect("serialize"))
            .expect("decode");
        assert_eq!(back, checkpoint);
    }

    #[tokio::test]
    async fn restore_refuses_schema_change_instead_of_misshaping() {
        let (_dir, db) = temp_db().await;
        seed_decision(&db).await;
        let checkpoint =
            checkpoint_memory_db(db.pool(), CancellationToken::new()).await.expect("capture");
        // A DDL change after capture: the declared-unsupported case.
        sqlx::query("ALTER TABLE decisions ADD COLUMN extra TEXT")
            .execute(db.pool())
            .await
            .expect("ddl");
        match restore_memory_db(
            db.pool(),
            &checkpoint,
            &UnownedScopes,
            "agent-a",
            CancellationToken::new(),
        )
        .await
        {
            Err(MemoryDbCheckpointError::SchemaChanged { detail }) => {
                assert!(detail.contains("decisions"), "{detail}");
            }
            other => panic!("schema drift must refuse loudly, got {other:?}"),
        }
        // And the refusal wrote nothing: the seeded row is untouched.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decisions")
            .fetch_one(db.pool())
            .await
            .expect("reread");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn restore_refuses_foreign_owned_scope_before_any_write() {
        let (_dir, db) = temp_db().await;
        seed_decision(&db).await;
        let checkpoint =
            checkpoint_memory_db(db.pool(), CancellationToken::new()).await.expect("capture");
        sqlx::query("DELETE FROM decisions").execute(db.pool()).await.expect("mutate");
        let oracle = ForeignScope { owner: "agent-b".to_string() };
        match restore_memory_db(
            db.pool(),
            &checkpoint,
            &oracle,
            "agent-a",
            CancellationToken::new(),
        )
        .await
        {
            Err(MemoryDbCheckpointError::OwnershipConflict { scope, owner }) => {
                assert_eq!(scope, SCOPE_WORKING_MEMORY);
                assert_eq!(owner, "agent-b");
            }
            other => panic!("foreign-owned scope must refuse, got {other:?}"),
        }
        // Self-owned proceeds.
        let oracle = ForeignScope { owner: "agent-a".to_string() };
        restore_memory_db(db.pool(), &checkpoint, &oracle, "agent-a", CancellationToken::new())
            .await
            .expect("self-owned restore proceeds");
    }

    #[tokio::test]
    async fn capture_and_restore_honor_cancellation() {
        let (_dir, db) = temp_db().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            checkpoint_memory_db(db.pool(), cancel.clone()).await,
            Err(MemoryDbCheckpointError::Cancelled)
        ));
        let checkpoint =
            checkpoint_memory_db(db.pool(), CancellationToken::new()).await.expect("capture");
        assert!(matches!(
            restore_memory_db(db.pool(), &checkpoint, &UnownedScopes, "agent-a", cancel).await,
            Err(MemoryDbCheckpointError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn checkpoint_rejects_unknown_versions() {
        let (_dir, db) = temp_db().await;
        let checkpoint =
            checkpoint_memory_db(db.pool(), CancellationToken::new()).await.expect("capture");
        let mut value = serde_json::to_value(&checkpoint).expect("to value");
        value["schema_version"] = serde_json::json!(999);
        let json = serde_json::to_string(&value).expect("to json");
        assert!(matches!(
            MemoryDbCheckpoint::from_json(&json),
            Err(MemoryDbCheckpointError::Serialization(_))
        ));
    }

    #[test]
    fn adapter_coverage_claims_database_only_as_qualified() {
        let offered = memory_db_adapter_coverage();
        let database =
            offered.iter().find(|entry| entry.kind == EffectKind::Database).expect("database");
        match &database.support {
            EffectSupport::Qualified { limitation, .. } => {
                assert!(limitation.contains("data rows only"), "{limitation}");
            }
            other => panic!("database must be qualified data-only, got {other:?}"),
        }
        for kind in [EffectKind::File, EffectKind::Process, EffectKind::Shell, EffectKind::Network]
        {
            let entry = offered.iter().find(|entry| entry.kind == kind).expect("entry");
            assert!(
                matches!(entry.support, EffectSupport::Unsupported { .. }),
                "adapter claims nothing outside its scope: {kind:?}"
            );
        }
    }
}
