//! Bridging MCP server tools into the shared [`Tool`] trait.

use crate::client::McpClient;
use crate::error::McpError;
use concerto_api_types::extension::McpToolDescriptor;
use concerto_core::error::ToolError;
use concerto_core::traits::policy::PolicyEngine;
use concerto_core::traits::tool::Tool;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::CancellationToken;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Hard ceiling applied to every MCP tool call, in seconds. The bridge clamps
/// `min(input timeout_secs or default, 300)` so a single remote call can
/// never run unbounded.
pub const HARD_TIMEOUT_CAP_SECS: u64 = 300;

/// Per-call timeout used when neither the call input nor the tool
/// configuration specifies one.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// One MCP server tool exposed through the shared `Tool` trait.
///
/// Tool names are namespaced as `mcp:<server_id>:<tool_name>` (ADR-43,
/// decision 2). The client is shared behind an `Arc<Mutex<McpClient>>` so the
/// Task 6 manager can hold one live client per server and hand out a
/// [`McpTool`] per remote tool. Policy is handled upstream by the
/// `ToolExecutor`; MCP servers themselves are trusted child processes (process
/// isolation + policy gating, ADR-43 decision 7).
#[derive(Clone)]
pub struct McpTool {
    server_id: String,
    /// Fully namespaced tool name: `mcp:<server_id>:<tool_name>`.
    tool_name: String,
    client: Arc<Mutex<McpClient>>,
    tool: McpToolDescriptor,
    default_timeout_secs: u64,
}

impl McpTool {
    /// Build a bridge for one remote tool on `client`'s server.
    pub fn new(server_id: String, client: Arc<Mutex<McpClient>>, tool: McpToolDescriptor) -> Self {
        let tool_name = format!("mcp:{server_id}:{}", tool.name);
        Self { server_id, tool_name, client, tool, default_timeout_secs: DEFAULT_TIMEOUT_SECS }
    }

    /// Override the fallback timeout used when a call supplies no
    /// `timeout_secs`. Builder-style.
    pub fn with_default_timeout_secs(mut self, secs: u64) -> Self {
        self.default_timeout_secs = secs;
        self
    }

    /// The namespaced tool name (`mcp:<server_id>:<tool_name>`).
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// The unqualified server-reported descriptor.
    pub fn descriptor(&self) -> &McpToolDescriptor {
        &self.tool
    }

    /// The namespaced server id this tool belongs to.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
}

impl fmt::Debug for McpTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTool")
            .field("tool_name", &self.tool_name)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        self.tool.description.as_deref().unwrap_or_default()
    }

    fn input_schema(&self) -> Value {
        self.tool.input_schema.clone()
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // The remote tool's capabilities are opaque to the policy engine;
        // policy gating of MCP tool execution happens by tool name in the
        // Task 6 policy wiring.
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if !input.is_object() {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "mcp tool '{}' requires a JSON object of arguments",
                    self.tool_name
                ),
            });
        }
        // `timeout_secs` is a concerto-side reserved key (ADM: it is consumed
        // by the bridge and stripped before forwarding to the server so it
        // never collides with a server-defined argument).
        let requested_timeout =
            input.get("timeout_secs").and_then(Value::as_u64).unwrap_or(self.default_timeout_secs);
        let timeout_secs = requested_timeout.clamp(1, HARD_TIMEOUT_CAP_SECS);
        let mut arguments = input.clone();
        if let Some(map) = arguments.as_object_mut() {
            map.remove("timeout_secs");
        }

        let call = {
            let mut client = self.client.lock().await;
            client.call_tool(&self.tool.name, arguments, timeout_secs, cancel.clone()).await
        };
        match call {
            Err(McpError::Cancelled) => Err(ToolError::Cancelled),
            Err(McpError::Timeout { .. }) => Err(ToolError::Timeout { timeout_secs }),
            Err(e) => Err(ToolError::ExecutionFailed {
                message: format!(
                    "mcp server '{}' tool '{}' failed: {e}",
                    self.server_id, self.tool.name
                ),
            }),
            Ok(call) if call.is_error => {
                // Recoverable tool-level failure surfaced to the model as an
                // execution error carrying the server's output.
                let text = call.text();
                let message = if text.is_empty() {
                    format!("mcp tool '{}' reported failure with no output", self.tool_name)
                } else {
                    text
                };
                Err(ToolError::ExecutionFailed { message })
            }
            Ok(call) => Ok(ToolOutput { summary: call.text(), data: call.raw }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use serde_json::json;
    use std::path::PathBuf;

    struct NoopAudit;
    #[async_trait::async_trait]
    impl AuditLog for NoopAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    fn descriptor(name: &str) -> McpToolDescriptor {
        McpToolDescriptor {
            name: name.to_string(),
            description: Some("A fixture tool".to_string()),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn harness() -> (Arc<Mutex<McpClient>>, SimplePolicyEngine, SessionContext) {
        let client = Arc::new(Mutex::new(McpClient::new("fixture")));
        let policy = SimplePolicyEngine::new(vec![], Arc::new(NoopAudit));
        let session = SessionContext::new(Ulid::new(), PathBuf::from("/tmp/test-project"));
        (client, policy, session)
    }

    #[test]
    fn namespaced_name_and_identity() {
        let (client, _, session) = harness();
        let tool = McpTool::new("srv".into(), client, descriptor("echo"));
        assert_eq!(tool.name(), "mcp:srv:echo");
        assert_eq!(tool.tool_name(), "mcp:srv:echo");
        assert_eq!(tool.server_id(), "srv");
        assert_eq!(tool.description(), "A fixture tool");
        assert_eq!(tool.capability_requirements(), CapabilitySet::default());
        assert!(!tool.rollback_support());
        assert!(tool.command_facts(&json!({}), &session).is_none());
    }

    #[test]
    fn missing_description_defaults_to_empty() {
        let (client, _, _) = harness();
        let mut tool = descriptor("echo");
        tool.description = None;
        let tool = McpTool::new("srv".into(), client, tool);
        assert_eq!(tool.description(), "");
    }

    #[tokio::test]
    async fn pre_cancelled_token_short_circuits_without_client() {
        let (client, policy, session) = harness();
        let tool = McpTool::new("fixture".into(), client, descriptor("echo"));
        let token = CancellationToken::new();
        token.cancel();
        let err = tool
            .execute(json!({ "text": "x" }), &policy, &session, token)
            .await
            .expect_err("must fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn non_object_input_is_rejected() {
        let (client, policy, session) = harness();
        let tool = McpTool::new("fixture".into(), client, descriptor("echo"));
        let err = tool
            .execute(json!([1, 2]), &policy, &session, CancellationToken::new())
            .await
            .expect_err("must fail");
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }
}
