//! Integration tests for the supervisor steady-state loop (ADR-60 S4b):
//! heartbeat recording, stale/EOF-driven one-for-one restarts, the restart
//! budget, and graceful shutdown of everything at the end of a run.
//!
//! The loop owns the supervisor exclusively while it runs, so assertions
//! happen on the [`RunSummary`] it returns once the shutdown token fires.

use std::process::Command;
use std::time::Duration;

use concerto_core::CancellationToken;
use concerto_orchestrator::supervisor::{
    AgentMeta, AgentState, RunSummary, Supervisor, SupervisorConfig,
};

/// The mock-agent fixture binary, speaking the ADR-60 D2 handshake.
fn mock_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"))
}

/// Config tuned for fast, deterministic loop tests: short heartbeat timeout,
/// tiny restart backoff, and a small restart budget.
fn fast_config() -> SupervisorConfig {
    SupervisorConfig {
        heartbeat_timeout: Duration::from_millis(300),
        restart_backoff: Duration::from_millis(20),
        max_restarts: 2,
        ..SupervisorConfig::default()
    }
}

/// Find `agent_id`'s snapshot in the summary; panics when absent.
fn snapshot<'a>(summary: &'a RunSummary, agent_id: &str) -> &'a AgentMeta {
    summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == agent_id)
        .unwrap_or_else(|| panic!("agent {agent_id} must appear in the shutdown snapshot"))
}

#[tokio::test(flavor = "multi_thread")]
async fn run_loop_records_heartbeats_and_stops_cleanly() {
    // Staleness must not interfere with this test: the wait is
    // condition-based (bounded by the 15s guard below), so set the
    // heartbeat timeout far above it — a healthy-but-slow child must be
    // recorded, not restarted.
    let mut supervisor = Supervisor::new(SupervisorConfig {
        heartbeat_timeout: Duration::from_secs(30),
        ..SupervisorConfig::default()
    });
    supervisor
        .spawn_agent(mock_agent().env("MOCK_AGENT_HEARTBEATS", "2"), "agent-a")
        .expect("spawn");

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        supervisor
            .run_until(task_shutdown, |s| s.agent("agent-a").is_some_and(|meta| meta.seq >= 2))
            .await
    });
    // Condition-wait, not wall-clock: the loop exits on the tick that
    // records the second heartbeat, however long a contended machine takes
    // to deliver it. The timeout only guards against a wedge.
    let result = tokio::time::timeout(Duration::from_secs(15), task).await;
    if result.is_err() {
        shutdown.cancel(); // unblock the loop before unwinding
    }
    let summary = result
        .expect("both heartbeats must be recorded within the deadline")
        .expect("run loop task");
    assert!(summary.failed.is_empty(), "no failures expected");
    let meta = snapshot(&summary, "agent-a");
    assert_eq!(meta.state, AgentState::Running, "healthy agent at shutdown");
    assert_eq!(meta.seq, 2, "both mock heartbeats must have been recorded; meta={meta:?}");
    assert_eq!(meta.restart_count, 0, "no restarts expected");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_loop_stops_healthy_silent_agent_within_timeout() {
    // A healthy agent that just has not heartbeated *yet* must not be
    // presumed stale: the completed handshake is the spawn-time liveness
    // proof.
    let mut supervisor = Supervisor::new(fast_config());
    supervisor.spawn_agent(&mut mock_agent(), "agent-a").expect("spawn");

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { supervisor.run(task_shutdown).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.cancel();

    let summary = task.await.expect("run loop task");
    assert!(summary.failed.is_empty());
    let meta = snapshot(&summary, "agent-a");
    assert_eq!(meta.state, AgentState::Running);
    assert_eq!(meta.restart_count, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn silent_agent_goes_stale_and_burns_restart_budget() {
    // A mock agent that never heartbeats is presumed dead after
    // heartbeat_timeout; each respawn proves liveness once (handshake), then
    // goes stale again, until the budget (2) is exhausted and the agent is
    // marked failed.
    let mut supervisor = Supervisor::new(fast_config());
    supervisor.spawn_agent(&mut mock_agent(), "agent-a").expect("spawn");

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        supervisor
            .run_until(task_shutdown, |s| {
                s.agent("agent-a").is_some_and(|meta| meta.state == AgentState::Failed)
            })
            .await
    });
    // Condition-wait on the terminal Failed state instead of a fixed sleep:
    // the budget burn takes ~3 stale-timeouts regardless of machine load.
    let result = tokio::time::timeout(Duration::from_secs(15), task).await;
    if result.is_err() {
        shutdown.cancel(); // unblock the loop before unwinding
    }
    let summary = result.expect("the budget must burn within the deadline").expect("run loop task");
    assert_eq!(summary.failed, vec!["agent-a"], "budget burn must mark the agent failed");
    let meta = snapshot(&summary, "agent-a");
    assert_eq!(meta.state, AgentState::Failed);
    assert_eq!(meta.restart_count, 2, "both restart attempts must have happened");
    assert!(meta.failed_at_ms.is_some(), "failed_at_ms must be recorded on failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_exit_is_terminal_and_never_restarts() {
    // ADR-60 S5: a one-run-per-process agent that exits 0 after its
    // handshake has *completed its task* — the supervisor must treat that as
    // the terminal `Completed` state and consume none of the restart budget.
    // (The old "clean exit restarts" behavior was the pre-S5 mock-only
    // assumption; the real agent-process child exits 0 on task completion.)
    let mut supervisor = Supervisor::new(fast_config());
    supervisor
        .spawn_agent(mock_agent().env("MOCK_AGENT_EXIT_AFTER", "1"), "agent-a")
        .expect("spawn");

    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        supervisor
            .run_until(task_shutdown, |s| {
                s.agent("agent-a").is_some_and(|meta| meta.state == AgentState::Completed)
            })
            .await
    });
    // Condition-wait on the terminal Completed state instead of a fixed
    // sleep.
    let result = tokio::time::timeout(Duration::from_secs(15), task).await;
    if result.is_err() {
        shutdown.cancel(); // unblock the loop before unwinding
    }
    let summary = result
        .expect("the clean exit must be observed within the deadline")
        .expect("run loop task");
    assert!(summary.failed.is_empty(), "a clean exit is not a failure");
    let meta = snapshot(&summary, "agent-a");
    assert_eq!(meta.state, AgentState::Completed);
    assert_eq!(meta.restart_count, 0, "no restart budget consumed");
}
