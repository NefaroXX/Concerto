//! Process-wide single-instance data-directory lock (ADR-11).
//!
//! Exactly one lock file, `.concerto.lock`, lives in the Concerto data root
//! and is held with an OS advisory lock (`fd-lock`, which is `flock` on Unix
//! and `LockFileEx` on Windows) for as long as the owning store/process needs
//! exclusive write access. Unlock happens via the OS when the guard — or the
//! whole process, on crash — goes away, so a stale lock file never needs
//! manual cleanup (ADR-11 relies on the kernel releasing locks, not on
//! deleting lock files).
//!
//! This module lives in `concerto-core` because both `concerto-sessions` and
//! `concerto-memory` (independent leaves above core) hold the same lock; any
//! home in one of them would force a new dependency edge between the leaves.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::CancellationToken;

/// Name of the single coordination lock file in the Concerto data root.
pub const LOCK_FILE_NAME: &str = ".concerto.lock";

/// Delay between `try_write` polls while waiting for a contended lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Errors from [`acquire_data_dir_lock`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    /// Another live Concerto instance holds the lock for this data directory.
    ///
    /// `pid_hint` is the process ID the holder wrote into the lock file, when
    /// it can be read back (best-effort, informational only).
    #[error(
        "another Concerto instance is using data directory {path}{}",
        crate::lock::format_pid_hint(.pid_hint)
    )]
    Locked { path: PathBuf, pid_hint: Option<u32> },
    /// An underlying filesystem failure (create dir, open file, flock).
    #[error("data directory lock I/O error: {0}")]
    Io(String),
    /// Acquisition was cancelled before the lock became available.
    #[error("data directory lock acquisition cancelled")]
    Cancelled,
}

/// Render the optional PID hint fragment for lock error messages.
pub(crate) fn format_pid_hint(pid_hint: &Option<u32>) -> String {
    pid_hint.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

/// A held, process-wide `.concerto.lock` for one Concerto data directory.
///
/// The guard is alive for as long as any [`Arc`] to this value is held;
/// dropping the last reference releases the OS lock (flock is released when
/// the underlying descriptor closes). Store types embed an `Arc<DataDirLock>`
/// so the lock covers the store's lifetime (ADR-11).
pub struct DataDirLock {
    data_dir: PathBuf,
    lock_path: PathBuf,
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
}

impl DataDirLock {
    /// The data directory whose single-instance lock is held.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The absolute-ish path of the lock file (`.concerto.lock` in the root).
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl std::fmt::Debug for DataDirLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataDirLock")
            .field("data_dir", &self.data_dir)
            .field("lock_path", &self.lock_path)
            .finish_non_exhaustive()
    }
}

/// Lock file path -> live [`DataDirLock`] for same-process re-entry.
///
/// Concurrent acquisitions of the *same* lock file inside one process share a
/// single guard from the registry instead of flock-ing twice (flock would
/// treat a second descriptor from the same process as a foreign holder and
/// deadlock/time out against itself).
type Registry = HashMap<PathBuf, Weak<DataDirLock>>;

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire the process-wide exclusive lock for `data_dir` (ADR-11).
///
/// Creates `data_dir` (and the lock file) on demand, so an unborn data root
/// works on first launch. With `timeout`, acquisition polls with a short
/// sleep and returns [`LockError::Locked`] when the deadline passes; with
/// `cancel`, a cancelled token aborts the wait with [`LockError::Cancelled`].
/// When both are `None`, acquisition blocks until the lock is free.
///
/// Repeated acquisitions of the same lock file within one process share the
/// same underlying OS lock (see the registry), so stores SRP-stack safely.
pub fn acquire_data_dir_lock(
    data_dir: &Path,
    timeout: Option<Duration>,
    cancel: Option<&CancellationToken>,
) -> Result<Arc<DataDirLock>, LockError> {
    let data_dir = data_dir.to_path_buf();
    std::fs::create_dir_all(&data_dir).map_err(|error| {
        LockError::Io(format!("failed to create data directory {}: {error}", data_dir.display()))
    })?;

    let lock_path = data_dir.join(LOCK_FILE_NAME);
    let key = canonical_key(&lock_path);

    // Fast path: this process already holds the lock for this data directory.
    if let Some(lock) = registry_get(&key) {
        return Ok(lock);
    }

    // NOTE: never truncate here — a contender must preserve the holder's PID
    // hint until it actually acquires the lock (then `write_pid_hint` opens
    // the file again with truncation).
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            LockError::Io(format!("failed to open lock file {}: {error}", lock_path.display()))
        })?;

    // The OS lock guard must outlive this call, so the `fd-lock` handle is
    // intentionally leaked (one small allocation per lock file, reclaimed at
    // process exit). The guard borrows the leaked handle for `'static`, so
    // the kernel-held lock lives exactly as long as the guard stored in
    // `DataDirLock`. A contended acquisition that times out cancels its wait
    // but leaves the fd (unlocked) open until process exit — a bounded,
    // deliberate cost.
    let rwlock: &'static mut fd_lock::RwLock<File> =
        Box::leak(Box::new(fd_lock::RwLock::new(file)));

    // A blocking wait cannot yield a `'static` guard from this function
    // (borrowck rejects reborrowing the leaked handle inside a loop), so
    // contention hangs on a dedicated thread that performs the single,
    // kernel-blocking `write()` and ships the guard back here. This caller
    // keeps its cancel/timeout semantics while the worker sleeps in flock.
    let path_for_worker = lock_path.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = rwlock.write().map_err(|error| {
            LockError::Io(format!("failed to lock {}: {error}", path_for_worker.display()))
        });
        let _ = tx.send(result);
    });

    let deadline = timeout.map(|duration| Instant::now() + duration);
    loop {
        if let Some(cancel) = cancel {
            if cancel.is_cancelled() {
                return Err(LockError::Cancelled);
            }
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(LockError::Locked {
                    path: lock_path.clone(),
                    pid_hint: read_pid_hint(&lock_path),
                });
            }
        }
        // A same-process competitor may have acquired (and registered) the
        // lock between our fast-path check and the worker's blocked flock —
        // share its guard rather than timing out against ourselves.
        if let Some(lock) = registry_get(&key) {
            return Ok(lock);
        }
        match rx.recv_timeout(LOCK_POLL_INTERVAL) {
            Ok(Ok(guard)) => {
                // Only the actual holder records its PID, so a blocked
                // contender reports who owns the lock (best-effort).
                write_pid_hint(&lock_path);
                let lock = Arc::new(DataDirLock { data_dir, lock_path, _guard: guard });
                let mut registry = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.insert(key, Arc::downgrade(&lock));
                return Ok(lock);
            }
            Ok(Err(error)) => return Err(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The worker only exits without a guard if it panicked; flock
                // failures are delivered through the channel above.
                return Err(LockError::Io(
                    "data directory lock worker thread terminated unexpectedly".into(),
                ));
            }
        }
    }
}

/// Canonical registry key for a lock file path, so `./data` and an absolute
/// path to the same file share one entry. Falls back to the raw path when the
/// filesystem cannot canonicalise (the file normally exists by then).
fn canonical_key(lock_path: &Path) -> PathBuf {
    std::fs::canonicalize(lock_path).unwrap_or_else(|_| lock_path.to_path_buf())
}

/// Look up a live in-process lock for `key`.
fn registry_get(key: &PathBuf) -> Option<Arc<DataDirLock>> {
    let registry = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.get(key).and_then(Weak::upgrade)
}

/// Best-effort PID record so a block-timed-out contender can report who holds
/// the lock. Purely informational; never affects locking semantics.
fn write_pid_hint(lock_path: &Path) {
    use std::io::Write;

    if let Ok(mut file) = OpenOptions::new().write(true).truncate(true).open(lock_path) {
        let _ = file.write_all(std::process::id().to_string().as_bytes());
    }
}

/// Read back the PID another holder wrote, when parseable.
fn read_pid_hint(lock_path: &Path) -> Option<u32> {
    let content = std::fs::read(lock_path).ok()?;
    let text = std::str::from_utf8(&content).ok()?.trim();
    text.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_creates_lock_file_and_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let unborn = root.join("nested").join("data");

        let lock = acquire_data_dir_lock(&unborn, Some(Duration::from_secs(1)), None).unwrap();
        assert!(unborn.is_dir());
        assert!(unborn.join(LOCK_FILE_NAME).is_file());
        assert!(lock.lock_path().ends_with(LOCK_FILE_NAME));
        assert_eq!(lock.data_dir(), unborn);
    }

    #[test]
    fn same_process_reentry_shares_guard() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_data_dir_lock(dir.path(), Some(Duration::from_secs(1)), None).unwrap();

        // A concurrent attempt on the same directory must succeed and share
        // the same underlying guard (flock would deadlock against ourselves
        // on a fresh descriptor without the registry).
        let second = acquire_data_dir_lock(dir.path(), Some(Duration::from_secs(1)), None).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn lock_is_released_when_last_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let lock = acquire_data_dir_lock(dir.path(), Some(Duration::from_secs(1)), None).unwrap();
        drop(lock);

        // The registry entry is now stale; a fresh acquisition must succeed
        // (it would hang forever if the OS lock were not released).
        let again = acquire_data_dir_lock(dir.path(), Some(Duration::from_secs(1)), None).unwrap();
        assert!(again.lock_path().ends_with(LOCK_FILE_NAME));
    }

    #[test]
    fn contended_lock_fails_after_timeout_with_holder_pid() {
        let dir = tempfile::tempdir().unwrap();

        // Spawn a child test process that acquires the lock and holds it.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("lock::tests::hold_lock_helper")
            .arg("--nocapture")
            .env("CONCERTO_LOCK_DIR", dir.path())
            .spawn()
            .expect("spawn lock helper");

        // Wait until the child holds the lock (our own acquisition blocks).
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed = None;
        while Instant::now() < deadline {
            match acquire_data_dir_lock(dir.path(), Some(Duration::from_millis(60)), None) {
                Err(LockError::Locked { ref path, pid_hint }) if pid_hint == Some(child.id()) => {
                    observed = Some((path.clone(), pid_hint));
                    break;
                }
                Ok(_) | Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let Some((path, pid_hint)) = observed else {
            let _ = child.kill();
            panic!("helper process never took the data directory lock");
        };
        assert!(path.ends_with(LOCK_FILE_NAME));
        assert_eq!(pid_hint, Some(child.id()));

        // Kill the holder: the kernel releases the flock, so a fresh
        // acquisition must now succeed with no stale-lock cleanup.
        let _ = child.kill();
        let _ = child.wait();
        let acquired = acquire_data_dir_lock(dir.path(), Some(Duration::from_secs(5)), None);
        assert!(acquired.is_ok(), "lock not released after holder exit: {:?}", acquired);
    }

    /// Helper for [`contended_lock_fails_after_timeout_with_holder_pid`]: holds
    /// the lock until killed. A no-op (returns immediately) unless spawned via
    /// `CONCERTO_LOCK_DIR`, so an accidental local run can never wedge CI.
    #[test]
    fn hold_lock_helper() {
        let Some(dir) = std::env::var_os("CONCERTO_LOCK_DIR") else {
            return;
        };
        let _guard =
            acquire_data_dir_lock(Path::new(&dir), None, None).expect("helper acquires lock");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn cancelled_acquisition_aborts_wait() {
        let dir = tempfile::tempdir().unwrap();

        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("lock::tests::hold_lock_helper")
            .arg("--nocapture")
            .env("CONCERTO_LOCK_DIR", dir.path())
            .spawn()
            .expect("spawn lock helper");

        let deadline = Instant::now() + Duration::from_secs(10);
        // Wait until the child actually holds the lock.
        loop {
            if matches!(
                acquire_data_dir_lock(dir.path(), Some(Duration::from_millis(60)), None),
                Err(LockError::Locked { .. })
            ) {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("helper never took the lock");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let token = CancellationToken::new();
        let token_child = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            token_child.cancel();
        });
        let result = acquire_data_dir_lock(dir.path(), None, Some(&token));
        assert!(matches!(result, Err(LockError::Cancelled)));

        let _ = child.kill();
        let _ = child.wait();
    }
}
