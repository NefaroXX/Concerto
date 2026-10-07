//! Staged-change review helpers for the code editor.
//!
//! Pure computations over the [`VirtualFs`] overlay and the diffs already
//! computed from it: assembling the review set (sorted diff results plus the
//! baseline entries a decision re-checks) and counting reviewable hunks.
//! `workspace.rs` keeps the wiring — it locks the overlay, computes the diffs,
//! and stores the results on `State`.

use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use concerto_api_types::diff::{DiffLine, DiffResult};
use concerto_tools::virtual_fs::{VirtualFs, VirtualFsEntry};

/// Assemble the staged review set from `vfs` and the already-computed `results`.
///
/// Contract, for every changed path in `vfs`:
///
/// * a create/delete operation without diff text still contributes an
///   empty-hunk [`DiffResult`], so file operations stay reviewable;
/// * a path with a diff result or a create/delete operation is recorded in the
///   returned baseline map as a clone of the overlay entry — the snapshot
///   `decide_staged` compares against before accepting or discarding;
/// * a path with no readable overlay entry is skipped.
///
/// Results are stably sorted by path so the review list order is deterministic.
/// Pure: `vfs` is only read, `results` is consumed, and nothing panics.
pub(crate) fn collect_staged_review(
    vfs: &VirtualFs,
    mut results: Vec<DiffResult>,
) -> (Vec<DiffResult>, HashMap<Utf8PathBuf, VirtualFsEntry>) {
    let mut entries = HashMap::new();
    for path in vfs.changed_paths() {
        let Some(entry) = vfs.get(path) else { continue };
        let has_diff = results.iter().any(|result| result.path == path);
        let file_operation =
            matches!(entry, VirtualFsEntry::Created { .. } | VirtualFsEntry::Deleted { .. });
        if !has_diff && file_operation {
            results.push(DiffResult { path: path.to_path_buf(), hunks: Vec::new() });
        }
        if has_diff || file_operation {
            entries.insert(path.to_path_buf(), entry.clone());
        }
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    (results, entries)
}

/// Count the reviewable hunks staged for `path`.
///
/// A hunk is reviewable when it carries at least one non-context line; a path
/// present with no such hunk (an empty file creation, for example) still counts
/// as one so it remains openable in the review UI. A path with no staged diff
/// counts as zero. Pure: no I/O, no panics.
pub(crate) fn count_staged_hunks(results: &[DiffResult], path: &Utf8Path) -> usize {
    results
        .iter()
        .find(|result| result.path == path)
        .map(|result| {
            result
                .hunks
                .iter()
                .filter(|hunk| {
                    hunk.lines.iter().any(|line| !matches!(line, DiffLine::Context { .. }))
                })
                .count()
                .max(1)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_api_types::diff::Hunk;

    fn hunk(lines: Vec<DiffLine>) -> Hunk {
        Hunk { old_start: 1, old_len: 1, new_start: 1, new_len: 1, lines }
    }

    /// A created file with no diff text still becomes a reviewable result with
    /// a baseline entry; a path without any staged state is absent entirely.
    #[test]
    fn collects_file_operations_without_diff_text() {
        let mut vfs = VirtualFs::new();
        let created = Utf8PathBuf::from("created.rs");
        vfs.write(&created, String::new()).expect("stage a created file");

        let (results, entries) = collect_staged_review(&vfs, Vec::new());

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, created);
        assert!(results[0].hunks.is_empty());
        assert!(matches!(entries.get(&created), Some(VirtualFsEntry::Created { .. })));
        assert_eq!(count_staged_hunks(&results, &created), 1);
        assert_eq!(count_staged_hunks(&results, Utf8Path::new("missing.rs")), 0);
    }

    /// Only hunks carrying a non-context line count; a results entry whose
    /// hunks are all context still counts as one reviewable hunk.
    #[test]
    fn counts_only_changed_hunks() {
        let path = Utf8PathBuf::from("edit.rs");
        let results = vec![DiffResult {
            path: path.clone(),
            hunks: vec![
                hunk(vec![DiffLine::Context { content: "keep".into(), line_num: 1 }]),
                hunk(vec![DiffLine::Addition { content: "new".into(), line_num: 2 }]),
            ],
        }];

        assert_eq!(count_staged_hunks(&results, &path), 1);
        assert_eq!(count_staged_hunks(&results, Utf8Path::new("other.rs")), 0);
    }
}
