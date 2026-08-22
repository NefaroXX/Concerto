//! End-to-end tests for the MCP stdio client and the `McpTool` bridge,
//! driven by the `fixture-mcp-server` binary (spawned via
//! `CARGO_BIN_EXE_fixture-mcp-server`).

use concerto_api_types::extension::McpToolDescriptor;
use concerto_core::error::ToolError;
use concerto_core::ids::Ulid;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::traits::tool::Tool;
use concerto_core::types::{CapabilitySet, SessionContext};
use concerto_core::CancellationToken;
use concerto_mcp::client::{McpClient, McpServerInfo};
use concerto_mcp::error::McpError;
use concerto_mcp::tool_bridge::McpTool;
use concerto_mcp::PROTOCOL_VERSION;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

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

async fn spawn_client(env: &[(&str, &str)]) -> McpClient {
    let mut client = McpClient::new("fixture");
    client.spawn(&fixture_bin(), &[], env).await.expect("spawn should succeed");
    client
}

async fn spawn_initialized(env: &[(&str, &str)]) -> McpClient {
    let mut client = spawn_client(env).await;
    let info = client.initialize(5).await.expect("initialize should succeed");
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    client
}

#[tokio::test]
async fn spawn_initialize_list_and_stop_cleanly() {
    let mut client = spawn_initialized(&[]).await;
    assert!(client.connected());
    let info: &McpServerInfo = client.server_info().expect("server info cached");
    assert_eq!(info.name, "fixture-mcp-server");

    let tools =
        client.list_tools(5, CancellationToken::new()).await.expect("list_tools should succeed");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "fail", "slow", "crash"]);
    for tool in &tools {
        assert!(tool.input_schema.is_object(), "{} must have an object inputSchema", tool.name);
    }

    let status = client.stop().await.expect("stop should succeed");
    assert_eq!(status.code(), Some(0));
    assert!(!client.connected());
}

#[tokio::test]
async fn echo_round_trip() {
    let mut client = spawn_initialized(&[]).await;
    let result = client
        .call_tool("echo", json!({ "text": "hello world" }), 5, CancellationToken::new())
        .await
        .expect("echo should succeed");
    assert!(!result.is_error);
    assert_eq!(result.text(), "hello world");
    assert_eq!(result.content.len(), 1);
}

#[tokio::test]
async fn fail_tool_surfaces_is_error() {
    let mut client = spawn_initialized(&[]).await;
    let result = client
        .call_tool("fail", json!({}), 5, CancellationToken::new())
        .await
        .expect("call should succeed at the protocol level");
    assert!(result.is_error);
    assert_eq!(result.text(), "boom");
}

#[tokio::test]
async fn unknown_tool_returns_jsonrpc_error() {
    let mut client = spawn_initialized(&[]).await;
    let err = client
        .call_tool("nope", json!({}), 5, CancellationToken::new())
        .await
        .expect_err("unknown tool must fail");
    assert!(matches!(err, McpError::JsonRpc { code: -32602, .. }));
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let mut client = spawn_initialized(&[]).await;
    let err = client
        .request("bogus_method", json!({}), 5, CancellationToken::new())
        .await
        .expect_err("unknown method must fail");
    assert!(matches!(err, McpError::JsonRpc { code: -32601, .. }));
}

#[tokio::test]
async fn ping_round_trip() {
    let mut client = spawn_initialized(&[]).await;
    let result = client.ping(5, CancellationToken::new()).await.expect("ping should succeed");
    assert_eq!(result, json!({}));
}

#[tokio::test]
async fn slow_tool_times_out_then_recovers() {
    let mut client = spawn_initialized(&[]).await;
    let err = client
        .call_tool("slow", json!({}), 1, CancellationToken::new())
        .await
        .expect_err("slow tool must time out");
    assert!(matches!(err, McpError::Timeout { .. }));

    // The pending slot must have been cleaned up: the next call is answered
    // immediately (the fixture handles each request on its own thread, so the
    // sleeping "slow" call does not block it).
    let result = client
        .call_tool("echo", json!({ "text": "again" }), 5, CancellationToken::new())
        .await
        .expect("echo after timeout should succeed");
    assert_eq!(result.text(), "again");
    let _ = client.stop().await;
}

#[tokio::test]
async fn cancel_token_aborts_in_flight_request() {
    let client = Arc::new(Mutex::new(spawn_initialized(&[]).await));
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let client = client.clone();
        let token = token.clone();
        async move { client.lock().await.call_tool("slow", json!({}), 60, token).await }
    });
    // Give the request a moment to reach the server.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    token.cancel();
    let err = task.await.expect("task should not panic").expect_err("call must be cancelled");
    assert!(matches!(err, McpError::Cancelled));
    // The server is still alive and responsive.
    let result = client
        .lock()
        .await
        .call_tool("echo", json!({ "text": "still here" }), 5, CancellationToken::new())
        .await
        .expect("echo after cancel should succeed");
    assert_eq!(result.text(), "still here");
    let _ = client.lock().await.stop().await;
}

#[tokio::test]
async fn crash_tool_exits_server_and_subsequent_calls_fail() {
    let mut client = spawn_initialized(&[]).await;
    let err = client
        .call_tool("crash", json!({}), 5, CancellationToken::new())
        .await
        .expect_err("crash tool must fail the call");
    match err {
        McpError::ServerExited { status, .. } => assert_eq!(status, Some(1)),
        other => panic!("expected ServerExited, got {other:?}"),
    }
    assert!(!client.connected());
    let err2 = client
        .call_tool("echo", json!({ "text": "x" }), 5, CancellationToken::new())
        .await
        .expect_err("call after crash must fail");
    assert!(matches!(err2, McpError::NotConnected));
    let status = client.stop().await.expect("stop after crash should reap the child");
    assert_eq!(status.code(), Some(1));
}

#[tokio::test]
async fn crash_on_start_surfaces_server_exit() {
    let mut client = spawn_client(&[("FIXTURE_CRASH_ON_START", "1")]).await;
    let err = client.initialize(5).await.expect_err("initialize against a dead server must fail");
    // The write may fail with EPIPE (server already gone) or the reader may
    // observe EOF; either way the connection is dead.
    match err {
        McpError::ServerExited { status, .. } => assert_eq!(status, Some(1)),
        McpError::Io { .. } => {}
        other => panic!("expected ServerExited or Io, got {other:?}"),
    }
    assert!(!client.connected());
    let status = client.stop().await.expect("stop should reap the dead child");
    assert_eq!(status.code(), Some(1));
}

#[tokio::test]
async fn version_mismatch_when_server_reports_unknown_version() {
    let mut client = spawn_client(&[("FIXTURE_VERSION", "2024-11-05")]).await;
    let err = client.initialize(5).await.expect_err("unknown server version must fail");
    match err {
        McpError::VersionMismatch { supported } => {
            assert_eq!(supported, vec![PROTOCOL_VERSION.to_string()]);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    let _ = client.stop().await;
}

#[tokio::test]
async fn version_mismatch_when_server_rejects_initialize() {
    let mut client = spawn_client(&[("FIXTURE_REJECT_INITIALIZE", "1")]).await;
    let err = client.initialize(5).await.expect_err("rejected initialize must fail");
    match err {
        McpError::VersionMismatch { supported } => {
            assert_eq!(supported, vec!["2024-11-05".to_string(), "2025-03-26".to_string()]);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    let _ = client.stop().await;
}

#[tokio::test]
async fn double_spawn_is_rejected() {
    let mut client = McpClient::new("fixture");
    client.spawn(&fixture_bin(), &[], &[]).await.expect("first spawn should succeed");
    let err = client.spawn(&fixture_bin(), &[], &[]).await.expect_err("second spawn must fail");
    assert!(matches!(err, McpError::AlreadySpawned));
    let _ = client.stop().await;
}

#[tokio::test]
async fn mcp_tool_bridge_executes_remote_tool() {
    let client = Arc::new(Mutex::new(spawn_initialized(&[]).await));
    let tool = McpTool::new(
        "fixture".into(),
        client,
        McpToolDescriptor {
            name: "echo".into(),
            description: Some("Echo the text argument back".into()),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
    );
    assert_eq!(tool.name(), "mcp:fixture:echo");
    assert_eq!(tool.capability_requirements(), CapabilitySet::default());

    let policy = SimplePolicyEngine::new(vec![], Arc::new(NoopAudit));
    let session = SessionContext::new(Ulid::new(), PathBuf::from("/tmp/test-project"));
    let output = tool
        .execute(json!({ "text": "hi" }), &policy, &session, CancellationToken::new())
        .await
        .expect("execute should succeed");
    assert_eq!(output.summary, "hi");
}

#[tokio::test]
async fn mcp_tool_bridge_maps_failure_timeout_and_cancel() {
    let client = Arc::new(Mutex::new(spawn_initialized(&[]).await));
    let policy = SimplePolicyEngine::new(vec![], Arc::new(NoopAudit));
    let session = SessionContext::new(Ulid::new(), PathBuf::from("/tmp/test-project"));
    let descriptor = |name: &str| McpToolDescriptor {
        name: name.to_string(),
        description: None,
        input_schema: json!({ "type": "object", "properties": {} }),
    };

    // Server-side tool failure (isError) → ToolError::ExecutionFailed.
    let fail_tool = McpTool::new("fixture".into(), client.clone(), descriptor("fail"));
    let err = fail_tool
        .execute(json!({}), &policy, &session, CancellationToken::new())
        .await
        .expect_err("fail tool must error");
    assert!(matches!(err, ToolError::ExecutionFailed { ref message } if message == "boom"));

    // Timeout (via the input's reserved timeout_secs key) → ToolError::Timeout.
    let slow_tool = McpTool::new("fixture".into(), client.clone(), descriptor("slow"));
    let err = slow_tool
        .execute(json!({ "timeout_secs": 1 }), &policy, &session, CancellationToken::new())
        .await
        .expect_err("slow tool must time out");
    assert!(matches!(err, ToolError::Timeout { timeout_secs: 1 }));

    // Pre-cancelled token → ToolError::Cancelled without touching the server.
    let echo_tool = McpTool::new("fixture".into(), client.clone(), descriptor("echo"));
    let token = CancellationToken::new();
    token.cancel();
    let err = echo_tool
        .execute(json!({ "text": "x" }), &policy, &session, token)
        .await
        .expect_err("pre-cancelled execute must fail");
    assert!(matches!(err, ToolError::Cancelled));

    // Unknown tool through the bridge → ToolError::ExecutionFailed.
    let unknown_tool = McpTool::new("fixture".into(), client.clone(), descriptor("nope"));
    let err = unknown_tool
        .execute(json!({}), &policy, &session, CancellationToken::new())
        .await
        .expect_err("unknown tool must fail");
    assert!(
        matches!(err, ToolError::ExecutionFailed { ref message } if message.contains("json-rpc error -32602"))
    );

    let _ = client.lock().await.stop().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn drop_reaps_child_without_orphan() {
    let pid_file = std::env::temp_dir().join(format!("mcp-fixture-pid-{}", Ulid::new()));
    {
        let mut client = McpClient::new("fixture");
        client
            .spawn(
                &fixture_bin(),
                &[],
                &[("FIXTURE_PID_FILE", pid_file.to_str().expect("utf-8 temp path"))],
            )
            .await
            .expect("spawn should succeed");
        client.initialize(5).await.expect("initialize should succeed");
        assert!(client.connected());
        // Drop without stop(): the Drop impl must kill + reap the child,
        // which would otherwise block on stdin forever.
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid file should exist")
        .trim()
        .parse()
        .expect("pid should parse");
    let proc_dir = format!("/proc/{pid}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::path::Path::new(&proc_dir).exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !std::path::Path::new(&proc_dir).exists(),
        "fixture process {pid} survived its client being dropped (orphan)"
    );
    let _ = std::fs::remove_file(&pid_file);
}
