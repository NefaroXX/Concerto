//! Project-scoped persistent session management for the live single-agent path.
//!
//! Wraps a `SessionStore` and tracks the active session per project directory
//! so follow-up prompts in the same project continue the same conversation.
//! Switching to a new session creates a fresh one and makes it active; the
//! project's indexed file memory is shared across all of its sessions.

use concerto_core::CancellationToken;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use concerto_core::ids::Ulid;
use concerto_core::types::{Message, ProviderMetrics};
use concerto_sessions::{Session, SessionError, SessionStore, SessionSummary, SqliteSessionStore};
use concerto_tools::git_init::{ensure_git_repo, GitInitOutcome};

/// A resolved active session for a project.
#[derive(Debug, Clone)]
pub struct ActiveProjectSession {
    pub session_id: Ulid,
    pub project_dir: Utf8PathBuf,
    pub provider: String,
    pub model: String,
}

/// Configuration for the session manager.
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Automatically `git init` a project directory at session start when it
    /// is not already inside a git repository (see
    /// `concerto_tools::git_init`). Defaults to true; opt out via
    /// `[tools] git_auto_init = false` in the app config.
    pub git_auto_init: bool,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self { git_auto_init: true }
    }
}

/// Manages persistent sessions scoped to a project directory.
pub struct ProjectSessionManager {
    store: Arc<dyn SessionStore>,
    config: SessionManagerConfig,
}

impl ProjectSessionManager {
    /// Connect to the default on-disk sessions database.
    pub async fn connect_default() -> Result<Self, SessionError> {
        let store = SqliteSessionStore::connect().await?;
        Ok(Self::from_store(Arc::new(store)))
    }

    /// Connect to the default on-disk sessions database with explicit config.
    pub async fn connect_with_config(config: SessionManagerConfig) -> Result<Self, SessionError> {
        let store = SqliteSessionStore::connect().await?;
        Ok(Self::new(Arc::new(store), config))
    }

    /// Wrap an existing session store with explicit session-manager config.
    pub fn new(store: Arc<dyn SessionStore>, config: SessionManagerConfig) -> Self {
        Self { store, config }
    }

    /// Wrap an existing session store without configuration.
    pub fn from_store(store: Arc<dyn SessionStore>) -> Self {
        Self { store, config: SessionManagerConfig::default() }
    }

    /// Return the active session for a project, creating one if none exists.
    pub async fn get_or_create_active_session(
        &self,
        project_dir: &Utf8Path,
        provider: &str,
        model: &str,
        cancel: CancellationToken,
    ) -> Result<ActiveProjectSession, SessionError> {
        // Ensure a brand-new project directory is a git repository before the
        // agent starts writing files (opt-out via `[tools] git_auto_init`,
        // default on). Never fails session setup: git absence or a failed
        // `git init` is logged and ignored — repos stay the user's domain.
        match ensure_git_repo(project_dir, self.config.git_auto_init) {
            GitInitOutcome::Initialized => {
                tracing::info!(%project_dir, "initialized git repository for project");
            }
            GitInitOutcome::GitUnavailable => {
                tracing::debug!(%project_dir, "git not available; skipping automatic git init");
            }
            GitInitOutcome::Failed(error) => {
                tracing::debug!(%project_dir, %error, "automatic git init failed; skipping");
            }
            GitInitOutcome::Disabled | GitInitOutcome::AlreadyInRepo => {}
        }

        if let Some(id) =
            self.store.get_active_session_for_project(project_dir, cancel.clone()).await?
        {
            if let Some(session) = self.store.load_session(id, cancel.clone()).await? {
                return Ok(ActiveProjectSession {
                    session_id: session.id,
                    project_dir: session.project_dir,
                    provider: session.provider,
                    model: session.model,
                });
            }
        }

        // No active mapping, or the mapped session is gone — create a fresh one.
        self.create_new_session(project_dir, provider, model, cancel).await
    }

    /// Always create a new session and make it the active one for the project.
    pub async fn create_new_session(
        &self,
        project_dir: &Utf8Path,
        provider: &str,
        model: &str,
        cancel: CancellationToken,
    ) -> Result<ActiveProjectSession, SessionError> {
        let session =
            self.store.create_session(project_dir, provider, model, cancel.clone()).await?;
        self.store.set_active_session_for_project(project_dir, session.id, cancel).await?;
        Ok(ActiveProjectSession {
            session_id: session.id,
            project_dir: session.project_dir,
            provider: session.provider,
            model: session.model,
        })
    }

    /// Load a session by id.
    pub async fn load_session(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Option<Session>, SessionError> {
        self.store.load_session(session_id, cancel).await
    }

    /// Load all persisted messages for a session in order.
    pub async fn load_recent_messages(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<Vec<Message>, SessionError> {
        self.store.load_messages(session_id, cancel).await
    }

    /// Append a single message to a session.
    pub async fn append_message(
        &self,
        session_id: Ulid,
        message: &Message,
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        self.store.append_messages(session_id, std::slice::from_ref(message), cancel).await
    }

    /// Append multiple messages to a session.
    pub async fn append_messages(
        &self,
        session_id: Ulid,
        messages: &[Message],
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        self.store.append_messages(session_id, messages, cancel).await
    }

    /// List sessions for a project, most recent first.
    pub async fn list_project_sessions(
        &self,
        project_dir: &Utf8Path,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        self.store.list_sessions_for_project(project_dir, limit, cancel).await
    }

    /// Record provider metrics against a session.
    pub async fn record_metrics(
        &self,
        session_id: Ulid,
        metrics: ProviderMetrics,
        cancel: CancellationToken,
    ) -> Result<(), SessionError> {
        self.store.record_metrics(session_id, metrics, cancel).await
    }

    /// List a session's spend records, oldest first.
    pub async fn list_spend_records(
        &self,
        session_id: Ulid,
    ) -> Result<Vec<concerto_sessions::spend::SpendRecord>, SessionError> {
        self.store.list_spend_records(session_id, CancellationToken::new()).await
    }

    /// Access the underlying store.
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use std::env::temp_dir;

    /// A unique project dir per test run so the on-disk active-session map
    /// doesn't collide with previous runs.
    fn unique_project_dir() -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(temp_dir().join(format!("concerto_test_{}", Ulid::new())))
            .unwrap()
    }

    async fn in_memory_manager() -> ProjectSessionManager {
        let store =
            SqliteSessionStore::connect_in_memory().await.expect("open in-memory sessions store");
        ProjectSessionManager::new(Arc::new(store), SessionManagerConfig::default())
    }

    #[test]
    fn session_manager_config_defaults_to_git_auto_init_on() {
        assert!(
            SessionManagerConfig::default().git_auto_init,
            "automatic git init must default to ON"
        );
    }

    #[tokio::test]
    async fn active_session_is_stable_and_switchable() {
        let mgr = in_memory_manager().await;
        let dir = unique_project_dir();

        // First resolve creates the active session...
        let a = mgr
            .get_or_create_active_session(&dir, "prov", "model", CancellationToken::new())
            .await
            .unwrap();
        // ...and a second resolve returns the SAME active session.
        let b = mgr
            .get_or_create_active_session(&dir, "prov", "model", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(a.session_id, b.session_id);

        // Starting a new session makes it the active one.
        let c =
            mgr.create_new_session(&dir, "prov", "model", CancellationToken::new()).await.unwrap();
        assert_ne!(a.session_id, c.session_id);

        let active = mgr
            .get_or_create_active_session(&dir, "prov", "model", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(active.session_id, c.session_id);
    }

    #[tokio::test]
    async fn list_project_sessions_includes_active() {
        let mgr = in_memory_manager().await;
        let dir = unique_project_dir();
        let created =
            mgr.create_new_session(&dir, "prov", "model", CancellationToken::new()).await.unwrap();

        let sessions = mgr.list_project_sessions(&dir, 10, CancellationToken::new()).await.unwrap();
        assert!(sessions.iter().any(|s| s.id == created.session_id));
    }
}
