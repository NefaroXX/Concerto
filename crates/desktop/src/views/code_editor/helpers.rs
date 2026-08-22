use iced::widget::text_editor;

/// Compute the LSP cursor position `(line, utf16_col)` from the editor content.
/// LSP positions use UTF-16 code-unit offsets for `character` by default (the
/// client capability `positionEncoding` defaults to "utf-16").
pub(crate) fn lsp_position(content: &text_editor::Content) -> (usize, usize) {
    let cursor = content.cursor();
    let line = cursor.position.line;
    let utf16_col = content
        .line(line)
        .and_then(|l| {
            l.text.get(..cursor.position.column).map(|s| s.chars().map(|c| c.len_utf16()).sum())
        })
        .unwrap_or(0);
    (line, utf16_col)
}

/// Convert an LSP UTF-16 column offset to a byte offset within a line string.
pub(crate) fn utf16_col_to_byte(line: &str, utf16_col: usize) -> usize {
    let mut sum = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if sum >= utf16_col {
            return byte_idx;
        }
        sum += ch.len_utf16();
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{lsp_position, utf16_col_to_byte};
    use std::sync::{Arc, Mutex};

    use camino::Utf8PathBuf;
    use concerto_core::CancellationToken;
    use concerto_tools::virtual_fs::VirtualFs;
    use iced::widget::text_editor;

    #[test]
    fn continuation_prefix_continues_line_comments() {
        assert_eq!(continuation_prefix("// hello"), Some("// ".to_string()));
        assert_eq!(continuation_prefix("    // hello"), Some("    // ".to_string()));
        assert_eq!(continuation_prefix("/// doc"), Some("/// ".to_string()));
        assert_eq!(continuation_prefix("# comment"), Some("# ".to_string()));
        assert_eq!(continuation_prefix("-- lua"), Some("-- ".to_string()));
        assert_eq!(continuation_prefix(" * block"), Some(" * ".to_string()));
    }

    #[test]
    fn continuation_prefix_exits_empty_comment() {
        assert_eq!(continuation_prefix("//"), None);
        assert_eq!(continuation_prefix("    // "), None);
        assert_eq!(continuation_prefix("#"), None);
    }

    #[test]
    fn continuation_prefix_preserves_indentation() {
        assert_eq!(continuation_prefix("    fn main() {"), Some("    ".to_string()));
        assert_eq!(continuation_prefix("let x = 1;"), Some(String::new()));
        assert_eq!(continuation_prefix("    "), None);
        assert_eq!(continuation_prefix(""), None);
    }

    #[test]
    fn trim_trailing_whitespace_strips_and_normalizes() {
        assert_eq!(trim_trailing_whitespace("a  \nb\t\n"), "a\nb\n");
        assert_eq!(trim_trailing_whitespace("abc"), "abc\n");
        assert_eq!(trim_trailing_whitespace("a\n\n\n"), "a\n");
        assert_eq!(trim_trailing_whitespace(""), "");
        assert_eq!(trim_trailing_whitespace("   "), "");
        assert_eq!(trim_trailing_whitespace("a b  c\n"), "a b  c\n");
    }

    #[test]
    fn tab_mode_cycles_through_all_modes() {
        assert_eq!(TabMode::Tabs.cycle(), TabMode::Spaces(2));
        assert_eq!(TabMode::Spaces(2).cycle(), TabMode::Spaces(4));
        assert_eq!(TabMode::Spaces(4).cycle(), TabMode::Spaces(8));
        assert_eq!(TabMode::Spaces(8).cycle(), TabMode::Tabs);
        assert_eq!(TabMode::Tabs.label(), "Tabs");
        assert_eq!(TabMode::Spaces(4).label(), "Spaces:4");
        assert_eq!(TabMode::Spaces(4).width(), 4);
    }

    #[test]
    fn snapshot_grouping_merges_typing_bursts() {
        assert!(should_snapshot(None, EditKind::InsertChar));
        assert!(!should_snapshot(Some(EditKind::InsertChar), EditKind::InsertChar));
        assert!(should_snapshot(Some(EditKind::InsertChar), EditKind::Delete));
        assert!(!should_snapshot(Some(EditKind::Delete), EditKind::Delete));
        // Structural edits always start a fresh entry.
        assert!(should_snapshot(Some(EditKind::InsertChar), EditKind::Newline));
        assert!(should_snapshot(Some(EditKind::Newline), EditKind::Newline));
        assert!(should_snapshot(Some(EditKind::Paste), EditKind::Paste));
        assert!(should_snapshot(Some(EditKind::Indent), EditKind::Indent));
    }

    #[test]
    fn cursor_line_col_counts_chars_not_bytes() {
        let content = text_editor::Content::with_text("héllo\nworld");
        let (line, col) = cursor_line_col(&content);
        assert_eq!((line, col), (1, 1));
    }

    #[test]
    fn clamp_cursor_stays_in_bounds() {
        let content = text_editor::Content::with_text("ab\ncd");
        let c = clamp_cursor(&content, 99, 99);
        assert_eq!(c.position.line, 1);
        assert!(c.position.column <= 2);
        let c = clamp_cursor(&content, 0, 1);
        assert_eq!((c.position.line, c.position.column), (0, 1));
    }

    #[test]
    fn find_matches_basic_and_line_tracking() {
        let (matches, overflow) = find_matches_in("foo bar\nfoo baz\nfoo", "foo", true);
        assert!(!overflow);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0], FindMatch { offset: 0, line: 0, col: 0 });
        assert_eq!(matches[1], FindMatch { offset: 8, line: 1, col: 0 });
        assert_eq!(matches[2], FindMatch { offset: 16, line: 2, col: 0 });
    }

    #[test]
    fn find_matches_case_insensitive_ascii() {
        let (matches, _) = find_matches_in("Foo fOO foo", "foo", false);
        assert_eq!(matches.len(), 3);
        // Case-sensitive finds only the exact one.
        let (matches, _) = find_matches_in("Foo fOO foo", "foo", true);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].offset, 8);
    }

    #[test]
    fn find_matches_non_overlapping_and_empty_query() {
        let (matches, _) = find_matches_in("aaaa", "aa", true);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[1].offset, 2);
        let (matches, _) = find_matches_in("anything", "", true);
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_multibyte_safe() {
        // Multi-byte chars before the match must not panic the scanner and
        // offsets stay byte-accurate.
        let text = "héllo wörld\nwörld";
        let (matches, _) = find_matches_in(text, "wörld", true);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[1].line, 1);
        assert_eq!(matches[1].col, 0);
        // Offsets index real bytes.
        assert_eq!(&text[matches[0].offset..matches[0].offset + 6], "wörld");
    }

    #[test]
    fn offset_to_line_col_maps_positions() {
        let text = "ab\ncde\nf";
        assert_eq!(offset_to_line_col(text, 0), (0, 0));
        assert_eq!(offset_to_line_col(text, 3), (1, 0));
        assert_eq!(offset_to_line_col(text, 5), (1, 2));
        assert_eq!(offset_to_line_col(text, 8), (2, 1));
        assert_eq!(offset_to_line_col(text, 99), (2, 1));
    }

    #[test]
    fn replace_all_splices_every_match() {
        let text = "foo bar\nfoo baz\nfoo";
        let (matches, _) = find_matches_in(text, "foo", true);
        let out = replace_all_from_matches(text, &matches, 3, "qux");
        assert_eq!(out, "qux bar\nqux baz\nqux");
    }

    #[test]
    fn replace_all_with_length_change() {
        // Every single-char match is replaced, including adjacent ones.
        let text = "a aa a";
        let (matches, _) = find_matches_in(text, "a", true);
        assert_eq!(matches.len(), 4);
        let out = replace_all_from_matches(text, &matches, 1, "bb");
        assert_eq!(out, "bb bbbb bb");
    }

    #[test]
    fn word_span_expands_both_directions() {
        //             01234567890123
        let text = "foo bar_baz qux";
        assert_eq!(find_word_span(text, 5), Some((4, 11)));
        assert_eq!(find_word_span(text, 4), Some((4, 11)));
        assert_eq!(find_word_span(text, 12), Some((12, 15)));
        assert_eq!(find_word_span(text, 0), Some((0, 3)));
        // Cursor right after "foo" is still on the word (left adjacency).
        assert_eq!(find_word_span(text, 3), Some((0, 3)));
        // Spaces on both sides: no word.
        assert_eq!(find_word_span("a  b", 2), None);
        // Single-char words are ignored.
        assert_eq!(find_word_span("a b", 0), None);
    }

    #[test]
    fn word_occurrences_are_whole_word() {
        let text = "foo foods\nbar foo\nfoo_bar\nfoo";
        let lines = word_occurrence_lines(text, "foo");
        // "foods" (line 0, second word) and "foo_bar" (line 2) don't count,
        // but the standalone "foo" at the start of line 0 does.
        assert_eq!(lines, vec![0, 1, 3]);
    }

    #[test]
    fn word_occurrences_skips_short_words() {
        assert!(word_occurrence_lines("a a a", "a").is_empty());
    }

    #[test]
    fn bracket_match_forward_and_backward() {
        //             0123456789...
        let text = "fn main() {\n    if (a) {\n    }\n}";
        // '(' is at offset 7; cursor just after it matches ')' at offset 8.
        let open = text.find('(').expect("has paren");
        assert_eq!(open, 7);
        match find_bracket_match(text, open + 1) {
            BracketStatus::Matched { other_line, other_col } => {
                assert_eq!((other_line, other_col), (0, 8));
            }
            other => panic!("expected match, got {other:?}"),
        }
        // Cursor just after ')' (offset 9) prefers the char before it and
        // matches back to '(' at offset 7.
        match find_bracket_match(text, 9) {
            BracketStatus::Matched { other_line, other_col } => {
                assert_eq!((other_line, other_col), (0, 7));
            }
            other => panic!("expected match, got {other:?}"),
        }
    }

    #[test]
    fn bracket_match_nested_and_unmatched() {
        let text = "( [ { } ] )";
        let brace = text.find('{').expect("has brace");
        match find_bracket_match(text, brace + 1) {
            BracketStatus::Matched { other_col, .. } => assert_eq!(other_col, 6),
            other => panic!("expected match, got {other:?}"),
        }
        assert_eq!(find_bracket_match("((", 1), BracketStatus::Unmatched);
        assert_eq!(find_bracket_match("no brackets", 2), BracketStatus::None);
        assert_eq!(find_bracket_match("", 0), BracketStatus::None);
    }

    #[test]
    fn bracket_match_respects_nesting_depth() {
        let text = "{ { } }";
        // Cursor after first '{' → partner is the last '}'.
        match find_bracket_match(text, 1) {
            BracketStatus::Matched { other_col, .. } => assert_eq!(other_col, 6),
            other => panic!("expected match, got {other:?}"),
        }
        // Cursor after inner '{' → partner is the inner '}'.
        match find_bracket_match(text, 3) {
            BracketStatus::Matched { other_col, .. } => assert_eq!(other_col, 4),
            other => panic!("expected match, got {other:?}"),
        }
    }

    #[test]
    fn fold_regions_indentation_basic() {
        let text = "fn a() {\n    x();\n    y();\n}\nfn b() {\n    z();\n}";
        let regions = compute_fold_regions(text);
        // Regions end at the last indented line; the closing brace stays
        // visible (Kate-style folding).
        assert!(regions.contains(&(0, 2)));
        assert!(regions.contains(&(4, 5)));
        let outer = outermost_regions(&regions);
        assert_eq!(outer, vec![(0, 2), (4, 5)]);
    }

    #[test]
    fn fold_regions_blank_lines_stay_inside() {
        let text = "fn a() {\n    x();\n\n}";
        let regions = compute_fold_regions(text);
        // The blank line folds away with the body, not the closing brace.
        assert!(regions.contains(&(0, 2)));
    }

    #[test]
    fn fold_expand_round_trip() {
        let text = "fn a() {\n    x();\n    y();\n}\nfn b() {\n    z();\n}\n";
        let regions = outermost_regions(&compute_fold_regions(text));
        assert_eq!(regions, vec![(0, 2), (4, 5)]);
        let (folded, folds) = fold_regions_in_text(text, &regions);
        assert_eq!(folds.len(), 2);
        // Anchors are in display coordinates.
        assert_eq!(folds[0].start, 0);
        assert_eq!(folds[0].hidden_count, 2);
        assert_eq!(folds[1].start, 3);
        assert_eq!(folds[1].hidden_count, 1);
        let folded_lines: Vec<&str> = folded.lines().collect();
        assert_eq!(folded_lines.len(), 6);
        assert!(folded_lines[1].contains("2 folded lines"));
        assert!(folded_lines[4].contains("1 folded lines"));
        // Full expansion restores the original byte-for-byte.
        let expanded = expand_all_in_text(&folded, &folds);
        assert_eq!(expanded, text);
    }

    #[test]
    fn fold_expand_round_trip_no_trailing_newline() {
        let text = "a\n    b\nc";
        let regions = compute_fold_regions(text);
        let (folded, folds) = fold_regions_in_text(text, &regions);
        let expanded = expand_all_in_text(&folded, &folds);
        assert_eq!(expanded, text);
    }

    #[test]
    fn map_lines_across_fold_and_expand() {
        let text = "fn a() {\n    x();\n    y();\n}\nlast\n";
        let regions = outermost_regions(&compute_fold_regions(text));
        assert_eq!(regions, vec![(0, 2)]);
        let (_, folds) = fold_regions_in_text(text, &regions);
        // Display after folding: [fn a() {, PH, }, last].
        assert_eq!(map_line_on_fold(4, &regions, &folds), 3); // "last" → 3
        assert_eq!(map_line_on_fold(3, &regions, &folds), 2); // "}" → 2
        assert_eq!(map_line_on_fold(2, &regions, &folds), 0); // hidden → anchor
        assert_eq!(map_line_on_fold(0, &regions, &folds), 0);
        // And back.
        assert_eq!(map_line_on_expand(3, &folds), 4);
        assert_eq!(map_line_on_expand(2, &folds), 3);
        assert_eq!(map_line_on_expand(1, &folds), 0); // placeholder → anchor
        assert_eq!(map_line_on_expand(0, &folds), 0);
    }

    #[test]
    fn edit_on_fold_anchor_expands_it_and_shifts_survivors() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());

        // Two foldable regions; fold both so two folds exist in the buffer.
        let text = "fn a() {\n    x();\n    y();\n}\nfn b() {\n    z();\n}\n";
        let regions = outermost_regions(&compute_fold_regions(text));
        assert_eq!(regions, vec![(0, 2), (4, 5)]);
        let (folded, folds) = fold_regions_in_text(text, &regions);
        assert_eq!(folds.len(), 2);
        assert_eq!(folds[0].start, 0);
        assert_eq!(folds[1].start, 3);

        let mut content = text_editor::Content::with_text(&folded);
        content.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        });
        state.content = Some(content);
        state.folds = folds;

        // Enter on the first fold's anchor line (display line 0): the anchor
        // fold expands, and the surviving fold below shifts down — +1 line
        // from the expansion (hidden_count - 1) and +1 from the Enter itself
        // (display anchor was 3, now 5).
        let _ = state.update(
            Message::Edit(text_editor::Action::Edit(text_editor::Edit::Enter)),
            &vfs,
            &root,
            &cancel,
        );

        assert_eq!(state.folds.len(), 1, "anchor fold must be expanded away");
        assert_eq!(state.folds[0].start, 5, "surviving fold must shift down (was 3)");
        let buffer = state.content.as_ref().map(|c| c.text()).unwrap_or_default();
        assert!(buffer.contains("    x();"), "expanded fold restores hidden lines");
        assert!(buffer.contains("1 folded lines"), "surviving fold keeps its placeholder");
    }

    #[test]
    fn save_clears_staged_vfs_entry_so_reopen_reads_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let file = root.join("foo.rs");
        std::fs::write(file.as_std_path(), "A").unwrap();

        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        // Stage a VFS entry (an agent-proposed change not yet committed).
        vfs.lock().unwrap().write(&file, "A".to_string()).unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());

        // Open: VFS wins over disk, so the buffer shows the staged content.
        let _ = state.update(Message::FileSelected(file.clone()), &vfs, &root, &cancel);
        assert_eq!(state.content.as_ref().map(|c| c.text()).as_deref(), Some("A"));

        // Edit the buffer, then save.
        let mut content = text_editor::Content::with_text("B");
        content.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 1 },
            selection: None,
        });
        state.content = Some(content);
        let _ = state.update(Message::Save, &vfs, &root, &cancel);
        assert!(!state.dirty);
        // Save normalizes the buffer to end with a newline (trim-trailing).
        assert_eq!(std::fs::read_to_string(file.as_std_path()).unwrap(), "B\n");

        // The staged entry must be gone, so a reopen reads the saved disk
        // content instead of shadowing it with stale staged text.
        assert!(vfs.lock().unwrap().get(&file).is_none());
        let _ = state.update(Message::FileSelected(file.clone()), &vfs, &root, &cancel);
        assert_eq!(state.content.as_ref().map(|c| c.text()).as_deref(), Some("B\n"));
    }

    #[test]
    fn delete_confirm_flow_removes_file_and_staged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let file = root.join("foo.rs");
        std::fs::write(file.as_std_path(), "A").unwrap();

        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        vfs.lock().unwrap().write(&file, "A".to_string()).unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());
        let _ = state.update(Message::FileSelected(file.clone()), &vfs, &root, &cancel);

        // First press only arms the confirmation gate.
        let _ = state.update(Message::DeleteFile, &vfs, &root, &cancel);
        assert!(state.pending_delete.is_some());
        assert!(file.as_std_path().exists(), "file must not be deleted before confirm");
        assert!(vfs.lock().unwrap().get(&file).is_some());

        // Cancelling disarms and touches nothing.
        let _ = state.update(Message::DeleteCancelled, &vfs, &root, &cancel);
        assert!(state.pending_delete.is_none());
        assert!(file.as_std_path().exists());

        // Confirm performs the delete and clears the staged entry.
        let _ = state.update(Message::DeleteFile, &vfs, &root, &cancel);
        let _ = state.update(Message::DeleteConfirmed, &vfs, &root, &cancel);
        assert!(state.pending_delete.is_none());
        assert!(state.active_file.is_none());
        assert!(state.content.is_none());
        assert!(!file.as_std_path().exists());
        assert!(vfs.lock().unwrap().get(&file).is_none());
    }

    #[test]
    fn new_file_name_creates_file_inside_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());

        let _ = state.update(Message::NewFileName("fresh.rs".into()), &vfs, &root, &cancel);
        // The write is the synchronous side effect; the FileSelected follow-up
        // is a returned Task the runtime would apply.
        assert_eq!(std::fs::read_to_string(root.join("fresh.rs").as_std_path()).unwrap(), "");
    }

    #[test]
    fn new_file_name_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());

        for bad in ["../../evil.rs", "/etc/evil.rs", "..", ""] {
            let _ = state.update(Message::NewFileName(bad.to_string()), &vfs, &root, &cancel);
            assert_eq!(state.active_file, None, "rejected name {bad:?} must not select a file");
        }
        assert_eq!(
            std::fs::read_dir(root.as_std_path()).unwrap().count(),
            0,
            "no file may be created for rejected names"
        );

        // An absolute path pointing inside the project is still rejected: the
        // name must not bypass the project-dir join.
        let absolute_inside = root.join("abs.rs");
        let _ = state.update(
            Message::NewFileName(absolute_inside.as_str().to_string()),
            &vfs,
            &root,
            &cancel,
        );
        assert!(!absolute_inside.as_std_path().exists());
        assert_eq!(state.active_file, None);
    }

    #[test]
    fn utf16_col_round_trip() {
        let line = "héllo wörld";
        // 'é' is 2 bytes, 1 UTF-16 code unit; 'ö' is 2 bytes, 1 UTF-16.
        let text = line.to_string();
        for (byte_idx, _) in line.char_indices() {
            let utf16 = text[..byte_idx].chars().map(|c| c.len_utf16()).sum();
            let back = utf16_col_to_byte(line, utf16);
            assert_eq!(byte_idx, back, "mismatch at byte {byte_idx}, utf16={utf16}");
        }
        assert_eq!(utf16_col_to_byte(line, 99), line.len());
    }

    #[test]
    fn lsp_position_uses_utf16_column() {
        let content = text_editor::Content::with_text("héllo");
        // Cursor is at the start; column = 0 utf-16.
        assert_eq!(lsp_position(&content), (0, 0));
    }

    #[test]
    fn parse_completion_items_empty() {
        assert!(parse_completion_items(&serde_json::Value::Null).is_empty());
        assert!(parse_completion_items(&serde_json::json!([])).is_empty());
    }

    #[test]
    fn parse_completion_items_array() {
        let json = serde_json::json!([
            { "label": "foo", "detail": "()", "insertText": "foo()" },
            { "label": "bar" },
        ]);
        let items = parse_completion_items(&json);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "foo");
        assert_eq!(items[0].detail.as_deref(), Some("()"));
        assert_eq!(items[0].insert_text, "foo()");
        assert_eq!(items[1].insert_text, "bar");
        assert!(items[1].detail.is_none());
    }

    #[test]
    fn parse_completion_items_list() {
        let json = serde_json::json!({
            "isIncomplete": false,
            "items": [
                { "label": "main" },
                { "label": "map" },
            ]
        });
        assert_eq!(parse_completion_items(&json).len(), 2);
    }

    #[test]
    fn parse_definition_location() {
        let json = serde_json::json!({
            "uri": "file:///home/project/src/lib.rs",
            "range": {
                "start": { "line": 42, "character": 7 },
                "end": { "line": 42, "character": 12 },
            }
        });
        let result = parse_definition(&json);
        assert!(result.is_some());
        let (path, line, col) = result.unwrap();
        assert!(path.as_str().ends_with("lib.rs"));
        assert_eq!(line, 42);
        assert_eq!(col, 7);
    }

    #[test]
    fn parse_definition_location_array() {
        let json = serde_json::json!([{
            "uri": "file:///main.rs",
            "range": { "start": { "line": 10, "character": 0 }, "end": { "line": 10, "character": 5 } },
        }]);
        let (path, line, _) = parse_definition(&json).unwrap();
        assert!(path.as_str().ends_with("main.rs"));
        assert_eq!(line, 10);
    }

    #[test]
    fn parse_definition_null() {
        assert!(parse_definition(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn parse_hover_contents_markup() {
        let json = serde_json::json!({
            "contents": { "kind": "markdown", "value": "Hello **world**" }
        });
        assert_eq!(parse_hover_contents(&json).as_deref(), Some("Hello **world**"));
    }

    #[test]
    fn parse_hover_contents_array() {
        let json = serde_json::json!({
            "contents": [
                { "language": "rust", "value": "fn foo()" },
                "Some docs",
            ]
        });
        let text = parse_hover_contents(&json).unwrap();
        assert!(text.contains("fn foo()"));
        assert!(text.contains("Some docs"));
    }

    #[test]
    fn parse_hover_contents_string() {
        let json = serde_json::json!({
            "contents": "plain hover text"
        });
        assert_eq!(parse_hover_contents(&json).as_deref(), Some("plain hover text"));
    }

    #[test]
    fn pane_resize_clamps_divider_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let vfs = Arc::new(Mutex::new(VirtualFs::new()));
        let cancel = CancellationToken::new();
        let mut state = State::new(root.clone());

        // The layout always has exactly the tree | editor split.
        let split = *state.pane_state.layout().splits().next().unwrap();

        let tree_share = |state: &State| {
            state.pane_state.layout().split_regions(0.0, 0.0, iced::Size::new(1000.0, 600.0))
                [&split]
                .2
        };

        // Dragging the tree far right would crush the editor: clamped to max.
        let _ = state.update(
            Message::PaneResized(iced::widget::pane_grid::ResizeEvent { split, ratio: 0.99 }),
            &vfs,
            &root,
            &cancel,
        );
        assert!((tree_share(&state) - TREE_PANE_MAX_RATIO).abs() < 1e-4);

        // Dragging the tree to nothing: clamped to min.
        let _ = state.update(
            Message::PaneResized(iced::widget::pane_grid::ResizeEvent { split, ratio: 0.001 }),
            &vfs,
            &root,
            &cancel,
        );
        assert!((tree_share(&state) - TREE_PANE_MIN_RATIO).abs() < 1e-4);

        // A middle ratio passes through unmodified.
        let _ = state.update(
            Message::PaneResized(iced::widget::pane_grid::ResizeEvent { split, ratio: 0.3 }),
            &vfs,
            &root,
            &cancel,
        );
        assert!((tree_share(&state) - 0.3).abs() < 1e-4);
    }

    #[test]
    fn pane_init_has_three_distinct_panes_with_narrow_diag() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let state = State::new(root);

        // Three distinct panes: tree | editor | diagnostics.
        assert_ne!(state.tree_pane, state.editor_pane);
        assert_ne!(state.tree_pane, state.diag_pane);
        assert_ne!(state.editor_pane, state.diag_pane);

        // Exactly two dividers.
        let splits: Vec<_> = state.pane_state.layout().splits().collect();
        assert_eq!(splits.len(), 2);

        let regions =
            state.pane_state.layout().split_regions(0.0, 0.0, iced::Size::new(2000.0, 600.0));

        // The tree|editor divider keeps the established default share (#90).
        let tree_split = state.tree_split.unwrap();
        let tree_share = regions[&tree_split].2;
        assert!((tree_share - TREE_PANE_DEFAULT_RATIO).abs() < 1e-4);

        // The editor|diag divider stores the editor's share of the split, so
        // the diagnostics pane must start at `1 - ratio` (narrow, #108).
        let diag_split = state.diag_split.unwrap();
        let diag_share = 1.0 - regions[&diag_split].2;
        assert!((diag_share - DIAG_PANE_DEFAULT_SHARE).abs() < 1e-4);
    }
}
