//! Integration tests for the `LspManager` start-failure and cooldown contract.
//!
//! # Why these do not assert the success path
//! A *successful* start requires a real `rust-analyzer` binary, which CI does
//! not install. The happy-path singleton/caching behaviour is therefore covered
//! deterministically by the unit tests inside `manager.rs`, which can seed the
//! private `CLIENTS` map directly. This file covers only what is observable
//! from outside the crate without a language server: start failures propagate
//! as `Err` (never as a cached, half-initialised client), a recent failure
//! suppresses re-spawning, and the manager stays race-free and panic-free under
//! concurrency and `stop_all`.
//!
//! # Serialisation
//! All tests here share the process-global `LspManager` state. A test-level
//! async mutex serialises every test function that touches it — no new
//! dependencies, no flaky CI.

use concerto_core::CancellationToken;
use concerto_core::ToolError;
use concerto_lsp::LspManager;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// Serialises all tests in this file against the global manager state.
static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// A directory that does not exist, so `Command::spawn` fails fast whether or
/// not a language server is installed.
fn missing_project_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("concerto-lsp-it-missing-{name}"))
}

/// A failed start must surface as an error rather than a cached client.
#[tokio::test]
async fn test_manager_start_failure_propagates() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("start-failure");
    let project_id = LspManager::key_for_dir(&project_dir);

    let result = LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await;

    assert!(result.is_err(), "a missing project dir must fail the start");
}

/// A failed start must not be cached as a usable client: a second call must
/// report a failure rather than hand back a half-initialised client.
#[tokio::test]
async fn test_manager_failed_start_is_not_cached_as_success() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("failed-not-cached");
    let project_id = LspManager::key_for_dir(&project_dir);

    let first =
        LspManager::get_or_start(project_id.clone(), project_dir.clone(), CancellationToken::new())
            .await;
    let second = LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await;

    assert!(first.is_err(), "first start attempt must fail");
    assert!(second.is_err(), "the retry must not receive a cached broken client");
}

/// Inside the cooldown window a second call is rejected without spawning: the
/// error is the synthetic suppression, not the raw spawn failure. This is the
/// retry-storm guard observed from outside the crate.
#[tokio::test]
async fn test_manager_cooldown_suppresses_respawn() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("cooldown");
    let project_id = LspManager::key_for_dir(&project_dir);

    let first =
        LspManager::get_or_start(project_id.clone(), project_dir.clone(), CancellationToken::new())
            .await
            .expect_err("the first start must fail");
    assert!(
        first.to_string().contains("Failed to spawn"),
        "the first attempt must reach the spawn: {first}"
    );

    let second = LspManager::get_or_start(project_id, project_dir, CancellationToken::new())
        .await
        .expect_err("the second start must be suppressed");
    let text = second.to_string();
    // The suppression branch runs before any spawn, so its presence proves the
    // second call never reached `LspClient::start`.
    assert!(
        text.contains("start suppressed"),
        "the second attempt must report suppression: {text}"
    );
}

/// Concurrent first-callers must all observe a failure without racing,
/// panicking, or deadlocking.
#[tokio::test]
async fn test_manager_concurrent_start_failure_is_stable() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("concurrent-failure");
    let project_id = LspManager::key_for_dir(&project_dir);

    let mut join_set = JoinSet::new();
    for _ in 0..20 {
        let project_id = project_id.clone();
        let project_dir = project_dir.clone();
        join_set.spawn(async move {
            LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await
        });
    }

    let mut attempted = 0;
    while let Some(result) = join_set.join_next().await {
        let outcome = result.expect("task must not panic");
        assert!(outcome.is_err(), "every concurrent caller must observe the failure");
        attempted += 1;
    }
    assert_eq!(attempted, 20);
}

/// An already-cancelled token fails the start with `Cancelled` and must not
/// leave a poisoned entry behind.
#[tokio::test]
async fn test_manager_cancelled_start_reports_cancelled() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("cancelled");
    let project_id = LspManager::key_for_dir(&project_dir);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = LspManager::get_or_start(project_id, project_dir, cancel).await;

    assert!(
        matches!(result, Err(ToolError::Cancelled)),
        "a pre-cancelled start must report Cancelled"
    );
}

/// `stop_all` must be safe on an arbitrary manager state and must not panic.
#[tokio::test]
async fn test_manager_stop_all_is_safe() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("stop-all");
    let project_id = LspManager::key_for_dir(&project_dir);

    let _ =
        LspManager::get_or_start(project_id.clone(), project_dir.clone(), CancellationToken::new())
            .await;
    LspManager::stop_all(CancellationToken::new()).await;

    // A subsequent request must still behave predictably (and not panic).
    let after = LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await;
    assert!(after.is_err(), "the missing dir still fails after stop_all");
}

/// Rapid start/stop cycles must not panic or deadlock.
#[tokio::test]
async fn test_manager_rapid_cycles() {
    let _guard = TEST_LOCK.lock().await;
    let project_dir = missing_project_dir("rapid");
    let project_id = LspManager::key_for_dir(&project_dir);

    for _ in 0..5 {
        let _ = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await;
        LspManager::stop_all(CancellationToken::new()).await;
    }

    let result = LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await;
    assert!(result.is_err(), "manager still behaves predictably after rapid cycles");
}

/// The public key function must be deterministic and path-based, so the editor
/// and the tools resolve one client per directory.
#[test]
fn test_manager_key_for_dir_is_path_based() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let first = LspManager::key_for_dir(dir.path());
    assert_eq!(first, LspManager::key_for_dir(dir.path()), "same path must yield the same key");

    let other = tempfile::TempDir::new().expect("temp dir");
    assert_ne!(first, LspManager::key_for_dir(other.path()), "different paths must differ");
}
