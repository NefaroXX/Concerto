#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! concerto-lsp – LSP client integration.

pub mod client;
pub mod manager;
pub mod tools;

// Re‑export commonly used items for convenience.
pub use client::LspClient;
pub use manager::LspManager;
pub use tools::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_lsp_client_creation() {
        let _client = LspClient::new("/tmp/test_project", "rust-analyzer");
        // Client creation should succeed without panicking
        // This confirms the constructor works as expected
    }

    #[tokio::test]
    async fn test_lsp_manager_get_or_start() {
        let cancel = CancellationToken::new();

        // Create a test project directory
        let project_dir = std::path::PathBuf::from("/tmp/test_project");
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        // Manager should return a valid Arc<Mutex<LspClient>>
        let client_arc = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;

        // Verify the client is usable (not None and can be locked)
        // This is the public API contract
        let _locked = client_arc.lock().await;
        // If we get here without panicking, the client is valid
    }

    #[tokio::test]
    async fn test_lsp_manager_stop_all() {
        let cancel = CancellationToken::new();

        // Initialize a client to ensure the manager has content
        let project_dir = std::path::PathBuf::from("/tmp/stop_test");
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        let _ = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;

        // Stop all should complete without errors
        LspManager::stop_all(cancel).await;
    }

    #[tokio::test]
    async fn test_lsp_client_send_request() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");

        // When the LSP server hasn't started, send_request should fail
        let result = client.send_request("initialize", serde_json::json!({})).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should error because stdin is not available
        assert!(err.to_string().contains("stdin not available"));
    }

    #[tokio::test]
    async fn test_lsp_client_send_notification() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");

        let result = client.send_notification("initialized", serde_json::json!({})).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stdin not available"));
    }

    #[tokio::test]
    async fn test_lsp_client_get_diagnostics() {
        let client = LspClient::new("/tmp", "rust-analyzer");
        let diagnostics = client.get_diagnostics("/some/file.rs").await;

        // Should return empty diagnostics since server hasn't started
        assert!(diagnostics.is_empty());
    }

    #[tokio::test]
    async fn test_lsp_manager_different_projects() {
        let cancel = CancellationToken::new();

        // Create two different project directories
        let project_dir1 = std::path::PathBuf::from("/tmp/project1");
        let project_dir2 = std::path::PathBuf::from("/tmp/project2");

        let project_id1 = concerto_core::types::ProjectId::resolve(&project_dir1);
        let project_id2 = concerto_core::types::ProjectId::resolve(&project_dir2);

        let client1 = LspManager::get_or_start(project_id1, project_dir1, cancel.clone()).await;
        let client2 = LspManager::get_or_start(project_id2, project_dir2, cancel).await;

        // Should be different Arc references for different projects
        assert!(Arc::as_ptr(&client1) != Arc::as_ptr(&client2));
    }
}
