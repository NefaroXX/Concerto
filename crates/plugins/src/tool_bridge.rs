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
    /// Stable plugin namespace, independent of builtin registration order.
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
            registered_name: format!("plugin:{plugin_id}:{tool_name}"),
            plugin_id,
            plugin,
            tool_name,
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
        let mut plugin = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            plugin = self.plugin.lock() => plugin,
        };
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

/// Stable names keep policy and audit identity independent of discovery order.
fn registered_name_for(
    tool_name: &str,
    plugin_id: &str,
    _registry: &concerto_core::types::ToolRegistry,
) -> String {
    format!("plugin:{plugin_id}:{tool_name}")
}

/// Register all tools from an ActivePlugin into the given ToolRegistry.
///
/// Every tool uses the stable `plugin:<plugin_id>:<tool>` identity.
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
    use concerto_core::ids::Ulid;
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
                "plugin:squat:weather".to_string(),
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
                "plugin:squat:weather".to_string(),
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
        assert_eq!(registered, vec!["plugin:calm:weather".to_string()]);
        assert!(registry.get("git").is_some() && registry.get("plugin:calm:weather").is_some());
    }

    /// Observe-class inspector names (core authorization `is_observe`: `read`,
    /// `search`, `inspect`, `diagnose`, `diagnostics`) are reserved too: a
    /// plugin declaring `read` under its friendly name would inherit the
    /// name-keyed free Observe-Allow with no grant (2026-09-11 hardening).
    /// The namespaced form must land and must classify like any unknown tool
    /// — never free Observe — while a legit free name stays verbatim.
    #[tokio::test]
    async fn observe_class_names_are_namespaced_and_get_no_free_observe() {
        let mut registry = concerto_core::types::ToolRegistry::default();
        let registered = register_plugin_tools(
            "squat",
            active_plugin(&["read", "weather"]).await,
            &[descriptor("read"), descriptor("weather")],
            &mut registry,
        );
        assert_eq!(
            registered,
            vec!["plugin:squat:read".to_string(), "plugin:squat:weather".to_string()],
            "observe-class squat name must be namespaced; free names keep the friendly name"
        );
        assert!(
            registry.get("read").is_none() && registry.get("plugin:squat:read").is_some(),
            "the friendly observe-class name must not be registered"
        );

        // The namespaced form does not inherit the observe-class name's free
        // Observe: the tier classifier treats it like any unknown tool.
        let input = serde_json::json!({});
        let action = concerto_core::types::PolicyAction {
            tool_name: "plugin:squat:read",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
            orchestrator_authority: false,
            path_facts: None,
        };
        assert_eq!(
            concerto_core::classify_tier(&action),
            concerto_core::IntentTier::MutateLocal,
            "namespaced plugin tool must not classify as Observe"
        );
    }

    /// F2 (2026-09-14): a declaration whose name literally impersonates a
    /// namespace (`plugin:<other-id>:…` or `mcp:<server-id>:…`) must not
    /// register verbatim — it would inherit any prefix-keyed policy treatment
    /// of that namespace or collide with another registration's namespaced
    /// form. Such declarations are forced under the declaring plugin's own
    /// namespace, while free names stay verbatim.
    #[tokio::test]
    async fn namespace_claiming_declarations_are_forced_under_plugin_namespace() {
        let mut registry = concerto_core::types::ToolRegistry::default();
        let registered = register_plugin_tools(
            "squat",
            active_plugin(&["mcp:srv:echo", "plugin:other:read", "weather"]).await,
            &[descriptor("mcp:srv:echo"), descriptor("plugin:other:read"), descriptor("weather")],
            &mut registry,
        );
        assert_eq!(
            registered,
            vec![
                "plugin:squat:mcp:srv:echo".to_string(),
                "plugin:squat:plugin:other:read".to_string(),
                "plugin:squat:weather".to_string(),
            ],
            "namespace-claiming names must be force-namespaced; free names keep the friendly name"
        );
        // Neither impersonated form is reachable under its verbatim name.
        assert!(
            registry.get("mcp:srv:echo").is_none() && registry.get("plugin:other:read").is_none(),
            "the impersonated namespace names must not be registered verbatim"
        );
        // The forced names carry the declaring plugin's own id: a rule keyed
        // on `mcp:` / another plugin's `plugin:<other-id>:` prefix can never
        // match them.
        assert!(
            registry.get("plugin:squat:mcp:srv:echo").is_some()
                && registry.get("plugin:squat:plugin:other:read").is_some(),
            "the forced-namespace registrations must be reachable under the declaring plugin's id"
        );

        // The forced names classify like any unknown tool — the `mcp:srv:echo`
        // impersonation must not drag an `mcp:`-keyed free pass along (the
        // classifier has no `mcp:` grant; the tier stays MutateLocal).
        let input = serde_json::json!({});
        let action = concerto_core::types::PolicyAction {
            tool_name: "plugin:squat:mcp:srv:echo",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
            orchestrator_authority: false,
            path_facts: None,
        };
        assert_eq!(
            concerto_core::classify_tier(&action),
            concerto_core::IntentTier::MutateLocal,
            "forced-namespace name must not classify as Observe"
        );
    }
}
