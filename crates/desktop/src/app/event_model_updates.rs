//! Backend event and model/configuration updates for [`App`].
//!
//! ResumeRequested confirms candidate continuation views for the active session.
//! The first fresh specialist event clears any unconfirmed candidate before
//! routing to graph/chat consumers. Spend, stage and settings updates retain
//! their existing behavior.

use super::*;

impl App {
    /// The `DesktopEvent` group (NORM S38).
    ///
    /// The parent routes `DesktopEvent` here. Spend events update the
    /// App-level cap chip state, the run-stage chip updates only while a
    /// run is `Running`, backend refusals surface as error toasts, and the
    /// event is then fanned out to the per-view states via
    /// `crate::runtime::route_event`. Returns the arm's `Task` unchanged;
    /// a message the parent never routes here is a documented no-op.
    pub(super) fn update_desktop_event(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::DesktopEvent(evt) => {
                // Spend events update App-level state (status-bar chip + cap
                // state) before the remaining variants route into per-view
                // states. Session-id mismatches are ignored: the chip tracks
                // the active session, and session switches reset this state.
                match &evt {
                    crate::runtime::DesktopEvent::ResumeRequested { session_id }
                        if self.run_status == RunStatus::Running
                            && self.active_session_id == Some(*session_id) =>
                    {
                        self.pending_task_view_reset = false;
                    }
                    crate::runtime::DesktopEvent::SubTaskCreated { .. }
                    | crate::runtime::DesktopEvent::SubTaskStarted { .. }
                        if self.run_status == RunStatus::Running =>
                    {
                        self.reset_pending_task_views();
                    }
                    crate::runtime::DesktopEvent::SpendUpdated { total_usd } => {
                        self.live_session_cost = *total_usd;
                        self.reconcile_cap_state(*total_usd);
                    }
                    crate::runtime::DesktopEvent::SpendCapApproaching {
                        current_usd,
                        cap_usd,
                        pct,
                    } => {
                        self.live_session_cost = *current_usd;
                        self.session_cap = Some(*cap_usd);
                        self.cap_state = CapUiState::Approaching {
                            current_usd: *current_usd,
                            cap_usd: *cap_usd,
                            pct: *pct,
                        };
                    }
                    crate::runtime::DesktopEvent::SpendCapExceeded { current_usd, cap_usd } => {
                        self.live_session_cost = *current_usd;
                        self.session_cap = Some(*cap_usd);
                        self.cap_state =
                            CapUiState::Exceeded { current_usd: *current_usd, cap_usd: *cap_usd };
                    }
                    crate::runtime::DesktopEvent::RunStageChanged { stage }
                        if self.run_status == RunStatus::Running =>
                    {
                        // The run-stage chip tracks the active run only. A
                        // stage event outside a run (e.g. a stale bus replay
                        // caught mid-dispatch) must not re-arm the chip: the
                        // arm is guarded on the run status, so a non-Running
                        // run falls through to the `_` catch-all.
                        self.run_stage = Some(*stage);
                    }
                    crate::runtime::DesktopEvent::ErrorOccurred { message } => {
                        // A backend refusal (e.g. a full ack queue) must be
                        // visible: surface it as an error toast rather than a
                        // silent log line.
                        self.toasts.push(ToastLevel::Error, message.clone());
                    }
                    _ => {}
                }
                crate::runtime::route_event(
                    &evt,
                    &mut self.chat,
                    &mut self.tool_log,
                    &mut self.agent_graph,
                    &mut self.memory,
                );
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The model/config selection group (NORM S38): the active
    /// provider/model switch (label→id resolution, role-assignment model
    /// re-derivation, persistence, and the tracked model-list refresh), the
    /// validated active-model switch, the per-agent model override, and the
    /// external config reload.
    ///
    /// The parent routes `SetActiveProvider`, `SetActiveModel`,
    /// `SetAgentModel`, and `ConfigReloaded` here. Returns the arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_model_config(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::SetActiveProvider(id) => {
                let resolved_id = self
                    .settings
                    .cached_provider_labels
                    .iter()
                    .position(|label| label == &id)
                    .and_then(|index| self.settings.cached_provider_ids.get(index))
                    .cloned()
                    .unwrap_or(id);
                self.active_provider_id = resolved_id.clone();
                if self.runtime_providers().iter().any(|provider| provider.id == resolved_id) {
                    // Models live on role assignments (Option-1), not providers,
                    // so derive the chat model from the assignment for this
                    // provider instead of the now-empty `provider.model`.
                    self.sync_chat_model_options();
                    self.active_model = self.resolve_default_model();
                }
                self.persist_active_model_selection();
                self.refresh_seq = self.refresh_seq.wrapping_add(1);
                let req_id = self.refresh_seq;
                self.pending_refresh.insert(resolved_id.clone(), req_id);
                self.fetch_models_for_provider(resolved_id, req_id)
            }
            Message::SetActiveModel(model) => {
                if self
                    .runtime_model_names(&self.active_provider_id)
                    .iter()
                    .any(|candidate| candidate == &model)
                {
                    self.active_model = model.clone();
                    self.persist_active_model_selection();
                } else {
                    tracing::warn!(
                        provider_id = %self.active_provider_id,
                        model = %model,
                        "ignored model selection that does not belong to the active provider"
                    );
                }
                iced::Task::none()
            }
            Message::SetAgentModel { agent_id, model } => {
                self.set_agent_model(agent_id, model);
                iced::Task::none()
            }

            // External config edit (ADR-57): reload from disk and re-derive
            // every config-derived field through the one shared helper. The
            // equality short-circuit inside makes our own saves no-ops.
            Message::ConfigReloaded => {
                self.reconcile_config_from_reload();
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
