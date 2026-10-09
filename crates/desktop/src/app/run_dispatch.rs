//! Run/dispatch helpers for [`App`] — NORM slice S49.
//!
//! This module owns the run-boundary helpers moved verbatim from `app.rs`:
//! the config-backed runtime projections (`runtime_providers`,
//! `runtime_assignments`, `runtime_model_names`), the dispatch-boundary
//! validator (`dispatch_validation_error`), and the run submitter
//! (`submit_to_agent`) with its async `iced::Task::perform` that captures the
//! bus/config/memory/VFS/session handles. Every body is line-for-line with
//! its origin at the same 4-space `impl` indent; the only structural edit is
//! each method becoming `pub(super)`, which keeps the existing callers — the
//! `App::new` boot batch, the sibling submodule groups (`update_routing`,
//! `settings_updates`, `event_model_updates`, `sync_config`), and the tests
//! in `app.rs`'s `mod tests` — resolving through `self.*` / `app.*` unchanged
//! (the same visibility pattern as the sibling group modules). No `Message` /
//! `App` shape change and no behavior change: the async `Task::perform`
//! timing and ordering inside `submit_to_agent` are preserved exactly.
//!
//! Left in `app.rs` deliberately: the sync VFS diff reload
//! (`load_diff_from_vfs`) and the Settings-save plugin re-collect
//! (`refresh_plugin_providers`) — neither is a run/dispatch seam.

use super::*;

impl App {
    /// Validate that the active provider/model and (in multi-agent mode) every
    /// agent assignment resolves to a ready, complete provider. Returns a
    /// human-readable reason when something is incomplete; `None` when ready.
    pub(super) fn runtime_providers(&self) -> &[concerto_config::ProviderConfig] {
        self.config
            .as_ref()
            .and_then(|config| config.model_settings.as_ref())
            .map(|settings| settings.providers.as_slice())
            .unwrap_or(self.settings.providers.as_slice())
    }

    pub(super) fn runtime_assignments(&self) -> &[concerto_config::AgentModelAssignment] {
        self.config
            .as_ref()
            .and_then(|config| config.model_settings.as_ref())
            .map(|settings| settings.agent_assignments.as_slice())
            .unwrap_or(&[])
    }

    /// Model names selectable for the active provider in the chat header,
    /// resolved through the shared picker resolver (selected / default / known
    /// / discovered / config-first `extra_models`) so every picker agrees.
    pub(super) fn runtime_model_names(&self, provider_id: &str) -> Vec<String> {
        self.runtime_providers()
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(picker_model_options)
            .unwrap_or_default()
    }

    pub(super) fn dispatch_validation_error(&self) -> Option<String> {
        if self.runtime_providers().is_empty() {
            return Some("no providers are configured".to_string());
        }
        let creds = CredentialStore::new();
        // The intent gate is always on (ADR-55 §7): there is no mode
        // picker, so every run is a potential Execute regardless of the chat
        // outcome the router eventually classifies. Validate the active
        // (composer) provider unconditionally and check every agent
        // assignment; nothing may slip through unvalidated.
        match self.runtime_providers().iter().find(|p| p.id == self.active_provider_id) {
            None => return Some("no active provider is selected".to_string()),
            Some(provider) => {
                let mut resolved = provider.clone();
                if !self.active_model.trim().is_empty() {
                    resolved.model = self.active_model.clone();
                }
                let def = provider_definition(&resolved.provider);
                let has_key = creds.exists(&resolved.keyring_key);
                if !provider_readiness(&resolved, &def, has_key).is_ready() {
                    return Some(format!(
                        "active provider '{}' is not ready (missing model or required API key)",
                        provider.name
                    ));
                }
            }
        }

        // Multi-agent assignment readiness: every assignment must be complete.
        if self.multi_agent {
            for assignment in self.runtime_assignments() {
                let provider =
                    self.runtime_providers().iter().find(|p| p.id == assignment.provider_config_id);
                let incomplete = match provider {
                    None => true,
                    Some(provider) => {
                        let model_ok = assignment
                            .model_override
                            .as_ref()
                            .map(|m| !m.is_empty())
                            .unwrap_or(false);
                        let mut resolved = provider.clone();
                        if let Some(model) = &assignment.model_override {
                            resolved.model = model.clone();
                        }
                        let def = provider_definition(&resolved.provider);
                        let has_key = creds.exists(&resolved.keyring_key);
                        let ready = provider_readiness(&resolved, &def, has_key).is_ready();
                        !ready || !model_ok
                    }
                };
                if incomplete {
                    return Some(format!(
                        "agent role '{}' is assigned to an incomplete provider/model",
                        assignment.agent_role
                    ));
                }
            }
        }

        None
    }

    pub(super) fn submit_to_agent(&mut self, user_input: String) -> iced::Task<Message> {
        if user_input.trim().is_empty() {
            return iced::Task::none();
        }
        if self.run_status != RunStatus::Idle {
            return iced::Task::none();
        }
        // The graph describes one orchestration run, not the lifetime of the
        // conversation. Reset it at the run boundary even when dispatch
        // validation fails, so an old phase cannot attach to a newer prompt.
        self.agent_graph = views::agent_graph::State::new();
        // Drop the previous run's per-subagent progress cards for the same
        // reason: a stale card must never attach to the new prompt.
        self.chat.begin_run();
        // Dispatch-boundary validation: block the run with a clear message if
        // the active provider/model or any agent assignment is incomplete,
        // rather than failing deep inside the orchestrator.
        if let Some(reason) = self.dispatch_validation_error() {
            self.chat.add_error(format!(
                "Cannot start run: {reason} Open Settings to finish provider setup."
            ));
            return iced::Task::none();
        }
        self.cancel_token = CancellationToken::new();
        // Fresh run boundary: reject any stale run-stage from a previous run
        // (the chip only re-appears once a stage event lands while Running).
        self.run_stage = None;
        self.run_status = RunStatus::Running;
        if let Some(ref cfg) = self.config {
            // Capture the values the async task needs; the session is resolved
            // inside the task because opening the store is async.
            let bus = self.bus.clone();
            let config = cfg.clone();
            let memory = self.memory_services.clone();
            let plugin_manager = self.plugin_manager.clone();
            let vfs = self.vfs.clone();
            let approval_sink = desktop_approval_sink(
                self.cap_pending.clone(),
                self.pending_ack.clone(),
                self.pending_intent.clone(),
                self.pending_plan.clone(),
                self.bus.clone(),
            );
            let session_manager = self.session_manager.clone();
            let active_provider_id = self.active_provider_id.clone();
            // If the composer has no explicit model, fall back to the model
            // assigned to a role that targets the active provider (Option-1).
            let active_model = if self.active_model.is_empty() {
                self.resolve_default_model()
            } else {
                self.active_model.clone()
            };
            let multi_agent = self.multi_agent;
            let fast = self.fast;
            let project_dir = self.project_dir.clone();
            let cancel_token = self.cancel_token.clone();
            let active_session_id = self.active_session_id;
            let resume_checkpoint = self.resume_checkpoint_json.clone();

            iced::Task::perform(
                async move {
                    let mut resolved_session_id = None;
                    let outcome: Result<AgentOutput, OrchestratorError> = async {
                        // Resolve (or lazily open) the project session handler.
                        // The lock guard is dropped before any `.await` so the
                        // future stays `Send`.
                        let existing =
                            session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let handler = if let Some(h) = existing {
                            h
                        } else {
                            let h = Arc::new(
                                DesktopSessionHandler::connect_with_config(&config).await.map_err(
                                    |e| {
                                        OrchestratorError::AgentLoopError(format!(
                                            "session store unavailable: {e}"
                                        ))
                                    },
                                )?,
                            );
                            *session_manager.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(h.clone());
                            h
                        };

                        let provider = if active_provider_id.is_empty() {
                            "default"
                        } else {
                            active_provider_id.as_str()
                        };
                        let model =
                            if active_model.is_empty() { "default" } else { active_model.as_str() };

                        // A blank UI is a genuinely new conversation. Resume
                        // backend history only after the user explicitly
                        // selected an existing session.
                        let session_id = match active_session_id {
                            Some(session_id) => session_id,
                            None => handler
                                .new_session(&project_dir, provider, model)
                                .await
                                .map_err(|e| {
                                    OrchestratorError::AgentLoopError(format!(
                                        "session creation failed: {e}"
                                    ))
                                })?,
                        };
                        resolved_session_id = Some(session_id);
                        let conversation_history =
                            handler.load_history(session_id).await.map_err(|e| {
                                OrchestratorError::AgentLoopError(format!(
                                    "session history load failed: {e}"
                                ))
                            })?;

                        let request =
                            RequestBuilder::new(user_input.clone(), project_dir, cancel_token)
                                .with_provider_model(
                                    (!active_provider_id.is_empty())
                                        .then_some(active_provider_id.clone()),
                                    (!active_model.is_empty()).then_some(active_model),
                                )
                                .with_session(session_id, conversation_history)
                                .with_single_agent(!multi_agent)
                                .with_memory_enabled(memory_enabled(fast, config.memory.enabled))
                                .with_resume_checkpoint(resume_checkpoint)
                                .build();

                        let services = ServicesBuilder::new(bus, config, approval_sink)
                            .with_vfs(vfs)
                            .with_session_manager(handler.manager())
                            .with_memory(memory)
                            .with_plugins(plugin_manager)
                            .build();

                        run_shared_agent(request, services).await
                    }
                    .await;
                    (resolved_session_id, outcome.map_err(ClassifiedFailure::from))
                },
                |(session_id, outcome)| Message::AgentRunCompleted(session_id, Box::new(outcome)),
            )
        } else {
            self.run_status = RunStatus::Idle;
            self.note_run_settled();
            let _ = self.chat.update(views::chat::Message::AddAssistant(
                "Concerto could not load its configuration. Open Settings, configure a provider, and save the settings before starting a task."
                    .to_string(),
            ));
            self.page = Page::Settings;
            iced::Task::none()
        }
    }
}
