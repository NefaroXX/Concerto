//! Tool-call argument integrity — one deterministic, model-agnostic parse
//! entry point for every connector.
//!
//! Streamed tool-call arguments are accumulated from fragments on the wire.
//! When the model (or an intermediary proxy) drops the tail of a fragmented
//! JSON object, the accumulated string is *truncated*, not merely malformed:
//! `{"command": "cargo te` never parses. Historically every connector swallowed
//! that failure with `unwrap_or(serde_json::Value::Null)`; the orchestrator's
//! `ensure_arguments_object` then coerced the `Null` to `{}` and the executor
//! ran the tool with **empty arguments** — a silent wrong tool call, not just
//! degraded output.
//!
//! This module replaces that pattern with a single entry point,
//! [`parse_tool_arguments`]:
//!
//! 1. valid JSON is returned untouched (no payload is rewritten);
//! 2. a bounded, **model-agnostic** repair pass fixes the real truncation
//!    modes — unbalanced braces/brackets, an unterminated string, a trailing
//!    comma, single-quoted keys, and trailing garbage after a complete
//!    top-level value;
//! 3. an empty/whitespace payload is a distinct, non-error
//!    [`ToolArgumentParse::Empty`] outcome (the tool genuinely received no
//!    arguments);
//! 4. anything still unrepairable is a loud [`ToolArgumentParseError`] that
//!    carries a truncated excerpt and the underlying parse error — the
//!    connector surfaces it through the normal provider error/retry
//!    machinery, so the turn fails loudly or retries. It NEVER silently
//!    returns `Null`.
//!
//! Repair is deliberately deterministic and vendor-neutral: it reasons only
//! about JSON lexical structure, never about a model family or a route/price
//! tier. Prediction from the model name was rejected (see ADR-75): the same
//! weights are served behind `:free` routes, so substring hints such as
//! `free`/`mini` systematically misclassify strong models.
//!
//! # Double-encoded proxy payloads
//!
//! Some proxies re-serialize arguments, so a payload parses to a JSON *string*
//! that itself contains JSON (`"\"{\\\"command\\\":\\\"ls\\\"}\""`). The
//! OpenAI connector historically unwrapped that class (row #38) so the
//! executor never received a bare string. [`unwrap_argument_string_layers`]
//! preserves that behavior as a separate, bounded step the connectors apply
//! after parsing; it is intentionally not folded into `parse_tool_arguments`
//! so a legitimate raw string argument (`"ls"`) stays exactly as the model
//! emitted it.

/// Maximum length of the raw excerpt carried by a parse error and shown in
/// diagnostics. Bounds log size and avoids injection of very large payloads.
const MAX_ERROR_EXCERPT_CHARS: usize = 200;

/// Maximum number of nested string layers [`unwrap_argument_string_layers`]
/// will peel. Matches the connector-local `MAX_ARGUMENT_STRING_LAYERS` that
/// this function replaces (row #38): bounded and iterative, never unbounded.
pub const MAX_ARGUMENT_STRING_LAYERS: usize = 3;

/// Outcome of parsing accumulated tool-call arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolArgumentParse {
    /// The payload parsed (directly or after deterministic repair).
    Value(serde_json::Value),
    /// The payload was empty/whitespace: the tool call carried no arguments.
    /// Distinct from an error — an argument-less tool is legitimate — and
    /// distinct from a repair failure, which must never be confused with it.
    Empty,
}

/// A tool-argument payload that could not be parsed even after repair.
///
/// Carries a bounded excerpt of the raw payload and the underlying
/// `serde_json` error. The full raw payload is deliberately not stored (it can
/// be arbitrarily large and is only needed for a human-readable diagnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArgumentParseError {
    /// Raw payload truncated to [`MAX_ERROR_EXCERPT_CHARS`] characters.
    pub excerpt: String,
    /// Total length in bytes of the raw payload (never the payload itself).
    pub raw_len: usize,
    /// The underlying parse error from the last (post-repair) attempt.
    pub source: String,
}

impl std::fmt::Display for ToolArgumentParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unparseable tool-call arguments ({} bytes): {}; excerpt: {}",
            self.raw_len, self.source, self.excerpt
        )
    }
}

impl std::error::Error for ToolArgumentParseError {}

/// Parse accumulated tool-call `arguments`.
///
/// See the module docs for the contract. Returns [`ToolArgumentParse::Empty`]
/// for an empty/whitespace payload, [`ToolArgumentParse::Value`] when the
/// payload parsed directly or after deterministic repair, and
/// [`ToolArgumentParseError`] otherwise.
pub fn parse_tool_arguments(raw: &str) -> Result<ToolArgumentParse, ToolArgumentParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ToolArgumentParse::Empty);
    }

    // 1. Direct parse — a valid payload is returned byte-for-byte as parsed.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Ok(ToolArgumentParse::Value(value));
    }

    // 2. Deterministic, model-agnostic repair for the observed truncation and
    //    proxy-corruption modes, each attempt validated by serde_json.
    if let Some(value) = repair_tool_arguments(trimmed) {
        return Ok(ToolArgumentParse::Value(value));
    }

    // 3. Unrepairable: hand back a bounded excerpt plus the underlying error.
    Err(ToolArgumentParseError {
        excerpt: excerpt(trimmed),
        raw_len: raw.len(),
        source: serde_json::from_str::<serde_json::Value>(trimmed)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown parse error".to_string()),
    })
}

/// Attempt every deterministic repair mode in turn, returning the first result
/// that `serde_json` accepts.
///
/// Repairs are independent by construction; each is validated by a real parse
/// before being trusted, so a wrong guess simply falls through to the next
/// mode rather than producing a corrupt object.
fn repair_tool_arguments(raw: &str) -> Option<serde_json::Value> {
    let single_quoted = raw.replace('\'', "\"");
    let mut candidates: Vec<String> = Vec::new();
    candidates.extend(truncate_to_first_value(raw));
    candidates.push(single_quoted.clone());
    // Trailing comma before a closer (serde_json rejects it).
    candidates.extend(drop_trailing_commas(raw));
    candidates.extend(drop_trailing_commas(&single_quoted));
    // Structural repairs over both the original and the single-quoted variant:
    // truncation marks often accompany the quote corruption.
    candidates.extend(brace_balanced(raw));
    candidates.extend(brace_balanced(&single_quoted));
    candidates.extend(close_open_string(raw));
    candidates.extend(close_open_string(&single_quoted));
    // Composite: a trailing comma on a truncated payload (comma dropped first,
    // then the remaining open braces closed).
    for base in [raw, single_quoted.as_str()] {
        if let Some(dropped) = drop_trailing_commas(base) {
            candidates.extend(brace_balanced(&dropped));
            candidates.extend(close_open_string(&dropped));
        }
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate.trim()) {
            return Some(value);
        }
    }
    None
}

/// Remove trailing commas that directly precede a `}` or `]` (JSON forbids
/// them; `serde_json` rejects the payload). String-aware so a comma inside a
/// string literal is never touched.
fn drop_trailing_commas(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len() + 2);
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = trimmed.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            index += 1;
            continue;
        }
        if ch == ',' {
            // Look ahead past whitespace for a closer.
            let mut probe = index + 1;
            while probe < chars.len() && chars[probe].is_whitespace() {
                probe += 1;
            }
            if probe < chars.len() && matches!(chars[probe], '}' | ']') {
                // Drop the comma and any whitespace between it and the closer.
                index = probe;
                continue;
            }
        }
        out.push(ch);
        index += 1;
    }
    if out == trimmed {
        return None;
    }
    Some(out)
}

/// Build a bounded excerpt of `raw` for diagnostics (never the whole payload).
fn excerpt(raw: &str) -> String {
    let mut excerpt: String = raw.chars().take(MAX_ERROR_EXCERPT_CHARS).collect();
    if raw.chars().count() > MAX_ERROR_EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

/// Unwrap bounded layers of proxy double-encoded arguments (row #38).
///
/// A payload that parses to a JSON string containing more JSON (a
/// re-serialized object) is unwrapped to the inner value, at most
/// [`MAX_ARGUMENT_STRING_LAYERS`] times. A string that is not itself
/// re-parseable (a legitimate raw string argument such as `"ls"`) is returned
/// exactly as it was passed in.
///
/// Applied by connectors after [`parse_tool_arguments`]; kept separate so the
/// general parse contract never rewrites a legitimate string-valued argument.
pub fn unwrap_argument_string_layers(mut value: serde_json::Value) -> serde_json::Value {
    let mut layers = 0usize;
    while layers < MAX_ARGUMENT_STRING_LAYERS {
        let inner = match &value {
            serde_json::Value::String(text) => text,
            _ => break,
        };
        match serde_json::from_str::<serde_json::Value>(inner.trim()) {
            Ok(parsed) => {
                value = parsed;
                layers += 1;
            }
            Err(_) => break,
        }
    }
    value
}

/// Keep only the first complete top-level JSON value in `raw` by rescanning
/// the end of the candidate range downward.
///
/// Targets "trailing garbage after a complete value" (e.g. `{"a":1}extra` or
/// concatenated objects). Scans from the longest prefix that could be a value
/// down to the shortest, so balanced inner braces are preferred.
///
/// Bounded: the scan is capped at [`MAX_REPAIR_SCAN_BYTES`] so an adversarial
/// or pathologically large payload cannot make the quadratic rescan dominate.
fn truncate_to_first_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let scan_end = trimmed.len().min(MAX_REPAIR_SCAN_BYTES);
    let scan_end = floor_char_boundary(trimmed, scan_end);
    for end in (1..=scan_end).rev() {
        let candidate = &trimmed[..end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Largest byte offset [`truncate_to_first_value`] will rescan.
const MAX_REPAIR_SCAN_BYTES: usize = 256 * 1024;

/// Largest `byte_index <= index` that lies on a UTF-8 character boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut index = index;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Balance an unterminated `{`/`[` sequence by appending the closers that the
/// nesting and string state require.
///
/// The closers are appended in reverse nesting order, skipping braces seen
/// inside string literals.
fn brace_balanced(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                // A closer with no matching opener: not this repair's shape.
                stack.pop()?;
            }
            _ => {}
        }
    }
    if stack.is_empty() && !in_string {
        // Already balanced — nothing for this repair to do.
        return None;
    }
    let mut repaired = trimmed.to_string();
    if in_string {
        repaired.push('"');
    }
    while let Some(closer) = stack.pop() {
        repaired.push(closer);
    }
    Some(repaired)
}

/// Close a single unterminated string literal and any braces left open at the
/// end of the payload.
fn close_open_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut in_string = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        }
    }
    if !in_string {
        return None;
    }
    Some(format!("{trimmed}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(raw: &str) -> serde_json::Value {
        match parse_tool_arguments(raw).expect("parse should succeed") {
            ToolArgumentParse::Value(value) => value,
            ToolArgumentParse::Empty => panic!("expected a value, got Empty for {raw:?}"),
        }
    }

    #[test]
    fn valid_json_is_untouched() {
        assert_eq!(parsed(r#"{"command":"ls"}"#), json!({"command": "ls"}));
        assert_eq!(parsed(r#"  {"a": [1, 2]}  "#), json!({"a": [1, 2]}));
        // Non-object top-level values parse verbatim (the connector decides
        // whether an object is required).
        assert_eq!(parsed(r#""ls""#), json!("ls"));
    }

    #[test]
    fn empty_and_whitespace_are_a_distinct_non_error_outcome() {
        assert_eq!(parse_tool_arguments("").unwrap(), ToolArgumentParse::Empty);
        assert_eq!(parse_tool_arguments("   \n\t ").unwrap(), ToolArgumentParse::Empty);
    }

    #[test]
    fn unbalanced_braces_are_closed() {
        assert_eq!(parsed(r#"{"command":"ls""#), json!({"command": "ls"}));
        assert_eq!(parsed(r#"{"a":{"b":1}"#), json!({"a": {"b": 1}}));
        assert_eq!(parsed(r#"{"a":[1,2"#), json!({"a": [1, 2]}));
    }

    #[test]
    fn unterminated_string_is_closed() {
        assert_eq!(parsed(r#"{"command":"cargo te"#), json!({"command": "cargo te"}));
        assert_eq!(parsed(r#"{"path":"src/main.rs"#), json!({"path": "src/main.rs"}));
    }

    #[test]
    fn trailing_comma_before_closer_is_dropped() {
        // `serde_json` rejects trailing commas; the single-quote substitution
        // does not address them, but truncation-to-first-value and brace
        // balancing do not either — so this mode is handled by the explicit
        // repair below.
        assert_eq!(parsed(r#"{"command":"ls",}"#), json!({"command": "ls"}));
        assert_eq!(parsed(r#"{"a":[1,2,]}"#), json!({"a": [1, 2]}));
    }

    #[test]
    fn trailing_garbage_after_complete_value_is_truncated() {
        assert_eq!(parsed(r#"{"command":"ls"}garbage"#), json!({"command": "ls"}));
        assert_eq!(parsed(r#"{"a":1}{"b":2}"#), json!({"a": 1}));
    }

    #[test]
    fn single_quoted_keys_are_repaired() {
        assert_eq!(parsed("{'command': 'ls'}"), json!({"command": "ls"}));
    }

    /// A realistic fragment of a stream that was cut off mid-command: the
    /// object and string are both unterminated.
    #[test]
    fn realistic_truncated_tool_call_fragment_is_repaired() {
        let fragment = r#"{"command": "cargo test --workspace --all-targets"#;
        assert_eq!(parsed(fragment), json!({"command": "cargo test --workspace --all-targets"}));

        let nested = r#"{"tool": "filesystem", "args": {"operation": "read", "path": "src/li"#;
        assert_eq!(
            parsed(nested),
            json!({"tool": "filesystem", "args": {"operation": "read", "path": "src/li"}})
        );
    }

    #[test]
    fn unrepairable_input_returns_error_with_excerpt_and_source() {
        let error = parse_tool_arguments("this is not json at all")
            .expect_err("non-JSON text must not be silently nulled");
        assert!(error.source.contains("expected"), "source: {}", error.source);
        assert_eq!(error.excerpt, "this is not json at all");
        assert_eq!(error.raw_len, "this is not json at all".len());

        // A long payload is excerpted, not carried whole.
        let long = "x".repeat(500);
        let error = parse_tool_arguments(&long).expect_err("garbage stays an error");
        assert!(error.excerpt.chars().count() <= MAX_ERROR_EXCERPT_CHARS + 1);
        assert_eq!(error.raw_len, 500);
    }

    /// The trailing-garbage rescan is byte-bounded: an oversized payload of
    /// non-JSON still terminates quickly and returns an error rather than
    /// performing an unbounded quadratic scan.
    #[test]
    fn oversized_unrepairable_payload_terminates() {
        let huge = "z".repeat(MAX_REPAIR_SCAN_BYTES + 1024);
        let error = parse_tool_arguments(&huge).expect_err("oversized garbage stays an error");
        assert_eq!(error.raw_len, huge.len());
    }

    #[test]
    fn double_encoded_arguments_are_unwrapped_with_a_bound() {
        // One layer: a JSON string containing a serialized object.
        let one = serde_json::Value::String(r#"{"command":"ls"}"#.to_string());
        assert_eq!(unwrap_argument_string_layers(one), json!({"command": "ls"}));

        // Two layers.
        let two = serde_json::Value::String(
            serde_json::to_string(&r#"{"command":"ls"}"#.to_string()).unwrap(),
        );
        assert_eq!(unwrap_argument_string_layers(two), json!({"command": "ls"}));

        // Not re-parseable: a legitimate raw string stays exactly as-is.
        let raw = serde_json::Value::String("ls".to_string());
        assert_eq!(unwrap_argument_string_layers(raw.clone()), raw);

        // Objects pass through untouched.
        let object = json!({"command": "ls"});
        assert_eq!(unwrap_argument_string_layers(object.clone()), object);

        // The layer budget is bounded: deeply nested strings stop peeling.
        let mut nested = json!({"command": "ls"});
        for _ in 0..MAX_ARGUMENT_STRING_LAYERS + 2 {
            nested = serde_json::Value::String(nested.to_string());
        }
        let result = unwrap_argument_string_layers(nested);
        assert!(result.is_string(), "budget must stop the unwrap: {result}");
    }
}
