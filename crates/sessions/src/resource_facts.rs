//! ADR-65 evidence spine: the `resource_facts` derived store (migration 029).
//!
//! `resource_facts` is a **derived projection, not a source of truth**: the
//! authoritative record is the `whiteboard_events` log, from which this table
//! is fully rebuildable forward ([`ResourceFacts::rebuild_from_log`], ADR-64
//! derived-view rule / ADR-65 §4). It is materialized because the hot path
//! (read dedupe, dirty checks) needs indexed per-path lookups.
//!
//! Per path it caches the workspace-generation facts most recently observed by
//! a `ToolExecuted` fact or a `WorkspaceSnapshot`: generation, size, mtime,
//! content hash, and the observation attribution (`last_event_id`,
//! `last_agent_id`, `observed_at`).
//!
//! Invariants this module enforces:
//!
//! - **Observations brand rows clean** (`dirty = 0`). A singleton path row
//!   comes from a snapshot/observe write; `dirty` starts at 1 for any other
//!   path to a row (the migration default), which is the "uncertain" state.
//! - **Dirtying events never rewrite observation columns.** `WriteApplied`,
//!   watcher change hints, and shell/git side effects flip `dirty = 1` only;
//!   the cached observation history survives for audit and reconciliation.
//! - **Rows are scoped per project root (ADR-65 F5c, migration 031).** The row
//!   identity is the `(project_root_hash, path)` pair, so two roots observing
//!   the same relative path never clobber each other. Legacy rows written
//!   before roots were recorded carry the empty hash `''` — preserved for
//!   attribution and rebuild, but never served (the serve path requires a
//!   matching non-empty root hash).
//! - **A missing row is also "uncertain".** `lookup` answers `None`; dirtying
//!   an absent path is a no-op (there is nothing to mark, and the read path
//!   already treats absence as execute-normally).
//! - **Snapshots reconcile, they do not truncate.** Paths listed by a snapshot
//!   that are already observed become/re-main clean with the new observation;
//!   paths that were clean but are *not* in the snapshot (vanished from the
//!   workspace) are **kept and marked dirty** — their observation stays for
//!   attribution while the workspace is explicitly uncertain about them.

use std::collections::HashSet;

use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::check_cancel;
use crate::whiteboard::WhiteboardKind;
use crate::SessionError;

/// ADR-65 §4 / F2b: maximum cached read content size in bytes. This is the
/// **single** shared bound for the whole evidence path: the orchestrator's
/// hashing budget (`tool_facts::MAX_HASH_BYTES`) aliases this constant, so the
/// effective serve bound equals the store cap — files above 64 KiB are never
/// hashable and therefore never content-cached, keeping the cache a small,
/// bounded side table.
pub const CACHE_LIMIT_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Typed payload models
// ---------------------------------------------------------------------------

/// One per-path observation recorded by a `ToolExecuted` fact (ADR-65 §3) or a
/// `WorkspaceSnapshot` inventory entry. `content_hash` is the post-observation
/// hash where the tool/file is hashable (e.g. reads); unhashed entries carry
/// `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedPath {
    pub path: String,
    /// Byte size observed; `None` when the producer did not record it.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// File mtime (unix epoch ms) observed; `None` when not recorded.
    #[serde(default)]
    pub mtime_ms: Option<u64>,
    /// Content hash observed; `None` when hashing was skipped/not applicable.
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// A `WorkspaceSnapshot` inventory entry — identical in shape to
/// [`ObservedPath`] (relative path + size + mtime + optional content hash,
/// ADR-65 §2).
pub type SnapshotEntry = ObservedPath;

/// Payload of a `ToolExecuted` whiteboard event — a machine-recorded, observed
/// fact on the execution hot path (ADR-65 §3). All fields are optional/default
/// so events written by future producers remain decodable in any order; the
/// store only reads `generation` and `paths`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutedPayload {
    /// Attribution from the producer (never inferred); may duplicate the
    /// event's `agent_id` column.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub tool: String,
    /// Canonical argument form (normalized by the producer, ADR-64).
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Workspace `generation` (content-addressed string id, ADR-65) at
    /// execution time.
    #[serde(default)]
    pub generation: String,
    /// Identity of the project root this observation belongs to (blake3 hex of
    /// the canonical root, ADR-65 F5c). `""` for legacy events that predate
    /// per-root scoping — their rows are preserved but never served.
    #[serde(default)]
    pub project_root_hash: String,
    /// The paths this tool execution affected/observed.
    #[serde(default)]
    pub paths: Vec<ObservedPath>,
    /// ADR-65 §4 read-dedupe: when a read was **served from cache** instead of
    /// invoking the executor, the `event_id` of the observation the served
    /// content is attributable to (`last_event_id` of the verified clean row).
    /// `None` for every ordinary executed tool call.
    #[serde(default)]
    pub served_from: Option<String>,
}

/// Payload of a `WorkspaceSnapshot` whiteboard event — a read-only workspace
/// inventory taken before planning begins (ADR-65 §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshotPayload {
    /// Workspace `generation` (content-addressed string id, ADR-65 §2) the
    /// snapshot captures.
    #[serde(default)]
    pub generation: String,
    /// Identity of the project root this snapshot belongs to (blake3 hex,
    /// ADR-65 F5c). `""` for legacy events — preserved, never served.
    #[serde(default)]
    pub project_root_hash: String,
    /// The inventory entries.
    #[serde(default)]
    pub files: Vec<SnapshotEntry>,
}

// ---------------------------------------------------------------------------
// Row model
// ---------------------------------------------------------------------------

/// One row of `resource_facts` as returned by [`ResourceFacts::lookup`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceFactRow {
    pub path: String,
    /// Project root identity (blake3 hex) this row is scoped to. `""` marks a
    /// legacy pre-scoping row — preserved for attribution, never served.
    pub project_root_hash: String,
    /// Workspace `generation` (content-addressed string id) of the latest
    /// observation.
    pub generation: String,
    /// Observed byte size; `None` when not recorded.
    pub size_bytes: Option<u64>,
    /// Observed mtime (unix epoch ms); `None` when not recorded.
    pub mtime_ms: Option<u64>,
    /// Observed content hash; `None` when not recorded.
    pub content_hash: Option<String>,
    /// The whiteboard `event_id` of the latest observation.
    pub last_event_id: Option<String>,
    /// The agent that produced the latest observation.
    pub last_agent_id: Option<String>,
    /// `created_at` (unix epoch ms) of the latest observation event.
    pub observed_at: i64,
    /// `false` means the row is clean and safe to serve from cache; `true`
    /// means the workspace state is uncertain (write/change/side effect).
    pub dirty: bool,
}

/// One verified cacheable read: the observation row the content belongs to
/// plus the cached content bytes themselves (ADR-65 §4). The caller (not the
/// store) is responsible for the never-stale validation: a fresh `stat`
/// matching `row.size_bytes`/`row.mtime_ms` and `blake3(content) ==
/// row.content_hash`. `content_cached` is `NULL` for any row that was never
/// cached, so `[`ResourceFacts::cached_read`]` answers `None` for those.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedRead {
    pub row: ResourceFactRow,
    /// The cached exact bytes of the last read of `row.path`.
    pub content: String,
}

/// Raw row shape for `resource_facts` decoding (TEXT columns stay strings;
/// integer columns come back as `i64`). `PartialEq` supports test assertions
/// over whole-table snapshots during rebuild idempotency checks.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ResourceFactRowDb {
    path: String,
    project_root_hash: String,
    generation: String,
    size_bytes: Option<i64>,
    mtime_ms: Option<i64>,
    content_hash: Option<String>,
    last_event_id: Option<String>,
    last_agent_id: Option<String>,
    observed_at: i64,
    dirty: i64,
}

/// One-column decode for the content cache: `Option<String>` here keeps a
/// `NULL` `content_cached` distinguishable from an absent row and from `""` —
/// the query_scalar + `Option` combination would conflate all three.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct ContentCacheRow {
    content_cached: Option<String>,
}

impl TryFrom<ResourceFactRowDb> for ResourceFactRow {
    type Error = SessionError;

    fn try_from(row: ResourceFactRowDb) -> Result<Self, SessionError> {
        Ok(Self {
            path: row.path,
            project_root_hash: row.project_root_hash,
            generation: row.generation,
            size_bytes: row.size_bytes.map(u64::try_from).transpose().map_err(|_| {
                SessionError::Storage("negative size_bytes in resource_facts".to_string())
            })?,
            mtime_ms: row.mtime_ms.map(u64::try_from).transpose().map_err(|_| {
                SessionError::Storage("negative mtime_ms in resource_facts".to_string())
            })?,
            content_hash: row.content_hash,
            last_event_id: row.last_event_id,
            last_agent_id: row.last_agent_id,
            observed_at: row.observed_at,
            dirty: row.dirty != 0,
        })
    }
}

/// The write-side attribution of one observation, kept together so upsert
/// helpers stay under clippy's argument-count limit.
struct CleanObservation<'a> {
    event_id: &'a str,
    agent_id: &'a str,
    /// Unix epoch ms of the observation event (`created_at`).
    observed_at: i64,
    /// Workspace generation (content-addressed string id) captured by the
    /// observation.
    generation: String,
    /// Project root identity the observation is scoped to (ADR-65 F5c).
    project_root_hash: &'a str,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Sqlite-backed `resource_facts` store (migration 029).
///
/// All methods are cancellation-aware and take `&CancellationToken`; a
/// cancelled token aborts the operation before any statement runs (and, for
/// multi-statement operations, at every statement boundary).
#[derive(Clone)]
pub struct ResourceFacts {
    pool: sqlx::SqlitePool,
}

/// Convert a `u64` to its `i64` SQLite storage form, rejecting values that
/// exceed `i64::MAX` (log-assigned quantities are far below that in practice).
fn to_i64(value: u64) -> Result<i64, SessionError> {
    i64::try_from(value)
        .map_err(|_| SessionError::Storage(format!("value {value} exceeds i64 range")))
}

/// Upsert one path as a **clean** observation: brand `dirty = 0` and overwrite
/// the observation columns with the new observation's values (newest-wins).
async fn upsert_clean_row(
    conn: &mut sqlx::SqliteConnection,
    obs: &CleanObservation<'_>,
    path: &str,
    size_bytes: Option<u64>,
    mtime_ms: Option<u64>,
    content_hash: Option<&str>,
) -> Result<(), SessionError> {
    sqlx::query(
        "INSERT INTO resource_facts
             (path, project_root_hash, generation, size_bytes, mtime_ms, content_hash,
              last_event_id, last_agent_id, observed_at, dirty)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
         ON CONFLICT(project_root_hash, path) DO UPDATE SET
             generation    = excluded.generation,
             size_bytes    = excluded.size_bytes,
             mtime_ms      = excluded.mtime_ms,
             content_hash  = excluded.content_hash,
             last_event_id = excluded.last_event_id,
             last_agent_id = excluded.last_agent_id,
             observed_at   = excluded.observed_at,
             dirty         = 0",
    )
    .bind(path)
    .bind(obs.project_root_hash)
    .bind(obs.generation.as_str())
    .bind(size_bytes.map(to_i64).transpose()?)
    .bind(mtime_ms.map(to_i64).transpose()?)
    .bind(content_hash)
    .bind(obs.event_id)
    .bind(obs.agent_id)
    .bind(obs.observed_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// Flip one row's `dirty` flag to 1 and **purge its cached content** (ADR-65
/// F2a: a dirtying event means the cached bytes are no longer trustworthy, so
/// nothing stale may remain to serve). The observation columns are left
/// untouched. A missing row is a no-op (absent = uncertain = execute normally).
async fn mark_dirty_row(
    conn: &mut sqlx::SqliteConnection,
    project_root_hash: &str,
    path: &str,
) -> Result<(), SessionError> {
    sqlx::query(
        "UPDATE resource_facts
         SET dirty = 1, content_cached = NULL, content_cached_bytes = NULL
         WHERE project_root_hash = ? AND path = ?",
    )
    .bind(project_root_hash)
    .bind(path)
    .execute(conn)
    .await?;
    Ok(())
}

/// Flip every row for `path` across **all** roots dirty and purge their cached
/// content. Used by `rebuild_from_log`'s `WriteApplied` fold: such events carry
/// no project-root attribution, so dirtying every root that observed the path
/// is the conservative (never-stale) choice.
async fn mark_dirty_row_all_roots(
    conn: &mut sqlx::SqliteConnection,
    path: &str,
) -> Result<(), SessionError> {
    sqlx::query(
        "UPDATE resource_facts
         SET dirty = 1, content_cached = NULL, content_cached_bytes = NULL
         WHERE path = ?",
    )
    .bind(path)
    .execute(conn)
    .await?;
    Ok(())
}

/// Apply a workspace snapshot to `conn`: upsert every listed file clean, then
/// mark previously-clean rows for **this root** that **vanished** from the
/// listing dirty (kept, never deleted — their observation attribution
/// survives).
async fn reconcile_snapshot(
    conn: &mut sqlx::SqliteConnection,
    obs: &CleanObservation<'_>,
    files: &[SnapshotEntry],
) -> Result<(), SessionError> {
    // Pre-state of clean rows only, scoped to this project root: rows already
    // dirty stay dirty, and the just-upserted snapshot rows must not be
    // dirtied by the vanish pass.
    let clean_before: Vec<String> = sqlx::query_scalar(
        "SELECT path FROM resource_facts WHERE dirty = 0 AND project_root_hash = ?",
    )
    .bind(obs.project_root_hash)
    .fetch_all(&mut *conn)
    .await?;

    for entry in files {
        upsert_clean_row(
            conn,
            obs,
            &entry.path,
            entry.size_bytes,
            entry.mtime_ms,
            entry.content_hash.as_deref(),
        )
        .await?;
    }

    let listed: HashSet<&str> = files.iter().map(|e| e.path.as_str()).collect();
    for path in clean_before {
        if !listed.contains(path.as_str()) {
            mark_dirty_row(conn, obs.project_root_hash, &path).await?;
        }
    }
    Ok(())
}

/// Extract the file paths touched by a `WriteApplied` event's payload, using
/// the same defensive grammar as `fold_ledger` in the orchestrator: the
/// `pre_images` map keys when present, else the `path`/`target`/`input.path`
/// string fields. An empty result means the event carried no path information.
fn write_applied_paths(payload: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(pre_images) = payload.get("pre_images").and_then(|v| v.as_object()) {
        paths.extend(pre_images.keys().cloned());
        return paths;
    }
    if let Some(path) = payload.get("path").and_then(|v| v.as_str()) {
        paths.push(path.to_owned());
    } else if let Some(target) = payload.get("target").and_then(|v| v.as_str()) {
        paths.push(target.to_owned());
    } else if let Some(input) = payload.get("input").and_then(|v| v.as_object()) {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            paths.push(path.to_owned());
        }
    }
    paths
}

impl ResourceFacts {
    /// Wrap a pool in a `ResourceFacts` store. All migrations (including 029)
    /// must already have been applied to the pool.
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Look up the cached fact for `path` within `project_root_hash`; `Ok(None)`
    /// when nothing has been observed for that root (equivalently: state
    /// uncertain, execute normally).
    pub async fn lookup(
        &self,
        project_root_hash: &str,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<ResourceFactRow>, SessionError> {
        check_cancel(cancel)?;
        let row = sqlx::query_as::<_, ResourceFactRowDb>(
            "SELECT path, project_root_hash, generation, size_bytes, mtime_ms,
                    content_hash, last_event_id, last_agent_id, observed_at, dirty
             FROM resource_facts WHERE project_root_hash = ? AND path = ?",
        )
        .bind(project_root_hash)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ResourceFactRow::try_from).transpose()
    }

    /// Mark `path` dirty within `project_root_hash` — its cached observation is
    /// no longer trusted, and its cached content is purged (ADR-65 F2a). The
    /// observation columns are left untouched; a missing row is a no-op.
    pub async fn mark_dirty(
        &self,
        project_root_hash: &str,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        let mut tx = self.pool.begin().await?;
        mark_dirty_row(&mut tx, project_root_hash, path).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record a `ToolExecuted` observation: upsert every affected path as a
    /// clean row with the given attribution and the payload's generation.
    /// Runs atomically in one transaction.
    pub async fn apply_observed(
        &self,
        event_id: &str,
        agent_id: &str,
        observed_at: i64,
        payload: &ToolExecutedPayload,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        if payload.paths.is_empty() {
            return Ok(());
        }
        let obs = CleanObservation {
            event_id,
            agent_id,
            observed_at,
            generation: payload.generation.clone(),
            project_root_hash: payload.project_root_hash.as_str(),
        };
        let mut tx = self.pool.begin().await?;
        for path in &payload.paths {
            check_cancel(cancel)?;
            upsert_clean_row(
                &mut tx,
                &obs,
                &path.path,
                path.size_bytes,
                path.mtime_ms,
                path.content_hash.as_deref(),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// ADR-65 §4: fetch the cached content for `path` (scoped to
    /// `project_root_hash`) together with its observation row. `Ok(None)` when
    /// the path was never observed for that root OR never content-cached — the
    /// caller then must not serve and executes normally. The caller still
    /// validates the cache (fresh stat + hash) before serving; this method
    /// never judges freshness.
    pub async fn cached_read(
        &self,
        project_root_hash: &str,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<CachedRead>, SessionError> {
        check_cancel(cancel)?;
        let Some(row) = self.lookup(project_root_hash, path, cancel).await? else {
            return Ok(None);
        };
        // Decode through a struct so a NULL `content_cached` stays `None`
        // (query_scalar + fetch_optional would conflate NULL with "") — a row
        // that was never content-cached is not servable.
        let cached: Option<ContentCacheRow> = sqlx::query_as(
            "SELECT content_cached FROM resource_facts \
             WHERE project_root_hash = ? AND path = ?",
        )
        .bind(project_root_hash)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        let Some(ContentCacheRow { content_cached: Some(content) }) = cached else {
            return Ok(None);
        };
        Ok(Some(CachedRead { row, content }))
    }

    /// ADR-65 §4: store the exact bytes of a successful read as the content
    /// cache for `path` within `project_root_hash`. Returns `Ok(true)` when
    /// stored, `Ok(false)` when the content was refused (NUL bytes, over
    /// [`CACHE_LIMIT_BYTES`], or no row to attach it to — the row must exist
    /// from the observation already). Errors only for genuine store failures;
    /// callers treat every failure as "not cached, no harm done" (the cache is
    /// derived, never a requirement).
    pub async fn store_read_content(
        &self,
        project_root_hash: &str,
        path: &str,
        content: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, SessionError> {
        check_cancel(cancel)?;
        if content.as_bytes().contains(&0) {
            return Ok(false);
        }
        if content.len() > CACHE_LIMIT_BYTES {
            return Ok(false);
        }
        let stored = sqlx::query(
            "UPDATE resource_facts
             SET content_cached = ?, content_cached_bytes = ?
             WHERE project_root_hash = ? AND path = ?",
        )
        .bind(content)
        .bind(to_i64(content.len() as u64)?)
        .bind(project_root_hash)
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(stored.rows_affected() == 1)
    }

    /// ADR-65 §4 action digest: the newest `limit` observed paths **within
    /// `project_root_hash`**, newest `observed_at` first with a deterministic
    /// path tiebreak. The caller renders each row as unchanged/dirty — never
    /// gates behavior on the list alone (rows carry the `dirty` honesty bit).
    pub async fn list_observations(
        &self,
        project_root_hash: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<ResourceFactRow>, SessionError> {
        check_cancel(cancel)?;
        let rows = sqlx::query_as::<_, ResourceFactRowDb>(
            "SELECT path, project_root_hash, generation, size_bytes, mtime_ms,
                    content_hash, last_event_id, last_agent_id, observed_at, dirty
             FROM resource_facts
             WHERE project_root_hash = ?
             ORDER BY observed_at DESC, path ASC
             LIMIT ?",
        )
        .bind(project_root_hash)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ResourceFactRow::try_from).collect()
    }

    /// Apply a `WorkspaceSnapshot` observation: upsert every listed file as
    /// clean, and mark previously-clean rows that vanished from the listing
    /// dirty (kept for attribution). Runs atomically in one transaction.
    pub async fn reconcile_from_snapshot(
        &self,
        event_id: &str,
        agent_id: &str,
        observed_at: i64,
        payload: &WorkspaceSnapshotPayload,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        let obs = CleanObservation {
            event_id,
            agent_id,
            observed_at,
            generation: payload.generation.clone(),
            project_root_hash: payload.project_root_hash.as_str(),
        };
        let mut tx = self.pool.begin().await?;
        reconcile_snapshot(&mut tx, &obs, &payload.files).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Alias for [`ResourceFacts::reconcile_from_snapshot`].
    pub async fn apply_snapshot(
        &self,
        event_id: &str,
        agent_id: &str,
        observed_at: i64,
        payload: &WorkspaceSnapshotPayload,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        self.reconcile_from_snapshot(event_id, agent_id, observed_at, payload, cancel).await
    }

    /// Invalidate the cache for every affected path **within
    /// `project_root_hash`** (e.g. a `WriteApplied` event, a watcher change
    /// hint, or observed shell/git side effects): each is marked dirty and its
    /// cached content purged; absent paths are ignored.
    pub async fn invalidate_on_write(
        &self,
        project_root_hash: &str,
        paths: &[String],
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        let mut tx = self.pool.begin().await?;
        for path in paths {
            check_cancel(cancel)?;
            mark_dirty_row(&mut tx, project_root_hash, path).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Rebuild the derived table **forward from the log** (ADR-65 §4): wipe
    /// the table, then fold every whiteboard event in `gate_seq` order.
    ///
    /// - `ToolExecuted` upserts each affected path clean (attribution from the
    ///   event columns, `observed_at` from the event `created_at` — so a
    ///   replay reproduces the original wall times; `project_root_hash` folds
    ///   from the payload, `''` for legacy events).
    /// - `WriteApplied` dirties the paths its payload names **across every
    ///   root** that observed them — the event carries no root attribution, so
    ///   the conservative choice is to dirty all of them.
    /// - `WorkspaceSnapshot` reconciles like a live snapshot.
    /// - Events with unparseable evidence payloads are skipped defensively —
    ///   one malformed sibling event never blocks the rebuild.
    ///
    /// The whole rebuild is atomic: a failure rolls back the wipe. Idempotent:
    /// rebuilding twice yields the same table.
    pub async fn rebuild_from_log(&self, cancel: &CancellationToken) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        let events =
            crate::whiteboard::load_whiteboard_events_up_to(&self.pool, u64::MAX, None).await?;

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM resource_facts").execute(tx.as_mut()).await?;

        for event in events {
            check_cancel(cancel)?;
            let obs = CleanObservation {
                event_id: &event.event_id,
                agent_id: &event.agent_id,
                observed_at: event.created_at,
                generation: String::new(),
                project_root_hash: "",
            };
            match event.kind {
                WhiteboardKind::ToolExecuted => {
                    let Ok(payload) =
                        serde_json::from_value::<ToolExecutedPayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    let obs = CleanObservation {
                        generation: payload.generation,
                        project_root_hash: payload.project_root_hash.as_str(),
                        ..obs
                    };
                    for path in &payload.paths {
                        upsert_clean_row(
                            &mut tx,
                            &obs,
                            &path.path,
                            path.size_bytes,
                            path.mtime_ms,
                            path.content_hash.as_deref(),
                        )
                        .await?;
                    }
                }
                WhiteboardKind::WriteApplied => {
                    for path in write_applied_paths(&event.payload) {
                        mark_dirty_row_all_roots(&mut tx, &path).await?;
                    }
                }
                WhiteboardKind::WorkspaceSnapshot => {
                    let Ok(payload) =
                        serde_json::from_value::<WorkspaceSnapshotPayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    let obs = CleanObservation {
                        generation: payload.generation,
                        project_root_hash: payload.project_root_hash.as_str(),
                        ..obs
                    };
                    reconcile_snapshot(&mut tx, &obs, &payload.files).await?;
                }
                _ => {}
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use sqlx::Connection;
    use tempfile::TempDir;

    use crate::whiteboard::{append_whiteboard_event, NewWhiteboardEvent};

    /// File-backed pool with the same PRAGMAs as production connectivity and
    /// all migrations applied (same bootstrap as `whiteboard.rs` tests).
    async fn test_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("resource_facts_test.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    /// Two distinct project roots for scope-isolation tests (ADR-65 F5c). The
    /// `observed()` helper brands everything with `ROOT_A`; tests that need the
    /// other root re-brand a payload explicitly.
    const ROOT_A: &str = "root-a";
    const ROOT_B: &str = "root-b";

    fn observed(path: &str, hash: &str, generation: &str) -> ToolExecutedPayload {
        ToolExecutedPayload {
            agent_id: Some("agent-a".to_owned()),
            task_id: None,
            run_id: None,
            tool: "apply_diff".to_owned(),
            args: json!({ "path": path }),
            success: true,
            exit_code: Some(0),
            generation: generation.to_owned(),
            project_root_hash: ROOT_A.to_owned(),
            paths: vec![ObservedPath {
                path: path.to_owned(),
                size_bytes: Some(42),
                mtime_ms: Some(1_000),
                content_hash: Some(hash.to_owned()),
            }],
            served_from: None,
        }
    }

    /// Append an evidence event to `whiteboard_events` (the log the store
    /// derives from). `created_at` is also what the live store call must pass
    /// as `observed_at` so live and rebuilt state stay comparable.
    async fn append_evidence(
        pool: &sqlx::SqlitePool,
        event_id: &str,
        agent_id: &str,
        kind: WhiteboardKind,
        payload: serde_json::Value,
        created_at: i64,
    ) {
        let event = NewWhiteboardEvent {
            event_id: event_id.to_owned(),
            agent_id: agent_id.to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload,
            pre_image_hash: None,
            created_at,
        };
        append_whiteboard_event(pool, &event).await.expect("append evidence event");
    }

    #[tokio::test]
    async fn migrations_produce_resource_facts_with_scoped_composite_primary_key() {
        let (_dir, pool) = test_pool(1).await;

        let objects: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table', 'index') AND name LIKE '%resource_facts%' \
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("schema query");
        assert!(
            objects.iter().any(|n| n == "resource_facts"),
            "resource_facts table exists; got: {objects:?}"
        );
        assert!(
            objects.iter().any(|n| n == "idx_resource_facts_dirty_path"),
            "dirty/root index exists; got: {objects:?}"
        );

        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('resource_facts') ORDER BY cid")
                .fetch_all(&pool)
                .await
                .expect("column query");
        assert_eq!(
            columns,
            vec![
                "path",
                "project_root_hash",
                "generation",
                "size_bytes",
                "mtime_ms",
                "content_hash",
                "last_event_id",
                "last_agent_id",
                "observed_at",
                "dirty",
                "content_cached",
                "content_cached_bytes",
            ]
        );

        // Row identity is the (project_root_hash, path) pair (ADR-65 F5c,
        // migration 031) — two roots observing the same relative path must not
        // clobber each other, so the primary key is the scoped composite.
        let pk: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_table_info('resource_facts') WHERE pk > 0 ORDER BY pk",
        )
        .fetch_all(&pool)
        .await
        .expect("pk query");
        assert_eq!(pk, vec!["project_root_hash", "path"], "scoped composite primary key");

        // The workspace generation is a content-addressed string id (ADR-65
        // §2), so the column is TEXT, not INTEGER.
        let generation_type: String = sqlx::query_scalar(
            "SELECT type FROM pragma_table_info('resource_facts') WHERE name = 'generation'",
        )
        .fetch_one(&pool)
        .await
        .expect("generation column type query");
        assert_eq!(generation_type, "TEXT", "generation column stores the string id");

        // A fresh table is empty, even for the legacy empty root hash.
        let store = ResourceFacts::new(pool);
        assert!(store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").is_none());
        assert!(store.lookup("", "a.md", &token()).await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn apply_observed_upserts_clean_rows_and_mark_dirty_flips_them() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);

        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("observe");

        let row = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row");
        assert_eq!(row.generation, "1");
        assert_eq!(row.size_bytes, Some(42));
        assert_eq!(row.mtime_ms, Some(1_000));
        assert_eq!(row.content_hash.as_deref(), Some("h1"));
        assert_eq!(row.last_event_id.as_deref(), Some("ev-1"));
        assert_eq!(row.last_agent_id.as_deref(), Some("agent-a"));
        assert_eq!(row.observed_at, 1_700_000_000_000);
        assert!(!row.dirty, "an observation brands the row clean");

        // Dirtying flips the flag but preserves the observation columns.
        store.mark_dirty(ROOT_A, "a.md", &token()).await.expect("mark dirty");
        let row = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row");
        assert!(row.dirty, "marked dirty");
        assert_eq!(row.content_hash.as_deref(), Some("h1"));
        assert_eq!(row.last_event_id.as_deref(), Some("ev-1"));
    }

    #[tokio::test]
    async fn apply_observed_overwrites_prior_observation() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);

        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("first observation");
        store
            .apply_observed(
                "ev-2",
                "agent-b",
                1_700_000_000_001,
                &observed("a.md", "h2", "2"),
                &token(),
            )
            .await
            .expect("second observation");

        let row = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row");
        assert_eq!(row.generation, "2");
        assert_eq!(row.content_hash.as_deref(), Some("h2"));
        assert_eq!(row.last_event_id.as_deref(), Some("ev-2"));
        assert_eq!(row.last_agent_id.as_deref(), Some("agent-b"));
        assert_eq!(row.observed_at, 1_700_000_000_001);
        assert!(!row.dirty, "a fresh observation re-cleans the row");
    }

    #[tokio::test]
    async fn invalidate_on_write_marks_rows_dirty_and_ignores_missing() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("observe");

        store
            .invalidate_on_write(ROOT_A, &["a.md".to_owned(), "ghost.md".to_owned()], &token())
            .await
            .expect("invalidate");

        let row = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row");
        assert!(row.dirty, "touched path is dirty");
        assert_eq!(row.content_hash.as_deref(), Some("h1"), "observation preserved");
        assert!(
            store.lookup(ROOT_A, "ghost.md", &token()).await.expect("lookup").is_none(),
            "dirtying an absent path is a no-op — it must not conjure a row"
        );
    }

    #[tokio::test]
    async fn snapshot_reconcile_cleans_listed_rows_and_dirties_vanished() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);

        let mut b = observed("b.md", "hb", "1");
        b.paths[0].size_bytes = Some(7);
        store
            .apply_observed(
                "ev-a",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "ha", "1"),
                &token(),
            )
            .await
            .expect("observe a");
        store
            .apply_observed("ev-b", "agent-a", 1_700_000_000_001, &b, &token())
            .await
            .expect("observe b");

        // Snapshot lists only a.md (b.md vanished from the workspace).
        let snapshot = WorkspaceSnapshotPayload {
            generation: "2".to_owned(),
            project_root_hash: ROOT_A.to_owned(),
            files: vec![SnapshotEntry {
                path: "a.md".to_owned(),
                size_bytes: Some(100),
                mtime_ms: Some(2_000),
                content_hash: Some("ha2".to_owned()),
            }],
        };
        store
            .apply_snapshot("snap-1", "agent-a", 1_700_000_000_002, &snapshot, &token())
            .await
            .expect("apply snapshot");

        let a = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("a row");
        assert!(!a.dirty, "snapshot-listed row is clean");
        assert_eq!(a.generation, "2");
        assert_eq!(a.content_hash.as_deref(), Some("ha2"));
        assert_eq!(a.last_event_id.as_deref(), Some("snap-1"));

        let b = store.lookup(ROOT_A, "b.md", &token()).await.expect("lookup").expect("b row");
        assert!(b.dirty, "vanished row is kept but marked dirty");
        assert_eq!(b.generation, "1", "observation preserved");
        assert_eq!(b.last_event_id.as_deref(), Some("ev-b"), "observation preserved");
    }

    #[tokio::test]
    async fn write_applied_paths_handles_every_payload_shape() {
        assert_eq!(
            write_applied_paths(&json!({ "pre_images": { "a.md": "pre-a", "b.rs": "pre-b" } })),
            vec!["a.md", "b.rs"]
        );
        assert_eq!(write_applied_paths(&json!({ "path": "x.txt" })), vec!["x.txt"]);
        assert_eq!(write_applied_paths(&json!({ "target": "y.txt" })), vec!["y.txt"]);
        assert_eq!(write_applied_paths(&json!({ "input": { "path": "z.txt" } })), vec!["z.txt"]);
        assert!(
            write_applied_paths(&json!({ "something": "else" })).is_empty(),
            "a payload without path info names no files"
        );
    }

    #[tokio::test]
    async fn evidence_payloads_round_trip_through_serde() {
        let executed = observed("a.md", "h1", "3");
        let json = serde_json::to_value(&executed).expect("serialize");
        let back: ToolExecutedPayload = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, executed, "ToolExecuted payload round trips");

        let snapshot = WorkspaceSnapshotPayload {
            generation: "3".to_owned(),
            project_root_hash: ROOT_A.to_owned(),
            files: vec![SnapshotEntry {
                path: "a.md".to_owned(),
                size_bytes: Some(42),
                mtime_ms: Some(1_000),
                content_hash: Some("h1".to_owned()),
            }],
        };
        let json = serde_json::to_value(&snapshot).expect("serialize");
        let back: WorkspaceSnapshotPayload = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, snapshot, "WorkspaceSnapshot payload round trips");

        // All fields default, so documents missing them still decode.
        let minimal: ToolExecutedPayload =
            serde_json::from_value(json!({ "tool": "ls" })).expect("minimal decodes");
        assert_eq!(minimal.generation, "");
        assert_eq!(minimal.project_root_hash, "", "root hash defaults empty for legacy events");
        assert!(minimal.paths.is_empty());
        assert_eq!(minimal.tool, "ls");
    }

    #[tokio::test]
    async fn rebuild_from_log_is_idempotent_and_restores_clean_table() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool.clone());

        // Append the evidence sequence to the log AND apply it live, so the
        // rebuild must reproduce the live table from the log alone:
        //   observe a.md (gen 1) → observe b.md (gen 2) → write dirties both →
        //   observe b.md again → a snapshot lists only a.md (b vanished).
        let t0 = 1_700_000_000_000;
        let payload_a = observed("a.md", "h1", "1");
        append_evidence(
            &pool,
            "ev-1",
            "agent-a",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_a).unwrap(),
            t0,
        )
        .await;
        store.apply_observed("ev-1", "agent-a", t0, &payload_a, &token()).await.expect("observe a");

        let t1 = t0 + 1;
        let payload_b = observed("b.md", "h2", "2");
        append_evidence(
            &pool,
            "ev-2",
            "agent-a",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_b).unwrap(),
            t1,
        )
        .await;
        store.apply_observed("ev-2", "agent-a", t1, &payload_b, &token()).await.expect("observe b");

        let t2 = t0 + 2;
        append_evidence(
            &pool,
            "ev-3",
            "agent-b",
            WhiteboardKind::WriteApplied,
            json!({ "pre_images": { "a.md": "pre-a", "b.md": "pre-b" } }),
            t2,
        )
        .await;
        store
            .invalidate_on_write(ROOT_A, &["a.md".to_owned(), "b.md".to_owned()], &token())
            .await
            .expect("write dirties");

        let t3 = t0 + 3;
        let payload_b2 = observed("b.md", "h2", "2");
        append_evidence(
            &pool,
            "ev-4",
            "agent-a",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_b2).unwrap(),
            t3,
        )
        .await;
        store
            .apply_observed("ev-4", "agent-a", t3, &payload_b2, &token())
            .await
            .expect("re-observe b");

        let t4 = t0 + 4;
        let snapshot = WorkspaceSnapshotPayload {
            generation: "2".to_owned(),
            project_root_hash: ROOT_A.to_owned(),
            files: vec![SnapshotEntry {
                path: "a.md".to_owned(),
                size_bytes: Some(42),
                mtime_ms: Some(1_000),
                content_hash: Some("h1".to_owned()),
            }],
        };
        append_evidence(
            &pool,
            "ev-5",
            "agent-a",
            WhiteboardKind::WorkspaceSnapshot,
            serde_json::to_value(&snapshot).unwrap(),
            t4,
        )
        .await;
        store.apply_snapshot("ev-5", "agent-a", t4, &snapshot, &token()).await.expect("snapshot");

        // Phantom rows whose events are NOT in the log: the rebuild must erase
        // them (the table is purely derived). They live on the legacy empty
        // root, which the wipe-and-rebuild naturally clears.
        sqlx::query(
            "INSERT INTO resource_facts
                 (path, project_root_hash, generation, observed_at, dirty) \
             VALUES ('ghost.md', '', 0, 0, 0), ('phantom.md', '', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("insert phantom rows");

        store.rebuild_from_log(&token()).await.expect("rebuild");

        let a = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("a row");
        assert!(!a.dirty, "a.md clean from the snapshot");
        assert_eq!(a.generation, "2");
        assert_eq!(a.content_hash.as_deref(), Some("h1"));
        assert_eq!(a.last_event_id.as_deref(), Some("ev-5"));
        assert_eq!(a.observed_at, t4, "observed_at comes from the event's created_at");

        let b = store.lookup(ROOT_A, "b.md", &token()).await.expect("lookup").expect("b row");
        assert!(b.dirty, "b.md vanished from the final snapshot → dirty");
        assert_eq!(b.generation, "2", "last observation preserved");
        assert_eq!(b.last_event_id.as_deref(), Some("ev-4"), "last observation preserved");
        assert_eq!(b.observed_at, t3, "observed_at comes from the event's created_at");

        for phantom in ["ghost.md", "phantom.md"] {
            assert!(
                store.lookup(ROOT_A, phantom, &token()).await.expect("lookup").is_none(),
                "{phantom} erased by rebuild — only the log is authoritative"
            );
        }

        // Idempotency: a second rebuild reproduces the exact same table.
        let before: Vec<ResourceFactRowDb> = sqlx::query_as(
            "SELECT path, project_root_hash, generation, size_bytes, mtime_ms, content_hash,
                    last_event_id, last_agent_id, observed_at, dirty
             FROM resource_facts ORDER BY path",
        )
        .fetch_all(&pool)
        .await
        .expect("snapshot rows");
        store.rebuild_from_log(&token()).await.expect("rebuild again");
        let after: Vec<ResourceFactRowDb> = sqlx::query_as(
            "SELECT path, project_root_hash, generation, size_bytes, mtime_ms, content_hash,
                    last_event_id, last_agent_id, observed_at, dirty
             FROM resource_facts ORDER BY path",
        )
        .fetch_all(&pool)
        .await
        .expect("snapshot rows");
        assert_eq!(after, before, "rebuild is idempotent");
        assert_eq!(after.len(), 2, "exactly the two log-derived rows survive");
    }

    #[tokio::test]
    async fn rebuild_from_log_skips_unparseable_evidence_payloads_defensively() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool.clone());

        // A ToolExecuted event whose payload is not a valid typed payload (an
        // older binary's unknown-kind write) must not block the rebuild.
        let broken = NewWhiteboardEvent {
            event_id: "broken-1".to_owned(),
            agent_id: "agent-a".to_owned(),
            kind: WhiteboardKind::ToolExecuted,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({ "paths": "not-an-array" }),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        };
        append_whiteboard_event(&pool, &broken).await.expect("append broken event");

        // A valid sibling ToolExecuted fact, also in the log.
        let t_good = 1_700_000_000_001;
        let payload_good = observed("good.md", "hg", "1");
        append_evidence(
            &pool,
            "ev-good",
            "agent-a",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_good).unwrap(),
            t_good,
        )
        .await;
        store
            .apply_observed("ev-good", "agent-a", t_good, &payload_good, &token())
            .await
            .expect("observe");

        store.rebuild_from_log(&token()).await.expect("rebuild despite broken payload");

        assert!(
            store.lookup(ROOT_A, "good.md", &token()).await.expect("lookup").is_some(),
            "a valid sibling event still folds into the rebuild"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resource_facts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1, "the unparseable fact was skipped, not fatal");
    }

    #[tokio::test]
    async fn precancelled_token_aborts_before_any_write() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool.clone());

        let cancelled = CancellationToken::new();
        cancelled.cancel();

        let res = store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &cancelled,
            )
            .await;
        assert!(res.is_err(), "a cancelled token aborts before writing");

        assert!(
            store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").is_none(),
            "nothing was written before the abort"
        );

        let res = store.rebuild_from_log(&cancelled).await;
        assert!(res.is_err(), "rebuild aborts on a cancelled token");
    }

    #[tokio::test]
    async fn store_read_content_caches_exact_bytes_for_an_observed_path() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("observe");

        assert!(
            store.store_read_content(ROOT_A, "a.md", "hello world", &token()).await.expect("store"),
            "content cached for an existing clean row"
        );

        let cached =
            store.cached_read(ROOT_A, "a.md", &token()).await.expect("read").expect("cached");
        assert_eq!(cached.content, "hello world", "exact bytes preserved");
        assert_eq!(cached.row.content_hash.as_deref(), Some("h1"));
        assert_eq!(cached.row.last_event_id.as_deref(), Some("ev-1"), "row carries attribution");

        let bytes: i64 = sqlx::query_scalar(
            "SELECT content_cached_bytes FROM resource_facts \
             WHERE project_root_hash = 'root-a' AND path = 'a.md'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("bytes column query");
        assert_eq!(bytes, 11, "content_cached_bytes mirrors the byte length");
    }

    #[tokio::test]
    async fn store_read_content_refuses_oversized_and_nul_content() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("observe");

        let too_big = "x".repeat(CACHE_LIMIT_BYTES + 1);
        assert!(
            !store.store_read_content(ROOT_A, "a.md", &too_big, &token()).await.expect("refused"),
            "content over the cache limit is refused, not stored"
        );
        let with_nul = "a\0b";
        assert!(
            !store.store_read_content(ROOT_A, "a.md", with_nul, &token()).await.expect("refused"),
            "NUL-bearing content is refused (TEXT column stays clean)"
        );

        assert_eq!(
            store.cached_read(ROOT_A, "a.md", &token()).await.expect("read").map(|c| c.content),
            None,
            "nothing cached after the refusals"
        );
    }

    #[tokio::test]
    async fn store_read_content_without_a_row_is_a_noop() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        assert!(
            !store.store_read_content(ROOT_A, "ghost.md", "x", &token()).await.expect("noop"),
            "no row to attach the cache to → refused"
        );
        assert_eq!(
            store.cached_read(ROOT_A, "ghost.md", &token()).await.expect("absent"),
            None,
            "an absent path has no cached read"
        );
    }

    #[tokio::test]
    async fn cached_read_is_none_for_observed_but_never_cached_rows() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        let mut big = observed("big.bin", "h1", "1");
        big.paths[0].content_hash = None; // > hashing budget: never content-cached
        store
            .apply_observed("ev-1", "agent-a", 1_700_000_000_000, &big, &token())
            .await
            .expect("observe");
        assert_eq!(
            store.cached_read(ROOT_A, "big.bin", &token()).await.expect("no cache"),
            None,
            "observed without cached content is not servable"
        );
    }

    #[tokio::test]
    async fn list_observations_orders_newest_first_and_respects_the_limit() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);
        let t0 = 1_700_000_000_000;
        store
            .apply_observed("ev-a", "agent-a", t0, &observed("a.md", "ha", "1"), &token())
            .await
            .expect("observe a");
        store
            .apply_observed("ev-b", "agent-a", t0 + 1, &observed("b.md", "hb", "1"), &token())
            .await
            .expect("observe b");
        store
            .apply_observed("ev-c", "agent-a", t0 + 2, &observed("c.md", "hc", "1"), &token())
            .await
            .expect("observe c");

        let all = store.list_observations(ROOT_A, 10, &token()).await.expect("all rows");
        let paths: Vec<&str> = all.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(paths, vec!["c.md", "b.md", "a.md"], "newest observed_at first");
        assert!(all.iter().all(|row| !row.dirty), "all observed rows clean");

        let capped = store.list_observations(ROOT_A, 2, &token()).await.expect("capped rows");
        let paths: Vec<&str> = capped.iter().map(|row| row.path.as_str()).collect();
        assert_eq!(paths, vec!["c.md", "b.md"], "limit honored, newest kept");

        // Dirty rows surface with their honesty bit intact for the renderer.
        store.mark_dirty(ROOT_A, "b.md", &token()).await.expect("dirty");
        let rows = store.list_observations(ROOT_A, 10, &token()).await.expect("rows");
        let b = rows.iter().find(|row| row.path == "b.md").expect("b row");
        assert!(b.dirty, "dirty flag preserved through list_observations");
    }

    #[tokio::test]
    async fn served_from_round_trips_and_defaults_to_none() {
        let mut executed = observed("a.md", "h1", "1");
        assert_eq!(executed.served_from, None, "ordinary facts carry no served_from");
        executed.served_from = Some("ev-1".to_owned());
        let json = serde_json::to_value(&executed).expect("serialize");
        let back: ToolExecutedPayload = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.served_from.as_deref(), Some("ev-1"), "served_from round trips");

        let minimal: ToolExecutedPayload =
            serde_json::from_value(json!({ "tool": "ls" })).expect("minimal decodes");
        assert_eq!(minimal.served_from, None, "old events decode with served_from absent");
    }

    #[tokio::test]
    async fn migration_031_preserves_legacy_rows_under_the_empty_root_hash() {
        // Apply migrations up to 030 on a dedicated connection, seed legacy
        // (pre-scoping) rows exactly as 029/030 produce them, then let 031
        // apply — the rebuild must preserve every legacy row under `''`.
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("preserve.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let mut conn =
            sqlx::sqlite::SqliteConnection::connect_with(&options).await.expect("connect");

        let migrator = sqlx::migrate!("./migrations");
        migrator.run_to(30, &mut conn).await.expect("migrations up to 030 apply");

        // Legacy rows: path is the whole identity; one carries a content cache.
        sqlx::query(
            "INSERT INTO resource_facts
                 (path, generation, size_bytes, mtime_ms, content_hash,
                  last_event_id, last_agent_id, observed_at, dirty, content_cached,
                  content_cached_bytes)
             VALUES ('legacy-a.md', '1', 42, 1000, 'ha',
                     'ev-1', 'agent-a', 1700000000000, 0, 'cached-bytes', 12),
                    ('legacy-b.md', '2', NULL, NULL, NULL,
                     'ev-2', 'agent-b', 1700000000001, 1, NULL, NULL)",
        )
        .execute(&mut conn)
        .await
        .expect("seed legacy rows");

        migrator.run(&mut conn).await.expect("migration 031 applies");

        let rows: Vec<ResourceFactRowDb> = sqlx::query_as(
            "SELECT path, project_root_hash, generation, size_bytes, mtime_ms,
                    content_hash, last_event_id, last_agent_id, observed_at, dirty
             FROM resource_facts ORDER BY path",
        )
        .fetch_all(&mut conn)
        .await
        .expect("post-migration rows");

        assert_eq!(rows.len(), 2, "both legacy rows survive the 031 rebuild");
        let a = rows.iter().find(|r| r.path == "legacy-a.md").expect("row a");
        assert_eq!(a.project_root_hash, "", "legacy row stays under the empty root hash");
        assert_eq!(a.generation, "1");
        assert_eq!(a.content_hash.as_deref(), Some("ha"));
        assert_eq!(a.last_event_id.as_deref(), Some("ev-1"));
        assert!(a.dirty == 0, "clean flag preserved");
        let b = rows.iter().find(|r| r.path == "legacy-b.md").expect("row b");
        assert_eq!(b.project_root_hash, "", "legacy row stays under the empty root hash");
        assert!(b.dirty == 1, "dirty flag preserved");

        let cached: Option<String> = sqlx::query_scalar(
            "SELECT content_cached FROM resource_facts WHERE path = 'legacy-a.md'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("cached content query");
        assert_eq!(cached.as_deref(), Some("cached-bytes"), "content cache survives 031");

        // The empty root never serves (F5c): a plain lookup under `''` still
        // sees the preserved rows, but scoped roots do not.
        let pool = sqlx::SqlitePool::connect_with(options).await.expect("pool connects");
        let store = ResourceFacts::new(pool);
        assert!(
            store.lookup("", "legacy-a.md", &token()).await.expect("lookup").is_some(),
            "legacy row is visible under the empty root hash"
        );
        assert!(
            store.lookup(ROOT_A, "legacy-a.md", &token()).await.expect("lookup").is_none(),
            "legacy row is not visible under a real root"
        );
    }

    #[tokio::test]
    async fn resource_facts_scope_rows_per_project_root() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool);

        // Both roots observe `a.md` — same relative path, different files.
        let mut a = observed("a.md", "ha1", "1");
        store
            .apply_observed("ev-ra", "agent-a", 1_700_000_000_000, &a, &token())
            .await
            .expect("observe under root A");
        a.project_root_hash = ROOT_B.to_owned();
        a.paths[0].content_hash = Some("hb1".to_owned());
        store
            .apply_observed("ev-rb", "agent-b", 1_700_000_000_001, &a, &token())
            .await
            .expect("observe under root B");

        // Scoped lookups never leak across roots (F5c).
        let row_a = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row a");
        assert_eq!(row_a.project_root_hash, ROOT_A);
        assert_eq!(row_a.content_hash.as_deref(), Some("ha1"));
        assert_eq!(row_a.last_agent_id.as_deref(), Some("agent-a"));

        let row_b = store.lookup(ROOT_B, "a.md", &token()).await.expect("lookup").expect("row b");
        assert_eq!(row_b.project_root_hash, ROOT_B);
        assert_eq!(row_b.content_hash.as_deref(), Some("hb1"));
        assert_eq!(row_b.last_agent_id.as_deref(), Some("agent-b"));

        // The legacy empty root sees none of the scoped rows.
        assert!(
            store.lookup("", "a.md", &token()).await.expect("lookup").is_none(),
            "empty-root lookup must not match scoped rows"
        );

        // Dirtying is scoped too: only root B's row flips.
        store.mark_dirty(ROOT_B, "a.md", &token()).await.expect("mark dirty");
        assert!(
            !store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row").dirty,
            "root A unaffected by a root B dirty"
        );
        assert!(
            store.lookup(ROOT_B, "a.md", &token()).await.expect("lookup").expect("row").dirty,
            "root B row is dirty"
        );

        // list_observations is scoped per root — same path, one row each.
        let for_a = store.list_observations(ROOT_A, 10, &token()).await.expect("rows");
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].path, "a.md");
        assert_eq!(for_a[0].project_root_hash, ROOT_A);
        let for_b = store.list_observations(ROOT_B, 10, &token()).await.expect("rows");
        assert_eq!(for_b.len(), 1);
        assert!(for_b[0].dirty);
    }

    #[tokio::test]
    async fn marking_dirty_purges_the_cached_content() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool.clone());
        store
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("observe");
        assert!(
            store.store_read_content(ROOT_A, "a.md", "hello world", &token()).await.expect("store"),
            "content cached while the row is clean"
        );
        assert!(
            store.cached_read(ROOT_A, "a.md", &token()).await.expect("read").is_some(),
            "cached read available before dirtying"
        );

        // Dirtying must purge the stale cached bytes (ADR-65 F2a).
        store.mark_dirty(ROOT_A, "a.md", &token()).await.expect("mark dirty");

        assert!(
            store.cached_read(ROOT_A, "a.md", &token()).await.expect("read").is_none(),
            "dirtying purges the cached content — nothing stale may remain to serve"
        );
        let row = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row");
        assert!(row.dirty);
        assert_eq!(row.content_hash.as_deref(), Some("h1"), "observation columns survive");
        assert_eq!(row.last_event_id.as_deref(), Some("ev-1"), "observation columns survive");

        // The columns are hard-NULL in storage, not just hidden by the API.
        let cached: Option<String> = sqlx::query_scalar(
            "SELECT content_cached FROM resource_facts \
             WHERE project_root_hash = 'root-a' AND path = 'a.md'",
        )
        .fetch_one(&pool)
        .await
        .expect("cached content query");
        assert_eq!(cached, None, "content_cached is NULL after a dirty");

        // Re-caching after a fresh observation is allowed (the row is clean
        // again), proving the purge did not corrupt the row.
        store
            .apply_observed(
                "ev-2",
                "agent-a",
                1_700_000_000_001,
                &observed("a.md", "h1", "1"),
                &token(),
            )
            .await
            .expect("re-observe");
        assert!(
            store.store_read_content(ROOT_A, "a.md", "hello again", &token()).await.expect("store"),
            "a clean row can be cached again after a dirty purged it"
        );
    }

    #[tokio::test]
    async fn rebuild_folds_project_root_hash_and_dirties_all_roots_on_write_applied() {
        let (_dir, pool) = test_pool(1).await;
        let store = ResourceFacts::new(pool.clone());

        // The same relative path observed under two different roots.
        let t0 = 1_700_000_000_000;
        let payload_a = observed("a.md", "ha1", "1");
        append_evidence(
            &pool,
            "ev-root-a",
            "agent-a",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_a).unwrap(),
            t0,
        )
        .await;
        store
            .apply_observed("ev-root-a", "agent-a", t0, &payload_a, &token())
            .await
            .expect("observe root A");

        let mut payload_b = observed("a.md", "hb1", "2");
        payload_b.project_root_hash = ROOT_B.to_owned();
        append_evidence(
            &pool,
            "ev-root-b",
            "agent-b",
            WhiteboardKind::ToolExecuted,
            serde_json::to_value(&payload_b).unwrap(),
            t0 + 1,
        )
        .await;
        store
            .apply_observed("ev-root-b", "agent-b", t0 + 1, &payload_b, &token())
            .await
            .expect("observe root B");

        // A WriteApplied names the path without root attribution: the rebuild
        // must dirty the row under EVERY root that observed it.
        append_evidence(
            &pool,
            "ev-write",
            "agent-c",
            WhiteboardKind::WriteApplied,
            json!({ "pre_images": { "a.md": "pre-a" } }),
            t0 + 2,
        )
        .await;

        store.rebuild_from_log(&token()).await.expect("rebuild");

        let row_a = store.lookup(ROOT_A, "a.md", &token()).await.expect("lookup").expect("row a");
        assert_eq!(row_a.project_root_hash, ROOT_A, "root folds from the ToolExecuted payload");
        assert!(row_a.dirty, "all-roots dirty on WriteApplied");
        assert_eq!(row_a.content_hash.as_deref(), Some("ha1"));
        assert_eq!(row_a.last_event_id.as_deref(), Some("ev-root-a"), "attribution preserved");

        let row_b = store.lookup(ROOT_B, "a.md", &token()).await.expect("lookup").expect("row b");
        assert_eq!(row_b.project_root_hash, ROOT_B, "root folds from the ToolExecuted payload");
        assert!(row_b.dirty, "all-roots dirty on WriteApplied");
        assert_eq!(row_b.content_hash.as_deref(), Some("hb1"));
        assert_eq!(row_b.last_event_id.as_deref(), Some("ev-root-b"), "attribution preserved");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM resource_facts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 2, "one row per root, no cross-root clobbering");
    }
}
