//! Application state shared across route handlers.

use concerto_core::event::EventBus;
use concerto_sessions::SessionStore;
use std::sync::Arc;

/// Shared application state for the API server.
#[derive(Clone)]
pub struct AppState {
    pub bus: EventBus,
    pub store: Arc<dyn SessionStore>,
    /// Project-root allowlist (ADR-44 §1/§2). When non-empty, `create_session`
    /// refuses session roots outside these canonical roots with 403. Empty
    /// (the default) keeps local-first behavior permissive.
    pub project_roots: Vec<camino::Utf8PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::EventKind;
    use concerto_core::ids::Ulid;
    use concerto_core::transcript::TranscriptEntry;
    use concerto_core::types::{AgentTask, Message, ProviderMetrics, TaskId};
    use concerto_core::CancellationToken;
    use concerto_sessions::replay::StoredEvent;
    use concerto_sessions::spend::{SpendRecord, SpendSummary};
    use concerto_sessions::{Session, SessionError, SessionSummary};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ------------------------------------------------------------------
    // Minimal mock store for state tests
    // ------------------------------------------------------------------

    #[derive(Clone)]
    struct MockStore {
        sessions: Arc<Mutex<HashMap<Ulid, Session>>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self { sessions: Arc::new(Mutex::new(HashMap::new())) }
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for MockStore {
        async fn create_session(
            &self,
            project_dir: &camino::Utf8Path,
            provider: &str,
            model: &str,
            _cancel: CancellationToken,
        ) -> Result<Session, SessionError> {
            let session = Session {
                id: Ulid::new(),
                created_at: time::OffsetDateTime::now_utc(),
                project_dir: project_dir.to_path_buf(),
                provider: provider.to_string(),
                model: model.to_string(),
                total_tokens_in: 0,
                total_tokens_out: 0,
                total_cost_usd: 0.0,
            };
            self.sessions.lock().unwrap().insert(session.id, session.clone());
            Ok(session)
        }

        async fn load_session(
            &self,
            id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Option<Session>, SessionError> {
            Ok(self.sessions.lock().unwrap().get(&id).cloned())
        }

        async fn save_message(
            &self,
            _session_id: Ulid,
            _msg: &Message,
            _tokens_in: u64,
            _tokens_out: u64,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn append_messages(
            &self,
            _session_id: Ulid,
            _messages: &[Message],
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_messages(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<Message>, SessionError> {
            Ok(Vec::new())
        }

        async fn list_recent_sessions(
            &self,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions
                .values()
                .map(|s| SessionSummary {
                    id: s.id,
                    created_at: s.created_at,
                    provider: s.provider.clone(),
                    model: s.model.clone(),
                    message_count: 0,
                    total_cost_usd: s.total_cost_usd,
                    total_tokens_in: s.total_tokens_in,
                    total_tokens_out: s.total_tokens_out,
                })
                .collect())
        }

        async fn list_sessions_older_than(
            &self,
            _before_unix: i64,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn delete_session(
            &self,
            _id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<bool, SessionError> {
            Ok(false)
        }

        async fn active_session_ids(
            &self,
            _cancel: CancellationToken,
        ) -> Result<Vec<Ulid>, SessionError> {
            Ok(Vec::new())
        }

        async fn list_sessions_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn get_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _cancel: CancellationToken,
        ) -> Result<Option<Ulid>, SessionError> {
            Ok(None)
        }

        async fn set_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn record_metrics(
            &self,
            _session_id: Ulid,
            _metrics: ProviderMetrics,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn record_event(
            &self,
            _session_id: Ulid,
            _event: &concerto_core::event::Event,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_events(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<StoredEvent>, SessionError> {
            Ok(Vec::new())
        }

        async fn load_events_until(
            &self,
            _session_id: Ulid,
            _max_seq: u64,
            _cancel: CancellationToken,
        ) -> Result<Vec<StoredEvent>, SessionError> {
            Ok(Vec::new())
        }

        async fn record_spend(
            &self,
            _record: SpendRecord,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn list_spend_records(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<SpendRecord>, SessionError> {
            Ok(Vec::new())
        }

        async fn spend_summary(
            &self,
            session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<SpendSummary, SessionError> {
            let sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.get(&session_id) {
                Ok(SpendSummary {
                    session_id,
                    total_cost_usd: s.total_cost_usd,
                    total_tokens_in: s.total_tokens_in,
                    total_tokens_out: s.total_tokens_out,
                    record_count: 0,
                })
            } else {
                Err(SessionError::NotFound(session_id.to_string()))
            }
        }

        async fn create_task(
            &self,
            _task: &AgentTask,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn update_task_status(
            &self,
            _task_id: TaskId,
            _status: &str,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn get_task(
            &self,
            _task_id: TaskId,
            _cancel: CancellationToken,
        ) -> Result<Option<AgentTask>, SessionError> {
            Ok(None)
        }

        async fn list_tasks(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<AgentTask>, SessionError> {
            Ok(Vec::new())
        }

        async fn create_checkpoint(
            &self,
            _session_id: Ulid,
            _task_id: TaskId,
            _label: &str,
            _vfs_snapshot: &str,
            _sequence_num: u64,
            _cancel: CancellationToken,
        ) -> Result<Ulid, SessionError> {
            Ok(Ulid::new())
        }

        async fn load_checkpoint(
            &self,
            _checkpoint_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(String, u64), SessionError> {
            Err(SessionError::NotFound("no checkpoints in mock".into()))
        }

        async fn list_checkpoints(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::CheckpointSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn save_orchestration_checkpoint(
            &self,
            _record: &concerto_sessions::OrchestrationCheckpointRecord,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<Option<concerto_sessions::OrchestrationCheckpointRecord>, SessionError>
        {
            Ok(None)
        }

        async fn clear_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn append_transcript(
            &self,
            _session_id: Ulid,
            _entries: &[TranscriptEntry],
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_transcript(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<TranscriptEntry>, SessionError> {
            Ok(Vec::new())
        }
    }

    // ==================================================================
    // AppState tests
    // ==================================================================

    /// AppState can be created with an EventBus and a SessionStore.
    #[test]
    fn app_state_creation() {
        let bus = EventBus::default();
        let store = Arc::new(MockStore::new()) as Arc<dyn SessionStore>;
        let state = AppState { bus, store, project_roots: Vec::new() };
        // Smoke check: state is usable via its public fields
        assert!(Arc::strong_count(&state.store) >= 1);
        assert!(state.project_roots.is_empty());
    }

    /// AppState derives Clone and both instances share the same Arc<SessionStore>.
    #[test]
    fn app_state_clone() {
        let bus = EventBus::default();
        let store = Arc::new(MockStore::new()) as Arc<dyn SessionStore>;
        let state1 = AppState { bus, store, project_roots: Vec::new() };
        let state2 = state1.clone();
        // Both point to the same Arc<dyn SessionStore>
        assert!(Arc::ptr_eq(&state1.store, &state2.store));
    }

    /// AppState can hold a mock SessionStore and handlers can interact
    /// with it through the shared Arc.
    #[tokio::test]
    async fn app_state_with_mock_session_store() {
        let store = Arc::new(MockStore::new()) as Arc<dyn SessionStore>;
        let state =
            AppState { bus: EventBus::default(), store: store.clone(), project_roots: Vec::new() };

        // Use the store through state to verify it works
        let session = state
            .store
            .create_session(
                camino::Utf8Path::new("/tmp"),
                "test-provider",
                "test-model",
                CancellationToken::new(),
            )
            .await
            .expect("create_session should succeed in mock");
        assert_eq!(session.provider, "test-provider");
        assert_eq!(session.model, "test-model");

        // Verify the same session is visible through the Arc
        let loaded = store
            .load_session(session.id, CancellationToken::new())
            .await
            .expect("load_session should succeed")
            .expect("session should exist");
        assert_eq!(loaded.id, session.id);
    }

    /// AppState can hold an EventBus that is usable for publishing and
    /// subscribing to events.
    #[tokio::test]
    async fn app_state_with_mock_event_bus() {
        let bus = EventBus::new(16);
        let state = AppState {
            bus: bus.clone(),
            store: Arc::new(MockStore::new()),
            project_roots: Vec::new(),
        };

        // Subscribe to the bus via the state
        let mut rx = state.bus.subscribe();

        // Publish through the cloned bus (simulating what real code does)
        bus.publish_raw(EventKind::SessionSaved).expect("publish should succeed");

        // Verify the event is received through the state's bus subscription
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("recv should succeed");
        assert!(matches!(received.kind, EventKind::SessionSaved));
    }

    /// AppState implements `Send` and `Sync` so it can be shared across
    /// threads (required by axum).
    #[test]
    fn app_state_thread_safety() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<AppState>();
        assert_sync::<AppState>();
    }
}
