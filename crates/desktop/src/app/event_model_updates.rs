//! Backend-event and model/config `App::update()` arms for [`App`] — NORM
//! slice S38.
//!
//! This module owns two groups of the root update match, extracted from
//! `app.rs`:
//!
//! * [`App::update_desktop_event`] — the `DesktopEvent` arm: the App-level
//!   spend / cap / run-stage chip updates, the `ErrorOccurred` error toast,
//!   and the `route_event` fan-out into the chat / tool-log / agent-graph /
//!   memory view states.
//! * [`App::update_model_config`] — `SetActiveProvider`, `SetActiveModel`,
//!   `SetAgentModel`, and `ConfigReloaded` (the active provider/model
//!   selection + refresh kickoff and the ADR-57 external config reload).
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is each group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and
//! the sibling `simple_updates` / `project_switch` / `delegated_updates`
//! groups), so every arm keeps its exact early-return `Task` semantics.
//! The parent `update` keeps one thin delegating arm per group; the
//! fallback arm in each method is unreachable through the parent
//! dispatcher and is a documented no-op. No `Message` / `App` shape change
//! and no behavior change: the tests in `app.rs`'s `mod tests` stay put
//! and drive `update()` unchanged.

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
