use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::ToolError;
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::CancellationToken;
use tokio::sync::Mutex;

use crate::active_plugin::ActivePlugin;
use crate::error::PluginError;
use crate::host_fns::COMPLETION_CANCELLED;

/// Wraps an ActivePlugin's call_tool export as a `dyn Tool`.
pub struct PluginTool {
    plugin_id: String,
    plugin: Arc<Mutex<ActivePlugin>>,
    tool_name: String,
    tool_description: String,
    /// JSON Schema for this tool's input parameters (from plugin manifest).
    input_schema: serde_json::Value,
    /// Snapshot of the plugin's declared capabilities.
    manifest_capabilities: CapabilitySet,
}

impl PluginTool {
    pub fn new(
        plugin_id: String,
        plugin: Arc<Mutex<ActivePlugin>>,
        tool_name: String,
        tool_description: String,
        input_schema: serde_json::Value,
        manifest_capabilities: CapabilitySet,
    ) -> Self {
        Self { plugin_id, plugin, tool_name, tool_description, input_schema, manifest_capabilities }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn capability_requirements(&self) -> CapabilitySet {
        self.manifest_capabilities.clone()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut plugin = self.plugin.lock().await;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // Thread the caller's cancellation token into the plugin store so
        // in-flight async host calls (e.g. `concerto.completion`) observe
        // agent/tool-call cancellation (ADR-38).
        plugin.set_cancel(Some(cancel.clone()));
        let result_json = plugin.call_tool(&self.tool_name, &input).await.map_err(|e| {
            // M4: a cancelled in-flight host call (e.g. the wasm plugin called
            // `concerto.completion` and its token was cancelled mid-flight)
            // surfaces as a RESULT_ERROR whose `last_error` is the host's
            // distinguishable cancellation marker. Map that — or any error
            // raised after the caller's token fired — to the canonical
            // `ToolError::Cancelled` instead of a generic execution failure.
            let cancelled = cancel.is_cancelled()
                || matches!(&e, PluginError::ToolCallFailed(m) if m == COMPLETION_CANCELLED);
            if cancelled {
                ToolError::Cancelled
            } else {
                ToolError::ExecutionFailed {
                    message: format!(
                        "plugin '{}' tool '{}' failed: {e}",
                        self.plugin_id, self.tool_name
                    ),
                }
            }
        })?;
        Ok(ToolOutput {
            summary: serde_json::to_string(&result_json).unwrap_or_default(),
            data: result_json,
        })
    }
}

impl std::fmt::Debug for PluginTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTool")
            .field("plugin_id", &self.plugin_id)
            .field("tool_name", &self.tool_name)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helper to register plugin tools into a ToolRegistry
// ---------------------------------------------------------------------------

/// Register all tools from an ActivePlugin into the given ToolRegistry.
pub fn register_plugin_tools(
    plugin_id: &str,
    plugin: Arc<Mutex<ActivePlugin>>,
    tools: &[concerto_api_types::plugin::ToolDescriptor],
    registry: &mut concerto_core::types::ToolRegistry,
) {
    for desc in tools {
        let cap_set = CapabilitySet::default(); // System-level policy; host functions enforce plugin caps
        let tool = PluginTool::new(
            plugin_id.to_string(),
            plugin.clone(),
            desc.name.clone(),
            desc.description.clone(),
            desc.input_schema.clone(),
            cap_set,
        );
        registry.register(Box::new(tool));
    }
}

/// Remove all tools belonging to a plugin from the registry.
pub fn unregister_plugin_tools(
    _plugin_id: &str,
    registry: &mut concerto_core::types::ToolRegistry,
    known_tools: &[String],
) {
    for tool_name in known_tools {
        registry.remove(tool_name);
    }
}
