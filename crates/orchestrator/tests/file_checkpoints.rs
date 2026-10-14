//! ADR-82 slice 5 + chunked completion: durable file checkpoints with honest
//! restore, end-to-end.
//!
//! Drives the *real* [`WriteGate`] (with the checkpoint sidecar attached) and
//! the *real* `FilesystemTool` through the specified verification flow:
//!
//! - write → checkpoint → mutate → restore returns byte-exact content (text,
//!   multi-chunk text, and binary alike — the sidecar banks
//!   `CHECKPOINT_BLOB_LIMIT_BYTES`-capped chunks of arbitrary bytes up to
//!   the per-checkpoint `CHECKPOINT_TOTAL_LIMIT_BYTES` budget);
//! - ownership-conflict restore is refused and surfaced (no silent overwrite);
//! - double restore is a recorded no-op;
//! - over-total-budget content degrades to `Manual` + `Partial` with
//!   limitations (hash-only, never a silent skip);
//! - the gate seam backfills a missed creation snapshot;
//! - the executor's direct-path hook captures without the gate;
//! - corrupt/missing/tampered chunk sets restore `Partial` (never a success
//!   claim over unverified bytes);
//! - retention prunes sidecar bytes (chunks included) and transitions the
//!   checkpoint to `Expired` (restorable claim withdrawn; restore refuses).
//!
//! Honest ceilings that remain: the per-checkpoint total budget and
//! symlink/permission/xattr metadata (never captured).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::checkpoint::{
    CheckpointExclusion, CheckpointExclusionClass, CheckpointScope, CheckpointStatus,
    RestoreMethod, CHECKPOINT_BLOB_LIMIT_BYTES, CHECKPOINT_TOTAL_LIMIT_BYTES,
};
use concerto_core::error::PolicyError;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::AuditLog;
use concerto_core::types::{Condition, PolicyRule, SessionContext, ToolRegistry};
use concerto_core::{CancellationToken, TaskId};
use concerto_orchestrator::file_checkpoint::{create_file_checkpoint, restore_file_checkpoint};
use concerto_orchestrator::gate::{FilePreImageReader, GateRequest, WriteGate};
use concerto_sessions::file_checkpoints::FileCheckpointStore;
use concerto_tools::filesystem::FilesystemTool;
use serde_json::json;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use tempfile::TempDir;

/// No-op audit log (mirrors the supervisor suites).
struct TestAudit;

#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: concerto_core::traits::policy::AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

async fn test_pool() -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("ckpt_test.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

/// One run's wiring: project dir, pool, sidecar store, an executor whose
/// registry holds the REAL `FilesystemTool` (with the direct-path capture
/// sink attached), and the gate with the sidecar seam attached.
struct Harness {
    _dir: TempDir,
    _db_dir: TempDir,
    root: PathBuf,
    pool: sqlx::SqlitePool,
    store: FileCheckpointStore,
    executor: Arc<ToolExecutor>,
    gate: Arc<WriteGate>,
    session_id: Ulid,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("project tempdir");
        let root = dir.path().to_path_buf();
        let (_db_dir, pool) = test_pool().await;
        let store = FileCheckpointStore::new(pool.clone());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(
            camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
        )));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), allow_engine())
                .with_mutation_sink(Arc::new(store.clone())),
        );
        let gate = Arc::new(
            WriteGate::new(
                allow_engine(),
                executor.clone(),
                pool.clone(),
                Arc::new(FilePreImageReader::new(root.clone())),
                root.clone(),
                1,
            )
            .with_checkpoint_store(store.clone()),
        );
        Self { _dir: dir, _db_dir, root, pool, store, executor, gate, session_id: Ulid::new() }
    }

    fn session(&self) -> SessionContext {
        SessionContext::new(self.session_id, self.root.clone())
    }

    fn write_request(&self, call_id: &str, agent: &str, path: &str, content: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: agent.to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({"operation": "write", "path": path, "content": content}),
            registered_as: None,
            session_id: Some(self.session_id.to_string()),
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
            orchestrator_authority: false,
        }
    }

    async fn gated_write(&self, call_id: &str, agent: &str, path: &str, content: &str) {
        self.gate
            .submit(self.write_request(call_id, agent, path, content), CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("gated write {call_id} must succeed: {error:?}"));
    }

    /// Gated binary write through the `content_b64` channel: exercises the
    /// gate (ownership, conflict checks, pre-image cache) with arbitrary
    /// bytes, exactly as checkpoint restore replays them.
    async fn gated_write_bytes(&self, call_id: &str, agent: &str, path: &str, bytes: &[u8]) {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let request = GateRequest {
            call_id: call_id.to_owned(),
            agent_id: agent.to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({"operation": "write", "path": path, "content_b64": b64}),
            registered_as: None,
            session_id: Some(self.session_id.to_string()),
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
            orchestrator_authority: false,
        };
        self.gate
            .submit(request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("gated bytes write {call_id} must succeed: {error:?}"));
    }

    fn disk(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).expect("read disk file")
    }

    fn disk_bytes(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root.join(path)).expect("read disk bytes")
    }
}

fn fs_scope(paths: &[&str]) -> CheckpointScope {
    CheckpointScope::FilesystemWrites { paths: paths.iter().map(|p| (*p).to_owned()).collect() }
}

/// Sidecar store failing one path's byte load exactly once (the deterministic
/// crash: the gated prefix lands, the rest never runs), then delegating every
/// method — journal included — to the real store. Shared by the interrupt and
/// crash-restart tests.
struct FailBytesStore {
    inner: FileCheckpointStore,
    fail_path: String,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl concerto_core::checkpoint::CheckpointStore for FailBytesStore {
    async fn load_record(
        &self,
        checkpoint_id: &str,
        cancel: CancellationToken,
    ) -> Result<
        Option<concerto_core::checkpoint::CheckpointRecord>,
        concerto_core::checkpoint::CheckpointStoreError,
    > {
        self.inner.load_record(checkpoint_id, cancel).await
    }

    async fn load_bytes(
        &self,
        checkpoint_id: &str,
        canonical_path: &str,
        cancel: CancellationToken,
    ) -> Result<
        concerto_core::checkpoint::PreImageBytes,
        concerto_core::checkpoint::CheckpointStoreError,
    > {
        if canonical_path == self.fail_path
            && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(concerto_core::checkpoint::CheckpointStoreError::Storage(
                "injected mid-restore byte loss".to_string(),
            ));
        }
        self.inner.load_bytes(checkpoint_id, canonical_path, cancel).await
    }

    async fn mark_restored(
        &self,
        checkpoint_id: &str,
        at_ms: i64,
        by: &str,
        cancel: CancellationToken,
    ) -> Result<bool, concerto_core::checkpoint::CheckpointStoreError> {
        self.inner.mark_restored(checkpoint_id, at_ms, by, cancel).await
    }

    async fn mark_partial(
        &self,
        checkpoint_id: &str,
        missing: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<(), concerto_core::checkpoint::CheckpointStoreError> {
        self.inner.mark_partial(checkpoint_id, missing, cancel).await
    }

    async fn load_restore_progress(
        &self,
        checkpoint_id: &str,
        cancel: CancellationToken,
    ) -> Result<
        Vec<concerto_core::checkpoint::RestoreProgressEntry>,
        concerto_core::checkpoint::CheckpointStoreError,
    > {
        concerto_core::checkpoint::CheckpointStore::load_restore_progress(
            &self.inner,
            checkpoint_id,
            cancel,
        )
        .await
    }

    async fn record_restore_progress(
        &self,
        checkpoint_id: &str,
        entry: &concerto_core::checkpoint::RestoreProgressEntry,
        cancel: CancellationToken,
    ) -> Result<(), concerto_core::checkpoint::CheckpointStoreError> {
        concerto_core::checkpoint::CheckpointStore::record_restore_progress(
            &self.inner,
            checkpoint_id,
            entry,
            cancel,
        )
        .await
    }

    async fn begin_restore_attempt(
        &self,
        attempt: &concerto_core::checkpoint::RestoreAttemptInfo,
        cancel: CancellationToken,
    ) -> Result<(), concerto_core::checkpoint::CheckpointStoreError> {
        concerto_core::checkpoint::CheckpointStore::begin_restore_attempt(
            &self.inner,
            attempt,
            cancel,
        )
        .await
    }

    async fn finish_restore_attempt(
        &self,
        attempt_id: &str,
        outcome: concerto_core::checkpoint::RestoreAttemptOutcome,
        finished_at_ms: i64,
        cancel: CancellationToken,
    ) -> Result<(), concerto_core::checkpoint::CheckpointStoreError> {
        concerto_core::checkpoint::CheckpointStore::finish_restore_attempt(
            &self.inner,
            attempt_id,
            outcome,
            finished_at_ms,
            cancel,
        )
        .await
    }
}

/// No-op stash restorer (a PreImageStore record never consults it).
#[derive(Debug)]
struct NoopStash;

impl concerto_core::checkpoint::StashRestorer for NoopStash {
    fn restore_stash(
        &self,
        _checkpoint: &concerto_core::checkpoint::CheckpointRecord,
    ) -> Result<String, String> {
        Err("not a stash checkpoint".to_string())
    }
}

/// Write recorder: delegates every `filesystem` call to the real
/// `FilesystemTool` (same name, schema, capabilities, path facts, policy
/// view — the executor cannot tell the difference) and records every
/// MUTATING operation it executes. Register it in place of the plain tool to
/// prove which paths an executor actually touched — e.g. that a verified
/// resume skips a resource's WRITE, not merely that its bytes matched.
struct RecordingFsTool {
    inner: FilesystemTool,
    ops: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl concerto_core::traits::tool::Tool for RecordingFsTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }

    fn capability_requirements(&self) -> concerto_core::types::CapabilitySet {
        self.inner.capability_requirements()
    }

    fn path_facts(
        &self,
        input: &serde_json::Value,
        session: &SessionContext,
    ) -> Option<concerto_core::types::PathPolicyFacts> {
        self.inner.path_facts(input, session)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        policy: &dyn concerto_core::traits::PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<concerto_core::types::ToolOutput, concerto_core::error::ToolError> {
        let op = input.get("operation").and_then(|value| value.as_str()).unwrap_or_default();
        let path = input.get("path").and_then(|value| value.as_str()).unwrap_or_default();
        if matches!(op, "write" | "delete" | "move" | "copy") {
            self.ops.lock().expect("ops lock").push(format!("{op} {path}"));
        }
        self.inner.execute(input, policy, session, cancel).await
    }
}

/// The specified flow: write → checkpoint → mutate → restore is byte-exact.
#[tokio::test]
async fn write_checkpoint_mutate_restore_is_byte_exact() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "notes.txt", "version one").await;

    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["notes.txt"]),
        vec!["notes.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    assert!(matches!(record.status, CheckpointStatus::Active));
    assert_eq!(record.restore_method, RestoreMethod::PreImageStore);
    assert!(record.excluded.is_empty());

    h.gated_write("w-2", "agent-a", "notes.txt", "version two").await;
    assert_eq!(h.disk("notes.txt"), "version two");

    let outcome = restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds");
    match outcome {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["notes.txt".to_string()]);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("notes.txt"), "version one", "byte-exact restore");

    // The status is now Restored: a second restore is a recorded no-op.
    let again = restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("re-restore surfaces");
    match again {
        concerto_core::checkpoint::RestoreOutcome::AlreadyRestored { by, .. } => {
            assert_eq!(by, "agent-a");
        }
        other => panic!("expected AlreadyRestored, got {other:?}"),
    }
    assert_eq!(h.disk("notes.txt"), "version one", "no-op writes nothing");
}

/// A foreign-owned path aborts the whole restore before any write.
#[tokio::test]
async fn ownership_conflict_restore_is_refused_and_surfaces() {
    let h = Harness::new().await;
    // Agent-b writes first: it owns the artifact from here on.
    h.gated_write("w-b", "agent-b", "shared.txt", "b-content").await;
    // Agent-a checkpoints the current (b-owned) state.
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["shared.txt"]),
        vec!["shared.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(concerto_core::checkpoint::RestoreError::OwnershipConflict(conflicts)) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "shared.txt");
            assert_eq!(conflicts[0].owner, "agent-b");
        }
        other => panic!("expected OwnershipConflict, got {other:?}"),
    }
    assert_eq!(h.disk("shared.txt"), "b-content", "refused restore writes nothing");
}

/// Over-total-budget content degrades honestly: hash-only + excluded +
/// `Manual` when nothing banked; `Partial` restore of the bankable subset
/// otherwise. Files within the budget (even multi-chunk ones) restore —
/// only the per-checkpoint TOTAL is a ceiling.
#[tokio::test]
async fn over_total_file_degrades_to_manual_with_limitations() {
    let h = Harness::new().await;
    // Genuinely over budget: one byte more than the per-checkpoint total.
    // Seeded directly (no gate payload bloat for megabytes of test data).
    let big_bytes = vec![b'x'; CHECKPOINT_TOTAL_LIMIT_BYTES + 1];
    std::fs::write(h.root.join("big.bin"), &big_bytes).expect("seed over-total file");

    // Pure over-budget checkpoint: nothing bankable → Manual.
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["big.bin"]),
        vec!["big.bin".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    assert_eq!(record.restore_method, RestoreMethod::Manual);
    assert!(
        matches!(record.status, CheckpointStatus::Partial { .. }),
        "hash-only checkpoint is Partial, got {:?}",
        record.status
    );
    assert_eq!(record.excluded.len(), 1);
    assert_eq!(record.excluded[0].class, CheckpointExclusionClass::OversizedFile);
    assert!(
        record.limitations.iter().any(|l| l.contains("CHECKPOINT_TOTAL_LIMIT_BYTES")),
        "the limitation names the real ceiling, got {:?}",
        record.limitations
    );

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("manual surfaces, never fails")
    {
        concerto_core::checkpoint::RestoreOutcome::RequiresOperator { limitations, .. } => {
            assert!(!limitations.is_empty());
        }
        other => panic!("expected RequiresOperator, got {other:?}"),
    }
    assert_eq!(h.disk_bytes("big.bin"), big_bytes, "Manual writes nothing");

    // Mixed checkpoint: the small file restores, the big one reports missing.
    h.gated_write("w-small", "agent-a", "small.txt", "small-v1").await;
    let mixed = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["small.txt", "big.bin"]),
        vec!["small.txt".to_string(), "big.bin".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("mixed checkpoint");
    assert_eq!(mixed.restore_method, RestoreMethod::PreImageStore);
    h.gated_write("w-small-2", "agent-a", "small.txt", "small-v2").await;

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &mixed.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("partial restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Partial {
            restored,
            missing,
            limitations,
            ..
        } => {
            assert_eq!(restored, vec!["small.txt".to_string()]);
            assert_eq!(missing, vec!["big.bin".to_string()]);
            assert!(limitations.iter().any(|l| l.contains("big.bin")));
        }
        other => panic!("expected Partial, got {other:?}"),
    }
    assert_eq!(h.disk("small.txt"), "small-v1");
}

/// The gate seam (a) backfills a missed creation snapshot: with the snapshot
/// row removed, the next gated mutation's pre-image (identical here — no
/// interleave) restores byte-exact, with pre-image provenance noted.
#[tokio::test]
async fn gate_seam_backfills_a_missed_creation_snapshot() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "seam.txt", "seam-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["seam.txt"]),
        vec!["seam.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    // Simulate the missed snapshot: drop the banked chunk rows (new
    // checkpoints bank chunks only — the legacy single-BLOB table holds no
    // row for them).
    sqlx::query("DELETE FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ?")
        .bind(record.checkpoint_id.to_string())
        .execute(&h.pool)
        .await
        .expect("drop snapshot chunks");

    // The next gated mutation backfills its pre-image (== checkpoint content).
    h.gated_write("w-2", "agent-a", "seam.txt", "seam-v2").await;

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, notes, .. } => {
            assert_eq!(restored, vec!["seam.txt".to_string()]);
            assert!(
                notes.iter().any(|n| n.contains("pre-image")),
                "backfilled bytes carry their provenance, got {notes:?}"
            );
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("seam.txt"), "seam-v1");
}

/// The executor's direct-path hook (b) captures without the gate: an
/// ungated `filesystem` write banks pre-image bytes for an active checkpoint.
#[tokio::test]
async fn direct_path_hook_captures_without_the_gate() {
    let h = Harness::new().await;
    // Seed on disk WITHOUT the gate (no capture runs on a plain write).
    std::fs::write(h.root.join("direct.txt"), "direct-v1").expect("seed");
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["direct.txt"]),
        vec!["direct.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    // Drop the snapshot chunk rows so only the hook can supply bytes.
    sqlx::query("DELETE FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ?")
        .bind(record.checkpoint_id.to_string())
        .execute(&h.pool)
        .await
        .expect("drop snapshot chunks");

    // Ungated write through the executor: the hook backfills pre-image bytes.
    h.executor
        .execute(
            "filesystem",
            json!({"operation": "write", "path": "direct.txt", "content": "direct-v2"}),
            &h.session(),
            CancellationToken::new(),
        )
        .await
        .expect("direct write executes");

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["direct.txt".to_string()]);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("direct.txt"), "direct-v1");
}

/// Retention: an aged Active checkpoint loses its bytes, transitions to
/// `Expired` (claim withdrawn), keeps its envelope — and restore refuses.
#[tokio::test]
async fn retention_expires_aged_checkpoints_and_restore_refuses() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "age.txt", "age-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["age.txt"]),
        vec!["age.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");

    // Age the checkpoint 30 days back.
    let old_ms = concerto_core::checkpoint::now_millis() - 30 * 86_400 * 1000;
    sqlx::query("UPDATE file_checkpoints SET created_at = ? WHERE checkpoint_id = ?")
        .bind(old_ms)
        .bind(record.checkpoint_id.to_string())
        .execute(&h.pool)
        .await
        .expect("age checkpoint");

    let report = h.store.prune_checkpoints(7, &CancellationToken::new()).await.expect("prune");
    assert_eq!(report.expired, 1);
    assert_eq!(report.sidecar_rows_deleted, 1);

    // (e) Retention covers the chunk table too: no chunk row survives.
    let chunks_left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ?",
    )
    .bind(record.checkpoint_id.to_string())
    .fetch_one(&h.pool)
    .await
    .expect("count chunks");
    assert_eq!(chunks_left, 0, "pruned chunks withdraw the restorable claim");

    let back = h
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("envelope survives for audit");
    assert!(
        matches!(back.status, CheckpointStatus::Expired { .. }),
        "pruned-bytes checkpoint reads Expired, got {:?}",
        back.status
    );

    h.gated_write("w-2", "agent-a", "age.txt", "age-v2").await;
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(concerto_core::checkpoint::RestoreError::NotRestorable { .. }) => {}
        other => panic!("expired checkpoint must refuse, got {other:?}"),
    }
    assert_eq!(h.disk("age.txt"), "age-v2", "refused restore writes nothing");
}

/// Caller-declared non-file effects are recorded honestly and never reversed.
#[tokio::test]
async fn declared_side_effects_are_excluded_never_reversed() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "f.txt", "v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["f.txt"]),
        vec!["f.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        vec![
            CheckpointExclusion {
                class: CheckpointExclusionClass::ShellSideEffect,
                path: None,
                reason: "migrated dev database".to_string(),
            },
            CheckpointExclusion {
                class: CheckpointExclusionClass::Network,
                path: None,
                reason: "POSTed a webhook".to_string(),
            },
        ],
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    assert_eq!(record.excluded.len(), 2);
    assert!(record.limitations.iter().any(|l| l.contains("shell side-effect")));
    assert!(record.limitations.iter().any(|l| l.contains("network")));
    assert!(matches!(record.status, CheckpointStatus::Active));

    h.gated_write("w-2", "agent-a", "f.txt", "v2").await;
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["f.txt".to_string()]);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("f.txt"), "v1");
}

/// Git-stash checkpoints delegate restore to the existing `UndoManager`.
#[tokio::test]
async fn git_stash_checkpoint_delegates_to_undo_manager() {
    use concerto_orchestrator::file_checkpoint::create_git_stash_checkpoint;
    use concerto_tools::undo::UndoManager;

    let h = Harness::new().await;
    // Tracked file: stash can round-trip it (untracked-file gap declared).
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&h.root)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&h.root)
        .output()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&h.root)
        .output()
        .expect("git config name");
    std::fs::write(h.root.join("tracked.txt"), "base").expect("seed");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&h.root)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "base"])
        .current_dir(&h.root)
        .output()
        .expect("git commit");

    std::fs::write(h.root.join("tracked.txt"), "dirty").expect("dirty");
    let undo = Arc::new(std::sync::Mutex::new(UndoManager::new(h.root.clone())));
    let task_id = TaskId::new();
    let record = create_git_stash_checkpoint(
        &h.store,
        &undo,
        h.session_id,
        task_id,
        CheckpointScope::Task { task_id },
        "agent-a",
        Vec::new(),
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("stash checkpoint");
    assert_eq!(record.restore_method, RestoreMethod::GitStash);
    assert!(record.limitations.iter().any(|l| l.contains("untracked-file")));

    // Post-checkpoint mutation, then restore pops the stash. A stash pop
    // requires a reconcilable tree (git refuses to overwrite local changes),
    // so discard the post-checkpoint edit first — the pop's loud conflict on
    // a dirty tree is the honest behavior, pinned by the refusal test below.
    std::fs::write(h.root.join("tracked.txt"), "post").expect("post");
    std::process::Command::new("git")
        .args(["checkout", "--", "tracked.txt"])
        .current_dir(&h.root)
        .output()
        .expect("git checkout");
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        Some(&undo),
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("stash restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::StashRestored { .. } => {}
        other => panic!("expected StashRestored, got {other:?}"),
    }
    assert_eq!(h.disk("tracked.txt"), "dirty", "stash pop restores stashed state");
}

/// A stash restore onto a conflicting tree fails loudly (never a silent
/// overwrite): the `ExecutionFailed` surfaces git's refusal verbatim.
#[tokio::test]
async fn git_stash_restore_onto_conflicting_tree_fails_loudly() {
    use concerto_orchestrator::file_checkpoint::create_git_stash_checkpoint;
    use concerto_tools::undo::UndoManager;

    let h = Harness::new().await;
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&h.root)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&h.root)
        .output()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&h.root)
        .output()
        .expect("git config name");
    std::fs::write(h.root.join("tracked.txt"), "base").expect("seed");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&h.root)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "base"])
        .current_dir(&h.root)
        .output()
        .expect("git commit");

    std::fs::write(h.root.join("tracked.txt"), "dirty").expect("dirty");
    let undo = Arc::new(std::sync::Mutex::new(UndoManager::new(h.root.clone())));
    let task_id = TaskId::new();
    let record = create_git_stash_checkpoint(
        &h.store,
        &undo,
        h.session_id,
        task_id,
        CheckpointScope::Task { task_id },
        "agent-a",
        Vec::new(),
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("stash checkpoint");

    // Leave a conflicting local change in place: the pop must refuse.
    std::fs::write(h.root.join("tracked.txt"), "post").expect("post");
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        Some(&undo),
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(concerto_core::checkpoint::RestoreError::ExecutionFailed(message)) => {
            assert!(
                message.contains("would be overwritten"),
                "git's refusal surfaces verbatim, got: {message}"
            );
        }
        other => panic!("conflicting pop must fail loudly, got {other:?}"),
    }
    assert_eq!(h.disk("tracked.txt"), "post", "refused pop touches nothing");
}

/// Policy denial aborts a restore: a denied checkpoint write surfaces
/// `PolicyDenied` and leaves the checkpoint `Active` (retriable).
#[tokio::test]
async fn policy_denial_aborts_restore_loudly() {
    // Deny-all engine for the RESTORE executor only: the checkpoint is built
    // with the allow-all harness, then restored under denial.
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "denied.txt", "v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["denied.txt"]),
        vec!["denied.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    h.gated_write("w-2", "agent-a", "denied.txt", "v2").await;

    let deny_policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoDeny(Condition::Always)],
        Arc::new(TestAudit),
    ));
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(h.root.clone()).expect("utf-8"),
    )));
    let deny_executor = Arc::new(ToolExecutor::new(Arc::new(registry), deny_policy));

    match restore_file_checkpoint(
        &deny_executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(concerto_core::checkpoint::RestoreError::PolicyDenied(_)) => {}
        other => panic!("expected PolicyDenied, got {other:?}"),
    }
    assert_eq!(h.disk("denied.txt"), "v2", "denied restore writes nothing");
    let back = h
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(matches!(back.status, CheckpointStatus::Active), "denial is retriable");
}

/// (a) Binary above the per-chunk size, within the total budget:
/// checkpoint → mutate → restore is BYTE-EXACT (bytes compared, not text).
/// Binary is no longer a limitation — it banks chunks like text and restores
/// through the same gated byte-write path, never `Partial`/`Manual`.
#[tokio::test]
async fn binary_checkpoint_mutate_restore_is_byte_exact() {
    let h = Harness::new().await;
    // Multi-chunk non-UTF-8 bytes: every 251st byte keeps 0x00-0xFA alive
    // (NULs and high bytes throughout — no valid UTF-8 interpretation).
    let original: Vec<u8> =
        (0..CHECKPOINT_BLOB_LIMIT_BYTES * 2 + 12345).map(|i| (i % 251) as u8).collect();
    assert!(original.len() > CHECKPOINT_BLOB_LIMIT_BYTES);
    assert!(std::str::from_utf8(&original).is_err(), "test bytes must be binary");
    std::fs::write(h.root.join("bin.dat"), &original).expect("seed binary");

    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["bin.dat"]),
        vec!["bin.dat".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    assert!(matches!(record.status, CheckpointStatus::Active), "binary banks: no Partial");
    assert!(record.excluded.is_empty(), "binary banks: no honesty entry");
    assert_eq!(record.restore_method, RestoreMethod::PreImageStore);

    // Mutate through the GATE's byte channel (ownership + conflict checks
    // included), then restore.
    let mutated: Vec<u8> = original.iter().rev().cloned().collect();
    h.gated_write_bytes("w-bin-2", "agent-a", "bin.dat", &mutated).await;
    assert_eq!(h.disk_bytes("bin.dat"), mutated);

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("binary restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["bin.dat".to_string()]);
        }
        other => panic!("expected Restored (never Partial/Manual), got {other:?}"),
    }
    assert_eq!(h.disk_bytes("bin.dat"), original, "byte-exact binary restore");
}

/// (c) Multi-chunk TEXT round-trips byte-exact through chunks + reassembly:
/// the chunked store changes nothing for text above the old per-blob cap.
#[tokio::test]
async fn multi_chunk_text_file_round_trips_byte_exact() {
    let h = Harness::new().await;
    let line = "the quick brown fox jumps over the lazy dog 0123456789\n";
    let original: String = line.repeat(4000);
    assert!(original.len() > CHECKPOINT_BLOB_LIMIT_BYTES);
    h.gated_write("w-big-1", "agent-a", "novel.txt", &original).await;

    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["novel.txt"]),
        vec!["novel.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    assert!(matches!(record.status, CheckpointStatus::Active));

    h.gated_write("w-big-2", "agent-a", "novel.txt", "rewritten entirely").await;
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["novel.txt".to_string()]);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("novel.txt"), original, "chunk reassembly is byte-exact");
}

/// (d) A corrupt/missing chunk set restores `Partial` with the path
/// `missing` — never a success claim, and the mutated disk content is
/// untouched for the missing path.
#[tokio::test]
async fn corrupt_chunk_set_restores_partial_without_success_claim() {
    let h = Harness::new().await;
    let original: Vec<u8> =
        (0..CHECKPOINT_BLOB_LIMIT_BYTES * 2 + 999).map(|i| (i % 251) as u8).collect();
    std::fs::write(h.root.join("frag.dat"), &original).expect("seed");
    h.gated_write("w-ok", "agent-a", "ok.txt", "ok-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["frag.dat", "ok.txt"]),
        vec!["frag.dat".to_string(), "ok.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");

    // Corrupt the chunk set the way a crash would: drop a middle chunk.
    let deleted: u64 = sqlx::query(
        "DELETE FROM checkpoint_pre_image_chunks \
         WHERE checkpoint_id = ? AND canonical_path = ? AND chunk_index = 1",
    )
    .bind(record.checkpoint_id.to_string())
    .bind("frag.dat")
    .execute(&h.pool)
    .await
    .expect("drop a chunk")
    .rows_affected();
    assert_eq!(deleted, 1);

    let mutated: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    std::fs::write(h.root.join("frag.dat"), &mutated).expect("mutate");
    h.gated_write("w-ok-2", "agent-a", "ok.txt", "ok-v2").await;

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("partial restore surfaces, never fails")
    {
        concerto_core::checkpoint::RestoreOutcome::Partial {
            restored,
            missing,
            limitations,
            ..
        } => {
            assert_eq!(restored, vec!["ok.txt".to_string()]);
            assert_eq!(missing, vec!["frag.dat".to_string()]);
            assert!(limitations.iter().any(|l| l.contains("frag.dat")));
        }
        other => panic!("expected Partial (never a success claim), got {other:?}"),
    }
    assert_eq!(h.disk_bytes("frag.dat"), mutated, "missing path is never written");
    assert_eq!(h.disk("ok.txt"), "ok-v1", "the intact subset still restored");
}

/// (f) A tampered chunk set is caught by hash re-verification: structurally
/// dense (so it reads `Stored`) but byte-divergent, the pre-write blake3
/// check refuses it → `Partial`, nothing written for the path.
#[tokio::test]
async fn tampered_chunk_set_fails_hash_verification() {
    let h = Harness::new().await;
    let original: Vec<u8> =
        (0..CHECKPOINT_BLOB_LIMIT_BYTES + 500).map(|i| (i % 251) as u8).collect();
    std::fs::write(h.root.join("sealed.dat"), &original).expect("seed");
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["sealed.dat"]),
        vec!["sealed.dat".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");

    // Tamper preserving structure (same chunk count and sizes): flip every
    // byte of the first chunk.
    sqlx::query(
        "UPDATE checkpoint_pre_image_chunks SET content = ? \
         WHERE checkpoint_id = ? AND canonical_path = ? AND chunk_index = 0",
    )
    .bind(vec![0xAA_u8; CHECKPOINT_BLOB_LIMIT_BYTES])
    .bind(record.checkpoint_id.to_string())
    .bind("sealed.dat")
    .execute(&h.pool)
    .await
    .expect("tamper a chunk");

    let mutated: Vec<u8> = vec![9, 9, 9];
    std::fs::write(h.root.join("sealed.dat"), &mutated).expect("mutate");

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("partial restore surfaces, never fails")
    {
        concerto_core::checkpoint::RestoreOutcome::Partial {
            restored,
            missing,
            limitations,
            ..
        } => {
            assert!(restored.is_empty());
            assert_eq!(missing, vec!["sealed.dat".to_string()]);
            assert!(
                limitations.iter().any(|l| l.contains("hash verification")),
                "the limitation names the failed verification, got {limitations:?}"
            );
        }
        other => panic!("expected Partial (never a success claim), got {other:?}"),
    }
    assert_eq!(h.disk_bytes("sealed.dat"), mutated, "unverified bytes are never written");
}

/// TOCTOU race (locks the Phase-2 ownership re-check): Phase 1 passes while
/// the path is self-owned; a coordinator-mediated transfer then hands it to
/// a sibling — injected at the restore's first sidecar-byte load, the only
/// await point between the pre-check and the gated write — and the Phase-2
/// write re-checks the gate's live table, aborting with the STRUCTURED
/// `OwnershipConflict` naming the new owner. Never a generic denial, never
/// a silent overwrite, status left retriable.
#[tokio::test]
async fn toctou_sibling_acquire_between_phases_surfaces_structured_conflict() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use concerto_core::checkpoint::{
        CheckpointRecord, CheckpointStore, CheckpointStoreError, PreImageBytes, RestoreError,
    };
    use concerto_orchestrator::file_checkpoint::GateOwnershipOracle;

    /// Sidecar store that fires the sibling's ownership acquisition on the
    /// first byte load (the Phase-1→Phase-2 interleave point), then
    /// delegates every method to the real store.
    struct HijackStore {
        inner: FileCheckpointStore,
        gate: Arc<WriteGate>,
        fired: AtomicBool,
    }

    impl HijackStore {
        /// The sibling acquires the contested path mid-restore via the only
        /// legitimate hand-off: a coordinator-mediated transfer (a steal
        /// through the gate would itself be refused).
        async fn acquire_as_sibling(&self) {
            self.gate
                .transfer_ownership("race.txt", "agent-a", "agent-b", "transfer-1")
                .await
                .expect("coordinator-mediated transfer hands the path to agent-b");
        }
    }

    #[async_trait]
    impl CheckpointStore for HijackStore {
        async fn load_record(
            &self,
            checkpoint_id: &str,
            cancel: CancellationToken,
        ) -> Result<Option<CheckpointRecord>, CheckpointStoreError> {
            self.inner.load_record(checkpoint_id, cancel).await
        }

        async fn load_bytes(
            &self,
            checkpoint_id: &str,
            canonical_path: &str,
            cancel: CancellationToken,
        ) -> Result<PreImageBytes, CheckpointStoreError> {
            if !self.fired.swap(true, Ordering::SeqCst) {
                self.acquire_as_sibling().await;
            }
            self.inner.load_bytes(checkpoint_id, canonical_path, cancel).await
        }

        async fn mark_restored(
            &self,
            checkpoint_id: &str,
            at_ms: i64,
            by: &str,
            cancel: CancellationToken,
        ) -> Result<bool, CheckpointStoreError> {
            self.inner.mark_restored(checkpoint_id, at_ms, by, cancel).await
        }

        async fn mark_partial(
            &self,
            checkpoint_id: &str,
            missing: Vec<String>,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            self.inner.mark_partial(checkpoint_id, missing, cancel).await
        }

        async fn load_restore_progress(
            &self,
            checkpoint_id: &str,
            cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::checkpoint::RestoreProgressEntry>, CheckpointStoreError>
        {
            concerto_core::checkpoint::CheckpointStore::load_restore_progress(
                &self.inner,
                checkpoint_id,
                cancel,
            )
            .await
        }

        async fn record_restore_progress(
            &self,
            checkpoint_id: &str,
            entry: &concerto_core::checkpoint::RestoreProgressEntry,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::record_restore_progress(
                &self.inner,
                checkpoint_id,
                entry,
                cancel,
            )
            .await
        }

        async fn begin_restore_attempt(
            &self,
            attempt: &concerto_core::checkpoint::RestoreAttemptInfo,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::begin_restore_attempt(
                &self.inner,
                attempt,
                cancel,
            )
            .await
        }

        async fn finish_restore_attempt(
            &self,
            attempt_id: &str,
            outcome: concerto_core::checkpoint::RestoreAttemptOutcome,
            finished_at_ms: i64,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::finish_restore_attempt(
                &self.inner,
                attempt_id,
                outcome,
                finished_at_ms,
                cancel,
            )
            .await
        }
    }

    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "race.txt", "checkpoint-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["race.txt"]),
        vec!["race.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    h.gated_write("w-2", "agent-a", "race.txt", "mutated-v2").await;

    let hijack =
        HijackStore { inner: h.store.clone(), gate: h.gate.clone(), fired: AtomicBool::new(false) };
    match h
        .executor
        .restore_checkpoint(
            &record.checkpoint_id.to_string(),
            &hijack,
            &GateOwnershipOracle::new(h.gate.clone()),
            &NoopStash,
            &h.session(),
            "agent-a",
            CancellationToken::new(),
        )
        .await
    {
        Err(RestoreError::OwnershipConflict(conflicts)) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "race.txt");
            assert_eq!(
                conflicts[0].owner, "agent-b",
                "the conflict names the CURRENT owner acquired mid-restore"
            );
        }
        other => panic!("expected structured OwnershipConflict, got {other:?}"),
    }
    assert_eq!(h.disk("race.txt"), "mutated-v2", "no restore write ran after the transfer");
    let back = h
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Active),
        "mid-restore abort leaves the status retriable"
    );
}

/// Partial retry convergence: a policy denial mid-restore aborts loudly
/// (`PolicyDenied`), writes only the already-gated prefix, and leaves the
/// status `Active` (retriable); the retry converges to a full, accurate
/// `Restored`. The interrupted attempt never claims success.
#[tokio::test]
async fn policy_denied_mid_restore_converges_on_retry() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a_first.txt", "first-v1").await;
    h.gated_write("w-2", "agent-a", "b_denied.txt", "second-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a_first.txt", "b_denied.txt"]),
        vec!["a_first.txt".to_string(), "b_denied.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    h.gated_write("w-3", "agent-a", "a_first.txt", "first-v2").await;
    h.gated_write("w-4", "agent-a", "b_denied.txt", "second-v2").await;

    // Deny only the second path (sorted scope order: a_first writes first,
    // b_denied aborts the restore mid-flight).
    let deny_second = Arc::new(SimplePolicyEngine::new(
        vec![
            PolicyRule::AutoDeny(Condition::PathGlob("b_denied*".to_string())),
            PolicyRule::AutoApprove(Condition::Always),
        ],
        Arc::new(TestAudit),
    ));
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(h.root.clone()).expect("utf-8"),
    )));
    let deny_executor = Arc::new(ToolExecutor::new(Arc::new(registry), deny_second));

    match restore_file_checkpoint(
        &deny_executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(concerto_core::checkpoint::RestoreError::PolicyDenied(_)) => {}
        other => panic!("expected PolicyDenied, got {other:?}"),
    }
    // Real mid-flight progress, no silent success: the first path is back,
    // the denied one is untouched, the status is still retriable.
    assert_eq!(h.disk("a_first.txt"), "first-v1", "the gated prefix restored");
    assert_eq!(h.disk("b_denied.txt"), "second-v2", "the denied path is untouched");
    let back = h
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Active),
        "the interrupted attempt left the status retriable"
    );

    // Retry with the allowing executor: the same bytes rewrite idempotently
    // and the second write lands — a full, accurate Restored.
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("retry restores")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { restored, .. } => {
            let mut sorted = restored.clone();
            sorted.sort();
            assert_eq!(sorted, vec!["a_first.txt".to_string(), "b_denied.txt".to_string()]);
        }
        other => panic!("expected Restored on retry, got {other:?}"),
    }
    assert_eq!(h.disk("a_first.txt"), "first-v1");
    assert_eq!(h.disk("b_denied.txt"), "second-v1", "retry converged byte-exact");
    let back = h
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Restored { .. }),
        "the converged retry records Restored"
    );
}

/// H07 crash-recovery half, end-to-end: interrupt mid-restore → the journal
/// shows precise partial progress → resume writes ONLY the remaining
/// resources (verified skips proven by exactly one sidecar presence-check
/// load per skipped row and by write counts, never a blind skip) and
/// converges to `Restored`. Crash-restart (reopening the store from the same
/// database file) behaves identically: no progress lives in process memory.
#[tokio::test]
async fn interrupted_restore_resumes_from_journal_and_converges() {
    use concerto_core::checkpoint::{
        CheckpointRecord, CheckpointStore, CheckpointStoreError, PreImageBytes, RestoreError,
    };

    /// Counting delegate: the verified row's sidecar is touched EXACTLY once
    /// — the skip's authoritative pre-image presence check — while rows that
    /// still need their gated write load once each for that write.
    struct CountingStore {
        inner: FileCheckpointStore,
        loads: Arc<std::sync::Mutex<BTreeMap<String, usize>>>,
    }

    #[async_trait]
    impl CheckpointStore for CountingStore {
        async fn load_record(
            &self,
            checkpoint_id: &str,
            cancel: CancellationToken,
        ) -> Result<Option<CheckpointRecord>, CheckpointStoreError> {
            self.inner.load_record(checkpoint_id, cancel).await
        }

        async fn load_bytes(
            &self,
            checkpoint_id: &str,
            canonical_path: &str,
            cancel: CancellationToken,
        ) -> Result<PreImageBytes, CheckpointStoreError> {
            *self
                .loads
                .lock()
                .expect("loads lock")
                .entry(canonical_path.to_owned())
                .or_insert(0) += 1;
            self.inner.load_bytes(checkpoint_id, canonical_path, cancel).await
        }

        async fn mark_restored(
            &self,
            checkpoint_id: &str,
            at_ms: i64,
            by: &str,
            cancel: CancellationToken,
        ) -> Result<bool, CheckpointStoreError> {
            self.inner.mark_restored(checkpoint_id, at_ms, by, cancel).await
        }

        async fn mark_partial(
            &self,
            checkpoint_id: &str,
            missing: Vec<String>,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            self.inner.mark_partial(checkpoint_id, missing, cancel).await
        }

        async fn load_restore_progress(
            &self,
            checkpoint_id: &str,
            cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::checkpoint::RestoreProgressEntry>, CheckpointStoreError>
        {
            concerto_core::checkpoint::CheckpointStore::load_restore_progress(
                &self.inner,
                checkpoint_id,
                cancel,
            )
            .await
        }

        async fn record_restore_progress(
            &self,
            checkpoint_id: &str,
            entry: &concerto_core::checkpoint::RestoreProgressEntry,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::record_restore_progress(
                &self.inner,
                checkpoint_id,
                entry,
                cancel,
            )
            .await
        }

        async fn begin_restore_attempt(
            &self,
            attempt: &concerto_core::checkpoint::RestoreAttemptInfo,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::begin_restore_attempt(
                &self.inner,
                attempt,
                cancel,
            )
            .await
        }

        async fn finish_restore_attempt(
            &self,
            attempt_id: &str,
            outcome: concerto_core::checkpoint::RestoreAttemptOutcome,
            finished_at_ms: i64,
            cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            concerto_core::checkpoint::CheckpointStore::finish_restore_attempt(
                &self.inner,
                attempt_id,
                outcome,
                finished_at_ms,
                cancel,
            )
            .await
        }
    }

    let h = Harness::new().await;
    for (call, path, content) in
        [("w-1", "a.txt", "a-v1"), ("w-2", "b.txt", "b-v1"), ("w-3", "c.txt", "c-v1")]
    {
        h.gated_write(call, "agent-a", path, content).await;
    }
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt", "b.txt", "c.txt"]),
        vec!["a.txt".to_string(), "b.txt".to_string(), "c.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    for (call, path, content) in
        [("w-4", "a.txt", "a-v2"), ("w-5", "b.txt", "b-v2"), ("w-6", "c.txt", "c-v2")]
    {
        h.gated_write(call, "agent-a", path, content).await;
    }

    // Attempt 1: byte loss on b.txt mid-restore (a.txt lands, c.txt never runs).
    let failing = FailBytesStore {
        inner: h.store.clone(),
        fail_path: "b.txt".to_string(),
        failed: std::sync::atomic::AtomicBool::new(false),
    };
    match h
        .executor
        .restore_checkpoint(
            &cid,
            &failing,
            &concerto_orchestrator::file_checkpoint::GateOwnershipOracle::new(h.gate.clone()),
            &NoopStash,
            &h.session(),
            "agent-a",
            CancellationToken::new(),
        )
        .await
    {
        Err(RestoreError::Store(_)) => {}
        other => panic!("expected the injected mid-restore store failure, got {other:?}"),
    }
    // Precise partial progress, never a success claim.
    let progress = h
        .store
        .load_restore_progress(&cid, &CancellationToken::new())
        .await
        .expect("journal reads");
    let states: BTreeMap<String, String> = progress
        .iter()
        .map(|entry| (entry.canonical_path.clone(), entry.state.as_str().to_owned()))
        .collect();
    assert_eq!(
        states,
        BTreeMap::from([
            ("a.txt".to_string(), "done".to_string()),
            ("b.txt".to_string(), "failed".to_string()),
        ]),
        "c.txt never ran: no row (unknown progress, never assumed done)"
    );
    assert_eq!(h.disk("a.txt"), "a-v1", "the gated prefix landed");
    assert_eq!(h.disk("b.txt"), "b-v2", "the failed path is untouched");
    let back = h
        .store
        .load_checkpoint_record(&cid, &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(matches!(back.status, CheckpointStatus::Active), "interrupted status stays retriable");

    // The progress query reports the same story with live disk verification.
    let items = h
        .executor
        .restore_progress(&cid, &h.store, &h.session(), CancellationToken::new())
        .await
        .expect("progress query reads");
    let report: BTreeMap<String, (String, bool)> = items
        .into_iter()
        .map(|item| (item.canonical_path, (item.state.as_str().to_owned(), item.verified_on_disk)))
        .collect();
    assert_eq!(
        report,
        BTreeMap::from([
            ("a.txt".to_string(), ("done".to_string(), true)),
            ("b.txt".to_string(), ("failed".to_string(), false)),
            ("c.txt".to_string(), ("pending".to_string(), false)),
        ]),
        "done+verified vs failed+drifted vs never-ran must read distinctly"
    );

    // Attempt 2 (resume through a counting store AND a write-recording
    // tool): the verified a.txt row performs only its sidecar presence check
    // and NO write — only b + c are written.
    let counting = CountingStore {
        inner: h.store.clone(),
        loads: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
    };
    let writes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut resume_registry = ToolRegistry::default();
    resume_registry.register(Box::new(RecordingFsTool {
        inner: FilesystemTool::new(
            camino::Utf8PathBuf::from_path_buf(h.root.clone()).expect("tempdir is utf-8"),
        ),
        ops: writes.clone(),
    }));
    let resume_executor = ToolExecutor::new(Arc::new(resume_registry), allow_engine())
        .with_mutation_sink(Arc::new(h.store.clone()));
    match resume_executor
        .restore_checkpoint(
            &cid,
            &counting,
            &concerto_orchestrator::file_checkpoint::GateOwnershipOracle::new(h.gate.clone()),
            &NoopStash,
            &h.session(),
            "agent-a",
            CancellationToken::new(),
        )
        .await
        .expect("resume restores")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { verified_skips, .. } => {
            assert_eq!(verified_skips, vec!["a.txt".to_string()], "only the verified row skips");
        }
        other => panic!("expected Restored on resume, got {other:?}"),
    }
    let loads = counting.loads.lock().expect("loads lock").clone();
    assert_eq!(
        loads.get("a.txt"),
        Some(&1),
        "the skip's ONE authoritative presence check: the recorded pre-image still banks its bytes"
    );
    assert_eq!(loads.get("b.txt"), Some(&1), "failed b.txt reloads exactly once");
    assert_eq!(loads.get("c.txt"), Some(&1), "never-ran c.txt loads exactly once");
    assert_eq!(
        writes.lock().expect("ops lock").clone(),
        vec!["write b.txt".to_string(), "write c.txt".to_string()],
        "the verified skip performs no WRITE: a.txt is never re-written"
    );
    assert_eq!(h.disk("a.txt"), "a-v1");
    assert_eq!(h.disk("b.txt"), "b-v1", "resume converged b.txt");
    assert_eq!(h.disk("c.txt"), "c-v1", "resume converged c.txt");
    let back = h
        .store
        .load_checkpoint_record(&cid, &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(matches!(back.status, CheckpointStatus::Restored { .. }), "resume converges");
    let progress = h
        .store
        .load_restore_progress(&cid, &CancellationToken::new())
        .await
        .expect("journal reads");
    assert_eq!(progress.len(), 3, "every resource journaled done");
    assert!(progress
        .iter()
        .all(|entry| { entry.state == concerto_core::checkpoint::RestoreResourceState::Done }));
}

/// Crash-restart: the service dies mid-restore (every in-memory handle
/// dropped — a brand-new pool, store, and executor over the same database
/// file and project dir). The reopened service reads precise journal
/// progress from disk and resumes to `Restored`.
#[tokio::test]
async fn crash_restart_reopens_store_and_resumes_to_restored() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a.txt", "a-v1").await;
    h.gated_write("w-2", "agent-a", "b.txt", "b-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt", "b.txt"]),
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    h.gated_write("w-3", "agent-a", "a.txt", "a-v2").await;
    h.gated_write("w-4", "agent-a", "b.txt", "b-v2").await;

    // Attempt 1 under the old service: byte loss on b.txt mid-restore, so
    // only a.txt lands and journals `done` — then the process dies.
    let failing = FailBytesStore {
        inner: h.store.clone(),
        fail_path: "b.txt".to_string(),
        failed: std::sync::atomic::AtomicBool::new(false),
    };
    match h
        .executor
        .restore_checkpoint(
            &cid,
            &failing,
            &concerto_orchestrator::file_checkpoint::GateOwnershipOracle::new(h.gate.clone()),
            &NoopStash,
            &h.session(),
            "agent-a",
            CancellationToken::new(),
        )
        .await
    {
        Err(concerto_core::checkpoint::RestoreError::Store(_)) => {}
        other => panic!("expected the injected mid-restore store failure, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "a-v1", "the gated prefix landed before the kill");

    // The kill: close the pool and drop the store, the gate, and the
    // executor — only the database file and the project dir survive. No
    // restore progress lives in process memory. (The database TempDir is
    // moved out first: dropping it would delete the file under the test.)
    let root = h.root.clone();
    let session_id = h.session_id;
    h.pool.close().await;
    let db_dir = h._db_dir;
    let db_file = db_dir.path().join("ckpt_test.db");

    // Reopen: brand-new pool over the same file (migrations re-apply
    // cleanly — 040 included), brand-new store, gate, and executor.
    let options = SqliteConnectOptions::new()
        .filename(&db_file)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("reopen pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations re-apply");
    let store = FileCheckpointStore::new(pool.clone());
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
    let executor = Arc::new(
        ToolExecutor::new(Arc::new(registry), allow_engine())
            .with_mutation_sink(Arc::new(store.clone())),
    );
    let gate = Arc::new(
        WriteGate::new(
            allow_engine(),
            executor.clone(),
            pool.clone(),
            Arc::new(FilePreImageReader::new(root.clone())),
            root.clone(),
            1,
        )
        .with_checkpoint_store(store.clone()),
    );
    let session = SessionContext::new(session_id, root.clone());

    // The reopened service reads precise journal progress from disk (not
    // memory): a.txt verifies and skips, only b.txt restores.
    match restore_file_checkpoint(
        &executor,
        &store,
        &gate,
        None,
        &session,
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("post-restart resume converges")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { verified_skips, .. } => {
            assert_eq!(
                verified_skips,
                vec!["a.txt".to_string()],
                "the pre-kill write is verified, not rewritten"
            );
        }
        other => panic!("expected Restored after restart, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read a"),
        "a-v1",
        "post-restart restore converged a.txt"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).expect("read b"),
        "b-v1",
        "post-restart restore converged b.txt"
    );
    let progress =
        store.load_restore_progress(&cid, &CancellationToken::new()).await.expect("journal reads");
    assert_eq!(progress.len(), 2, "the restarted service journaled every resource");
    assert!(progress
        .iter()
        .all(|entry| entry.state == concerto_core::checkpoint::RestoreResourceState::Done));
}

/// Concurrent duplicate submission converges without double-apply
/// corruption: every outcome is `Restored`/`AlreadyRestored`, disk is
/// byte-exact, and the journal ends all-`done`.
#[tokio::test]
async fn concurrent_duplicate_restore_converges_without_corruption() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a.txt", "a-v1").await;
    h.gated_write("w-2", "agent-a", "b.txt", "b-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt", "b.txt"]),
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    h.gated_write("w-3", "agent-a", "a.txt", "a-v2").await;
    h.gated_write("w-4", "agent-a", "b.txt", "b-v2").await;

    let session = h.session();
    let cancel_first = CancellationToken::new();
    let cancel_second = CancellationToken::new();
    let (first, second) = tokio::join!(
        restore_file_checkpoint(
            &h.executor,
            &h.store,
            &h.gate,
            None,
            &session,
            &cid,
            "agent-a",
            &cancel_first,
        ),
        restore_file_checkpoint(
            &h.executor,
            &h.store,
            &h.gate,
            None,
            &session,
            &cid,
            "agent-a",
            &cancel_second,
        )
    );
    for outcome in [&first, &second] {
        match outcome {
            Ok(concerto_core::checkpoint::RestoreOutcome::Restored { .. })
            | Ok(concerto_core::checkpoint::RestoreOutcome::AlreadyRestored { .. }) => {}
            other => panic!("concurrent duplicates must converge, got {other:?}"),
        }
    }
    assert_eq!(h.disk("a.txt"), "a-v1", "no double-apply corruption on a.txt");
    assert_eq!(h.disk("b.txt"), "b-v1", "no double-apply corruption on b.txt");
    let back = h
        .store
        .load_checkpoint_record(&cid, &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(matches!(back.status, CheckpointStatus::Restored { .. }));
}

/// A resource marked `done` by an earlier run but modified since is
/// re-restored, never blindly skipped.
#[tokio::test]
async fn changed_since_done_marker_is_rewritten_not_skipped() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a.txt", "a-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt"]),
        vec!["a.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    h.gated_write("w-2", "agent-a", "a.txt", "a-v2").await;

    // First restore lands a.txt and journals `done`.
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("first restore lands")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { .. } => {}
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "a-v1");

    // A sibling edits the file after the `done` marker; the checkpoint goes
    // back to Active (simulating a crash between the last write and
    // `mark_restored`, or a deliberate re-restore request).
    std::fs::write(h.root.join("a.txt"), b"sibling-edit").expect("sibling edit lands");
    sqlx::query("UPDATE file_checkpoints SET status = 'active' WHERE checkpoint_id = ?")
        .bind(&cid)
        .execute(&h.pool)
        .await
        .expect("re-arm Active");

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("second restore rewrites")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { verified_skips, .. } => {
            assert!(
                verified_skips.is_empty(),
                "drifted content must never skip, got {verified_skips:?}"
            );
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "a-v1", "the drifted path is rewritten byte-exact");
}

/// Bytes that vanish after checkpointing (retention prune between capture
/// and restore) never claim `Restored`: the restore reports `Partial`,
/// journals the byte-less path `missing`, and leaves the landed prefix
/// resumable.
#[tokio::test]
async fn pruned_bytes_mid_restore_never_claim_restored() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a.txt", "a-v1").await;
    h.gated_write("w-2", "agent-a", "b.txt", "b-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt", "b.txt"]),
        vec!["a.txt".to_string(), "b.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    h.gated_write("w-3", "agent-a", "a.txt", "a-v2").await;
    h.gated_write("w-4", "agent-a", "b.txt", "b-v2").await;

    // Retention collects b.txt's sidecar rows while the record still claims
    // Active (the prune/status race the journal must survive honestly).
    sqlx::query(
        "DELETE FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ? AND canonical_path = 'b.txt'",
    )
    .bind(&cid)
    .execute(&h.pool)
    .await
    .expect("drop b.txt chunks");

    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("partial restore surfaces, never fails")
    {
        concerto_core::checkpoint::RestoreOutcome::Partial { restored, missing, .. } => {
            assert_eq!(restored, vec!["a.txt".to_string()]);
            assert_eq!(missing, vec!["b.txt".to_string()]);
        }
        other => panic!("expected Partial (never Restored without bytes), got {other:?}"),
    }
    let back = h
        .store
        .load_checkpoint_record(&cid, &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Partial { .. }),
        "byte-less restore records Partial, never Restored"
    );
    let progress = h
        .store
        .load_restore_progress(&cid, &CancellationToken::new())
        .await
        .expect("journal reads");
    let states: BTreeMap<String, String> = progress
        .iter()
        .map(|entry| (entry.canonical_path.clone(), entry.state.as_str().to_owned()))
        .collect();
    assert_eq!(
        states,
        BTreeMap::from([
            ("a.txt".to_string(), "done".to_string()),
            ("b.txt".to_string(), "missing".to_string()),
        ])
    );
    assert_eq!(h.disk("b.txt"), "b-v2", "no bytes: nothing written");
}

/// The verified-skip invariant, end-to-end: a prior run journaled a path
/// `done`, then retention collected its sidecar rows while the live disk
/// still coincidentally hashes to the record's `post_hash`. Without the
/// recorded pre-image the skip is refused — nothing is restored — so the
/// outcome is `Partial` (never `Restored` + `verified_skips`), the status is
/// `Partial`, the journal keeps `done` (the monotonic guard refuses
/// `done → missing`), and no write runs.
#[tokio::test]
async fn pruned_sidecar_on_done_row_restores_partial_never_a_skip() {
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "a.txt", "a-v1").await;
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["a.txt"]),
        vec!["a.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint creation");
    let cid = record.checkpoint_id.to_string();
    h.gated_write("w-2", "agent-a", "a.txt", "a-v2").await;

    // First restore lands a.txt and journals `done`.
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("first restore lands")
    {
        concerto_core::checkpoint::RestoreOutcome::Restored { .. } => {}
        other => panic!("expected Restored, got {other:?}"),
    }
    assert_eq!(h.disk("a.txt"), "a-v1");

    // Re-arm Active, then retention-collect the sidecar rows for a.txt: the
    // record still claims restorable, the journal still says `done`, and the
    // disk still hashes to post_hash — but no restorable bytes remain.
    sqlx::query("UPDATE file_checkpoints SET status = 'active' WHERE checkpoint_id = ?")
        .bind(&cid)
        .execute(&h.pool)
        .await
        .expect("re-arm Active");
    sqlx::query(
        "DELETE FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ? AND canonical_path = 'a.txt'",
    )
    .bind(&cid)
    .execute(&h.pool)
    .await
    .expect("drop a.txt chunks");

    // Second restore: disk match alone must not earn a verified skip.
    match restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &cid,
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("byte-less restore surfaces, never fails")
    {
        concerto_core::checkpoint::RestoreOutcome::Partial {
            restored,
            missing,
            limitations,
            ..
        } => {
            assert!(
                restored.is_empty(),
                "a sidecar-less `done` row must never be claimed as restored, got {restored:?}"
            );
            assert_eq!(missing, vec!["a.txt".to_string()]);
            assert!(
                limitations.iter().any(|l| l.contains("no restorable bytes banked")),
                "the limitation names the byte loss, got {limitations:?}"
            );
        }
        other => {
            panic!("expected Partial (disk match without a pre-image is not proof), got {other:?}")
        }
    }
    let back = h
        .store
        .load_checkpoint_record(&cid, &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Partial { .. }),
        "the dishonest Restored claim is recorded as Partial, got {:?}",
        back.status
    );
    let progress = h
        .store
        .load_restore_progress(&cid, &CancellationToken::new())
        .await
        .expect("journal reads");
    let states: BTreeMap<String, String> = progress
        .iter()
        .map(|entry| (entry.canonical_path.clone(), entry.state.as_str().to_owned()))
        .collect();
    assert_eq!(
        states,
        BTreeMap::from([("a.txt".to_string(), "done".to_string())]),
        "the monotonic guard keeps the earlier true `done` (`done → missing` refused)"
    );
    assert_eq!(h.disk("a.txt"), "a-v1", "no bytes: nothing written, disk left as found");
}

/// H16 honesty defect 2: every restore path reports the `RestoreMethod` it
/// actually ran, end-to-end through the real store/executor — an operator
/// reading an outcome never reverse-engineers the branch from the variant.
/// Drives one real restore per method (PreImageStore, Manual, GitStash).
#[tokio::test]
async fn every_restore_method_is_named_on_its_outcome() {
    use concerto_core::checkpoint::RestoreOutcome;
    use concerto_orchestrator::file_checkpoint::create_git_stash_checkpoint;
    use concerto_tools::undo::UndoManager;

    // --- PreImageStore: the byte path. ---
    let h = Harness::new().await;
    h.gated_write("w-1", "agent-a", "notes.txt", "version one").await;
    let preimage = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["notes.txt"]),
        vec!["notes.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("pre-image checkpoint");
    assert_eq!(preimage.restore_method, RestoreMethod::PreImageStore);
    h.gated_write("w-2", "agent-a", "notes.txt", "version two").await;
    let outcome = restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &preimage.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("pre-image restore succeeds");
    match outcome {
        RestoreOutcome::Restored { method, .. } => {
            assert_eq!(method, RestoreMethod::PreImageStore, "byte path names itself");
        }
        other => panic!("expected Restored, got {other:?}"),
    }

    // A second restore (the recorded no-op) names the method the FIRST ran.
    let again = restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &preimage.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("re-restore surfaces");
    match again {
        RestoreOutcome::AlreadyRestored { method, .. } => {
            assert_eq!(method, RestoreMethod::PreImageStore, "no-op echoes the first method");
        }
        other => panic!("expected AlreadyRestored, got {other:?}"),
    }

    // --- Manual: an over-total file degrades to Manual and says so. ---
    let big_bytes = vec![b'x'; CHECKPOINT_TOTAL_LIMIT_BYTES + 1];
    std::fs::write(h.root.join("big.bin"), &big_bytes).expect("seed over-total file");
    let manual = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["big.bin"]),
        vec!["big.bin".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("manual checkpoint");
    assert_eq!(manual.restore_method, RestoreMethod::Manual);
    let outcome = restore_file_checkpoint(
        &h.executor,
        &h.store,
        &h.gate,
        None,
        &h.session(),
        &manual.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("manual restore surfaces");
    match outcome {
        RestoreOutcome::RequiresOperator { method, limitations } => {
            assert_eq!(method, RestoreMethod::Manual, "manual path names itself");
            assert!(!limitations.is_empty(), "manual still explains why");
        }
        other => panic!("expected RequiresOperator, got {other:?}"),
    }

    // --- GitStash: the stash path names itself. ---
    let g = Harness::new().await;
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&g.root)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&g.root)
        .output()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&g.root)
        .output()
        .expect("git config name");
    std::fs::write(g.root.join("tracked.txt"), "base").expect("seed");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&g.root)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "base"])
        .current_dir(&g.root)
        .output()
        .expect("git commit");

    std::fs::write(g.root.join("tracked.txt"), "dirty").expect("dirty");
    let undo = Arc::new(std::sync::Mutex::new(UndoManager::new(g.root.clone())));
    let task_id = TaskId::new();
    let stash = create_git_stash_checkpoint(
        &g.store,
        &undo,
        g.session_id,
        task_id,
        CheckpointScope::Task { task_id },
        "agent-a",
        Vec::new(),
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("stash checkpoint");
    assert_eq!(stash.restore_method, RestoreMethod::GitStash);

    std::fs::write(g.root.join("tracked.txt"), "post").expect("post");
    std::process::Command::new("git")
        .args(["checkout", "--", "tracked.txt"])
        .current_dir(&g.root)
        .output()
        .expect("git checkout");
    let outcome = restore_file_checkpoint(
        &g.executor,
        &g.store,
        &g.gate,
        Some(&undo),
        &g.session(),
        &stash.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("stash restore succeeds");
    match outcome {
        RestoreOutcome::StashRestored { method, detail } => {
            assert_eq!(method, RestoreMethod::GitStash, "stash path names itself");
            assert!(!detail.is_empty(), "stash detail is still reported");
        }
        other => panic!("expected StashRestored, got {other:?}"),
    }
}
