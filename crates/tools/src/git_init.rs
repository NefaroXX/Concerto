//! Automatic `git init` for projects that are not yet version-controlled.
//!
//! At session start the session manager calls [`ensure_git_repo`] so a brand
//! new project directory becomes a real git repository before the agent starts
//! writing files. This is deliberately conservative: it only runs a bare
//! `git init` — no initial commit, no `.gitignore`, no identity, no remotes.
//! Creating the first commit remains the agent's job. The behavior is
//! opt-out-able via the `[tools] git_auto_init` config key (default true).

use camino::Utf8Path;
use std::process::Command;

/// Outcome of [`ensure_git_repo`]. Logging of these outcomes is the caller's
/// responsibility; this function itself stays infallible so session setup
/// never fails because of git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitInitOutcome {
    /// Automatic git init is disabled in configuration.
    Disabled,
    /// The `git` binary is not available on PATH.
    GitUnavailable,
    /// The project directory is already inside a git repository (its own or
    /// an ancestor's, standard git semantics).
    AlreadyInRepo,
    /// A new repository was created in `project_dir`.
    Initialized,
    /// `git init` ran but failed; carries a human-readable reason.
    Failed(String),
}

/// Whether a path is already inside a git repository (the directory itself or
/// any ancestor, standard git semantics).
pub fn is_initialized(project_dir: &Utf8Path) -> bool {
    match Command::new("git").args(["rev-parse", "--git-dir"]).current_dir(project_dir).output() {
        Ok(output) => output.status.success(),
        // `git` missing or unspawnable — treat as not initialized.
        Err(_) => false,
    }
}

/// Ensure a project directory is a git repository, when enabled.
///
/// * `enable == false` returns [`GitInitOutcome::Disabled`] without touching
///   the filesystem.
/// * The directory is created if missing, so a brand-new project root works.
/// * Already inside a repo (own or ancestor) → [`GitInitOutcome::AlreadyInRepo`].
/// * `git` unavailable → [`GitInitOutcome::GitUnavailable`], confirmed via
///   `git --version` on PATH (run without a working directory on purpose: the
///   project may not exist yet).
/// * Otherwise a bare `git init` runs in `project_dir`; success →
///   [`GitInitOutcome::Initialized`], failure → [`GitInitOutcome::Failed`].
pub fn ensure_git_repo(project_dir: &Utf8Path, enable: bool) -> GitInitOutcome {
    if !enable {
        return GitInitOutcome::Disabled;
    }

    if let Err(error) = std::fs::create_dir_all(project_dir.as_std_path()) {
        return GitInitOutcome::Failed(format!("could not create project directory: {error}"));
    }

    if is_initialized(project_dir) {
        return GitInitOutcome::AlreadyInRepo;
    }

    // The rev-parse above failed with a non-zero status or an I/O error.
    // Confirm the binary actually resolves before labeling it unavailable, so
    // a transient spawn failure is not silently treated as "git not installed".
    if Command::new("git").arg("--version").output().is_err() {
        return GitInitOutcome::GitUnavailable;
    }

    match Command::new("git").args(["init"]).current_dir(project_dir).output() {
        Ok(output) if output.status.success() => GitInitOutcome::Initialized,
        Ok(output) => {
            GitInitOutcome::Failed(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
        Err(error) => GitInitOutcome::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git` must be present to run these tests, matching the rest of the
    /// crate's git tests (see `testing.rs`/`undo.rs`).
    #[test]
    fn initializes_repo_when_missing() {
        let tmp = tempfile::tempdir().expect("create temp dir for git init test");
        let dir = Utf8Path::from_path(tmp.path()).expect("tempdir path is UTF-8");

        assert_eq!(ensure_git_repo(dir, true), GitInitOutcome::Initialized);
        assert!(is_initialized(dir), "git rev-parse must succeed after init");
    }

    #[test]
    fn skips_when_ancestor_repo_exists() {
        let tmp = tempfile::tempdir().expect("create temp dir for git init test");
        let root = Utf8Path::from_path(tmp.path()).expect("tempdir path is UTF-8");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(nested.as_std_path()).expect("create nested project dir");

        assert_eq!(ensure_git_repo(root, true), GitInitOutcome::Initialized);
        assert_eq!(
            ensure_git_repo(&nested, true),
            GitInitOutcome::AlreadyInRepo,
            "a project nested under a repo must not be re-inited"
        );
    }

    #[test]
    fn skips_when_disabled() {
        let tmp = tempfile::tempdir().expect("create temp dir for git init test");
        let dir = Utf8Path::from_path(tmp.path()).expect("tempdir path is UTF-8");

        assert_eq!(ensure_git_repo(dir, false), GitInitOutcome::Disabled);
        assert!(!is_initialized(dir), "disabled config must not create a repository");
    }

    #[test]
    fn already_in_repo_for_second_call() {
        let tmp = tempfile::tempdir().expect("create temp dir for git init test");
        let dir = Utf8Path::from_path(tmp.path()).expect("tempdir path is UTF-8");

        assert_eq!(ensure_git_repo(dir, true), GitInitOutcome::Initialized);
        assert_eq!(
            ensure_git_repo(dir, true),
            GitInitOutcome::AlreadyInRepo,
            "the second call must observe the repo created by the first"
        );
    }
}
