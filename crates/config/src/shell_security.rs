//! Human-client security editing. Not registered as an agent tool (ADR-81).

use crate::saving::atomic_write;
use concerto_core::error::ConfigError;
use concerto_core::shell_security::ShellSecurity;
use std::path::Path;

/// Validate and persist only the security table, preserving unrelated settings.
/// `expected_revision` prevents a stale client from overwriting a newer policy.
pub fn save_shell_security(
    path: &Path,
    expected_revision: u64,
    mut security: ShellSecurity,
) -> Result<ShellSecurity, ConfigError> {
    let mut lock = config_lock(path)?;
    let _guard = lock.write().map_err(|e| ConfigError::Load(e.to_string()))?;
    security.validate().map_err(ConfigError::Load)?;
    let current = read_shell_security(path)?;
    if current.revision != expected_revision {
        return Err(ConfigError::Load("shell security changed; reload before saving".into()));
    }
    security.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| ConfigError::Load("shell security revision exhausted".into()))?;
    let raw = if path.exists() {
        std::fs::read_to_string(path).map_err(|e| ConfigError::Load(e.to_string()))?
    } else {
        String::new()
    };
    let mut document =
        raw.parse::<toml_edit::DocumentMut>().map_err(|e| ConfigError::Load(e.to_string()))?;
    let serialized = toml::to_string(&security).map_err(|e| ConfigError::Load(e.to_string()))?;
    let table = serialized
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Load(e.to_string()))?;
    document["shell_security"] = toml_edit::Item::Table(table.as_table().clone());
    atomic_write(path, document.to_string().as_bytes())?;
    Ok(security)
}

/// Read the security table without migrations or startup writers while locked.
pub(crate) fn read_shell_security(path: &Path) -> Result<ShellSecurity, ConfigError> {
    #[derive(serde::Deserialize, Default)]
    struct Document {
        #[serde(default)]
        shell_security: ShellSecurity,
    }
    let document = if path.exists() {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Load(e.to_string()))?;
        toml::from_str::<Document>(&raw).map_err(|e| ConfigError::Load(e.to_string()))?
    } else {
        Document::default()
    };
    document.shell_security.validate().map_err(ConfigError::Load)?;
    Ok(document.shell_security)
}

/// Serialize human-client security edits and generic config saves across processes.
pub(crate) fn config_lock(path: &Path) -> Result<fd_lock::RwLock<std::fs::File>, ConfigError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Load(e.to_string()))?;
    }
    let lock_path = path.with_extension("toml.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| ConfigError::Load(e.to_string()))?;
    Ok(fd_lock::RwLock::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    // verifies: project files cannot widen user-global permissions or change the interpreter profile.
    #[test]
    fn project_security_cannot_override_global() {
        let directory = tempfile::tempdir().expect("directory");
        let global = directory.path().join("config.toml");
        let project = directory.path().join("project");
        std::fs::create_dir(&project).expect("project");
        std::fs::write(&global, "[shell_security]\nprocesses = 'deny'\n").expect("global");
        std::fs::write(
            project.join(".concerto.toml"),
            "[shell_security]\nprocesses = 'policy'\ninterpreter_compatibility = true\n",
        )
        .expect("project config");
        let config = crate::load_config(Some(&global), Some(&project)).expect("load");
        assert_eq!(
            config.shell_security.processes,
            concerto_core::shell_security::ShellPermission::Deny
        );
        assert!(!config.shell_security.interpreter_compatibility);
    }

    // verifies: client edits increment revision, preserve unrelated settings, and reject stale writes.
    #[test]
    fn editing_is_revision_checked() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# keep this\nprimary_provider = 'ollama'\n").expect("config");
        let result = save_shell_security(&path, 0, ShellSecurity::read_only()).expect("save");
        assert_eq!(result.revision, 1);
        assert!(std::fs::read_to_string(&path).expect("read").contains("# keep this"));
        assert!(save_shell_security(&path, 0, ShellSecurity::default()).is_err());
    }

    // verifies: generic settings saves cannot overwrite newer security, including on first creation.
    #[test]
    fn generic_save_cannot_change_or_overwrite_security() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config.toml");
        let mut config =
            crate::AppConfig { shell_security: ShellSecurity::read_only(), ..Default::default() };
        assert!(crate::save_config(&config, &path).is_err());
        config.shell_security = ShellSecurity::default();
        crate::save_config(&config, &path).expect("initial config");
        save_shell_security(&path, 0, ShellSecurity::read_only()).expect("security update");
        assert!(crate::save_config(&config, &path).is_err());
        assert_eq!(read_shell_security(&path).expect("current").revision, 1);
    }

    // verifies: security-only changes participate in live configuration equality.
    #[test]
    fn configuration_equality_includes_shell_security() {
        let config = crate::AppConfig::default();
        let mut changed = config.clone();
        changed.shell_security.timeout_secs += 1;
        assert_ne!(config, changed);
    }
}
