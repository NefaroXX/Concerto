//! Resume / continue-from-checkpoint coverage for [`super::CoordinatorAgent`].
//!
//! Mechanical extraction (R02): these tests moved verbatim out of the
//! `coordinator::tests` module so their names and assertions are unchanged.
//! `use super::*;` keeps them in scope for the shared `coordinator::tests`
//! fixtures, so no helper or assertion was rewritten.

use super::*;

#[tokio::test]
async fn resume_checkpoint_exhausted_blocked_returns_partial() {
    // ── 1. Minimal coordinator wiring (unused in the fast exit) ──
    let bus = EventBus::new(256);
    let spend_tracker = Arc::new(SpendTracker::default());
    let registry = Arc::new(AgentRegistry::new());
    let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig {
            pins: std::collections::HashMap::new(),
            ..Default::default()
        },
        EventBus::default(),
    ));
    let model_registry = Arc::new(ModelRegistry::from_profiles(vec![]));
    let model_selector = Arc::new(ModelSelector::new(model_registry, routing));

    let mut coordinator = CoordinatorAgent::new(
        registry,
        runner,
        model_selector,
        spend_tracker,
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    );

    // ── 2. Graph with a single blocked Coder subtask ────────────
    let mut graph = TaskGraph::new();
    let coder_id = TaskId::new();
    graph.add_subtask(SubTask {
        id: coder_id,
        parent_id: None,
        session_id: Ulid::new(),
        role: AgentId::new("coder"),
        description: "implement feature".into(),
        status: SubTaskStatus::Blocked,
        dependencies: vec![],
        deliverable: None,
        created_at: time::OffsetDateTime::now_utc(),
        completed_at: None,
    });

    let mut subtask_attempts = HashMap::new();
    subtask_attempts.insert(coder_id, DEFAULT_MAX_SUBTASK_ATTEMPTS);

    let task = AgentTask::new(Ulid::new(), "test task");
    let context =
        concerto_core::types::AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            std::env::current_dir().unwrap(),
        ));

    // ── 3. Execute graph — should hit Partial fast-exit ─────────
    let run_objective = task.description.clone();
    let run_objective_hash = blake3::hash(run_objective.as_bytes()).to_hex().to_string();
    let result = coordinator
        .execute_graph(
            task,
            context,
            CancellationToken::new(),
            graph,
            HashMap::new(), // completed_results
            0.0,            // total_cost
            0,              // total_tool_calls
            vec![],         // all_files
            vec![],         // provider_metrics
            subtask_attempts,
            HashMap::new(), // retry_feedback
            HashMap::new(), // model_assignments
            Vec::new(),     // action_ledger
            run_objective,
            run_objective_hash,
            Vec::new(), // loop_notes
            None,       // requested_user_input
            None,       // pending_approval
            None,       // dispatch_direct_answer
        )
        .await;

    assert!(result.is_ok(), "expected Ok(Partial), got error: {result:?}");
    let (output, _notes) = result.unwrap();
    assert_eq!(
        output.completion_status,
        concerto_core::types::AgentCompletionStatus::Partial,
        "resumed checkpoint with exhausted blocked task should yield Partial",
    );
    assert!(
        output.final_message.contains("exhausting recovery attempts"),
        "final_message should mention recovery exhaustion, got: {}",
        output.final_message,
    );
}

/// ADR-42 §4 resume semantics: a resumed run restores the ladder guards
/// from the checkpoint (see the `decompose_task` resume path), so a task
/// whose tier-1 default-model attempt already fired before an interruption
/// must NOT re-walk tier 1 after resume. Seed the `default_model_attempted`
/// guard — exactly what the checkpoint-restore does — and verify the
/// ladder jumps straight to tier 2 (coordinator takeover).
#[tokio::test]
async fn resume_with_default_model_guard_skips_tier1() {
    let bus = EventBus::new(256);
    // Response 1 fails hard; response 2 is consumed by the tier-2 takeover
    // dispatch (the resumed run's only ladder tier). A tier-1 re-dispatch
    // would additionally emit its `Using test/mid` queue note — its
    // absence proves tier 1 never ran.
    let architect = MockExpertAgent::sequence(
        AgentId::new("architect"),
        vec![err_auth(), ok_result("architect", "resumed via coordinator")],
    );
    let session_id = Ulid::new();
    // Tier 2 dispatches through the runner on the coordinator's pipe; the
    // planning provider's own output is not consulted.
    let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let mut coordinator = coordinator_for_ladder(
        bus.clone(),
        vec![architect],
        concerto_config::ModelPinConfig { default_model: Some("mid".into()), ..Default::default() },
        planning,
    );
    let (graph, task_id) = single_pending_graph(session_id, "architect");
    // Simulate a checkpoint restore: the tier-1 guard fired before the
    // interruption, so the resumed run must not re-dispatch the agent on
    // the default model.
    coordinator.default_model_attempted.insert(task_id);

    let (output, events) =
        run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new()).await;

    assert_eq!(
        output.completion_status,
        concerto_core::types::AgentCompletionStatus::Completed,
        "the resumed run should complete via tier 2 takeover, got: {:?}",
        output.completion_status,
    );
    // The tier-1 re-dispatch never happened: its `Using test/mid` queue
    // note was never published (the first dispatch used test/cheap, the
    // tier-2 dispatch uses the coordinator's planning profile).
    let tier1_redispatched = events.iter().any(|kind| {
        matches!(
            kind,
            EventKind::AgentThought { content, .. }
                if content.contains("Using test/mid") && content.contains("Queued subtask")
        )
    });
    assert!(!tier1_redispatched, "tier 1 must not be re-walked after a resume");
    let self_executed = output
        .provider_metrics
        .iter()
        .any(|metrics| metrics.provider == "coordinator-self-execute");
    assert!(self_executed, "expected the resumed run to take over the subtask in tier 2");
}

/// A checkpointless `continue` whose session log carries a hash-verified
/// `plan-approved` payload restores that CONTEXT, and the Coordinator
/// re-decides: here it dispatches the coder (the architect stays
/// untouched — a re-design would need its own recorded decision), and the
/// dispatch is a `Decision` event citing the REAL plan-approved event id
/// (ADR-65 §7; a fabricated id is rejected at append — acceptance 8).
#[tokio::test]
async fn headless_resume_dispatches_the_coder_not_the_architect() {
    let session_id = Ulid::new();
    let (_workspace, _store_dir, pool, snapshot, plan_event_id, seed) =
        headless_resume_fixture(session_id, true).await;
    let bus = EventBus::new(256);
    let mocks = vec![
        // Poisoned: a resume that re-enters design dispatches this and
        // fails the run — the assertion below catches the dispatch.
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
    ];
    // The Coordinator cites the plan-approved event id from its restored
    // context — a REAL id, accepted at append.
    let (output, events) = run_for_test(
        coordinator_with_grounded_turns(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            vec![
                CoordinatorTurn::Calls(vec![call_specialist_with(
                    "coder",
                    "implement",
                    None,
                    &[plan_event_id.as_str()],
                )]),
                CoordinatorTurn::Calls(vec![call_specialist("validator", "validate the build")]),
                CoordinatorTurn::Text("resumed build finished".into()),
            ],
            &["src/main.rs"],
        )
        .with_workspace_snapshot(snapshot)
        .with_review_store(Some(pool.clone()))
        .with_headless_resume_seed(seed),
        bus.clone(),
    )
    .await;

    assert!(
        !events.iter().any(|kind| matches!(
            kind,
            EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect"
        )),
        "the headless resume must never re-enter design (the architect is poisoned)"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::SubTaskStarted { role, .. } if role.as_str() == "coder"
        )),
        "the Coordinator re-decides the implement dispatch from the restored context"
    );
    assert_eq!(
        output.completion_status,
        concerto_core::types::AgentCompletionStatus::Completed,
        "the resumed build completes: {}",
        output.final_message
    );

    // The dispatch Decision cites the REAL plan-approved event id.
    let logged = concerto_sessions::whiteboard::load_whiteboard_events(
        &pool,
        &concerto_sessions::whiteboard::WhiteboardLoadOpts {
            after_gate_seq: 0,
            session_id: None,
            scope: None,
            limit: usize::MAX,
        },
    )
    .await
    .expect("whiteboard loads");
    let decisions: Vec<_> = logged
        .iter()
        .filter(|event| {
            event.kind == WhiteboardKind::Decision
                && event.payload.get("final_shape").is_none()
                && event.payload.get("selected_agent").is_some()
        })
        .collect();
    assert_eq!(decisions.len(), 2, "two recorded dispatch decisions: {decisions:?}");
    let decision = &decisions[0];
    assert_eq!(decision.payload["selected_agent"], "coder");
    let validation_decision = &decisions[1];
    assert_eq!(validation_decision.payload["selected_agent"], "validator");
    assert!(
        decision.payload["supporting_evidence_ids"]
            .as_array()
            .expect("supporting ids array")
            .iter()
            .any(|id| id == &plan_event_id),
        "the Decision cites the plan-approved event as evidence: {:?}",
        decision.payload
    );
}

/// A checkpointless `continue` whose evidence chain has NO research
/// facts: the Coordinator grounds the workspace FIRST (researcher),
/// then dispatches the implement step — a chained pair it chose from the
/// restored context. Both dispatches are recorded as `Decision` events
/// in order, and every cited evidence id is a REAL log row.
#[tokio::test]
async fn headless_resume_without_research_facts_explores_first_then_implements() {
    let session_id = Ulid::new();
    let (_workspace, _store_dir, pool, snapshot, _plan_event_id, seed) =
        headless_resume_fixture(session_id, false).await;
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
    ];
    let (output, events) = run_for_test(
        coordinator_with_grounded_turns(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            vec![
                CoordinatorTurn::Calls(vec![call_specialist("researcher", "ground first")]),
                CoordinatorTurn::Calls(vec![call_specialist("coder", "implement")]),
                CoordinatorTurn::Text("resumed with fresh evidence".into()),
            ],
            &["src/main.rs"],
        )
        .with_workspace_snapshot(snapshot)
        .with_review_store(Some(pool.clone()))
        .with_headless_resume_seed(seed),
        bus.clone(),
    )
    .await;

    assert!(
        !events.iter().any(|kind| matches!(
            kind,
            EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect"
        )),
        "no evidence-chain resume re-enters design"
    );
    let started: Vec<&AgentId> = events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::SubTaskStarted { role, .. } => Some(role),
            _ => None,
        })
        .collect();
    assert!(
        started.iter().any(|role| role.as_str() == "researcher"),
        "the Coordinator grounds first: {started:?}"
    );
    assert!(
        started.iter().any(|role| role.as_str() == "coder"),
        "the exploration is followed by the implement step: {started:?}"
    );

    // BOTH dispatches are recorded, in order, with real-evidence
    // discipline.
    let logged = concerto_sessions::whiteboard::load_whiteboard_events(
        &pool,
        &concerto_sessions::whiteboard::WhiteboardLoadOpts {
            after_gate_seq: 0,
            session_id: None,
            scope: None,
            limit: usize::MAX,
        },
    )
    .await
    .expect("whiteboard loads");
    let decisions: Vec<&concerto_sessions::whiteboard::WhiteboardEvent> = logged
        .iter()
        .filter(|event| {
            event.kind == WhiteboardKind::Decision
                && event.payload.get("final_shape").is_none()
                && event.payload.get("selected_agent").is_some()
        })
        .collect();
    assert_eq!(decisions.len(), 2, "explore + implement both recorded: {decisions:?}");
    assert_eq!(decisions[0].payload["selected_agent"], "researcher");
    assert_eq!(decisions[1].payload["selected_agent"], "coder");
    assert!(
        decisions.iter().all(|decision| decision.payload["supporting_evidence_ids"]
            .as_array()
            .is_none_or(|ids| ids
                .iter()
                .all(|id| logged.iter().any(|event| &event.event_id == id)))),
        "every cited evidence id is a REAL log row"
    );
    let _ = output;
}

/// Run-continuity Phase 1 (Task A): a resumed run keeps recording the
/// ORIGINAL objective text + hash in every checkpoint it persists — the
/// resume input ("continue") never replaces it, so a later resume still
/// names the same work.
#[tokio::test]
async fn resumed_run_keeps_original_objective_in_checkpoints() {
    let dir = tempfile::tempdir().expect("tempdir for test workspace");

    // ── Phase 1: fresh run stalls (review unresolved) ────────────
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
    ];
    let mut registry = AgentRegistry::from_mocks(mocks);
    registry.attach_configs_for_test(
        std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
    );
    registry.register(Arc::new(AlwaysRevise));
    let registry = Arc::new(registry);
    let (mut coordinator, store, session_id) = coordinator_with_store(
        bus.clone(),
        registry,
        vec![
            CoordinatorTurn::Calls(vec![
                call_specialist("architect", "design it"),
                call_specialist("coder", "implement"),
            ]),
            CoordinatorTurn::Calls(vec![call_specialist("reviewer", "review the work")]),
            CoordinatorTurn::Text("done".into()),
        ],
        dir.path(),
    )
    .await;
    let task = AgentTask::new(session_id, "build the thing");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let first = coordinator
        .run(task, context, CancellationToken::new(), None)
        .await
        .expect("first run should succeed");
    let stored = first.checkpoint_json.clone().expect("the stalled first run carries a checkpoint");

    // ── Phase 2: bare "continue" resumes from the stored checkpoint ──
    let bus2 = EventBus::new(256);
    // The architect is a canary: the resume path restores the graph and
    // must never re-derive it.
    let mocks2 = vec![
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
    ];
    let mut registry2 = AgentRegistry::from_mocks(mocks2);
    registry2.register(Arc::new(AlwaysRevise));
    let registry2 = Arc::new(registry2);
    // The resume phase shares phase 1's store and session row — the
    // checkpoint cycle must survive across runs on the same session.
    let mut coordinator2 = coordinator_on_store(
        bus2,
        registry2,
        vec![CoordinatorTurn::Text(String::new())],
        store.clone(),
    );
    let continue_task = AgentTask::new(session_id, "continue");
    let continue_ctx = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let second = coordinator2
        .run(continue_task, continue_ctx, CancellationToken::new(), Some(stored))
        .await
        .expect("resumed run should succeed");

    assert_eq!(
        second.completion_status,
        concerto_core::types::AgentCompletionStatus::Partial,
        "the resumed run stalls again on the unresolved review"
    );
    let record = store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("the stalled resume keeps its checkpoint");
    let cp: checkpoint::GraphCheckpoint =
        serde_json::from_str(&record.state_json).expect("valid resumed checkpoint");
    assert_eq!(cp.objective, "build the thing", "the original objective survives the resume");
    assert_eq!(
        cp.objective_hash,
        blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
        "the objective hash is the ORIGINAL objective's hash, not the resume input's"
    );
    // ADR-65 §7: the resumed run's checkpoints keep carrying the §7
    // state — the doc resolution captured by the verifier and the
    // snapshot generation both survive into the post-resume row.
    assert_eq!(
        cp.schema_version,
        checkpoint::GRAPH_CHECKPOINT_SCHEMA_VERSION,
        "resumed checkpoints are canonical v4"
    );
    assert!(
        cp.doc_resolution.is_some(),
        "the doc resolution captured at verify time rides every persist"
    );
}

/// Resume decisions are recorded with reason codes and REAL evidence
/// ids; a restore-and-continue resets its graph in place.
#[tokio::test]
async fn resume_continues_progressing_blocked_step_from_the_cursor() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // Pre-cursor: one failure fact BEFORE the cursor — checkpoint-era
    // state, never replayed into the evidence view.
    append_tool_fact(&pool, session_id, "ev-pre-fail", &subtask_id.to_string(), false).await;
    // Post-cursor: a successful tool execution — the agent made progress.
    append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true).await;

    let registry = Arc::new(AgentRegistry::new()); // no candidates
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry,
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        0,
        Some(1),
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds");

    // The step was re-armed for the SAME agent (facts show progress).
    let graph_task = result
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == subtask_id)
        .expect("restored step");
    assert_eq!(graph_task.status, SubTaskStatus::Pending, "Continue re-arms the step");
    assert_eq!(graph_task.role.as_str(), "coder", "progress ⇒ the same agent continues");
    assert!(
        result.action_ledger.iter().any(|entry| entry.kind == "resume-continued"),
        "the decision lands in the checkpoint ledger"
    );

    // The evidence view is the log AFTER the cursor: the pre-cursor
    // failure is not cited (no replay), the post-cursor progress fact
    // is.
    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let decisions: Vec<_> =
        logged.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
    assert_eq!(decisions.len(), 1, "exactly one resume decision, got: {decisions:?}");
    assert_eq!(decisions[0].payload["reason"], "resume-continue-blocked");
    assert_eq!(decisions[0].payload["selected_agent"], "coder");
    let cited: Vec<&str> = decisions[0].payload["supporting_evidence_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        cited,
        vec!["ev-post-progress"],
        "cited ids are the REAL post-cursor facts only (cursor respected), got: {cited:?}"
    );
}

/// ADR-65 acceptance 9: the blocked-step continuation loop completes with
/// vector memory entirely DISABLED, and no path ever writes a memory
/// entry (the store panics on any write — the run's success is the proof).
#[tokio::test]
async fn continuation_loop_completes_with_vector_memory_disabled() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // Pre-cursor failure (checkpoint-era, never replayed) + post-cursor
    // progress fact: the same evidence shape the Phase 7 continue test
    // uses.
    append_tool_fact(&pool, session_id, "ev-pre-fail", &subtask_id.to_string(), false).await;
    append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true).await;

    let registry = Arc::new(AgentRegistry::new()); // no candidates
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry,
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(ForbiddenMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        0,
        Some(1),
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds without vector memory");

    // The restore still reads the log, re-arms the step, and appends its
    // decision — projected memory was simply never consulted for state.
    let graph_task = result
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == subtask_id)
        .expect("restored step");
    assert_eq!(graph_task.status, SubTaskStatus::Pending, "Continue re-arms the step");
    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let decisions: Vec<_> =
        logged.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
    assert_eq!(decisions.len(), 1, "exactly one resume decision");
    assert_eq!(decisions[0].payload["selected_agent"], "coder");
    assert_eq!(decisions[0].payload["reason"], "resume-continue-blocked");
}

/// No progress ⇒ replace the agent, never a blind same-agent
/// re-dispatch (the 5-repeat live failure's first guard).
#[tokio::test]
async fn resume_replaces_the_blocked_step_agent_without_progress() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(MockExpertAgent::always_succeed(AgentId::new("coder"), "x")));
    registry.register(Arc::new(
        MockExpertAgent::always_succeed(AgentId::new("coder2"), "done")
            .with_stage(Some(AgentStage::new("implement"))),
    ));
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let registry = Arc::new(registry);
    let mut coordinator = CoordinatorAgent::new(
        registry.clone(),
        AgentRunner::new(registry, bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        1, // one failed outcome before the checkpoint
        None,
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds");

    let graph_task = result
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == subtask_id)
        .expect("restored step");
    assert_eq!(
        graph_task.role.as_str(),
        "coder2",
        "no progress ⇒ replace the agent, never a blind same-coder re-dispatch"
    );
    assert_eq!(graph_task.status, SubTaskStatus::Pending, "the replacement re-arms");

    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let decision = logged
        .iter()
        .find(|event| event.kind == WhiteboardKind::Decision)
        .expect("the replace decision is recorded");
    assert_eq!(decision.payload["reason"], "resume-replace-agent");
    assert_eq!(decision.payload["selected_agent"], "coder2");
    assert!(result.action_ledger.iter().any(|entry| entry.kind == "resume-replaced"));
}

/// Repeated identical failures with no alternative left ⇒ skip — the
/// bound that kills the documented 5-repeat failure: at most ONE bounded
/// same-agent continue is ever granted, then the step is skipped.
#[tokio::test]
async fn resume_skips_after_repeated_identical_failures() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // One more failure AFTER the checkpoint (2 in the ledger → 3 total),
    // a real row the decision may cite.
    let fact =
        append_tool_fact(&pool, session_id, "ev-post-fail", &subtask_id.to_string(), false).await;

    let registry = Arc::new(AgentRegistry::new()); // no alternative agent
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry,
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        2,
        Some(fact.gate_seq - 1),
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds");

    let graph_task = result
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == subtask_id)
        .expect("restored step");
    assert_eq!(
        graph_task.status,
        SubTaskStatus::Failed,
        "the step is skipped (honest terminal state), never re-dispatched again"
    );
    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let decision = logged
        .iter()
        .find(|event| event.kind == WhiteboardKind::Decision)
        .expect("the skip decision is recorded");
    assert_eq!(decision.payload["reason"], "resume-skip-step");
    // The skip cites the REAL post-cursor failure fact.
    assert_eq!(decision.payload["supporting_evidence_ids"], serde_json::json!(["ev-post-fail"]),);
    assert!(result.action_ledger.iter().any(|entry| entry.kind == "resume-skipped"));
}

/// Acceptance 7 (e2e): with NO recorded, evidence-backed decision
/// selecting the architect, a blocked architect step is never re-armed
/// for dispatch — and with a recorded scheduler decision (a logged
/// Decision row explicitly selecting the researcher) plus progress
/// facts, the dispatch IS allowed.
#[tokio::test]
async fn resume_never_dispatches_architect_or_researcher_without_recorded_decision() {
    // ── Negative: no recorded decision ⇒ the architect step stays
    //    terminated, never re-armed. ──────────────────────────────────
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // The architect is registered with its design stage so the
    // evaluation classifies the step as architect work (acceptance 7).
    let mut architect_registry = AgentRegistry::new();
    architect_registry
        .register(Arc::new(MockExpertAgent::always_succeed(AgentId::new("architect"), "x")));
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        Arc::new(architect_registry),
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "architect",
        "Failed",
        0,
        None,
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds");
    let step = result.graph.all_tasks().into_iter().next().expect("the blocked step");
    assert_ne!(
        step.status,
        SubTaskStatus::Pending,
        "the architect step is NEVER re-armed for dispatch without a recorded decision"
    );
    assert_eq!(step.status, SubTaskStatus::Failed, "the step is skipped instead");
    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let decision = logged
        .iter()
        .find(|event| event.kind == WhiteboardKind::Decision)
        .expect("the skip decision is recorded");
    assert_eq!(decision.payload["reason"], "resume-skip-step");

    // ── Positive: a recorded, evidence-backed decision (the Phase-6
    //    scheduler's logged dispatch decision) explicitly selects the
    //    researcher, and post-cursor progress facts exist: the resume
    //    continues the dispatch behind that decision. ─────────────────
    let researcher_id = Ulid::new();
    let progress = append_tool_fact(
        &pool,
        session_id,
        "ev-research-progress",
        &researcher_id.to_string(),
        true,
    )
    .await;
    // The scheduler's dispatch decision cites a REAL evidence id.
    let scheduled_decision = append_whiteboard_event(
        &pool,
        &NewWhiteboardEvent {
            event_id: "ev-scheduled-exploration".to_owned(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "selected_agent": "researcher",
                "reason": "evidence-gap-explore",
                "required_output": "Grounded fact inventory (tool reads only)",
                "supporting_evidence_ids": ["ev-research-progress"],
            }),
            pre_image_hash: None,
            created_at: 2,
        },
    )
    .await
    .expect("the scheduler decision is recorded");

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        researcher_id,
        "researcher",
        "Blocked",
        0,
        Some(progress.gate_seq - 1),
    );
    let result = coordinator
        .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
        .await
        .expect("restore succeeds");
    let step = result
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == researcher_id)
        .expect("the blocked researcher step");
    assert_eq!(
        step.status,
        SubTaskStatus::Pending,
        "with a recorded, evidence-backed decision the dispatch is allowed"
    );
    assert_eq!(step.role.as_str(), "researcher");
    let _ = scheduled_decision;
}

/// Run-continuity Phase 1 (Task D): a bare "continue" over a stored
/// checkpoint restores the graph AND the DesignDoc — the architect is
/// NOT re-invoked (the poisoned canary would fail the run if it were) —
/// and the resumed run completes and clears the checkpoint.
#[tokio::test]
async fn continue_with_stored_checkpoint_skips_architect() {
    let dir = tempfile::tempdir().expect("tempdir for test workspace");

    // ── Phase 1: fresh run stalls (review unresolved) ────────────
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
    ];
    let mut registry = AgentRegistry::from_mocks(mocks);
    registry.attach_configs_for_test(
        std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
    );
    registry.register(Arc::new(AlwaysRevise));
    let registry = Arc::new(registry);
    let (mut coordinator, store, session_id) = coordinator_with_store(
        bus.clone(),
        registry,
        vec![
            CoordinatorTurn::Calls(vec![
                call_specialist("architect", "design it"),
                call_specialist("coder", "implement"),
            ]),
            CoordinatorTurn::Calls(vec![call_specialist("reviewer", "review the work")]),
            CoordinatorTurn::Calls(vec![call_specialist("validator", "validate the build")]),
            CoordinatorTurn::Text("done".into()),
        ],
        dir.path(),
    )
    .await;
    let task = AgentTask::new(session_id, "build the thing");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let first = coordinator
        .run(task, context, CancellationToken::new(), None)
        .await
        .expect("first run should succeed");
    let stored = first.checkpoint_json.clone().expect("the stalled first run carries a checkpoint");

    // ── Phase 2: "continue" restores and completes ───────────────
    let bus2 = EventBus::new(256);
    let mut rx = bus2.subscribe();
    let mocks2 = vec![
        // Poisoned canary: any architect dispatch fails the run.
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
    ];
    // The resume phase shares phase 1's store and session row.
    let mut coordinator2 = coordinator_on_store(
        bus2,
        Arc::new(AgentRegistry::from_mocks(mocks2)),
        vec![CoordinatorTurn::Text(String::new())],
        store.clone(),
    );
    let continue_task = AgentTask::new(session_id, "continue");
    let continue_ctx = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let second = coordinator2
        .run(continue_task, continue_ctx, CancellationToken::new(), Some(stored))
        .await
        .expect("resumed run should succeed");

    assert_eq!(
        second.completion_status,
        concerto_core::types::AgentCompletionStatus::Completed,
        "the resumed run completes off the restored graph: {}",
        second.final_message
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    assert!(
        !events.iter().any(|kind| {
            matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect")
        }),
        "the architect must never be re-invoked on a checkpoint resume"
    );
    let doc =
        coordinator2.design_doc_snapshot().expect("the DesignDoc is restored from the checkpoint");
    assert_eq!(doc.goals, vec!["do the thing".to_owned()]);
    assert!(
        store
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("checkpoint store read")
            .is_none(),
        "the successful resumed run clears the checkpoint"
    );
}

/// DEFERRED #18 / TODO.md "Coordinator restart/resume": the end-to-end
/// claim the row still owes. Phase A stalls and persists, then its
/// coordinator is dropped — the in-memory `checkpoint_json` it returns is
/// deliberately thrown away. Phase B is a FRESH coordinator + bus over the
/// same session store that reloads `state_json` from the durable row
/// itself (the production post-restart flow in `runtime_runner`), and it
/// must
///
/// 1. never re-dispatch a subtask phase A already completed — the canary
///    registry is derived from the persisted `Completed` set, so any
///    re-dispatch fails the run — and
/// 2. reconcile the subtask that was still `Running` when the process
///    died to `Pending` and dispatch it exactly once (once, not zero:
///    a dropped step would hang the run).
///
/// (2) is the hard-kill case only: the cooperative cancel path normalizes
/// `Running` → `Pending` *before* persisting, so an orphaned `Running`
/// row can only exist when a process died mid-dispatch. The test writes
/// that durable row explicitly and resumes through the production
/// `checkpoint::restore_graph` path.
///
/// Boundary under test: the durable ROW, not the OS process — a second
/// process would need a second DB connection, and
/// `SqliteSessionStore::connect_path` is crate-private.
#[tokio::test]
async fn continue_after_restart_loads_durable_checkpoint_and_reconciles_running() {
    let dir = tempfile::tempdir().expect("tempdir for restart test");

    // ── Phase A: "process one" dispatches, stalls, persists ─────
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
    ];
    let mut registry = AgentRegistry::from_mocks(mocks);
    registry.attach_configs_for_test(
        std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
    );
    registry.register(Arc::new(AlwaysRevise));
    let registry = Arc::new(registry);
    let (mut coordinator, store, session_id) = coordinator_with_store(
        bus.clone(),
        registry,
        vec![
            CoordinatorTurn::Calls(vec![
                call_specialist("architect", "design it"),
                call_specialist("coder", "implement"),
            ]),
            CoordinatorTurn::Calls(vec![call_specialist("reviewer", "review the work")]),
            CoordinatorTurn::Calls(vec![call_specialist("validator", "validate the build")]),
            CoordinatorTurn::Text("done".into()),
        ],
        dir.path(),
    )
    .await;
    let task = AgentTask::new(session_id, "build the thing");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    coordinator
        .run(task, context, CancellationToken::new(), None)
        .await
        .expect("phase A run should succeed");
    // Process one dies: nothing it held in memory reaches phase B.
    drop(coordinator);

    // ── The durable row, rewritten as a hard kill would leave it ─
    let mut record = store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("the stalled run persisted a resumable checkpoint");
    assert!(!record.completed, "phase A left the run resumable");
    let mut graph =
        checkpoint::GraphCheckpoint::from_json(&record.state_json).expect("state_json loads");
    let statuses = graph
        .subtasks
        .iter()
        .map(|st| format!("{}:{}:{:?}", st.id, st.role, st.status))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        graph.subtasks.iter().any(|st| st.status == SubTaskStatus::Completed),
        "phase A completed at least one subtask (persisted: {statuses})"
    );
    // The hard-kill row: process one died while this step was in flight,
    // so the durable record carries `Running` and NO completion record
    // for it — the same "an interrupted call has no durable completion
    // record" rule the cooperative cancel path applies before persisting.
    let in_flight_index = graph
        .subtasks
        .iter()
        .position(|st| st.status == SubTaskStatus::Completed && st.role.as_str() != "architect")
        .or_else(|| graph.subtasks.iter().position(|st| st.status == SubTaskStatus::Completed))
        .unwrap_or_else(|| panic!("phase A completed at least one subtask ({statuses})"));
    let in_flight_id = graph.subtasks[in_flight_index].id;
    let in_flight_role = graph.subtasks[in_flight_index].role.clone();
    graph.subtasks[in_flight_index].status = SubTaskStatus::Running;
    graph.completed_results.remove(&in_flight_id);
    record.state_json = serde_json::to_string(&graph).expect("checkpoint reserializes");
    store.save_orchestration_checkpoint(&record).await.expect("rewrite the durable row");

    // ── Phase B: "process two" continues off the STORE only ─────
    let bus2 = EventBus::new(256);
    let mut rx = bus2.subscribe();
    // Canary registry derived from the persisted graph: a role whose
    // subtasks are ALL completed is poisoned (a re-dispatch fails the
    // run); every other role must be able to run.
    let mut phase2_mocks = Vec::new();
    let mut role_statuses: Vec<(AgentId, Vec<SubTaskStatus>)> = Vec::new();
    for subtask in &graph.subtasks {
        match role_statuses.iter_mut().find(|(role, _)| role == &subtask.role) {
            Some((_, statuses)) => statuses.push(subtask.status),
            None => role_statuses.push((subtask.role.clone(), vec![subtask.status])),
        }
    }
    for (role, statuses) in role_statuses {
        let only_completed = statuses.iter().all(|status| *status == SubTaskStatus::Completed);
        phase2_mocks.push(if only_completed {
            MockExpertAgent::always_fail(role, "completed step must not re-dispatch")
        } else {
            MockExpertAgent::always_succeed(role, "resumed work").with_artifact_writer()
        });
    }
    let resumed = store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("phase B reloads the durable row");
    assert_eq!(
        resumed.sequence_num, record.sequence_num,
        "phase B reads back the row phase A wrote"
    );
    let mut coordinator2 = coordinator_on_store(
        bus2,
        Arc::new(AgentRegistry::from_mocks(phase2_mocks)),
        vec![CoordinatorTurn::Text(String::new())],
        store.clone(),
    );
    let second = coordinator2
        .run(
            AgentTask::new(session_id, "continue"),
            AgentContext::new(concerto_core::types::SessionContext::new(
                session_id,
                dir.path().to_path_buf(),
            )),
            CancellationToken::new(),
            Some(resumed.state_json),
        )
        .await
        .expect("resumed run should succeed");

    assert_eq!(
        second.completion_status,
        concerto_core::types::AgentCompletionStatus::Completed,
        "the restarted run completes off the restored graph: {}",
        second.final_message
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    let started = |wanted: TaskId| {
        events
                .iter()
                .filter(|kind| matches!(kind, EventKind::SubTaskStarted { task_id, .. } if *task_id == wanted))
                .count()
    };
    for subtask in &graph.subtasks {
        if subtask.status == SubTaskStatus::Completed {
            assert_eq!(
                started(subtask.id),
                0,
                "completed subtask {} ({}) must not be re-dispatched after restart",
                subtask.id,
                subtask.role
            );
        }
    }
    assert_eq!(
        started(in_flight_id),
        1,
        "the orphaned Running step {} ({}) is reconciled to Pending and dispatched once",
        in_flight_id,
        in_flight_role
    );
    assert!(
        store
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("checkpoint store read")
            .is_none(),
        "the successful restarted run clears the checkpoint"
    );
}

/// Fail-soft fallback: a resume-shaped run with NO stored checkpoint
/// decomposes fresh — the Coordinator's decision loop runs and its
/// architect dispatch happens exactly once; the run completes like a
/// first run.
#[tokio::test]
async fn continue_without_checkpoint_falls_back_to_fresh_decompose() {
    let dir = tempfile::tempdir().expect("tempdir for test workspace");
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
    ];
    let mut registry = AgentRegistry::from_mocks(mocks);
    registry.attach_configs_for_test(
        std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
    );
    let (mut coordinator, store, session_id) = coordinator_with_store(
        bus.clone(),
        Arc::new(registry),
        vec![
            CoordinatorTurn::Calls(vec![
                call_specialist("architect", "design it"),
                call_specialist("coder", "implement"),
            ]),
            CoordinatorTurn::Calls(vec![call_specialist("validator", "validate the build")]),
            CoordinatorTurn::Text("done".into()),
        ],
        dir.path(),
    )
    .await;

    // A bare "continue" whose session holds NO orchestration checkpoint.
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let output = coordinator
        .run(task, context, CancellationToken::new(), None)
        .await
        .expect("coordinator run should succeed");

    assert_eq!(
        output.completion_status,
        concerto_core::types::AgentCompletionStatus::Completed,
        "the fallback run completes like a fresh run: {}",
        output.final_message
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    let architect_dispatches = events
            .iter()
            .filter(|kind| {
                matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect")
            })
            .count();
    assert_eq!(
        architect_dispatches, 1,
        "with nothing stored, a continue falls back to a fresh decompose (one architect pass)"
    );
    assert!(
        store
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("checkpoint store read")
            .is_none(),
        "the successful fallback run still clears its checkpoint"
    );
}

/// Issue #52: the stale-pending-decision resume gate. A checkpoint whose
/// pending decision (a fabricated evidence id or an unregistered
/// target) cannot stand: the resume deterministically forces Replan —
/// the restored graph is discarded for a FRESH decompose (never a model
// repair loop) — even though the step facts alone would have continued.
#[tokio::test]
async fn resume_with_stale_pending_decision_forces_replan() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // Post-cursor progress fact: WITHOUT the stale gate the resume
    // would continue behind it — the Replan below is attributable ONLY
    // to the stale pending decision.
    append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true).await;

    let registry = AgentRegistry::from_mocks(vec![
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not re-design"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
    ]);
    let registry = Arc::new(registry);
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry,
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    // The pending decision cites a FABRICATED id — stale by the exact
    // acceptance-8 predicate (per-id existence), validated BEFORE the
    // resume evaluation consumes the decision.
    let cp_json = blocked_step_checkpoint_json_with_pending(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        0,
        Some(1),
        serde_json::json!({
            "selected_agent": "coder",
            "reason": "resume-continue-blocked",
            "required_output": "the blocked work",
            "supporting_evidence_ids": ["ev-fabricated-0001"],
            "task_id": subtask_id.to_string(),
        }),
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let result = coordinator
        .restore_and_evaluate(&cp_json, &task, &context, &CancellationToken::new())
        .await
        .expect("stale pending never crashes the restore");

    // Forced Replan: the restored graph is thrown away (Ok(None)) — the
    // model is never asked to repair coordinator state, and the restored
    // step is never re-dispatched behind a stale decision.
    assert!(
        result.is_none(),
        "a stale pending decision forces Replan; the restored graph must not resume"
    );

    // No resume-decision decision event lands for a forced Replan (the
    // staleness gate runs BEFORE the resume evaluation writes anything).
    let logged = load_whiteboard_events(
        &pool,
        &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
    )
    .await
    .expect("log loads");
    let holder: Vec<serde_json::Value> = logged
        .iter()
        .filter(|event| event.payload.get("reason").is_some())
        .map(|event| event.payload.clone())
        .collect();
    assert!(
        !holder.iter().any(|payload| payload["reason"] == "resume-continue-blocked"),
        "a stale decision is never continued: {holder:?}"
    );
}

/// Issue #52 (control): a pending decision whose evidence is REAL and
/// whose target is on the roster re-validates cleanly — the resume
/// proceeds exactly like the pre-#52 evaluation (continue-behind).
#[tokio::test]
async fn resume_with_valid_pending_decision_still_continues() {
    let (_dir, pool) = resume_log_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = Ulid::new();
    let subtask_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // Post-cursor progress fact — the SAME id the pending decision cites.
    append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true).await;

    let registry = Arc::new(AgentRegistry::from_mocks(vec![MockExpertAgent::always_succeed(
        AgentId::new("coder"),
        "implemented",
    )]));
    let bus = EventBus::new(16);
    let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
        Arc::new(MockProvider::default());
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry,
        AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker.clone(),
        bus.clone(),
        provider,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool.clone()));

    let cp_json = blocked_step_checkpoint_json_with_pending(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        0,
        Some(1),
        serde_json::json!({
            "selected_agent": "coder",
            "reason": "resume-continue-blocked",
            "required_output": "the blocked work",
            "supporting_evidence_ids": ["ev-post-progress"],
            "task_id": subtask_id.to_string(),
        }),
    );
    let task = AgentTask::new(session_id, "continue");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let restored = coordinator
        .restore_and_evaluate(&cp_json, &task, &context, &CancellationToken::new())
        .await
        .expect("a valid pending decision still restores");
    assert!(restored.is_some(), "a FRESH pending decision does not force Replan");
    let restored = restored.expect("checked");
    let graph_task = restored
        .graph
        .all_tasks()
        .into_iter()
        .find(|subtask| subtask.id.0 == subtask_id)
        .expect("the restored step");
    assert_eq!(graph_task.status, SubTaskStatus::Pending, "continue re-arms the step");
}

/// Resume-drive regression (stop-before-coder): a freshly-restored run
/// whose graph still holds a PENDING implement step must ATTEMPT that
/// implement dispatch — not fall into the canned unattempted-implementation
/// Partial. Asserts on the dispatch attempt itself (`SubTaskStarted` for
/// the coder), never merely on the absence of the guard note.
#[tokio::test]
async fn resume_with_pending_implement_step_dispatches_the_coder() {
    let (_dir, pool) = resume_log_pool().await;
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), "designed"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
    ];
    let mut coordinator = coordinator_with_turns(
        bus.clone(),
        Arc::new(AgentRegistry::from_mocks(mocks)),
        vec![CoordinatorTurn::Text(String::new())],
    )
    .with_review_store(Some(pool.clone()))
    // The plan was approved: implementation is promised, so the completion
    // guard is armed exactly as it is on the live run.
    .with_run_shape_context(RunShapeContext { has_approved_plan: true, ..Default::default() });

    let workspace = tempfile::tempdir().expect("tempdir for test workspace");
    let session_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;
    let cp_json =
        stop_before_coder_checkpoint_json(&project_id, session_id, TaskId::new(), TaskId::new());

    let task = AgentTask::new_action_required(session_id, "build the thing");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let output = coordinator
        .run(task, context, CancellationToken::new(), Some(cp_json))
        .await
        .expect("resumed run should return an output");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    let coder_dispatched = events.iter().any(
        |kind| matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "coder"),
    );
    assert!(
        coder_dispatched,
        "a restored PENDING implement step must be dispatched on resume; the run ended \
             {:?} with message: {}",
        output.completion_status, output.final_message
    );
    assert!(
        !output.final_message.contains("Unattempted-implementation guard"),
        "the unattempted-implementation guard must not fire when dispatch was attempted: {}",
        output.final_message
    );
}

/// Resume-drive regression (stop-before-coder, settled shape): the
/// durable row the stop actually leaves behind holds ONLY the settled
/// design step — zero implement dispatches, no code artifact, every node
/// Completed. The resumed run must dispatch the coder from the restored
/// decision loop, not fall into the canned unattempted-implementation
/// Partial. Asserts on the dispatch attempt itself (`SubTaskStarted` for
/// the coder), never merely on the absence of the guard note.
#[tokio::test]
async fn resume_with_design_only_checkpoint_dispatches_the_pending_implement_step() {
    let (_dir, pool) = resume_log_pool().await;
    let bus = EventBus::new(256);
    let mut rx = bus.subscribe();
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), "designed"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
    ];
    let mut coordinator = coordinator_with_turns(
        bus.clone(),
        Arc::new(AgentRegistry::from_mocks(mocks)),
        vec![
            CoordinatorTurn::Calls(vec![call_specialist("coder", "implement the plan")]),
            CoordinatorTurn::Text("done".into()),
        ],
    )
    .with_review_store(Some(pool.clone()))
    .with_run_shape_context(RunShapeContext { has_approved_plan: true, ..Default::default() });

    let workspace = tempfile::tempdir().expect("tempdir for test workspace");
    let session_id = Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;
    let design_id = TaskId::new();
    let cp_json = design_only_checkpoint_json(&project_id, session_id, design_id);

    let task = AgentTask::new_action_required(session_id, "build the thing");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        workspace.path().to_path_buf(),
    ));
    let output = coordinator
        .run(task, context, CancellationToken::new(), Some(cp_json))
        .await
        .expect("resumed run should return an output");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    let coder_dispatched = events.iter().any(
        |kind| matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "coder"),
    );
    assert!(
        coder_dispatched,
        "a fully-settled-but-unattempted implement stage must be dispatched on resume; the \
             run ended {:?} with message: {}",
        output.completion_status, output.final_message
    );
    assert!(
        !output.final_message.contains("Unattempted-implementation guard"),
        "the unattempted-implementation guard must not fire when dispatch was attempted: {}",
        output.final_message
    );
    assert!(
        !completion_guard_decision(&pool, "completion-blocked-unattempted-implementation").await,
        "no unattempted-implementation verdict may be recorded once the dispatch was attempted"
    );
}

/// Requirement (b): the F3 resume reconciliation's external evidence
/// becomes explicit, conflict-flagged records instead of a one-shot
/// verdict — and re-detection of the same change upserts, not duplicates.
#[tokio::test]
async fn resume_f3_evidence_becomes_explicit_reconcilable_records() {
    let workspace = tempfile::tempdir().expect("workspace dir");
    let cancel = CancellationToken::new();
    external_change_fixture(workspace.path());

    let (_dir, pool) = resume_log_pool().await;
    let gate = transfer_test_gate(pool.clone(), workspace.path()).await;
    gate.restore_ownership_state(&crate::ownership::OwnershipState {
        records: vec![crate::ownership::OwnershipRecord {
            artifact: "src/held-resume.rs".to_owned(),
            owner: "coder".to_owned(),
            acquiring_event_id: "ev-held-resume".to_owned(),
            acquired_at_ms: 1,
            status: crate::ownership::OwnershipStatus::Owned,
        }],
    });

    let bus = EventBus::new(16);
    let mut coordinator = coordinator_with_turns(
        bus,
        Arc::new(AgentRegistry::from_mocks(vec![])),
        vec![CoordinatorTurn::Text(String::new())],
    )
    .with_workspace_snapshot(
        crate::workspace_snapshot::run_snapshot_barrier(
            None,
            workspace.path(),
            &Ulid::new().to_string(),
            &cancel,
        )
        .await
        .expect("snapshot re-captures for the coordinator"),
    )
    .with_write_gate(Some(gate.clone()));

    let task_id = TaskId::new();
    let mut graph = TaskGraph::new();
    graph.add_root(SubTask {
        id: task_id,
        parent_id: None,
        session_id: Ulid::new(),
        role: AgentId::new("coder"),
        description: "held work".into(),
        status: SubTaskStatus::Pending,
        dependencies: vec![],
        deliverable: Some("src/held-resume.rs".into()),
        created_at: time::OffsetDateTime::now_utc(),
        completed_at: None,
    });

    let change = resume::WorkspaceChange {
        generation_mismatch: true,
        externally_changed: vec![
            ("src/held-resume.rs".to_owned(), Some("ev-1".to_owned())),
            ("src/other.rs".to_owned(), None),
        ],
        plan_drift: Vec::new(),
    };
    coordinator
        .record_resume_external_changes(&change, &graph, Some("gen-checkpoint".to_owned()), &cancel)
        .await;

    let held = coordinator
        .external_changes
        .iter()
        .find(|record| record.first_path() == Some("src/held-resume.rs"))
        .expect("the F3 evidence for the held path becomes a record");
    assert_eq!(held.known_owner.as_deref(), Some("coder"));
    assert_eq!(held.known_task.as_deref(), Some(task_id.to_string().as_str()));
    assert!(held.conflicts_with_coordinator_work(), "held + changed externally conflicts");
    assert_eq!(held.previous_generation.as_deref(), Some("gen-checkpoint"));

    let other = coordinator
        .external_changes
        .iter()
        .find(|record| record.first_path() == Some("src/other.rs"))
        .expect("the unrelated F3 evidence becomes a record");
    assert_eq!(other.known_owner, None);
    assert!(!other.conflicts_with_coordinator_work(), "unrelated change never conflicts");

    assert_eq!(coordinator.external_changes.len(), 2);

    // A resumed run re-detects the SAME evidence (same previous
    // generation): upsert dedups instead of accumulating duplicates.
    coordinator
        .record_resume_external_changes(&change, &graph, Some("gen-checkpoint".to_owned()), &cancel)
        .await;
    assert_eq!(
        coordinator.external_changes.len(),
        2,
        "re-detecting the same change upserts, never duplicates"
    );

    // And the records ride the checkpoint projection (requirement 5).
    let context = coordinator.checkpoint_context(&HashMap::new(), &[]);
    assert_eq!(context.external_changes.len(), 2);
    let checkpoint = crate::checkpoint::build_checkpoint(
        &checkpoint::CheckpointScope {
            run_id: Ulid::new(),
            session_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "test".into(),
            objective: "test objective".into(),
            objective_hash: "hash".into(),
            source_revision: None,
            sequence_num: 0,
        },
        checkpoint::CheckpointStage::Executing,
        None,
        &concerto_core::memory::WorkingMemorySnapshot {
            id: Ulid::new(),
            session_id: Ulid::new(),
            decisions: vec![],
            task_tree: vec![],
            created_at: time::OffsetDateTime::now_utc(),
        },
        &graph,
        &HashMap::new(),
        0.0,
        0,
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &context,
    );
    let json = serde_json::to_string(&checkpoint).expect("checkpoint serializes");
    let restored = crate::checkpoint::GraphCheckpoint::from_json(&json).expect("checkpoint loads");
    assert_eq!(restored.external_changes.len(), 2, "records survive the checkpoint JSON");
}

// ---------------------------------------------------------------------------
// D-41r: repeated interrupt/resume stress over a STORAGE-MEDIATED substrate
// ---------------------------------------------------------------------------

/// Open a real SQLite memory database in `dir` (the production `MemoryDb`
/// constructor, migrations included) — the storage the decision/task stores
/// are hydrated from, so a resume reads run 1's decisions from DISK.
async fn open_test_memory_db(dir: &std::path::Path) -> concerto_memory::storage::MemoryDb {
    let db_path = camino::Utf8PathBuf::from_path_buf(dir.join("h18_memory.db"))
        .expect("memory db path is UTF-8");
    concerto_memory::storage::MemoryDb::connect(&db_path)
        .await
        .expect("memory db opens (migrations run)")
}

/// A FRESH `DecisionStore`/`TaskTreeStore` pair hydrated from the shared
/// memory DB — exactly the production construction (`DecisionStore::load`,
/// `TaskTreeStore::load`), never a shared in-memory map. Each resume cycle
/// builds its own pair so its grounding can only come from storage.
async fn storage_backed_stores(
    mem_db: &Arc<concerto_memory::storage::MemoryDb>,
) -> (
    Arc<concerto_memory::decision_store::DecisionStore>,
    Arc<concerto_memory::task_tree::TaskTreeStore>,
) {
    let decisions = Arc::new(
        concerto_memory::decision_store::DecisionStore::load(mem_db.clone())
            .await
            .expect("decision store hydrates from SQLite"),
    );
    let task_tree = Arc::new(
        concerto_memory::task_tree::TaskTreeStore::load(mem_db.clone())
            .await
            .expect("task tree hydrates from SQLite"),
    );
    (decisions, task_tree)
}

/// Poll the disk view until `pred` holds. The decision write-back is detached
/// onto the runtime (`spawn_persistence`), so the read-back must wait for it;
/// the poll is bounded (≤512 × 1 ms) and deterministic in outcome.
async fn wait_for_decisions(
    mem_db: &concerto_memory::storage::MemoryDb,
    pred: impl Fn(&[concerto_core::memory::Decision]) -> bool,
) -> Vec<concerto_core::memory::Decision> {
    let mut rows = Vec::new();
    for _ in 0..512 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        rows = mem_db.list_decisions().await.expect("list decisions");
        if pred(&rows) {
            break;
        }
    }
    rows
}

/// A resumed run must never read as `Completed`; a recoverable stop surfaces
/// as `Partial`, and a hard stop as `Err(Cancelled)`.
fn assert_honestly_interrupted(outcome: &Result<AgentOutput, OrchestratorError>, cycle: &str) {
    match outcome {
        Ok(output) => assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "{cycle}: an interrupted run must not read as Completed: {}",
            output.final_message
        ),
        Err(OrchestratorError::Cancelled) => {}
        Err(other) => panic!("{cycle}: unexpected interrupt outcome: {other:?}"),
    }
}

/// D-41r (4): the Phase-0 grounding (a) cites the architect decision that was
/// persisted to SQLite and (b) keeps its plan-scoped chunks standing-free by
/// construction (`interrupted == false`, `degraded == false`) — the deliberate
/// Option-B decision documented on `CoordinatorAgent::retrieve_memory_context`.
fn assert_grounded_in_persisted_decision(chunks: &[EvidentChunk], cycle: &str) {
    assert!(!chunks.is_empty(), "{cycle}: Phase-0 grounding must not be empty");
    assert!(
        chunks.iter().any(|evident| evident.chunk.content.contains("architect completed subtask")),
        "{cycle}: grounding must cite the STORAGE-persisted architect decision: {chunks:?}"
    );
    assert!(
        chunks.iter().all(|evident| evident.chunk.id.starts_with("plan://")),
        "{cycle}: storage-mediated grounding is the plan-scoped projection: {chunks:?}"
    );
    assert!(
        chunks.iter().all(|evident| !evident.interrupted && !evident.degraded),
        "{cycle}: standing-free plan chunks carry no interrupted/degraded flag: {chunks:?}"
    );
}

/// The checkpoint objective, read through the production checkpoint parser.
fn checkpoint_objective(state_json: &str) -> String {
    checkpoint::GraphCheckpoint::from_json(state_json)
        .expect("the persisted checkpoint parses")
        .objective
}

/// The checkpoint's objective hash, read through the production parser.
fn checkpoint_objective_hash(state_json: &str) -> String {
    checkpoint::GraphCheckpoint::from_json(state_json)
        .expect("the persisted checkpoint parses")
        .objective_hash
}

/// How many persisted decisions match `session_id` and contain `needle`.
fn decision_count_for(
    decisions: &[concerto_core::memory::Decision],
    session_id: Ulid,
    needle: &str,
) -> usize {
    decisions
        .iter()
        .filter(|decision| decision.session_id == session_id && decision.what.contains(needle))
        .count()
}

/// Populate a fresh coordinator's Phase-0 context exactly as `run()` does for
/// a resume (the checkpoint's run-id seed), returning the task + context the
/// caller then drives the run with. Because the context is pre-populated,
/// `run()` will not re-derive it — the grounding asserted here IS the
/// grounding the run uses.
async fn phase0_grounding(
    coordinator: &mut CoordinatorAgent,
    session_id: Ulid,
    dir: &std::path::Path,
    checkpoint_json: Option<&str>,
) -> (AgentTask, AgentContext) {
    let seed = CoordinatorAgent::checkpoint_run_id_hint(checkpoint_json);
    let task = AgentTask::new(session_id, "continue");
    let mut context =
        AgentContext::new(concerto_core::types::SessionContext::new(session_id, dir.to_path_buf()));
    coordinator
        .retrieve_memory_context(&task, &mut context, seed.as_deref(), CancellationToken::new())
        .await;
    (task, context)
}

/// H12 / DEFERRED row 41 (D-41r) — REPEATED interrupt/resume stress on one
/// session, over a STORAGE-MEDIATED decision substrate.
///
/// The original H12 evidence was a single interrupt/resume cycle whose
/// decision store was `DecisionStore::new()` — in-memory and `Arc`-shared
/// between the two coordinators — so only the checkpoint half of the resume
/// was storage-mediated. This test closes the recorded residuals:
///
/// * **N = 3 consecutive interrupt/resume cycles** on the same session,
///   interrupting at a different point in the run lifecycle each cycle, each
///   on a DISTINCT planner-issued implement node so the resumed planner always
///   has fresh work:
///   (1) the fresh run's `coder` interrupted **mid-dispatch** — the call is
///   issued and never settles (`Err(Cancelled)`);
///   (2) the first resume's `coder_b` interrupted **during the resume**,
///   mid-dispatch;
///   (3) the second resume's `coder_c` interrupted **after dispatch, before
///   settlement** — the call settles while the operator stop wins the race
///   (the C4 crash-window, `Ok(Success)`).
///
///   The settle-boundary race durably records the node `Completed`, after
///   which the resumed run closes instead of re-entering the planner — so it
///   is the FINAL cycle. That is a real boundary of the resume-drive
///   semantics, not a test artifact: cycles that must leave re-armable work
///   interrupt BEFORE a node settles (cycles 1 and 2).
/// * **A real SQLite decision substrate**: run 1's settled architect decision
///   is persisted through `DecisionStore::load(MemoryDb)`; the next cycle
///   reads it back through a FRESHLY-hydrated `DecisionStore`; and Phase-0
///   grounding is asserted to cite that persisted text — never a shared
///   in-memory map.
/// * **No silent drift**: every cycle ends honestly interrupted (never
///   `Completed`) with a resumable checkpoint; the checkpoint's objective and
///   hash never change; the settled architect decision is never re-dispatched
///   (a zero-invocation canary) and never duplicated.
///
/// The specialists stay mocks **by construction**: the interrupt must be a
/// deterministic, in-process operator stop (`MockExpertAgent::cancel_on_run`)
/// — no live provider can be driven from CI, and a failing specialist is what
/// forces the interrupt. What this test proves is not that the specialist is
/// real but that the decision/memory substrate it reads IS storage-backed and
/// real.
#[tokio::test]
async fn interrupt_midrun_then_resume_stays_memory_grounded() {
    let dir = tempfile::tempdir().expect("tempdir for interrupt/resume workspace");
    // The SQLite storage every cycle's decision/task stores are hydrated from.
    let mem_db = Arc::new(open_test_memory_db(dir.path()).await);

    // ── Cycle 1: fresh run, interrupted MID-DISPATCH ─────────────
    let stop1 = CancellationToken::new();
    let coder1 =
        MockExpertAgent::sequence(AgentId::new("coder"), vec![Err(OrchestratorError::Cancelled)])
            .cancel_on_run(stop1.clone());
    let coder_calls1 = coder1.call_count_handle();
    let registry1 = {
        let mut registry = AgentRegistry::from_mocks(vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            // The call is issued and cancelled before it ever settles.
            coder1,
        ]);
        registry.attach_configs_for_test(
            std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
        );
        Arc::new(registry)
    };
    let (ds1, tt1) = storage_backed_stores(&mem_db).await;
    // The in-memory session/checkpoint store is shared across every cycle so
    // each resume matches the SAME session row (a resume must match its row).
    let (coordinator1, session_store, session_id) = coordinator_with_store(
        EventBus::new(256),
        registry1,
        vec![
            CoordinatorTurn::Calls(vec![call_specialist("architect", "design it")]),
            CoordinatorTurn::Calls(vec![call_specialist("coder", "implement")]),
            CoordinatorTurn::Text("done".into()),
        ],
        dir.path(),
    )
    .await;
    let mut coordinator1 = coordinator1.with_memory_writeback(Some(ds1), Some(tt1));

    let task1 = AgentTask::new(session_id, "build the thing");
    let context1 = AgentContext::new(concerto_core::types::SessionContext::new(
        session_id,
        dir.path().to_path_buf(),
    ));
    let first = coordinator1.run(task1, context1, stop1.clone(), None).await;
    assert_honestly_interrupted(&first, "cycle 1 (mid-dispatch)");
    assert!(
        coder_calls1.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "cycle 1 must interrupt the coder WHILE its dispatch is in flight"
    );

    // Run 1's settled architect decision is on DISK — read it back through the
    // production SQLite view, then confirm a fresh store hydrates it.
    let persisted = wait_for_decisions(&mem_db, |rows| !rows.is_empty()).await;
    assert!(
        persisted.iter().any(|decision| decision.session_id == session_id
            && decision.what.contains("architect completed subtask")),
        "run 1's settled decision survives to disk: {persisted:?}"
    );
    let record1 = session_store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("cycle 1 persisted a resumable checkpoint");
    assert!(!record1.completed, "cycle 1 stays resumable");
    assert_eq!(
        checkpoint_objective(&record1.state_json),
        "build the thing",
        "the checkpoint objective never drifts"
    );
    assert_eq!(
        checkpoint_objective_hash(&record1.state_json),
        blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
        "the checkpoint objective hash is the ORIGINAL objective's hash"
    );

    // ── Cycle 2: resume 1 — the planner issues a fresh implement node
    //    (`coder_b`) and the operator stop lands MID-DISPATCH ────────────
    let stop2 = CancellationToken::new();
    let architect_canary2 =
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched");
    let architect_calls2 = architect_canary2.call_count_handle();
    // Mid-dispatch stop on the resume re-dispatch: the call is issued and
    // cancelled before it ever settles. A DISTINCT implement role (`coder_b`)
    // guarantees the resumed planner has fresh work to issue.
    let coder2 =
        MockExpertAgent::sequence(AgentId::new("coder_b"), vec![Err(OrchestratorError::Cancelled)])
            .with_stage(Some(concerto_core::types::AgentStage::new("implement")))
            .cancel_on_run(stop2.clone());
    let coder_calls2 = coder2.call_count_handle();
    let registry2 = Arc::new(AgentRegistry::from_mocks(vec![architect_canary2, coder2]));
    let (ds2, tt2) = storage_backed_stores(&mem_db).await;
    let mut coordinator2 = coordinator_on_store(
        EventBus::new(256),
        registry2,
        vec![
            CoordinatorTurn::Calls(vec![call_specialist("coder_b", "implement")]),
            CoordinatorTurn::Text("continuing".into()),
        ],
        session_store.clone(),
    )
    .with_memory_writeback(Some(ds2), Some(tt2));
    let (resume_task2, resume_ctx2) =
        phase0_grounding(&mut coordinator2, session_id, dir.path(), Some(&record1.state_json))
            .await;
    assert_grounded_in_persisted_decision(&resume_ctx2.retrieved_chunks, "cycle 2");
    let second =
        coordinator2.run(resume_task2, resume_ctx2, stop2.clone(), Some(record1.state_json)).await;
    assert_honestly_interrupted(&second, "cycle 2 (mid-dispatch on resume)");
    assert!(
        coder_calls2.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "cycle 2 must dispatch coder_b from the resumed planner"
    );
    assert_eq!(
        architect_calls2.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the settled design step must never re-dispatch on resume"
    );
    let record2 = session_store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("cycle 2 persisted a resumable checkpoint");
    assert_eq!(
        checkpoint_objective(&record2.state_json),
        "build the thing",
        "the checkpoint objective never drifts across cycles"
    );
    let decisions2 = wait_for_decisions(&mem_db, |rows| !rows.is_empty()).await;
    assert_eq!(
        decision_count_for(&decisions2, session_id, "architect completed subtask"),
        1,
        "the architect decision is persisted exactly once, never duplicated: {decisions2:?}"
    );

    // ── Cycle 3: resume 2 — the planner issues a fresh implement node
    //    (`coder_c`) and the call SETTLES while the stop wins the race ────
    let stop3 = CancellationToken::new();
    let architect_canary3 =
        MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched");
    let architect_calls3 = architect_canary3.call_count_handle();
    let coder3 = MockExpertAgent::always_succeed(AgentId::new("coder_c"), "implemented")
        .with_stage(Some(concerto_core::types::AgentStage::new("implement")))
        .with_artifact_writer()
        .cancel_on_run(stop3.clone());
    let coder_calls3 = coder3.call_count_handle();
    let registry3 = Arc::new(AgentRegistry::from_mocks(vec![architect_canary3, coder3]));
    let (ds3, tt3) = storage_backed_stores(&mem_db).await;
    let mut coordinator3 = coordinator_on_store(
        EventBus::new(256),
        registry3,
        vec![
            CoordinatorTurn::Calls(vec![call_specialist("coder_c", "implement")]),
            CoordinatorTurn::Text("continuing".into()),
        ],
        session_store.clone(),
    )
    .with_memory_writeback(Some(ds3), Some(tt3));
    let (resume_task3, resume_ctx3) =
        phase0_grounding(&mut coordinator3, session_id, dir.path(), Some(&record2.state_json))
            .await;
    assert_grounded_in_persisted_decision(&resume_ctx3.retrieved_chunks, "cycle 3");
    let third =
        coordinator3.run(resume_task3, resume_ctx3, stop3.clone(), Some(record2.state_json)).await;
    assert_honestly_interrupted(&third, "cycle 3 (settle race during resume)");
    assert!(
        coder_calls3.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "cycle 3 must dispatch coder_c from the resumed planner"
    );
    assert_eq!(
        architect_calls3.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the settled design step must never re-dispatch on the second resume"
    );
    // The run stays honestly interrupted with a resumable checkpoint; the
    // decision set never shrank and the architect decision is still singular.
    let final_record = session_store
        .load_orchestration_checkpoint(session_id)
        .await
        .expect("checkpoint store read")
        .expect("cycle 3 persisted a resumable checkpoint");
    assert!(!final_record.completed, "cycle 3 stays resumable, never Completed");
    assert_eq!(
        checkpoint_objective(&final_record.state_json),
        "build the thing",
        "the checkpoint objective survives all three cycles"
    );
    assert_eq!(
        checkpoint_objective_hash(&final_record.state_json),
        blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
        "the checkpoint objective hash survives all three cycles"
    );
    let decisions3 = wait_for_decisions(&mem_db, |rows| !rows.is_empty()).await;
    assert!(
        decisions3.len() >= persisted.len(),
        "the persisted decision set never shrinks across cycles: {decisions3:?}"
    );
    assert_eq!(
        decision_count_for(&decisions3, session_id, "architect completed subtask"),
        1,
        "no drift: the architect decision is still singular after three cycles: {decisions3:?}"
    );
}
