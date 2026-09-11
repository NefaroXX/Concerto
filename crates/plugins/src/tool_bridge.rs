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
    /// Registry-facing name. Defaults to the friendly [`Self::tool_name`];
    /// namespaced (`plugin:<plugin_id>:<name>`) whenever the friendly name is
    /// already taken at registration time or is a reserved
    /// grant-sensitive/orchestration name, so a locally installed plugin can
    /// neither inherit the name-keyed grant treatment of a builtin tool nor
    /// squat the coordinator's dispatch tool.
    registered_name: String,
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
        Self {
            plugin_id,
            plugin,
            tool_name: tool_name.clone(),
            registered_name: tool_name,
            tool_description,
            input_schema,
            manifest_capabilities,
        }
    }

    /// Override the registry-facing name ([`Tool::name`]). The guest dispatch
    /// ([`Self::tool_name`], forwarded to the plugin's `call_tool` export) is
    /// untouched, so the plugin still receives the friendly name it declared.
    fn with_registered_name(mut self, registered_name: String) -> Self {
        self.registered_name = registered_name;
        self
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.registered_name
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

/// Tool names that are grant-sensitive or orchestration-owned and can never be
/// exposed under a plugin's own (unnamespaced) declaration:
///
/// - `call_specialist`: the coordinator's specialist-dispatch tool
///   (orchestrator `CALL_SPECIALIST_TOOL`) — a WASM plugin registering this
///   exact name would inherit the name-keyed treatment the registry/policy
///   grant that orchestration name;
/// - `filesystem` / `git` / `shell`: the tools crate's builtin tools — a
///   squatting declaration would inherit their grant treatment (and silently
///   overwrite the builtin in the registry, since `ToolRegistry::register`
///   replaces colliding entries).
const RESERVED_TOOL_NAMES: &[&str] = &["call_specialist", "filesystem", "git", "shell"];

/// Decide the registry-facing name for one plugin tool: the friendly name when
/// it is free and non-reserved, otherwise `plugin:<plugin_id>:<name>`.
///
/// Option chosen for the 2026-09-11 name-squat guard: **conditional
/// namespacing**, not a registry-level reserved-name guard. (a) The
/// `plugin:<id>:`-on-conflict behavior is the documented contract of
/// `PluginManager::register_tools` and mirrors the `mcp:<server_id>:<tool>`
/// namespacing precedent (ADR-43). (b) A hard reject in
/// `ToolRegistry::register` would cut through the same registration path the
/// tools crate legitimately uses for the `filesystem`/`git`/`shell` builtins,
/// turning a hardening knob into a core-registry invariant change. Here the
/// builtins stay untouched and plugin tool discovering its name taken simply
/// lands under its own namespace — the grant treatment keyed by the friendly
/// name can never be inherited.
fn registered_name_for(
    tool_name: &str,
    plugin_id: &str,
    registry: &concerto_core::types::ToolRegistry,
) -> String {
    let conflict = RESERVED_TOOL_NAMES.contains(&tool_name) || registry.get(tool_name).is_some();
    if conflict {
        format!("plugin:{plugin_id}:{tool_name}")
    } else {
        tool_name.to_string()
    }
}

/// Register all tools from an ActivePlugin into the given ToolRegistry.
///
/// Tool names are prefixed with `plugin:<plugin_id>:` if a conflict exists
/// with existing tools, otherwise the friendly name is used.
///
/// Returns the registry-facing names actually registered, so the caller's
/// bookkeeping (info display, unregistration) matches the registry keys.
pub fn register_plugin_tools(
    plugin_id: &str,
    plugin: Arc<Mutex<ActivePlugin>>,
    tools: &[concerto_api_types::plugin::ToolDescriptor],
    registry: &mut concerto_core::types::ToolRegistry,
) -> Vec<String> {
    let mut registered_names = Vec::with_capacity(tools.len());
    for desc in tools {
        let cap_set = CapabilitySet::default(); // System-level policy; host functions enforce plugin caps
        let registered = registered_name_for(&desc.name, plugin_id, registry);
        let tool = PluginTool::new(
            plugin_id.to_string(),
            plugin.clone(),
            desc.name.clone(),
            desc.description.clone(),
            desc.input_schema.clone(),
            cap_set,
        )
        .with_registered_name(registered.clone());
        registry.register(Box::new(tool));
        registered_names.push(registered);
    }
    registered_names
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

// ---------------------------------------------------------------------------
// Namespacing / name-squat guard tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::GrantedCapabilities;
    use concerto_api_types::plugin::ToolDescriptor;
    use std::path::Path;

    fn descriptor(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: "test tool".into(),
            input_schema: serde_json::json!({}),
        }
    }

    /// Named builtin stand-in: the real `filesystem`/`git`/`shell` builtins
    /// register under their friendly names through this same registration
    /// path, so a stub carrying the name is enough to observe squatting.
    struct BuiltinStub {
        name: String,
    }

    #[async_trait::async_trait]
    impl Tool for BuiltinStub {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "builtin stand-in"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Cancelled)
        }
    }

    fn registry_names(registry: &concerto_core::types::ToolRegistry) -> Vec<String> {
        let mut names: Vec<String> =
            registry.all_tool_definitions().into_iter().map(|d| d.name).collect();
        names.sort();
        names
    }

    fn squat_plugin_wasm(tool_names: &[&str]) -> Vec<u8> {
        let manifest = serde_json::json!({
            "id": "squat-test",
            "name": "Squat Test",
            "version": "0.1.0",
            "description": "Squat",
            "abi_version": 1,
            "capabilities_required": [],
            "provides": tool_names
                .iter()
                .map(|n| serde_json::json!({
                    "Tool": {
                        "name": n,
                        "description": n,
                        "input_schema": {}
                    }
                }))
                .collect::<Vec<_>>(),
        });
        let manifest_json = manifest.to_string();
        let escaped = manifest_json.replace('\\', "\\\\").replace('"', "\\\"");
        let wat = format!(
            r#"(module
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (data (i32.const 256) "{escaped}")
  (func (export "manifest") (result i64)
    (i64.or (i64.shl (i64.const 256) (i64.const 32)) (i64.const {}))
  )
  (func (export "init") (result i32) i32.const 0)
)"#,
            manifest_json.len()
        );
        wat::parse_str(&wat).expect("squat WAT should parse")
    }

    async fn active_plugin(tool_names: &[&str]) -> Arc<Mutex<ActivePlugin>> {
        let host = crate::host::PluginHost::new().expect("PluginHost should initialise");
        let loader = crate::loader::PluginLoader::new(Arc::new(host));
        let wasm = squat_plugin_wasm(tool_names);
        let loaded = loader.load_from_bytes(&wasm, Path::new("squat.wasm")).await.expect("load");
        let active =
            loader.initialise(&loaded, GrantedCapabilities::new()).await.expect("initialise");
        Arc::new(Mutex::new(active))
    }

    /// A locally installed plugin declaring grant-sensitive/orchestration
    /// names must land under `plugin:<id>:<name>`: it can never inherit the
    /// name-keyed grant treatment of the builtins or the coordinator's
    /// dispatch tool, and it must not overwrite a builtin registered under
    /// the same friendly name.
    #[tokio::test]
    async fn reserved_names_are_namespaced_not_squatted() {
        let mut registry = concerto_core::types::ToolRegistry::default();
        registry.register(Box::new(BuiltinStub { name: "filesystem".into() }));

        let registered = register_plugin_tools(
            "squat",
            active_plugin(&["call_specialist", "filesystem", "weather"]).await,
            &[descriptor("call_specialist"), descriptor("filesystem"), descriptor("weather")],
            &mut registry,
        );

        assert_eq!(
            registered,
            vec![
                "plugin:squat:call_specialist".to_string(),
                "plugin:squat:filesystem".to_string(),
                "weather".to_string(),
            ],
            "reserved names must be namespaced; free names keep the friendly name"
        );
        // The builtin survives the squat attempt (no silent overwrite).
        let names = registry_names(&registry);
        assert_eq!(
            names,
            vec![
                "filesystem".to_string(),
                "plugin:squat:call_specialist".to_string(),
                "plugin:squat:filesystem".to_string(),
                "weather".to_string(),
            ],
            "builtin filesystem must not be replaced by the plugin declaration"
        );
    }

    /// The builtins themselves register untouched: an empty reserved set and
    /// no prior registration keep the friendly names through the same path.
    #[tokio::test]
    async fn builtins_register_through_the_same_path_unchanged() {
        let mut registry = concerto_core::types::ToolRegistry::default();
        // `shell`/`git` are guarded names but the guard lives in the plugin
        // bridge only — a non-plugin Tool (the builtins) registers verbatim.
        registry.register(Box::new(BuiltinStub { name: "git".into() }));

        // Registering the SAME name twice from the builtins' own path still
        // overwrites (ToolRegistry::register contract) — but a plugin with a
        // free name keeps it verbatim, documented behavior.
        let registered = register_plugin_tools(
            "calm",
            active_plugin(&["weather"]).await,
            &[descriptor("weather")],
            &mut registry,
        );
        assert_eq!(registered, vec!["weather".to_string()]);
        assert!(registry.get("git").is_some() && registry.get("weather").is_some());
    }
}
