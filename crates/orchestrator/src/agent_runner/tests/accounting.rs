//! Attempt isolation for cloned fallback contexts.

use super::*;

#[tokio::test]
async fn cloned_fallback_context_must_not_charge_prior_failed_attempt_again() {
    let tracker = Arc::new(SpendTracker::new(None, None, None));
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(UsageThenFailAgent { id: AgentId::new("researcher") }));
    let runner = AgentRunner::new(Arc::new(registry), EventBus::new(64), tracker.clone());
    let directory = tempfile::tempdir().expect("disk-backed fixture directory");
    let session_id = Ulid::new();
    let context =
        AgentContext::new(SessionContext::new(session_id, directory.path().to_path_buf()));
    let task = make_task(TaskId::new(), session_id);
    for _ in 0..2 {
        runner
            .run(
                AgentId::new("researcher"),
                &task,
                context.clone(),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect_err("scripted later provider failure");
    }
    assert!(
        (tracker.session_total() - 0.02).abs() < 1e-9,
        "each failed attempt must settle only its own completed requests"
    );
    assert_eq!(context.agent_history.usage().measured_requests, 0, "caller state stays untouched");
}

struct ConcurrentUsageAgent {
    barrier: tokio::sync::Barrier,
}

#[async_trait::async_trait]
impl ExpertAgent for ConcurrentUsageAgent {
    fn id(&self) -> AgentId {
        AgentId::new("researcher")
    }
    fn capabilities(&self) -> concerto_core::types::CapabilitySet {
        Default::default()
    }
    async fn run(
        &self,
        _task: &SubTask,
        context: AgentContext,
        _model: &str,
        _cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        assert_eq!(context.agent_history.seeded()[0].content, "retained context");
        assert!(context.agent_history.produced().is_empty());
        context.agent_history.record_usage(1_000, 200, 0.01);
        // Both attempts finish their request before either runner can settle.
        self.barrier.wait().await;
        Err(OrchestratorError::Provider(ProviderError::AuthFailure))
    }
}

#[tokio::test]
async fn concurrent_context_clones_have_independent_usage_and_keep_the_seed() {
    let tracker = Arc::new(SpendTracker::new(None, None, None));
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(ConcurrentUsageAgent { barrier: tokio::sync::Barrier::new(2) }));
    let runner = AgentRunner::new(Arc::new(registry), EventBus::new(64), tracker.clone());
    let directory = tempfile::tempdir().unwrap();
    let session_id = Ulid::new();
    let mut context =
        AgentContext::new(SessionContext::new(session_id, directory.path().to_path_buf()));
    context.agent_history = concerto_core::types::AgentHistory::with_seed(vec![history_message(
        Role::User,
        "retained context",
    )]);
    context.agent_history.record_usage(9_000, 900, 1.0);
    let mut recorded = 0;
    context
        .agent_history
        .record_from(&[history_message(Role::Assistant, "prior attempt")], &mut recorded);
    let first = make_task(TaskId::new(), session_id);
    let second = make_task(TaskId::new(), session_id);
    let profile = make_profile();
    let outcomes = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            runner.run(
                AgentId::new("researcher"),
                &first,
                context.clone(),
                &profile,
                CancellationToken::new()
            ),
            runner.run(
                AgentId::new("researcher"),
                &second,
                context.clone(),
                &profile,
                CancellationToken::new()
            )
        )
    })
    .await
    .expect("concurrent attempts must reach the barrier");
    assert!(outcomes.0.is_err() && outcomes.1.is_err());
    assert!((tracker.session_total() - 0.02).abs() < 1e-9);
    assert_eq!(context.agent_history.usage().cost_usd, 1.0, "caller usage is never mutated");
    assert_eq!(context.agent_history.produced().len(), 1, "caller history stays untouched");
}

#[tokio::test]
async fn history_load_failure_does_not_reuse_previous_attempt_usage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.db");
    let store = Arc::new(SqliteSessionStore::connect_path(&path).await.unwrap());
    let session = store
        .create_session(camino::Utf8Path::new("."), "mock", "mock", CancellationToken::new())
        .await
        .unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("DROP TABLE agent_messages").execute(&pool).await.unwrap();
    pool.close().await;
    assert!(store
        .load_agent_messages(session.id, "researcher", CancellationToken::new())
        .await
        .is_err());
    let tracker = Arc::new(SpendTracker::new(None, None, None));
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(UsageThenFailAgent { id: AgentId::new("researcher") }));
    let runner = AgentRunner::new(Arc::new(registry), EventBus::new(64), tracker.clone())
        .with_session_store(Some(store.clone()));
    let context =
        AgentContext::new(SessionContext::new(session.id, directory.path().to_path_buf()));
    let task = make_task(TaskId::new(), session.id);
    for _ in 0..2 {
        runner
            .run(
                AgentId::new("researcher"),
                &task,
                context.clone(),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect_err("scripted provider error");
    }
    assert!((tracker.session_total() - 0.02).abs() < 1e-9);
    let records = store.list_spend_records(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| (record.cost_usd - 0.01).abs() < 1e-9));
}
