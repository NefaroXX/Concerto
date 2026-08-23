//! ADR-60 Testing §3 — parallel e2e (Phase 4): two agents CONCURRENTLY active
//! on one project sharing one file, under the supervisor's single write gate.
//!
//! Pinned obligations:
//! - **Attribution**: every applied row's `agent_id` is bound to the
//!   registered process, and per-agent sequences stay consistent.
//! - **Loud conflict**: agent-b's declared STALE `base_version` attempt
//!   deterministically surfaces as a gate conflict (`-32005` on the wire,
//!   retriable) and appends ZERO whiteboard rows — never silent, never a
//!   crash; the agent continues and completes.
//! - **Per-agent revert** (D5): restore-and-replay-excluding-one-agent,
//!   computed by replaying the total-ordered log without that agent's
//!   `event_ids`. Which sibling survives is derived from the log — either
//!   sibling can win the race or be the conflict victim, so no outcome is
//!   hard-coded.
//! - **Disk consistency**: the shared file ends holding exactly one complete
//!   applied write. The gate commits the whiteboard row BEFORE executing the
//!   tool (WAL-before-execute), so under concurrency the physical completion
//!   order may invert relative to `gate_seq` order — disk is asserted against
//!   the set of applied contents, never against the log's last writer.
//!
//! The deterministic replay variant of the shared-file scenario lives in
//! `replay_diff.rs`; this file tolerates every interleaving.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

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
    AgentState, RunSummary, Supervisor, SupervisorConfig, SupervisorServices,
};
use concerto_sessions::whiteboard::{
    load_whiteboard_events, WhiteboardEvent, WhiteboardKind, WhiteboardLoadOpts,
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

fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

fn fs_gate(pool: sqlx::SqlitePool, root: PathBuf) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
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

async fn whiteboard_pool() -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("parallel_e2e.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
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
    git(&["config", "user.email", "parallel-e2e@invalid"]);
    git(&["config", "user.name", "Parallel E2E"]);
    std::fs::write(dir.join(".gitkeep"), "").expect("seed file written");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);
}

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

fn done_turn() -> Value {
    json!([{ "delta": "done", "reasoning": null, "tool_call": null, "is_final": true, "usage": null }])
}

fn agent_process(root: &Path, script: Value, agent_id: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-agent-process"));
    command
        .env("CONCERTO_AGENT_ID", agent_id)
        .env("CONCERTO_PROJECT_ROOT", root.display().to_string())
        .env("CONCERTO_TASK_DESCRIPTION", "parallel fixture task")
        .env("CONCERTO_MAX_ITERATIONS", "20")
        .env("CONCERTO_PROVIDER", "mock")
        .env("CONCERTO_MOCK_SCRIPT_JSON", script.to_string());
    command
}

async fn all_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
    load_whiteboard_events(pool, &WhiteboardLoadOpts::default()).await.expect("load events")
}

/// Per-agent revert (ADR-60 D5): restore-to-base + replay excluding one
/// agent's event ids. Slice scope: `write` operations only (the only ops this
/// fixture performs); move/copy/delete revert remains future work.
fn revert_agent(
    events: &[WhiteboardEvent],
    exclude_agent: Option<&str>,
) -> BTreeMap<String, String> {
    let mut state = BTreeMap::new();
    for event in events {
        if event.kind != WhiteboardKind::WriteApplied {
            continue;
        }
        if exclude_agent.is_some_and(|agent| event.agent_id == agent) {
            continue;
        }
        if event.payload["input"]["operation"] != json!("write") {
            continue;
        }
        let path = event.payload["input"]["path"].as_str().expect("path");
        let content = event.payload["input"]["content"].as_str().expect("content");
        state.insert(path.to_owned(), content.to_owned());
    }
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_agents_share_a_file_with_loud_conflict_and_clean_revert() {
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    std::fs::write(gate_root.path().join("shared.txt"), "seed").expect("seed written");
    git_commit_all(gate_root.path());
    // Distinct worktrees per child: the local undo gate must not collide on
    // the git index lock (mirrors supervisor_conflict.rs).
    let root_a = tempfile::tempdir().expect("agent-a worktree");
    let root_b = tempfile::tempdir().expect("agent-b worktree");
    git_commit_all(root_a.path());
    git_commit_all(root_b.path());
    let (_pool_dir, pool) = whiteboard_pool().await;

    // agent-b opens with a DECLARED STALE claim on the shared file: the gate
    // refuses deterministically (-32005 conflict) no matter who arrives
    // first, appending zero rows; b then retries with a fresh unclaimed
    // write, which may itself race a's write (every resolution is asserted
    // below via the event log).
    let script_a = json!([write_turn("par-a", "shared.txt", "from-agent-a"), done_turn()]);
    let script_b = json!([
        write_turn_claimed_stale("par-b-stale", "shared.txt", "hijack", "stale-hash"),
        write_turn("par-b", "shared.txt", "from-agent-b"),
        done_turn(),
    ]);

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut agent_process(root_a.path(), script_a, "agent-a"), "agent-a")
        .expect("spawn agent-a");
    supervisor
        .spawn_agent(&mut agent_process(root_b.path(), script_b, "agent-b"), "agent-b")
        .expect("spawn agent-b");

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        memory: Arc::new(NullMemory),
        project_id: ProjectId("proj-parallel".to_owned()),
        consolidation: None,
    };
    supervisor = supervisor.with_services(services);
    let shutdown = CancellationToken::new();
    // Terminate on OBSERVED SUPERVISOR STATE, not on whiteboard events:
    // `SubtaskCompleted` is published by the child BEFORE it exits, while the
    // supervisor flips its meta to `Completed` only on a later tick when it
    // reaps the child's clean EOF. Cancelling shutdown off the event count
    // raced that transition — the teardown drain's short grace window then
    // left a finished agent spuriously `Running` in the summary. `run_until`
    // returns at the first tick where both agents are terminal, so shutdown
    // can never fire mid-transition; failures surface as `Failed` immediately
    // instead of hanging.
    let summary: RunSummary = tokio::time::timeout(
        Duration::from_secs(30),
        supervisor.run_until(shutdown.clone(), |supervisor| {
            ["agent-a", "agent-b"].iter().all(|agent_id| {
                supervisor.agent(agent_id).is_some_and(|meta| {
                    matches!(meta.state, AgentState::Completed | AgentState::Failed)
                })
            })
        }),
    )
    .await
    .expect("both supervised agents reach a terminal state well within the timeout");
    shutdown.cancel();
    // Every applied row is committed WAL-first inside the gate and a child
    // cannot exit (→ `Completed`) until its last gated reply arrives, so the
    // log is fully settled once both agents are terminal.
    let events = all_events(&pool).await;

    // Both agents complete: a conflict is a RETRIABLE tool error, never a
    // crash or restart.
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    for meta in &summary.agents {
        assert_eq!(meta.state, AgentState::Completed, "{:?} must complete", meta.agent_id);
        assert_eq!(meta.restart_count, 0, "no restarts without crashes");
    }

    // Loud conflict: the stale attempt left ZERO rows - it was surfaced to
    // the agent as an error, never silently dropped nor logged as applied.
    assert!(
        !events.iter().any(|event| event.event_id == "par-b-stale"),
        "a conflicted write appends nothing to the whiteboard"
    );

    // Attribution: every applied row belongs to exactly its registered
    // process, and applied rows follow the total order.
    let applied: Vec<&WhiteboardEvent> =
        events.iter().filter(|e| e.kind == WhiteboardKind::WriteApplied).collect();
    assert!(!applied.is_empty(), "at least one concurrent write must apply");
    for row in &applied {
        let expected_agent = match row.event_id.as_str() {
            "par-a" => "agent-a",
            "par-b" => "agent-b",
            other => panic!("unexpected event id {other}"),
        };
        assert_eq!(row.agent_id, expected_agent, "attribution bound at the process boundary");
    }
    for pair in applied.windows(2) {
        assert!(pair[0].gate_seq < pair[1].gate_seq, "applied rows follow the total order");
    }

    // Disk holds exactly one complete sibling write — never torn, never the
    // seed. Which sibling physically won is deliberately NOT pinned: the
    // gate commits its whiteboard row before executing the tool, so under
    // concurrency the physical write order can invert relative to `gate_seq`
    // order, and either sibling's fresh write can be the loud-conflict
    // victim (zero rows) when the other lands between its base_version stamp
    // and its gate check.
    let disk = std::fs::read_to_string(gate_root.path().join("shared.txt")).expect("shared file");
    let applied_contents: Vec<&str> = applied
        .iter()
        .map(|event| event.payload["input"]["content"].as_str().expect("applied write content"))
        .collect();
    assert!(
        applied_contents.contains(&disk.as_str()),
        "disk holds one complete applied write, got {disk:?}; applied {applied_contents:?}"
    );

    // Full-log replay follows the total order (`gate_seq`): the surviving
    // value is the last applied row's content — the log is the restore/replay
    // source of truth, independent of physical completion order.
    let replay = revert_agent(&events, None);
    assert_eq!(
        replay.get("shared.txt").map(String::as_str),
        applied_contents.last().copied(),
        "full replay resolves to the log's last writer"
    );

    // Per-agent revert (D5): each sibling contributes at most one applied
    // write (single scripted attempt), so excluding one leaves exactly the
    // other's content if that sibling applied, else nothing — whichever way
    // the race resolved. Derived from the log; no winner is hard-coded.
    for (excluded, survivor_event_id, survivor_content) in
        [("agent-b", "par-a", "from-agent-a"), ("agent-a", "par-b", "from-agent-b")]
    {
        let expected = if applied.iter().any(|event| event.event_id == survivor_event_id) {
            Some(survivor_content)
        } else {
            None
        };
        let reverted = revert_agent(&events, Some(excluded));
        assert_eq!(
            reverted.get("shared.txt").map(String::as_str),
            expected,
            "reverting {excluded} leaves exactly the survivor's state"
        );
    }
}
