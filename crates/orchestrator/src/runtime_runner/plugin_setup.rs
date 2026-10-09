//! Plugin & tool-setup cluster for `runtime_runner_impl`.
//!
//! This module owns the run's plugin/tool bootstrap: WASM plugin host,
//! capability-store, and manager construction (`build_plugin_manager`), the
//! per-candidate plugin loader plus the shared-handle wrapper
//! (`load_discovered_plugins` / `load_and_configure_plugins`), the tool
//! registry assembly (`build_tool_registry`), provider/model resolution
//! (`resolve_provider_and_model` over the parent's `resolve_provider` /
//! `resolve_model_id`), and the ADR-66 §3 tool-support predicates
//! (`tool_support_override` / `advertised_tool_support`). The cluster moves
//! verbatim from `runtime_runner.rs` (NORM S21c): no behavior, signature, or
//! call-site change.
//!
//! Stability contract: every item is `pub(crate)` and re-exported by the
//! parent's `pub(crate) use plugin_setup::*;`, so all call sites stay
//! byte-identical — including `memory_bootstrap`'s
//! `use super::{resolve_provider_and_model, ...}` and the ADR-66 precedence
//! test `tool_support_override_reads_the_resolved_config_override` in
//! `runtime_runner::runtime_runner_tests`. This file is loaded via the
//! parent's explicit `#[path = "runtime_runner/plugin_setup.rs"]`, mirroring
//! `runtime_runner/recorders.rs` and `runtime_runner/memory_bootstrap.rs`
//! (the parent itself is loaded via `#[path]` as `runtime_runner_impl`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use concerto_config::AppConfig;
use concerto_core::event::EventBus;
use concerto_core::traits::policy::AuditLog;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::ToolRegistry;
use concerto_core::OrchestratorError;

use concerto_lsp::tools::*;

use concerto_plugins::capability::CapabilityManager;
use concerto_plugins::discovery::DiscoveryConfig as PluginDiscoveryCfg;
use concerto_plugins::host::PluginHost;
use concerto_plugins::manager::PluginManager;
use concerto_providers::factory::ProviderFactory;
use concerto_tools::filesystem::{FilesystemTool, WriteTool};
use concerto_tools::git::GitTool;
use concerto_tools::virtual_fs::VirtualFs;

use super::{resolve_model_id, resolve_provider};

/// Build a fresh WASM plugin host, capability store, and manager. Returns both
/// the host and the manager (the per-candidate loader in
/// [`load_discovered_plugins`] needs the host).
///
/// Starts the epoch ticker once — call this exactly once per host. Per-run
/// tickers on the same retained engine would compound epoch increments and
/// silently shorten the ~100 s interruption budget. The spawned ticker task
/// runs until the tokio runtime shuts down (dropping the returned value only
/// detaches it), matching the pre-existing lifecycle.
pub(crate) fn build_plugin_manager() -> Option<(Arc<PluginHost>, PluginManager)> {
    let Ok(host) = PluginHost::new() else {
        tracing::warn!("failed to create WASM plugin host — continuing without plugins");
        return None;
    };
    let host = Arc::new(host);

    // Start the epoch ticker for WASM interruption (belt-and-suspenders with
    // fuel). This runs in the background and periodically increments the
    // engine epoch, allowing long-running plugins to be interrupted after
    // EPOCH_DEADLINE ticks (~EPOCH_BUDGET_SECS of wall-clock time at the
    // configured interval).
    let _epoch_ticker = host.start_epoch_ticker(PluginHost::EPOCH_TICKER_INTERVAL_MS);

    let data_dir = concerto_sessions::app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".").join("concerto"))
        .join("plugins");
    let Ok(cap_mgr) = CapabilityManager::open(&data_dir) else {
        tracing::warn!("failed to open capability store — continuing without plugins");
        return None;
    };
    Some((host.clone(), PluginManager::new(host, cap_mgr, None, None)))
}

/// Load and initialise ONE discovered plugin candidate, registering its tools
/// into `registry`. Fail-soft per candidate: a read/load/initialise failure
/// logs a warning and returns, leaving the rest of the batch to run. The
/// `continue` arms of the pre-extraction loop body become `return`s here.
async fn load_single_plugin(
    manager: &mut PluginManager,
    candidate: &concerto_plugins::discovery::PluginCandidate,
    project_dir: &std::path::Path,
    registry: &mut ToolRegistry,
) {
    use concerto_plugins::capability::{DenyUnapproved, GrantedCapabilities};

    let read = concerto_plugins::loader::PluginLoader::read_wasm_bytes(&candidate.wasm_path);
    let wasm_bytes = match read {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                path = %candidate.wasm_path.display(),
                error = %e,
                "failed to read plugin WASM"
            );
            return;
        }
    };
    let loaded = match manager.load_plugin(&wasm_bytes, &candidate.wasm_path, &DenyUnapproved).await
    {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                path = %candidate.wasm_path.display(),
                error = %e,
                "failed to load plugin module"
            );
            return;
        }
    };

    let mut granted = GrantedCapabilities::new();
    granted.set_root(project_dir.to_path_buf());

    let plugin_id = loaded.manifest.id.clone();
    match manager.initialise_plugin(&loaded, granted).await {
        Ok(()) => {
            if let Err(e) = manager.register_tools(&plugin_id, registry) {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "failed to register plugin tools"
                );
            } else {
                tracing::info!(
                    plugin_id = %plugin_id,
                    "plugin loaded with approved grants"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                plugin_id = %plugin_id,
                error = %e,
                "failed to initialise plugin"
            );
        }
    }
}

/// Load every discovered `.wasm` into `manager` with approved, hash-pinned
/// grants rooted at `project_dir`, register their tools into `registry`, and
/// collect the plugin-backed providers for this run. Never fails a run:
/// discovery/load/init failures degrade to "no plugin providers".
pub(crate) async fn load_discovered_plugins(
    manager: &mut PluginManager,
    _host: Arc<PluginHost>,
    disc_cfg: PluginDiscoveryCfg,
    project_dir: &std::path::Path,
    registry: &mut ToolRegistry,
) -> HashMap<String, Arc<dyn LlmProvider>> {
    let mut plugin_providers = HashMap::new();
    let Ok(candidates) = manager.discover(disc_cfg) else {
        tracing::warn!("plugin discovery failed — continuing without plugins");
        return plugin_providers;
    };

    for candidate in &candidates {
        load_single_plugin(manager, candidate, project_dir, registry).await;
    }
    match manager.collect_providers().await {
        Ok(providers) => plugin_providers = providers,
        Err(error) => tracing::warn!(
            %error,
            "failed to collect plugin-backed providers"
        ),
    }
    plugin_providers
}

/// Load and initialise WASM plugins, registering their tools and collecting
/// provider instances. Errors are logged silently — a missing plugin host or
/// invalid WASM module never fails the agent run.
///
/// When the caller passes a [`SharedPluginManager`] handle (desktop), the
/// manager is materialised here on first use and reused across runs, matching
/// the pattern used for the shared memory wrapper. Without a handle (CLI,
/// tests) a per-run manager is built and dropped on return, preserving the
/// previous behaviour.
pub(crate) async fn load_and_configure_plugins(
    config: &AppConfig,
    project_dir: &std::path::Path,
    registry: &mut ToolRegistry,
    plugins: Option<&concerto_plugins::manager::SharedPluginManager>,
    bus: &EventBus,
    audit: Option<Arc<dyn AuditLog>>,
) -> (HashMap<String, Arc<dyn LlmProvider>>, Option<concerto_plugins::host::PluginHostContext>) {
    let plugin_providers = HashMap::new();
    let Some(ref plugin_cfg) = config.plugins else {
        return (plugin_providers, None);
    };
    if !plugin_cfg.enabled || !plugin_cfg.auto_load {
        if plugin_cfg.enabled {
            tracing::info!(
                "WASM plugins enabled but auto_load=false — no frontend loads them automatically"
            );
        }
        return (plugin_providers, None);
    }

    let context: concerto_plugins::host::PluginHostContext = Arc::new(std::sync::RwLock::new(None));
    let search_paths: Vec<std::path::PathBuf> = if plugin_cfg.search_paths.is_empty() {
        PluginDiscoveryCfg::default().search_paths
    } else {
        plugin_cfg.search_paths.iter().map(std::path::PathBuf::from).collect()
    };
    let bundled_path = if plugin_cfg.bundled_plugins_enabled {
        std::env::current_exe().ok().and_then(|path| path.parent().map(|dir| dir.join("plugins")))
    } else {
        None
    };
    let disc_cfg = PluginDiscoveryCfg { search_paths, bundled_path };

    // Desktop retained path: materialise the process-lifetime manager on the
    // first run (inside a tokio runtime context — the epoch ticker needs one)
    // and reuse it across runs, so the Settings UI's revoke/refresh operations
    // act on the same live plugin instances.
    if let Some(handle) = plugins {
        let mut guard = handle.lock().await;
        if guard.is_none() {
            *guard = build_plugin_manager();
        }
        let Some((host, manager)) = guard.as_mut() else {
            return (plugin_providers, None);
        };
        manager.prepare_run(context.clone()).await;
        manager.set_event_bus(bus.clone());
        if let Some(audit) = audit.clone() {
            manager.set_audit_log(audit);
        }
        let providers =
            load_discovered_plugins(manager, host.clone(), disc_cfg, project_dir, registry).await;
        return (providers, Some(context));
    }

    // No shared handle (CLI, tests): per-run manager, dropped on return.
    let Some((host, mut manager)) = build_plugin_manager() else {
        return (plugin_providers, None);
    };
    manager.prepare_run(context.clone()).await;
    manager.set_event_bus(bus.clone());
    if let Some(audit) = audit {
        manager.set_audit_log(audit);
    }
    let providers =
        load_discovered_plugins(&mut manager, host, disc_cfg, project_dir, registry).await;
    (providers, Some(context))
}

/// Build the tool registry with filesystem, shell, git, and LSP tools.
///
/// The filesystem tool is anchored to the project directory (not the CWD).
/// The shell tool uses the canonical selected profile when available.
pub(crate) fn build_tool_registry(
    project_dir: &std::path::Path,
    vfs: &Option<Arc<Mutex<VirtualFs>>>,
    config: &AppConfig,
) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    let cwd_path: camino::Utf8PathBuf = if project_dir.as_os_str().is_empty() {
        std::env::current_dir()
            .map(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref()))
            .unwrap_or_default()
    } else {
        camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf()).unwrap_or_default()
    };
    if let Some(vfs) = vfs {
        registry.register(Box::new(FilesystemTool::new_shared(cwd_path.clone(), vfs.clone())));
        // Claude-habit `write` alias: same VFS, canonical filesystem-write
        // policy coverage (see `WriteTool::policy_view`).
        registry.register(Box::new(WriteTool::new_shared(cwd_path.clone(), vfs.clone())));
    } else {
        registry.register(Box::new(FilesystemTool::new(cwd_path.clone())));
        registry.register(Box::new(WriteTool::new(cwd_path.clone())));
    }
    registry.register(Box::new(concerto_tools::native_process::NativeProcessTool::new(
        config.shell_security.clone(),
    )));
    registry.register(Box::new(GitTool));

    // LSP tools — unconditional registration; each tool lazily starts the LSP
    // server on first use. If the server is not installed the tool returns a
    // recoverable error at call time.
    registry.register(Box::new(GetHover));
    registry.register(Box::new(FindReferences));
    registry.register(Box::new(RenameSymbol));
    registry.register(Box::new(GetDiagnostics));
    registry.register(Box::new(GetSemanticTokens));
    registry.register(Box::new(GetCodeActions));
    registry.register(Box::new(ExecuteCodeAction));
    registry.register(Box::new(GetInlayHints));

    registry
}

/// Resolve both the provider and the effective model ID from configuration.
///
/// Delegates to the existing [`resolve_model_id`] and [`resolve_provider`]
/// functions. Returns both values so callers need not duplicate the resolution
/// logic.
/// A fully-resolved run model: the built provider, the effective model name,
/// and the offering provider-config id (`None` for plugin-backed and
/// env-fallback providers).
pub(crate) type ResolvedRunModel = (Arc<dyn LlmProvider>, String, Option<String>);

pub(crate) fn resolve_provider_and_model(
    config: &AppConfig,
    selected_provider_id: Option<String>,
    selected_model: Option<String>,
    plugin_providers: &HashMap<String, Arc<dyn LlmProvider>>,
) -> Result<ResolvedRunModel, OrchestratorError> {
    let model =
        resolve_model_id(config, selected_provider_id.as_deref(), selected_model.as_deref());
    let (provider, provider_config_id) = resolve_provider(
        config,
        selected_provider_id,
        selected_model.as_deref(),
        plugin_providers,
    )?;
    Ok((provider, model, provider_config_id))
}

/// The `supports_tool_calling` explicit-config override for the resolved
/// provider configuration, if one is set (ADR-66 §3 precedence level 1).
///
/// `provider_config_id` is `None` for plugin-backed and env-fallback
/// providers, which have no `[model_profiles.<id>]` entry to consult.
pub(crate) fn tool_support_override(
    config: &AppConfig,
    provider_config_id: Option<&str>,
) -> Option<bool> {
    let settings = config.model_settings.as_ref()?;
    let id = provider_config_id?;
    settings
        .model_profile_overrides
        .get(id)
        .and_then(|override_config| override_config.supports_tool_calling)
}

/// The provider-advertised tool-calling capability for the resolved provider
/// configuration/model, when model discovery published one (ADR-66 §3
/// precedence level 2 / ADR-75).
///
/// `provider_config_id` is `None` for plugin-backed and env-fallback
/// providers; a provider that advertised no per-model flag yields `None`,
/// leaving the optimistic default.
pub(crate) fn advertised_tool_support(
    config: &AppConfig,
    provider_config_id: Option<&str>,
    model: &str,
) -> Option<bool> {
    let settings = config.model_settings.as_ref()?;
    let id = provider_config_id?;
    settings
        .providers
        .iter()
        .find(|provider| ProviderFactory::config_id(provider) == id)
        .and_then(|provider| provider.advertised_tool_support_for(model))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-candidate loader is fail-soft: a candidate whose WASM file is
    /// missing logs and returns without touching the manager or registry, so
    /// the rest of the discovery batch still runs.
    #[tokio::test]
    async fn load_single_plugin_missing_wasm_is_fail_soft() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_mgr = CapabilityManager::open(dir.path()).expect("capability manager");
        let host = Arc::new(PluginHost::new().expect("plugin host"));
        let mut manager = PluginManager::new(host, cap_mgr, None, None);
        let candidate = concerto_plugins::discovery::PluginCandidate {
            wasm_path: dir.path().join("absent.wasm"),
            sidecar_manifest_path: None,
        };
        let mut registry = ToolRegistry::default();

        load_single_plugin(&mut manager, &candidate, dir.path(), &mut registry).await;

        assert!(
            registry.all_tool_definitions().is_empty(),
            "fail-soft path must not register tools"
        );
    }
}
