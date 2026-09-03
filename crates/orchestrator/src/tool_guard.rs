//! Adaptive tool-call guard — VALIDATE → COERCE → REPAIR between the provider
//! and the tool executor.
//!
//! Weak models (audit: mimo-v2.5-free, 21× filesystem / 4× shell argument
//! stalls) emit tool calls whose accumulated `arguments` are `null` or empty,
//! JSON wrapped in fenced code blocks, stringified numbers, capitalized enum
//! values, or objects carrying hallucinated extra keys. This module repairs
//! those arguments against the tool's advertised JSON Schema
//! ([`concerto_core::types::ToolDefinition::parameters`], served from the
//! execution backend's registry) and, when arguments still cannot be repaired,
//! builds a structured corrective `ToolResult` so the model retries with
//! corrected arguments inside the same conversation.
//!
//! The parse, coerce, validate, and repair layers are generic over every
//! tool: they only read standard JSON Schema keywords (`properties`,
//! `required`, `type`, `enum`, `additionalProperties`) and never special-case
//! tool names. The one deliberate exception is the heuristic-inference layer
//! (Phase 2.5, adaptive tool-guard Solution 3): a bounded, name-keyed
//! dispatch to per-tool heuristics owned by `concerto-tools` — next to each
//! tool's schema — that recover missing required fields (e.g. filesystem
//! `operation` from path shape, shell `command` from a `cmd` alias).
//! Heuristic fills are logged, only applied to absent/`null` slots, and
//! accepted only after the completed arguments re-validate; otherwise the
//! corrective reject below proceeds unchanged.
//!
//! Provider-side coercion (`concerto_providers::protocol::ensure_arguments_object`)
//! is left untouched — this is the orchestrator-side second line of defense.

use serde_json::{Map, Value};

/// Maximum corrective-retry injections per tool name within one run before
/// the guard stops coaching and tells the model to move on. Two corrective
/// retries are allowed; the third consecutive rejection flips the message to
/// the exhausted form, bounding the retry loop at 2-3 attempts.
pub(crate) const MAX_TOOL_GUARD_REJECTS: u32 = 2;

/// Keys consumed by the execution-backend protocol rather than the tool
/// schema; the guard must never strip them as "unknown".
///
/// ADR-60 D5: `base_versions` rides inside tool-call arguments as a declared
/// concurrency-claim map and is lifted out by the write-gate backend
/// (`gate_proxy` / `in_process_gate`) before the tool sees the payload.
/// Stripping it upstream would silently disable declared-stale conflict
/// detection, so hallucination cleanup stops at this boundary.
const RESERVED_ARGUMENT_KEYS: [&str; 1] = ["base_versions"];

// ---------------------------------------------------------------------------
// Phase 1 — parse: normalize the raw `arguments` value into a JSON object
// ---------------------------------------------------------------------------

/// Normalize a provider-accumulated tool-call `arguments` value into a JSON
/// object.
///
/// * objects pass through unchanged;
/// * `null` and empty strings become `{}` (the audit's `null → {}` stalls);
/// * strings are parsed as JSON — first directly, then by extracting a fenced
///   ```json block, which weak models often wrap arguments in;
/// * any other shape (array, number, boolean) becomes `{}` so validation can
///   report the missing required fields instead of crashing on the shape.
pub(crate) fn parse_tool_arguments(raw: &Value) -> Value {
    match raw {
        Value::Object(map) => Value::Object(map.clone()),
        Value::String(text) => {
            parse_arguments_text(text).unwrap_or_else(|| Value::Object(Map::new()))
        }
        _ => Value::Object(Map::new()),
    }
}

/// Parse a stringified `arguments` value into a JSON object, if possible.
fn parse_arguments_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.is_object() {
            return Some(value);
        }
    }
    // Fenced ```json ... ``` block: weak models frequently wrap arguments in
    // markdown code fences instead of emitting raw JSON.
    let block = fenced_json_block(trimmed)?;
    match serde_json::from_str::<Value>(&block) {
        Ok(value) if value.is_object() => Some(value),
        _ => None,
    }
}

/// Extract the contents of the last fenced code block in `text`, if any.
fn fenced_json_block(text: &str) -> Option<String> {
    let start = text.find("```")? + 3;
    let rest = text[start..].trim_start();
    // Skip an optional language tag (e.g. `json`) on the fence's first line —
    // but only when that line is not already the JSON payload itself, so a
    // tagless fence keeps its content intact.
    let rest = if rest.starts_with('{') || rest.starts_with('[') {
        rest
    } else {
        match rest.find('\n') {
            Some(newline) => rest[newline + 1..].trim_start(),
            None => rest,
        }
    };
    let end = rest.rfind("```")?;
    Some(rest[..end].trim().to_string())
}

// ---------------------------------------------------------------------------
// Phase 2 — coerce: apply schema-guided safe fixes
// ---------------------------------------------------------------------------

/// Apply schema-guided coercions to a parsed `arguments` object.
///
/// Returns the (possibly modified) arguments plus a human-readable list of
/// every applied fix for `tracing::warn` visibility. Coercions are safe fixes
/// only: string → number/boolean where the schema demands it (and does not
/// also allow strings), case-insensitive enum normalization, and dropping
/// keys the schema does not declare (only when the schema itself does not
/// allow additional properties, and never for backend-protocol keys — see
/// [`RESERVED_ARGUMENT_KEYS`]). Values that already fit the schema —
/// including strings for string-typed fields — are never touched.
pub(crate) fn coerce_arguments(args: Value, schema: &Value) -> (Value, Vec<String>) {
    let mut coercions = Vec::new();
    let Value::Object(mut map) = args else {
        // `parse_tool_arguments` always yields an object; stay defensive and
        // hand non-objects to validation untouched.
        return (args, coercions);
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return (Value::Object(map), coercions);
    };

    // Strip keys the schema does not declare. In-repo tools deserialize with
    // serde's default (unknown fields ignored), so this is behavior-
    // preserving for them; external (MCP) tools are only stripped when the
    // schema does not explicitly allow additional properties. An empty or
    // absent `properties` map disables stripping entirely so free-form
    // schemas keep everything.
    let allows_additional = matches!(schema.get("additionalProperties"), Some(Value::Bool(true)));
    if !properties.is_empty() && !allows_additional {
        let unknown: Vec<String> = map
            .keys()
            .filter(|key| {
                !properties.contains_key(*key) && !RESERVED_ARGUMENT_KEYS.contains(&key.as_str())
            })
            .cloned()
            .collect();
        for key in unknown {
            map.remove(&key);
            coercions.push(format!("removed unknown property '{key}'"));
        }
    }

    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        let Some(prop) = properties.get(key.as_str()) else {
            continue;
        };
        let Value::String(text) = &map[&key] else {
            continue;
        };
        let text = text.clone();

        // 1. Enum normalization: case-insensitive match against declared
        //    values (weak models capitalize enum entries).
        if let Some(values) = enum_array(prop) {
            let declared = values.iter().any(|v| v.as_str() == Some(text.as_str()));
            if !declared {
                if let Some(normalized) = values.iter().find_map(|v| {
                    v.as_str().filter(|candidate| candidate.eq_ignore_ascii_case(&text))
                }) {
                    map.insert(key.clone(), Value::String(normalized.to_string()));
                    coercions.push(format!("normalized '{key}' value '{text}' to '{normalized}'"));
                    continue;
                }
            }
        }

        let types = schema_types(prop);
        // Never coerce a string where the schema also allows strings: a path
        // of "123" must stay a string.
        if types.contains(&"string") {
            continue;
        }
        // 2. Stringified numbers where the schema demands a number.
        if types.contains(&"integer") || types.contains(&"number") {
            if let Some(number) = parse_number(&text, types.contains(&"integer")) {
                map.insert(key.clone(), Value::Number(number));
                coercions.push(format!("coerced '{key}' string '{text}' to a number"));
                continue;
            }
        }
        // 3. Stringified booleans where the schema demands a boolean.
        if types.contains(&"boolean") {
            if let Some(boolean) = parse_bool(&text) {
                map.insert(key.clone(), Value::Bool(boolean));
                coercions.push(format!("coerced '{key}' string '{text}' to a boolean"));
            }
        }
    }

    (Value::Object(map), coercions)
}

/// Parse a stringified number, preferring exact integers. Returns `None` when
/// `integer_only` and the text has a fractional part, or when the value is
/// not representable as a JSON number (NaN/Infinity).
fn parse_number(text: &str, integer_only: bool) -> Option<serde_json::Number> {
    let text = text.trim();
    if let Ok(value) = text.parse::<i64>() {
        return Some(serde_json::Number::from(value));
    }
    if let Ok(value) = text.parse::<u64>() {
        return Some(serde_json::Number::from(value));
    }
    if integer_only {
        return None;
    }
    text.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
}

/// Parse a stringified boolean using the common truthy/falsy vocabulary.
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Phase 2.5 — heuristic inference: per-tool last-mile recovery
// ---------------------------------------------------------------------------

/// The required fields of `schema` that carry no usable value in `args`:
/// either absent or explicitly `null`. Validation reports those as missing
/// fields or type errors; this is the trigger set for heuristic inference.
fn unresolved_required_fields(args: &Value, schema: &Value) -> Vec<String> {
    let Some(object) = args.as_object() else {
        return Vec::new();
    };
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|field| object.get(*field).is_none_or(Value::is_null))
        .map(str::to_string)
        .collect()
}

/// Attempt conservative, per-tool heuristic inference for unresolved required
/// fields (adaptive tool-guard Solution 3).
///
/// Dispatches by tool name to heuristics exported by `concerto-tools`
/// ([`concerto_tools::filesystem::infer_missing_arguments`],
/// [`concerto_tools::shell::infer_missing_arguments`]). Tools without a
/// registered heuristic — including every external MCP tool — get none:
/// generic inference would be guessing, which this layer must not do.
///
/// `raw` is the model's ORIGINAL parsed argument object (pre-coercion, so
/// hallucinated alias keys such as `cmd` are still present for alias
/// recovery); `arguments` is the post-coercion object, mutated in place.
///
/// Returns human-readable notes for `tracing` when at least one field was
/// filled in, `None` otherwise. Fills only touch absent/`null` slots — a real
/// value is never overwritten — and the caller MUST re-run
/// [`coerce_arguments`] and [`validate_arguments`]: inference is accepted
/// only when the completed arguments validate cleanly, otherwise the usual
/// corrective reject applies.
pub(crate) fn heuristic_infer(
    tool: &str,
    raw: &Value,
    arguments: &mut Value,
    schema: &Value,
) -> Option<Vec<String>> {
    let raw_map = raw.as_object()?;
    let missing = unresolved_required_fields(arguments, schema);
    if missing.is_empty() {
        return None;
    }
    let insertions = match tool {
        "filesystem" => concerto_tools::filesystem::infer_missing_arguments(raw_map, &missing),
        "shell" => concerto_tools::shell::infer_missing_arguments(raw_map, &missing),
        _ => Vec::new(),
    };
    if insertions.is_empty() {
        return None;
    }
    let target = arguments.as_object_mut()?;
    let mut notes = Vec::new();
    for (field, value) in insertions {
        match target.get(&field) {
            None | Some(Value::Null) => {
                notes.push(format!("filled missing '{field}' with {value}"));
                target.insert(field, value);
            }
            Some(_) => {}
        }
    }
    if notes.is_empty() {
        None
    } else {
        Some(notes)
    }
}

// ---------------------------------------------------------------------------
// Phase 3 — validate: required fields, types, and enum membership
// ---------------------------------------------------------------------------

/// Validate parsed arguments against a tool's JSON Schema.
///
/// Returns field-level error strings (empty when valid). Validation is
/// deliberately one level deep over the schema's top-level `properties` —
/// bounded work, and every in-repo tool contract is a flat object. Unknown
/// type keywords never invent failures.
pub(crate) fn validate_arguments(args: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(object) = args.as_object() else {
        errors.push("arguments: must be a JSON object".to_string());
        return errors;
    };

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required {
            if let Some(name) = field.as_str() {
                if !object.contains_key(name) {
                    errors.push(missing_field_error(name, schema));
                }
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, value) in object {
            let Some(prop) = properties.get(name.as_str()) else {
                continue;
            };
            validate_property(name, value, prop, &mut errors);
        }
    }
    errors
}

/// Validate one property value against its sub-schema (enum + type).
fn validate_property(name: &str, value: &Value, prop: &Value, errors: &mut Vec<String>) {
    if let Some(values) = enum_array(prop) {
        if !values.contains(value) {
            errors.push(format!("{name}: must be one of {} (got {value})", enum_list_text(values)));
            return;
        }
    }
    let types = schema_types(prop);
    if types.is_empty() || types.iter().any(|kind| type_matches(value, kind)) {
        return;
    }
    errors.push(format!("{name}: expected {}, got {}", types.join(" or "), json_kind(value)));
}

/// Missing-required-field error, carrying the schema's enum membership as a
/// hint when the field is an enum (e.g. `operation`).
fn missing_field_error(name: &str, schema: &Value) -> String {
    let values =
        schema.get("properties").and_then(|properties| properties.get(name)).and_then(enum_array);
    match values {
        Some(values) => {
            format!("missing required field '{name}' (one of {})", enum_list_text(values))
        }
        None => format!("missing required field '{name}'"),
    }
}

/// Does `value` satisfy the JSON Schema type keyword `kind`?
fn type_matches(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value.as_f64().is_some_and(|float| float.fract() == 0.0)
        }
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown type keyword: never invent a failure from it.
        _ => true,
    }
}

/// Human-readable JSON kind name for "got X" diagnostics.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Phase 4 — repair: structured corrective ToolResult
// ---------------------------------------------------------------------------

/// Structured payload for the corrective `ToolResult` injected back to the
/// model (mirrors the `validation_tool_result` contract used by the
/// specialist agents' forced submissions).
pub(crate) fn corrective_tool_result(
    tool_name: &str,
    errors: &[String],
    schema: &Value,
    exhausted: bool,
) -> Value {
    if exhausted {
        serde_json::json!({
            "error": "tool_guard_exhausted",
            "tool": tool_name,
            "message": corrective_message_text(tool_name, errors, schema, true),
            "field_errors": errors,
            "recovery": "stop_or_ask_user",
        })
    } else {
        serde_json::json!({
            "error": "invalid_tool_arguments",
            "tool": tool_name,
            "message": corrective_message_text(tool_name, errors, schema, false),
            "field_errors": errors,
            "example": schema_example(schema),
            "recovery": "correct_and_retry",
        })
    }
}

/// Human-readable corrective sentence shown to the model as the tool message
/// content, e.g.
/// `Tool call invalid for 'filesystem': missing required field 'operation'
/// (one of ["read","write",...]). Example: {"operation":"read",...}.
/// Please retry with corrected arguments.`
pub(crate) fn corrective_message_text(
    tool_name: &str,
    errors: &[String],
    schema: &Value,
    exhausted: bool,
) -> String {
    let joined = errors.join("; ");
    if exhausted {
        format!(
            "Tool call invalid for '{tool_name}': {joined}. Repeated corrections have not fixed \
             the arguments. Stop calling '{tool_name}' with invalid arguments: either construct \
             a fully valid argument object or continue the task without this tool and report \
             the blocker to the user."
        )
    } else {
        let example =
            serde_json::to_string(&schema_example(schema)).unwrap_or_else(|_| "{}".to_string());
        format!(
            "Tool call invalid for '{tool_name}': {joined}. Example: {example}. Please retry \
             with corrected arguments."
        )
    }
}

/// Build a minimal example object covering the schema's required fields.
fn schema_example(schema: &Value) -> Value {
    let mut example = Map::new();
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Value::Object(example);
    };
    let properties = schema.get("properties").and_then(Value::as_object);
    for field in required.iter().filter_map(Value::as_str) {
        let prop = properties.and_then(|p| p.get(field));
        example.insert(field.to_string(), placeholder_for(prop));
    }
    Value::Object(example)
}

/// Placeholder value for one property in a generated example.
fn placeholder_for(prop: Option<&Value>) -> Value {
    let Some(prop) = prop else {
        return Value::String("<value>".to_string());
    };
    if let Some(values) = enum_array(prop) {
        if let Some(first) = values.iter().find_map(Value::as_str) {
            return Value::String(first.to_string());
        }
    }
    for kind in schema_types(prop) {
        match kind {
            "string" => return Value::String("<value>".to_string()),
            "integer" | "number" => return Value::from(0),
            "boolean" => return Value::Bool(false),
            "array" => return Value::Array(Vec::new()),
            "object" => return Value::Object(Map::new()),
            _ => continue,
        }
    }
    Value::String("<value>".to_string())
}

// ---------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------

/// The property's `enum` values, when declared as an array.
fn enum_array(prop: &Value) -> Option<&Vec<Value>> {
    prop.get("enum").and_then(Value::as_array)
}

/// Compact text form of an enum list, e.g. `["read","write"]`.
fn enum_list_text(values: &[Value]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

/// The property's `type` keyword(s): a single string or an array of strings.
fn schema_types(prop: &Value) -> Vec<&str> {
    match prop.get("type") {
        Some(Value::String(single)) => vec![single.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::traits::tool::Tool;
    use concerto_tools::filesystem::FilesystemTool;

    /// Filesystem-tool-shaped schema (mirrors the real one, enum included).
    fn filesystem_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["read", "write", "delete", "exists", "list", "move", "copy"]
                },
                "path": { "type": "string" },
                "content": { "type": ["string", "null"] },
                "destination": { "type": ["string", "null"] }
            },
            "required": ["operation", "path"]
        })
    }

    /// Shell-tool-shaped schema with numeric/boolean optionals.
    fn shell_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "args": { "type": "array" },
                "cwd": { "type": ["string", "null"] },
                "timeout_secs": { "type": ["integer", "null"] },
                "dry_run": { "type": ["boolean", "null"] }
            },
            "required": ["command"]
        })
    }

    // -- parse ---------------------------------------------------------------

    #[test]
    fn null_arguments_parse_to_empty_object() {
        let parsed = parse_tool_arguments(&Value::Null);
        assert_eq!(parsed, serde_json::json!({}));
    }

    #[test]
    fn empty_string_arguments_parse_to_empty_object() {
        assert_eq!(parse_tool_arguments(&Value::String(String::new())), serde_json::json!({}));
        assert_eq!(parse_tool_arguments(&Value::String("   ".into())), serde_json::json!({}));
    }

    #[test]
    fn stringified_json_parses_to_object() {
        let parsed = parse_tool_arguments(&Value::String(r#"{"operation":"read"}"#.into()));
        assert_eq!(parsed, serde_json::json!({"operation": "read"}));
    }

    #[test]
    fn fenced_json_block_parses_to_object() {
        let raw = Value::String("```json\n{\"operation\": \"read\"}\n```".into());
        assert_eq!(parse_tool_arguments(&raw), serde_json::json!({"operation": "read"}));

        // Without a language tag, and embedded in surrounding prose.
        let raw = Value::String("args: ```\n{\"path\": \"a.rs\"}\n```".into());
        assert_eq!(parse_tool_arguments(&raw), serde_json::json!({"path": "a.rs"}));
    }

    #[test]
    fn non_object_json_string_yields_empty_object_for_validation() {
        // A bare quoted string parses as JSON but is not an object → `{}` so
        // validation reports the missing required fields.
        let parsed = parse_tool_arguments(&Value::String("\"ls -la\"".into()));
        assert_eq!(parsed, serde_json::json!({}));
    }

    #[test]
    fn non_object_shapes_yield_empty_object() {
        assert_eq!(parse_tool_arguments(&serde_json::json!([1, 2])), serde_json::json!({}));
        assert_eq!(parse_tool_arguments(&serde_json::json!(3)), serde_json::json!({}));
    }

    // -- coerce --------------------------------------------------------------

    #[test]
    fn string_integer_coercion() {
        let (coerced, notes) = coerce_arguments(
            serde_json::json!({"command": "sleep", "timeout_secs": "30"}),
            &shell_schema(),
        );
        assert_eq!(coerced, serde_json::json!({"command": "sleep", "timeout_secs": 30}));
        assert!(notes.iter().any(|note| note.contains("timeout_secs")), "notes: {notes:?}");
    }

    #[test]
    fn string_boolean_coercion() {
        let (coerced, _) = coerce_arguments(
            serde_json::json!({"command": "rm", "dry_run": "yes"}),
            &shell_schema(),
        );
        assert_eq!(coerced, serde_json::json!({"command": "rm", "dry_run": true}));
    }

    #[test]
    fn unparseable_number_string_is_left_for_validation() {
        let (coerced, notes) = coerce_arguments(
            serde_json::json!({"command": "sleep", "timeout_secs": "abc"}),
            &shell_schema(),
        );
        assert_eq!(coerced["timeout_secs"], serde_json::json!("abc"));
        assert!(notes.is_empty());
    }

    #[test]
    fn enum_value_normalized_case_insensitively() {
        let (coerced, notes) = coerce_arguments(
            serde_json::json!({"operation": "READ", "path": "a.rs"}),
            &filesystem_schema(),
        );
        assert_eq!(coerced, serde_json::json!({"operation": "read", "path": "a.rs"}));
        assert!(notes.iter().any(|note| note.contains("normalized")), "notes: {notes:?}");
    }

    #[test]
    fn unknown_properties_are_stripped() {
        let (coerced, notes) = coerce_arguments(
            serde_json::json!({"operation": "read", "path": "a.rs", "hallucinated": true}),
            &filesystem_schema(),
        );
        assert_eq!(coerced, serde_json::json!({"operation": "read", "path": "a.rs"}));
        assert!(notes.iter().any(|note| note.contains("hallucinated")), "notes: {notes:?}");
    }

    #[test]
    fn string_typed_fields_are_never_numeric_coerced() {
        // `path` allows strings: "123" must stay a string.
        let (coerced, notes) =
            coerce_arguments(serde_json::json!({"path": "123"}), &filesystem_schema());
        assert_eq!(coerced, serde_json::json!({"path": "123"}));
        assert!(notes.is_empty());
    }

    #[test]
    fn schema_allowing_additional_properties_keeps_unknown_keys() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "additionalProperties": true,
            "required": ["command"]
        });
        let (coerced, notes) =
            coerce_arguments(serde_json::json!({"command": "ls", "extra": 1}), &schema);
        assert_eq!(coerced, serde_json::json!({"command": "ls", "extra": 1}));
        assert!(notes.is_empty());
    }

    #[test]
    fn schema_without_properties_never_strips() {
        // EchoTool-style free-form schema: everything passes through.
        let (coerced, _) =
            coerce_arguments(serde_json::json!({"text": "hi"}), &serde_json::json!({}));
        assert_eq!(coerced, serde_json::json!({"text": "hi"}));
    }

    #[test]
    fn gate_protocol_keys_are_never_stripped() {
        // ADR-60 D5: `base_versions` is a declared concurrency claim consumed
        // by the write-gate backend (not a tool-schema field) — stripping it
        // would disable declared-stale conflict detection.
        let (coerced, _) = coerce_arguments(
            serde_json::json!({
                "operation": "write",
                "path": "shared.txt",
                "content": "hi",
                "base_versions": { "shared.txt": "abc123" },
                "hallucinated": true
            }),
            &filesystem_schema(),
        );
        assert_eq!(
            coerced["base_versions"],
            serde_json::json!({ "shared.txt": "abc123" }),
            "declared gate claims must survive the guard"
        );
        assert!(coerced.get("hallucinated").is_none(), "hallucinated keys still stripped");
    }

    // -- validate ------------------------------------------------------------

    #[test]
    fn missing_required_fields_reported_with_enum_hint() {
        let errors = validate_arguments(&serde_json::json!({}), &filesystem_schema());
        assert_eq!(errors.len(), 2, "errors: {errors:?}");
        assert!(
            errors.iter().any(|error| error
                == "missing required field 'operation' (one of \
                    [\"read\",\"write\",\"delete\",\"exists\",\"list\",\"move\",\"copy\"])"),
            "errors: {errors:?}"
        );
        assert!(errors.iter().any(|error| error == "missing required field 'path'"));
    }

    #[test]
    fn valid_arguments_produce_no_errors() {
        let errors = validate_arguments(
            &serde_json::json!({"operation": "read", "path": "src/main.rs"}),
            &filesystem_schema(),
        );
        assert!(errors.is_empty(), "errors: {errors:?}");
    }

    #[test]
    fn type_mismatch_reported() {
        let errors = validate_arguments(
            &serde_json::json!({"operation": "read", "path": 42}),
            &filesystem_schema(),
        );
        assert_eq!(errors, vec!["path: expected string, got number".to_string()]);
    }

    #[test]
    fn enum_mismatch_reported() {
        let errors = validate_arguments(
            &serde_json::json!({"operation": "destroy", "path": "a.rs"}),
            &filesystem_schema(),
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert!(errors[0].starts_with("operation: must be one of "), "errors: {errors:?}");
    }

    #[test]
    fn integer_type_accepts_integral_float() {
        let errors = validate_arguments(
            &serde_json::json!({"command": "sleep", "timeout_secs": 2.0}),
            &shell_schema(),
        );
        assert!(errors.is_empty(), "errors: {errors:?}");
    }

    // -- corrective result ---------------------------------------------------

    #[test]
    fn corrective_message_matches_contract() {
        let errors = vec!["missing required field 'operation'".to_string()];
        let message = corrective_message_text("filesystem", &errors, &filesystem_schema(), false);
        assert!(message.starts_with("Tool call invalid for 'filesystem':"), "message: {message}");
        assert!(message.contains("missing required field 'operation'"), "message: {message}");
        assert!(message.contains("Example: {\"operation\":\"read\""), "message: {message}");
        assert!(message.ends_with("Please retry with corrected arguments."), "message: {message}");
    }

    #[test]
    fn exhausted_message_tells_model_to_stop() {
        let errors = vec!["missing required field 'command'".to_string()];
        let message = corrective_message_text("shell", &errors, &shell_schema(), true);
        assert!(message.contains("Stop calling 'shell'"), "message: {message}");
        assert!(message.contains("report the blocker to the user"), "message: {message}");
        assert!(!message.contains("Please retry"), "message: {message}");
    }

    #[test]
    fn corrective_tool_result_carries_structured_fields() {
        let errors = vec!["missing required field 'operation'".to_string()];
        let payload = corrective_tool_result("filesystem", &errors, &filesystem_schema(), false);
        assert_eq!(payload["error"], "invalid_tool_arguments");
        assert_eq!(payload["tool"], "filesystem");
        assert_eq!(payload["recovery"], "correct_and_retry");
        assert_eq!(payload["example"]["operation"], "read");

        let exhausted = corrective_tool_result("filesystem", &errors, &filesystem_schema(), true);
        assert_eq!(exhausted["error"], "tool_guard_exhausted");
        assert_eq!(exhausted["recovery"], "stop_or_ask_user");
    }

    // -- real tool schema ----------------------------------------------------

    #[test]
    fn real_filesystem_tool_schema_advertises_operation_enum() {
        let schema = FilesystemTool::new(camino::Utf8PathBuf::from(".")).input_schema();
        // The guard's enum hint depends on the advertised schema carrying the
        // operation enum — pin it so the hint cannot silently regress.
        let operation = &schema["properties"]["operation"];
        assert!(operation["enum"].is_array(), "real schema: {schema}");
        assert!(operation["enum"].as_array().unwrap().len() >= 5, "real schema: {schema}");

        let errors = validate_arguments(&serde_json::json!({"path": "src/main.rs"}), &schema);
        assert!(
            errors.iter().any(|error| error.contains("one of")),
            "missing-operation error must carry the enum hint: {errors:?}"
        );
    }

    // -- heuristic inference (Solution 3) --------------------------------------

    #[test]
    fn heuristic_infers_read_for_file_like_path() {
        // Canonical Solution-3 example: filesystem with only `path` infers
        // `read`.
        let mut args = serde_json::json!({});
        let notes = heuristic_infer(
            "filesystem",
            &serde_json::json!({ "path": "src/main.rs" }),
            &mut args,
            &filesystem_schema(),
        );
        assert!(notes.is_some(), "notes: {notes:?}");
        assert_eq!(args["operation"], "read");
        assert!(notes.unwrap().iter().any(|note| note.contains("'operation'")));
    }

    #[test]
    fn heuristic_infers_list_for_directory_like_paths() {
        for path in [".", "src/", "src\\"] {
            let mut args = serde_json::json!({});
            let notes = heuristic_infer(
                "filesystem",
                &serde_json::json!({ "path": path }),
                &mut args,
                &filesystem_schema(),
            );
            assert!(notes.is_some(), "path {path:?}: {notes:?}");
            assert_eq!(args["operation"], "list", "path {path:?} must infer list");
        }
    }

    #[test]
    fn heuristic_infers_write_when_content_is_present() {
        let mut args = serde_json::json!({});
        let notes = heuristic_infer(
            "filesystem",
            &serde_json::json!({ "path": "a.rs", "content": "hi" }),
            &mut args,
            &filesystem_schema(),
        );
        assert!(notes.is_some(), "notes: {notes:?}");
        assert_eq!(args["operation"], "write");
    }

    #[test]
    fn heuristic_alias_beats_shape_inference() {
        // Direct evidence (an alias carrying an operation value) wins over
        // path-shape guessing; case normalization happens in the re-coerce.
        let mut args = serde_json::json!({});
        heuristic_infer(
            "filesystem",
            &serde_json::json!({ "op": "LIST", "path": "src/main.rs" }),
            &mut args,
            &filesystem_schema(),
        );
        assert_eq!(args["operation"], "LIST");
    }

    #[test]
    fn heuristic_recovers_path_from_file_alias() {
        let mut args = serde_json::json!({});
        let notes = heuristic_infer(
            "filesystem",
            &serde_json::json!({ "file": "src/main.rs" }),
            &mut args,
            &filesystem_schema(),
        );
        assert!(notes.is_some(), "notes: {notes:?}");
        assert_eq!(args["operation"], "read", "path alias feeds shape inference too");
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn heuristic_shell_recovers_command_from_cmd_alias() {
        // Canonical Solution-3 example: shell with `cmd` infers `command`.
        let mut args = serde_json::json!({});
        let notes = heuristic_infer(
            "shell",
            &serde_json::json!({ "cmd": "cargo test" }),
            &mut args,
            &shell_schema(),
        );
        assert!(notes.is_some(), "notes: {notes:?}");
        assert_eq!(args["command"], "cargo test");
    }

    #[test]
    fn heuristic_never_overwrites_present_values() {
        // `operation` is present → only `path` is unresolved; the present
        // value must survive while the alias fills the gap.
        let mut args = serde_json::json!({ "operation": "write" });
        let notes = heuristic_infer(
            "filesystem",
            &serde_json::json!({ "file": "a.rs" }),
            &mut args,
            &filesystem_schema(),
        );
        assert!(notes.is_some(), "path alias should fill the missing path");
        assert_eq!(args["operation"], "write");
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn heuristic_treats_null_required_fields_as_unresolved() {
        // Realistic pipeline state: the model sent a null `operation` beside a
        // valid `path`; parse/coerce leave both, and the null is unresolved.
        let raw = serde_json::json!({ "operation": null, "path": "a.rs" });
        let (coerced, _) = coerce_arguments(raw.clone(), &filesystem_schema());
        let mut repaired = coerced;
        let notes = heuristic_infer("filesystem", &raw, &mut repaired, &filesystem_schema());
        assert!(notes.is_some(), "null operation is unresolved: {notes:?}");
        assert_eq!(repaired["operation"], "read");
        assert_eq!(repaired["path"], "a.rs");
    }

    #[test]
    fn heuristic_fully_resolved_arguments_yield_none() {
        let mut args = serde_json::json!({ "operation": "read", "path": "a.rs" });
        let notes = heuristic_infer(
            "filesystem",
            &serde_json::json!({ "op": "write", "file": "b.rs" }),
            &mut args,
            &filesystem_schema(),
        );
        assert!(notes.is_none());
        assert_eq!(args, serde_json::json!({ "operation": "read", "path": "a.rs" }));
    }

    #[test]
    fn heuristic_unknown_tool_yields_none() {
        // Tools without a registered heuristic (MCP, git, ...) get none:
        // generic inference would be guessing.
        let mut args = serde_json::json!({});
        let notes = heuristic_infer(
            "mcp:srv:tool",
            &serde_json::json!({ "cmd": "ls" }),
            &mut args,
            &shell_schema(),
        );
        assert!(notes.is_none());
    }

    #[test]
    fn heuristic_without_grounding_signal_yields_none() {
        // No path, no content, no alias: nothing to ground `operation` on,
        // and a `path` is never invented — the guard must reject instead.
        let mut args = serde_json::json!({});
        let notes =
            heuristic_infer("filesystem", &serde_json::json!({}), &mut args, &filesystem_schema());
        assert!(notes.is_none());
    }

    #[test]
    fn heuristic_pipeline_completes_path_only_filesystem_call() {
        // Full pipeline against the REAL filesystem schema: parse → coerce →
        // infer → re-coerce → re-validate.
        let schema = FilesystemTool::new(camino::Utf8PathBuf::from(".")).input_schema();
        let parsed = parse_tool_arguments(&serde_json::json!({ "path": "src/main.rs" }));
        let (coerced, _) = coerce_arguments(parsed.clone(), &schema);
        assert!(!validate_arguments(&coerced, &schema).is_empty());

        let mut repaired = coerced;
        let notes = heuristic_infer("filesystem", &parsed, &mut repaired, &schema)
            .expect("path-only call must be inferable");
        let (repaired, _) = coerce_arguments(repaired, &schema);
        assert!(
            validate_arguments(&repaired, &schema).is_empty(),
            "inferred arguments must validate: {notes:?}"
        );
        assert_eq!(repaired["operation"], "read");
    }
}
