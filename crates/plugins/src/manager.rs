use crate::active_plugin::ActivePlugin;
use crate::capability::{
    CapabilityApprovalUI, CapabilityDiscriminant, CapabilityManager, GrantedCapabilities,
};
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

/// A process-lifetime, lazily-materialised plugin-manager handle shared
/// between the desktop App and the orchestrator runtime.
///
/// The manager (and its host, kept alive so the orchestrator can still build
/// per-candidate loaders) is materialised on the first agent run — the epoch
/// ticker in [`PluginHost::start_epoch_ticker`] needs a tokio runtime
/// context — after which it lives for the rest of the process so the Settings
/// UI's live revocation and provider re-discovery operate on the same plugin
/// instances an agent run uses. `None` inside the mutex means "not
/// materialised yet"; frontends without such a handle (CLI, tests) build a
/// per-run manager as before.
pub type SharedPluginManager = Arc<Mutex<Option<(Arc<PluginHost>, PluginManager)>>>;

/// Create a new shared plugin-manager handle (desktop).
pub fn new_shared_plugin_manager() -> SharedPluginManager {
    Arc::new(Mutex::new(None))
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
    /// Resolved persistent grants for plugins whose `load_plugin` skipped the
    /// approval prompt because persisted (hash-pinned) grants already covered
    /// every required capability (ADR-37 prompt-skip). Consumed by
    /// `initialise_plugin` and cleaned up on unload.
    resolved_grants: HashMap<String, GrantedCapabilities>,
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
            resolved_grants: HashMap::new(),
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
        // A fresh load always starts from a clean resolved-grant slate; the
        // prompt-skip path below re-populates it when applicable.
        self.resolved_grants.remove(&loaded.manifest.id);
        let manifest_hash = if loaded.manifest.capabilities_required.is_empty() {
            None
        } else {
            Some(crate::capability::sha256_hex(wasm_bytes))
        };

        // Request capability approval if needed.
        if !loaded.manifest.capabilities_required.is_empty() {
            // ADR-37 prompt-skip: persisted (hash-pinned) grants that still
            // cover every required capability mean the user already approved
            // this exact binary — load without re-prompting and resolve the
            // live grant set from the store for `initialise_plugin`.
            let persisted =
                self.capability_manager.load_grants(&loaded.manifest.id, manifest_hash.as_deref());
            let all_covered = loaded.manifest.capabilities_required.iter().all(|req| {
                let disc: CapabilityDiscriminant = req.into();
                persisted.iter().any(|(d, _, _)| *d == disc)
            });
            if all_covered {
                tracing::info!(
                    plugin_id = %loaded.manifest.id,
                    "persisted grants cover all required capabilities — skipping approval prompt"
                );
                self.resolved_grants.insert(
                    loaded.manifest.id.clone(),
                    GrantedCapabilities::with_persistent(&loaded.manifest.id, persisted),
                );
                return Ok(loaded);
            }

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
        // ADR-37 prompt-skip: when `load_plugin` resolved the grant set from
        // persisted grants without prompting, prefer those over the
        // (typically empty) caller-provided set. Consumed once so an
        // un-initialised plugin never lingers in the map.
        let effective_caps =
            self.resolved_grants.remove(&loaded.manifest.id).unwrap_or(granted_caps);
        let active = self.loader.initialise(loaded, effective_caps).await?;
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
    /// with existing tools (or the name is a reserved grant-sensitive /
    /// orchestration name: `call_specialist`, `filesystem`, `git`, `shell`),
    /// otherwise the friendly name is used.
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

        let registered_names =
            register_plugin_tools(plugin_id, plugin_arc.clone(), &tools, registry);

        self.plugin_tools.insert(plugin_id.to_string(), registered_names.clone());

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
        self.resolved_grants.remove(plugin_id);

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

    /// Best-effort live revocation (ADR-37): clears a loaded plugin's
    /// in-memory grants via [`revoke_grants`], tolerating `NotActive` (the
    /// plugin is not loaded in this manager) and logging unexpected failures.
    /// Returns `true` when a loaded plugin's grants were actually cleared.
    ///
    /// Shared helper for callers that cannot afford to fail the revocation
    /// workflow on `NotActive` — the CLI process and the desktop Settings UI
    /// both tolerate "plugin not loaded in this process".
    pub async fn revoke_grants_best_effort(&self, plugin_id: &str) -> bool {
        match self.revoke_grants(plugin_id).await {
            Ok(()) => {
                tracing::info!(plugin_id, "revoke_grants: live grants cleared");
                true
            }
            Err(PluginError::NotActive { .. }) => {
                tracing::debug!(
                    plugin_id,
                    "revoke_grants: plugin not active in this process (expected)"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    plugin_id,
                    error = %error,
                    "revoke_grants: failed to clear in-memory grants"
                );
                false
            }
        }
    }

    /// Re-run discovery and initialise any newly-appeared plugins that are not
    /// already active, so a settings-time "provider added" save surfaces new
    /// `.wasm` files to the retained manager without disturbing plugins a
    /// running agent holds.
    ///
    /// Already-active plugins are skipped. New plugins are initialised with the
    /// grants produced by `grant` — callers typically pass an empty set so
    /// host functions stay fail-closed until a run re-initialises the plugin
    /// with its run-scoped, auto-approved grant set. Tools are NOT registered
    /// here (there is no per-run registry at this point); the next agent run
    /// registers them. Returns the number of newly loaded plugins.
    pub async fn refresh_new_plugins(
        &mut self,
        config: DiscoveryConfig,
        mut grant: impl FnMut(&PluginManifest) -> GrantedCapabilities,
    ) -> Result<usize, PluginError> {
        let candidates = self.discover(config)?;
        let mut newly_loaded = 0;
        for candidate in &candidates {
            let wasm_bytes = match std::fs::read(&candidate.wasm_path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(
                        path = %candidate.wasm_path.display(),
                        error = %error,
                        "plugin refresh: failed to read plugin WASM"
                    );
                    continue;
                }
            };
            // The manifest id is only known after loading; a previously-loaded
            // plugin is then skipped so a refresh never displaces it.
            let loaded = match self.loader.load_from_bytes(&wasm_bytes, &candidate.wasm_path).await
            {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(
                        path = %candidate.wasm_path.display(),
                        error = %error,
                        "plugin refresh: failed to load plugin module"
                    );
                    continue;
                }
            };
            let plugin_id = loaded.manifest.id.clone();
            if self.active.contains_key(&plugin_id) {
                tracing::debug!(plugin_id, "plugin refresh: already active — skipping");
                continue;
            }
            match self.initialise_plugin(&loaded, grant(&loaded.manifest)).await {
                Ok(()) => {
                    tracing::info!(plugin_id, "plugin refresh: loaded newly discovered plugin");
                    newly_loaded += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        plugin_id,
                        error = %error,
                        "plugin refresh: failed to initialise new plugin"
                    );
                }
            }
        }
        Ok(newly_loaded)
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

    /// `revoke_grants_best_effort` must tolerate `NotActive` (unknown plugin)
    /// and clear a LIVE plugin's in-memory grants, reporting whether anything
    /// was actually cleared. This is the CLI/desktop shared helper, so the
    /// "expected" `NotActive` outcome must never surface as an error.
    #[tokio::test]
    async fn test_manager_revoke_grants_best_effort_tolerates_not_active() {
        let dir = std::env::temp_dir().join("plugin_test_revoke_best_effort");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mut mgr = PluginManager::new(host, cap_mgr, None, None);

        // Unknown plugin: `NotActive` must be tolerated, not surfaced.
        assert!(
            !mgr.revoke_grants_best_effort("no-such-plugin").await,
            "no live plugin -> nothing cleared"
        );

        // Build a minimal plugin with its manifest JSON carried as data, using
        // the runtime-computed byte length so the decl never drifts from the
        // actual JSON.
        let manifest = r#"{"id":"revoke-be-test","name":"Revoke Be Test","version":"0.1.0","description":"Revoke best-effort test","abi_version":1,"capabilities_required":[],"provides":[]}"#;
        let escaped = manifest.replace('"', "\\\"");
        let wasm_source = format!(
            r#"(module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{escaped}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const {})
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )"#,
            manifest.len()
        );
        let wasm = wat::parse_str(&wasm_source).expect("WAT should parse");

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
            .load_plugin(&wasm, std::path::Path::new("revoke_be_test.wasm"), &AutoApprove)
            .await
            .expect("load_plugin should succeed");

        let mut caps = GrantedCapabilities::new();
        caps.grant_session(CapabilityDiscriminant::FilesystemWrite, CapabilityScope::default());
        mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

        // Live plugin: grants must be cleared and the helper must report it.
        assert!(
            mgr.revoke_grants_best_effort("revoke-be-test").await,
            "live plugin grants must be cleared"
        );
        let plugin_arc = mgr.active.get("revoke-be-test").expect("plugin should still be active");
        let active = plugin_arc.lock().await;
        assert!(
            active.store.data().granted_caps.session_grants.is_empty(),
            "session grants must be cleared on best-effort revocation"
        );
    }

    /// `refresh_new_plugins` must initialise a newly-appeared `.wasm`, skip
    /// already-active plugins on a second pass, and apply the caller-provided
    /// grants (empty set — fail-closed until a run re-grants).
    #[tokio::test]
    async fn test_manager_refresh_new_plugins_loads_new_plugins_once() {
        let dir = std::env::temp_dir().join("plugin_test_refresh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
        let mut mgr = PluginManager::new(host, cap_mgr, None, None);

        // Build a minimal plugin with its manifest JSON carried as data, using
        // the runtime-computed byte length so the decl never drifts from the
        // actual JSON.
        let manifest = r#"{"id":"refresh-test","name":"Refresh Test","version":"0.1.0","description":"Refresh test","abi_version":1,"capabilities_required":[],"provides":[]}"#;
        let escaped = manifest.replace('"', "\\\"");
        let wasm_source = format!(
            r#"(module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{escaped}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const {})
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )"#,
            manifest.len()
        );
        let wasm = wat::parse_str(&wasm_source).expect("WAT should parse");
        std::fs::write(dir.join("refresh-test.wasm"), &wasm).expect("should write plugin wasm");

        let disc = DiscoveryConfig { search_paths: vec![dir.clone()], bundled_path: None };
        let count = mgr
            .refresh_new_plugins(disc, |_| GrantedCapabilities::new())
            .await
            .expect("refresh should succeed");
        assert_eq!(count, 1, "first refresh must load the new plugin");
        assert!(mgr.is_active("refresh-test"), "plugin must be active after refresh");

        // A second pass must not re-initialise an already-active plugin.
        let disc = DiscoveryConfig { search_paths: vec![dir.clone()], bundled_path: None };
        let count = mgr
            .refresh_new_plugins(disc, |_| GrantedCapabilities::new())
            .await
            .expect("second refresh should succeed");
        assert_eq!(count, 0, "already-active plugins must be skipped");
        assert!(mgr.is_active("refresh-test"));
    }
}
