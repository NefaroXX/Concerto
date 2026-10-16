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
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_lsp_client_creation() {
        let _client = LspClient::new("/tmp/test_project", "rust-analyzer");
        // Client creation should succeed without panicking
        // This confirms the constructor works as expected
    }

    #[tokio::test]
    async fn test_lsp_manager_get_or_start_surfaces_start_failure() {
        // Serialise against every other test that touches the global manager map.
        let _guard = crate::manager::MANAGER_TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();

        // A directory that does not exist makes the spawn fail deterministically,
        // with or without a language server installed on the host.
        let project_dir = std::path::PathBuf::from("/nonexistent/concerto-lsp/lib-test-project");
        let project_id = LspManager::key_for_dir(&project_dir);

        let result = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;

        // The failure must propagate instead of yielding a cached broken client.
        assert!(result.is_err(), "a failed start must be reported to the caller");
    }

    #[tokio::test]
    async fn test_lsp_manager_stop_all() {
        // Serialise against every other test that touches the global manager map;
        // `stop_all` drains the whole map, so it must not run concurrently with
        // the cache assertions in `manager::tests`.
        let _guard = crate::manager::MANAGER_TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();

        // Seed an entry so `stop_all` is exercised against a non-empty map.
        let project_dir = std::path::PathBuf::from("/tmp/stop_test");
        let project_id = LspManager::key_for_dir(&project_dir);

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
}
