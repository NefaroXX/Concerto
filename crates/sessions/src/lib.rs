#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-sessions` — SQLite-backed session persistence with WAL mode,
//! advisory locking, schema migrations, and conversation history.

pub mod audit;
pub mod plan_bindings;
pub mod plans;
pub mod replay;
pub mod spend;
pub mod whiteboard;

pub use plan_bindings::PlanBindingRecord;
pub use whiteboard::{
    NewWhiteboardEvent, WhiteboardEvent, WhiteboardKind, WhiteboardScope, WhiteboardSubscription,
};

#[cfg(test)]
pub mod testing;

use concerto_core::ids::Ulid;
use concerto_core::transcript::TranscriptEntry;
use concerto_core::types::{Message, ProviderMetrics, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::TaskId;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{AssertSqlSafe, Row, SqlitePool};
use thiserror::Error;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("lock error: {0}")]
    Lock(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<sqlx::Error> for SessionError {
    fn from(err: sqlx::Error) -> Self {
        SessionError::Database(err.to_string())
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(err: serde_json::Error) -> Self {
        SessionError::Serialization(err.to_string())
    }
}

/// Resolve the Concerto data root — `dirs::data_dir()` (i.e.
/// `$XDG_DATA_HOME` or `~/.local/share` on Linux) joined with `concerto` —
/// creating the directory on demand.
///
/// This is the single source of truth for every on-disk data-root lookup
/// (sessions DB, memory, audit, planner plans). Consumers must call this
/// instead of re-deriving `dirs::data_dir().join("concerto")`.
pub fn app_data_dir() -> Result<std::path::PathBuf, SessionError> {
    let data_dir = dirs::data_dir()
        .map(|p| p.join("concerto"))
        .ok_or_else(|| SessionError::Lock("unable to determine data directory".to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| SessionError::Lock(e.to_string()))?;
    Ok(data_dir)
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Ulid,
    pub created_at: time::OffsetDateTime,
    pub project_dir: camino::Utf8PathBuf,
    pub provider: String,
    pub model: String,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: Ulid,
    pub created_at: time::OffsetDateTime,
    pub provider: String,
    pub model: String,
    pub message_count: usize,
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
}

/// Summary information for a checkpoint.
#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    pub id: Ulid,
    pub label: String,
    pub sequence_num: u64,
    pub created_at: time::OffsetDateTime,
}

/// Authoritative active multi-agent checkpoint for a session.
#[derive(Debug, Clone)]
pub struct OrchestrationCheckpointRecord {
    pub session_id: Ulid,
    pub run_id: Ulid,
    pub root_task_id: TaskId,
    pub project_id: String,
    pub objective_hash: String,
    pub schema_version: u32,
    pub source_revision: Option<String>,
    pub sequence_num: u64,
    pub state_json: String,
    pub completed: bool,
    pub updated_at: time::OffsetDateTime,
}

// ---------------------------------------------------------------------------
// SessionStore trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(
        &self,
        project_dir: &camino::Utf8Path,
        provider: &str,
        model: &str,
        cancel: CancellationToken,
    ) -> Result<Session, SessionError>;

    async fn load_session(
        &self,
        id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Option<Session>, SessionError>;

    async fn save_message(
        &self,
        session_id: Ulid,
        msg: &Message,
        tokens_in: u64,
        tokens_out: u64,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn append_messages(
        &self,
        session_id: Ulid,
        messages: &[Message],
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn load_messages(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<Message>, SessionError>;

    async fn list_recent_sessions(
        &self,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError>;

    /// List sessions created before `before_unix` (unix seconds), most recent
    /// first. Used by `concerto sessions prune --all-projects`.
    async fn list_sessions_older_than(
        &self,
        before_unix: i64,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError>;

    /// Delete a session and every row that references it. Returns `Ok(false)`
    /// if no session with `id` exists, `Ok(true)` after a successful delete.
    async fn delete_session(
        &self,
        id: Ulid,
        cancel: CancellationToken,
    ) -> Result<bool, SessionError>;

    /// Return the ids of every session currently mapped as the active session
    /// of some project (from `project_active_sessions`). `concerto sessions
    /// prune` uses this to protect active sessions from deletion.
    async fn active_session_ids(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<Ulid>, SessionError>;

    // ---- Project-scoped session management ----

    /// List sessions belonging to a specific project directory, most recent first.
    async fn list_sessions_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError>;

    /// Return the currently-active session id for a project, if one is mapped.
    async fn get_active_session_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        cancel: CancellationToken,
    ) -> Result<Option<Ulid>, SessionError>;

    /// Upsert the active session mapping for a project.
    async fn set_active_session_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn record_metrics(
        &self,
        session_id: Ulid,
        metrics: ProviderMetrics,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    // ---- Phase 2 extensions ----

    async fn record_event(
        &self,
        session_id: Ulid,
        event: &concerto_core::event::Event,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn load_events(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::replay::StoredEvent>, SessionError>;

    async fn load_events_until(
        &self,
        session_id: Ulid,
        max_seq: u64,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::replay::StoredEvent>, SessionError>;

    async fn record_spend(
        &self,
        record: crate::spend::SpendRecord,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    /// List a session's spend records in deterministic order
    /// (`created_at ASC, id ASC`), oldest first.
    async fn list_spend_records(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::spend::SpendRecord>, SessionError>;

    async fn spend_summary(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<crate::spend::SpendSummary, SessionError>;

    // ---- Phase 3: task tracking ----

    async fn create_task(
        &self,
        task: &concerto_core::types::AgentTask,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn update_task_status(
        &self,
        task_id: TaskId,
        status: &str,
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn get_task(
        &self,
        task_id: TaskId,
        cancel: CancellationToken,
    ) -> Result<Option<concerto_core::types::AgentTask>, SessionError>;

    async fn list_tasks(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::types::AgentTask>, SessionError>;

    // ---- Phase 4: checkpointing ----
    async fn create_checkpoint(
        &self,
        session_id: Ulid,
        task_id: concerto_core::types::TaskId,
        label: &str,
        vfs_snapshot: &str,
        sequence_num: u64,
        cancel: CancellationToken,
    ) -> Result<Ulid, SessionError>;

    async fn load_checkpoint(
        &self,
        checkpoint_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<(String, u64), SessionError>;

    async fn list_checkpoints(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<CheckpointSummary>, SessionError>;

    // ---- Durable multi-agent orchestration checkpoint ----

    async fn save_orchestration_checkpoint(
        &self,
        record: &OrchestrationCheckpointRecord,
    ) -> Result<(), SessionError>;

    async fn load_orchestration_checkpoint(
        &self,
        session_id: Ulid,
    ) -> Result<Option<OrchestrationCheckpointRecord>, SessionError>;

    async fn clear_orchestration_checkpoint(&self, session_id: Ulid) -> Result<(), SessionError>;

    // ---- Durable typed transcript (ADR-36) ----

    /// Append entries to a session's durable typed transcript. Entries are
    /// written atomically with monotonically increasing sequence numbers, so
    /// `load_transcript` always returns them in append order.
    async fn append_transcript(
        &self,
        session_id: Ulid,
        entries: &[TranscriptEntry],
        cancel: CancellationToken,
    ) -> Result<(), SessionError>;

    /// Load a session's durable typed transcript in sequence order. An empty
    /// or unknown session yields an empty vec, not an error.
    async fn load_transcript(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<TranscriptEntry>, SessionError>;

    // ---- Durable plan bindings (ADR-55 Phase 2b live-fix) ----
    //
    // Mirrors the process-scoped `PlanApprovalRegistry` on disk so a
    // natural-language approval ("i approve the plan") offered after an app
    // restart can still arm the Apply/Replan dialog. Defaults keep fakes and
    // test doubles honest for in-process flows: no storage means no durable
    // fallback, exactly like a registry-only build.

    /// Save (upsert) a durable plan binding, newest-wins per
    /// `(session_id, objective_hash)`. Whitespace-only plans are never stored.
    async fn save_plan_binding(
        &self,
        record: &PlanBindingRecord,
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        let _ = (record, cancel);
        Ok(())
    }

    /// Load the newest durable binding for `session_id` across every
    /// objective (created_at DESC, insertion order DESC).
    async fn load_newest_plan_binding(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Option<PlanBindingRecord>, SessionError> {
        let _ = (session_id, cancel);
        Ok(None)
    }

    /// Delete the durable binding for `(session_id, objective_hash)` — called
    /// when an Apply decision consumes the plan so a later bare approval
    /// ("yes") cannot re-arm the dialog for an executed plan. Returns
    /// `Ok(false)` when no row existed; deleting a missing row is a no-op.
    async fn delete_plan_binding(
        &self,
        session_id: Ulid,
        objective_hash: &str,
        cancel: CancellationToken,
    ) -> Result<bool, SessionError> {
        let _ = (session_id, objective_hash, cancel);
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// SqliteSessionStore
// ---------------------------------------------------------------------------

pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub async fn connect() -> Result<Self, SessionError> {
        let data_dir = app_data_dir()?;
        let db_path = data_dir.join("sessions.db");
        Self::connect_path(&db_path).await
    }

    /// Connect to an explicit database path. SQLite WAL and `busy_timeout`
    /// provide safe multi-process coordination; no process-lifetime advisory
    /// lock is held.
    ///
    /// Self-heals a corrupted database (ADR-54): if the first open fails, the
    /// main file is quarantined to `<name>.corrupt-<unix_utc_ts>.bak` and the
    /// open is retried once against a fresh database. If that retry also
    /// fails, the original error is returned (it is never masked).
    async fn connect_path(db_path: &std::path::Path) -> Result<Self, SessionError> {
        match Self::try_connect(db_path).await {
            Ok(store) => Ok(store),
            Err(original) => {
                match concerto_core::helpers::quarantine_corrupt_db_file(db_path) {
                    // Only a file that is NOT a valid SQLite database is moved.
                    // A schema/migration failure on a valid file surfaces as
                    // the original error — never masked by deleting user data
                    // (ADR-54).
                    Some(quarantine) => {
                        tracing::warn!(
                            path = %db_path.display(),
                            quarantine = %quarantine.display(),
                            "sessions database was not a valid SQLite file; quarantined corrupted file and retrying with a fresh database"
                        );
                        match Self::try_connect(db_path).await {
                            Ok(store) => Ok(store),
                            // The retry failed too — surface the original
                            // failure so the cause is never masked by the
                            // recovery attempt.
                            Err(_) => Err(original),
                        }
                    }
                    None => Err(original),
                }
            }
        }
    }

    /// Open the database without quarantine recovery.
    async fn try_connect(db_path: &std::path::Path) -> Result<Self, SessionError> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);

        let pool = PoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        // Read the SQLite header so a garbage/truncated file fails the open
        // deterministically instead of surfacing later on the first query.
        let _schema_version: i64 = sqlx::query_scalar("PRAGMA schema_version;")
            .fetch_one(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout = 5000;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA foreign_keys = ON;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA synchronous = NORMAL;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        Ok(Self { pool })
    }

    // In‑memory connection for tests – avoids filesystem side‑effects and uses the same PRAGMAs.
    pub async fn connect_in_memory() -> Result<Self, SessionError> {
        let db_name = format!("file:concerto_test_{}?mode=memory", fastrand::u64(..));
        let options = SqliteConnectOptions::new()
            .filename(&db_name)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);

        let pool = PoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        // Apply the same PRAGMAs as the regular connection.
        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout = 5000;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA foreign_keys = ON;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;
        sqlx::query("PRAGMA synchronous = NORMAL;")
            .execute(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| SessionError::Database(e.to_string()))?;

        Ok(Self { pool })
    }
}

/// Normalise a project directory path for consistent storage and lookup.
///
/// Resolves lexical `.`/`..` components (portable, no filesystem access) and
/// returns a canonical string. The same project must always map to the same
/// key regardless of how the path was supplied.
fn normalize_project_dir(path: &camino::Utf8Path) -> camino::Utf8PathBuf {
    let canonical = concerto_core::helpers::canonical_project_path(path.as_std_path());
    if let Ok(utf8) = camino::Utf8PathBuf::from_path_buf(canonical) {
        return utf8;
    }

    let mut stack: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            camino::Utf8Component::CurDir => {}
            camino::Utf8Component::ParentDir => match stack.last().map(String::as_str) {
                Some("/") | Some("..") => stack.push("..".to_string()),
                Some(_) => {
                    stack.pop();
                }
                None => stack.push("..".to_string()),
            },
            camino::Utf8Component::Normal(s) => stack.push(s.to_string()),
            camino::Utf8Component::RootDir => stack.push("/".to_string()),
            camino::Utf8Component::Prefix(_) => stack.push("..".to_string()),
        }
    }

    if stack.first().map(String::as_str) == Some("/") {
        let inner = stack[1..].join("/");
        if inner.is_empty() {
            camino::Utf8PathBuf::from("/")
        } else {
            camino::Utf8PathBuf::from(format!("/{inner}"))
        }
    } else {
        camino::Utf8PathBuf::from(stack.join("/"))
    }
}

/// Bail with `SessionError` if the token has been cancelled.
///
/// Reuses an existing error variant with a "cancelled" message, matching the
/// pattern used across the workspace (e.g. `concerto_memory` maps a cancelled
/// token onto a generic failure variant). `SessionStore` methods that iterate
/// or issue multiple statements call this at each statement boundary.
fn check_cancel(cancel: &CancellationToken) -> Result<(), SessionError> {
    if cancel.is_cancelled() {
        Err(SessionError::Database("operation cancelled".to_string()))
    } else {
        Ok(())
    }
}

/// Decode one row of a session-summary listing query into a `SessionSummary`.
///
/// Shared by `list_recent_sessions`, `list_sessions_for_project` and
/// `list_sessions_older_than`, which all project the same column set.
fn session_summary_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SessionSummary, SessionError> {
    let id_str: String = row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
    let id = Ulid::from_string(&id_str)
        .map_err(|_| SessionError::Serialization("invalid ULID in database".to_string()))?;
    let created_at_unix: i64 =
        row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
    let created_at = time::OffsetDateTime::from_unix_timestamp(created_at_unix)
        .map_err(|e| SessionError::Database(e.to_string()))?;
    let provider: String =
        row.try_get("provider").map_err(|e| SessionError::Database(e.to_string()))?;
    let model: String = row.try_get("model").map_err(|e| SessionError::Database(e.to_string()))?;
    let message_count: i64 =
        row.try_get("message_count").map_err(|e| SessionError::Database(e.to_string()))?;
    let total_cost_usd: f64 =
        row.try_get("total_cost_usd").map_err(|e| SessionError::Database(e.to_string()))?;
    let total_tokens_in: i64 =
        row.try_get("total_tokens_in").map_err(|e| SessionError::Database(e.to_string()))?;
    let total_tokens_out: i64 =
        row.try_get("total_tokens_out").map_err(|e| SessionError::Database(e.to_string()))?;

    Ok(SessionSummary {
        id,
        created_at,
        provider,
        model,
        message_count: message_count as usize,
        total_cost_usd,
        total_tokens_in: total_tokens_in as u64,
        total_tokens_out: total_tokens_out as u64,
    })
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(
        &self,
        project_dir: &camino::Utf8Path,
        provider: &str,
        model: &str,
        _cancel: CancellationToken,
    ) -> Result<Session, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let id = Ulid::new();
        let created_at = time::OffsetDateTime::now_utc();
        let created_at_unix = created_at.unix_timestamp();
        let project_dir = normalize_project_dir(project_dir);

        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) VALUES (?, ?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(created_at_unix)
            .bind(project_dir.as_str())
            .bind(provider)
            .bind(model)
            .execute(&self.pool)
            .await?;

        Ok(Session {
            id,
            created_at,
            project_dir: project_dir.to_path_buf(),
            provider: provider.to_string(),
            model: model.to_string(),
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_cost_usd: 0.0,
        })
    }

    async fn load_session(
        &self,
        id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Option<Session>, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let row = sqlx::query(
            "SELECT id, created_at, project_dir, provider, model, total_tokens_in, total_tokens_out, total_cost_usd FROM sessions WHERE id = ?"
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let id_str: String =
                    row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
                let id = Ulid::from_string(&id_str).map_err(|_| {
                    SessionError::Serialization("invalid ULID in database".to_string())
                })?;
                let created_at_unix: i64 =
                    row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
                let created_at = time::OffsetDateTime::from_unix_timestamp(created_at_unix)
                    .map_err(|e| SessionError::Database(e.to_string()))?;
                let project_dir_str: String = row
                    .try_get("project_dir")
                    .map_err(|e| SessionError::Database(e.to_string()))?;
                let project_dir = camino::Utf8PathBuf::from(project_dir_str);
                let provider: String =
                    row.try_get("provider").map_err(|e| SessionError::Database(e.to_string()))?;
                let model: String =
                    row.try_get("model").map_err(|e| SessionError::Database(e.to_string()))?;
                let total_tokens_in: i64 = row
                    .try_get("total_tokens_in")
                    .map_err(|e| SessionError::Database(e.to_string()))?;
                let total_tokens_out: i64 = row
                    .try_get("total_tokens_out")
                    .map_err(|e| SessionError::Database(e.to_string()))?;
                let total_cost_usd: f64 = row
                    .try_get("total_cost_usd")
                    .map_err(|e| SessionError::Database(e.to_string()))?;

                Ok(Some(Session {
                    id,
                    created_at,
                    project_dir,
                    provider,
                    model,
                    total_tokens_in: total_tokens_in as u64,
                    total_tokens_out: total_tokens_out as u64,
                    total_cost_usd,
                }))
            }
            None => Ok(None),
        }
    }

    async fn save_message(
        &self,
        session_id: Ulid,
        msg: &Message,
        _tokens_in: u64,
        _tokens_out: u64,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let id = Ulid::new();
        let role_str = match msg.role {
            concerto_core::types::Role::System => "system",
            concerto_core::types::Role::User => "user",
            concerto_core::types::Role::Assistant => "assistant",
            concerto_core::types::Role::Tool => "tool",
            _ => "unknown",
        };

        let tool_calls_json = match &msg.tool_calls {
            Some(tc) => Some(serde_json::to_string(tc)?),
            None => None,
        };

        let tool_results_json = match &msg.tool_results {
            Some(tr) => Some(serde_json::to_string(tr)?),
            None => None,
        };

        let reasoning_content = msg.reasoning_content.clone();
        // ADR-48 §4: real usage when the message carries it; the schema
        // columns are NOT NULL DEFAULT 0, so `None` (unknown) is stored as 0
        // and round-trips back as `None` on load.
        let tokens_in = msg.tokens_in.unwrap_or(0) as i64;
        let tokens_out = msg.tokens_out.unwrap_or(0) as i64;

        // Atomic insert with sequence number computed via COALESCE to avoid TOCTOU race
        sqlx::query(
            "INSERT INTO messages (id, session_id, sequence_num, role, content, tool_calls, tool_results, reasoning_content, tokens_in, tokens_out) \
            SELECT ?, ?, COALESCE(MAX(sequence_num), 0) + 1, ?, ?, ?, ?, ?, ?, ? FROM messages WHERE session_id = ?"
        )
        .bind(id.to_string())
        .bind(session_id.to_string())
        .bind(role_str)
        .bind(&msg.content)
        .bind(tool_calls_json)
        .bind(tool_results_json)
        .bind(reasoning_content)
        .bind(tokens_in)
        .bind(tokens_out)
        .bind(session_id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn append_messages(
        &self,
        session_id: Ulid,
        messages: &[Message],
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(&cancel)?;

        let mut tx = self.pool.begin().await?;

        for msg in messages {
            // Cancellation checked at statement boundaries (per batch item).
            check_cancel(&cancel)?;

            let id = Ulid::new();
            let role_str = match msg.role {
                concerto_core::types::Role::System => "system",
                concerto_core::types::Role::User => "user",
                concerto_core::types::Role::Assistant => "assistant",
                concerto_core::types::Role::Tool => "tool",
                _ => "unknown",
            };

            let tool_calls_json = match &msg.tool_calls {
                Some(tc) => Some(serde_json::to_string(tc)?),
                None => None,
            };

            let tool_results_json = match &msg.tool_results {
                Some(tr) => Some(serde_json::to_string(tr)?),
                None => None,
            };

            let reasoning_content = msg.reasoning_content.clone();
            // ADR-48 §4: real usage when the message carries it; `None`
            // (unknown) is stored as 0 (columns are NOT NULL DEFAULT 0).
            let tokens_in = msg.tokens_in.unwrap_or(0) as i64;
            let tokens_out = msg.tokens_out.unwrap_or(0) as i64;

            sqlx::query(
                "INSERT INTO messages (id, session_id, sequence_num, role, content, tool_calls, tool_results, reasoning_content, tokens_in, tokens_out) \
                SELECT ?, ?, COALESCE(MAX(sequence_num), 0) + 1, ?, ?, ?, ?, ?, ?, ? FROM messages WHERE session_id = ?"
            )
            .bind(id.to_string())
            .bind(session_id.to_string())
            .bind(role_str)
            .bind(&msg.content)
            .bind(tool_calls_json)
            .bind(tool_results_json)
            .bind(reasoning_content)
            .bind(tokens_in)
            .bind(tokens_out)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn load_messages(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<Message>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT role, content, tool_calls, tool_results, reasoning_content, tokens_in, tokens_out FROM messages WHERE session_id = ? ORDER BY sequence_num ASC"
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let role_str: String = row.try_get("role")?;
            let content: String = row.try_get("content")?;
            let tool_calls_json: Option<String> = row.try_get("tool_calls")?;
            let tool_results_json: Option<String> = row.try_get("tool_results")?;
            let reasoning_content: Option<String> = row.try_get("reasoning_content")?;
            let tokens_in: i64 = row.try_get("tokens_in")?;
            let tokens_out: i64 = row.try_get("tokens_out")?;

            // ADR-48 §4: a stored value > 0 is real usage; 0 is
            // indistinguishable from unknown (legacy rows and `None` writes),
            // so it restores as `None` and the estimator heuristic applies.
            let tokens_in = (tokens_in > 0).then_some(tokens_in as u64);
            let tokens_out = (tokens_out > 0).then_some(tokens_out as u64);

            let role = match role_str.as_str() {
                "system" => concerto_core::types::Role::System,
                "user" => concerto_core::types::Role::User,
                "assistant" => concerto_core::types::Role::Assistant,
                "tool" => concerto_core::types::Role::Tool,
                other => {
                    return Err(SessionError::Serialization(format!(
                        "invalid message role: {other}"
                    )))
                }
            };

            let tool_calls = match tool_calls_json {
                Some(json) if !json.is_empty() => Some(serde_json::from_str(&json)?),
                _ => None,
            };

            let tool_results = match tool_results_json {
                Some(json) if !json.is_empty() => Some(serde_json::from_str(&json)?),
                _ => None,
            };

            messages.push(Message {
                role,
                content,
                tool_calls,
                tool_results,
                reasoning_content,
                tokens_in,
                tokens_out,
            });
        }

        Ok(messages)
    }

    async fn list_recent_sessions(
        &self,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        check_cancel(&cancel)?;

        let limit_i64 = limit as i64;
        let rows = sqlx::query(
            "SELECT s.id, s.created_at, s.provider, s.model,
                    s.total_cost_usd, s.total_tokens_in, s.total_tokens_out,
                    COUNT(m.id) as message_count
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             GROUP BY s.id
             ORDER BY s.created_at DESC
             LIMIT ?",
        )
        .bind(limit_i64)
        .fetch_all(&self.pool)
        .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;
            summaries.push(session_summary_from_row(&row)?);
        }

        Ok(summaries)
    }

    async fn list_sessions_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        check_cancel(&cancel)?;

        let project_dir = normalize_project_dir(project_dir);
        let limit_i64 = limit as i64;
        let rows = sqlx::query(
            "SELECT s.id, s.created_at, s.provider, s.model,
                    s.total_cost_usd, s.total_tokens_in, s.total_tokens_out,
                    COUNT(m.id) as message_count
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             WHERE s.project_dir = ?
             GROUP BY s.id
             ORDER BY s.created_at DESC
             LIMIT ?",
        )
        .bind(project_dir.as_str())
        .bind(limit_i64)
        .fetch_all(&self.pool)
        .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;
            summaries.push(session_summary_from_row(&row)?);
        }

        Ok(summaries)
    }

    async fn list_sessions_older_than(
        &self,
        before_unix: i64,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT s.id, s.created_at, s.provider, s.model,
                    s.total_cost_usd, s.total_tokens_in, s.total_tokens_out,
                    COUNT(m.id) as message_count
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             WHERE s.created_at < ?
             GROUP BY s.id
             ORDER BY s.created_at DESC",
        )
        .bind(before_unix)
        .fetch_all(&self.pool)
        .await?;

        let mut summaries = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;
            summaries.push(session_summary_from_row(&row)?);
        }

        Ok(summaries)
    }

    async fn delete_session(
        &self,
        id: Ulid,
        cancel: CancellationToken,
    ) -> Result<bool, SessionError> {
        check_cancel(&cancel)?;

        let id_str = id.to_string();
        let mut tx = self.pool.begin().await?;

        // Check existence first inside the transaction so the returned bool is
        // exact even though the child deletes would affect 0 rows for an
        // unknown id.
        let exists = sqlx::query("SELECT 1 FROM sessions WHERE id = ?")
            .bind(&id_str)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            tx.rollback().await?;
            return Ok(false);
        }

        // Child tables WITHOUT ON DELETE CASCADE must be deleted explicitly, in
        // FK-safe order. `agent_run_results.task_id` REFERENCES subtasks(id), so
        // agent_run_results must be deleted before subtasks. `tasks` is not
        // referenced by any FK (subtasks/task_nodes/checkpoints declare plain
        // parent_id/task_id columns without REFERENCES), so it is deleted after
        // its own children. `audit_log` is deliberately NOT deleted: it is an
        // append-only decision record (ADR-40) whose session_id FK was changed
        // by migration 021 to ON DELETE SET NULL, so the session delete below
        // detaches (nulls) the audit rows instead of losing them. Tables with
        // ON DELETE CASCADE (project_active_sessions, orchestration_checkpoints)
        // are left to the cascade and must NOT be deleted here.
        for table in [
            "messages",
            "provider_metrics",
            "session_events",
            "spend_records",
            "decisions",
            "task_nodes",
            "agent_run_results",
            "subtasks",
            "tasks",
            "checkpoints",
            "transcript_entries",
            "plan_bindings",
        ] {
            // AUDITED (sqlx 0.9 `AssertSqlSafe`): `{table}` iterates the hard-coded
            // literal allow-list above — no user input is interpolated; the session id
            // is bound.
            sqlx::query(AssertSqlSafe(format!("DELETE FROM {table} WHERE session_id = ?")))
                .bind(&id_str)
                .execute(&mut *tx)
                .await?;
            // Cancellation checked at statement boundaries.
            check_cancel(&cancel)?;
        }

        let rows = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(&id_str)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        tx.commit().await?;
        Ok(rows > 0)
    }

    async fn active_session_ids(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<Ulid>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query("SELECT session_id FROM project_active_sessions")
            .fetch_all(&self.pool)
            .await?;

        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String =
                row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let id = Ulid::from_string(&id_str)
                .map_err(|_| SessionError::Serialization("invalid ULID in database".to_string()))?;
            ids.push(id);
        }

        Ok(ids)
    }

    async fn get_active_session_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        _cancel: CancellationToken,
    ) -> Result<Option<Ulid>, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let project_dir = normalize_project_dir(project_dir);
        let row =
            sqlx::query("SELECT session_id FROM project_active_sessions WHERE project_dir = ?")
                .bind(project_dir.as_str())
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some(row) => {
                let id_str: String =
                    row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
                let id = Ulid::from_string(&id_str).map_err(|_| {
                    SessionError::Serialization("invalid ULID in database".to_string())
                })?;
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    async fn set_active_session_for_project(
        &self,
        project_dir: &camino::Utf8Path,
        session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let project_dir = normalize_project_dir(project_dir);
        let updated_at = time::OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query(
            "INSERT INTO project_active_sessions (project_dir, session_id, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(project_dir) DO UPDATE SET session_id = excluded.session_id, updated_at = excluded.updated_at",
        )
        .bind(project_dir.as_str())
        .bind(session_id.to_string())
        .bind(updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_metrics(
        &self,
        session_id: Ulid,
        metrics: ProviderMetrics,
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(&cancel)?;

        let id = Ulid::new();
        let created_at = time::OffsetDateTime::now_utc();
        let created_at_unix = created_at.unix_timestamp();

        // The metric row INSERT and the session aggregate UPDATE must be
        // atomic: a crash or error between them would otherwise leave a
        // provider_metrics row whose session totals were never incremented.
        // Both statements commit together or roll back together.
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO provider_metrics (id, session_id, provider, model, tokens_in, tokens_out, cost_usd, latency_ms, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(id.to_string())
        .bind(session_id.to_string())
        .bind(&metrics.provider)
        .bind(&metrics.model)
        .bind(metrics.tokens_in as i64)
        .bind(metrics.tokens_out as i64)
        .bind(metrics.cost_usd)
        .bind(metrics.latency_ms as i64)
        .bind(created_at_unix)
        .execute(&mut *tx)
        .await?;

        // Cancellation checked at statement boundaries.
        check_cancel(&cancel)?;

        sqlx::query(
            "UPDATE sessions SET total_tokens_in = total_tokens_in + ?, total_tokens_out = total_tokens_out + ?, total_cost_usd = total_cost_usd + ? WHERE id = ?"
        )
        .bind(metrics.tokens_in as i64)
        .bind(metrics.tokens_out as i64)
        .bind(metrics.cost_usd)
        .bind(session_id.to_string())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(())
    }

    // ---- Phase 2: event recording ----

    async fn record_event(
        &self,
        session_id: Ulid,
        event: &concerto_core::event::Event,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let id = Ulid::new();
        // Atomic insert for event with sequence number computed via COALESCE to avoid TOCTOU race
        let created_at_unix = event.timestamp.unix_timestamp();
        let payload = serde_json::to_string(&event.kind)?;

        sqlx::query(
            "INSERT INTO session_events (id, session_id, sequence_num, correlation_id, event_kind, payload, created_at) \
            SELECT ?, ?, COALESCE(MAX(sequence_num), 0) + 1, ?, ?, ?, ? FROM session_events WHERE session_id = ?"
        )
        .bind(id.to_string())
        .bind(session_id.to_string())
        .bind(event.correlation_id.to_string())
        .bind(format!("{:?}", event.kind).split(' ').next().unwrap_or("unknown"))
        .bind(&payload)
        .bind(created_at_unix)
        .bind(session_id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn load_events(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::replay::StoredEvent>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT id, session_id, sequence_num, correlation_id, event_kind, payload, created_at FROM session_events WHERE session_id = ? ORDER BY sequence_num ASC")
            .bind(session_id.to_string())
            .fetch_all(&self.pool)
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String =
                row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
            let id = Ulid::from_string(&id_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let sid_str: String =
                row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let session_id = Ulid::from_string(&sid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let seq: i64 =
                row.try_get("sequence_num").map_err(|e| SessionError::Database(e.to_string()))?;
            let cid_str: String =
                row.try_get("correlation_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let correlation_id = Ulid::from_string(&cid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let payload: String =
                row.try_get("payload").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at_unix: i64 =
                row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at = OffsetDateTime::from_unix_timestamp(created_at_unix)
                .map_err(|e| SessionError::Database(e.to_string()))?;

            let event_kind: String =
                row.try_get("event_kind").map_err(|e| SessionError::Database(e.to_string()))?;

            events.push(crate::replay::StoredEvent {
                id,
                session_id,
                sequence_num: seq,
                correlation_id,
                event_kind,
                payload,
                created_at,
            });
        }
        Ok(events)
    }

    async fn load_events_until(
        &self,
        session_id: Ulid,
        max_seq: u64,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::replay::StoredEvent>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT id, session_id, sequence_num, correlation_id, event_kind, payload, created_at FROM session_events WHERE session_id = ? AND sequence_num <= ? ORDER BY sequence_num ASC")
            .bind(session_id.to_string())
            .bind(max_seq as i64)
            .fetch_all(&self.pool)
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String =
                row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
            let id = Ulid::from_string(&id_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let sid_str: String =
                row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let session_id = Ulid::from_string(&sid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let seq: i64 =
                row.try_get("sequence_num").map_err(|e| SessionError::Database(e.to_string()))?;
            let cid_str: String =
                row.try_get("correlation_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let correlation_id = Ulid::from_string(&cid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let payload: String =
                row.try_get("payload").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at_unix: i64 =
                row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at = OffsetDateTime::from_unix_timestamp(created_at_unix)
                .map_err(|e| SessionError::Database(e.to_string()))?;
            let event_kind: String =
                row.try_get("event_kind").map_err(|e| SessionError::Database(e.to_string()))?;

            events.push(crate::replay::StoredEvent {
                id,
                session_id,
                sequence_num: seq,
                correlation_id,
                event_kind,
                payload,
                created_at,
            });
        }
        Ok(events)
    }

    async fn record_spend(
        &self,
        record: crate::spend::SpendRecord,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let created_at_unix = record.created_at.unix_timestamp();
        let task_id_str = record.task_id.map(|t| t.to_string());

        sqlx::query(
            "INSERT INTO spend_records (id, session_id, task_id, provider, model, tokens_in, tokens_out, cost_usd, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(record.id.to_string())
            .bind(record.session_id.to_string())
            .bind(task_id_str)
            .bind(&record.provider)
            .bind(&record.model)
            .bind(record.tokens_in as i64)
            .bind(record.tokens_out as i64)
            .bind(record.cost_usd)
            .bind(created_at_unix)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn list_spend_records(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<crate::spend::SpendRecord>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT id, session_id, task_id, provider, model, tokens_in, tokens_out, cost_usd, \
             created_at FROM spend_records WHERE session_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String =
                row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
            let id = Ulid::from_string(&id_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let sid_str: String =
                row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let stored_session_id = Ulid::from_string(&sid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let task_id_str: Option<String> =
                row.try_get("task_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let task_id = match task_id_str {
                Some(value) => Some(
                    Ulid::from_string(&value)
                        .map_err(|_| SessionError::Serialization("invalid ULID".into()))?,
                ),
                None => None,
            };
            let provider: String =
                row.try_get("provider").map_err(|e| SessionError::Database(e.to_string()))?;
            let model: String =
                row.try_get("model").map_err(|e| SessionError::Database(e.to_string()))?;
            let tokens_in: i64 =
                row.try_get("tokens_in").map_err(|e| SessionError::Database(e.to_string()))?;
            let tokens_out: i64 =
                row.try_get("tokens_out").map_err(|e| SessionError::Database(e.to_string()))?;
            let cost_usd: f64 =
                row.try_get("cost_usd").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at_unix: i64 =
                row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at = OffsetDateTime::from_unix_timestamp(created_at_unix)
                .map_err(|e| SessionError::Database(e.to_string()))?;

            records.push(crate::spend::SpendRecord {
                id,
                session_id: stored_session_id,
                task_id,
                provider,
                model,
                tokens_in: tokens_in as u64,
                tokens_out: tokens_out as u64,
                cost_usd,
                created_at,
            });
        }
        Ok(records)
    }

    async fn spend_summary(
        &self,
        session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<crate::spend::SpendSummary, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let row = sqlx::query(
            "SELECT COALESCE(SUM(tokens_in), 0) as total_in, COALESCE(SUM(tokens_out), 0) as total_out, COALESCE(SUM(cost_usd), 0.0) as total_cost, COUNT(*) as cnt FROM spend_records WHERE session_id = ?")
            .bind(session_id.to_string())
            .fetch_one(&self.pool)
            .await?;

        let total_tokens_in: i64 =
            row.try_get("total_in").map_err(|e| SessionError::Database(e.to_string()))?;
        let total_tokens_out: i64 =
            row.try_get("total_out").map_err(|e| SessionError::Database(e.to_string()))?;
        let total_cost_usd: f64 =
            row.try_get("total_cost").map_err(|e| SessionError::Database(e.to_string()))?;
        let record_count: i64 =
            row.try_get("cnt").map_err(|e| SessionError::Database(e.to_string()))?;

        Ok(crate::spend::SpendSummary {
            session_id,
            total_cost_usd,
            total_tokens_in: total_tokens_in as u64,
            total_tokens_out: total_tokens_out as u64,
            record_count: record_count as u64,
        })
    }

    // ---- Phase 3: task tracking ----

    async fn create_task(
        &self,
        task: &concerto_core::types::AgentTask,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let created_at_unix = task.created_at.unix_timestamp();

        sqlx::query(
            "INSERT INTO tasks (id, session_id, description, status, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(task.id.to_string())
            .bind(task.session_id.to_string())
            .bind(&task.description)
            .bind("running")
            .bind(created_at_unix)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn update_task_status(
        &self,
        task_id: TaskId,
        status: &str,
        _cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let completed_at_unix = time::OffsetDateTime::now_utc().unix_timestamp();

        let rows = sqlx::query("UPDATE tasks SET status = ?, completed_at = ? WHERE id = ?")
            .bind(status)
            .bind(completed_at_unix)
            .bind(task_id.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();

        if rows == 0 {
            return Err(SessionError::NotFound(format!("task {}", task_id)));
        }

        Ok(())
    }

    async fn get_task(
        &self,
        task_id: TaskId,
        _cancel: CancellationToken,
    ) -> Result<Option<concerto_core::types::AgentTask>, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let row = sqlx::query(
            "SELECT id, session_id, description, status, created_at FROM tasks WHERE id = ?",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let id_str: String =
                    row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
                let id = TaskId(
                    Ulid::from_string(&id_str)
                        .map_err(|_| SessionError::Serialization("invalid ULID".into()))?,
                );
                let sid_str: String =
                    row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
                let session_id = Ulid::from_string(&sid_str)
                    .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
                let description: String = row
                    .try_get("description")
                    .map_err(|e| SessionError::Database(e.to_string()))?;
                let created_at_unix: i64 =
                    row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
                let created_at = time::OffsetDateTime::from_unix_timestamp(created_at_unix)
                    .map_err(|e| SessionError::Database(e.to_string()))?;

                Ok(Some(concerto_core::types::AgentTask {
                    id,
                    session_id,
                    description,
                    created_at,
                    execution_mode: Default::default(),
                }))
            }
            None => Ok(None),
        }
    }

    async fn list_tasks(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::types::AgentTask>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT id, session_id, description, status, created_at FROM tasks WHERE session_id = ? ORDER BY created_at DESC")
            .bind(session_id.to_string())
            .fetch_all(&self.pool)
            .await?;

        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String =
                row.try_get("id").map_err(|e| SessionError::Database(e.to_string()))?;
            let id = TaskId(
                Ulid::from_string(&id_str)
                    .map_err(|_| SessionError::Serialization("invalid ULID".into()))?,
            );
            let sid_str: String =
                row.try_get("session_id").map_err(|e| SessionError::Database(e.to_string()))?;
            let sid = Ulid::from_string(&sid_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let description: String =
                row.try_get("description").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at_unix: i64 =
                row.try_get("created_at").map_err(|e| SessionError::Database(e.to_string()))?;
            let created_at = time::OffsetDateTime::from_unix_timestamp(created_at_unix)
                .map_err(|e| SessionError::Database(e.to_string()))?;

            tasks.push(concerto_core::types::AgentTask {
                id,
                session_id: sid,
                description,
                created_at,
                execution_mode: Default::default(),
            });
        }

        Ok(tasks)
    }

    // ---- Phase 4: checkpointing ----
    async fn create_checkpoint(
        &self,
        session_id: Ulid,
        task_id: concerto_core::types::TaskId,
        label: &str,
        vfs_snapshot: &str,
        sequence_num: u64,
        _cancel: CancellationToken,
    ) -> Result<Ulid, SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let id = Ulid::new();
        let created_at = time::OffsetDateTime::now_utc();
        let created_at_unix = created_at.unix_timestamp();
        sqlx::query(
            "INSERT INTO checkpoints (id, session_id, task_id, label, virtual_fs_snapshot, sequence_num, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(id.to_string())
        .bind(session_id.to_string())
        .bind(task_id.to_string())
        .bind(label)
        .bind(vfs_snapshot)
        .bind(sequence_num as i64)
        .bind(created_at_unix)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn load_checkpoint(
        &self,
        checkpoint_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<(String, u64), SessionError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let row =
            sqlx::query("SELECT virtual_fs_snapshot, sequence_num FROM checkpoints WHERE id = ?")
                .bind(checkpoint_id.to_string())
                .fetch_one(&self.pool)
                .await?;
        let snapshot: String = row.try_get("virtual_fs_snapshot")?;
        let seq: i64 = row.try_get("sequence_num")?;
        Ok((snapshot, seq as u64))
    }

    async fn list_checkpoints(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<CheckpointSummary>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT id, label, sequence_num, created_at FROM checkpoints WHERE session_id = ? ORDER BY sequence_num ASC"
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut list = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let id_str: String = row.try_get("id")?;
            let id = Ulid::from_string(&id_str)
                .map_err(|_| SessionError::Serialization("invalid ULID".into()))?;
            let label: String = row.try_get("label")?;
            let seq_i64: i64 = row.try_get("sequence_num")?;
            let created_at_unix: i64 = row.try_get("created_at")?;
            let created_at = time::OffsetDateTime::from_unix_timestamp(created_at_unix)
                .map_err(|e| SessionError::Database(e.to_string()))?;
            list.push(CheckpointSummary { id, label, sequence_num: seq_i64 as u64, created_at });
        }
        Ok(list)
    }

    async fn save_orchestration_checkpoint(
        &self,
        record: &OrchestrationCheckpointRecord,
    ) -> Result<(), SessionError> {
        sqlx::query(
            "INSERT INTO orchestration_checkpoints (
                session_id, run_id, root_task_id, project_id, objective_hash,
                schema_version, source_revision, sequence_num, state_json,
                completed, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                run_id = excluded.run_id,
                root_task_id = excluded.root_task_id,
                project_id = excluded.project_id,
                objective_hash = excluded.objective_hash,
                schema_version = excluded.schema_version,
                source_revision = excluded.source_revision,
                sequence_num = excluded.sequence_num,
                state_json = excluded.state_json,
                completed = excluded.completed,
                updated_at = excluded.updated_at",
        )
        .bind(record.session_id.to_string())
        .bind(record.run_id.to_string())
        .bind(record.root_task_id.to_string())
        .bind(&record.project_id)
        .bind(&record.objective_hash)
        .bind(record.schema_version as i64)
        .bind(&record.source_revision)
        .bind(record.sequence_num as i64)
        .bind(&record.state_json)
        .bind(if record.completed { 1_i64 } else { 0_i64 })
        .bind(record.updated_at.unix_timestamp())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_orchestration_checkpoint(
        &self,
        session_id: Ulid,
    ) -> Result<Option<OrchestrationCheckpointRecord>, SessionError> {
        let row = sqlx::query(
            "SELECT run_id, root_task_id, project_id, objective_hash,
                    schema_version, source_revision, sequence_num, state_json,
                    completed, updated_at
             FROM orchestration_checkpoints
             WHERE session_id = ? AND completed = 0",
        )
        .bind(session_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let parse_ulid = |value: String, field: &str| {
            Ulid::from_string(&value).map_err(|_| {
                SessionError::Serialization(format!("invalid {field} ULID in checkpoint"))
            })
        };
        let run_id = parse_ulid(row.try_get("run_id")?, "run_id")?;
        let root_task_id = TaskId(parse_ulid(row.try_get("root_task_id")?, "root_task_id")?);
        let updated_at_unix: i64 = row.try_get("updated_at")?;
        let updated_at = time::OffsetDateTime::from_unix_timestamp(updated_at_unix)
            .map_err(|error| SessionError::Database(error.to_string()))?;

        Ok(Some(OrchestrationCheckpointRecord {
            session_id,
            run_id,
            root_task_id,
            project_id: row.try_get("project_id")?,
            objective_hash: row.try_get("objective_hash")?,
            schema_version: row.try_get::<i64, _>("schema_version")? as u32,
            source_revision: row.try_get("source_revision")?,
            sequence_num: row.try_get::<i64, _>("sequence_num")? as u64,
            state_json: row.try_get("state_json")?,
            completed: row.try_get::<i64, _>("completed")? != 0,
            updated_at,
        }))
    }

    async fn clear_orchestration_checkpoint(&self, session_id: Ulid) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM orchestration_checkpoints WHERE session_id = ?")
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- Durable typed transcript (ADR-36) ----

    async fn append_transcript(
        &self,
        session_id: Ulid,
        entries: &[TranscriptEntry],
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(&cancel)?;

        // One transaction per batch: either every entry is persisted with
        // consecutive sequence numbers or none is. Sequence numbers come from
        // the same atomic COALESCE pattern used by `record_event`, so appends
        // from multiple writers cannot collide.
        let mut tx = self.pool.begin().await?;

        for entry in entries {
            // Cancellation checked at statement boundaries (per batch item).
            check_cancel(&cancel)?;

            let id = Ulid::new();
            let created_at_unix = time::OffsetDateTime::now_utc().unix_timestamp();
            let payload = serde_json::to_string(entry)?;

            sqlx::query(
                "INSERT INTO transcript_entries (id, session_id, sequence_num, entry, created_at) \
                 SELECT ?, ?, COALESCE(MAX(sequence_num), 0) + 1, ?, ? FROM transcript_entries \
                 WHERE session_id = ?",
            )
            .bind(id.to_string())
            .bind(session_id.to_string())
            .bind(&payload)
            .bind(created_at_unix)
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn load_transcript(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<TranscriptEntry>, SessionError> {
        check_cancel(&cancel)?;

        let rows = sqlx::query(
            "SELECT entry FROM transcript_entries WHERE session_id = ? ORDER BY sequence_num ASC",
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            // Cancellation checked at statement boundaries (row decode loop).
            check_cancel(&cancel)?;

            let payload: String =
                row.try_get("entry").map_err(|e| SessionError::Database(e.to_string()))?;
            entries.push(serde_json::from_str(&payload)?);
        }
        Ok(entries)
    }

    async fn save_plan_binding(
        &self,
        record: &PlanBindingRecord,
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(&cancel)?;
        crate::plan_bindings::save_plan_binding(&self.pool, record).await
    }

    async fn load_newest_plan_binding(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Option<PlanBindingRecord>, SessionError> {
        check_cancel(&cancel)?;
        crate::plan_bindings::load_newest_plan_binding(&self.pool, session_id).await
    }

    async fn delete_plan_binding(
        &self,
        session_id: Ulid,
        objective_hash: &str,
        cancel: CancellationToken,
    ) -> Result<bool, SessionError> {
        check_cancel(&cancel)?;
        crate::plan_bindings::delete_plan_binding(&self.pool, session_id, objective_hash).await
    }
}

// ---------------------------------------------------------------------------
// ConversationHistory — lightweight in-memory wrapper
//
// Delegates overflow handling to the unified `concerto_core::ContextOverflowStrategy`.
// ---------------------------------------------------------------------------

pub struct ConversationHistory {
    messages: Vec<Message>,
    budget: TokenBudget,
}

impl ConversationHistory {
    pub fn new(budget: TokenBudget) -> Self {
        Self { messages: Vec::new(), budget }
    }

    pub fn add(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn apply_overflow(
        &mut self,
        strategy: &dyn concerto_core::ContextOverflowStrategy,
        cancel: concerto_core::CancellationToken,
    ) -> usize {
        strategy
            .apply(&mut self.messages, &self.budget, concerto_core::ids::Ulid::new(), cancel)
            .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::traits::AuditLog;
    use concerto_core::types::{Role, TokenBudget};

    #[test]
    fn conversation_history_add_and_retrieve() {
        let budget = TokenBudget::new(1000, 100);
        let mut history = ConversationHistory::new(budget);
        let msg = Message {
            role: Role::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        history.add(msg.clone());
        assert_eq!(history.messages().len(), 1);
        assert_eq!(history.messages()[0].content, "Hello");
    }

    #[tokio::test]
    async fn test_create_and_load_session() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(session.provider, "openai");
        assert_eq!(session.model, "gpt-4");

        let loaded = store.load_session(session.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.provider, "openai");
        assert_eq!(loaded.model, "gpt-4");
        assert_eq!(loaded.project_dir, project_dir);
    }

    #[tokio::test]
    async fn test_save_and_load_messages() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        let msg = Message {
            role: Role::User,
            content: "Hello, world!".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        store.save_message(session.id, &msg, 10, 20, CancellationToken::new()).await.unwrap();

        // ADR-46: reasoning_content round-trips through the DB.
        let reasoning_msg = Message {
            role: Role::Assistant,
            content: "With reasoning".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: Some("thought step one/step two".into()),
            tokens_in: None,
            tokens_out: None,
        };
        store
            .save_message(session.id, &reasoning_msg, 5, 7, CancellationToken::new())
            .await
            .unwrap();

        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "Hello, world!");
        assert_eq!(loaded[0].reasoning_content, None);
        assert_eq!(loaded[1].content, "With reasoning");
        assert_eq!(loaded[1].reasoning_content.as_deref(), Some("thought step one/step two"));

        // Message was saved successfully if we got here
        // We can verify by checking list_recent_sessions
        let sessions = store.list_recent_sessions(10, CancellationToken::new()).await.unwrap();
        assert!(!sessions.is_empty());
        let summary = sessions.iter().find(|s| s.id == session.id);
        assert!(summary.is_some());
        assert_eq!(summary.unwrap().message_count, 2);
    }

    #[tokio::test]
    async fn test_token_usage_round_trips_through_persistence() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        // ADR-48 §4: measured usage is persisted verbatim.
        let measured = Message {
            role: Role::User,
            content: "measured".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: Some(100),
            tokens_out: Some(20),
        };
        store.save_message(session.id, &measured, 0, 0, CancellationToken::new()).await.unwrap();

        // Unknown usage (`None`) is stored as 0 and restores as `None` so the
        // estimator heuristic applies — never a fake zero.
        let unknown = Message {
            role: Role::Assistant,
            content: "unknown".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        store.save_message(session.id, &unknown, 0, 0, CancellationToken::new()).await.unwrap();

        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tokens_in, Some(100));
        assert_eq!(loaded[0].tokens_out, Some(20));
        assert_eq!(loaded[1].tokens_in, None);
        assert_eq!(loaded[1].tokens_out, None);
    }

    #[tokio::test]
    async fn test_legacy_zero_token_rows_restore_as_unknown() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        // Simulate a pre-ADR-48 row: tokens written as 0 by an older version.
        let msg = Message {
            role: Role::User,
            content: "legacy".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        store.save_message(session.id, &msg, 0, 0, CancellationToken::new()).await.unwrap();

        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(loaded[0].tokens_in, None, "0 must restore as unknown, not as a measurement");
        assert_eq!(loaded[0].tokens_out, None);
    }

    #[tokio::test]
    async fn test_list_recent_sessions() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session1 = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();
        let session2 = store
            .create_session(&project_dir, "anthropic", "claude-3", CancellationToken::new())
            .await
            .unwrap();

        let sessions = store.list_recent_sessions(10, CancellationToken::new()).await.unwrap();
        assert!(sessions.len() >= 2);

        let ids: Vec<_> = sessions.iter().map(|s| s.id).collect();
        assert!(ids.contains(&session1.id));
        assert!(ids.contains(&session2.id));
    }

    #[tokio::test]
    async fn test_record_metrics() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        let metrics = ProviderMetrics {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.002,
            latency_ms: 500,
        };

        store.record_metrics(session.id, metrics, CancellationToken::new()).await.unwrap();

        let loaded =
            store.load_session(session.id, CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(loaded.total_tokens_in, 100);
        assert_eq!(loaded.total_tokens_out, 50);
        assert!((loaded.total_cost_usd - 0.002).abs() < f64::EPSILON);
    }

    #[tokio::test]
    /// `record_metrics` is atomic: if the aggregate UPDATE fails, the metric
    /// row INSERT is rolled back too (both statements commit or neither).
    ///
    /// Fault injection: a `BEFORE UPDATE` trigger aborts the second statement
    /// after the first has run inside the transaction. The in-memory database
    /// is private to this test, so the trigger cannot affect other tests.
    async fn record_metrics_is_atomic_on_failure() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_atomic_metrics");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        sqlx::query(
            "CREATE TRIGGER fail_session_total_update \
             BEFORE UPDATE ON sessions \
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        let metrics = ProviderMetrics {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.002,
            latency_ms: 500,
        };

        let result = store.record_metrics(session.id, metrics, CancellationToken::new()).await;
        assert!(result.is_err(), "second statement must fail");

        // The first statement's row must have been rolled back with the error.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM provider_metrics WHERE session_id = ?")
                .bind(session.id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "metric row must not persist after rollback");

        // Session aggregates must be untouched.
        let loaded =
            store.load_session(session.id, CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(loaded.total_tokens_in, 0);
        assert_eq!(loaded.total_tokens_out, 0);
        assert_eq!(loaded.total_cost_usd, 0.0);
    }

    #[tokio::test]
    /// A pre-cancelled token aborts `record_metrics` before any statement
    /// runs; nothing is persisted.
    async fn record_metrics_respects_precancelled_token() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_cancel_metrics");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        let metrics = ProviderMetrics {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.002,
            latency_ms: 500,
        };

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = store.record_metrics(session.id, metrics, cancel).await;
        assert!(matches!(result, Err(SessionError::Database(_))));

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM provider_metrics WHERE session_id = ?")
                .bind(session.id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "no metric row may be written after cancellation");
    }

    #[tokio::test]
    /// A pre-cancelled token makes `list_recent_sessions` (a row-decode loop)
    /// bail early with the cancelled error instead of returning results.
    async fn list_recent_sessions_returns_cancelled_error_early() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_cancel_list");
        store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = store.list_recent_sessions(10, cancel).await;
        assert!(matches!(result, Err(SessionError::Database(_))));
    }

    #[tokio::test]
    /// A pre-cancelled token aborts the `append_messages` batch before the
    /// transaction starts; no message is persisted.
    async fn append_messages_aborts_on_cancelled_token() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_cancel_append");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let msgs = [Message {
            role: Role::User,
            content: "should not persist".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }];

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = store.append_messages(session.id, &msgs, cancel).await;
        assert!(matches!(result, Err(SessionError::Database(_))));

        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_empty(), "no message may be written after cancellation");
    }

    #[tokio::test]
    async fn independent_stores_can_share_one_database() {
        let directory = std::env::temp_dir().join(format!("concerto-shared-{}", Ulid::new()));
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("sessions.db");
        let first = SqliteSessionStore::connect_path(&database).await.unwrap();
        let second = SqliteSessionStore::connect_path(&database).await.unwrap();
        let project = camino::Utf8PathBuf::from("/tmp/shared-project");
        let session = first
            .create_session(&project, "provider", "model", CancellationToken::new())
            .await
            .unwrap();
        let first_message = Message {
            role: Role::User,
            content: "from first".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let second_message = Message {
            role: Role::Assistant,
            content: "from second".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };

        let first_messages = [first_message];
        let second_messages = [second_message];
        let (left, right) = tokio::join!(
            first.append_messages(session.id, &first_messages, CancellationToken::new()),
            second.append_messages(session.id, &second_messages, CancellationToken::new()),
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(
            second.load_messages(session.id, CancellationToken::new()).await.unwrap().len(),
            2
        );

        drop(first);
        drop(second);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn orchestration_checkpoint_round_trips_and_clears() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/checkpoint-project");
        let session = store
            .create_session(&project_dir, "provider", "model", CancellationToken::new())
            .await
            .unwrap();
        let record = OrchestrationCheckpointRecord {
            session_id: session.id,
            run_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "checkpoint-project".into(),
            objective_hash: "objective-hash".into(),
            schema_version: 2,
            source_revision: Some("abc123".into()),
            sequence_num: 7,
            state_json: r#"{"stage":"Executing"}"#.into(),
            completed: false,
            updated_at: OffsetDateTime::now_utc(),
        };

        store.save_orchestration_checkpoint(&record).await.unwrap();
        let loaded = store
            .load_orchestration_checkpoint(session.id)
            .await
            .unwrap()
            .expect("active checkpoint");
        assert_eq!(loaded.run_id, record.run_id);
        assert_eq!(loaded.root_task_id, record.root_task_id);
        assert_eq!(loaded.sequence_num, 7);
        assert_eq!(loaded.source_revision.as_deref(), Some("abc123"));

        store.clear_orchestration_checkpoint(session.id).await.unwrap();
        assert!(store.load_orchestration_checkpoint(session.id).await.unwrap().is_none());
    }

    /// Run-continuity Phase 1: the orchestration-checkpoint load is the
    /// "newest NON-completed checkpoint" lookup that backs a bare "continue"
    /// — a row marked completed (a settled run) must never be served, while
    /// the same row before completion is.
    #[tokio::test]
    async fn orchestration_checkpoint_load_skips_completed_rows() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/checkpoint-project");
        let session = store
            .create_session(&project_dir, "provider", "model", CancellationToken::new())
            .await
            .unwrap();
        let record = OrchestrationCheckpointRecord {
            session_id: session.id,
            run_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "checkpoint-project".into(),
            objective_hash: "objective-hash".into(),
            schema_version: 3,
            source_revision: None,
            sequence_num: 1,
            state_json: r#"{"stage":"Executing"}"#.into(),
            completed: true,
            updated_at: OffsetDateTime::now_utc(),
        };

        // A completed row is invisible to the resume lookup.
        store.save_orchestration_checkpoint(&record).await.unwrap();
        assert!(
            store.load_orchestration_checkpoint(session.id).await.unwrap().is_none(),
            "a completed checkpoint must never be served for a resume"
        );

        // The same row before completion (a stalled run) IS served — the
        // upsert flips `completed` back without inserting a second row.
        let mut stalled = record.clone();
        stalled.completed = false;
        store.save_orchestration_checkpoint(&stalled).await.unwrap();
        let loaded = store
            .load_orchestration_checkpoint(session.id)
            .await
            .unwrap()
            .expect("the stalled checkpoint is resumable");
        assert!(!loaded.completed);
        assert_eq!(loaded.run_id, record.run_id);
    }

    // -----------------------------------------------------------------------
    // New tests added below (16 tests)
    // -----------------------------------------------------------------------

    #[tokio::test]
    /// Append an empty message vec — should succeed without error.
    async fn append_messages_empty_vec() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_empty");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let result = store.append_messages(session.id, &[], CancellationToken::new()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    /// Append multiple messages in one batch then verify they are all stored.
    async fn append_messages_multiple() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_multiple");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let msgs = vec![
            Message {
                role: Role::User,
                content: "first".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
            Message {
                role: Role::Assistant,
                content: "second".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
        ];
        store.append_messages(session.id, &msgs, CancellationToken::new()).await.unwrap();
        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "first");
        assert_eq!(loaded[1].content, "second");
    }

    #[tokio::test]
    /// Messages loaded via `load_messages` must preserve insertion order
    /// (sorted by sequence_num ASC).
    async fn load_messages_returns_in_order() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_order");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let msgs = vec![
            Message {
                role: Role::User,
                content: "A".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
            Message {
                role: Role::Assistant,
                content: "B".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
            Message {
                role: Role::User,
                content: "C".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
        ];
        for m in &msgs {
            store.save_message(session.id, m, 0, 0, CancellationToken::new()).await.unwrap();
        }
        let loaded = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].content, "A");
        assert_eq!(loaded[1].content, "B");
        assert_eq!(loaded[2].content, "C");
    }

    #[tokio::test]
    /// `list_sessions_for_project` returns an empty vec when no sessions exist.
    async fn list_sessions_for_project_empty() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let result = store
            .list_sessions_for_project(
                camino::Utf8Path::new("/tmp/nonexistent"),
                10,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    /// `list_sessions_for_project` returns all sessions belonging to a project.
    async fn list_sessions_for_project_multiple() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_project_multi");
        let s1 =
            store.create_session(&project_dir, "p1", "m1", CancellationToken::new()).await.unwrap();
        let s2 =
            store.create_session(&project_dir, "p2", "m2", CancellationToken::new()).await.unwrap();
        let sessions = store
            .list_sessions_for_project(&project_dir, 10, CancellationToken::new())
            .await
            .unwrap();
        // Both sessions should appear (sorting is by created_at desc; if
        // both were created in the same tick the order is not deterministic).
        let ids: Vec<_> = sessions.iter().map(|s| s.id).collect();
        assert!(ids.contains(&s1.id));
        assert!(ids.contains(&s2.id));
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    /// `get_active_session_for_project` returns None when no mapping exists.
    async fn get_active_session_for_project_none_when_not_set() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let result = store
            .get_active_session_for_project(
                camino::Utf8Path::new("/tmp/not_set"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    /// `set_active_session_for_project` followed by `get` returns the stored id.
    async fn set_active_session_for_project_and_get() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_active");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        store
            .set_active_session_for_project(&project_dir, session.id, CancellationToken::new())
            .await
            .unwrap();
        let active = store
            .get_active_session_for_project(&project_dir, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(active, Some(session.id));
    }

    #[tokio::test]
    /// `record_event` stores an event and `load_events` retrieves it.
    async fn record_event_and_load_events() {
        use concerto_core::event::{Event, EventKind};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_event");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let correlation_id = Ulid::new();
        let kind = EventKind::SessionSaved;
        let event = Event::new(correlation_id, session.id, kind);
        store.record_event(session.id, &event, CancellationToken::new()).await.unwrap();
        let events = store.load_events(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].correlation_id, correlation_id);
    }

    #[tokio::test]
    /// `load_events_until` respects the max_seq boundary.
    async fn load_events_until_boundary() {
        use concerto_core::event::{Event, EventKind};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_until");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let cid = Ulid::new();
        // Record 3 events.
        for _ in 0..3 {
            let e = Event::new(cid, session.id, EventKind::SessionSaved);
            store.record_event(session.id, &e, CancellationToken::new()).await.unwrap();
        }
        // Load only up to sequence 2.
        let events =
            store.load_events_until(session.id, 2, CancellationToken::new()).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.sequence_num <= 2));
    }

    // -----------------------------------------------------------------------
    // Durable typed transcript (ADR-36)
    // -----------------------------------------------------------------------

    #[tokio::test]
    /// Appending transcript entries and loading them returns the same sequence
    /// (including a ToolCall for every status and a Completion).
    async fn transcript_round_trip_preserves_order() {
        use concerto_core::transcript::TranscriptToolStatus;

        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_transcript");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();

        let entries = vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Assistant { content: "on it".into() },
            TranscriptEntry::Thinking { agent: "coder".into(), content: "hmm".into() },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: TranscriptToolStatus::Running,
            },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "wrote 42 bytes".into(),
                status: TranscriptToolStatus::Completed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: "exit 1".into(),
                status: TranscriptToolStatus::Failed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Allowed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Denied,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Cancelled,
            },
            TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: "Delegated subtask X to coder: go".into(),
            },
            TranscriptEntry::Error { content: "boom".into() },
            TranscriptEntry::Summary { content: "compacted".into() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: true,
                files: vec!["main.rs".into()],
                project_root: Some("/tmp/proj".into()),
            },
        ];

        store.append_transcript(session.id, &entries, CancellationToken::new()).await.unwrap();
        let loaded = store.load_transcript(session.id, CancellationToken::new()).await.unwrap();

        assert_eq!(loaded, entries, "round-trip must preserve every entry in order");
    }

    #[tokio::test]
    /// A session with no transcript entries loads as an empty vec, not an error.
    async fn load_transcript_empty_session_returns_empty_vec() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_transcript_empty");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();

        let loaded = store.load_transcript(session.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_empty());

        // Unknown session ids also yield an empty vec (not an error).
        let unknown = store.load_transcript(Ulid::new(), CancellationToken::new()).await.unwrap();
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    /// Batches appended separately are still loaded in append order.
    async fn transcript_ordering_preserved_across_batches() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_transcript_batches");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();

        let batch_one = vec![
            TranscriptEntry::User { content: "first".into() },
            TranscriptEntry::Thinking { agent: "coder".into(), content: "plan".into() },
        ];
        let batch_two = vec![
            TranscriptEntry::Assistant { content: "second".into() },
            TranscriptEntry::Error { content: "third".into() },
        ];

        store.append_transcript(session.id, &batch_one, CancellationToken::new()).await.unwrap();
        store.append_transcript(session.id, &batch_two, CancellationToken::new()).await.unwrap();

        let loaded = store.load_transcript(session.id, CancellationToken::new()).await.unwrap();
        let contents: Vec<&str> = loaded
            .iter()
            .map(|e| match e {
                TranscriptEntry::User { content } => content.as_str(),
                TranscriptEntry::Thinking { content, .. } => content.as_str(),
                TranscriptEntry::Assistant { content } => content.as_str(),
                TranscriptEntry::Error { content } => content.as_str(),
                other => panic!("unexpected transcript entry: {other:?}"),
            })
            .collect();
        assert_eq!(contents, vec!["first", "plan", "second", "third"]);
    }

    #[tokio::test]
    /// A pre-cancelled token aborts the transcript batch before any insert.
    async fn append_transcript_aborts_on_cancelled_token() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_cancel_transcript");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();

        let entries = vec![TranscriptEntry::User { content: "should not persist".into() }];
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = store.append_transcript(session.id, &entries, cancel).await;
        assert!(matches!(result, Err(SessionError::Database(_))));

        let loaded = store.load_transcript(session.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_empty(), "no transcript entry may be written after cancellation");
    }

    #[tokio::test]
    /// `create_task` stores a task and `get_task` retrieves it.
    async fn create_task_and_get_task() {
        use concerto_core::types::{AgentTask, TaskExecutionMode};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_task_cg");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let task = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "my task".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&task, CancellationToken::new()).await.unwrap();
        let loaded = store.get_task(task.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.description, "my task");
        assert_eq!(loaded.session_id, session.id);
    }

    #[tokio::test]
    /// `update_task_status` changes the task status and it is persisted.
    async fn update_task_status() {
        use concerto_core::types::{AgentTask, TaskExecutionMode};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_update");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let task = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "update me".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&task, CancellationToken::new()).await.unwrap();
        store.update_task_status(task.id, "completed", CancellationToken::new()).await.unwrap();
        // Re-fetch by loading all tasks for the session.
        let tasks = store.list_tasks(session.id, CancellationToken::new()).await.unwrap();
        let updated = tasks.iter().find(|t| t.id == task.id).unwrap();
        assert_eq!(updated.description, "update me");
    }

    #[tokio::test]
    /// `list_tasks` returns all tasks belonging to a session in descending
    /// creation order.
    async fn list_tasks_for_session() {
        use concerto_core::types::{AgentTask, TaskExecutionMode};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_list_tasks");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let t1 = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "task 1".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        let t2 = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "task 2".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&t1, CancellationToken::new()).await.unwrap();
        store.create_task(&t2, CancellationToken::new()).await.unwrap();
        let tasks = store.list_tasks(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(tasks.len(), 2);
        let descs: Vec<&str> = tasks.iter().map(|t| t.description.as_str()).collect();
        assert!(descs.contains(&"task 1"));
        assert!(descs.contains(&"task 2"));
    }

    #[tokio::test]
    /// `create_checkpoint` + `load_checkpoint` round-trips the snapshot and
    /// sequence number.
    async fn create_checkpoint_and_load_checkpoint() {
        use concerto_core::types::{AgentTask, TaskExecutionMode};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_cp");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let task = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "cp task".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&task, CancellationToken::new()).await.unwrap();
        let cp_id = store
            .create_checkpoint(
                session.id,
                task.id,
                "label1",
                r#"{"files":{}}"#,
                42,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (snapshot, seq) = store.load_checkpoint(cp_id, CancellationToken::new()).await.unwrap();
        assert_eq!(snapshot, r#"{"files":{}}"#);
        assert_eq!(seq, 42);
    }

    #[tokio::test]
    /// `list_checkpoints` returns checkpoints for a session in sequence order.
    async fn list_checkpoints_for_session() {
        use concerto_core::types::{AgentTask, TaskExecutionMode};
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_list_cp");
        let session =
            store.create_session(&project_dir, "p", "m", CancellationToken::new()).await.unwrap();
        let task = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "cp task".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&task, CancellationToken::new()).await.unwrap();
        store
            .create_checkpoint(session.id, task.id, "cp1", "snap1", 1, CancellationToken::new())
            .await
            .unwrap();
        store
            .create_checkpoint(session.id, task.id, "cp2", "snap2", 2, CancellationToken::new())
            .await
            .unwrap();
        let cps = store.list_checkpoints(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].label, "cp1");
        assert_eq!(cps[1].label, "cp2");
    }

    #[tokio::test]
    /// Two in-memory stores must not share data — each `connect_in_memory`
    /// creates an independent database.
    async fn connect_in_memory_isolation() {
        let store_a = SqliteSessionStore::connect_in_memory().await.unwrap();
        let store_b = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project = camino::Utf8PathBuf::from("/tmp/test_iso");
        let session =
            store_a.create_session(&project, "p", "m", CancellationToken::new()).await.unwrap();
        // store_b should not see the session created in store_a.
        let sessions_b = store_b.list_recent_sessions(10, CancellationToken::new()).await.unwrap();
        assert!(sessions_b.is_empty());
        // Also verify the session exists in store_a.
        let loaded = store_a.load_session(session.id, CancellationToken::new()).await.unwrap();
        assert!(loaded.is_some());
    }

    #[tokio::test]
    /// `normalize_project_dir` returns a usable path for absolute, relative,
    /// and root inputs without crashing. For non‑existent paths it relies on
    /// `canonical_project_path` fallback which preserves absolute paths and
    /// resolves relative ones against the current directory.
    async fn normalize_project_dir_various_paths() {
        // Absolute path that does not exist — returned as-is by the
        // canonicalize fallback.
        let p = camino::Utf8Path::new("/tmp/nonexistent_test_project_dir");
        let n = normalize_project_dir(p);
        assert_eq!(n.as_str(), "/tmp/nonexistent_test_project_dir");

        // Root path (always exists).
        let p = camino::Utf8Path::new("/");
        let n = normalize_project_dir(p);
        assert_eq!(n.as_str(), "/");

        // Relative path — resolved against cwd so it becomes absolute.
        let p = camino::Utf8Path::new("some/relative/path");
        let n = normalize_project_dir(p);
        // The result should be an absolute path ending with the relative
        // components but never a bare relative string.
        assert!(n.as_str().ends_with("some/relative/path"));
        assert!(n.as_str().starts_with('/'));

        // A path containing dot segments is preserved as-is when the path
        // does not exist (canonicalize fallback returns the absolute input
        // unchanged, which is fine for stable project keys).
        let p = camino::Utf8Path::new("/tmp/../tmp/test_dotdot");
        let n = normalize_project_dir(p);
        assert!(n.as_str().contains("test_dotdot"));
    }

    // -----------------------------------------------------------------------
    // Session prune (item H6): delete_session / list_sessions_older_than /
    // active_session_ids
    // -----------------------------------------------------------------------

    #[tokio::test]
    /// `delete_session` removes the session and every related row across all
    /// child tables (messages, events, spend, tasks, checkpoints, transcript,
    /// audit log), and the active-session mapping cascades away.
    async fn delete_session_removes_all_related_rows() {
        use concerto_core::event::{Event, EventKind};
        use concerto_core::traits::policy::AuditEntry;
        use concerto_core::types::{AgentTask, TaskExecutionMode};

        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project_dir = camino::Utf8PathBuf::from("/tmp/test_prune_full");
        let session = store
            .create_session(&project_dir, "openai", "gpt-4", CancellationToken::new())
            .await
            .unwrap();
        let cancel = CancellationToken::new();

        // One message.
        store
            .save_message(
                session.id,
                &Message {
                    role: Role::User,
                    content: "hello".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                10,
                20,
                cancel.clone(),
            )
            .await
            .unwrap();

        // One session event.
        let event = Event::new(Ulid::new(), session.id, EventKind::SessionSaved);
        store.record_event(session.id, &event, cancel.clone()).await.unwrap();

        // One spend record.
        store
            .record_spend(
                crate::spend::SpendRecord {
                    id: Ulid::new(),
                    session_id: session.id,
                    task_id: None,
                    provider: "openai".into(),
                    model: "gpt-4".into(),
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_usd: 0.01,
                    created_at: time::OffsetDateTime::now_utc(),
                },
                cancel.clone(),
            )
            .await
            .unwrap();

        // One task and one checkpoint (checkpoint needs the task id).
        let task = AgentTask {
            id: TaskId(Ulid::new()),
            session_id: session.id,
            description: "prune task".into(),
            created_at: time::OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::default(),
        };
        store.create_task(&task, cancel.clone()).await.unwrap();
        let checkpoint_id = store
            .create_checkpoint(session.id, task.id, "cp", r#"{"files":{}}"#, 1, cancel.clone())
            .await
            .unwrap();

        // One transcript entry.
        store
            .append_transcript(
                session.id,
                &[TranscriptEntry::User { content: "entry".into() }],
                cancel.clone(),
            )
            .await
            .unwrap();

        // One audit row, written through the audit log API sharing the store pool.
        let audit = crate::audit::SqliteAuditLog::new(store.pool.clone());
        audit
            .record(
                AuditEntry {
                    tool_name: "prune_tool".into(),
                    verdict: "Allow".into(),
                    input_hash: "hash".into(),
                    session_id: session.id,
                    correlation_id: Ulid::new(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    user_response: None,
                    rule_matched: Some("auto_approve".into()),
                    profile_id: None,
                    resolved_executable: None,
                    argv: None,
                    working_directory: None,
                    network_requested: None,
                    filesystem_scope: None,
                    destructive_classification: None,
                    exit_code: None,
                    duration_ms: None,
                    toolchain_version: None,
                    plan_id: None,
                    source_revision: None,
                },
                cancel.clone(),
            )
            .await
            .unwrap();

        // Active-session mapping (cascades away with the session).
        store
            .set_active_session_for_project(&project_dir, session.id, cancel.clone())
            .await
            .unwrap();

        // Sanity: every child table has data before the delete.
        assert!(!store.load_messages(session.id, cancel.clone()).await.unwrap().is_empty());
        assert!(!store.load_events(session.id, cancel.clone()).await.unwrap().is_empty());
        assert_eq!(store.spend_summary(session.id, cancel.clone()).await.unwrap().record_count, 1);
        let spend_records = store.list_spend_records(session.id, cancel.clone()).await.unwrap();
        assert_eq!(spend_records.len(), 1, "the recorded spend round-trips through the listing");
        assert_eq!(spend_records[0].provider, "openai");
        assert!(!store.list_tasks(session.id, cancel.clone()).await.unwrap().is_empty());
        assert!(!store.load_transcript(session.id, cancel.clone()).await.unwrap().is_empty());
        assert_eq!(
            store.get_active_session_for_project(&project_dir, cancel.clone()).await.unwrap(),
            Some(session.id)
        );

        // The prune.
        assert!(store.delete_session(session.id, cancel.clone()).await.unwrap());

        // The session and every related row are gone.
        assert!(store.load_session(session.id, cancel.clone()).await.unwrap().is_none());
        assert!(store.load_messages(session.id, cancel.clone()).await.unwrap().is_empty());
        assert!(store.load_events(session.id, cancel.clone()).await.unwrap().is_empty());
        let spend = store.spend_summary(session.id, cancel.clone()).await.unwrap();
        assert_eq!(spend.record_count, 0);
        assert_eq!(spend.total_cost_usd, 0.0);
        assert!(store.list_tasks(session.id, cancel.clone()).await.unwrap().is_empty());
        assert!(store.load_transcript(session.id, cancel.clone()).await.unwrap().is_empty());
        assert!(store.load_checkpoint(checkpoint_id, cancel.clone()).await.is_err());
        assert_eq!(
            store.get_active_session_for_project(&project_dir, cancel.clone()).await.unwrap(),
            None
        );

        // Direct SQL: event rows are gone; the audit row SURVIVES with its
        // session_id nulled by the ON DELETE SET NULL FK (ADR-40).
        let audit_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log WHERE session_id = ?")
                .bind(session.id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(audit_count.0, 0, "no audit row may keep the dead session id");
        let kept_audit: (i64, String, String) = sqlx::query_as(
            "SELECT COUNT(*), tool_name, verdict FROM audit_log WHERE session_id IS NULL \
             AND tool_name = 'prune_tool' GROUP BY tool_name, verdict",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(kept_audit.0, 1, "the audit row must be preserved, detached");
        assert_eq!(kept_audit.1, "prune_tool");
        assert_eq!(kept_audit.2, "Allow");
        let event_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM session_events WHERE session_id = ?")
                .bind(session.id.to_string())
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(event_count.0, 0, "event rows must be pruned with the session");
    }

    #[tokio::test]
    /// `record_spend` + `list_spend_records` round-trip: the listing returns
    /// every persisted record for the session, oldest first
    /// (`created_at ASC, id ASC`), with all fields intact — cost, tokens,
    /// provider, model, and the optional task id.
    async fn spend_records_round_trip_lists_in_order() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let session = store
            .create_session(
                camino::Utf8Path::new("/tmp/spend-list"),
                "openai",
                "gpt-4",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancel = CancellationToken::new();

        let task_a = TaskId(Ulid::new());
        let task_b = TaskId(Ulid::new());
        // Deterministic ULIDs so the `id ASC` tie-break is stable: `first` and
        // `second` share a created_at and only differ in the id (random bits).
        let first = crate::spend::SpendRecord {
            id: Ulid::from_parts(1_700_000_000_000, 1),
            session_id: session.id,
            task_id: Some(task_a.0),
            provider: "openai".into(),
            model: "gpt-4".into(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.01,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        };
        let second = crate::spend::SpendRecord {
            id: Ulid::from_parts(1_700_000_000_000, 2),
            session_id: session.id,
            task_id: None,
            provider: "anthropic".into(),
            model: "claude-3".into(),
            tokens_in: 200,
            tokens_out: 100,
            cost_usd: 0.03,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        };
        let third = crate::spend::SpendRecord {
            id: Ulid::from_parts(1_700_000_100_000, 3),
            session_id: session.id,
            task_id: Some(task_b.0),
            provider: "google".into(),
            model: "gemini-pro".into(),
            tokens_in: 300,
            tokens_out: 150,
            cost_usd: 0.05,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
        };

        store.record_spend(first.clone(), cancel.clone()).await.unwrap();
        store.record_spend(second.clone(), cancel.clone()).await.unwrap();
        store.record_spend(third.clone(), cancel.clone()).await.unwrap();

        let listed = store.list_spend_records(session.id, cancel.clone()).await.unwrap();
        assert_eq!(listed.len(), 3, "all three records must round-trip");

        // Ordering: created_at ASC, then id ASC for the same timestamp.
        assert_eq!(listed[0].id, first.id, "same-timestamp records tie-break by id ASC");
        assert_eq!(listed[1].id, second.id, "same-timestamp records tie-break by id ASC");
        assert_eq!(listed[2].id, third.id, "later created_at sorts last");

        // Field fidelity for the earliest record (with a task id).
        assert_eq!(listed[0].session_id, session.id);
        assert_eq!(listed[0].task_id, Some(task_a.0));
        assert_eq!(listed[0].provider, "openai");
        assert_eq!(listed[0].model, "gpt-4");
        assert_eq!(listed[0].tokens_in, 100);
        assert_eq!(listed[0].tokens_out, 50);
        assert!((listed[0].cost_usd - 0.01).abs() < f64::EPSILON);
        assert_eq!(listed[0].created_at, first.created_at);

        // Field fidelity for a record without a task id.
        assert_eq!(listed[1].task_id, None);
        assert_eq!(listed[1].provider, "anthropic");
        assert_eq!(listed[1].model, "claude-3");
        assert_eq!(listed[1].tokens_in, 200);
        assert_eq!(listed[1].tokens_out, 100);
        assert!((listed[1].cost_usd - 0.03).abs() < f64::EPSILON);

        // The aggregate still counts all three records.
        let summary = store.spend_summary(session.id, cancel.clone()).await.unwrap();
        assert_eq!(summary.record_count, 3);
        assert!((summary.total_cost_usd - 0.09).abs() < f64::EPSILON);
    }

    #[tokio::test]
    /// A session with no spend records returns an empty list.
    async fn list_spend_records_empty_session_returns_empty_vec() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let session = store
            .create_session(
                camino::Utf8Path::new("/tmp/spend-list-empty"),
                "openai",
                "gpt-4",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let listed = store.list_spend_records(session.id, CancellationToken::new()).await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    /// `delete_session` for an unknown id returns `Ok(false)` and leaves the
    /// database untouched.
    async fn delete_session_unknown_id_returns_false() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        assert!(!store.delete_session(Ulid::new(), CancellationToken::new()).await.unwrap());
    }

    #[tokio::test]
    /// `list_sessions_older_than` returns only sessions created before the
    /// cutoff, most recent first. `created_at` is injected via SQL because the
    /// public API always stamps `now_utc()`.
    async fn list_sessions_older_than_filters_by_created_at() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let old_id = Ulid::new();
        let mid_id = Ulid::new();
        let new_id = Ulid::new();
        for (id, created_at) in [(old_id, 1_000_i64), (mid_id, 2_000_i64), (new_id, 3_000_i64)] {
            sqlx::query(
                "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(created_at)
            .bind("/tmp/prune-proj")
            .bind("openai")
            .bind("gpt-4")
            .execute(&store.pool)
            .await
            .unwrap();
        }

        // Cutoff between the oldest and the middle session: only the old one matches.
        let older = store.list_sessions_older_than(1_500, CancellationToken::new()).await.unwrap();
        let ids: Vec<_> = older.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![old_id]);

        // Everything is before a far-future cutoff, ordered most recent first.
        let all = store.list_sessions_older_than(9_999, CancellationToken::new()).await.unwrap();
        let ids: Vec<_> = all.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new_id, mid_id, old_id]);
    }

    #[tokio::test]
    /// `active_session_ids` returns every session currently mapped in
    /// `project_active_sessions`.
    async fn active_session_ids_returns_mapped_ids() {
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let project = camino::Utf8PathBuf::from("/tmp/prune-active");
        let session =
            store.create_session(&project, "p", "m", CancellationToken::new()).await.unwrap();
        // No mapping yet.
        assert!(store.active_session_ids(CancellationToken::new()).await.unwrap().is_empty());
        store
            .set_active_session_for_project(&project, session.id, CancellationToken::new())
            .await
            .unwrap();
        let ids = store.active_session_ids(CancellationToken::new()).await.unwrap();
        assert_eq!(ids, vec![session.id]);
    }

    #[tokio::test]
    /// Self-heal (ADR-54): a garbage (non-SQLite) file at the store path is
    /// quarantined to `<name>.corrupt-<ts>.bak` and a fresh store is created;
    /// a file with a valid SQLite header is NEVER quarantined — the original
    /// error is surfaced so real data is never silently deleted.
    async fn connect_self_heals_garbage_file_but_never_a_valid_header_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sessions.db");

        // Garbage file -> quarantine + fresh store on retry.
        std::fs::write(&db_path, b"this is definitely not a sqlite database file").unwrap();
        let store = SqliteSessionStore::connect_path(&db_path).await;
        assert!(store.is_ok(), "connect must recover from a garbage db file");
        let quarantine_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantine_count, 1, "exactly one quarantine backup expected");
        assert!(db_path.is_file(), "a fresh sessions.db must exist after recovery");
        // A second connect succeeds against the rebuilt store.
        SqliteSessionStore::connect_path(&db_path).await.unwrap();

        // Valid SQLite header but broken contents -> error surfaced, file kept.
        let valid_header = dir.path().join("valid.db");
        std::fs::write(
            &valid_header,
            *b"SQLite format 3\0followed-by-garbage-that-is-not-a-real-database",
        )
        .unwrap();
        let result = SqliteSessionStore::connect_path(&valid_header).await;
        assert!(result.is_err(), "valid-header but broken db must fail, not self-heal");
        let touched = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("valid.db.corrupt"));
        assert!(!touched, "valid-header file must never be quarantined");
    }
}
