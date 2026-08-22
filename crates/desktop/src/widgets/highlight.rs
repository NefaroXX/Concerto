use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

/// Highlight `code` line‑by‑line using the optional language token.
///
/// Returns a vector where each entry corresponds to a line and contains a
/// vector of `(Style, &str)` tuples for the styled fragments of that line.
pub fn highlight_lines<'a>(code: &'a str, lang_token: Option<&str>) -> Vec<Vec<(Style, &'a str)>> {
    // Resolve syntax – fall back to plain text when unknown.
    let syntax = match lang_token {
        Some(tok) => SYNTAX_SET
            .find_syntax_by_token(tok)
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text()),
        None => SYNTAX_SET.find_syntax_plain_text(),
    };

    // Choose a theme – prefer a dark theme if available, otherwise the first.
    let Some(theme) =
        THEME_SET.themes.get("base16-ocean.dark").or_else(|| THEME_SET.themes.values().next())
    else {
        // `ThemeSet::load_defaults()` always ships at least one theme, so this
        // is unreachable in practice. Fall back to plain (unhighlighted) text
        // rather than panicking in library code.
        return code.lines().map(|line| vec![(Style::default(), line)]).collect();
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut highlighted = Vec::new();

    for line in LinesWithEndings::from(code) {
        // `highlight_line` returns a Vec<(Style, &str)> for the line.
        let ranges = highlighter.highlight_line(line, &SYNTAX_SET).unwrap_or_default();
        highlighted.push(ranges);
    }
    highlighted
}
