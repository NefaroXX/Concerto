//! ADR-65 §3 — tool-level evidence facts.
//!
//! This module is the **only** place the orchestrator records completed tool
//! commands as `ToolExecuted` whiteboard events with real agent attribution.
//! Every writer is wired behind [`ToolFactContext`], a tiny (pool + identity)
//! token that callers attach explicitly — a `None` pool makes every operation a
//! fail-soft no-op so evidence can never break a tool call.
//!
//! Contract with the derived store (`concerto-sessions::ResourceFacts`):
//! - **read-only success** → `apply_observed` (clean rows, safe to serve from
//!   cache);
//! - **file-affecting** (write/delete/edit, success *or* failure) →
//!   `invalidate_on_write` (dirty rows; a failed write still mutates state
//!   unpredictably);
//! - **failed read-only** → event only, no derived-store sync.
//!
//! Attribution is never inferred: the caller passes its own `agent_id`,
//! `task_id`, `run_id` and `generation` per call.

use std::collections::HashMap;
use std::path::Path;

use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::append_whiteboard_event;
use concerto_sessions::{
    NewWhiteboardEvent, ObservedPath, ResourceFacts, ToolExecutedPayload, WhiteboardKind,
};
use tracing::warn;

/// Canonical argument JSON up to this size is stored verbatim; larger blobs are
/// replaced by a content fingerprint (`{ "hash": ..., "len": ... }`) so facts
/// stay bounded (ADR-65 §3).
pub const MAX_CANONICAL_ARGS_BYTES: usize = 4096;

/// Files at or below this size get a fresh content hash after tool execution —
/// mirrors the `workspace_snapshot` hashing budget so the walk stays bounded.
pub const MAX_HASH_BYTES: u64 = 64 * 1024;

/// Identity tag for the single-agent loop's facts (no run/task ownership of a
/// multi-agent run; ADR-65 §3 attribution is still explicit).
pub const SINGLE_AGENT_FACT_ID: &str = "single-agent";

/// A per-agent evidence writer token. Cheap to clone and thread through
/// constructors; disabled entirely when the pool is `None`.
#[derive(Clone, Debug)]
pub struct ToolFactContext {
    pool: Option<sqlx::SqlitePool>,
    agent_id: String,
}

impl ToolFactContext {
    /// Wrap a session pool (or `None` for a disabled writer) under `agent_id`.
    pub fn new(pool: Option<sqlx::SqlitePool>, agent_id: impl Into<String>) -> Self {
        Self { pool, agent_id: agent_id.into() }
    }

    /// Re-tag this context for a different agent (used by registry builders
    /// stamping one shared pool onto each specialist).
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = agent_id.into();
        self
    }

    /// The backing pool, so sibling modules (e.g. the read-dedupe cache in
    /// [`crate::read_cache`]) can query the same `resource_facts` store the
    /// writer keeps in sync. `None` when the writer is disabled.
    pub(crate) fn pool(&self) -> Option<&sqlx::SqlitePool> {
        self.pool.as_ref()
    }

    /// Read the pre-write `content_hash` for every path the tool is about to
    /// touch, straight from `resource_facts` (no filesystem access). Missing
    /// rows and lookup errors yield `None`; this is advisory — a `None` merely
    /// means "no clean cached hash to record".
    pub async fn pre_image_hashes(
        &self,
        paths: &[String],
        cancel: &CancellationToken,
    ) -> HashMap<String, Option<String>> {
        let Some(pool) = &self.pool else {
            return HashMap::new();
        };
        let facts = ResourceFacts::new(pool.clone());
        let mut out = HashMap::with_capacity(paths.len());
        for path in paths {
            match facts.lookup(path, cancel).await {
                Ok(Some(row)) => {
                    out.insert(path.clone(), row.content_hash);
                }
                Ok(None) => {
                    out.insert(path.clone(), None);
                }
                Err(err) => {
                    warn!(%path, %err, agent_id = %self.agent_id, "tool fact: pre-image lookup failed");
                    out.insert(path.clone(), None);
                }
            }
        }
        out
    }

    /// Record a completed tool execution as a `ToolExecuted` whiteboard event
    /// and sync the derived `resource_facts` store. **Never fails the caller**:
    /// every persistence error degrades to a warning.
    pub async fn record_tool_executed(
        &self,
        fact: &ToolExecutedFact<'_>,
        cancel: &CancellationToken,
    ) {
        let Some(pool) = &self.pool else {
            return;
        };

        // Fresh post-execution observation of the affected paths, so the event
        // carries what the workspace looked like after the tool ran.
        let observed = observe_paths(fact.project_root, fact.paths).await;
        let event_id = Ulid::new().to_string();
        let created_at = unix_ms();

        let event = NewWhiteboardEvent {
            event_id: event_id.clone(),
            agent_id: self.agent_id.clone(),
            kind: WhiteboardKind::ToolExecuted,
            scope: String::new(),
            session_id: Some(fact.session_id.to_owned()),
            plan_id: None,
            causation: None,
            // ADR-65 §3: attribution and evidence live in the typed payload;
            // `pre_image_hash` carries the pre-write hash of a single-path
            // file-affecting write (None for multi-path or read-only tools).
            payload: serde_json::json!({
                "agent_id": fact.task_attribution().map_or_else(
                    || self.agent_id.clone(),
                    |agent_id| agent_id.to_owned()
                ),
                "task_id": fact.task_id,
                "run_id": fact.run_id,
                "tool": fact.tool,
                "args": canonical_args(fact.args),
                "success": fact.success,
                "exit_code": fact.exit_code,
                "generation": fact.generation,
                "paths": serde_json::to_value(&observed).unwrap_or_else(|_| serde_json::json!([])),
            }),
            pre_image_hash: single_path_pre_image(fact),
            created_at,
        };

        if let Err(err) = append_whiteboard_event(pool, &event).await {
            warn!(
                tool = fact.tool,
                agent_id = %self.agent_id,
                %err,
                "tool fact: whiteboard append failed; fact dropped"
            );
            return;
        }

        // Derive the store from the log entry (log remains the source of truth).
        let facts = ResourceFacts::new(pool.clone());
        if fact.file_affecting {
            if let Err(err) = facts.invalidate_on_write(fact.paths, cancel).await {
                warn!(
                    tool = fact.tool,
                    agent_id = %self.agent_id,
                    %err,
                    "tool fact: invalidate_on_write failed"
                );
            }
        } else if fact.success {
            let payload = ToolExecutedPayload {
                agent_id: Some(self.agent_id.clone()),
                task_id: fact.task_id.map(str::to_owned),
                run_id: fact.run_id.map(str::to_owned),
                tool: fact.tool.to_owned(),
                args: canonical_args(fact.args),
                success: fact.success,
                exit_code: fact.exit_code,
                generation: fact.generation.to_owned(),
                paths: observed,
                // ADR-65 §4: an ordinary executed tool call is never a cache
                // serve — served reads are recorded via `record_served_read`.
                served_from: None,
            };
            if let Err(err) =
                facts.apply_observed(&event_id, &self.agent_id, created_at, &payload, cancel).await
            {
                warn!(
                    tool = fact.tool,
                    agent_id = %self.agent_id,
                    %err,
                    "tool fact: apply_observed failed"
                );
            }
        }
    }

    /// ADR-65 §4: record that a read was **served from cache** rather than
    /// executed. The `served_from` fact is a `ToolExecuted` whiteboard event
    /// with `served_from` set to the verified clean row's `last_event_id` and
    /// **empty `paths`** — reconstructing paths from `list_observations` could
    /// fold a possibly-empty `generation` and clobber the clean row's
    /// generation, so the served fact deliberately does NOT re-sync the
    /// derived store. Fail-soft: never fails the caller.
    pub async fn record_served_read(
        &self,
        fact: &ToolExecutedFact<'_>,
        served_from: &str,
        cancel: &CancellationToken,
    ) {
        // Cancellation is intentionally not consulted: appending the served
        // fact is a single bounded write straight after an already-completed
        // tool result; the caller's cancel token was already honored when the
        // serve decision was made.
        let _ = cancel;
        let Some(pool) = &self.pool else {
            return;
        };
        let event_id = Ulid::new().to_string();
        let event = NewWhiteboardEvent {
            event_id: event_id.clone(),
            agent_id: self.agent_id.clone(),
            kind: WhiteboardKind::ToolExecuted,
            scope: String::new(),
            session_id: Some(fact.session_id.to_owned()),
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "agent_id": fact.task_attribution().map_or_else(
                    || self.agent_id.clone(),
                    |agent_id| agent_id.to_owned()
                ),
                "task_id": fact.task_id,
                "run_id": fact.run_id,
                "tool": fact.tool,
                "args": canonical_args(fact.args),
                "success": true,
                "exit_code": serde_json::Value::Null,
                "generation": fact.generation,
                "paths": serde_json::json!([]),
                "served_from": served_from,
            }),
            pre_image_hash: None,
            created_at: unix_ms(),
        };
        if let Err(err) = append_whiteboard_event(pool, &event).await {
            warn!(
                tool = fact.tool,
                agent_id = %self.agent_id,
                %err,
                "tool fact: served read append failed; fact dropped"
            );
        }
    }

    /// ADR-65 §4: cache the exact bytes of a successful plain filesystem read
    /// into `resource_facts` content cache, so an identical later read can be
    /// served without re-reading the disk. Runs AFTER `record_tool_executed`
    /// has applied the observation (the row must already exist — this method
    /// only ever attaches content to an existing clean row). Fail-soft and
    /// bounded: no row, NUL bytes, or over-limit content all no-op silently.
    pub async fn cache_read_content(&self, path: &str, content: &str, cancel: &CancellationToken) {
        let Some(pool) = &self.pool else {
            return;
        };
        let facts = ResourceFacts::new(pool.clone());
        if let Err(err) = facts.store_read_content(path, content, cancel).await {
            warn!(
                path,
                agent_id = %self.agent_id,
                %err,
                "tool fact: read content cache write failed; cache skipped"
            );
        }
    }
}

/// One completed tool execution, captured at the call site where identity,
/// arguments and outcome are all known (ADR-65 §3 "never inferred").
#[derive(Debug)]
pub struct ToolExecutedFact<'a> {
    /// The owning session (whiteboard partition key).
    pub session_id: &'a str,
    /// Explicit task/run attribution; `None` when the caller has no such
    /// concept (single-agent loop) rather than inventing one.
    pub task_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    /// Workspace `generation` (content-addressed id, ADR-65) at execution time;
    /// empty when the caller has no snapshot barrier.
    pub generation: &'a str,
    /// Project root used to resolve relative paths before stat/hash.
    pub project_root: &'a Path,
    /// The tool's registered name.
    pub tool: &'a str,
    /// The exact arguments passed to the tool.
    pub args: &'a serde_json::Value,
    /// Whether execution completed successfully (policy denial is `false`).
    pub success: bool,
    /// Exit code when available (shell/git); `None` otherwise.
    pub exit_code: Option<i32>,
    /// Paths this execution affected/observed, in the tool's own naming.
    pub paths: &'a [String],
    /// True for write/delete/edit tools: derived state becomes dirty even on
    /// failure.
    pub file_affecting: bool,
    /// Pre-write content hashes captured before execution
    /// ([`ToolFactContext::pre_image_hashes`]).
    pub pre_image_hashes: HashMap<String, Option<String>>,
}

impl ToolExecutedFact<'_> {
    /// The writer records the tool's task attribution when present; the event's
    /// `agent_id` column always carries the producing agent (never inferred).
    fn task_attribution(&self) -> Option<&str> {
        self.task_id
    }
}

/// Serialize arguments compactly; replace oversized blobs with a content
/// fingerprint so facts stay bounded.
fn canonical_args(args: &serde_json::Value) -> serde_json::Value {
    match serde_json::to_string(args) {
        Ok(compact) if compact.len() <= MAX_CANONICAL_ARGS_BYTES => args.clone(),
        Ok(compact) => serde_json::json!({
            "hash": blake3::hash(compact.as_bytes()).to_hex().to_string(),
            "len": compact.len(),
        }),
        Err(_) => serde_json::json!({}),
    }
}

/// The event column `pre_image_hash` — the pre-write hash of a single-path
/// file-affecting write; `None` for multi-path or read-only tools.
///
/// A tool often reports its own naming of the target (the key the pre-image
/// map uses) plus the resolved absolute path echoed in its output — both
/// spellings of the SAME file. Counting reported paths that actually carry a
/// recorded pre-image distinguishes that one-target case from a genuine
/// multi-target write (which records several), so the event column stays
/// hash-less unless exactly one target was written.
fn single_path_pre_image(fact: &ToolExecutedFact<'_>) -> Option<String> {
    if !fact.file_affecting {
        return None;
    }
    let recorded: Vec<&str> = fact
        .paths
        .iter()
        .filter(|path| fact.pre_image_hashes.get(*path).map(|h| h.is_some()).unwrap_or(false))
        .map(String::as_str)
        .collect();
    match recorded.as_slice() {
        [raw] => fact.pre_image_hashes.get(*raw).and_then(|h| h.clone()),
        _ => None,
    }
}

/// Post-execution observations of the affected paths: size + mtime always,
/// content hash for files ≤ [`MAX_HASH_BYTES`]. Paths that can no longer be
/// resolved (deleted by the tool) are omitted from the observation.
async fn observe_paths(project_root: &Path, paths: &[String]) -> Vec<ObservedPath> {
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let resolved = resolve_path(project_root, raw);
        let Ok(meta) = std::fs::metadata(&resolved) else {
            continue;
        };
        let content_hash = if meta.len() <= MAX_HASH_BYTES {
            std::fs::read(&resolved).ok().map(|bytes| blake3::hash(&bytes).to_hex().to_string())
        } else {
            None
        };
        out.push(ObservedPath {
            path: raw.clone(),
            size_bytes: Some(meta.len()),
            mtime_ms: mtime_ms(&meta),
            content_hash,
        });
    }
    out
}

/// Resolve a tool-reported path against the project root when relative.
pub(crate) fn resolve_path(project_root: &Path, raw: &str) -> std::path::PathBuf {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    }
}

/// mtime as unix-epoch milliseconds (UTC-agnostic epoch time, the store's
/// convention).
pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    modified.duration_since(std::time::UNIX_EPOCH).ok().map(|duration| duration.as_millis() as u64)
}

/// Unix-epoch milliseconds for `created_at` timestamps.
pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Whether a tool command is file-affecting (write/delete/edit), using the
/// same defensive grammar the single-agent loop already uses for its
/// file-change accounting: dedicated write tools, or the `filesystem` tool
/// with a mutating operation. Side-effecting shells/git are intentionally out
/// of scope — their writes are surfaced by `WriteApplied` gate events instead.
pub(crate) fn is_file_affecting_tool(tool: &str, args: &serde_json::Value) -> bool {
    let operation = args.get("operation").and_then(|v| v.as_str());
    matches!(tool, "write_file" | "delete_file" | "edit_file" | "create_file" | "modify_file")
        || (tool == "filesystem" && matches!(operation, Some("write" | "delete" | "move" | "copy")))
}

/// Extract the paths a tool command touches, using the same defensive grammar
/// as the derived store's `write_applied_paths`: `pre_images` map keys when
/// present, else `path` / `target` / `input.path`. Output data (when the tool
/// returned) additionally contributes `absolute_path` / `path` / `file_path`
/// which are authoritative for e.g. `write_file` full-path results.
pub(crate) fn extract_affected_paths(
    args: &serde_json::Value,
    output_data: Option<&serde_json::Value>,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(pre_images) = args.get("pre_images").and_then(|v| v.as_object()) {
        paths.extend(pre_images.keys().cloned());
    } else {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            paths.push(path.to_owned());
        } else if let Some(target) = args.get("target").and_then(|v| v.as_str()) {
            paths.push(target.to_owned());
        } else if let Some(input) = args.get("input").and_then(|v| v.as_object()) {
            if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                paths.push(path.to_owned());
            }
        }
    }
    if let Some(data) = output_data {
        for key in ["absolute_path", "path", "file_path"] {
            if let Some(value) = data.get(key).and_then(|v| v.as_str()) {
                if !paths.iter().any(|existing| existing == value) {
                    paths.push(value.to_owned());
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use concerto_sessions::whiteboard::{load_whiteboard_events, WhiteboardLoadOpts};
    use concerto_sessions::WhiteboardEvent;

    use super::*;

    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let db_path = dir.path().join("tool_facts_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations")
            .run(&pool)
            .await
            .expect("sessions migrations apply");
        (dir, pool)
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    async fn load_tool_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
        load_whiteboard_events(
            pool,
            &WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: Some("session-1".to_owned()),
                scope: None,
                limit: 200,
            },
        )
        .await
        .expect("load whiteboard events")
    }

    fn facts_for(pool: &sqlx::SqlitePool) -> (ToolFactContext, ResourceFacts) {
        (ToolFactContext::new(Some(pool.clone()), "writer-agent"), ResourceFacts::new(pool.clone()))
    }

    #[tokio::test]
    async fn records_full_payload_and_clean_observation_for_read_success() {
        let (_dir, pool) = test_pool().await;
        let (ctx, store) = facts_for(&pool);
        let root = tempfile::tempdir().expect("tempdir created");
        std::fs::write(root.path().join("a.md"), b"hello").expect("write fixture");
        let cancel = cancel();

        ctx.record_tool_executed(
            &ToolExecutedFact {
                session_id: "session-1",
                task_id: Some("task-9"),
                run_id: Some("run-2"),
                generation: "gen-abc",
                project_root: root.path(),
                tool: "filesystem",
                args: &serde_json::json!({ "operation": "read", "path": "a.md" }),
                success: true,
                exit_code: None,
                paths: &["a.md".to_owned()],
                file_affecting: false,
                pre_image_hashes: HashMap::new(),
            },
            &cancel,
        )
        .await;

        let events = load_tool_events(&pool).await;
        assert_eq!(events.len(), 1, "one ToolExecuted event appended");

        let event = &events[0];
        assert_eq!(event.agent_id, "writer-agent", "agent column carries the producer id");
        assert_eq!(event.session_id.as_deref(), Some("session-1"), "session partition preserved");

        let payload = &event.payload;
        assert_eq!(payload["agent_id"], "task-9", "agent attribution from the task");
        assert_eq!(payload["task_id"], "task-9");
        assert_eq!(payload["run_id"], "run-2");
        assert_eq!(payload["tool"], "filesystem");
        assert_eq!(payload["success"], true);
        assert_eq!(payload["generation"], "gen-abc");
        assert_eq!(payload["args"]["operation"], "read");
        let paths = payload["paths"].as_array().expect("paths array");
        assert_eq!(paths.len(), 1, "affected path observed post-execution");
        assert_eq!(paths[0]["path"], "a.md");
        assert_eq!(
            paths[0]["content_hash"].as_str().map(|hash| hash.to_owned()),
            Some(blake3::hash(b"hello").to_hex().to_string()),
            "read file content-hashed"
        );
        assert!(event.pre_image_hash.is_none(), "read-only tool carries no pre-image hash");

        // Read-only success → clean row, safe to serve from cache.
        let row =
            store.lookup("a.md", &cancel).await.expect("lookup succeeds").expect("row observed");
        assert!(!row.dirty, "read-only success brands the row clean");
        assert_eq!(row.generation, "gen-abc");
        assert_eq!(row.last_agent_id.as_deref(), Some("writer-agent"));
    }

    #[tokio::test]
    async fn file_affecting_success_dirties_rows_and_carries_pre_image_hash() {
        let (_dir, pool) = test_pool().await;
        let (ctx, store) = facts_for(&pool);
        let root = tempfile::tempdir().expect("tempdir created");
        let target = root.path().join("b.rs");
        std::fs::write(&target, b"old").expect("write fixture");
        let cancel = cancel();

        // Seed a clean cached fact, then record a successful write.
        store
            .apply_observed(
                "evt-0",
                "seeder",
                unix_ms(),
                &ToolExecutedPayload {
                    agent_id: Some("seeder".to_owned()),
                    task_id: None,
                    run_id: None,
                    tool: "filesystem".to_owned(),
                    args: serde_json::json!({}),
                    success: true,
                    exit_code: None,
                    generation: "g0".to_owned(),
                    paths: vec![ObservedPath {
                        path: "b.rs".to_owned(),
                        size_bytes: Some(3),
                        mtime_ms: Some(1),
                        content_hash: Some("pre".to_owned()),
                    }],
                    served_from: None,
                },
                &cancel,
            )
            .await
            .expect("seed applies");

        let pre_images = ctx.pre_image_hashes(&["b.rs".to_owned()], &cancel).await;
        assert_eq!(
            pre_images.get("b.rs").and_then(|hash| hash.clone()).as_deref(),
            Some("pre"),
            "pre-image hash read from the derived store"
        );

        std::fs::write(&target, b"new content").expect("mutate fixture");

        ctx.record_tool_executed(
            &ToolExecutedFact {
                session_id: "session-1",
                task_id: None,
                run_id: None,
                generation: "g1",
                project_root: root.path(),
                tool: "write_file",
                args: &serde_json::json!({ "path": "b.rs", "content": "new content" }),
                success: true,
                exit_code: None,
                paths: &["b.rs".to_owned()],
                file_affecting: true,
                pre_image_hashes: pre_images,
            },
            &cancel,
        )
        .await;

        let events = load_tool_events(&pool).await;
        let event = events
            .iter()
            .find(|event| event.kind == WhiteboardKind::ToolExecuted)
            .expect("ToolExecuted event present");
        assert_eq!(
            event.pre_image_hash.as_deref(),
            Some("pre"),
            "single-path write carries the pre-write hash"
        );

        let row = store.lookup("b.rs", &cancel).await.expect("lookup").expect("row present");
        assert!(row.dirty, "file-affecting success dirties the row");
    }

    #[tokio::test]
    async fn failed_tools_are_recorded_with_success_false() {
        let (_dir, pool) = test_pool().await;
        let (ctx, _store) = facts_for(&pool);
        let root = tempfile::tempdir().expect("tempdir created");
        std::fs::write(root.path().join("c.txt"), b"x").expect("write fixture");
        let cancel = cancel();

        // A failed read-only tool: event recorded (audit trail) but no sync.
        ctx.record_tool_executed(
            &ToolExecutedFact {
                session_id: "session-1",
                task_id: None,
                run_id: Some("run-1"),
                generation: "",
                project_root: root.path(),
                tool: "filesystem",
                args: &serde_json::json!({ "operation": "read", "path": "c.txt" }),
                success: false,
                exit_code: Some(1),
                paths: &["c.txt".to_owned()],
                file_affecting: false,
                pre_image_hashes: HashMap::new(),
            },
            &cancel,
        )
        .await;

        let events = load_tool_events(&pool).await;
        assert_eq!(events.len(), 1, "failed execution still recorded");
        assert_eq!(events[0].payload["success"], false);
        assert_eq!(events[0].payload["exit_code"], 1);
    }

    #[tokio::test]
    async fn disabled_writer_is_a_fail_soft_noop() {
        let ctx = ToolFactContext::new(None, "ghost");
        let cancel = cancel();
        ctx.record_tool_executed(
            &ToolExecutedFact {
                session_id: "session-1",
                task_id: None,
                run_id: None,
                generation: "",
                project_root: Path::new("."),
                tool: "anything",
                args: &serde_json::json!({}),
                success: true,
                exit_code: None,
                paths: &["x".to_owned()],
                file_affecting: true,
                pre_image_hashes: HashMap::new(),
            },
            &cancel,
        )
        .await;
        // No panic, nothing persisted (no pool to persist to).
    }

    #[test]
    fn oversized_args_become_a_fingerprint() {
        let big = serde_json::json!({ "content": "x".repeat(MAX_CANONICAL_ARGS_BYTES + 1) });
        let compact = serde_json::to_string(&big).expect("json serializes");
        let canon = canonical_args(&big);
        assert!(canon.get("hash").is_some(), "oversized args fingerprinted");
        assert_eq!(
            canon["len"].as_u64(),
            Some(compact.len() as u64),
            "len reflects the serialized byte size (content plus JSON framing)"
        );

        let small = serde_json::json!({ "a": 1 });
        assert_eq!(canonical_args(&small), small, "small args stored verbatim");
    }

    #[test]
    fn pre_image_column_only_for_single_path_writes() {
        let root = Path::new(".");
        let args = serde_json::json!({});
        fn make<'a>(
            file_affecting: bool,
            paths: &'a [String],
            root: &'a Path,
            args: &'a serde_json::Value,
            pre_images: &'a HashMap<String, Option<String>>,
        ) -> ToolExecutedFact<'a> {
            ToolExecutedFact {
                session_id: "s",
                task_id: None,
                run_id: None,
                generation: "",
                project_root: root,
                tool: "t",
                args,
                success: true,
                exit_code: None,
                paths,
                file_affecting,
                pre_image_hashes: pre_images.clone(),
            }
        }
        let single = HashMap::from([("one.md".to_owned(), Some("h".to_owned()))]);
        assert_eq!(
            single_path_pre_image(&make(true, &["one.md".to_owned()], root, &args, &single))
                .as_deref(),
            Some("h"),
            "single-path write carries pre-image"
        );
        // The tool echoes the resolved absolute path of the SAME target in its
        // output; the pre-image map is keyed by the tool's own naming, so the
        // event still carries exactly one recorded pre-image.
        let abs = Path::new("/sandbox").join("one.md");
        let abs_str = abs.to_string_lossy().into_owned();
        assert_eq!(
            single_path_pre_image(&make(
                true,
                &["one.md".to_owned(), abs_str],
                root,
                &args,
                &single
            ))
            .as_deref(),
            Some("h"),
            "raw + resolved spelling of one target still yields the pre-image"
        );
        let multi = HashMap::from([
            ("one.md".to_owned(), Some("h".to_owned())),
            ("two.md".to_owned(), Some("h2".to_owned())),
        ]);
        assert_eq!(
            single_path_pre_image(&make(
                true,
                &["one.md".to_owned(), "two.md".to_owned()],
                root,
                &args,
                &multi
            )),
            None,
            "multi-target write has no pre-image hash"
        );
        assert_eq!(
            single_path_pre_image(&make(false, &["one.md".to_owned()], root, &args, &single)),
            None,
            "read-only tool has no pre-image hash"
        );
    }
}
