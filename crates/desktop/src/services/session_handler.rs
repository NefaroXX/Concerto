//! Desktop glue between the UI and the project‑scoped session manager.
//!
//! Wraps `ProjectSessionManager` so the desktop can resolve the active session
//! for the current project, load prior conversation history, and start fresh
//! sessions — all backed by the on‑disk `SessionStore`.

use concerto_core::CancellationToken;
use std::path::Path;
use std::sync::Arc;

use camino::Utf8PathBuf;
use concerto_config::AppConfig;
use concerto_core::ids::Ulid;
use concerto_core::transcript::TranscriptEntry;
use concerto_core::types::Message;
use concerto_orchestrator::session_manager::{ProjectSessionManager, SessionManagerConfig};
use concerto_sessions::SessionError;

/// Build the session-manager config from the app config. A missing `[tools]`
/// section (or a missing config at all — e.g. `AppConfig::default()`) keeps
/// `git_auto_init` at its default ON behavior.
fn session_manager_config(config: &AppConfig) -> SessionManagerConfig {
    SessionManagerConfig {
        git_auto_init: config.tool_settings.as_ref().is_none_or(|settings| settings.git_auto_init),
    }
}

/// Desktop‑facing wrapper around [`ProjectSessionManager`].
pub struct DesktopSessionHandler {
    manager: Arc<ProjectSessionManager>,
}

impl DesktopSessionHandler {
    /// Open the default on‑disk session store.
    pub async fn connect_default() -> Result<Self, SessionError> {
        let manager = ProjectSessionManager::connect_default().await?;
        Ok(Self { manager: Arc::new(manager) })
    }

    /// Open the default on-disk session store, honoring the app config's
    /// `[tools] git_auto_init` flag for automatic repository initialization.
    pub async fn connect_with_config(config: &AppConfig) -> Result<Self, SessionError> {
        let manager =
            ProjectSessionManager::connect_with_config(session_manager_config(config)).await?;
        Ok(Self { manager: Arc::new(manager) })
    }

    /// Wrap an existing manager (e.g. a shared instance).
    pub fn new(manager: Arc<ProjectSessionManager>) -> Self {
        Self { manager }
    }

    /// Return (and remember) the active session for `project_dir`, creating one
    /// if none exists yet.
    pub async fn ensure_active_session(
        &self,
        project_dir: &Path,
        provider: &str,
        model: &str,
    ) -> Result<Ulid, SessionError> {
        let utf8 = Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .unwrap_or_else(|_| Utf8PathBuf::from("."));
        let active = self
            .manager
            .get_or_create_active_session(&utf8, provider, model, CancellationToken::new())
            .await?;
        Ok(active.session_id)
    }

    /// Load the complete durable message history for display when restoring a
    /// session. The manager's recent-message limit remains reserved for model
    /// context construction and must not truncate the visible transcript.
    pub async fn load_history(&self, session_id: Ulid) -> Result<Vec<Message>, SessionError> {
        self.manager.store().load_messages(session_id, CancellationToken::new()).await
    }

    /// Load the durable typed transcript (ADR-36) for display when restoring a
    /// session. Newer sessions persist the full typed transcript (user,
    /// assistant, thinking, correlated tool calls, activity, errors, summaries
    /// and the completion marker); legacy sessions return an empty vec so
    /// callers fall back to [`Self::load_history`].
    pub async fn load_transcript(
        &self,
        session_id: Ulid,
    ) -> Result<Vec<TranscriptEntry>, SessionError> {
        self.manager.store().load_transcript(session_id, CancellationToken::new()).await
    }

    /// Load the durable event stream used to rebuild session-scoped logs.
    pub async fn load_events(
        &self,
        session_id: Ulid,
    ) -> Result<Vec<concerto_sessions::replay::StoredEvent>, SessionError> {
        self.manager.store().load_events(session_id, CancellationToken::new()).await
    }

    /// Load the session's persisted spend log (one record per settled provider
    /// call), oldest first, for the spend-log modal.
    pub async fn list_spend_records(
        &self,
        session_id: Ulid,
    ) -> Result<Vec<concerto_sessions::spend::SpendRecord>, SessionError> {
        self.manager.list_spend_records(session_id).await
    }

    /// Make an existing session the active one for its project, so the next
    /// run (and `ensure_active_session`) resumes it. Used by the session picker.
    pub async fn set_active_session(
        &self,
        project_dir: &Path,
        session_id: Ulid,
    ) -> Result<(), SessionError> {
        let utf8 = Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .unwrap_or_else(|_| Utf8PathBuf::from("."));
        self.manager
            .store()
            .set_active_session_for_project(&utf8, session_id, CancellationToken::new())
            .await
    }

    /// Start a brand‑new session and make it the active one for the project.
    pub async fn new_session(
        &self,
        project_dir: &Path,
        provider: &str,
        model: &str,
    ) -> Result<Ulid, SessionError> {
        let utf8 = Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .unwrap_or_else(|_| Utf8PathBuf::from("."));
        let session = self
            .manager
            .create_new_session(&utf8, provider, model, CancellationToken::new())
            .await?;
        Ok(session.session_id)
    }

    /// Access the underlying manager (used to feed `SharedServices`).
    pub fn manager(&self) -> Arc<ProjectSessionManager> {
        self.manager.clone()
    }
}
