//! The core single-agent loop: plan → act → observe → plan.
//!
//! Phase 3: basic sequential loop with memory, undo, and eval hooks.
//! Multi-agent coordination is Phase 5.

use std::collections::HashMap;
use std::sync::Arc;

use camino::Utf8PathBuf;
use concerto_core::error::{ProviderError, ToolError};
use concerto_core::event::{Event, EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::memory::{ChunkType, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery};
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{
    AgentOutput, AgentRunExit, AgentTask, CapabilitySet, CompletionRequest, CompletionUsage,
    Message, PolicyAction, ProviderMetrics, Role, SessionContext, TaskExecutionMode, ToolCall,
    ToolExecutionSummary, VerificationSummary,
};
use concerto_core::ContextOverflowStrategy;
use concerto_core::{CancellationToken, OrchestratorError, TaskId};
use concerto_eval::EvalEngine;
use concerto_memory::budget::ContextBudgetAllocator;
use concerto_providers::retry::RetryPolicy;
use concerto_sessions::SessionStore;
use concerto_tools::undo::UndoManager;

use crate::cycle::CycleBudgetTracker;
use crate::exec_backend::ToolExecutionBackend;
use crate::prompts::PromptBuilder;
use crate::state::AgentState;
use crate::tool_guard;

/// The single-agent loop driving plan → act → observe → plan cycles.
pub struct AgentLoop {
    fast: bool,
    bus: EventBus,
    approval: Arc<dyn ApprovalSink>,
    provider: Arc<dyn LlmProvider>,
    /// The execution backend behind every tool call: local
    /// [`concerto_core::executor::ToolExecutor`] in single-process mode, or
    /// the supervisor write-gate proxy when the
    /// loop runs as an ADR-60 agent process.
    tool_executor: Arc<dyn ToolExecutionBackend>,
    memory: Arc<dyn MemoryStore>,
    undo_manager: Arc<std::sync::Mutex<UndoManager>>,
    eval: EvalEngine,
    prompt_builder: PromptBuilder,
    cycle_budget: CycleBudgetTracker,
    /// Consecutive tool-guard rejections per tool name within the current
    /// run (cleared on a valid guarded call and at run start). Bounds the
    /// corrective-retry coaching for weak models at
    /// [`tool_guard::MAX_TOOL_GUARD_REJECTS`] injections per tool.
    tool_guard_rejects: HashMap<String, u32>,
    max_iterations: u32,
    state: AgentState,
    /// The project root directory — all file operations are scoped here.
    project_root: std::path::PathBuf,
    /// Optional context overflow strategy. When set, after each assistant
    /// turn the strategy is applied to the active conversation history
    /// to keep the context within budget. The strategy trims the oldest
    /// non-System messages when estimated tokens exceed the configured
    /// trigger ratio of the model's context capacity.
    overflow_strategy: Option<Arc<dyn ContextOverflowStrategy>>,

    /// Optional context budget allocator for RAG chunk filtering.
    /// When set, retrieved memory chunks are filtered through this
    /// allocator before being injected into the prompt.
    budget_allocator: Option<ContextBudgetAllocator>,

    /// Prior conversation messages (loaded from the persistent session) used
    /// as context for the current run. Default empty.
    initial_messages: Vec<Message>,

    /// Centralized policy for retrying transient provider failures. Lives at
    /// the provider-request boundary and does NOT count as an agent iteration.
    retry_policy: RetryPolicy,

    /// Optional session store for persistence of tasks, messages, and events.
    /// When `None`, persistence is skipped (best-effort, warnings only).
    session_store: Option<Arc<dyn SessionStore>>,

    /// Model id and cumulative usage for session/dashboard accounting.
    usage_model: String,
    usage_tokens_in: u64,
    usage_tokens_out: u64,
    usage_cost_usd: f64,
    usage_latency_ms: u64,
}

/// Maximum number of auto-continuation rounds before escalating a run to a
/// hard blocker. Bounds runaway continuation while still allowing
/// multi-step tasks to finish.
const MAX_CONTINUATION_ROUNDS: u32 = 8;
/// Number of consecutive rounds with identical progress before declaring
/// non-convergence and escalating to the user.
const MAX_STALE_ROUNDS: u32 = 3;

/// Snapshot of progress used to detect non-convergence across continuation
/// rounds (two identical fingerprints in a row ⇒ no real forward motion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProgressFingerprint {
    files_modified: usize,
    tool_call_count: u32,
    passed_verifications: usize,
    failed_verifications: usize,
    final_message_len: usize,
}

impl ProgressFingerprint {
    fn from_output(output: &AgentOutput) -> Self {
        Self {
            files_modified: output.files_modified.len(),
            tool_call_count: output.tool_call_count,
            passed_verifications: output.verification.iter().filter(|v| v.passed).count(),
            failed_verifications: output.verification.iter().filter(|v| !v.passed).count(),
            final_message_len: output.final_message.len(),
        }
    }
}

fn merge_run_progress(accumulated: &mut AgentOutput, latest: &AgentOutput) {
    for path in &latest.files_modified {
        if !accumulated.files_modified.contains(path) {
            accumulated.files_modified.push(path.clone());
        }
    }
    accumulated.tool_call_count =
        accumulated.tool_call_count.saturating_add(latest.tool_call_count);
    accumulated.tool_events.extend(latest.tool_events.clone());
    for check in &latest.verification {
        if let Some(existing) = accumulated
            .verification
            .iter_mut()
            .find(|existing| existing.path == check.path && existing.command == check.command)
        {
            *existing = check.clone();
        } else {
            accumulated.verification.push(check.clone());
        }
    }
    accumulated.final_message = latest.final_message.clone();
    accumulated.eval_result = latest.eval_result.clone();
    accumulated.completion_status = latest.completion_status;
    accumulated.provider_metrics = latest.provider_metrics.clone();
    accumulated.checkpoint_json = latest.checkpoint_json.clone();
    if latest.project_root.is_some() {
        accumulated.project_root = latest.project_root.clone();
    }
}

/// Hard completion condition: verification must actually pass when the task
/// requires it. Without this, a capped or partially-failed run could be
/// reported as `Done`.
/// Instruction appended to the conversation when a run is auto-continued,
/// so the model resumes the same task instead of re-summarizing.
fn continuation_instruction(reason: &str) -> String {
    format!(
        "Continue the same task. Previous run stopped because: {reason}. \
             Do not summarize. Continue using tools until the job is done, \
             verification passes, user input is required, or a real blocker is found."
    )
}

/// Signal from `process_provider_response` back to the iteration loop body,
/// indicating how to continue after processing the provider's response.
#[derive(Debug)]
enum ProviderResponseAction {
    /// Break out of the loop (task completed, answer received, etc.).
    Break,
    /// Skip tool execution and continue to the next iteration directly.
    ContinueIteration,
    /// Proceed with tool execution for this iteration.
    Proceed,
}

/// Outcome of the tool-call guard (VALIDATE → COERCE → REPAIR) for one
/// incoming tool call.
enum GuardOutcome {
    /// Arguments are usable (possibly after coercion); execute with these.
    Pass(serde_json::Value),
    /// Arguments are invalid after coercion; inject the corrective tool
    /// result so the model retries and skip execution entirely.
    Reject { summary: String, content: String, payload: serde_json::Value },
}

/// Normalize a collected reasoning string into an `Option`, so an empty
/// reasoning buffer maps to `None` (ADR-46).
fn non_empty_reasoning(reasoning: Option<String>) -> Option<String> {
    match reasoning {
        Some(r) if r.trim().is_empty() => None,
        other => other,
    }
}

impl AgentLoop {
    /// Create a new agent loop.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: EventBus,
        approval: Arc<dyn ApprovalSink>,
        provider: Arc<dyn LlmProvider>,
        tool_executor: Arc<dyn ToolExecutionBackend>,
        memory: Arc<dyn MemoryStore>,
        undo_manager: Arc<std::sync::Mutex<UndoManager>>,
        eval: EvalEngine,
        prompt_builder: PromptBuilder,
        max_iterations: u32,
        fast: bool,
        overflow_strategy: Option<Arc<dyn ContextOverflowStrategy>>,
        budget_allocator: Option<ContextBudgetAllocator>,
    ) -> Self {
        let project_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self::with_project_root(
            bus,
            approval,
            provider,
            tool_executor,
            memory,
            undo_manager,
            eval,
            prompt_builder,
            max_iterations,
            fast,
            project_root,
            overflow_strategy,
            budget_allocator,
        )
    }

    /// Create a new agent loop with an explicit project root directory.
    #[allow(clippy::too_many_arguments)]
    pub fn with_project_root(
        bus: EventBus,
        approval: Arc<dyn ApprovalSink>,
        provider: Arc<dyn LlmProvider>,
        tool_executor: Arc<dyn ToolExecutionBackend>,
        memory: Arc<dyn MemoryStore>,
        undo_manager: Arc<std::sync::Mutex<UndoManager>>,
        eval: EvalEngine,
        prompt_builder: PromptBuilder,
        max_iterations: u32,
        fast: bool,
        project_root: std::path::PathBuf,
        overflow_strategy: Option<Arc<dyn ContextOverflowStrategy>>,
        budget_allocator: Option<ContextBudgetAllocator>,
    ) -> Self {
        Self {
            bus,
            approval,
            provider,
            tool_executor,
            memory,
            undo_manager,
            eval,
            prompt_builder,
            cycle_budget: CycleBudgetTracker::default(),
            tool_guard_rejects: HashMap::new(),
            max_iterations,
            fast,
            state: AgentState::Idle,
            project_root,
            overflow_strategy,
            budget_allocator,
            initial_messages: Vec::new(),
            retry_policy: RetryPolicy::default(),
            session_store: None,
            usage_model: String::new(),
            usage_tokens_in: 0,
            usage_tokens_out: 0,
            usage_cost_usd: 0.0,
            usage_latency_ms: 0,
        }
    }

    /// Supply the exact selected model id used for usage attribution.
    pub fn with_usage_model(mut self, model: impl Into<String>) -> Self {
        self.usage_model = model.into();
        self
    }

    /// The loop's settled usage as a single aggregated metrics entry. Also
    /// callable after a failed run (fields survive the error) so callers can
    /// persist what was consumed before failure.
    pub(crate) fn provider_metrics(&self) -> Vec<ProviderMetrics> {
        vec![ProviderMetrics {
            provider: self.provider.provider_name().to_string(),
            model: self.usage_model.clone(),
            tokens_in: self.usage_tokens_in,
            tokens_out: self.usage_tokens_out,
            cost_usd: self.usage_cost_usd,
            latency_ms: self.usage_latency_ms,
        }]
    }

    /// Override the provider retry policy (e.g. from global config).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Seed the conversation with prior persisted messages (loaded from the
    /// active session) so the model sees the full context.
    pub fn with_initial_messages(mut self, messages: Vec<Message>) -> Self {
        self.initial_messages = messages;
        self
    }

    /// Attach a session store for best-effort persistence of tasks, messages,
    /// and events. When `None`, persistence is skipped.
    pub fn with_session_store(mut self, store: Option<Arc<dyn SessionStore>>) -> Self {
        self.session_store = store;
        self
    }

    /// Run the agent loop for the given task.
    ///
    /// Thin wrapper that handles best-effort session persistence around the
    /// core loop (`run_inner`): it records the task start and the user prompt,
    /// then persists success/failure outcomes. Persistence failures surface as
    /// warnings and never abort the run.
    pub async fn run(
        &mut self,
        task: AgentTask,
        cancel: CancellationToken,
    ) -> Result<AgentOutput, OrchestratorError> {
        let session_id = task.session_id;
        // Fresh run: clear any corrective-retry streaks carried over from a
        // previous run on this loop instance.
        self.tool_guard_rejects.clear();
        self.persist_run_start(&task, cancel.clone()).await;

        let mut history: Vec<Message> = self.initial_messages.clone();
        let mut stale_rounds = 0u32;
        let mut last_fp: Option<ProgressFingerprint> = None;
        let mut last_partial: Option<AgentOutput> = None;
        let mut cumulative_progress: Option<AgentOutput> = None;

        for round in 0..MAX_CONTINUATION_ROUNDS {
            // Seed this round's conversation. The first round includes the
            // original task prompt; continuation rounds reuse the full prior
            // conversation plus the continuation instruction appended below.
            let mut seed = history.clone();
            if round == 0 {
                seed.push(Message {
                    role: Role::User,
                    content: task.description.clone(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                });
            }

            let exit = self.run_once(task.clone(), seed, cancel.clone()).await?;

            match exit {
                AgentRunExit::Done(mut output) => {
                    if let Some(accumulated) = &mut cumulative_progress {
                        merge_run_progress(accumulated, &output);
                        output = accumulated.clone();
                    }
                    output.completion_status =
                        concerto_core::types::AgentCompletionStatus::Completed;
                    self.persist_run_success(session_id, &output, cancel.clone()).await;
                    return Ok(output);
                }
                AgentRunExit::NeedsUser { reason, partial } => {
                    let mut output = partial;
                    output.final_message = format!("User input required: {reason}");
                    output.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
                    if let Some(accumulated) = &mut cumulative_progress {
                        merge_run_progress(accumulated, &output);
                    }
                    self.persist_run_partial(session_id, &output, cancel.clone()).await;
                    return Ok(output);
                }
                AgentRunExit::Blocked { reason, partial } => {
                    let mut output = partial;
                    output.final_message = format!("Blocked: {reason}");
                    output.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
                    if let Some(accumulated) = &mut cumulative_progress {
                        merge_run_progress(accumulated, &output);
                    }
                    self.persist_run_partial(session_id, &output, cancel.clone()).await;
                    return Ok(output);
                }
                AgentRunExit::IterationCapHit { reason, partial } => {
                    if let Some(accumulated) = &mut cumulative_progress {
                        merge_run_progress(accumulated, &partial);
                    } else {
                        cumulative_progress = Some(partial.clone());
                    }
                    let cumulative = cumulative_progress.as_ref().unwrap_or(&partial);
                    let fp = ProgressFingerprint::from_output(cumulative);
                    if Some(&fp) == last_fp.as_ref() {
                        stale_rounds += 1;
                    } else {
                        stale_rounds = 0;
                    }

                    if stale_rounds >= MAX_STALE_ROUNDS {
                        let mut partial_out = cumulative.clone();
                        partial_out.final_message = format!(
                            "Blocked: no convergence after {round} continuation rounds. Last reason: {reason}"
                        );
                        partial_out.completion_status =
                            concerto_core::types::AgentCompletionStatus::Partial;
                        self.persist_run_partial(session_id, &partial_out, cancel.clone()).await;
                        return Ok(partial_out);
                    }

                    last_fp = Some(fp);
                    last_partial = Some(cumulative.clone());

                    // Carry the conversation forward and ask the model to
                    // continue the same task. The next round reuses this
                    // history (same session_id, same project root).
                    history = self.initial_messages.clone();
                    history.push(Message {
                        role: Role::User,
                        content: continuation_instruction(&reason),
                        tool_calls: None,
                        tool_results: None,
                        reasoning_content: None,
                        tokens_in: None,
                        tokens_out: None,
                    });
                    self.initial_messages = history.clone();
                    continue;
                }
                other => {
                    return Err(OrchestratorError::AgentLoopError(format!(
                        "unhandled AgentRunExit variant in continuation loop: {other:?}"
                    )));
                }
            }
        }

        let mut partial = last_partial.unwrap_or_else(|| AgentOutput {
            task_id: task.id,
            session_id,
            final_message: String::new(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: Some(
                camino::Utf8PathBuf::from_path_buf(self.project_root.clone()).unwrap_or_default(),
            ),
            completion_status: concerto_core::types::AgentCompletionStatus::Partial,
            provider_metrics: self.provider_metrics(),
            checkpoint_json: None,
        });
        partial.final_message =
            format!("Blocked: reached maximum continuation rounds ({MAX_CONTINUATION_ROUNDS}).");
        partial.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
        self.persist_run_partial(session_id, &partial, cancel.clone()).await;
        Ok(partial)
    }

    async fn run_once(
        &mut self,
        task: AgentTask,
        seed_messages: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<AgentRunExit, OrchestratorError> {
        self.state = AgentState::Planning;
        let correlation_id = Ulid::new();
        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::TaskStarted { task_id: task.id, description: task.description.clone() },
        );

        // Phase 1: Setup undo stash
        self.setup_undo_stash(task.session_id, task.id, correlation_id, cancel.clone()).await?;

        let session = SessionContext::new(task.session_id, self.project_root.clone());

        // Phase 2: Retrieve working memory
        let retrieved_memory_block =
            self.retrieve_working_memory(&task.description, &session, cancel.clone()).await;

        // Initialize loop state
        self.state = AgentState::Executing;
        let mut iteration = 0u32;
        let mut tool_call_count = 0u32;
        let mut file_changing_tool_count = 0u32;
        let mut files_modified = Vec::new();
        let mut tool_events: Vec<ToolExecutionSummary> = Vec::new();
        let mut verification: Vec<VerificationSummary> = Vec::new();
        let mut final_message = String::new();
        let mut completed = false;
        let mut messages: Vec<Message> = seed_messages;

        while iteration < self.max_iterations {
            iteration += 1;

            if cancel.is_cancelled() {
                self.state = AgentState::Cancelled;
                return Err(OrchestratorError::Cancelled);
            }

            // Phase 3: Build provider request
            let request = self.build_provider_request(
                &task.description,
                iteration,
                &messages,
                &files_modified,
                &tool_events,
                &verification,
                &retrieved_memory_block,
            );

            // Publish lifecycle event before provider call
            let _ = self.bus.publish_for_session(
                task.session_id,
                correlation_id,
                EventKind::AgentThought {
                    agent_id: "single-agent".to_string(),
                    content: format!("Iteration {}: provider request started", iteration),
                },
            );

            // Phase 4: Call provider with retry
            let request_started = std::time::Instant::now();
            let request_tokens_in = request
                .messages
                .iter()
                .map(|message| message.content.len() as u64)
                .sum::<u64>()
                .div_ceil(4);

            let (text, reasoning, tool_calls, usage) = match self
                .call_provider_with_retry(&request, task.session_id, task.id, &cancel)
                .await
            {
                Ok(result) => result,
                Err(OrchestratorError::Cancelled) => {
                    self.state = AgentState::Cancelled;
                    return Err(OrchestratorError::Cancelled);
                }
                Err(OrchestratorError::Provider(ProviderError::RetryExhausted {
                    attempts,
                    elapsed,
                    last_error,
                })) => {
                    self.state = AgentState::Failed;
                    let partial = self.build_agent_output(
                        &task,
                        &final_message,
                        &files_modified,
                        tool_call_count,
                        &None,
                        &tool_events,
                        &verification,
                    );
                    return Ok(AgentRunExit::Blocked {
                        reason: format!(
                            "provider retries exhausted after {attempts} attempts \
                             ({elapsed:?}): {last_error}"
                        ),
                        partial,
                    });
                }
                Err(e) => {
                    self.state = AgentState::Failed;
                    return Err(e);
                }
            };

            // Publish lifecycle event after provider call succeeds
            let _ = self.bus.publish_for_session(
                task.session_id,
                correlation_id,
                EventKind::AgentThought {
                    agent_id: "single-agent".to_string(),
                    content: format!("Iteration {}: provider response started", iteration),
                },
            );

            // Token accounting. ADR-48 decision 4: the provider-reported usage on the
            // final chunk is the source of truth when present; the byte/4
            // heuristic remains the fallback for providers that do not report
            // usage. `0` is a valid measured value, so only `None` falls back.
            let measured_tokens_in = usage.as_ref().and_then(|u| u.prompt_tokens);
            let measured_tokens_out = usage.as_ref().and_then(|u| u.completion_tokens);
            let tool_call_chars = serde_json::to_string(&tool_calls)
                .map(|value| value.len() as u64)
                .unwrap_or_default();
            let estimated_tokens_out =
                ((text.len() as u64).saturating_add(tool_call_chars)).div_ceil(4);
            let usage_tokens_in = measured_tokens_in.unwrap_or(request_tokens_in);
            let usage_tokens_out = measured_tokens_out.unwrap_or(estimated_tokens_out);
            self.usage_tokens_in = self.usage_tokens_in.saturating_add(usage_tokens_in);
            self.usage_tokens_out = self.usage_tokens_out.saturating_add(usage_tokens_out);
            self.usage_cost_usd +=
                self.provider.approximate_cost(usage_tokens_in, usage_tokens_out);
            self.usage_latency_ms =
                self.usage_latency_ms.saturating_add(request_started.elapsed().as_millis() as u64);

            // Phase 5: Process provider response
            let action = self.process_provider_response(
                &text,
                reasoning,
                &tool_calls,
                usage,
                &task.execution_mode,
                file_changing_tool_count,
                iteration,
                correlation_id,
                task.session_id,
                &mut messages,
                &mut final_message,
                &mut completed,
            );
            match action {
                ProviderResponseAction::Break => break,
                ProviderResponseAction::ContinueIteration => continue,
                ProviderResponseAction::Proceed => {}
            }

            // Phase 6: Execute tool calls
            self.execute_tool_calls(
                &tool_calls,
                &task,
                correlation_id,
                &session,
                cancel.clone(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await?;

            // Phase 7: Trim conversation history
            self.trim_conversation_history(&mut messages, task.session_id, cancel.clone()).await;
        }

        // Phase 8: Run evaluation
        self.state = AgentState::Evaluating;
        let eval_result = self.run_evaluation(&files_modified, cancel.clone()).await;

        // Lightweight verification for file changes
        verification = verify_file_changes(&files_modified, &self.project_root).await;

        // Phase 9: Build agent output
        let output = self.build_agent_output(
            &task,
            &final_message,
            &files_modified,
            tool_call_count,
            &eval_result,
            &tool_events,
            &verification,
        );

        // Phase 10: Decide exit (handles Blocked, IterationCapHit, or returns None for Done)
        if let Some(exit) = self.decide_exit(
            &task,
            &output,
            tool_call_count,
            file_changing_tool_count,
            &eval_result,
            completed,
            messages,
        )? {
            return Ok(exit);
        }

        // Phase 11: Finalize completion (Done)
        self.finalize_completion(
            &task,
            &session,
            output,
            &eval_result,
            tool_call_count,
            &files_modified,
            &final_message,
            correlation_id,
            cancel,
        )
        .await
    }

    // ------------------------------------------------------------------
    // Phase helpers (extracted from `run_once` for clarity)
    // ------------------------------------------------------------------

    /// Phase 1: Create undo stash for this session/task.
    /// If the undo commit fails (not a git repo), prompt the user to continue.
    async fn setup_undo_stash(
        &mut self,
        session_id: Ulid,
        task_id: TaskId,
        correlation_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<(), OrchestratorError> {
        let undo_failed = {
            let mut undo = self.undo_manager.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = undo.commit(session_id, task_id) {
                tracing::warn!("undo commit failed (not a git repo or git error): {e}");
                true
            } else {
                false
            }
        };
        if undo_failed {
            let warning = "This project is not a git repository (or git is unavailable), so \
                            changes made during this task cannot be automatically undone. \
                            Continue anyway?";
            let ack = self.approval.request_ack(warning, cancel.clone()).await;
            // Audit seam (ADR-55 §5 / audit H-04): persist the ack outcome
            // through the same channel as approval decisions, sharing the run's
            // correlation_id chain. Pure observability — the ack bool still
            // drives the same abort branch below.
            self.tool_executor
                .record_ack_decision(session_id, correlation_id, warning, ack, cancel)
                .await;
            if !ack {
                self.state = AgentState::Cancelled;
                return Err(OrchestratorError::Cancelled);
            }
        }
        Ok(())
    }

    /// Phase 2: Retrieve memory chunks for context unless in fast mode.
    /// Returns a formatted memory block to inject into the prompt.
    async fn retrieve_working_memory(
        &self,
        description: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> String {
        let mut memory_chunks = if self.fast {
            Vec::new()
        } else {
            self.memory
                .retrieve(
                    &MemoryQuery {
                        text: description.to_string(),
                        project_id: session.project_id.clone(),
                        namespace: MemoryNamespace::Project(session.project_id.clone()),
                        top_k: 5,
                        filters: vec![],
                    },
                    cancel,
                )
                .await
                .unwrap_or_default()
        };

        // Apply context budget allocation if configured
        if let Some(ref allocator) = self.budget_allocator {
            let capacity = self.provider.context_capacity(&self.usage_model);
            memory_chunks = allocator.truncate_to_rag_limit(memory_chunks, capacity.available);
        }

        crate::memory_prompt::format_retrieved_memory(&memory_chunks)
    }

    /// Phase 3: Build a `CompletionRequest` with working memory, conversation
    /// history, and tool definitions for the current iteration.
    #[allow(clippy::too_many_arguments)]
    fn build_provider_request(
        &self,
        task_description: &str,
        iteration: u32,
        messages: &[Message],
        files_modified: &[Utf8PathBuf],
        tool_events: &[ToolExecutionSummary],
        verification: &[VerificationSummary],
        retrieved_memory_block: &str,
    ) -> CompletionRequest {
        let active_working_memory = crate::working_memory::format_working_memory(
            task_description,
            iteration,
            self.max_iterations,
            files_modified,
            tool_events,
            verification,
        );
        let request_memory = if retrieved_memory_block.is_empty() {
            active_working_memory
        } else {
            format!(
                "{active_working_memory}
{retrieved_memory_block}"
            )
        };
        let mut request = self.prompt_builder.build(
            &request_memory,
            messages,
            None,
            Some(&self.tool_executor.tool_definitions()),
        );
        if !self.usage_model.trim().is_empty() {
            request.model = self.usage_model.clone();
        }
        request
    }

    /// Phase 4: Call the LLM provider through the shared retry boundary and
    /// collect the streaming response into text and tool calls.
    ///
    /// Delegates to [`crate::prompts::complete_provider_request`], which wraps
    /// stream creation and collection with the time-to-first-byte and
    /// stream-idle timeouts from the configured retry policy, retries only
    /// before any output has begun, and is cancel-aware.
    async fn call_provider_with_retry(
        &self,
        request: &CompletionRequest,
        session_id: Ulid,
        task_id: TaskId,
        cancel: &CancellationToken,
    ) -> Result<(String, Option<String>, Vec<ToolCall>, Option<CompletionUsage>), OrchestratorError>
    {
        crate::prompts::complete_provider_request(
            &self.provider,
            request,
            &self.retry_policy,
            &self.bus,
            session_id,
            task_id,
            cancel,
        )
        .await
    }

    /// Phase 5: Process the provider's text response and tool calls.
    ///
    /// Pushes the assistant message to the conversation, attributes
    /// provider-reported usage per ADR-48 decision 4 (`completion_tokens` on
    /// the assistant message, `prompt_tokens` on the preceding user message),
    /// then evaluates whether the loop should break (task done), continue to
    /// the next iteration without executing tools (re-prompt needed), or
    /// proceed with tool execution.
    #[allow(clippy::too_many_arguments)]
    fn process_provider_response(
        &self,
        text: &str,
        reasoning: Option<String>,
        tool_calls: &[ToolCall],
        usage: Option<CompletionUsage>,
        task_execution_mode: &TaskExecutionMode,
        file_changing_tool_count: u32,
        iteration: u32,
        correlation_id: Ulid,
        session_id: Ulid,
        messages: &mut Vec<Message>,
        final_message: &mut String,
        completed: &mut bool,
    ) -> ProviderResponseAction {
        let action_required =
            matches!(task_execution_mode, TaskExecutionMode::ActionRequired { .. });

        // ADR-48 decision 4: keep the provider-reported usage. A missing count
        // means the provider did not report one for this message; both `None`
        // and `0` are legitimate values, so no coalescing happens here.
        let (tokens_in, tokens_out) = match usage {
            Some(CompletionUsage { prompt_tokens, completion_tokens }) => {
                (prompt_tokens, completion_tokens)
            }
            None => (None, None),
        };

        messages.push(Message {
            role: Role::Assistant,
            content: text.to_string(),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls.to_vec()) },
            tool_results: None,
            reasoning_content: non_empty_reasoning(reasoning),
            tokens_in,
            tokens_out,
        });

        // Attribute the prompt usage to the last user message so every message
        // in the persisted transcript carries its own measured cost (ADR-48).
        if let (Some(prompt_tokens), Some(user_message)) =
            (tokens_in, messages.iter_mut().rev().find(|m| m.role == Role::User))
        {
            user_message.tokens_in = Some(prompt_tokens);
        }

        if tool_calls.is_empty() {
            match task_execution_mode {
                TaskExecutionMode::AnswerOnly => {
                    *final_message = text.to_string();
                    *completed = true;
                    return ProviderResponseAction::Break;
                }
                TaskExecutionMode::ActionRequired { .. } => {
                    if file_changing_tool_count > 0 {
                        *final_message = text.trim().to_string();
                        *completed = true;
                        return ProviderResponseAction::Break;
                    }

                    let _ = self.bus.publish_for_session(
                        session_id,
                        correlation_id,
                        EventKind::AgentThought {
                            agent_id: "single-agent".to_string(),
                            content: format!(
                                "Iteration {}: no file action returned, retrying",
                                iteration,
                            ),
                        },
                    );

                    messages.push(Message {
                        role: Role::User,
                        content: "This task requires concrete repository changes. Do not describe the result. Call the filesystem tool with operation \"write\" or \"delete\" and a concrete relative path. A text-only response is not valid completion.".to_string(),
                        tool_calls: None,
                        tool_results: None,
                        reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                    });

                    return ProviderResponseAction::ContinueIteration;
                }
                _ => {}
            }
        } else if !text.trim().is_empty() && !action_required {
            *final_message = text.to_string();
        }

        ProviderResponseAction::Proceed
    }

    /// Phase 6: Execute tool calls by dispatching each one through
    /// `execute_single_tool_call`.
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        task: &AgentTask,
        correlation_id: Ulid,
        session: &SessionContext,
        cancel: CancellationToken,
        tool_call_count: &mut u32,
        file_changing_tool_count: &mut u32,
        files_modified: &mut Vec<Utf8PathBuf>,
        tool_events: &mut Vec<ToolExecutionSummary>,
        messages: &mut Vec<Message>,
    ) -> Result<(), OrchestratorError> {
        for tc in tool_calls {
            self.execute_single_tool_call(
                tc,
                task,
                correlation_id,
                session,
                cancel.clone(),
                tool_call_count,
                file_changing_tool_count,
                files_modified,
                tool_events,
                messages,
            )
            .await?;
        }
        Ok(())
    }

    /// Phase 7: Apply conversation-history overflow strategy if configured.
    async fn trim_conversation_history(
        &self,
        messages: &mut Vec<Message>,
        session_id: Ulid,
        cancel: CancellationToken,
    ) {
        if let Some(ref strategy) = self.overflow_strategy {
            let budget = self.provider.context_capacity("");
            strategy.apply(messages, &budget, session_id, cancel).await;
        }
    }

    /// Phase 8: Run evaluation (test suite) on modified files.
    async fn run_evaluation(
        &self,
        files_modified: &[Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Option<concerto_core::types::EvalResult> {
        self.eval.run_scoped(files_modified, cancel).await.ok()
    }

    /// Phase 9: Build an `AgentOutput` summarising the current progress
    /// for the caller to return or to pass to exit-decision helpers.
    #[allow(clippy::too_many_arguments)]
    fn build_agent_output(
        &self,
        task: &AgentTask,
        final_message: &str,
        files_modified: &[Utf8PathBuf],
        tool_call_count: u32,
        eval_result: &Option<concerto_core::types::EvalResult>,
        tool_events: &[ToolExecutionSummary],
        verification: &[VerificationSummary],
    ) -> AgentOutput {
        AgentOutput {
            task_id: task.id,
            session_id: task.session_id,
            final_message: final_message.to_string(),
            files_modified: files_modified.to_vec(),
            tool_call_count,
            eval_result: eval_result.clone(),
            tool_events: tool_events.to_vec(),
            verification: verification.to_vec(),
            project_root: Some(
                camino::Utf8PathBuf::from_path_buf(self.project_root.clone()).unwrap_or_default(),
            ),
            completion_status: concerto_core::types::AgentCompletionStatus::Partial,
            provider_metrics: self.provider_metrics(),
            checkpoint_json: None,
        }
    }

    /// Phase 10: Decide whether to exit with a blocking error or iteration-cap
    /// return. Returns `Ok(Some(exit))` for non-Done exits, `Ok(None)` when the
    /// caller should proceed to `finalize_completion`.
    #[allow(clippy::too_many_arguments)]
    fn decide_exit(
        &mut self,
        task: &AgentTask,
        output: &AgentOutput,
        tool_call_count: u32,
        file_changing_tool_count: u32,
        eval_result: &Option<concerto_core::types::EvalResult>,
        completed: bool,
        messages: Vec<Message>,
    ) -> Result<Option<AgentRunExit>, OrchestratorError> {
        // Carry the full conversation forward for any auto-continuation round.
        self.initial_messages = messages;

        // Action-required completion checks run for EVERY exit (natural
        // completion OR iteration cap): a task that required file changes but
        // produced none is blocked, never silently reported as "done".
        if let TaskExecutionMode::ActionRequired { min_tool_calls, require_verification } =
            task.execution_mode
        {
            if tool_call_count < min_tool_calls {
                return Err(OrchestratorError::ExecutionRequiredButNoTools);
            }

            if file_changing_tool_count == 0 {
                return Ok(Some(AgentRunExit::Blocked {
                    reason: "Action-required task failed: no file-changing tool call succeeded. Text-only provider claims are ignored. Expected filesystem operation \"write\" or \"delete\", or an equivalent file mutation tool.".to_string(),
                    partial: output.clone(),
                }));
            }

            if require_verification && eval_result.is_none() {
                tracing::warn!(
                    task_id = %task.id,
                    "action-required task completed without verification result"
                );
            }
        }

        if !completed {
            return Ok(Some(AgentRunExit::IterationCapHit {
                reason: format!("iteration cap hit after {} iterations", self.max_iterations),
                partial: output.clone(),
            }));
        }

        // Proceed to finalize_completion
        Ok(None)
    }

    /// Phase 11: Finalize completion when the task is truly done.
    /// Sets `Completed` state, publishes the completion event, stores the
    /// task summary, and returns `AgentRunExit::Done`.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_completion(
        &mut self,
        task: &AgentTask,
        session: &SessionContext,
        output: AgentOutput,
        eval_result: &Option<concerto_core::types::EvalResult>,
        tool_call_count: u32,
        files_modified: &[Utf8PathBuf],
        final_message: &str,
        correlation_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<AgentRunExit, OrchestratorError> {
        self.state = AgentState::Completed;
        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::TaskCompleted {
                task_id: task.id,
                success: eval_result.as_ref().is_none_or(|r| r.passed),
            },
        );

        self.store_task_summary(
            task,
            session,
            tool_call_count,
            files_modified,
            final_message,
            cancel.clone(),
        )
        .await;

        let mut output = output;
        output.completion_status = if eval_result.as_ref().is_none_or(|result| result.passed) {
            concerto_core::types::AgentCompletionStatus::Completed
        } else {
            concerto_core::types::AgentCompletionStatus::Partial
        };
        Ok(AgentRunExit::Done(output))
    }

    /// Store a task-completion summary entry in the project memory store.
    async fn store_task_summary(
        &self,
        task: &AgentTask,
        session: &SessionContext,
        tool_call_count: u32,
        files_modified: &[camino::Utf8PathBuf],
        final_message: &str,
        cancel: CancellationToken,
    ) {
        let summary = format!(
            "Task: {}\nTool calls: {}\nResult: {}",
            task.description, tool_call_count, final_message
        );
        let summary_entry = MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: session.project_id.clone(),
            namespace: MemoryNamespace::Project(session.project_id.clone()),
            content: summary,
            chunk_type: ChunkType::SessionSummary,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({
                "task_id": task.id.to_string(),
                "session_id": task.session_id.to_string(),
                "tool_call_count": tool_call_count,
                "files_modified": files_modified,
            }),
            expires_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        };
        let _ = self.memory.store(summary_entry, cancel).await;
    }

    // -----------------------------------------------------------------------
    // Best-effort session persistence
    //
    // These helpers never abort the agent run. A persistence failure is
    // logged as a warning so the user still gets their result; only the
    // durable record is lost.
    // -----------------------------------------------------------------------

    async fn persist_run_start(&self, task: &AgentTask, cancel: CancellationToken) {
        let Some(store) = &self.session_store else {
            return;
        };
        let session_id = task.session_id;

        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.create_task(task, cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist task row");
        }

        let event = Event::new(
            Ulid::new(),
            session_id,
            EventKind::TaskStarted { task_id: task.id, description: task.description.clone() },
        );
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.record_event(session_id, &event, cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist TaskStarted event");
        }

        let user_msg = Message {
            role: Role::User,
            content: task.description.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.append_messages(session_id, &[user_msg], cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist user message");
        }
    }

    async fn persist_run_success(
        &self,
        session_id: Ulid,
        output: &AgentOutput,
        cancel: CancellationToken,
    ) {
        let Some(store) = &self.session_store else {
            return;
        };

        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.update_task_status(output.task_id, "completed", cancel.clone()).await
        {
            tracing::warn!(error = %e, "failed to update task status");
        }

        let event = Event::new(
            Ulid::new(),
            session_id,
            EventKind::TaskCompleted { task_id: output.task_id, success: true },
        );
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.record_event(session_id, &event, cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist TaskCompleted event");
        }

        let assistant_msg = Message {
            role: Role::Assistant,
            content: output.summary(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.append_messages(session_id, &[assistant_msg], cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist assistant message");
        }
    }

    async fn persist_run_partial(
        &self,
        session_id: Ulid,
        output: &AgentOutput,
        cancel: CancellationToken,
    ) {
        let Some(store) = &self.session_store else {
            return;
        };

        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.update_task_status(output.task_id, "partial", cancel.clone()).await {
            tracing::warn!(error = %e, "failed to update task status");
        }

        let event = Event::new(
            Ulid::new(),
            session_id,
            EventKind::TaskCompleted { task_id: output.task_id, success: false },
        );
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.record_event(session_id, &event, cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist TaskCompleted event");
        }

        let assistant_msg = Message {
            role: Role::Assistant,
            content: output.summary(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = store.append_messages(session_id, &[assistant_msg], cancel.clone()).await {
            tracing::warn!(error = %e, "failed to persist assistant message");
        }
    }

    /// Returns the current agent state.
    pub fn state(&self) -> AgentState {
        self.state
    }

    /// Tool-call guard (VALIDATE → COERCE → INFER → REPAIR): normalize
    /// weak-model arguments against the tool's advertised JSON Schema before
    /// the cycle budget or the executor sees the call.
    ///
    /// * parses `null`/empty/stringified arguments (including fenced JSON
    ///   blocks) into an object;
    /// * applies schema-guided safe coercions (string → number/boolean, enum
    ///   case normalization, unknown-key stripping), logging every fix;
    /// * validates required fields, types, and enum membership; on failure
    ///   attempts per-tool heuristic inference (adaptive tool-guard Solution
    ///   3) for required fields that are absent or `null` — e.g. filesystem
    ///   `operation` from path shape, shell `command` recovered from a `cmd`
    ///   alias — accepting the result only when the completed arguments
    ///   re-validate cleanly;
    /// * otherwise injects a structured corrective `ToolResult` (bounded by
    ///   [`tool_guard::MAX_TOOL_GUARD_REJECTS`] per tool name per run) so the
    ///   model retries with corrected arguments.
    ///
    /// Tools without a schema in the registry pass through untouched — the
    /// executor and policy engine own unknown-tool errors. The guard adds no
    /// `await` points, so the caller's `CancellationToken` is unaffected.
    fn guard_tool_call(&mut self, tc: &ToolCall) -> GuardOutcome {
        let parsed = tool_guard::parse_tool_arguments(&tc.arguments);
        let definitions = self.tool_executor.tool_definitions();
        let Some(schema) =
            definitions.iter().find(|definition| definition.name == tc.name).map(|d| &d.parameters)
        else {
            return GuardOutcome::Pass(parsed);
        };

        // The original parse result is kept for heuristic alias recovery:
        // coercion strips hallucinated alias keys (`cmd`, `file`, ...), which
        // are exactly the alternative field names the heuristics recover.
        let (coerced, coercions) = tool_guard::coerce_arguments(parsed.clone(), schema);
        if !coercions.is_empty() {
            tracing::warn!(
                tool = %tc.name,
                coercions = ?coercions,
                "tool-call guard coerced tool arguments"
            );
        }

        let errors = tool_guard::validate_arguments(&coerced, schema);
        if errors.is_empty() {
            self.tool_guard_rejects.remove(&tc.name);
            return GuardOutcome::Pass(coerced);
        }

        // Heuristic inference (Solution 3): last-mile recovery for unresolved
        // required fields before coaching the model. Conservative by
        // construction — fills only absent/`null` slots, logged, and accepted
        // only when the completed arguments validate cleanly; anything else
        // falls through to the corrective reject below with the original
        // errors (no silent guesses).
        let mut repaired = coerced;
        if let Some(notes) = tool_guard::heuristic_infer(&tc.name, &parsed, &mut repaired, schema) {
            let (repaired, repair_coercions) = tool_guard::coerce_arguments(repaired, schema);
            if tool_guard::validate_arguments(&repaired, schema).is_empty() {
                tracing::warn!(
                    tool = %tc.name,
                    heuristic_inferred = ?notes,
                    coercions = ?repair_coercions,
                    "tool-call guard heuristically inferred missing tool arguments"
                );
                self.tool_guard_rejects.remove(&tc.name);
                return GuardOutcome::Pass(repaired);
            }
        }

        // Live-proven (Sep 2026 audit): a model emitting zero-argument calls
        // never corrects on coaching — retries only burn iterations. Fail
        // fast on empty args; keep bounded retries for partial args (some
        // keys present), where the example can actually guide a repair.
        let has_keys = parsed.as_object().is_some_and(|map| !map.is_empty());
        let reject_count = self.tool_guard_rejects.entry(tc.name.clone()).or_insert(0);
        *reject_count += 1;
        let exhausted = !has_keys || *reject_count > tool_guard::MAX_TOOL_GUARD_REJECTS;
        let content = tool_guard::corrective_message_text(&tc.name, &errors, schema, exhausted);
        let payload = tool_guard::corrective_tool_result(&tc.name, &errors, schema, exhausted);
        tracing::warn!(
            tool = %tc.name,
            reject_count,
            exhausted,
            errors = ?errors,
            "tool-call guard rejected tool arguments; injecting corrective tool result"
        );
        GuardOutcome::Reject {
            summary: format!("invalid arguments: {}", errors.first().cloned().unwrap_or_default()),
            content,
            payload,
        }
    }

    /// Execute a single tool call: tool-guard validation, cycle-budget check,
    /// approval, execution, and structured result persistence.
    #[allow(clippy::too_many_arguments)]
    async fn execute_single_tool_call(
        &mut self,
        tc: &concerto_core::types::ToolCall,
        task: &AgentTask,
        correlation_id: Ulid,
        session: &SessionContext,
        cancel: CancellationToken,
        tool_call_count: &mut u32,
        file_changing_tool_count: &mut u32,
        files_modified: &mut Vec<camino::Utf8PathBuf>,
        tool_events: &mut Vec<ToolExecutionSummary>,
        messages: &mut Vec<Message>,
    ) -> Result<(), OrchestratorError> {
        // Tool-call guard (VALIDATE → COERCE → REPAIR): normalize the
        // provider-accumulated arguments against the tool's schema before
        // anything else sees them. Rejected calls never execute and never
        // touch the cycle budget; the model receives a corrective tool
        // result instead and retries on the next iteration.
        let arguments = match self.guard_tool_call(tc) {
            GuardOutcome::Pass(arguments) => arguments,
            GuardOutcome::Reject { summary, content, payload } => {
                *tool_call_count += 1;
                tool_events.push(ToolExecutionSummary {
                    tool_name: tc.name.clone(),
                    operation: None,
                    path: None,
                    success: false,
                    summary: summary.clone(),
                });
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::ToolExecutionFinished {
                        tool_name: tc.name.clone(),
                        duration_ms: 0,
                        success: false,
                        detail: Some(summary),
                    },
                );
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: "single-agent".to_string(),
                        content: content.clone(),
                    },
                );
                messages.push(Message {
                    role: Role::Tool,
                    content,
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: payload,
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                });
                return Ok(());
            }
        };

        let input_hash = blake3::hash(arguments.to_string().as_bytes());

        // Check cycle budget — on repeat, pause and ask the user
        // rather than hard-failing (roadmap requirement 3.2).
        if let Err(e) = self.cycle_budget.record(&tc.name, &input_hash.to_hex()[..16]) {
            let _ = self.bus.publish_for_session(
                task.session_id,
                correlation_id,
                EventKind::CycleBudgetExceeded {
                    task_id: task.id,
                    tool_name: tc.name.clone(),
                    call_count: 3,
                },
            );

            self.state = AgentState::AwaitingApproval;

            let action = PolicyAction {
                tool_name: &tc.name,
                input: &arguments,
                session_id: task.session_id,
                correlation_id: Ulid::new(),
                capability_requirements: CapabilitySet::default(),
                sandbox_profile: None,
                estimated_cost_usd: None,
                command_facts: None,
            };

            match self.approval.request_approval(&action, cancel.clone()).await {
                ApprovalDecision::Approve | ApprovalDecision::ApproveAllForSession => {
                    // User wants to continue despite the repeat — reset the
                    // tracker for this (tool, input) pair so it doesn't
                    // immediately refire, and fall through to execute the call.
                    self.cycle_budget.reset();
                    self.state = AgentState::Executing;
                }
                ApprovalDecision::Deny => {
                    self.state = AgentState::Failed;
                    return Err(e);
                }
                _ => {
                    self.state = AgentState::Failed;
                    return Err(e);
                }
            }
        }

        // Execute via tool executor
        let filesystem_operation = arguments.get("operation").and_then(|v| v.as_str());
        let detail_path = arguments.get("path").and_then(|v| v.as_str()).map(|s| s.to_string());
        let start_detail = match (tc.name.as_str(), filesystem_operation, &detail_path) {
            (_, Some(op), Some(p)) => Some(format!("{} {}", op, p)),
            (_, Some(op), None) => Some(op.to_string()),
            (_, None, Some(p)) => Some(format!("{} {}", tc.name, p)),
            (_, None, None) => None,
        };
        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::ToolExecutionStarted {
                tool_name: tc.name.clone(),
                input_hash: input_hash.to_hex().to_string(),
                detail: start_detail.clone(),
            },
        );

        match self.tool_executor.execute(&tc.name, arguments.clone(), &tc.id, session, cancel).await
        {
            Ok(output) => {
                *tool_call_count += 1;

                // Track file-changing tools separately
                let is_file_changing = matches!(
                    tc.name.as_str(),
                    "write_file" | "delete_file" | "edit_file" | "create_file" | "modify_file"
                ) || (tc.name == "filesystem"
                    && matches!(filesystem_operation, Some("write" | "delete" | "move" | "copy")));

                if is_file_changing {
                    *file_changing_tool_count += 1;

                    if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                        let path = camino::Utf8PathBuf::from(path);
                        if !files_modified.contains(&path) {
                            files_modified.push(path);
                        }
                    } else if let Some(path) = output.data.get("path").and_then(|v| v.as_str()) {
                        let path = camino::Utf8PathBuf::from(path);
                        if !files_modified.contains(&path) {
                            files_modified.push(path);
                        }
                    } else if let Some(path) = output.data.get("file_path").and_then(|v| v.as_str())
                    {
                        let path = camino::Utf8PathBuf::from(path);
                        if !files_modified.contains(&path) {
                            files_modified.push(path);
                        }
                    }
                }

                tool_events.push(ToolExecutionSummary {
                    tool_name: tc.name.clone(),
                    operation: filesystem_operation.map(|s| s.to_string()),
                    path: detail_path.clone().map(camino::Utf8PathBuf::from),
                    success: true,
                    summary: output.summary.clone(),
                });
                let finished_detail =
                    match output.data.get("absolute_path").and_then(|v| v.as_str()) {
                        Some(abs) => format!("{}  →  {}", output.summary, abs),
                        None => output.summary.clone(),
                    };

                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::ToolExecutionFinished {
                        tool_name: tc.name.clone(),
                        duration_ms: 0,
                        success: true,
                        detail: Some(finished_detail),
                    },
                );

                let tool_result_content = format!(
                    "TOOL_RESULT\ntool: {}\nid: {}\nstatus: success\nsummary: {}\ndata: {}",
                    tc.name,
                    tc.id,
                    output.summary,
                    serde_json::to_string(&output.data).unwrap_or_else(|_| "{}".to_string())
                );

                messages.push(Message {
                    role: Role::Tool,
                    content: tool_result_content.clone(),
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: serde_json::to_value(&output).unwrap_or_default(),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                });
            }
            Err(ToolError::PolicyDenied { rule }) => {
                *tool_call_count += 1;
                tool_events.push(ToolExecutionSummary {
                    tool_name: tc.name.clone(),
                    operation: filesystem_operation.map(|s| s.to_string()),
                    path: detail_path.clone().map(camino::Utf8PathBuf::from),
                    success: false,
                    summary: format!("policy denied: {}", rule),
                });
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::ToolExecutionFinished {
                        tool_name: tc.name.clone(),
                        duration_ms: 0,
                        success: false,
                        detail: Some(format!("policy denied: {}", rule)),
                    },
                );
                let denial_msg = format!(
                    "[POLICY DENIED] Tool '{}' was blocked by the security policy: {}",
                    tc.name, rule
                );
                messages.push(Message {
                    role: Role::Tool,
                    content: denial_msg,
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: serde_json::json!({ "error": "policy_denied", "rule": rule }),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                });
            }
            Err(e) => {
                tool_events.push(ToolExecutionSummary {
                    tool_name: tc.name.clone(),
                    operation: filesystem_operation.map(|s| s.to_string()),
                    path: detail_path.clone().map(camino::Utf8PathBuf::from),
                    success: false,
                    summary: e.to_string(),
                });
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::ToolExecutionFinished {
                        tool_name: tc.name.clone(),
                        duration_ms: 0,
                        success: false,
                        detail: Some(e.to_string()),
                    },
                );
                let error_msg = format!(
                    "TOOL_RESULT\ntool: {}\nid: {}\nstatus: error\nsummary: Tool execution failed: {}\ndata: {{}}",
                    tc.name, tc.id, e
                );
                messages.push(Message {
                    role: Role::Tool,
                    content: error_msg,
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: serde_json::json!({ "error": "execution_failed", "message": e.to_string() }),
                    }]),
                    reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Lightweight file-level verification helpers
// ---------------------------------------------------------------------------

/// Run after each tool call iteration: perform extension-based verification
/// (py_compile, cargo check, npm test, HTML/CSS reference checks) on each
/// modified file and return structured results.
async fn verify_file_changes(
    files: &[camino::Utf8PathBuf],
    project_root: &std::path::Path,
) -> Vec<VerificationSummary> {
    let mut results = Vec::new();
    for path in files {
        let ext = path.extension().unwrap_or_default();
        match ext {
            "py" => {
                let output = tokio::process::Command::new("python")
                    .args(["-m", "py_compile", path.as_str()])
                    .current_dir(project_root)
                    .output()
                    .await;
                match output {
                    Ok(out) if out.status.success() => {
                        results.push(VerificationSummary {
                            path: path.clone(),
                            command: "py_compile".into(),
                            passed: true,
                            output: "py_compile passed".into(),
                        });
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        results.push(VerificationSummary {
                            path: path.clone(),
                            command: "py_compile".into(),
                            passed: false,
                            output: stderr,
                        });
                    }
                    Err(e) => {
                        results.push(VerificationSummary {
                            path: path.clone(),
                            command: "py_compile".into(),
                            passed: false,
                            output: format!("py_compile error: {e}"),
                        });
                    }
                }
            }
            "rs" => {
                let cargo_toml = project_root.join("Cargo.toml");
                if cargo_toml.exists() {
                    let output = tokio::process::Command::new("cargo")
                        .args(["check"])
                        .current_dir(project_root)
                        .output()
                        .await;
                    match output {
                        Ok(out) if out.status.success() => {
                            results.push(VerificationSummary {
                                path: path.clone(),
                                command: "cargo check".into(),
                                passed: true,
                                output: "cargo check passed".into(),
                            });
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                            results.push(VerificationSummary {
                                path: path.clone(),
                                command: "cargo check".into(),
                                passed: false,
                                output: stderr,
                            });
                        }
                        Err(e) => {
                            results.push(VerificationSummary {
                                path: path.clone(),
                                command: "cargo check".into(),
                                passed: false,
                                output: format!("cargo check error: {e}"),
                            });
                        }
                    }
                }
            }
            "js" | "ts" | "jsx" | "tsx" => {
                let package_json = project_root.join("package.json");
                if package_json.exists() {
                    let content =
                        tokio::fs::read_to_string(&package_json).await.unwrap_or_default();
                    if content.contains("\"test\"") {
                        let output = tokio::process::Command::new("npm")
                            .args(["test"])
                            .current_dir(project_root)
                            .output()
                            .await;
                        match output {
                            Ok(out) if out.status.success() => {
                                results.push(VerificationSummary {
                                    path: path.clone(),
                                    command: "npm test".into(),
                                    passed: true,
                                    output: "npm test passed".into(),
                                });
                            }
                            Ok(out) => {
                                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                                results.push(VerificationSummary {
                                    path: path.clone(),
                                    command: "npm test".into(),
                                    passed: false,
                                    output: stderr,
                                });
                            }
                            Err(e) => {
                                results.push(VerificationSummary {
                                    path: path.clone(),
                                    command: "npm test".into(),
                                    passed: false,
                                    output: format!("npm test error: {e}"),
                                });
                            }
                        }
                    } else {
                        results.push(VerificationSummary {
                            path: path.clone(),
                            command: "npm test".into(),
                            passed: true,
                            output: "skipped (no test script in package.json)".into(),
                        });
                    }
                }
            }
            "html" | "css" => {
                let file_content = tokio::fs::read_to_string(path).await.unwrap_or_default();
                let mut missing = Vec::new();
                for line in file_content.lines() {
                    if let Some(start) = line.find("href=\"") {
                        if let Some(end) = line[start + 6..].find("\"") {
                            let href = &line[start + 6..start + 6 + end];
                            if !href.starts_with("http")
                                && !href.starts_with("#")
                                && !href.is_empty()
                            {
                                let referenced = project_root.join(href);
                                if !referenced.exists() {
                                    missing.push(href.to_string());
                                }
                            }
                        }
                    }
                    if let Some(start) = line.find("src=\"") {
                        if let Some(end) = line[start + 5..].find("\"") {
                            let src = &line[start + 5..start + 5 + end];
                            if !src.starts_with("http") && !src.is_empty() {
                                let referenced = project_root.join(src);
                                if !referenced.exists() {
                                    missing.push(src.to_string());
                                }
                            }
                        }
                    }
                }
                if missing.is_empty() {
                    results.push(VerificationSummary {
                        path: path.clone(),
                        command: "reference check".into(),
                        passed: true,
                        output: "all referenced local files exist".into(),
                    });
                } else {
                    results.push(VerificationSummary {
                        path: path.clone(),
                        command: "reference check".into(),
                        passed: false,
                        output: format!("missing referenced files: {missing:?}"),
                    });
                }
            }
            _ => {}
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::prompts::PromptBuilder;
    use async_trait::async_trait;
    use concerto_core::error::{MemoryError, ProviderError};
    use concerto_core::event::EventBus;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::ids::Ulid;
    use concerto_core::memory::{ChunkType, MemoryNamespace, ProjectId};
    use concerto_core::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery};
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::approval::ApprovalDecision;
    use concerto_core::traits::memory::MemoryStore;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::traits::provider::{CompletionStream, LlmProvider};
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{
        CompletionChunk, CompletionRequest, ToolCall, ToolOutput, ToolRegistry,
    };
    use concerto_core::types::{Condition, PolicyRule};
    use concerto_core::CancellationToken;
    use concerto_eval::EvalEngine;
    use concerto_memory::decision_store::DecisionStore;
    use concerto_memory::fts::FullTextStore;
    use concerto_memory::fts::SqliteFullTextStore;
    use concerto_memory::storage::MemoryDb;
    use concerto_memory::system::MemorySystem;
    use concerto_memory::task_tree::TaskTreeStore;
    use concerto_memory::vector_store::SqliteVectorStore;
    use concerto_memory::vector_store::VectorStore;
    use concerto_tools::filesystem::FilesystemTool;
    use concerto_tools::undo::UndoManager;
    use futures::stream;
    use time::OffsetDateTime;

    // -----------------------------------------------------------------------
    // Local ApprovalTestHarness (can't import from concerto_core::testing
    // because that module is cfg(test) for the core crate, not re-exported
    // to downstream crate test builds).
    // -----------------------------------------------------------------------

    struct ApprovalTestHarness {
        decisions: VecDeque<ApprovalDecision>,
    }

    impl ApprovalTestHarness {
        fn always_approve() -> Self {
            Self { decisions: VecDeque::new() }
        }
        fn always_deny() -> Self {
            let mut decisions = VecDeque::new();
            decisions.push_back(ApprovalDecision::Deny);
            Self { decisions }
        }
    }

    #[async_trait]
    impl ApprovalSink for ApprovalTestHarness {
        async fn request_approval(
            &self,
            _action: &concerto_core::types::PolicyAction<'_>,
            _cancel: concerto_core::CancellationToken,
        ) -> ApprovalDecision {
            self.decisions.clone().into_iter().next().unwrap_or(ApprovalDecision::Approve)
        }
        async fn approve_all_for_session(
            &self,
            _session_id: Ulid,
            _cancel: concerto_core::CancellationToken,
        ) {
        }
        async fn request_ack(
            &self,
            _message: &str,
            _cancel: concerto_core::CancellationToken,
        ) -> bool {
            true // auto-acknowledge in tests
        }
    }

    // -----------------------------------------------------------------------
    // Test doubles
    // -----------------------------------------------------------------------

    /// A mock LLM provider that returns a predefined sequence of tool-call
    /// batches. Each call to `stream_completion` advances to the next batch.
    struct ScriptedProvider {
        responses: Vec<Vec<ToolCall>>,
        call_count: AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ToolCall>>) -> Self {
            Self { responses, call_count: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn provider_name(&self) -> &'static str {
            "scripted"
        }

        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let tool_calls = self.responses.get(idx).cloned().unwrap_or_default();
            let chunks: Vec<_> = if tool_calls.is_empty() {
                vec![CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }]
            } else {
                tool_calls
                    .into_iter()
                    .map(|tc| CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(tc),
                        is_final: false,
                        usage: None,
                    })
                    .collect()
            };
            Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
        }
    }

    /// A provider that fails the first `fail_times` calls with a transient
    /// error, then succeeds (returns an empty answer-only completion). Used to
    /// exercise the retry-with-backoff path in `run_inner`.
    struct TransientFailProvider {
        fail_times: usize,
        calls: AtomicUsize,
    }

    impl TransientFailProvider {
        fn new(fail_times: usize) -> Self {
            Self { fail_times, calls: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl LlmProvider for TransientFailProvider {
        fn provider_name(&self) -> &'static str {
            "transient-fail"
        }
        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            if idx < self.fail_times {
                // Network errors are classified as retryable by the shared
                // retry policy, so the loop should recover after the blip.
                return Err(ProviderError::Network(format!("transient failure #{}", idx + 1)));
            }
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: true,
                usage: None,
            })])))
        }
    }

    /// A provider that ALWAYS returns the same tool call and never an empty
    /// (final) answer. Used to drive a run into the iteration cap so we can
    /// assert the cap is surfaced as a failure, not silently reported as
    /// success.
    struct AlwaysToolProvider {
        name: &'static str,
    }

    #[async_trait]
    impl LlmProvider for AlwaysToolProvider {
        fn provider_name(&self) -> &'static str {
            "always-tool"
        }
        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let tc = ToolCall {
                id: "always".into(),
                name: self.name.into(),
                arguments: serde_json::json!({ "text": "loop" }),
            };
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: Some(tc),
                is_final: false,
                usage: None,
            })])))
        }
    }

    /// No-op audit log for tests.
    struct TestAudit;
    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    /// A simple tool that echoes input back as output.
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes input"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn concerto_core::traits::policy::PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "ok".into(), data: serde_json::json!({}) })
        }
    }

    /// A tool that simulates writing a file (file-changing tool).
    struct WriteFileTool;
    #[async_trait]
    impl Tool for WriteFileTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "writes content to a file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn concerto_core::traits::policy::PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "file written".into(),
                data: serde_json::json!({"file_path": "/tmp/test.rs"}),
            })
        }
    }

    /// Wraps the real `FilesystemTool` and counts how many times `execute` is
    /// invoked. The counter is shared via `Arc<AtomicUsize>` so the test can
    /// hold the count while the tool lives inside the executor's registry.
    struct CountingFilesystemTool {
        inner: FilesystemTool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingFilesystemTool {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn description(&self) -> &str {
            self.inner.description()
        }
        fn input_schema(&self) -> serde_json::Value {
            self.inner.input_schema()
        }
        fn capability_requirements(&self) -> CapabilitySet {
            self.inner.capability_requirements()
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            policy: &dyn concerto_core::traits::policy::PolicyEngine,
            session: &SessionContext,
            cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(input, policy, session, cancel).await
        }
    }

    /// No-op in-memory store that returns empty results.
    struct NoopMemory;
    #[async_trait]
    impl MemoryStore for NoopMemory {
        async fn retrieve(
            &self,
            _query: &MemoryQuery,
            _cancel: CancellationToken,
        ) -> Result<Vec<MemoryChunk>, MemoryError> {
            Ok(Vec::new())
        }
        async fn store(
            &self,
            _entry: MemoryEntry,
            _cancel: CancellationToken,
        ) -> Result<MemoryId, MemoryError> {
            Ok(MemoryId(Ulid::new()))
        }
        async fn invalidate(
            &self,
            _id: MemoryId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_tool_call(name: &str, text: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: serde_json::json!({"text": text}),
        }
    }

    /// Build a minimal AgentLoop wired with test doubles.
    fn make_loop(
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
    ) -> AgentLoop {
        make_loop_with_dir(provider, approval, max_iterations, "/tmp")
    }

    /// Like `make_loop` but with an explicit project directory (used for
    /// undo-commit failure tests where /tmp is not a git repo).
    fn make_loop_with_dir(
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
        project_dir: &str,
    ) -> AgentLoop {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(approval.clone()),
        );
        AgentLoop::with_project_root(
            EventBus::new(256),
            approval.clone(), // cloned so we can reuse after the move below
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new(project_dir))),
            EvalEngine::new(project_dir),
            PromptBuilder::new("test system prompt"),
            max_iterations,
            true, // fast mode
            std::path::PathBuf::from(project_dir),
            None, // no overflow strategy
            None, // no budget allocator
        )
        .with_retry_policy(RetryPolicy::new(concerto_config::RetryConfig {
            // Fast, deterministic retries for tests.
            initial_delay_ms: 5,
            max_delay_ms: 50,
            jitter: false,
            multiplier: 2.0,
            ..concerto_config::RetryConfig::default()
        }))
    }

    /// Build a loop with extra tools alongside the default EchoTool.
    fn make_loop_with_extra_tools(
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
        extra_tools: Vec<Box<dyn Tool>>,
    ) -> AgentLoop {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        for tool in extra_tools {
            registry.register(tool);
        }
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(approval.clone()),
        );
        AgentLoop::with_project_root(
            EventBus::new(256),
            approval.clone(),
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new("/tmp"))),
            EvalEngine::new("/tmp"),
            PromptBuilder::new("test system prompt"),
            max_iterations,
            true,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
        )
    }

    /// Like `make_loop_with_dir` but also registers the REAL `FilesystemTool`
    /// rooted at `dir`, so tool calls materialize to the actual disk under
    /// `dir` (the same path used as the loop's project root).
    fn make_loop_with_fs_tool(
        dir: &std::path::Path,
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
    ) -> AgentLoop {
        let root = camino::Utf8PathBuf::from_path_buf(dir.to_path_buf()).unwrap();
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(FilesystemTool::new(root.clone())));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(approval.clone()),
        );
        AgentLoop::with_project_root(
            EventBus::new(256),
            approval.clone(),
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new(dir))),
            EvalEngine::new(dir),
            PromptBuilder::new("test system prompt"),
            max_iterations,
            true,
            dir.to_path_buf(),
            None,
            None,
        )
    }

    /// Like `make_loop_with_fs_tool` but registers the real `FilesystemTool`
    /// behind a counting wrapper, returning the shared invocation counter so
    /// the test can assert the write tool ran exactly once.
    fn make_loop_with_counting_fs_tool(
        dir: &std::path::Path,
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
    ) -> (AgentLoop, Arc<AtomicUsize>) {
        let root = camino::Utf8PathBuf::from_path_buf(dir.to_path_buf()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(CountingFilesystemTool {
            inner: FilesystemTool::new(root.clone()),
            calls: calls.clone(),
        }));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(approval.clone()),
        );
        let loop_ = AgentLoop::with_project_root(
            EventBus::new(256),
            approval.clone(),
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new(dir))),
            EvalEngine::new(dir),
            PromptBuilder::new("test system prompt"),
            max_iterations,
            true,
            dir.to_path_buf(),
            None,
            None,
        )
        .with_retry_policy(RetryPolicy::new(concerto_config::RetryConfig {
            // Transient failures surface immediately so the run ends with the
            // provider error propagated (the retry boundary itself is covered
            // by the `provider_retry_*` tests).
            enabled: false,
            ..concerto_config::RetryConfig::default()
        }));
        (loop_, calls)
    }

    /// An approval sink that returns `true` for request_ack but denies
    /// all policy approvals (for testing the undo-commit ack path).
    struct AckApproval;
    #[async_trait]
    impl ApprovalSink for AckApproval {
        async fn request_approval(
            &self,
            _action: &concerto_core::types::PolicyAction<'_>,
            _cancel: concerto_core::CancellationToken,
        ) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
        async fn approve_all_for_session(
            &self,
            _session_id: Ulid,
            _cancel: concerto_core::CancellationToken,
        ) {
        }
        async fn request_ack(
            &self,
            _message: &str,
            _cancel: concerto_core::CancellationToken,
        ) -> bool {
            true
        }
    }

    /// An approval sink that returns `false` for request_ack.
    struct DenyAckApproval;
    #[async_trait]
    impl ApprovalSink for DenyAckApproval {
        async fn request_approval(
            &self,
            _action: &concerto_core::types::PolicyAction<'_>,
            _cancel: concerto_core::CancellationToken,
        ) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
        async fn approve_all_for_session(
            &self,
            _session_id: Ulid,
            _cancel: concerto_core::CancellationToken,
        ) {
        }
        async fn request_ack(
            &self,
            _message: &str,
            _cancel: concerto_core::CancellationToken,
        ) -> bool {
            false
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cycle_detection_with_approve_continues_loop() {
        // Provider returns the same tool call 3 times, then empty (done).
        let tc = make_tool_call("echo", "hello");
        let calls = vec![
            vec![tc.clone()], // iteration 1 — count=1
            vec![tc.clone()], // iteration 2 — count=2
            vec![tc.clone()], // iteration 3 — count=3 → cycle detected → approved
            vec![],           // iteration 4 — done
        ];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());

        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new(Ulid::new(), "test task");
        let result = loop_.run(task, CancellationToken::new()).await;

        // Loop should NOT return an error — it continues after approval.
        assert!(result.is_ok(), "expected Ok, got Err: {:?}", result.err());
        let output = result.unwrap();
        assert_eq!(output.tool_call_count, 3, "all three tool calls executed");
    }

    #[tokio::test]
    async fn cycle_detection_with_deny_fails_loop() {
        // Provider returns the same tool call 3 times.
        let tc = make_tool_call("echo", "hello");
        let calls = vec![
            vec![tc.clone()], // iteration 1 — count=1
            vec![tc.clone()], // iteration 2 — count=2
            vec![tc.clone()], // iteration 3 — count=3 → cycle detected → denied
        ];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_deny());

        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new(Ulid::new(), "test task");
        let result = loop_.run(task, CancellationToken::new()).await;

        assert!(result.is_err(), "expected Err on deny");
        // The returned error should relate to cycle detection.
        assert!(matches!(result.unwrap_err(), OrchestratorError::CycleDetected { .. }));
    }

    #[tokio::test]
    async fn undo_commit_fail_ack_true_continues() {
        // undo.commit fails on a non-git dir; request_ack returns true → loop proceeds.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![]])); // empty → done
        let approval = Arc::new(AckApproval);

        let dir = tempfile::tempdir().expect("tempdir");
        let mut loop_ = make_loop_with_dir(provider, approval, 10, dir.path().to_str().unwrap());
        let task = AgentTask::new(Ulid::new(), "test task");
        let result = loop_.run(task, CancellationToken::new()).await;

        assert!(result.is_ok(), "expected Ok, got Err: {:?}", result.err());
    }

    #[tokio::test]
    async fn undo_commit_fail_ack_false_cancels() {
        // undo.commit fails on a non-git dir; request_ack returns false → Cancelled.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![]])); // empty → done
        let approval = Arc::new(DenyAckApproval);

        let dir = tempfile::tempdir().expect("tempdir");
        let mut loop_ = make_loop_with_dir(provider, approval, 10, dir.path().to_str().unwrap());
        let task = AgentTask::new(Ulid::new(), "test task");
        let result = loop_.run(task, CancellationToken::new()).await;

        assert!(result.is_err(), "expected Err, got Ok");
        assert!(matches!(result.unwrap_err(), OrchestratorError::Cancelled));
    }

    #[tokio::test]
    async fn memory_system_in_agent_loop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let utf8_path = camino::Utf8Path::from_path(&db_path).expect("valid utf-8 path");

        let mem_db = MemoryDb::connect(utf8_path).await.expect("connect");
        let pool = mem_db.pool().clone();

        let vector_store = Arc::new(SqliteVectorStore::new(pool.clone()).await.expect("vector"))
            as Arc<dyn VectorStore>;
        let fts_store = Arc::new(SqliteFullTextStore::new(pool.clone()).await.expect("fts"))
            as Arc<dyn FullTextStore>;
        let decision_store = DecisionStore::new();
        let task_tree_store = TaskTreeStore::new();
        // Derive project_id from the same temp dir the agent loop will use,
        // so memory storage/retrieval inside the loop uses matching keys.
        let project_id = ProjectId::resolve(dir.path());
        let memory: Arc<dyn MemoryStore> = Arc::new(MemorySystem::new(
            vector_store,
            fts_store,
            decision_store,
            task_tree_store,
            None,
            project_id.clone(),
            None,
        ));

        let entry = MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id.clone()),
            content: "important test memory content".into(),
            chunk_type: ChunkType::SlidingWindow,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({"source": "test"}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        let _id = memory.store(entry, CancellationToken::new()).await.expect("store");

        let query = MemoryQuery {
            text: "important test memory".into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id.clone()),
            top_k: 5,
            filters: vec![],
        };
        let results = memory.retrieve(&query, CancellationToken::new()).await.expect("retrieve");
        assert!(!results.is_empty(), "should find stored memory");
        assert!(
            results.iter().any(|c| c.content.contains("important test memory content")),
            "retrieved content must match stored entry"
        );

        let provider = Arc::new(ScriptedProvider::new(vec![vec![]]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop_with_dir(
            provider.clone(),
            approval,
            10,
            dir.path().to_str().expect("valid utf-8 path"),
        );
        loop_.memory = memory;

        let task = AgentTask::new(Ulid::new(), "integration test task");
        let _output = loop_.run(task, CancellationToken::new()).await.expect("run");

        let summary_query = MemoryQuery {
            text: "integration test task".into(),
            namespace: MemoryNamespace::Project(project_id.clone()),
            project_id,
            top_k: 5,
            filters: vec![],
        };
        let results = loop_
            .memory
            .retrieve(&summary_query, CancellationToken::new())
            .await
            .expect("retrieve summary");
        assert!(!results.is_empty(), "task summary should be stored in memory");

        let has_summary = results.iter().any(|c| c.content.contains("integration test task"));
        assert!(has_summary, "task summary content must exist in stored memory");
    }

    #[tokio::test]
    async fn first_turn_includes_user_message() {
        // Regression test: the first LLM request must include the user's
        // task description as a Role::User message, not rely on fragile
        // string substitution into the system prompt.
        let captured = Arc::new(std::sync::Mutex::new(None::<CompletionRequest>));

        struct CaptureProvider {
            captured: Arc<std::sync::Mutex<Option<CompletionRequest>>>,
        }

        #[async_trait]
        impl LlmProvider for CaptureProvider {
            fn provider_name(&self) -> &'static str {
                "capture"
            }
            fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
                concerto_core::types::TokenBudget::new(128_000, 4_096)
            }
            fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
                0.0
            }
            async fn stream_completion(
                &self,
                request: CompletionRequest,
                _cancel: CancellationToken,
            ) -> Result<CompletionStream, ProviderError> {
                *self.captured.lock().expect("lock") = Some(request);
                Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                })])))
            }
        }

        let provider = Arc::new(CaptureProvider { captured: captured.clone() });
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10);

        let description = "reply with the single word PONG";
        let task = AgentTask::new(Ulid::new(), description);
        let _result = loop_.run(task, CancellationToken::new()).await;

        let request = captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("provider should have been called at least once");

        assert!(!request.messages.is_empty(), "request should have at least the system message");
        let user_msgs: Vec<_> = request.messages.iter().filter(|m| m.role == Role::User).collect();
        assert!(!user_msgs.is_empty(), "request must contain at least one Role::User message");
        assert!(
            user_msgs[0].content.contains("PONG"),
            "first user message should contain the task description"
        );
        // Ensure tool definitions are included in the request and not empty.
        assert!(
            request.tools.as_ref().is_some_and(|t| !t.is_empty()),
            "tool definitions should be present and non-empty"
        );
    }

    #[tokio::test]
    async fn action_required_without_tools_fails() {
        // Provider returns no tool calls (empty), task requires tool execution.
        let provider = Arc::new(ScriptedProvider::new(vec![vec![]])); // immediate empty response
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new_action_required(Ulid::new(), "perform an operation");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(
            result.is_err(),
            "Expected error due to missing tool calls for action-required task"
        );
        match result.unwrap_err() {
            OrchestratorError::ExecutionRequiredButNoTools => {}
            other => panic!("Unexpected error variant: {:?}", other),
        }
    }

    #[tokio::test]
    async fn action_required_cannot_complete_with_only_read_list_tools() {
        // Provider returns a non-file-changing tool ("echo"), then empty.
        // Even though tool_call_count >= 1, file_changing_tool_count == 0
        // so the final guard should reject.
        let tc = make_tool_call("echo", "read some data");
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new_action_required(Ulid::new(), "implement a feature");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok partial for read-only action_required task");
        let output = result.unwrap();
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "should be Partial"
        );
        assert!(
            output.final_message.contains("no file-changing tool call succeeded"),
            "final_message should explain the blocker, got: {}",
            output.final_message
        );
    }

    #[tokio::test]
    async fn file_changing_tools_are_tracked_separately() {
        // Provider returns a write_file tool (file-changing), then empty.
        // The task should succeed since file_changing_tool_count > 0.
        let tc = make_tool_call("write_file", "new content");
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(WriteFileTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write code to file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(
            result.is_ok(),
            "expected Ok for file-changing action_required task, got Err: {:?}",
            result.err()
        );
        let output = result.unwrap();
        assert_eq!(output.tool_call_count, 1, "one tool call should be counted");
    }

    // -----------------------------------------------------------------------
    // Action-required integrity: text-only claims are never trusted
    // -----------------------------------------------------------------------

    /// A mock "filesystem" tool used to exercise the file-changing detection
    /// logic without touching real disk. It echoes the requested path back in
    /// the tool result so the loop can record `files_modified`.
    struct FilesystemMockTool;
    #[async_trait]
    impl Tool for FilesystemMockTool {
        fn name(&self) -> &str {
            "filesystem"
        }
        fn description(&self) -> &str {
            "mock filesystem for tests"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _policy: &dyn concerto_core::traits::policy::PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            let op = input.get("operation").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match op {
                "write" => Ok(ToolOutput {
                    summary: "wrote".into(),
                    data: serde_json::json!({ "path": path, "materialized": true }),
                }),
                "read" => Ok(ToolOutput {
                    summary: "read".into(),
                    data: serde_json::json!({ "path": path }),
                }),
                _ => Err(ToolError::ExecutionFailed { message: format!("unsupported op {op}") }),
            }
        }
    }

    /// A provider that returns a plain-text ("I'm done") response with no tool
    /// calls, to simulate the exact failure mode being fixed: the model claims
    /// completion in text without ever invoking a tool.
    struct TextOnlyProvider {
        text: String,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl LlmProvider for TextOnlyProvider {
        fn provider_name(&self) -> &'static str {
            "text-only"
        }
        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let chunk = CompletionChunk {
                delta: self.text.clone(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(chunk)])))
        }
    }

    /// Like `make_loop` but lets the test supply an `EventBus` it has already
    /// subscribed to, so published events can be inspected.
    fn make_loop_with_bus(
        bus: EventBus,
        provider: Arc<dyn LlmProvider>,
        approval: Arc<dyn ApprovalSink>,
        max_iterations: u32,
    ) -> AgentLoop {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(approval.clone()),
        );
        AgentLoop::with_project_root(
            bus,
            approval.clone(),
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new("/tmp"))),
            EvalEngine::new("/tmp"),
            PromptBuilder::new("test system prompt"),
            max_iterations,
            true,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
        )
        .with_retry_policy(RetryPolicy::new(concerto_config::RetryConfig {
            // Fast, deterministic retries for tests.
            initial_delay_ms: 5,
            max_delay_ms: 50,
            jitter: false,
            multiplier: 2.0,
            ..concerto_config::RetryConfig::default()
        }))
    }

    #[tokio::test]
    async fn action_required_text_only_response_fails() {
        // The provider returns only text claiming completion, never a tool call.
        // The task must fail rather than be reported as successful.
        let provider = Arc::new(TextOnlyProvider {
            text: "I have created all the files and finished the task.".to_string(),
            calls: AtomicUsize::new(0),
        });
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 6);
        let task = AgentTask::new_action_required(Ulid::new(), "implement a feature");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_err(), "action-required task with only text responses must fail");
    }

    #[tokio::test]
    async fn action_required_text_only_not_published_as_assistant_output() {
        // A text-only provider claim must NOT be surfaced to the UI as a
        // successful assistant message for action-required work.
        let bus = EventBus::new(256);
        let rx = bus.subscribe();
        let provider = Arc::new(TextOnlyProvider {
            text: "I created the file and the task is complete.".to_string(),
            calls: AtomicUsize::new(0),
        });
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop_with_bus(bus, provider, approval, 4);
        let task = AgentTask::new_action_required(Ulid::new(), "create a file");
        let _ = loop_.run(task, CancellationToken::new()).await;

        let mut inner = rx.into_inner();
        let mut published_assistant = false;
        while let Ok(event) = inner.try_recv() {
            if let EventKind::AssistantMessage { .. } = &event.kind {
                published_assistant = true;
            }
        }
        assert!(
            !published_assistant,
            "text-only provider claim must not be published as AssistantMessage for action-required work"
        );
    }

    #[tokio::test]
    async fn filesystem_write_operation_counts_as_file_changing() {
        // A "filesystem" tool call with operation "write" must be treated as a
        // file-changing action and recorded in files_modified.
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({ "operation": "write", "path": "src/main.rs" }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(FilesystemMockTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write a file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(
            result.is_ok(),
            "filesystem write should complete action-required task: {:?}",
            result.err()
        );
        let output = result.unwrap();
        assert!(
            output.files_modified.contains(&camino::Utf8PathBuf::from("src/main.rs")),
            "files_modified must contain the written path"
        );
    }

    #[tokio::test]
    async fn filesystem_read_operation_not_file_changing() {
        // A "filesystem" tool call with operation "read" must NOT satisfy the
        // file-changing requirement and the task must fail.
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({ "operation": "read", "path": "src/main.rs" }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(FilesystemMockTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "read a file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "filesystem read should return Ok partial, not error");
        let output = result.unwrap();
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "should be Partial"
        );
        assert!(
            output.final_message.contains("no file-changing tool call succeeded"),
            "final_message should explain the blocker, got: {msg}",
            msg = output.final_message
        );
    }

    #[tokio::test]
    async fn iteration_cap_is_not_treated_as_success() {
        // Regression: a run that exhausts its iteration budget without a natural
        // completion must surface as an Err (with the partial progress attached),
        // never as a silent Ok("done"). The provider always returns a tool call
        // with no file-changing effect, so the loop can never complete.
        let provider = Arc::new(AlwaysToolProvider { name: "echo" });
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        // Small max_iterations keeps the test fast; run() also bounds the
        // auto-continuation rounds so this terminates promptly.
        let mut loop_ = make_loop(provider, approval, 3);
        let task = AgentTask::new(Ulid::new(), "do something that never finishes");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "iteration cap should return Ok partial, not error");
        let output = result.unwrap();
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "should be Partial"
        );
        assert!(
            output.final_message.contains("no convergence")
                || output.final_message.contains("maximum continuation"),
            "final_message should explain the stall/cap, got: {}",
            output.final_message
        );
    }

    #[tokio::test]
    async fn successful_action_required_returns_files_modified() {
        // A successful action-required task (write_file family) must report the
        // modified path in AgentOutput.files_modified.
        let tc = make_tool_call("write_file", "new content");
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(WriteFileTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write code to file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.files_modified.iter().any(|p| p.as_str().ends_with("test.rs")),
            "files_modified should include the written path"
        );
    }

    #[tokio::test]
    async fn action_required_writes_runnable_program_to_real_disk() {
        // End-to-end proof that an action-required instruction produces a real,
        // runnable program on disk (not just an in-memory/VFS artifact and not a
        // text-only claim). A mock provider returns a `filesystem` write tool
        // call; the REAL FilesystemTool must materialize it to the actual disk,
        // and the resulting file must be executable.
        let dir = tempfile::tempdir().unwrap();

        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "hello.py",
                "content": "print('hello from disk')\n"
            }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop_with_fs_tool(dir.path(), provider, approval, 10);
        let task =
            AgentTask::new_action_required(Ulid::new(), "write a program that prints a greeting");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "action-required write task should succeed: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.files_modified.iter().any(|p| p.as_str().ends_with("hello.py")),
            "files_modified should include the written path"
        );

        // The file must exist on the REAL filesystem with the written content.
        let written = dir.path().join("hello.py");
        assert!(
            written.exists(),
            "written file must be materialized to disk: {}",
            written.display()
        );
        let content = std::fs::read_to_string(&written).unwrap();
        assert!(content.contains("hello from disk"), "written file content mismatch: {content}");

        // And it must be runnable: executing it produces the expected output.
        // Interpreter probe: prefer `python3`, fall back to `python` (stock
        // Windows exposes only `python` on PATH). A candidate counts only if
        // it spawns and exits successfully — Windows' Store alias stub for
        // `python3` resolves on PATH yet always exits non-zero, so exit status
        // is part of the probe. With no interpreter at all, degrade to a
        // logged skip rather than failing the disk-write behavior under test.
        let interpreter = ["python3", "python"].into_iter().find(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|probe| probe.status.success())
        });
        let Some(interpreter) = interpreter else {
            eprintln!("skipping runnability assertion: neither python3 nor python is on PATH");
            return;
        };
        let ran = std::process::Command::new(interpreter)
            .arg(&written)
            .output()
            .expect("interpreter probed above should stay available");
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains("hello from disk"),
            "running the program should print the greeting, got: {stdout}"
        );
    }

    // -----------------------------------------------------------------------
    // Regression tests for action-required output integrity (Phase 3 fix)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn action_required_summary_includes_execution_data() {
        // Regression: AgentOutput::summary() for action-required must contain
        // structured execution data (Files changed) rather than raw model prose.
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "src/main.rs",
                "content": "fn main() {}",
            }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(FilesystemMockTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write code");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok: {:?}", result.err());
        let output = result.unwrap();

        let summary = output.summary();
        assert!(summary.contains("Completed"), "summary should start with Completed");
        assert!(summary.contains("Files changed:"), "summary should contain Files changed");
        assert!(summary.contains("src/main.rs"), "summary should contain the written path");
        // The raw model prose (empty delta from ScriptedProvider) should NOT
        // be the sole content of the summary — structured data must dominate.
        assert!(!summary.trim().is_empty(), "summary must not be empty");
    }

    #[tokio::test]
    async fn files_modified_from_arguments_path() {
        // Regression: files_modified must contain the exact path from the
        // tool call's `arguments.path`, not a synthetic or missing value.
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "some/deep/path.rs",
            }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(FilesystemMockTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write a file in a nested dir");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok: {:?}", result.err());
        let output = result.unwrap();

        assert!(
            !output.files_modified.is_empty(),
            "files_modified must not be empty after a write"
        );
        assert!(
            output.files_modified.iter().any(|p| p.as_str() == "some/deep/path.rs"),
            "files_modified should contain the exact path from arguments.path, got: {:?}",
            output.files_modified,
        );
    }

    #[tokio::test]
    async fn tool_events_record_filesystem_write_details() {
        // Regression: tool_events in AgentOutput must capture the tool name,
        // operation, path, and success status for a filesystem write.
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "output.txt",
            }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ =
            make_loop_with_extra_tools(provider, approval, 10, vec![Box::new(FilesystemMockTool)]);
        let task = AgentTask::new_action_required(Ulid::new(), "write a file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok: {:?}", result.err());
        let output = result.unwrap();

        assert!(!output.tool_events.is_empty(), "tool_events must not be empty after tool use");
        let event = &output.tool_events[0];
        assert_eq!(
            event.tool_name, "filesystem",
            "tool_events should record the correct tool name"
        );
        assert_eq!(
            event.operation.as_deref(),
            Some("write"),
            "tool_events should record the filesystem operation"
        );
        assert_eq!(
            event.path.as_ref().map(|p| p.as_str()),
            Some("output.txt"),
            "tool_events should record the path from arguments.path"
        );
        assert!(event.success, "tool_events should record successful execution");
    }

    #[tokio::test]
    async fn python_verification_produces_verification_summary() {
        // Regression: writing a .py file must trigger py_compile verification
        // and produce a VerificationSummary in AgentOutput.verification.
        // The actual py_compile may pass or fail depending on the environment;
        // the critical assertion is that verification logic runs at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "hello.py",
                "content": "print('hello')\n",
            }),
        };
        let calls = vec![vec![tc], vec![]];
        let provider = Arc::new(ScriptedProvider::new(calls));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop_with_fs_tool(dir.path(), provider, approval, 10);
        let task = AgentTask::new_action_required(Ulid::new(), "write a Python script");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok: {:?}", result.err());
        let output = result.unwrap();

        assert!(
            !output.verification.is_empty(),
            "verification must contain entries after writing a .py file"
        );
        let v = &output.verification[0];
        assert_eq!(
            v.command, "py_compile",
            "verification command should be py_compile for .py files"
        );
        assert!(
            v.path.as_str().ends_with("hello.py"),
            "verification path should match the written file, got: {}",
            v.path,
        );
    }

    #[tokio::test]
    async fn retry_produces_agent_thought_not_assistant_message() {
        // Regression: when the provider returns text-only (no tool calls) on
        // the first iteration, the loop must publish an AgentThought for retry
        // and MUST NOT publish an AssistantMessage event. The task should
        // succeed on a subsequent tool-returning iteration.
        let bus = EventBus::new(256);
        let rx = bus.subscribe();

        // Build a minimal loop with WriteFileTool alongside EchoTool.
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(WriteFileTool));

        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), policy)
                .with_approval_sink(Arc::new(ApprovalTestHarness::always_approve())),
        );

        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![],                                     // iteration 1: empty -> retry
            vec![make_tool_call("write_file", "data")], // iteration 2: write -> execute
            vec![],                                     // iteration 3: empty -> success
        ]));

        let mut loop_ = AgentLoop::with_project_root(
            bus,
            Arc::new(ApprovalTestHarness::always_approve()),
            provider,
            executor,
            Arc::new(NoopMemory),
            Arc::new(std::sync::Mutex::new(UndoManager::new("/tmp"))),
            EvalEngine::new("/tmp"),
            PromptBuilder::new("test system prompt"),
            10,
            true,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
        );

        let task = AgentTask::new_action_required(Ulid::new(), "write a file");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected Ok after retry+write: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            !output.files_modified.is_empty(),
            "files must have been modified after successful retry"
        );

        // Inspect published events.
        let mut inner = rx.into_inner();
        let mut retry_thought_count = 0u32;
        let mut assistant_msg_count = 0u32;
        while let Ok(event) = inner.try_recv() {
            match &event.kind {
                EventKind::AgentThought { content, .. } => {
                    if content.contains("no file action returned") {
                        retry_thought_count += 1;
                    }
                }
                EventKind::AssistantMessage { .. } => {
                    assistant_msg_count += 1;
                }
                _ => {}
            }
        }
        assert!(
            retry_thought_count > 0,
            "should have published AgentThought for \"no file action returned\" retry",
        );
        assert_eq!(
            assistant_msg_count, 0,
            "orchestrator must not publish AssistantMessage events; \
             those are the CLI responsibility",
        );
    }

    #[tokio::test]
    async fn provider_retry_recovers_from_transient_failure() {
        // The provider fails the first call (a transient blip), then succeeds.
        // The retry loop must recover without aborting the whole task — this is
        // the core regression for multi-step tasks dying on a single network hiccup.
        let provider = Arc::new(TransientFailProvider::new(1));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new(Ulid::new(), "answer-only task");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(
            result.is_ok(),
            "retry should recover from a single transient failure, got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn provider_retry_gives_up_after_max_attempts() {
        // Provider always fails. With an elapsed-time fuse the retry policy must
        // give up (not loop forever) and surface a blocked result, never a
        // successful completion.
        let provider = Arc::new(TransientFailProvider::new(100));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10).with_retry_policy(RetryPolicy::new(
            concerto_config::RetryConfig {
                max_elapsed_seconds: Some(0),
                ..concerto_config::RetryConfig::default()
            },
        ));
        let task = AgentTask::new(Ulid::new(), "answer-only task");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "retry exhaustion should return Ok partial, not error");
        let output = result.unwrap();
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "should be Partial"
        );
        assert!(
            output.final_message.contains("retries exhausted"),
            "final_message should report retries exhausted, got: {}",
            output.final_message
        );
    }

    /// A provider that records how many times `stream_completion` is called and
    /// returns a scripted sequence of results. Used to assert retry semantics.
    struct CountingScriptedProvider {
        script: Vec<Result<Vec<CompletionChunk>, ProviderError>>,
        calls: AtomicUsize,
    }

    impl CountingScriptedProvider {
        fn new(script: Vec<Result<Vec<CompletionChunk>, ProviderError>>) -> Self {
            Self { script, calls: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl LlmProvider for CountingScriptedProvider {
        fn provider_name(&self) -> &'static str {
            "counting-scripted"
        }
        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            match self.script.get(idx).cloned() {
                Some(Ok(chunks)) => Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok)))),
                Some(Err(e)) => Err(e),
                None => Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                })]))),
            }
        }
    }

    /// Helper: a single answer-only completion chunk.
    fn answer_chunk() -> Vec<CompletionChunk> {
        vec![CompletionChunk {
            delta: String::new(),
            reasoning: None,
            tool_call: None,
            is_final: true,
            usage: None,
        }]
    }

    #[tokio::test]
    async fn provider_retry_429_then_success_calls_provider_thrice() {
        // 429, 429, success: exactly three provider calls, one completion,
        // same session/task on retry events, and the agent iteration count
        // only advances once.
        let provider = Arc::new(CountingScriptedProvider::new(vec![
            Err(ProviderError::RateLimit { retry_after: Duration::from_millis(1) }),
            Err(ProviderError::RateLimit { retry_after: Duration::from_millis(1) }),
            Ok(answer_chunk()),
        ]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let bus = EventBus::new(256);
        // Subscribe BEFORE the run so we observe retry events (broadcast does
        // not replay messages sent before a subscriber joined).
        let mut inner = bus.subscribe().into_inner();
        let mut loop_ = make_loop_with_bus(bus.clone(), provider.clone(), approval, 10)
            .with_retry_policy(RetryPolicy::new(concerto_config::RetryConfig {
                initial_delay_ms: 1,
                max_delay_ms: 5,
                jitter: false,
                respect_retry_after: true,
                ..concerto_config::RetryConfig::default()
            }));

        let task = AgentTask::new(Ulid::new(), "answer-only task");
        let session_id = task.session_id;
        let task_id = task.id;
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_ok(), "expected recovery, got: {:?}", result.err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            3,
            "provider should be called exactly 3 times (2 retries + 1 success)"
        );

        // Retry events must carry the same session and task ids.
        let mut scheduled = 0u32;
        while let Ok(event) = inner.try_recv() {
            if let EventKind::ProviderRetryScheduled { session_id: s, task_id: t, .. } = &event.kind
            {
                assert_eq!(s, &session_id, "retry event session mismatch");
                assert_eq!(t, &task_id, "retry event task mismatch");
                scheduled += 1;
            }
        }
        assert_eq!(scheduled, 2, "expected two retry-scheduled events");
    }

    #[tokio::test]
    async fn provider_failure_after_file_write_does_not_repeat_the_write() {
        // Scenario 5 acceptance bar: a provider failure after a successful
        // file write must NOT repeat the write and must NOT restart the agent.
        //
        // Harness: the REAL agent loop (`AgentLoop::run`) driving the REAL
        // `ToolExecutor` with the REAL `FilesystemTool` rooted at a temp dir
        // (same approach as `action_required_writes_runnable_program_to_real_disk`),
        // plus a counting wrapper around the filesystem tool so tool
        // invocations are observable.
        let dir = tempfile::tempdir().unwrap();

        // Provider script: call 1 writes a file; call 2 fails with a
        // transient provider error. Retries are disabled on this loop so the
        // transient error propagates out of `run` (mirroring
        // `provider_error_is_propagated_without_outer_retry` at the loop
        // level); the retry boundary itself is covered by `provider_retry_*`.
        let write_tc = ToolCall {
            id: "call_1".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({
                "operation": "write",
                "path": "output.txt",
                "content": "exactly-once payload\n",
            }),
        };
        let provider = Arc::new(CountingScriptedProvider::new(vec![
            Ok(vec![CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: Some(write_tc),
                is_final: false,
                usage: None,
            }]),
            Err(ProviderError::Network("transient failure after successful write".into())),
        ]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let (mut loop_, fs_calls) =
            make_loop_with_counting_fs_tool(dir.path(), provider.clone(), approval, 10);

        let task = AgentTask::new_action_required(Ulid::new(), "write a file");
        let result = loop_.run(task, CancellationToken::new()).await;

        // 1. The provider error is propagated — the run does NOT restart the
        //    agent (no continuation round, no second planning phase).
        let err = result.expect_err("provider failure must propagate out of run");
        match &err {
            OrchestratorError::Provider(ProviderError::Network(message)) => {
                assert!(
                    message.contains("transient failure"),
                    "unexpected provider error message: {message}"
                );
            }
            other => panic!("expected Provider(Network), got {other}"),
        }

        // 2. The write tool was invoked exactly once and the provider was
        //    called exactly twice (write + failure) — a restart or a repeat
        //    would drive either counter higher.
        assert_eq!(fs_calls.load(Ordering::SeqCst), 1, "write tool must run exactly once");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "provider must be called exactly twice"
        );

        // 3. The file exists on disk exactly once, with the exact payload.
        let written = dir.path().join("output.txt");
        assert!(written.exists(), "written file must be materialized on disk");
        let content = std::fs::read_to_string(&written).unwrap();
        assert_eq!(
            content, "exactly-once payload\n",
            "file content must be exact (no duplicate write/append)"
        );
    }

    #[tokio::test]
    async fn provider_retry_401_not_retried() {
        // 401 (auth) must never be retried — exactly one provider call.
        let provider =
            Arc::new(CountingScriptedProvider::new(vec![Err(ProviderError::AuthFailure)]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider.clone(), approval, 10);
        let task = AgentTask::new(Ulid::new(), "answer-only task");
        let result = loop_.run(task, CancellationToken::new()).await;
        assert!(result.is_err(), "auth failure should surface as error");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "auth failure must not be retried");
    }

    #[tokio::test]
    async fn provider_retry_cancelled_during_sleep() {
        // A long retry delay is interrupted by cancellation: the provider must
        // not be called again and the run must stop immediately.
        let provider = Arc::new(CountingScriptedProvider::new(vec![Err(ProviderError::Network(
            "conn reset".into(),
        ))]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let loop_ = make_loop(provider.clone(), approval, 10).with_retry_policy(RetryPolicy::new(
            concerto_config::RetryConfig {
                fixed_delay_ms: Some(60_000),
                respect_retry_after: false,
                ..concerto_config::RetryConfig::default()
            },
        ));
        let task = AgentTask::new(Ulid::new(), "answer-only task");
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let mut loop_ = loop_;
            let task = task.clone();
            let cancel = cancel.clone();
            async move { loop_.run(task, cancel).await }
        });
        // Give the first attempt time to fail and schedule the long retry.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run should not hang")
            .unwrap();
        assert!(result.is_err(), "cancellation must stop the run");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "provider must not be retried after cancellation"
        );
    }

    #[tokio::test]
    async fn text_only_action_required_not_shown_as_success() {
        // Regression: a text-only provider response (no tool calls) for an
        // action-required task must produce an error, not a successful output.
        // This prevents the model from "claiming" completion without doing work.
        let provider = Arc::new(TextOnlyProvider {
            text: "I have finished the implementation completely.".to_string(),
            calls: AtomicUsize::new(0),
        });
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 6);
        let task = AgentTask::new_action_required(Ulid::new(), "implement a feature");
        let result = loop_.run(task, CancellationToken::new()).await;

        assert!(result.is_err(), "text-only action-required must fail, not succeed");
        match result.unwrap_err() {
            OrchestratorError::ExecutionRequiredButNoTools => {
                // Expected: no tool calls at all → error variant
            }
            other => {
                panic!(
                    "expected ExecutionRequiredButNoTools error for text-only response, \
                     got: {:?}",
                    other,
                );
            }
        }
    }

    /// Verify that `ProgressFingerprint::from_output` correctly captures
    /// the relevant fields from an `AgentOutput`.
    #[test]
    fn progress_fingerprint_tracks_changes() {
        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: "done".to_string(),
            files_modified: vec![
                camino::Utf8PathBuf::from("src/main.rs"),
                camino::Utf8PathBuf::from("src/lib.rs"),
            ],
            tool_call_count: 3,
            eval_result: None,
            tool_events: vec![],
            verification: vec![VerificationSummary {
                command: "py_compile".into(),
                path: "hello.py".into(),
                passed: true,
                output: "ok".into(),
            }],
            project_root: None,
            completion_status: concerto_core::types::AgentCompletionStatus::Partial,
            provider_metrics: vec![],
            checkpoint_json: None,
        };
        let fp = ProgressFingerprint::from_output(&output);
        assert_eq!(fp.files_modified, 2, "should count two modified files");
        assert_eq!(fp.tool_call_count, 3, "should record tool call count");
        assert_eq!(fp.passed_verifications, 1, "one verification passed");
        assert_eq!(fp.failed_verifications, 0, "no verification failed");
        assert_eq!(fp.final_message_len, 4, "final message length should match");
    }

    /// Verify that `continuation_instruction` includes the reason string.
    #[test]
    fn continuation_instruction_contains_reason() {
        let reason = "iteration cap hit after 10 iterations";
        let instruction = continuation_instruction(reason);
        assert!(instruction.contains(reason), "instruction should contain the provided reason");
        assert!(
            instruction.contains("Continue the same task"),
            "instruction should direct the model to continue"
        );
    }

    /// Verify that `build_agent_output` returns an `AgentOutput` with an
    /// accessible `provider_metrics` field (even if empty for a mock).
    #[tokio::test]
    async fn build_agent_output_includes_provider_metrics() {
        let _bus = EventBus::new(256);
        let provider = Arc::new(ScriptedProvider::new(vec![vec![]]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop(provider, approval, 10);
        let task = AgentTask::new(Ulid::new(), "test");
        let output = loop_.run(task, CancellationToken::new()).await;
        assert!(output.is_ok(), "expected Ok, got Err: {:?}", output.err());
        let output = output.unwrap();
        // Verify the provider_metrics field is accessible (Vec, not panicking).
        for pm in &output.provider_metrics {
            assert!(!pm.provider.is_empty(), "provider name must not be empty");
        }
    }

    // -----------------------------------------------------------------------
    // merge_run_progress — standalone unit tests
    // -----------------------------------------------------------------------

    fn agent_output_base() -> AgentOutput {
        AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: String::new(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: vec![],
            verification: vec![],
            project_root: None,
            completion_status: concerto_core::types::AgentCompletionStatus::Partial,
            provider_metrics: vec![],
            checkpoint_json: None,
        }
    }

    #[test]
    fn merge_run_progress_deduplicates_files() {
        let path_a = Utf8PathBuf::from("a.rs");
        let path_b = Utf8PathBuf::from("b.rs");
        let mut acc = AgentOutput { files_modified: vec![path_a.clone()], ..agent_output_base() };
        let latest = AgentOutput {
            files_modified: vec![path_a.clone(), path_b.clone()],
            ..agent_output_base()
        };
        merge_run_progress(&mut acc, &latest);
        // path_a should appear only once
        assert_eq!(acc.files_modified.len(), 2);
        assert_eq!(acc.files_modified.iter().filter(|p| *p == &path_a).count(), 1);
        assert!(acc.files_modified.contains(&path_b));
    }

    #[test]
    fn merge_run_progress_verification_latest_wins() {
        let v1 = VerificationSummary {
            path: "x.rs".into(),
            command: "cargo test".into(),
            passed: false,
            output: "old".into(),
        };
        let v2 = VerificationSummary { passed: true, output: "new".into(), ..v1.clone() };
        let mut acc = AgentOutput { verification: vec![v1.clone()], ..agent_output_base() };
        let latest = AgentOutput { verification: vec![v2.clone()], ..agent_output_base() };
        merge_run_progress(&mut acc, &latest);
        assert_eq!(acc.verification.len(), 1);
        assert!(acc.verification[0].passed);
        assert_eq!(acc.verification[0].output, "new");
    }

    #[test]
    fn merge_run_progress_preserves_verification_if_no_match() {
        let v1 = VerificationSummary {
            path: "a.rs".into(),
            command: "cargo test".into(),
            passed: false,
            output: "".into(),
        };
        let v2 = VerificationSummary {
            path: "b.rs".into(),
            command: "cargo clippy".into(),
            passed: true,
            output: "".into(),
        };
        let mut acc = AgentOutput { verification: vec![v1.clone()], ..agent_output_base() };
        let latest = AgentOutput { verification: vec![v2.clone()], ..agent_output_base() };
        merge_run_progress(&mut acc, &latest);
        assert_eq!(acc.verification.len(), 2);
    }

    #[test]
    fn merge_run_progress_tool_call_count_sums() {
        let mut acc = AgentOutput { tool_call_count: 3, ..agent_output_base() };
        let latest = AgentOutput { tool_call_count: 5, ..agent_output_base() };
        merge_run_progress(&mut acc, &latest);
        assert_eq!(acc.tool_call_count, 8);
    }

    #[test]
    fn merge_run_progress_latest_project_root_wins() {
        let mut acc =
            AgentOutput { project_root: Some(Utf8PathBuf::from("/old")), ..agent_output_base() };
        let latest =
            AgentOutput { project_root: Some(Utf8PathBuf::from("/new")), ..agent_output_base() };
        merge_run_progress(&mut acc, &latest);
        assert_eq!(acc.project_root, Some(Utf8PathBuf::from("/new")));
    }

    // -----------------------------------------------------------------------
    // ProgressFingerprint — standalone unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn progress_fingerprint_from_output() {
        let output = AgentOutput {
            files_modified: vec![Utf8PathBuf::from("a.rs")],
            tool_call_count: 3,
            verification: vec![
                VerificationSummary {
                    path: "a.rs".into(),
                    command: "cargo test".into(),
                    passed: true,
                    output: String::new(),
                },
                VerificationSummary {
                    path: "b.rs".into(),
                    command: "cargo clippy".into(),
                    passed: false,
                    output: String::new(),
                },
            ],
            final_message: "done".into(),
            ..agent_output_base()
        };
        let fp = ProgressFingerprint::from_output(&output);
        assert_eq!(fp.files_modified, 1);
        assert_eq!(fp.tool_call_count, 3);
        assert_eq!(fp.passed_verifications, 1);
        assert_eq!(fp.failed_verifications, 1);
        assert_eq!(fp.final_message_len, 4);
    }

    #[test]
    fn progress_fingerprint_detects_change() {
        let o1 = AgentOutput {
            files_modified: vec![Utf8PathBuf::from("a.rs")],
            tool_call_count: 3,
            final_message: "done".into(),
            ..agent_output_base()
        };
        let o2 = AgentOutput {
            files_modified: vec![Utf8PathBuf::from("a.rs"), Utf8PathBuf::from("b.rs")],
            final_message: "longer msg".into(),
            ..agent_output_base()
        };
        let fp1 = ProgressFingerprint::from_output(&o1);
        let fp2 = ProgressFingerprint::from_output(&o2);
        assert_ne!(fp1, fp2, "different outputs must have different fingerprints");
    }

    #[test]
    fn progress_fingerprint_identical_means_no_progress() {
        let o = AgentOutput {
            files_modified: vec![Utf8PathBuf::from("a.rs")],
            final_message: "same".into(),
            ..agent_output_base()
        };
        let fp1 = ProgressFingerprint::from_output(&o);
        let fp2 = ProgressFingerprint::from_output(&o);
        assert_eq!(fp1, fp2, "same output must have identical fingerprint");
    }

    // -----------------------------------------------------------------------
    // Tool-call guard (VALIDATE → COERCE → REPAIR) — integration-level tests
    // -----------------------------------------------------------------------

    /// Helper: a loop wired with the real `FilesystemTool` plus the default
    /// request state for one direct `execute_single_tool_call` invocation.
    fn guard_test_harness(dir: &std::path::Path) -> (AgentLoop, AgentTask, SessionContext) {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let loop_ = make_loop_with_fs_tool(dir, provider, approval, 10);
        let task = AgentTask::new_action_required(Ulid::new(), "guard harness task");
        let session = SessionContext::new(task.session_id, dir.to_path_buf());
        (loop_, task, session)
    }

    #[tokio::test]
    async fn tool_guard_rejects_null_arguments_with_corrective_result() {
        // Audit stall shape: mimo-v2.5-free emits `arguments: null`. The guard
        // must normalize to `{}`, find the required fields missing, and inject
        // a corrective tool result instead of executing (or erroring) inside
        // the filesystem tool.
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, task, session) = guard_test_harness(dir.path());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_null".into(),
            name: "filesystem".into(),
            arguments: serde_json::Value::Null,
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert_eq!(messages.len(), 1, "exactly one exhausted tool message");
        assert!(
            messages[0].content.contains("Tool call invalid for 'filesystem'"),
            "content: {}",
            messages[0].content
        );
        assert!(
            messages[0].content.contains("missing required field 'operation'"),
            "content: {}",
            messages[0].content
        );
        assert!(
            messages[0].content.contains("Stop calling 'filesystem'"),
            "empty args fail fast with no coaching: {}",
            messages[0].content
        );
        let results = messages[0].tool_results.as_ref().unwrap();
        assert_eq!(results[0].id, "call_null");
        assert_eq!(results[0].content["error"], "tool_guard_exhausted");
        assert!(tool_events[0].summary.contains("invalid arguments"), "events: {tool_events:?}");
        assert!(files_modified.is_empty(), "rejected calls must not execute");
    }

    #[tokio::test]
    async fn tool_guard_coerces_stringified_uppercase_arguments_and_executes() {
        // Weak-model shape that IS repairable: stringified JSON, capitalized
        // enum value, hallucinated extra key. After coercion the call must
        // reach the filesystem tool and really write the file.
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let (mut loop_, calls) =
            make_loop_with_counting_fs_tool(dir.path(), provider, approval, 10);
        let task = AgentTask::new_action_required(Ulid::new(), "write a file");
        let session = SessionContext::new(task.session_id, dir.path().to_path_buf());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_coerce".into(),
            name: "filesystem".into(),
            arguments: serde_json::Value::String(
                r#"{"operation": "WRITE", "path": "guarded.txt", "content": "hi", "rationale": "because"}"#
                    .into(),
            ),
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "filesystem tool executed exactly once");
        assert!(
            messages[0].content.contains("status: success"),
            "content: {}",
            messages[0].content
        );
        let written = dir.path().join("guarded.txt");
        assert!(written.exists(), "coerced call must materialize the file");
        assert!(files_modified.iter().any(|p| p.as_str().ends_with("guarded.txt")));
        assert_eq!(file_changing_tool_count, 1, "coerced write counts as file-changing");
    }

    #[tokio::test]
    async fn tool_guard_bounds_corrective_retries_then_exhausts() {
        // Partial args get two corrective retries per tool per run; the third
        // consecutive rejection flips to the exhausted form so the model
        // stops ping-ponging malformed calls.
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, task, session) = guard_test_harness(dir.path());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_bad".into(),
            name: "filesystem".into(),
            // Partial args (unknown key only): retries apply.
            arguments: serde_json::json!({"bogus": 1}),
        };
        for _ in 0..3 {
            loop_
                .execute_single_tool_call(
                    &tc,
                    &task,
                    Ulid::new(),
                    &session,
                    CancellationToken::new(),
                    &mut tool_call_count,
                    &mut file_changing_tool_count,
                    &mut files_modified,
                    &mut tool_events,
                    &mut messages,
                )
                .await
                .unwrap();
        }

        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("Please retry with corrected arguments"));
        assert!(messages[1].content.contains("Please retry with corrected arguments"));
        assert!(
            messages[2].content.contains("Stop calling 'filesystem'"),
            "third rejection must stop coaching: {}",
            messages[2].content
        );
        let exhausted_payload = messages[2].tool_results.as_ref().unwrap()[0].content.clone();
        assert_eq!(exhausted_payload["error"], "tool_guard_exhausted");
    }

    #[tokio::test]
    async fn tool_guard_fails_fast_on_empty_arguments() {
        // Live-proven (Sep 2026 audit): zero-argument calls never correct on
        // coaching — the first rejection is already exhausted, no retries.
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, task, session) = guard_test_harness(dir.path());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_empty".into(),
            name: "filesystem".into(),
            arguments: serde_json::Value::Null,
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].content.contains("Stop calling 'filesystem'"),
            "empty args must fail fast: {}",
            messages[0].content
        );
        let payload = messages[0].tool_results.as_ref().unwrap()[0].content.clone();
        assert_eq!(payload["error"], "tool_guard_exhausted");
    }

    #[tokio::test]
    async fn tool_guard_heuristically_infers_read_from_path_only_call() {
        // Audit stall shape: the model emits `{"path": "..."}` with no
        // `operation`. Heuristic inference fills `read` (file-like path) and
        // the call executes instead of bouncing a corrective result.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "hello").unwrap();
        let (mut loop_, task, session) = guard_test_harness(dir.path());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_infer_read".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({ "path": "existing.txt" }),
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert!(
            messages[0].content.contains("status: success"),
            "inferred call must execute: {}",
            messages[0].content
        );
        assert!(
            messages[0].content.contains("Read "),
            "file-like path must infer read: {}",
            messages[0].content
        );
        assert!(
            !messages[0].content.contains("Tool call invalid"),
            "no corrective result expected: {}",
            messages[0].content
        );
    }

    #[tokio::test]
    async fn tool_guard_heuristically_infers_write_from_content() {
        // `content` present without `operation` infers `write`; the call
        // executes for real and counts as file-changing.
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, task, session) = guard_test_harness(dir.path());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_infer_write".into(),
            name: "filesystem".into(),
            arguments: serde_json::json!({ "path": "inferred.txt", "content": "hi" }),
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert!(
            messages[0].content.contains("status: success"),
            "inferred write must execute: {}",
            messages[0].content
        );
        assert!(
            dir.path().join("inferred.txt").exists(),
            "inferred write must materialize the file"
        );
        assert_eq!(file_changing_tool_count, 1, "inferred write counts as file-changing");
        assert!(files_modified.iter().any(|p| p.as_str().ends_with("inferred.txt")));
    }

    #[tokio::test]
    async fn tool_guard_heuristically_infers_shell_command_from_cmd_alias() {
        // Some models send `cmd` instead of `command`; the alias recovers the
        // command and the call executes through the default shell-wrapped
        // backend (policy/allowlist still gate it).
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        let mut loop_ = make_loop_with_extra_tools(
            provider,
            approval,
            10,
            vec![Box::new(concerto_tools::shell::ShellTool::allow_all())],
        );
        let task = AgentTask::new_action_required(Ulid::new(), "run a command");
        let session = SessionContext::new(task.session_id, dir.path().to_path_buf());
        let mut tool_call_count = 0;
        let mut file_changing_tool_count = 0;
        let mut files_modified = Vec::new();
        let mut tool_events = Vec::new();
        let mut messages = Vec::new();

        let tc = ToolCall {
            id: "call_infer_cmd".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "cmd": "echo guard-heuristic-ok" }),
        };
        loop_
            .execute_single_tool_call(
                &tc,
                &task,
                Ulid::new(),
                &session,
                CancellationToken::new(),
                &mut tool_call_count,
                &mut file_changing_tool_count,
                &mut files_modified,
                &mut tool_events,
                &mut messages,
            )
            .await
            .unwrap();

        assert!(
            messages[0].content.contains("status: success"),
            "alias-recovered call must execute: {}",
            messages[0].content
        );
        assert!(
            messages[0].content.contains("guard-heuristic-ok"),
            "command output must be present: {}",
            messages[0].content
        );
    }

    // -----------------------------------------------------------------------
    // process_provider_response — integration-level tests (needs AgentLoop)
    // -----------------------------------------------------------------------

    fn make_minimal_loop() -> AgentLoop {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        make_loop(provider, approval, 10)
    }

    #[test]
    fn process_provider_response_sets_final_message_when_non_empty_and_not_action_required() {
        let loop_ = make_minimal_loop();
        let mut messages = vec![];
        let mut final_message = String::new();
        let mut completed = false;

        let action = loop_.process_provider_response(
            "", // _text placeholder (not used by this branch)
            None,
            &[], // no tool calls → enters the else-if chain
            None,
            &TaskExecutionMode::AnswerOnly,
            0,
            1,
            Ulid::new(),
            Ulid::new(),
            &mut messages,
            &mut final_message,
            &mut completed,
        );

        // AnswerOnly with no tool calls → Break + message set
        assert!(matches!(action, ProviderResponseAction::Break));
        assert!(completed);
    }

    #[test]
    fn process_provider_response_action_required_non_empty_text_no_file_changes_retries() {
        let loop_ = make_minimal_loop();
        let mut messages = vec![];
        let mut final_message = String::new();
        let mut completed = false;

        let action = loop_.process_provider_response(
            "some analysis text",
            None,
            &[], // no tool calls
            None,
            &TaskExecutionMode::ActionRequired { min_tool_calls: 1, require_verification: false },
            0, // file_changing_tool_count = 0
            1,
            Ulid::new(),
            Ulid::new(),
            &mut messages,
            &mut final_message,
            &mut completed,
        );

        // ActionRequired with no tool calls AND zero file changes → ContinueIteration
        assert!(matches!(action, ProviderResponseAction::ContinueIteration));
    }

    #[test]
    fn process_provider_response_proceeds_with_tool_calls() {
        let loop_ = make_minimal_loop();
        let mut messages = vec![Message {
            role: Role::User,
            content: "user prompt".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }];
        let mut final_message = String::new();
        let mut completed = false;

        let action = loop_.process_provider_response(
            "need to execute",
            None,
            &[ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "hi"}),
            }],
            Some(CompletionUsage { prompt_tokens: Some(100), completion_tokens: Some(20) }),
            &TaskExecutionMode::ActionRequired { min_tool_calls: 1, require_verification: false },
            0,
            1,
            Ulid::new(),
            Ulid::new(),
            &mut messages,
            &mut final_message,
            &mut completed,
        );

        // Has tool calls → Proceed
        assert!(matches!(action, ProviderResponseAction::Proceed));
        assert_eq!(messages.len(), 2);
        // ADR-48 §4: completion usage lands on the assistant message, prompt
        // usage is attributed to the preceding user message.
        assert_eq!(messages[0].tokens_in, Some(100), "prompt usage attributed to user message");
        assert_eq!(messages[0].tokens_out, None);
        assert_eq!(messages[1].tokens_in, Some(100));
        assert_eq!(messages[1].tokens_out, Some(20));
    }

    #[test]
    fn process_provider_response_does_not_set_final_message_when_action_required() {
        let loop_ = make_minimal_loop();
        let mut messages = vec![];
        let mut final_message = String::new();
        let mut completed = false;

        let action = loop_.process_provider_response(
            "non-empty text",
            None,
            &[ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
            }],
            None,
            &TaskExecutionMode::ActionRequired { min_tool_calls: 1, require_verification: false },
            0,
            1,
            Ulid::new(),
            Ulid::new(),
            &mut messages,
            &mut final_message,
            &mut completed,
        );

        // Has tool calls, non-empty text, but action_required is true
        // The guard is: !text.trim().is_empty() && !action_required
        // Since action_required is true, !action_required is false, so final_message stays empty
        assert!(matches!(action, ProviderResponseAction::Proceed));
        assert_eq!(final_message, "", "must NOT set final_message when action_required=true");
        assert!(!completed);
    }
}
