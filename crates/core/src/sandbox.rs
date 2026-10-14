//! Container-runtime detection for the `SandboxProfile::Containerized` profile
//! (ADR-72), plus the portable sandbox capability interface (H10, ADR-83).
//!
//! Detection (above) is deliberately small and side-effect free. It answers one
//! question — "is an OS-level container runtime (docker or podman) available on
//! this host?" — for the policy engine's admission gate and the shell tool's
//! invocation routing. It does **not** start the runtime, contact a daemon, or
//! pull an image; daemon/image failures are surfaced later by the runtime
//! itself and handled fail-closed by the caller.
//!
//! The capability interface (below) answers the wider operator question —
//! "what isolation is actually in force for this profile on this platform,
//! and what does the sandbox restore" — as a backend-neutral, versioned
//! [`SandboxCapabilityDescriptor`]. It is descriptive only: it never admits,
//! denies, or routes anything (the policy engine never reads it), and it
//! offers no restore coverage — **no container/runtime checkpoint-restore
//! backend ships in this milestone** (ADR-83). Its offers are shaped for
//! H09's preflight ([`EffectCoverage`], consumable by
//! `assemble_rollback_offers`), so composition is structural, never a
//! preflight-logic fork.
//!
//! Detection is a pure `PATH` probe: a runtime is available when its binary
//! resolves to a regular, executable file on `PATH`. The probe is behind the
//! [`ContainerRuntimeProbe`] trait so policy and shell tests can inject
//! found/absent/malformed results without a container runtime installed — no
//! test or CI job may require one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::checkpoint::{EffectCoverage, EffectKind, EffectSupport};
use crate::types::SandboxProfile;

/// Supported OS-level container runtimes, in probe preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
const SUPPORTED_RUNTIMES: [ContainerRuntime; 2] =
    [ContainerRuntime::Docker, ContainerRuntime::Podman];

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

// ---------------------------------------------------------------------------
// Portable sandbox capability interface (H10, ADR-83)
// ---------------------------------------------------------------------------
//
// A backend-neutral, versioned capability description for sandbox profiles,
// per profile × per platform. It expresses what isolation is actually in
// force (including the honest "none/unconfined" case), the declared resource
// coverage as H09 preflight offers (all `Unsupported` — no checkpoint-restore
// backend ships in this milestone), per-platform availability as an explicit
// tri-state (never a default that silently reads as "fine"), and what a
// sandbox backend explicitly cannot restore.
//
// Descriptive only: nothing here admits, denies, or routes. The fail-closed
// admission gate stays in `crate::policy`; this interface only reports.

/// Current [`SandboxCapabilityDescriptor`] envelope schema version.
///
/// Named-version policy (per the checkpoint-record precedent): this exact
/// version loads as-is; anything else is rejected by consumers that enforce
/// it. There are no legacy versions — the descriptor is greenfield in H10.
pub const SANDBOX_CAPABILITY_SCHEMA_VERSION: u32 = 1;

/// The decision that keeps Windows unsupported: ADR-72 §5 (v1, in force).
/// Cited by every `windows` availability, so the gate stays greppable.
pub const SANDBOX_WINDOWS_DECISION: &str = "ADR-72 §5";

/// A container with a host bind mount is not a rollback system (ADR-72
/// constraint 5): deleting and recreating the container drops the writable
/// layer only and restores nothing on the host or the network.
pub const CONTAINER_RECREATION_LIMITATION: &str = "a container with a host bind mount is not a rollback system: deleting and recreating the container drops the container's own writable layer only; mounted host data and remote services are not restored (ADR-72 constraint 5)";

/// Mounted host data is explicitly unrestorable by any sandbox backend: host
/// paths bind-mounted into the container survive container removal.
pub const MOUNTED_HOST_DATA_UNRESTORABLE: &str = "mounted host data is not restored: host paths bind-mounted into the container (-v <root>:<root> and operator extra_mounts) survive container deletion/recreation; writes through the mount are never rolled back";

/// Remote services are explicitly unrestorable by any sandbox backend:
/// network effects and state held outside the host are never reversed.
pub const REMOTE_SERVICES_UNRESTORABLE: &str = "remote services are not restored: network effects and state held outside the host are never reversed by container deletion/recreation";

/// Anything outside the container's own writable layer is explicitly
/// unrestorable: only that layer is ephemeral on deletion.
pub const OUTSIDE_WRITABLE_LAYER_UNRESTORABLE: &str = "anything outside the container's own writable layer (mounted host data, remote services, images) is not restored: only the writable layer is ephemeral on deletion";

/// No sandbox checkpoint/restore backend ships in this milestone: the sandbox
/// declares isolation only, never restoration (ADR-83). Full sandbox
/// restoration is an optional later capability, not a promise of this one.
pub const NO_SANDBOX_RESTORE_MILESTONE: &str = "no container/runtime checkpoint-restore backend ships in this milestone: the sandbox declares isolation only and restores nothing; full sandbox restoration is an optional later capability (ADR-83)";

/// Per-platform availability as an explicit tri-state (H10 exit criterion).
///
/// There is no "fine by default": every platform states `Available`,
/// `Unavailable`, or `UnsupportedByDecision` with the reason attached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlatformAvailability {
    /// The profile's stated isolation is in force on this platform.
    Available {
        /// What is in force and how it was determined (operator-readable).
        detail: String,
    },
    /// The profile cannot be provided on this platform right now
    /// (missing runtime, unimplemented stub — never a silent fallback).
    Unavailable {
        /// Why (operator-readable).
        reason: String,
    },
    /// The platform is decided-not-supported: the decision is the answer, so
    /// no probe is consulted for it (a probe would be a lie).
    UnsupportedByDecision {
        /// The deciding record (e.g. [`SANDBOX_WINDOWS_DECISION`]).
        decision: String,
        /// Why (operator-readable).
        reason: String,
    },
}

impl PlatformAvailability {
    /// Whether the profile's isolation is in force on this platform.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// The stable single-word spelling (`available` / `unavailable` /
    /// `unsupported_by_decision`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available { .. } => "available",
            Self::Unavailable { .. } => "unavailable",
            Self::UnsupportedByDecision { .. } => "unsupported_by_decision",
        }
    }
}

/// What isolation is actually in force for a profile — including the honest
/// "none/unconfined" case. Descriptive: naming a mechanism here grants
/// nothing and is never consulted by any gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SandboxIsolation {
    /// No isolation: tools run as the invoking user (the `None` profile), or
    /// the profile is denied/never admitted so no isolation is in force.
    Unconfined,
    /// OS-level container isolation via a container runtime (the
    /// `Containerized` profile). The runtime is `Some` when the probe found
    /// one, `None` when the profile is refused for lack of one.
    Containerized {
        /// The detected runtime, when the probe found one.
        runtime: Option<ContainerRuntime>,
    },
}

impl SandboxIsolation {
    /// The stable single-word spelling (`unconfined` / `containerized`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unconfined => "unconfined",
            Self::Containerized { .. } => "containerized",
        }
    }
}

/// One class of state a sandbox backend explicitly cannot restore (H10 exit
/// criterion: the declared-unsupported resources are named, not footnoted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UnrestorableResourceKind {
    /// Host paths bind-mounted into the container.
    MountedHostData,
    /// Network effects and state held outside the host.
    RemoteServices,
    /// Anything outside the container's own writable layer.
    OutsideContainerWritableLayer,
}

impl UnrestorableResourceKind {
    /// The stable single-word spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MountedHostData => "mounted_host_data",
            Self::RemoteServices => "remote_services",
            Self::OutsideContainerWritableLayer => "outside_container_writable_layer",
        }
    }
}

/// One explicitly unrestorable resource class, with the operator-readable
/// reason it is out of every sandbox restore claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnrestorableResource {
    /// The unrestorable class.
    pub kind: UnrestorableResourceKind,
    /// Why nothing restores this (operator-readable).
    pub reason: String,
}

/// A backend-neutral, versioned sandbox capability description: one sandbox
/// profile × every platform (H10 deliverable).
///
/// Per-profile availability is explicit per platform: `linux`/`macos` derive
/// from the supplied runtime probe (same PATH-probe mechanics on both —
/// parity is claimed only where the implementation has it), while `windows`
/// is always [`PlatformAvailability::UnsupportedByDecision`] citing
/// [`SANDBOX_WINDOWS_DECISION`] — the decision is the answer and the probe is
/// never consulted for Windows.
///
/// `coverage` is the profile's H09 preflight share: every kind
/// [`EffectSupport::Unsupported`](crate::checkpoint::EffectSupport::Unsupported),
/// because no checkpoint-restore backend ships in this milestone. A sandbox
/// descriptor feeds [`preflight_rollback_guarantee`](crate::checkpoint::preflight_rollback_guarantee)
/// through [`Self::offers`]; it never duplicates the preflight logic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SandboxCapabilityDescriptor {
    /// Envelope schema version ([`SANDBOX_CAPABILITY_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The profile described.
    pub profile: SandboxProfile,
    /// What isolation is actually in force (if any).
    pub isolation: SandboxIsolation,
    /// Availability on Linux (from the supplied probe, or the stub verdict).
    pub linux: PlatformAvailability,
    /// Availability on macOS (same probe mechanics as Linux).
    pub macos: PlatformAvailability,
    /// Always `UnsupportedByDecision` citing [`SANDBOX_WINDOWS_DECISION`].
    pub windows: PlatformAvailability,
    /// Declared resource coverage as H09 offers (all `Unsupported` this
    /// milestone — one entry per [`EffectKind`](crate::checkpoint::EffectKind)).
    pub coverage: Vec<EffectCoverage>,
    /// What the backend explicitly cannot restore (mounted host data, remote
    /// services, anything outside the writable layer).
    pub cannot_restore: Vec<UnrestorableResource>,
    /// Honest ceilings (never collapsed).
    pub limitations: Vec<String>,
}

impl SandboxCapabilityDescriptor {
    /// This descriptor's share of a preflight's `offered` list, for
    /// `assemble_rollback_offers` to consume (adaptation, not a preflight
    /// fork: the verdict logic stays in
    /// [`preflight_rollback_guarantee`](crate::checkpoint::preflight_rollback_guarantee)).
    #[must_use]
    pub fn offers(&self) -> Vec<EffectCoverage> {
        self.coverage.clone()
    }

    /// This descriptor's offer for one effect kind (unclaimed kinds read
    /// `Unsupported` — never assumed recoverable).
    #[must_use]
    pub fn offer_for(&self, kind: EffectKind) -> EffectSupport {
        self.coverage
            .iter()
            .find(|entry| entry.kind == kind)
            .map(|entry| entry.support.clone())
            .unwrap_or(EffectSupport::Unsupported {
                reason: format!(
                    "no sandbox offer claims {} effects: unrecoverable by construction",
                    kind.as_str()
                ),
            })
    }

    /// Availability on the compiling host: `windows` on Windows, `macos` on
    /// macOS, `linux` elsewhere (other Unix hosts share Linux's PATH-probe
    /// mechanics, so they read the Linux evaluation).
    #[must_use]
    pub fn availability_for_current_host(&self) -> &PlatformAvailability {
        if cfg!(target_os = "windows") {
            &self.windows
        } else if cfg!(target_os = "macos") {
            &self.macos
        } else {
            &self.linux
        }
    }
}

/// Build the capability descriptor for `profile` from a runtime probe result.
///
/// Pure (no I/O, no environment reads): the caller supplies the probe — pass
/// [`detect_container_runtime`] (or [`probe_with_path`] in tests) for live
/// truth, or a canned result for failure injection. The `windows` field
/// ignores `runtime` by design (decided-not-supported needs no probe).
#[must_use]
pub fn sandbox_capability_for(
    profile: SandboxProfile,
    runtime: &RuntimeAvailability,
) -> SandboxCapabilityDescriptor {
    match profile {
        SandboxProfile::None => {
            let reason =
                "SandboxProfile::None declares no sandbox isolation: tools run as the invoking user with ambient authority; no sandbox backend is in force"
                    .to_string();
            SandboxCapabilityDescriptor {
                schema_version: SANDBOX_CAPABILITY_SCHEMA_VERSION,
                profile,
                isolation: SandboxIsolation::Unconfined,
                linux: PlatformAvailability::Unavailable { reason: reason.clone() },
                macos: PlatformAvailability::Unavailable { reason },
                windows: windows_unsupported(
                    "no sandbox isolation is declared on any platform; on Windows additionally no sandbox evaluation is offered (ADR-72 §5) — the run proceeds unconfined and must not be read as sandboxed",
                ),
                coverage: unsupported_coverage(&|kind| {
                    format!(
                        "no sandbox isolation in force (SandboxProfile::None): the sandbox banks no pre-images and reverses no {} effects",
                        kind.as_str()
                    )
                }),
                cannot_restore: unrestorable_set(),
                limitations: vec![
                    "SandboxProfile::None runs tools as the invoking user with ambient authority"
                        .to_string(),
                    NO_SANDBOX_RESTORE_MILESTONE.to_string(),
                ],
            }
        }
        SandboxProfile::Containerized => {
            let isolation = SandboxIsolation::Containerized { runtime: runtime.runtime() };
            let posix = match runtime {
                RuntimeAvailability::Available(found) => PlatformAvailability::Available {
                    detail: format!(
                        "container runtime '{}' resolved on PATH; daemon/image health is verified only at invocation, fail-closed (never a silent unconfined run)",
                        found.as_str()
                    ),
                },
                RuntimeAvailability::Unavailable { reason } => PlatformAvailability::Unavailable {
                    reason: format!(
                        "{reason}: Containerized is refused (sandbox_containerized_runtime_unavailable), never run unconfined"
                    ),
                },
            };
            SandboxCapabilityDescriptor {
                schema_version: SANDBOX_CAPABILITY_SCHEMA_VERSION,
                profile,
                isolation,
                linux: posix.clone(),
                macos: posix,
                windows: windows_unsupported(
                    "the Containerized profile is not supported on Windows (v1, in force): the runtime probe is not consulted and Containerized fails closed without probing",
                ),
                coverage: unsupported_coverage(&|kind| match kind {
                    EffectKind::File => "container isolation banks no file pre-images: the writable layer is ephemeral on deletion but host-mounted paths survive it — file restore belongs to file checkpoints, never the sandbox"
                        .to_string(),
                    EffectKind::Database => "container isolation reverses no database effects: rows written to mounted host data or remote services survive deletion"
                        .to_string(),
                    EffectKind::Process => "container isolation restores no process state: recreating the container starts fresh processes with fresh memory"
                        .to_string(),
                    EffectKind::Shell => "shell side-effects are never reversed by the sandbox"
                        .to_string(),
                    EffectKind::Network => "network effects are never reversed by the sandbox"
                        .to_string(),
                }),
                cannot_restore: unrestorable_set(),
                limitations: vec![
                    CONTAINER_RECREATION_LIMITATION.to_string(),
                    NO_SANDBOX_RESTORE_MILESTONE.to_string(),
                ],
            }
        }
        SandboxProfile::ReadOnlyFs | SandboxProfile::NetworkIsolated => {
            let name = profile.as_str();
            let reason = format!(
                "SandboxProfile::{name} is an unimplemented stub: denied by policy (sandbox_profiles_not_implemented) and never admitted, so no isolation is in force"
            );
            SandboxCapabilityDescriptor {
                schema_version: SANDBOX_CAPABILITY_SCHEMA_VERSION,
                profile,
                isolation: SandboxIsolation::Unconfined,
                linux: PlatformAvailability::Unavailable { reason: reason.clone() },
                macos: PlatformAvailability::Unavailable { reason },
                windows: windows_unsupported(&format!(
                    "SandboxProfile::{name} is unimplemented on every platform; no Windows path exists either (the Windows story needs a superseding ADR per ADR-72 §5)"
                )),
                coverage: unsupported_coverage(&|kind| {
                    format!(
                        "SandboxProfile::{name} is never admitted: the sandbox claims no {} effects",
                        kind.as_str()
                    )
                }),
                cannot_restore: unrestorable_set(),
                limitations: vec![
                    format!(
                        "SandboxProfile::{name} is denied by policy and never admitted: no run executes under it"
                    ),
                    NO_SANDBOX_RESTORE_MILESTONE.to_string(),
                ],
            }
        }
    }
}

/// The capability table: every profile's descriptor from one probe result.
///
/// Pure like [`sandbox_capability_for`]: the caller supplies the probe, so
/// tests inject found/absent/malformed runtimes and production passes the
/// cached system probe — no test or CI job needs a real runtime.
#[must_use]
pub fn sandbox_capability_table(runtime: &RuntimeAvailability) -> Vec<SandboxCapabilityDescriptor> {
    [
        SandboxProfile::None,
        SandboxProfile::ReadOnlyFs,
        SandboxProfile::NetworkIsolated,
        SandboxProfile::Containerized,
    ]
    .into_iter()
    .map(|profile| sandbox_capability_for(profile, runtime))
    .collect()
}

/// The live capability table from the process-wide cached system probe.
#[must_use]
pub fn current_sandbox_capability_table() -> Vec<SandboxCapabilityDescriptor> {
    sandbox_capability_table(&system_probe().probe())
}

/// One profile's H09 preflight share: the offers
/// `assemble_rollback_offers` merges (strongest claim wins, so these
/// all-`Unsupported` offers never upgrade another adapter's claim).
#[must_use]
pub fn sandbox_offers_for(
    profile: SandboxProfile,
    runtime: &RuntimeAvailability,
) -> Vec<EffectCoverage> {
    sandbox_capability_for(profile, runtime).offers()
}

/// The `windows` field for every descriptor: decided-not-supported, citing
/// [`SANDBOX_WINDOWS_DECISION`]. The probe is never consulted for it.
fn windows_unsupported(reason: &str) -> PlatformAvailability {
    PlatformAvailability::UnsupportedByDecision {
        decision: SANDBOX_WINDOWS_DECISION.to_string(),
        reason: reason.to_string(),
    }
}

/// The three explicitly unrestorable resource classes (H10 exit criterion),
/// with the container-worded reasons that hold for every profile: nothing in
/// this milestone restores them.
fn unrestorable_set() -> Vec<UnrestorableResource> {
    vec![
        UnrestorableResource {
            kind: UnrestorableResourceKind::MountedHostData,
            reason: MOUNTED_HOST_DATA_UNRESTORABLE.to_string(),
        },
        UnrestorableResource {
            kind: UnrestorableResourceKind::RemoteServices,
            reason: REMOTE_SERVICES_UNRESTORABLE.to_string(),
        },
        UnrestorableResource {
            kind: UnrestorableResourceKind::OutsideContainerWritableLayer,
            reason: OUTSIDE_WRITABLE_LAYER_UNRESTORABLE.to_string(),
        },
    ]
}

/// One `Unsupported` offer per effect kind: the sandbox restores nothing in
/// this milestone, and each kind says so with its own reason.
fn unsupported_coverage(reason_for: &dyn Fn(EffectKind) -> String) -> Vec<EffectCoverage> {
    EffectKind::all()
        .iter()
        .map(|kind| EffectCoverage {
            kind: *kind,
            support: EffectSupport::Unsupported { reason: reason_for(*kind) },
        })
        .collect()
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

/// Portable capability interface tests (H10): failure injection over canned
/// probe results — no test here needs a real container runtime, on any host.
///
/// The `#[cfg(all(test, unix))]` module above owns the PATH-probe tests; this
/// module is deliberately ungated (pure constructors only) so the Windows
/// decision and the unrestorable declarations are proven on every platform.
#[cfg(test)]
mod capability_tests {
    use super::*;
    use crate::checkpoint::{preflight_rollback_guarantee, PreflightVerdict};

    fn unavailable_runtime() -> RuntimeAvailability {
        RuntimeAvailability::Unavailable {
            reason: "no container runtime (docker or podman) found on PATH".to_string(),
        }
    }

    fn available_docker() -> RuntimeAvailability {
        RuntimeAvailability::Available(ContainerRuntime::Docker)
    }

    fn unrestorable_kinds(
        descriptor: &SandboxCapabilityDescriptor,
    ) -> Vec<UnrestorableResourceKind> {
        descriptor.cannot_restore.iter().map(|entry| entry.kind).collect()
    }

    #[test]
    fn unconfined_profile_reports_unavailable_and_refuses_shell_network() {
        // A profile declaring no sandbox isolation reports Unavailable with a
        // reason — never a default that reads as "fine" — and its offers
        // refuse a guarantee covering shell/network rather than silently
        // offering them.
        let descriptor = sandbox_capability_for(SandboxProfile::None, &unavailable_runtime());
        assert_eq!(descriptor.schema_version, SANDBOX_CAPABILITY_SCHEMA_VERSION);
        assert_eq!(descriptor.isolation, SandboxIsolation::Unconfined);
        for platform in [&descriptor.linux, &descriptor.macos] {
            match platform {
                PlatformAvailability::Unavailable { reason } => {
                    assert!(!reason.is_empty(), "the reason is the report");
                    assert!(
                        reason.contains("no sandbox isolation") || reason.contains("None"),
                        "unexpected reason: {reason}"
                    );
                }
                other => panic!("unconfined must be Unavailable, got {other:?}"),
            }
        }
        let report = preflight_rollback_guarantee(
            &[EffectKind::File, EffectKind::Shell, EffectKind::Network],
            &descriptor.offers(),
        );
        assert_eq!(report.verdict, PreflightVerdict::Refused);
        assert!(report.covered.is_empty(), "the sandbox covers nothing: {:?}", report.covered);
        for kind in [EffectKind::Shell, EffectKind::Network] {
            let gap = report.gaps.iter().find(|gap| gap.kind == kind).expect("gap");
            assert!(!gap.qualified, "shell/network are refused, never qualified");
        }
    }

    #[test]
    fn unavailable_runtime_is_fail_closed() {
        // Malformed/absent runtime (the probe tests cover the probe itself —
        // this covers the capability verdict over it): Containerized is
        // Unavailable on Linux/macOS with the refusal named, and preflight
        // over its offers still refuses everything.
        let descriptor =
            sandbox_capability_for(SandboxProfile::Containerized, &unavailable_runtime());
        assert_eq!(
            descriptor.isolation,
            SandboxIsolation::Containerized { runtime: None },
            "no runtime found: the isolation names the miss"
        );
        for platform in [&descriptor.linux, &descriptor.macos] {
            match platform {
                PlatformAvailability::Unavailable { reason } => {
                    assert!(
                        reason.contains("sandbox_containerized_runtime_unavailable"),
                        "the fail-closed rule is named: {reason}"
                    );
                }
                other => panic!("absent runtime must be Unavailable, got {other:?}"),
            }
        }
        let report = preflight_rollback_guarantee(EffectKind::all(), &descriptor.offers());
        assert_eq!(report.verdict, PreflightVerdict::Refused);
    }

    #[test]
    fn available_runtime_is_honored_on_linux_macos_but_not_on_windows() {
        // The probe result is honored where probes are meaningful …
        let descriptor = sandbox_capability_for(SandboxProfile::Containerized, &available_docker());
        assert_eq!(
            descriptor.isolation,
            SandboxIsolation::Containerized { runtime: Some(ContainerRuntime::Docker) }
        );
        for platform in [&descriptor.linux, &descriptor.macos] {
            match platform {
                PlatformAvailability::Available { detail } => {
                    assert!(detail.contains("docker"), "{detail}");
                }
                other => panic!("found runtime must be Available, got {other:?}"),
            }
        }
        // … and ignored where the decision is the answer: Windows evaluation
        // returns Unsupported-by-decision WITHOUT probing for a runtime (a
        // probe would be a lie — hence this test passes a FOUND runtime and
        // still requires the decision).
        match &descriptor.windows {
            PlatformAvailability::UnsupportedByDecision { decision, reason } => {
                assert_eq!(decision, SANDBOX_WINDOWS_DECISION);
                assert!(reason.contains("not supported on Windows"), "{reason}");
                assert!(
                    reason.contains("not consulted") || reason.contains("without probing"),
                    "{reason}"
                );
            }
            other => panic!("Windows must be Unsupported-by-decision, got {other:?}"),
        }
        // The decision holds for every profile, including the unconfined one.
        let unconfined = sandbox_capability_for(SandboxProfile::None, &available_docker());
        assert!(
            matches!(unconfined.windows, PlatformAvailability::UnsupportedByDecision { .. }),
            "Windows is decided, not probed — for every profile"
        );
    }

    #[test]
    fn declared_unrestorable_resources_are_named() {
        // Mounted host data, remote services, and everything outside the
        // writable layer are named in the interface for every profile — and
        // container deletion/recreation is stated as a limitation, not a
        // caveat.
        for profile in [
            SandboxProfile::None,
            SandboxProfile::ReadOnlyFs,
            SandboxProfile::NetworkIsolated,
            SandboxProfile::Containerized,
        ] {
            let descriptor = sandbox_capability_for(profile, &unavailable_runtime());
            let kinds = unrestorable_kinds(&descriptor);
            for expected in [
                UnrestorableResourceKind::MountedHostData,
                UnrestorableResourceKind::RemoteServices,
                UnrestorableResourceKind::OutsideContainerWritableLayer,
            ] {
                assert!(kinds.contains(&expected), "{profile:?} must name {}", expected.as_str());
            }
            for entry in &descriptor.cannot_restore {
                assert!(!entry.reason.is_empty(), "every unrestorable names its reason");
            }
        }
        let container =
            sandbox_capability_for(SandboxProfile::Containerized, &unavailable_runtime());
        assert!(
            container.limitations.iter().any(|line| line.contains("not a rollback system")),
            "the ADR-72 constraint-5 point is a stated limitation: {:?}",
            container.limitations
        );
        assert!(
            container.limitations.iter().any(|line| line.contains("restores nothing")),
            "the no-restore milestone promise is explicit: {:?}",
            container.limitations
        );
    }

    #[test]
    fn sandbox_offers_cover_every_kind_and_restore_nothing() {
        // Each backend proves the coverage it declares: the sandbox declares
        // NO restore coverage — every kind Unsupported — and the offer set is
        // complete (no kind silently missing, which preflight would also
        // refuse, but completeness keeps the declaration honest).
        for profile in [
            SandboxProfile::None,
            SandboxProfile::ReadOnlyFs,
            SandboxProfile::NetworkIsolated,
            SandboxProfile::Containerized,
        ] {
            let descriptor = sandbox_capability_for(profile, &unavailable_runtime());
            assert_eq!(
                descriptor.offers().len(),
                EffectKind::all().len(),
                "{profile:?} must offer every kind"
            );
            for kind in EffectKind::all() {
                match descriptor.offer_for(*kind) {
                    EffectSupport::Unsupported { reason } => {
                        assert!(!reason.is_empty(), "{profile:?}/{kind:?} names its reason");
                    }
                    other => panic!("{profile:?}/{kind:?} must be Unsupported, got {other:?}"),
                }
                assert!(!descriptor.offer_for(*kind).is_full());
            }
        }
    }

    #[test]
    fn capability_table_is_explicit_for_every_profile_and_platform() {
        let table = sandbox_capability_table(&unavailable_runtime());
        assert_eq!(table.len(), 4, "one descriptor per profile");
        for descriptor in &table {
            assert_eq!(descriptor.schema_version, SANDBOX_CAPABILITY_SCHEMA_VERSION);
            // No platform ever defaults to Available: stubs and unconfined
            // are Unavailable, Windows is decided.
            for platform in [&descriptor.linux, &descriptor.macos, &descriptor.windows] {
                assert_ne!(
                    platform.as_str(),
                    "available",
                    "only a found runtime may report Available: {descriptor:?}"
                );
            }
            assert!(
                matches!(descriptor.windows, PlatformAvailability::UnsupportedByDecision { .. }),
                "Windows is Unsupported-by-decision for every profile: {descriptor:?}"
            );
            match &descriptor.windows {
                PlatformAvailability::UnsupportedByDecision { decision, .. } => {
                    assert_eq!(decision, SANDBOX_WINDOWS_DECISION);
                }
                other => panic!("Windows must cite the decision, got {other:?}"),
            }
        }
        // Linux and macOS share the probe mechanics: the same probe reports
        // the same verdict on both (parity claimed only where implemented).
        let table = sandbox_capability_table(&available_docker());
        let container = table
            .iter()
            .find(|entry| entry.profile == SandboxProfile::Containerized)
            .expect("containerized row");
        assert_eq!(container.linux, container.macos);
    }

    #[test]
    fn availability_spellings_are_stable() {
        assert_eq!(PlatformAvailability::Available { detail: String::new() }.as_str(), "available");
        assert_eq!(
            PlatformAvailability::Unavailable { reason: String::new() }.as_str(),
            "unavailable"
        );
        assert_eq!(
            PlatformAvailability::UnsupportedByDecision {
                decision: String::new(),
                reason: String::new()
            }
            .as_str(),
            "unsupported_by_decision"
        );
        assert_eq!(SandboxIsolation::Unconfined.as_str(), "unconfined");
        assert_eq!(SandboxIsolation::Containerized { runtime: None }.as_str(), "containerized");
        assert_eq!(UnrestorableResourceKind::MountedHostData.as_str(), "mounted_host_data");
        assert_eq!(UnrestorableResourceKind::RemoteServices.as_str(), "remote_services");
        assert_eq!(
            UnrestorableResourceKind::OutsideContainerWritableLayer.as_str(),
            "outside_container_writable_layer"
        );
    }

    #[test]
    fn container_runtime_and_availability_round_trip() {
        for runtime in [ContainerRuntime::Docker, ContainerRuntime::Podman] {
            let encoded = serde_json::to_value(runtime).expect("serializes");
            let decoded: ContainerRuntime = serde_json::from_value(encoded).expect("deserializes");
            assert_eq!(decoded, runtime);
        }
        let descriptor = sandbox_capability_for(SandboxProfile::Containerized, &available_docker());
        let encoded = serde_json::to_value(&descriptor).expect("descriptor serializes");
        let decoded: SandboxCapabilityDescriptor =
            serde_json::from_value(encoded).expect("descriptor deserializes");
        assert_eq!(decoded, descriptor);
    }

    #[test]
    fn current_host_reads_its_own_platform_field() {
        let descriptor = sandbox_capability_for(SandboxProfile::Containerized, &available_docker());
        let current = descriptor.availability_for_current_host();
        let expected = if cfg!(target_os = "windows") {
            &descriptor.windows
        } else if cfg!(target_os = "macos") {
            &descriptor.macos
        } else {
            &descriptor.linux
        };
        assert_eq!(current, expected);
    }
}
