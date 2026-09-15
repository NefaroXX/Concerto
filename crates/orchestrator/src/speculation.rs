//! Speculative read-only investigation machinery (issue #62, parent #51).
//!
//! The `investigate_hypotheses` tool allows the Coordinator to run bounded
//! concurrent read-only investigations under distinct hypotheses, each using
//! the same [`consultation`] machinery as `consult_specialist`. The findings
//! are advisory: the coordinator model compares them, and a separate
//! explicit verification step is required before promoting any finding to
//! verified state.
//!
//! ## Concurrency
//!
//! N hypotheses are launched concurrently via [`futures::future::join_all`]
//! (mirroring the existing [`crate::coordinator::CoordinatorAgent::execute_graph`]
//! precedent). Each investigation uses its own [`crate::agent_runner::AgentRunner`]
//! with a read-only policy-gated executor, so concurrent investigations cannot
//! interfere with each other or the workspace.
//!
//! ## Non-goals
//!
//! - No auto-promotion: findings are advisory only; verification is a separate
//!   explicit step (issue #62 acceptance: "no auto-promotion to verified state").
//! - No write-gate integration: investigations are strictly read-only.
//! - No schema changes: `hypothesis_id` is an additive attribution field on the
//!   existing Decision and Finding payloads.

use std::sync::Arc;

use concerto_config::{AgentCapabilities, PromptSections};
use concerto_core::event::EventBus;
use concerto_core::traits::policy::PolicyEngine;
use concerto_core::types::{
    AgentContext, AgentId, AgentRunResult, AgentStage, AgentTask, OutputMode, SubTaskStatus,
    TaskId, ToolDefinition,
};
use concerto_core::CancellationToken;
use concerto_providers::model::ModelProfile;
use concerto_providers::retry::RetryPolicy;
use concerto_sessions::spend::SpendTracker;

use crate::agent_runner::AgentRunner;
use crate::agents::GenericSpecialistAgent;
use crate::consultation::{consult_read_only_executor, consult_task_description};
use crate::registry::AgentRegistry;

/// The tool name with which the Coordinator initiates a speculative
/// read-only investigation (issue #62).
pub(crate) const INVESTIGATE_HYPOTHESES_TOOL: &str = "investigate_hypotheses";

/// Minimum hypotheses in a batch. Below this, the caller should use
/// `consult_specialist` instead (a single hypothesis is not speculative).
pub const MIN_SPECULATIVE_HYPOTHESES: usize = 2;

/// Maximum hypotheses in a batch. Beyond this the decision loop's budget is
/// spread too thin and the signal-to-noise ratio of findings drops.
pub const MAX_SPECULATIVE_HYPOTHESES: usize = 4;

/// Hard ceiling on the total tool-call budget across ALL hypotheses in a
/// single batch. Per-hypothesis effort caps are individually bounded by
/// [`concerto_consultation::MAX_CONSULT_MAX_TOOL_CALLS`], but their sum must
/// not exceed this value (defense in depth; prevents runaway concurrent
/// read-only work).
pub const MAX_SPECULATION_TOTAL_TOOL_CALLS: u32 = 32;

/// Argument schema for the Coordinator's `investigate_hypotheses` tool.
pub(crate) fn investigate_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: INVESTIGATE_HYPOTHESES_TOOL.to_string(),
        description: "Investigate multiple competing hypotheses concurrently in \
                      read-only mode. Each hypothesis runs a bounded advisory \
                      consultation under a distinct question; findings are \
                      recorded as citable evidence for later verification. \
                      Use this to identify conflicts, one-sided failures, and \
                      budget exhaustion BEFORE verifying any hypothesis."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "hypotheses": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "hypothesis_id": {
                                "type": "string",
                                "description": "A short, stable identifier for this hypothesis (e.g. 'A', 'B')."
                            },
                            "agent_id": {
                                "type": "string",
                                "description": "The id of the registered specialist to consult, exactly as listed in the roster."
                            },
                            "question": {
                                "type": "string",
                                "description": "The complete, self-contained question for this hypothesis."
                            },
                            "notes": {
                                "type": "string",
                                "description": "Optional short context for the consultant (pointers, constraints)."
                            },
                            "supporting_evidence_ids": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Optional real whiteboard event ids that ground the question."
                            },
                            "max_tool_calls": {
                                "type": "integer",
                                "description": "Optional effort cap: the maximum read-only tool executions this hypothesis may perform (default small)."
                            }
                        },
                        "required": ["hypothesis_id", "agent_id", "question"]
                    },
                    "minItems": 2,
                    "description": "The competing hypotheses to investigate concurrently (minimum 2)."
                }
            },
            "required": ["hypotheses"]
        }),
    }
}

/// One hypothesis spec parsed from the `investigate_hypotheses` tool call.
pub(crate) struct HypothesisSpec {
    pub hypothesis_id: String,
    pub agent_id: String,
    pub question: String,
    pub notes: Option<String>,
    pub supporting_evidence_ids: Vec<String>,
    pub max_tool_calls: Option<u32>,
}

/// Parsed arguments for `investigate_hypotheses`.
pub(crate) struct InvestigateHypothesesArgs {
    pub hypotheses: Vec<HypothesisSpec>,
}

impl InvestigateHypothesesArgs {
    /// Parse tool arguments. Malformed input yields `None` (caller answers
    /// with a structured tool error, never a crash).
    pub(crate) fn parse(arguments: &serde_json::Value) -> Option<Self> {
        let hypotheses = arguments.get("hypotheses")?.as_array()?;
        let mut parsed = Vec::with_capacity(hypotheses.len());
        for item in hypotheses {
            let hypothesis_id =
                item.get("hypothesis_id").and_then(serde_json::Value::as_str)?.to_owned();
            let agent_id = item.get("agent_id").and_then(serde_json::Value::as_str)?.to_owned();
            let question = item.get("question").and_then(serde_json::Value::as_str)?.to_owned();
            let notes = item.get("notes").and_then(serde_json::Value::as_str).map(str::to_owned);
            let max_tool_calls = item
                .get("max_tool_calls")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            let supporting_evidence_ids =
                crate::coordinator::parse_string_array(item, "supporting_evidence_ids");
            parsed.push(HypothesisSpec {
                hypothesis_id,
                agent_id,
                question,
                notes,
                supporting_evidence_ids,
                max_tool_calls,
            });
        }
        Some(Self { hypotheses: parsed })
    }
}

// ── Comparator ─────────────────────────────────────────────────────────────

/// Terminal state of a hypothesis after settlement.
pub(crate) enum HypothesisTerminal {
    Completed,
    Failed,
}

/// One hypothesis's outcome, fed into the comparator.
pub(crate) struct HypothesisCompareInput {
    pub hypothesis_id: String,
    pub agent_id: AgentId,
    pub terminal: HypothesisTerminal,
    /// The whiteboard event id of the appended finding, if it landed.
    pub evidence_id: Option<String>,
    /// Bounded findings text, if the run succeeded.
    pub findings: Option<String>,
    pub tool_call_count: u32,
    pub cost_usd: f64,
    /// Structured reasons explaining the terminal state.
    pub reasons: Vec<String>,
}

/// Rank hypotheses and produce the comparator JSON for the tool result.
///
/// Ranking: `Completed-with-evidence` first (creation-order tiebreak),
/// then other terminal states in creation order. The recommended hypothesis
/// is the first `Completed-with-evidence` one (advisory; verification is a
/// separate step).
///
/// Deterministic: identical input always produces identical JSON (no
/// timestamp-dependent fields, stable sort, no non-deterministic set
/// iteration).
pub(crate) fn compare_hypotheses(hypotheses: &[HypothesisCompareInput]) -> serde_json::Value {
    let rank = |c: &HypothesisCompareInput| match &c.terminal {
        HypothesisTerminal::Completed if c.evidence_id.is_some() => 0,
        HypothesisTerminal::Completed => 1,
        HypothesisTerminal::Failed => 2,
    };
    let mut ranked: Vec<(usize, &HypothesisCompareInput)> = hypotheses.iter().enumerate().collect();
    ranked.sort_by_key(|(idx, c)| (rank(c), *idx));

    let blocks: Vec<serde_json::Value> = ranked
        .iter()
        .map(|(_, c)| {
            serde_json::json!({
                "hypothesis_id": c.hypothesis_id,
                "agent_id": c.agent_id.as_str(),
                "status": match c.terminal {
                    HypothesisTerminal::Completed => "completed",
                    HypothesisTerminal::Failed => "failed",
                },
                "evidence_id": c.evidence_id,
                "findings": c.findings,
                "tool_call_count": c.tool_call_count,
                "cost_usd": c.cost_usd,
                "reasons": c.reasons,
            })
        })
        .collect();

    let recommended = hypotheses
        .iter()
        .find(|c| matches!(c.terminal, HypothesisTerminal::Completed) && c.evidence_id.is_some())
        .map(|c| c.hypothesis_id.clone());

    let mut reasons: Vec<String> = Vec::new();
    for (original_idx, c) in hypotheses.iter().enumerate() {
        let status = match &c.terminal {
            HypothesisTerminal::Completed if c.evidence_id.is_some() => "completed-with-evidence",
            HypothesisTerminal::Completed => "completed",
            HypothesisTerminal::Failed => "failed",
        };
        reasons.push(format!(
            "[{}] hypothesis {} ranked #{} ({}, creation order {})",
            original_idx,
            c.hypothesis_id,
            ranked
                .iter()
                .position(|(_, r)| r.hypothesis_id == c.hypothesis_id)
                .unwrap_or(original_idx),
            status,
            original_idx,
        ));
    }

    serde_json::json!({
        "hypotheses": blocks,
        "recommended_hypothesis_id": recommended,
        "ranking": "completed-with-evidence first (creation-order tiebreak), then other terminal states",
        "reasons": reasons,
    })
}

// ── Consult execution helper ────────────────────────────────────────────────

/// Shared inputs for a single read-only consult execution (issue #62).
/// Extracted from `handle_consult_specialist` to allow reuse in both the
/// sequential consult path and the concurrent investigation path.
pub(crate) struct ConsultExecutionInputs {
    pub project_dir: std::path::PathBuf,
    pub agent_id: AgentId,
    pub name: String,
    pub stage: Option<AgentStage>,
    pub profile: ModelProfile,
    pub planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>,
    pub bus: EventBus,
    pub retry_policy: RetryPolicy,
    pub prompt_sections: PromptSections,
    pub skills_section: String,
    pub policy: Arc<dyn PolicyEngine>,
    pub spend_tracker: Arc<SpendTracker>,
    pub effort_cap: u32,
    pub question: String,
    pub notes: Option<String>,
    pub task: AgentTask,
    pub session: concerto_core::types::SessionContext,
    pub working_memory: concerto_core::memory::WorkingMemorySnapshot,
    pub retrieved_chunks: Vec<concerto_core::memory::MemoryChunk>,
    pub snapshot_digest: Option<String>,
    pub run_id: Option<String>,
    pub workspace_generation: Option<String>,
    pub cancel: CancellationToken,
}

/// Run a single read-only consult execution, reusing the same read-only
/// executor, agent construction, and runner path as `handle_consult_specialist`.
///
/// This is a pure function of its inputs — no `self` reference, no state
/// mutation. Extracted so the investigation handler can spawn N concurrent
/// consult executions without duplicating the construction logic.
pub(crate) async fn run_consult_execution(
    inputs: ConsultExecutionInputs,
) -> Result<AgentRunResult, OrchestratorError> {
    let ConsultExecutionInputs {
        project_dir,
        agent_id,
        name,
        stage,
        profile,
        planning_provider,
        bus,
        retry_policy,
        prompt_sections,
        skills_section,
        policy,
        spend_tracker,
        effort_cap,
        question,
        notes,
        task,
        session,
        working_memory,
        retrieved_chunks,
        snapshot_digest,
        run_id,
        workspace_generation,
        cancel,
    } = inputs;

    let consult_executor =
        Arc::new(consult_read_only_executor(project_dir.as_path(), policy, effort_cap));
    let consult_agent = Arc::new(
        GenericSpecialistAgent::new(
            agent_id.clone(),
            name,
            stage,
            planning_provider,
            Some(consult_executor),
            bus.clone(),
            retry_policy,
            prompt_sections,
            // Read-only capability shape: the consultant declares read
            // access only, matching the enforced boundary.
            AgentCapabilities {
                fs_read: Some(true),
                fs_write: Some(false),
                shell: Some(false),
                git: Some(false),
                lsp: Some(false),
                eval: Some(false),
            },
        )
        .with_output_mode(OutputMode::Freeform)
        .with_skills_section(&skills_section),
    );
    let mut consult_registry = AgentRegistry::new();
    consult_registry.register(consult_agent);
    let consult_runner = AgentRunner::new(Arc::new(consult_registry), bus, spend_tracker);

    // The transient SubTask is ONLY the runner's input record — it is
    // never added to the graph, the ledger, or any completion state.
    let consult_task_id = TaskId::new();
    let consult_subtask = concerto_core::types::SubTask {
        id: consult_task_id,
        parent_id: None,
        session_id: task.session_id,
        role: agent_id.clone(),
        description: consult_task_description(&question, notes.as_deref()),
        status: SubTaskStatus::Running,
        dependencies: Vec::new(),
        deliverable: None,
        created_at: time::OffsetDateTime::now_utc(),
        completed_at: None,
    };
    let consult_ctx = AgentContext {
        session,
        parent_task: Some(task),
        working_memory,
        retrieved_chunks,
        previous_results: Vec::new(),
        budget_remaining_usd: None,
        expected_artifacts: Vec::new(),
        workspace_capsule: None,
        workspace_snapshot_digest: snapshot_digest,
        run_id,
        workspace_generation,
    };
    consult_runner.run(agent_id, &consult_subtask, consult_ctx, &profile, cancel).await
}

// Re-export OrchestratorError for use within this module.
use concerto_core::OrchestratorError;

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn investigate_tool_def_names_the_operation() {
        let def = investigate_tool_definition();
        assert_eq!(def.name, INVESTIGATE_HYPOTHESES_TOOL);
        assert!(
            def.description.contains("read-only"),
            "the description must signal read-only contract"
        );
    }

    #[test]
    fn parse_rejects_missing_hypotheses_array() {
        assert!(
            InvestigateHypothesesArgs::parse(&json!({})).is_none(),
            "missing hypotheses key must yield None"
        );
        assert!(
            InvestigateHypothesesArgs::parse(&json!({ "hypotheses": "not-array" })).is_none(),
            "non-array hypotheses must yield None"
        );
    }

    #[test]
    fn parse_rejects_incomplete_hypothesis_entry() {
        assert!(
            InvestigateHypothesesArgs::parse(&json!({
                "hypotheses": [
                    { "hypothesis_id": "A", "agent_id": "researcher" }
                ]
            }))
            .is_none(),
            "missing question must yield None"
        );
        assert!(
            InvestigateHypothesesArgs::parse(&json!({
                "hypotheses": [
                    { "hypothesis_id": "A", "question": "why?" }
                ]
            }))
            .is_none(),
            "missing agent_id must yield None"
        );
    }

    #[test]
    fn parse_accepts_minimal_valid_entry() {
        let args = InvestigateHypothesesArgs::parse(&json!({
            "hypotheses": [
                { "hypothesis_id": "A", "agent_id": "r1", "question": "q1?" },
                { "hypothesis_id": "B", "agent_id": "r2", "question": "q2?" }
            ]
        }))
        .expect("parses");
        assert_eq!(args.hypotheses.len(), 2);
        assert_eq!(args.hypotheses[0].hypothesis_id, "A");
        assert_eq!(args.hypotheses[1].agent_id, "r2");
        assert_eq!(args.hypotheses[0].max_tool_calls, None);
    }

    #[test]
    fn parse_preserves_optional_fields() {
        let args = InvestigateHypothesesArgs::parse(&json!({
            "hypotheses": [{
                "hypothesis_id": "X",
                "agent_id": "researcher",
                "question": "why?",
                "notes": "check twice",
                "supporting_evidence_ids": ["ev-1", 3, "ev-2"],
                "max_tool_calls": 4
            }]
        }))
        .expect("parses");
        assert_eq!(args.hypotheses[0].notes.as_deref(), Some("check twice"));
        assert_eq!(
            args.hypotheses[0].supporting_evidence_ids,
            vec!["ev-1".to_owned(), "ev-2".to_owned()]
        );
        assert_eq!(args.hypotheses[0].max_tool_calls, Some(4));
    }

    #[test]
    fn comparator_empty_input_produces_empty_result() {
        let result = compare_hypotheses(&[]);
        assert_eq!(result["hypotheses"], json!([]));
        assert!(result["recommended_hypothesis_id"].is_null());
        assert_eq!(
            result["ranking"],
            "completed-with-evidence first (creation-order tiebreak), then other terminal states"
        );
    }

    #[test]
    fn comparator_ranking_and_recommended_deterministic() {
        let inputs = vec![
            HypothesisCompareInput {
                hypothesis_id: "fail-first".into(),
                agent_id: AgentId::new("coder"),
                terminal: HypothesisTerminal::Failed,
                evidence_id: None,
                findings: None,
                tool_call_count: 1,
                cost_usd: 0.0,
                reasons: vec!["run error".into()],
            },
            HypothesisCompareInput {
                hypothesis_id: "success-no-evidence".into(),
                agent_id: AgentId::new("researcher"),
                terminal: HypothesisTerminal::Completed,
                evidence_id: None, // evidence append failed
                findings: Some("looked everywhere".into()),
                tool_call_count: 3,
                cost_usd: 0.0,
                reasons: vec![],
            },
            HypothesisCompareInput {
                hypothesis_id: "success-with-evidence".into(),
                agent_id: AgentId::new("researcher"),
                terminal: HypothesisTerminal::Completed,
                evidence_id: Some("ev-real-id".into()),
                findings: Some("found the answer".into()),
                tool_call_count: 2,
                cost_usd: 0.0,
                reasons: vec![],
            },
        ];

        let result = compare_hypotheses(&inputs);
        // Recommended: the first completed-with-evidence hypothesis.
        assert_eq!(result["recommended_hypothesis_id"].as_str(), Some("success-with-evidence"));
        // Ranking order: success-with-evidence (#0), success-no-evidence (#1),
        // fail-first (#2).
        let ids: Vec<&str> = result["hypotheses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["hypothesis_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["success-with-evidence", "success-no-evidence", "fail-first"]);

        // Determinism: identical input → identical JSON.
        let result2 = compare_hypotheses(&inputs);
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            serde_json::to_string(&result2).unwrap()
        );
    }

    #[test]
    fn comparator_all_failed_no_recommendation() {
        let inputs = vec![
            HypothesisCompareInput {
                hypothesis_id: "A".into(),
                agent_id: AgentId::new("r1"),
                terminal: HypothesisTerminal::Failed,
                evidence_id: None,
                findings: None,
                tool_call_count: 0,
                cost_usd: 0.0,
                reasons: vec!["error".into()],
            },
            HypothesisCompareInput {
                hypothesis_id: "B".into(),
                agent_id: AgentId::new("r2"),
                terminal: HypothesisTerminal::Failed,
                evidence_id: None,
                findings: None,
                tool_call_count: 0,
                cost_usd: 0.0,
                reasons: vec!["timeout".into()],
            },
        ];
        let result = compare_hypotheses(&inputs);
        assert!(
            result["recommended_hypothesis_id"].is_null(),
            "no hypothesis qualifies as recommended"
        );
        // Both in input order.
        let ids: Vec<&str> = result["hypotheses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["hypothesis_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["A", "B"]);
    }

    #[test]
    fn comparator_completed_without_evidence_does_not_recommend() {
        let inputs = vec![HypothesisCompareInput {
            hypothesis_id: "X".into(),
            agent_id: AgentId::new("r"),
            terminal: HypothesisTerminal::Completed,
            evidence_id: None, // evidence append failed
            findings: Some("found answer but append failed".into()),
            tool_call_count: 2,
            cost_usd: 0.0,
            reasons: vec![],
        }];
        let result = compare_hypotheses(&inputs);
        assert!(
            result["recommended_hypothesis_id"].is_null(),
            "completed without evidence is not recommended"
        );
    }
}
