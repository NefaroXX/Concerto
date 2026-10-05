//! Canonical location names and path helpers.
//!
//! Every current name (config dir, data dir, env prefix, keyring service,
//! project config file) is declared here so the rest of the workspace
//! resolves those paths through one seam. Only these names are read: there
//! are no alternate directories, env prefixes, or keyring services.
//!
//! # Design
//! - **Read/Write**: always the canonical path below.
//! - **Env vars**: `CONCERTO_*` is the only merged prefix.
//! - **Keyring**: service name `concerto`.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Canonical names (use these for all reads and writes)
// ---------------------------------------------------------------------------

/// Config directory name (e.g. `~/.config/concerto/`).
pub const NEW_CONFIG_DIR: &str = "concerto";

/// Data directory name (e.g. `~/.local/share/concerto/`).
pub const NEW_DATA_DIR: &str = "concerto";

/// Environment variable prefix.
pub const NEW_ENV_PREFIX: &str = "CONCERTO_";

/// Keyring service name.
pub const NEW_KEYRING_SERVICE: &str = "concerto";

/// Project-scoped config filename.
pub const NEW_PROJECT_CONFIG_FILE: &str = ".concerto.toml";

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve the config file path (`~/.config/concerto/config.toml`).
///
/// Returns `None` when the platform has no resolvable config dir (e.g. some
/// minimal containers); callers then fall back to defaults + env only.
pub fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(NEW_CONFIG_DIR).join("config.toml"))
}

/// Resolve the project-scoped config file path (always `.concerto.toml`).
///
/// The project file is typically gitignored and regenerated; only this name
/// is ever read, so a stale file under any other name in a project repo
/// cannot resurrect config values.
pub fn project_config_path(root: &std::path::Path) -> PathBuf {
    root.join(NEW_PROJECT_CONFIG_FILE)
}

/// Resolve the data directory path (`~/.local/share/concerto/`).
///
/// Returns `None` when the platform has no resolvable data dir.
pub fn data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(NEW_DATA_DIR))
}

/// Return the keyring service name used for credential reads and writes.
pub fn keyring_service_name() -> &'static str {
    NEW_KEYRING_SERVICE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are load-bearing: they must match what already exists on
    /// disk and in the OS keychain, so a typo in any of them fails loudly.
    #[test]
    fn canonical_names_are_stable() {
        assert_eq!(NEW_CONFIG_DIR, "concerto");
        assert_eq!(NEW_DATA_DIR, "concerto");
        assert_eq!(NEW_ENV_PREFIX, "CONCERTO_");
        assert_eq!(NEW_KEYRING_SERVICE, "concerto");
        assert_eq!(NEW_PROJECT_CONFIG_FILE, ".concerto.toml");
    }

    #[test]
    fn data_dir_returns_some() {
        // At minimum the function should not panic and should return Some
        // (even if the directory doesn't exist, it returns a path)
        assert!(data_dir().is_some());
    }
}
