//! Loose-tier tool-schema adaptation for weak tool-calling models.
//!
//! Weak models (audit: the MiMo family) stall the agent loop on nested
//! JSON-Schema tool parameters: they omit required nested fields, hallucinate
//! keys, and produce malformed argument objects. This module adapts the wire
//! presentation of tool schemas for such models *without* changing the tools'
//! advertised contracts:
//!
//! 1. FLATTEN — nested object properties are promoted to dot-notation leaves
//!    (`filter.name`, `options.retry.limit`) and removed from the schema, so
//!    the model only ever sees a flat key/value map. Required entries are
//!    remapped: a required nested field stays required as its dotted leaf,
//!    everything else becomes optional.
//! 2. ENRICH — every property carrying an `enum` gets a `Must be one of: …`
//!    sentence appended to its description, so weak models stop inventing
//!    enum members.
//! 3. EXAMPLE — built-in tools (`filesystem`, `shell`) get a concrete
//!    arguments example appended to their description. Field names in the
//!    examples MUST match the real advertised schemas (shell uses `cwd`, not
//!    `workdir`).
//! 4. NON-STREAMED TRANSPORT — completions for the tier are requested with
//!    `stream: false` so tool-call arguments arrive as one whole JSON object
//!    instead of streamed deltas that weak models sometimes drop
//!    mid-argument. See [`non_streaming_transport_active`].
//!
//! The round trip is closed by [`unflatten_tool_arguments`]: the owning
//! connector re-nests dot-notation argument keys on the way back, so the
//! executor and the orchestrator's tool-call guard still validate against the
//! tools' original nested schemas. Strict models are untouched: connectors
//! only call into this module when [`adaptive_tool_schemas_active`] resolves
//! to `true` for the request's model, and [`ToolSchemaMode::Auto`] (the
//! default) keeps every non-weak model on the verbatim schema — strict wire
//! output stays byte-identical.
//!
//! # Bounds
//!
//! All transforms are depth- and size-bounded so that adversarial or
//! degenerate schemas/model output cannot blow up memory or the stack:
//! flattening stops at [`MAX_FLATTEN_DEPTH`] levels and aborts for a tool
//! once [`MAX_FLATTENED_PROPERTIES`] leaves would be produced; un-nesting
//! refuses keys deeper than [`MAX_UNFLATTEN_DEPTH`] segments.

use concerto_config::ToolSchemaMode;
use concerto_core::types::ToolDefinition;

/// Maximum dot-path depth produced by flattening (`a.b.c` = depth 3).
///
/// Schemas nested deeper than this keep their nested form; weak models get
/// the guard's corrective-retry path there instead of an unbounded key list.
const MAX_FLATTEN_DEPTH: usize = 3;

/// Upper bound on the total number of properties a flattened tool schema may
/// have. Tools whose flattened leaf count would exceed this keep their nested
/// schema (flattening is skipped entirely for the tool).
const MAX_FLATTENED_PROPERTIES: usize = 64;

/// Maximum number of dot segments [`unflatten_tool_arguments`] will nest.
/// Deeper keys are left flat; the tool-call guard rejects them with a
/// corrective message instead of recursing unboundedly.
const MAX_UNFLATTEN_DEPTH: usize = 8;

/// Maximum enum members spelled out in an enriched description before the
/// list is truncated.
const MAX_ENUM_LISTING: usize = 20;

/// Decide whether tool schemas must be adapted (loose presentation) for a
/// request to `model`.
///
/// `Strict` never adapts, `Loose` always adapts, and `Auto` (the default)
/// adapts exactly when [`last_resort_weak_tool_calling_model`] matches. The name
/// heuristic is the last-resort default of the ADR-66 §3 capability chain,
/// never a first-line capability fact (see that function's docs and ADR-75).
pub fn adaptive_tool_schemas_active(configured: ToolSchemaMode, model: &str) -> bool {
    match configured {
        ToolSchemaMode::Strict => false,
        ToolSchemaMode::Loose => true,
        ToolSchemaMode::Auto => last_resort_weak_tool_calling_model(model),
    }
}

/// Resolve the wire transport for a completion request to `model`.
///
/// Weak tool-calling models can drop tool-call argument fragments
/// mid-stream, truncating the JSON arguments. Completions for the loose
/// tier are therefore requested **non-streamed** (`stream: false`) so every
/// arguments object arrives whole. The decision shares the loose-tier
/// predicate with [`adaptive_tool_schemas_active`] by construction — the
/// schema adaptation and the transport switch are one weak-model tier, not
/// two independent dials — so connectors resolve the tier once and apply
/// both adaptations together. Strong models keep the streamed transport.
pub fn non_streaming_transport_active(configured: ToolSchemaMode, model: &str) -> bool {
    adaptive_tool_schemas_active(configured, model)
}

/// Last-resort name heuristic for models with historically weak tool-calling.
///
/// This is a **LAST-RESORT default**, not a capability fact: it is the
/// bottom of the ADR-75 capability precedence chain
/// (`explicit user declaration > advertised metadata > this heuristic >
/// optimistic strict default`) and exists only because `Auto` is the default
/// dial and some models have no advertised capability metadata at all. The
/// authoritative sources are the explicit `tool_schema_mode` dial
/// (precedence level 1) and the provider-advertised capability flag
/// (`ModelInfo::supports_tool_calling`, level 2); unknown models with no
/// metadata resolve to the optimistic strict end — verbatim schema, streamed
/// transport — matching ADR-66 §3's "unknown models attempt native first".
/// This heuristic only selects the *presentation tier* (loose schema +
/// non-streamed transport); it can never mark a model as lacking tool
/// support.
///
/// Name-based prediction was rejected as a primary mechanism (ADR-75): the
/// same weights are served behind `:free` routes, so a price-tier token is
/// not a capability signal — e.g. `space-bunny-free` is MiniMax M3.1, a
/// strong tool-caller, and must never be degraded by its route name. The
/// only remaining hints are two weak-tier markers with an observed failure
/// history:
///
/// * `mimo` — the audit's MiMo-family stall pattern;
/// * `mini` — small/fast variants that historically omit nested fields.
///
/// Matching is **token-bounded** (ADR-66 §5): the name is split by the
/// shared [`crate::opencode::tokenize_model_name`] and a hint matches only
/// when it is *equal* to a whole dash-delimited token — never when it is a
/// substring of a larger one. So `gpt-5.4-mini` matches, while
/// `gemini-2.5-flash-lite` (`gemini` contains `mini`) and `minimax-m1`
/// (`minimax` contains `mini`) do not.
///
/// A future model whose name happens to carry one of the hint tokens can
/// still be forced onto the reliable path with the explicit
/// `tool_schema_mode = "strict"` dial (precedence level 1) or an advertised
/// capability flag (level 2); the real safety net is the detect-and-repair
/// path in [`crate::tool_args`], which is model-agnostic.
pub fn last_resort_weak_tool_calling_model(model: &str) -> bool {
    let tokens = crate::opencode::tokenize_model_name(model);
    ["mimo", "mini"].iter().any(|hint| tokens.iter().any(|token| token == hint))
}

/// Adapt a batch of tool definitions in place for weak-model presentation.
///
/// Pure and idempotent-flavored: applying it to an already-flat schema only
/// appends nothing (flat schemas have nothing to flatten) and re-appends no
/// examples (the example append is the caller's per-request decision, so a
/// re-render of the same request always starts from the original
/// definitions). Enum enrichment is idempotent — a description already
/// carrying the exact `Must be one of: …` sentence is left untouched.
pub fn adapt_tool_definitions(tools: &mut [ToolDefinition]) {
    for tool in tools {
        flatten_tool_schema(&mut tool.parameters);
        enrich_enum_descriptions(&mut tool.parameters);
        if let Some(example) = builtin_tool_example(&tool.name) {
            tool.description.push_str("\n\nExample arguments: ");
            tool.description.push_str(example);
        }
    }
}

/// Re-nest dot-notation argument keys into nested objects.
///
/// Applied by the connector on model-emitted tool arguments whenever the
/// request was rendered with loose schemas. Plain keys are laid down first,
/// dotted keys are overlaid afterwards (schema-driven dotted paths win), and
/// objects merge recursively. Keys without dots (the overwhelmingly common
/// case) are a cheap no-op, and keys deeper than [`MAX_UNFLATTEN_DEPTH`] or
/// containing empty segments are left flat rather than recursed.
pub fn unflatten_tool_arguments(arguments: &mut serde_json::Value) {
    let Some(object) = arguments.as_object_mut() else { return };
    if !object.keys().any(|key| key.contains('.')) {
        return;
    }

    let original = std::mem::take(object);
    let mut plain = serde_json::Map::new();
    let mut dotted: Vec<(Vec<String>, serde_json::Value)> = Vec::new();
    for (key, value) in original {
        let segments: Vec<String> = key.split('.').map(str::to_owned).collect();
        if is_unflattenable(&segments) {
            plain.insert(key, value);
        } else {
            dotted.push((segments, value));
        }
    }

    *object = plain;
    for (path, value) in dotted {
        insert_path(object, &path, value);
    }
}

/// A dot path is left flat when nesting it would be unbounded or degenerate:
/// too many segments, or an empty segment (e.g. `a..b` / a trailing dot).
fn is_unflattenable<S: AsRef<str>>(segments: &[S]) -> bool {
    segments.len() > MAX_UNFLATTEN_DEPTH
        || segments.iter().any(|segment| segment.as_ref().is_empty())
}

/// Insert `value` at `path` into `map`, merging with existing objects.
///
/// A scalar already occupying an intermediate path is replaced by an object:
/// the dotted key exists because the flattened schema demanded that path, so
/// the schema-driven shape wins.
fn insert_path(
    map: &mut serde_json::Map<String, serde_json::Value>,
    path: &[String],
    value: serde_json::Value,
) {
    let Some((head, tail)) = path.split_first() else { return };
    if tail.is_empty() {
        // Leaf: merge into an existing object at this key when both sides
        // are objects, otherwise overwrite.
        match map.get_mut(head).and_then(serde_json::Value::as_object_mut) {
            Some(existing) if value.is_object() => {
                for (key, member) in value.as_object().expect("checked is_object") {
                    existing.insert(key.clone(), member.clone());
                }
            }
            _ => {
                map.insert(head.clone(), value);
            }
        }
        return;
    }

    let entry = map
        .entry(head.clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(nested) = entry.as_object_mut() {
        insert_path(nested, tail, value);
    } else {
        let mut replacement = serde_json::Map::new();
        insert_path(&mut replacement, tail, value);
        map.insert(head.to_string(), serde_json::Value::Object(replacement));
    }
}

/// Flatten the top-level nested object properties of one tool schema.
///
/// Operates in place on the schema root. Each property that is an object
/// with declared `properties` is replaced by its promoted leaf properties
/// (dot-notation), recursively up to [`MAX_FLATTEN_DEPTH`]. `required` is
/// remapped: a required nested field survives as its dotted leaf, and
/// optional nested fields stay optional. The whole tool is left untouched
/// when nothing can be flattened or the leaf budget would be exceeded.
fn flatten_tool_schema(root: &mut serde_json::Value) {
    let Some(root_object) = root.as_object_mut() else { return };
    let Some(properties) =
        root_object.get_mut("properties").and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    // Pass 1 (pure): attempt to flatten every top-level property without
    // mutating the schema, so the leaf-budget check can abort cleanly.
    let mut promoted: Vec<(String, serde_json::Value)> = Vec::new();
    let mut promoted_names: Vec<String> = Vec::new();
    let mut required_leaves: Vec<String> = Vec::new();
    let mut retained: Vec<(String, serde_json::Value)> = Vec::new();
    for (name, schema) in properties.iter() {
        match flatten_object_property(name, schema, 1) {
            Some(mut flattened) => {
                promoted_names.push(name.clone());
                promoted.append(&mut flattened.entries);
                required_leaves.append(&mut flattened.required_leaves);
            }
            None => retained.push((name.clone(), schema.clone())),
        }
    }
    if promoted.is_empty() || retained.len() + promoted.len() > MAX_FLATTENED_PROPERTIES {
        return;
    }

    // Pass 2: rebuild `properties` in place (retained first, then promoted
    // leaves) and remap `required`.
    properties.clear();
    for (name, schema) in retained {
        properties.insert(name, schema);
    }
    for (name, schema) in promoted {
        properties.insert(name, schema);
    }

    let original_required: Vec<String> = root_object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect()
        })
        .unwrap_or_default();

    let mut required: Vec<String> = Vec::new();
    for entry in original_required {
        if promoted_names.contains(&entry) {
            // Promoted parent: its originally-required children survive as
            // dotted leaves; optional children stay optional. Entries that
            // were never in `properties` (dangling) are kept untouched.
            required.extend(
                required_leaves
                    .iter()
                    .filter(|leaf| leaf.starts_with(&format!("{entry}.")))
                    .cloned(),
            );
        } else {
            required.push(entry);
        }
    }
    if !required.is_empty() {
        root_object.insert(
            "required".to_owned(),
            serde_json::Value::Array(required.into_iter().map(serde_json::Value::String).collect()),
        );
    } else {
        root_object.remove("required");
    }
}

/// Flattened form of one nested object property: the dotted `(key, schema)`
/// leaves plus the dotted keys that were required in the nested schema.
struct FlattenedProperty {
    entries: Vec<(String, serde_json::Value)>,
    required_leaves: Vec<String>,
}

/// Flatten one object-typed property into dotted leaf entries.
///
/// Returns `None` when the property must stay nested: it is not an object
/// schema with declared (non-empty) `properties`, the recursion budget is
/// exhausted, or it is a free-form object (no `properties`) whose arbitrary
/// keys cannot be enumerated. On success it returns the dotted leaves and
/// the dotted keys that were required in the nested schema.
fn flatten_object_property(
    name: &str,
    schema: &serde_json::Value,
    depth: usize,
) -> Option<FlattenedProperty> {
    if depth >= MAX_FLATTEN_DEPTH {
        return None;
    }
    let object = schema.as_object()?;
    if let Some(kind) = object.get("type") {
        if kind.as_str() != Some("object") {
            return None;
        }
    }
    let nested = object.get("properties")?.as_object()?;
    if nested.is_empty() {
        return None;
    }
    let nested_required: std::collections::HashSet<&str> = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|entries| entries.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();

    let mut flattened = FlattenedProperty { entries: Vec::new(), required_leaves: Vec::new() };
    for (child_name, child_schema) in nested {
        let dotted = format!("{name}.{child_name}");
        match flatten_object_property(&dotted, child_schema, depth + 1) {
            Some(mut grandchildren) => {
                if nested_required.contains(child_name.as_str()) {
                    flattened.required_leaves.append(&mut grandchildren.required_leaves);
                }
                flattened.entries.extend(grandchildren.entries);
            }
            None => {
                if nested_required.contains(child_name.as_str()) {
                    flattened.required_leaves.push(dotted.clone());
                }
                flattened.entries.push((dotted, child_schema.clone()));
            }
        }
    }
    Some(flattened)
}

/// Append a `Must be one of: …` sentence to the description of every
/// property schema carrying a non-empty `enum`.
///
/// Walks `properties` and `items` trees recursively, so promoted leaves and
/// nested schemas that legitimately stay nested (arrays, free-form objects)
/// are covered alike. Idempotent: an exact duplicate sentence is not
/// appended twice.
fn enrich_enum_descriptions(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else { return };

    if let Some(enum_values) =
        object.get("enum").and_then(serde_json::Value::as_array).filter(|values| !values.is_empty())
    {
        let listing: Vec<String> =
            enum_values.iter().take(MAX_ENUM_LISTING).map(serde_json::Value::to_string).collect();
        let mut sentence = String::from("Must be one of: ");
        sentence.push_str(&listing.join(", "));
        if enum_values.len() > MAX_ENUM_LISTING {
            sentence.push_str(", …");
        }
        sentence.push('.');
        match object.get_mut("description") {
            Some(serde_json::Value::String(description)) if !description.contains(&sentence) => {
                description.push(' ');
                description.push_str(&sentence);
            }
            Some(serde_json::Value::String(_)) => {}
            _ => {
                object.insert("description".to_owned(), serde_json::Value::String(sentence));
            }
        }
    }

    if let Some(properties) =
        object.get_mut("properties").and_then(serde_json::Value::as_object_mut)
    {
        for child in properties.values_mut() {
            enrich_enum_descriptions(child);
        }
    }
    if let Some(items) = object.get_mut("items") {
        enrich_enum_descriptions(items);
    }
}

/// Concrete argument examples for the built-in tools, appended to tool
/// descriptions in loose mode.
///
/// Field names MUST match the advertised schemas exactly — a wrong field
/// name in an example would teach weak models to hallucinate that key.
/// (`shell` exposes the working directory as `cwd`, not `workdir`.)
fn builtin_tool_example(name: &str) -> Option<&'static str> {
    match name {
        "filesystem" => Some(r#"{"operation": "read", "path": "src/main.rs"}"#),
        "shell" => Some(r#"{"command": "cargo test", "cwd": "."}"#),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn weak_model_heuristic_matches_weak_family_tokens() {
        // The two remaining last-resort markers: the audit's MiMo family and
        // small/fast `mini` variants.
        assert!(last_resort_weak_tool_calling_model("mimo-v2.5"));
        assert!(last_resort_weak_tool_calling_model("MIMO-V2.5"));
        assert!(last_resort_weak_tool_calling_model("gpt-4o-mini"));
        assert!(last_resort_weak_tool_calling_model("gemini-2.0-flash-mini"));

        // Strong tool-callers never match, so Auto leaves them untouched.
        assert!(!last_resort_weak_tool_calling_model("claude-sonnet-4"));
        assert!(!last_resort_weak_tool_calling_model("gpt-4o"));
        assert!(!last_resort_weak_tool_calling_model(""));
    }

    /// Regression (ADR-75): a `*-free`/`:free` route name is a price tier, not
    /// a capability signal. `space-bunny-free` is MiniMax M3.1, a strong
    /// tool-caller, and must NOT be auto-classified weak — the removed `"free"`
    /// hint silently degraded exactly this class of model.
    #[test]
    fn free_route_names_are_not_classified_weak() {
        assert!(!last_resort_weak_tool_calling_model("space-bunny-free"));
        assert!(!last_resort_weak_tool_calling_model("space-bunny:free"));
        assert!(!last_resort_weak_tool_calling_model("deepseek/deepseek-r1:free"));
        assert!(!last_resort_weak_tool_calling_model("qwen3-coder:free"));
        assert!(!last_resort_weak_tool_calling_model("llama-3.3-70b-free"));

        // ...and the loose tier / non-streamed transport stay OFF for them.
        assert!(!adaptive_tool_schemas_active(ToolSchemaMode::Auto, "space-bunny-free"));
        assert!(!non_streaming_transport_active(ToolSchemaMode::Auto, "space-bunny-free"));
    }

    /// Genuine weak variants keep their tier: in each of these the hint IS a
    /// whole dash-delimited token (`mimo`, `mini`), so the token-bounded
    /// heuristic still classifies them weak and routes them onto loose
    /// schemas + non-streamed transport.
    #[test]
    fn weak_variant_names_still_classify_weak() {
        for model in ["mimo-v2.5-free", "gpt-5.4-mini", "qwen2.5-mini-instruct"] {
            assert!(
                last_resort_weak_tool_calling_model(model),
                "{model} must stay on the weak tier"
            );
            assert!(
                adaptive_tool_schemas_active(ToolSchemaMode::Auto, model),
                "{model}: loose schemas stay ON"
            );
            assert!(
                non_streaming_transport_active(ToolSchemaMode::Auto, model),
                "{model}: non-streamed transport stays ON"
            );
        }
    }

    /// Regression (ADR-66 §5): a hint must never match *inside* a larger
    /// token. `gemini` and `minimax` merely contain the `mini` substring —
    /// classifying them weak flipped every Gemini/MiniMax model onto loose
    /// schemas and `stream: false`.
    #[test]
    fn hint_inside_a_larger_token_does_not_classify_weak() {
        for model in ["gemini-2.5-flash-lite", "gemini-3.1-pro", "minimax-m1", "minimax-m2"] {
            assert!(
                !last_resort_weak_tool_calling_model(model),
                "{model} must stay on the strict tier"
            );
            assert!(
                !adaptive_tool_schemas_active(ToolSchemaMode::Auto, model),
                "{model}: loose schemas stay OFF"
            );
            assert!(
                !non_streaming_transport_active(ToolSchemaMode::Auto, model),
                "{model}: streamed transport stays ON"
            );
        }
    }

    /// The substring-vs-token distinction stated explicitly: `gemini` and
    /// `minimax` DO contain the `mini` substring, but neither carries it as a
    /// whole dash-delimited token, so the heuristic must not match them.
    #[test]
    fn substring_hint_never_matches_inside_a_larger_token() {
        assert!("gemini".contains("mini"), "precondition: the substring collision is real");
        assert!("minimax".contains("mini"), "precondition: the substring collision is real");

        for model in ["gemini", "gemini-2.5-flash-lite", "minimax", "minimax-m1"] {
            let tokens = crate::opencode::tokenize_model_name(model);
            assert!(!tokens.iter().any(|token| token == "mini"), "{model} tokens: {tokens:?}");
            assert!(
                !last_resort_weak_tool_calling_model(model),
                "{model} must not match the mini hint"
            );
        }

        // The converse: a name that carries the hint token itself matches.
        assert!(last_resort_weak_tool_calling_model("mini"));
        assert!(last_resort_weak_tool_calling_model("mimo"));
    }

    /// Pre-fix raw-substring heuristic, inlined ONLY as the "before" column
    /// of [`weak_heuristic_classifies_by_whole_tokens_not_substrings`] — it
    /// reproduces the historical bug (`gemini`/`minimax` matched `mini`) so
    /// the table below documents the actual before/after classifications.
    fn legacy_substring_heuristic(model: &str) -> bool {
        let lower = model.to_ascii_lowercase();
        ["mimo", "mini"].iter().any(|hint| lower.contains(hint))
    }

    /// Runnable before/after classification table: every weak-variant name
    /// that must stay weak, every substring false positive that must go
    /// strict, plus the explicit substring-vs-token rows. Run with
    /// `--nocapture` to see the table; each row pins BOTH the pre-fix
    /// substring column (documenting the bug) and the current token-bounded
    /// column (the contract).
    ///
    /// Columns: `before` = raw-substring heuristic (`free` already removed),
    /// `after` = [`last_resort_weak_tool_calling_model`] as implemented now.
    #[test]
    fn weak_heuristic_classifies_by_whole_tokens_not_substrings() {
        // (model, after — token-bounded, before — pre-fix substring)
        let table: &[(&str, bool, bool)] = &[
            // Genuinely weak: the hint IS a whole dash-delimited token.
            ("mimo-v2.5-free", true, true),
            ("gpt-5.4-mini", true, true),
            ("qwen2.5-mini-instruct", true, true),
            // False positives under substring matching: the hint appears
            // only INSIDE a larger token (`gemini`, `minimax`).
            ("gemini-2.5-flash-lite", false, true),
            ("gemini-3.1-pro", false, true),
            ("minimax-m1", false, true),
            ("minimax-m2", false, true),
            // Already correct before this fix (`free` hint removed earlier);
            // kept as a regression row.
            ("space-bunny-free", false, false),
            // The explicit substring-vs-token distinction: the bare family
            // token contains `mini` as a substring but is not `mini`…
            ("gemini", false, true),
            // …while a Gemini variant that really carries the `mini` token
            // stays weak after the fix.
            ("gemini-2.0-flash-mini", true, true),
        ];

        let render = |weak: bool| if weak { "WEAK" } else { "STRICT" };
        println!("{:<24} {:<16} after(token)", "model", "before(substring)");
        for &(model, after, before) in table {
            assert_eq!(
                legacy_substring_heuristic(model),
                before,
                "pre-fix substring column drifted for {model}"
            );
            assert_eq!(
                last_resort_weak_tool_calling_model(model),
                after,
                "token-bounded classification wrong for {model}"
            );
            println!("{model:<24} {:<16} {}", render(before), render(after));
        }
    }

    #[test]
    fn adaptive_active_resolves_all_modes() {
        assert!(adaptive_tool_schemas_active(ToolSchemaMode::Loose, "claude-sonnet-4"));
        assert!(!adaptive_tool_schemas_active(ToolSchemaMode::Strict, "mimo-v2.5"));
        assert!(adaptive_tool_schemas_active(ToolSchemaMode::Auto, "mimo-v2.5"));
        assert!(!adaptive_tool_schemas_active(ToolSchemaMode::Auto, "claude-sonnet-4"));
    }

    /// The weak tier is one tier with two coupled adaptations: loose schemas
    /// AND the non-streamed transport. Weak model names resolve to
    /// non-streamed completions so tool-call arguments arrive whole; strong
    /// names keep the streamed transport. Explicit `Loose`/`Strict` dials
    /// still win over the name heuristic, and the `free` route token no longer
    /// selects the tier.
    #[test]
    fn weak_models_resolve_to_non_streamed_transport() {
        assert!(non_streaming_transport_active(ToolSchemaMode::Auto, "mimo-v2.5"));
        assert!(non_streaming_transport_active(ToolSchemaMode::Auto, "gpt-4o-mini"));

        assert!(!non_streaming_transport_active(ToolSchemaMode::Auto, "claude-sonnet-4"));
        assert!(!non_streaming_transport_active(ToolSchemaMode::Auto, "gpt-4o"));
        assert!(!non_streaming_transport_active(ToolSchemaMode::Auto, "space-bunny-free"));
        assert!(!non_streaming_transport_active(ToolSchemaMode::Auto, ""));

        assert!(non_streaming_transport_active(ToolSchemaMode::Loose, "gpt-4o"));
        assert!(!non_streaming_transport_active(ToolSchemaMode::Strict, "mimo-v2.5"));
    }

    /// Representative two-level nested schema: the promoted required leaf
    /// survives in `required`, optional leaves become optional, and the
    /// nested parent disappears from `properties`.
    #[test]
    fn nested_schema_is_flattened_with_required_remap() {
        let mut tool = ToolDefinition {
            name: "query".into(),
            description: "Run a query.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["run", "explain"]},
                    "options": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "verbose": {"type": "boolean"},
                            "retry": {
                                "type": "object",
                                "properties": {"limit": {"type": "integer"}}
                            }
                        },
                        "required": ["name"]
                    }
                },
                "required": ["operation", "options"]
            }),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        let params = &tool.parameters;

        let properties = params["properties"].as_object().expect("properties object");
        assert!(!properties.contains_key("options"), "nested parent must be removed");
        for expected in ["operation", "options.name", "options.verbose", "options.retry.limit"] {
            assert!(properties.contains_key(expected), "missing flattened leaf {expected}");
        }
        // The three-level budget flattens `options.retry` fully into the
        // `options.retry.limit` leaf — no intermediate object remains.
        assert!(properties.get("options.retry").is_none());

        assert_eq!(
            params["required"],
            json!(["operation", "options.name"]),
            "required nested field survives as dotted leaf; optional ones drop out"
        );

        // Enum enrichment reached the top-level property.
        let operation = properties["operation"]["description"].as_str().expect("description");
        assert!(operation.contains("Must be one of: \"run\", \"explain\"."), "{operation}");
    }

    /// Flattening the real builtin tool shapes leaves them effectively
    /// unchanged (they are already flat) and appends the example line.
    #[test]
    fn builtin_flat_schemas_keep_shape_and_gain_examples() {
        let mut tools = vec![
            ToolDefinition {
                name: "filesystem".into(),
                description: "File ops.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "operation": {"type": "string", "enum": ["read", "write", "delete", "exists", "list", "move", "copy"]},
                        "path": {"type": "string", "description": "Path relative to project root."},
                        "content": {"type": ["string", "null"]}
                    },
                    "required": ["operation", "path"]
                }),
            },
            ToolDefinition {
                name: "shell".into(),
                description: "Shell exec.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The shell command to execute."},
                        "cwd": {"type": "string"}
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "mcp:server:custom".into(),
                description: "No builtin example.".into(),
                parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            },
        ];
        adapt_tool_definitions(&mut tools);

        let fs = &tools[0];
        assert!(fs
            .description
            .contains(r#"Example arguments: {"operation": "read", "path": "src/main.rs"}"#));
        assert!(fs.parameters["properties"].get("operation.path").is_none(), "nothing to flatten");
        assert_eq!(fs.parameters["required"], json!(["operation", "path"]));

        let shell = &tools[1];
        // The example MUST use the real `cwd` field, never a made-up key
        // (an example like `workdir` would teach weak models to hallucinate
        // exactly the kind of extra key the guard has to repair).
        assert!(shell.description.contains(r#"{"command": "cargo test", "cwd": "."}"#));
        assert!(!shell.description.contains("workdir"));

        let custom = &tools[2];
        assert_eq!(custom.description, "No builtin example.", "examples are builtin-only");
    }

    /// A schema whose flattened leaf count would exceed the budget keeps its
    /// nested form entirely.
    #[test]
    fn leaf_budget_aborts_flattening() {
        let mut wide_children = serde_json::Map::new();
        for i in 0..70 {
            wide_children.insert(format!("field{i}"), json!({"type": "string"}));
        }
        let mut tool = ToolDefinition {
            name: "wide".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {"big": {"type": "object", "properties": wide_children}},
                "required": ["big"]
            }),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        assert!(
            tool.parameters["properties"].get("big").is_some(),
            "flattening must be skipped when the leaf budget is exceeded"
        );
        assert_eq!(tool.parameters["required"], json!(["big"]));
    }

    /// Schemas nested beyond the depth budget keep their nested objects.
    #[test]
    fn depth_budget_keeps_deep_nesting() {
        let deep = json!({
            "type": "object",
            "properties": {
                "l1": {"type": "object", "properties": {
                    "l2": {"type": "object", "properties": {
                        "l3": {"type": "object", "properties": {
                            "l4": {"type": "object", "properties": {"leaf": {"type": "string"}}}
                        }}
                    }}
                }}
            }
        });
        let mut tool =
            ToolDefinition { name: "deep".into(), description: String::new(), parameters: deep };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));

        let properties = tool.parameters["properties"].as_object().unwrap();
        assert!(properties.contains_key("l1.l2.l3"), "three segments are within budget");
        assert!(
            properties["l1.l2.l3"]["properties"].get("l4").is_some(),
            "fourth level stays nested"
        );
    }

    /// Arrays (including arrays of objects) are never flattened — the
    /// element count is unknown, so dot-notation cannot address them.
    #[test]
    fn arrays_are_not_flattened() {
        let mut tool = ToolDefinition {
            name: "tags".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tag": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}}}}
                }
            }),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        let properties = tool.parameters["properties"].as_object().unwrap();
        assert!(properties.contains_key("tag"), "array property stays");
        assert!(properties.get("tag.name").is_none());
    }

    /// Free-form objects (no `properties`) stay nested: their keys cannot be
    /// enumerated ahead of time.
    #[test]
    fn free_form_objects_stay_nested() {
        let mut tool = ToolDefinition {
            name: "meta".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "labels": {"type": "object", "additionalProperties": {"type": "string"}}
                }
            }),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        assert!(tool.parameters["properties"].get("labels").is_some());
    }

    /// Enum enrichment is idempotent and covers array items too.
    #[test]
    fn enum_enrichment_is_idempotent_and_reaches_items() {
        let mut tool = ToolDefinition {
            name: "items".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kinds": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}}
                }
            }),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        let first = tool.parameters.clone();
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        assert_eq!(tool.parameters, first, "second pass must not double-append");

        let sentence = tool.parameters["properties"]["kinds"]["items"]["description"]
            .as_str()
            .expect("items description");
        assert_eq!(sentence, "Must be one of: \"a\", \"b\".");
    }

    /// Long enums are truncated in the description but kept in the schema.
    #[test]
    fn enum_listing_is_truncated() {
        let values: Vec<serde_json::Value> =
            (0..30).map(|i| serde_json::Value::String(format!("v{i}"))).collect();
        let mut tool = ToolDefinition {
            name: "long".into(),
            description: String::new(),
            parameters: json!({"type": "object", "properties": {"pick": {"type": "string", "enum": values}}}),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        let description = tool.parameters["properties"]["pick"]["description"].as_str().unwrap();
        assert!(description.contains("\"v19\""), "listing reaches the cap: {description}");
        assert!(description.contains('…'), "truncation marker present: {description}");
        assert!(!description.contains("\"v20\""), "listing stops at the cap: {description}");
        assert_eq!(tool.parameters["properties"]["pick"]["enum"].as_array().unwrap().len(), 30);
    }

    /// Model output shaped by the flattened schema re-nests into the
    /// original nested argument object.
    #[test]
    fn unflatten_round_trips_flattened_output() {
        let mut args = json!({
            "operation": "read",
            "options.name": "main",
            "options.retry.limit": 3,
            "options.retry": {"backoff": true}
        });
        unflatten_tool_arguments(&mut args);
        assert_eq!(
            args,
            json!({
                "operation": "read",
                "options": {"name": "main", "retry": {"limit": 3, "backoff": true}}
            })
        );
    }

    #[test]
    fn unflatten_merges_and_resolves_conflicts() {
        // Dotted overlay wins over a scalar occupying an intermediate path.
        let mut conflicting = json!({"a": "scalar", "a.b": 1});
        unflatten_tool_arguments(&mut conflicting);
        assert_eq!(conflicting, json!({"a": {"b": 1}}));

        // Dotless arguments are untouched (the common case).
        let mut plain = json!({"operation": "read", "path": "x"});
        unflatten_tool_arguments(&mut plain);
        assert_eq!(plain, json!({"operation": "read", "path": "x"}));

        // Non-object arguments pass through untouched.
        let mut null_args = serde_json::Value::Null;
        unflatten_tool_arguments(&mut null_args);
        assert_eq!(null_args, serde_json::Value::Null);
    }

    #[test]
    fn unflatten_respects_bounds() {
        // Empty segments and over-deep paths stay flat instead of recursing.
        let mut degenerate = json!({"a..b": 1, "x": {"y": 2}});
        unflatten_tool_arguments(&mut degenerate);
        assert!(degenerate.as_object().unwrap().contains_key("a..b"));

        let deep_key = (0..12).map(|i| format!("k{i}")).collect::<Vec<_>>().join(".");
        let mut too_deep = json!({ deep_key.clone(): 1 });
        unflatten_tool_arguments(&mut too_deep);
        assert!(too_deep.as_object().unwrap().contains_key(&deep_key), "over-deep key stays flat");
    }

    /// End-to-end shape: adapt the schema, emit the dotted arguments a weak
    /// model would produce, un-flatten them, and land on exactly the object
    /// the ORIGINAL nested schema expects.
    #[test]
    fn adapt_emit_unflatten_lands_on_original_shape() {
        let original = json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "properties": {
                        "mode": {"type": "string", "enum": ["fast", "safe"]},
                        "retries": {"type": "integer"}
                    },
                    "required": ["mode"]
                }
            },
            "required": ["config"]
        });

        let mut tool = ToolDefinition {
            name: "runner".into(),
            description: String::new(),
            parameters: original.clone(),
        };
        adapt_tool_definitions(std::slice::from_mut(&mut tool));
        let properties = tool.parameters["properties"].as_object().expect("properties object");
        assert!(properties.contains_key("config.mode"));

        let mut emitted = json!({"config.mode": "fast", "config.retries": 2});
        unflatten_tool_arguments(&mut emitted);
        assert_eq!(
            emitted,
            json!({"config": {"mode": "fast", "retries": 2}}),
            "re-nested arguments satisfy the original nested schema"
        );
    }
}
