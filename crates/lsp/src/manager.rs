use crate::client::LspClient;
use concerto_core::error::ToolError;
use concerto_core::helpers::canonical_project_path;
use concerto_core::types::ProjectId;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long a **failed** start is remembered per project before another spawn
/// is attempted.
///
/// Without this, a language server that is missing, badly configured, or times
/// out during initialisation is re-spawned on every keystroke (each edit fires a
/// `didChange`). The cooldown turns that retry storm into at most one spawn per
/// window while still allowing the server to recover promptly once it is fixed.
/// A deliberate cancellation is *not* recorded, so aborting a start does not
/// lock the project out.
const START_FAILURE_COOLDOWN: Duration = Duration::from_secs(20);

/// Global manager holding one LspClient per project.
pub struct LspManager;

/// The last recorded start failure for a project, with the time it happened.
#[derive(Debug, Clone)]
struct StartFailure {
    at: Instant,
    message: String,
}

static CLIENTS: Lazy<Mutex<HashMap<ProjectId, Arc<Mutex<LspClient>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Shutdown generation, bumped once per [`LspManager::stop_all`].
///
/// The global [`CLIENTS`] lock is released before `LspClient::start` (the
/// handshake can block for seconds), so a start that is in flight when
/// `stop_all` drains the map would otherwise insert its freshly spawned server
/// *after* the drain — a running server no stop will ever reach. To close that
/// window, `stop_all` increments this counter under the [`CLIENTS`] lock, a
/// start captures it in the same critical section, and the commit re-checks it
/// under the lock: a mismatch means a stop interleaved with the spawn, so the
/// server is killed instead of inserted. A start that captures the *new* value
/// is a genuine post-shutdown start and is allowed to commit.
///
/// Both the increment ([`LspManager::stop_all`]) and the read/compare (the
/// commit in `get_or_start_with`) happen under the [`CLIENTS`] lock, so a
/// captured value is totally ordered against a concurrent drain: either the
/// drain already finished (fresh start) or the post-start check catches it.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Per-project single-flight locks.
///
/// The global [`CLIENTS`] lock is **never** held across `LspClient::start`
/// (which can block for seconds on the `initialize` handshake). Instead, each
/// project gets its own async mutex: concurrent callers for the *same* project
/// serialise onto one spawn, while callers for *different* projects proceed in
/// parallel. Entries are keyed by project and bounded by the number of distinct
/// projects touched in a session.
static START_LOCKS: Lazy<Mutex<HashMap<ProjectId, Arc<Mutex<()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Recent start failures, consulted before spawning so a down server is not
/// re-spawned on every edit. Cleared on a successful start (or cache hit).
static START_FAILURES: Lazy<Mutex<HashMap<ProjectId, StartFailure>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Serialises every unit test that touches the process-global [`CLIENTS`] map.
///
/// `LspManager` is a process-wide singleton, so tests in *any* module that call
/// [`LspManager::get_or_start`] or [`LspManager::stop_all`] must hold this lock.
/// Without a crate-wide lock, a concurrent `stop_all` (for example from
/// `lib.rs`'s `test_lsp_manager_stop_all`, which does not otherwise coordinate
/// with this module) can drain the map between the two `get_or_start` calls in
/// `test_manager_caches_same_project`, making that cache assertion flaky.
#[cfg(test)]
pub(crate) static MANAGER_TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Fetch (creating on first use) the per-project start mutex.
async fn project_start_lock(project_id: &ProjectId) -> Arc<Mutex<()>> {
    let mut locks = START_LOCKS.lock().await;
    locks.entry(project_id.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

/// `Some(error)` when `project_id` is still inside its failure cooldown.
///
/// Expired entries are dropped here so the failure map does not retain dead
/// projects. The returned error is cheap and deterministic: it never touches
/// the filesystem or spawns a process.
async fn cooldown_error(project_id: &ProjectId) -> Option<ToolError> {
    let mut failures = START_FAILURES.lock().await;
    let (at, message) =
        failures.get(project_id).map(|failure| (failure.at, failure.message.clone()))?;
    let elapsed = at.elapsed();
    if elapsed >= START_FAILURE_COOLDOWN {
        failures.remove(project_id);
        return None;
    }
    Some(ToolError::LspError {
        message: format!(
            "LSP server start suppressed for {}s after a recent failure: {}",
            (START_FAILURE_COOLDOWN - elapsed).as_secs().max(1),
            message
        ),
    })
}

/// Remember a start failure so the next caller is suppressed for the cooldown.
async fn record_start_failure(project_id: &ProjectId, error: &ToolError) {
    let mut failures = START_FAILURES.lock().await;
    failures.insert(
        project_id.clone(),
        StartFailure { at: Instant::now(), message: error.to_string() },
    );
}

/// Forget any recorded start failure (a successful start or a cache hit).
async fn clear_start_failure(project_id: &ProjectId) {
    START_FAILURES.lock().await.remove(project_id);
}

impl LspManager {
    /// Path-based identity for the LSP manager key.
    ///
    /// Unlike [`ProjectId::resolve`] (which hashes the git remote so a project
    /// keeps one identity across clones and moves) an LSP server is bound to its
    /// *working directory*: two checkouts of the same remote are two different
    /// servers with two different `cwd`s. The manager therefore keys on the
    /// canonical project path. Session/memory scope keeps using
    /// [`ProjectId::resolve`]; only the LSP manager key changes.
    ///
    /// Mirrors `ProjectIdHelper::from_dir` for existing directories
    /// (`blake3(canonical path)`); a non-existent path falls back to an absolute
    /// spelling so the key stays deterministic for diagnostics and tests.
    pub fn key_for_dir(dir: &Path) -> ProjectId {
        let canonical = canonical_project_path(dir);
        ProjectId(blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex().to_string())
    }

    /// Get the client for a project, starting it if necessary.
    ///
    /// A **successful** start is cached; a **failed** start is not. The
    /// half-initialised client is dropped (its `kill_on_drop` child is reaped)
    /// and the error returned, so the next caller retries instead of replaying
    /// `stdin not available` for the rest of the session. A failed start is
    /// followed by a short per-project cooldown (`START_FAILURE_COOLDOWN`) so a
    /// keystroke burst against a down server does not re-spawn it every time.
    ///
    /// Concurrency: the global [`CLIENTS`] lock is released before
    /// `LspClient::start` and the start is single-flighted per project via
    /// [`START_LOCKS`], so concurrent callers for the same project share one
    /// spawn while different projects never block each other on a slow start.
    /// A start that overlaps [`Self::stop_all`] is discarded (never inserted)
    /// and its server killed, so shutdown cannot orphan a freshly spawned
    /// server — see [`EPOCH`].
    pub async fn get_or_start(
        project_id: ProjectId,
        project_dir: std::path::PathBuf,
        cancel: CancellationToken,
    ) -> Result<Arc<Mutex<LspClient>>, ToolError> {
        Self::get_or_start_with(project_id, cancel, move |cancel| async move {
            // Default to rust-analyzer; could be configurable via config.
            let mut client = LspClient::new(project_dir, "rust-analyzer");
            client.start(cancel).await?;
            Ok(client)
        })
        .await
    }

    /// [`Self::get_or_start`] with the spawn/handshake step injected.
    ///
    /// Production passes a closure that creates and starts a real
    /// `rust-analyzer`. Splitting it out lets tests drive the concurrency
    /// protocol (single-flight, epoch check, commit-or-kill) deterministically
    /// with a controllable "start" — no language server required. All lock and
    /// epoch logic lives here so it is what production actually runs.
    async fn get_or_start_with<F, Fut>(
        project_id: ProjectId,
        cancel: CancellationToken,
        start: F,
    ) -> Result<Arc<Mutex<LspClient>>, ToolError>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: std::future::Future<Output = Result<LspClient, ToolError>>,
    {
        // Fast path: an already-started client wins over any recorded failure.
        let cached = { CLIENTS.lock().await.get(&project_id).cloned() };
        if let Some(client) = cached {
            clear_start_failure(&project_id).await;
            return Ok(client);
        }
        // Fast path: a recent failure suppresses the spawn entirely.
        if let Some(error) = cooldown_error(&project_id).await {
            return Err(error);
        }

        // Single-flight per project. Only the *same* project serialises here;
        // the global client map is not held across the start.
        let start_lock = project_start_lock(&project_id).await;
        let _start_guard = start_lock.lock().await;

        // Double-check under the per-project lock: another caller may have
        // completed the start (or recorded a failure) while we waited. The
        // epoch is read in this same critical section, so its value is ordered
        // against a concurrent `stop_all`'s increment-and-drain.
        let (cached, epoch) = {
            let map = CLIENTS.lock().await;
            (map.get(&project_id).cloned(), EPOCH.load(Ordering::SeqCst))
        };
        if let Some(client) = cached {
            clear_start_failure(&project_id).await;
            return Ok(client);
        }
        if let Some(error) = cooldown_error(&project_id).await {
            return Err(error);
        }

        match start(cancel).await {
            Ok(mut client) => {
                // Commit under the global lock. If `stop_all` drained the map
                // while the spawn was in flight, the epoch moved: this server
                // is not covered by that stop, so kill it instead of inserting
                // it (which would leave a running server no future stop owns).
                let mut map = CLIENTS.lock().await;
                if EPOCH.load(Ordering::SeqCst) != epoch {
                    drop(map);
                    // Report the abort as cancellation: it is not a server
                    // failure, so it records no cooldown and may be retried.
                    // Use a fresh token so a cancelled caller token cannot skip
                    // the kill and strand the child.
                    let _ = client.stop(CancellationToken::new()).await;
                    return Err(ToolError::Cancelled);
                }
                let arc = Arc::new(Mutex::new(client));
                map.insert(project_id.clone(), arc.clone());
                drop(map);
                clear_start_failure(&project_id).await;
                Ok(arc)
            }
            // A deliberate cancellation is not a server failure: report it but
            // do not cool the project down, so an abort can be retried at once.
            Err(ToolError::Cancelled) => Err(ToolError::Cancelled),
            Err(error) => {
                tracing::warn!(%error, project = %project_id, "LSP server failed to start");
                // Cool the project down only if no `stop_all` raced this
                // attempt; otherwise the cooldown would outlive the shutdown
                // that deliberately cleared it.
                if EPOCH.load(Ordering::SeqCst) == epoch {
                    record_start_failure(&project_id, &error).await;
                }
                Err(error)
            }
        }
    }

    /// Stop all clients (used on shutdown).
    ///
    /// The global map is drained under the lock but each server is stopped
    /// *outside* it, so stopping one slow server never blocks starts for other
    /// projects. The shutdown epoch is bumped in the same critical section as
    /// the drain, so a start that is mid-handshake notices and kills its server
    /// rather than committing it after the drain (see [`EPOCH`]). Failure state
    /// is cleared: after a shutdown, the next request may spawn immediately
    /// instead of waiting out a stale cooldown.
    pub async fn stop_all(cancel: CancellationToken) {
        let clients: Vec<Arc<Mutex<LspClient>>> = {
            let mut map = CLIENTS.lock().await;
            EPOCH.fetch_add(1, Ordering::SeqCst);
            map.drain().map(|(_, client)| client).collect()
        };
        START_FAILURES.lock().await.clear();
        for client in clients {
            let mut c = client.lock().await;
            let _ = c.stop(cancel.clone()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Tests here acquire the crate-wide `MANAGER_TEST_LOCK` defined in the
    // parent module; `lib.rs` and integration tests that touch the manager
    // take the same lock.

    /// Insert a not-yet-started client directly so the cache-hit path can be
    /// exercised without a live language server (none is guaranteed in CI).
    async fn seed(project_id: &ProjectId, dir: &std::path::Path) {
        let client = LspClient::new(dir, "rust-analyzer");
        CLIENTS.lock().await.insert(project_id.clone(), Arc::new(Mutex::new(client)));
    }

    /// A path that does not exist makes `Command::spawn` fail immediately,
    /// independent of whether a language server is installed — the
    /// deterministic fixture for the failed-start contract.
    fn missing_project_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("concerto-lsp-manager-missing-dir");
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // ------------------------------------------------------------------
    // Path-based manager key
    // ------------------------------------------------------------------

    /// The same path must always map to the same key, and different paths to
    /// different keys.
    #[test]
    fn key_for_dir_is_stable_and_path_based() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let first = LspManager::key_for_dir(dir.path());
        let second = LspManager::key_for_dir(dir.path());
        assert_eq!(first, second, "the same path must map to the same key");

        let other = tempfile::TempDir::new().expect("temp dir");
        assert_ne!(first, LspManager::key_for_dir(other.path()), "different paths must differ");
    }

    /// Pins the contract to `blake3(canonical path)` so the desktop, tools and
    /// orchestrator paths cannot silently drift onto a different scheme and
    /// spawn duplicate servers for one directory.
    #[test]
    fn key_for_dir_matches_blake3_of_canonical_path() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let expected =
            ProjectId(blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex().to_string());
        assert_eq!(LspManager::key_for_dir(dir.path()), expected);
    }

    /// A path that does not exist still yields a deterministic key (canonical
    /// fallback), so the failure/cooldown bookkeeping stays coherent.
    #[test]
    fn key_for_dir_is_deterministic_for_missing_paths() {
        let missing = std::env::temp_dir().join("concerto-lsp-key-missing-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert_eq!(LspManager::key_for_dir(&missing), LspManager::key_for_dir(&missing));
    }

    // ------------------------------------------------------------------
    // Cache hit
    // ------------------------------------------------------------------

    /// A cached client is returned untouched: `get_or_start` must not
    /// start (or re-start) a client already in the map.
    #[tokio::test]
    async fn test_manager_returns_cached_client_without_starting() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("cache-hit-project".into());
        seed(&project_id, dir.path()).await;

        let client = LspManager::get_or_start(
            project_id,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .await
        .expect("a cached client must be returned");
        let _locked = client.lock().await; // Must not panic or deadlock.
        let _ = dir;
    }

    /// A cached client wins over a recorded failure, and clears it: once the
    /// server is up, a stale cooldown must not suppress it.
    #[tokio::test(start_paused = true)]
    async fn test_manager_cache_hit_clears_failure_cooldown() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("clear-on-success".into());
        record_start_failure(&project_id, &ToolError::LspError { message: "boom".into() }).await;
        assert!(START_FAILURES.lock().await.contains_key(&project_id));
        seed(&project_id, dir.path()).await;

        let client = LspManager::get_or_start(
            project_id.clone(),
            dir.path().to_path_buf(),
            CancellationToken::new(),
        )
        .await
        .expect("a cached client must be returned");
        let _ = client;
        assert!(
            !START_FAILURES.lock().await.contains_key(&project_id),
            "a successful start must clear the failure cooldown"
        );
        let _ = dir;
    }

    // ------------------------------------------------------------------
    // Failure propagation and cooldown
    // ------------------------------------------------------------------

    /// A failed start must propagate and must not be cached, so a later call
    /// can retry instead of receiving a broken client.
    #[tokio::test]
    async fn test_manager_failed_start_is_not_cached() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let project_dir = missing_project_dir();
        let project_id = ProjectId("failed-start-project".into());

        let first = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(first.is_err(), "a failed start must surface as an error");
        assert!(
            !CLIENTS.lock().await.contains_key(&project_id),
            "a failed start must not be cached"
        );

        let second =
            LspManager::get_or_start(project_id.clone(), project_dir, CancellationToken::new())
                .await;
        assert!(second.is_err(), "the retry must also surface the failure");
        assert!(!CLIENTS.lock().await.contains_key(&project_id));
    }

    /// Inside the cooldown the second call must be rejected *without* spawning
    /// again: the error is the synthetic suppression, not the raw spawn error.
    #[tokio::test(start_paused = true)]
    async fn test_manager_cooldown_suppresses_respawn_within_window() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let project_dir = missing_project_dir();
        let project_id = ProjectId("cooldown-within".into());

        let first = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await
        .expect_err("the first start must fail");
        assert!(
            first.to_string().contains("Failed to spawn"),
            "the first attempt must reach the spawn: {first}"
        );

        let second = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await
        .expect_err("the second start must be suppressed");
        let text = second.to_string();
        // The suppression branch runs before any spawn, so its presence proves
        // the second call never reached `LspClient::start`.
        assert!(
            text.contains("start suppressed"),
            "the second attempt must report suppression: {text}"
        );
        assert!(!CLIENTS.lock().await.contains_key(&project_id), "nothing may be cached");
    }

    /// After the cooldown elapses the next call attempts a real spawn again.
    #[tokio::test(start_paused = true)]
    async fn test_manager_cooldown_expires_and_allows_retry() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let project_dir = missing_project_dir();
        let project_id = ProjectId("cooldown-expiry".into());

        let _ = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(START_FAILURES.lock().await.contains_key(&project_id));

        tokio::time::advance(START_FAILURE_COOLDOWN + Duration::from_secs(1)).await;

        let retry = LspManager::get_or_start(
            project_id.clone(),
            project_dir.clone(),
            CancellationToken::new(),
        )
        .await
        .expect_err("the retry must attempt and fail again");
        let text = retry.to_string();
        assert!(
            text.contains("Failed to spawn"),
            "after the cooldown a real spawn must be attempted: {text}"
        );
        assert!(!text.contains("suppressed"), "the cooldown must have expired: {text}");
    }

    /// An already-cancelled token fails the start before any process is
    /// spawned, and that failure is not cached. Cancellation is not a server
    /// failure, so it records no cooldown.
    #[tokio::test]
    async fn test_manager_cancelled_start_propagates_and_is_not_cached() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("cancelled-start-project".into());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result =
            LspManager::get_or_start(project_id.clone(), dir.path().to_path_buf(), cancel).await;
        assert!(matches!(result, Err(ToolError::Cancelled)));
        assert!(!CLIENTS.lock().await.contains_key(&project_id));
        assert!(
            !START_FAILURES.lock().await.contains_key(&project_id),
            "a cancellation must not record a failure cooldown"
        );
        let _ = dir;
    }

    // ------------------------------------------------------------------
    // Concurrency
    // ------------------------------------------------------------------

    /// Concurrent cache hits all receive the same `Arc`, without racing to
    /// start one client per caller.
    #[tokio::test]
    async fn test_manager_concurrent_cache_hits_share_one_client() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("concurrent-cache-hit".into());
        seed(&project_id, dir.path()).await;

        let mut handles = Vec::new();
        for _ in 0..20 {
            let project_id = project_id.clone();
            let path = dir.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                LspManager::get_or_start(project_id, path, CancellationToken::new()).await
            }));
        }

        let mut first: Option<Arc<Mutex<LspClient>>> = None;
        for handle in handles {
            let client = handle.await.expect("task").expect("cached client");
            match &first {
                Some(previous) => {
                    assert!(Arc::ptr_eq(previous, &client), "all callers must share one client")
                }
                None => first = Some(client),
            }
        }
        let _ = dir;
    }

    /// Concurrent first-callers for the *same* project must single-flight the
    /// spawn: exactly one reaches `LspClient::start`, the rest are suppressed by
    /// the cooldown recorded by the winner. A missing dir makes every attempt
    /// fail observably, so the spawn count is asserted from the error text.
    #[tokio::test]
    async fn test_manager_concurrent_first_callers_spawn_once() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let project_dir = missing_project_dir();
        let project_id = ProjectId("concurrent-single-flight".into());

        let mut handles = Vec::new();
        for _ in 0..20 {
            let project_id = project_id.clone();
            let project_dir = project_dir.clone();
            handles.push(tokio::spawn(async move {
                LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await
            }));
        }

        let mut raw_spawn = 0;
        let mut suppressed = 0;
        for handle in handles {
            let error =
                handle.await.expect("task").expect_err("every caller must observe a failure");
            let text = error.to_string();
            // Classify by the suppression prefix first: the suppression message
            // embeds the original failure text, so it also contains
            // "Failed to spawn".
            if text.contains("start suppressed") {
                suppressed += 1;
            } else if text.contains("Failed to spawn") {
                raw_spawn += 1;
            }
        }
        assert_eq!(raw_spawn, 1, "exactly one caller may reach the spawn");
        assert_eq!(suppressed, 19, "the remaining callers must be suppressed");
    }

    /// Concurrent first-callers for *different* projects must not serialise on
    /// one another or deadlock: each has its own start lock and fails
    /// independently.
    #[tokio::test]
    async fn test_manager_different_projects_do_not_deadlock() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let base = std::env::temp_dir().join("concerto-lsp-manager-distinct");
        let _ = std::fs::remove_dir_all(&base);

        let mut handles = Vec::new();
        for i in 0..4 {
            let project_id = ProjectId(format!("distinct-project-{i}"));
            let project_dir = base.join(format!("p{i}"));
            handles.push(tokio::spawn(async move {
                LspManager::get_or_start(project_id, project_dir, CancellationToken::new()).await
            }));
        }

        let joined = futures::future::join_all(handles);
        let results = tokio::time::timeout(Duration::from_secs(10), joined)
            .await
            .expect("concurrent starts for distinct projects must not deadlock");
        for result in results {
            assert!(result.expect("task").is_err(), "each missing dir must fail independently");
        }
    }

    /// `stop_all` on a manager with no entries must not panic or deadlock.
    #[tokio::test]
    async fn test_manager_empty_stop_all() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let cancel = CancellationToken::new();

        // Ensure the static map starts clean for this test by calling stop_all first.
        LspManager::stop_all(cancel.clone()).await;
        // Second stop_all on an empty map should be a no-op.
        LspManager::stop_all(cancel).await;
    }

    // ------------------------------------------------------------------
    // stop_all vs an in-flight start (orphan race)
    // ------------------------------------------------------------------

    /// Whether `pid` still names a live (or unreaped) process. `kill -0` also
    /// succeeds for a zombie, so this catches a child that was signalled but
    /// never waited on as well as one still running; `stop` reaps the child, so
    /// a completed kill reads as dead.
    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Regression for the orphan race: a start that is in flight while
    /// `stop_all` drains the map must not insert its server afterwards. The
    /// injected start signals it has spawned, then parks until the test has
    /// returned from `stop_all`, forcing the exact interleaving the epoch guard
    /// exists to catch — no language server required.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_manager_stop_all_during_start_kills_the_stale_server() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("stop-race-orphan".into());

        let (began_tx, began_rx) = tokio::sync::oneshot::channel::<()>();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();
        let child_pid = Arc::new(AtomicU32::new(0));

        let handle = {
            let pid_slot = child_pid.clone();
            let project_dir = dir.path().to_path_buf();
            let project_id = project_id.clone();
            tokio::spawn(async move {
                LspManager::get_or_start_with(
                    project_id,
                    CancellationToken::new(),
                    move |_cancel| {
                        async move {
                            let mut client = LspClient::new(&project_dir, "rust-analyzer");
                            let pid = client.attach_test_child(30)?;
                            pid_slot.store(pid, Ordering::SeqCst);
                            let _ = began_tx.send(());
                            // Hold the "handshake" open until stop_all has drained.
                            let _ = resume_rx.await;
                            Ok(client)
                        }
                    },
                )
                .await
            })
        };

        began_rx.await.expect("the injected start must begin");
        // stop_all drains the still-empty map and bumps the shutdown epoch.
        LspManager::stop_all(CancellationToken::new()).await;
        // Release the start so it tries to commit a server spawned before the
        // drain — the orphan window.
        let _ = resume_tx.send(());

        let result = handle.await.expect("task must not panic");
        assert!(
            matches!(result, Err(ToolError::Cancelled)),
            "a start overlapping stop_all must be discarded"
        );
        assert!(
            !CLIENTS.lock().await.contains_key(&project_id),
            "the discarded client must not be tracked"
        );
        assert!(
            START_FAILURES.lock().await.is_empty(),
            "stop_all must leave the failure/cooldown state clean"
        );

        let pid = child_pid.load(Ordering::SeqCst);
        assert!(pid > 0, "the injected server must have spawned");
        assert!(
            !process_alive(pid),
            "the discarded server must be killed, not orphaned (pid {pid})"
        );
    }

    /// The complement of the race test: `stop_all` still drains and kills a
    /// client that committed *before* the drain, so the epoch guard does not
    /// regress the ordinary shutdown path.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_manager_stop_all_kills_a_committed_client() {
        let _guard = MANAGER_TEST_LOCK.lock().await;
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project_id = ProjectId("stop-all-drains".into());
        let child_pid = Arc::new(AtomicU32::new(0));

        let pid_slot = child_pid.clone();
        let project_dir = dir.path().to_path_buf();
        let client = LspManager::get_or_start_with(
            project_id.clone(),
            CancellationToken::new(),
            move |_cancel| async move {
                let mut client = LspClient::new(&project_dir, "rust-analyzer");
                let pid = client.attach_test_child(30)?;
                pid_slot.store(pid, Ordering::SeqCst);
                Ok(client)
            },
        )
        .await
        .expect("the injected start must commit");
        drop(client);
        assert!(CLIENTS.lock().await.contains_key(&project_id), "a fresh start must be tracked");

        LspManager::stop_all(CancellationToken::new()).await;
        assert!(!CLIENTS.lock().await.contains_key(&project_id), "stop_all must drain the map");

        let pid = child_pid.load(Ordering::SeqCst);
        assert!(pid > 0, "the injected server must have spawned");
        assert!(!process_alive(pid), "stop_all must kill the drained server (pid {pid})");
    }
}
