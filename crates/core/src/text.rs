//! Normalization of typographic (Unicode) punctuation to ASCII forms.
//!
//! LLM providers (for example `nemotron-3.5-lightning-free`) frequently write
//! typographic Unicode into model output and file content: U+2011
//! non-breaking hyphens, U+2013 en dashes, U+2192 rightwards arrows, and
//! "smart" curly quotes (U+2018, U+2019, U+201C, U+201D). These are legal
//! prose but break anything that depends on ASCII delimiters:
//!
//! * Rust source and JSON both require ASCII `'` and `"` as string
//!   delimiters. Curly quotes are distinct codepoints, so `serde_json` rejects
//!   them, and string-aware scanners (such as the orchestrator's
//!   balanced-bracket walker) stop tracking string boundaries correctly.
//! * Dashes such as U+2011 and U+2013 are visually near-identical to `-` but
//!   are not it, so identifiers, paths, and flags written by a model split or
//!   fail when file content or shell commands are re-read.
//!
//! [`normalize_typographic`] maps the common typographic characters to their
//! canonical ASCII equivalents and keeps a zero-allocation fast path when the
//! input is already clean. The module is deliberately dependency-free so
//! `concerto-core` (model-reply JSON extraction) and `concerto-tools`
//! (file-write hygiene) can both reuse it without adding dependencies.

use std::borrow::Cow;

/// Normalize typographic punctuation to ASCII.
///
/// Mappings applied (everything else is left untouched):
///
/// | Codepoint | Name | Replaced with |
/// |---|---|---|
/// | U+00A0 | NO-BREAK SPACE | `' '` |
/// | U+202F | NARROW NO-BREAK SPACE | `' '` |
/// | U+2018 | LEFT SINGLE QUOTATION MARK | `'` |
/// | U+2019 | RIGHT SINGLE QUOTATION MARK | `'` |
/// | U+201C | LEFT DOUBLE QUOTATION MARK | `"` |
/// | U+201D | RIGHT DOUBLE QUOTATION MARK | `"` |
/// | U+2010 | HYPHEN | `-` |
/// | U+2011 | NON-BREAKING HYPHEN | `-` |
/// | U+2012 | FIGURE DASH | `-` |
/// | U+2013 | EN DASH | `-` |
/// | U+2192 | RIGHTWARDS ARROW | `->` |
///
/// U+2014 EM DASH (`—`) and U+2015 HORIZONTAL BAR (`―`) are deliberately left
/// untouched: they are legal punctuation in prose and comments, not a compile
/// or parse hazard.
///
/// Returns [`Cow::Borrowed`] (zero allocation) when the input contains no
/// typographic characters; [`Cow::Owned`] with the normalized text otherwise.
pub fn normalize_typographic(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|ch| typographic_replacement(ch).is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match typographic_replacement(ch) {
            Some(replacement) => out.push_str(replacement),
            None => out.push(ch),
        }
    }
    Cow::Owned(out)
}

/// ASCII replacement for a typographic character, if any.
fn typographic_replacement(ch: char) -> Option<&'static str> {
    match ch {
        '\u{00A0}' | '\u{202F}' => Some(" "),
        '\u{2018}' | '\u{2019}' => Some("'"),
        '\u{201C}' | '\u{201D}' => Some("\""),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' => Some("-"),
        '\u{2192}' => Some("->"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn assert_normalized(input: &str, expected: &str) {
        assert_eq!(normalize_typographic(input), Cow::from(expected));
    }

    #[test]
    fn maps_every_typographic_codepoint_to_ascii() {
        assert_normalized("\u{00A0}", " ");
        assert_normalized("\u{202F}", " ");
        assert_normalized("\u{2018}", "'");
        assert_normalized("\u{2019}", "'");
        assert_normalized("\u{201C}", "\"");
        assert_normalized("\u{201D}", "\"");
        assert_normalized("\u{2010}", "-");
        assert_normalized("\u{2011}", "-");
        assert_normalized("\u{2012}", "-");
        assert_normalized("\u{2013}", "-");
        assert_normalized("\u{2192}", "->");
    }

    #[test]
    fn leaves_em_dash_and_horizontal_bar_untouched() {
        assert_normalized("\u{2014}", "\u{2014}");
        assert_normalized("\u{2015}", "\u{2015}");
    }

    #[test]
    fn borrows_unchanged_input_without_allocating() {
        let normalized = normalize_typographic("plain ASCII: -<>\"'");
        assert!(matches!(normalized, Cow::Borrowed(_)));
    }

    #[test]
    fn normalizes_mixed_input() {
        assert_normalized(
            "He said \u{201C}go \u{2011}fast\u{201D} then \u{2192} \u{2013} ('ok')",
            "He said \"go -fast\" then -> - ('ok')",
        );
    }
}
