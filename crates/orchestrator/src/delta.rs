//! `FileDeltaTracker` — tracks net file changes across agent runs.
//!
//! Uses file metadata (size + mtime) instead of pure path sets so that an
//! edit to the same file is correctly detected as progress. This feeds the
//! coordinator's cycle-detection [`OrchestratorState`] so that iterative
//! edit-review cycles do not trigger false-positive aborts.

use camino::Utf8PathBuf;
use concerto_core::types::TaskId;
use std::collections::HashMap;
use std::time::SystemTime;

/// Lightweight file metadata used to detect content changes without reading
/// the file: `(length_in_bytes, mtime_unix_seconds, mtime_subsec_nanos)`.
type FileMeta = (u64, u64, u32);

/// Tracks net file changes across agent runs within a session.
///
/// Each snapshot records the metadata of files an agent reported modifying
/// so that subsequent runs (even on the same set of paths) can distinguish
/// real edits from repeated writes of identical content.
pub struct FileDeltaTracker {
    snapshots: HashMap<TaskId, HashMap<Utf8PathBuf, FileMeta>>,
}

impl FileDeltaTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self { snapshots: HashMap::new() }
    }

    /// Record file metadata for a task before (or after) a run.
    ///
    /// The caller supplies the file paths to track — typically the files the
    /// agent reported modifying. Metadata is computed internally from the
    /// filesystem.
    pub fn snapshot(&mut self, task_id: TaskId, files: &[Utf8PathBuf]) {
        self.snapshots.insert(task_id, Self::current_metadata(files));
    }

    /// Check whether any tracked file has changed since the last snapshot.
    ///
    /// Returns `true` if:
    /// - No prior snapshot exists for this task (first run → assumed progress).
    /// - A new file appeared that wasn't in the snapshot.
    /// - The size or mtime of a previously-snapped file changed.
    ///
    /// When a change is detected the snapshot is **updated** in place so the
    /// caller does not need a separate snapshot call after confirming progress.
    pub fn has_progress_since(&mut self, task_id: &TaskId, current_files: &[Utf8PathBuf]) -> bool {
        let current = Self::current_metadata(current_files);
        let Some(previous) = self.snapshots.get(task_id) else {
            // First snapshot — store and report progress.
            self.snapshots.insert(*task_id, current);
            return true;
        };

        // Quick check: different number of files.
        if current.len() != previous.len() {
            self.snapshots.insert(*task_id, current);
            return true;
        }

        // Check every tracked path: different size/mtime = content change.
        let mut changed = false;
        for (path, meta) in &current {
            match previous.get(path) {
                Some(prev_meta) if prev_meta == meta => {} // unchanged
                _ => {
                    changed = true;
                    break;
                }
            }
        }

        if changed {
            self.snapshots.insert(*task_id, current);
        }
        changed
    }

    /// Compute file metadata for a set of paths.
    fn current_metadata(files: &[Utf8PathBuf]) -> HashMap<Utf8PathBuf, FileMeta> {
        files
            .iter()
            .filter_map(|path| {
                std::fs::metadata(path).ok().map(|m| {
                    let (secs, nanos) = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| (d.as_secs(), d.subsec_nanos()))
                        .unwrap_or((0, 0));
                    (path.clone(), (m.len(), secs, nanos))
                })
            })
            .collect()
    }

    /// Remove snapshot for a task.
    pub fn remove(&mut self, task_id: &TaskId) {
        self.snapshots.remove(task_id);
    }
}

impl Default for FileDeltaTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    fn write_file(dir: &Utf8PathBuf, name: &str, content: &[u8]) -> Utf8PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    #[test]
    fn no_snapshot_reports_progress() {
        let mut tracker = FileDeltaTracker::new();
        assert!(tracker.has_progress_since(&TaskId::new(), &[]));
    }

    #[test]
    fn same_files_no_progress() {
        let mut tracker = FileDeltaTracker::new();
        let tid = TaskId::new();
        let (_dir, base) = scratch_dir();
        let p1 = write_file(&base, "main.rs", b"hello");
        let p2 = write_file(&base, "lib.rs", b"world");

        // First call creates the snapshot.
        assert!(tracker.has_progress_since(&tid, &[p1.clone(), p2.clone()]));
        // Second call with unchanged files → no progress.
        assert!(!tracker.has_progress_since(&tid, &[p1, p2]));
    }

    #[test]
    fn new_file_is_progress() {
        let mut tracker = FileDeltaTracker::new();
        let tid = TaskId::new();
        let (_dir, base) = scratch_dir();
        let p1 = write_file(&base, "main.rs", b"hello");

        // First run: just main.rs.
        assert!(tracker.has_progress_since(&tid, std::slice::from_ref(&p1)));
        // Second run: main.rs + lib.rs → new file = progress.
        let p2 = write_file(&base, "lib.rs", b"world");
        assert!(tracker.has_progress_since(&tid, &[p1, p2]));
    }

    #[test]
    fn content_edit_is_progress() {
        let mut tracker = FileDeltaTracker::new();
        let tid = TaskId::new();
        let (_dir, base) = scratch_dir();
        let p = write_file(&base, "main.rs", b"hello");

        // First run.
        assert!(tracker.has_progress_since(&tid, std::slice::from_ref(&p)));

        // Edit the file with different-length content.
        // NB: use a different byte length so the length field in FileMeta
        // catches the change regardless of filesystem mtime granularity.
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"world!").unwrap();
        f.flush().unwrap();

        // Same path, different content → should detect progress.
        assert!(tracker.has_progress_since(&tid, &[p]));
    }

    #[test]
    fn removed_file_is_progress() {
        let mut tracker = FileDeltaTracker::new();
        let tid = TaskId::new();
        let (_dir, base) = scratch_dir();
        let p1 = write_file(&base, "main.rs", b"hello");
        let p2 = write_file(&base, "lib.rs", b"world");

        // Two files.
        assert!(tracker.has_progress_since(&tid, &[p1.clone(), p2]));
        // One file removed.
        assert!(tracker.has_progress_since(&tid, &[p1]));
        assert!(tracker.has_progress_since(&tid, &[]));
    }
}
