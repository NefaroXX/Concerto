//! Shared JSON-Schema sanitizer for tool parameter schemas.
//!
//! Every LLM provider accepts a slightly different JSON Schema subset, but all
//! reject the same core set of draft-2020-12 constructs that `schemars` emits:
//!
//! - `$defs` / `definitions` — named type maps that only make sense for
//!   `$ref` resolution and must be hoisted and inlined before going on the
//!   wire.
//! - `$ref` — pointer into a hoisted definitions map; must be resolved
//!   inline.
//! - `$schema` — metadata keyword; harmless to strip.
//! - `prefixItems` — draft-2020-12 tuple form; must be degraded to a
//!   homogeneous `items` schema.
//!
//! This module provides [`sanitize_tool_schema`], the single entry point that
//! all adapters call before putting a `ToolDefinition`'s `parameters` on the
//! wire. The function is a pure, stateless transform — it mutates the value
//! in place and returns nothing.
//!
//! Google's Gemini adapter applies additional keyword allowlist filtering
//! *after* this common normalization. The shared sanitizer handles the
//! dangerous constructs; provider-specific keyword filtering is the adapter's
//! responsibility.
//!
//! # Design
//!
//! The sanitizer is intentionally *permissive* for the OpenAI-compat family:
//! it strips only the keywords that cause real 4xx/5xx failures on
//! forwarder-gateways and free-tier pilots. Standard JSON Schema keywords
//! (`additionalProperties`, `oneOf`, `allOf`, `const`, `exclusiveMinimum`,
//! `exclusiveMaximum`, `patternProperties`, `propertyNames`, `uniqueItems`)
//! are left intact — OpenAI, Anthropic, Ollama and most gateways accept them.
//! If a future model rejects a new keyword, the fix belongs in the
//! adapter-level allowlist, not here.

use concerto_core::types::ToolDefinition;

/// Normalize a tool parameter schema in place for safe wire transmission.
///
/// See the module docs for which constructs are handled. The function is
/// idempotent: applying it twice to the same value yields the same result.
pub fn sanitize_tool_schema(parameters: &mut serde_json::Value) {
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
    resolve_refs(parameters, &defs, &mut resolving);
    degrade_prefix_items(parameters);
}

/// Recursively resolve `$ref` objects, replacing each with the referenced
/// definition from the hoisted `defs` map.
///
/// A dangling pointer or one already being expanded (a reference cycle)
/// degrades to `{"type": "object"}` rather than leaking `$ref` onto the wire.
fn resolve_refs(
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
                resolve_refs(&mut resolved, defs, resolving);
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
                resolve_refs(child, defs, resolving);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_refs(item, defs, resolving);
            }
        }
        _ => {}
    }
}

/// Degrade draft-2020-12 tuple schemas (`prefixItems`) to a homogeneous
/// array: positional item schemas are dropped and an integer `items` fallback
/// is installed unless one already exists.
fn degrade_prefix_items(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if map.remove("prefixItems").is_some() {
                tracing::debug!(
                    "tuple schema (prefixItems) degraded to homogeneous array for tool schema"
                );
                map.entry("items".to_owned())
                    .or_insert_with(|| serde_json::json!({ "type": "integer" }));
            }
            for child in map.values_mut() {
                degrade_prefix_items(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                degrade_prefix_items(item);
            }
        }
        _ => {}
    }
}

/// Sanitize a slice of tool definitions in place, normalizing each tool's
/// parameter schema for safe wire transmission.
pub fn sanitize_tool_definitions(tools: &mut [ToolDefinition]) {
    for tool in tools {
        sanitize_tool_schema(&mut tool.parameters);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::SubmitDesignDocInput;

    /// The real schemars-generated `submit_design_doc` schema survives
    /// sanitization: `$defs` are inlined, `$ref` pointers resolved, and
    /// `$schema` stripped.
    #[test]
    fn submit_design_doc_schema_sanitized() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let raw = serde_json::to_value(&schema).unwrap();
        assert!(raw.get("$schema").is_some(), "precondition: schemars emits $schema");

        let mut tool = ToolDefinition {
            name: "submit_design_doc".into(),
            description: "Submit a structured design document.".into(),
            parameters: raw,
        };
        sanitize_tool_schema(&mut tool.parameters);

        assert!(tool.parameters.get("$schema").is_none(), "$schema must be stripped");
        assert!(
            tool.parameters.get("$defs").is_none(),
            "$defs must be removed (hoisted and inlined)"
        );
        assert!(tool.parameters.get("$ref").is_none(), "root $ref must be inlined");
        // The schema should still be valid JSON.
        let params: serde_json::Value = serde_json::from_value(tool.parameters.clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));
    }

    /// `$defs`/`definitions` maps are hoisted and their referenced types
    /// inlined via `$ref` resolution.
    #[test]
    fn dollar_defs_and_refs_are_inlined() {
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
                            "title": {"type": "string"}
                        },
                        "required": ["title"]
                    }
                },
                "definitions": {
                    "LegacyDef": {
                        "type": "object",
                        "properties": {"ok": {"type": "boolean"}}
                    }
                }
            }),
        }];

        let mut tools = tools;
        sanitize_tool_definitions(&mut tools);
        let params = &tools[0].parameters;

        assert!(params.get("$schema").is_none());
        assert!(params.get("$defs").is_none());
        assert!(params.get("definitions").is_none());

        // $ref resolved to the actual definition.
        assert_eq!(
            params["properties"]["findings"]["items"],
            serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"}
                },
                "required": ["title"]
            })
        );
        // Legacy $ref also resolved.
        assert_eq!(
            params["properties"]["legacy"],
            serde_json::json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}}
            })
        );
    }

    /// Dangling and cyclic `$ref`s degrade to `{"type":"object"}`.
    #[test]
    fn dangling_and_cyclic_refs_fall_back_to_object() {
        let mut tool = ToolDefinition {
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
        };
        sanitize_tool_schema(&mut tool.parameters);

        assert_eq!(
            tool.parameters["properties"]["dangling"],
            serde_json::json!({"type": "object"})
        );
        // The cycle terminates: TreeNode expands once, then self-reference
        // collapses to the plain-object fallback.
        assert_eq!(
            tool.parameters["properties"]["tree"]["properties"]["children"]["items"],
            serde_json::json!({"type": "object"})
        );
    }

    /// `prefixItems` tuples degrade to a homogeneous array with integer items.
    #[test]
    fn prefix_items_degraded_to_homogeneous_array() {
        let mut tool = ToolDefinition {
            name: "tuple_tool".into(),
            description: "Has tuple schema.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "coords": {
                        "type": "array",
                        "prefixItems": [{"type": "integer"}, {"type": "integer"}],
                        "minItems": 2,
                        "maxItems": 2
                    }
                }
            }),
        };
        sanitize_tool_schema(&mut tool.parameters);

        let coords = &tool.parameters["properties"]["coords"];
        assert!(coords.get("prefixItems").is_none());
        assert_eq!(coords["items"], serde_json::json!({"type": "integer"}));
        assert_eq!(coords["minItems"], 2);
        assert_eq!(coords["maxItems"], 2);
    }

    /// `$schema` is stripped from the root level.
    #[test]
    fn schema_keyword_stripped() {
        let mut tool = ToolDefinition {
            name: "test".into(),
            description: "".into(),
            parameters: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object"
            }),
        };
        sanitize_tool_schema(&mut tool.parameters);
        assert!(tool.parameters.get("$schema").is_none());
        assert_eq!(tool.parameters["type"], "object");
    }

    /// Idempotency: applying the sanitizer twice yields the same value.
    #[test]
    fn sanitizer_is_idempotent() {
        let mut tool = ToolDefinition {
            name: "test".into(),
            description: "".into(),
            parameters: serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "x": {"$ref": "#/$defs/X"}
                },
                "$defs": {"X": {"type": "string"}},
                "prefixItems": [{"type": "integer"}]
            }),
        };
        sanitize_tool_schema(&mut tool.parameters);
        let first = tool.parameters.clone();
        sanitize_tool_schema(&mut tool.parameters);
        assert_eq!(tool.parameters, first);
    }

    /// Standard JSON Schema keywords pass through untouched.
    #[test]
    fn standard_keywords_preserved() {
        let mut tool = ToolDefinition {
            name: "test".into(),
            description: "".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "minLength": 1, "maxLength": 100, "pattern": "^[a-z]+$"},
                    "count": {"type": "integer", "minimum": 0, "maximum": 10, "exclusiveMinimum": -1, "exclusiveMaximum": 11},
                    "choice": {"enum": ["a", "b", "c"]},
                    "flag": {"const": true},
                    "items": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
                    "nested": {"type": "object", "properties": {"x": {"type": "number"}}},
                    "complex": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                    "all": {"allOf": [{"type": "string"}, {"minLength": 2}]},
                    "extra": {"type": "object", "additionalProperties": false, "patternProperties": {"^x-": {"type": "string"}}}
                },
                "required": ["name"]
            }),
        };
        sanitize_tool_schema(&mut tool.parameters);
        // Verify all standard keywords are preserved.
        assert_eq!(tool.parameters["properties"]["name"]["minLength"], 1);
        assert_eq!(tool.parameters["properties"]["count"]["exclusiveMinimum"], -1);
        assert_eq!(
            tool.parameters["properties"]["choice"]["enum"],
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(tool.parameters["properties"]["flag"]["const"], true);
        assert_eq!(tool.parameters["properties"]["items"]["uniqueItems"], true);
        assert!(tool.parameters["properties"]["complex"]["oneOf"].is_array());
        assert!(tool.parameters["properties"]["all"]["allOf"].is_array());
        assert_eq!(tool.parameters["properties"]["extra"]["additionalProperties"], false);
        assert!(tool.parameters["properties"]["extra"]["patternProperties"].is_object());
    }
}
