//! Shared project selection registry for all Concerto frontends.

use concerto_core::helpers::canonical_project_path;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use concerto_core::error::ConfigError;

const MAX_RECENT_PROJECTS: usize = 20;

/// Persisted active project, startup behaviour, and most-recently-used project list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRegistry {
    #[serde(default)]
    active: Option<PathBuf>,
    #[serde(default)]
    recent: Vec<PathBuf>,
    /// Reopen the last active project on startup instead of showing the project chooser.
    #[serde(default)]
    reopen_last_project: bool,
}

impl ProjectRegistry {
    /// Load the registry from the standard Concerto data directory.
    pub fn load() -> Result<Self, ConfigError> {
        let path = registry_path().ok_or_else(|| {
            ConfigError::Load("unable to determine Concerto data directory".to_string())
        })?;
        Self::load_from(&path)
    }

    /// Load from an explicit path (also used by tests).
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path).map_err(|error| {
            ConfigError::Load(format!("failed to read project registry: {error}"))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            ConfigError::Load(format!("failed to parse project registry: {error}"))
        })
    }

    /// Persist the registry atomically at the standard location.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = registry_path().ok_or_else(|| {
            ConfigError::Load("unable to determine Concerto data directory".to_string())
        })?;
        self.save_to(&path)
    }

    /// Persist to an explicit path (also used by tests).
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ConfigError::Load(format!("failed to create project registry directory: {error}"))
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            ConfigError::Load(format!("failed to serialize project registry: {error}"))
        })?;
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temporary, bytes).map_err(|error| {
            ConfigError::Load(format!("failed to write project registry: {error}"))
        })?;
        if let Err(rename_error) = std::fs::rename(&temporary, path) {
            // Windows does not consistently replace an existing destination
            // with `rename`. Preserve the update with a truncate-and-copy
            // fallback, then remove the private temporary file.
            if std::fs::copy(&temporary, path).is_err() {
                let _ = std::fs::remove_file(temporary);
                return Err(ConfigError::Load(format!(
                    "failed to replace project registry: {rename_error}"
                )));
            }
            let _ = std::fs::remove_file(temporary);
        }
        Ok(())
    }

    /// Select an existing directory and move it to the front of the MRU list.
    pub fn select(&mut self, path: &Path) -> Result<PathBuf, ConfigError> {
        if !path.is_dir() {
            return Err(ConfigError::InvalidValue(format!(
                "project directory does not exist: {}",
                path.display()
            )));
        }
        let canonical = canonical_project_path(path);
        self.recent.retain(|candidate| canonical_project_path(candidate) != canonical);
        self.recent.insert(0, canonical.clone());
        self.recent.truncate(MAX_RECENT_PROJECTS);
        self.active = Some(canonical.clone());
        Ok(canonical)
    }

    /// Currently selected project, if it still exists.
    pub fn active(&self) -> Option<&Path> {
        self.active.as_deref().filter(|path| path.is_dir())
    }

    /// Existing projects in most-recently-used order.
    pub fn recent(&self) -> impl Iterator<Item = &Path> {
        self.recent.iter().map(PathBuf::as_path).filter(|path| path.is_dir())
    }

    /// Whether the last active project should be reopened automatically at startup.
    pub fn reopen_last_project(&self) -> bool {
        self.reopen_last_project
    }

    /// Change whether Concerto reopens the last active project at startup.
    pub fn set_reopen_last_project(&mut self, reopen: bool) {
        self.reopen_last_project = reopen;
    }
}

/// Standard registry location.
pub fn registry_path() -> Option<PathBuf> {
    crate::legacy::data_dir().map(|dir| dir.join("projects.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_canonical_deduplicated_and_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let path = temp.path().join("projects.json");

        let mut registry = ProjectRegistry::default();
        registry.select(&first).unwrap();
        registry.select(&second).unwrap();
        registry.select(&first.join(".")).unwrap();
        registry.save_to(&path).unwrap();

        let loaded = ProjectRegistry::load_from(&path).unwrap();
        assert_eq!(loaded.active(), Some(canonical_project_path(&first).as_path()));
        assert_eq!(loaded.recent().count(), 2);
    }

    #[test]
    fn reopen_last_project_is_opt_in_and_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("projects.json");

        let mut registry = ProjectRegistry::default();
        assert!(!registry.reopen_last_project());
        registry.set_reopen_last_project(true);
        registry.save_to(&path).unwrap();

        let loaded = ProjectRegistry::load_from(&path).unwrap();
        assert!(loaded.reopen_last_project());
    }

    #[test]
    fn older_registry_files_default_to_project_chooser() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let path = temp.path().join("projects.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "active": project,
                "recent": []
            })
            .to_string(),
        )
        .unwrap();

        let loaded = ProjectRegistry::load_from(&path).unwrap();
        assert!(!loaded.reopen_last_project());
    }

    #[test]
    fn select_nonexistent_directory_returns_error() {
        let mut registry = ProjectRegistry::default();
        let result = registry.select(Path::new("/definitely/does/not/exist/xyz-12345"));
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("does not exist"));
    }
}
