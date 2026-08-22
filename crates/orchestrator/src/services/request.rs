//! Builder for [`AgentRunRequest`].
//!
//! Reduces the ~15‑line inline construction in each frontend to a single
//! chain.

use std::path::PathBuf;

use concerto_core::ids::Ulid;
use concerto_core::types::Message;
use concerto_core::CancellationToken;

use crate::runtime_runner::AgentRunRequest;

/// Builder for [`AgentRunRequest`].
///
/// # Required
/// - `input`, `project_dir`, `cancel_token` — passed to [`new`](RequestBuilder::new).
///
/// # Optional
/// - [`with_provider_model`](RequestBuilder::with_provider_model) — provider and/or model override.
/// - [`with_session`](RequestBuilder::with_session) — session ID and conversation history.
/// - [`with_single_agent`](RequestBuilder::with_single_agent) — force single-agent mode (default: true).
/// - [`with_memory_enabled`](RequestBuilder::with_memory_enabled) — enable memory (default: true).
/// - [`with_resume_checkpoint`](RequestBuilder::with_resume_checkpoint) — serialised checkpoint for resumption.
///
/// # Build
/// - [`build`](RequestBuilder::build) — produces [`AgentRunRequest`].
pub struct RequestBuilder {
    input: String,
    selected_provider_id: Option<String>,
    selected_model: Option<String>,
    force_single_agent: bool,
    project_dir: PathBuf,
    session_id: Option<Ulid>,
    conversation_history: Vec<Message>,
    memory_enabled: bool,
    cancel_token: CancellationToken,
    resume_checkpoint_json: Option<String>,
}

impl RequestBuilder {
    /// Start building an agent run request.
    pub fn new(input: String, project_dir: PathBuf, cancel_token: CancellationToken) -> Self {
        Self {
            input,
            selected_provider_id: None,
            selected_model: None,
            force_single_agent: true,
            project_dir,
            session_id: None,
            conversation_history: Vec::new(),
            memory_enabled: true,
            cancel_token,
            resume_checkpoint_json: None,
        }
    }

    /// Override the provider and/or model.
    ///
    /// When `provider_id` is empty or `None`, the default provider from config
    /// is used.  When `model` is empty or `None`, the provider's default model
    /// is used.
    pub fn with_provider_model(
        mut self,
        provider_id: Option<String>,
        model: Option<String>,
    ) -> Self {
        self.selected_provider_id = provider_id.filter(|p| !p.is_empty());
        self.selected_model = model.filter(|m| !m.is_empty());
        self
    }

    /// Attach a session ID and its conversation history.
    pub fn with_session(mut self, session_id: Ulid, history: Vec<Message>) -> Self {
        self.session_id = Some(session_id);
        self.conversation_history = history;
        self
    }

    /// Force single-agent mode (default: true).
    ///
    /// When `false`, the coordinator multi-agent loop is used.
    pub fn with_single_agent(mut self, single: bool) -> Self {
        self.force_single_agent = single;
        self
    }

    /// Enable or disable memory (default: enabled).
    pub fn with_memory_enabled(mut self, enabled: bool) -> Self {
        self.memory_enabled = enabled;
        self
    }

    /// Attach a serialised checkpoint for resuming a previous partial run.
    pub fn with_resume_checkpoint(mut self, json: Option<String>) -> Self {
        self.resume_checkpoint_json = json;
        self
    }

    /// Consume the builder and produce an [`AgentRunRequest`].
    pub fn build(self) -> AgentRunRequest {
        AgentRunRequest {
            input: self.input,
            selected_provider_id: self.selected_provider_id,
            selected_model: self.selected_model,
            force_single_agent: self.force_single_agent,
            project_dir: self.project_dir,
            session_id: self.session_id,
            conversation_history: self.conversation_history,
            memory_enabled: self.memory_enabled,
            cancel_token: self.cancel_token,
            resume_checkpoint_json: self.resume_checkpoint_json,
        }
    }
}
