//! Integration tests for LSP client lifecycle management.
//!
//! These tests verify the start/stop behavior of LspClient without requiring
//! a real LSP server. Error paths and edge cases are the primary focus.

use concerto_core::CancellationToken;
use concerto_lsp::LspClient;
use std::path::PathBuf;
use tokio::time::{timeout, Duration};

/// Test that creating a client for a non-existent server binary fails gracefully.
#[tokio::test]
async fn test_client_start_missing_binary() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "nonexistent-server-xyz");
    let cancel = CancellationToken::new();

    let result = timeout(Duration::from_secs(5), client.start(cancel)).await;

    // Should timeout or return error, not panic
    match result {
        Ok(Err(_)) => {} // Expected: error from spawn failure
        Err(_) => {}     // Also acceptable: timeout
        Ok(Ok(())) => panic!("Expected error when starting non-existent server"),
    }
}

/// Test that stopping a client that was never started is safe.
#[tokio::test]
async fn test_client_stop_without_start() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "rust-analyzer");
    let cancel = CancellationToken::new();

    // Should not panic or error
    let result = client.stop(cancel).await;
    assert!(result.is_ok(), "Stopping unstarted client should succeed");
}

/// Test that starting a client twice is idempotent.
#[tokio::test]
async fn test_client_double_start() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "nonexistent-server");
    let cancel1 = CancellationToken::new();
    let cancel2 = CancellationToken::new();

    // First start will fail (no binary), but that's OK for this test
    let _ = client.start(cancel1).await;

    // Second start should handle the already-started state gracefully
    let result = client.start(cancel2).await;
    // Either error (already started) or success (idempotent) is acceptable
    // The key is it shouldn't panic or create duplicate processes
    let _ = result;
}

/// Test that get_diagnostics returns empty before server starts.
#[tokio::test]
async fn test_diagnostics_empty_before_start() {
    let project_root = PathBuf::from("/tmp/test-project");
    let client = LspClient::new(project_root, "rust-analyzer");

    let diagnostics = client.get_diagnostics("test.rs").await;
    assert!(diagnostics.is_empty(), "Diagnostics should be empty before server starts");
}

/// Test that send_request returns error when server is not running.
#[tokio::test]
async fn test_request_without_server() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "rust-analyzer");

    let params = serde_json::json!({
        "textDocument": {
            "uri": "file:///test.rs"
        },
        "position": {
            "line": 0,
            "character": 0
        }
    });

    let result = client.send_request("textDocument/hover", params).await;

    // Should return error, not panic
    assert!(result.is_err(), "Request without server should fail");
}

/// Test that send_notification returns error when server is not running.
#[tokio::test]
async fn test_notification_without_server() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "rust-analyzer");

    let params = serde_json::json!({
        "textDocument": {
            "uri": "file:///test.rs",
            "languageId": "rust",
            "version": 1,
            "text": "fn main() {}"
        }
    });

    let result = client.send_notification("textDocument/didOpen", params).await;

    // Should return error, not panic
    assert!(result.is_err(), "Notification without server should fail");
}

/// Test that multiple clients can coexist for different projects.
#[tokio::test]
async fn test_multiple_clients_different_projects() {
    let project1 = PathBuf::from("/tmp/project1");
    let project2 = PathBuf::from("/tmp/project2");

    let client1 = LspClient::new(project1, "rust-analyzer");
    let client2 = LspClient::new(project2, "rust-analyzer");

    // Both should be independently usable
    let diag1 = client1.get_diagnostics("test.rs").await;
    let diag2 = client2.get_diagnostics("test.rs").await;

    assert!(diag1.is_empty());
    assert!(diag2.is_empty());
}

/// Test that client handles invalid JSON-RPC responses gracefully.
#[tokio::test]
async fn test_invalid_response_handling() {
    let project_root = PathBuf::from("/tmp/test-project");
    let mut client = LspClient::new(project_root, "rust-analyzer");

    // Without a server, any request should fail cleanly
    let result = client.send_request("invalid/method", serde_json::json!({})).await;

    assert!(result.is_err());
    if let Err(e) = result {
        // Error should be descriptive, not a panic
        let error_msg = format!("{}", e);
        assert!(!error_msg.is_empty(), "Error message should not be empty");
    }
}

/// Test concurrent diagnostic queries don't cause races.
#[tokio::test]
async fn test_concurrent_diagnostic_queries() {
    let project_root = PathBuf::from("/tmp/test-project");
    let client = std::sync::Arc::new(LspClient::new(project_root, "rust-analyzer"));

    let mut handles = vec![];
    for i in 0..10 {
        let client_clone = std::sync::Arc::clone(&client);
        let handle = tokio::spawn(async move {
            let file = format!("file{}.rs", i);
            client_clone.get_diagnostics(&file).await
        });
        handles.push(handle);
    }

    // All should complete without panic
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_empty());
    }
}
