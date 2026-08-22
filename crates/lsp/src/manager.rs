use crate::client::LspClient;
use concerto_core::types::ProjectId;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Global manager holding one LspClient per project.
pub struct LspManager;

static CLIENTS: Lazy<Mutex<HashMap<ProjectId, Arc<Mutex<LspClient>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl LspManager {
    /// Get the client for a project, starting it if necessary.
    pub async fn get_or_start(
        project_id: ProjectId,
        project_dir: std::path::PathBuf,
        cancel: CancellationToken,
    ) -> Arc<Mutex<LspClient>> {
        let mut map = CLIENTS.lock().await;
        if let Some(client) = map.get(&project_id) {
            return client.clone();
        }
        // Default to rust-analyzer; could be configurable via config.
        let mut client = LspClient::new(project_dir, "rust-analyzer");
        // Start the server (ignore errors for now, they will surface on use).
        let _ = client.start(cancel.clone()).await;
        let arc = Arc::new(Mutex::new(client));
        map.insert(project_id, arc.clone());
        arc
    }

    /// Stop all clients (used on shutdown).
    pub async fn stop_all(cancel: CancellationToken) {
        let mut map = CLIENTS.lock().await;
        for (_, client) in map.drain() {
            let mut c = client.lock().await;
            let _ = c.stop(cancel.clone()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Serialises tests in this module that access the global `CLIENTS` static.
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    /// `get_or_start` must return an `Arc<Mutex<LspClient>>` that can be locked.
    #[tokio::test]
    async fn test_manager_returns_arc_mutex() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        let project_dir = std::path::PathBuf::from("/tmp/arc_test");
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        let client_arc = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;
        let _locked = client_arc.lock().await; // Must not panic or deadlock.
    }

    /// Calling `get_or_start` with the same project ID must return the same `Arc`
    /// pointer (cached behaviour).
    #[tokio::test]
    async fn test_manager_caches_same_project() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        // Use a unique temp dir to avoid accidental collisions.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_dir = dir.path().to_path_buf();
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        let arc1 =
            LspManager::get_or_start(project_id.clone(), project_dir.clone(), cancel.clone()).await;
        let arc2 = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;

        // Both calls must return the same Arc pointer (caching).
        assert!(
            Arc::as_ptr(&arc1) == Arc::as_ptr(&arc2),
            "get_or_start should cache the client for the same project_id",
        );

        // Keep the temp dir alive until the end of the test.
        let _ = dir;
    }

    /// Calling `get_or_start` when the server binary does not exist should still
    /// return a valid `Arc` — the start error is swallowed.
    #[tokio::test]
    async fn test_manager_get_or_start_nonexistent_binary() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        // Use a binary path that definitely does not exist on any system.
        let project_dir = std::path::PathBuf::from("/tmp/nonexistent_binary");
        // Ensure the directory exists (the LspClient uses the dir as project_dir).
        let _ = std::fs::create_dir_all(&project_dir);
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        // This would fail to spawn the server, but the manager should still return
        // a valid Arc (the error is ignored inside get_or_start).
        let client_arc = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;
        let _locked = client_arc.lock().await; // Must not panic.
    }

    /// `stop_all` on a manager that has no entries must not panic or deadlock.
    #[tokio::test]
    async fn test_manager_empty_stop_all() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();

        // Ensure the static map starts clean for this test by calling stop_all first.
        LspManager::stop_all(cancel.clone()).await;
        // Second stop_all on an empty map should be a no-op.
        LspManager::stop_all(cancel).await;
    }

    /// Calling `get_or_start` with a project dir that does not exist on disk
    /// must still return a valid `Arc` without panicking.
    #[tokio::test]
    async fn test_manager_get_or_start_nonexistent_dir() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();
        // Use a directory that definitely does not exist.
        let project_dir = std::path::PathBuf::from("/tmp/_nonexistent_dir_42a9b1c7");
        // Remove it if it somehow exists.
        let _ = std::fs::remove_dir_all(&project_dir);
        let project_id = concerto_core::types::ProjectId::resolve(&project_dir);

        let client_arc = LspManager::get_or_start(project_id, project_dir, cancel.clone()).await;
        let _locked = client_arc.lock().await; // Must not panic.
    }

    /// Multiple concurrent calls to `get_or_start` for different projects must
    /// each return distinct `Arc` pointers.
    #[tokio::test]
    async fn test_manager_concurrent_get_or_start() {
        let _guard = TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();

        let dirs: Vec<std::path::PathBuf> =
            (0..5).map(|i| std::path::PathBuf::from(format!("/tmp/concurrent_{}", i))).collect();

        let mut handles = Vec::new();
        for dir in dirs.clone() {
            let cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let project_id = concerto_core::types::ProjectId::resolve(&dir);
                LspManager::get_or_start(project_id, dir, cancel).await
            }));
        }

        let mut arcs = Vec::new();
        for h in handles {
            arcs.push(h.await.unwrap());
        }

        // All arcs should be distinct (different projects).
        for i in 0..arcs.len() {
            for j in (i + 1)..arcs.len() {
                assert!(
                    Arc::as_ptr(&arcs[i]) != Arc::as_ptr(&arcs[j]),
                    "project {} and {} must return different Arc pointers",
                    i,
                    j,
                );
            }
        }
    }
}
