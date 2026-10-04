//! User-owned native shell permissions (ADR-81). Configuration is not a sandbox.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Scope limitation of [`ShellSecurity::protected_paths`], shared verbatim by
/// the CLI help and the desktop Settings → Shell view so those surfaces cannot
/// drift from each other or from the documentation.
///
/// The list binds only Concerto's built-in filesystem tool. Native `run`
/// commands and container project mounts are **not** covered: host programs
/// keep their ambient OS access to the same paths.
pub const PROTECTED_PATHS_SCOPE_NOTE: &str = "protected_paths constrains only the built-in \
    filesystem tool; native run commands and container project mounts are not covered.";

/// Additional permission ceiling applied after ordinary policy rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellPermission {
    Deny,
    #[default]
    Ask,
    Policy,
}

/// Host mode uses ambient OS permissions; isolated mode requires a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellIsolation {
    #[default]
    Host,
    Container,
}

/// Offline is an enforced boundary requirement, not a command heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellNetwork {
    Offline,
    #[default]
    Unrestricted,
}

/// Versioned user-global settings. No secret values belong in this structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellSecurity {
    pub revision: u64,
    pub processes: ShellPermission,
    pub writes: ShellPermission,
    pub interpreter_compatibility: bool,
    pub isolation: ShellIsolation,
    pub network: ShellNetwork,
    pub container_image: String,
    /// Empty permits any executable subject to the central policy/approval gate.
    /// Entries must be absolute canonical file paths; bare PATH names are refused.
    pub allowed_executables: Vec<PathBuf>,
    /// Only these host variables are inherited; additions are deliberately absent.
    pub environment_allowlist: Vec<String>,
    pub timeout_secs: u64,
    pub max_output_bytes: u64,
    pub max_argument_bytes: usize,
    /// Optional kernel resource requirements. Unsupported requirements fail closed.
    pub memory_bytes: Option<u64>,
    pub max_processes: Option<u32>,
    pub cpu_seconds: Option<u64>,
    /// Native reads/writes refuse these project-relative subtrees.
    pub protected_paths: Vec<PathBuf>,
    pub history_enabled: bool,
}

impl Default for ShellSecurity {
    fn default() -> Self {
        Self {
            revision: 0,
            processes: ShellPermission::Ask,
            writes: ShellPermission::Ask,
            interpreter_compatibility: false,
            isolation: ShellIsolation::Host,
            network: ShellNetwork::Unrestricted,
            container_image: "debian:bookworm-slim".into(),
            allowed_executables: Vec::new(),
            environment_allowlist: vec![
                "PATH".into(),
                "LANG".into(),
                "LC_ALL".into(),
                "SystemRoot".into(),
                "WINDIR".into(),
                "TEMP".into(),
                "TMP".into(),
            ],
            timeout_secs: 120,
            max_output_bytes: 1024 * 1024,
            max_argument_bytes: 32 * 1024,
            memory_bytes: None,
            max_processes: None,
            cpu_seconds: None,
            protected_paths: vec![".env".into(), ".concerto.toml".into()],
            history_enabled: true,
        }
    }
}

impl ShellSecurity {
    pub fn read_only() -> Self {
        Self { processes: ShellPermission::Deny, writes: ShellPermission::Deny, ..Self::default() }
    }

    pub fn isolated() -> Self {
        Self {
            isolation: ShellIsolation::Container,
            network: ShellNetwork::Offline,
            memory_bytes: Some(1024 * 1024 * 1024),
            max_processes: Some(128),
            ..Self::default()
        }
    }

    /// Validate before persistence/loading, never silently clamp bad settings.
    pub fn validate(&self) -> Result<(), String> {
        if self.timeout_secs == 0 || self.timeout_secs > 86_400 {
            return Err("timeout_secs must be between 1 and 86400".into());
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > 10 * 1024 * 1024 {
            return Err("max_output_bytes must be between 1 and 10485760".into());
        }
        if self.max_argument_bytes == 0 || self.max_argument_bytes > 1024 * 1024 {
            return Err("max_argument_bytes must be between 1 and 1048576".into());
        }
        if self.memory_bytes == Some(0)
            || self.max_processes == Some(0)
            || self.cpu_seconds == Some(0)
        {
            return Err("resource limits must be positive when set".into());
        }
        if self.container_image.trim().is_empty() || self.container_image.starts_with('-') {
            return Err("container_image must be a non-empty image reference".into());
        }
        for path in &self.allowed_executables {
            if !path.is_absolute() {
                return Err("allowed_executables entries must be absolute paths".into());
            }
        }
        for path in &self.protected_paths {
            if path.as_os_str().is_empty()
                || path.is_absolute()
                || path.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(
                    "protected_paths entries must be relative paths without traversal".into()
                );
            }
        }
        for key in &self.environment_allowlist {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err("environment_allowlist entries must be variable names".into());
            }
            if matches!(
                key.to_ascii_uppercase().as_str(),
                "LD_PRELOAD" | "LD_LIBRARY_PATH" | "DYLD_INSERT_LIBRARIES" | "DYLD_LIBRARY_PATH"
            ) {
                return Err("loader injection variables cannot be inherited".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // verifies: malformed settings cannot create disabled resource limits or executable PATH grants.
    #[test]
    fn invalid_security_settings_are_rejected() {
        let mut settings = ShellSecurity { timeout_secs: 0, ..Default::default() };
        assert!(settings.validate().is_err());
        settings.timeout_secs = 1;
        settings.allowed_executables.push("cargo".into());
        assert!(settings.validate().is_err());
        settings.allowed_executables.clear();
        settings.protected_paths.push("../config".into());
        assert!(settings.validate().is_err());
    }

    // verifies: defaults and supported presets round-trip while unknown fields fail closed.
    #[test]
    fn presets_roundtrip_and_unknown_fields_fail() {
        for settings in
            [ShellSecurity::default(), ShellSecurity::read_only(), ShellSecurity::isolated()]
        {
            assert!(settings.validate().is_ok());
            let value = serde_json::to_value(&settings).expect("serialize");
            assert_eq!(
                serde_json::from_value::<ShellSecurity>(value).expect("deserialize"),
                settings
            );
        }
        assert!(serde_json::from_str::<ShellSecurity>(r#"{"allow_everything":true}"#).is_err());
    }

    // verifies: the shared scope note keeps stating every limitation it exists to state.
    #[test]
    fn protected_paths_scope_note_states_its_limits() {
        for claim in ["built-in filesystem tool", "native run commands", "container project mounts"]
        {
            assert!(
                PROTECTED_PATHS_SCOPE_NOTE.contains(claim),
                "the shared scope note must keep its {claim:?} claim"
            );
        }
    }
}
