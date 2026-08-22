use concerto_api_types::extension::{McpToolDescriptor, SkillDescriptor};
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
/// never blocked. Returns the found packs, or a human-readable error.
pub(crate) fn discover_skills(search_paths: Vec<String>) -> Result<Vec<SkillDescriptor>, String> {
    let paths = search_paths.iter().map(std::path::PathBuf::from).collect();
    concerto_skills::SkillManager::new(paths).discover().map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::super::{readable_provider_label, PolicyActionChoice, PolicyConditionChoice};
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
    // missing or not directories — a nonexistent path is *not* an error. The
    // only discovery failures are malformed manifests, which surface as `Err`.

    #[test]
    fn discover_skills_skips_missing_path_without_error() {
        let result = super::discover_skills(vec!["/definitely/does/not/exist/xyzzy-99999".into()]);
        assert_eq!(
            result.as_deref().map(|skills| skills.len()),
            Ok(0),
            "a missing search path is skipped, not an error: {result:?}"
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
        let skills = result.expect("discovery should succeed");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "rust-testing");
        assert_eq!(skills[0].manifest.tools, vec!["cargo nextest run"]);
        assert_eq!(skills[0].instructions, "Prefer cargo nextest.");
    }

    #[test]
    fn discover_skills_surfaces_malformed_manifest_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pack = temp.path().join("pack");
        std::fs::create_dir_all(&pack).expect("create pack dir");
        std::fs::write(pack.join("skill.toml"), "id = [unclosed\n").expect("write manifest");

        let result = super::discover_skills(vec![temp.path().to_string_lossy().into_owned()]);
        assert!(result.is_err(), "a malformed manifest must fail discovery loudly: {result:?}");
    }
}
