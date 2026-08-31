//! Google Gemini chat dialect (`generateContent` / `streamGenerateContent`).
//!
//! This adapter lowers a canonical [`CompletionRequest`] to the wire body of
//! Google's Gemini `generateContent` endpoint. The logic here is a verbatim
//! port of the body builder that previously lived in `crate::google`; only the
//! seam changed, not the bytes.
//!
//! # Wire contract
//!
//! - `contents`: a flat array of entries with `role` (`"user"`/`"model"`) and
//!   `parts`.
//! - System prompt: top-level `system_instruction` with
//!   `{"parts":[{"text": ...}]}`; the last `Role::System` message wins.
//! - User parts: `{"text": ...}`. Tool results are appended as `user` parts
//!   `{"functionResponse":{"name":..., "response":...}}` — one entry per tool
//!   result, matched by *function name*, not call id.
//! - Assistant parts: `{"text": ...}`; when the assistant message carries
//!   `tool_calls` the `parts` array is *replaced* by
//!   `{"functionCall":{"name":...,"args":...}}` entries (the text part is
//!   dropped, matching the historical builder).
//! - Tools: `tools:[{"functionDeclarations":[{name, description,
//!   parameters}]}]`. Parameter schemas are sanitized down to Google's
//!   restricted JSON Schema subset before going on the wire: `$defs`/
//!   `definitions` maps are hoisted and every `$ref` inlined, then the tree
//!   is filtered to Gemini's supported keywords (mirroring langchain-google's
//!   `GEMINI_SUPPORTED_SCHEMA_KEYWORDS`) — expressible constructs are
//!   rewritten (`"type": ["string","null"]` → `nullable`, `const` → `enum`,
//!   `exclusive*` bounds → inclusive, `allOf` merged into the parent,
//!   `prefixItems` tuples → homogeneous arrays) and everything else is
//!   dropped by omission. Google rejects every unknown name with
//!   `INVALID_ARGUMENT`, so an allowlist — not a denylist — is the only
//!   future-proof shape here.
//! - Tool choice: when tools are present, a top-level `toolConfig` object
//!   carries `functionCallingConfig` mapping the canonical [`ToolChoice`] to
//!   Gemini `AUTO`/`NONE`/`ANY` modes. `generationConfig` only ever carries
//!   sampling knobs (`temperature`, `maxOutputTokens`) — Google rejects a
//!   `functionCallingConfig` placed there.
//! - There is no top-level `stream` field: the connector selects the streaming
//!   variant through the URL (`:streamGenerateContent?alt=sse`).
//!
//! This family never echoes `reasoning_content` back on assistant messages
//! (Gemini has no such field), so the `ReasoningEcho` argument is accepted for
//! the uniform [`Dialect`] signature but has no effect on the wire body.

use concerto_core::types::{CompletionRequest, Role, ToolChoice, ToolDefinition};

use super::{Dialect, ReasoningEcho};

/// The Google Gemini chat dialect (`generateContent`).
///
/// Stateless unit struct; construct one (or keep one per provider) and call
/// [`Dialect::render_chat_body`] for each completion request. See the module
/// docs for the wire contract it implements.
pub struct GeminiChatDialect;

impl Dialect for GeminiChatDialect {
    fn kind(&self) -> &'static str {
        "gemini"
    }

    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        _model: &str,
        _echo: ReasoningEcho,
    ) -> serde_json::Value {
        // Note: unlike the chat-completions families, Gemini does not send the
        // model name in the request body — it is part of the endpoint URL.
        let mut contents: Vec<serde_json::Value> = Vec::new();
        let mut system_instruction: Option<String> = None;

        for msg in &request.messages {
            match msg.role {
                Role::System => {
                    system_instruction = Some(msg.content.clone());
                }
                Role::User => {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{"text": msg.content}]
                    }));
                }
                Role::Assistant => {
                    let part = serde_json::json!({"text": msg.content});
                    let mut entry = serde_json::json!({
                        "role": "model",
                        "parts": [part]
                    });
                    if let Some(ref tcs) = msg.tool_calls {
                        let tool_calls: Vec<serde_json::Value> = tcs
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "functionCall": {
                                        "name": tc.name,
                                        "args": tc.arguments
                                    }
                                })
                            })
                            .collect();
                        entry["parts"] = serde_json::json!(tool_calls);
                    }
                    contents.push(entry);
                }
                Role::Tool => {
                    // Gemini uses functionResponse to relay tool results back.
                    // The `name` must be the function/tool name (from
                    // ToolResult.name), not the tool call ID — Gemini matches
                    // this against the function declaration sent in the
                    // `tools` array.
                    if let Some(results) = &msg.tool_results {
                        for result in results {
                            contents.push(serde_json::json!({
                                "role": "user",
                                "parts": [{
                                    "functionResponse": {
                                        "name": result.name,
                                        "response": result.content
                                    }
                                }]
                            }));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
        });

        if let Some(si) = system_instruction {
            body["system_instruction"] = serde_json::json!({
                "parts": [{"text": si}]
            });
        }

        let mut gen_config = serde_json::json!({});
        if let Some(temp) = request.temperature {
            gen_config["temperature"] = serde_json::json!(temp);
        }
        if let Some(max_t) = request.max_tokens {
            gen_config["maxOutputTokens"] = serde_json::json!(max_t);
        }
        if gen_config.as_object().is_some_and(|o| !o.is_empty()) {
            body["generationConfig"] = gen_config;
        }

        if let Some(tools) = &request.tools {
            // Send tool/function declarations so Gemini knows what functions
            // exist.
            let decls = build_google_tool_declarations(tools);
            if !decls.is_empty() {
                body["tools"] = serde_json::json!([{
                    "functionDeclarations": decls
                }]);
            }

            // Map generic ToolChoice to Gemini's functionCallingConfig. This
            // is a *tool-level* setting on the Gemini wire: it lives in the
            // top-level `toolConfig` object — Google rejects it inside
            // `generationConfig`.
            body["toolConfig"]["functionCallingConfig"] = build_google_function_calling_config(
                request.tool_choice.as_ref().unwrap_or(&ToolChoice::Auto),
            );
        }

        body
    }
}

// ---------------------------------------------------------------------------
// Family-specific renderers (one canonical value -> the Gemini wire shape).
// Field shape matters for golden tests: keep these builds byte-identical to the
// previous `google.rs` implementations.
// ---------------------------------------------------------------------------

/// Convert `ToolDefinition`s into Gemini's `functionDeclarations` entries.
///
/// Google's function-calling API accepts only a restricted JSON Schema subset
/// (the fields of its `Schema` proto); anything outside it — dialect keywords
/// (`$schema`, `$defs`, `$ref`), applicators (`allOf`, `oneOf`, `not`),
/// object-shape extras (`additionalProperties`, `patternProperties`), tuple
/// forms (`prefixItems`) — is rejected as an unknown name
/// (`INVALID_ARGUMENT`). `schemars` emits several of those whenever a tool
/// input contains named nested structs (e.g. the report submission inputs),
/// so each declaration's parameter schema is sanitized before going on the
/// wire — see [`sanitize_google_parameters`].
fn build_google_tool_declarations(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            let mut parameters = t.parameters.clone();
            sanitize_google_parameters(&mut parameters);
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": parameters,
            })
        })
        .collect()
}

/// Sanitize one tool parameter schema in place for Google's restricted JSON
/// Schema subset (see [`build_google_tool_declarations`] and
/// [`GEMINI_SUPPORTED_SCHEMA_KEYWORDS`]).
///
/// The pipeline is: hoist the root `$defs`/`definitions` maps (both `schemars`
/// spellings) and drop the root `$schema` keyword; [`sanitize_google_refs`]
/// then inlines every pointer against them; finally
/// [`sanitize_google_keywords`] rewrites expressible constructs onto Google
/// equivalents and filters the tree down to the allowlist.
fn sanitize_google_parameters(parameters: &mut serde_json::Value) {
    let mut defs = serde_json::Map::new();
    if let Some(root) = parameters.as_object_mut() {
        for keyword in ["$defs", "definitions"] {
            if let Some(map) = root.remove(keyword).and_then(|v| v.as_object().cloned()) {
                defs.extend(map);
            }
        }
        root.remove("$schema");
    }
    let mut resolving = Vec::new();
    sanitize_google_refs(parameters, &defs, &mut resolving);
    sanitize_google_keywords(parameters);
}

/// Recursively resolve `$ref` objects, replacing each with the referenced
/// definition.
///
/// Each `{"$ref": "#/$defs/Name"}` / `"#/definitions/Name"` object is replaced
/// wholesale by a clone of the referenced definition, which itself has its
/// references resolved so nested pointers resolve too. Only the pointer's last
/// segment is used as the definition name — exactly how `schemars` addresses
/// top-level definitions. A dangling pointer, or one already being expanded
/// (a reference cycle), degrades to `{"type": "object"}` rather than hanging
/// the resolver or leaking a `$ref` onto the wire.
fn sanitize_google_refs(
    value: &mut serde_json::Value,
    defs: &serde_json::Map<String, serde_json::Value>,
    resolving: &mut Vec<String>,
) {
    let target = value
        .as_object()
        .and_then(|obj| obj.get("$ref"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if let Some(pointer) = target {
        let name = pointer.rsplit('/').next().unwrap_or_default();
        *value = match defs.get(name) {
            Some(definition)
                if !resolving.iter().any(|in_progress| in_progress.as_str() == name) =>
            {
                let mut resolved = definition.clone();
                resolving.push(name.to_owned());
                sanitize_google_refs(&mut resolved, defs, resolving);
                resolving.pop();
                resolved
            }
            _ => {
                tracing::debug!(
                    %pointer,
                    "$ref target missing or cyclic; substituting {{\"type\":\"object\"}}"
                );
                serde_json::json!({ "type": "object" })
            }
        };
        return;
    }

    match value {
        serde_json::Value::Object(map) => {
            for child in map.values_mut() {
                sanitize_google_refs(child, defs, resolving);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_google_refs(item, defs, resolving);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Allowlist sanitization (Google `Schema` proto keyword subset).
// ---------------------------------------------------------------------------

/// JSON-Schema keywords accepted inside Gemini `functionDeclarations[]
/// .parameters` — the field set of Google Developer API's `Schema` proto,
/// mirroring langchain-google's `GEMINI_SUPPORTED_SCHEMA_KEYWORDS`. Every
/// keyword not listed here is rejected by the API as an unknown name
/// (`INVALID_ARGUMENT`), which is why the sanitizer filters by *allowlist*
/// instead of stripping known offenders: tomorrow's schema dialect additions
/// (`$id`, new applicators, …) are dropped automatically rather than 400-ing
/// the next request.
const GEMINI_SUPPORTED_SCHEMA_KEYWORDS: [&str; 22] = [
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "default",
    "items",
    "minItems",
    "maxItems",
    "enum",
    "properties",
    "propertyOrdering",
    "required",
    "minProperties",
    "maxProperties",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "example",
    "anyOf",
];

/// Rewrite and filter `value` down to Google's restricted JSON Schema subset
/// (see [`GEMINI_SUPPORTED_SCHEMA_KEYWORDS`]).
///
/// Unsupported-but-expressible keywords are rewritten first:
/// - `"type": ["string", "null"]` unions collapse to their first non-null
///   type plus `nullable: true` (Google spells optionality with `nullable`);
/// - null-only entries are pruned from `anyOf` lists for the same reason;
/// - `exclusiveMinimum`/`exclusiveMaximum` bounds collapse onto their
///   inclusive `minimum`/`maximum` counterparts (numeric form carries the
///   bound; the legacy boolean form is dropped as unrepresentable);
/// - `const` becomes a single-entry `enum`;
/// - `allOf` entries merge into the parent schema (schemars' flattened-struct
///   pattern), parent keys winning conflicts except that `properties` maps
///   union per-field and `required` lists concatenate;
/// - draft-2020-12 tuple schemas (`prefixItems`) degrade to a homogeneous
///   array with an integer `items` fallback.
///
/// Everything else — `$id`, `additionalProperties`, `oneOf`, `not`,
/// `patternProperties`, … — is then dropped by omission and primitives pass
/// through untouched. Only the three retained keywords that carry
/// subschemas (`properties` values, `items`, `anyOf` entries) are descended
/// into: every other retained value is a primitive or opaque data (e.g.
/// `enum` members, `example` payloads), and sibling-keyword filtering must
/// never be applied to property *names*.
fn sanitize_google_keywords(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            rewrite_for_google(map);
            let original = std::mem::take(map);
            for (keyword, child) in original {
                if GEMINI_SUPPORTED_SCHEMA_KEYWORDS.contains(&keyword.as_str()) {
                    map.insert(keyword, child);
                } else {
                    tracing::debug!(%keyword, "dropping Google-unsupported schema keyword");
                }
            }
            if let Some(properties) =
                map.get_mut("properties").and_then(serde_json::Value::as_object_mut)
            {
                properties.values_mut().for_each(sanitize_google_keywords);
            }
            if let Some(items) = map.get_mut("items") {
                sanitize_google_keywords(items);
            }
            if let Some(entries) = map.get_mut("anyOf").and_then(serde_json::Value::as_array_mut) {
                entries.iter_mut().for_each(sanitize_google_keywords);
            }
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(sanitize_google_keywords);
        }
        _ => {}
    }
}

/// Apply all pre-filter rewrites to one schema object (see
/// [`sanitize_google_keywords`]).
fn rewrite_for_google(map: &mut serde_json::Map<String, serde_json::Value>) {
    fold_all_of_into_parent(map);
    collapse_type_union(map);
    prune_null_any_of_entries(map);
    rewrite_exclusive_bounds(map);
    rewrite_const_to_enum(map);
    degrade_prefix_items(map);
}

/// Merge every entry of an `allOf` array into the parent schema and drop the
/// key. Parent keys win conflicts, except `properties` maps union per-field
/// and `required` lists concatenate — the two shapes schemars' flatten emits.
fn fold_all_of_into_parent(map: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(entries) = map.remove("allOf").and_then(|value| value.as_array().cloned()) else {
        return;
    };
    for entry in entries.iter() {
        let Some(fields) = entry.as_object() else { continue };
        for (keyword, merged) in fields {
            match keyword.as_str() {
                "properties" => {
                    match map.get_mut("properties").and_then(serde_json::Value::as_object_mut) {
                        Some(existing) => {
                            for (property, schema) in merged.as_object().into_iter().flatten() {
                                existing.entry(property.clone()).or_insert_with(|| schema.clone());
                            }
                            continue;
                        }
                        None => {
                            map.insert(keyword.clone(), merged.clone());
                            continue;
                        }
                    }
                }
                "required" => {
                    match map.get_mut("required").and_then(serde_json::Value::as_array_mut) {
                        Some(existing) => {
                            for name in merged.as_array().into_iter().flatten() {
                                if !existing.contains(name) {
                                    existing.push(name.clone());
                                }
                            }
                            continue;
                        }
                        None => {
                            map.insert(keyword.clone(), merged.clone());
                            continue;
                        }
                    }
                }
                _ => {}
            }
            map.entry(keyword.clone()).or_insert_with(|| merged.clone());
        }
    }
}

/// Collapse a union-typed `"type"` array (e.g. `["string","null"]`) to its
/// first non-null type plus `nullable: true`.
fn collapse_type_union(map: &mut serde_json::Map<String, serde_json::Value>) {
    let types = map.get("type").and_then(serde_json::Value::as_array).map(|entries| {
        entries.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect::<Vec<_>>()
    });
    let Some(types) = types else { return };
    if types.is_empty() {
        return;
    }
    let nullable = types.iter().any(|name| name == "null");
    let primary = types.into_iter().find(|name| name != "null");
    map.remove("type");
    if let Some(primary) = primary {
        map.insert("type".to_owned(), serde_json::json!(primary));
    }
    if nullable && !map.contains_key("nullable") {
        map.insert("nullable".to_owned(), serde_json::Value::Bool(true));
    }
}

/// Prune `{"type":"null"}` entries from `anyOf` lists (Google has no null
/// type; optionality is spelled `nullable`). If pruning empties the list the
/// `anyOf` key is removed entirely.
fn prune_null_any_of_entries(map: &mut serde_json::Map<String, serde_json::Value>) {
    let pruned_null = match map.get_mut("anyOf").and_then(serde_json::Value::as_array_mut) {
        Some(entries) => {
            let before = entries.len();
            entries.retain(|entry| {
                entry.get("type").and_then(serde_json::Value::as_str) != Some("null")
            });
            before != entries.len()
        }
        None => false,
    };
    if pruned_null {
        if map.get("anyOf").and_then(serde_json::Value::as_array).is_some_and(Vec::is_empty) {
            map.remove("anyOf");
        }
        if !map.contains_key("nullable") {
            map.insert("nullable".to_owned(), serde_json::Value::Bool(true));
        }
    }
}

/// Fold numeric `exclusiveMinimum`/`exclusiveMaximum` bounds onto their
/// inclusive `minimum`/`maximum` counterparts when absent. The legacy boolean
/// form asserts exclusivity of an existing bound and cannot be expressed, so
/// it is simply dropped.
fn rewrite_exclusive_bounds(map: &mut serde_json::Map<String, serde_json::Value>) {
    for (exclusive, inclusive) in [("exclusiveMinimum", "minimum"), ("exclusiveMaximum", "maximum")]
    {
        let Some(bound) = map.remove(exclusive) else { continue };
        if bound.is_number() && !map.contains_key(inclusive) {
            map.insert(inclusive.to_owned(), bound);
        }
    }
}

/// Rewrite `const` into a single-entry `enum` (Google supports `enum`, not
/// `const`).
fn rewrite_const_to_enum(map: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(constant) = map.remove("const") else { return };
    if !map.contains_key("enum") {
        map.insert("enum".to_owned(), serde_json::Value::Array(vec![constant]));
    }
}

/// Degrade a draft-2020-12 tuple schema (`prefixItems`) to a homogeneous
/// array: the positional item schemas are dropped and an integer `items`
/// fallback is installed unless one already exists. The only tuple in the
/// tool-input surface is `CodeSnippet.lines: (u32, u32)`, which now declares
/// `[u32; 2]` for schemars anyway — this guards hand-written or future tuple
/// schemas.
fn degrade_prefix_items(map: &mut serde_json::Map<String, serde_json::Value>) {
    if map.remove("prefixItems").is_some() {
        tracing::debug!("tuple schema (prefixItems) degraded to homogeneous array for Gemini");
        map.entry("items".to_owned()).or_insert_with(|| serde_json::json!({ "type": "integer" }));
    }
}

/// Map the generic `ToolChoice` to Gemini's `functionCallingConfig`.
fn build_google_function_calling_config(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({"mode": "AUTO"}),
        ToolChoice::None => serde_json::json!({"mode": "NONE"}),
        ToolChoice::Required => serde_json::json!({"mode": "ANY"}),
        ToolChoice::Forced(name) => serde_json::json!({
            "mode": "ANY",
            "allowedFunctionNames": [name]
        }),
        _ => serde_json::json!({"mode": "AUTO"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{Message, SubmitDesignDocInput, ToolCall, ToolResult};

    fn render(request: &CompletionRequest) -> serde_json::Value {
        GeminiChatDialect.render_chat_body(request, "gemini-2.0-flash", ReasoningEcho::IfPresent)
    }

    /// The same canonical multi-turn fixture used across the family adapters:
    /// system, user, assistant-with-tool-calls (with captured reasoning),
    /// assistant-with-reasoning, a tool result, and a follow-up user turn,
    /// plus a tool list and a forced tool choice.
    fn fixture_request() -> CompletionRequest {
        CompletionRequest {
            model: String::new(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: "You are a helpful assistant.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Hello!".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Assistant,
                    content: "Here is the file list.".into(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                    }]),
                    tool_results: None,
                    reasoning_content: Some("I'll list the files.".into()),
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Assistant,
                    content: "Here".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: Some("compact reasoning".into()),
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Tool,
                    content: String::new(),
                    tool_calls: None,
                    tool_results: Some(vec![ToolResult {
                        id: "call_1".into(),
                        name: "shell".into(),
                        content: serde_json::json!({"output": "file list"}),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Run that again.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            tools: Some(vec![ToolDefinition {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input": {"type": "string"}
                    },
                    "required": ["input"]
                }),
            }]),
            tool_choice: Some(ToolChoice::Forced("test_tool".into())),
            temperature: Some(0.5),
            max_tokens: Some(4096),
            stream: true,
        }
    }

    #[test]
    fn dialect_kind_is_gemini() {
        assert_eq!(GeminiChatDialect.kind(), "gemini");
    }

    /// Gemini encodes streaming through the URL (`:streamGenerateContent?alt=sse`),
    /// so the body never carries a `stream` field.
    #[test]
    fn body_has_no_stream_field() {
        let request = CompletionRequest { stream: true, ..Default::default() };
        let body = render(&request);
        assert!(body.get("stream").is_none(), "gemini body must not carry stream");
    }

    #[test]
    fn no_model_field_in_body() {
        let request = CompletionRequest::default();
        let body = render(&request);
        assert!(body.get("model").is_none(), "gemini body must not carry model");
    }

    #[test]
    fn system_renders_as_system_instruction() {
        let request = fixture_request();
        let body = render(&request);
        assert_eq!(
            body["system_instruction"],
            serde_json::json!({"parts": [{"text": "You are a helpful assistant."}]})
        );
        let contents = body["contents"].as_array().unwrap();
        assert!(
            contents.iter().all(|c| c["role"] != "system"),
            "system messages must not appear in contents"
        );
    }

    #[test]
    fn user_messages_render_user_parts() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"], serde_json::json!([{"text": "Hello!"}]));
    }

    /// When an assistant message has tool calls, Gemini replaces the parts
    /// array with `functionCall` entries — the text flash is dropped, matching
    /// the previous `google.rs` builder.
    #[test]
    fn assistant_tool_calls_replace_parts_with_function_call() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"],
            serde_json::json!([{
                "functionCall": {
                    "name": "shell",
                    "args": {"command": "ls"}
                }
            }])
        );
    }

    /// ADR-46 parity: Gemini never echoes `reasoning_content` onto assistant
    /// parts; a reasoning-only assistant renders as a bare text part.
    #[test]
    fn assistant_reasoning_is_not_echoed() {
        let request = fixture_request();
        let body = render(&request);

        let serialized = serde_json::to_string(&body).expect("body serializes");
        assert!(
            !serialized.contains("reasoning"),
            "gemini body must not carry reasoning_content: {serialized}"
        );

        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents[2],
            serde_json::json!({
                "role": "model",
                "parts": [{"text": "Here"}]
            })
        );
    }

    #[test]
    fn tool_results_render_function_response_parts() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents[3],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "shell",
                        "response": {"output": "file list"}
                    }
                }]
            })
        );
    }

    #[test]
    fn tools_serialize_as_function_declarations() {
        let request = fixture_request();
        let body = render(&request);

        let tools = body["tools"].as_array().expect("should have tools array");
        let declarations =
            tools[0]["functionDeclarations"].as_array().expect("should have functionDeclarations");
        let decl = &declarations[0];
        assert_eq!(decl["name"], "test_tool");
        assert_eq!(decl["description"], "A test tool");
        assert_eq!(decl["parameters"]["properties"]["input"]["type"], "string");
        // Unlike OpenAI, Gemini declarations carry no `type: "function"` wrapper.
        assert!(tools[0].get("functionDeclarations").is_some());
        assert!(decl.get("input_schema").is_none(), "gemini uses parameters, not input_schema");
    }

    #[test]
    fn function_calling_config_maps_all_modes() {
        for (choice, expected_mode) in
            [(ToolChoice::Auto, "AUTO"), (ToolChoice::None, "NONE"), (ToolChoice::Required, "ANY")]
        {
            let request = CompletionRequest {
                tools: Some(vec![ToolDefinition {
                    name: "test_tool".into(),
                    description: "A test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]),
                tool_choice: Some(choice),
                ..Default::default()
            };
            let body = render(&request);
            assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], expected_mode);
            // The config lives in the top-level `toolConfig`; it must never
            // leak into `generationConfig` (Google rejects that placement).
            assert!(
                body["generationConfig"].get("functionCallingConfig").is_none(),
                "functionCallingConfig must not sit under generationConfig"
            );
        }

        // Strictly no other keys for non-forced modes: no allowedFunctionNames,
        // and without temperature/max tokens there is no generationConfig at all.
        let request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "NONE"})
        );
        assert!(
            body.get("generationConfig").is_none(),
            "sampling knobs only: no generationConfig without temperature/max tokens"
        );

        // Forced tool pins the allowed function name under ANY.
        let request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "submit_design_doc".into(),
                description: "Submit a structured design document.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            tool_choice: Some(ToolChoice::Forced("submit_design_doc".into())),
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["submit_design_doc"]})
        );
    }

    /// Without tools there is no `functionCallingConfig` and no `tools` field;
    /// temperature/max tokens alone keep `generationConfig` on the body.
    #[test]
    fn generation_config_is_absent_without_options_or_tools() {
        let request = CompletionRequest::default();
        let body = render(&request);
        assert!(body.get("generationConfig").is_none());
        assert!(body.get("tools").is_none());
    }

    /// With tools present but zero declarations, Gemini still attaches the
    /// top-level `toolConfig.functionCallingConfig` but omits the empty
    /// `tools` field (parity with the previous builder).
    #[test]
    fn empty_tools_still_sets_function_calling_config() {
        let request = CompletionRequest { tools: Some(vec![]), ..Default::default() };
        let body = render(&request);
        assert!(body.get("tools").is_none(), "empty declarations => no tools field");
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "AUTO"})
        );
        assert!(
            body.get("generationConfig").is_none(),
            "no temperature/max tokens => no generationConfig"
        );
    }

    /// Exact serialized body is part of the Gemini wire contract: golden-test
    /// it so a refactor that reorders or renames fields fails loudly.
    #[test]
    fn golden_serialized_body_system_and_user() {
        let request = CompletionRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: "You are a test assistant.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Hello".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            r#"{"contents":[{"parts":[{"text":"Hello"}],"role":"user"}],"system_instruction":{"parts":[{"text":"You are a test assistant."}]}}"#
        );
    }

    /// VALUE-level parity: the full fixture renders to exactly the Gemini shape
    /// the previous `google.rs` builder produced.
    #[test]
    fn full_fixture_value_snapshot() {
        let request = fixture_request();
        let body = render(&request);

        let expected = serde_json::json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hello!"}]},
                {
                    "role": "model",
                    "parts": [{
                        "functionCall": {"name": "shell", "args": {"command": "ls"}}
                    }]
                },
                {"role": "model", "parts": [{"text": "Here"}]},
                {
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": "shell",
                            "response": {"output": "file list"}
                        }
                    }]
                },
                {"role": "user", "parts": [{"text": "Run that again."}]}
            ],
            "system_instruction": {
                "parts": [{"text": "You are a helpful assistant."}]
            },
            "tools": [
                {
                    "functionDeclarations": [
                        {
                            "name": "test_tool",
                            "description": "A test tool",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "input": {"type": "string"}
                                },
                                "required": ["input"]
                            }
                        }
                    ]
                }
            ],
            "generationConfig": {
                "temperature": 0.5,
                "maxOutputTokens": 4096
            },
            "toolConfig": {
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": ["test_tool"]
                }
            }
        });

        assert_eq!(body, expected);
    }

    /// Recursively assert that none of `keys` appears anywhere in a rendered
    /// value (objects and arrays are both walked).
    fn assert_no_keys(value: &serde_json::Value, keys: &[&str]) {
        match value {
            serde_json::Value::Object(map) => {
                for key in map.keys() {
                    assert!(
                        !keys.contains(&key.as_str()),
                        "unexpected Google-unsupported key `{key}` in {value}"
                    );
                }
                map.values().for_each(|child| assert_no_keys(child, keys));
            }
            serde_json::Value::Array(items) => {
                items.iter().for_each(|item| assert_no_keys(item, keys));
            }
            _ => {}
        }
    }

    /// JSON-Schema keywords that must never reach the Gemini wire: dialect
    /// keywords, pointers, applicators, object-shape extras, tuple forms,
    /// exclusive bounds, and other non-allowlisted names.
    const GOOGLE_UNSUPPORTED_KEYS: [&str; 21] = [
        "$schema",
        "$defs",
        "definitions",
        "$ref",
        "$id",
        "$comment",
        "$anchor",
        "prefixItems",
        "additionalProperties",
        "patternProperties",
        "propertyNames",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "const",
        "allOf",
        "oneOf",
        "not",
        "uniqueItems",
        "contains",
        "if",
        "dependencies",
    ];

    /// Contract 9 (Google Gemini): the real schemars-generated
    /// `submit_design_doc` schema survives the function-declaration builder
    /// with its root `$schema` keyword stripped and *no* Google-unsupported
    /// JSON-Schema dialect keywords anywhere in the declaration (`$defs`,
    /// `definitions`, `$ref`), and the forced tool choice maps to Gemini's
    /// top-level `toolConfig.functionCallingConfig` with `mode: ANY` + the
    /// allowed function name.
    #[test]
    fn submit_design_doc_schema_flows_through_tools_builder() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let raw = serde_json::to_value(&schema).unwrap();
        assert!(
            raw.get("$schema").is_some(),
            "precondition: schemars emits a root $schema keyword"
        );
        let tools = vec![ToolDefinition {
            name: "submit_design_doc".into(),
            description: "Submit a structured design document.".into(),
            parameters: raw,
        }];
        let decls = build_google_tool_declarations(&tools);
        assert_eq!(decls[0]["name"], "submit_design_doc");
        assert!(
            decls[0]["parameters"].get("$schema").is_none(),
            "root $schema must be stripped from declaration parameters"
        );
        assert!(
            decls[0]["parameters"].get("$defs").is_none(),
            "$defs must be removed from declaration parameters"
        );
        assert_no_keys(&decls[0]["parameters"], &GOOGLE_UNSUPPORTED_KEYS);
        let params: serde_json::Value =
            serde_json::from_value(decls[0]["parameters"].clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));

        let config =
            build_google_function_calling_config(&ToolChoice::Forced("submit_design_doc".into()));
        assert_eq!(config["mode"], "ANY");
        assert_eq!(config["allowedFunctionNames"], serde_json::json!(["submit_design_doc"]));
    }

    /// Regression guard for the Gemini `INVALID_ARGUMENT` failure on researcher
    /// subtasks ("Unknown name \"$defs\" ... Unknown name \"$ref\""): a schema
    /// with named nested structs carries a root `$defs` map plus `$ref`
    /// pointers into it. The sanitizer must inline every referenced definition
    /// — including references nested inside other definitions and the legacy
    /// `definitions` spelling — so no dialect keyword remains.
    #[test]
    fn dollar_defs_and_refs_are_inlined_for_gemini() {
        let tools = vec![ToolDefinition {
            name: "submit_research_report".into(),
            description: "Submit research findings.".into(),
            parameters: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "findings": {
                        "type": "array",
                        "items": {"$ref": "#/$defs/ResearchFinding"}
                    },
                    "legacy": {"$ref": "#/definitions/LegacyDef"}
                },
                "required": ["findings"],
                "$defs": {
                    "ResearchFinding": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "detail": {"$ref": "#/$defs/Detail"}
                        },
                        "required": ["title"]
                    },
                    "Detail": {"type": ["string", "null"]}
                },
                "definitions": {
                    "LegacyDef": {
                        "type": "object",
                        "properties": {"ok": {"type": "boolean"}}
                    }
                }
            }),
        }];

        let decls = build_google_tool_declarations(&tools);
        let params = &decls[0]["parameters"];

        // No dialect keyword survives anywhere in the declaration.
        assert_no_keys(params, &GOOGLE_UNSUPPORTED_KEYS);

        // Each `$ref` was replaced by the referenced definition itself,
        // with refs *inside* definitions resolved too. The union type in the
        // `Detail` definition collapsed to Gemini's `nullable` spelling.
        assert_eq!(
            params["properties"]["findings"]["items"],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "detail": {"type": "string", "nullable": true}
                },
                "required": ["title"]
            })
        );
        assert_eq!(
            params["properties"]["legacy"],
            serde_json::json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}}
            })
        );
    }

    /// Dangling `$ref`s (target missing from every definitions map) and
    /// reference cycles (self-referential types) degrade to
    /// `{"type":"object"}` instead of hanging the resolver or leaking a `$ref`
    /// onto the wire.
    #[test]
    fn dangling_and_cyclic_refs_fall_back_to_object() {
        let tools = vec![ToolDefinition {
            name: "recursive_tool".into(),
            description: "Has self-referential input.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "dangling": {"$ref": "#/$defs/Missing"},
                    "tree": {"$ref": "#/$defs/TreeNode"}
                },
                "$defs": {
                    "TreeNode": {
                        "type": "object",
                        "properties": {
                            "children": {
                                "type": "array",
                                "items": {"$ref": "#/$defs/TreeNode"}
                            }
                        }
                    }
                }
            }),
        }];

        let decls = build_google_tool_declarations(&tools);
        let params = &decls[0]["parameters"];

        assert_no_keys(params, &GOOGLE_UNSUPPORTED_KEYS);
        assert_eq!(params["properties"]["dangling"], serde_json::json!({"type": "object"}));
        // The cycle terminates: TreeNode expands once, then its children
        // array's self-reference collapses to the plain-object fallback.
        assert_eq!(
            params["properties"]["tree"]["properties"]["children"]["items"],
            serde_json::json!({"type": "object"})
        );
    }

    /// Allowlist contract: every Google-unsupported keyword is stripped or
    /// rewritten onto a supported equivalent, so a hand-written schema full
    /// of draft-2020-12 constructs still lands within Gemini's `Schema`
    /// subset instead of failing with `INVALID_ARGUMENT`.
    #[test]
    fn allowlist_strips_unsupported_and_rewrites_nullable_and_tuple() {
        let tools = vec![ToolDefinition {
            name: "synthetic_tool".into(),
            description: "Exercises every sanitizer rewrite path.".into(),
            parameters: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$id": "https://example.com/synthetic.json",
                "title": "Synthetic",
                "type": ["object", "null"],
                "properties": {
                    "tuple_field": {
                        "type": "array",
                        "prefixItems": [{"type": "integer"}, {"type": "integer"}],
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "bounded": {
                        "type": "number",
                        "exclusiveMinimum": 0,
                        "exclusiveMaximum": 100
                    },
                    "fixed": {"const": "yes"},
                    "closed": {"type": "string", "additionalProperties": false},
                    "variant": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                    "flattened": {
                        "allOf": [{"type": "string", "minLength": 2}, {"maxLength": 8}]
                    }
                },
                "required": ["tuple_field"],
                "not": {"type": "string"},
                "patternProperties": {"^x-": {"type": "string"}},
                "uniqueItems": true
            }),
        }];

        let decls = build_google_tool_declarations(&tools);
        let params = &decls[0]["parameters"];

        // No denylisted key survives anywhere in the declaration.
        assert_no_keys(params, &GOOGLE_UNSUPPORTED_KEYS);

        // Root union type collapsed to the single non-null type + nullable.
        assert_eq!(params["type"], "object");
        assert_eq!(params["nullable"], serde_json::Value::Bool(true));

        // Tuple degraded to a homogeneous array, bounds preserved.
        let tuple = &params["properties"]["tuple_field"];
        assert!(tuple.get("prefixItems").is_none());
        assert_eq!(tuple["items"], serde_json::json!({"type": "integer"}));
        assert_eq!(tuple["minItems"], 2);
        assert_eq!(tuple["maxItems"], 2);

        // Exclusive bounds folded onto their inclusive counterparts.
        let bounded = &params["properties"]["bounded"];
        assert_eq!(bounded["minimum"], 0);
        assert_eq!(bounded["maximum"], 100);
        assert_eq!(*bounded, serde_json::json!({"type": "number", "minimum": 0, "maximum": 100}));

        // `const` became a single-entry `enum`.
        assert_eq!(params["properties"]["fixed"], serde_json::json!({"enum": ["yes"]}));

        // `oneOf` is not representable: the variant collapses to an empty
        // schema rather than leaking the keyword.
        assert_eq!(params["properties"]["variant"], serde_json::json!({}));

        // `allOf` entries merged into the parent (parent wins conflicts).
        assert_eq!(
            params["properties"]["flattened"],
            serde_json::json!({"type": "string", "minLength": 2, "maxLength": 8})
        );
    }

    /// Regression guard for the next-payload failure class: schemars renders
    /// `CodeSnippet.lines: (u32, u32)` as a draft-2020-12 `prefixItems`
    /// tuple. The field now declares `[u32; 2]` for schemars (serde shape
    /// unchanged), so the *real* `submit_research_report` schema — after `$ref`
    /// inlining plus sanitization — must expose a homogeneous integer array
    /// and never the tuple form.
    #[test]
    fn code_snippet_lines_schema_is_gemini_safe() {
        let schema = schemars::schema_for!(concerto_core::types::ResearchReport);
        let raw = serde_json::to_value(&schema).unwrap();
        let tools = vec![ToolDefinition {
            name: "submit_research_report".into(),
            description: "Submit research findings.".into(),
            parameters: raw,
        }];

        let decls = build_google_tool_declarations(&tools);
        let params = &decls[0]["parameters"];
        let lines = &params["properties"]["code_snippets"]["items"]["properties"]["lines"];

        assert_eq!(lines["type"], "array");
        assert_eq!(lines["items"]["type"], "integer");
        assert_eq!(lines["minItems"], 2);
        assert_eq!(lines["maxItems"], 2);
        assert_no_keys(params, &GOOGLE_UNSUPPORTED_KEYS);
    }

    /// Ground-truth guard: every structured submission input's real
    /// schemars-generated schema survives the declaration builder with zero
    /// unsupported keywords, and optional fields surface Gemini's `nullable`
    /// spelling (`Option<u32>` arrives as `"type": ["integer","null"]`).
    #[test]
    fn submission_schemas_are_gemini_clean() {
        for (name, schema) in [
            ("submit_design_doc", schemars::schema_for!(SubmitDesignDocInput)),
            ("submit_research_report", schemars::schema_for!(concerto_core::types::ResearchReport)),
            ("submit_review_report", schemars::schema_for!(concerto_core::types::ReviewReport)),
        ] {
            let raw = serde_json::to_value(&schema).unwrap();
            let tools = vec![ToolDefinition {
                name: name.into(),
                description: "Structured submission.".into(),
                parameters: raw,
            }];
            let decls = build_google_tool_declarations(&tools);
            assert_no_keys(&decls[0]["parameters"], &GOOGLE_UNSUPPORTED_KEYS);
        }

        // Spot-check the Option<u32> path inside ReviewReport.issues[].line.
        let schema = schemars::schema_for!(concerto_core::types::ReviewReport);
        let tools = vec![ToolDefinition {
            name: "submit_review_report".into(),
            description: "Structured submission.".into(),
            parameters: serde_json::to_value(&schema).unwrap(),
        }];
        let decls = build_google_tool_declarations(&tools);
        let line = &decls[0]["parameters"]["properties"]["issues"]["items"]["properties"]["line"];
        assert_eq!(line["type"], "integer");
        assert_eq!(line["nullable"], serde_json::Value::Bool(true));
    }

    /// Oracle guard F-1: multiple tool results flatten into one `contents`
    /// entry per result, each keyed by the result's *function name* (Gemini
    /// matches `functionResponse.name` against the declared functions, not the
    /// call id).
    #[test]
    fn multiple_tool_results_flatten_into_entries() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![
                    ToolResult {
                        id: "call_a".into(),
                        name: "tool_a".into(),
                        content: serde_json::json!("first result"),
                    },
                    ToolResult {
                        id: "call_b".into(),
                        name: "tool_b".into(),
                        content: serde_json::json!("second result"),
                    },
                ]),
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request);
        let contents = body["contents"].as_array().expect("should have contents");

        assert_eq!(contents.len(), 2, "one entry per tool result");
        assert_eq!(
            contents[0],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "tool_a",
                        "response": "first result"
                    }
                }]
            })
        );
        assert_eq!(
            contents[1],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "tool_b",
                        "response": "second result"
                    }
                }]
            })
        );
    }

    /// Oracle guard F-3: a tool-role message with no `tool_results` emits
    /// *no* contents entry — Gemini has no "bare tool message" shape.
    #[test]
    fn tool_message_without_results_emits_no_entry() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: "discarded".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request);
        let contents = body["contents"].as_array().expect("should have contents");

        assert_eq!(contents.len(), 0, "no tool_results => no contents entry");
    }

    /// Oracle guard F-4: a numeric zero is a set value — `temperature` and
    /// `max_tokens` explicitly set to `0` still render `generationConfig`
    /// fields.
    #[test]
    fn zero_temperature_and_max_tokens_render_in_generation_config() {
        let request =
            CompletionRequest { temperature: Some(0.0), max_tokens: Some(0), ..Default::default() };
        let body = render(&request);

        assert_eq!(body["generationConfig"]["temperature"], 0.0);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 0);
    }

    /// Oracle guard F-6: when several `Role::System` messages are present the
    /// *last* one wins `system_instruction`, matching the previous builder's
    /// overwrite loop.
    #[test]
    fn last_system_message_wins() {
        let request = CompletionRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: "first system".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::System,
                    content: "second system".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };
        let body = render(&request);

        assert_eq!(
            body["system_instruction"],
            serde_json::json!({"parts": [{"text": "second system"}]}),
            "last system message wins"
        );
    }
}
