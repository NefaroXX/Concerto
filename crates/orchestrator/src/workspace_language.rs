//! One-shot Rust language queries for operator file workflows.
//! Each query owns its client, so diagnostics cannot be an old shared cache.
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, PathPolicyFacts, SessionContext, ToolOutput};
use concerto_core::{CancellationToken, ToolError};
use concerto_lsp::client::{file_uri, LspClient};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::harness_service::{HarnessService, HarnessServiceError};

pub(crate) const TOOL_NAME: &str = "workspace.language";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    operation: String,
    file_path: String,
    content: String,
    line: u32,
    character: u32,
}

fn error(message: impl Into<String>) -> ToolError {
    ToolError::LspError { message: message.into() }
}

fn request(input: &Input, uri: &str) -> Result<(&'static str, Value), ToolError> {
    let method = match input.operation.as_str() {
        "hover" => "textDocument/hover",
        "definition" => "textDocument/definition",
        "completion" => "textDocument/completion",
        "references" => "textDocument/references",
        _ => return Err(error("unsupported language query")),
    };
    let mut params = json!({"textDocument": {"uri": uri}, "position": {"line": input.line, "character": input.character}});
    if input.operation == "references" {
        params["context"] = json!({"includeDeclaration": true});
    }
    Ok((method, params))
}

impl HarnessService {
    /// Positions are zero-based UTF-16 units, matching LSP and Desktop.
    pub async fn workspace_language(
        &self,
        path: &str,
        operation: &str,
        line: u32,
        character: u32,
        cancel: CancellationToken,
    ) -> Result<Value, HarnessServiceError> {
        if Path::new(path).extension().and_then(|s| s.to_str()) != Some("rs") {
            return Err(error("the current language backend supports Rust (.rs) files only").into());
        }
        let document = self.workspace_read(path, cancel.clone()).await?;
        let root = self.session().project_dir.canonicalize().map_err(ToolError::from)?;
        let absolute = root.join(path).canonicalize().map_err(ToolError::from)?;
        if !absolute.starts_with(&root) {
            return Err(error("file is outside the selected project").into());
        }
        let file_path = absolute.to_str().ok_or_else(|| error("file path must be UTF-8"))?;
        Ok(self.invoke_executor(TOOL_NAME, json!({
            "operation": operation, "file_path": file_path, "content": document.content, "line": line, "character": character,
        }), cancel).await?.data)
    }
}

pub(crate) struct WorkspaceLanguageTool;

#[async_trait]
impl Tool for WorkspaceLanguageTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> &str {
        "Inspect Rust language information for an explicitly selected document"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "operation": {"type": "string", "enum": ["diagnostics", "hover", "definition", "completion", "references"]},
            "file_path": {"type": "string"}, "content": {"type": "string"},
            "line": {"type": "integer", "minimum": 0}, "character": {"type": "integer", "minimum": 0}
        }, "required": ["operation", "file_path", "content", "line", "character"], "additionalProperties": false})
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
    fn path_facts(&self, input: &Value, _: &SessionContext) -> Option<PathPolicyFacts> {
        Some(PathPolicyFacts {
            operation: TOOL_NAME.into(),
            attempted_path: input["file_path"].as_str().map(str::to_owned),
            resolved_path: None,
            attempted_destination: None,
            resolved_destination: None,
        })
    }
    async fn execute(
        &self,
        input: Value,
        _: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = serde_json::from_value(input).map_err(|e| error(e.to_string()))?;
        if !matches!(
            input.operation.as_str(),
            "diagnostics" | "hover" | "definition" | "completion" | "references"
        ) {
            return Err(error("unsupported language query"));
        }
        if input.content.len() > crate::workspace_files::MAX_DOCUMENT_BYTES {
            return Err(error("document exceeds 1 MiB limit"));
        }
        let root = session.project_dir.canonicalize()?;
        let path = Path::new(&input.file_path).canonicalize()?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(error("file is outside the selected project"));
        }
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut client = LspClient::new(&root, "rust-analyzer");
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(60), query(&mut client, &input, cancel.clone())) => {
                result.unwrap_or_else(|_| Err(error("language query timed out after 60 seconds")))
            }
        };
        // This client belongs to this operation; never stop another frontend's
        // language server or leave a cancelled query's child behind.
        let cleanup = cancel.child_token();
        cleanup.cancel();
        let _ = client.stop(cleanup).await;
        let data = result?;
        Ok(ToolOutput {
            summary: format!("Rust {} for {}", input.operation, input.file_path),
            data,
        })
    }
}

async fn query(
    client: &mut LspClient,
    input: &Input,
    cancel: CancellationToken,
) -> Result<Value, ToolError> {
    client.start(cancel).await?;
    let uri = file_uri(Path::new(&input.file_path))?;
    client
        .send_notification(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "rust", "version": 1, "text": input.content,
            }}),
        )
        .await?;
    if input.operation == "diagnostics" {
        // The existing client's cache keys are URI paths (percent-encoded),
        // so use the URI we sent, not an unescaped filesystem path.
        let cache_key = uri.trim_start_matches("file://");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if client.has_seen_file(cache_key).await {
                return Ok(
                    json!({"state": "received", "diagnostics": client.get_diagnostics(cache_key).await, "file_path": input.file_path}),
                );
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(
                    json!({"state": "pending", "diagnostics": Value::Null, "file_path": input.file_path, "message": "No diagnostic publication received within 15 seconds; this is not a clean result."}),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let (method, params) = request(input, &uri)?;
    Ok(
        json!({"operation": input.operation, "file_path": input.file_path, "result": client.send_request(method, params).await?}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_requests_preserve_utf16_positions_and_reference_context() {
        let input = Input {
            operation: "references".into(),
            file_path: "/tmp/a.rs".into(),
            content: String::new(),
            line: 2,
            character: 5,
        };
        let (method, params) = request(&input, "file:///tmp/a.rs").unwrap();
        assert_eq!(method, "textDocument/references");
        assert_eq!(params["position"], json!({"line": 2, "character": 5}));
        assert_eq!(params["context"]["includeDeclaration"], true);
        assert!(
            request(&Input { operation: "execute".into(), ..input }, "file:///tmp/a.rs").is_err()
        );
    }
}
