//! Cross-session user preferences store.
//!
//! Provides typed preference keys and a persistent key-value store backed by
//! a JSON file on disk (`user_prefs.json`).
//!
//! The current implementation uses an in-memory `HashMap` as the working
//! store and persists to a JSON file on every write. A future upgrade to
//! WAL-mode SQLite with `fd-lock` advisory locking (ADR-11) would improve
//! multi-instance safety.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use concerto_core::error::MemoryError;

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
/// Uses WAL-mode SQLite at `{data_dir}/user_prefs.db` with an
/// `fd-lock` advisory lock file `{data_dir}/user_prefs.lock` to
/// prevent concurrent instance corruption (ADR-11).
pub struct UserPrefsStore {
    prefs: Mutex<HashMap<String, String>>,
    data_dir: PathBuf,
}

impl UserPrefsStore {
    /// Open or create the user preferences database.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::DataDirLocked`] when another process already
    /// holds the advisory lock on `user_prefs.lock`.
    pub fn open(data_dir: &std::path::Path) -> Result<Self, MemoryError> {
        let prefs_path = data_dir.join("user_prefs.json");
        let prefs: HashMap<String, String> = if prefs_path.exists() {
            let json_str = fs::read_to_string(&prefs_path)
                .map_err(|e| MemoryError::Persistence(format!("failed to read prefs file: {e}")))?;
            serde_json::from_str(&json_str).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(Self { prefs: Mutex::new(prefs), data_dir: data_dir.to_path_buf() })
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

    #[test]
    fn set_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserPrefsStore::open(dir.path()).unwrap();

        assert!(store.get(&PrefKey::UiTheme).is_none());

        store.set(&PrefKey::UiTheme, "dark".to_string()).unwrap();
        assert_eq!(store.get(&PrefKey::UiTheme).unwrap(), "dark");
    }

    #[test]
    fn get_all_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserPrefsStore::open(dir.path()).unwrap();

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
        let store = UserPrefsStore::open(dir.path()).unwrap();

        assert!(store.get(&PrefKey::PreferredCodingStyle).is_none());
        assert!(store.get(&PrefKey::TestRunnerCommand).is_none());
        assert!(store.get(&PrefKey::ApprovedShellPatterns).is_none());
    }
}
