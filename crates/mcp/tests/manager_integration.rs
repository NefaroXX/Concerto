//! End-to-end tests for the runtime-owned [`McpManager`] (ADR-43 §7, plan §6
//! C3), driven by the `fixture-mcp-server` binary:
//!
//! - namespaced, collision-checked registration into a shared `ToolRegistry`
//! - per-run re-registration (the runtime builds a fresh registry per run)
//! - failure modes: duplicate tool name and crash-on-start mark the server
//!   `Failed` without blocking the remaining servers
//! - crash mid-flight flips the server to `Failed` and publishes
//!   `EventKind::McpServerStateChanged`
//! - `stop_server` / `stop_all` remove tools and stop children
//! - policy gating through the shared `ToolExecutor` (prefix deny/allow)

use concerto_config::{McpConfig, McpServerConfig};
use concerto_core::error::ToolError;
use concerto_core::event::{EventBus, EventKind, EventReceiver};
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::types::{Condition, PolicyRule, SessionContext, ToolRegistry};
use concerto_core::{CancellationToken, McpServerState};
use concerto_mcp::client::McpClient;
use concerto_mcp::tool_bridge::McpTool;
use concerto_mcp::McpManager;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

struct NoopAudit;
#[async_trait::async_trait]
impl AuditLog for NoopAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        Ok(())
    }
}

fn fixture_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-mcp-server").to_string()
}

fn server_config(id: &str, env: &[(&str, &str)]) -> McpServerConfig {
    let env = if env.is_empty() {
        None
    } else {
        Some(env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>())
    };
    McpServerConfig {
        id: id.to_string(),
        command: fixture_bin(),
        args: Vec::new(),
        env,
        enabled: true,
        timeout_secs: Some(5),
    }
}

fn manager_with(servers: Vec<McpServerConfig>) -> (McpManager, EventBus) {
    let bus = EventBus::new(64);
    let manager = McpManager::new(McpConfig { enabled: true, servers }, bus.clone());
    (manager, bus)
}

fn session() -> SessionContext {
    SessionContext::new(Ulid::new(), PathBuf::from("/tmp/test-project"))
}

/// Drain the bus until a `McpServerStateChanged` event for `server_id` in
/// state `wanted` arrives (other events are skipped). Returns the error
/// detail carried by the event, if any.
async fn wait_for_state(
    rx: &mut EventReceiver,
    server_id: &str,
    wanted: McpServerState,
) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv()).await
        {
            if let EventKind::McpServerStateChanged { server_id: sid, state, error } =
                event.kind.clone()
            {
                if sid == server_id && state == wanted {
                    return error;
                }
            }
        }
    }
    panic!("expected McpServerStateChanged({wanted:?}) event for '{server_id}' within 5s");
}

#[tokio::test]
async fn fixture_e2e_registers_namespaced_tools_and_stops() {
    let (manager, _bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut registry = ToolRegistry::default();

    manager.register_tools(&mut registry).await.expect("registration must succeed");

    // Namespaced names are what the LLM sees (the tool log).
    let mut names: Vec<String> =
        registry.all_tool_definitions().into_iter().map(|def| def.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["mcp:fixture:crash", "mcp:fixture:echo", "mcp:fixture:fail", "mcp:fixture:slow"]
    );

    // Live state and per-server tool listing for the Task 7 UI.
    assert_eq!(manager.server_state("fixture"), McpServerState::Connected);
    assert_eq!(manager.servers(), vec![("fixture".to_string(), McpServerState::Connected, 4)]);
    let tools = manager.tools_for("fixture");
    assert_eq!(tools.len(), 4);
    assert!(tools.iter().any(|tool| tool.name == "echo"));
    assert!(tools.iter().all(|tool| tool.description.is_some()));

    // Graceful teardown removes the tools and stops the child.
    manager.stop_all(&mut registry).await;
    assert!(registry.get("mcp:fixture:echo").is_none());
    assert_eq!(manager.server_state("fixture"), McpServerState::Stopped);
    assert_eq!(manager.tools_for("fixture").len(), 0);
}

#[tokio::test]
async fn second_run_re_registers_tools_without_respawning() {
    let (manager, _bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut first = ToolRegistry::default();
    manager.register_tools(&mut first).await.expect("first run must succeed");
    assert_eq!(manager.servers().len(), 1);

    // Second agent run: the runtime builds a fresh registry, the manager
    // re-bridges the connected server's tools without a second spawn.
    let mut second = ToolRegistry::default();
    manager.register_tools(&mut second).await.expect("second run must succeed");
    let mut names: Vec<String> =
        second.all_tool_definitions().into_iter().map(|def| def.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["mcp:fixture:crash", "mcp:fixture:echo", "mcp:fixture:fail", "mcp:fixture:slow"]
    );

    // Still exactly one live server holding the same four tools.
    assert_eq!(manager.servers(), vec![("fixture".to_string(), McpServerState::Connected, 4)]);
    manager.stop_all(&mut second).await;
}

#[tokio::test]
async fn duplicate_tool_collision_marks_server_failed_without_clobbering() {
    let (manager, _bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut registry = ToolRegistry::default();

    // Simulate a plugin tool that already owns mcp:fixture:echo.
    let dummy = McpTool::new(
        "fixture".into(),
        Arc::new(AsyncMutex::new(McpClient::new("fixture"))),
        concerto_api_types::extension::McpToolDescriptor {
            name: "echo".into(),
            description: None,
            input_schema: json!({ "type": "object", "properties": {} }),
        },
    );
    registry.register(Box::new(dummy));

    // The collision must not abort startup: registration continues (Ok), the
    // server is marked Failed, and the pre-existing tool is never clobbered.
    manager.register_tools(&mut registry).await.expect("collision must not abort startup");
    assert!(registry.get("mcp:fixture:echo").is_some(), "pre-existing tool must survive");
    assert_eq!(manager.server_state("fixture"), McpServerState::Failed);
    assert_eq!(manager.tools_for("fixture").len(), 0, "partial tools must be rolled back");

    // The spawned child is still reaped on teardown.
    manager.stop_all(&mut registry).await;
    assert_eq!(manager.server_state("fixture"), McpServerState::Stopped);
}

#[tokio::test]
async fn crash_tool_marks_server_failed_and_publishes_event() {
    let (manager, bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut rx = bus.subscribe();
    let mut registry = ToolRegistry::default();
    manager.register_tools(&mut registry).await.expect("registration must succeed");

    // Invoke the crash tool through the executor, exactly like the agent loop.
    let policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(NoopAudit),
    ));
    let executor = ToolExecutor::new(Arc::new(registry), policy);
    let ctx = session();
    let result =
        executor.execute("mcp:fixture:crash", json!({}), &ctx, CancellationToken::new()).await;
    // The fixture exits(1) mid-request; the bridge surfaces it as an
    // execution failure (the agent loop never panics on MCP failure).
    assert!(matches!(result, Err(ToolError::ExecutionFailed { .. })));

    // The reader observes EOF → Failed, and the manager's watcher publishes
    // the event with the recorded detail.
    let error = wait_for_state(&mut rx, "fixture", McpServerState::Failed).await;
    assert!(error.is_some(), "Failed event must carry the failure detail");
    assert_eq!(manager.server_state("fixture"), McpServerState::Failed);
    assert_eq!(manager.tools_for("fixture").len(), 0);
}

#[tokio::test]
async fn crash_on_start_marks_failed_and_keeps_other_servers() {
    let (manager, _bus) = manager_with(vec![
        server_config("broken", &[("FIXTURE_CRASH_ON_START", "1")]),
        server_config("fixture", &[]),
    ]);
    let mut registry = ToolRegistry::default();

    manager.register_tools(&mut registry).await.expect("registration must continue past failures");

    assert_eq!(manager.server_state("broken"), McpServerState::Failed);
    assert_eq!(manager.server_state("fixture"), McpServerState::Connected);
    assert!(registry.get("mcp:fixture:echo").is_some());
    assert!(registry.get("mcp:broken:echo").is_none());
    manager.stop_all(&mut registry).await;
}

#[tokio::test]
async fn stop_server_removes_tools_and_reconnect_restores_them() {
    let (manager, _bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut registry = ToolRegistry::default();
    manager.register_tools(&mut registry).await.expect("registration must succeed");
    assert!(registry.get("mcp:fixture:echo").is_some());

    manager.stop_server("fixture", &mut registry).await.expect("stop must succeed");
    assert!(registry.get("mcp:fixture:echo").is_none());
    assert_eq!(manager.server_state("fixture"), McpServerState::Stopped);
    assert_eq!(manager.tools_for("fixture").len(), 0);

    // UI reconnect path: a fresh handle replaces the stopped one.
    let count = manager.start_server("fixture", &mut registry).await.expect("reconnect");
    assert_eq!(count, 4);
    assert_eq!(manager.server_state("fixture"), McpServerState::Connected);
    assert!(registry.get("mcp:fixture:echo").is_some());
    manager.stop_all(&mut registry).await;
}

#[tokio::test]
async fn unknown_server_operations_fail_cleanly() {
    let (manager, _bus) = manager_with(Vec::new());
    let mut registry = ToolRegistry::default();
    assert_eq!(manager.server_state("nope"), McpServerState::Disabled);
    assert!(manager.servers().is_empty());
    assert!(manager.tools_for("nope").is_empty());
    let err = manager.stop_server("nope", &mut registry).await.expect_err("unknown server");
    assert!(matches!(
        err,
        concerto_mcp::error::McpError::UnknownServer { ref server_id } if server_id == "nope"
    ));
    let err = manager.start_server("nope", &mut registry).await.expect_err("unknown server");
    assert!(matches!(
        err,
        concerto_mcp::error::McpError::UnknownServer { ref server_id } if server_id == "nope"
    ));
}

#[tokio::test]
async fn policy_deny_blocks_mcp_tool_and_allow_executes() {
    let (manager, _bus) = manager_with(vec![server_config("fixture", &[])]);
    let mut registry = ToolRegistry::default();
    manager.register_tools(&mut registry).await.expect("registration must succeed");
    let registry = Arc::new(registry);
    let ctx = session();

    // Prefix rule (AMEND-A3): deny everything under mcp:.
    let deny_policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoDeny(Condition::ToolNamePrefix("mcp:".into()))],
        Arc::new(NoopAudit),
    ));
    let executor = ToolExecutor::new(registry.clone(), deny_policy);
    let result = executor
        .execute("mcp:fixture:echo", json!({ "text": "x" }), &ctx, CancellationToken::new())
        .await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));

    // Allow rule scoped to one server: the namespaced tool executes end-to-end.
    let allow_policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::ToolNamePrefix("mcp:fixture:".into()))],
        Arc::new(NoopAudit),
    ));
    let executor = ToolExecutor::new(registry.clone(), allow_policy);
    let output = executor
        .execute("mcp:fixture:echo", json!({ "text": "hi" }), &ctx, CancellationToken::new())
        .await
        .expect("allowed mcp tool must execute");
    assert_eq!(output.summary, "hi");

    // A rule for a *different* server does not match this one.
    let other_policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::ToolNamePrefix("mcp:other:".into()))],
        Arc::new(NoopAudit),
    ));
    let executor = ToolExecutor::new(registry.clone(), other_policy);
    let result = executor
        .execute("mcp:fixture:echo", json!({ "text": "x" }), &ctx, CancellationToken::new())
        .await;
    assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
}
