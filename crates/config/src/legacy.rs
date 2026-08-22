//! Legacy fallback support for the rename from `opencode-rs` → `concerto`.
//!
//! These helpers check the new `concerto` paths/names first and fall back
//! to the old `opencode-rs` equivalents so existing installations continue
//! working without manual migration.
//!
//! # Design
//! - **Read**: try new path → try old path → return what exists (or new path
//!   as default when neither exists).
//! - **Write**: always use the new path. Old data is never automatically
//!   migrated — callers can optionally migrate on first write.
//! - **Env vars**: `CONCERTO_*` is checked first. If a key is not found,
//!   `OPENCODE_RS_*` is tried as a fallback in `CredentialStore`.
//! - **Keyring**: service name `concerto` is tried first; if an account is
//!   not found, `opencode-rs` is tried as a fallback.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Old names (read-only reference — do not write to these)
// ---------------------------------------------------------------------------

/// Old config directory name (e.g. `~/.config/opencode-rs/`).
pub const OLD_CONFIG_DIR: &str = "opencode-rs";

/// Old data directory name (e.g. `~/.local/share/opencode-rs/`).
pub const OLD_DATA_DIR: &str = "opencode-rs";

/// Old environment variable prefix.
pub const OLD_ENV_PREFIX: &str = "OPENCODE_RS_";

/// Old keyring service name.
pub const OLD_KEYRING_SERVICE: &str = "opencode-rs";

/// Old project-scoped config filename.
pub const OLD_PROJECT_CONFIG_FILE: &str = ".opencode-rs.toml";

// ---------------------------------------------------------------------------
// New names (canonical — use these for all writes)
// ---------------------------------------------------------------------------

/// New config directory name (e.g. `~/.config/concerto/`).
pub const NEW_CONFIG_DIR: &str = "concerto";

/// New data directory name (e.g. `~/.local/share/concerto/`).
pub const NEW_DATA_DIR: &str = "concerto";

/// New environment variable prefix.
pub const NEW_ENV_PREFIX: &str = "CONCERTO_";

/// New keyring service name.
pub const NEW_KEYRING_SERVICE: &str = "concerto";

/// New project-scoped config filename.
pub const NEW_PROJECT_CONFIG_FILE: &str = ".concerto.toml";

// ---------------------------------------------------------------------------
// Path helpers with fallback
// ---------------------------------------------------------------------------

/// Resolve the config directory path.
///
/// Returns the new path (`~/.config/concerto/config.toml`) if it exists,
/// falling back to the old path (`~/.config/opencode-rs/config.toml`).
/// When neither exists, returns the new path (callers will create it).
pub fn config_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;

    let new_path = dir.join(NEW_CONFIG_DIR).join("config.toml");
    if new_path.exists() {
        return Some(new_path);
    }

    let old_path = dir.join(OLD_CONFIG_DIR).join("config.toml");
    if old_path.exists() {
        return Some(old_path);
    }

    Some(new_path)
}

/// Resolve the project-scoped config file path.
///
/// Always returns the new name (`.concerto.toml`). The project file is
/// typically gitignored and regenerated — there is no benefit to carrying
/// the old name forward for this file.
pub fn project_config_path(root: &std::path::Path) -> PathBuf {
    root.join(NEW_PROJECT_CONFIG_FILE)
}

/// Resolve the data directory path.
///
/// Returns the new path (`~/.local/share/concerto/`) if it exists,
/// falling back to the old path (`~/.local/share/opencode-rs/`).
/// When neither exists, returns the new path.
pub fn data_dir() -> Option<PathBuf> {
    let dir = dirs::data_dir()?;

    let new_dir = dir.join(NEW_DATA_DIR);
    if new_dir.exists() {
        return Some(new_dir);
    }

    let old_dir = dir.join(OLD_DATA_DIR);
    if old_dir.exists() {
        return Some(old_dir);
    }

    Some(new_dir)
}

/// Resolve the data directory path, preferring the legacy path.
///
/// Use this for **read-only** operations where you specifically want the
/// old location (e.g. plugin discovery that may have installed plugins
/// under the old name).
pub fn legacy_data_dir() -> Option<PathBuf> {
    let dir = dirs::data_dir()?;
    Some(dir.join(OLD_DATA_DIR))
}

/// Return the keyring service name to use, preferring the new name.
///
/// This does NOT check at runtime — it always returns the new name.
/// The actual fallback logic lives in `CredentialStore` which tries
/// the new service first and falls back to the old one on miss.
pub fn keyring_service_name() -> &'static str {
    NEW_KEYRING_SERVICE
}

/// Return the old keyring service name (for fallback lookups).
pub fn old_keyring_service_name() -> &'static str {
    OLD_KEYRING_SERVICE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_distinct() {
        assert_ne!(NEW_CONFIG_DIR, OLD_CONFIG_DIR);
        assert_ne!(NEW_DATA_DIR, OLD_DATA_DIR);
        assert_ne!(NEW_ENV_PREFIX, OLD_ENV_PREFIX);
        assert_ne!(NEW_KEYRING_SERVICE, OLD_KEYRING_SERVICE);
        assert_ne!(NEW_PROJECT_CONFIG_FILE, OLD_PROJECT_CONFIG_FILE);
    }

    #[test]
    fn data_dir_returns_some() {
        // At minimum the function should not panic and should return Some
        // (even if the directory doesn't exist, it returns a path)
        assert!(data_dir().is_some());
    }
}
