//! Path-expansion helpers for configured skill search paths.
//!
//! Search paths may carry Windows-style `%VAR%` references (e.g.
//! `%APPDATA%`) and a leading `~`; this module owns that expansion plus the
//! absolute-path resolution used when a pack's `instructions_path` and
//! `resources` are loaded. It is extracted from `manager.rs` (the path
//! cluster: [`expanded_search_path`] and its `~` / `%VAR%` helpers) so the
//! parent file keeps discovery, loading, and CRUD focused.
//!
//! [`expanded_search_path`] is the only `pub` item here and is re-exported by
//! the parent (`manager.rs`), so the public
//! `concerto_skills::expanded_search_path` surface is unchanged; the helpers
//! the parent module still calls are `pub(super)` and the rest stay private.

use super::*;

/// Expand a configured search path for display or pre-checks: Windows-style
/// `%NAME%` environment-variable references (e.g. `%USERPROFILE%`, `%APPDATA%`)
/// and a leading `~` are resolved to their absolute form. Paths with none of
/// these tokens are returned unchanged. Returns `None` when a `~` path cannot
/// be expanded because no home directory is known.
///
/// This is the same expansion applied during discovery; it performs no I/O and
/// does not check whether the result exists.
pub fn expanded_search_path(path: &Path) -> Option<PathBuf> {
    expand_home(path)
}

/// Expand a search path: Windows-style `%NAME%` environment-variable
/// references (e.g. `%USERPROFILE%`, `%APPDATA%`), then a leading `~`
/// (`~`, `~/...`, `~\...`). Paths with none of these tokens are returned
/// unchanged; `None` means a `~` path could not be expanded because no home
/// directory is known.
pub(super) fn expand_home(path: &Path) -> Option<PathBuf> {
    let expanded = expand_env_refs(&path.to_string_lossy());
    resolve_tilde(&expanded, user_home_dir())
}

/// Cross-platform `~` expansion against an explicit home directory. `~` maps
/// to `home` itself; `~/...` and `~\...` join the remainder. Non-tilde paths
/// pass through unchanged.
fn resolve_tilde(path: &str, home: Option<PathBuf>) -> Option<PathBuf> {
    match strip_tilde(path) {
        Some(rest) => Some(home?.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// Strip a leading `~`, `~/`, or `~\` marker, returning the path remainder
/// (empty for a bare `~`). `None` when the path does not start with `~`.
fn strip_tilde(text: &str) -> Option<&str> {
    if text == "~" {
        return Some("");
    }
    text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
}

/// Replace Windows-style `%NAME%` environment-variable references in a path
/// with their values. Tokens whose variable is unset — including a lone or
/// unmatched `%` — are left verbatim so the caller can still report the
/// original path. Returns the path unchanged when it contains no `%`.
fn expand_env_refs(path: &str) -> String {
    if !path.contains('%') {
        return path.to_string();
    }
    let mut result = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(percent) = rest.find('%') {
        result.push_str(&rest[..percent]);
        let after_percent = &rest[percent + 1..];
        let Some(closing) = after_percent.find('%') else {
            // Unmatched `%`: keep the remainder verbatim.
            result.push_str(&rest[percent..]);
            return result;
        };
        let name = &after_percent[..closing];
        match std::env::var(name) {
            Ok(value) => result.push_str(&value),
            Err(_) => {
                result.push('%');
                result.push_str(name);
                result.push('%');
            }
        }
        rest = &after_percent[closing + 1..];
    }
    result.push_str(rest);
    result
}

/// Resolve the user's home directory, with platform-appropriate fallbacks.
///
/// - Windows: `$USERPROFILE`, then a profile path derived from `$APPDATA`
///   (`<profile>\AppData\Roaming`), then `dirs::home_dir()`.
/// - Other platforms: `dirs::home_dir()` (which consults `$HOME` and friends).
fn user_home_dir() -> Option<PathBuf> {
    for (var, derive_profile) in home_env_candidates() {
        if let Some(value) = std::env::var_os(var) {
            let path = PathBuf::from(value);
            if *derive_profile {
                if let Some(home) = profile_from_appdata(&path) {
                    return Some(home);
                }
            } else {
                return Some(path);
            }
        }
    }
    dirs::home_dir()
}

/// `(env var holding the profile, bool: true when it is `%APPDATA%`, whose
/// parent-of-parent is the profile directory)`. The Windows variables are only
/// consulted on Windows; on other platforms the list is empty and
/// `dirs::home_dir()` is authoritative.
fn home_env_candidates() -> &'static [(&'static str, bool)] {
    #[cfg(windows)]
    {
        &[("USERPROFILE", false), ("APPDATA", true)]
    }
    #[cfg(not(windows))]
    {
        &[]
    }
}

/// Derive the profile directory from a Windows `%APPDATA%` path
/// (`<profile>\AppData\Roaming`) by walking up two levels. Returns `None`
/// when the path has fewer than two parents.
fn profile_from_appdata(app_data: &Path) -> Option<PathBuf> {
    app_data.parent().and_then(Path::parent).map(Path::to_path_buf)
}

/// Resolve a path to absolute form relative to `base` (absolute paths pass
/// through unchanged).
pub(super) fn absolute_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Resolve every path in `paths` to absolute form relative to `base`.
pub(super) fn resolve_all(base: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| absolute_path(base, p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shared fixtures (`TempDir`, `write_toml_pack`, `ids`, …) stay in
    // the parent's test module — marked `pub(super)` — so these relocated
    // tests and the discovery/CRUD tests there use one definition instead
    // of duplicated scaffolding.
    use super::super::tests::{ids, write_toml_pack, TempDir};

    #[test]
    fn discovery_expands_tilde_via_home() -> Result<(), SkillsError> {
        let temp = TempDir::new("tilde")?;
        let home = temp.path().join("home");
        write_toml_pack(&home.join("skills/pack-a"), "tilde-pack", "from home")?;

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let discovered = SkillManager::new(vec![PathBuf::from("~/skills")]).discover();
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }

        let found = discovered?;
        assert_eq!(ids(&found), vec!["tilde-pack"]);
        Ok(())
    }

    #[test]
    fn resolve_tilde_expands_tilde_forms_and_leaves_plain_paths() {
        let home = PathBuf::from("/home/alice");
        assert_eq!(resolve_tilde("~", Some(home.clone())), Some(home.clone()));
        assert_eq!(resolve_tilde("~/skills", Some(home.clone())), Some(home.join("skills")));
        // Windows separator form joins the same way.
        assert_eq!(resolve_tilde("~\\skills", Some(home.clone())), Some(home.join("skills")));
        assert_eq!(
            resolve_tilde("plain/skills", Some(home.clone())),
            Some(PathBuf::from("plain/skills"))
        );
        assert_eq!(resolve_tilde("~", None), None);
        assert_eq!(resolve_tilde("~/skills", None), None);
    }

    #[test]
    fn strip_tilde_handles_bare_and_separator_forms() {
        assert_eq!(strip_tilde("~"), Some(""));
        assert_eq!(strip_tilde("~/a"), Some("a"));
        assert_eq!(strip_tilde("~\\a"), Some("a"));
        assert_eq!(strip_tilde("plain"), None);
        assert_eq!(strip_tilde("~~/a"), None);
    }

    #[test]
    fn expand_env_refs_replaces_percent_vars_and_leaves_unset_alone() {
        // A distinctive variable name so concurrent tests never observe it.
        // Must differ from the var used in expanded_search_path_is_public_expansion:
        // env vars are process-global and tests run on threads in parallel.
        std::env::set_var("CONCERTO_SKILLS_TEST_EXPAND_REFS", "/home/alice");
        assert_eq!(
            expand_env_refs("%CONCERTO_SKILLS_TEST_EXPAND_REFS%/skills"),
            "/home/alice/skills"
        );
        std::env::remove_var("CONCERTO_SKILLS_TEST_EXPAND_REFS");

        // Unset variables are left verbatim (callers still warn on the path).
        assert_eq!(
            expand_env_refs("%CONCERTO_SKILLS_TEST_UNSET%/skills"),
            "%CONCERTO_SKILLS_TEST_UNSET%/skills"
        );
        // No `%` at all: unchanged.
        assert_eq!(expand_env_refs("plain/path"), "plain/path");
        // Lone or unmatched `%`: remainder verbatim.
        assert_eq!(expand_env_refs("100%/skills"), "100%/skills");
        assert_eq!(expand_env_refs("a%unclosed"), "a%unclosed");
    }

    #[test]
    fn expand_home_expands_env_refs_in_path() {
        let home = std::env::temp_dir().join("concerto-skills-fake-home");
        std::env::set_var("CONCERTO_SKILLS_TEST_HOME", &home);
        let expanded = expand_home(Path::new("%CONCERTO_SKILLS_TEST_HOME%/skills"));
        std::env::remove_var("CONCERTO_SKILLS_TEST_HOME");
        assert_eq!(expanded, Some(home.join("skills")));
    }

    #[test]
    fn profile_from_appdata_walks_up_two_levels() {
        // Forward slashes parse as separators on both Windows and Unix, so the
        // derivation logic is exercised the same way on every platform.
        let app_data = PathBuf::from("C:/Users/alice/AppData/Roaming");
        assert_eq!(profile_from_appdata(&app_data), Some(PathBuf::from("C:/Users/alice")));
        // A path with fewer than two parents cannot be a Windows profile
        // (a bare component has no two parents on Unix or Windows).
        assert_eq!(profile_from_appdata(Path::new("Roaming")), None);
    }
}
