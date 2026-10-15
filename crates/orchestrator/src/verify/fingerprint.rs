//! Bounded fingerprints of the source tree a workspace check actually reads.

use std::io::Read;
use std::path::Path;

use concerto_core::CancellationToken;

pub const WORKSPACE_PREFIX: &str = "workspace-blake3-v1:";
const MAX_ENTRIES: usize = 8192;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Hash sorted, workspace-relative paths and complete file bytes. Unknown,
/// unreadable, linked, cancelled or oversized scopes have no fingerprint.
/// Cache/build exclusions are the existing workspace inventory's exclusions.
pub fn workspace_fingerprint(
    project_root: &Path,
    scope: &Path,
    cancel: &CancellationToken,
) -> Option<String> {
    let root = project_root.canonicalize().ok()?;
    let scope = scope.canonicalize().ok()?;
    if !scope.starts_with(&root) || !scope.is_dir() {
        return None;
    }
    let mut stack = vec![scope.clone()];
    let mut files = Vec::new();
    let mut entries = 0_usize;
    while let Some(directory) = stack.pop() {
        if cancel.is_cancelled() {
            return None;
        }
        for entry in std::fs::read_dir(directory).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if crate::workspace_snapshot::dir_component_is_skipped(&path) {
                continue;
            }
            entries = entries.saturating_add(1);
            if entries > MAX_ENTRIES || cancel.is_cancelled() {
                return None;
            }
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                return None;
            }
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                files.push(path);
            } else {
                return None;
            }
        }
    }
    files.sort();
    let mut digest = blake3::Hasher::new();
    digest.update(b"concerto-workspace-verification-v1\0");
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    for path in files {
        let relative = path.strip_prefix(&scope).ok()?.to_str()?.replace('\\', "/");
        digest.update(&(relative.len() as u64).to_be_bytes());
        digest.update(relative.as_bytes());
        let mut file = std::fs::File::open(&path).ok()?;
        let size = file.metadata().ok()?.len();
        digest.update(&size.to_be_bytes());
        let mut read = 0_u64;
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            let count = file.read(&mut buffer).ok()?;
            if count == 0 {
                break;
            }
            read = read.saturating_add(count as u64);
            total = total.saturating_add(count as u64);
            if total > MAX_BYTES {
                return None;
            }
            digest.update(&buffer[..count]);
        }
        if read != size {
            return None;
        }
    }
    Some(format!("{WORKSPACE_PREFIX}{}", digest.finalize().to_hex()))
}

pub async fn checked_fingerprint(
    project_root: &Path,
    path: &camino::Utf8PathBuf,
    cancel: &CancellationToken,
) -> String {
    let root = project_root.to_path_buf();
    let resolved = super::resolve_verification_path(project_root, path);
    let token = cancel.clone();
    tokio::task::spawn_blocking(move || {
        if token.is_cancelled() {
            return String::new();
        }
        if resolved.is_dir() {
            workspace_fingerprint(&root, &resolved, &token).unwrap_or_default()
        } else {
            let Ok(canonical) = resolved.canonicalize() else { return String::new() };
            let Ok(root) = root.canonicalize() else { return String::new() };
            if !canonical.starts_with(root) {
                return String::new();
            }
            std::fs::metadata(&canonical)
                .ok()
                .filter(|metadata| metadata.len() <= MAX_BYTES)
                .and_then(|_| std::fs::read(canonical).ok())
                .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
                .unwrap_or_default()
        }
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_fingerprint_tracks_bytes_paths_and_excludes_build_cache() -> std::io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(root.join("a.rs"), "one")?;
        let token = CancellationToken::new();
        let first = workspace_fingerprint(root, root, &token);
        assert!(first.is_some());
        std::fs::create_dir(root.join("target"))?;
        std::fs::write(root.join("target/noise"), "cache")?;
        assert_eq!(first, workspace_fingerprint(root, root, &token));
        std::fs::write(root.join("a.rs"), "two")?;
        let changed = workspace_fingerprint(root, root, &token);
        assert_ne!(first, changed);
        std::fs::rename(root.join("a.rs"), root.join("b.rs"))?;
        assert_ne!(changed, workspace_fingerprint(root, root, &token));
        token.cancel();
        assert!(workspace_fingerprint(root, root, &token).is_none());
        Ok(())
    }

    #[test]
    fn foreign_scope_cannot_gain_a_workspace_fingerprint() -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let foreign = tempfile::tempdir()?;
        assert!(
            workspace_fingerprint(root.path(), foreign.path(), &CancellationToken::new()).is_none()
        );
        Ok(())
    }
}
