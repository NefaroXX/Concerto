//! Shell profile and toolchain configuration (ADR-28).
//!
//! Defines configurable shell profiles and the single canonical selection used
//! by agent commands, validation, and the integrated Terminal (ADR-30). The
//! managed runtime is provided by [`crate::managed`]: it is installed under the
//! user's data dir, integrity-checked, and opt-in.

use crate::managed::ManagedRuntimeManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Backend kind a profile launches through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShellBackendType {
    /// A system-installed executable launched directly.
    #[default]
    System,
    /// A Concerto-managed runtime (bundled, integrity-checked, opt-in).
    Managed,
    /// A user-supplied custom executable.
    Custom,
}

/// Default working-directory behaviour when a shell starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkingDirBehavior {
    /// Always start in the active project root.
    #[default]
    ProjectRoot,
    /// Start in the user's home directory.
    Home,
    /// Do not override the working directory (shell's natural default).
    ShellDefault,
}

/// Availability status of a profile (populated by availability checks).
///
/// `Unknown` is the safe default for freshly loaded config; the UI and the
/// `Test profile` action promote it to `Available` / `Unavailable(reason)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProfileAvailability {
    #[default]
    Unknown,
    Available,
    Unavailable(String),
}

/// A single configurable shell profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellProfileConfig {
    /// Stable identifier, e.g. "system-default", "managed-bash".
    pub id: String,
    /// User-friendly display name.
    pub name: String,
    #[serde(default)]
    pub backend: ShellBackendType,
    /// Executable path or bare name resolved via `PATH`.
    pub executable: String,
    /// Arguments passed before any command/startup flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Explicit environment additions, merged over the base environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Directories prepended to `PATH` for this profile.
    #[serde(default)]
    pub path_additions: Vec<PathBuf>,
    /// Optional startup script sourced/run at launch (backend-specific).
    #[serde(default)]
    pub startup_script: Option<PathBuf>,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default)]
    pub default_working_dir: WorkingDirBehavior,
    /// Last-known availability; `Unknown` until checked.
    #[serde(default)]
    pub status: ProfileAvailability,
}

fn default_encoding() -> String {
    "utf-8".into()
}

impl Default for ShellProfileConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            backend: ShellBackendType::System,
            executable: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            path_additions: Vec::new(),
            startup_script: None,
            login: false,
            interactive: false,
            encoding: default_encoding(),
            default_working_dir: WorkingDirBehavior::ProjectRoot,
            status: ProfileAvailability::Unknown,
        }
    }
}

impl ShellProfileConfig {
    /// Effective environment for spawning: `base` with `env` overrides applied
    /// and `path_additions` prepended to `PATH`.
    pub fn effective_env(&self, base: &HashMap<String, String>) -> HashMap<String, String> {
        let mut out = base.clone();
        for (k, v) in &self.env {
            out.insert(k.clone(), v.clone());
        }
        if !self.path_additions.is_empty() {
            let mut parts = self.path_additions.clone();
            if let Some(existing) = out.get("PATH") {
                parts.extend(std::env::split_paths(OsStr::new(existing)));
            }
            if let Ok(path) = std::env::join_paths(parts) {
                out.insert("PATH".to_string(), path.to_string_lossy().into_owned());
            }
        }
        out
    }

    /// Resolve the working directory for this profile.
    pub fn resolve_working_dir(&self, project_root: &Path, home: Option<&Path>) -> Option<PathBuf> {
        match self.default_working_dir {
            WorkingDirBehavior::ProjectRoot => Some(project_root.to_path_buf()),
            WorkingDirBehavior::Home => home.map(|h| h.to_path_buf()),
            WorkingDirBehavior::ShellDefault => None,
        }
    }

    /// Arguments used to launch this profile *interactively* (Terminal page).
    ///
    /// Combines the user-supplied `args`, login/interactive flags, and a
    /// backend-appropriate startup-script flag. Startup-script semantics are
    /// shell-specific; we cover the common POSIX and PowerShell forms and fall
    /// back to a positional argument otherwise.
    pub fn interactive_launch_args(&self) -> Vec<String> {
        let mut args = self.args.clone();
        if self.login {
            args.push("-l".into());
        }
        if self.interactive {
            args.push("-i".into());
        }
        if let Some(script) = &self.startup_script {
            let path = script.to_string_lossy().into_owned();
            let base = self.executable_base();
            if base.contains("pwsh") || base.contains("powershell") {
                args.push("-File".into());
                args.push(path);
            } else if self.interactive {
                // bash/zsh/sh honour --rcfile only for interactive shells.
                args.push("--rcfile".into());
                args.push(path);
            } else {
                args.push(path);
            }
        }
        args
    }

    /// Arguments used to run a single `command` through this profile (agent
    /// shell tool). The agent always passes a command; we append the correct
    /// "execute this string" flag for the shell family.
    pub fn command_args(&self, command: &str) -> Vec<String> {
        let mut args = self.args.clone();
        let base = self.executable_base();
        if base.contains("pwsh") || base.contains("powershell") {
            args.push("-Command".into());
        } else if base == "cmd" || base == "cmd.exe" {
            args.push("/C".into());
        } else {
            // sh, bash, zsh, fish, nu, and most POSIX-ish shells use -c.
            args.push("-c".into());
        }
        args.push(command.to_string());
        args
    }

    fn executable_base(&self) -> String {
        let base =
            self.executable.rsplit(std::path::MAIN_SEPARATOR).next().unwrap_or(&self.executable);
        base.to_lowercase()
    }

    /// Resolve the executable to an absolute path when possible.
    ///
    /// For the [`ShellBackendType::Managed`] backend this returns the path of
    /// the installed managed Bash if present, otherwise a `managed-bash`
    /// sentinel so callers can surface a clear "not installed" error. For other
    /// backends an absolute or path-qualified `executable` is returned as-is; a
    /// bare name is searched against `PATH`, and if no candidate is found we
    /// return the bare name and let the OS resolver decide at spawn time.
    /// `resolve_executable` keeps the profile self-contained so spawners do not
    /// re-implement PATH search (ADR-28).
    pub fn resolve_executable(&self) -> PathBuf {
        if self.backend == ShellBackendType::Managed {
            if let Some(manifest) = ManagedRuntimeManager::auto_detect() {
                return manifest.bash_executable;
            }
            return PathBuf::from("managed-bash");
        }
        if self.executable.is_empty() {
            return PathBuf::from("/bin/sh");
        }
        let path = Path::new(&self.executable);
        if path.is_absolute() || self.executable.contains(std::path::MAIN_SEPARATOR) {
            return path.to_path_buf();
        }
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join(&self.executable);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        PathBuf::from(&self.executable)
    }

    /// Cheap, side-effect-free availability probe (ADR-28 Slice 1).
    ///
    /// Returns [`ProfileAvailability::Available`] only when the resolved
    /// executable exists on the filesystem; otherwise a human-readable reason.
    /// For the [`ShellBackendType::Managed`] backend this reports availability
    /// based on whether a managed runtime has been installed
    /// (`ManagedRuntimeManager::auto_detect`).
    pub fn availability(&self) -> ProfileAvailability {
        if self.backend == ShellBackendType::Managed {
            return match ManagedRuntimeManager::auto_detect() {
                Some(_) => ProfileAvailability::Available,
                None => ProfileAvailability::Unavailable(
                    "Concerto Managed Bash is not installed. Install it from Settings → Terminal (ADR-28 Slice 2).".into(),
                ),
            };
        }
        if self.executable.trim().is_empty() {
            return ProfileAvailability::Unavailable("no executable configured".into());
        }
        match self.find_executable() {
            Some(_) => ProfileAvailability::Available,
            None => ProfileAvailability::Unavailable(format!(
                "executable not found on PATH or filesystem: {}",
                self.executable
            )),
        }
    }

    /// Resolve to an existing path, returning `None` when no candidate is found.
    /// Used by [`ShellProfileConfig::availability`] and
    /// [`ShellProfileConfig::version_string`].
    fn find_executable(&self) -> Option<PathBuf> {
        let path = Path::new(&self.executable);
        if path.is_absolute() || self.executable.contains(std::path::MAIN_SEPARATOR) {
            return if path.exists() { Some(path.to_path_buf()) } else { None };
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths).find_map(|dir| {
                let candidate = dir.join(&self.executable);
                if candidate.is_file() {
                    Some(candidate)
                } else {
                    None
                }
            })
        })
    }

    /// Best-effort shell version string (e.g. `GNU bash, version 5.2.15`).
    ///
    /// For the [`ShellBackendType::Managed`] backend this returns the version
    /// recorded at install time. For other backends it probes
    /// `<exe> --version` then `-V`, returning the first non-empty output line.
    /// The probe is bounded by a short timeout on a background thread so a
    /// broken or hung executable can never stall the caller. Returns `None`
    /// when the executable is unavailable or the probe fails.
    pub fn version_string(&self) -> Option<String> {
        if self.backend == ShellBackendType::Managed {
            return ManagedRuntimeManager::auto_detect().map(|m| m.bash_version);
        }
        let exe = self.find_executable()?;
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        let _ = std::thread::spawn(move || {
            let probe = || -> Option<String> {
                for flag in ["--version", "-V", "--Version"] {
                    if let Ok(out) = std::process::Command::new(&exe).arg(flag).output() {
                        let stdout_line = String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(str::to_string);
                        if let Some(line) = stdout_line {
                            return Some(line);
                        }
                        if out.status.success() {
                            let stderr_line = String::from_utf8_lossy(&out.stderr)
                                .lines()
                                .map(str::trim)
                                .find(|line| !line.is_empty())
                                .map(str::to_string);
                            if stderr_line.is_some() {
                                return stderr_line;
                            }
                        }
                    }
                }
                None
            };
            let _ = tx.send(probe());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok().flatten()
    }
}

/// Legacy shell bindings accepted while loading pre-unification configs.
///
/// New configs persist a single `selected_profile`. The former agent binding
/// wins during migration because agent execution is the primary consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct LegacyShellDefaultBindings {
    #[serde(default = "default_binding_system", rename = "interactive_terminal")]
    _interactive_terminal: String,
    #[serde(default = "default_binding_system")]
    agent_shell: String,
    #[serde(default = "default_binding_system", rename = "build_validation")]
    _build_validation: String,
}

fn default_binding_system() -> String {
    "system-default".into()
}

/// Concerto-managed runtime configuration (ADR-28 Slice 2). Populated when a
/// managed environment is installed; never modifies global PATH/registry/shell
/// config. The live runtime is discovered via [`crate::managed`]; this mirrors
/// that state in config so the UI can show the current installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ManagedEnvConfig {
    pub install_dir: Option<PathBuf>,
    pub version: Option<String>,
    pub runtime_manifest: Option<PathBuf>,
    pub tool_manifest: Option<PathBuf>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub integrity_enabled: bool,
}

/// Top-level shell settings section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellSettings {
    #[serde(default)]
    pub profiles: Vec<ShellProfileConfig>,
    /// The one profile used by agents, validation, and the integrated terminal.
    #[serde(default)]
    pub selected_profile: String,
    /// Read old configs without perpetuating independently drifting bindings.
    #[serde(default, rename = "defaults", skip_serializing)]
    legacy_defaults: Option<LegacyShellDefaultBindings>,
    /// Managed environment configuration; `None` until installed.
    #[serde(default)]
    pub managed: Option<ManagedEnvConfig>,
}

impl Default for ShellSettings {
    fn default() -> Self {
        default_shell_settings()
    }
}

impl ShellSettings {
    /// Construct canonical settings with one selected shell profile.
    pub fn new(
        profiles: Vec<ShellProfileConfig>,
        selected_profile: String,
        managed: Option<ManagedEnvConfig>,
    ) -> Self {
        Self { profiles, selected_profile, legacy_defaults: None, managed }
    }

    /// Resolve a profile by id.
    pub fn profile(&self, id: &str) -> Option<&ShellProfileConfig> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The canonical selected profile id.
    ///
    /// Old three-binding configs migrate from `agent_shell`. Invalid or empty
    /// selections fall back deterministically so agent execution is not left
    /// without a shell after a profile is removed.
    pub fn selected_profile_id(&self) -> &str {
        let configured = if self.selected_profile.trim().is_empty() {
            self.legacy_defaults
                .as_ref()
                .map(|defaults| defaults.agent_shell.as_str())
                .unwrap_or("")
        } else {
            self.selected_profile.as_str()
        };
        if self.profile(configured).is_some() {
            configured
        } else if self.profile("system-default").is_some() {
            "system-default"
        } else {
            self.profiles.first().map(|profile| profile.id.as_str()).unwrap_or(configured)
        }
    }

    /// The one profile used by every process-launching Concerto surface.
    pub fn selected_profile(&self) -> Option<&ShellProfileConfig> {
        self.profile(self.selected_profile_id())
    }

    /// Refresh host-detected shells while preserving user-created profiles.
    ///
    /// This also removes the old placeholder presets, which advertised shells
    /// that were not necessarily installed and duplicated the discovery list.
    pub fn normalized_for_host(self) -> Self {
        self.normalized_with_detected(discover_os_shells())
    }

    fn normalized_with_detected(mut self, detected: Vec<ShellProfileConfig>) -> Self {
        let selected = self.selected_profile_id().to_owned();
        let mut existing = std::mem::take(&mut self.profiles)
            .into_iter()
            .filter(|profile| !is_legacy_preset(&profile.id))
            .collect::<Vec<_>>();
        let mut refreshed = Vec::with_capacity(detected.len() + existing.len());
        for detected_profile in detected {
            let existing_index = existing.iter().position(|profile| {
                profile.id == detected_profile.id
                    || (profile.id.starts_with("os-")
                        && profile.resolve_executable() == detected_profile.resolve_executable())
            });
            if let Some(index) = existing_index {
                let mut profile = existing.remove(index);
                profile.id = detected_profile.id;
                profile.name = detected_profile.name;
                profile.executable = detected_profile.executable;
                profile.status = detected_profile.status;
                refreshed.push(profile);
            } else {
                refreshed.push(detected_profile);
            }
        }
        refreshed.extend(existing.into_iter().filter(|profile| !profile.id.starts_with("os-")));
        self.profiles = refreshed;
        self.selected_profile = if self.profile(&selected).is_some() {
            selected
        } else {
            preferred_detected_profile_id(&self.profiles).unwrap_or_default()
        };
        self.legacy_defaults = None;
        self
    }
}

/// Default shell settings contain only shells detected on the current host.
pub fn default_shell_settings() -> ShellSettings {
    let profiles = default_shell_profiles();
    let selected_profile = preferred_detected_profile_id(&profiles).unwrap_or_default();
    ShellSettings::new(profiles, selected_profile, None)
}

/// Shell profiles detected on the current host.
pub fn default_shell_profiles() -> Vec<ShellProfileConfig> {
    discover_os_shells()
}

fn is_legacy_preset(id: &str) -> bool {
    matches!(
        id,
        "system-default"
            | "managed-bash"
            | "bash"
            | "msys2-bash"
            | "git-bash"
            | "powershell"
            | "cmd"
            | "zsh"
            | "fish"
            | "nushell"
            | "custom"
    )
}

fn preferred_detected_profile_id(profiles: &[ShellProfileConfig]) -> Option<String> {
    #[cfg(windows)]
    if let Some(profile) = profiles.iter().find(|profile| profile.id == "os-comspec") {
        return Some(profile.id.clone());
    }

    #[cfg(unix)]
    if let Some(login_shell) = std::env::var_os("SHELL") {
        let login_shell = Path::new(&login_shell);
        if let Some(profile) =
            profiles.iter().find(|profile| Path::new(&profile.executable) == login_shell)
        {
            return Some(profile.id.clone());
        }
    }

    profiles.first().map(|profile| profile.id.clone())
}

/// Known shells we look for when discovering what the OS has installed, with a
/// human-friendly display name and whether the shell is interactive by default.
const KNOWN_SHELLS: &[(&str, &str, bool)] = &[
    ("bash", "Bash", true),
    ("zsh", "Zsh", true),
    ("fish", "Fish", true),
    ("sh", "Bourne shell (sh)", false),
    ("dash", "Debian Almquist shell (dash)", false),
    ("ksh", "KornShell (ksh)", true),
    ("tcsh", "TC Shell (tcsh)", true),
    ("csh", "C Shell (csh)", true),
    ("pwsh", "PowerShell", true),
    ("powershell", "PowerShell (Windows)", true),
    ("nu", "Nushell", true),
    ("elvish", "Elvish", true),
    ("ion", "Ion", true),
    ("oil", "Oil", true),
    ("cmd", "Command Prompt (cmd)", false),
];

/// Resolve a bare program name against `PATH`, returning the first existing
/// executable. Absolute or path-qualified programs are returned as-is when they
/// exist. Mirrors the spawner's PATH search so discovery matches what will
/// actually run (ADR-28).
fn resolve_in_path_with(program: &str, paths: Option<&OsStr>) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || program.contains(std::path::MAIN_SEPARATOR) {
        return if path.exists() { Some(path.to_path_buf()) } else { None };
    }
    paths.and_then(|paths| {
        std::env::split_paths(paths).find_map(|dir| {
            let candidate = dir.join(program);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

fn resolve_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH");
    resolve_in_path_with(program, paths.as_deref())
}

/// Parse an `/etc/shells`-style file, returning one profile per non-comment,
/// existing login-shell path. Pure and testable (touches no filesystem itself).
#[cfg(unix)]
fn parse_etc_shells(contents: &str) -> Vec<ShellProfileConfig> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p = Path::new(line);
        if !p.exists() {
            continue;
        }
        let base = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let (_, name, interactive) =
            KNOWN_SHELLS.iter().find(|(n, _, _)| *n == base).copied().unwrap_or((base, base, true));
        out.push(ShellProfileConfig {
            id: format!("os-{base}"),
            name: name.to_string(),
            backend: ShellBackendType::System,
            executable: line.to_string(),
            interactive,
            status: ProfileAvailability::Available,
            ..Default::default()
        });
    }
    out
}

/// Discover shells installed on the host OS.
///
/// Concerto does **not** bundle or integrate shells into the program (ADR-28
/// Slice 3 is deferred behind a licensing review). Instead this surfaces the
/// shells the OS already provides so the user can choose among them.
///
/// * Unix: parses `/etc/shells` and scans `PATH` for [`KNOWN_SHELLS`],
///   deduplicated by resolved executable path.
/// * Windows: scans `PATH` for `pwsh`/`powershell`/`nu`, honours `%COMSPEC%`,
///   and detects WSL (`wsl.exe`) and Git Bash (`bash.exe`).
///
/// Each returned profile uses the `System` backend and is marked
/// [`ProfileAvailability::Available`] because discovery confirmed presence.
pub fn discover_os_shells() -> Vec<ShellProfileConfig> {
    let mut found: Vec<ShellProfileConfig> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut insert = |profile: ShellProfileConfig| {
        if seen.insert(profile.resolve_executable()) {
            found.push(profile);
        }
    };

    // 1) /etc/shells (Unix login shells).
    #[cfg(unix)]
    {
        if let Ok(contents) = std::fs::read_to_string("/etc/shells") {
            for profile in parse_etc_shells(&contents) {
                insert(profile);
            }
        }
    }

    // 2) Known shells present on PATH.
    for (name, disp, interactive) in KNOWN_SHELLS {
        if let Some(resolved) = resolve_in_path(name) {
            insert(ShellProfileConfig {
                id: format!("os-{name}"),
                name: (*disp).to_string(),
                backend: ShellBackendType::System,
                executable: resolved.to_string_lossy().into_owned(),
                interactive: *interactive,
                status: ProfileAvailability::Available,
                ..Default::default()
            });
        }
    }

    // 3) Windows-specific extras.
    #[cfg(windows)]
    {
        if let Ok(comspec) = std::env::var("COMSPEC") {
            let comspec = comspec.trim();
            if !comspec.is_empty() {
                insert(ShellProfileConfig {
                    id: "os-comspec".into(),
                    name: "Command Prompt (COMSPEC)".into(),
                    backend: ShellBackendType::System,
                    executable: comspec.to_string(),
                    interactive: false,
                    status: ProfileAvailability::Available,
                    ..Default::default()
                });
            }
        }
        if resolve_in_path("wsl.exe").is_some() {
            insert(ShellProfileConfig {
                id: "os-wsl".into(),
                name: "WSL".into(),
                backend: ShellBackendType::System,
                executable: "wsl.exe".into(),
                interactive: true,
                status: ProfileAvailability::Available,
                ..Default::default()
            });
        }
        for base in
            ["C:\\Program Files\\Git\\bin\\bash.exe", "C:\\Program Files (x86)\\Git\\bin\\bash.exe"]
        {
            if Path::new(base).is_file() {
                insert(ShellProfileConfig {
                    id: "os-git-bash".into(),
                    name: "Git Bash".into(),
                    backend: ShellBackendType::System,
                    executable: base.to_string(),
                    interactive: true,
                    status: ProfileAvailability::Available,
                    ..Default::default()
                });
            }
        }
    }

    // 4) Concerto-managed Bash, but only when it is actually installed.
    if let Some(manifest) = ManagedRuntimeManager::auto_detect() {
        insert(ShellProfileConfig {
            id: "managed-bash".into(),
            name: "Concerto Managed Bash".into(),
            backend: ShellBackendType::Managed,
            executable: manifest.bash_executable.to_string_lossy().into_owned(),
            interactive: true,
            status: ProfileAvailability::Available,
            ..Default::default()
        });
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_backend_availability_matches_install_state() {
        let profile = ShellProfileConfig {
            id: "managed-bash".into(),
            name: "Concerto Managed Bash".into(),
            backend: ShellBackendType::Managed,
            ..Default::default()
        };
        match (ManagedRuntimeManager::auto_detect(), profile.availability()) {
            (Some(_), ProfileAvailability::Available) => {}
            (None, ProfileAvailability::Unavailable(reason)) => {
                assert!(reason.contains("not installed"));
            }
            (installed, status) => {
                panic!("managed runtime state and availability disagree: {installed:?}, {status:?}")
            }
        }
    }

    #[test]
    fn missing_executable_reports_unavailable() {
        let profile = ShellProfileConfig {
            id: "ghost".into(),
            name: "Ghost shell".into(),
            backend: ShellBackendType::System,
            executable: "definitely-not-a-real-shell-binary-xyz".into(),
            ..Default::default()
        };
        match profile.availability() {
            ProfileAvailability::Unavailable(reason) => {
                assert!(reason.contains("definitely-not-a-real-shell-binary-xyz"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn empty_executable_reports_unavailable() {
        let profile = ShellProfileConfig {
            id: "empty".into(),
            name: "Empty".into(),
            backend: ShellBackendType::System,
            executable: String::new(),
            ..Default::default()
        };
        assert!(matches!(profile.availability(), ProfileAvailability::Unavailable(_)));
    }

    #[cfg(unix)]
    #[test]
    fn parse_etc_shells_keeps_existing_skips_missing_and_comments() {
        // Use a path that exists on every Unix runner plus a comment and a
        // non-existent entry; only the existing shell should be returned.
        let contents = "# this is a comment\n/bin/sh\n/definitely/missing/shell-xyz\n";
        let profiles = parse_etc_shells(contents);
        let ids: Vec<String> = profiles.iter().map(|p| p.id.clone()).collect();
        assert!(ids.contains(&"os-sh".to_string()), "expected os-sh, got {ids:?}");
        assert!(
            !ids.iter().any(|i| i.contains("missing")),
            "missing shell must be skipped, got {ids:?}"
        );
    }

    #[test]
    fn discover_finds_at_least_one_shell_on_runner() {
        // Every dev/CI machine has at least one shell discoverable via PATH or
        // /etc/shells; this guards against the discovery path silently returning
        // nothing.
        let shells = discover_os_shells();
        assert!(!shells.is_empty(), "expected at least one OS shell to be discovered");
        // Every discovered profile must be a host-backed shell and marked available.
        for s in &shells {
            assert!(matches!(s.backend, ShellBackendType::System | ShellBackendType::Managed));
            assert_eq!(s.status, ProfileAvailability::Available);
        }
    }

    #[test]
    fn path_resolution_handles_missing_path_without_mutating_process_state() {
        assert!(resolve_in_path_with("definitely-not-a-shell", None).is_none());
    }

    #[test]
    fn effective_env_uses_platform_path_list_encoding() {
        let profile = ShellProfileConfig {
            path_additions: vec![PathBuf::from("first"), PathBuf::from("second")],
            ..Default::default()
        };
        let base = HashMap::from([("PATH".to_string(), "existing".to_string())]);

        let effective = profile.effective_env(&base);
        let paths = std::env::split_paths(OsStr::new(&effective["PATH"])).collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![PathBuf::from("first"), PathBuf::from("second"), PathBuf::from("existing")]
        );
    }

    #[test]
    fn legacy_bindings_migrate_from_agent_shell() {
        let settings: ShellSettings = serde_json::from_value(serde_json::json!({
            "profiles": [
                { "id": "terminal", "name": "Terminal", "executable": "sh" },
                { "id": "agent", "name": "Agent", "executable": "sh" },
                { "id": "build", "name": "Build", "executable": "sh" }
            ],
            "defaults": {
                "interactive_terminal": "terminal",
                "agent_shell": "agent",
                "build_validation": "build"
            }
        }))
        .expect("legacy settings should deserialize");

        assert_eq!(settings.selected_profile_id(), "agent");
        let serialized = serde_json::to_value(settings).expect("settings should serialize");
        assert!(serialized.get("defaults").is_none());
    }

    #[test]
    fn invalid_selection_falls_back_to_first_profile() {
        let fallback = ShellProfileConfig {
            id: "os-fallback".into(),
            name: "Fallback".into(),
            executable: "fallback".into(),
            ..Default::default()
        };
        let settings = ShellSettings::new(vec![fallback], "removed-profile".to_owned(), None);

        assert_eq!(settings.selected_profile_id(), "os-fallback");
        assert_eq!(
            settings.selected_profile().map(|profile| profile.id.as_str()),
            Some("os-fallback")
        );
    }

    #[test]
    fn normalization_replaces_presets_and_stale_detection_but_keeps_custom_profiles() {
        let legacy = ShellProfileConfig {
            id: "system-default".into(),
            name: "System default".into(),
            executable: "cmd.exe".into(),
            ..Default::default()
        };
        let stale = ShellProfileConfig {
            id: "os-stale".into(),
            name: "Stale".into(),
            executable: "stale".into(),
            ..Default::default()
        };
        let custom = ShellProfileConfig {
            id: "custom-1".into(),
            name: "Project shell".into(),
            executable: "project-shell".into(),
            ..Default::default()
        };
        let detected = ShellProfileConfig {
            id: "os-detected".into(),
            name: "Detected".into(),
            executable: "detected".into(),
            status: ProfileAvailability::Available,
            ..Default::default()
        };
        let settings =
            ShellSettings::new(vec![legacy, stale, custom], "system-default".into(), None)
                .normalized_with_detected(vec![detected]);

        let ids = settings.profiles.iter().map(|profile| profile.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["os-detected", "custom-1"]);
        assert_eq!(settings.selected_profile_id(), "os-detected");
    }

    // ------------------------------------------------------------------
    // resolve_working_dir
    // ------------------------------------------------------------------

    #[test]
    fn resolve_working_dir_project_root_returns_project() {
        let profile = ShellProfileConfig {
            default_working_dir: WorkingDirBehavior::ProjectRoot,
            ..Default::default()
        };
        let project = Path::new("/tmp/project");
        let result = profile.resolve_working_dir(project, None);
        assert_eq!(result, Some(project.to_path_buf()));
    }

    #[test]
    fn resolve_working_dir_home_returns_home() {
        let profile = ShellProfileConfig {
            default_working_dir: WorkingDirBehavior::Home,
            ..Default::default()
        };
        let home = Path::new("/home/user");
        let result = profile.resolve_working_dir(Path::new("/tmp/project"), Some(home));
        assert_eq!(result, Some(home.to_path_buf()));
    }

    #[test]
    fn resolve_working_dir_shell_default_returns_none() {
        let profile = ShellProfileConfig {
            default_working_dir: WorkingDirBehavior::ShellDefault,
            ..Default::default()
        };
        let result = profile.resolve_working_dir(Path::new("/tmp/project"), None);
        assert_eq!(result, None);
    }

    // ------------------------------------------------------------------
    // command_args by shell family
    // ------------------------------------------------------------------

    #[test]
    fn command_args_posix_uses_dash_c() {
        let profile = ShellProfileConfig { executable: "/bin/bash".into(), ..Default::default() };
        let args = profile.command_args("echo hello");
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"echo hello".to_string()));
    }

    #[test]
    fn command_args_powershell_uses_command_flag() {
        let profile =
            ShellProfileConfig { executable: "/usr/bin/pwsh".into(), ..Default::default() };
        let args = profile.command_args("Write-Output hi");
        assert!(args.contains(&"-Command".to_string()));
        assert!(args.contains(&"Write-Output hi".to_string()));
    }

    // ------------------------------------------------------------------
    // effective_env
    // ------------------------------------------------------------------

    #[test]
    fn effective_env_without_path_additions_preserves_base() {
        let profile = ShellProfileConfig::default();
        let base = HashMap::from([
            ("HOME".to_string(), "/home/user".to_string()),
            ("SHELL".to_string(), "/bin/zsh".to_string()),
        ]);
        let env = profile.effective_env(&base);
        assert_eq!(env.get("HOME").unwrap(), "/home/user");
        assert_eq!(env.get("SHELL").unwrap(), "/bin/zsh");
    }

    #[test]
    fn effective_env_overrides_base_with_profile_env() {
        let profile = ShellProfileConfig {
            env: HashMap::from([("EDITOR".to_string(), "vim".to_string())]),
            ..Default::default()
        };
        let base = HashMap::from([("EDITOR".to_string(), "nano".to_string())]);
        let env = profile.effective_env(&base);
        assert_eq!(env.get("EDITOR").unwrap(), "vim");
    }

    // ------------------------------------------------------------------
    // profile lookup
    // ------------------------------------------------------------------

    #[test]
    fn profile_returns_none_for_missing_id() {
        let settings = ShellSettings::default();
        assert!(settings.profile("non-existent").is_none());
    }

    #[test]
    fn profile_returns_some_for_valid_id() {
        let profile = ShellProfileConfig {
            id: "test-shell".into(),
            executable: "sh".into(),
            ..Default::default()
        };
        let settings = ShellSettings::new(vec![profile], "test-shell".into(), None);
        let found = settings.profile("test-shell");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "test-shell");
    }

    // ------------------------------------------------------------------
    // selected_profile_id fallback
    // ------------------------------------------------------------------

    #[test]
    fn selected_profile_id_uses_first_profile_when_everything_missing() {
        let profile = ShellProfileConfig {
            id: "only-one".into(),
            executable: "sh".into(),
            ..Default::default()
        };
        let settings = ShellSettings::new(vec![profile], "".into(), None);
        assert_eq!(settings.selected_profile_id(), "only-one");
    }
}
