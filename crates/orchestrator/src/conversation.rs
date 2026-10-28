//! Application-level conversation lifecycle shared by terminal and desktop.
//!
//! Renderers submit text and display results. Session preparation, in-flight
//! delivery, cancellation and stale-result rejection belong here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use concerto_core::event::{EventBus, EventKind};
use concerto_core::failures::ClassifiedFailure;
use concerto_core::ids::Ulid;
use concerto_core::transcript::TranscriptEntry;
use concerto_core::types::{AgentOutput, Message, Role};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_sessions::SessionStore;

use crate::runtime_runner::{memory_enabled, run_shared_agent, SharedServices};
use crate::services::RequestBuilder;
use crate::session_manager::{ProjectSessionManager, SessionManagerConfig};

const MAX_PENDING_MESSAGES: usize = 32;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;

mod reply;
mod setup;
pub(crate) use reply::{reply_to_user, reply_tool_definition, REPLY_TOOL};
pub use setup::validate_provider_setup;

pub const MESSAGE_QUEUED: &str = "Message received; it will be read at the next decision point.";
pub(crate) const CONVERSATION_GUIDANCE: &str = "\nYou are in an ongoing conversation. Respond naturally to discussion and questions. When the user requests or implies action, carry out the work using your tools. Incoming user messages steer the active objective unless they explicitly replace it. Answer status questions without forgetting or falsely completing unfinished work. If reply_to_user is available, use it to answer a question or acknowledge steering while continuing work in this same turn; an informative message is not a request to stop. Finish with plain text when the conversation needs only a reply, work is done, or the user asks to pause or discuss before proceeding.\n";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
    Cancelling,
}

#[derive(Default)]
struct InboxState {
    messages: VecDeque<String>,
    delivering: usize,
    closed: bool,
    reply_only: bool,
    handoff: bool,
}

/// Only the root conversational agent receives this inbox; specialists cannot
/// steal user input. Delivery happens between model/tool turns.
#[derive(Clone, Default)]
pub struct ConversationInbox(Arc<Mutex<InboxState>>);

impl ConversationInbox {
    pub(crate) fn yield_to_input(&self) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).handoff = true;
    }

    pub(crate) fn is_handoff(&self) -> bool {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).handoff
    }

    /// Atomically either accept the next queued turn or close delivery. A
    /// message cannot be accepted into a gap between checking and closing.
    fn next_turn_or_close(&self) -> Option<String> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let next = state.messages.pop_front();
        if next.is_some() {
            state.reply_only = false;
            state.handoff = false;
        } else {
            state.closed = true;
        }
        next
    }
    pub(crate) fn mark_reply_only(&self) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).reply_only = true;
    }

    pub(crate) fn is_reply_only(&self) -> bool {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).reply_only
    }

    pub(crate) fn send(&self, input: String) -> Result<(), &'static str> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err("The previous turn is finishing. Your draft is preserved; send it again.");
        }
        if state.messages.len() + state.delivering >= MAX_PENDING_MESSAGES {
            return Err(
                "The conversation inbox is full. Your draft is preserved; wait for a reply.",
            );
        }
        state.messages.push_back(input);
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        !self.0.lock().unwrap_or_else(|error| error.into_inner()).messages.is_empty()
    }

    fn close(&self) -> Vec<String> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.messages.drain(..).collect()
    }

    /// Persist delivered messages before exposing them to the model. On a
    /// storage error they stay queued for recovery by the application.
    pub(crate) async fn receive(
        &self,
        store: Option<&Arc<dyn SessionStore>>,
        bus: Option<&EventBus>,
        session_id: Ulid,
        cancel: &CancellationToken,
    ) -> Result<Vec<Message>, OrchestratorError> {
        if cancel.is_cancelled() {
            return Err(OrchestratorError::Cancelled);
        }
        let inputs: Vec<String> = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            let inputs: Vec<_> = state.messages.drain(..).collect();
            state.delivering += inputs.len();
            inputs
        };
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let messages: Vec<Message> = inputs
            .iter()
            .map(|content| Message {
                role: Role::User,
                content: content.clone(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            })
            .collect();
        if let Some(store) = store {
            if let Err(error) = store.append_messages(session_id, &messages, cancel.clone()).await {
                let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
                state.delivering -= inputs.len();
                for input in inputs.into_iter().rev() {
                    state.messages.push_front(input);
                }
                return Err(OrchestratorError::AgentLoopError(format!(
                    "could not save conversation message: {error}"
                )));
            }
            if bus.is_none() {
                let transcript: Vec<_> = inputs
                    .iter()
                    .cloned()
                    .map(|content| TranscriptEntry::User { content })
                    .collect();
                if let Err(error) =
                    store.append_transcript(session_id, &transcript, cancel.clone()).await
                {
                    tracing::warn!(%error, "could not append delivered input to transcript; message history retained");
                }
            }
        }
        if let Some(bus) = bus {
            for content in &inputs {
                let _ = bus.publish_for_session(
                    session_id,
                    Ulid::new(),
                    EventKind::ConversationMessageReceived { content: content.clone() },
                );
            }
        }
        self.0.lock().unwrap_or_else(|error| error.into_inner()).delivering -= inputs.len();
        Ok(messages)
    }
}

/// Presentation-independent state of the currently selected conversation.
#[derive(Default)]
pub struct ConversationController {
    pub session_id: Option<Ulid>,
    pub status: RunStatus,
    pub checkpoint_json: Option<String>,
    pub cancel_token: CancellationToken,
    active_turn: Option<Ulid>,
    inbox: ConversationInbox,
}

pub enum Submission {
    Start(ConversationTurn),
    Delivered,
}

pub struct ConversationTurn {
    id: Ulid,
    input: String,
    session_id: Option<Ulid>,
    checkpoint_json: Option<String>,
    cancel: CancellationToken,
    inbox: ConversationInbox,
}

#[derive(Debug, Clone)]
pub struct ConversationCompletion {
    pub turn_id: Ulid,
    pub session_id: Option<Ulid>,
    pub result: Result<AgentOutput, ClassifiedFailure>,
    pub reply_only: bool,
    /// Accepted text that could not reach a decision boundary (cancel/error/
    /// iteration limit). Renderers restore it to the composer, never lose it.
    pub undelivered: Vec<String>,
}

impl ConversationCompletion {
    pub fn display_message(&self) -> String {
        match &self.result {
            Ok(output) if self.reply_only => output.final_message.clone(),
            Ok(output) => output.summary(),
            Err(failure) => failure.user_message.clone(),
        }
    }
}

pub struct ConversationOptions {
    pub project_dir: PathBuf,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub fast: bool,
}

impl ConversationController {
    pub fn submit_configured(
        &mut self,
        input: String,
        config: &concerto_config::AppConfig,
        options: &ConversationOptions,
    ) -> Result<Submission, String> {
        if !self.is_running() {
            if let Some(reason) = setup::validate_options(config, options) {
                return Err(format!(
                    "Cannot send: {reason}. Open Settings to finish provider setup."
                ));
            }
        }
        self.submit(input).map_err(str::to_owned)
    }

    pub fn is_running(&self) -> bool {
        self.status != RunStatus::Idle
    }

    pub fn submit(&mut self, input: String) -> Result<Submission, &'static str> {
        if input.trim().is_empty() {
            return Err("Write a message before sending.");
        }
        if input.len() > MAX_MESSAGE_BYTES {
            return Err("This message is too large. Attach a file or send a shorter message.");
        }
        if self.status == RunStatus::Cancelling {
            return Err(
                "Stopping the current work. Your draft is preserved; send it after work stops.",
            );
        }
        if self.status == RunStatus::Running {
            self.inbox.send(input)?;
            return Ok(Submission::Delivered);
        }
        self.cancel_token = CancellationToken::new();
        self.inbox = ConversationInbox::default();
        let id = Ulid::new();
        self.active_turn = Some(id);
        self.status = RunStatus::Running;
        Ok(Submission::Start(ConversationTurn {
            id,
            input,
            session_id: self.session_id,
            checkpoint_json: self.checkpoint_json.clone(),
            cancel: self.cancel_token.clone(),
            inbox: self.inbox.clone(),
        }))
    }

    pub fn cancel(&mut self) {
        if self.is_running() {
            self.status = RunStatus::Cancelling;
            self.cancel_token.cancel();
        }
    }

    /// Reject late completions after switching project/session. A renderer
    /// must call this before applying any completion to its display.
    pub fn settle(&mut self, completion: &ConversationCompletion) -> bool {
        if self.active_turn != Some(completion.turn_id) {
            return false;
        }
        self.active_turn = None;
        self.status = RunStatus::Idle;
        if let Some(id) = completion.session_id {
            self.session_id = Some(id);
        }
        if let Ok(output) = &completion.result {
            self.session_id = Some(output.session_id);
            self.checkpoint_json = output.checkpoint_json.clone();
        } else {
            // The cancellation/error path may have saved a newer checkpoint.
            // Let the runtime reload that durable row on the next message.
            self.checkpoint_json = None;
        }
        true
    }

    pub fn select_session(&mut self, session_id: Option<Ulid>) {
        self.cancel_token.cancel();
        self.inbox.close();
        let mut replacement = Self::default();
        replacement.session_id = session_id;
        *self = replacement;
    }
}

impl Drop for ConversationController {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

impl ConversationTurn {
    pub fn id(&self) -> Ulid {
        self.id
    }

    /// The single run-preparation path used by both presentation adapters.
    pub async fn execute(
        self,
        options: ConversationOptions,
        mut services: SharedServices,
    ) -> ConversationCompletion {
        let mut session_id = self.session_id;
        let mut pending_input = None;
        let result = async {
            let manager = match services.session_manager.clone() {
                Some(manager) => manager,
                None => Arc::new(
                    ProjectSessionManager::connect_with_config(SessionManagerConfig {
                        git_auto_init: services
                            .config
                            .tool_settings
                            .as_ref()
                            .map(|settings| settings.git_auto_init)
                            .unwrap_or(true),
                    })
                    .await
                    .map_err(|error| OrchestratorError::AgentLoopError(error.to_string()))?,
                ),
            };
            let project =
                camino::Utf8PathBuf::from_path_buf(options.project_dir.clone()).map_err(|_| {
                    OrchestratorError::AgentLoopError("Project path is not valid UTF-8".into())
                })?;
            let id = match session_id {
                Some(id) => id,
                None => {
                    manager
                        .create_new_session(
                            &project,
                            options.provider_id.as_deref().unwrap_or("default"),
                            options.model.as_deref().unwrap_or("default"),
                            self.cancel.clone(),
                        )
                        .await
                        .map_err(|error| OrchestratorError::AgentLoopError(error.to_string()))?
                        .session_id
                }
            };
            session_id = Some(id);
            services.session_manager = Some(manager.clone());
            services.conversation_inbox = Some(self.inbox.clone());
            let mut input = self.input;
            let mut checkpoint = self.checkpoint_json;
            // Usually the coordinator consumes the inbox inside its decision
            // loop. A graph batch or a finishing run can yield at a later
            // boundary; continue those messages here using the same session.
            for continuation in 0..=MAX_PENDING_MESSAGES {
                let history = manager
                    .load_recent_messages(id, self.cancel.clone())
                    .await
                    .map_err(|error| OrchestratorError::AgentLoopError(error.to_string()))?;
                let request =
                    RequestBuilder::new(input, options.project_dir.clone(), self.cancel.clone())
                        .with_provider_model(options.provider_id.clone(), options.model.clone())
                        .with_session(id, history)
                        .with_single_agent(false)
                        .with_memory_enabled(memory_enabled(
                            options.fast,
                            services.config.memory.enabled,
                        ))
                        .with_resume_checkpoint(checkpoint)
                        .build();
                let output = run_shared_agent(request, services.clone()).await?;
                pending_input = None;
                if self.cancel.is_cancelled()
                    || output.provider_error_class.is_some()
                    || continuation == MAX_PENDING_MESSAGES
                {
                    return Ok(output);
                }
                let handoff = self.inbox.is_handoff();
                let reply_only = self.inbox.is_reply_only();
                let Some(next) = self.inbox.next_turn_or_close() else {
                    return Ok(output);
                };
                // The previous runtime already persisted this answer and
                // stopped its recorder. Publish only for the live renderers.
                if !handoff {
                    let _ = services.bus.publish_for_session(
                        id,
                        output.task_id.0,
                        EventKind::AssistantMessage {
                            task_id: output.task_id,
                            content: if reply_only {
                                output.final_message
                            } else {
                                output.summary()
                            },
                        },
                    );
                }
                pending_input = Some(next.clone());
                input = next;
                checkpoint = output.checkpoint_json;
            }
            Err(OrchestratorError::AgentLoopError("Conversation turn limit reached".into()))
        }
        .await
        .map_err(ClassifiedFailure::from);
        let mut undelivered = self.inbox.close();
        if let Some(input) = pending_input {
            undelivered.insert(0, input);
        }
        ConversationCompletion {
            turn_id: self.id,
            session_id,
            result,
            reply_only: self.inbox.is_reply_only(),
            undelivered,
        }
    }
}

#[cfg(test)]
mod tests;
