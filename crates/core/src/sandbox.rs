//! Container-runtime detection for the `SandboxProfile::Containerized` profile
//! (ADR-72).
//!
//! This module is deliberately small and side-effect free. It answers one
//! question — "is an OS-level container runtime (docker or podman) available on
//! this host?" — for the policy engine's admission gate and the shell tool's
//! invocation routing. It does **not** start the runtime, contact a daemon, or
//! pull an image; daemon/image failures are surfaced later by the runtime
//! itself and handled fail-closed by the caller.
//!
//! Detection is a pure `PATH` probe: a runtime is available when its binary
//! resolves to a regular, executable file on `PATH`. The probe is behind the
//! [`ContainerRuntimeProbe`] trait so policy and shell tests can inject
//! found/absent/malformed results without a container runtime installed — no
//! test or CI job may require one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// Supported OS-level container runtimes, in probe preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

impl ContainerRuntime {
    /// The executable name resolved on `PATH`.
    pub fn binary(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    /// Stable spelling for audit rows and diagnostics.
    pub fn as_str(self) -> &'static str {
        self.binary()
    }
}

impl std::fmt::Display for ContainerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a container-runtime probe.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeAvailability {
    /// A supported runtime binary was found on `PATH`.
    Available(ContainerRuntime),
    /// No supported runtime binary was found; `reason` is operator-facing.
    Unavailable { reason: String },
}

impl RuntimeAvailability {
    /// The detected runtime, when available.
    pub fn runtime(&self) -> Option<ContainerRuntime> {
        match self {
            Self::Available(runtime) => Some(*runtime),
            Self::Unavailable { .. } => None,
        }
    }

    /// Whether a runtime is available.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
}

/// Injectable container-runtime probe (ADR-72 §3).
///
/// Implementations must be cheap, synchronous, and panic-free: the policy
/// engine calls [`probe`](Self::probe) on the evaluation hot path.
pub trait ContainerRuntimeProbe: Send + Sync {
    /// Current availability. May cache; call [`refresh`](Self::refresh) to
    /// invalidate.
    fn probe(&self) -> RuntimeAvailability;

    /// Invalidate any cached result (default: no-op).
    fn refresh(&self) {}
}

/// The runtimes probed, in preference order (ADR-72 §3).
const SUPPORTED_RUNTIMES: [ContainerRuntime; 2] = [ContainerRuntime::Docker, ContainerRuntime::Podman];

/// Probe `path_var` for the supported runtimes without touching the process
/// environment. Pure and unit-testable.
///
/// A runtime counts as available when its binary resolves to a regular,
/// executable file. Malformed `PATH` entries (empty, nonexistent directories,
/// non-executable files) are skipped; an empty/absent `PATH` is `Unavailable`,
/// never a panic.
pub fn probe_with_path(path_var: Option<&OsStr>) -> RuntimeAvailability {
    let Some(path_var) = path_var else {
        return RuntimeAvailability::Unavailable {
            reason: "PATH is not set; cannot locate a container runtime".into(),
        };
    };
    for runtime in SUPPORTED_RUNTIMES {
        if find_binary_on_path(runtime.binary(), path_var).is_some() {
            return RuntimeAvailability::Available(runtime);
        }
    }
    RuntimeAvailability::Unavailable {
        reason: "no container runtime (docker or podman) found on PATH".into(),
    }
}

/// Resolve `binary` against the `PATH`-like `path_var`, returning the first
/// regular, executable match. Pure.
pub fn find_binary_on_path(binary: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a regular file the current user can execute.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// System probe with a refreshable cache (ADR-72 §3).
///
/// The first [`probe`](ContainerRuntimeProbe::probe) scans `PATH` and caches
/// the result; later calls are lock-only. [`refresh`](ContainerRuntimeProbe::refresh)
/// drops the cache so a runtime installed mid-session is picked up.
#[derive(Debug, Default)]
pub struct SystemContainerRuntime {
    cached: RwLock<Option<RuntimeAvailability>>,
}

impl SystemContainerRuntime {
    pub fn new() -> Self {
        Self { cached: RwLock::new(None) }
    }

    /// Probe the live process `PATH`.
    fn probe_env() -> RuntimeAvailability {
        probe_with_path(std::env::var_os("PATH").as_deref())
    }
}

impl ContainerRuntimeProbe for SystemContainerRuntime {
    fn probe(&self) -> RuntimeAvailability {
        if let Ok(guard) = self.cached.read() {
            if let Some(cached) = guard.clone() {
                return cached;
            }
        }
        let availability = Self::probe_env();
        if let Ok(mut guard) = self.cached.write() {
            *guard = Some(availability.clone());
        }
        availability
    }

    fn refresh(&self) {
        if let Ok(mut guard) = self.cached.write() {
            *guard = None;
        }
    }
}

/// Process-wide system probe, so repeated policy evaluations and shell plans
/// share one cached `PATH` scan.
pub fn system_probe() -> &'static SystemContainerRuntime {
    static PROBE: OnceLock<SystemContainerRuntime> = OnceLock::new();
    PROBE.get_or_init(SystemContainerRuntime::new)
}

/// Convenience: current availability from the process-wide system probe.
pub fn detect_container_runtime() -> RuntimeAvailability {
    system_probe().probe()
}

/// ADR-72 §2: the only tool whose execution this ADR routes through a
/// container. A `Containerized` action for any other tool is refused as
/// unenforceable rather than run unconfined.
pub const CONTAINER_ROUTABLE_TOOL: &str = "shell";

/// Whether `tool_name` has a container-routable execution path.
pub fn is_container_routable_tool(tool_name: &str) -> bool {
    tool_name == CONTAINER_ROUTABLE_TOOL
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_path_var(dir: &Path) -> std::ffi::OsString {
        std::env::join_paths([dir]).expect("join path")
    }

    fn write_exec(dir: &Path, name: &str, executable: bool) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write");
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(if executable { 0o755 } else { 0o644 });
        std::fs::set_permissions(&path, perms).expect("chmod");
        path
    }

    #[test]
    fn probe_finds_docker_preferentially() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_exec(dir.path(), "docker", true);
        write_exec(dir.path(), "podman", true);
        let found = probe_with_path(Some(&temp_path_var(dir.path())));
        assert_eq!(found, RuntimeAvailability::Available(ContainerRuntime::Docker));
    }

    #[test]
    fn probe_falls_back_to_podman() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_exec(dir.path(), "podman", true);
        let found = probe_with_path(Some(&temp_path_var(dir.path())));
        assert_eq!(found, RuntimeAvailability::Available(ContainerRuntime::Podman));
    }

    #[test]
    fn probe_absent_when_no_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = probe_with_path(Some(&temp_path_var(dir.path())));
        assert!(!found.is_available());
        assert!(matches!(found, RuntimeAvailability::Unavailable { .. }));
    }

    #[test]
    fn probe_treats_nonexistent_path_entries_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let found = probe_with_path(Some(&temp_path_var(&missing)));
        assert!(!found.is_available());
    }

    #[test]
    fn probe_skips_non_executable_file() {
        // Malformed runtime: a regular file named `docker` without the execute
        // bit is not a usable runtime and must not be reported available.
        let dir = tempfile::tempdir().expect("tempdir");
        write_exec(dir.path(), "docker", false);
        let found = probe_with_path(Some(&temp_path_var(dir.path())));
        assert!(!found.is_available());
    }

    #[test]
    fn probe_handles_missing_path_var() {
        let found = probe_with_path(None);
        assert!(!found.is_available());
    }

    #[test]
    fn find_binary_on_path_returns_first_executable_match() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        write_exec(first.path(), "docker", false);
        let expected = write_exec(second.path(), "docker", true);
        let path_var = std::env::join_paths([first.path(), second.path()]).expect("join");
        assert_eq!(find_binary_on_path("docker", &path_var), Some(expected));
    }

    #[test]
    fn system_probe_caches_and_refreshes() {
        let probe = SystemContainerRuntime::new();
        let first = probe.probe();
        assert_eq!(probe.probe(), first, "cached probe must be stable");
        probe.refresh();
        // After refresh it re-probes the live environment; we only assert it
        // stays internally consistent and does not panic.
        let again = probe.probe();
        assert_eq!(probe.probe(), again);
    }

    #[test]
    fn container_routable_tool_is_shell_only() {
        assert!(is_container_routable_tool("shell"));
        assert!(!is_container_routable_tool("filesystem"));
        assert!(!is_container_routable_tool(""));
    }
}
