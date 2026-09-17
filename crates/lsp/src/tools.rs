use async_trait::async_trait;
use concerto_core::error::ToolError;
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::CancellationToken;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing;

use crate::LspManager;

// ---------------------------------------------------------------------------
// Typed LSP tool inputs (single source of truth for the advertised schemas)
// ---------------------------------------------------------------------------

/// Input for file-only LSP tools ([`GetDiagnostics`], [`GetSemanticTokens`]).
///
/// The JSON schema advertised by those tools is derived from this struct via
/// [`derived_input_schema`], so the advertised contract can never drift from
/// the deserialization target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LspDocIdInput {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

/// Input for position-based LSP tools ([`GetHover`], [`GetCodeActions`],
/// [`FindReferences`], [`GetInlayHints`]).
///
/// `line`/`character` are optional and default to `0` at the boundary, so a
/// caller that omits them (as the schema allows) still gets a usable request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LspDocPositionInput {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    #[serde(default)]
    #[schemars(description = "Zero-based line number. Defaults to 0.")]
    pub line: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Zero-based character offset. Defaults to 0.")]
    pub character: Option<i64>,
}

/// Input for [`RenameSymbol`].
///
/// Unlike the previous hand-written macro schema, which marked every method's
/// extra fields optional, `new_name` is required here because the rename
/// builder cannot produce a well-formed request without it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LspRenameInput {
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    #[serde(default)]
    #[schemars(description = "Zero-based line number. Defaults to 0.")]
    pub line: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Zero-based character offset. Defaults to 0.")]
    pub character: Option<i64>,
    #[schemars(description = "New name for the symbol.")]
    pub new_name: String,
}

/// Input for [`ExecuteCodeAction`].
///
/// Takes a resolved command identifier, not a file path — the previous
/// hand-written macro schema wrongly advertised `file_path`/`line`/
/// `character`/`new_name` for the `workspace/executeCommand` method.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LspCommandInput {
    #[schemars(description = "Command identifier to execute.")]
    pub command: String,
    #[serde(default)]
    #[schemars(description = "Arguments passed to the command.")]
    pub arguments: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Schema derivation + lenient boundary coercion
// ---------------------------------------------------------------------------

/// Removes JSON Schema dialect/definition keywords that some tool-calling APIs
/// reject. The parsed definitions (`$defs`/`definitions`) stay inlined in
/// `properties`, so the contract is unchanged.
fn sanitise_schema(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("$schema");
        object.remove("$defs");
        object.remove("definitions");
    }
    value
}

/// Derives the advertised input schema from a struct type via `schemars` so
/// the contract can never drift from the deserialization target.
///
/// Requiredness follows the struct: plain fields (`file_path`, `new_name`,
/// `command`) are required; `#[serde(default)]`/`Option` fields are optional.
fn derived_input_schema<T: JsonSchema>() -> serde_json::Value {
    let root = schemars::schema_for!(T);
    let value = serde_json::to_value(&root).unwrap_or_else(|error| {
        tracing::error!(%error, "failed to serialize LSP input schema");
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    });
    sanitise_schema(value)
}

/// Leniently binds a raw LSP tool argument into a typed input struct.
///
/// The JSON Schema advertised by [`Tool::input_schema`] (derived from the same
/// struct) stays authoritative; only the boundary parser is lenient. Strict
/// deserialization is tried first. If it fails, per-field normalization is
/// applied and a second strict parse is attempted so a model that emits
/// string-typed integers for `line`/`character` still gets a usable tool call.
/// If the normalized input also fails to parse, the ORIGINAL strict
/// deserialize error is returned (message shape `invalid lsp input: {e}`).
fn coerce_lsp_input<T>(input: &serde_json::Value) -> Result<T, ToolError>
where
    T: serde::de::DeserializeOwned,
{
    match serde_json::from_value(input.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(strict_error) => {
            let normalized = normalize_lsp_input(input);
            serde_json::from_value(normalized).map_err(|_| ToolError::LspError {
                message: format!("invalid lsp input: {strict_error}"),
            })
        }
    }
}

/// Normalizes an LSP tool input after strict parsing has failed.
///
/// Only `line`/`character` accept a well-defined lenient coercion (string
/// typed integers), mirroring the git tool boundary. String fields stay
/// strict so a non-string `file_path`/`command`/`new_name` still fails fast
/// instead of producing a nonsensical request.
fn normalize_lsp_input(input: &serde_json::Value) -> serde_json::Value {
    let Some(object) = input.as_object() else {
        return input.clone();
    };
    let mut normalized = object.clone();
    for (field, value) in object {
        if matches!(field.as_str(), "line" | "character") {
            normalized.insert(field.clone(), coerce_i64(value));
        }
    }
    serde_json::Value::Object(normalized)
}

/// Coerces a string-typed integer field (e.g. `"line": "5"`) into a number.
/// Whitespace is trimmed before parsing; non-strings and non-numeric strings
/// pass through untouched so the strict parser reports them accurately.
fn coerce_i64(value: &serde_json::Value) -> serde_json::Value {
    match value.as_str() {
        Some(text) => match text.trim().parse::<i64>() {
            Ok(number) => serde_json::json!(number),
            Err(_) => serde_json::json!(text),
        },
        None => value.clone(),
    }
}

fn file_uri(file_path: &str) -> String {
    format!("file://{file_path}")
}

macro_rules! lsp_tool {
    ($name:ident, $method:expr, $desc:expr, $input_type:ty, $build_params:expr) => {
        pub struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                stringify!($name)
            }
            fn description(&self) -> &str {
                $desc
            }
            fn input_schema(&self) -> serde_json::Value {
                derived_input_schema::<$input_type>()
            }
            fn capability_requirements(&self) -> CapabilitySet {
                CapabilitySet::default()
            }
            async fn execute(
                &self,
                input: serde_json::Value,
                _policy: &dyn PolicyEngine,
                session: &SessionContext,
                cancel: CancellationToken,
            ) -> Result<ToolOutput, ToolError> {
                let client = LspManager::get_or_start(
                    session.project_id.clone(),
                    session.project_dir.clone(),
                    cancel,
                )
                .await;
                let mut client = client.lock().await;
                let params = ($build_params)(&input)?;
                let result = client.send_request($method, params).await?;
                Ok(ToolOutput { summary: format!("{} completed", $desc), data: result })
            }
        }
    };
    ($name:ident, $method:expr, $desc:expr, $input_type:ty, $build_params:expr, $result_transform:expr) => {
        pub struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                stringify!($name)
            }
            fn description(&self) -> &str {
                $desc
            }
            fn input_schema(&self) -> serde_json::Value {
                derived_input_schema::<$input_type>()
            }
            fn capability_requirements(&self) -> CapabilitySet {
                CapabilitySet::default()
            }
            async fn execute(
                &self,
                input: serde_json::Value,
                _policy: &dyn PolicyEngine,
                session: &SessionContext,
                cancel: CancellationToken,
            ) -> Result<ToolOutput, ToolError> {
                let client = LspManager::get_or_start(
                    session.project_id.clone(),
                    session.project_dir.clone(),
                    cancel,
                )
                .await;
                let mut client = client.lock().await;
                let params = ($build_params)(&input)?;
                let raw = client.send_request($method, params).await?;
                let data = ($result_transform)(raw);
                Ok(ToolOutput { summary: format!("{} completed", $desc), data })
            }
        }
    };
}

fn text_doc_id(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspDocIdInput = coerce_lsp_input(input)?;
    Ok(serde_json::json!({
        "textDocument": { "uri": file_uri(&parsed.file_path) }
    }))
}

fn text_doc_position(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspDocPositionInput = coerce_lsp_input(input)?;
    let uri = file_uri(&parsed.file_path);
    let line = parsed.line.unwrap_or(0);
    let character = parsed.character.unwrap_or(0);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    }))
}

fn text_doc_rename(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspRenameInput = coerce_lsp_input(input)?;
    let uri = file_uri(&parsed.file_path);
    let line = parsed.line.unwrap_or(0);
    let character = parsed.character.unwrap_or(0);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "newName": parsed.new_name
    }))
}

fn code_action_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspDocPositionInput = coerce_lsp_input(input)?;
    let uri = file_uri(&parsed.file_path);
    let line = parsed.line.unwrap_or(0);
    let character = parsed.character.unwrap_or(0);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character }
        },
        "context": { "diagnostics": [] }
    }))
}

fn execute_code_action_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspCommandInput = coerce_lsp_input(input)?;
    // `arguments` is an opaque, schema-optional payload forwarded unchanged:
    // missing defaults to `[]`, while an explicit `null` passes through as
    // null (preserving the original `unwrap_or`-only-on-`None` behavior).
    let arguments = input.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([]));
    Ok(serde_json::json!({
        "command": parsed.command,
        "arguments": arguments
    }))
}

fn find_references_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let parsed: LspDocPositionInput = coerce_lsp_input(input)?;
    let uri = file_uri(&parsed.file_path);
    let line = parsed.line.unwrap_or(0);
    let character = parsed.character.unwrap_or(0);
    let mut params = serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    params["context"] = serde_json::json!({ "includeDeclaration": true });
    Ok(params)
}

/// Diagnostics are pushed by the server and cached. This tool reads the cache.
pub struct GetDiagnostics;

#[async_trait]
impl Tool for GetDiagnostics {
    fn name(&self) -> &str {
        "GetDiagnostics"
    }
    fn description(&self) -> &str {
        "Retrieve diagnostics for a file"
    }
    fn input_schema(&self) -> serde_json::Value {
        derived_input_schema::<LspDocIdInput>()
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: LspDocIdInput = coerce_lsp_input(&input)?;
        let client = LspManager::get_or_start(
            session.project_id.clone(),
            session.project_dir.clone(),
            cancel,
        )
        .await;
        let client = client.lock().await;
        let diags = client.get_diagnostics(&parsed.file_path).await;
        Ok(ToolOutput {
            summary: format!("{} diagnostics for {}", diags.len(), parsed.file_path),
            data: serde_json::json!({ "diagnostics": diags }),
        })
    }
}

lsp_tool!(
    GetHover,
    "textDocument/hover",
    "Get hover information at a position",
    LspDocPositionInput,
    text_doc_position
);
lsp_tool!(
    GetSemanticTokens,
    "textDocument/semanticTokens/full",
    "Retrieve semantic tokens for a file",
    LspDocIdInput,
    text_doc_id
);
lsp_tool!(
    GetCodeActions,
    "textDocument/codeAction",
    "List code actions at a position",
    LspDocPositionInput,
    code_action_params
);
lsp_tool!(
    ExecuteCodeAction,
    "workspace/executeCommand",
    "Execute a specific code action",
    LspCommandInput,
    execute_code_action_params
);
lsp_tool!(
    RenameSymbol,
    "textDocument/rename",
    "Rename a symbol across the workspace",
    LspRenameInput,
    text_doc_rename
);
lsp_tool!(
    FindReferences,
    "textDocument/references",
    "Find all references to a symbol",
    LspDocPositionInput,
    find_references_params
);
lsp_tool!(
    GetInlayHints,
    "textDocument/inlayHint",
    "Retrieve inlay hints for a range",
    LspDocPositionInput,
    text_doc_position
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // Parameter builders
    // ------------------------------------------------------------------

    #[test]
    fn text_doc_id_with_file_path() {
        let input = json!({"file_path": "/home/user/main.rs"});
        let result = text_doc_id(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///home/user/main.rs" }
            })
        );
    }

    #[test]
    fn text_doc_id_missing_file_path_errors() {
        let input = json!({});
        let err = text_doc_id(&input).unwrap_err();
        assert_missing_field(&err, "file_path");
    }

    #[test]
    fn text_doc_position_with_all_fields() {
        let input = json!({"file_path": "/a.rs", "line": 10, "character": 5});
        let result = text_doc_position(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///a.rs" },
                "position": { "line": 10, "character": 5 }
            })
        );
    }

    #[test]
    fn text_doc_position_defaults_line_character() {
        let input = json!({"file_path": "/a.rs"});
        let result = text_doc_position(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///a.rs" },
                "position": { "line": 0, "character": 0 }
            })
        );
    }

    #[test]
    fn text_doc_position_missing_file_path_errors() {
        let input = json!({});
        let err = text_doc_position(&input).unwrap_err();
        assert_missing_field(&err, "file_path");
    }

    #[test]
    fn text_doc_rename_with_all_fields() {
        let input = json!({"file_path": "/b.rs", "line": 3, "character": 7, "new_name": "foo"});
        let result = text_doc_rename(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///b.rs" },
                "position": { "line": 3, "character": 7 },
                "newName": "foo"
            })
        );
    }

    #[test]
    fn text_doc_rename_missing_new_name_errors() {
        let input = json!({"file_path": "/b.rs", "line": 3, "character": 7});
        let err = text_doc_rename(&input).unwrap_err();
        assert_missing_field(&err, "new_name");
    }

    #[test]
    fn code_action_params_structure() {
        let input = json!({"file_path": "/c.rs", "line": 5, "character": 2});
        let result = code_action_params(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///c.rs" },
                "range": {
                    "start": { "line": 5, "character": 2 },
                    "end": { "line": 5, "character": 2 }
                },
                "context": { "diagnostics": [] }
            })
        );
    }

    #[test]
    fn execute_code_action_params_with_args() {
        let input = json!({"command": "cmd", "arguments": [1, "two"]});
        let result = execute_code_action_params(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "command": "cmd",
                "arguments": [1, "two"]
            })
        );
    }

    #[test]
    fn execute_code_action_params_defaults_empty_args() {
        let input = json!({"command": "cmd"});
        let result = execute_code_action_params(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "command": "cmd",
                "arguments": []
            })
        );
    }

    #[test]
    fn execute_code_action_params_missing_command_errors() {
        let input = json!({});
        let err = execute_code_action_params(&input).unwrap_err();
        assert_missing_field(&err, "command");
    }

    #[test]
    fn find_references_params_includes_context() {
        let input = json!({"file_path": "/d.rs", "line": 1, "character": 2});
        let result = find_references_params(&input).unwrap();
        assert_eq!(
            result,
            json!({
                "textDocument": { "uri": "file:///d.rs" },
                "position": { "line": 1, "character": 2 },
                "context": { "includeDeclaration": true }
            })
        );
    }

    // ------------------------------------------------------------------
    // Tool metadata
    // ------------------------------------------------------------------

    fn all_tools() -> [&'static dyn Tool; 8] {
        [
            &GetDiagnostics,
            &GetHover,
            &GetSemanticTokens,
            &GetCodeActions,
            &ExecuteCodeAction,
            &RenameSymbol,
            &FindReferences,
            &GetInlayHints,
        ]
    }

    #[test]
    fn tool_names() {
        assert_eq!(GetDiagnostics.name(), "GetDiagnostics");
        assert_eq!(GetHover.name(), "GetHover");
        assert_eq!(GetSemanticTokens.name(), "GetSemanticTokens");
        assert_eq!(GetCodeActions.name(), "GetCodeActions");
        assert_eq!(ExecuteCodeAction.name(), "ExecuteCodeAction");
        assert_eq!(RenameSymbol.name(), "RenameSymbol");
        assert_eq!(FindReferences.name(), "FindReferences");
        assert_eq!(GetInlayHints.name(), "GetInlayHints");
    }

    #[test]
    fn tool_descriptions() {
        fn desc(t: &dyn Tool) -> &str {
            t.description()
        }

        assert_eq!(desc(&GetDiagnostics), "Retrieve diagnostics for a file");
        assert_eq!(desc(&GetHover), "Get hover information at a position");
        assert_eq!(desc(&GetSemanticTokens), "Retrieve semantic tokens for a file");
        assert_eq!(desc(&GetCodeActions), "List code actions at a position");
        assert_eq!(desc(&ExecuteCodeAction), "Execute a specific code action");
        assert_eq!(desc(&RenameSymbol), "Rename a symbol across the workspace");
        assert_eq!(desc(&FindReferences), "Find all references to a symbol");
        assert_eq!(desc(&GetInlayHints), "Retrieve inlay hints for a range");
    }

    #[test]
    fn get_diagnostics_schema_has_no_position_fields() {
        let schema = GetDiagnostics.input_schema();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        assert!(props.contains_key("file_path"));
        assert!(!props.contains_key("line"));
        assert!(!props.contains_key("character"));
    }

    #[test]
    fn macro_tool_schema_has_optional_position_fields() {
        let schema = GetHover.input_schema();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        assert!(props.contains_key("file_path"));
        assert!(props.contains_key("line"));
        assert!(props.contains_key("character"));
    }

    #[test]
    fn macro_tool_schema_has_optional_new_name_field() {
        let schema = RenameSymbol.input_schema();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        assert!(props.contains_key("new_name"));
    }

    #[test]
    fn capability_requirements_default() {
        for tool in all_tools() {
            let caps = tool.capability_requirements();
            assert_eq!(
                caps,
                CapabilitySet::default(),
                "{} should have default capabilities",
                tool.name()
            );
        }
    }

    #[test]
    fn lsp_tool_schemas_are_derived_and_sanitised() {
        // Every advertised schema must be an object and stripped of
        // provider-incompatible dialect/definition keywords (shell pattern).
        for tool in all_tools() {
            let schema = tool.input_schema();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} schema should have type=object",
                tool.name()
            );
            assert!(schema.get("$schema").is_none(), "{} must not emit $schema", tool.name());
            assert!(schema.get("$defs").is_none(), "{} must not emit $defs", tool.name());
            assert!(
                schema.get("definitions").is_none(),
                "{} must not emit definitions",
                tool.name()
            );
        }
    }

    #[test]
    fn get_diagnostics_schema_only_file_path_required() {
        let schema = GetDiagnostics.input_schema();
        let required = schema.get("required").and_then(|r| r.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("file_path"));
    }

    #[test]
    fn macro_tool_schema_includes_new_name_for_rename() {
        let schema = RenameSymbol.input_schema();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        let new_name = props.get("new_name").unwrap();
        assert_eq!(new_name.get("type").and_then(|v| v.as_str()), Some("string"));
    }

    #[test]
    fn macro_tool_schema_line_and_character_integers() {
        let schema = GetHover.input_schema();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        for field in &["line", "character"] {
            let val = props.get(*field).unwrap();
            // `Option<i64>` renders as a nullable integer (the parser accepts
            // missing/null and defaults both to 0 at the boundary).
            let types = val.get("type").and_then(|t| t.as_array()).unwrap();
            assert!(types.contains(&json!("integer")), "{field} must be an integer");
            assert!(types.contains(&json!("null")), "{field} must be nullable");
        }
    }

    #[test]
    fn text_doc_position_preserves_explicit_zero_values() {
        let input = json!({"file_path": "/z.rs", "line": 0, "character": 0});
        let result = text_doc_position(&input).unwrap();
        assert_eq!(result["position"]["line"], 0);
        assert_eq!(result["position"]["character"], 0);
    }

    #[test]
    fn text_doc_rename_defaults_line_character_when_absent() {
        let input = json!({"file_path": "/r.rs", "new_name": "bar"});
        let result = text_doc_rename(&input).unwrap();
        assert_eq!(result["position"]["line"], 0);
        assert_eq!(result["position"]["character"], 0);
        assert_eq!(result["newName"], "bar");
    }

    // ------------------------------------------------------------------
    // Schema/runtime contract tests (shell.rs pattern): the schema's required
    // fields deserialize into the struct that produced the schema.
    // ------------------------------------------------------------------

    /// Builds the smallest valid input from a schema's own `required` list,
    /// filling each required field with a plausible sample value.
    fn minimal_required_input(schema: &serde_json::Value) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        for field in schema["required"].as_array().expect("required must be an array") {
            let name = field.as_str().expect("required entry is a string");
            let value = match name {
                "file_path" => json!("/tmp/x.rs"),
                "command" => json!("codeAction.resolve"),
                "new_name" => json!("renamed"),
                other => panic!("no sample value for schema-required field `{other}`"),
            };
            object.insert(name.to_string(), value);
        }
        serde_json::Value::Object(object)
    }

    /// Asserts that `err` is an `LspError` reporting the given missing
    /// required field in the struct-derived validation message.
    fn assert_missing_field(err: &ToolError, field: &str) {
        let expected = format!("invalid lsp input: missing field `{field}`");
        match err {
            ToolError::LspError { message } => assert!(
                message.starts_with(&expected),
                "expected message starting with `{expected}`, got: {message}"
            ),
            other => panic!("expected ToolError::LspError, got: {other}"),
        }
    }

    #[test]
    fn doc_id_schema_runtime_contract_minimal() {
        let schema = GetDiagnostics.input_schema();
        let minimal = minimal_required_input(&schema);
        let parsed: LspDocIdInput =
            coerce_lsp_input(&minimal).expect("schema-required input must deserialize");
        assert_eq!(parsed.file_path, "/tmp/x.rs");
    }

    #[test]
    fn doc_position_schema_runtime_contract_minimal() {
        let schema = GetHover.input_schema();
        let minimal = minimal_required_input(&schema);
        let parsed: LspDocPositionInput =
            coerce_lsp_input(&minimal).expect("schema-required input must deserialize");
        assert_eq!(parsed.file_path, "/tmp/x.rs");
        assert!(parsed.line.is_none(), "line must default to None when absent");
        assert!(parsed.character.is_none(), "character must default to None when absent");
        // The boundary itself is lenient, defaulting omitted positions to 0:0.
        let params = text_doc_position(&minimal).expect("minimal input must build params");
        assert_eq!(params["position"]["line"], 0);
        assert_eq!(params["position"]["character"], 0);
    }

    #[test]
    fn rename_schema_runtime_contract_minimal() {
        let schema = RenameSymbol.input_schema();
        let minimal = minimal_required_input(&schema);
        let parsed: LspRenameInput =
            coerce_lsp_input(&minimal).expect("schema-required input must deserialize");
        assert_eq!(parsed.file_path, "/tmp/x.rs");
        assert_eq!(parsed.new_name, "renamed");
        assert!(parsed.line.is_none(), "line must default to None when absent");
        assert!(parsed.character.is_none(), "character must default to None when absent");
    }

    #[test]
    fn command_schema_runtime_contract_minimal() {
        let schema = ExecuteCodeAction.input_schema();
        let minimal = minimal_required_input(&schema);
        let parsed: LspCommandInput =
            coerce_lsp_input(&minimal).expect("schema-required input must deserialize");
        assert_eq!(parsed.command, "codeAction.resolve");
        assert!(parsed.arguments.is_none(), "arguments must be optional");
        // Missing arguments default to an empty array at the boundary.
        let params = execute_code_action_params(&minimal).expect("minimal input must build params");
        assert_eq!(params["arguments"], json!([]));
    }

    #[test]
    fn schemas_runtime_contract_full_input_deserializes() {
        // A representative fully-populated input (all optional fields present)
        // must deserialize, proving the schema's optional fields map onto the
        // structs correctly.
        let input = json!({
            "file_path": "/a.rs",
            "line": 3,
            "character": 7,
            "new_name": "foo",
            "command": "cmd",
            "arguments": [1, "two"]
        });
        let parsed: LspDocPositionInput =
            coerce_lsp_input(&input).expect("position input must accept the full payload");
        assert_eq!(parsed.line, Some(3));
        assert_eq!(parsed.character, Some(7));
        let parsed: LspRenameInput =
            coerce_lsp_input(&input).expect("rename input must accept the full payload");
        assert_eq!(parsed.new_name, "foo");
        let parsed: LspCommandInput =
            coerce_lsp_input(&input).expect("command input must accept the full payload");
        assert_eq!(parsed.command, "cmd");
        assert_eq!(parsed.arguments, Some(json!([1, "two"])));
    }

    // ------------------------------------------------------------------
    // Per-method requiredness tests (the drift the structs eliminate)
    // ------------------------------------------------------------------

    #[test]
    fn tool_required_fields_match_each_method() {
        // Replaces the old blanket "every tool requires file_path" check: the
        // per-method structs now advertise exactly what each LSP method needs.
        let cases: [(&dyn Tool, &[&str]); 8] = [
            (&GetDiagnostics, &["file_path"]),
            (&GetHover, &["file_path"]),
            (&GetSemanticTokens, &["file_path"]),
            (&GetCodeActions, &["file_path"]),
            (&ExecuteCodeAction, &["command"]),
            (&RenameSymbol, &["file_path", "new_name"]),
            (&FindReferences, &["file_path"]),
            (&GetInlayHints, &["file_path"]),
        ];
        for (tool, expected) in cases {
            let schema = tool.input_schema();
            let required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required must be an array")
                .iter()
                .filter_map(|value| value.as_str())
                .collect();
            assert_eq!(required.as_slice(), expected, "{} required fields", tool.name());
        }
    }

    #[test]
    fn execute_code_action_schema_advertises_command_not_file_path() {
        // Drift regression: the old hand-written macro schema advertised the
        // same file/position fields for every method, which was wrong for the
        // workspace/executeCommand tool (it consumes a command id, not a path).
        let schema = ExecuteCodeAction.input_schema();
        let props = schema["properties"].as_object().expect("properties must be an object");
        assert!(props.contains_key("command"), "schema must advertise command");
        assert!(props.contains_key("arguments"), "schema must advertise arguments");
        assert!(!props.contains_key("file_path"), "schema must not advertise file_path");
        assert!(!props.contains_key("line"), "schema must not advertise line");
    }

    #[test]
    fn rename_symbol_schema_requires_new_name() {
        // Drift regression: the runtime builder always required `new_name`, but
        // the old schema marked it optional.
        let schema = RenameSymbol.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be an array")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert_eq!(required, vec!["file_path", "new_name"]);
    }

    // ------------------------------------------------------------------
    // Additional parameter-builder edge cases
    // ------------------------------------------------------------------

    /// `code_action_params` defaults `line` and `character` to 0 when not provided.
    #[test]
    fn test_code_action_params_defaults_line_character() {
        let input = json!({"file_path": "/a.rs"});
        let result = code_action_params(&input).unwrap();
        assert_eq!(result["range"]["start"]["line"], 0);
        assert_eq!(result["range"]["start"]["character"], 0);
        assert_eq!(result["range"]["end"]["line"], 0);
        assert_eq!(result["range"]["end"]["character"], 0);
    }

    /// `code_action_params` must error when `file_path` is missing.
    #[test]
    fn test_code_action_params_missing_file_path_errors() {
        let input = json!({"line": 1, "character": 2});
        let err = code_action_params(&input).unwrap_err();
        assert_missing_field(&err, "file_path");
    }

    /// `execute_code_action_params` with null arguments passes null through
    /// (the raw-value read only defaults `[]` when the key is absent).
    #[test]
    fn test_execute_code_action_params_null_arguments() {
        let input = json!({"command": "cmd", "arguments": null});
        let result = execute_code_action_params(&input).unwrap();
        assert_eq!(result["command"], "cmd");
        assert_eq!(result["arguments"], json!(null));
    }

    /// `find_references_params` defaults `line` and `character` to 0.
    #[test]
    fn test_find_references_params_defaults_line_character() {
        let input = json!({"file_path": "/d.rs"});
        let result = find_references_params(&input).unwrap();
        assert_eq!(result["position"]["line"], 0);
        assert_eq!(result["position"]["character"], 0);
        assert_eq!(result["context"], json!({"includeDeclaration": true}));
    }

    /// `find_references_params` must error when `file_path` is missing.
    #[test]
    fn test_find_references_params_missing_file_path_errors() {
        let input = json!({"line": 5, "character": 3});
        let err = find_references_params(&input).unwrap_err();
        assert_missing_field(&err, "file_path");
    }

    /// The lenient boundary accepts string-typed integers for positions
    /// (mirror of the git tool's normalization; the old raw-Value parser
    /// silently mapped them to 0, so this is strictly a usability fix).
    #[test]
    fn lenient_boundary_coerces_string_typed_positions() {
        let input = json!({ "file_path": "/a.rs", "line": "5", "character": " 3 " });
        let result = text_doc_position(&input).unwrap();
        assert_eq!(result["position"]["line"], 5);
        assert_eq!(result["position"]["character"], 3);
    }

    /// `Option<i64>` treats an explicit null like a missing field, so null
    /// positions still default to 0 exactly as `as_i64().unwrap_or(0)` did.
    #[test]
    fn line_and_character_null_defaults_to_zero() {
        let input = json!({ "file_path": "/a.rs", "line": null, "character": null });
        let result = text_doc_position(&input).unwrap();
        assert_eq!(result["position"]["line"], 0);
        assert_eq!(result["position"]["character"], 0);
    }

    /// Non-numeric positions (e.g. a boolean or a float) now fail fast with a
    /// struct-derived validation error instead of silently becoming 0.
    #[test]
    fn malformed_positions_fail_fast() {
        let input = json!({ "file_path": "/a.rs", "line": true });
        let err = text_doc_position(&input).unwrap_err();
        match err {
            ToolError::LspError { message } => assert!(
                message.starts_with("invalid lsp input:"),
                "expected validation error, got: {message}"
            ),
            other => panic!("expected ToolError::LspError, got: {other}"),
        }
    }

    /// Every tool must have a non-empty human-readable description.
    #[test]
    fn test_all_tools_descriptions_are_non_empty() {
        for tool in all_tools() {
            let desc = tool.description();
            assert!(!desc.is_empty(), "{} has an empty description", tool.name());
            // Descriptions should be at least a few words.
            assert!(desc.len() > 10, "{} description is too short: '{}'", tool.name(), desc);
        }
    }
}
