//! Shared cross-cutting helper utilities used across the memory subsystem.
//!
//! These are independently testable and have no dependencies on the rest
//! of the memory crate (no `MemoryStore`, no vector store, etc.).

use crate::memory::{MemoryNamespace, ProjectId};
use blake3;
use camino::Utf8Path;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Helpers for computing project IDs and global user namespace hashes.
pub struct ProjectIdHelper;

impl ProjectIdHelper {
    /// Canonicalise `path` and return its blake3 hex digest as a `ProjectId`.
    ///
    /// Symlinks are resolved to their target. Returns `Err` if the path
    /// does not exist or canonicalisation fails.
    pub fn from_dir(path: &Utf8Path) -> Result<ProjectId, std::io::Error> {
        let canonical = std::fs::canonicalize(path.as_std_path())?;
        let hash = blake3::hash(canonical.to_string_lossy().as_bytes());
        Ok(ProjectId(hash.to_hex().to_string()))
    }

    /// Compute the global user namespace hash (blake3 of username + hostname).
    ///
    /// This is stable across runs on the same machine for the same user.
    /// Uses `whoami` crate or environment variables as fallback.
    pub fn user_id_hash() -> String {
        let username = env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown_user".into());
        let hostname = hostname();
        let input = format!("{username}@{hostname}");
        let hash = blake3::hash(input.as_bytes());
        hash.to_hex().to_string()
    }

    /// Build a `MemoryNamespace::Global` value using the stable user hash.
    pub fn global_namespace() -> MemoryNamespace {
        MemoryNamespace::Global { user_id_hash: Self::user_id_hash() }
    }
}

/// Return a best-effort canonical project path.
///
/// Existing paths resolve symlinks and lexical aliases through the operating
/// system. Non-existent paths remain usable (important for diagnostics and
/// tests) and are made absolute when the current directory is available.
pub fn canonical_project_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// The 16-byte SQLite database file header (magic).
const SQLITE_HEADER: [u8; 16] = *b"SQLite format 3\0";

/// Return `true` when `path` exists and starts with the SQLite magic header.
///
/// A truncated or garbage file fails this check; a real (even schema-invalid
/// or migration-broken) database passes it. Used to decide whether a store
/// failure is a *corrupted file* (repairable by quarantine + recreate) or a
/// *valid file that failed for another reason* (must never be deleted).
pub fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 16];
    file.read_exact(&mut magic).is_ok() && magic == SQLITE_HEADER
}

/// Quarantine a corrupted SQLite main database file so the next open creates
/// a fresh one (ADR-54 self-heal). Returns `Some(quarantine path)` when the
/// invalid file was renamed, `None` when there was nothing to rename.
///
/// Only files that FAIL the SQLite header check are moved. A file with a
/// valid header is never touched — a schema or migration failure on real data
/// must surface as an error, not be silently deleted. Only the main `.db`
/// file is moved; `-wal`/`-shm` sidecars are left untouched.
pub fn quarantine_corrupt_db_file(db_path: &Path) -> Option<PathBuf> {
    if !db_path.is_file() || is_sqlite_file(db_path) {
        return None;
    }
    let file_name =
        db_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let quarantine = db_path.with_file_name(format!("{file_name}.corrupt-{ts}.bak"));
    match std::fs::rename(db_path, &quarantine) {
        Ok(()) => Some(quarantine),
        Err(error) => {
            tracing::warn!(
                %error,
                path = %db_path.display(),
                "failed to quarantine invalid database file"
            );
            None
        }
    }
}

/// Stable string path for a project file across frontend processes.
pub fn project_path_key(path: &Path) -> String {
    let key = canonical_project_path(path).to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        key.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        key
    }
}

/// Stable 16-hex-character project id derived from a project directory path.
///
/// Used to scope per-project persisted state (memory DB, chat transcripts, …)
/// so switching project directories never mixes one project's state into
/// another's. Existing paths are canonicalised first so symlink and relative
/// spellings share one namespace.
pub fn project_id_hash(cwd: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    project_path_key(cwd).hash(&mut hasher);
    let raw = format!("{:016x}", hasher.finish());
    raw[..16].to_string()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown_host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_dir_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path()).unwrap();
        let id1 = ProjectIdHelper::from_dir(path).unwrap();
        let id2 = ProjectIdHelper::from_dir(path).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn from_dir_nonexistent_returns_err() {
        let path = Utf8Path::new("/tmp/does_not_exist_12345");
        assert!(ProjectIdHelper::from_dir(path).is_err());
    }

    #[test]
    fn user_id_hash_is_stable() {
        let h1 = ProjectIdHelper::user_id_hash();
        let h2 = ProjectIdHelper::user_id_hash();
        assert_eq!(h1, h2);
    }

    #[test]
    fn global_namespace_is_global() {
        let ns = ProjectIdHelper::global_namespace();
        assert!(matches!(ns, MemoryNamespace::Global { .. }));
    }

    #[test]
    fn project_id_hash_is_stable_and_scoped() {
        let a = project_id_hash(std::path::Path::new("/home/user/project-a"));
        let b = project_id_hash(std::path::Path::new("/home/user/project-b"));
        let a2 = project_id_hash(std::path::Path::new("/home/user/project-a"));
        assert_eq!(a, a2, "must be deterministic for the same path");
        assert_ne!(a, b, "different project dirs must produce different ids");
        assert_eq!(a.len(), 16, "must be 16 hex chars");
    }

    #[test]
    fn project_id_hash_normalizes_relative_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = project_id_hash(dir.path());
        let nested = dir.path().join("child");
        std::fs::create_dir_all(&nested).unwrap();
        let aliased = nested.join("..");
        assert_eq!(canonical, project_id_hash(&aliased));
    }

    #[test]
    fn sqlite_header_detection() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, b"this is not a sqlite database").unwrap();
        assert!(!is_sqlite_file(&garbage));

        let real = dir.path().join("real.db");
        std::fs::write(&real, *b"SQLite format 3\0plus-more-bytes").unwrap();
        assert!(is_sqlite_file(&real));

        let missing = dir.path().join("missing.db");
        assert!(!is_sqlite_file(&missing));
    }

    #[test]
    fn quarantine_moves_garbage_file_but_not_a_valid_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("store.db");
        std::fs::write(&garbage, b"garbage").unwrap();
        let quarantined =
            quarantine_corrupt_db_file(&garbage).expect("garbage file must be quarantined");
        assert!(!garbage.exists(), "garbage file must be moved away");
        assert!(quarantined.exists(), "quarantine backup must exist");
        assert!(
            quarantined
                .file_name()
                .map(|n| n.to_string_lossy().contains(".corrupt-")
                    && n.to_string_lossy().ends_with(".bak"))
                .unwrap_or(false),
            "quarantine name must be <name>.corrupt-<ts>.bak"
        );

        let real = dir.path().join("real.db");
        std::fs::write(&real, *b"SQLite format 3\0with-padding").unwrap();
        assert_eq!(
            quarantine_corrupt_db_file(&real),
            None,
            "a file with a valid SQLite header must never be quarantined"
        );
        assert!(real.exists(), "valid file must be untouched");

        let missing = dir.path().join("missing.db");
        assert_eq!(quarantine_corrupt_db_file(&missing), None);
    }
}
