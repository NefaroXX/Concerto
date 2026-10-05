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
//! - Extra mounts and forwarded environment entries are **operator-supplied**
//!   via [`ContainerConfig`] (programmatic-only today — there is no TOML key).
//!   They are validated here as defense-in-depth: an escaping mount, a
//!   conflicting access mode, or a security-sensitive env override is refused
//!   with a named error rather than passed to the runtime.
//!
//! Fail-closed: no runtime, an unsupported platform (Windows), an
//! out-of-root working directory, or an invalid mount/env entry all produce an
//! explicit error, never a silent fallback to unconfined execution.

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

/// Policy refusal with a named rule, for an invalid supplied entry.
fn invalid_entry(rule: &str) -> ToolError {
    ToolError::PolicyDenied { rule: rule.to_string() }
}

/// Environment variable names treated as security-sensitive: an operator-
/// supplied `-e` that overrides one of these could subvert the container's
/// loader/path resolution. Refused unless [`ContainerConfig::allow_sensitive_env`]
/// is explicitly set.
const SENSITIVE_ENV_VARS: [&str; 3] = ["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

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
    /// Explicit opt-in for [`ContainerConfig::env`] entries that override a
    /// security-sensitive name ([`SENSITIVE_ENV_VARS`]). Defaults to `false`,
    /// so the safe behaviour is fail-closed.
    pub allow_sensitive_env: bool,
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
            allow_sensitive_env: false,
        }
    }
}

/// Validate every operator-supplied mount and env entry.
///
/// Called before any argv is built, so an invalid entry is a named refusal
/// rather than a silently dropped argument. See the module docs for why this
/// exists (programmatic-only inputs, defense-in-depth).
fn validate_entries(config: &ContainerConfig, project_root: &Utf8Path) -> Result<(), ToolError> {
    for mount in &config.extra_mounts {
        validate_mount(mount, project_root)?;
    }
    for (key, value) in &config.env {
        validate_env(key, value, config.allow_sensitive_env)?;
    }
    Ok(())
}

/// Validate one `-v source:dest[:options]` entry.
///
/// - `source` is a host path that must resolve inside `project_root`.
/// - `dest` is an absolute container path that must not shadow or overlap the
///   project-root mount.
/// - Extra mounts are read-only: only the project root is writable, so an
///   explicit `rw` (or `ro` combined with `rw`) conflicts with that policy.
fn validate_mount(mount: &str, project_root: &Utf8Path) -> Result<(), ToolError> {
    if mount.trim().is_empty() {
        return Err(invalid_entry("sandbox_containerized_mount_empty"));
    }
    let parts: Vec<&str> = mount.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(invalid_entry("sandbox_containerized_mount_malformed"));
    }
    let source = parts[0];
    let dest = parts[1];
    if source.is_empty() || dest.is_empty() || !dest.starts_with('/') {
        return Err(invalid_entry("sandbox_containerized_mount_malformed"));
    }

    // Extra mounts must stay read-only; `rw` would add a writable path beyond
    // the project root. `ro,rw` is contradictory and also refused.
    if let Some(options) = parts.get(2) {
        if options.split(',').any(|option| option.trim() == "rw") {
            return Err(invalid_entry("sandbox_containerized_mount_access_mode_conflict"));
        }
    }

    let dest_norm = normalize_lexically(Utf8Path::new(dest));
    let root_norm = normalize_lexically(project_root);
    if dest_norm == root_norm || root_norm.starts_with(&dest_norm) {
        return Err(invalid_entry("sandbox_containerized_mount_shadows_project_root"));
    }

    let source_path = Utf8Path::new(source);
    let resolved = if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        project_root.join(source_path)
    };
    let resolved_norm = normalize_lexically(&resolved);
    if !resolved_norm.starts_with(&root_norm) {
        return Err(invalid_entry("sandbox_containerized_mount_escapes_project_root"));
    }
    // Defense in depth: when the paths exist, the canonical (symlink-resolved)
    // source must also stay inside the canonical root.
    if let (Ok(real_source), Ok(real_root)) =
        (std::fs::canonicalize(&resolved), std::fs::canonicalize(project_root))
    {
        if !real_source.starts_with(&real_root) {
            return Err(invalid_entry("sandbox_containerized_mount_escapes_project_root"));
        }
    }
    Ok(())
}

/// Validate one `-e KEY=VALUE` entry.
fn validate_env(key: &str, value: &str, allow_sensitive: bool) -> Result<(), ToolError> {
    if key.is_empty() || value.is_empty() {
        return Err(invalid_entry("sandbox_containerized_env_empty"));
    }
    let is_control =
        |text: &str| text.bytes().any(|byte| byte == b'\n' || byte == b'\r' || byte == b'\0');
    // A key carrying `=` would shift the `KEY=VALUE` split inside the runtime.
    if is_control(key) || is_control(value) || key.contains('=') {
        return Err(invalid_entry("sandbox_containerized_env_control_char"));
    }
    if !allow_sensitive && SENSITIVE_ENV_VARS.contains(&key.to_ascii_uppercase().as_str()) {
        return Err(invalid_entry("sandbox_containerized_env_sensitive_override"));
    }
    Ok(())
}

/// Resolve `.` and `..` components lexically, without touching the filesystem
/// (the path may not exist yet). Used for containment checks.
fn normalize_lexically(path: &Utf8Path) -> camino::Utf8PathBuf {
    use camino::Utf8Component;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components.into_iter().collect()
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
///
/// Both `cwd` and `project_root` are canonicalized before the containment test
/// so a symlink inside the project root that points outside it cannot pass a
/// purely lexical `starts_with`. Resolution failure is fail-closed (a named
/// refusal), never a silent fallback.
pub(crate) fn container_run_plan(
    plan: &ShellPlan,
    config: &ContainerConfig,
    runtime: ContainerRuntime,
    cwd: &Utf8Path,
    project_root: &Utf8Path,
) -> Result<ShellPlan, ToolError> {
    // Resolve symlinks on both sides; a path that cannot be resolved is
    // refused (the plan cannot be proven to run inside the mount).
    let cwd = cwd.canonicalize_utf8().map_err(|_| ToolError::PolicyDenied {
        rule: "sandbox_containerized_cwd_unresolvable".into(),
    })?;
    let project_root = project_root.canonicalize_utf8().map_err(|_| ToolError::PolicyDenied {
        rule: "sandbox_containerized_cwd_unresolvable".into(),
    })?;
    if !cwd.starts_with(&project_root) {
        return Err(ToolError::PolicyDenied { rule: "sandbox_containerized_unenforceable".into() });
    }
    validate_entries(config, &project_root)?;

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

    /// A real project root (canonicalized) plus a real subdirectory, so the
    /// symlink-resolving containment check sees paths that exist. The returned
    /// `TempDir` must be held for the test's lifetime.
    fn paths() -> (tempfile::TempDir, camino::Utf8PathBuf, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(dir.path()).expect("canonical tempdir"),
        )
        .expect("utf8 root");
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).expect("create subdir");
        (dir, root, cwd)
    }

    /// A real directory outside any test project root, for containment tests.
    fn outside_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("outside tempdir")
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

    /// A mount `source` under the project root, spelled in the form the
    /// colon-delimited `src:dst[:opts]` grammar can carry on this platform.
    ///
    /// Off Windows the absolute spelling `{root}/{tail}` is used, exactly as a
    /// caller would write it. On Windows it never can be: the canonicalized
    /// root is a drive path (`\\?\C:\...` after `std::fs::canonicalize`), and
    /// its own colons split the mount into too many parts, so `validate_mount`
    /// would report `sandbox_containerized_mount_malformed` before the rule
    /// under test is evaluated. A relative `source` is legal — `validate_mount`
    /// resolves it against the project root — so the containment rule the test
    /// is about still runs, fail-closed, on both platforms.
    fn mount_source(root: &Utf8Path, tail: &str) -> String {
        if cfg!(windows) {
            tail.to_string()
        } else {
            format!("{root}/{tail}")
        }
    }

    #[test]
    fn container_run_plan_mounts_project_and_sets_cwd() {
        let (_dir, root, cwd) = paths();
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
        let (_dir, root, cwd) = paths();
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
        let (_dir, root, cwd) = paths();
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
        let (_dir, root, cwd) = paths();
        let cfg = ContainerConfig::new("alpine:3");
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("plan");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        let net = args.iter().position(|a| a == "--network").expect("--network");
        assert_eq!(args[net + 1], "none");
    }

    #[test]
    fn container_run_plan_passes_only_configured_env() {
        let (_dir, root, cwd) = paths();
        let mut cfg = ContainerConfig::new("alpine:3");
        cfg.env.push(("FOO".into(), "bar".into()));
        cfg.network = true;
        let mount = format!("{}:/cache:ro", mount_source(&root, "cache"));
        cfg.extra_mounts.push(mount.clone());
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("plan");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        let e = args.iter().position(|a| a == "-e").expect("-e");
        assert_eq!(args[e + 1], "FOO=bar");
        let extra = args.windows(2).any(|w| w[0] == "-v" && w[1] == mount);
        assert!(extra, "extra mount missing");
        let net = args.iter().position(|a| a == "--network").expect("--network");
        assert_eq!(args[net + 1], "bridge");
        // Host PATH etc. must never be forwarded wholesale.
        assert!(!args.iter().any(|a| a.starts_with("PATH=")));
    }

    #[test]
    fn container_config_rejects_empty_mount() {
        let (_dir, root, cwd) = paths();
        for mount in ["", "   "] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.extra_mounts.push(mount.into());
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("empty mount must be refused");
            assert!(matches!(
                err,
                ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_mount_empty"
            ));
        }
    }

    #[test]
    fn container_config_rejects_malformed_mount() {
        let (_dir, root, cwd) = paths();
        for mount in ["/only-a-source", "relative-dest", ":"] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.extra_mounts.push(mount.into());
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("malformed mount must be refused");
            assert!(matches!(
                err,
                ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_mount_malformed"
            ));
        }
        // A relative destination is malformed.
        let mut cfg = ContainerConfig::new("alpine:3");
        cfg.extra_mounts.push(format!("{root}/cache:relative"));
        let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect_err("relative destination must be refused");
        assert!(matches!(
            err,
            ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_mount_malformed"
        ));
    }

    #[test]
    fn container_config_rejects_mount_escaping_project_root() {
        let (_dir, root, cwd) = paths();
        // An absolute source anchored at the project root that climbs out of
        // it. `mount_source` spells it relatively on Windows, where that drive
        // path cannot be written in the `src:dst[:opts]` grammar at all (see
        // the helper); the refusal under test is the same either way.
        let escaped = format!("{}:/etc:ro", mount_source(&root, "../../etc"));
        for mount in ["/etc:/etc:ro", "../../etc:/etc:ro", escaped.as_str()] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.extra_mounts.push(mount.into());
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("escaping mount must be refused");
            assert!(
                matches!(
                    err,
                    ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_mount_escapes_project_root"
                ),
                "unexpected refusal for {mount:?}"
            );
        }
    }

    #[test]
    fn container_config_rejects_writable_extra_mount() {
        let (_dir, root, cwd) = paths();
        for mount in [
            format!("{}:/cache:rw", mount_source(&root, "cache")),
            format!("{}:/cache:ro,rw", mount_source(&root, "cache")),
        ] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.extra_mounts.push(mount);
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("writable extra mount must be refused");
            assert!(matches!(
                err,
                ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_mount_access_mode_conflict"
            ));
        }
    }

    #[test]
    fn container_config_rejects_mount_shadowing_project_root() {
        let (_dir, root, cwd) = paths();
        let parent = root.parent().expect("tempdir has a parent");
        for mount in [
            format!("{}:{root}:ro", mount_source(&root, "cache")),
            format!("{}:{parent}:ro", mount_source(&root, "cache")),
        ] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.extra_mounts.push(mount.clone());
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("shadowing mount must be refused");
            // The destination has to be the project root (or an ancestor of it)
            // to trip the shadow rule. On Windows that destination is a drive
            // path, whose colon cannot be written in the `src:dst[:opts]`
            // grammar at all, so `validate_mount` reports the string as
            // malformed before the shadow rule is evaluated. Still fail-closed,
            // still a refusal — only the named rule is platform-defined here.
            let expected = if cfg!(windows) {
                "sandbox_containerized_mount_malformed"
            } else {
                "sandbox_containerized_mount_shadows_project_root"
            };
            assert!(
                matches!(err, ToolError::PolicyDenied { ref rule } if rule == expected),
                "unexpected refusal for {mount:?}: {err:?}"
            );
        }
    }

    #[test]
    fn container_config_rejects_empty_env() {
        let (_dir, root, cwd) = paths();
        for entry in [("", "value"), ("KEY", "")] {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.env.push((entry.0.into(), entry.1.into()));
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("empty env entry must be refused");
            assert!(matches!(
                err,
                ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_env_empty"
            ));
        }
    }

    #[test]
    fn container_config_rejects_env_control_characters() {
        let (_dir, root, cwd) = paths();
        for entry in [("K\nEY", "value"), ("KEY", "va\nlue"), ("KEY", "va\0lue"), ("K=EY", "value")]
        {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.env.push((entry.0.into(), entry.1.into()));
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("env control characters must be refused");
            assert!(matches!(
                err,
                ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_env_control_char"
            ));
        }
    }

    #[test]
    fn container_config_rejects_sensitive_env_override() {
        let (_dir, root, cwd) = paths();
        for name in SENSITIVE_ENV_VARS {
            let mut cfg = ContainerConfig::new("alpine:3");
            cfg.env.push((name.into(), "/tmp/evil".into()));
            let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
                .expect_err("sensitive env override must be refused");
            assert!(
                matches!(
                    err,
                    ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_env_sensitive_override"
                ),
                "unexpected refusal for {name}"
            );
        }
    }

    #[test]
    fn container_config_allows_sensitive_env_when_explicitly_opted_in() {
        let (_dir, root, cwd) = paths();
        let mut cfg = ContainerConfig::new("alpine:3");
        cfg.allow_sensitive_env = true;
        cfg.env.push(("PATH".into(), "/custom/bin".into()));
        let plan = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &cwd, &root)
            .expect("explicit opt-in must allow the override");
        let ShellPlan::Direct { args, .. } = plan else { panic!("argv-direct") };
        assert!(args.windows(2).any(|w| w[0] == "-e" && w[1] == "PATH=/custom/bin"));
    }

    #[test]
    fn container_run_plan_refuses_cwd_outside_project_root() {
        let (_dir, root, _) = paths();
        let outside_tmp = outside_dir();
        let outside = camino::Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(outside_tmp.path()).expect("canonical outside"),
        )
        .expect("utf8 outside");
        let cfg = ContainerConfig::new("alpine:3");
        let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &outside, &root)
            .expect_err("out-of-root cwd must be refused");
        assert!(
            matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_unenforceable")
        );
    }

    /// A symlink inside the project root that resolves outside it must not pass
    /// the containment check: the lexical `starts_with` would accept it, so the
    /// canonicalized check refuses it.
    #[cfg(unix)]
    #[test]
    fn container_run_plan_refuses_symlink_cwd_escaping_project_root() {
        let (_dir, root, _) = paths();
        let outside_tmp = outside_dir();
        let link = root.join("escape");
        std::os::unix::fs::symlink(outside_tmp.path(), &link).expect("symlink");
        let cfg = ContainerConfig::new("alpine:3");
        let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &link, &root)
            .expect_err("symlink escape must be refused");
        assert!(
            matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_unenforceable"),
            "symlink-out must be refused as out-of-root: {err:?}"
        );
    }

    /// A working directory that cannot be resolved is fail-closed.
    #[test]
    fn container_run_plan_refuses_unresolvable_cwd() {
        let (_dir, root, _) = paths();
        let missing = root.join("does-not-exist");
        let cfg = ContainerConfig::new("alpine:3");
        let err = container_run_plan(&wrapped(), &cfg, ContainerRuntime::Docker, &missing, &root)
            .expect_err("unresolvable cwd must be refused");
        assert!(
            matches!(err, ToolError::PolicyDenied { ref rule } if rule == "sandbox_containerized_cwd_unresolvable"),
            "unresolvable cwd must use the named rule: {err:?}"
        );
    }

    #[test]
    fn containerize_refuses_without_runtime() {
        // ADR-72 §2/§3: an absent detected runtime is an explicit refusal,
        // never a silent fallback to the unconfined inner plan. Detection is
        // injected so this is deterministic and never requires a runtime.
        #[cfg(not(windows))]
        {
            let (_dir, root, cwd) = paths();
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
            let (_dir, root, cwd) = paths();
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
        let (_dir, root, cwd) = paths();
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
        let (_dir, root, cwd) = paths();
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
        let (_dir, root, _) = paths();
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
