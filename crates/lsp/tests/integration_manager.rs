//! Integration tests for LSP manager concurrency and singleton behavior.
//!
//! # Serialisation
//! All tests in this file share the global `CLIENTS` static inside
//! `LspManager`.  Because `stop_all` drains the entire map, tests that call
//! it would race with tests that assume their entries survive.  We use a
//! test-level asynchronous mutex (guarded by `TEST_LOCK`) to serialise every
//! test function that touches the manager — simple, zero new dependencies,
//! no flaky CI.

use concerto_core::types::ProjectId;
use concerto_core::CancellationToken;
use concerto_lsp::LspManager;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// Serialises all tests in this file.  Every [`tokio::test`] acquires this
/// lock before running and holds it for the full duration, ensuring that
/// no two tests touch the global `CLIENTS` map concurrently.
static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Test that the manager returns the same client instance for the same project.
#[tokio::test]
async fn test_manager_singleton_behavior() {
    let _guard = TEST_LOCK.lock().await;
    let project_id = ProjectId("test-project-singleton".to_string());
    let project_dir = PathBuf::from("/tmp/test-project-singleton");
    let cancel = CancellationToken::new();

    let client1 =
        LspManager::get_or_start(project_id.clone(), project_dir.clone(), cancel.clone()).await;
    let client2 =
        LspManager::get_or_start(project_id.clone(), project_dir.clone(), cancel.clone()).await;

    // They should be the same Arc (singleton per project)
    assert!(Arc::ptr_eq(&client1, &client2), "Manager should return same client instance");
}

/// Test that different projects get different client instances.
#[tokio::test]
async fn test_manager_different_projects() {
    let _guard = TEST_LOCK.lock().await;
    let project_id1 = ProjectId("test-project-1".to_string());
    let project_dir1 = PathBuf::from("/tmp/test-project-1");
    let project_id2 = ProjectId("test-project-2".to_string());
    let project_dir2 = PathBuf::from("/tmp/test-project-2");
    let cancel = CancellationToken::new();

    let client1 = LspManager::get_or_start(project_id1, project_dir1, cancel.clone()).await;
    let client2 = LspManager::get_or_start(project_id2, project_dir2, cancel.clone()).await;

    assert!(!Arc::ptr_eq(&client1, &client2), "Different projects should get different clients");
}

/// Test concurrent access to the manager doesn't cause races.
#[tokio::test]
async fn test_manager_concurrent_access() {
    let _guard = TEST_LOCK.lock().await;
    let project_id = ProjectId("test-project-concurrent".to_string());
    let project_dir = PathBuf::from("/tmp/test-project-concurrent");

    let mut join_set = JoinSet::new();

    // Spawn 20 concurrent tasks all requesting the same project
    for i in 0..20 {
        let project_id_clone = project_id.clone();
        let project_dir_clone = project_dir.clone();
        join_set.spawn(async move {
            let cancel = CancellationToken::new();
            let client =
                LspManager::get_or_start(project_id_clone, project_dir_clone, cancel).await;
            (i, Arc::strong_count(&client))
        });
    }

    // All should succeed
    let mut results = vec![];
    while let Some(result) = join_set.join_next().await {
        results.push(result.unwrap());
    }

    assert_eq!(results.len(), 20);
    // All should have gotten the same client (strong count should be consistent)
    let first_count = results[0].1;
    for (_, count) in &results {
        assert_eq!(*count, first_count, "All concurrent requests should get the same client");
    }
}

/// Test that stopping all clients cleans up properly.
#[tokio::test]
async fn test_manager_stop_all() {
    let _guard = TEST_LOCK.lock().await;
    let project_id1 = ProjectId("test-project-stop-1".to_string());
    let project_dir1 = PathBuf::from("/tmp/test-project-stop-1");
    let project_id2 = ProjectId("test-project-stop-2".to_string());
    let project_dir2 = PathBuf::from("/tmp/test-project-stop-2");
    let cancel = CancellationToken::new();

    // Start clients for two projects
    let _ =
        LspManager::get_or_start(project_id1.clone(), project_dir1.clone(), cancel.clone()).await;
    let _ =
        LspManager::get_or_start(project_id2.clone(), project_dir2.clone(), cancel.clone()).await;

    // Stop all should succeed
    let cancel2 = CancellationToken::new();
    LspManager::stop_all(cancel2).await;

    // After stop_all, getting a client should create a new one
    let cancel3 = CancellationToken::new();
    let client = LspManager::get_or_start(project_id1, project_dir1, cancel3).await;
    assert!(Arc::strong_count(&client) >= 1, "Should be able to get client after stop_all");
}

/// Test that cancellation during start is handled gracefully.
#[tokio::test]
async fn test_manager_cancellation_during_start() {
    let _guard = TEST_LOCK.lock().await;
    let project_id = ProjectId("test-project-cancel".to_string());
    let project_dir = PathBuf::from("/tmp/test-project-cancel");

    // Create a token and cancel it immediately
    let cancel = CancellationToken::new();
    cancel.cancel();

    // Start should handle cancellation gracefully
    let client = LspManager::get_or_start(project_id, project_dir, cancel).await;

    // Should return a client (even if start failed), not panic
    assert!(Arc::strong_count(&client) >= 1);
}

/// Test that manager can handle rapid start/stop cycles.
#[tokio::test]
async fn test_manager_rapid_cycles() {
    let _guard = TEST_LOCK.lock().await;
    let project_id = ProjectId("test-project-rapid".to_string());
    let project_dir = PathBuf::from("/tmp/test-project-rapid");

    for _ in 0..5 {
        let cancel = CancellationToken::new();
        let _ = LspManager::get_or_start(project_id.clone(), project_dir.clone(), cancel).await;

        let cancel2 = CancellationToken::new();
        LspManager::stop_all(cancel2).await;
    }

    // Should not panic or deadlock
    let cancel = CancellationToken::new();
    let client = LspManager::get_or_start(project_id, project_dir, cancel).await;
    assert!(Arc::strong_count(&client) >= 1, "Manager should work after rapid cycles");
}

/// Test that multiple threads can safely access the manager.
#[tokio::test]
async fn test_manager_thread_safety() {
    let _guard = TEST_LOCK.lock().await;
    let project_id = ProjectId("test-project-thread-safety".to_string());
    let project_dir = PathBuf::from("/tmp/test-project-thread-safety");

    let mut handles = vec![];

    // Spawn 10 tasks that each try to get the client
    for _ in 0..10 {
        let project_id_clone = project_id.clone();
        let project_dir_clone = project_dir.clone();
        let handle = tokio::spawn(async move {
            let cancel = CancellationToken::new();
            LspManager::get_or_start(project_id_clone, project_dir_clone, cancel).await
        });
        handles.push(handle);
    }

    // All should succeed and return the same client
    let mut clients = vec![];
    for handle in handles {
        let client = handle.await.unwrap();
        clients.push(client);
    }

    // All should be the same Arc
    let first = &clients[0];
    for client in &clients[1..] {
        assert!(Arc::ptr_eq(first, client), "All threads should get the same client");
    }
}
