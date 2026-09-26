//! Container invocation construction for `SandboxProfile::Containerized`
//! (ADR-72).
//!
//! This module is the pure seam between an already-planned shell invocation
//! (row 42's [`ShellPlan`]) and OS-level container isolation. It does **not**
//! start a runtime or pull an image; it constructs the argv that will be
//! spawned argv-direct, so the audited plan and the executed argv stay the same
//! object.
//!
//! Composition rules (ADR-72 §4):
//! - The existing plan is the *inner* command: `Direct` stays argv-direct
//!   inside the container; `Wrapped` keeps its shell + switches + operand, so
//!   row 42's quoting choice is preserved.
//! - The row-45 CPU budget is not duplicated: its `ulimit` prelude is already
//!   embedded in a `Wrapped` operand and rides inside the container command
//!   unchanged.
//! - The project root is the only default mount, at its own absolute path; the
//!   working directory must fall under it (fail-closed otherwise).
//! - Network defaults to none; host environment is never forwarded wholesale.
//!
//! Fail-closed: no runtime, an unsupported platform (Windows), or an
//! out-of-root working directory all produce an explicit error, never a silent
//! fallback to unconfined execution.

use crate::error::ToolError;
use crate::shell::ShellPlan;
use camino::Utf8Path;
use concerto_core::sandbox::{detect_container_runtime, ContainerRuntime, RuntimeAvailability};

/// Explicit refusal produced when the `Containerized` profile cannot be
/// enforced. Kept as `PolicyDenied` so callers see a policy refusal, not a
/// generic execution failure.
fn runtime_unavailable() -> ToolError {
    ToolError::PolicyDenied { rule: "sandbox_containerized_runtime_unavailable".into() }
}

/// Configuration for routing a shell invocation through a container runtime.
///
/// The image is operator-supplied and never pulled implicitly. `network`
/// defaults to `false` (`--network none`); environment variables are forwarded
/// only when explicitly listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerConfig {
    /// Container image to run, e.g. `debian:bookworm-slim`.
    pub image: String,
    /// Explicit runtime override; `None` uses detection (docker, then podman).
    pub runtime: Option<ContainerRuntime>,
    /// Whether the container gets network access (default: none).
    pub network: bool,
    /// Environment entries passed with `-e KEY=VALUE`. Never populated from the
    /// host environment implicitly.
    pub env: Vec<(String, String)>,
    /// Additional bind mounts in `docker`/`podman` `-v` syntax.
    pub extra_mounts: Vec<String>,
}

impl ContainerConfig {
    /// A config for `image` with network disabled and no forwarded environment.
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            runtime: None,
            network: false,
            env: Vec::new(),
            extra_mounts: Vec::new(),
        }
    }
}

/// The inner argv a container executes for `plan`, preserving row 42's launch
/// shape.
pub(crate) fn inner_argv(plan: &ShellPlan) -> Vec<String> {
    match plan {
        ShellPlan::Direct { program, args } => {
            std::iter::once(program.clone()).chain(args.iter().cloned()).collect()
        }
        ShellPlan::Wrapped { program, switches, operand, .. } => std::iter::once(program.clone())
            .chain(switches.iter().cloned())
            .chain(std::iter::once(operand.clone()))
            .collect(),
    }
}

/// Construct the runtime argv (`docker run …` / `podman run …`) that executes
/// `plan` inside a container over the project root.
///
/// The returned plan is always [`ShellPlan::Direct`] with `program` set to the
/// runtime binary: the runtime is spawned argv-direct, so the sandbox
/// introduces no host shell.
pub(crate) fn container_run_plan(
    plan: &ShellPlan,
    config: &ContainerConfig,
    runtime: ContainerRuntime,
    cwd: &Utf8Path,
    project_root: &Utf8Path,
) -> Result<ShellPlan, ToolError> {
    if !cwd.starts_with(project_root) {
        return Err(ToolError::PolicyDenied { rule: "sandbox_containerized_unenforceable".into() });
    }

    let mut args = vec!["run".to_string(), "--rm".to_string(), "--init".to_string()];
    args.push("--network".to_string());
    args.push(if config.network { "bridge" } else { "none" }.to_string());
    args.push("-w".to_string());
    args.push(cwd.as_str().to_string());
    args.push("-v".to_string());
    args.push(format!("{root}:{root}", root = project_root.as_str()));
    for mount in &config.extra_mounts {
        args.push("-v".to_string());
        args.push(mount.clone());
    }
    for (key, value) in &config.env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(config.image.clone());
    args.extend(inner_argv(plan));

    Ok(ShellPlan::Direct { program: runtime.binary().to_string(), args })
}

/// Apply container isolation to `plan` when configured, detecting a runtime
/// from the live process `PATH`.
///
/// Fail-closed: `Containerized` on an unsupported platform or with no detected
/// runtime is refused, never passed through unconfined.
pub(crate) fn containerize(
    plan: &ShellPlan,
    config: &ContainerConfig,
    cwd: &Utf8Path,
    project_root: &Utf8Path,
) -> Result<ShellPlan, ToolError> {
    containerize_with(plan, config, detect_container_runtime(), cwd, project_root)
}

/// Availability-injected core of [`containerize`] (ADR-72 §3).
///
/// The fail-closed matrix is identical; only the detection input is supplied by
/// the caller. This is the seam the shell tool injects its
/// [`ContainerRuntimeProbe`](concerto_core::sandbox::ContainerRuntimeProbe)
/// through, so absent/malformed/found detection is unit-testable without a
/// container runtime installed. An explicit [`ContainerConfig::runtime`]
/// override takes precedence over `availability`.
pub(crate) fn containerize_with(
    plan: &ShellPlan,
    config: &ContainerConfig,
    availability: RuntimeAvailability,
    cwd: &Utf8Path,
    project_root: &Utf8Path,
) -> Result<ShellPlan, ToolError> {
    // Windows has no POSIX shell inside the default images and Job Object
    // isolation needs an unsafe FFI dependency this workspace denies
    // (ADR-72 §5), so v1 refuses rather than mis-executes — even for an
    // explicit runtime override, because the invocation shape is unsupported.
    if cfg!(windows) {
        return Err(ToolError::PolicyDenied {
            rule: "sandbox_containerized_unsupported_platform".into(),
        });
    }
    let runtime = match config.runtime {
        Some(runtime) => runtime,
        None => availability.runtime().ok_or_else(runtime_unavailable)?,
    };
    container_run_plan(plan, config, runtime, cwd, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (camino::Utf8PathBuf, camino::Utf8PathBuf) {
        (camino::Utf8PathBuf::from("/work/project"), camino::Utf8PathBuf::from("/work/project/sub"))
    }

    fn wrapped() -> ShellPlan {
        ShellPlan::Wrapped {
            program: "/bin/sh".into(),
            switches: vec!["-c".into()],
            // Row 45's CPU prelude embedded in the operand, as `with_cpu_limit`
            // would produce it.
            operand: "ulimit -S -t 5; echo hello".into(),
            verbatim: false,
        }
    }

    #[test]
    fn container_run_plan_mounts_project_and_sets_cwd() {
        let (root, cwd) = paths();
        let cfg = ContainerConfig::new("debian:bookworm-slim");
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("container plan");
        let ShellPlan::Direct { program, args } = plan else {
            panic!("container plan must be argv-direct");
        };
        assert_eq!(program, "docker");
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_string()));
        let mount = format!("{root}:{root}");
        let v_index = args.iter().position(|a| a == "-v").expect("-v");
        assert_eq!(args[v_index + 1], mount);
        let w_index = args.iter().position(|a| a == "-w").expect("-w");
        assert_eq!(args[w_index + 1], cwd.as_str());
        // Image immediately precedes the inner command.
        let image_index = args.iter().position(|a| a == "debian:bookworm-slim").expect("image");
        assert_eq!(args[image_index + 1], "/bin/sh");
    }

    #[test]
    fn container_run_plan_composes_with_row_42_and_45() {
        let (root, cwd) = paths();
        let cfg = ContainerConfig::new("alpine:3");
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Podman, &cwd, &root)
            .expect("plan");
        let ShellPlan::Direct { program, args } = plan else {
            panic!("argv-direct");
        };
        assert_eq!(program, "podman");
        // Row 42: shell + switches + operand are the inner command, verbatim.
        let image_index = args.iter().position(|a| a == "alpine:3").expect("image");
        assert_eq!(&args[image_index + 1..], &["/bin/sh", "-c", "ulimit -S -t 5; echo hello"]);
        // Row 45: the CPU prelude is carried inside, not duplicated by the
        // container runtime (no `--ulimit` / `--cpus` invented here).
        assert!(!args.iter().any(|a| a == "--ulimit" || a == "--cpus"));
    }

    #[test]
    fn container_run_plan_preserves_argv_direct_plans() {
        let (root, cwd) = paths();
        let cfg = ContainerConfig::new("alpine:3");
        let direct = ShellPlan::Direct {
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
        };
        let plan =
            container_run_plan(&direct, &cfg, ContainerRuntime::Docker, &cwd, &root).expect("plan");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        let image_index = args.iter().position(|a| a == "alpine:3").expect("image");
        assert_eq!(&args[image_index + 1..], &["cargo", "test", "--workspace"]);
    }

    #[test]
    fn container_run_plan_defaults_to_no_network() {
        let (root, cwd) = paths();
        let cfg = ContainerConfig::new("alpine:3");
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("plan");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        let net = args.iter().position(|a| a == "--network").expect("--network");
        assert_eq!(args[net + 1], "none");
    }

    #[test]
    fn container_run_plan_passes_only_configured_env() {
        let (root, cwd) = paths();
        let mut cfg = ContainerConfig::new("alpine:3");
        cfg.env.push(("FOO".into(), "bar".into()));
        cfg.network = true;
        cfg.extra_mounts.push("/cache:/cache:ro".into());
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("plan");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        let e = args.iter().position(|a| a == "-e").expect("-e");
        assert_eq!(args[e + 1], "FOO=bar");
        let extra = args.windows(2).any(|w| w[0] == "-v" && w[1] == "/cache:/cache:ro");
        assert!(extra, "extra mount missing");
        let net = args.iter().position(|a| a == "--network").expect("--network");
        assert_eq!(args[net + 1], "bridge");
        // Host PATH etc. must never be forwarded wholesale.
        assert!(!args.iter().any(|a| a.starts_with("PATH=")));
    }

    #[test]
    fn container_run_plan_refuses_cwd_outside_project_root() {
        let (root, _) = paths();
        let outside = camino::Utf8PathBuf::from("/tmp/elsewhere");
        let cfg = ContainerConfig::new("alpine:3");
        let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &outside, &root)
            .expect_err("out-of-root cwd must be refused");
        assert!(
            matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_unenforceable")
        );
    }

    #[test]
    fn containerize_refuses_without_runtime() {
        // ADR-72 §2/§3: an absent detected runtime is an explicit refusal,
        // never a silent fallback to the unconfined inner plan. Detection is
        // injected so this is deterministic and never requires a runtime.
        #[cfg(not(windows))]
        {
            let (root, cwd) = paths();
            let cfg = ContainerConfig::new("alpine:3");
            let err = containerize_with(
                &wrapped(),
                &cfg,
                RuntimeAvailability::Unavailable { reason: "test: no runtime on PATH".into() },
                &cwd,
                &root,
            )
            .expect_err("absent runtime must refuse");
            assert!(
                matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_runtime_unavailable")
            );
        }
        // On Windows the invocation shape is unsupported and refused first
        // (ADR-72 §5), independent of detection.
        #[cfg(windows)]
        {
            let (root, cwd) = paths();
            let cfg = ContainerConfig::new("alpine:3");
            let err = containerize_with(
                &wrapped(),
                &cfg,
                RuntimeAvailability::Available(ContainerRuntime::Docker),
                &cwd,
                &root,
            )
            .expect_err("windows must refuse");
            assert!(
                matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_unsupported_platform")
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn containerize_uses_detected_runtime_when_unset() {
        let (root, cwd) = paths();
        let cfg = ContainerConfig::new("alpine:3");
        let plan = containerize_with(
            &wrapped(),
            &cfg,
            RuntimeAvailability::Available(ContainerRuntime::Podman),
            &cwd,
            &root,
        )
        .expect("an available runtime must route");
        let ShellPlan::Direct { program, .. } = plan else { panic!("argv-direct") };
        assert_eq!(program, "podman");
    }

    #[cfg(not(windows))]
    #[test]
    fn containerize_explicit_runtime_overrides_absent_detection() {
        let (root, cwd) = paths();
        let mut cfg = ContainerConfig::new("alpine:3");
        cfg.runtime = Some(ContainerRuntime::Docker);
        let plan = containerize_with(
            &wrapped(),
            &cfg,
            RuntimeAvailability::Unavailable { reason: "test: no runtime".into() },
            &cwd,
            &root,
        )
        .expect("explicit runtime override must route");
        let ShellPlan::Direct { program, .. } = plan else { panic!("argv-direct") };
        assert_eq!(program, "docker");
    }

    #[cfg(not(windows))]
    #[test]
    fn containerize_refuses_out_of_root_cwd_under_available_runtime() {
        // Independent of detection: an out-of-root cwd is always refused.
        let (root, _) = paths();
        let outside = camino::Utf8PathBuf::from("/");
        let cfg = ContainerConfig::new("alpine:3");
        let err = containerize_with(
            &wrapped(),
            &cfg,
            RuntimeAvailability::Available(ContainerRuntime::Docker),
            &outside,
            &root,
        )
        .expect_err("must refuse");
        assert!(
            matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_unenforceable")
        );
    }
}
