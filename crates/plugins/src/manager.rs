use crate::active_plugin::ActivePlugin;
use crate::capability::{CapabilityApprovalUI, CapabilityManager, GrantedCapabilities};
use crate::dialect_host::DialectHost;
use crate::discovery::{DiscoveryConfig, PluginCandidate, PluginDiscovery};
use crate::error::PluginError;
use crate::host::PluginHost;
use crate::loader::{LoadedPlugin, PluginLoader};
use crate::memory_adapter_host::PluginBackedVectorStore;
use crate::provider_host::PluginBackedProvider;
use crate::tool_bridge::{register_plugin_tools, unregister_plugin_tools};
use concerto_api_types::plugin::{PluginManifest, PluginProvides};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::VectorStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Status of a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginStatus {
    /// Plugin is active and ready for tool calls.
    Active,
    /// Plugin was disabled due to a fatal error or unauthorized host call.
    Disabled { reason: String },
}

/// Information about an active plugin.
#[derive(Debug)]
pub struct ActivePluginInfo {
    pub manifest: PluginManifest,
    pub status: PluginStatus,
    pub tool_names: Vec<String>,
}

/// Central plugin lifecycle manager.
pub struct PluginManager {
    loader: PluginLoader,
    capability_manager: CapabilityManager,
    /// Active plugins keyed by plugin ID.
    active: HashMap<String, Arc<Mutex<ActivePlugin>>>,
    /// Plugin status tracking.
    status: HashMap<String, PluginStatus>,
    /// Tool names registered per plugin (for unregistration).
    plugin_tools: HashMap<String, Vec<String>>,
    /// Violation counts per plugin.
    violations: HashMap<String, u32>,
}

impl PluginManager {
    /// Create a new PluginManager.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: Arc<PluginHost>,
        capability_manager: CapabilityManager,
        event_bus: Option<tokio::sync::broadcast::Sender<Arc<serde_json::Value>>>,
        provider: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        let mut loader = match event_bus {
            Some(bus) => PluginLoader::with_event_bus(host.clone(), bus),
            None => PluginLoader::new(host.clone()),
        };
        loader.set_provider(provider);
        Self {
            loader,
            capability_manager,
            active: HashMap::new(),
            status: HashMap::new(),
            plugin_tools: HashMap::new(),
            violations: HashMap::new(),
        }
    }

    /// Discover plugins in the given directories.
    pub fn discover(&self, config: DiscoveryConfig) -> Result<Vec<PluginCandidate>, PluginError> {
        let discovery = PluginDiscovery::new(config);
        discovery.discover()
    }

    /// Load a plugin from WASM bytes and request capability approval.
    ///
    /// Returns the loaded plugin if all required capabilities are approved.
    /// If the plugin requires capabilities, the `approval_ui` is consulted.
    /// The SHA-256 hash of the WASM binary is passed to the capability manager
    /// for manifest hash pinning (ADR-37).
    pub async fn load_plugin(
        &mut self,
        wasm_bytes: &[u8],
        source: &std::path::Path,
        approval_ui: &dyn CapabilityApprovalUI,
    ) -> Result<LoadedPlugin, PluginError> {
        let loaded = self.loader.load_from_bytes(wasm_bytes, source).await?;
        let manifest_hash = if loaded.manifest.capabilities_required.is_empty() {
            None
        } else {
            Some(crate::capability::sha256_hex(wasm_bytes))
        };

        // Request capability approval if needed.
        if !loaded.manifest.capabilities_required.is_empty() {
            let decisions = self
                .capability_manager
                .request_approval(
                    &loaded.manifest,
                    &loaded.manifest.capabilities_required,
                    approval_ui,
                    manifest_hash,
                )
                .await?;

            // Check if all required capabilities were granted.
            let all_granted = decisions.iter().all(|d| {
                matches!(
                    d,
                    crate::capability::GrantDecision::Granted
                        | crate::capability::GrantDecision::GrantedPersistent
                )
            });
            if !all_granted {
                return Err(PluginError::CapabilityDenied(
                    "one or more required capabilities were denied".into(),
                ));
            }
        }

        Ok(loaded)
    }

    /// Initialize a loaded plugin with granted capabilities.
    ///
    /// Async (ADR-38): the underlying loader runs instantiation and the `init`
    /// export call on an async wasmtime store.
    pub async fn initialise_plugin(
        &mut self,
        loaded: &LoadedPlugin,
        granted_caps: GrantedCapabilities,
    ) -> Result<(), PluginError> {
        let active = self.loader.initialise(loaded, granted_caps).await?;
        let plugin_id = active.manifest.id.clone();

        // Track active plugin wrapped in Arc<Mutex> for tool registration.
        let active = Arc::new(Mutex::new(active));
        self.active.insert(plugin_id.clone(), active);
        self.status.insert(plugin_id, PluginStatus::Active);

        Ok(())
    }

    /// Register plugin tools into the given registry.
    ///
    /// Tool names are prefixed with `plugin:<plugin_id>:` if a conflict exists
    /// with existing tools, otherwise the friendly name is used.
    pub fn register_tools(
        &mut self,
        plugin_id: &str,
        registry: &mut concerto_core::types::ToolRegistry,
    ) -> Result<(), PluginError> {
        let plugin_arc = self
            .active
            .get(plugin_id)
            .ok_or_else(|| PluginError::NotActive { id: plugin_id.to_string() })?;

        // Extract tool descriptors from provides.
        let manifest = {
            let active = plugin_arc.blocking_lock();
            active.manifest.clone()
        };

        let tools: Vec<_> = manifest
            .provides
            .iter()
            .filter_map(|p| match p {
                PluginProvides::Tool(desc) => Some(desc.clone()),
                _ => None,
            })
            .collect();

        if tools.is_empty() {
            return Ok(());
        }

        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

        // Register tools into the registry.
        register_plugin_tools(plugin_id, plugin_arc.clone(), &tools, registry);

        self.plugin_tools.insert(plugin_id.to_string(), tool_names);

        Ok(())
    }

    /// Disable a plugin (e.g., after unauthorized host call).
    pub fn disable_plugin(&mut self, plugin_id: &str, reason: &str) {
        self.status
            .insert(plugin_id.to_string(), PluginStatus::Disabled { reason: reason.to_string() });
    }

    /// Check if a plugin is active.
    pub fn is_active(&self, plugin_id: &str) -> bool {
        self.status.get(plugin_id).map(|s| matches!(s, PluginStatus::Active)).unwrap_or(false)
    }

    /// Get info about an active plugin.
    pub fn get_plugin_info(&self, plugin_id: &str) -> Option<ActivePluginInfo> {
        let plugin_arc = self.active.get(plugin_id)?;
        let manifest = {
            let active = plugin_arc.blocking_lock();
            active.manifest.clone()
        };
        let status = self.status.get(plugin_id)?.clone();
        let tool_names = self.plugin_tools.get(plugin_id).cloned().unwrap_or_default();

        Some(ActivePluginInfo { manifest, status, tool_names })
    }

    /// List all loaded plugin IDs.
    pub fn list_plugins(&self) -> Vec<String> {
        self.active.keys().cloned().collect()
    }

    /// Unload a plugin and remove its tools from the registry.
    pub fn unload_plugin(
        &mut self,
        plugin_id: &str,
        registry: &mut concerto_core::types::ToolRegistry,
    ) -> Result<(), PluginError> {
        // Unregister tools.
        if let Some(tool_names) = self.plugin_tools.remove(plugin_id) {
            unregister_plugin_tools(plugin_id, registry, &tool_names);
        }

        // Remove from tracking.
        self.active.remove(plugin_id);
        self.status.remove(plugin_id);
        self.violations.remove(plugin_id);

        Ok(())
    }

    /// Clear a LIVE plugin's in-memory capability grants so subsequent
    /// host-function capability checks fail closed (fail-closed revocation).
    ///
    /// This only affects the running plugin's grant set; the persisted grants
    /// are removed by [`CapabilityManager::revoke_plugin`]. Call both for full
    /// revocation: this method signals the runtime, the capability manager
    /// deletes the persisted grants.
    pub async fn revoke_grants(&self, plugin_id: &str) -> Result<(), PluginError> {
        let plugin_arc = self
            .active
            .get(plugin_id)
            .ok_or_else(|| PluginError::NotActive { id: plugin_id.to_string() })?;
        let mut active = plugin_arc.lock().await;
        let caps = &mut active.store.data_mut().granted_caps;
        caps.session_grants.clear();
        caps.persistent_grants.clear();
        Ok(())
    }

    /// Record a capability violation for a plugin.
    pub fn record_violation(&mut self, plugin_id: &str) {
        let count = self.violations.entry(plugin_id.to_string()).or_insert(0);
        *count += 1;
    }

    /// Check if a plugin has exceeded the violation threshold and should be disabled.
    pub fn should_disable(&self, plugin_id: &str) -> bool {
        self.violations.get(plugin_id).copied().unwrap_or(0) >= PluginHost::MAX_VIOLATIONS
    }

    /// Get the violation count for a plugin.
    pub fn violation_count(&self, plugin_id: &str) -> u32 {
        self.violations.get(plugin_id).copied().unwrap_or(0)
    }

    /// List plugin IDs that declare a `Provider` descriptor.
    pub async fn list_providers(&self) -> Vec<(String, String)> {
        let mut results = Vec::new();
        for (id, arc) in &self.active {
            let active = arc.lock().await;
            if let Some(PluginProvides::Provider(desc)) =
                active.manifest.provides.iter().find(|p| matches!(p, PluginProvides::Provider(_)))
            {
                results.push((id.clone(), desc.name.clone()));
            }
        }
        results
    }

    /// List plugin IDs that declare a `MemoryAdapter` descriptor.
    pub async fn list_memory_adapters(&self) -> Vec<String> {
        let mut results = Vec::new();
        for (id, arc) in &self.active {
            let active = arc.lock().await;
            let has_adapter = active
                .manifest
                .provides
                .iter()
                .any(|p| matches!(p, PluginProvides::MemoryAdapter(_)));
            if has_adapter {
                results.push(id.clone());
            }
        }
        results
    }

    /// List plugin IDs that declare a `Dialect` descriptor (ADR-53).
    pub async fn list_dialects(&self) -> Vec<String> {
        let mut results = Vec::new();
        for (id, arc) in &self.active {
            let active = arc.lock().await;
            let has_dialect =
                active.manifest.provides.iter().any(|p| matches!(p, PluginProvides::Dialect(_)));
            if has_dialect {
                results.push(id.clone());
            }
        }
        results
    }

    /// Create an [`Arc<dyn LlmProvider>`] from a loaded plugin that provides
    /// a `Provider` descriptor.
    ///
    /// The plugin must be active and must declare `PluginProvides::Provider`
    /// in its manifest. Returns `PluginError::PluginNotFound` if the plugin
    /// does not exist or has no `Provider` descriptor.
    pub async fn create_provider(
        &self,
        plugin_id: &str,
    ) -> Result<Arc<dyn LlmProvider>, PluginError> {
        let plugin_arc = self
            .active
            .get(plugin_id)
            .ok_or_else(|| PluginError::NotActive { id: plugin_id.to_string() })?;

        let (model, provider_name, heartbeat_interval_secs) = {
            let active = plugin_arc.lock().await;
            let provider_desc = active
                .manifest
                .provides
                .iter()
                .find_map(|p| match p {
                    PluginProvides::Provider(desc) => Some(desc.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    PluginError::InvalidManifest(format!(
                        "plugin '{plugin_id}' does not provide a Provider descriptor"
                    ))
                })?;
            (provider_desc.model, provider_desc.name, provider_desc.heartbeat_interval_secs)
        };

        // Heartbeat interval (ADR-53 §4): emit a keepalive chunk on this cadence
        // while awaiting the plugin completion, if the manifest requests it.
        let heartbeat = heartbeat_interval_secs
            .map(|secs| Duration::from_secs(u64::from(secs)))
            .filter(|d| !d.is_zero());

        // If the same plugin also declares a `Dialect` descriptor (ADR-53), the
        // provider renders its request body through that dialect.
        let has_dialect = {
            let active = plugin_arc.lock().await;
            active.manifest.provides.iter().any(|p| matches!(p, PluginProvides::Dialect(_)))
        };

        let provider = if has_dialect {
            let dialect = Arc::new(DialectHost::new(plugin_arc.clone()));
            PluginBackedProvider::with_dialect(
                plugin_arc.clone(),
                &provider_name,
                model,
                dialect,
                heartbeat,
            )
        } else {
            PluginBackedProvider::with_heartbeat(
                plugin_arc.clone(),
                &provider_name,
                model,
                heartbeat,
            )
        };
        Ok(Arc::new(provider))
    }

    /// Collect all plugin-backed providers into a `HashMap` keyed by
    /// `"plugin:<plugin_id>"`.
    ///
    /// This is the primary integration point — call this after loading plugins
    /// and merge the result into the provider map from
    /// `ProviderFactory::build_all()`.
    ///
    /// ```ignore
    /// let mut providers = ProviderFactory::build_all(&settings, &creds)?;
    /// providers.extend(plugin_manager.collect_providers().await?);
    /// ```
    pub async fn collect_providers(
        &self,
    ) -> Result<HashMap<String, Arc<dyn LlmProvider>>, PluginError> {
        let mut map = HashMap::new();
        for (id, _name) in self.list_providers().await {
            let provider = self.create_provider(&id).await?;
            map.insert(format!("plugin:{id}"), provider);
        }
        Ok(map)
    }

    /// Create an [`Arc<dyn VectorStore>`] from a loaded plugin that provides
    /// a `MemoryAdapter` descriptor.
    ///
    /// The plugin must be active and must declare `PluginProvides::MemoryAdapter`
    /// in its manifest. Returns `PluginError::PluginNotFound` if the plugin
    /// does not exist or has no `MemoryAdapter` descriptor.
    pub async fn create_memory_adapter(
        &self,
        plugin_id: &str,
    ) -> Result<Arc<dyn VectorStore>, PluginError> {
        let plugin_arc = self
            .active
            .get(plugin_id)
            .ok_or_else(|| PluginError::NotActive { id: plugin_id.to_string() })?;

        // Verify the plugin actually declares a MemoryAdapter descriptor.
        {
            let active = plugin_arc.lock().await;
            let has_adapter = active
                .manifest
                .provides
                .iter()
                .any(|p| matches!(p, PluginProvides::MemoryAdapter(_)));
            if !has_adapter {
                return Err(PluginError::InvalidManifest(format!(
                    "plugin '{plugin_id}' does not provide a MemoryAdapter descriptor"
                )));
            }
        }

        let store = PluginBackedVectorStore::new(plugin_arc.clone());
        Ok(Arc::new(store))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{
        CapabilityApprovalUI, CapabilityDiscriminant, CapabilityManager, CapabilityScope,
        GrantDecision,
    };
    use crate::host::PluginHost;
    use std::sync::Arc;

    /// A freshly created `PluginManager` must have no active plugins.
    #[test]
    fn test_manager_new_has_no_active_plugins() {
        let dir = std::env::temp_dir().join("plugin_test_new_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mgr = PluginManager::new(host, cap_mgr, None, None);

        assert!(mgr.list_plugins().is_empty(), "new manager should have no plugins");
        assert!(!mgr.is_active("any-plugin"), "unknown plugin should not be active");
        assert_eq!(mgr.violation_count("any-plugin"), 0);
        assert!(!mgr.should_disable("any-plugin"));
        assert!(mgr.get_plugin_info("any-plugin").is_none());
    }

    /// `is_active` must return `false` for plugins that have not been loaded.
    #[test]
    fn test_manager_is_active_returns_false_for_unknown() {
        let dir = std::env::temp_dir().join("plugin_test_is_active");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mgr = PluginManager::new(host, cap_mgr, None, None);

        assert!(!mgr.is_active("nonexistent-plugin"));
        assert!(!mgr.is_active(""));
        assert!(!mgr.is_active("plugin-with-no-capabilities"));
    }

    /// Disabling a plugin via `disable_plugin` must update the status.
    #[tokio::test]
    async fn test_manager_disable_plugin_updates_status() {
        let dir = std::env::temp_dir().join("plugin_test_disable");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mut mgr = PluginManager::new(host, cap_mgr, None, None);

        // Check that disabling with no plugin loaded is a no-op (no panic).
        mgr.disable_plugin("non-existent", "never loaded");

        // Load a minimal plugin so we can test disable on an active plugin.
        // JSON: {"id":"disable-test","name":"Disable Test","version":"0.1.0","description":"Disable test","abi_version":1,"capabilities_required":[],"provides":[]}
        // Length: 147 bytes → stored at offset 256
        let wasm = wat::parse_str(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{\"id\":\"disable-test\",\"name\":\"Disable Test\",\"version\":\"0.1.0\",\"description\":\"Disable test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 147)
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )
            "#,
        )
        .expect("WAT should parse");

        struct AutoApprove;
        #[async_trait::async_trait]
        impl CapabilityApprovalUI for AutoApprove {
            async fn request(
                &self,
                _plugin: &PluginManifest,
                capabilities: &[concerto_api_types::plugin::CapabilityRequest],
            ) -> Result<Vec<GrantDecision>, PluginError> {
                Ok(vec![GrantDecision::Granted; capabilities.len()])
            }
        }

        let loaded = mgr
            .load_plugin(&wasm, std::path::Path::new("disable_test.wasm"), &AutoApprove)
            .await
            .expect("load_plugin should succeed");

        let caps = GrantedCapabilities::new();
        mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

        // Plugin should be active now.
        assert!(mgr.is_active("disable-test"));

        // Disable it.
        mgr.disable_plugin("disable-test", "unauthorized host call");
        assert!(!mgr.is_active("disable-test"));

        // Verify the status map directly (avoid blocking_lock via get_plugin_info).
        // The plugin is still in the active map but status changed.
        assert!(mgr.list_plugins().contains(&"disable-test".to_string()));
    }

    /// `revoke_grants` must clear a LIVE plugin's in-memory grants (both
    /// session and persistent) so host-function checks fail closed, and must
    /// return `NotActive` for unknown plugins.
    #[tokio::test]
    async fn test_manager_revoke_grants_clears_live_grants() {
        let dir = std::env::temp_dir().join("plugin_test_revoke_grants");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mut mgr = PluginManager::new(host, cap_mgr, None, None);

        // JSON: {"id":"revoke-test","name":"Revoke Test","version":"0.1.0","description":"Revoke test","abi_version":1,"capabilities_required":[],"provides":[]}
        // Length: 144 bytes → stored at offset 256
        let wasm = wat::parse_str(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{\"id\":\"revoke-test\",\"name\":\"Revoke Test\",\"version\":\"0.1.0\",\"description\":\"Revoke test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 144)
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )
            "#,
        )
        .expect("WAT should parse");

        struct AutoApprove;
        #[async_trait::async_trait]
        impl CapabilityApprovalUI for AutoApprove {
            async fn request(
                &self,
                _plugin: &PluginManifest,
                capabilities: &[concerto_api_types::plugin::CapabilityRequest],
            ) -> Result<Vec<GrantDecision>, PluginError> {
                Ok(vec![GrantDecision::Granted; capabilities.len()])
            }
        }

        let loaded = mgr
            .load_plugin(&wasm, std::path::Path::new("revoke_test.wasm"), &AutoApprove)
            .await
            .expect("load_plugin should succeed");

        // Grant both a persistent capability (future expiry) and a session
        // capability so revocation must clear BOTH live grant maps.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut caps = GrantedCapabilities::with_persistent(
            "revoke-test",
            vec![(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default(), now + 3600)],
        );
        caps.grant_session(CapabilityDiscriminant::NetworkOutbound, CapabilityScope::default());
        mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

        // The grants must have reached the runtime store.
        {
            let plugin_arc = mgr.active.get("revoke-test").expect("plugin should be active");
            let active = plugin_arc.lock().await;
            let granted = &active.store.data().granted_caps;
            assert!(
                granted
                    .persistent_grants
                    .get("revoke-test")
                    .is_some_and(|m| m.contains_key(&CapabilityDiscriminant::FilesystemRead)),
                "persistent grant should be live in the runtime store"
            );
            assert!(
                granted.session_grants.contains_key(&CapabilityDiscriminant::NetworkOutbound),
                "session grant should be live in the runtime store"
            );
        }

        // Revoking grants for an unknown plugin fails closed.
        let err = mgr.revoke_grants("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, PluginError::NotActive { ref id } if id == "nonexistent"),
            "expected NotActive for unknown plugin, got: {err}"
        );

        // Revoking grants for the live plugin clears BOTH in-memory maps.
        mgr.revoke_grants("revoke-test").await.expect("revoke_grants should succeed");
        let plugin_arc = mgr.active.get("revoke-test").expect("plugin should still be active");
        let active = plugin_arc.lock().await;
        let granted = &active.store.data().granted_caps;
        assert!(granted.session_grants.is_empty(), "session grants must be cleared on revocation");
        assert!(
            granted.persistent_grants.is_empty(),
            "persistent grants must be cleared on revocation"
        );
    }

    /// Recording a violation must increment the count and `should_disable`
    /// must return `true` when the threshold is reached.
    #[test]
    fn test_manager_record_violation_tracks_count() {
        let dir = std::env::temp_dir().join("plugin_test_violation");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mut mgr = PluginManager::new(host, cap_mgr, None, None);

        let plugin_id = "violation-test";

        // Initial state.
        assert_eq!(mgr.violation_count(plugin_id), 0);
        assert!(!mgr.should_disable(plugin_id));

        // Record one violation (MAX_VIOLATIONS = 1).
        mgr.record_violation(plugin_id);
        assert_eq!(mgr.violation_count(plugin_id), 1);
        assert!(mgr.should_disable(plugin_id));

        // Additional violations still increment.
        mgr.record_violation(plugin_id);
        assert_eq!(mgr.violation_count(plugin_id), 2);
        assert!(mgr.should_disable(plugin_id));
    }
}
