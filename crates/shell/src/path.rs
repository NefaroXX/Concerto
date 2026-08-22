//! Root-confined path resolution for shell builtins (ADR-55 §2, Phase 2).
//!
//! The filesystem tool confines its paths via
//! `concerto-tools::common::resolve_path`; the shell crate cannot depend on
//! `concerto-tools` (dependency direction: tools depends on core), so this
//! module implements the same containment semantics for shell path arguments
//! and working directories: resolve against the project root, rejecting `..`
//! traversal, absolute-path escapes, and symlink escapes, while still
//! supporting not-yet-existing paths (new files) by canonicalizing the deepest
//! existing ancestor.
//!
//! Resolution contract (mirrors `common::resolve_path` and
//! `containment::resolve_within`):
//!
//! - The project root is the trust anchor; it is canonicalized and every
//!   resolved result must start with it.
//! - An absolute `requested` path under the *lexical* root is re-anchored at
//!   the canonical root; an absolute path outside the root is rejected.
//! - A relative `requested` path resolves against the canonical `cwd` (itself
//!   verified inside the root), so a symlinked workspace root or cwd keeps
//!   new-file resolution anchored at the real location.
//! - A candidate that exists is canonicalized (defeating `..` and symlink
//!   tricks) and must start with the root.
//! - A candidate that does not exist yet resolves through its deepest existing
//!   ancestor (canonicalized and verified), re-appending the remaining
//!   segments with `..` collapsed and link-like components rejected, so
//!   creating a new in-root file stays reachable while a `..`/symlink climb is
//!   caught.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use thiserror::Error;

/// Error resolving a user-supplied path against the project root.
#[derive(Debug, Error)]
#[non_exhaustive]
pub(crate) enum PathError {
    /// The trust anchor itself could not be canonicalized; nothing can be
    /// resolved safely.
    #[error("project root `{root}` is inaccessible: {source}")]
    RootInaccessible { root: Utf8PathBuf, source: std::io::Error },
    /// The resolved path lands outside the project root (`..` traversal or an
    /// absolute path that escapes the root).
    #[error("path `{requested}` is outside project root `{root}`")]
    OutsideRoot { requested: Utf8PathBuf, root: Utf8PathBuf },
    /// The resolved path passes through a symlink pointing outside the root
    /// (detected while reconstructing a not-yet-existing candidate).
    #[error("path `{requested}` passes through a symlink pointing outside project root `{root}`")]
    SymlinkEscape { requested: Utf8PathBuf, root: Utf8PathBuf },
    /// No existing ancestor of the candidate lies inside the root (the
    /// working directory itself is broken, or the path is entirely synthetic).
    #[error("cannot resolve `{requested}` (no existing ancestor inside the project root)")]
    Unresolvable { requested: String },
}

/// Canonicalize the containment trust anchor (the project root).
///
/// Every resolved path must start with this canonical form. Fails when the
/// root itself is inaccessible, in which case no path can be resolved safely.
pub(crate) fn canonicalize_root(root: &Utf8Path) -> Result<Utf8PathBuf, PathError> {
    root.canonicalize_utf8()
        .map_err(|source| PathError::RootInaccessible { root: root.to_owned(), source })
}

/// Resolve `requested` against `cwd`, confined to `root`.
///
/// Convenience entry point that canonicalizes the root first. Returns a
/// canonical absolute path inside the canonical root, or a [`PathError`].
pub(crate) fn resolve_path_in_root(
    root: &Utf8Path,
    cwd: &Utf8Path,
    requested: &str,
) -> Result<Utf8PathBuf, PathError> {
    let root_canonical = canonicalize_root(root)?;
    resolve_in_canonical_root(&root_canonical, root, cwd, requested)
}

/// Resolve `requested` against `cwd` with an already-canonicalized root.
///
/// `root_canonical` is the canonical trust anchor (see [`canonicalize_root`]);
/// `root_lexical` is the original, possibly symlinked root, used to re-anchor
/// absolute requests that the caller spelled with the lexical root.
pub(crate) fn resolve_in_canonical_root(
    root_canonical: &Utf8Path,
    root_lexical: &Utf8Path,
    cwd: &Utf8Path,
    requested: &str,
) -> Result<Utf8PathBuf, PathError> {
    let requested_path = Utf8Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        // Mirror `common::resolve_path`: an absolute path under the lexical
        // root is re-anchored at the canonical root; any other absolute path
        // is kept as-is and rejected by the containment check below.
        requested_path
            .strip_prefix(root_lexical)
            .map_or_else(|_| requested_path.to_owned(), |relative| root_canonical.join(relative))
    } else {
        // Anchor relative requests at the canonical cwd so a symlinked
        // workspace root (or a symlinked cwd) cannot make the new-file
        // re-append resolve against the wrong prefix.
        let cwd_canonical = cwd
            .canonicalize_utf8()
            .map_err(|_| PathError::Unresolvable { requested: requested.to_owned() })?;
        if !cwd_canonical.starts_with(root_canonical) {
            return Err(PathError::OutsideRoot {
                requested: cwd.to_owned(),
                root: root_canonical.to_owned(),
            });
        }
        cwd_canonical.join(requested_path)
    };
    resolve_candidate(root_canonical, &candidate, requested)
}

/// Canonicalize `candidate` and verify it stays within `root_canonical`.
///
/// A candidate that does not exist yet is resolved through its deepest
/// existing ancestor by [`resolve_not_exists`].
fn resolve_candidate(
    root_canonical: &Utf8Path,
    candidate: &Utf8Path,
    requested: &str,
) -> Result<Utf8PathBuf, PathError> {
    match candidate.canonicalize_utf8() {
        Ok(canonical) => {
            if canonical.starts_with(root_canonical) {
                Ok(canonical)
            } else {
                Err(PathError::OutsideRoot {
                    requested: Utf8PathBuf::from(requested),
                    root: root_canonical.to_owned(),
                })
            }
        }
        Err(_) => resolve_not_exists(root_canonical, candidate, requested),
    }
}

/// Resolve a not-yet-existing candidate inside `root_canonical`.
///
/// Walks up to the deepest existing ancestor, canonicalizes it (verifying it
/// stays within the root), then re-appends the remaining segments with `..`
/// collapsed and link-like components rejected, so a dangling symlink cannot
/// smuggle the new path outside the root.
fn resolve_not_exists(
    root_canonical: &Utf8Path,
    candidate: &Utf8Path,
    requested: &str,
) -> Result<Utf8PathBuf, PathError> {
    let mut ancestor = candidate.to_path_buf();
    let existing_ancestor = loop {
        let Some(parent) = ancestor.parent() else {
            return Err(PathError::Unresolvable { requested: requested.to_owned() });
        };
        match parent.canonicalize_utf8() {
            Ok(buf) => {
                // A deepest existing ancestor that resolves outside the root
                // was reached through a link or a `..` climb. A link climb is
                // a deliberate escape and named as such; a plain `..` climb is
                // reported as outside-root.
                if !buf.starts_with(root_canonical)
                    && escapes_via_link_below(root_canonical, parent)
                {
                    return Err(PathError::SymlinkEscape {
                        requested: Utf8PathBuf::from(requested),
                        root: root_canonical.to_owned(),
                    });
                }
                break buf;
            }
            Err(_) => ancestor = parent.to_path_buf(),
        }
    };
    if !existing_ancestor.starts_with(root_canonical) {
        return Err(PathError::OutsideRoot {
            requested: Utf8PathBuf::from(requested),
            root: root_canonical.to_owned(),
        });
    }

    let suffix = candidate.strip_prefix(&existing_ancestor).unwrap_or(candidate);
    let mut resolved = existing_ancestor;
    for component in suffix.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if let Some(parent) = resolved.parent() {
                    resolved = parent.to_path_buf();
                }
            }
            other => {
                resolved.push(other.as_str());
                if is_symlink(&resolved) {
                    return Err(PathError::SymlinkEscape {
                        requested: Utf8PathBuf::from(requested),
                        root: root_canonical.to_owned(),
                    });
                }
            }
        }
    }
    if !resolved.starts_with(root_canonical) {
        return Err(PathError::OutsideRoot {
            requested: Utf8PathBuf::from(requested),
            root: root_canonical.to_owned(),
        });
    }
    Ok(resolved)
}

/// Whether `path` exists as a symlink. Used only while reconstructing a
/// not-yet-existing candidate, where canonicalization cannot vouch for the
/// final location.
fn is_symlink(path: &Utf8Path) -> bool {
    std::fs::symlink_metadata(path.as_std_path())
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// Whether walking `path` (a lexical path anchored at `root_canonical`)
/// crosses a symlink whose canonical target resolves outside the root. Used to
/// classify a deepest-existing-ancestor escape as a link climb rather than a
/// plain `..` climb, so a not-yet-existing candidate like `<root>/link/new.txt`
/// with `link -> /outside-dir` is rejected as link-like even though
/// canonicalizing `root/link` alone already escapes.
fn escapes_via_link_below(root_canonical: &Utf8Path, path: &Utf8Path) -> bool {
    let Ok(relative) = path.strip_prefix(root_canonical) else {
        return false;
    };
    let mut prefix = root_canonical.to_path_buf();
    for component in relative.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                // A parent climb is a plain `..`, not a link.
                prefix.pop();
            }
            other => {
                prefix.push(other.as_str());
                if is_symlink(&prefix)
                    && prefix
                        .canonicalize_utf8()
                        .is_ok_and(|target| !target.starts_with(root_canonical))
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh temp root; returns (root, TempDir) keeping the dir alive.
    fn temp_root() -> (Utf8PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 temp path");
        (root, dir)
    }

    #[test]
    fn in_root_existing_path_resolves() {
        let (root, _dir) = temp_root();
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("write fixture");
        let got = resolve_path_in_root(&root, &root, "src/main.rs").expect("in-root resolves");
        assert_eq!(got, root.join("src/main.rs"));
        assert!(got.starts_with(&root));
    }

    #[test]
    fn dotdot_escape_rejected() {
        let (root, _dir) = temp_root();
        let err = resolve_path_in_root(&root, &root, "../escape").expect_err("escape must fail");
        assert!(matches!(err, PathError::OutsideRoot { .. }));
    }

    #[test]
    fn double_dotdot_from_subdir_rejected() {
        let (root, _dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        let err =
            resolve_path_in_root(&root, &sub, "../..").expect_err("climb above root must fail");
        assert!(matches!(err, PathError::OutsideRoot { .. }));
    }

    #[test]
    fn dotdot_from_subdir_that_stays_in_root_is_allowed() {
        let (root, _dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        std::fs::write(root.join("notes.txt"), "x").expect("write fixture");
        let got = resolve_path_in_root(&root, &sub, "../notes.txt").expect("in-root .. allowed");
        assert_eq!(got, root.join("notes.txt"));
    }

    #[test]
    fn absolute_path_outside_root_rejected() {
        let (root, _dir) = temp_root();
        let err = resolve_path_in_root(&root, &root, "/etc/passwd").expect_err("must fail");
        assert!(matches!(err, PathError::OutsideRoot { .. }));
    }

    #[test]
    fn absolute_path_inside_root_resolves() {
        let (root, _dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        let got = resolve_path_in_root(&root, &root, sub.as_str()).expect("in-root absolute");
        assert_eq!(got, root.join("subdir"));
    }

    #[test]
    fn new_file_path_inside_root_resolves() {
        let (root, _dir) = temp_root();
        std::fs::create_dir_all(root.join("src")).expect("create src");
        let got = resolve_path_in_root(&root, &root, "src/new_module/lib.rs")
            .expect("new in-root file resolves");
        assert_eq!(got, root.join("src/new_module/lib.rs"));
        assert!(got.starts_with(&root));
    }

    #[test]
    fn deeply_nested_new_dir_resolves() {
        let (root, _dir) = temp_root();
        // Only the root exists; everything below is new.
        let got = resolve_path_in_root(&root, &root, "a/b/c/d.rs").expect("new path resolves");
        assert_eq!(got, root.join("a/b/c/d.rs"));
        assert!(got.starts_with(&root));
    }

    #[test]
    fn new_file_through_dotdot_collapses_within_root() {
        let (root, _dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        // `subdir/../new.txt` collapses to `root/new.txt` — stays in root.
        let got =
            resolve_path_in_root(&root, &root, "subdir/../new.txt").expect("in-root collapse");
        assert_eq!(got, root.join("new.txt"));
    }

    #[test]
    fn inaccessible_root_rejected() {
        let (_root, _dir) = temp_root();
        let missing = Utf8PathBuf::from("/nonexistent-concerto-path");
        let err = resolve_path_in_root(&missing, &missing, "src").expect_err("must fail");
        assert!(matches!(err, PathError::RootInaccessible { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected() {
        let (root, dir) = temp_root();
        let outside = dir.path().parent().expect("temp parent");
        std::os::unix::fs::symlink(outside, root.join("escape")).expect("symlink");
        // An existing path through the link resolves outside the root.
        let err = resolve_path_in_root(&root, &root, "escape").expect_err("link escape must fail");
        assert!(matches!(err, PathError::OutsideRoot { .. }));
        // A not-yet-existing file through the link is a link escape.
        let err = resolve_path_in_root(&root, &root, "escape/new.txt").expect_err("must fail");
        assert!(matches!(err, PathError::SymlinkEscape { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_root_keeps_new_files_in_root() {
        let (root, _dir) = temp_root();
        let real = tempfile::tempdir().expect("real dir");
        let real_utf8 =
            Utf8PathBuf::from_path_buf(real.path().to_path_buf()).expect("utf8 real path");
        std::fs::create_dir_all(real_utf8.join("src")).expect("create src");
        let link = root.join("project-link");
        std::os::unix::fs::symlink(&real_utf8, &link).expect("symlink workspace");
        // The lexical root is a symlink; the resolved target must be anchored
        // at the canonical root, not the lexical prefix.
        let got = resolve_path_in_root(&link, &link, "src/new/lib.rs").expect("new file resolves");
        assert_eq!(got, real_utf8.join("src/new/lib.rs"));
        assert!(got.starts_with(&real_utf8));
    }
}
