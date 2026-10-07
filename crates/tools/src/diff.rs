use camino::Utf8PathBuf;
use concerto_api_types::diff::{DiffLine, DiffResult, Hunk};
use concerto_core::types::{Condition, PolicyRule};
use concerto_core::PolicyPresets;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::OnceLock;

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

/// Replacement marker for diff line content that matches the `no_secrets`
/// secret pattern.
const REDACTED: &str = "[REDACTED]";

/// The core `no_secrets` `Condition::SecretPattern` regex, compiled once per
/// process (SEC-001: reuse the policy's pattern instead of a local copy).
///
/// Returns `None` only if the preset's fixed pattern fails to compile or the
/// preset stops carrying a `SecretPattern` rule; redaction is then skipped
/// with a warning instead of panicking, mirroring how
/// [`concerto_core::sanitizer`] degrades on an invalid pattern.
fn no_secrets_pattern() -> Option<&'static regex::Regex> {
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    PATTERN
        .get_or_init(|| {
            let Some(pattern) =
                PolicyPresets::no_secrets().into_iter().find_map(|rule| match rule {
                    PolicyRule::AutoDeny(Condition::SecretPattern(pattern)) => Some(pattern),
                    _ => None,
                })
            else {
                tracing::warn!(
                    "core no_secrets preset carries no SecretPattern; diff redaction disabled"
                );
                return None;
            };
            match regex::Regex::new(&pattern) {
                Ok(regex) => Some(regex),
                Err(error) => {
                    tracing::warn!(%error, "invalid no_secrets pattern; diff redaction disabled");
                    None
                }
            }
        })
        .as_ref()
}

/// Replace the content of every diff line matching the core `no_secrets`
/// secret pattern with [`REDACTED`], preserving hunk structure, line counts,
/// and line numbers (SEC-001 compute-time redaction, shared by the CLI and
/// desktop diff views because both consume [`compute_diff`] output).
///
/// The reused pattern matches secret *keywords* (`api_key`, `password`,
/// `token`, ...) rather than the credential value, so redacting only the
/// matched substring would leave the value on screen; a matching line is
/// therefore replaced wholesale. This is display-only: reject/apply paths
/// (`reject_change_hunks`, `VirtualFs::reject_hunks`) keep operating on the
/// raw file content.
fn redact_secret_lines(hunks: &mut [Hunk]) {
    let Some(pattern) = no_secrets_pattern() else {
        return;
    };
    for hunk in hunks {
        for line in &mut hunk.lines {
            match line {
                DiffLine::Addition { content, .. }
                | DiffLine::Deletion { content, .. }
                | DiffLine::Context { content, .. }
                    if pattern.is_match(content) =>
                {
                    content.clear();
                    content.push_str(REDACTED);
                }
                // Either a non-content variant or a line without a secret
                // keyword — `DiffLine` is `#[non_exhaustive]` outside
                // `api-types`, so the wildcard arm stays.
                _ => {}
            }
        }
    }
}

/// Compute a unified diff between two strings.
/// Uses `imara-diff`'s Histogram algorithm for line-level diffs.
/// Returns a `DiffResult` containing all hunks.
///
/// Line content matching the core `no_secrets` secret pattern is replaced
/// with `[REDACTED]` before the result is built (see `redact_secret_lines`);
/// hunk structure and line numbers are preserved.
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

    let mut result = DiffResult { path, hunks };
    redact_secret_lines(&mut result.hunks);
    result
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
///
/// Secret-pattern line content is redacted at compute time by
/// [`compute_diff`], so every consumer of these results (CLI and desktop
/// diff review alike) sees `[REDACTED]` lines instead of raw credentials.
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

    // ---- SEC-001: compute-time secret redaction --------------------------

    /// Every `DiffLine` content string in `result`, in hunk order.
    fn line_contents(result: &DiffResult) -> Vec<&str> {
        result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|line| match line {
                DiffLine::Addition { content, .. }
                | DiffLine::Deletion { content, .. }
                | DiffLine::Context { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn compute_diff_redacts_secret_key_addition() {
        let old = "fn main() {}\n";
        let new = "fn main() {}\nlet api_key = \"sk-proj-abcd1234efgh5678\";\n";
        let result = compute_diff(Utf8PathBuf::from("cfg.rs"), old, new);

        let additions: Vec<&str> = result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|line| match line {
                DiffLine::Addition { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(additions, vec![REDACTED], "secret key line must be redacted");

        let serialized = serde_json::to_string(&result).expect("DiffResult serializes");
        assert!(
            !serialized.contains("sk-proj-abcd1234efgh5678"),
            "raw key material must not survive redaction: {serialized}"
        );
        assert!(serialized.contains(REDACTED));
    }

    #[test]
    fn compute_diff_redacts_password_deletion() {
        let old = "let a = 1;\npassword = \"hunter2secret99\";\nlet b = 2;\n";
        let new = "let a = 1;\nlet b = 2;\n";
        let result = compute_diff(Utf8PathBuf::from("app.env"), old, new);

        let deletions: Vec<&str> = result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|line| match line {
                DiffLine::Deletion { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deletions, vec![REDACTED], "password line must be redacted");

        let serialized = serde_json::to_string(&result).expect("DiffResult serializes");
        assert!(
            !serialized.contains("hunter2secret99"),
            "raw password must not survive redaction: {serialized}"
        );
    }

    #[test]
    fn compute_diff_redacts_token_context_line() {
        let old = "auth_token = load_token();\nlet x = 1;\n";
        let new = "auth_token = load_token();\nlet x = 2;\n";
        let result = compute_diff(Utf8PathBuf::from("auth.rs"), old, new);

        let context: Vec<(&str, u64)> = result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|line| match line {
                DiffLine::Context { content, line_num } => Some((content.as_str(), *line_num)),
                _ => None,
            })
            .collect();
        assert_eq!(
            context,
            vec![(REDACTED, 1)],
            "token-bearing context line must be redacted with its line number preserved"
        );

        let serialized = serde_json::to_string(&result).expect("DiffResult serializes");
        assert!(
            !serialized.contains("load_token"),
            "raw token reference must not survive redaction: {serialized}"
        );
    }

    #[test]
    fn compute_diff_keeps_clean_lines_unchanged() {
        let old = "fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let new = "fn add(a: u32, b: u32) -> u32 {\n    a + b + 1\n}\n";
        let result = compute_diff(Utf8PathBuf::from("math.rs"), old, new);

        assert!(!result.hunks.is_empty(), "a change must still produce hunks");
        let contents = line_contents(&result);
        assert!(
            contents.iter().all(|c| !c.contains(REDACTED)),
            "clean lines must not be redacted: {contents:?}"
        );
        assert!(
            contents.contains(&"fn add(a: u32, b: u32) -> u32 {"),
            "clean context must pass through verbatim: {contents:?}"
        );
        assert!(contents.contains(&"    a + b + 1"), "clean addition verbatim: {contents:?}");
    }

    #[test]
    fn redaction_preserves_hunk_structure_and_line_numbers() {
        let old = "let a = 1;\npassword = \"hunter2secret99\";\nlet b = 2;\n";
        let new = "let a = 1;\nlet b = 2;\n";
        let result = compute_diff(Utf8PathBuf::from("app.env"), old, new);

        // Structure identical to an unredacted diff: context, change, context.
        assert_eq!(result.hunks.len(), 3);

        let leading = &result.hunks[0];
        assert_eq!(
            (leading.old_start, leading.old_len, leading.new_start, leading.new_len),
            (1, 1, 1, 1)
        );
        assert_eq!(
            leading.lines,
            vec![DiffLine::Context { content: "let a = 1;".into(), line_num: 1 }]
        );

        let change = &result.hunks[1];
        assert_eq!(
            (change.old_start, change.old_len, change.new_start, change.new_len),
            (2, 1, 2, 0),
            "hunk ranges must not shift under redaction"
        );
        assert_eq!(
            change.lines,
            vec![DiffLine::Deletion { content: REDACTED.into(), line_num: 2 }],
            "line count and line_num preserved while content is redacted"
        );

        let trailing = &result.hunks[2];
        assert_eq!(
            (trailing.old_start, trailing.old_len, trailing.new_start, trailing.new_len),
            (3, 1, 2, 1)
        );
        assert_eq!(
            trailing.lines,
            vec![DiffLine::Context { content: "let b = 2;".into(), line_num: 3 }]
        );
    }

    #[test]
    fn binary_placeholder_is_not_redacted() {
        // Same shape git.rs substitutes for non-UTF-8 blobs; none of the
        // no_secrets keywords appear, so it must pass through verbatim.
        let placeholder = "[binary file: 128 bytes — contents not decoded]";
        let result = compute_diff(Utf8PathBuf::from("blob.bin"), "", &format!("{placeholder}\n"));

        let additions: Vec<&str> = result
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter_map(|line| match line {
                DiffLine::Addition { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(additions, vec![placeholder], "binary placeholder must stay unchanged");
    }

    #[test]
    fn compute_diffs_from_virtual_fs_redacts_secret_lines() {
        use crate::virtual_fs::{VirtualFs, VirtualFsEntry};
        let mut vfs = VirtualFs::new();
        vfs.insert(VirtualFsEntry::Modified {
            path: Utf8PathBuf::from(".env"),
            original: "debug=true\n".to_string(),
            current: "debug=true\npassword = \"hunter2secret99\"\n".to_string(),
        });

        let results = compute_diffs_from_virtual_fs(&vfs);
        assert_eq!(results.len(), 1);

        let serialized = serde_json::to_string(&results).expect("results serialize");
        assert!(serialized.contains(REDACTED), "marker missing: {serialized}");
        assert!(
            !serialized.contains("hunter2secret99"),
            "raw secret must not reach diff consumers: {serialized}"
        );
        assert!(serialized.contains("debug=true"), "clean lines must stay readable: {serialized}");
    }
}
