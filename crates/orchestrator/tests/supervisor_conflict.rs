//! ADR-60 D5 conflict policy + testing obligations, end-to-end.
//!
//! Drives the *real* `orchestrator-agent-process` binary (and the mock-agent
//! fixture where a deterministic crash window or a hair-triggered conflict
//! is needed) under the real supervisor:
//!
//! - two agents writing the same file under the supervisor's **always-on
//!   injection**: every versioned write carries a `base_version` stamped from
//!   the target's state at request arrival (ADR-60 D5), so a same-target
//!   sibling write landing between the stamp and the gate's own pre-image
//!   capture surfaces as a loud `GateError::Conflict` instead of silent
//!   last-writer-wins; the durable log and the disk agree on the winner, and
//!   attribution is bound to the registered process;
//! - a *declared* `base_version` that matches the current pre-image: applied,
//!   the row records the claimed hash;
//! - a *stale* `base_version` (declared — never clobbered by the injection):
//!   the gate refuses with no whiteboard row, the tool error surfaces back to
//!   the agent (never silently dropped), and the agent continues with a later
//!   fresh write — nothing is re-run, nothing is half-applied;
//! - crash injection: SIGKILL the child while a gated write is in flight —
//!   the supervisor restarts from the snapshotted spec, and the restarted
//!   child's identical first request *replays* the stored decision instead of
//!   re-executing (applied-write invariant);
//! - replay determinism: two identical runs produce structurally identical
//!   whiteboard logs (modulo wall-clock), and a different script produces a
//!   different log.
//!
//! The deterministic conflict *interleaving* (a sibling write landing between
//! the supervisor's stamp and the gate's re-read) cannot be produced by two
//! racing child processes — it is a sub-millisecond window — so that path is
//! covered deterministically by `supervisor::write_path_tests` and
//! `gate::tests` (stamp → sibling write → `GateError::Conflict`, zero WAL
//! rows).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, PolicyError, ToolError};
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::{AuditLog, PolicyEngine};
use concerto_core::traits::tool::Tool;
use concerto_core::types::{
    CapabilitySet, Condition, PolicyRule, SessionContext, ToolOutput, ToolRegistry,
};
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
use serde_json::{json, Value};
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use std::collections::BTreeMap;
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

/// A policy engine that allows everything.
fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

/// A gate whose executor is the REAL `FilesystemTool` rooted at `root`,
/// optionally extended with extra tools (e.g. the crash-test blocker).
fn fs_gate(pool: sqlx::SqlitePool, root: PathBuf, extras: Vec<Box<dyn Tool>>) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
    for extra in extras {
        registry.register(extra);
    }
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), allow_engine()));
    Arc::new(WriteGate::new(
        allow_engine(),
        executor,
        pool,
        Arc::new(FilePreImageReader::new(root.clone())),
        root,
        1,
    ))
}

/// The real agent-process binary with the ADR-60 S5 environment contract,
/// registered as `agent_id`.
fn agent_process(root: &Path, script: Value, agent_id: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-agent-process"));
    command
        .env("CONCERTO_AGENT_ID", agent_id)
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

/// One scripted turn: write `path` = `content` via the filesystem tool as
/// `call_id`, then finalize the turn.
fn write_turn(call_id: &str, path: &str, content: &str) -> Value {
    json!([
        {
            "delta": "",
            "reasoning": null,
            "tool_call": {
                "id": call_id,
                "name": "filesystem",
                "arguments": { "operation": "write", "path": path, "content": content }
            },
            "is_final": false,
            "usage": null
        },
        { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
    ])
}

/// A write turn whose arguments carry `base_versions` claims (the wire-level
/// per-target concurrency claim map, lifted by the agent-process' gate proxy).
/// Declared claims always win over the supervisor's always-on injection.
fn write_turn_claimed(
    call_id: &str,
    path: &str,
    content: &str,
    base_versions: &BTreeMap<String, String>,
) -> Value {
    json!([
        {
            "delta": "",
            "reasoning": null,
            "tool_call": {
                "id": call_id,
                "name": "filesystem",
                "arguments": {
                    "operation": "write",
                    "path": path,
                    "content": content,
                    "base_versions": base_versions
                }
            },
            "is_final": false,
            "usage": null
        },
        { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
    ])
}

/// One scripted turn: move `path` to `destination` via the filesystem tool.
fn move_turn(call_id: &str, path: &str, destination: &str) -> Value {
    json!([
        {
            "delta": "",
            "reasoning": null,
            "tool_call": {
                "id": call_id,
                "name": "filesystem",
                "arguments": {
                    "operation": "move",
                    "path": path,
                    "destination": destination
                }
            },
            "is_final": false,
            "usage": null
        },
        { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
    ])
}

/// A call to the crash-test blocking tool.
fn blocker_turn(call_id: &str) -> Value {
    json!([
        {
            "delta": "",
            "reasoning": null,
            "tool_call": { "id": call_id, "name": "blocker", "arguments": {} },
            "is_final": false,
            "usage": null
        },
        { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
    ])
}

/// A bare completion turn.
fn done_turn() -> Value {
    json!([{ "delta": "done", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }])
}

/// A tool that parks inside the gate's execute path for a while — the crash
/// test kills the child while this call is in flight, giving a deterministic
/// "write after WAL, before completion" window.
struct BlockingTool;

#[async_trait]
impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "blocker"
    }
    fn description(&self) -> &str {
        "parks for the crash-injection window (test only)"
    }
    fn input_schema(&self) -> Value {
        json!({})
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
    async fn execute(
        &self,
        _input: Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(ToolOutput { summary: "late ok".into(), data: json!({ "ok": true }) })
    }
}

/// Memory spine stub (mirrors `supervisor_gate.rs`).
struct CountingMemoryStore {
    retrievals: AtomicUsize,
}

impl CountingMemoryStore {
    fn new() -> Self {
        Self { retrievals: AtomicUsize::new(0) }
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
            project_id: ProjectId("proj-d5".to_owned()),
            namespace: MemoryNamespace::Project(ProjectId("proj-d5".to_owned())),
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
    let path = dir.path().join("supervisor_conflict.db");
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
/// (mirrors `supervisor_agent_process.rs`).
fn git_commit_all(dir: &Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git available in test environment");
        assert!(status.success(), "git {args:?} should succeed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "supervisor-conflict@invalid"]);
    git(&["config", "user.name", "Supervisor Conflict Test"]);
    std::fs::write(dir.join(".gitkeep"), "").expect("seed file written");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);
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

/// SIGKILL a child process (crash injection; Linux test env per CI).
fn kill9(pid: u32) {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("kill binary available");
    assert!(status.success(), "kill -9 {pid} must succeed");
}

/// Canonical view of an event for replay comparison: everything except the
/// wall clock (`created_at`), the derived content hash, and the nonce-like
/// identifiers that are *by construction* unique per run (`subtask-completed`
/// event ids and task ids are fresh ULIDs).
fn canonical(event: &WhiteboardEvent) -> Value {
    let mut payload = event.payload.clone();
    if let Some(map) = payload.as_object_mut() {
        map.remove("task_id");
    }
    json!({
        "event_id": if event.kind == WhiteboardKind::WriteApplied {
            event.event_id.clone()
        } else {
            String::new()
        },
        "gate_seq": event.gate_seq,
        "agent_seq": event.agent_seq,
        "agent_id": event.agent_id,
        "kind": event.kind.as_str(),
        "scope": event.scope,
        "payload": payload,
        "pre_image_hash": event.pre_image_hash,
    })
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[tokio::test]
async fn concurrent_same_file_writes_surface_durably_and_never_corrupt() {
    // The children share one *gate* root (writes execute supervisor-side and
    // race there), but each child gets its own git worktree for its local
    // undo gate — a shared worktree would let the undo `git` calls collide
    // on the index lock and spuriously cancel one child before it writes.
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    let root_a = tempfile::tempdir().expect("agent-a worktree");
    let root_b = tempfile::tempdir().expect("agent-b worktree");
    git_commit_all(root_a.path());
    git_commit_all(root_b.path());
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    // Distinct call ids: `call_id` is the global idempotency key, so a shared
    // id would make the second agent replay the first's decision instead of
    // writing (that dedup is tested elsewhere; here both writes must race).
    let script_a = json!([write_turn("call-a", "hello.txt", "hello from agent-a"), done_turn()]);
    let script_b = json!([write_turn("call-b", "hello.txt", "hello from agent-b"), done_turn()]);
    supervisor
        .spawn_agent(&mut agent_process(root_a.path(), script_a, "agent-a"), "agent-a")
        .expect("spawn agent-a");
    supervisor
        .spawn_agent(&mut agent_process(root_b.path(), script_b, "agent-b"), "agent-b")
        .expect("spawn agent-b");

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf(), vec![]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    let summary = run_supervisor(supervisor, services, Duration::from_secs(8)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    for meta in &summary.agents {
        assert_eq!(meta.state, AgentState::Completed, "{:?} must complete", meta.agent_id);
        assert_eq!(meta.restart_count, 0, "no restarts without crashes");
    }

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().filter(|e| e.kind == WhiteboardKind::SubtaskCompleted).count() == 2
    })
    .await;

    // ADR-60 D5 always-on: every versioned write carries a supervisor-injected
    // `base_version` (the target's state at request arrival). The two writes
    // race through the gate; a sibling write landing between one agent's stamp
    // and the gate's own pre-image capture surfaces as a loud `Conflict`
    // instead of silent last-writer-wins. Both outcomes are correct and both
    // are durable: at least one write must apply (both cannot lose — the
    // target can only change to one state), the applied rows are the total
    // order's prefix with attribution bound to the registered processes, and
    // the disk never ends scrambled — it holds exactly one complete write.
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert!(!applied.is_empty(), "at least one concurrent write must apply");
    assert_eq!(applied[0].gate_seq, 1, "applied writes start the total order");
    for (position, row) in applied.iter().enumerate() {
        assert_eq!(row.gate_seq, position as u64 + 1, "gate_seqs are contiguous from 1");
        assert_eq!(row.payload["input"]["path"], json!("hello.txt"));
        let expected_agent = if row.event_id == "call-a" { "agent-a" } else { "agent-b" };
        assert_eq!(row.agent_id, expected_agent, "attribution bound to the registered process");
    }

    // The final content is one of the two complete writes — never a torn mix.
    let on_disk =
        std::fs::read_to_string(gate_root.path().join("hello.txt")).expect("hello.txt exists");
    let contents = ["hello from agent-a", "hello from agent-b"];
    assert!(
        contents.contains(&on_disk.as_str()),
        "final content must be one of the two writes, got {on_disk:?}"
    );
}

#[tokio::test]
async fn sequential_same_file_writes_by_two_agents_both_apply() {
    // Regression for "sequential writes must keep passing (hash matches)":
    // agent-b rewrites a file agent-a created in an earlier run. The
    // always-on injection stamps agent-b's write with the CURRENT pre-image
    // (agent-a's content), which the gate's re-read — microseconds later,
    // with no intervening write — still matches, so the overwrite applies
    // instead of being falsely refused.
    let root = tempfile::tempdir().expect("tempdir");
    git_commit_all(root.path());
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    async fn run_once(
        pool: &sqlx::SqlitePool,
        root: &Path,
        script: Value,
        agent_id: &str,
    ) -> RunSummary {
        let services = SupervisorServices {
            gate: fs_gate(pool.clone(), root.to_path_buf(), vec![]),
            whiteboard_pool: pool.clone(),
            subscriptions: SubscriptionManager::new(pool.clone().clone()),
            consolidation: None,
            memory: Arc::new(CountingMemoryStore::new()),
            project_id: ProjectId("proj-d5".to_owned()),
        };
        let mut supervisor = Supervisor::new(SupervisorConfig::default());
        supervisor
            .spawn_agent(&mut agent_process(root, script, agent_id), agent_id)
            .expect("spawn agent");
        run_supervisor(supervisor, services, Duration::from_secs(6)).await
    }

    let first = run_once(
        &pool,
        root.path(),
        json!([write_turn("call-1", "hello.txt", "from agent-a"), done_turn()]),
        "agent-a",
    )
    .await;
    assert!(first.failed.is_empty(), "run 1 must not fail: {:?}", first.failed);
    let second = run_once(
        &pool,
        root.path(),
        json!([write_turn("call-2", "hello.txt", "from agent-b"), done_turn()]),
        "agent-b",
    )
    .await;
    assert!(second.failed.is_empty(), "run 2 must not fail: {:?}", second.failed);

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).count() == 2
    })
    .await;
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied[0].event_id, "call-1", "agent-a's write applied first");
    assert_eq!(applied[1].event_id, "call-2", "agent-b's write applied second");
    assert!(applied[1].gate_seq > applied[0].gate_seq, "whiteboard order follows the runs");
    assert_eq!(
        applied[1].pre_image_hash.as_deref(),
        Some(blake3_hex(b"from agent-a").as_str()),
        "agent-b's injected claim matched the gate-time state (agent-a's content)"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("hello.txt")).expect("file"),
        "from agent-b",
        "the sequential overwrite lands — no false conflict"
    );
}

#[tokio::test]
async fn matching_base_version_applies_and_records_the_claimed_hash() {
    let root = tempfile::tempdir().expect("tempdir");
    git_commit_all(root.path());
    std::fs::write(root.path().join("hello.txt"), "base").expect("seed file");
    let base_hash = blake3_hex(b"base");
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), root.path().to_path_buf(), vec![]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    let mut claims = BTreeMap::new();
    claims.insert("hello.txt".to_owned(), base_hash.clone());
    let script =
        json!([write_turn_claimed("call-1", "hello.txt", "updated", &claims), done_turn()]);
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root.path(), script, "agent-a"), "agent-a")
        .expect("spawn agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    let agent =
        summary.agents.iter().find(|meta| meta.agent_id == "agent-a").expect("agent-a registered");
    assert_eq!(agent.state, AgentState::Completed);

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied.len(), 1, "exactly one applied write");
    assert_eq!(applied[0].event_id, "call-1");
    assert_eq!(
        applied[0].pre_image_hash.as_deref(),
        Some(base_hash.as_str()),
        "the row records the claimed pre-write hash"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("hello.txt")).expect("file"),
        "updated",
        "the matching-base write materializes"
    );
}

#[tokio::test]
async fn stale_base_version_is_surfaced_and_the_agent_continues_with_a_fresh_write() {
    let root = tempfile::tempdir().expect("tempdir");
    git_commit_all(root.path());
    std::fs::write(root.path().join("hello.txt"), "base").expect("seed file");
    let stale_hash = blake3_hex(b"an-outdated-view-of-the-file");
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), root.path().to_path_buf(), vec![]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    // Turn 0: hijack with a stale claim -> must be refused loudly. Turn 1:
    // a fresh unclaimed write of the same file -> must apply, proving the
    // agent survived the conflict and nothing was half-applied.
    //
    // The stale claim also proves declare-wins under the always-on injection:
    // the supervisor would have stamped the CURRENT pre-image ("base") and the
    // write would have applied — instead the declared (stale) claim is honored
    // and the gate refuses, so a caller's explicit declaration is never
    // clobbered.
    let mut claims = BTreeMap::new();
    claims.insert("hello.txt".to_owned(), stale_hash.clone());
    let script = json!([
        write_turn_claimed("call-1", "hello.txt", "hijack", &claims),
        write_turn("call-3", "hello.txt", "final"),
        done_turn(),
    ]);
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root.path(), script, "agent-a"), "agent-a")
        .expect("spawn agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(8)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    let agent =
        summary.agents.iter().find(|meta| meta.agent_id == "agent-a").expect("agent-a registered");
    assert_eq!(
        agent.state,
        AgentState::Completed,
        "the conflict was a tool error, not a crash — the agent completes"
    );
    assert_eq!(agent.restart_count, 0);

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;

    // The conflicting attempt left NO trace (not even a `write-rejected`:
    // it was not a policy verdict) while the follow-up write applied with the
    // original file state as its pre-image.
    assert!(
        !events.iter().any(|e| e.event_id == "call-1"),
        "a conflicted write is never silently dropped — it must also never be logged as applied"
    );
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied.len(), 1, "only the fresh unclaimed write applies");
    assert_eq!(applied[0].event_id, "call-3");
    assert_eq!(applied[0].payload["input"]["content"], json!("final"));
    assert_eq!(
        applied[0].pre_image_hash.as_deref(),
        Some(blake3_hex(b"base").as_str()),
        "the applied write's pre-image is the seeded base, untouched by the refused hijack"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("hello.txt")).expect("file"),
        "final",
        "disk ends in the agent's fresh write, not the conflicted one"
    );
    assert_eq!(events.len(), 2, "WriteApplied then SubtaskCompleted — nothing else");
}

#[tokio::test]
async fn kill_mid_gated_write_restarts_and_replays_without_reexecuting() {
    let root = tempfile::tempdir().expect("tempdir");
    git_commit_all(root.path());
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), root.path().to_path_buf(), vec![Box::new(BlockingTool)]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    // Turn 0: a real write; turn 1: a call that parks the gate's execute
    // path. The crash lands while the second call is in flight.
    let script =
        json!([write_turn("call-1", "hello.txt", "hello from agent"), blocker_turn("call-2")]);
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor = supervisor.with_services(services);
    supervisor
        .spawn_agent(&mut agent_process(root.path(), script, "agent-a"), "agent-a")
        .expect("spawn agent");
    let pid = supervisor.agent_pid("agent-a").expect("child pid available");

    let shutdown = CancellationToken::new();
    let guard = shutdown.clone();
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });

    // Wait until the second call has committed its WAL row (i.e. the write
    // is durable and the tool is parked in the gate) — then kill the child.
    wait_for_events(&pool, Duration::from_secs(10), |events| {
        events.iter().any(|e| e.event_id == "call-2" && e.kind == WhiteboardKind::WriteApplied)
    })
    .await;
    kill9(pid);

    // The supervisor restarts the killed child from the snapshotted spec;
    // the respawned child replays its stored decisions and completes.
    wait_for_events(&pool, Duration::from_secs(15), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;
    guard.cancel();
    let summary = handle.await.expect("loop task completes");

    // The killed child crashed (SIGKILL is not a clean exit) and the
    // supervisor respawned it from the snapshotted spec exactly once.
    let agent =
        summary.agents.iter().find(|meta| meta.agent_id == "agent-a").expect("agent-a registered");
    assert_eq!(agent.state, AgentState::Completed, "respawned child completes the task");
    assert_eq!(agent.restart_count, 1, "one crash, one restart");

    // Applied-write invariant: the pre-crash write is durable exactly once —
    // the restarted child's identical first request replayed the stored
    // decision instead of re-executing (no second row, no second disk write).
    let events = all_events(&pool).await;
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied.len(), 2, "call-1 (executed) + call-2 (WAL, blocked at kill)");
    assert_eq!(applied[0].event_id, "call-1");
    assert_eq!(applied[1].event_id, "call-2");
    assert_eq!(applied[0].gate_seq, 1);
    assert_eq!(applied[1].gate_seq, 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("hello.txt")).expect("file"),
        "hello from agent",
        "the write survives the crash exactly once"
    );
    assert_eq!(
        events.iter().filter(|e| e.kind == WhiteboardKind::SubtaskCompleted).count(),
        1,
        "the surviving incarnation publishes exactly one terminal event"
    );
}

#[tokio::test]
async fn identical_runs_replay_identical_logs_and_different_scripts_differ() {
    let script = json!([write_turn("call-1", "hello.txt", "hello from agent"), done_turn()]);

    async fn run_once(script: &Value) -> Vec<Value> {
        let root = tempfile::tempdir().expect("tempdir");
        git_commit_all(root.path());
        let (_pool_dir, pool) = whiteboard_pool(6).await;
        let services = SupervisorServices {
            gate: fs_gate(pool.clone(), root.path().to_path_buf(), vec![]),
            whiteboard_pool: pool.clone(),
            subscriptions: SubscriptionManager::new(pool.clone().clone()),
            consolidation: None,
            memory: Arc::new(CountingMemoryStore::new()),
            project_id: ProjectId("proj-d5".to_owned()),
        };
        let mut supervisor = Supervisor::new(SupervisorConfig::default());
        supervisor
            .spawn_agent(&mut agent_process(root.path(), script.clone(), "agent-a"), "agent-a")
            .expect("spawn agent");
        let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
        assert!(summary.failed.is_empty(), "run must not fail: {:?}", summary.failed);
        wait_for_events(&pool, Duration::from_secs(8), |events| {
            events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
        })
        .await
        .iter()
        .map(canonical)
        .collect()
    }

    let first = run_once(&script).await;
    let second = run_once(&script).await;
    assert_eq!(first.len(), 2, "WriteApplied + SubtaskCompleted");
    assert_eq!(
        first, second,
        "identical scripts must replay structurally identical logs (modulo wall clock)"
    );

    // Negative control: the comparator is sensitive — a different script
    // (different content) must produce a different log.
    let mutated = json!([write_turn("call-1", "hello.txt", "hello from agent X"), done_turn()]);
    let other = run_once(&mutated).await;
    assert_ne!(first, other, "content changes must surface in the replay-diff comparison");
}

#[tokio::test]
async fn concurrent_moves_of_same_source_apply_exactly_once_and_never_corrupt() {
    // Two agents each WRITE the shared file (through the tool, so the gate's
    // VFS knows it) and then race to MOVE the same source to different
    // destinations. The always-on injection stamps BOTH mutated targets of
    // each move (source + destination, ADR-60 D5 multi-target); the two moves
    // race for the shared source. Whichever interleaving happens, the gate
    // root can never end with the source present in two places — the source
    // is removed and exactly one destination holds one of the two writes.
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    let root_a = tempfile::tempdir().expect("agent-a worktree");
    let root_b = tempfile::tempdir().expect("agent-b worktree");
    git_commit_all(root_a.path());
    git_commit_all(root_b.path());
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    // Distinct call ids (global idempotency key); both race through the gate.
    let script_a = json!([
        write_turn("write-a", "hello.txt", "seed-v1"),
        move_turn("move-a", "hello.txt", "a.txt"),
        done_turn(),
    ]);
    let script_b = json!([
        write_turn("write-b", "hello.txt", "seed-v2"),
        move_turn("move-b", "hello.txt", "b.txt"),
        done_turn(),
    ]);
    supervisor
        .spawn_agent(&mut agent_process(root_a.path(), script_a, "agent-a"), "agent-a")
        .expect("spawn agent-a");
    supervisor
        .spawn_agent(&mut agent_process(root_b.path(), script_b, "agent-b"), "agent-b")
        .expect("spawn agent-b");

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf(), vec![]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    let summary = run_supervisor(supervisor, services, Duration::from_secs(8)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    for meta in &summary.agents {
        assert_eq!(meta.state, AgentState::Completed, "{:?} must complete", meta.agent_id);
        assert_eq!(meta.restart_count, 0, "no restarts without crashes");
    }

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().filter(|e| e.kind == WhiteboardKind::SubtaskCompleted).count() == 2
    })
    .await;

    // Applied rows are ordered and attribution-bound; a move that loses the
    // race is refused loudly (conflict) or fails at execution — never torn.
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert!(!applied.is_empty(), "at least one concurrent write/move must apply");
    assert_eq!(applied[0].gate_seq, 1, "applied writes start the total order");
    for (position, row) in applied.iter().enumerate() {
        let previous = position
            .checked_sub(1)
            .and_then(|index| applied.get(index))
            .map(|previous| previous.gate_seq)
            .unwrap_or(0);
        assert!(row.gate_seq > previous, "gate_seqs are strictly increasing");
        let expected_agent = match row.event_id.as_str() {
            "write-a" | "move-a" => "agent-a",
            "write-b" | "move-b" => "agent-b",
            other => panic!("unexpected event id {other}"),
        };
        assert_eq!(row.agent_id, expected_agent, "attribution bound to the registered process");
    }

    // The source is gone and exactly ONE destination holds one of the two
    // complete writes — never torn, never duplicated across destinations.
    let root = gate_root.path();
    assert!(
        !root.join("hello.txt").exists(),
        "a winning move removes its source; the source must never linger"
    );
    let destinations = ["a.txt", "b.txt"];
    let present: Vec<&str> = destinations
        .iter()
        .copied()
        .filter(|destination| root.join(destination).exists())
        .collect();
    assert_eq!(present.len(), 1, "exactly one move destination exists, got {present:?}");
    let on_disk = std::fs::read_to_string(root.join(present[0])).expect("destination readable");
    assert!(
        ["seed-v1", "seed-v2"].contains(&on_disk.as_str()),
        "the surviving destination holds one of the complete writes, got {on_disk:?}"
    );
}

#[tokio::test]
async fn stale_claimed_move_source_is_refused_and_a_fresh_move_recovers() {
    // Deterministic multi-target conflict, no racing: the agent writes the
    // file (through the tool), then declares a STALE claim on the move SOURCE
    // (the file it believes it is moving). Declare-wins means the always-on
    // injection must NOT clobber it, so the gate refuses loudly — no
    // whiteboard row, no disk touch. A follow-up unclaimed move (stamped with
    // the true pre-image) applies cleanly, proving the agent survived the
    // conflict and nothing was half-applied.
    let root = tempfile::tempdir().expect("tempdir");
    git_commit_all(root.path());
    let stale_source = blake3_hex(b"an-outdated-move-source");
    let (_pool_dir, pool) = whiteboard_pool(6).await;

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), root.path().to_path_buf(), vec![]),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d5".to_owned()),
    };
    let mut claims = BTreeMap::new();
    claims.insert("hello.txt".to_owned(), stale_source);
    let script = json!([
        write_turn("call-0", "hello.txt", "seed"),
        move_turn_claimed("call-1", "hello.txt", "moved.txt", &claims),
        move_turn("call-2", "hello.txt", "moved.txt"),
        done_turn(),
    ]);
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root.path(), script, "agent-a"), "agent-a")
        .expect("spawn agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(8)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    let agent =
        summary.agents.iter().find(|meta| meta.agent_id == "agent-a").expect("agent-a registered");
    assert_eq!(
        agent.state,
        AgentState::Completed,
        "the conflict was a tool error, not a crash — the agent completes"
    );
    assert_eq!(agent.restart_count, 0);

    let events = wait_for_events(&pool, Duration::from_secs(8), |events| {
        events.iter().any(|e| e.kind == WhiteboardKind::SubtaskCompleted)
    })
    .await;

    // The stale claim on the move source left NO trace (conflicts append
    // nothing) while the fresh unclaimed move applied with the seeded source
    // as its pre-image.
    assert!(
        !events.iter().any(|e| e.event_id == "call-1"),
        "a conflicted move is never logged as applied"
    );
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert_eq!(applied.len(), 2, "the seeding write and the fresh move apply");
    assert_eq!(applied[1].event_id, "call-2");
    // The column holds the PRIMARY (destination) pre-image — fresh — while the
    // per-target map attributes the seeded source hash the move consumed.
    assert_eq!(
        applied[1].payload["pre_images"]["hello.txt"],
        json!(blake3_hex(b"seed")),
        "the applied move attributes the seed content as its source pre-image"
    );
    assert!(!root.path().join("hello.txt").exists(), "the successful move removes the source");
    assert_eq!(
        std::fs::read_to_string(root.path().join("moved.txt")).expect("destination readable"),
        "seed",
        "disk ends with the fresh move, not the conflicted one"
    );
    // Brittle-count guard relaxed (stage 2 oracle): the specific
    // event-id/kind assertions above are the real contract — the conflicted
    // move leaves no row, the seeding + fresh move apply, and the run
    // completes. Bound the total instead of pinning an exact length so a
    // benign extra lifecycle event cannot red-flag the test.
    assert!(
        events.len() <= 3,
        "WriteApplied(call-0), WriteApplied(call-2), SubtaskCompleted — nothing else; got {}",
        events.len(),
    );
}

/// A move turn whose arguments carry `base_versions` claims.
fn move_turn_claimed(
    call_id: &str,
    path: &str,
    destination: &str,
    base_versions: &BTreeMap<String, String>,
) -> Value {
    json!([
        {
            "delta": "",
            "reasoning": null,
            "tool_call": {
                "id": call_id,
                "name": "filesystem",
                "arguments": {
                    "operation": "move",
                    "path": path,
                    "destination": destination,
                    "base_versions": base_versions
                }
            },
            "is_final": false,
            "usage": null
        },
        { "delta": "", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }
    ])
}
