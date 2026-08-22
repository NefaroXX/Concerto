//! Integration tests for the supervisor process lifecycle (ADR-60 S4a):
//! spawning the mock-agent fixture child, the versioned stdio handshake,
//! error paths (timeout, early exit, double-spawn), and graceful teardown.
//!
//! These must live here (not in `supervisor.rs` unit tests) because
//! `CARGO_BIN_EXE_*` paths are only set for integration tests.

use std::process::Command;
use std::time::Duration;

use concerto_orchestrator::supervisor::{
    AgentState, Supervisor, SupervisorConfig, SupervisorError,
};

/// The mock-agent fixture binary, speaking the ADR-60 D2 handshake.
fn mock_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"))
}

#[test]
fn spawn_handshake_accepted_then_stop() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut mock_agent(), "agent-a")
        .expect("mock agent handshake must be accepted");

    let meta = supervisor.agent("agent-a").expect("agent must be registered");
    assert_eq!(meta.agent_id, "agent-a");
    assert_eq!(meta.state, AgentState::Running, "handshake success => Running");

    supervisor.stop_agent("agent-a").expect("graceful stop must succeed");
    assert!(
        supervisor.agent("agent-a").is_none(),
        "stopped agent must be removed from the registry"
    );
}

#[test]
fn double_spawn_same_id_is_rejected() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor.spawn_agent(&mut mock_agent(), "agent-a").expect("first spawn ok");

    let error = supervisor
        .spawn_agent(&mut mock_agent(), "agent-a")
        .expect_err("second spawn with the same id must fail");
    assert!(matches!(error, SupervisorError::AlreadyRunning(_)));

    supervisor.stop_agent("agent-a").expect("cleanup ok");
}

#[test]
fn handshake_timeout_kills_silent_child() {
    let mut supervisor = Supervisor::new(SupervisorConfig {
        handshake_timeout: Duration::from_millis(300),
        ..SupervisorConfig::default()
    });

    // `sleep` never speaks the protocol: the supervisor must time out, kill
    // the child, and leave nothing registered.
    let error = supervisor
        .spawn_agent(Command::new("sleep").arg("30"), "agent-silent")
        .expect_err("silent child must time out");
    match error {
        SupervisorError::HandshakeTimeout { agent_id, waited } => {
            assert_eq!(agent_id, "agent-silent");
            assert_eq!(waited, Duration::from_millis(300));
        }
        other => panic!("expected HandshakeTimeout, got {other:?}"),
    }
    assert!(supervisor.agent("agent-silent").is_none(), "failed spawn must leave no agent behind");
}

#[test]
fn child_exit_during_handshake_is_reported() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());

    // `true` exits immediately without speaking the protocol. The observed
    // failure mode races: the supervisor's hello write can hit the already
    // exited child's closed stdin (EPIPE → `Io`) or the reader can see the
    // child's EOF (`ChildExited`). Both mean the same thing — the child is
    // gone, the spawn failed, and nothing is registered.
    let error = supervisor
        .spawn_agent(&mut Command::new("true"), "agent-exit")
        .expect_err("early child exit must fail the spawn");
    match error {
        SupervisorError::ChildExited { ref agent_id, .. } if agent_id == "agent-exit" => {}
        SupervisorError::Io(_) => {}
        other => panic!("expected ChildExited or Io, got {other:?}"),
    }
    assert!(supervisor.agent("agent-exit").is_none(), "failed spawn must leave no agent behind");
}

#[test]
fn stop_unknown_agent_is_an_error() {
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    let error = supervisor.stop_agent("nobody").expect_err("stopping an unknown agent must fail");
    assert!(matches!(error, SupervisorError::UnknownAgent(_)));
}
