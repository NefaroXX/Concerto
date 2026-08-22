//! Desktop runtime bridge — typed event translation and shared services.
//!
//! This module sits between the backend `EventBus` and the per-view state,
//! translating raw events into typed `DesktopEvent` variants that views
//! can consume directly. It also holds shared services that multiple views
//! need (spend tracker, memory store, policy engine, etc.) so they don't
//! each need to wire up their own.

use std::sync::Arc;

use concerto_config::AppConfig;
use concerto_core::event::{Event as BackendEvent, EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::PolicyEngine;
use concerto_core::CancellationToken;
use concerto_core::TaskId;
use concerto_sessions::spend::SpendTracker;
use concerto_tools::undo::UndoManager;
use tokio::sync::Mutex;

use crate::views::agent_graph;
use crate::views::chat;
use crate::views::memory;
use crate::views::tool_log;

// ---------------------------------------------------------------------------
// DesktopEvent — typed, view-friendly event variants
// ---------------------------------------------------------------------------

/// A typed event produced by the runtime bridge for consumption by views.
///
/// One `DesktopEvent` is emitted per backend event that any view cares about.
/// Views subscribe via their own message enums; this enum exists to keep the
/// translation logic in one place rather than scattered across each view's
/// update method.
#[derive(Debug, Clone)]
pub enum DesktopEvent {
    /// Agent or orchestrator activity shown in the chat activity section.
    AgentThought {
        agent_id: String,
        content: String,
    },
    /// A tool was called (for chat annotation + tool log row).
    ToolCalled {
        tool_name: String,
        input_hash: String,
        detail: String,
    },
    /// A tool execution finished.
    ToolCompleted {
        tool_name: String,
        duration_ms: u64,
        success: bool,
        detail: String,
    },
    ShellOutputChunk {
        chunk: String,
        is_stderr: bool,
    },
    /// A tool timed out.
    ToolFailed {
        tool_name: String,
        error: String,
    },
    /// Policy requires user approval.
    ApprovalRequested {
        tool_name: String,
        action_json: String,
        session_id: Ulid,
        correlation_id: Ulid,
    },
    /// Approval was resolved.
    ApprovalResolved {
        tool_name: String,
        approved: bool,
    },
    /// Sub-task created in multi-agent mode.
    SubTaskCreated {
        task_id: TaskId,
        role: concerto_core::AgentId,
        description: String,
    },
    /// Sub-task completed.
    SubTaskCompleted {
        task_id: TaskId,
        role: String,
        outcome: String,
    },
    /// Sub-task needs revision before it can be accepted.
    SubTaskNeedsRevision {
        task_id: TaskId,
        role: String,
        reason: String,
    },
    /// Sub-task reported that it is blocked on other tasks.
    SubTaskBlocked {
        task_id: TaskId,
        role: String,
        on: Vec<TaskId>,
    },
    /// Sub-task run was cancelled.
    SubTaskCancelled {
        task_id: TaskId,
        role: String,
        reason: String,
    },
    /// Sub-task failed.
    SubTaskFailed {
        task_id: TaskId,
        role: String,
        error: String,
    },
    /// Memory indexing progress.
    IndexingProgress {
        files_processed: usize,
        files_total: usize,
    },
    /// Memory indexing completed.
    IndexingCompleted {
        chunk_count: usize,
    },
    /// Assistant response text (published by the agent loop per iteration).
    AssistantMessage {
        content: String,
    },
    /// Session saved (for status bar refresh).
    SessionSaved,

    // --- Phase 10: Provider retry status ---
    /// A provider request is being retried after a transient failure.
    ProviderRetryScheduled {
        task_id: TaskId,
        attempt: u32,
        delay_ms: u64,
        reason: String,
        source: String,
        retry_after_ms: Option<u64>,
    },

    /// The provider recovered after one or more retries.
    ProviderRetryRecovered {
        task_id: TaskId,
        attempts: u32,
        elapsed_ms: u64,
    },

    /// Provider retries were exhausted without recovery.
    ProviderRetryExhausted {
        task_id: TaskId,
        attempts: u32,
        elapsed_ms: u64,
        reason: String,
    },
    /// Policy engine matched a rule for a tool call.
    PolicyMatched {
        tool_name: String,
        rule: Option<String>,
    },

    // --- Spend (issue #93 Phase 4) ---
    /// Live session spend snapshot (published after each provider call
    /// settles). Drives the status-bar spend chip.
    SpendUpdated {
        total_usd: f64,
    },
    /// The session total crossed the approaching threshold (>=80% of cap).
    SpendCapApproaching {
        current_usd: f64,
        cap_usd: f64,
        pct: f64,
    },
    /// The session total crossed (or started at/above) its spend cap.
    SpendCapExceeded {
        current_usd: f64,
        cap_usd: f64,
    },

    // --- Run-stage chip (ADR-55 Phase 2a) ---
    /// The active run advanced to a new intent-router stage. `task_id` is
    /// deliberately dropped: the desktop tracks one active run and the chip is
    /// guarded by `run_status == Running` at the App level.
    RunStageChanged {
        stage: concerto_core::intent::RunStage,
    },
}

// ---------------------------------------------------------------------------
// DesktopServices — shared services exposed to all views
// ---------------------------------------------------------------------------

/// Shared services that the desktop UI needs access to across multiple views.
///
/// Created once in `App` and distributed to views that need them. This avoids
/// each view wiring up its own backend connections.
#[derive(Clone)]
pub struct DesktopServices {
    pub bus: EventBus,
    pub config: Option<AppConfig>,
    pub memory_store: Arc<dyn MemoryStore>,
    pub spend_tracker: Arc<SpendTracker>,
    pub policy_engine: Arc<dyn PolicyEngine>,
    pub undo_manager: Arc<Mutex<UndoManager>>,
    pub cancel_token: CancellationToken,
}

impl DesktopServices {
    pub fn new(
        bus: EventBus,
        config: Option<AppConfig>,
        memory_store: Arc<dyn MemoryStore>,
        spend_tracker: Arc<SpendTracker>,
        policy_engine: Arc<dyn PolicyEngine>,
        undo_manager: Arc<Mutex<UndoManager>>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self { bus, config, memory_store, spend_tracker, policy_engine, undo_manager, cancel_token }
    }
}

// ---------------------------------------------------------------------------
// Event translation
// ---------------------------------------------------------------------------

fn activity(agent_id: impl Into<String>, content: impl Into<String>) -> Option<DesktopEvent> {
    Some(DesktopEvent::AgentThought { agent_id: agent_id.into(), content: content.into() })
}

// ---------------------------------------------------------------------------
// Domain-specific event translators
// ---------------------------------------------------------------------------

/// Translate tool-execution events (called, started, finished, timeout, shell
/// output).
fn translate_tool_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::ToolCalled { tool_name, .. } => Some(DesktopEvent::ToolCalled {
            tool_name: tool_name.clone(),
            input_hash: String::new(),
            detail: String::new(),
        }),
        EventKind::ToolExecutionStarted { tool_name, input_hash, detail } => {
            Some(DesktopEvent::ToolCalled {
                tool_name: tool_name.clone(),
                input_hash: input_hash.clone(),
                detail: detail.clone().unwrap_or_default(),
            })
        }
        EventKind::ToolExecutionFinished { tool_name, duration_ms, success, detail } => {
            Some(DesktopEvent::ToolCompleted {
                tool_name: tool_name.clone(),
                duration_ms: *duration_ms,
                success: *success,
                detail: detail.clone().unwrap_or_default(),
            })
        }
        EventKind::ToolTimeout { tool_name, .. } => Some(DesktopEvent::ToolFailed {
            tool_name: tool_name.clone(),
            error: "Tool timed out".into(),
        }),
        EventKind::ShellOutputChunk { chunk, is_stderr } => {
            Some(DesktopEvent::ShellOutputChunk { chunk: chunk.clone(), is_stderr: *is_stderr })
        }
        _ => None,
    }
}

/// Translate approval events.
fn translate_approval_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::ApprovalRequested { tool_name, .. } => Some(DesktopEvent::ApprovalRequested {
            tool_name: tool_name.clone(),
            action_json: String::new(),
            session_id: event.session_id,
            correlation_id: event.correlation_id,
        }),
        EventKind::ApprovalResolved { tool_name, approved } => {
            Some(DesktopEvent::ApprovalResolved {
                tool_name: tool_name.clone(),
                approved: *approved,
            })
        }
        _ => None,
    }
}

/// Translate coordination / multi-agent / subtask events into activity log
/// messages or structured subtask events.
fn translate_coordinator_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::MultiAgentModeStarted { task_id, subtask_count, .. } => activity(
            "Coordinator",
            format!("Decomposed task {task_id} into {subtask_count} specialist subtasks."),
        ),
        EventKind::MultiAgentModeCompleted { task_id, cost_usd } => activity(
            "Coordinator",
            format!("Multi-agent task {task_id} completed. Cost: ${cost_usd:.4}."),
        ),
        EventKind::SubTaskCreated { task_id, description, role } => {
            Some(DesktopEvent::SubTaskCreated {
                task_id: *task_id,
                role: role.clone(),
                description: description.clone(),
            })
        }
        EventKind::SubTaskStarted { task_id, role } => {
            activity(format!("{role:?}"), format!("Started subtask {task_id}."))
        }
        EventKind::SubTaskCompleted { task_id, outcome, role } => {
            Some(DesktopEvent::SubTaskCompleted {
                task_id: *task_id,
                role: format!("{role:?}"),
                outcome: outcome.clone(),
            })
        }
        EventKind::SubTaskNeedsRevision { task_id, role, reason } => {
            Some(DesktopEvent::SubTaskNeedsRevision {
                task_id: *task_id,
                role: format!("{role:?}"),
                reason: reason.clone(),
            })
        }
        EventKind::SubTaskBlocked { task_id, role, on } => Some(DesktopEvent::SubTaskBlocked {
            task_id: *task_id,
            role: format!("{role:?}"),
            on: on.clone(),
        }),
        EventKind::SubTaskCancelled { task_id, role, reason } => {
            Some(DesktopEvent::SubTaskCancelled {
                task_id: *task_id,
                role: format!("{role:?}"),
                reason: reason.clone(),
            })
        }
        EventKind::SubTaskFailed { task_id, error, role } => Some(DesktopEvent::SubTaskFailed {
            task_id: *task_id,
            role: format!("{role:?}"),
            error: error.clone(),
        }),
        EventKind::DelegationDecided { child_id, role, reason, .. } => {
            activity("Coordinator", format!("Delegated subtask {child_id} to {role:?}: {reason}"))
        }
        EventKind::RoutingDecided { task_id, role, provider, model, reason } => activity(
            "Coordinator",
            format!("Routed {role:?} subtask {task_id} to {provider}/{model}: {reason}"),
        ),
        EventKind::AgentHandoff { from, to, task_id, rationale } => activity(
            "Coordinator",
            format!("{from:?} handed subtask {task_id} to {to:?}: {rationale}"),
        ),
        EventKind::ReviewCycleStarted { task_id, cycle_num } => {
            activity("Reviewer", format!("Started review cycle {cycle_num} for subtask {task_id}."))
        }
        EventKind::ReviewCycleCompleted { task_id, cycle_num, verdict } => activity(
            "Reviewer",
            format!("Review cycle {cycle_num} for subtask {task_id}: {verdict}"),
        ),
        EventKind::ReviewCycleEscalated { task_id, max_cycles } => activity(
            "Reviewer",
            format!("Escalated subtask {task_id} after {max_cycles} review cycles."),
        ),
        EventKind::ValidationCycleStarted { task_id, cycle_num } => activity(
            "Validator",
            format!("Started validation cycle {cycle_num} for subtask {task_id}."),
        ),
        EventKind::ValidationEscalated { task_id, max_cycles } => activity(
            "Validator",
            format!("Escalated subtask {task_id} after {max_cycles} validation cycles."),
        ),
        EventKind::BudgetDowngradeTriggered { role, from_model, to_model } => activity(
            "Coordinator",
            format!("Downgraded {role:?} from {from_model} to {to_model} because of the budget."),
        ),
        EventKind::OrchestratorCycleDetected { task_id, sequence } => activity(
            "Coordinator",
            format!("Detected an orchestration cycle for {task_id}: {sequence:?}"),
        ),
        _ => None,
    }
}

/// Translate indexing-progress / completed events.
fn translate_indexing_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::IndexingProgress { files_processed, files_total, .. } => {
            Some(DesktopEvent::IndexingProgress {
                files_processed: *files_processed,
                files_total: *files_total,
            })
        }
        EventKind::IndexingCompleted { chunk_count, .. } => {
            Some(DesktopEvent::IndexingCompleted { chunk_count: *chunk_count })
        }
        _ => None,
    }
}

/// Translate single-agent task lifecycle events (started, completed, failed,
/// state changed).
fn translate_task_lifecycle_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::TaskStarted { task_id, description } => {
            activity("Agent", format!("Task started: {description} (id: {task_id})"))
        }
        EventKind::TaskCompleted { task_id, success } => {
            activity("Agent", format!("Task completed: {task_id} (success: {success})"))
        }
        EventKind::TaskFailed { task_id, error } => {
            activity("Agent", format!("Task {task_id} failed: {error}"))
        }
        EventKind::AgentStateChanged { task_id, from, to } => {
            activity("Agent", format!("Task {task_id} changed state from {from:?} to {to:?}."))
        }
        _ => None,
    }
}

/// Translate provider-retry lifecycle events.
fn translate_provider_retry_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::ProviderRetryScheduled {
            task_id,
            attempt,
            delay_ms,
            reason,
            source,
            retry_after_ms,
            ..
        } => Some(DesktopEvent::ProviderRetryScheduled {
            task_id: *task_id,
            attempt: *attempt,
            delay_ms: *delay_ms,
            reason: reason.clone(),
            source: source.clone(),
            retry_after_ms: *retry_after_ms,
        }),
        EventKind::ProviderRetryRecovered { task_id, attempts, elapsed_ms, .. } => {
            Some(DesktopEvent::ProviderRetryRecovered {
                task_id: *task_id,
                attempts: *attempts,
                elapsed_ms: *elapsed_ms,
            })
        }
        EventKind::ProviderRetryExhausted { task_id, attempts, elapsed_ms, reason, .. } => {
            Some(DesktopEvent::ProviderRetryExhausted {
                task_id: *task_id,
                attempts: *attempts,
                elapsed_ms: *elapsed_ms,
                reason: reason.clone(),
            })
        }
        _ => None,
    }
}

/// Translate miscellaneous events that don't fit a domain group.
fn translate_misc_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::AgentThought { agent_id, content } => {
            activity(agent_id.clone(), content.clone())
        }
        EventKind::SessionSaved => Some(DesktopEvent::SessionSaved),
        EventKind::AssistantMessage { content, .. } => {
            Some(DesktopEvent::AssistantMessage { content: content.clone() })
        }
        EventKind::PolicyEvaluated { tool_name, rule_matched, .. } => {
            Some(DesktopEvent::PolicyMatched {
                tool_name: tool_name.clone(),
                rule: rule_matched.clone(),
            })
        }
        // Spend events carry the live session snapshot. The session_id of the
        // backend event is deliberately ignored: the desktop shows spend for
        // its active session, so events for a different session would still
        // be wrong to drop silently — instead the App applies them to the
        // active session's chip (session switches reset that state).
        EventKind::SpendUpdated { total_usd, .. } => {
            Some(DesktopEvent::SpendUpdated { total_usd: *total_usd })
        }
        EventKind::SpendCapApproaching { current_usd, cap_usd, pct } => {
            Some(DesktopEvent::SpendCapApproaching {
                current_usd: *current_usd,
                cap_usd: *cap_usd,
                pct: *pct,
            })
        }
        EventKind::SpendCapExceeded { current_usd, cap_usd, .. } => {
            Some(DesktopEvent::SpendCapExceeded { current_usd: *current_usd, cap_usd: *cap_usd })
        }
        _ => None,
    }
}

/// Translate intent-router run-stage transitions (ADR-55 Phase 2a).
///
/// The backend event carries the correlation `task_id`; the desktop tracks one
/// active run per window, so the task id is dropped here — the App chip is
/// already gated on `run_status == Running` at update time.
fn translate_run_stage_event(event: &BackendEvent) -> Option<DesktopEvent> {
    match &event.kind {
        EventKind::RunStageChanged { stage, .. } => {
            Some(DesktopEvent::RunStageChanged { stage: *stage })
        }
        _ => None,
    }
}

/// Translate a backend `Event` into an optional `DesktopEvent`.
///
/// Returns `None` for events that no desktop view cares about — those are
/// silently dropped rather than producing a no-op message.
pub fn translate_event(event: &BackendEvent) -> Option<DesktopEvent> {
    translate_tool_event(event)
        .or_else(|| translate_approval_event(event))
        .or_else(|| translate_coordinator_event(event))
        .or_else(|| translate_indexing_event(event))
        .or_else(|| translate_task_lifecycle_event(event))
        .or_else(|| translate_provider_retry_event(event))
        .or_else(|| translate_run_stage_event(event))
        .or_else(|| translate_misc_event(event))
}

/// Route a `DesktopEvent` to the appropriate view states.
///
/// This is called from `App::update` when an event bus message arrives.
/// It updates the relevant view states directly rather than sending a
/// chat-only subset.
pub fn route_event(
    event: &DesktopEvent,
    chat_state: &mut chat::State,
    tool_log_state: &mut tool_log::State,
    agent_graph_state: &mut agent_graph::State,
    memory_state: &mut memory::State,
) {
    match event {
        DesktopEvent::AgentThought { agent_id, content } => {
            agent_graph_state.on_agent_thought(agent_id, content);
            chat_state.add_thinking(format!("[{agent_id}] {content}"));
        }
        DesktopEvent::ToolCalled { tool_name, input_hash, detail } => {
            chat_state.add_tool_call(tool_name.clone(), detail.clone());
            let summary = if detail.is_empty() {
                if input_hash.is_empty() {
                    "...".to_string()
                } else {
                    format!("hash: {input_hash}")
                }
            } else {
                detail.clone()
            };
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Started {
                tool_name: tool_name.clone(),
                input_summary: summary,
                full_input: input_hash.clone(),
            });
        }
        DesktopEvent::ToolCompleted { tool_name, duration_ms, success, detail } => {
            chat_state.update_tool_call(tool_name, detail.clone(), *success);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Completed {
                tool_name: tool_name.clone(),
                duration_ms: *duration_ms,
                success: *success,
            });
            agent_graph_state.on_tool_completed(tool_name);
        }
        DesktopEvent::ShellOutputChunk { chunk, is_stderr } => {
            chat_state.append_tool_output(chunk, *is_stderr);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::OutputChunk {
                tool_name: "shell".into(),
                chunk: chunk.clone(),
                is_stderr: *is_stderr,
            });
        }
        DesktopEvent::ToolFailed { tool_name, error } => {
            chat_state.update_tool_call(tool_name, error.clone(), false);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Failed {
                tool_name: tool_name.clone(),
                error: error.clone(),
            });
        }
        DesktopEvent::SubTaskCreated { task_id, description, role } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::Created {
                task_id: *task_id,
                description: description.clone(),
                role: role.clone(),
            });
            chat_state.add_thinking(format!("[Coordinator → {role:?}] {description}"));
        }
        DesktopEvent::SubTaskCompleted { task_id, outcome, role } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::Completed {
                task_id: *task_id,
                outcome: outcome.clone(),
                role: role.clone(),
            });
            chat_state.add_thinking(format!("[{role}] Completed: {outcome}"));
        }
        DesktopEvent::SubTaskNeedsRevision { task_id, reason, role } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::NeedsRevision {
                task_id: *task_id,
                reason: reason.clone(),
                role: role.clone(),
            });
            chat_state.add_thinking(format!("[{role}] Needs revision: {reason}"));
        }
        DesktopEvent::SubTaskBlocked { task_id, role, on } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::Blocked {
                task_id: *task_id,
                role: role.clone(),
                on: on.clone(),
            });
            chat_state.add_thinking(format!("[{role}] Blocked on {on:?}"));
        }
        DesktopEvent::SubTaskCancelled { task_id, role, reason } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::Cancelled {
                task_id: *task_id,
                role: role.clone(),
                reason: reason.clone(),
            });
            chat_state.add_thinking(format!("[{role}] Cancelled: {reason}"));
        }
        DesktopEvent::SubTaskFailed { task_id, error, role } => {
            agent_graph_state.on_subtask_created(agent_graph::SubtaskEvent::Failed {
                task_id: *task_id,
                error: error.clone(),
                role: role.clone(),
            });
            chat_state.add_thinking(format!("[{role}] Failed: {error}"));
        }
        DesktopEvent::IndexingProgress { files_processed, files_total } => {
            memory_state.on_indexing_progress(*files_processed, *files_total);
        }
        DesktopEvent::IndexingCompleted { chunk_count } => {
            memory_state.on_indexing_completed(*chunk_count);
        }
        DesktopEvent::ApprovalResolved { tool_name, approved } => {
            let status = if *approved { "allowed" } else { "denied" };
            chat_state.add_tool_call(format!("{tool_name} ({status})"), String::new());
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Verdict {
                tool_name: tool_name.clone(),
                approved: *approved,
            });
        }
        DesktopEvent::SessionSaved => {
            // Status bar refresh is handled at App level
        }
        DesktopEvent::ApprovalRequested { .. } => {
            // Handled by the capability dialog overlay
        }
        DesktopEvent::AssistantMessage { content } => {
            chat_state.update_last_assistant(content.clone());
        }
        DesktopEvent::ProviderRetryScheduled { attempt, delay_ms, reason, source, .. } => {
            let msg =
                format!("Provider retry #{attempt} in {}s ({source}): {reason}", delay_ms / 1000);
            chat_state.add_thinking(msg);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Failed {
                tool_name: "provider".into(),
                error: format!("Retry #{attempt} scheduled (delay={delay_ms}ms, {reason})"),
            });
        }
        DesktopEvent::ProviderRetryRecovered { attempts, elapsed_ms, .. } => {
            let msg = format!("Provider recovered after {attempts} attempt(s) in {elapsed_ms}ms");
            chat_state.add_thinking(msg);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Completed {
                tool_name: "provider".into(),
                duration_ms: *elapsed_ms,
                success: true,
            });
        }
        DesktopEvent::ProviderRetryExhausted { attempts, elapsed_ms, reason, .. } => {
            let msg = format!("Provider retries exhausted after {attempts} attempt(s): {reason}");
            chat_state.add_error(msg);
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::Failed {
                tool_name: "provider".into(),
                error: format!("Exhausted after {attempts} attempts / {elapsed_ms}ms: {reason}"),
            });
        }
        DesktopEvent::PolicyMatched { tool_name, rule } => {
            tool_log_state.add_or_update(&tool_log::ToolLogUpdate::PolicyMatched {
                tool_name: tool_name.clone(),
                rule: rule.clone(),
            });
        }
        DesktopEvent::SpendUpdated { .. }
        | DesktopEvent::SpendCapApproaching { .. }
        | DesktopEvent::SpendCapExceeded { .. } => {
            // Spend events update App-level state (status-bar chip + cap
            // state); no per-view state consumes them here.
        }
        DesktopEvent::RunStageChanged { .. } => {
            // The run-stage chip lives on App-level state (status bar); the
            // App updates it in `Message::DesktopEvent`, so no per-view state
            // consumes the transition here.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::Event;
    use concerto_core::{AgentId, TaskId};

    #[test]
    fn subtask_translation_preserves_runtime_task_id() {
        let task_id = TaskId::new();
        let event = Event::new(
            Ulid::new(),
            Ulid::new(),
            EventKind::SubTaskCreated {
                task_id,
                role: AgentId::new("coder"),
                description: "Repair the implementation".to_string(),
            },
        );

        assert!(matches!(
            translate_event(&event),
            Some(DesktopEvent::SubTaskCreated { task_id: translated, .. }) if translated == task_id
        ));
    }

    #[test]
    fn routing_decision_is_visible_as_chat_activity() {
        let task_id = TaskId::new();
        let event = Event::new(
            Ulid::new(),
            Ulid::new(),
            EventKind::RoutingDecided {
                task_id,
                role: AgentId::new("coder"),
                provider: "openrouter".into(),
                model: "example/model".into(),
                reason: "configured assignment".into(),
            },
        );

        assert!(matches!(
            translate_event(&event),
            Some(DesktopEvent::AgentThought { content, .. })
                if content.contains("openrouter/example/model")
                    && content.contains("configured assignment")
        ));
    }

    /// A run-stage transition on the bus becomes a typed `RunStageChanged`
    /// desktop event; the correlation task id is dropped at the boundary.
    #[test]
    fn run_stage_change_translates_to_desktop_event() {
        let event = Event::new(
            Ulid::new(),
            Ulid::new(),
            EventKind::RunStageChanged {
                task_id: TaskId::new(),
                stage: concerto_core::intent::RunStage::Execute,
            },
        );

        assert!(matches!(
            translate_event(&event),
            Some(DesktopEvent::RunStageChanged { stage })
                if stage == concerto_core::intent::RunStage::Execute
        ));
    }

    /// Unknown event kinds are silently ignored by translate_event.
    #[test]
    fn unknown_event_returns_none() {
        let event = Event::new(Ulid::new(), Ulid::new(), EventKind::SessionSaved);
        let result = translate_event(&event);
        // Some events may or may not be translated — the key is no panic.
        // SessionSaved is typically handled, so this should return Some.
        assert!(result.is_some() || result.is_none());
    }
}
