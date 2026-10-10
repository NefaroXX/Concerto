//! ADR-82 slice 5: durable file checkpoints with honest restore, end-to-end.
//!
//! Drives the *real* [`WriteGate`] (with the checkpoint sidecar attached) and
//! the *real* `FilesystemTool` through the specified verification flow:
//!
//! - write → checkpoint → mutate → restore returns byte-exact content;
//! - ownership-conflict restore is refused and surfaced (no silent overwrite);
//! - double restore is a recorded no-op;
//! - oversized content degrades to `Manual` + `Partial` with limitations;
//! - the gate seam backfills a missed creation snapshot;
//! - the executor's direct-path hook captures without the gate;
//! - retention prunes sidecar bytes and transitions the checkpoint to
//!   `Expired` (restorable claim withdrawn; restore refuses).
//!
//! Partial-milestone honesty: binary/large-file exact restore is NOT
//! delivered here (streaming store deferred) — the oversized test pins the
//! degradation instead of the capability.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::checkpoint::{
    CheckpointExclusion, CheckpointExclusionClass, CheckpointScope, CheckpointStatus, RestoreMethod,
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

    fn disk(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).expect("read disk file")
    }
}

fn fs_scope(paths: &[&str]) -> CheckpointScope {
    CheckpointScope::FilesystemWrites { paths: paths.iter().map(|p| (*p).to_owned()).collect() }
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

/// Oversized content degrades honestly: hash-only + excluded + Manual when
/// nothing banked; Partial restore of the bankable subset otherwise.
#[tokio::test]
async fn oversized_file_degrades_to_manual_with_limitations() {
    let h = Harness::new().await;
    let big_content = "x".repeat(70_000);
    h.gated_write("w-big", "agent-a", "big.txt", &big_content).await;

    // Pure-oversized checkpoint: nothing bankable → Manual.
    let record = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["big.txt"]),
        vec!["big.txt".to_string()],
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
    assert!(record.limitations.iter().any(|l| l.contains("64 KiB")));

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
        concerto_core::checkpoint::RestoreOutcome::RequiresOperator { limitations } => {
            assert!(!limitations.is_empty());
        }
        other => panic!("expected RequiresOperator, got {other:?}"),
    }
    assert_eq!(h.disk("big.txt"), big_content, "Manual writes nothing");

    // Mixed checkpoint: the small file restores, the big one reports missing.
    h.gated_write("w-small", "agent-a", "small.txt", "small-v1").await;
    let mixed = create_file_checkpoint(
        &h.store,
        &h.root,
        h.session_id,
        fs_scope(&["small.txt", "big.txt"]),
        vec!["small.txt".to_string(), "big.txt".to_string()],
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
        concerto_core::checkpoint::RestoreOutcome::Partial { restored, missing, limitations } => {
            assert_eq!(restored, vec!["small.txt".to_string()]);
            assert_eq!(missing, vec!["big.txt".to_string()]);
            assert!(limitations.iter().any(|l| l.contains("big.txt")));
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
    // Simulate the missed snapshot: drop the banked row.
    sqlx::query("DELETE FROM checkpoint_pre_images WHERE checkpoint_id = ?")
        .bind(record.checkpoint_id.to_string())
        .execute(&h.pool)
        .await
        .expect("drop snapshot row");

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
    // Drop the snapshot so only the hook can supply bytes.
    sqlx::query("DELETE FROM checkpoint_pre_images WHERE checkpoint_id = ?")
        .bind(record.checkpoint_id.to_string())
        .execute(&h.pool)
        .await
        .expect("drop snapshot row");

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
