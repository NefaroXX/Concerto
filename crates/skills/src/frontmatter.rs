//! Minimal YAML-subset parser for `SKILL.md` front matter (see the crate-level
//! docs in `lib.rs` for the full grammar contract).
//!
//! This module deliberately implements only the flat subset used by skill
//! manifests — scalar `key: value` pairs, `key:` headers followed by `- item`
//! list lines, optional single/double quoting, and `#` comments. It is NOT a
//! general YAML parser: no nested structures, flow lists (`[a, b]`), block
//! scalars (`|`/`>`), anchors, or escapes.

use std::path::PathBuf;

/// Fields understood by the front-matter subset parser, plus the markdown body
/// that follows the closing `---` delimiter.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct FrontMatter {
    pub id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub tools: Vec<String>,
    pub resources: Vec<PathBuf>,
    /// Instruction text: the markdown body below the closing `---` line.
    pub body: String,
}

/// Parse the front matter (and body) of a `SKILL.md` file.
///
/// Errors carry a human-readable reason only; the caller attaches the file
/// path (`SkillsError::FrontMatter`).
pub(crate) fn parse_front_matter(text: &str) -> Result<FrontMatter, String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.first() else {
        return Err("file is empty; expected `---` opening delimiter".into());
    };
    if first.trim() != "---" {
        return Err("missing opening `---` delimiter at the start of the file".into());
    }

    let mut fm_lines = Vec::new();
    let mut fm_end = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            fm_end = Some(i);
            break;
        }
        fm_lines.push(*line);
    }
    let fm_end = fm_end.ok_or("unterminated front matter: no closing `---` delimiter")?;

    let mut fm = FrontMatter { body: lines[fm_end + 1..].join("\n"), ..FrontMatter::default() };
    // Key whose `- item` lines are currently being collected (`tools`/`resources`).
    let mut list_key: Option<&str> = None;

    for line in fm_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // List item: `- item` lines attach to the most recent list key.
        if let Some(item) = trimmed.strip_prefix('-') {
            let key = list_key.ok_or_else(|| {
                format!(
                    "list item `{trimmed}` without a preceding list key (`tools:` or `resources:`)"
                )
            })?;
            let value = parse_scalar(item)?;
            match key {
                "tools" => fm.tools.push(value),
                "resources" => fm.resources.push(PathBuf::from(value)),
                _ => return Err(format!("key `{key}` does not accept list items")),
            }
            continue;
        }

        // Scalar: `key: value`.
        let Some((key, value)) = split_key_value(trimmed) else {
            return Err(format!("unrecognized front matter line `{trimmed}`"));
        };
        match key {
            "id" | "name" | "version" | "description" | "instructions" => {
                list_key = None;
                if value.is_empty() {
                    // `key:` alone leaves the field unset; defaults apply later.
                    continue;
                }
                let value = parse_scalar(value)?;
                match key {
                    "id" => fm.id = Some(value),
                    "name" => fm.name = Some(value),
                    "version" => fm.version = Some(value),
                    "description" => fm.description = Some(value),
                    _ => fm.instructions = Some(value),
                }
            }
            "tools" | "resources" => {
                if value.is_empty() {
                    list_key = Some(key);
                } else {
                    return Err(format!(
                        "key `{key}` expects a list of `- item` lines, got inline value `{value}`"
                    ));
                }
            }
            _ => {
                // Unknown keys are ignored, mirroring `skill.toml` behavior.
                list_key = None;
            }
        }
    }
    Ok(fm)
}

/// Split a `key: value` line at its first colon. Returns `None` when the line
/// has no colon or an empty key.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() {
        return None;
    }
    let value = line[colon + 1..].trim();
    Some((key, value))
}

/// Parse a scalar value: strip a trailing `#` comment (outside quotes) and
/// surrounding single/double quotes. Quoted values are taken literally — no
/// escape processing.
fn parse_scalar(value: &str) -> Result<String, String> {
    let value = strip_inline_comment(value).trim();
    if value.starts_with('"') {
        if value.len() < 2 || !value.ends_with('"') {
            return Err(format!("unterminated double-quoted string `{value}`"));
        }
        Ok(value[1..value.len() - 1].to_string())
    } else if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err(format!("unterminated single-quoted string `{value}`"));
        }
        Ok(value[1..value.len() - 1].to_string())
    } else {
        Ok(value.to_string())
    }
}

/// Remove a trailing `#` comment, respecting single/double quotes. A comment
/// starts at a `#` that is preceded by whitespace; `#` inside quotes is data.
fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && i > 0 && bytes[i - 1] == b' ' => {
                return &value[..i];
            }
            _ => {}
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_and_lists() {
        let fm = parse_front_matter(
            r#"---
# a comment
id: front-pack
name: Front Pack
version: "1.0.0" # inline comment
description: 'Single quoted'
tools:
  - cargo test # tool comment
  - "cargo clippy"
resources:
  - fixtures/sample.rs
instructions: fallback text
---
Body line one.
Body line two.
"#,
        )
        .expect("parse should succeed");
        assert_eq!(fm.id.as_deref(), Some("front-pack"));
        assert_eq!(fm.name.as_deref(), Some("Front Pack"));
        assert_eq!(fm.version.as_deref(), Some("1.0.0"));
        assert_eq!(fm.description.as_deref(), Some("Single quoted"));
        assert_eq!(fm.tools, vec!["cargo test", "cargo clippy"]);
        assert_eq!(fm.resources, vec![PathBuf::from("fixtures/sample.rs")]);
        assert_eq!(fm.instructions.as_deref(), Some("fallback text"));
        assert_eq!(fm.body, "Body line one.\nBody line two.");
    }

    #[test]
    fn body_is_empty_when_no_content_below_delimiter() {
        let fm = parse_front_matter("---\nid: x\n---\n").expect("parse should succeed");
        assert_eq!(fm.body, "");
        assert_eq!(fm.id.as_deref(), Some("x"));
    }

    #[test]
    fn missing_opening_delimiter_is_error() {
        let err =
            parse_front_matter("# No front matter\n\nJust docs.\n").expect_err("should error");
        assert!(err.contains("opening `---`"), "unexpected: {err}");
    }

    #[test]
    fn unterminated_front_matter_is_error() {
        let err = parse_front_matter("---\nid: broken\n").expect_err("should error");
        assert!(err.contains("closing `---`"), "unexpected: {err}");
    }

    #[test]
    fn empty_file_is_error() {
        let err = parse_front_matter("").expect_err("should error");
        assert!(err.contains("`---`"), "unexpected: {err}");
    }

    #[test]
    fn malformed_scalar_lines_are_errors() {
        let err =
            parse_front_matter("---\nid: x\nthis is not yaml\n---\n").expect_err("should error");
        assert!(err.contains("unrecognized"), "unexpected: {err}");

        let err = parse_front_matter("---\nid: x\n- orphan item\n---\n").expect_err("should error");
        assert!(err.contains("without a preceding list key"), "unexpected: {err}");

        let err = parse_front_matter("---\nid: x\ntools: inline\n---\n").expect_err("should error");
        assert!(err.contains("expects a list"), "unexpected: {err}");

        let err = parse_front_matter("---\nid: x\ndescription: \"unterminated\n---\n")
            .expect_err("should error");
        assert!(err.contains("unterminated"), "unexpected: {err}");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let fm = parse_front_matter("---\nid: x\nfuture_key: whatever\ntools:\n  - a\n---\n")
            .expect("parse should succeed");
        assert_eq!(fm.id.as_deref(), Some("x"));
        assert_eq!(fm.tools, vec!["a"]);
    }

    #[test]
    fn repeated_keys_last_wins() {
        let fm = parse_front_matter("---\nname: first\nname: second\n---\n")
            .expect("parse should succeed");
        assert_eq!(fm.name.as_deref(), Some("second"));
    }

    #[test]
    fn comments_inside_quotes_are_data() {
        let fm = parse_front_matter("---\ndescription: \"Fix #42 now\"\n---\n")
            .expect("parse should succeed");
        assert_eq!(fm.description.as_deref(), Some("Fix #42 now"));
    }

    #[test]
    fn quoted_scalar_with_trailing_comment() {
        let fm = parse_front_matter("---\nname: \"My Skill\" # comment\n---\n")
            .expect("parse should succeed");
        assert_eq!(fm.name.as_deref(), Some("My Skill"));
    }

    #[test]
    fn values_may_contain_colons() {
        let fm = parse_front_matter("---\ndescription: Use colons: here\n---\n")
            .expect("parse should succeed");
        assert_eq!(fm.description.as_deref(), Some("Use colons: here"));
    }
}
