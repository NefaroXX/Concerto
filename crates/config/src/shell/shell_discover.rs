//! OS shell discovery (ADR-28) — the host-scan half of the shell settings.
//!
//! This module owns the known-shell table and the discovery primitives: the
//! `PATH` resolver ([`resolve_in_path_with`] / [`resolve_in_path`]), the
//! `/etc/shells` parser ([`parse_etc_shells`]), and [`discover_os_shells`],
//! which scans the host (plus the Windows `COMSPEC`/WSL/Git-Bash and the
//! Concerto-managed-runtime extras) and returns the profiles the settings
//! picker offers. The bodies move verbatim from `shell.rs` — including the
//! platform `cfg(windows)` blocks, kept verbatim — and the parent module
//! re-exports [`discover_os_shells`] so the public
//! `crate::shell::discover_os_shells` path keeps resolving unchanged.
//! Discovery is read-only: nothing here writes config.

use super::*;

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
pub(super) fn resolve_in_path_with(program: &str, paths: Option<&OsStr>) -> Option<PathBuf> {
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

pub(super) fn resolve_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH");
    resolve_in_path_with(program, paths.as_deref())
}

/// Parse an `/etc/shells`-style file, returning one profile per non-comment,
/// existing login-shell path. Pure and testable (touches no filesystem itself).
#[cfg(unix)]
pub(super) fn parse_etc_shells(contents: &str) -> Vec<ShellProfileConfig> {
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
}
