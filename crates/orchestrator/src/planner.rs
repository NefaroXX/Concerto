use std::sync::Arc;

use crate::prompts::complete_provider_request;
use concerto_config::blueprint::StageKind;
use concerto_config::{AgentCapabilities, BlueprintFacade};
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::AgentTask;
use concerto_core::types::{AgentId, AgentStage, DesignDoc, TaskId};
use concerto_core::types::{CompletionRequest, Message, Role};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_providers::retry::RetryPolicy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

const MAX_PLANNED_SUBTASKS: usize = 12;

/// Plan documents the planner itself instructs a Coder to write. These are
/// artifacts of the planning/implementation workflow, not code files that need
/// design approval, so a Coder may own them even when they are absent from the
/// DesignDoc's proposed_files. Ownership-overlap detection still applies to
/// them: two Coders must never both claim the same artifact.
const PLANNING_ARTIFACTS: &[&str] = &["PLAN.md"];

/// Represents a subtask planned by the LLM.
#[derive(Debug)]
pub struct PlannedSubTask {
    pub id: TaskId,
    pub role: AgentId,
    pub description: String,
    /// TaskIds of tasks this subtask depends on (filled after parsing).
    pub dependencies: Vec<TaskId>,
    /// Indices (0‑based) from the original JSON array that this subtask depends on.
    pub depends_on: Vec<usize>,
    /// Disjoint artifact ownership for Coder tasks.
    pub expected_artifacts: Vec<camino::Utf8PathBuf>,
}

/// One planned subtask rendered for the durable plan artifact.
///
/// Plain string fields keep the on-disk JSON stable and readable regardless
/// of how internal ID types evolve.
#[derive(Debug, Clone, Serialize)]
pub struct PlanArtifactTask {
    pub id: String,
    pub role: String,
    pub description: String,
    pub dependencies: Vec<String>,
    pub expected_artifacts: Vec<String>,
}

/// Durable, self-describing JSON snapshot of a task plan (ADR-52). The
/// orchestrator writes it to `<app_data_dir>/plans/plan-<plan_id>.json` so a
/// task's plan is reproducible and auditable independent of the session DB.
#[derive(Debug, Clone, Serialize)]
pub struct PlanArtifact {
    /// Run-scoped id that also names the file (`plan-<id>.json`).
    pub plan_id: String,
    /// The task text the plan was produced for (readable without the DB).
    pub task_description: String,
    /// Ordered subtasks with their resolved dependencies.
    pub tasks: Vec<PlanArtifactTask>,
}

impl PlanArtifact {
    /// Render a plan artifact from planned subtasks, generating a fresh
    /// run-scoped id. Fills expected artifacts per planned task.
    pub fn from_planned(task: &AgentTask, planned: &[PlannedSubTask]) -> Self {
        Self {
            plan_id: Ulid::new().to_string(),
            task_description: task.description.clone(),
            tasks: planned
                .iter()
                .map(|pst| PlanArtifactTask {
                    id: pst.id.to_string(),
                    role: pst.role.to_string(),
                    description: pst.description.clone(),
                    dependencies: pst.dependencies.iter().map(ToString::to_string).collect(),
                    expected_artifacts: pst
                        .expected_artifacts
                        .iter()
                        .map(|path| path.as_str().to_string())
                        .collect(),
                })
                .collect(),
        }
    }

    /// Render a plan artifact from a restored checkpoint graph (resume path):
    /// the graph's subtasks plus their expected-artifact expectations, using
    /// the checkpoint's run id as the plan id.
    pub fn from_graph(
        plan_id: String,
        task: &AgentTask,
        graph: &crate::graph::TaskGraph,
        expected_artifacts: &HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
    ) -> Self {
        Self {
            plan_id,
            task_description: task.description.clone(),
            tasks: graph
                .all_tasks()
                .iter()
                .map(|subtask| PlanArtifactTask {
                    id: subtask.id.to_string(),
                    role: subtask.role.to_string(),
                    description: subtask.description.clone(),
                    dependencies: subtask.dependencies.iter().map(ToString::to_string).collect(),
                    expected_artifacts: expected_artifacts
                        .get(&subtask.id)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|path| path.as_str().to_string())
                        .collect(),
                })
                .collect(),
        }
    }

    /// Pretty-printed JSON for the plan file (the durable artifact).
    pub fn pretty_json(&self) -> Result<String, OrchestratorError> {
        serde_json::to_string_pretty(self).map_err(|e| OrchestratorError::MultiAgentPlanFailed {
            reason: format!("failed to serialize plan artifact: {e}"),
        })
    }
}

/// Successful planning outcome: the planned subtasks handed to the caller
/// plus the durable artifact bundle the orchestrator may persist.
#[derive(Debug)]
pub struct PlanOutcome {
    pub tasks: Vec<PlannedSubTask>,
    pub artifact: PlanArtifact,
}

/// An agent the planner may assign work to, as seen from the registry.
///
/// ADR-35 §5: roles in the plan are matched against *registered* agents.
/// Stage tags decide lifecycle participation: research-stage agents are
/// "Researcher" tasks, implement-stage agents are "Coder" tasks (they may
/// own files and receive research dependencies), design/review/validate
/// stages are lifecycle-managed (rejected from plans), and stage-less or
/// unknown-stage agents are freeform custom roles.
///
/// ADR-35 phase 4 (roster enrichment): `capabilities` and `description` are
/// populated from the merged `CustomAgentConfig`s retained by the registry,
/// so the planner prompt describes what each role can actually do instead of
/// hardcoding assumptions about custom agents.
pub struct PlannerAgentInfo {
    pub id: AgentId,
    pub stage: Option<AgentStage>,
    /// Tool capability flags from the merged config (default when the agent
    /// was registered without a config, e.g. coordinator self-execution).
    pub capabilities: AgentCapabilities,
    /// Compact human-readable line (config name — role) for the prompt.
    pub description: String,
}

/// Compact per-role line for the planner system prompt: the role word, the
/// registered id, a capability summary when the role declares any tool
/// capability, and the human-readable description when present. Roles with no
/// notable capabilities render in the short historical form, so an
/// unconfigured (all-defaults) roster keeps a terse "Researcher (id: x)".
fn render_role(word: &str, agent: &PlannerAgentInfo) -> String {
    let mut line = format!("{word} (id: {}", agent.id);
    if let Some(summary) = capability_summary(&agent.capabilities) {
        line.push_str(&format!("; {summary}"));
    }
    line.push(')');
    if !agent.description.is_empty() {
        line.push_str(&format!(" — {}", agent.description));
    }
    line
}

/// Summarize a role's tool capabilities into a compact `; `-joined string
/// (e.g. `fs: read, write; shell; git`), or `None` when the role declares no
/// tool capability — those roles render without a summary.
fn capability_summary(caps: &AgentCapabilities) -> Option<String> {
    let caps = caps.effective();
    let mut parts: Vec<String> = Vec::new();
    if caps.fs_read || caps.fs_write {
        let mut modes: Vec<&str> = Vec::new();
        if caps.fs_read {
            modes.push("read");
        }
        if caps.fs_write {
            modes.push("write");
        }
        parts.push(format!("fs: {}", modes.join(", ")));
    }
    if caps.shell {
        parts.push("shell".into());
    }
    if caps.git {
        parts.push("git".into());
    }
    if caps.lsp {
        parts.push("lsp".into());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// Planner partition for a registered agent's stage (ADR-58 P2+P3, R7): the
/// closed `StageKind` classification when a resolved-blueprint facade is
/// attached, the tag-based `AgentStage` classification otherwise — identical
/// on the default `standard` blueprint, whose tags map 1:1 to their kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannerPartition {
    Research,
    Implement,
    Lifecycle,
    Custom,
}

fn planner_partition(
    stage: Option<&AgentStage>,
    facade: Option<&BlueprintFacade>,
) -> PlannerPartition {
    let Some(stage) = stage else {
        return PlannerPartition::Custom;
    };
    if let Some(facade) = facade {
        match facade.stage_kind(stage.as_str()) {
            Some(StageKind::Research) => PlannerPartition::Research,
            Some(StageKind::Execution) => PlannerPartition::Implement,
            Some(StageKind::Planning | StageKind::Review | StageKind::Acceptance) => {
                PlannerPartition::Lifecycle
            }
            // RunOnce and tags with no matching blueprint stage keep the
            // Freeform/custom semantics (ADR-58 D2).
            Some(StageKind::RunOnce) | None => PlannerPartition::Custom,
        }
    } else {
        // No facade attached: the tag-based `AgentStage` classification,
        // identical on the default `standard` blueprint, whose tags map 1:1
        // to their kinds.
        if stage.is_research() {
            PlannerPartition::Research
        } else if stage.is_implement() {
            PlannerPartition::Implement
        } else if stage.is_design() || stage.is_review() || stage.is_validate() {
            PlannerPartition::Lifecycle
        } else {
            PlannerPartition::Custom
        }
    }
}

/// ADR-58 P2+P3 (R8): the plan-artifact contract for an `Execution` task is
/// the primary `Execution` stage's `files.expected_artifacts` (blueprint
/// §5.7) when the blueprint defines a non-empty list, else the LLM's proposed
/// `raw.files` — byte-identical to legacy on the default `standard`
/// blueprint, which carries no `files` block.
fn implementation_artifact_contract(
    facade: Option<&BlueprintFacade>,
    fallback: &[camino::Utf8PathBuf],
) -> Vec<camino::Utf8PathBuf> {
    match facade.and_then(BlueprintFacade::primary_execution_stage) {
        Some(stage) => match stage.def.files.as_ref() {
            Some(def) if !def.expected_artifacts.is_empty() => {
                def.expected_artifacts.iter().map(camino::Utf8PathBuf::from).collect()
            }
            _ => fallback.to_vec(),
        },
        None => fallback.to_vec(),
    }
}

/// LLM-driven planner for the research and implementation portion of a task.
pub struct TaskPlanner;

impl TaskPlanner {
    /// System prompt describing the registered roles and the required JSON
    /// output. Built per-plan because which agents participate in which
    /// lifecycle stage is configuration-driven (ADR-35 §5).
    ///
    /// `facade` is the resolved-blueprint lookup surface (ADR-58 P2+P3, F3):
    /// the role lists follow the same `StageKind`/Execution-staffing partition
    /// the rest of the planner uses, so a renamed-tag blueprint classifies
    /// identically everywhere. On the default `standard` blueprint the rendered
    /// prompt is byte-identical to the tag-based match.
    fn system_prompt(agents: &[PlannerAgentInfo], facade: Option<&BlueprintFacade>) -> String {
        let mut research_roles: Vec<&PlannerAgentInfo> = Vec::new();
        let mut implement_roles: Vec<&PlannerAgentInfo> = Vec::new();
        let mut lifecycle_roles: Vec<&PlannerAgentInfo> = Vec::new();
        let mut custom_roles: Vec<&PlannerAgentInfo> = Vec::new();
        for agent in agents {
            match planner_partition(agent.stage.as_ref(), facade) {
                PlannerPartition::Research => research_roles.push(agent),
                PlannerPartition::Implement => implement_roles.push(agent),
                PlannerPartition::Lifecycle => lifecycle_roles.push(agent),
                PlannerPartition::Custom => custom_roles.push(agent),
            }
        }

        let design_desc = if research_roles.iter().chain(implement_roles.iter()).next().is_some()
            && !lifecycle_roles.is_empty()
        {
            let lifecycle_ids =
                lifecycle_roles.iter().map(|agent| agent.id.as_str()).collect::<Vec<_>>();
            format!(
                "Design is handled automatically before planning (by {}).",
                lifecycle_ids.join(", ")
            )
        } else {
            "No design agent is registered; planning starts directly from the task.".to_string()
        };
        let available = {
            let mut parts = Vec::new();
            for agent in &research_roles {
                parts.push(render_role("Researcher", agent));
            }
            for agent in &implement_roles {
                parts.push(render_role("Coder", agent));
            }
            for agent in &custom_roles {
                parts.push(render_role("custom agent", agent));
            }
            if parts.is_empty() {
                "none".to_string()
            } else {
                parts.join("; ")
            }
        };

        format!(
            "{design_desc}\n\
             Review and validation are managed automatically after implementation.\n\n\
             Plan research, implementation, and any custom-agent work as a JSON array of objects with fields:\n\
             - \"role\": the lowercase id of one of the available roles (e.g. \"Researcher\" -> \"researcher\"); do not invent ids\n\
             - \"description\": what this subtask should do\n\
             - \"depends_on\": array of 0-based indices of earlier subtasks that must complete first\n\
             - \"files\": for Coder tasks, the disjoint subset of DesignDoc proposed_files owned by this task\n\n\
             Available roles: {available}\n\n\
             List all Researcher tasks before Coder tasks. Every Coder task must depend on all research it consumes. \
             Include at least one Coder task. Independent Coder tasks may run in parallel only when their files sets are disjoint. \
             Custom agents cannot write files; reserve file writes for Coder tasks. Return at most 12 items and only the JSON array.\n"
        )
    }

    /// Build the full prompt for the planner LLM.
    ///
    /// `facade` is forwarded to the system prompt so the role lists partition
    /// by the same resolved `StageKind` classification as the rest of the
    /// planner (ADR-58 P2+P3, F3); `None` keeps the tag-based match.
    fn build_prompt(
        &self,
        task: &AgentTask,
        design_doc: Option<&DesignDoc>,
        agents: &[PlannerAgentInfo],
        skills_section: &str,
        facade: Option<&BlueprintFacade>,
    ) -> String {
        let mut prompt = String::new();
        prompt.push_str(&Self::system_prompt(agents, facade));
        // ADR-43 Task 4: session skills apply to the planner too. The section
        // is pre-budgeted by `SkillsContext`; it is injected as-is.
        if !skills_section.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(skills_section);
        }
        prompt.push_str(&format!("\n\nTask: {}\n", task.description));
        if let Some(doc) = design_doc {
            if let Ok(doc_json) = serde_json::to_string(doc) {
                prompt.push_str(&format!("DesignDoc: {}\n", doc_json));
            }
        }
        prompt
    }

    /// Call the LLM provider and parse the JSON response into a vector of
    /// `PlannedSubTask`. Returns an `OrchestratorError::MultiAgentPlanFailed`
    /// on any parsing or validation problem.
    ///
    /// `agents` is the set of registered agents (id + stage tag) that the
    /// plan may reference. A pipeline with no implement-stage agent is
    /// rejected before any LLM call.
    ///
    /// The full context (task, design doc, agent roster, provider policy,
    /// event bus, cancellation) is passed directly to keep this a thin
    /// stateless planner; grouping would hide intent.
    ///
    /// `skills_section` carries the session's budgeted skills instructions
    /// (ADR-43, Task 4); pass an empty string to omit them.
    ///
    /// `facade` is the resolved-blueprint lookup surface (ADR-58 P2+P3):
    /// partitions and artifact ownership key off `StageKind` classifications
    /// and the primary `Execution` stage's plan-artifact contract instead of
    /// literal stage tags. `None` (unit tests, coordinators built without a
    /// facade) falls back to the tag-based `AgentStage` classification —
    /// byte-identical on the default `standard` blueprint, whose tags map 1:1
    /// to their kinds.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan(
        &self,
        task: &AgentTask,
        design_doc: Option<&DesignDoc>,
        agents: &[PlannerAgentInfo],
        provider: Arc<dyn LlmProvider>,
        retry_policy: &RetryPolicy,
        bus: &EventBus,
        cancel: CancellationToken,
        skills_section: &str,
        facade: Option<&BlueprintFacade>,
    ) -> Result<PlanOutcome, OrchestratorError> {
        // Partition the registered agents by stage (ADR-35 §5, R7): the
        // blueprint's closed `StageKind` classification when a facade is
        // attached, the tag-based classification otherwise.
        let research_agent_ids: Vec<AgentId> = agents
            .iter()
            .filter(|agent| {
                planner_partition(agent.stage.as_ref(), facade) == PlannerPartition::Research
            })
            .map(|agent| agent.id.clone())
            .collect();
        let implement_agent_ids: Vec<AgentId> = agents
            .iter()
            .filter(|agent| {
                planner_partition(agent.stage.as_ref(), facade) == PlannerPartition::Implement
            })
            .map(|agent| agent.id.clone())
            .collect();
        let lifecycle_agent_ids: Vec<AgentId> = agents
            .iter()
            .filter(|agent| {
                planner_partition(agent.stage.as_ref(), facade) == PlannerPartition::Lifecycle
            })
            .map(|agent| agent.id.clone())
            .collect();
        // Everything else is a freeform custom role: stage-less agents and
        // agents carrying an unknown stage tag (e.g. "documentation") can be
        // planned directly like the generic specialists they are.
        let custom_agent_ids: Vec<AgentId> = agents
            .iter()
            .filter(|agent| {
                planner_partition(agent.stage.as_ref(), facade) == PlannerPartition::Custom
            })
            .map(|agent| agent.id.clone())
            .collect();

        // ADR-58 P2+P3 (R10): the fails-fast keys off the primary
        // `Execution`-kind stage staffing instead of the `implement` tag.
        // On the default blueprint (primary Execution stage tagged
        // `implement`) the message is byte-identical to the legacy one; a
        // differently-tagged primary Execution stage enriches it with the
        // stage's human-readable label.
        if implement_agent_ids.is_empty() {
            let reason = match facade.and_then(BlueprintFacade::primary_execution_stage) {
                Some(stage) if stage.def.tag != "implement" => format!(
                    "no '{label}' Execution-stage agent is registered; cannot plan \
                     implementation work",
                    label = stage.def.label
                ),
                _ => "no implementation-stage agent is registered; cannot plan implementation work"
                    .to_string(),
            };
            return Err(OrchestratorError::MultiAgentPlanFailed { reason });
        }

        // Build request
        let prompt = self.build_prompt(task, design_doc, agents, skills_section, facade);
        let request = CompletionRequest {
            // Empty means "use the configured model" across all provider
            // implementations. A literal "planner" was sent to provider APIs
            // as a model ID and made real multi-agent planning fail immediately.
            model: String::new(),
            messages: vec![Message {
                role: Role::User,
                content: prompt,
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            tools: None,
            tool_choice: None,
            temperature: Some(0.7),
            max_tokens: Some(2048),
            stream: false,
        };
        // Perform the LLM call(s). A provider may return EMPTY content on an
        // otherwise successful call; without handling, the empty text fell
        // through to the JSON parser and failed with a positional "got: " error
        // that drove the coordinator's heuristic fallback with garbage
        // subtasks built from the degenerate task text. Treat empty output as
        // a retriable failure: retry once (2 attempts total, same prompt), then
        // return a clear `MultiAgentPlanFailed` so the coordinator's heuristic
        // fallback still engages. `complete_provider_request` already applies
        // `retry_policy` for transient transport/stream errors; this loop is
        // separate and manual.
        //
        // Reasoning-capable models stream long `reasoning_content` deltas that
        // consume the output token budget BEFORE any `content` delta arrives.
        // With a tight `max_tokens` cap the visible content can therefore come
        // back empty on an otherwise successful call. The first attempt uses
        // the base budget (2048); the single retry raises it to 16384 so content
        // can still arrive after the reasoning deltas even on complex tasks.
        // The prompt is identical on both attempts — the request differs only
        // in the output budget.
        let mut attempt = 0;
        let text = loop {
            attempt += 1;
            let attempt_request = CompletionRequest {
                max_tokens: Some(if attempt == 1 { 2048 } else { 16384 }),
                ..request.clone()
            };
            let (response, reasoning, _tool_calls, usage) = complete_provider_request(
                &provider,
                &attempt_request,
                retry_policy,
                bus,
                task.session_id,
                task.id,
                &cancel,
            )
            .await?;
            if !response.trim().is_empty() {
                break response;
            }
            if attempt >= 2 {
                return Err(OrchestratorError::MultiAgentPlanFailed {
                    reason: "planner returned an empty response (no content) after 2 attempts"
                        .into(),
                });
            }
            tracing::warn!(
                provider = provider.provider_name(),
                attempt,
                reasoning_len = reasoning.as_ref().map_or(0, String::len),
                completion_tokens = usage.and_then(|usage| usage.completion_tokens),
                "planner returned an empty response; retrying once with a raised output budget"
            );
        };

        // Parse JSON array
        #[derive(Deserialize)]
        struct RawItem {
            role: String,
            description: String,
            #[serde(default)]
            depends_on: Vec<usize>,
            #[serde(default)]
            files: Vec<camino::Utf8PathBuf>,
        }
        let raw_items: Vec<RawItem> =
            crate::prompts::parse_json_substring(&text).ok_or_else(|| {
                OrchestratorError::MultiAgentPlanFailed {
                    reason: format!(
                        "JSON parse error: expected a JSON array of plan items, got: {}",
                        text.chars().take(200).collect::<String>()
                    ),
                }
            })?;
        if raw_items.len() > MAX_PLANNED_SUBTASKS {
            return Err(OrchestratorError::MultiAgentPlanFailed {
                reason: format!(
                    "plan contains {} tasks; maximum is {MAX_PLANNED_SUBTASKS}",
                    raw_items.len()
                ),
            });
        }

        // Convert to PlannedSubTask with concrete TaskIds. Roles in the plan
        // are resolved against the registered agents; only research-stage
        // and implement-stage agents may be planned directly.
        let mut planned: Vec<PlannedSubTask> = Vec::new();
        let mut implement_seen = false;
        for raw in &raw_items {
            let lower = raw.role.to_ascii_lowercase();
            let role = if let Some(id) = research_agent_ids.iter().find(|id| id.as_str() == lower) {
                if implement_seen {
                    return Err(OrchestratorError::MultiAgentPlanFailed {
                        reason: "Researcher tasks must be planned before Coder tasks".into(),
                    });
                }
                id.clone()
            } else if let Some(id) = implement_agent_ids.iter().find(|id| id.as_str() == lower) {
                implement_seen = true;
                id.clone()
            } else if let Some(id) = custom_agent_ids.iter().find(|id| id.as_str() == lower) {
                id.clone()
            } else if lifecycle_agent_ids.iter().any(|id| id.as_str() == lower) {
                // Design/review/validate stages are lifecycle-managed and
                // are run by the coordinator, not planned directly.
                return Err(OrchestratorError::MultiAgentPlanFailed {
                    reason: format!(
                        "Role '{}' is lifecycle-managed and cannot be planned directly",
                        raw.role
                    ),
                });
            } else {
                return Err(OrchestratorError::MultiAgentPlanFailed {
                    reason: format!("Role '{}' does not match any registered agent", raw.role),
                });
            };
            let is_implement = implement_agent_ids.contains(&role);
            planned.push(PlannedSubTask {
                id: TaskId::new(),
                role: role.clone(),
                description: raw.description.clone(),
                dependencies: Vec::new(),
                depends_on: raw.depends_on.clone(),
                expected_artifacts: if is_implement {
                    implementation_artifact_contract(facade, &raw.files)
                } else {
                    Vec::new()
                },
            });
        }

        if !implement_seen {
            return Err(OrchestratorError::MultiAgentPlanFailed {
                reason: "plan must contain at least one implementation task".into(),
            });
        }

        let implement_indices = planned
            .iter()
            .enumerate()
            .filter_map(|(index, task)| implement_agent_ids.contains(&task.role).then_some(index))
            .collect::<Vec<_>>();
        let mut owned = std::collections::HashSet::new();
        for index in &implement_indices {
            for path in &planned[*index].expected_artifacts {
                // Planning artifacts (see `PLANNING_ARTIFACTS`) are plan
                // documents the planner itself tells a Coder to write; they
                // are exempt from the DesignDoc proposed_files membership
                // check because they are not code files requiring design
                // approval. They still take part in the ownership-overlap
                // check below, so two Coders cannot both claim PLAN.md.
                let is_planning_artifact = PLANNING_ARTIFACTS.contains(&path.as_str());
                if let Some(doc) = design_doc {
                    if !is_planning_artifact && !doc.proposed_files.contains(path) {
                        return Err(OrchestratorError::MultiAgentPlanFailed {
                            reason: format!(
                                "planned {} claims {path}, which is not in DesignDoc proposed_files",
                                planned[*index].role
                            ),
                        });
                    }
                }
                if !owned.insert(path.clone()) {
                    return Err(OrchestratorError::MultiAgentPlanFailed {
                        reason: format!(
                            "planned {} artifact ownership overlaps at {path}",
                            planned[*index].role
                        ),
                    });
                }
            }
        }
        if let Some(doc) = design_doc {
            let mut next_owner = 0usize;
            for path in &doc.proposed_files {
                if owned.insert(path.clone()) {
                    let owner = implement_indices[next_owner % implement_indices.len()];
                    planned[owner].expected_artifacts.push(path.clone());
                    next_owner = next_owner.saturating_add(1);
                }
            }
        }

        // Resolve index‑based dependencies into TaskIds and validate indices
        let len = planned.len();
        for i in 0..len {
            let deps_idx = planned[i].depends_on.clone();
            for idx in deps_idx {
                if idx >= i {
                    return Err(OrchestratorError::MultiAgentPlanFailed {
                        reason: format!(
                            "depends_on index {idx} for task {i} must reference an earlier task"
                        ),
                    });
                }
                let dep_id = planned[idx].id;
                if !planned[i].dependencies.contains(&dep_id) {
                    planned[i].dependencies.push(dep_id);
                }
            }
        }

        // Research is a prerequisite for implementation even when a model
        // omits depends_on. This preserves parallelism within each stage while
        // preventing Coder from racing the context it needs.
        let research_task_ids: Vec<TaskId> = planned
            .iter()
            .filter(|task| research_agent_ids.contains(&task.role))
            .map(|task| task.id)
            .collect();
        for task in &mut planned {
            if implement_agent_ids.contains(&task.role) {
                for research_id in &research_task_ids {
                    if !task.dependencies.contains(research_id) {
                        task.dependencies.push(*research_id);
                    }
                }
            }
        }

        // A Coder without declared file ownership cannot be proven
        // conflict-free. Serialize all Coders in plan order in that case.
        if implement_indices.len() > 1
            && implement_indices.iter().any(|index| planned[*index].expected_artifacts.is_empty())
        {
            for pair in implement_indices.windows(2) {
                let previous = planned[pair[0]].id;
                if !planned[pair[1]].dependencies.contains(&previous) {
                    planned[pair[1]].dependencies.push(previous);
                }
            }
        }

        // ADR-52: wrap the plan in a durable artifact bundle. The pretty JSON
        // is logged here (and persisted by the orchestrator) so the plan is
        // reproducible from traces and on disk.
        let artifact = PlanArtifact::from_planned(task, &planned);
        match artifact.pretty_json() {
            Ok(json) => {
                debug!(target: "orchestrator::planner", plan = %json, "planner produced plan")
            }
            Err(e) => {
                debug!(target: "orchestrator::planner", error = %e, "planner artifact serialization failed");
            }
        }
        Ok(PlanOutcome { tasks: planned, artifact })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use concerto_core::error::ProviderError;
    use concerto_core::traits::provider::CompletionStream;
    use concerto_core::types::{CompletionChunk, CompletionRequest, TokenBudget};
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct StaticProvider {
        response: String,
    }

    #[async_trait]
    impl LlmProvider for StaticProvider {
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                reasoning: None,
                delta: self.response.clone(),
                tool_call: None,
                is_final: true,
                usage: None,
            })])))
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        fn provider_name(&self) -> &'static str {
            "static"
        }
    }

    /// A provider that serves a predetermined sequence of responses, one per
    /// `stream_completion` call, and counts the calls made. The LAST response
    /// repeats for any further calls, so "empty forever" needs only one `""`
    /// entry. Used to exercise the planner's manual empty-content retry.
    /// `seen_max_tokens` records the `max_tokens` budget of every request so
    /// tests can assert the retry raises the output budget.
    struct SequenceProvider {
        responses: Vec<String>,
        calls: AtomicUsize,
        seen_max_tokens: Mutex<Vec<u64>>,
    }

    impl SequenceProvider {
        /// The next queued response, advancing the call counter. Indexing
        /// saturates at the final entry so a short queue stays deterministic.
        fn next_delta(&self) -> String {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let index = call.min(self.responses.len().saturating_sub(1));
            self.responses[index].clone()
        }
    }

    #[async_trait]
    impl LlmProvider for SequenceProvider {
        async fn stream_completion(
            &self,
            request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            self.seen_max_tokens
                .lock()
                .expect("max-token recording lock is not poisoned")
                .push(request.max_tokens.unwrap_or_default());
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                reasoning: None,
                delta: self.next_delta(),
                tool_call: None,
                is_final: true,
                usage: None,
            })])))
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        fn provider_name(&self) -> &'static str {
            "sequence"
        }
    }

    /// Standard agent roster for planner tests: the five built-in lifecycle
    /// roles plus a freeform custom agent (ADR-35 phase 2). Entries carry
    /// default (no tool) capabilities and no description — roster enrichment
    /// is exercised by dedicated tests.
    fn builtin_agents() -> Vec<PlannerAgentInfo> {
        vec![
            PlannerAgentInfo {
                id: AgentId::new("architect"),
                stage: Some(AgentStage::new("design")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("coder"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("reviewer"),
                stage: Some(AgentStage::new("review")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("validator"),
                stage: Some(AgentStage::new("validate")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("docs-writer"),
                stage: None,
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ]
    }

    async fn plan_with_design_doc(
        response: &str,
        design_doc: Option<DesignDoc>,
        agents: Vec<PlannerAgentInfo>,
    ) -> Result<Vec<PlannedSubTask>, OrchestratorError> {
        TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "test"),
                design_doc.as_ref(),
                &agents,
                Arc::new(StaticProvider { response: response.to_string() }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                None,
            )
            .await
            .map(|outcome| outcome.tasks)
    }

    async fn plan_with_agents(
        response: &str,
        agents: Vec<PlannerAgentInfo>,
    ) -> Result<Vec<PlannedSubTask>, OrchestratorError> {
        plan_with_design_doc(response, None, agents).await
    }

    async fn plan(response: &str) -> Result<Vec<PlannedSubTask>, OrchestratorError> {
        plan_with_agents(response, builtin_agents()).await
    }

    #[tokio::test]
    async fn coder_waits_for_research_when_model_omits_dependency() {
        let tasks = plan(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"Coder","description":"implement","depends_on":[]}
            ]"#,
        )
        .await
        .unwrap();

        assert_eq!(tasks[0].role, AgentId::new("researcher"));
        assert_eq!(tasks[1].role, AgentId::new("coder"));
        assert_eq!(tasks[1].dependencies, vec![tasks[0].id]);
    }

    #[tokio::test]
    async fn custom_role_is_accepted_as_freeform_agent_id() {
        let tasks = plan(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"docs-writer","description":"write docs","depends_on":[0]},
                {"role":"Coder","description":"implement","depends_on":[0]}
            ]"#,
        )
        .await
        .unwrap();

        assert_eq!(tasks[0].role, AgentId::new("researcher"));
        assert_eq!(tasks[1].role, AgentId::new("docs-writer"));
        assert_eq!(tasks[2].role, AgentId::new("coder"));
        // Custom agents do not carry artifact ownership.
        assert!(tasks[1].expected_artifacts.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_managed_roles_are_rejected_from_plan() {
        let result = plan(
            r#"[
                {"role":"Coder","description":"implement","depends_on":[]},
                {"role":"Reviewer","description":"review","depends_on":[0]}
            ]"#,
        )
        .await;

        assert!(matches!(result, Err(OrchestratorError::MultiAgentPlanFailed { .. })));
    }

    #[tokio::test]
    async fn custom_implement_stage_agent_is_treated_as_coder() {
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("copilot"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("docs-writer"),
                stage: None,
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let tasks = plan_with_agents(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"docs-writer","description":"write docs","depends_on":[0]},
                {"role":"Copilot","description":"implement","depends_on":[0],"files":["src/lib.rs"]}
            ]"#,
            agents,
        )
        .await
        .unwrap();

        assert_eq!(tasks[1].role, AgentId::new("docs-writer"));
        assert_eq!(tasks[2].role, AgentId::new("copilot"));
        // Implement-stage roles receive file ownership and research deps.
        assert_eq!(tasks[2].expected_artifacts, vec![camino::Utf8PathBuf::from("src/lib.rs")]);
        assert_eq!(tasks[2].dependencies, vec![tasks[0].id]);
        // Freeform custom agents still do not carry artifacts.
        assert!(tasks[1].expected_artifacts.is_empty());
    }

    #[tokio::test]
    async fn coder_may_own_planning_artifact_absent_from_proposed_files() {
        // The planner legitimately assigns writing the plan artifact PLAN.md
        // to a Coder, but the design doc's proposed_files only lists code
        // files. Validation must not reject that ownership: planning
        // artifacts are exempt from the proposed_files membership check.
        let doc = DesignDoc {
            goals: Vec::new(),
            constraints: Vec::new(),
            proposed_files: vec![camino::Utf8PathBuf::from("src/lib.rs")],
            interface_sketch: String::new(),
            risks: Vec::new(),
        };
        let tasks = plan_with_design_doc(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"Coder","description":"implement","depends_on":[0],"files":["src/lib.rs","PLAN.md"]}
            ]"#,
            Some(doc),
            builtin_agents(),
        )
        .await
        .expect("a Coder may own PLAN.md even when it is absent from proposed_files");

        let coder = &tasks[1];
        assert_eq!(coder.role, AgentId::new("coder"));
        assert!(
            coder.expected_artifacts.iter().any(|path| path.as_str() == "PLAN.md"),
            "the Coder must retain ownership of the planning artifact, got: {:?}",
            coder.expected_artifacts,
        );
    }

    #[tokio::test]
    async fn design_doc_contract_enforces_proposed_files_membership() {
        // The coordinator passes the DesignDoc to the planner ONLY as a
        // `binding_doc` — `Some` for a Verified (active) claim, `None` for a
        // Quarantined/Skipped one (ADR-65 §5). This test proves the ON/OFF
        // semantics of that binding: with the doc attached, a Coder claiming a
        // path the design never proposed is REJECTED; without it (the passive
        // Quarantined/Skipped route) the same claim is accepted untouched.
        let doc = DesignDoc {
            goals: Vec::new(),
            constraints: Vec::new(),
            proposed_files: vec![camino::Utf8PathBuf::from("src/lib.rs")],
            interface_sketch: String::new(),
            risks: Vec::new(),
        };
        let response = r#"[
            {"role":"Researcher","description":"inspect","depends_on":[]},
            {"role":"Coder","description":"implement","depends_on":[0],"files":["src/lib.rs","src/hallucinated.rs"]}
        ]"#;

        // Bound (Verified binding_doc = Some): the unproposed claim is a plan
        // failure — the contract is enforced.
        let bound = plan_with_design_doc(response, Some(doc.clone()), builtin_agents()).await;
        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = bound else {
            panic!("a bound doc must reject an unproposed file claim, got: {bound:?}");
        };
        assert!(
            reason.contains("src/hallucinated.rs"),
            "the rejection must name the offending file: {reason}"
        );
        assert!(
            reason.contains("not in DesignDoc proposed_files"),
            "the rejection must cite the contract: {reason}"
        );

        // Unbound (Quarantined/Skipped binding_doc = None): no membership
        // gate — the pipeline degrades to the pre-verification path without
        // blocking planning.
        let unbound = plan_with_design_doc(response, None, builtin_agents()).await;
        let tasks = unbound.unwrap_or_else(|err| {
            panic!("an unbound doc must not enforce proposed_files membership: {err}")
        });
        let coder = tasks
            .iter()
            .find(|task| task.role == AgentId::new("coder"))
            .expect("the plan has a Coder");
        assert!(
            coder.expected_artifacts.iter().any(|path| path.as_str() == "src/hallucinated.rs"),
            "the unbound Coder keeps its claimed artifact: {:?}",
            coder.expected_artifacts
        );
    }

    #[tokio::test]
    async fn coder_artifact_ownership_overlap_is_rejected() {
        // Two Coders must not both claim the same file — including a planning
        // artifact like PLAN.md, which is exempt from the proposed_files check
        // but still subject to ownership-overlap detection.
        let result = plan(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"Coder","description":"implement A","depends_on":[0],"files":["src/lib.rs","PLAN.md"]},
                {"role":"Coder","description":"implement B","depends_on":[0],"files":["PLAN.md"]}
            ]"#,
        )
        .await;

        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = result else {
            panic!("expected MultiAgentPlanFailed, got: {result:?}");
        };
        assert!(reason.contains("overlaps at PLAN.md"), "unexpected reason: {reason}");
    }

    #[tokio::test]
    async fn unknown_role_is_rejected() {
        let result = plan(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"ghost","description":"not registered","depends_on":[0]},
                {"role":"Coder","description":"implement","depends_on":[0]}
            ]"#,
        )
        .await;

        assert!(matches!(result, Err(OrchestratorError::MultiAgentPlanFailed { .. })));
    }

    #[tokio::test]
    async fn custom_lifecycle_stage_role_is_rejected() {
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("coder"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("quality-gate"),
                stage: Some(AgentStage::new("validate")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let result = plan_with_agents(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"quality-gate","description":"validate","depends_on":[0]},
                {"role":"Coder","description":"implement","depends_on":[0]}
            ]"#,
            agents,
        )
        .await;

        assert!(matches!(result, Err(OrchestratorError::MultiAgentPlanFailed { .. })));
    }

    #[tokio::test]
    async fn no_implement_agent_fails_fast_before_llm_call() {
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("reviewer"),
                stage: Some(AgentStage::new("review")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let result = plan_with_agents(
            r#"[{"role":"Researcher","description":"inspect","depends_on":[]}]"#,
            agents,
        )
        .await;

        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = result else {
            panic!("expected MultiAgentPlanFailed, got: {result:?}");
        };
        assert!(reason.contains("no implementation-stage agent"), "unexpected reason: {reason}");
    }

    /// ADR-35 §8 trigger 1: when the coordinator self-executes (no registered
    /// implement agent), the roster carries the reserved `coordinator` id with
    /// an implement-stage tag — the planner must accept it as a plan role and
    /// therefore NOT fail fast on the missing-implement-agent check.
    #[tokio::test]
    async fn coordinator_self_role_counts_as_implement_stage_agent() {
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("coordinator"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let tasks = plan_with_agents(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[]},
                {"role":"coordinator","description":"implement","depends_on":[0]}
            ]"#,
            agents,
        )
        .await
        .unwrap_or_else(|error| panic!("coordinator self-role must plan, got: {error}"));

        assert_eq!(tasks[1].role, AgentId::new("coordinator"));
        assert_eq!(
            tasks[1].dependencies,
            vec![tasks[0].id],
            "the implement task must depend on the research task"
        );
    }

    #[tokio::test]
    async fn planner_retries_empty_response_once_then_succeeds() {
        // First call returns empty content, the retry returns a valid plan:
        // the planner must keep the second response and call back exactly twice.
        let provider = Arc::new(SequenceProvider {
            responses: vec![String::new(), artifacts_plan_json().to_owned()],
            calls: AtomicUsize::new(0),
            seen_max_tokens: Mutex::new(Vec::new()),
        });
        let probe = provider.clone();

        let outcome = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "retry task"),
                None,
                &builtin_agents(),
                provider,
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                None,
            )
            .await
            .expect("a plan succeeds after one empty-content retry");
        assert_eq!(outcome.tasks.len(), 2, "the retried response yields the planned subtasks");
        assert_eq!(outcome.tasks[0].role, AgentId::new("researcher"));
        assert_eq!(outcome.tasks[1].role, AgentId::new("coder"));
        assert_eq!(
            probe.calls.load(Ordering::Relaxed),
            2,
            "the planner retried the empty response exactly once"
        );
        // The first attempt ran with the base budget; the retry raised it so
        // reasoning deltas cannot starve the visible content.
        assert_eq!(
            probe
                .seen_max_tokens
                .lock()
                .expect("max-token recording lock is not poisoned")
                .as_slice(),
            &[2048, 16384],
            "the retry must use the raised output budget"
        );
    }

    #[tokio::test]
    async fn planner_fails_with_clear_reason_after_two_empty_responses() {
        // A single "" entry repeats forever: both attempts stay empty, so the
        // planner must give up with a clear `MultiAgentPlanFailed` reason.
        let provider = Arc::new(SequenceProvider {
            responses: vec![String::new()],
            calls: AtomicUsize::new(0),
            seen_max_tokens: Mutex::new(Vec::new()),
        });
        let probe = provider.clone();

        let result = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "empty task"),
                None,
                &builtin_agents(),
                provider,
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                None,
            )
            .await;

        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = result else {
            panic!("expected MultiAgentPlanFailed, got: {result:?}");
        };
        assert!(reason.contains("empty response"), "unexpected reason: {reason}");
        assert_eq!(
            probe.calls.load(Ordering::Relaxed),
            2,
            "the planner stopped after two empty attempts"
        );
        // Both attempts ran, and the retry still used the raised budget even
        // though it failed to produce content.
        assert_eq!(
            probe
                .seen_max_tokens
                .lock()
                .expect("max-token recording lock is not poisoned")
                .as_slice(),
            &[2048, 16384],
            "the retry must use the raised output budget"
        );
    }

    #[tokio::test]
    async fn forward_or_self_dependencies_are_rejected() {
        let result = plan(
            r#"[
                {"role":"Researcher","description":"inspect","depends_on":[0]},
                {"role":"Coder","description":"implement","depends_on":[0]}
            ]"#,
        )
        .await;

        assert!(matches!(result, Err(OrchestratorError::MultiAgentPlanFailed { .. })));
    }

    #[test]
    fn build_prompt_injects_skills_section() {
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "test task");
        let prompt =
            TaskPlanner.build_prompt(&task, None, &builtin_agents(), "## Skills\nDo X.", None);
        assert!(prompt.contains("## Skills"));
        assert!(prompt.contains("Do X."));
        assert!(prompt.contains("Task: test task"));
        // System prompt content still precedes the skills section.
        assert!(prompt.find("Available roles").unwrap() < prompt.find("## Skills").unwrap());
    }

    #[test]
    fn build_prompt_omits_skills_when_empty() {
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "test task");
        let prompt = TaskPlanner.build_prompt(&task, None, &builtin_agents(), "", None);
        assert!(!prompt.contains("## Skills"));
        assert!(prompt.contains("Task: test task"));
    }

    // ------------------------------------------------------------------
    // ADR-35 phase 4: roster enrichment — the prompt describes what each
    // role can actually do via capabilities/description.
    // ------------------------------------------------------------------

    #[test]
    fn custom_role_with_write_capability_renders_write_and_description() {
        // A write-capable custom role is described by its capability flags
        // and human description. The file-ownership instruction stays the
        // historical, always-accurate copy: the built-in Coder owns plan
        // files despite carrying no config capability flags (its write
        // access is the legacy tool-calling route), so a claimed fs_write
        // gate would contradict the built-in pipeline.
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("docs-writer"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities {
                    fs_read: Some(true),
                    fs_write: Some(true),
                    shell: Some(true),
                    ..Default::default()
                },
                description: "Docs Writer — docs-writer".into(),
            },
        ];
        let prompt = TaskPlanner::system_prompt(&agents, None);
        assert!(
            prompt.contains(
                "Coder (id: docs-writer; fs: read, write; shell) — Docs Writer — docs-writer"
            ),
            "the write capability and description must be rendered: {prompt}"
        );
        assert!(
            prompt.contains("Custom agents cannot write files"),
            "the file-ownership instruction must remain: {prompt}"
        );
        // The available-roles section still instructs the model not to
        // invent ids.
        assert!(prompt.contains("do not invent ids"));
    }

    #[test]
    fn read_only_custom_role_renders_without_write_capability() {
        // A read-only custom role shows its fs: read capability only — no
        // write flag leaks into the roster line.
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("auditor"),
                stage: None,
                capabilities: AgentCapabilities { fs_read: Some(true), ..Default::default() },
                description: "Auditor — auditor".into(),
            },
            PlannerAgentInfo {
                id: AgentId::new("coder"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let prompt = TaskPlanner::system_prompt(&agents, None);
        assert!(
            prompt.contains("custom agent (id: auditor; fs: read) — Auditor — auditor"),
            "the read capability must render without a write flag: {prompt}"
        );
        assert!(
            !prompt.contains("fs: read, write"),
            "no role on this roster declares the write flag: {prompt}"
        );
    }

    // ------------------------------------------------------------------
    // ADR-52: durable plan artifact
    // ------------------------------------------------------------------

    /// `PlanArtifact::from_planned` carries the task text, a fresh run-scoped
    /// id, stringified dependencies, and per-task expected artifacts — the
    /// fields that keep the on-disk JSON readable and reproducible.
    #[tokio::test]
    async fn plan_artifact_from_planned_renders_durable_snapshot() {
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "Test durable plan");
        let tasks = plan(artifacts_plan_json()).await.unwrap();
        assert!(tasks.len() >= 2, "expected the plan to produce subtasks");

        let artifact = PlanArtifact::from_planned(&task, &tasks);
        assert!(!artifact.plan_id.is_empty(), "plan_id must be populated");
        assert_eq!(artifact.task_description, "Test durable plan");
        assert_eq!(artifact.tasks.len(), tasks.len());
        // The mirrored snapshot mirrors the planned subtasks in order, with
        // the researcher first and its dependency set reflected.
        let first = &artifact.tasks[0];
        assert_eq!(first.description, "inspect the API");
        assert!(first.dependencies.is_empty());
        let second = &artifact.tasks[1];
        assert_eq!(second.description, "implement the change");
        assert_eq!(second.dependencies, vec![first.id.clone()]);
        // The coder's planned expected artifacts are serialized as strings.
        let coder = artifact.tasks.iter().find(|t| t.role == "coder").expect("coder task");
        assert!(
            coder.expected_artifacts.iter().any(|path| path == "src/lib.rs"),
            "expected the coder artifact to carry its owned files, got: {:?}",
            coder.expected_artifacts,
        );
    }

    /// `pretty_json` renders stable, parseable JSON — the durable artifact
    /// written to `<app_data_dir>/plans/plan-<plan_id>.json`.
    #[tokio::test]
    async fn plan_artifact_pretty_json_round_trips() {
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "Round trip");
        let tasks = plan(artifacts_plan_json()).await.unwrap();
        let artifact = PlanArtifact::from_planned(&task, &tasks);
        let json = artifact.pretty_json().expect("artifact serializes");
        assert!(json.contains("\"plan_id\""), "serialized json lacks plan_id: {json}");
        assert!(json.contains("\"task_description\""));

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["plan_id"], artifact.plan_id);
        assert_eq!(parsed["tasks"].as_array().map(Vec::len), Some(artifact.tasks.len()));
    }

    #[tokio::test]
    async fn plan_outcome_carries_artifact_with_matching_tasks() {
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "outcome task");
        let outcome = TaskPlanner
            .plan(
                &task,
                None,
                &builtin_agents(),
                Arc::new(StaticProvider { response: artifacts_plan_json().into() }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                None,
            )
            .await
            .expect("plan should succeed");
        assert_eq!(outcome.tasks.len(), outcome.artifact.tasks.len());
        for (planned, rendered) in outcome.tasks.iter().zip(outcome.artifact.tasks.iter()) {
            assert_eq!(planned.id.to_string(), rendered.id);
            assert_eq!(planned.description, rendered.description);
        }
    }

    /// Standard planner response exercising artifact ownership.
    fn artifacts_plan_json() -> &'static str {
        r#"[
            {"role":"Researcher","description":"inspect the API","depends_on":[]},
            {"role":"Coder","description":"implement the change","depends_on":[0],"files":["src/lib.rs"]}
        ]"#
    }

    /// A primary `Execution` stage tagged `build` (not `implement`) with a
    /// `files` plan-artifact contract (blueprint §5.7), resolved through the
    /// crate facade.
    fn build_facade() -> concerto_config::BlueprintFacade {
        let blueprint = concerto_config::Blueprint {
            schema_version: concerto_config::ORCHESTRATION_SCHEMA_VERSION,
            name: "build-execution".to_string(),
            description: None,
            pipeline: concerto_config::PipelineDef {
                stages: vec![
                    concerto_config::StageDef {
                        tag: "research".to_string(),
                        label: "Research".to_string(),
                        kind: concerto_config::StageKind::Research.as_str().to_string(),
                        version: 1,
                        flags: concerto_config::StageFlags::default(),
                        condition: concerto_config::StageCondition::Always,
                        max_cycles: None,
                        feed: None,
                        primary: false,
                        agents: vec!["researcher".to_string()],
                        fallback: None,
                        files: None,
                    },
                    concerto_config::StageDef {
                        tag: "build".to_string(),
                        label: "Builder".to_string(),
                        kind: concerto_config::StageKind::Execution.as_str().to_string(),
                        version: 1,
                        flags: concerto_config::StageFlags::default(),
                        condition: concerto_config::StageCondition::Always,
                        max_cycles: None,
                        feed: None,
                        primary: true,
                        agents: vec!["builder".to_string()],
                        fallback: None,
                        files: Some(concerto_config::ExecutionFilesDef {
                            ownership: "plan.files".to_string(),
                            expected_artifacts: vec!["src/main.rs".to_string()],
                        }),
                    },
                    concerto_config::StageDef {
                        tag: "accept".to_string(),
                        label: "Acceptance".to_string(),
                        kind: concerto_config::StageKind::Acceptance.as_str().to_string(),
                        version: 1,
                        flags: concerto_config::StageFlags::default(),
                        condition: concerto_config::StageCondition::Always,
                        max_cycles: None,
                        feed: None,
                        primary: false,
                        agents: vec!["validator".to_string()],
                        fallback: None,
                        files: None,
                    },
                ],
            },
            relationships: Vec::new(),
        };
        let resolved = concerto_config::resolve_blueprint(&blueprint)
            .expect("the fixture blueprint resolves against the rulebook");
        concerto_config::BlueprintFacade::new(&resolved)
    }

    /// ADR-58 P2+P3 (R7/R8): with a facade attached, a primary `Execution`
    /// stage tagged `build` is partitioned as the implement stage and its
    /// `files.expected_artifacts` contract (not the LLM's proposed `files`)
    /// becomes the task's artifact ownership.
    #[tokio::test]
    async fn facade_standard_and_custom_execution_partition_and_artifact_contract() {
        let facade = build_facade();
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("builder"),
                stage: Some(AgentStage::new("build")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("coder"),
                stage: Some(AgentStage::new("implement")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("validator"),
                stage: Some(AgentStage::new("accept")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        // The plan references the `build`-staged role as a planner role with a
        // proposed file the files block does not list.
        let outcome = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "test"),
                None,
                &agents,
                Arc::new(StaticProvider {
                    response: r#"[
                        {"role":"builder","description":"implement","depends_on":[],
                         "files":["proposed.rs"]}
                    ]"#
                    .to_string(),
                }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                Some(&facade),
            )
            .await
            .expect("a build-staged role is an implement role");
        assert_eq!(outcome.tasks.len(), 1);
        assert_eq!(
            outcome.tasks[0].expected_artifacts,
            vec![camino::Utf8PathBuf::from("src/main.rs")],
            "the Execution files contract supersedes the LLM's proposed files"
        );

        // Without a facade the same tags fall back to the tag-based
        // classification: `build` is a freeform custom role (its planned file
        // is not an artifact), while `implement` is the implement role and
        // keeps its proposed files verbatim.
        let outcome = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "test"),
                None,
                &agents,
                Arc::new(StaticProvider {
                    response: r#"[
                        {"role":"builder","description":"build the crate","depends_on":[],
                         "files":["proposed.rs"]},
                        {"role":"coder","description":"implement","depends_on":[0],
                         "files":["proposed.rs"]}
                    ]"#
                    .to_string(),
                }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                None,
            )
            .await
            .expect("the legacy path plans the unknown tag as a custom role");
        assert_eq!(
            outcome.tasks[0].expected_artifacts,
            Vec::<camino::Utf8PathBuf>::new(),
            "a freeform custom role owns no artifacts"
        );
        assert_eq!(
            outcome.tasks[1].expected_artifacts,
            vec![camino::Utf8PathBuf::from("proposed.rs")],
            "the implement role keeps its proposed files without a facade"
        );
    }

    /// ADR-58 P2+P3 (R10): the missing-implement fails-fast keys off the
    /// primary `Execution`-kind stage; a differently-tagged primary stage
    /// enriches the reason with its label, while the default `implement`
    /// primary keeps the legacy message byte-for-byte.
    #[tokio::test]
    async fn facade_primary_execution_stage_drives_implement_fails_fast() {
        let facade = build_facade();
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            // No agent is staffed in the primary Execution (`build`) stage.
        ];
        let result = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "test"),
                None,
                &agents,
                Arc::new(StaticProvider { response: String::new() }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                Some(&facade),
            )
            .await;
        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = result else {
            panic!("expected MultiAgentPlanFailed, got: {result:?}");
        };
        assert!(
            reason.contains("Builder") && reason.contains("Execution-stage"),
            "enriched reason references the primary stage label: {reason}"
        );

        // The default `standard` blueprint primary stage is tagged `implement`:
        // reason stays byte-identical to the legacy message.
        let standard = concerto_config::BlueprintFacade::new(
            &concerto_config::resolve_blueprint(
                &concerto_config::named_blueprint("standard")
                    .expect("the standard blueprint ships by name"),
            )
            .expect("the standard blueprint resolves"),
        );
        let result = TaskPlanner
            .plan(
                &AgentTask::new(concerto_core::ids::Ulid::new(), "test"),
                None,
                &agents,
                Arc::new(StaticProvider { response: String::new() }),
                &RetryPolicy::default(),
                &EventBus::default(),
                CancellationToken::new(),
                "",
                Some(&standard),
            )
            .await;
        let Err(OrchestratorError::MultiAgentPlanFailed { reason }) = result else {
            panic!("expected MultiAgentPlanFailed, got: {result:?}");
        };
        assert_eq!(
            reason,
            "no implementation-stage agent is registered; cannot plan implementation work"
        );
    }

    /// ADR-58 P2+P3 (F3): `system_prompt` partitions the role lists through
    /// the same facade lookup as the rest of the planner — a renamed-tag
    /// blueprint classifies identically everywhere, while the default
    /// `standard` blueprint renders byte-identical to the tag-based match.
    #[test]
    fn system_prompt_classifies_renamed_stage_tags_via_facade_partition() {
        // `build_facade` retags the primary Execution stage "build" and the
        // Acceptance stage "accept": with the facade the `build` role renders
        // as a Coder and the `accept` role as lifecycle-managed; without it
        // both are freeform custom roles (legacy tag-based classification).
        let facade = build_facade();
        let agents = vec![
            PlannerAgentInfo {
                id: AgentId::new("researcher"),
                stage: Some(AgentStage::new("research")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("builder"),
                stage: Some(AgentStage::new("build")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
            PlannerAgentInfo {
                id: AgentId::new("validator"),
                stage: Some(AgentStage::new("accept")),
                capabilities: AgentCapabilities::default(),
                description: String::new(),
            },
        ];
        let with_facade = TaskPlanner::system_prompt(&agents, Some(&facade));
        assert!(
            with_facade.contains("Coder (id: builder)"),
            "the Execution-kind `build` stage renders as a Coder with the facade: {with_facade}"
        );
        assert!(
            with_facade.contains("Design is handled automatically before planning (by validator)."),
            "the Acceptance-kind `accept` stage is lifecycle-managed: {with_facade}"
        );
        assert!(
            !with_facade.contains("custom agent (id: builder"),
            "no renamed Execution-stage role leaks into the custom list: {with_facade}"
        );

        let without_facade = TaskPlanner::system_prompt(&agents, None);
        assert!(
            without_facade.contains("custom agent (id: builder)"),
            "without a facade the `build` tag keeps the legacy freeform classification: \
             {without_facade}"
        );
        assert!(
            without_facade.contains("No design agent is registered"),
            "without a facade no renamed lifecycle stage is known: {without_facade}"
        );

        // On the default `standard` blueprint the facade partition and the
        // tag-based match are byte-identical — the pinned byte-identical
        // contract (§5).
        let standard = concerto_config::BlueprintFacade::new(
            &concerto_config::resolve_blueprint(
                &concerto_config::named_blueprint("standard")
                    .expect("the standard blueprint ships by name"),
            )
            .expect("the standard blueprint resolves"),
        );
        let standard_agents = builtin_agents();
        assert_eq!(
            TaskPlanner::system_prompt(&standard_agents, Some(&standard)),
            TaskPlanner::system_prompt(&standard_agents, None),
            "on `standard` the facade partition must render byte-identical to the tag-based match"
        );
    }

    /// Slice 1b: an unknown open kind string (e.g. "blogger") staffed by a
    /// custom agent classifies through the facade as a custom/freeform role —
    /// the generic dispatch path, exactly like `RunOnce`/stage-less agents —
    /// while the role still resolves to its stage. Nothing rejects or panics
    /// on the unknown kind.
    #[test]
    fn unknown_kind_stage_partitions_as_custom_via_facade() {
        let facade = blogger_facade();
        assert_eq!(
            planner_partition(Some(&AgentStage::new("blogger")), Some(&facade)),
            PlannerPartition::Custom,
            "an unknown-kind stage dispatches generically, like RunOnce/freeform"
        );
        assert_eq!(
            facade.stage_for_agent(&AgentId::new("blogger")).map(|s| s.def.tag.as_str()),
            Some("blogger"),
            "the staffed role still resolves to its stage"
        );
        // No known Execution stage: the primary-resolution degrades to
        // `None` and the fails-fast path keeps its legacy message.
        assert_eq!(facade.primary_execution_stage(), None);
        assert_eq!(
            implementation_artifact_contract(Some(&facade), &[]),
            Vec::<camino::Utf8PathBuf>::new(),
            "an Execution-free blueprint keeps the legacy fallback contract"
        );
    }

    /// A blueprint whose only stage is an unknown-kind `blogger` stage
    /// staffed by the `blogger` agent (Slice 1b generic-dispatch surface).
    fn blogger_facade() -> concerto_config::BlueprintFacade {
        let blueprint = concerto_config::Blueprint {
            schema_version: concerto_config::ORCHESTRATION_SCHEMA_VERSION,
            name: "blog-era".to_string(),
            description: None,
            pipeline: concerto_config::PipelineDef {
                stages: vec![concerto_config::StageDef {
                    tag: "blogger".to_string(),
                    label: "Blogger".to_string(),
                    kind: "blogger".to_string(), // open unknown user kind
                    version: 1,
                    flags: concerto_config::StageFlags::default(),
                    condition: concerto_config::StageCondition::Always,
                    max_cycles: None,
                    feed: None,
                    primary: false,
                    agents: vec!["blogger".to_string()],
                    fallback: None,
                    files: None,
                }],
            },
            relationships: Vec::new(),
        };
        concerto_config::BlueprintFacade::new(
            &concerto_config::resolve_blueprint(&blueprint)
                .expect("a custom-kind blueprint must validate and resolve"),
        )
    }
}
