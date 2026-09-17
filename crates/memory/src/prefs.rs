//! Cross-session user preferences store.
//!
//! Provides typed preference keys and a persistent key-value store backed by
//! a JSON file on disk (`user_prefs.json`).
//!
//! Reads and writes are serialised by the single `.concerto.lock` (ADR-11):
//! `UserPrefsStore::open` acquires the process-wide lock at the Concerto data
//! root and holds it for the store's lifetime, so every `get`/`set`/`get_all`
//! on the returned store runs while the lock is held.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use concerto_core::error::MemoryError;
use concerto_core::lock::{acquire_data_dir_lock, DataDirLock};

/// How long `open()` waits for a `.concerto.lock` held by another instance
/// (ADR-11) before failing with [`MemoryError::DataDirLocked`].
const PREF_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Typed preference keys
// ---------------------------------------------------------------------------

/// Typed preference keys for cross-session user preferences.
///
/// Each variant maps to a stable string key used in the underlying
/// storage layer.
#[non_exhaustive]
pub enum PrefKey {
    /// Preferred coding style (e.g. "rust", "python", "go").
    PreferredCodingStyle,
    /// Command used to run tests (e.g. "cargo test").
    TestRunnerCommand,
    /// Shell glob patterns that have been explicitly approved by the user.
    ApprovedShellPatterns,
    /// UI theme identifier (e.g. "dark", "light", "system").
    UiTheme,
    /// UI font base size in pixels.
    UiFontSize,
    /// Unit for displaying cost information (e.g. "usd", "tokens").
    CostDisplayUnit,
    /// Percentage of context window allocated to budget (0-100).
    ContextBudgetAllocation,
}

impl PrefKey {
    /// Return the stable string representation of this key.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrefKey::PreferredCodingStyle => "preferred_coding_style",
            PrefKey::TestRunnerCommand => "test_runner_command",
            PrefKey::ApprovedShellPatterns => "approved_shell_patterns",
            PrefKey::UiTheme => "ui_theme",
            PrefKey::UiFontSize => "ui_font_size",
            PrefKey::CostDisplayUnit => "cost_display_unit",
            PrefKey::ContextBudgetAllocation => "context_budget_allocation",
        }
    }
}

// ---------------------------------------------------------------------------
// UserPrefsStore
// ---------------------------------------------------------------------------

/// Cross-session user preferences store.
///
/// Backed by `{data_dir}/user_prefs.json`. Opening the store acquires the
/// process-wide `.concerto.lock` at the Concerto data root — the parent of
/// `data_dir`, because the prefs directory is `<root>/prefs` — and keeps it
/// for the store's lifetime, so every read/write runs under the single
/// instance lock (ADR-11).
pub struct UserPrefsStore {
    prefs: Mutex<HashMap<String, String>>,
    data_dir: PathBuf,
    _data_dir_lock: Arc<DataDirLock>,
}

impl UserPrefsStore {
    /// Open (or create) the user preferences store.
    ///
    /// Acquires the `.concerto.lock` at the parent of `data_dir` (the data
    /// root) and holds it for the returned store's lifetime. Timeouts /
    /// second-instance contention surface as [`MemoryError::DataDirLocked`]
    /// with the data root path and the holder's PID hint when readable.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::DataDirLocked`] when another process already
    /// holds the lock on the data root's `.concerto.lock`.
    pub fn open(data_dir: &std::path::Path) -> Result<Self, MemoryError> {
        // The prefs directory is `<root>/prefs`, so the ADR-11 lock file lives
        // at `<root>/.concerto.lock`. Fall back to `data_dir` itself for the
        // (relative-path) edge case where `parent()` is empty.
        let data_root =
            data_dir.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(data_dir);
        let _data_dir_lock = acquire_data_dir_lock(data_root, Some(PREF_LOCK_TIMEOUT), None)?;

        let prefs_path = data_dir.join("user_prefs.json");
        let prefs: HashMap<String, String> = if prefs_path.exists() {
            let json_str = fs::read_to_string(&prefs_path)
                .map_err(|e| MemoryError::Persistence(format!("failed to read prefs file: {e}")))?;
            serde_json::from_str(&json_str).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(Self { prefs: Mutex::new(prefs), data_dir: data_dir.to_path_buf(), _data_dir_lock })
    }

    /// Retrieve the value associated with `key`, if any.
    pub fn get(&self, key: &PrefKey) -> Option<String> {
        // In an infallible context - recover from poison
        let store = self.prefs.lock().unwrap_or_else(|e| e.into_inner());
        store.get(key.as_str()).cloned()
    }

    /// Set `key` to `value`.
    pub fn set(&self, key: &PrefKey, value: String) -> Result<(), MemoryError> {
        let mut store = self
            .prefs
            .lock()
            .map_err(|_| MemoryError::Persistence("prefs lock poisoned".into()))?;
        store.insert(key.as_str().to_string(), value);

        // Persist to file
        let prefs_path = self.data_dir.join("user_prefs.json");
        let json = serde_json::to_string_pretty(&store.clone())
            .map_err(|e| MemoryError::Serialization(format!("failed to serialize prefs: {e}")))?;
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| MemoryError::Persistence(format!("failed to create data dir: {e}")))?;
        std::fs::write(&prefs_path, json)
            .map_err(|e| MemoryError::Persistence(format!("failed to write prefs file: {e}")))?;
        Ok(())
    }

    /// Return a copy of all stored preferences.
    pub fn get_all(&self) -> HashMap<String, String> {
        // In an infallible context - recover from poison
        let store = self.prefs.lock().unwrap_or_else(|e| e.into_inner());
        store.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefs directory inside a hermetic tempdir; the `.concerto.lock` is
    /// then created at the tempdir root (the data root).
    fn prefs_dir(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("prefs")
    }

    #[test]
    fn set_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();

        assert!(store.get(&PrefKey::UiTheme).is_none());

        store.set(&PrefKey::UiTheme, "dark".to_string()).unwrap();
        assert_eq!(store.get(&PrefKey::UiTheme).unwrap(), "dark");
    }

    #[test]
    fn get_all_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();

        store.set(&PrefKey::UiTheme, "dark".to_string()).unwrap();
        store.set(&PrefKey::CostDisplayUnit, "usd".to_string()).unwrap();

        let all = store.get_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("ui_theme"), Some(&"dark".to_string()));
        assert_eq!(all.get("cost_display_unit"), Some(&"usd".to_string()));
    }

    #[test]
    fn missing_key_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();

        assert!(store.get(&PrefKey::PreferredCodingStyle).is_none());
        assert!(store.get(&PrefKey::TestRunnerCommand).is_none());
        assert!(store.get(&PrefKey::ApprovedShellPatterns).is_none());
    }

    #[test]
    fn open_creates_the_data_root_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let _store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();

        // ADR-11: one `.concerto.lock` at the data root (parent of prefs/).
        assert!(dir.path().join(".concerto.lock").is_file());
        assert!(!prefs_dir(&dir).join(".concerto.lock").exists());
    }

    #[test]
    fn prefs_write_persists_under_lock_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            // Write happens while the store holds the root lock.
            let store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();
            store.set(&PrefKey::UiTheme, "dark".to_string()).unwrap();
        }
        // The store dropped -> lock released; a fresh store re-reads the JSON.
        let store = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();
        assert_eq!(store.get(&PrefKey::UiTheme).unwrap(), "dark");
    }

    #[test]
    fn same_process_reentry_opens_two_stores_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let first = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();
        first.set(&PrefKey::UiTheme, "dark".to_string()).unwrap();
        // A second store on the same data root shares the in-process lock
        // (the registry) instead of flock-ing against ourselves, and re-reads
        // the persisted JSON.
        let second = UserPrefsStore::open(&prefs_dir(&dir)).unwrap();

        assert_eq!(second.get(&PrefKey::UiTheme).unwrap(), "dark");
    }

    #[test]
    fn second_instance_contention_fails_with_path_and_pid() {
        let dir = tempfile::tempdir().unwrap();

        // Spawn a child that opens a prefs store and holds the root lock.
        let exe = std::env::current_exe().unwrap();
        let lock_dir = prefs_dir(&dir).clone();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("prefs::tests::hold_prefs_lock_helper")
            .arg("--nocapture")
            .env("CONCERTO_PREFS_DIR", &lock_dir)
            .spawn()
            .expect("spawn prefs lock helper");

        // The parent must fail to open its own store while the child holds
        // the lock.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut failure = None;
        while std::time::Instant::now() < deadline {
            if let Err(err @ MemoryError::DataDirLocked { .. }) = UserPrefsStore::open(&lock_dir) {
                failure = Some(err);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();

        let err = failure.expect("helper process never held the prefs lock");
        let MemoryError::DataDirLocked { path, pid_hint } = err else {
            panic!("expected DataDirLocked, got {err:?}");
        };
        assert!(path.ends_with(".concerto.lock") || path == dir.path().to_path_buf());
        assert_eq!(pid_hint, Some(child.id()), "lock holder PID should be recorded");
    }

    /// Helper for [`second_instance_contention_fails_with_path_and_pid`]: holds
    /// a prefs store (and therefore the root lock) until killed. No-op unless
    /// spawned with `CONCERTO_PREFS_DIR`.
    #[test]
    fn hold_prefs_lock_helper() {
        let Some(dir) = std::env::var_os("CONCERTO_PREFS_DIR") else {
            return;
        };
        let _store =
            UserPrefsStore::open(&std::path::PathBuf::from(dir)).expect("helper opens prefs");
        std::thread::sleep(Duration::from_secs(60));
    }
}
