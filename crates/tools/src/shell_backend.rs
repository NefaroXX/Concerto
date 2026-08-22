//! Shell backend abstraction (ADR-28).
//!
//! Mirrors `concerto-providers`' `ProviderFactory`: a [`ShellBackend`] trait
//! with [`SystemProfile`] implemented for direct shell launches and
//! [`ManagedBash`] wiring the Concerto-managed runtime (ADR-28 Slice 2) into
//! the same execution path. [`ShellProfileFactory`] resolves a configured
//! [`ShellProfileConfig`] (from `concerto-config`) into the appropriate
//! backend.
//!
//! The interactive Terminal reads [`ShellProfileConfig`] directly (it builds
//! `iced_term::BackendSettings` itself). This backend abstraction is what the
//! *agent* shell tool routes through, so the managed runtime can be selected
//! without touching the tool's execution path.

use std::collections::HashMap;
use std::path::PathBuf;

use concerto_config::managed::ManagedRuntimeManager;
use concerto_config::shell::{ShellBackendType, ShellProfileConfig};
use concerto_core::ToolError;

/// A shell execution backend selected by a [`ShellProfileConfig`].
pub trait ShellBackend: Send + Sync {
    /// Backend kind, for diagnostics and audit.
    fn backend_type(&self) -> ShellBackendType;

    /// Resolve the absolute program path to spawn for this backend, honouring
    /// the profile. The default returns `profile.resolve_executable()`; a
    /// managed backend overrides it to point at the installed managed Bash.
    fn resolved_program(&self, profile: &ShellProfileConfig) -> PathBuf {
        profile.resolve_executable()
    }

    /// Resolve the executable + arguments needed to run `command` through this
    /// backend, honouring the profile's configured args and shell family.
    fn command_args(&self, profile: &ShellProfileConfig, command: &str) -> Vec<String>;

    /// Effective environment for spawning under this backend.
    fn effective_env(
        &self,
        profile: &ShellProfileConfig,
        base: &HashMap<String, String>,
    ) -> HashMap<String, String>;

    /// Human-readable availability / readiness check (used by `Test profile`).
    fn check_available(&self, profile: &ShellProfileConfig) -> Result<(), ToolError>;
}

/// System-installed (or custom) shell launched directly.
pub struct SystemProfile;

impl ShellBackend for SystemProfile {
    fn backend_type(&self) -> ShellBackendType {
        ShellBackendType::System
    }

    fn command_args(&self, profile: &ShellProfileConfig, command: &str) -> Vec<String> {
        profile.command_args(command)
    }

    fn effective_env(
        &self,
        profile: &ShellProfileConfig,
        base: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        profile.effective_env(base)
    }

    fn check_available(&self, profile: &ShellProfileConfig) -> Result<(), ToolError> {
        if profile.executable.trim().is_empty() {
            return Err(ToolError::ExecutionFailed {
                message: "shell profile has no executable configured".into(),
            });
        }
        resolve_executable(&profile.executable)
            .ok_or_else(|| ToolError::ExecutionFailed {
                message: format!("shell executable not found on PATH: {}", profile.executable),
            })
            .map(|_| ())
    }
}

/// Concerto-managed runtime (ADR-28 Slice 2). Resolves the installed managed
/// Bash via [`ManagedRuntimeManager`] so the agent shell tool runs a
/// Concerto-owned, integrity-checked executable that never touches the system
/// `PATH`, registry, or shell config. When no runtime is installed it surfaces a
/// recoverable diagnostic.
pub struct ManagedBash;

impl ShellBackend for ManagedBash {
    fn backend_type(&self) -> ShellBackendType {
        ShellBackendType::Managed
    }

    fn resolved_program(&self, _profile: &ShellProfileConfig) -> PathBuf {
        match ManagedRuntimeManager::auto_detect() {
            Some(manifest) => manifest.bash_executable,
            None => PathBuf::from("managed-bash"),
        }
    }

    fn command_args(&self, profile: &ShellProfileConfig, command: &str) -> Vec<String> {
        // Managed Bash is a POSIX shell; reuse the profile's POSIX command
        // wrapping (`-c`), which is what `ShellProfileConfig::command_args`
        // selects for a non-cmd/powershell base.
        profile.command_args(command)
    }

    fn effective_env(
        &self,
        _profile: &ShellProfileConfig,
        base: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        base.clone()
    }

    fn check_available(&self, _profile: &ShellProfileConfig) -> Result<(), ToolError> {
        match ManagedRuntimeManager::auto_detect() {
            Some(_) => Ok(()),
            None => Err(ToolError::ExecutionFailed {
                message: "Concerto Managed Bash is not installed. Install it from Settings → Terminal (ADR-28 Slice 2).".into(),
            }),
        }
    }
}

/// Resolves a profile into the appropriate backend.
pub struct ShellProfileFactory;

impl ShellProfileFactory {
    /// Build the backend for a profile. `System` and `Custom` both use the
    /// direct-launch backend; `Managed` uses the Concerto-managed runtime.
    pub fn backend_for(profile: &ShellProfileConfig) -> Box<dyn ShellBackend> {
        match profile.backend {
            ShellBackendType::System | ShellBackendType::Custom => Box::new(SystemProfile),
            ShellBackendType::Managed => Box::new(ManagedBash),
            _ => Box::new(SystemProfile),
        }
    }

    /// Find a profile by id within a profile list.
    pub fn resolve<'a>(
        profiles: &'a [ShellProfileConfig],
        id: &str,
    ) -> Option<&'a ShellProfileConfig> {
        profiles.iter().find(|p| p.id == id)
    }
}

/// Resolve an executable name via `PATH`, or confirm an absolute/relative path
/// exists. Returns `None` if not found.
fn resolve_executable(name: &str) -> Option<PathBuf> {
    let path = std::path::Path::new(name);
    if path.is_absolute() || name.contains(std::path::MAIN_SEPARATOR) {
        return if path.exists() { Some(path.to_path_buf()) } else { None };
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::shell::ShellProfileConfig;

    #[test]
    fn managed_backend_resolves_to_managed_program() {
        let profile = ShellProfileConfig {
            id: "managed-bash".into(),
            name: "Concerto Managed Bash".into(),
            backend: ShellBackendType::Managed,
            ..Default::default()
        };
        let backend = ShellProfileFactory::backend_for(&profile);
        assert!(matches!(backend.backend_type(), ShellBackendType::Managed));
        // Deterministic regardless of install state: when nothing is installed
        // the resolved sentinel is "managed-bash"; when installed it points at
        // the real managed binary. Both are distinct from an empty/external exe.
        let program = backend.resolved_program(&profile);
        assert!(!program.as_os_str().is_empty());
    }

    #[test]
    fn managed_backend_availability_matches_install_state() {
        let profile = ShellProfileConfig {
            id: "managed-bash".into(),
            name: "Concerto Managed Bash".into(),
            backend: ShellBackendType::Managed,
            ..Default::default()
        };
        let backend = ShellProfileFactory::backend_for(&profile);
        // Availability must mirror what the manager reports for the same env.
        let installed = concerto_config::managed::ManagedRuntimeManager::auto_detect().is_some();
        assert_eq!(backend.check_available(&profile).is_ok(), installed);
    }

    #[test]
    fn system_backend_checks_executable_presence() {
        let profile = ShellProfileConfig {
            id: "sh".into(),
            name: "Sh".into(),
            backend: ShellBackendType::System,
            // "definitely-not-a-real-shell-binary" should not be on PATH.
            executable: "definitely-not-a-real-shell-binary".into(),
            ..Default::default()
        };
        let backend = ShellProfileFactory::backend_for(&profile);
        assert!(backend.check_available(&profile).is_err());
    }
}
