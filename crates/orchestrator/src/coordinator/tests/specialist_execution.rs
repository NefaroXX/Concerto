//! Settlement, checkpoint and same-task continuation regressions.

use super::*;
use crate::agents::execution_state::{continuation, ExecutionState};

#[derive(Default)]
struct ContinuingAgent {
    dispatches: std::sync::Mutex<Vec<(SubTask, AgentContext)>>,
}

#[async_trait::async_trait]
impl ExpertAgent for ContinuingAgent {
    fn id(&self) -> AgentId {
        AgentId::new("coder")
    }

    fn stage(&self) -> Option<AgentStage> {
        Some(AgentStage::new("implement"))
    }

    fn capabilities(&self) -> concerto_core::types::CapabilitySet {
        concerto_core::types::CapabilitySet::default()
    }

    async fn run(
        &self,
        task: &SubTask,
        context: AgentContext,
        _model: &str,
        _cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let mut dispatches = self.dispatches.lock().unwrap();
        dispatches.push((task.clone(), context));
        let mut progress = ExecutionState::default();
        progress.begin_turn();
        progress.succeeded("write-a", "write_file", &serde_json::json!({"path": "a.rs"}));
        if dispatches.len() > 1 {
            progress.final_answer();
        }
        Ok(AgentRunResult {
            task_id: task.id,
            role: task.role.clone(),
            outcome: progress.outcome(task, "retained a.rs"),
            summary: "retained a.rs".into(),
            files_modified: vec![camino::Utf8PathBuf::from("a.rs")],
            tool_call_count: 1,
            cost_usd: 0.01,
            latency_ms: 0,
            provider: "mock".into(),
            model: "mock".into(),
            tokens_in: 10,
            tokens_out: 10,
        })
    }
}

fn install_continuing_agent(coordinator: &mut CoordinatorAgent) -> Arc<ContinuingAgent> {
    let agent = Arc::new(ContinuingAgent::default());
    let rebuild = agent.clone();
    let mut registry = AgentRegistry::new();
    registry.register_with_factory(
        AgentId::new("coder"),
        agent.clone(),
        Arc::new(move |_provider| rebuild.clone()),
    );
    coordinator.registry = Arc::new(registry);
    // The runner shares the old registry unless rebuilt with the same roster.
    coordinator.runner = AgentRunner::new(
        coordinator.registry.clone(),
        coordinator.bus.clone(),
        coordinator.spend_tracker.clone(),
    );
    agent
}

#[tokio::test]
async fn unfinished_execution_continues_same_node_and_retains_dependency_position() {
    let (mut coordinator, task, context, mut graph, mut ledger, mut scope, _workspace) =
        obligation_harness(vec![MockExpertAgent::always_succeed(AgentId::new("coder"), "done")]);
    let agent = install_continuing_agent(&mut coordinator);
    let declared = declare_for_test(
        &mut coordinator,
        &task,
        &context,
        &mut graph,
        &mut ledger,
        &mut scope,
        &serde_json::json!({"obligations": [
            {"agent_id": "coder", "task": "repair"},
            {"agent_id": "coder", "task": "dependent", "after": [0]}
        ]}),
    )
    .await;
    let raw = declared["obligations"][0]["task_id"].as_str().unwrap();
    let id = Ulid::from_string(raw).map(TaskId).unwrap();
    let dependent_raw = declared["obligations"][1]["task_id"].as_str().unwrap();
    let dependent_id = Ulid::from_string(dependent_raw).map(TaskId).unwrap();
    let args = serde_json::json!({"agent_id": "coder", "task": "repair", "task_id": raw});
    let mut state = DispatchSessionState::default();
    let held = coordinator
        .handle_call_specialist(
            &mut graph,
            &task,
            &context,
            &CancellationToken::new(),
            &mut scope,
            &mut ledger,
            &mut state,
            None,
            &args,
        )
        .await;
    assert_eq!(held["outcome"], "needs_revision", "{held:?}");
    assert_eq!(held["continuation"]["task_id"], raw);
    assert_eq!(graph.get(&id).unwrap().status, SubTaskStatus::NeedsRevision);
    assert!(graph.get(&id).unwrap().completed_at.is_none());
    assert_eq!(graph.blocked_on(&dependent_id), vec![id]);
    assert!(test_ledger_for(&coordinator, &graph).has_open_implementation());
    let premature = coordinator.handle_call_specialist(
        &mut graph, &task, &context, &CancellationToken::new(), &mut scope,
        &mut ledger, &mut state, None,
        &serde_json::json!({"agent_id": "coder", "task": "dependent", "task_id": dependent_raw}),
    ).await;
    assert_eq!(premature["error"], "decision_not_ready", "{premature:?}");
    assert_eq!(agent.dispatches.lock().unwrap().len(), 1);

    // Existing checkpoint rows preserve the held status and structured result.
    let checkpoint = checkpoint::build_checkpoint(
        &scope,
        checkpoint::CheckpointStage::Executing,
        None,
        &context.working_memory,
        &graph,
        &ledger.completed_results,
        ledger.total_cost,
        ledger.total_tool_calls,
        &ledger.provider_metrics,
        &ledger.all_files,
        &HashMap::new(),
        &ledger.subtask_attempts,
        &HashMap::new(),
        &checkpoint::CheckpointContext::default(),
    );
    let json = serde_json::to_string(&checkpoint).unwrap();
    let loaded = checkpoint::GraphCheckpoint::from_json(&json).unwrap();
    graph = checkpoint::restore_graph(&loaded).unwrap();
    ledger.completed_results = loaded.completed_results;
    assert_eq!(graph.get(&id).unwrap().status, SubTaskStatus::NeedsRevision);
    let AgentOutcome::NeedsRevision { reason } = &ledger.completed_results[&id].outcome else {
        panic!("unfinished result survives checkpoint restoration");
    };
    assert_eq!(continuation(reason).unwrap().successful_calls[0].call_id, "write-a");

    let finished = coordinator
        .handle_call_specialist(
            &mut graph,
            &task,
            &context,
            &CancellationToken::new(),
            &mut scope,
            &mut ledger,
            &mut state,
            None,
            &args,
        )
        .await;
    assert_eq!(finished["outcome"], "success", "{finished:?}");
    assert_eq!(graph.len(), 2, "continuation creates no duplicate node");
    assert_eq!(graph.get(&id).unwrap().status, SubTaskStatus::Completed);
    assert!(graph.blocked_on(&dependent_id).is_empty());
    assert_eq!(ledger.total_tool_calls, 2);
    let dispatches = agent.dispatches.lock().unwrap();
    let (continued_task, continued_context) = &dispatches[1];
    assert_eq!(continued_task.id, id);
    assert!(continued_task.parent_id.is_none());
    assert!(continued_task.dependencies.is_empty(), "no synthetic self-dependency");
    assert_eq!(continued_context.previous_results.len(), 1);
    assert_eq!(continued_context.previous_results[0].task_id, id);
    assert!(matches!(
        continued_context.previous_results[0].outcome,
        AgentOutcome::NeedsRevision { .. }
    ));
}

#[tokio::test]
async fn unfinished_owner_can_be_retargeted_without_releasing_or_duplicating_work() {
    let (mut coordinator, task, context, mut graph, mut ledger, mut scope, _workspace) =
        obligation_harness(vec![
            MockExpertAgent::always_succeed(AgentId::new("coder"), "done"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "verified"),
        ]);
    let declared = declare_for_test(
        &mut coordinator,
        &task,
        &context,
        &mut graph,
        &mut ledger,
        &mut scope,
        &serde_json::json!({"obligations": [{"agent_id": "coder", "task": "repair"}]}),
    )
    .await;
    let raw = declared["obligations"][0]["task_id"].as_str().unwrap();
    let id = Ulid::from_string(raw).map(TaskId).unwrap();
    graph.get_mut(&id).unwrap().status = SubTaskStatus::NeedsRevision;
    let updated = coordinator.handle_update_obligations(
        &mut graph, &task, &context, &CancellationToken::new(), &mut scope, &mut ledger,
        &serde_json::json!({"task_id": raw, "agent_id": "validator", "task": "inspect retained work"}),
    ).await;
    assert_eq!(updated["status"], "updated", "{updated:?}");
    assert_eq!(graph.get(&id).unwrap().status, SubTaskStatus::NeedsRevision);
    assert_eq!(graph.get(&id).unwrap().role, AgentId::new("validator"));
    assert_eq!(graph.len(), 1);
}

#[tokio::test]
async fn completed_review_recommendation_still_settles_the_review_node() {
    let (mut coordinator, task, context, mut graph, mut ledger, mut scope, _workspace) =
        obligation_harness(vec![MockExpertAgent::always_revise(
            AgentId::new("reviewer"),
            "fix tests",
        )]);
    let result = coordinator
        .handle_call_specialist(
            &mut graph,
            &task,
            &context,
            &CancellationToken::new(),
            &mut scope,
            &mut ledger,
            &mut DispatchSessionState::default(),
            None,
            &serde_json::json!({"agent_id": "reviewer", "task": "Review the changes"}),
        )
        .await;
    assert_eq!(result["outcome"], "needs_revision", "{result:?}");
    assert!(result.get("continuation").is_none());
    assert!(graph.all_completed(), "a completed review recommendation is not unfinished execution");
}
