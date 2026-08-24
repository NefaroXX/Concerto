//! ADR-60 Testing §1 — replay-diff harness (Phase 4, oracle comment 3).
//!
//! Runs the SAME deterministic fixture twice against independent
//! supervisor/gate/log instances and asserts byte-identical results across
//! three surfaces:
//!
//! 1. the **total-ordered whiteboard log** — canonicalized rows including
//!    `gate_seq`, so a different interleaving with the same end state is a
//!    loud failure, not a pass;
//! 2. the final **VirtualFs** state under the gate root (file bytes hashed);
//! 3. the persisted **subscription cursors** (`whiteboard_subscriptions`).
//!
//! Divergence anywhere fails loudly (`assert_eq!` prints the diff).
//!
//! Determinism strategy: true concurrency cannot produce a deterministic
//! total order, so the fixture sequences its two agents through explicit
//! phases against one shared log — agent-a writes the shared file and
//! completes, and only then agent-b runs, first attempting a DECLARED STALE
//! `base_version` (a deterministic conflict that appends zero rows) and then
//! a fresh unclaimed rewrite that applies. Every `gate_seq` in the expected
//! order is pinned, which is exactly what makes replay comparison meaningful.
//! Genuine concurrent interleaving is covered by `supervisor_conflict.rs`
//! and `supervisor_parallel_e2e.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, PolicyError};
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery, ProjectId};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::AuditLog;
use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_orchestrator::gate::{FilePreImageReader, WriteGate};
use concerto_orchestrator::subscriptions::SubscriptionManager;
use concerto_orchestrator::supervisor::{
    AgentState, Supervisor, SupervisorConfig, SupervisorServices,
};
use concerto_sessions::whiteboard::{
    load_whiteboard_events, load_whiteboard_subscription, WhiteboardEvent, WhiteboardKind,
    WhiteboardLoadOpts,
};
use concerto_tools::filesystem::FilesystemTool;
use serde_json::{json, Value};
use tempfile::TempDir;

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

fn allow_engine() -> std::sync::Arc<SimplePolicyEngine> {
    std::sync::Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        std::sync::Arc::new(TestAudit),
    ))
}

fn fs_gate(pool: sqlx::SqlitePool, root: PathBuf) -> std::sync::Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
    let executor =
        std::sync::Arc::new(ToolExecutor::new(std::sync::Arc::new(registry), allow_engine()));
    std::sync::Arc::new(WriteGate::new(
        allow_engine(),
        executor,
        pool,
        std::sync::Arc::new(FilePreImageReader::new(root.clone())),
        root,
        1,
    ))
}

/// Memory spine stub (the harness exercises writes, not retrieval).
struct NullMemory;

#[async_trait]
impl MemoryStore for NullMemory {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(vec![])
    }
    async fn store(
        &self,
        _entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        Ok(MemoryId(ulid::Ulid::new()))
    }
    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}

async fn whiteboard_pool(name: &str) -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join(name))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = sqlx::pool::PoolOptions::new()
        .max_connections(6)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

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
    git(&["config", "user.email", "replay-diff@invalid"]);
    git(&["config", "user.name", "Replay Diff Harness"]);
    std::fs::write(dir.join(".gitkeep"), "").expect("seed file written");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
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

/// A write turn whose arguments carry a DECLARED `base_versions` claim —
/// declare-wins over the supervisor's always-on injection, so a stale claim
/// deterministically surfaces as a conflict (zero whiteboard rows).
fn write_turn_claimed_stale(call_id: &str, path: &str, content: &str, stale_hash: &str) -> Value {
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
                    "base_versions": { path: stale_hash }
                }
            },
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

/// The real agent-process binary with the S5 environment contract.
fn agent_process(root: &Path, script: Value, agent_id: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-agent-process"));
    command
        .env("CONCERTO_AGENT_ID", agent_id)
        .env("CONCERTO_PROJECT_ROOT", root.display().to_string())
        .env("CONCERTO_TASK_DESCRIPTION", "replay fixture task")
        .env("CONCERTO_MAX_ITERATIONS", "20")
        .env("CONCERTO_PROVIDER", "mock")
        .env("CONCERTO_MOCK_SCRIPT_JSON", script.to_string());
    command
}

/// Canonical replay view of one event: the log's total-order content minus
/// everything that is wall-clock or nonce-like BY CONSTRUCTION (fresh ULIDs
/// for terminal events/task ids, session ids, timestamps, derived hashes).
/// `event_id` is kept only for writes (the caller's idempotency key).
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

async fn all_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
    load_whiteboard_events(pool, &WhiteboardLoadOpts::default()).await.expect("load events")
}

/// Deterministic filesystem snapshot: relative path → blake3 of bytes.
fn fs_snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut snapshot = BTreeMap::new();
    for entry in walk(root) {
        let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with(".git") {
            continue;
        }
        let rel = entry.strip_prefix(root).expect("under root").to_string_lossy().to_string();
        let bytes = std::fs::read(&entry).expect("readable");
        snapshot.insert(rel, blake3_hex(&bytes));
    }
    snapshot
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The persisted subscription cursors (subscriber → cursor), sorted.
async fn cursors(pool: &sqlx::SqlitePool) -> Vec<(String, u64)> {
    let mut rows = Vec::new();
    for subscriber in ["agent-a", "agent-b"] {
        if let Some(subscription) =
            load_whiteboard_subscription(pool, subscriber).await.expect("cursor load")
        {
            rows.push((subscription.subscriber_id, subscription.cursor_gate_seq));
        }
    }
    rows
}

/// One full deterministic fixture run: fresh supervisor/gate/log, agent-a
/// writes the shared file to completion, then agent-b attempts the stale-
/// claimed write (deterministic conflict, zero rows) and rewrites freshly.
async fn run_fixture() -> (Vec<Value>, BTreeMap<String, String>, Vec<(String, u64)>) {
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    std::fs::write(gate_root.path().join("shared.txt"), "seed").expect("seed written");
    git_commit_all(gate_root.path());
    let root_a = tempfile::tempdir().expect("agent-a worktree");
    let root_b = tempfile::tempdir().expect("agent-b worktree");
    git_commit_all(root_a.path());
    git_commit_all(root_b.path());
    let (_pool_dir, pool) = whiteboard_pool("replay_diff.db").await;

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        memory: std::sync::Arc::new(NullMemory),
        project_id: ProjectId("proj-replay".to_owned()),
        consolidation: None,
    };
    // Both workers subscribe like production (Decision topics); neither
    // acks, so both cursors stay at 0 — still a compared surface.
    let config = SupervisorConfig::default()
        .with_whiteboard_subscription("agent-a", vec![WhiteboardKind::Decision])
        .with_whiteboard_subscription("agent-b", vec![WhiteboardKind::Decision]);

    let mut supervisor = Supervisor::new(config);
    supervisor = supervisor.with_services(services);

    // Phase 1 — agent-a alone: deterministic prefix of the total order.
    let script_a = json!([write_turn("call-a", "shared.txt", "hello from agent-a"), done_turn()]);
    supervisor
        .spawn_agent(&mut agent_process(root_a.path(), script_a, "agent-a"), "agent-a")
        .expect("spawn agent-a");
    let shutdown = CancellationToken::new();
    let summary_a = supervisor
        .run_until(shutdown.clone(), |supervisor| {
            supervisor.agent("agent-a").is_some_and(|meta| {
                matches!(meta.state, AgentState::Completed | AgentState::Failed)
            })
        })
        .await;
    assert!(summary_a.failed.is_empty(), "phase 1 must not fail: {:?}", summary_a.failed);
    assert_eq!(
        summary_a.agents.iter().find(|m| m.agent_id == "agent-a").map(|m| m.state),
        Some(AgentState::Completed)
    );

    // Phase 2 — agent-b against the SAME log/gate: stale claim first.
    let stale = blake3_hex(b"an-outdated-view-of-shared.txt");
    let script_b = json!([
        write_turn_claimed_stale("call-b-stale", "shared.txt", "hijack", &stale),
        write_turn("call-b", "shared.txt", "hello from agent-b"),
        done_turn(),
    ]);
    supervisor
        .spawn_agent(&mut agent_process(root_b.path(), script_b, "agent-b"), "agent-b")
        .expect("spawn agent-b");
    let summary_b = supervisor
        .run_until(shutdown.clone(), |supervisor| {
            supervisor.agent("agent-b").is_some_and(|meta| {
                matches!(meta.state, AgentState::Completed | AgentState::Failed)
            })
        })
        .await;
    shutdown.cancel();
    assert!(summary_b.failed.is_empty(), "phase 2 must not fail: {:?}", summary_b.failed);
    assert_eq!(
        summary_b.agents.iter().find(|m| m.agent_id == "agent-b").map(|m| m.state),
        Some(AgentState::Completed)
    );

    let events = poll_events(&pool, 4).await;
    assert!(
        !events.iter().any(|event| event.event_id == "call-b-stale"),
        "the declared-stale attempt must leave zero whiteboard rows"
    );
    (events.iter().map(canonical).collect(), fs_snapshot(gate_root.path()), cursors(&pool).await)
}

/// Poll the log until `expected` rows exist or the timeout expires (detached
/// write-path handlers may still be committing when the loop returns).
async fn poll_events(pool: &sqlx::SqlitePool, expected: usize) -> Vec<WhiteboardEvent> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let events = all_events(pool).await;
        if events.len() >= expected {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} whiteboard events"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_fixture_replays_byte_identical_log_fs_and_cursors() {
    let (log_1, fs_1, cursors_1) = run_fixture().await;
    let (log_2, fs_2, cursors_2) = run_fixture().await;

    // The pinned TOTAL ORDER — divergence in interleaving fails here even
    // when end states would have matched (oracle comment 3).
    let expected = [
        json!({
            "event_id": "call-a",
            "gate_seq": 1,
            "kind": "write-applied",
            "agent_id": "agent-a",
        }),
        json!({ "event_id": "", "gate_seq": 2, "kind": "subtask-completed", "agent_id": "agent-a" }),
        json!({
            "event_id": "call-b",
            "gate_seq": 3,
            "kind": "write-applied",
            "agent_id": "agent-b",
        }),
        json!({ "event_id": "", "gate_seq": 4, "kind": "subtask-completed", "agent_id": "agent-b" }),
    ];
    assert_eq!(log_1.len(), 4, "exactly the four deterministic events: {log_1:?}");
    for (index, event) in log_1.iter().enumerate() {
        for key in ["event_id", "gate_seq", "kind", "agent_id"] {
            assert_eq!(
                event[key], expected[index][key],
                "event {index} field {key} diverged from the pinned order"
            );
        }
    }

    // Full-surface byte-identical replay across independent runs.
    assert_eq!(log_1, log_2, "total-ordered logs must be byte-identical");
    assert_eq!(fs_1, fs_2, "final VirtualFs snapshots must be identical");
    assert_eq!(cursors_1, cursors_2, "subscription cursors must be identical");

    // And the shared file ends with exactly one complete rewrite.
    assert_eq!(
        fs_1.get("shared.txt").map(String::as_str),
        Some(blake3_hex(b"hello from agent-b").as_str()),
        "agent-b's fresh rewrite is the deterministic final state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mutated_fixture_diverges_loudly() {
    // Negative control for the comparator: one changed input must surface as
    // a diff, proving the equality assertions above are sensitive.
    let baseline = run_fixture().await;

    let gate_root = tempfile::tempdir().expect("gate root");
    std::fs::write(gate_root.path().join("shared.txt"), "seed").expect("seed written");
    git_commit_all(gate_root.path());
    let root_a = tempfile::tempdir().expect("worktree");
    git_commit_all(root_a.path());
    let (_pool_dir, pool) = whiteboard_pool("replay_divergent.db").await;
    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        memory: std::sync::Arc::new(NullMemory),
        project_id: ProjectId("proj-replay".to_owned()),
        consolidation: None,
    };
    let config = SupervisorConfig::default().with_whiteboard_subscription("agent-a", vec![]);
    let mut supervisor = Supervisor::new(config);
    supervisor = supervisor.with_services(services);
    let script = json!([write_turn("call-a", "shared.txt", "MUTATED"), done_turn()]);
    supervisor
        .spawn_agent(&mut agent_process(root_a.path(), script, "agent-a"), "agent-a")
        .expect("spawn agent-a");
    let shutdown = CancellationToken::new();
    supervisor
        .run_until(shutdown.clone(), |supervisor| {
            supervisor.agent("agent-a").is_some_and(|meta| {
                matches!(meta.state, AgentState::Completed | AgentState::Failed)
            })
        })
        .await;
    shutdown.cancel();

    let mutated_log: Vec<Value> = all_events(&pool).await.iter().map(canonical).collect();
    assert_ne!(
        baseline.0, mutated_log,
        "a content change MUST surface as a replay diff (divergence = loud failure)"
    );
}
