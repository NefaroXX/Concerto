//! Glob pattern matching backing filesystem scope enforcement.

use std::path::Path;

/// Simple glob pattern matching for path components.
///
/// Supports:
/// - `*` — matches any characters except `/`
/// - `**` — matches any characters including `/`
/// - `?` — matches any single character except `/`
///
/// A pattern without any wildcards is treated as an exact (byte-for-byte) match.
pub(super) fn glob_match(pattern: &str, path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    // Fast path: no wildcards → exact string comparison.
    if !pattern.contains('*') && !pattern.contains('?') {
        return path_str.as_ref() == pattern;
    }

    let pat_bytes = pattern.as_bytes();
    let path_bytes = path_str.as_bytes();

    glob_match_bytes(pat_bytes, path_bytes, false)
}

/// Recursive byte-level glob matcher.
///
/// `consumed_slash` tracks whether the previous pattern segment ended with `**`
/// matching a `/`, which prevents `**` from matching zero path segments more
/// than once (avoids exponential blowup on `**/**/**...` patterns).
fn glob_match_bytes(pattern: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if pattern.is_empty() {
        return input.is_empty();
    }

    // `**` — match any number of characters (including path separators).
    if is_double_star(pattern) {
        return match_double_star(pattern, input, consumed_slash);
    }

    if input.is_empty() {
        return false;
    }

    match pattern[0] {
        b'*' => match_single_star(&pattern[1..], input, consumed_slash),
        b'?' => match_qmark(&pattern[1..], input),
        _ => match_literal(pattern, input),
    }
}

/// Check whether the pattern starts with `**`.
fn is_double_star(pattern: &[u8]) -> bool {
    pattern.len() >= 2 && &pattern[..2] == b"**"
}

/// Match `**/` — zero or more directory levels.
///
/// `rest` is the pattern after `**` (starts with `/`).  Tries zero
/// directory levels first (skip `**/` entirely), then one or more
/// levels (consume characters including at least one `/`).
fn match_double_star_slash(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    debug_assert!(rest.starts_with(b"/"), "expected rest to start with '/'");

    let after = &rest[1..];

    // Try zero directory levels: skip `**/` entirely.
    if glob_match_bytes(after, input, consumed_slash) {
        return true;
    }
    // Try one or more directory levels: consume at least one character,
    // matching through / past at least one `/`.
    for i in 1..=input.len() {
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match bare `**` (no following `/`).
///
/// If `rest` is non-empty, tries matching zero characters (skip `**`
/// entirely) then one or more characters.  A trailing `**` with no
/// following pattern matches everything.
fn match_bare_double_star(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if !rest.is_empty() {
        // Try matching zero characters (skip `**` entirely).
        if glob_match_bytes(rest, input, consumed_slash) {
            return true;
        }
        // Try matching one or more characters.
        for i in 1..=input.len() {
            if glob_match_bytes(rest, &input[i..], false) {
                return true;
            }
        }
        return false;
    }

    // Trailing `**` matches everything.
    true
}

/// Match `**/` (zero or more directory levels) or bare `**`.
fn match_double_star(pattern: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    let rest = &pattern[2..];

    if rest.starts_with(b"/") {
        return match_double_star_slash(rest, input, consumed_slash);
    }

    match_bare_double_star(rest, input, consumed_slash)
}

/// Match `*` that can cross `/` (after `**` already crossed a separator).
fn match_star_crossing(rest: &[u8], input: &[u8]) -> bool {
    for i in 0..=input.len() {
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match `*` — any characters except `/`.
fn match_single_star(rest: &[u8], input: &[u8], consumed_slash: bool) -> bool {
    if consumed_slash {
        // `**` already crossed a separator, so this `*` can cross too.
        return match_star_crossing(rest, input);
    }
    // Normal `*`: match any non-`/` characters.
    for i in 0..=input.len() {
        if i > 0 && input[i - 1] == b'/' {
            break;
        }
        if glob_match_bytes(rest, &input[i..], false) {
            return true;
        }
    }
    false
}

/// Match `?` — exactly one character except `/`.
fn match_qmark(rest: &[u8], input: &[u8]) -> bool {
    if input.is_empty() || input[0] == b'/' {
        return false;
    }
    glob_match_bytes(rest, &input[1..], false)
}

/// Match a literal character.
fn match_literal(pattern: &[u8], input: &[u8]) -> bool {
    if input.is_empty() || input[0] != pattern[0] {
        return false;
    }
    glob_match_bytes(&pattern[1..], &input[1..], false)
}

#[cfg(test)]
mod tests;
