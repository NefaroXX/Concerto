//! Auto-Apply (A5) tail, StageTracker sequence, and apply-path checkpoint
//! coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24H): the ADR-55 §4 (M2) banner and the seven
//! remaining stage-tracker / auto-Apply tests from
//! `a5_unverifiable_session_newest_binding_falls_through_fail_soft` through
//! `apply_path_suppresses_stale_orchestration_checkpoint` move verbatim out of
//! `runtime_runner::runtime_runner_tests`, so test names and assertions are
//! unchanged. `use super::super::*;` keeps the parent (`runtime_runner_impl`)
//! items in scope. The harness fixtures these tests drive live in
//! `stage_harness.rs` (slice G) and are imported at that path; the sibling
//! `EventRecorderStore` mock comes from `super` (`tests::mod.rs`), as in the
//! donor.

use super::super::*;
use super::stage_harness::{
    drain_stage_events, make_executor, make_services, make_tool_call, FailingProvider,
    ScriptedProvider,
};
use super::EventRecorderStore;
use crate::plan_approval::plan_artifact_hash;
use concerto_core::transcript::GateLabels;

/// A5 (fail-soft leg): an unverifiable session-newest durable row —
/// tampered text or a legacy row without an artifact hash — falls through
/// to the generic intent gate (`Ok(None)`), never arms an auto-Apply.
#[tokio::test]
async fn a5_unverifiable_session_newest_binding_falls_through_fail_soft() {
    use concerto_sessions::SqliteSessionStore;

    let store: Arc<dyn SessionStore> =
        Arc::new(SqliteSessionStore::connect_in_memory().await.expect("in-memory store"));
    let session = Ulid::new();
    let created_at =
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
    // Tampered: text altered after creation, hash kept.
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-1".to_owned(),
                plan_id: "plan-1".to_owned(),
                plan_text: "step 1: build verdict AND delete everything".to_owned(),
                source_revision: Some("abc1234".to_owned()),
                artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                created_at,
            },
            CancellationToken::new(),
        )
        .await
        .expect("tampered save");

    let resolved =
        resolve_auto_apply_binding(session, "obj-hash-1", Some(&store), CancellationToken::new())
            .await
            .expect("resolution succeeds");
    assert!(resolved.is_none(), "an unverifiable durable row never auto-Applies (fail-soft)");

    // A legacy row without any artifact hash is unverifiable too.
    let session = Ulid::new();
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-2".to_owned(),
                plan_id: "plan-2".to_owned(),
                plan_text: "step 1: build verdict".to_owned(),
                source_revision: Some("abc1234".to_owned()),
                artifact_hash: None,
                created_at,
            },
            CancellationToken::new(),
        )
        .await
        .expect("legacy save");
    let resolved =
        resolve_auto_apply_binding(session, "obj-hash-2", Some(&store), CancellationToken::new())
            .await
            .expect("resolution succeeds");
    assert!(resolved.is_none(), "a legacy row without a hash falls through");
}

/// A5: no stored binding resolves `Ok(None)` — the unified loop proceeds
/// normally.
#[tokio::test]
async fn a5_no_binding_never_auto_applies() {
    let resolved = resolve_auto_apply_binding(
        Ulid::new(),
        "44444444444444444444444444444444",
        None,
        CancellationToken::new(),
    )
    .await
    .expect("resolution succeeds");
    assert!(resolved.is_none(), "no stored binding means no interception");
}

/// A5 scope: a stored binding for a DIFFERENT objective never
/// auto-Applies. Without keyword routing, only the exact objective can
/// safely arm an interception.
#[tokio::test]
async fn a5_binding_for_different_objective_never_auto_applies() {
    let store: Arc<dyn SessionStore> = Arc::new(
        concerto_sessions::SqliteSessionStore::connect_in_memory().await.expect("in-memory store"),
    );
    let session = Ulid::new();
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-original".to_owned(),
                plan_id: "plan-1".to_owned(),
                plan_text: "step 1: build verdict".to_owned(),
                source_revision: None,
                artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                    .expect("valid timestamp"),
            },
            CancellationToken::new(),
        )
        .await
        .expect("durable save");

    for objective in ["55555555555555555555555555555555", "88888888888888888888888888888888"] {
        let resolved =
            resolve_auto_apply_binding(session, objective, Some(&store), CancellationToken::new())
                .await
                .expect("resolution succeeds");
        assert!(
            resolved.is_none(),
            "a binding for a different objective never auto-Applies ({objective})"
        );
    }
}

/// End-to-end through real SQLite: a binding tampered IN STORAGE after
/// insert is rejected at rehydration — it must not be re-seeded into the
/// registry and must never auto-Apply.
#[tokio::test]
async fn rehydration_rejects_tampered_durable_binding() {
    use concerto_sessions::SqliteSessionStore;

    let store = SqliteSessionStore::connect_in_memory().await.expect("in-memory store");
    let session = Ulid::new();
    let created_at =
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-1".to_owned(),
                plan_id: "plan-1".to_owned(),
                plan_text: "step 1: build verdict".to_owned(),
                source_revision: Some("abc1234".to_owned()),
                artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                created_at,
            },
            CancellationToken::new(),
        )
        .await
        .expect("durable save");

    // Tamper the stored text while keeping the ORIGINAL artifact hash —
    // exactly what a corrupted or hand-edited database looks like.
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-1".to_owned(),
                plan_id: "plan-1".to_owned(),
                plan_text: "step 1: build verdict AND delete everything".to_owned(),
                source_revision: Some("abc1234".to_owned()),
                artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                created_at,
            },
            CancellationToken::new(),
        )
        .await
        .expect("tampered save");

    assert!(
        rehydrate_durable_binding(&store, session, CancellationToken::new()).await.is_none(),
        "a durable binding whose text no longer matches its artifact hash is not rehydrated"
    );
}

#[tokio::test]
async fn stage_tracker_emits_execute_sequence_for_action_required_run() {
    let bus = EventBus::new(256);
    let mut receiver = bus.subscribe();
    let session_id = Ulid::new();
    let store: Arc<dyn SessionStore> = Arc::new(EventRecorderStore::new());
    let event_recorder = start_event_recorder(&bus, store.clone(), session_id);
    let transcript_recorder =
        start_transcript_recorder(&bus, store.clone(), session_id, GateLabels::default());

    let services = make_services(bus.clone());
    let provider: Arc<dyn LlmProvider> =
        Arc::new(ScriptedProvider::new(vec![vec![make_tool_call("write_file", "edit")], vec![]]));
    let executor = make_executor();

    let task = AgentTask::new_action_required(session_id, "apply the fix");
    // `run_shared_agent` creates the tracker and seeds Understand before
    // dispatching; mirror that here so the full sequence is asserted.
    let stage_tracker = Arc::new(Mutex::new(StageTracker::new(bus, session_id, task.id)));
    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

    let project_dir = tempfile::tempdir().expect("tempdir for run");
    let req = AgentRunRequest {
        input: "apply the fix".into(),
        selected_provider_id: Some("scripted".into()),
        selected_model: Some("test-model".into()),
        force_single_agent: true,
        project_dir: project_dir.path().to_path_buf(),
        session_id: Some(session_id),
        conversation_history: Vec::new(),
        memory_enabled: false,
        cancel_token: CancellationToken::new(),
        resume_checkpoint_json: None,
    };

    let output = execute_agent_loop(
        req,
        &services,
        provider,
        "test-model".into(),
        None, // no advertised capability in this test
        executor,
        Arc::new(NullMemoryStore),
        None,
        session_id,
        task,
        event_recorder,
        transcript_recorder,
        RunEnvelope::Acting,
        true,
        &stage_tracker,
        None, // no fact-writer pool in this test
        Arc::new(crate::project_context::ProjectContext::disabled()),
    )
    .await
    .expect("action-required run should complete");

    assert_eq!(output.completion_status, AgentCompletionStatus::Completed);
    assert_eq!(
        drain_stage_events(&mut receiver, session_id),
        vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute, RunStage::Complete,],
        "action-required execute run reports Inspect -> Execute -> Complete"
    );
}

#[tokio::test]
async fn stage_tracker_omits_complete_when_run_fails() {
    let bus = EventBus::new(256);
    let mut receiver = bus.subscribe();
    let session_id = Ulid::new();
    let store: Arc<dyn SessionStore> = Arc::new(EventRecorderStore::new());
    let event_recorder = start_event_recorder(&bus, store.clone(), session_id);
    let transcript_recorder =
        start_transcript_recorder(&bus, store.clone(), session_id, GateLabels::default());

    let services = make_services(bus.clone());
    let provider: Arc<dyn LlmProvider> = Arc::new(FailingProvider);
    let executor = make_executor();

    let task = AgentTask::new_action_required(session_id, "apply the fix");
    let stage_tracker = Arc::new(Mutex::new(StageTracker::new(bus, session_id, task.id)));
    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

    let project_dir = tempfile::tempdir().expect("tempdir for run");
    let req = AgentRunRequest {
        input: "apply the fix".into(),
        selected_provider_id: Some("failing".into()),
        selected_model: Some("test-model".into()),
        force_single_agent: true,
        project_dir: project_dir.path().to_path_buf(),
        session_id: Some(session_id),
        conversation_history: Vec::new(),
        memory_enabled: false,
        cancel_token: CancellationToken::new(),
        resume_checkpoint_json: None,
    };

    let result = execute_agent_loop(
        req,
        &services,
        provider,
        "test-model".into(),
        None, // no advertised capability in this test
        executor,
        Arc::new(NullMemoryStore),
        None,
        session_id,
        task,
        event_recorder,
        transcript_recorder,
        RunEnvelope::Acting,
        true,
        &stage_tracker,
        None, // no fact-writer pool in this test
        Arc::new(crate::project_context::ProjectContext::disabled()),
    )
    .await;

    assert!(result.is_err(), "a non-transient provider error fails the run");
    assert_eq!(
        drain_stage_events(&mut receiver, session_id),
        vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute],
        "an errored run never reports Complete"
    );
}

// ------------------------------------------------------------------
// ADR-55 §4 (M2): plan-driven Execute must not silently resume a
// stale partial-graph checkpoint
// ------------------------------------------------------------------

#[tokio::test]
async fn apply_path_suppresses_stale_orchestration_checkpoint() {
    use concerto_sessions::{OrchestrationCheckpointRecord, SqliteSessionStore};

    let store = Arc::new(SqliteSessionStore::connect_in_memory().await.expect("in-memory store"));
    let store_dyn: Arc<dyn SessionStore> = store.clone();
    let manager = ProjectSessionManager::from_store(store_dyn.clone());

    // The checkpoint table FK-references the session row — create one
    // first (mirrors `get_or_create_active_session` in the runner) and
    // use its generated id.
    let project_dir = camino::Utf8PathBuf::from("/tmp/concerto-test-project");
    let session_id = store_dyn
        .create_session(&project_dir, "mock", "test-model", CancellationToken::new())
        .await
        .expect("create session")
        .id;

    // Seed a stale checkpoint from a previous partial Execute of the same
    // objective (fields mirror `persist_orchestration_checkpoint`).
    store_dyn
        .save_orchestration_checkpoint(&OrchestrationCheckpointRecord {
            session_id,
            run_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "test-project".into(),
            objective_hash: "h".into(),
            schema_version: 3,
            source_revision: Some("abc123".into()),
            sequence_num: 1,
            state_json: r#"{"partial":true}"#.into(),
            completed: false,
            updated_at: time::OffsetDateTime::now_utc(),
        })
        .await
        .expect("seed checkpoint");

    assert!(
        store_dyn
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("load seeded checkpoint")
            .is_some(),
        "the stale checkpoint must exist before the Apply path runs"
    );

    // The Apply path clears it (M2) so the run re-plans from the
    // approved plan instead of resuming the old partial graph.
    suppress_stale_checkpoint_for_apply(&manager, session_id, None, None).await;

    assert!(
        store_dyn
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("load after suppression")
            .is_none(),
        "the Apply path must clear the stale checkpoint"
    );
}
