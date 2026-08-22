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
            // Path exists — normal case.
            if canonical.starts_with(root_canonical) {
                Ok(Utf8PathBuf::from_path_buf(canonical).map_err(|_| {
                    ToolError::ExecutionFailed {
                        message: "non-UTF-8 path after canonicalization".into(),
                    }
                })?)
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
                            if !canonical.starts_with(root_canonical) {
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
            if !resolved.starts_with(root_canonical) {
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

    /// Creates a fresh, unique temp root and returns its path.
    /// Each call returns a different directory so parallel tests don't race
    /// on `create_dir_all` / `remove_dir_all`.
    fn temp_root() -> Utf8PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "concerto_resolve_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::create_dir_all(&base);
        Utf8PathBuf::from_path_buf(base).expect("temp root is valid UTF-8")
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
}
