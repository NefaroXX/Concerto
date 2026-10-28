//! Run/dispatch helpers for [`App`]: runtime configuration projections,
//! provider/assignment validation, and run submission.
//!
//! The shared conversation application owns validation and execution. This
//! adapter prepares desktop service handles and updates presentation only
//! after a submission is accepted.

use super::*;

impl App {
    pub(super) fn reset_pending_task_views(&mut self) {
        if self.pending_task_view_reset {
            self.agent_graph = views::agent_graph::State::new();
            self.chat.begin_run();
            self.pending_task_view_reset = false;
        }
    }

    /// Provider rows from the effective runtime configuration.
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

    #[cfg(test)]
    pub(super) fn dispatch_validation_error(&self) -> Option<String> {
        concerto_orchestrator::conversation::validate_provider_setup(
            self.runtime_providers(),
            self.runtime_assignments(),
            &self.active_provider_id,
            &self.active_model,
        )
    }

    pub(super) fn submit_to_agent(&mut self, user_input: String) -> iced::Task<Message> {
        if user_input.trim().is_empty() {
            return iced::Task::none();
        }
        let Some(config) = self.config.clone() else {
            self.chat
                .add_error("Configuration is unavailable. Open Settings to finish setup.".into());
            self.show_settings_modal = true;
            return iced::Task::none();
        };
        let options = ConversationOptions {
            project_dir: self.project_dir.clone(),
            provider_id: (!self.active_provider_id.is_empty())
                .then(|| self.active_provider_id.clone()),
            model: Some(if self.active_model.is_empty() {
                self.resolve_default_model()
            } else {
                self.active_model.clone()
            }),
            fast: self.fast,
        };
        let submission = match self.conversation.submit_configured(user_input, &config, &options) {
            Ok(submission) => submission,
            Err(message) => {
                self.chat.add_error(message);
                return iced::Task::none();
            }
        };
        // Clear the draft only after the shared application accepts it.
        TEXT_FOCUSED.store(false, Ordering::Relaxed);
        self.text_focused = false;
        let chat_task = self.chat.update(views::chat::Message::SubmitInput).map(Message::Chat);
        let Submission::Start(turn) = submission else {
            self.chat.add_thinking(
                "coordinator",
                concerto_orchestrator::conversation::MESSAGE_QUEUED.into(),
                concerto_core::event::ThinkingKind::Headline,
            );
            return chat_task;
        };
        self.pending_task_view_reset = self.conversation.session_id.is_some()
            || (self.multi_agent && self.conversation.checkpoint_json.is_some());
        if !self.pending_task_view_reset {
            self.agent_graph = views::agent_graph::State::new();
            self.chat.begin_run();
        }
        self.run_stage = None;

        let approval_sink = desktop_approval_sink(
            self.cap_pending.clone(),
            self.pending_ack.clone(),
            self.pending_intent.clone(),
            self.pending_plan.clone(),
            self.bus.clone(),
        );
        let mut builder = ServicesBuilder::new(self.bus.clone(), config, approval_sink)
            .with_vfs(self.vfs.clone())
            .with_memory(self.memory_services.clone())
            .with_plugins(self.plugin_manager.clone());
        if let Some(handler) =
            self.session_manager.lock().unwrap_or_else(|error| error.into_inner()).as_ref()
        {
            builder = builder.with_session_manager(handler.manager());
        }
        let services = builder.build();
        let run = iced::Task::perform(turn.execute(options, services), |completion| {
            Message::AgentRunCompleted(Box::new(completion))
        });
        iced::Task::batch([chat_task, run])
    }
}
