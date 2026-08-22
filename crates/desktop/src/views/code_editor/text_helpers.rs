use iced::widget::text_editor;

use super::{
    ActiveFold, BracketStatus, EditKind, FindMatch, MAX_BRACKET_SCAN, MAX_MATCHES, MAX_SEARCH_BYTES,
};

// ---------------------------------------------------------------------------
// Pure editing helpers
// ---------------------------------------------------------------------------

/// Classify a text edit for undo-history grouping.
pub fn classify_edit(edit: &text_editor::Edit) -> EditKind {
    match edit {
        text_editor::Edit::Insert(_) => EditKind::InsertChar,
        text_editor::Edit::Enter => EditKind::Newline,
        text_editor::Edit::Backspace | text_editor::Edit::Delete => EditKind::Delete,
        text_editor::Edit::Paste(_) => EditKind::Paste,
        text_editor::Edit::Indent | text_editor::Edit::Unindent => EditKind::Indent,
    }
}

/// Whether an edit of `kind` should start a fresh undo entry. Structural
/// edits always do; typing/deleting bursts merge with the previous entry.
pub fn should_snapshot(last: Option<EditKind>, kind: EditKind) -> bool {
    match kind {
        EditKind::Newline | EditKind::Paste | EditKind::Indent => true,
        EditKind::InsertChar | EditKind::Delete => last != Some(kind),
    }
}

/// Compute what should follow an Enter keypress, given the line text before
/// the cursor. Returns `None` for a plain Enter. Comment lines continue their
/// marker; pressing Enter on an otherwise-empty comment line exits the
/// comment. Other lines continue their indentation.
pub fn continuation_prefix(line_before_cursor: &str) -> Option<String> {
    let indent_len = line_before_cursor.len() - line_before_cursor.trim_start().len();
    let indent = &line_before_cursor[..indent_len];
    let trimmed = &line_before_cursor[indent_len..];
    if trimmed.is_empty() {
        return None;
    }
    // Longest markers first so `///` wins over `//`.
    const MARKERS: &[(&str, &str)] = &[
        ("<!--", "<!-- "),
        ("///", "/// "),
        ("//!", "//! "),
        ("//", "// "),
        ("--", "-- "),
        ("#", "# "),
        ("/*", "* "),
        ("*", "* "),
    ];
    for (marker, continuation) in MARKERS {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            if rest.trim().is_empty() {
                return None;
            }
            return Some(format!("{indent}{continuation}"));
        }
    }
    Some(indent.to_string())
}

/// Strip trailing spaces/tabs from every line and ensure the file ends with
/// exactly one newline. Empty input stays empty.
pub fn trim_trailing_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t']));
    }
    let collapsed = out.trim_end_matches('\n');
    if collapsed.is_empty() {
        // Empty or whitespace-only buffers normalize to empty.
        return String::new();
    }
    let mut result = collapsed.to_string();
    result.push('\n');
    result
}

/// Clamp a (line, byte-column) cursor into the buffer's bounds, snapping the
/// column down to a UTF-8 char boundary. Note iced's `Position.column` is a
/// byte offset (it maps directly to cosmic-text's cursor index).
pub fn clamp_cursor(
    content: &text_editor::Content,
    line: usize,
    column: usize,
) -> text_editor::Cursor {
    let line_count = content.line_count().max(1);
    let line = line.min(line_count - 1);
    let mut column = column;
    if let Some(l) = content.line(line) {
        column = column.min(l.text.len());
        while column > 0 && !l.text.is_char_boundary(column) {
            column -= 1;
        }
    } else {
        column = 0;
    }
    text_editor::Cursor { position: text_editor::Position { line, column }, selection: None }
}

/// 1-based (line, column) for the status bar. The column counts characters,
/// not bytes, so multi-byte text displays naturally.
pub fn cursor_line_col(content: &text_editor::Content) -> (usize, usize) {
    let cursor = content.cursor();
    let col = content
        .line(cursor.position.line)
        .and_then(|l| l.text.get(..cursor.position.column).map(|s| s.chars().count()))
        .unwrap_or(cursor.position.column);
    (cursor.position.line + 1, col + 1)
}

/// Scan `text` for non-overlapping occurrences of `query`, returning matches
/// with byte offset, line, and byte-column. Case-insensitive mode uses ASCII
/// comparison so byte offsets stay exact for any UTF-8 content. Bounded:
/// buffers over MAX_SEARCH_BYTES are skipped, and collection stops at
/// MAX_MATCHES (the bool reports overflow).
pub fn find_matches_in(text: &str, query: &str, case_sensitive: bool) -> (Vec<FindMatch>, bool) {
    let mut matches = Vec::new();
    if query.is_empty() || text.len() > MAX_SEARCH_BYTES {
        return (matches, false);
    }
    let bytes = text.as_bytes();
    let mut line = 0usize;
    let mut line_start = 0usize;
    let mut pos = 0usize;
    let mut overflow = false;
    while pos < bytes.len() {
        if bytes[pos] == b'\n' {
            line += 1;
            line_start = pos + 1;
            pos += 1;
            continue;
        }
        let end = pos + query.len();
        if end <= bytes.len() {
            // `get` is None at non-char boundaries — safe to skip one byte.
            if let Some(window) = text.get(pos..end) {
                let hit = if case_sensitive {
                    window == query
                } else {
                    window.eq_ignore_ascii_case(query)
                };
                if hit {
                    matches.push(FindMatch { offset: pos, line, col: pos - line_start });
                    if matches.len() >= MAX_MATCHES {
                        overflow = true;
                        break;
                    }
                    // Track lines crossed inside the match (multi-line query).
                    for (i, &b) in bytes[pos..end].iter().enumerate() {
                        if b == b'\n' {
                            line += 1;
                            line_start = pos + i + 1;
                        }
                    }
                    pos = end;
                    continue;
                }
            }
        }
        pos += 1;
    }
    (matches, overflow)
}

/// Convert a byte offset into a 0-based (line, byte-column) pair.
pub fn offset_to_line_col(text: &str, target: usize) -> (usize, usize) {
    let target = target.min(text.len());
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if i >= target {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, target - line_start)
}

/// Byte offset of the editor cursor in the full text (lines joined with the
/// buffer's line ending, mirroring `Content::text()`).
pub fn cursor_byte_offset(content: &text_editor::Content) -> usize {
    let cursor = content.cursor();
    let nl = match content.line_ending() {
        Some(ending) if ending != text_editor::LineEnding::None => ending.as_str().len(),
        // No explicit ending detected: `Content::text()` falls back to the
        // platform default, so mirror that here.
        _ => text_editor::LineEnding::default().as_str().len(),
    };
    let mut offset = 0usize;
    for i in 0..cursor.position.line {
        let Some(l) = content.line(i) else {
            break;
        };
        offset += l.text.len() + nl;
    }
    offset + cursor.position.column
}

/// Whether a byte is a "word" character (ASCII letters, digits, underscore).
/// Multi-byte UTF-8 never matches, so byte scanning stays boundary-safe.
pub fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Find the span of the word adjacent to `cursor` (either side counts, like
/// Kate's word-under-cursor). Words shorter than 2 chars are ignored.
pub fn find_word_span(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());
    let before = cursor > 0 && is_word_char(bytes[cursor - 1]);
    let after = cursor < bytes.len() && is_word_char(bytes[cursor]);
    if !before && !after {
        return None;
    }
    let mut start = cursor;
    let mut end = cursor;
    while start > 0 && is_word_char(bytes[start - 1]) {
        start -= 1;
    }
    while end < bytes.len() && is_word_char(bytes[end]) {
        end += 1;
    }
    (end - start >= 2).then_some((start, end))
}

/// Collect the 0-based line indices of every whole-word occurrence of `word`.
/// Bounded by MAX_SEARCH_BYTES; lines are deduplicated.
pub fn word_occurrence_lines(text: &str, word: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    if word.len() < 2 || text.len() > MAX_SEARCH_BYTES {
        return lines;
    }
    let bytes = text.as_bytes();
    let wlen = word.len();
    let mut line = 0usize;
    let mut pos = 0usize;
    while pos + wlen <= bytes.len() {
        if bytes[pos] == b'\n' {
            line += 1;
            pos += 1;
            continue;
        }
        if text.get(pos..pos + wlen) == Some(word) {
            let left_ok = pos == 0 || !is_word_char(bytes[pos - 1]);
            let right_ok = pos + wlen >= bytes.len() || !is_word_char(bytes[pos + wlen]);
            if left_ok && right_ok {
                if lines.last().copied() != Some(line) {
                    lines.push(line);
                }
                // Words contain no newlines, so skipping ahead is safe.
                pos += wlen;
                continue;
            }
        }
        pos += 1;
    }
    lines
}

/// Find the partner of the bracket adjacent to the cursor (preferring the
/// char before the cursor, like Kate). Byte-based scan with a depth counter,
/// bounded to MAX_BRACKET_SCAN bytes. Limitation: brackets inside strings and
/// comments are counted (no syntax awareness).
pub fn find_bracket_match(text: &str, cursor: usize) -> BracketStatus {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return BracketStatus::None;
    }
    let cursor = cursor.min(bytes.len());
    let (pos, b) = if cursor > 0 && is_bracket(bytes[cursor - 1]) {
        (cursor - 1, bytes[cursor - 1])
    } else if cursor < bytes.len() && is_bracket(bytes[cursor]) {
        (cursor, bytes[cursor])
    } else {
        return BracketStatus::None;
    };
    let (open, close, forward) = match b {
        b'(' => (b'(', b')', true),
        b'[' => (b'[', b']', true),
        b'{' => (b'{', b'}', true),
        b')' => (b'(', b')', false),
        b']' => (b'[', b']', false),
        b'}' => (b'{', b'}', false),
        _ => return BracketStatus::None,
    };
    let mut depth = 1usize;
    if forward {
        let limit = (pos + 1).saturating_add(MAX_BRACKET_SCAN).min(bytes.len());
        for (i, &ch) in bytes[pos + 1..limit].iter().enumerate() {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    let (other_line, other_col) = offset_to_line_col(text, pos + 1 + i);
                    return BracketStatus::Matched { other_line, other_col };
                }
            }
        }
    } else {
        let floor = pos.saturating_sub(MAX_BRACKET_SCAN);
        let mut i = pos;
        while i > floor {
            i -= 1;
            let ch = bytes[i];
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    let (other_line, other_col) = offset_to_line_col(text, i);
                    return BracketStatus::Matched { other_line, other_col };
                }
            }
        }
    }
    BracketStatus::Unmatched
}

fn is_bracket(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'[' | b']' | b'{' | b'}')
}

/// Compute indentation-based foldable regions as inclusive `(start, end)`
/// line pairs: `start` is the anchor (stays visible), lines `start+1..=end`
/// are hidden when folded. Blank lines belong to the enclosing region and
/// never close it. Sorted by start line; nested regions are included.
pub fn compute_fold_regions(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut regions = Vec::new();
    // Stack of (indent, start_line) for open regions.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        while let Some(&(open_indent, start)) = stack.last() {
            if indent <= open_indent {
                stack.pop();
                if i > start + 1 {
                    regions.push((start, i - 1));
                }
            } else {
                break;
            }
        }
        stack.push((indent, i));
    }
    let last = lines.len().saturating_sub(1);
    while let Some((_, start)) = stack.pop() {
        if last > start {
            regions.push((start, last));
        }
    }
    regions.sort_unstable();
    regions.dedup();
    regions
}

/// Keep only regions not contained inside another (for Fold All, which must
/// not produce overlapping folds).
pub fn outermost_regions(regions: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &r in regions {
        if let Some(last) = out.last() {
            if r.0 > last.0 && r.1 <= last.1 {
                continue;
            }
        }
        out.push(r);
    }
    out
}

/// Substitute each region's hidden lines with a single placeholder line.
/// Regions must be ascending and non-overlapping. Returns the new buffer and
/// the fold records (with `start` in the NEW buffer's display coordinates).
pub fn fold_regions_in_text(text: &str, regions: &[(usize, usize)]) -> (String, Vec<ActiveFold>) {
    let trailing_nl = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut folds = Vec::new();
    let mut shift = 0usize; // net lines removed by earlier folds
    for &(start, end) in regions {
        if end < start + 1 || start >= lines.len() {
            continue;
        }
        let dstart = start - shift;
        let dend = (end - shift).min(lines.len() - 1);
        if dstart >= dend {
            continue;
        }
        let hidden: Vec<String> = lines.drain(dstart + 1..=dend).collect();
        let count = hidden.len();
        let hidden_text = hidden.join("\n");
        lines.insert(dstart + 1, format!(" \u{22ef} {count} folded lines \u{22ef}"));
        shift += count.saturating_sub(1);
        folds.push(ActiveFold { start: dstart, hidden_count: count, hidden_text });
    }
    let mut out = lines.join("\n");
    if trailing_nl {
        out.push('\n');
    }
    (out, folds)
}

/// Pure full expansion: splice every fold's hidden text back over its
/// placeholder line (bottom-up so anchors stay valid).
pub fn expand_all_in_text(text: &str, folds: &[ActiveFold]) -> String {
    if folds.is_empty() {
        return text.to_string();
    }
    let trailing_nl = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut sorted: Vec<&ActiveFold> = folds.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.start));
    for f in sorted {
        let pos = f.start + 1;
        if pos >= lines.len() {
            continue;
        }
        let hidden: Vec<String> = f.hidden_text.split('\n').map(str::to_string).collect();
        lines.splice(pos..=pos, hidden);
    }
    let mut out = lines.join("\n");
    if trailing_nl {
        out.push('\n');
    }
    out
}

/// Map a line number from the real (pre-fold) buffer to the folded display
/// buffer: inside a hidden region → its anchor; below → shifted up.
/// `regions` and `folds` are parallel (same fold operation).
pub fn map_line_on_fold(line: usize, regions: &[(usize, usize)], folds: &[ActiveFold]) -> usize {
    let mut shift = 0usize;
    for (i, &(start, end)) in regions.iter().enumerate() {
        if line > start && line <= end {
            return folds.get(i).map(|f| f.start).unwrap_or(line.saturating_sub(shift));
        }
        if line > end {
            shift += (end - start).saturating_sub(1);
        }
    }
    line.saturating_sub(shift)
}

/// Map a line number from the folded display buffer to the expanded buffer:
/// on a placeholder line → its anchor; below → shifted down. `folds` are the
/// regions being expanded (ascending display anchors).
pub fn map_line_on_expand(line: usize, folds: &[ActiveFold]) -> usize {
    let mut shift = 0usize;
    for f in folds {
        if line == f.start + 1 {
            return f.start + shift;
        }
        if line > f.start + 1 {
            shift += f.hidden_count - 1;
        } else {
            break;
        }
    }
    line + shift
}

/// The (lo, hi) display-line range an edit touches: the selection bounds, or
/// just the caret line.
pub fn selection_line_range(content: &text_editor::Content) -> (usize, usize) {
    let cursor = content.cursor();
    match cursor.selection {
        Some(sel) => {
            let lo = sel.line.min(cursor.position.line);
            let hi = sel.line.max(cursor.position.line);
            (lo, hi)
        }
        None => (cursor.position.line, cursor.position.line),
    }
}

/// Splice `replacement` over every match. Matches are in ascending offset
/// order (guaranteed by `find_matches_in`), making this a single O(n) pass.
pub fn replace_all_from_matches(
    text: &str,
    matches: &[FindMatch],
    query_len: usize,
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for m in matches {
        let end = m.offset + query_len;
        if m.offset < last || end > text.len() {
            continue;
        }
        out.push_str(&text[last..m.offset]);
        out.push_str(replacement);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}
