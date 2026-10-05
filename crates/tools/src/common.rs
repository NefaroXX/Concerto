use camino::{Utf8Path, Utf8PathBuf};
use concerto_core::ToolError;

/// Resolves `user_path` relative to `root`, returning an error if the
/// resolved path would escape `root`. Prevents path-traversal attacks.
pub fn canonicalize_within(
    root: &Utf8Path,
    user_path: &Utf8Path,
) -> Result<Utf8PathBuf, ToolError> {
    // Resolve symlinks and normalize the root.
    let root_canonical = root.canonicalize().map_err(ToolError::Io)?;
    // Join user_path to root_canonical and canonicalize the result.
    let candidate = root_canonical.join(user_path);
    let candidate_canonical = candidate.canonicalize().map_err(ToolError::Io)?;

    // Check the candidate is under root.
    if candidate_canonical.starts_with(&root_canonical) {
        // Convert back to Utf8PathBuf
        Ok(Utf8PathBuf::from_path_buf(candidate_canonical).map_err(|_| {
            ToolError::ExecutionFailed { message: "non-UTF-8 path after canonicalization".into() }
        })?)
    } else {
        Err(ToolError::VirtualFsConflict {
            path: Utf8PathBuf::from(user_path),
            reason: "path traversal detected — resolved path escapes workspace root".into(),
        })
    }
}

/// The platform whose path rules apply.
///
/// Passed explicitly into the pure path-form helpers rather than read from
/// `cfg!` at each call site, so the Windows-only verbatim/plain branches are
/// exercised on the Linux-only CI runner (same seam as `shell.rs`'s `Host`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    Unix,
    Windows,
}

/// The host this binary was compiled for.
pub(crate) fn current_host() -> Host {
    if cfg!(windows) {
        Host::Windows
    } else {
        Host::Unix
    }
}

/// Normalized comparison key for `path` under `host`'s rules.
///
/// On Windows, `canonicalize()` yields the extended-length ("verbatim") form
/// (`\\?\C:\...`, `\\?\UNC\server\share`) while the lexical fallback used for
/// protected Desktop folders yields the plain form (`C:\...`,
/// `\\server\share`). Both address the *same* file, so the verbatim prefix is
/// stripped and ASCII case is folded (Windows paths are case-insensitive;
/// separators are normalized to `/`). Elsewhere the path string is returned
/// unchanged. Pure and host-parameterized so this is unit-testable off
/// Windows.
pub(crate) fn path_form_key(host: Host, path: &Utf8Path) -> String {
    let raw = path.as_str();
    if host != Host::Windows {
        return raw.to_string();
    }
    let un_verbatim = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.to_string()
    };
    un_verbatim.replace('\\', "/").to_ascii_lowercase()
}

/// Whether `candidate` is `root` or lies beneath it under `host`'s path-form
/// rules, so a verbatim candidate and a plain root naming the same directory
/// compare equal. The explicit separator guard prevents a sibling such as
/// `/root_evil` from matching `/root`. Containment is never weakened: only the
/// spelling of the same absolute path is normalized.
pub(crate) fn is_within(host: Host, root: &Utf8Path, candidate: &Utf8Path) -> bool {
    let root_key = path_form_key(host, root);
    let candidate_key = path_form_key(host, candidate);
    if candidate_key == root_key {
        return true;
    }
    let prefixed = if root_key.ends_with('/') { root_key } else { format!("{root_key}/") };
    candidate_key.starts_with(&prefixed)
}

/// Resolves a user-provided path against the workspace root, enforcing
/// isolation. Handles absolute paths, `..` traversal, and symlink escapes.
/// Returns a canonical absolute path within the root, or an error.
///
/// # New-file support
///
/// If the resolved path does not yet exist (e.g. a file being created),
/// the parent directory is canonicalized and the filename joined to it.
/// This avoids failing on `canonicalize()` for paths that legitimately
/// do not exist yet.
pub fn resolve_path(root: &Utf8Path, user_path: &Utf8Path) -> Result<Utf8PathBuf, ToolError> {
    // Convert the root to a canonical Utf8PathBuf.
    let root_canonical_buf = match root.canonicalize() {
        Ok(path) => path,
        Err(error)
            if cfg!(windows)
                && error.kind() == std::io::ErrorKind::PermissionDenied
                && root.is_absolute()
                && root.is_dir() =>
        {
            // Windows can return ERROR_ACCESS_DENIED from canonicalize() for a
            // directory that the current user can still enumerate and write
            // (notably some protected/redirected Desktop folders). Fall back
            // to lexical resolution while retaining the workspace boundary
            // and rejecting link-like child components.
            return resolve_lexically(root, user_path);
        }
        Err(error) => {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "cannot access workspace root '{}': {error}. Choose an existing folder that Concerto can read and write",
                    root
                ),
            })
        }
    };
    let root_canonical = Utf8PathBuf::from_path_buf(root_canonical_buf).map_err(|_| {
        ToolError::ExecutionFailed { message: "non-UTF-8 root after canonicalization".into() }
    })?;

    if user_path.is_absolute() {
        let relative = user_path.strip_prefix(root).unwrap_or(user_path);
        let candidate = root_canonical.join(relative);
        return resolve_not_exists(&root_canonical, &candidate, user_path);
    }
    // Otherwise treat as relative to root.
    let candidate = root_canonical.join(user_path);
    resolve_not_exists(&root_canonical, &candidate, user_path)
}

/// Resolve without `canonicalize` for the narrow Windows-permission fallback.
/// The selected root is treated as the trust anchor; traversal and link-like
/// children are still rejected before the path is returned.
fn resolve_lexically(root: &Utf8Path, user_path: &Utf8Path) -> Result<Utf8PathBuf, ToolError> {
    let root_absolute = lexical_normalize(root.as_std_path())?;
    let candidate = if user_path.is_absolute() {
        lexical_normalize(user_path.as_std_path())?
    } else {
        lexical_normalize(&root_absolute.join(user_path.as_std_path()))?
    };

    if !candidate.starts_with(&root_absolute) {
        return Err(ToolError::VirtualFsConflict {
            path: Utf8PathBuf::from(user_path),
            reason: "path traversal detected — resolved path escapes workspace root".into(),
        });
    }

    let relative =
        candidate.strip_prefix(&root_absolute).map_err(|_| ToolError::VirtualFsConflict {
            path: Utf8PathBuf::from(user_path),
            reason: "path escapes workspace root".into(),
        })?;
    let mut current = root_absolute.clone();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_link_like(&metadata) => {
                return Err(ToolError::VirtualFsConflict {
                    path: Utf8PathBuf::from(user_path),
                    reason: format!(
                        "link-like path component '{}' is not allowed in lexical fallback mode",
                        current.display()
                    ),
                })
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(ToolError::ExecutionFailed {
                    message: format!(
                        "cannot inspect workspace path '{}': {error}",
                        current.display()
                    ),
                })
            }
        }
    }

    Utf8PathBuf::from_path_buf(candidate).map_err(|_| ToolError::ExecutionFailed {
        message: "workspace path is not valid UTF-8".into(),
    })
}

fn lexical_normalize(path: &std::path::Path) -> Result<std::path::PathBuf, ToolError> {
    let absolute = std::path::absolute(path).map_err(|error| ToolError::ExecutionFailed {
        message: format!("cannot make workspace path '{}' absolute: {error}", path.display()),
    })?;
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

/// Try to canonicalize `candidate`.  If it does not exist yet, canonicalize
/// the parent directory and append the filename.  Both paths are verified
/// to stay within `root_canonical`.
fn resolve_not_exists(
    root_canonical: &Utf8Path,
    candidate: &Utf8Path,
    user_path: &Utf8Path,
) -> Result<Utf8PathBuf, ToolError> {
    match candidate.canonicalize() {
        Ok(canonical) => {
            let canonical =
                Utf8PathBuf::from_path_buf(canonical).map_err(|_| ToolError::ExecutionFailed {
                    message: "non-UTF-8 path after canonicalization".into(),
                })?;
            // Path exists — normal case. Compare forms tolerantly so a
            // canonical (verbatim) candidate and a lexical (plain) root for
            // the same directory are not misread as an escape on Windows.
            if is_within(current_host(), root_canonical, &canonical) {
                Ok(canonical)
            } else {
                Err(ToolError::VirtualFsConflict {
                    path: Utf8PathBuf::from(user_path),
                    reason: "path traversal detected — resolved path escapes workspace root".into(),
                })
            }
        }
        Err(_) => {
            // Path does not exist yet (new file). Walk up to the deepest
            // existing ancestor, canonicalize that (verifying it stays within
            // the root), then re-append the remaining segments with `..`
            // collapsed so the result can never escape the workspace. This
            // allows creating files inside directories that do not exist yet
            // (e.g. `src/new_module/lib.rs`), while still rejecting traversal.
            let mut ancestor = candidate.to_path_buf();
            let existing_ancestor = loop {
                match ancestor.parent() {
                    Some(parent) => match parent.canonicalize() {
                        Ok(buf) => {
                            let canonical = Utf8PathBuf::from_path_buf(buf).map_err(|_| {
                                ToolError::ExecutionFailed {
                                    message: "non-UTF-8 ancestor after canonicalization".into(),
                                }
                            })?;
                            if !is_within(current_host(), root_canonical, &canonical) {
                                return Err(ToolError::VirtualFsConflict {
                                    path: Utf8PathBuf::from(user_path),
                                    reason: "path escapes workspace root".into(),
                                });
                            }
                            break canonical;
                        }
                        Err(_) => ancestor = parent.to_path_buf(),
                    },
                    None => {
                        return Err(ToolError::ExecutionFailed {
                            message: format!(
                            "cannot resolve path for new file (no existing ancestor): {candidate}"
                        ),
                        })
                    }
                }
            };

            // Re-append the remaining segments (candidate relative to the
            // existing ancestor), collapsing `..` so the joined path cannot
            // climb above the workspace root.
            let suffix = candidate.strip_prefix(&existing_ancestor).unwrap_or(candidate);
            let mut resolved = existing_ancestor;
            for raw in suffix.as_str().split('/') {
                match raw {
                    "" | "." => {}
                    ".." => {
                        if let Some(parent) = resolved.parent() {
                            resolved = parent.to_path_buf();
                        }
                    }
                    other => resolved = resolved.join(other),
                }
            }
            if !is_within(current_host(), root_canonical, &resolved) {
                return Err(ToolError::VirtualFsConflict {
                    path: Utf8PathBuf::from(user_path),
                    reason: "path escapes workspace root".into(),
                });
            }
            Ok(resolved)
        }
    }
}

/// BLAKE3 hash of JSON-serialised input; used as `input_hash` in audit rows.
pub fn compute_input_hash(input: &serde_json::Value) -> String {
    // `serde_json::Value` always serializes, so this only falls back on a
    // poisoned/cyclic value; hash a sentinel instead of empty bytes to avoid
    // an all-zero collision for the (unreachable) failure case.
    let json_bytes =
        serde_json::to_vec(input).unwrap_or_else(|_| b"<unserializable input>".to_vec());
    let hash = blake3::hash(&json_bytes);
    hash.to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Creates a fresh, unique temp root and returns its **canonical** path.
    /// Each call returns a different directory so parallel tests don't race
    /// on `create_dir_all` / `remove_dir_all`.
    ///
    /// The root is canonicalized once, here, so every assertion compares
    /// canonical-to-canonical: `resolve_path` returns canonical paths by
    /// contract, while `TMPDIR` on macOS is a symlinked `/var/folders` →
    /// `/private/var/folders`.
    fn temp_root() -> Utf8PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "concerto_resolve_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::create_dir_all(&base);
        let base = Utf8PathBuf::from_path_buf(base).expect("temp root is valid UTF-8");
        base.canonicalize_utf8().expect("temp root canonicalizes")
    }

    #[test]
    fn resolve_path_allows_nested_new_dir() {
        let root = temp_root();
        // `src/new_module/lib.rs` does not exist yet, but `src/` does.
        let _ = fs::create_dir_all(root.join("src"));
        let got = resolve_path(&root, Utf8Path::new("src/new_module/lib.rs")).expect("resolves");
        assert_eq!(got, root.join("src/new_module/lib.rs"));
        assert!(got.starts_with(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_path_allows_deeply_nested_new_dir() {
        let root = temp_root();
        // Only the root exists; everything below is new.
        let got = resolve_path(&root, Utf8Path::new("a/b/c/d.rs")).expect("resolves");
        assert_eq!(got, root.join("a/b/c/d.rs"));
        assert!(got.starts_with(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_path_rejects_traversal() {
        let root = temp_root();
        let result = resolve_path(&root, Utf8Path::new("../../etc/passwd"));
        assert!(result.is_err(), "path traversal must be rejected");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lexical_fallback_allows_new_file_inside_root() {
        let root = temp_root();
        let got = resolve_lexically(&root, Utf8Path::new("src/main.rs")).expect("resolves");
        assert_eq!(got, root.join("src/main.rs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lexical_fallback_rejects_traversal() {
        let root = temp_root();
        let result = resolve_lexically(&root, Utf8Path::new("../outside.txt"));
        assert!(matches!(result, Err(ToolError::VirtualFsConflict { .. })));
        let _ = fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Pure path-form handling (Windows verbatim vs plain). Exercised on the
    // Linux CI via the explicit `Host` seam, mirroring `shell.rs`.
    // -----------------------------------------------------------------------

    #[test]
    fn unix_path_form_key_is_identity() {
        let key = path_form_key(Host::Unix, Utf8Path::new("/root/sub/file.txt"));
        assert_eq!(key, "/root/sub/file.txt");
    }

    #[test]
    fn windows_verbatim_and_plain_forms_compare_equal() {
        let verbatim = path_form_key(Host::Windows, Utf8Path::new(r"\\?\C:\Root\Sub\file.txt"));
        let plain = path_form_key(Host::Windows, Utf8Path::new(r"C:\Root\Sub\file.txt"));
        assert_eq!(verbatim, plain);
        // Separator and case differences from the same file are folded too.
        assert_eq!(plain, path_form_key(Host::Windows, Utf8Path::new("c:/root/sub/FILE.TXT")));
    }

    #[test]
    fn windows_verbatim_unc_maps_to_unc_form() {
        let verbatim = path_form_key(Host::Windows, Utf8Path::new(r"\\?\UNC\server\share\f.txt"));
        let plain = path_form_key(Host::Windows, Utf8Path::new(r"\\server\share\f.txt"));
        assert_eq!(verbatim, plain);
    }

    #[test]
    fn verbatim_candidate_is_within_plain_root() {
        assert!(is_within(
            Host::Windows,
            Utf8Path::new(r"C:\root"),
            Utf8Path::new(r"\\?\C:\root\file_test.txt"),
        ));
    }

    #[test]
    fn plain_candidate_is_within_verbatim_root() {
        assert!(is_within(
            Host::Windows,
            Utf8Path::new(r"\\?\C:\root"),
            Utf8Path::new(r"C:\root\file_test.txt"),
        ));
    }

    #[test]
    fn sibling_prefix_is_not_within_root() {
        assert!(!is_within(
            Host::Windows,
            Utf8Path::new(r"C:\root"),
            Utf8Path::new(r"\\?\C:\root_evil\file.txt"),
        ));
        assert!(!is_within(
            Host::Unix,
            Utf8Path::new("/root"),
            Utf8Path::new("/root_evil/file.txt"),
        ));
    }

    #[test]
    fn path_form_key_does_not_widen_containment() {
        // A genuinely outside path stays outside in every form.
        assert!(!is_within(
            Host::Windows,
            Utf8Path::new(r"C:\root"),
            Utf8Path::new(r"\\?\C:\other\file.txt"),
        ));
        assert!(!is_within(
            Host::Windows,
            Utf8Path::new(r"\\?\C:\root\sub"),
            Utf8Path::new(r"C:\root\file.txt"),
        ));
    }
}
