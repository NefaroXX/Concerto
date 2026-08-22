use camino::Utf8PathBuf;
use concerto_api_types::diff::{DiffLine, DiffResult, Hunk};
use std::collections::HashSet;
use std::ops::Range;

fn change_ranges(old: &str, new: &str) -> Vec<(Range<u32>, Range<u32>)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{diff, Algorithm};

    let input = InternedInput::new(old, new);
    let mut changes = Vec::new();
    diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| {
        changes.push((before, after));
    });
    changes
}

/// Number of independently reviewable change hunks between two texts.
pub fn change_hunk_count(old: &str, new: &str) -> usize {
    change_ranges(old, new).len()
}

/// Reconstruct `new` while replacing selected change hunks with their content
/// from `old`. Unchanged lines are always preserved.
pub fn reject_change_hunks(
    old: &str,
    new: &str,
    rejected: &HashSet<usize>,
) -> Result<String, concerto_core::ToolError> {
    let changes = change_ranges(old, new);
    if let Some(index) = rejected.iter().find(|index| **index >= changes.len()) {
        return Err(concerto_core::ToolError::ExecutionFailed {
            message: format!("hunk index {index} out of bounds ({} change hunks)", changes.len()),
        });
    }
    if rejected.is_empty() {
        return Ok(new.to_string());
    }

    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut new_cursor = 0usize;
    for (index, (before, after)) in changes.iter().enumerate() {
        let after_start = after.start as usize;
        result.extend_from_slice(&new_lines[new_cursor.min(new_lines.len())..after_start]);
        if rejected.contains(&index) {
            result.extend_from_slice(&old_lines[before.start as usize..before.end as usize]);
        } else {
            result.extend_from_slice(&new_lines[after.start as usize..after.end as usize]);
        }
        new_cursor = after.end as usize;
    }
    result.extend_from_slice(&new_lines[new_cursor.min(new_lines.len())..]);

    let mut content = result.join("\n");
    let last_change_rejected_at_eof = changes.last().is_some_and(|(before, after)| {
        rejected.contains(&(changes.len() - 1))
            && after.end as usize == new_lines.len()
            && before.end as usize == old_lines.len()
    });
    let trailing_newline =
        if last_change_rejected_at_eof { old.ends_with('\n') } else { new.ends_with('\n') };
    if trailing_newline && !content.is_empty() {
        content.push('\n');
    }
    Ok(content)
}

/// Compute a unified diff between two strings.
/// Uses `imara-diff`'s Histogram algorithm for line-level diffs.
/// Returns a `DiffResult` containing all hunks.
pub fn compute_diff(path: Utf8PathBuf, old: &str, new: &str) -> DiffResult {
    if old == new {
        return DiffResult { path, hunks: Vec::new() };
    }
    let changes = change_ranges(old, new);

    let mut hunks = Vec::new();
    let mut old_pos: u64 = 0;
    let mut new_pos: u64 = 0;

    for (before, after) in &changes {
        let old_start = before.start as u64;
        let new_start = after.start as u64;

        // Emit context lines (unchanged lines between last change and this one)
        if old_pos < old_start || new_pos < new_start {
            let ctx_old_start = old_pos;
            let ctx_new_start = new_pos;
            let ctx_len = (old_start - old_pos).max(new_start - new_pos);
            let mut context_lines = Vec::new();
            for i in 0..ctx_len {
                let old_idx = ctx_old_start + i;
                if let Some(line) = get_line_at(old, old_idx as usize) {
                    context_lines.push(DiffLine::Context {
                        content: line.to_string(),
                        line_num: old_idx + 1,
                    });
                }
            }
            if !context_lines.is_empty() {
                hunks.push(Hunk {
                    old_start: ctx_old_start + 1,
                    old_len: ctx_len,
                    new_start: ctx_new_start + 1,
                    new_len: ctx_len,
                    lines: context_lines,
                });
            }
        }

        // Emit the change itself
        let hunk_old_start = old_start + 1;
        let hunk_new_start = new_start + 1;
        let mut lines = Vec::new();

        if before.is_empty() {
            // Pure insertion
            for i in after.clone() {
                let content = get_line_at(new, i as usize).unwrap_or("").to_string();
                lines.push(DiffLine::Addition { content, line_num: i as u64 + 1 });
            }
        } else if after.is_empty() {
            // Pure deletion
            for i in before.clone() {
                let content = get_line_at(old, i as usize).unwrap_or("").to_string();
                lines.push(DiffLine::Deletion { content, line_num: i as u64 + 1 });
            }
        } else {
            // Replacement: delete old lines, insert new lines
            for i in before.clone() {
                let content = get_line_at(old, i as usize).unwrap_or("").to_string();
                lines.push(DiffLine::Deletion { content, line_num: i as u64 + 1 });
            }
            for i in after.clone() {
                let content = get_line_at(new, i as usize).unwrap_or("").to_string();
                lines.push(DiffLine::Addition { content, line_num: i as u64 + 1 });
            }
        }

        hunks.push(Hunk {
            old_start: hunk_old_start,
            old_len: (before.end - before.start) as u64,
            new_start: hunk_new_start,
            new_len: (after.end - after.start) as u64,
            lines,
        });

        old_pos = before.end as u64;
        new_pos = after.end as u64;
    }

    // Tail context lines (unchanged lines after the last change)
    let old_total = old.lines().count() as u64;
    let new_total = new.lines().count() as u64;
    if old_pos < old_total || new_pos < new_total {
        let tail_len = (old_total - old_pos).max(new_total - new_pos);
        let mut context_lines = Vec::new();
        for i in 0..tail_len {
            let idx = old_pos + i;
            if let Some(line) = get_line_at(old, idx as usize) {
                context_lines
                    .push(DiffLine::Context { content: line.to_string(), line_num: idx + 1 });
            }
        }
        if !context_lines.is_empty() {
            hunks.push(Hunk {
                old_start: old_pos + 1,
                old_len: tail_len,
                new_start: new_pos + 1,
                new_len: tail_len,
                lines: context_lines,
            });
        }
    }

    DiffResult { path, hunks }
}

fn get_line_at(content: &str, index: usize) -> Option<&str> {
    content.lines().nth(index)
}

/// Lightweight reference type for virtual FS entry types during diff.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum VirtualFsEntryRef<'a> {
    Unchanged(&'a str),
    Modified { original: &'a str, current: &'a str },
    Deleted { original: &'a str },
    Created { current: &'a str },
}

/// Compute diffs from a set of virtual FS entry references.
pub fn compute_all_virtual_diffs(
    entries: &[(Utf8PathBuf, VirtualFsEntryRef<'_>)],
) -> Vec<DiffResult> {
    entries
        .iter()
        .filter_map(|(path, entry)| match entry {
            VirtualFsEntryRef::Unchanged(_) => None,
            VirtualFsEntryRef::Modified { original, current } => {
                let result = compute_diff(path.clone(), original, current);
                if result.hunks.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }
            VirtualFsEntryRef::Deleted { original } => {
                let result = compute_diff(path.clone(), original, "");
                if result.hunks.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }
            VirtualFsEntryRef::Created { current } => {
                let result = compute_diff(path.clone(), "", current);
                if result.hunks.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }
        })
        .collect()
}

/// Compute diffs for all changed entries in a [`VirtualFs`].
///
/// Iterates over every non-Original entry, converts it to a
/// [`VirtualFsEntryRef`], and delegates to [`compute_all_virtual_diffs`].
/// Entries whose content has not actually changed produce no diff.
pub fn compute_diffs_from_virtual_fs(vfs: &crate::virtual_fs::VirtualFs) -> Vec<DiffResult> {
    use crate::virtual_fs::VirtualFsEntry;

    let entry_refs: Vec<(Utf8PathBuf, VirtualFsEntryRef<'_>)> = vfs
        .changed_paths()
        .into_iter()
        .filter_map(|path| {
            let entry = vfs.get(path)?;
            match entry {
                VirtualFsEntry::Modified { original, current, .. } => {
                    Some((path.to_path_buf(), VirtualFsEntryRef::Modified { original, current }))
                }
                VirtualFsEntry::Deleted { original, .. } => {
                    Some((path.to_path_buf(), VirtualFsEntryRef::Deleted { original }))
                }
                VirtualFsEntry::Created { current, .. } => {
                    Some((path.to_path_buf(), VirtualFsEntryRef::Created { current }))
                }
                VirtualFsEntry::Original { .. } => None,
            }
        })
        .collect();

    if entry_refs.is_empty() {
        return Vec::new();
    }

    compute_all_virtual_diffs(&entry_refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_identical_content() {
        let result =
            compute_diff(Utf8PathBuf::from("test.txt"), "hello\nworld\n", "hello\nworld\n");
        assert!(result.hunks.is_empty());
    }

    #[test]
    fn diff_single_line_addition() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\n";
        let result = compute_diff(Utf8PathBuf::from("test.txt"), old, new);
        assert!(!result.hunks.is_empty());
        let has_addition = result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| matches!(l, DiffLine::Addition { .. }));
        assert!(has_addition);
    }

    #[test]
    fn diff_single_line_deletion() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nline3\n";
        let result = compute_diff(Utf8PathBuf::from("test.txt"), old, new);
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn diff_empty_new_file() {
        let result = compute_diff(Utf8PathBuf::from("new.txt"), "", "hello\n");
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn diff_deleted_file() {
        let result = compute_diff(Utf8PathBuf::from("old.txt"), "hello\n", "");
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn rejecting_one_change_preserves_other_changes_and_context() {
        let old = "zero\none\ntwo\nthree\nfour\n";
        let new = "zero\nONE\ntwo\nTHREE\nfour\n";
        assert_eq!(change_hunk_count(old, new), 2);
        let rejected = HashSet::from([0]);
        assert_eq!(
            reject_change_hunks(old, new, &rejected).unwrap(),
            "zero\none\ntwo\nTHREE\nfour\n"
        );
    }

    #[test]
    fn diff_single_hunk_change() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let result = compute_diff(Utf8PathBuf::from("test.txt"), old, new);
        // The implementation produces separate context and change hunks:
        // hunk[0] = context before ("a")
        // hunk[1] = the change itself ("b" → "B")
        // hunk[2] = context after  ("c")
        assert_eq!(result.hunks.len(), 3);
        assert!(result.hunks[1].lines.iter().any(|l| matches!(l, DiffLine::Addition { .. })));
        assert!(result.hunks[1].lines.iter().any(|l| matches!(l, DiffLine::Deletion { .. })));
    }

    #[test]
    fn diff_multiple_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "A\nb\nc\nD\ne\nf\nG\nh\n";
        let result = compute_diff(Utf8PathBuf::from("test.txt"), old, new);
        // Three changes should produce three hunks (or one combined depending on proximity)
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn change_hunk_count_detects_no_changes() {
        assert_eq!(change_hunk_count("same\ncontent\n", "same\ncontent\n"), 0);
    }

    #[test]
    fn change_hunk_count_detects_single_change() {
        assert_eq!(change_hunk_count("old\ncontent\n", "new\ncontent\n"), 1);
    }

    #[test]
    fn reject_all_changes_returns_original() {
        let old = "original\ncontent\n";
        let new = "changed\ncontent\n";
        let rejected = HashSet::from([0]);
        assert_eq!(reject_change_hunks(old, new, &rejected).unwrap(), "original\ncontent\n");
    }

    #[test]
    fn reject_change_hunks_empty_hunks_returns_new() {
        let old = "original\n";
        let new = "changed\n";
        let rejected = HashSet::new();
        assert_eq!(reject_change_hunks(old, new, &rejected).unwrap(), "changed\n");
    }

    #[test]
    fn diff_line_equality() {
        let a = DiffLine::Addition { content: "hello".into(), line_num: 1 };
        let b = DiffLine::Addition { content: "hello".into(), line_num: 1 };
        assert_eq!(a, b);
        // Different line_num → not equal.
        let c = DiffLine::Addition { content: "hello".into(), line_num: 2 };
        assert_ne!(a, c);
    }

    #[test]
    fn diff_file_summary_empty_files() {
        let result = compute_diff(Utf8PathBuf::from("empty.txt"), "", "");
        assert!(result.hunks.is_empty());
        assert_eq!(result.path.as_str(), "empty.txt");
    }

    #[test]
    fn compute_diffs_from_virtual_fs_empty() {
        let vfs = crate::virtual_fs::VirtualFs::new();
        assert!(compute_diffs_from_virtual_fs(&vfs).is_empty());
    }

    #[test]
    fn compute_diffs_from_virtual_fs_modified_file() {
        use crate::virtual_fs::{VirtualFs, VirtualFsEntry};
        let mut vfs = VirtualFs::new();
        vfs.insert(VirtualFsEntry::Modified {
            path: Utf8PathBuf::from("a.txt"),
            original: "hello\n".to_string(),
            current: "world\n".to_string(),
        });
        let results = compute_diffs_from_virtual_fs(&vfs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path.as_str(), "a.txt");
        assert!(!results[0].hunks.is_empty());
    }

    #[test]
    fn compute_diffs_from_virtual_fs_skips_unchanged() {
        use crate::virtual_fs::{VirtualFs, VirtualFsEntry};
        let mut vfs = VirtualFs::new();
        // Original entry — should be skipped
        vfs.insert(VirtualFsEntry::Original {
            path: Utf8PathBuf::from("unchanged.txt"),
            content: "same\n".to_string(),
        });
        // Modified — should appear
        vfs.insert(VirtualFsEntry::Modified {
            path: Utf8PathBuf::from("changed.txt"),
            original: "old\n".to_string(),
            current: "new\n".to_string(),
        });
        let results = compute_diffs_from_virtual_fs(&vfs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path.as_str(), "changed.txt");
    }

    #[test]
    fn compute_diffs_from_virtual_fs_created_and_deleted() {
        use crate::virtual_fs::{VirtualFs, VirtualFsEntry};
        let mut vfs = VirtualFs::new();
        vfs.insert(VirtualFsEntry::Created {
            path: Utf8PathBuf::from("new.txt"),
            current: "fresh\ncontent\n".to_string(),
        });
        vfs.insert(VirtualFsEntry::Deleted {
            path: Utf8PathBuf::from("gone.txt"),
            original: "bye\n".to_string(),
        });
        let results = compute_diffs_from_virtual_fs(&vfs);
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.path.as_str() == "new.txt"));
        assert!(results.iter().any(|r| r.path.as_str() == "gone.txt"));
    }
}
