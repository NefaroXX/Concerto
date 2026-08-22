//! ADR-60 S5 end-to-end: the real agent process behind the supervisor.
//!
//! Spawns the production child binary `orchestrator-agent-process` — an
//! [`AgentLoop`] wired to the gate-proxy backend — under the *real*
//! supervisor loop, with the write gate, whiteboard pool and memory spine
//! attached. This proves the vertical slice over the wire:
//!
//! - D2 handshake + the `list-tools` registry fetch at child startup (the
//!   tool registry comes from the supervisor's gate, the single source of
//!   truth);
//! - the loop's tool calls ride `execute-tool` through the single gate:
//!   policy evaluation, WAL + pre-image capture, real `FilesystemTool`
//!   execution on disk, and the persisted `WriteApplied` row with
//!   attribution bound to the registered agent (never the wire value);
//! - the child publishes its terminal `subtask-completed` event through the
//!   same log (shared `gate_seq` order);
//! - per-iteration `retrieve-memory` reaches the memory spine;
//! - a denied write fails the tool call back to the agent without
//!   persisting anything, and the agent still completes cleanly.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, PolicyError};
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::AuditLog;
use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_orchestrator::gate::{FilePreImageReader, WriteGate};
use concerto_orchestrator::subscriptions::SubscriptionManager;
use concerto_orchestrator::supervisor::{
    AgentState, RunSummary, Supervisor, SupervisorConfig, SupervisorServices,
};
use concerto_sessions::whiteboard::{
    load_whiteboard_events, WhiteboardEvent, WhiteboardKind, WhiteboardLoadOpts,
};
use concerto_tools::filesystem::FilesystemTool;
use serde_json::json;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use tempfile::TempDir;

/// No-op audit log (mirrors `supervisor_gate.rs`).
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

/// A policy engine that allows (or denies) everything.
fn engine(rules: Vec<PolicyRule>) -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(rules, Arc::new(TestAudit)))
}

/// A gate whose executor is the REAL `FilesystemTool` rooted at `root`.
fn fs_gate(
    policy: Arc<SimplePolicyEngine>,
    pool: sqlx::SqlitePool,
    root: PathBuf,
) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
    Arc::new(WriteGate::new(
        policy,
        executor,
        pool,
        Arc::new(FilePreImageReader::new(root.clone())),
        root,
        1,
    ))
}

/// The real agent-process binary with the ADR-60 S5 environment contract.
fn agent_process(root: &std::path::Path, script: serde_json::Value) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-agent-process"));
    command
        .env("CONCERTO_AGENT_ID", "agent-a")
        .env("CONCERTO_PROJECT_ROOT", root.display().to_string())
        .env(
            "CONCERTO_TASK_DESCRIPTION",
            "write a file named hello.txt containing the text hello from agent",
        )
        .env("CONCERTO_MAX_ITERATIONS", "20")
        .env("CONCERTO_PROVIDER", "mock")
        .env("CONCERTO_MOCK_SCRIPT_JSON", script.to_string());
    command
}

/// A one-write script: turn 0 calls `write_file`, turn 1 completes.
fn one_write_script() -> serde_json::Value {
    json!([
        [
            {
                "delta": "",
                "reasoning": null,
                "tool_call": {
                    "id": "call-1",
                    "name": "filesystem",
                    "arguments": {
                        "operation": "write",
                        "path": "hello.txt",
                        "content": "hello from agent"
                    }
                },
                "is_final": false,
                "usage": null
            },
            { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
        ],
        [
            { "delta": "done", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
        ]
    ])
}

/// Memory spine stub that counts retrievals (mirrors `supervisor_gate.rs`).
struct CountingMemoryStore {
    retrievals: AtomicUsize,
}

impl CountingMemoryStore {
    fn new() -> Self {
        Self { retrievals: AtomicUsize::new(0) }
    }

    fn retrieval_count(&self) -> usize {
        self.retrievals.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MemoryStore for CountingMemoryStore {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        self.retrievals.fetch_add(1, Ordering::SeqCst);
        Ok(vec![MemoryChunk {
            id: "chunk-1".to_owned(),
            project_id: ProjectId("proj-s5".to_owned()),
            namespace: MemoryNamespace::Project(ProjectId("proj-s5".to_owned())),
            content: "a remembered fact".to_owned(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Test,
            score: 0.9,
            model_id: "test-model".to_owned(),
            model_version: "0".to_owned(),
        }])
    }

    async fn store(
        &self,
        _entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        Ok(MemoryId(concerto_core::ids::Ulid::new()))
    }

    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}

/// A sqlite pool over the whiteboard schema in a temp dir.
async fn whiteboard_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("supervisor_agent_process.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

/// Run the supervisor loop for `run_for`, then return the summary.
async fn run_supervisor(
    mut supervisor: Supervisor,
    services: SupervisorServices,
    run_for: Duration,
) -> RunSummary {
    supervisor = supervisor.with_services(services);
    let shutdown = CancellationToken::new();
    let guard = shutdown.clone();
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });
    tokio::time::sleep(run_for).await;
    guard.cancel();
    handle.await.expect("loop task completes")
}

/// All whiteboard rows in gate_seq order.
async fn all_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
    load_whiteboard_events(pool, &WhiteboardLoadOpts::default()).await.expect("load events")
}

/// Poll `all_events` until `predicate` holds or the timeout expires.
async fn wait_for_events<F>(
    pool: &sqlx::SqlitePool,
    timeout: Duration,
    predicate: F,
) -> Vec<WhiteboardEvent>
where
    F: Fn(&[WhiteboardEvent]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let events = all_events(pool).await;
        if predicate(&events) {
            return events;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for whiteboard events");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Initialize a git repo with one initial commit so the child's undo stash
/// (`git stash push`) succeeds — a freshly `git init`-ed directory has no
/// commits and stash refuses to run (the loop then aborts via its ack gate).
fn git_commit_all(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git available in test environment");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "agent-process-test@invalid"]);
    git(&["config", "user.name", "Agent Process Test"]);
    std::fs::write(dir.join(".gitkeep"), "").expect("seed file written");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);
}

#[tokio::test]
async fn real_agent_process_gated_write_completes_end_to_end() {
    let root_dir = tempfile::tempdir().expect("tempdir");
    git_commit_all(root_dir.path());
    let (_pool_dir, pool) = whiteboard_pool(2).await;

    let memory = Arc::new(CountingMemoryStore::new());
    let services = SupervisorServices {
        gate: fs_gate(
            engine(vec![PolicyRule::AutoApprove(Condition::Always)]),
            pool.clone(),
            root_dir.path().to_path_buf(),
        ),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        memory: memory.clone(),
        project_id: ProjectId("proj-s5".to_owned()),
    };

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root_dir.path(), one_write_script()), "agent-a")
        .expect("spawn agent process");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    let agent = summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == "agent-a")
        .expect("agent-a registered in the snapshot");
    assert_eq!(
        agent.state,
        AgentState::Completed,
        "the child's exit 0 is the terminal completed state"
    );
    assert_eq!(agent.restart_count, 0, "no restarts on the happy path");

    // The gated write materialized on disk through the real FilesystemTool.
    let written = root_dir.path().join("hello.txt");
    assert_eq!(
        std::fs::read_to_string(&written).expect("hello.txt exists"),
        "hello from agent",
        "the write must land on disk via the supervisor's gate"
    );

    // The whiteboard log carries WriteApplied before the terminal event,
    // with attribution bound to the registered agent id.
    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;

    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied.len(), 1, "exactly one gated write");
    let applied = applied[0];
    assert_eq!(applied.event_id, "call-1", "WAL keys on the loop's call id");
    assert_eq!(applied.agent_id, "agent-a", "wire agent_id is never trusted");
    assert_eq!(applied.scope, "fs");
    assert_eq!(applied.payload["tool"], json!("filesystem"));
    assert_eq!(applied.payload["input"]["path"], json!("hello.txt"));
    assert_eq!(applied.payload["policy_verdict"], json!("allow"));

    let completed: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::SubtaskCompleted).collect();
    assert_eq!(completed.len(), 1, "the child publishes its terminal event");
    assert_eq!(completed[0].agent_id, "agent-a");
    assert!(
        applied.gate_seq < completed[0].gate_seq,
        "the write precedes the terminal event in one global order"
    );
    assert_eq!(applied.agent_seq, 1, "per-agent sequence starts at the write");
    assert_eq!(completed[0].agent_seq, 2);

    // Each loop iteration retrieves working memory through the spine.
    assert!(
        memory.retrieval_count() >= 1,
        "the child's `retrieve-memory` requests must reach the supervisor spine"
    );
}

#[tokio::test]
async fn denied_write_fails_the_tool_call_but_the_agent_completes() {
    let root_dir = tempfile::tempdir().expect("tempdir");
    git_commit_all(root_dir.path());
    let (_pool_dir, pool) = whiteboard_pool(2).await;

    let services = SupervisorServices {
        gate: fs_gate(
            engine(vec![PolicyRule::AutoDeny(Condition::Always)]),
            pool.clone(),
            root_dir.path().to_path_buf(),
        ),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-s5".to_owned()),
    };

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root_dir.path(), one_write_script()), "agent-a")
        .expect("spawn agent process");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    let agent = summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == "agent-a")
        .expect("agent-a registered in the snapshot");
    assert_eq!(
        agent.state,
        AgentState::Completed,
        "the child still completes after a denied write (the denial was a tool result)"
    );

    // Fail-closed: nothing materialized, nothing persisted.
    assert!(!root_dir.path().join("hello.txt").exists(), "a denied write must never reach disk");
    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;
    // The denial itself is persisted (auditability), but the tool never ran.
    assert!(
        !events.iter().any(|e| e.kind == WhiteboardKind::WriteApplied),
        "a denied write must persist no WriteApplied row"
    );
    assert_eq!(events.len(), 2, "WriteRejected then the terminal event");
    assert_eq!(events[0].kind, WhiteboardKind::WriteRejected);
    assert_eq!(events[0].agent_id, "agent-a", "wire agent_id is never trusted");
    assert_eq!(events[0].gate_seq, 1);
    assert_eq!(events[0].agent_seq, 1);
    assert_eq!(
        events[0].payload["reason"],
        json!("Deny"),
        "the policy verdict is recorded on the row"
    );
    assert_eq!(events[1].kind, WhiteboardKind::SubtaskCompleted);
    assert_eq!(events[1].agent_id, "agent-a");
    assert_eq!(events[1].gate_seq, 2);
    assert_eq!(events[1].agent_seq, 2);
}
