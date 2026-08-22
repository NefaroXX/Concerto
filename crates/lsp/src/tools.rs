use async_trait::async_trait;
use concerto_core::error::ToolError;
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::CancellationToken;

use crate::LspManager;

macro_rules! lsp_tool {
    ($name:ident, $method:expr, $desc:expr, $build_params:expr) => {
        pub struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str { stringify!($name) }
            fn description(&self) -> &str { $desc }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "line": {"type": "integer"},
                        "character": {"type": "integer"},
                        "new_name": {"type": "string"}
                    },
                    "required": ["file_path"]
                })
            }
            fn capability_requirements(&self) -> CapabilitySet { CapabilitySet::default() }
            async fn execute(&self, input: serde_json::Value, _policy: &dyn PolicyEngine, session: &SessionContext, cancel: CancellationToken) -> Result<ToolOutput, ToolError> {
                let client = LspManager::get_or_start(session.project_id.clone(), session.project_dir.clone(), cancel).await;
                let mut client = client.lock().await;
                let params = ($build_params)(&input)?;
                let result = client.send_request($method, params).await?;
                Ok(ToolOutput {
                    summary: format!("{} completed", $desc),
                    data: result,
                })
            }
        }
    };
    ($name:ident, $method:expr, $desc:expr, $build_params:expr, $result_transform:expr) => {
        pub struct $name;
        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str { stringify!($name) }
            fn description(&self) -> &str { $desc }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "line": {"type": "integer"},
                        "character": {"type": "integer"},
                        "new_name": {"type": "string"}
                    },
                    "required": ["file_path"]
                })
            }
            fn capability_requirements(&self) -> CapabilitySet { CapabilitySet::default() }
            async fn execute(&self, input: serde_json::Value, _policy: &dyn PolicyEngine, session: &SessionContext, cancel: CancellationToken) -> Result<ToolOutput, ToolError> {
                let client = LspManager::get_or_start(session.project_id.clone(), session.project_dir.clone(), cancel).await;
                let mut client = client.lock().await;
                let params = ($build_params)(&input)?;
                let raw = client.send_request($method, params).await?;
                let data = ($result_transform)(raw);
                Ok(ToolOutput {
                    summary: format!("{} completed", $desc),
                    data,
                })
            }
        }
    };
}

fn text_doc_id(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing file_path".into() })?;
    let uri = format!("file://{}", file_path);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri }
    }))
}

fn text_doc_position(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing file_path".into() })?;
    let line = input.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
    let character = input.get("character").and_then(|v| v.as_i64()).unwrap_or(0);
    let uri = format!("file://{}", file_path);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    }))
}

fn text_doc_rename(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing file_path".into() })?;
    let line = input.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
    let character = input.get("character").and_then(|v| v.as_i64()).unwrap_or(0);
    let new_name = input
        .get("new_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing new_name for rename".into() })?;
    let uri = format!("file://{}", file_path);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "newName": new_name
    }))
}

fn code_action_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing file_path".into() })?;
    let line = input.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
    let character = input.get("character").and_then(|v| v.as_i64()).unwrap_or(0);
    let uri = format!("file://{}", file_path);
    Ok(serde_json::json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character }
        },
        "context": {
            "diagnostics": []
        }
    }))
}

fn execute_code_action_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::LspError { message: "missing command".into() })?;
    let args = input.get("arguments").cloned().unwrap_or(serde_json::json!([]));
    Ok(serde_json::json!({
        "command": command,
        "arguments": args
    }))
}

fn find_references_params(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    let pos = text_doc_position(input)?;
    let mut obj = pos.as_object().cloned().unwrap_or_default();
    obj.insert("context".into(), serde_json::json!({ "includeDeclaration": true }));
    Ok(serde_json::Value::Object(obj))
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
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"}
            },
            "required": ["file_path"]
        })
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
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::LspError { message: "missing file_path".into() })?;
        let client = LspManager::get_or_start(
            session.project_id.clone(),
            session.project_dir.clone(),
            cancel,
        )
        .await;
        let client = client.lock().await;
        let diags = client.get_diagnostics(file_path).await;
        Ok(ToolOutput {
            summary: format!("{} diagnostics for {}", diags.len(), file_path),
            data: serde_json::json!({ "diagnostics": diags }),
        })
    }
}

lsp_tool!(GetHover, "textDocument/hover", "Get hover information at a position", text_doc_position);
lsp_tool!(
    GetSemanticTokens,
    "textDocument/semanticTokens/full",
    "Retrieve semantic tokens for a file",
    text_doc_id
);
lsp_tool!(
    GetCodeActions,
    "textDocument/codeAction",
    "List code actions at a position",
    code_action_params
);
lsp_tool!(
    ExecuteCodeAction,
    "workspace/executeCommand",
    "Execute a specific code action",
    execute_code_action_params
);
lsp_tool!(
    RenameSymbol,
    "textDocument/rename",
    "Rename a symbol across the workspace",
    text_doc_rename
);
lsp_tool!(
    FindReferences,
    "textDocument/references",
    "Find all references to a symbol",
    find_references_params
);
lsp_tool!(
    GetInlayHints,
    "textDocument/inlayHint",
    "Retrieve inlay hints for a range",
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
        assert!(matches!(&err, ToolError::LspError { message } if message == "missing file_path"));
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
        assert!(matches!(&err, ToolError::LspError { message } if message == "missing file_path"));
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
        assert!(
            matches!(&err, ToolError::LspError { message } if message == "missing new_name for rename")
        );
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
        assert!(matches!(&err, ToolError::LspError { message } if message == "missing command"));
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
    fn all_tools_require_file_path_by_default() {
        // All macro-generated tools require file_path.
        // GetDiagnostics also requires file_path (manual impl).
        let tools: [&dyn Tool; 8] = [
            &GetDiagnostics,
            &GetHover,
            &GetSemanticTokens,
            &GetCodeActions,
            &ExecuteCodeAction,
            &RenameSymbol,
            &FindReferences,
            &GetInlayHints,
        ];
        for tool in &tools {
            let schema = tool.input_schema();
            let required = schema.get("required").and_then(|r| r.as_array()).unwrap();
            let paths: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
            assert!(paths.contains(&"file_path"), "{} should require file_path", tool.name());
        }
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
        let tools: [&dyn Tool; 8] = [
            &GetDiagnostics,
            &GetHover,
            &GetSemanticTokens,
            &GetCodeActions,
            &ExecuteCodeAction,
            &RenameSymbol,
            &FindReferences,
            &GetInlayHints,
        ];
        for tool in &tools {
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
    fn all_tool_schemas_are_object_type() {
        let tools: [&dyn Tool; 8] = [
            &GetDiagnostics,
            &GetHover,
            &GetSemanticTokens,
            &GetCodeActions,
            &ExecuteCodeAction,
            &RenameSymbol,
            &FindReferences,
            &GetInlayHints,
        ];
        for tool in &tools {
            let schema = tool.input_schema();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} schema should have type=object",
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
            assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("integer"));
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
        assert!(matches!(&err, ToolError::LspError { message } if message == "missing file_path"));
    }

    /// `execute_code_action_params` with null arguments passes null through
    /// (the `get` returns `Some(Value::Null)`, so `unwrap_or` does not fire).
    #[test]
    fn test_execute_code_action_params_null_arguments() {
        let input = json!({"command": "cmd", "arguments": null});
        let result = execute_code_action_params(&input).unwrap();
        assert_eq!(result["command"], "cmd");
        // null passes through because `unwrap_or` only fires on `None`.
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
        assert!(matches!(&err, ToolError::LspError { message } if message == "missing file_path"));
    }

    /// Every tool must have a non-empty human-readable description.
    #[test]
    fn test_all_tools_descriptions_are_non_empty() {
        let tools: [&dyn Tool; 8] = [
            &GetDiagnostics,
            &GetHover,
            &GetSemanticTokens,
            &GetCodeActions,
            &ExecuteCodeAction,
            &RenameSymbol,
            &FindReferences,
            &GetInlayHints,
        ];
        for tool in &tools {
            let desc = tool.description();
            assert!(!desc.is_empty(), "{} has an empty description", tool.name());
            // Descriptions should be at least a few words.
            assert!(desc.len() > 10, "{} description is too short: '{}'", tool.name(), desc);
        }
    }
}
