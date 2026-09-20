use concerto_api_types::extension::{McpToolDescriptor, SkillManifest};
use concerto_config::managed::{IntegrityStatus, ManagedRuntimeManager};
use concerto_config::shell::{ProfileAvailability, ShellProfileConfig};
use concerto_config::McpServerConfig;
use concerto_core::CancellationToken;

/// Sensible default source path for the (adopt) install action (ADR-28 Slice 2).
#[cfg(windows)]
pub(crate) fn default_managed_source() -> String {
    "C:\\Program Files\\Git\\bin\\bash.exe".into()
}
#[cfg(not(windows))]
pub(crate) fn default_managed_source() -> String {
    "/bin/bash".into()
}

/// Run a best-effort availability check for a shell profile (ADR-28): resolve
/// the executable and invoke `--version`. Returns `(available, detail)`.
pub(crate) fn test_shell_profile(profile: Option<ShellProfileConfig>) -> (bool, String) {
    let profile = match profile {
        Some(p) => p,
        None => return (false, "no profile selected".into()),
    };
    match profile.availability() {
        ProfileAvailability::Available => match profile.version_string() {
            Some(version) => (true, format!("available — {version}")),
            None => (true, "available (version string unavailable)".into()),
        },
        ProfileAvailability::Unavailable(reason) => (false, reason),
        ProfileAvailability::Unknown => (true, "available (not checked)".into()),
        _ => (false, "unknown availability".into()),
    }
}

/// ADR-28 Slice 2 — Managed Bash runtime actions invoked from the Settings UI.
/// Each returns a human-readable result line; they run inside `Task::perform`
/// so the UI thread is never blocked. Install adopts a local Bash (offline);
/// the later, licensing-gated slice replaces this with a vetted-binary fetch.
pub(crate) fn managed_install(source: String) -> String {
    let path = std::path::PathBuf::from(source.trim());
    if !path.is_file() {
        return format!("Source not found: {}", path.display());
    }
    match ManagedRuntimeManager::for_data_dir() {
        Ok(mgr) => match mgr.install_from(&path) {
            Ok(m) => {
                format!("Installed managed Bash {} at {}", m.version, m.bash_executable.display())
            }
            Err(e) => format!("Install failed: {e}"),
        },
        Err(e) => format!("Cannot initialise runtime: {e}"),
    }
}

pub(crate) fn managed_remove() -> String {
    match ManagedRuntimeManager::for_data_dir() {
        Ok(mgr) => match mgr.remove() {
            Ok(()) => "Managed Bash removed.".into(),
            Err(e) => format!("Remove failed: {e}"),
        },
        Err(e) => format!("Cannot initialise runtime: {e}"),
    }
}

pub(crate) fn managed_verify() -> String {
    match ManagedRuntimeManager::auto_detect() {
        Some(m) => match ManagedRuntimeManager::for_data_dir() {
            Ok(mgr) => match mgr.verify(&m) {
                Ok(report) => {
                    let tools = report
                        .entries
                        .iter()
                        .map(|e| {
                            let s = match &e.status {
                                IntegrityStatus::Ok => "ok",
                                IntegrityStatus::Mismatch { .. } => "MISMATCH",
                                IntegrityStatus::Missing => "missing",
                                IntegrityStatus::Unknown => "unknown",
                                _ => "unknown",
                            };
                            format!("{}: {s}", e.name)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Integrity — runtime_ok={}, tools=[{tools}]", report.runtime_ok)
                }
                Err(e) => format!("Verify failed: {e}"),
            },
            Err(e) => format!("Cannot initialise runtime: {e}"),
        },
        None => "No managed Bash installed.".into(),
    }
}

pub(crate) fn managed_export(path: String) -> String {
    let dest = std::path::PathBuf::from(path.trim());
    match ManagedRuntimeManager::auto_detect() {
        Some(m) => match ManagedRuntimeManager::export_manifest(&m) {
            Ok(json) => match std::fs::write(&dest, json) {
                Ok(()) => format!("Exported manifest to {}", dest.display()),
                Err(e) => format!("Write failed: {e}"),
            },
            Err(e) => format!("Export failed: {e}"),
        },
        None => "No managed Bash installed.".into(),
    }
}

pub(crate) fn managed_import(path: String) -> String {
    let src = std::path::PathBuf::from(path.trim());
    let content = match std::fs::read_to_string(&src) {
        Ok(c) => c,
        Err(e) => return format!("Read failed: {e}"),
    };
    match ManagedRuntimeManager::import_manifest(&content) {
        Ok(m) => match ManagedRuntimeManager::for_data_dir() {
            Ok(mgr) => match ManagedRuntimeManager::export_manifest(&m) {
                Ok(json) => match std::fs::write(mgr.manifest_path(), json) {
                    Ok(()) => format!("Imported manifest for {}; now detected.", m.version),
                    Err(e) => format!("Failed to persist imported manifest: {e}"),
                },
                Err(e) => format!("Serialize failed: {e}"),
            },
            Err(e) => format!("Cannot initialise runtime: {e}"),
        },
        Err(e) => format!("Import failed: {e}"),
    }
}

/// ADR-43 — Discover skill packs under the configured search paths. Runs
/// inside `Task::perform` (wrapped in an async block) so the UI thread is
/// never blocked. Returns a discovery report — the found packs plus per-path
/// diagnostics (missing/invalid search paths, skipped packs) — or a
/// human-readable error.
pub(crate) fn discover_skills(
    search_paths: Vec<String>,
) -> Result<concerto_skills::DiscoveryReport, String> {
    let paths = search_paths.iter().map(std::path::PathBuf::from).collect();
    concerto_skills::SkillManager::new(paths).discover_with_report().map_err(|e| e.to_string())
}

/// Expand a raw configured parent path (`~` / `%VAR%`) to its absolute form,
/// falling back to the literal when expansion fails. Mirrors the display
/// helper in the settings view and the skills crate's own discovery expansion.
fn expanded_parent(raw: &str) -> std::path::PathBuf {
    concerto_skills::expanded_search_path(std::path::Path::new(raw))
        .unwrap_or_else(|| std::path::PathBuf::from(raw))
}

/// ADR-43 — Create a new `skill.toml` skill pack from the Settings wizard at
/// `parent/<id>`. The parent directory is created when missing; an existing
/// pack with the same id fails with an "already exists" error. Runs inside
/// `Task::perform` so the UI thread is never blocked. Returns a human-readable
/// outcome line, or an error message that keeps the wizard open.
pub(crate) fn create_skill_pack(
    raw_parent: String,
    id: String,
    name: String,
    version: String,
    description: String,
    instructions: String,
) -> Result<String, String> {
    let parent = expanded_parent(&raw_parent);
    let manager = concerto_skills::SkillManager::new(vec![parent.clone()]);
    let manifest = SkillManifest {
        id,
        name,
        version,
        description,
        instructions_path: None,
        instructions: Some(instructions),
        tools: Vec::new(),
        resources: Vec::new(),
    };
    match manager.create_pack(&parent, &manifest) {
        Ok(pack_dir) => Ok(format!("Created '{}' at {}", manifest.id, pack_dir.display())),
        Err(e) => Err(e.to_string()),
    }
}

/// ADR-43 — Rewrite a skill pack's `skill.toml` in place from the edit form.
/// The manifest `id` is forced to the pack directory name by the skills crate
/// (identity and path stay in sync); inline `instructions` always win over an
/// `instructions_path`. Returns a human-readable outcome line.
pub(crate) fn update_skill_pack(
    pack_dir: String,
    manifest: SkillManifest,
) -> Result<String, String> {
    let dir = std::path::PathBuf::from(pack_dir);
    concerto_skills::SkillManager::new(Vec::new())
        .update_pack(&dir, &manifest)
        .map_err(|e| e.to_string())?;
    Ok(format!("Saved '{}'", dir.display()))
}

/// ADR-43 — Delete a skill pack from the Settings UI. The manifest file(s)
/// are renamed to hidden `.deleted-<stamp>-skill.toml` / `-SKILL.md` backups
/// in place (reversible delete), so the pack disappears from discovery but the
/// committed files remain recoverable. Returns a human-readable outcome line.
pub(crate) fn delete_skill_pack(pack_dir: String) -> Result<String, String> {
    let dir = std::path::PathBuf::from(pack_dir);
    let backups = concerto_skills::SkillManager::new(Vec::new())
        .delete_pack(&dir)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Deleted '{}' — {} manifest file(s) moved to hidden backups",
        dir.display(),
        backups.len()
    ))
}

/// ADR-43 — Probe one MCP server end-to-end: spawn the stdio child, run the
/// `initialize` handshake, list its tools, then stop the server. Returns the
/// discovered tools, or a human-readable error. Runs inside `Task::perform`
/// so the UI thread is never blocked.
pub(crate) async fn probe_mcp_server(
    server: McpServerConfig,
) -> Result<Vec<McpToolDescriptor>, String> {
    let timeout = server.timeout_secs.unwrap_or(60);
    let env_pairs: Vec<(&str, &str)> = server
        .env
        .as_ref()
        .map(|map| map.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect())
        .unwrap_or_default();

    let mut client = concerto_mcp::McpClient::new(&server.id);
    if let Err(error) = client.spawn(&server.command, &server.args, &env_pairs).await {
        return Err(format!("could not start server: {error}"));
    }
    if let Err(error) = client.initialize(timeout).await {
        let _ = client.stop().await;
        return Err(format!("initialize failed: {error}"));
    }
    let tools = client.list_tools(timeout, CancellationToken::new()).await;
    // Always stop the child so the probe never orphans a server process.
    let _ = client.stop().await;
    tools.map_err(|error| format!("tools/list failed: {error}"))
}

/// ADR-37 — Revoke a plugin's capability grants from the Settings UI. First
/// removes the persisted grants via [`CapabilityManager::revoke_plugin`], then
/// signals a best-effort live clear through [`PluginManager::revoke_grants`]
/// so host-function checks fail closed while the plugin is still loaded.
///
/// `plugin_manager` is the desktop's process-lifetime manager handle (plugin
/// liveness): when present and already materialised by a run, the LIVE plugin
/// instance's in-memory grants are cleared. When it is absent (headless/tests)
/// or not yet materialised (no run yet — inner `None`), a fresh manager bound
/// to the same capability store is used and `NotActive` — the expected outcome
/// when the plugin is not loaded in this process — is tolerated and logged,
/// mirroring `concerto plugin revoke` in the CLI. Returns a human-readable
/// outcome line for the Plugins section.
pub(crate) async fn revoke_plugin_grants(
    plugin_id: String,
    plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>,
) -> Result<String, String> {
    let data_dir = concerto_plugins::capability::CapabilityManager::data_dir();
    let cap_mgr = concerto_plugins::capability::CapabilityManager::open(&data_dir)
        .map_err(|e| format!("could not open capability store: {e}"))?;
    cap_mgr.revoke_plugin(&plugin_id).map_err(|e| e.to_string())?;

    // Best-effort live revocation (ADR-37): clear the in-memory grant set of
    // a loaded plugin. The desktop process keeps a retained manager handle
    // since the runtime materialised it on the first run, so a live plugin's
    // grants are actually cleared here; `NotActive` is tolerated whenever the
    // plugin is not loaded and only unexpected failures are surfaced.
    match plugin_manager {
        Some(handle) => {
            let mut guard = handle.lock().await;
            match guard.as_mut().map(|(_, manager)| manager) {
                Some(manager) => {
                    manager.revoke_grants_best_effort(&plugin_id).await;
                }
                None => {
                    tracing::debug!(
                        plugin_id,
                        "revoke_grants: no materialised plugin manager (no run yet) — skipped"
                    );
                }
            }
        }
        None => {
            // Headless / tests: no retained handle — fall back to a fresh
            // best-effort manager against the same store. NotActive expected.
            if let Ok(host) = concerto_plugins::host::PluginHost::new() {
                let manager = concerto_plugins::manager::PluginManager::new(
                    std::sync::Arc::new(host),
                    cap_mgr,
                    None,
                    None,
                );
                manager.revoke_grants_best_effort(&plugin_id).await;
            } else {
                tracing::warn!("revoke_grants: could not construct plugin host — skipped");
            }
        }
    }

    Ok(format!("Revoked grants for '{plugin_id}'"))
}

/// ADR-37 — Open the native file dialog for a `.wasm` plugin. Returns `None`
/// when the user cancels; the picked path is the source for the install card.
pub(crate) async fn pick_plugin_file() -> Option<String> {
    rfd::AsyncFileDialog::new()
        .set_title("Select a WebAssembly plugin")
        .add_filter("WebAssembly plugin", &["wasm"])
        .pick_file()
        .await
        .map(|handle| handle.path().display().to_string())
}

/// ADR-37 — Re-scan the canonical plugins directory and fold each plugin's
/// persisted capability-grant summary into [`InstalledPluginInfo`].
///
/// Plugins that fail to load (e.g. a hand-dropped malformed module) surface as
/// unreadable entries carrying the error instead of failing the whole scan, so
/// they stay visible and deletable. The directory is always scanned, never the
/// configured `.wasm` list, so the tab reflects what will be discovered next
/// run.
pub(crate) async fn list_installed_plugins() -> Result<Vec<super::InstalledPluginInfo>, String> {
    let plugins_dir = concerto_plugins::discovery::plugins_dir();
    let config = concerto_plugins::discovery::DiscoveryConfig {
        search_paths: vec![plugins_dir.clone()],
        bundled_path: None,
    };
    let candidates = concerto_plugins::discovery::PluginDiscovery::new(config)
        .discover()
        .map_err(|e| e.to_string())?;
    let cap_mgr = concerto_plugins::capability::CapabilityManager::open(&plugins_dir)
        .map_err(|e| format!("could not open capability store: {e}"))?;

    let host = concerto_plugins::host::PluginHost::new()
        .map_err(|e| format!("could not construct plugin host: {e}"))?;
    let loader = concerto_plugins::loader::PluginLoader::new(std::sync::Arc::new(host));

    let mut installed: Vec<super::InstalledPluginInfo> = Vec::new();
    for candidate in candidates {
        let wasm_path = candidate.wasm_path.clone();
        let stem =
            wasm_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let (manifest, load_error) = match loader.load(&wasm_path).await {
            Ok(loaded) => (Some(loaded.manifest), None),
            Err(error) => {
                tracing::warn!(
                    path = %wasm_path.display(),
                    error = %error,
                    "plugin list: unreadable module stays visible"
                );
                (None, Some(error.to_string()))
            }
        };
        let id = manifest.as_ref().map(|m| m.id.clone()).unwrap_or_else(|| stem.clone());
        let grants = cap_mgr.load_grants(&id, None);
        let capability_summary =
            grants.iter().map(|(d, _, _)| format!("{d:?}")).collect::<Vec<_>>().join(", ");
        installed.push(super::InstalledPluginInfo {
            id,
            name: manifest.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| stem.clone()),
            version: manifest.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
            description: manifest
                .as_ref()
                .map(|m| m.description.clone())
                .unwrap_or_else(|| format!("Unreadable plugin at {}", wasm_path.display())),
            provides: manifest.as_ref().map(|m| provides_label(&m.provides)).unwrap_or_default(),
            capability_summary,
            wasm_path,
            load_error,
        });
    }
    installed.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(installed)
}

/// ADR-37 — Install (or replace) the plugin at `source`.
///
/// Pipeline: strict validation via [`PluginInstaller::validate`] (compiles the
/// module, extracts and checks the manifest, sidecar and ABI, and enforces the
/// size/memory caps) → capability approval through the desktop dialog
/// **before** any file is written → atomic write under the plugin id.
///
/// Replace semantics: when `<store_dir>/<id>.wasm` already exists, the stale
/// hash-pinned grants (ADR-37) are revoked and the previous live instance
/// unloaded before the new binary is prompted for and written. The retained
/// manager is then re-discovered with empty (fail-closed) grants so host
/// functions deny until a run initialises the plugin with its stored grants.
pub(crate) async fn install_plugin(
    source: std::path::PathBuf,
    store_dir: std::path::PathBuf,
    plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>,
    approval: Option<crate::services::plugin_approval::PluginApprovalService>,
) -> Result<String, String> {
    let host = concerto_plugins::host::PluginHost::new()
        .map_err(|e| format!("could not construct plugin host: {e}"))?;
    let installer = concerto_plugins::installer::PluginInstaller::new(std::sync::Arc::new(host));
    // Strict validation: refuses to install anything that would not load at
    // run time, oversized modules, or modules whose linear memory could exceed
    // the host cap.
    let validated =
        installer.validate(&source).await.map_err(|e| format!("Plugin rejected: {e}"))?;

    let id = validated.manifest.id.clone();
    let wasm_dest = store_dir.join(format!("{id}.wasm"));
    let replacing = wasm_dest.exists();
    let mut notes = Vec::new();

    if replacing {
        // The on-disk binary is about to change: hash-pinned grants (ADR-37)
        // can never match the new bytes, so clear persisted + live grants
        // before prompting for the replacement.
        match concerto_plugins::capability::CapabilityManager::open(&store_dir) {
            Ok(cap_mgr) => {
                if let Err(error) = cap_mgr.revoke_plugin(&id) {
                    notes.push(format!("stale grants not fully removed: {error}"));
                }
            }
            Err(error) => notes.push(format!("capability store unavailable: {error}")),
        }
        if let Some(handle) = &plugin_manager {
            let mut guard = handle.lock().await;
            if let Some(manager) = guard.as_mut().map(|(_, manager)| manager) {
                manager.revoke_grants_best_effort(&id).await;
            }
        }
    }

    // Capability approval — required capabilities are granted (or denied)
    // through the same dialog the runtime uses, before anything is written.
    if !validated.manifest.capabilities_required.is_empty() {
        let Some(approval) = approval else {
            return Err(
                "plugin requests capabilities but the approval bridge is unavailable".to_string()
            );
        };
        let cap_mgr = concerto_plugins::capability::CapabilityManager::open(&store_dir)
            .map_err(|e| format!("could not open capability store: {e}"))?;
        let decisions = cap_mgr
            .request_approval(
                &validated.manifest,
                &validated.manifest.capabilities_required,
                &approval,
                Some(validated.sha256.clone()),
            )
            .await
            .map_err(|e| e.to_string())?;
        if decisions
            .iter()
            .any(|decision| matches!(decision, concerto_plugins::capability::GrantDecision::Denied))
        {
            return Err(format!("capability approval declined for '{id}'; plugin not installed"));
        }
        let granted = decisions
            .iter()
            .filter(|decision| {
                matches!(decision, concerto_plugins::capability::GrantDecision::GrantedPersistent)
            })
            .count();
        notes.push(format!("{granted} capability grant(s) stored"));
    }

    let installed =
        installer.write(&validated, &store_dir).map_err(|e| format!("Install failed: {e}"))?;

    // Live-manager re-discovery: surface the new module so it is usable
    // without a run. Replace unloads the previous instance first (there is no
    // per-run registry to unregister from; the next run registers tools).
    sync_live_manager(&plugin_manager, replacing, &id, &store_dir).await;

    let verb = if installed.replaced { "Replaced" } else { "Installed" };
    let detail =
        if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) };
    Ok(format!("{verb} '{id}' v{}{detail}", installed.manifest.version))
}

/// ADR-37 — Delete an installed plugin: persisted grants, live grants and
/// active instance, then the `.wasm` and sidecar files. Idempotent when the
/// file is already gone (stale selection); grant-store failures are reported
/// as notes on the success line rather than aborting the file removal.
pub(crate) async fn delete_plugin(
    plugin_id: String,
    wasm_path: Option<std::path::PathBuf>,
    plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>,
) -> Result<String, String> {
    let store_dir = concerto_plugins::capability::CapabilityManager::data_dir();
    let mut notes = Vec::new();

    // 1. Persisted grants first, so the store and filesystem stay in sync.
    match concerto_plugins::capability::CapabilityManager::open(&store_dir) {
        Ok(cap_mgr) => {
            if let Err(error) = cap_mgr.revoke_plugin(&plugin_id) {
                notes.push(format!("grants not fully removed: {error}"));
            }
        }
        Err(error) => notes.push(format!("capability store unavailable: {error}")),
    }

    // 2. Best-effort live revocation + unload so host functions fail closed
    // for any alias still holding the instance; the next run rediscovers the
    // (now missing) plugin fresh.
    if let Some(handle) = &plugin_manager {
        let mut guard = handle.lock().await;
        if let Some(manager) = guard.as_mut().map(|(_, manager)| manager) {
            manager.revoke_grants_best_effort(&plugin_id).await;
            if let Err(error) = manager.unload_without_registry(&plugin_id).await {
                tracing::warn!(
                    plugin_id,
                    error = %error,
                    "plugin delete: unload failed (next run rediscovers)"
                );
            }
        }
    }

    // 3. Files (idempotent); a stale selection simply has nothing to remove.
    if let Some(path) = wasm_path {
        concerto_plugins::installer::delete_plugin_file(&path)
            .map_err(|e| format!("Delete failed: {e}"))?;
    } else {
        tracing::debug!(plugin_id, "plugin delete: no known wasm path — file already absent");
    }

    if notes.is_empty() {
        Ok(format!("Deleted '{plugin_id}'"))
    } else {
        Ok(format!("Deleted '{plugin_id}' (with notes: {})", notes.join("; ")))
    }
}

/// Best-effort live-manager reconciliation after an install/replace, so a
/// freshly written module is discoverable without an agent run. New plugins
/// are initialised with empty (fail-closed) grants; a full run initialises
/// them with their run-scoped, store-backed grant set.
async fn sync_live_manager(
    plugin_manager: &Option<concerto_plugins::manager::SharedPluginManager>,
    replacing: bool,
    plugin_id: &str,
    store_dir: &std::path::Path,
) {
    let Some(handle) = plugin_manager else { return };
    let mut guard = handle.lock().await;
    let Some(manager) = guard.as_mut().map(|(_, manager)| manager) else { return };
    if replacing {
        // The previous instance holds the old binary; drop it so the refresh
        // below initialises the replacement rather than skipping an "active"
        // plugin.
        if let Err(error) = manager.unload_without_registry(plugin_id).await {
            tracing::warn!(
                plugin_id,
                error = %error,
                "plugin install: failed to unload previous instance"
            );
        }
    }
    let config = concerto_plugins::discovery::DiscoveryConfig {
        search_paths: vec![store_dir.to_owned()],
        bundled_path: None,
    };
    match manager
        .refresh_new_plugins(config, |_| concerto_plugins::capability::GrantedCapabilities::new())
        .await
    {
        Ok(loaded) => tracing::info!(plugin_id, loaded, "plugin install: live manager refreshed"),
        Err(error) => tracing::warn!(
            plugin_id,
            error = %error,
            "plugin install: live manager refresh failed (next run loads it)"
        ),
    }
}

/// Human-readable summary of what a plugin provides, for the Settings list
/// (e.g. `tool:edit_file, provider:anthropic`).
fn provides_label(provides: &[concerto_api_types::plugin::PluginProvides]) -> String {
    use concerto_api_types::plugin::PluginProvides;
    provides
        .iter()
        .map(|p| match p {
            PluginProvides::Tool(tool) => format!("tool:{}", tool.name),
            PluginProvides::Provider(provider) => format!("provider:{}", provider.name),
            PluginProvides::MemoryAdapter(adapter) => format!("memory:{}", adapter.name),
            PluginProvides::Dialect(dialect) => format!("dialect:{}", dialect.name),
            _ => "unknown".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::message::SectionId;
    use super::super::*;
    use super::super::{readable_provider_label, PolicyActionChoice, PolicyConditionChoice};
    use concerto_api_types::extension::SkillManifest;
    use concerto_config::{
        AgentRelationshipConfig, AppConfig, ConditionDef, ModelSettings, PolicyRuleDef,
        ProviderConfig,
    };

    fn provider(id: &str, kind: &str, model: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: kind.into(),
            provider: kind.into(),
            model: model.into(),
            keyring_key: format!("{kind}/api_key"),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn provider_labels_use_human_readable_names() {
        let openai = provider("openai", "openai", "gpt-4.1");
        assert_eq!(readable_provider_label(&openai), "OpenAI");

        let mut custom = openai;
        custom.name = "Production".into();
        assert_eq!(readable_provider_label(&custom), "Production (OpenAI)");
    }

    #[test]
    fn loading_config_with_global_default_model() {
        let config = AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![provider("openrouter", "openrouter", "openai/gpt-4.1")],
                global_default_id: Some("openrouter".into()),
                global_default_model: Some("openai/gpt-4.1".into()),
                ..ModelSettings::default()
            }),
            ..AppConfig::default()
        };

        let state = State::from_config(&config);

        assert_eq!(
            state.global_default_model.as_deref(),
            Some("openai/gpt-4.1"),
            "global default model must be loaded from config"
        );
    }

    #[test]
    fn model_choices_are_scoped_to_their_provider() {
        let config = AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![
                    provider("router", "openrouter", "anthropic/claude-sonnet-4"),
                    provider("code", "opencode", "opencode/deepseek-v4-flash-free"),
                ],
                global_default_id: Some("router".into()),
                global_default_model: Some("anthropic/claude-sonnet-4".into()),
                ..ModelSettings::default()
            }),
            ..AppConfig::default()
        };

        let state = State::from_config(&config);

        // Each provider's model list is scoped to that provider's own pinned
        // models. Agent-assignment overrides are managed by the Orchestration
        // Studio and are not mirrored into the settings model cache.
        let router_models = state.model_names_for_provider("router");
        let code_models = state.model_names_for_provider("code");
        assert!(router_models.contains(&"anthropic/claude-sonnet-4".to_string()));
        assert!(code_models.contains(&"opencode/deepseek-v4-flash-free".to_string()));
        // The two providers do not leak each other's pinned models.
        assert!(!router_models.contains(&"opencode/deepseek-v4-flash-free".to_string()));

        assert_eq!(
            state.global_default_model.as_deref(),
            Some("anthropic/claude-sonnet-4"),
            "global default model must be loaded from config"
        );
    }

    #[test]
    fn relationship_self_reference_is_rejected_with_warning() {
        let mut state = State::from_config(&AppConfig::default());
        state.new_relationship_from = "coder";
        state.new_relationship_to = "coder";
        let _ = state.update(Message::RelationshipAdded);
        assert!(state.relationship_rules.is_empty(), "self-reference must not create a rule");
        assert_eq!(
            state.relationship_warning.as_deref(),
            Some("An agent cannot have a relationship with itself.")
        );
    }

    #[test]
    fn relationship_duplicate_shows_warning_and_replaces() {
        let mut state = State::from_config(&AppConfig::default());
        state.new_relationship_from = "reviewer";
        state.new_relationship_to = "coder";
        state.new_relationship_type = "supervises";
        state.new_relationship_cycles = "3".into();
        let _ = state.update(Message::RelationshipAdded);
        assert_eq!(state.relationship_rules.len(), 1);
        assert!(state.relationship_warning.is_none());

        // A second rule with the same from→to must replace, not duplicate,
        // and must surface an inline warning.
        state.new_relationship_type = "validates";
        state.new_relationship_cycles = "1".into();
        let _ = state.update(Message::RelationshipAdded);
        assert_eq!(state.relationship_rules.len(), 1, "duplicate from→to must replace, not add");
        assert!(state.relationship_warning.is_some());
        assert_eq!(state.relationship_rules[0].relationship, "validates");
    }

    #[test]
    fn relationship_display_formats_readable_sentence() {
        let rule = AgentRelationshipConfig {
            from: "reviewer".into(),
            to: "coder".into(),
            relationship: "supervises".into(),
            max_cycles: Some(3),
        };
        assert_eq!(State::relationship_display(&rule), "reviewer supervises coder (max 3 cycles)");
        let no_cycles = AgentRelationshipConfig { max_cycles: None, ..rule };
        assert_eq!(State::relationship_display(&no_cycles), "reviewer supervises coder");
    }

    #[test]
    fn sync_relationships_from_config_refreshes_rows_but_preserves_inflight_edits() {
        let mut state = State::from_config(&AppConfig::default());
        assert!(state.relationship_rules.is_empty());

        // The studio saved relationships into the merged config; the Settings
        // list was seeded at startup and must pick them up on entry.
        let studio_rule = AgentRelationshipConfig {
            from: "architect".into(),
            to: "coder".into(),
            relationship: "supervises".into(),
            max_cycles: Some(5),
        };
        let live = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                relationships: vec![studio_rule.clone()],
                ..Default::default()
            }),
            ..Default::default()
        };
        state.sync_relationships_from_config(&live);
        assert_eq!(state.relationship_rules, vec![studio_rule.clone()]);

        // Once the user edits the manager here, the refresh must not clobber
        // their in-flight work, even if the config changed again meanwhile.
        state.new_relationship_from = "reviewer";
        state.new_relationship_to = "coder";
        state.new_relationship_type = "supervises";
        state.new_relationship_cycles = "3".into();
        let _ = state.update(Message::RelationshipAdded);
        assert!(state.relationship_dirty);
        let inflight = state.relationship_rules.clone();
        assert_eq!(inflight.len(), 2, "synced row + the user's new edit");
        let changed = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                relationships: Vec::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        state.sync_relationships_from_config(&changed);
        assert_eq!(
            state.relationship_rules, inflight,
            "dirty list must keep the user's edits and not be refreshed"
        );
    }

    #[test]
    fn sync_providers_from_config_refreshes_rows_from_live_config() {
        let mut state = State::from_config(&AppConfig::default());
        assert!(state.providers.is_empty());

        // An external config edit added providers; the Settings list was
        // seeded at startup and must pick them up, along with the derived
        // caches.
        let live = AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![
                    provider("router", "openrouter", "anthropic/claude-sonnet-4"),
                    provider("code", "opencode", "opencode/deepseek-v4-flash-free"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        state.sync_providers_from_config(&live);

        assert_eq!(state.providers.len(), 2);
        assert_eq!(
            state.cached_provider_ids,
            vec!["router".to_string(), "code".to_string()],
            "derived caches must be rebuilt from the refreshed rows"
        );
        assert!(!state.settings_dirty, "a row sync must not arm the dirty flag");
    }

    #[test]
    fn sync_providers_from_config_preserves_unsaved_edits() {
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));
        state.settings_dirty = true;
        let inflight = state.providers.clone();

        let changed = AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![
                    provider("router", "openrouter", "anthropic/claude-sonnet-4"),
                    provider("code", "opencode", "opencode/deepseek-v4-flash-free"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        state.sync_providers_from_config(&changed);

        assert_eq!(
            state.providers, inflight,
            "dirty rows must keep the user's edits and not be refreshed"
        );
    }

    #[test]
    fn sync_providers_from_config_resets_dangling_row_ui_state() {
        let live = AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![provider("router", "openrouter", "anthropic/claude-sonnet-4")],
                ..Default::default()
            }),
            ..Default::default()
        };

        // In-flight key-typing guards the sync entirely: rows and per-row UI
        // state stay untouched (the guard also covers the case where typing
        // has not yet armed the dirty flag).
        let mut typing = State::from_config(&AppConfig::default());
        typing.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));
        let rows_before = typing.providers.clone();
        typing.editing_key_for = Some(0);
        typing.key_edit_text = "rotated-secret".into();
        typing.confirm_delete_for = Some(0);
        typing.confirm_clear_for = Some(0);
        typing.sync_providers_from_config(&live);
        assert_eq!(typing.providers, rows_before, "key-typing must guard the row sync");
        assert_eq!(typing.editing_key_for, Some(0));
        assert_eq!(typing.key_edit_text, "rotated-secret");
        assert_eq!(typing.confirm_delete_for, Some(0));
        assert_eq!(typing.confirm_clear_for, Some(0));

        // Without in-flight edits the sync runs and clears dangling per-row
        // UI state left over from the replaced rows.
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));
        state.confirm_delete_for = Some(0);
        state.confirm_clear_for = Some(0);
        state.sync_providers_from_config(&live);

        assert!(state.confirm_delete_for.is_none());
        assert!(state.confirm_clear_for.is_none());
        assert!(state.editing_key_for.is_none());
        assert!(state.key_edit_text.is_empty());
        assert_eq!(state.providers.len(), 1);
        assert_eq!(state.providers[0].id, "router");
    }

    #[test]
    fn all_settings_sections_start_collapsed() {
        let state = State::from_config(&AppConfig::default());
        for section in SectionId::ALL {
            assert!(
                state.collapsed_sections.contains(&section),
                "section {section:?} must start collapsed"
            );
        }
    }

    #[test]
    fn jump_to_section_expands_but_never_folds() {
        let mut state = State::from_config(&AppConfig::default());
        // The sidebar jump expands its target...
        let _ = state.update(Message::JumpToSection(SectionId::Extensions));
        assert!(
            !state.collapsed_sections.contains(&SectionId::Extensions),
            "a sidebar jump must expand its target"
        );
        // ...and is idempotent: jumping again never folds it.
        let _ = state.update(Message::JumpToSection(SectionId::Extensions));
        assert!(!state.collapsed_sections.contains(&SectionId::Extensions));
        // The section header keeps the toggle semantics.
        let _ = state.update(Message::ToggleSection(SectionId::Extensions));
        assert!(state.collapsed_sections.contains(&SectionId::Extensions));
    }

    #[test]
    fn provider_model_options_include_extra_models() {
        let mut gateway = provider("gateway", "openai", "gpt-4o");
        gateway.extra_models = vec!["gateway-only".into(), "  ".into()];
        let config = AppConfig {
            model_settings: Some(ModelSettings { providers: vec![gateway], ..Default::default() }),
            ..Default::default()
        };
        let mut state = State::from_config(&AppConfig::default());
        state.refresh_provider_cache_from_config(&config);

        assert!(
            state.model_names_for_provider("gateway").contains(&"gateway-only".to_string()),
            "config-first extra_models must become selectable in the Settings pickers"
        );
    }

    #[test]
    fn to_config_preserves_studio_relationships_unless_settings_edited_them() {
        // Base config holds relationships the studio saved independently.
        let studio_rule = AgentRelationshipConfig {
            from: "architect".into(),
            to: "coder".into(),
            relationship: "supervises".into(),
            max_cycles: Some(5),
        };
        let base = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                relationships: vec![studio_rule.clone()],
                ..Default::default()
            }),
            ..Default::default()
        };
        // Settings seeded from a different (startup-era) relationship list.
        let mut state = State::from_config(&AppConfig::default());

        // An incidental save (theme/retry/etc.) must not clobber the studio's
        // relationships with this stale snapshot.
        let saved = state.to_config(&base);
        let saved_rels = saved.multi_agent.unwrap().relationships;
        assert_eq!(saved_rels, vec![studio_rule.clone()]);

        // Once the user explicitly edits the relationship manager, Settings
        // takes ownership of the list and its edit wins.
        state.new_relationship_from = "reviewer";
        state.new_relationship_to = "coder";
        state.new_relationship_type = "supervises";
        state.new_relationship_cycles = "3".into();
        let _ = state.update(Message::RelationshipAdded);
        assert!(state.relationship_dirty);

        let edited = state.to_config(&base);
        let edited_rels = edited.multi_agent.unwrap().relationships;
        assert_eq!(edited_rels.len(), 1);
        assert_eq!(edited_rels[0].from, "reviewer");
    }

    #[test]
    fn policy_preview_reflects_current_selection() {
        assert_eq!(
            State::policy_preview(
                PolicyActionChoice::Allow,
                PolicyConditionChoice::Always,
                "filesystem",
                "write",
                "",
            ),
            "Allow automatically for every operation"
        );
        assert_eq!(
            State::policy_preview(
                PolicyActionChoice::Deny,
                PolicyConditionChoice::Tool,
                "shell",
                "write",
                "",
            ),
            "Deny when the tool is 'shell'"
        );
        assert_eq!(
            State::policy_preview(
                PolicyActionChoice::Ask,
                PolicyConditionChoice::ProjectPath,
                "filesystem",
                "write",
                "",
            ),
            "Ask for approval when a project path matches the glob you enter"
        );
    }

    #[test]
    fn policy_builder_creates_tool_scoped_operation_rule() {
        let mut state = State::new();
        state.new_policy_action = PolicyActionChoice::Allow;
        state.new_policy_condition_kind = PolicyConditionChoice::ToolOperation;
        state.new_policy_tool = "filesystem";
        state.new_policy_operation = "write";

        let _ = state.update(Message::PolicyRuleAdded);

        assert_eq!(
            state.policy_rules,
            vec![PolicyRuleDef {
                action: "auto_approve".into(),
                condition: ConditionDef::ToolOperation {
                    tool_name: "filesystem".into(),
                    operation: "write".into(),
                },
            }]
        );
    }

    #[test]
    fn policy_rules_can_be_reordered_to_control_precedence() {
        let mut state = State::new();
        state.policy_rules = vec![
            PolicyRuleDef {
                action: "auto_approve".into(),
                condition: ConditionDef::Always { always: true },
            },
            PolicyRuleDef {
                action: "auto_deny".into(),
                condition: ConditionDef::ToolName { tool_name: "shell".into() },
            },
        ];

        let _ = state.update(Message::PolicyRuleMovedUp(1));

        assert_eq!(state.policy_rules[0].action, "auto_deny");
        assert_eq!(state.policy_rules[1].action, "auto_approve");
    }

    #[test]
    fn retry_settings_round_trip_through_the_settings_page() {
        let base = AppConfig::default();
        let mut state = State::from_config(&base);
        state.retry_enabled = true;
        state.retry_initial_delay_ms = 1500.0;
        state.retry_max_delay_ms = 45000.0;
        state.retry_multiplier = 1.5;
        state.retry_fixed_delay_ms = "2500".into();
        state.retry_respect_after = false;
        state.retry_jitter = false;
        state.retry_max_elapsed_seconds = "120".into();

        let config = state.to_config(&base);

        assert!(config.retry.enabled);
        assert_eq!(config.retry.initial_delay_ms, 1_500);
        assert_eq!(config.retry.max_delay_ms, 45_000);
        assert_eq!(config.retry.multiplier, 1.5);
        assert_eq!(config.retry.fixed_delay_ms, Some(2_500));
        assert!(!config.retry.respect_retry_after);
        assert!(!config.retry.jitter);
        assert_eq!(config.retry.max_elapsed_seconds, Some(120));
    }

    #[test]
    fn retry_optional_fields_validate_on_change() {
        let mut state = State::new();

        // Valid: empty
        let _ = state.update(Message::RetryFixedDelayChanged("".into()));
        assert!(state.retry_fixed_delay_error.is_none());

        // Valid: positive integer
        let _ = state.update(Message::RetryFixedDelayChanged("2500".into()));
        assert!(state.retry_fixed_delay_error.is_none());

        // Invalid: negative
        let _ = state.update(Message::RetryFixedDelayChanged("-100".into()));
        assert!(state.retry_fixed_delay_error.is_some());

        // Invalid: not a number
        let _ = state.update(Message::RetryFixedDelayChanged("abc".into()));
        assert!(state.retry_fixed_delay_error.is_some());

        // Invalid: zero
        let _ = state.update(Message::RetryFixedDelayChanged("0".into()));
        assert!(state.retry_fixed_delay_error.is_some());

        // Same for max_elapsed
        let _ = state.update(Message::RetryMaxElapsedChanged("".into()));
        assert!(state.retry_max_elapsed_error.is_none());

        let _ = state.update(Message::RetryMaxElapsedChanged("120".into()));
        assert!(state.retry_max_elapsed_error.is_none());

        let _ = state.update(Message::RetryMaxElapsedChanged("xyz".into()));
        assert!(state.retry_max_elapsed_error.is_some());
    }

    #[test]
    fn memory_settings_round_trip_through_the_settings_page() {
        let base = AppConfig::default();
        let mut state = State::from_config(&base);
        state.memory_enabled = false;
        state.memory_ttl_days = 91.0;

        let config = state.to_config(&base);

        assert!(!config.memory.enabled);
        assert_eq!(config.memory.ttl_days, 91);
    }

    // ── Unsaved-changes tracking ────────────────────────────────────────────
    //
    // All settings edits (policy/relationship/memory/retry/shell/providers)
    // only persist on an explicit "Save Settings"; `settings_dirty` drives the
    // unsaved-changes indicator.

    #[test]
    fn settings_edit_arms_dirty_flag_until_save() {
        let mut state = State::from_config(&AppConfig::default());
        assert!(!state.settings_dirty, "fresh state must be clean");

        let _ = state.update(Message::RelationshipCyclesChanged("5".into()));
        assert!(state.settings_dirty, "relationship edit must arm the dirty flag");

        let _ = state.update(Message::SaveSettings);
        assert!(!state.settings_dirty, "Save Settings must clear the dirty flag");
        assert!(state.settings_saved_notice, "save must show the success notice");
    }

    #[test]
    fn provider_messages_arm_dirty_flag() {
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));

        // Provider changes must not auto-persist: they arm the dirty flag and
        // only persist together with other changes on Save Settings.
        let task = state.update(Message::ProviderDeletePressed(0));
        assert_eq!(task.units(), 0, "provider messages must not trigger a persist task");
        assert!(state.settings_dirty, "provider delete must arm the dirty flag");

        let task = state.update(Message::ProviderDeleteConfirmed(0));
        assert_eq!(task.units(), 0, "provider messages must not trigger a persist task");
        assert!(state.settings_dirty, "provider deletion must keep the dirty flag armed");

        let _ = state.update(Message::SaveSettings);
        assert!(!state.settings_dirty, "Save Settings must clear the dirty flag");
    }

    // ── Credential lifecycle (plan §5.3) ─────────────────────────────────────
    //
    // NOTE: the keyring side-effects (CredentialStore::set/delete) are exercised
    // by the handlers but cannot be asserted in a headless unit test: in test
    // mode the store is read-only (env vars), and production mode targets the
    // OS keychain (unavailable in CI). Those calls mirror the already-shipping
    // FormConfirmAdd path and are correct by construction. These tests verify
    // the State-level wiring that the new §5.3 messages drive.

    #[test]
    fn delete_provider_requires_confirmation_then_removes_it() {
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));

        // First press only arms the confirmation prompt (destructive action).
        let _ = state.update(Message::ProviderDeletePressed(0));
        assert_eq!(
            state.confirm_delete_for,
            Some(0),
            "first delete press must arm the confirmation prompt"
        );
        assert_eq!(state.providers.len(), 1, "provider must not be removed before confirm");

        // Cancelling disarms the prompt without deleting.
        let _ = state.update(Message::ProviderDeleteCancelled(0));
        assert!(state.confirm_delete_for.is_none(), "cancel must disarm the prompt");
        assert_eq!(state.providers.len(), 1, "cancel must not remove the provider");

        // Confirming performs the actual removal.
        let _ = state.update(Message::ProviderDeletePressed(0));
        let _ = state.update(Message::ProviderDeleteConfirmed(0));
        assert!(state.providers.is_empty(), "provider must be removed after confirm");
    }

    #[test]
    fn edit_key_exits_edit_mode_and_clears_text() {
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));
        let idx = 0;
        state.editing_key_for = Some(idx);
        state.key_edit_text = "rotated-secret".into();

        let _ = state.update(Message::FormSaveKey(idx));

        assert!(state.editing_key_for.is_none(), "edit mode must exit after save");
        assert!(
            state.key_edit_text.is_empty(),
            "edit buffer must clear after save (secret handed to keyring)"
        );
    }

    #[test]
    fn clear_key_confirm_flow_toggles_then_resets_edit_state() {
        let mut state = State::from_config(&AppConfig::default());
        state.providers.push(provider("anthropic", "anthropic", "claude-3-5-sonnet"));
        let idx = 0;
        state.editing_key_for = Some(idx);

        let _ = state.update(Message::FormClearKey(idx));
        assert_eq!(
            state.confirm_clear_for,
            Some(idx),
            "first Clear press must arm the confirmation prompt"
        );

        let _ = state.update(Message::FormClearKey(idx));
        assert!(
            state.confirm_clear_for.is_none(),
            "second Clear press must cancel the confirmation prompt"
        );

        let _ = state.update(Message::FormClearKeyConfirmed(idx));
        assert!(state.editing_key_for.is_none(), "confirmed clear must exit edit mode");
        assert!(state.confirm_clear_for.is_none());
    }

    // ── ADR-43 — skill discovery helper ────────────────────────────────────
    //
    // `SkillManager::discover` skips (with a warning) search paths that are
    // missing or not directories, and also skips packs that fail to load
    // (malformed manifest, invalid id, unreadable directory). A broken pack
    // is *not* fatal — remaining packs still surface, mirroring the skills
    // crate's own `malformed_skill_toml_is_skipped_not_fatal` semantics.

    #[test]
    fn discover_skills_skips_missing_path_without_error() {
        let result = super::discover_skills(vec!["/definitely/does/not/exist/xyzzy-99999".into()]);
        let report = result.expect("a missing search path is skipped, not an error");
        assert_eq!(report.descriptors.len(), 0);
        assert_eq!(report.resolved_paths.len(), 1, "the missing path is still reported");
        assert!(
            report.warnings.iter().any(|w| w.contains("xyzzy-99999") && w.contains("missing")),
            "the missing path must be surfaced in warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn discover_skills_finds_a_valid_pack() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pack = temp.path().join("pack");
        std::fs::create_dir_all(&pack).expect("create pack dir");
        std::fs::write(
            pack.join("skill.toml"),
            "id = \"rust-testing\"\nname = \"Rust Testing\"\nversion = \"1.0.0\"\ndescription = \"Cargo verification guidance\"\ninstructions = \"Prefer cargo nextest.\"\ntools = [\"cargo nextest run\"]\n",
        )
        .expect("write manifest");

        let result = super::discover_skills(vec![temp.path().to_string_lossy().into_owned()]);
        let report = result.expect("discovery should succeed");
        assert_eq!(report.descriptors.len(), 1);
        let skill = &report.descriptors[0];
        assert_eq!(skill.id, "rust-testing");
        assert_eq!(skill.manifest.tools, vec!["cargo nextest run"]);
        assert_eq!(skill.instructions, "Prefer cargo nextest.");
        assert_eq!(report.resolved_paths, vec![temp.path()], "resolved path is the scanned dir");
        assert!(report.warnings.is_empty(), "a healthy pack must not warn: {:?}", report.warnings);
    }

    #[test]
    fn discover_skills_skips_malformed_manifest_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pack = temp.path().join("pack");
        std::fs::create_dir_all(&pack).expect("create pack dir");
        std::fs::write(pack.join("skill.toml"), "id = [unclosed\n").expect("write manifest");

        let result = super::discover_skills(vec![temp.path().to_string_lossy().into_owned()]);
        let report = result.expect("a malformed manifest is skipped with a warning, not fatal");
        assert_eq!(report.descriptors.len(), 0);
        assert!(
            report.warnings.iter().any(|w| w.contains("failed to load")),
            "the malformed pack must be surfaced in warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn skill_pack_crud_round_trip_via_helpers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().to_string_lossy().into_owned();

        let created = super::create_skill_pack(
            parent.clone(),
            "reviewer".into(),
            "Code Reviewer".into(),
            "0.1.0".into(),
            "second pair of eyes".into(),
            "Look for bugs.".into(),
        )
        .expect("create should succeed");
        assert!(created.contains("reviewer"), "outcome names the pack: {created}");

        let report = super::discover_skills(vec![parent.clone()]).expect("discovery");
        assert_eq!(report.descriptors.len(), 1);
        assert_eq!(report.descriptors[0].id, "reviewer");
        assert_eq!(report.descriptors[0].manifest.name, "Code Reviewer");
        assert_eq!(report.descriptors[0].instructions, "Look for bugs.");

        // Update preserves the id/version/tools and replaces instructions.
        let updated = super::update_skill_pack(
            report.descriptors[0].pack_dir.to_string_lossy().into_owned(),
            SkillManifest {
                id: "reviewer".into(),
                name: "Senior Reviewer".into(),
                version: "0.1.0".into(),
                description: "second pair of eyes".into(),
                instructions_path: None,
                instructions: Some("Review harder.".into()),
                tools: vec!["cargo review".into()],
                resources: Vec::new(),
            },
        )
        .expect("update should succeed");
        assert!(updated.contains("Saved"), "outcome names the action: {updated}");

        let report = super::discover_skills(vec![parent.clone()]).expect("rediscovery");
        assert_eq!(report.descriptors[0].manifest.name, "Senior Reviewer");
        assert_eq!(report.descriptors[0].manifest.tools, vec!["cargo review"]);
        assert_eq!(report.descriptors[0].instructions, "Review harder.");

        // Delete backs the manifest up (reversible), so discovery is empty
        // again while the hidden backup still exists in the pack directory.
        let deleted =
            super::delete_skill_pack(report.descriptors[0].pack_dir.to_string_lossy().into_owned())
                .expect("delete should succeed");
        assert!(deleted.contains("Deleted"), "outcome names the action: {deleted}");
        assert!(deleted.contains("1 manifest file(s)"), "one backup reported: {deleted}");

        let report = super::discover_skills(vec![parent.clone()]).expect("post-delete discovery");
        assert!(report.descriptors.is_empty(), "the pack disappears from discovery");

        let backups = std::fs::read_dir(temp.path().join("reviewer"))
            .expect("pack dir survives as the backup container")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".deleted-"))
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1, "one hidden backup remains: {backups:?}");
    }
}
