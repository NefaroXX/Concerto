//! Hunk-aware write staging (ADR-60 D5, conflict policy for shared files).
//!
//! When two agents concurrently write the same file, the file-level
//! `base_version` check refuses the loser wholesale. This module refines that
//! refusal: both writes are diffed against their **shared base** (the version
//! the loser declared), and only if their changed line ranges overlap is the
//! collision surfaced loudly ([`HunkStaging::Collision`]). Disjoint hunks are
//! **staged**: the late write's hunks are spliced onto the sibling's current
//! content ([`HunkStaging::Merged`]), so neither side's edits are lost.
//!
//! Splicing matters — simply "allowing" the late whole-file write would
//! silently revert the sibling's hunks, which is exactly the silent-loss
//! mechanism ADR-60 rejects ("conflict-free ≠ intent-free"). Same-position
//! insertions and insertions strictly inside a sibling's replaced/deleted
//! block count as overlapping (conservative: loud beats silent).
//!
//! Deliberate limits (each degrades to the file-level `base_version` check,
//! never to silent last-writer-wins):
//! - Content containing `\r` is refused: `str::lines()` + `"\n"` joining
//!   would normalize CRLF files as a side effect of a conflict check.
//! - Non-UTF-8 (binary) content cannot be line-diffed.
//! - The shared base text must be recoverable (the gate caches observed
//!   pre-image bytes); a base it never saw cannot be diffed.
//!
//! Diffing uses `imara-diff`'s Histogram algorithm, per ADR-05.

use imara_diff::intern::InternedInput;
use imara_diff::{diff, Algorithm};
use std::ops::Range;

/// One contiguous change between two texts: the replaced/deleted/inserted-at
/// line range in the OLD text (`before`) and the replacing range in the NEW
/// text (`after`). An empty `before` is a pure insertion at that position; an
/// empty `after` is a pure deletion. Ranges are 0-based, end-exclusive, in
/// `str::lines()` coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineChange {
    pub before: Range<u32>,
    pub after: Range<u32>,
}

/// Outcome of a hunk-aware staging attempt for one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HunkStaging {
    /// The write's hunks do not overlap the sibling's: `merged` is the
    /// sibling's current content with the write's hunks spliced in. Executing
    /// this content loses nothing from either side.
    Merged(String),
    /// Same-hunk collision: both edits touch the same base lines. The detail
    /// names the colliding ranges for the manual-resolution path.
    Collision(String),
    /// Staging could not be attempted safely (CRLF/binary/unavailable base).
    /// The caller must fall back to the file-level `base_version` conflict
    /// check — never to silent last-writer-wins. Carries the reason.
    NotApplicable(String),
}

/// Compute the changed line ranges between two texts with imara-diff's
/// Histogram algorithm (the ADR-05 convention shared with
/// `concerto_tools::diff`).
fn changed_line_ranges(old: &str, new: &str) -> Vec<LineChange> {
    let input = InternedInput::new(old, new);
    let mut changes = Vec::new();
    diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| {
        changes.push(LineChange { before, after });
    });
    changes
}

/// Whether two changed ranges in the SAME base text overlap.
///
/// Non-empty ranges use standard interval intersection. An insertion (empty
/// range) collides with another insertion only at the exact same base point,
/// and with a replacement/deletion only strictly inside it — an edit adjacent
/// to a sibling hunk is not an overlap.
fn ranges_overlap(a: &Range<u32>, b: &Range<u32>) -> bool {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => a.start == b.start,
        (true, false) => b.start < a.start && a.start < b.end,
        (false, true) => a.start < b.start && b.start < a.end,
        (false, false) => a.start < b.end && b.start < a.end,
    }
}

/// Map a point in BASE line coordinates to the corresponding point in
/// CURRENT's coordinates, given the sibling's changes (`theirs`, ascending).
///
/// Only valid for points outside every `theirs.before` range (guaranteed by
/// the collision check): each sibling hunk entirely before `point` replaces
/// its base lines with its own (net delta applied), and unchanged base lines
/// between hunks pass through 1:1.
fn map_point_to_current(point: u32, theirs: &[LineChange]) -> usize {
    let mut after_lines = 0usize;
    let mut before_lines = 0u32;
    for change in theirs {
        if change.before.end <= point {
            let len = change.after.end.saturating_sub(change.after.start);
            after_lines += len as usize;
            before_lines += change.before.end - change.before.start;
        } else {
            break;
        }
    }
    after_lines + (point - before_lines) as usize
}

/// Attempt hunk-aware three-way staging (see module docs).
///
/// `base` is the text the writer declared (its `base_version`), `current` is
/// what is on disk now (a sibling diverged from the same base), and
/// `proposed` is the writer's full new content.
pub(crate) fn stage_three_way(base: &str, current: &str, proposed: &str) -> HunkStaging {
    // CRLF guard: rejoining `lines()` with "\n" would silently rewrite line
    // endings — refuse rather than mutate bytes as a side effect.
    if [base, current, proposed].iter().any(|text| text.contains('\r')) {
        return HunkStaging::NotApplicable(
            "content contains carriage returns (line-ending normalization hazard)".to_owned(),
        );
    }

    let ours = changed_line_ranges(base, proposed);
    let theirs = changed_line_ranges(base, current);

    // The write changes nothing relative to its declared base. Writing
    // `proposed` verbatim would REVERT the sibling's hunks; staging the empty
    // hunk set onto current keeps their state (a harmless no-op rewrite).
    if ours.is_empty() {
        return HunkStaging::Merged(current.to_owned());
    }

    // Same-hunk collisions surface loudly with resolution context.
    for our_change in &ours {
        for their_change in &theirs {
            if ranges_overlap(&our_change.before, &their_change.before) {
                return HunkStaging::Collision(format!(
                    "same-hunk collision on the shared base: this edit touches base lines {}..{}, \
                     the sibling edit touches base lines {}..{} — manual resolution required",
                    our_change.before.start + 1,
                    our_change.before.end,
                    their_change.before.start + 1,
                    their_change.before.end,
                ));
            }
        }
    }

    // Disjoint hunks: splice our hunks (ascending) onto the sibling's current
    // content, translating each base interval through their changes.
    let current_lines: Vec<&str> = current.lines().collect();
    let proposed_lines: Vec<&str> = proposed.lines().collect();
    let mut merged: Vec<&str> = Vec::new();
    let mut cursor = 0usize;
    for our_change in &ours {
        let start = map_point_to_current(our_change.before.start, &theirs).min(current_lines.len());
        let end = map_point_to_current(our_change.before.end, &theirs).min(current_lines.len());
        merged.extend_from_slice(&current_lines[cursor..start]);
        let p_start = (our_change.after.start as usize).min(proposed_lines.len());
        let p_end = (our_change.after.end as usize).min(proposed_lines.len());
        merged.extend_from_slice(&proposed_lines[p_start..p_end]);
        cursor = end;
    }
    merged.extend_from_slice(&current_lines[cursor.min(current_lines.len())..]);

    // Trailing-newline preservation: whichever text contributed the final
    // merged line decides whether the file ends with '\n'.
    let ends_with_spliced_content = cursor >= current_lines.len();
    let trailing_newline =
        if ends_with_spliced_content { proposed.ends_with('\n') } else { current.ends_with('\n') };

    let mut content = merged.join("\n");
    if trailing_newline && !content.is_empty() {
        content.push('\n');
    }
    HunkStaging::Merged(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules_cover_insertions_and_replacements() {
        let replacement_3_7 = 3..7;
        // Standard interval overlap / disjointness.
        assert!(ranges_overlap(&(4..6), &replacement_3_7));
        assert!(!ranges_overlap(&(0..3), &replacement_3_7), "adjacent is not overlapping");
        assert!(!ranges_overlap(&(7..9), &replacement_3_7));
        // Insertion strictly inside a replacement collides...
        assert!(ranges_overlap(&(5..5), &(3..7)));
        // ...but touching either edge does not.
        assert!(!ranges_overlap(&(3..3), &(3..7)));
        assert!(!ranges_overlap(&(7..7), &(3..7)));
        // Two insertions collide only at the identical point.
        assert!(ranges_overlap(&(5..5), &(5..5)));
        assert!(!ranges_overlap(&(4..4), &(5..5)));
    }

    #[test]
    fn disjoint_edits_stage_onto_the_sibling_state() {
        let base = "l01\nl02\nl03\nl04\nl05\n";
        // Sibling rewrote line 2 first.
        let current = "l01\nSIBLING\nl03\nl04\nl05\n";
        // Late write rewrote line 4 off the ORIGINAL base.
        let proposed = "l01\nl02\nl03\nLATE\nl05\n";

        let staged = stage_three_way(base, current, proposed);
        assert_eq!(
            staged,
            HunkStaging::Merged("l01\nSIBLING\nl03\nLATE\nl05\n".to_owned()),
            "both edits survive: sibling's line 2 AND late writer's line 4"
        );
    }

    #[test]
    fn same_line_edits_collide_with_resolution_detail() {
        let base = "a\nb\nc\n";
        let current = "a\nTHEIRS\nc\n";
        let proposed = "a\nOURS\nc\n";
        match stage_three_way(base, current, proposed) {
            HunkStaging::Collision(detail) => {
                assert!(detail.contains("same-hunk"), "{detail}");
                assert!(detail.contains("manual resolution"), "{detail}");
            }
            other => panic!("expected Collision, got {other:?}"),
        }
    }

    #[test]
    fn no_op_write_never_reverts_the_sibling() {
        let base = "keep\nmine\n";
        let current = "keep\nmine\nsibling-added\n";
        // Proposed == base: zero hunks. Verbatim execution would drop the
        // sibling's appended line; staging must keep it instead.
        let staged = stage_three_way(base, current, base);
        assert_eq!(staged, HunkStaging::Merged(current.to_owned()));
    }

    #[test]
    fn trailing_newline_follows_the_final_contributor() {
        // Last spliced chunk comes from proposed (no current tail): proposed's
        // missing trailing newline wins.
        let base = "one\ntwo\nthree\n";
        let current = "ONE\ntwo\nthree\n";
        let proposed = "one\ntwo\nTHREE"; // no trailing newline
        assert_eq!(
            stage_three_way(base, current, proposed),
            HunkStaging::Merged("ONE\ntwo\nTHREE".to_owned())
        );

        // Current tail survives past the splice: current's newline wins.
        let proposed = "one\ntwo\nTHREE\nfour\n";
        assert_eq!(
            stage_three_way(base, current, proposed),
            HunkStaging::Merged("ONE\ntwo\nTHREE\nfour\n".to_owned())
        );
    }

    #[test]
    fn carriage_returns_refuse_to_diff() {
        let staged = stage_three_way("a\r\nb\r\n", "a\r\nB\r\n", "a\r\nb\r\nc\r\n");
        match staged {
            HunkStaging::NotApplicable(reason) => {
                assert!(reason.contains("carriage"), "CRLF refusal explains itself: {reason}");
            }
            other => panic!("expected NotApplicable for CRLF content, got {other:?}"),
        }
    }

    #[test]
    fn multi_hunk_splice_translates_through_sibling_deletions() {
        let base = "l01\nl02\nl03\nl04\nl05\nl06\nl07\n";
        // Sibling DELETED lines 2-3 (base coords [1..3)).
        let current = "l01\nl04\nl05\nl06\nl07\n";
        // Late write edits old lines 5 and 7 (base coords [4..5), [6..7)),
        // both after the deleted block.
        let proposed = "l01\nl02\nl03\nl04\nE5\nl06\nE7\n";

        let staged = stage_three_way(base, current, proposed);
        assert_eq!(
            staged,
            HunkStaging::Merged("l01\nl04\nE5\nl06\nE7\n".to_owned()),
            "spliced positions shift by the sibling's net deletions"
        );
    }
}
