//! Integration tests for the supervisor restart path (ADR-60 S4a): respawn
//! from a snapshotted spec, restart-count bookkeeping, and the one_for_one
//! cap.

use std::process::Command;

use concerto_orchestrator::supervisor::{
    AgentState, Supervisor, SupervisorConfig, SupervisorError,
};

/// The mock-agent fixture binary, speaking the ADR-60 D2 handshake.
fn mock_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"))
}

#[tokio::test]
async fn restart_replaces_child_and_counts() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor.spawn_agent(&mut mock_agent(), "agent-a").expect("first spawn ok");

    supervisor.restart_agent("agent-a").await.expect("restart must succeed");
    let meta = supervisor.agent("agent-a").expect("agent still registered");
    assert_eq!(meta.state, AgentState::Running);
    assert_eq!(meta.restart_count, 1, "one restart consumed");

    supervisor.restart_agent("agent-a").await.expect("second restart must succeed");
    let meta = supervisor.agent("agent-a").expect("agent still registered");
    assert_eq!(meta.restart_count, 2, "two restarts consumed");

    supervisor.stop_agent("agent-a").expect("cleanup ok");
}

#[tokio::test]
async fn restart_exhausts_cap_and_refuses() {
    let mut supervisor =
        Supervisor::new(SupervisorConfig { max_restarts: 1, ..SupervisorConfig::default() });
    supervisor.spawn_agent(&mut mock_agent(), "agent-a").expect("first spawn ok");

    supervisor.restart_agent("agent-a").await.expect("first restart (count 0 -> 1) ok");

    let error = supervisor.restart_agent("agent-a").await.expect_err("cap reached");
    assert!(
        matches!(
            error,
            SupervisorError::RestartsExhausted { ref agent_id, restart_count: 1 } if agent_id == "agent-a"
        ),
        "expected RestartsExhausted with count 1, got {error:?}"
    );
    // The agent stays registered in its current state for the caller to
    // mark failed; verify we can still stop it cleanly.
    supervisor.stop_agent("agent-a").expect("cleanup ok");
}

#[tokio::test]
async fn restart_unknown_agent_is_an_error() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    let error = supervisor.restart_agent("nobody").await.expect_err("unknown id");
    assert!(matches!(error, SupervisorError::UnknownAgent(_)));
}

#[tokio::test]
async fn respawn_uses_snapshotted_arguments() {
    // Give the child an argument and an exit-after-handshake trigger: the
    // respawned child must still handshake (proving program + args + env
    // flowed into the rebuilt command).
    let mut command = mock_agent();
    command.arg("--fixture-arg").env("MOCK_AGENT_EXIT_AFTER", "1");

    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor.spawn_agent(&mut command, "agent-a").expect("first spawn ok");

    // The first child exits on its own right after the handshake (its stdin
    // stays open but it chose to leave); restarting replaces it regardless.
    supervisor.restart_agent("agent-a").await.expect("restart must respawn from snapshot");
    let meta = supervisor.agent("agent-a").expect("agent still registered");
    assert_eq!(meta.state, AgentState::Running);
    assert_eq!(meta.restart_count, 1);

    supervisor.stop_agent("agent-a").expect("cleanup ok");
}
