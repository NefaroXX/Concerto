//! Integration tests for LSP tool execution paths.
//!
//! These tests verify that tools handle errors gracefully when the LSP server
//! is not available, and that parameter validation works correctly.

use concerto_core::error::PolicyError;
use concerto_core::ids::Ulid;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::traits::tool::Tool;
use concerto_core::types::SessionContext;
use concerto_core::CancellationToken;
use concerto_lsp::tools::*;
use std::path::PathBuf;
use std::sync::Arc;

/// No-op audit log for testing.
struct TestAuditLog;

#[async_trait::async_trait]
impl AuditLog for TestAuditLog {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// Helper to create a test policy engine.
fn test_policy() -> SimplePolicyEngine {
    SimplePolicyEngine::new(vec![], Arc::new(TestAuditLog))
}

/// Helper to create a test session context.
fn test_session() -> SessionContext {
    SessionContext::new(Ulid::new(), PathBuf::from("/tmp/test-project"))
}

/// Test that GetHover returns an error when LSP server is not available.
#[tokio::test]
async fn test_get_hover_no_server() {
    let tool = GetHover;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs",
        "line": 10,
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    // Should return an error, not panic
    assert!(result.is_err(), "GetHover should fail without LSP server");
}

/// Test that GetDiagnostics returns empty when LSP server is not available.
#[tokio::test]
async fn test_get_diagnostics_no_server() {
    let tool = GetDiagnostics;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs"
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    // Should return an error or empty result
    match result {
        Ok(output) => {
            // If it succeeds, diagnostics should be empty
            let diags: Vec<serde_json::Value> =
                serde_json::from_value(output.data.get("diagnostics").cloned().unwrap_or_default())
                    .unwrap_or_default();
            assert!(diags.is_empty(), "Diagnostics should be empty without server");
        }
        Err(_) => {
            // Error is also acceptable when server is not available
        }
    }
}

/// Test that FindReferences returns an error when LSP server is not available.
#[tokio::test]
async fn test_find_references_no_server() {
    let tool = FindReferences;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs",
        "line": 10,
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "FindReferences should fail without LSP server");
}

/// Test that RenameSymbol returns an error when LSP server is not available.
#[tokio::test]
async fn test_rename_symbol_no_server() {
    let tool = RenameSymbol;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs",
        "line": 10,
        "column": 5,
        "new_name": "new_name"
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "RenameSymbol should fail without LSP server");
}

/// Test that GetCodeActions returns an error when LSP server is not available.
#[tokio::test]
async fn test_get_code_actions_no_server() {
    let tool = GetCodeActions;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs",
        "line": 10,
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "GetCodeActions should fail without LSP server");
}

/// Test that ExecuteCodeAction returns an error when LSP server is not available.
#[tokio::test]
async fn test_execute_code_action_no_server() {
    let tool = ExecuteCodeAction;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs",
        "action": "quickfix"
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "ExecuteCodeAction should fail without LSP server");
}

/// Test that GetSemanticTokens returns an error when LSP server is not available.
#[tokio::test]
async fn test_get_semantic_tokens_no_server() {
    let tool = GetSemanticTokens;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs"
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "GetSemanticTokens should fail without LSP server");
}

/// Test that GetInlayHints returns an error when LSP server is not available.
#[tokio::test]
async fn test_get_inlay_hints_no_server() {
    let tool = GetInlayHints;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();
    let input = serde_json::json!({
        "file": "test.rs"
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    assert!(result.is_err(), "GetInlayHints should fail without LSP server");
}

/// Test that tools validate required parameters correctly.
#[tokio::test]
async fn test_tool_parameter_validation() {
    let tool = GetHover;
    let session = test_session();
    let policy = test_policy();
    let cancel = CancellationToken::new();

    // Missing required parameter "file"
    let input = serde_json::json!({
        "line": 10,
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel.clone()).await;
    assert!(result.is_err(), "GetHover should fail with missing 'file' parameter");

    // Missing required parameter "line"
    let input = serde_json::json!({
        "file": "test.rs",
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel).await;
    assert!(result.is_err(), "GetHover should fail with missing 'line' parameter");
}

/// Test that tools handle cancellation gracefully.
#[tokio::test]
async fn test_tool_cancellation() {
    let tool = GetHover;
    let session = test_session();
    let policy = test_policy();

    // Create a cancelled token
    let cancel = CancellationToken::new();
    cancel.cancel();

    let input = serde_json::json!({
        "file": "test.rs",
        "line": 10,
        "column": 5
    });

    let result = tool.execute(input, &policy, &session, cancel).await;

    // Should return an error (cancelled), not panic
    assert!(result.is_err(), "Tool should handle cancellation gracefully");
}

/// Test that all tools have valid metadata.
#[test]
fn test_tool_metadata() {
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(GetHover),
        Box::new(GetDiagnostics),
        Box::new(FindReferences),
        Box::new(RenameSymbol),
        Box::new(GetCodeActions),
        Box::new(ExecuteCodeAction),
        Box::new(GetSemanticTokens),
        Box::new(GetInlayHints),
    ];

    for tool in tools {
        let name = tool.name();
        assert!(!name.is_empty(), "Tool name should not be empty");

        let description = tool.description();
        assert!(!description.is_empty(), "Tool description should not be empty");

        let schema = tool.input_schema();
        assert!(schema.is_object(), "Tool input schema should be a JSON object");
    }
}
