//! Durable typed session transcript (ADR-36).
//!
//! [`TranscriptEntry`] is the single canonical, UI-faithful record of a
//! session's conversation: user prompts, assistant text, agent thinking,
//! correlated tool calls with approval outcomes, orchestration activity,
//! errors, context summaries and the run completion. Both live and restored
//! UIs render from this model; the recorder (Stage 2) derives entries from the
//! event stream via [`transcript_entry_from_event`] and persists them through
//! `concerto_sessions::SessionStore::append_transcript`.
//!
//! The transcript is intentionally NOT a full payload record: tool args and
//! results remain hash + detail summaries (full payloads stay in the audit
//! log).

use crate::event::EventKind;
use serde::{Deserialize, Serialize};

/// Outcome of a tool call shown in the transcript.
///
/// The live UI shows a `Running` call until a terminal event arrives; restored
/// transcripts may also settle still-Running entries as `Cancelled` at run end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptToolStatus {
    /// Tool invocation started but has no terminal outcome yet.
    Running,
    /// Tool execution finished successfully.
    Completed,
    /// Tool execution failed or timed out.
    Failed,
    /// The invocation was approved by the user.
    Allowed,
    /// The invocation was denied by the user.
    Denied,
    /// The approval request expired without a user decision.
    Cancelled,
}

/// One canonical, durable line of a session transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptEntry {
    /// The user's prompt.
    User { content: String },
    /// Assistant text response.
    Assistant { content: String },
    /// Agent thinking / internal reasoning surfaced to the UI.
    Thinking { agent: String, content: String },
    /// A tool call. The recorder pushes one entry per invocation and merges
    /// terminal/approval events into it (Stage 2); the pure mapping below
    /// produces one entry per event.
    ToolCall { tool_name: String, detail: String, status: TranscriptToolStatus },
    /// Orchestration activity (delegation, routing, handoffs, reviews,
    /// validations, task lifecycle, provider retries).
    Activity { agent: String, content: String },
    /// An error surfaced to the user.
    Error { content: String },
    /// Context compaction summary.
    Summary { content: String },
    /// Run completion marker appended at the end of a session.
    Completion {
        multi_agent: bool,
        completed: bool,
        files: Vec<String>,
        project_root: Option<String>,
    },
}

/// User-facing gate names used in the review/validation activity entries.
///
/// ADR-58 P2+P3 (F8): the orchestrator resolves these from the resolved
/// blueprint's `StageDef.label` per run and threads them through the recorder,
/// so a renamed gate renders its configured label in live and restored
/// transcripts. The defaults reproduce the pre-blueprint strings exactly
/// ("Reviewer"/"Validator"), keeping every transcript on the default
/// `standard` blueprint byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateLabels {
    /// Label for review-cycle activity entries.
    pub review: String,
    /// Label for validation-cycle activity entries.
    pub validate: String,
}

impl Default for GateLabels {
    fn default() -> Self {
        Self { review: "Reviewer".to_string(), validate: "Validator".to_string() }
    }
}

/// Map a single [`EventKind`] to its standalone transcript entry using the
/// default gate labels ([[`GateLabels::default`]]).
///
/// This is the pure, per-event mapping used by the Stage-2 recorder. Each
/// event maps to its own entry — tool-call *correlation* (merging terminal
/// events into the `Running` entry) is the recorder's job, not this function's.
///
/// Activity strings mirror the desktop's live rendering in
/// `crates/desktop/src/runtime.rs` (`translate_coordinator_event`,
/// `translate_task_lifecycle_event` and the chat thinking lines in
/// `route_event`) so restored transcripts match the live UI text.
///
/// Noise events (tokens, cost, policy verdicts, indexing, shell output, eval,
/// undo, memory, spend caps, observability, LSP, ...) map to `None`.
pub fn transcript_entry_from_event(kind: &EventKind) -> Option<TranscriptEntry> {
    transcript_entry_from_event_with_labels(kind, &GateLabels::default())
}

/// Like [`transcript_entry_from_event`], with caller-supplied gate labels
/// (ADR-58 P2+P3, F8): the orchestrator threads the resolved blueprint's
/// `StageDef.label` for the review/validate gates here so a renamed gate
/// surfaces its configured label in restored transcripts. `core` sits below
/// the config crate, so the labels are a plain parameter — never a config
/// import — and the defaults keep every existing caller byte-identical.
pub fn transcript_entry_from_event_with_labels(
    kind: &EventKind,
    labels: &GateLabels,
) -> Option<TranscriptEntry> {
    match kind {
        // ---- Tool lifecycle: one entry per event (correlation is the
        //      recorder's job). ----
        EventKind::ToolExecutionStarted { tool_name, detail, .. } => {
            Some(TranscriptEntry::ToolCall {
                tool_name: tool_name.clone(),
                detail: detail.clone().unwrap_or_default(),
                status: TranscriptToolStatus::Running,
            })
        }
        EventKind::ToolExecutionFinished { tool_name, success, detail, .. } => {
            Some(TranscriptEntry::ToolCall {
                tool_name: tool_name.clone(),
                detail: detail.clone().unwrap_or_default(),
                status: if *success {
                    TranscriptToolStatus::Completed
                } else {
                    TranscriptToolStatus::Failed
                },
            })
        }
        EventKind::ToolTimeout { tool_name, .. } => Some(TranscriptEntry::ToolCall {
            tool_name: tool_name.clone(),
            detail: String::new(),
            status: TranscriptToolStatus::Failed,
        }),
        EventKind::ApprovalResolved { tool_name, approved } => Some(TranscriptEntry::ToolCall {
            tool_name: tool_name.clone(),
            detail: String::new(),
            status: if *approved {
                TranscriptToolStatus::Allowed
            } else {
                TranscriptToolStatus::Denied
            },
        }),
        EventKind::ApprovalTimeout { tool_name, .. } => Some(TranscriptEntry::ToolCall {
            tool_name: tool_name.clone(),
            detail: String::new(),
            status: TranscriptToolStatus::Cancelled,
        }),

        // ---- Agent output ----
        EventKind::AgentThought { agent_id, content } => {
            Some(TranscriptEntry::Thinking { agent: agent_id.clone(), content: content.clone() })
        }
        EventKind::AssistantMessage { content, .. } => {
            Some(TranscriptEntry::Assistant { content: content.clone() })
        }
        EventKind::ErrorOccurred { message } => {
            Some(TranscriptEntry::Error { content: message.clone() })
        }

        // ---- Context compaction ----
        EventKind::SummarizationStarted { messages_to_summarize, .. } => {
            Some(TranscriptEntry::Summary {
                content: format!("Context compacted: {messages_to_summarize} messages summarized"),
            })
        }
        EventKind::SummarizationCompleted { summary_len, .. } => Some(TranscriptEntry::Summary {
            content: format!("Context compaction complete: {summary_len} chars"),
        }),

        // ---- Orchestration / agent lifecycle activity. Strings mirror the
        //      desktop live rendering (crates/desktop/src/runtime.rs). ----
        EventKind::SubTaskCreated { task_id, description, .. } => Some(TranscriptEntry::Activity {
            agent: "Coordinator".to_string(),
            content: format!("Decomposed task {task_id} into specialist subtask: {description}"),
        }),
        EventKind::MultiAgentModeStarted { task_id, subtask_count, .. } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!(
                    "Decomposed task {task_id} into {subtask_count} specialist subtasks."
                ),
            })
        }
        EventKind::MultiAgentModeCompleted { task_id, cost_usd } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!("Multi-agent task {task_id} completed. Cost: ${cost_usd:.4}."),
            })
        }
        EventKind::SubTaskStarted { task_id, role } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Started subtask {task_id}."),
        }),
        EventKind::SubTaskCompleted { role, outcome, .. } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Completed: {outcome}"),
        }),
        EventKind::SubTaskNeedsRevision { role, reason, .. } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Needs revision: {reason}"),
        }),
        EventKind::SubTaskBlocked { role, on, .. } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Blocked on {on:?}"),
        }),
        EventKind::SubTaskCancelled { role, reason, .. } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Cancelled: {reason}"),
        }),
        EventKind::SubTaskFailed { role, error, .. } => Some(TranscriptEntry::Activity {
            agent: format!("{role:?}"),
            content: format!("Failed: {error}"),
        }),
        EventKind::DelegationDecided { child_id, role, reason, .. } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!("Delegated subtask {child_id} to {role:?}: {reason}"),
            })
        }
        EventKind::RoutingDecided { task_id, role, provider, model, reason } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!(
                    "Routed {role:?} subtask {task_id} to {provider}/{model}: {reason}"
                ),
            })
        }
        EventKind::AgentHandoff { from, to, task_id, rationale } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!("{from:?} handed subtask {task_id} to {to:?}: {rationale}"),
            })
        }
        EventKind::ReviewCycleStarted { task_id, cycle_num } => Some(TranscriptEntry::Activity {
            agent: labels.review.clone(),
            content: format!("Started review cycle {cycle_num} for subtask {task_id}."),
        }),
        EventKind::ReviewCycleCompleted { task_id, cycle_num, verdict } => {
            Some(TranscriptEntry::Activity {
                agent: labels.review.clone(),
                content: format!("Review cycle {cycle_num} for subtask {task_id}: {verdict}"),
            })
        }
        EventKind::ReviewCycleEscalated { task_id, max_cycles } => {
            Some(TranscriptEntry::Activity {
                agent: labels.review.clone(),
                content: format!("Escalated subtask {task_id} after {max_cycles} review cycles."),
            })
        }
        EventKind::ValidationCycleStarted { task_id, cycle_num } => {
            Some(TranscriptEntry::Activity {
                agent: labels.validate.clone(),
                content: format!("Started validation cycle {cycle_num} for subtask {task_id}."),
            })
        }
        EventKind::ValidationEscalated { task_id, max_cycles } => Some(TranscriptEntry::Activity {
            agent: labels.validate.clone(),
            content: format!("Escalated subtask {task_id} after {max_cycles} validation cycles."),
        }),
        EventKind::BudgetDowngradeTriggered { role, from_model, to_model } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!(
                    "Downgraded {role:?} from {from_model} to {to_model} because of the budget."
                ),
            })
        }
        EventKind::OrchestratorCycleDetected { task_id, sequence } => {
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".to_string(),
                content: format!("Detected an orchestration cycle for {task_id}: {sequence:?}"),
            })
        }
        EventKind::TaskStarted { task_id, description } => Some(TranscriptEntry::Activity {
            agent: "Agent".to_string(),
            content: format!("Task started: {description} (id: {task_id})"),
        }),
        EventKind::TaskCompleted { task_id, success } => Some(TranscriptEntry::Activity {
            agent: "Agent".to_string(),
            content: format!("Task completed: {task_id} (success: {success})"),
        }),
        EventKind::TaskFailed { task_id, error } => Some(TranscriptEntry::Activity {
            agent: "Agent".to_string(),
            content: format!("Task {task_id} failed: {error}"),
        }),
        EventKind::AgentStateChanged { task_id, from, to } => Some(TranscriptEntry::Activity {
            agent: "Agent".to_string(),
            content: format!("Task {task_id} changed state from {from:?} to {to:?}."),
        }),

        // ---- Provider retry status (rendered as chat thinking lines). ----
        EventKind::ProviderRetryScheduled { attempt, delay_ms, reason, source, .. } => {
            Some(TranscriptEntry::Activity {
                agent: "Provider".to_string(),
                content: format!(
                    "Provider retry #{attempt} in {}s ({source}): {reason}",
                    delay_ms / 1000
                ),
            })
        }
        EventKind::ProviderRetryRecovered { attempts, elapsed_ms, .. } => {
            Some(TranscriptEntry::Activity {
                agent: "Provider".to_string(),
                content: format!(
                    "Provider recovered after {attempts} attempt(s) in {elapsed_ms}ms"
                ),
            })
        }
        EventKind::ProviderRetryExhausted { attempts, reason, .. } => {
            Some(TranscriptEntry::Activity {
                agent: "Provider".to_string(),
                content: format!(
                    "Provider retries exhausted after {attempts} attempt(s): {reason}"
                ),
            })
        }

        // ---- Everything else is noise for the transcript. ----
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Ulid;
    use crate::types::{AgentId, TaskId};

    fn task_id() -> TaskId {
        TaskId::new()
    }

    /// Representative noise events must not produce transcript entries.
    #[test]
    fn noise_events_map_to_none() {
        let sid = Ulid::new();
        let tid = task_id();
        let noise: Vec<EventKind> = vec![
            EventKind::ToolCalled { tool_name: "fs".into() },
            EventKind::PolicyVerdict { tool_name: "fs".into(), verdict: "deny".into() },
            EventKind::TokenUsed { tokens_in: 1, tokens_out: 2 },
            EventKind::CostIncurred { cost_usd: 0.01 },
            EventKind::SessionSaved,
            EventKind::ShellOutputChunk { chunk: "x".into(), is_stderr: false },
            EventKind::SpendUpdated { session_id: sid, total_usd: 0.1 },
            EventKind::SpendCapExceeded { session_id: sid, current_usd: 2.0, cap_usd: 1.0 },
            EventKind::SpendCapApproaching { current_usd: 0.9, cap_usd: 1.0, pct: 0.9 },
            EventKind::SpendCapExceededSession { session_id: sid, current_usd: 2.0, cap_usd: 1.0 },
            EventKind::SpendCapExceededTask { task_id: tid, current_usd: 2.0, cap_usd: 1.0 },
            EventKind::SpendCapExceededDaily { current_usd: 2.0, cap_usd: 1.0 },
            EventKind::ContextWindowApproaching {
                session_id: sid,
                used_tokens: 100,
                capacity_tokens: 200,
            },
            EventKind::CycleBudgetExceeded { task_id: tid, tool_name: "fs".into(), call_count: 3 },
            EventKind::RateLimitEnforced { provider: "openai".into(), rpm: 10 },
            EventKind::ProviderCallCompleted {
                cost: crate::types::CostInfo {
                    provider: "openai".into(),
                    model: "gpt-4".into(),
                    total_usd: 0.01,
                    tokens_in: 1,
                    tokens_out: 2,
                },
            },
            EventKind::AutoUpdateAvailable {
                current_version: "0.1".into(),
                latest_version: "0.2".into(),
                download_url: "https://example.com".into(),
            },
            EventKind::IndexingStarted { project_id: "p".into(), file_count: 3 },
            EventKind::IndexingProgress {
                project_id: "p".into(),
                files_processed: 1,
                files_total: 3,
            },
            EventKind::IndexingCompleted { project_id: "p".into(), chunk_count: 4, duration_ms: 5 },
            EventKind::EvalStarted { task_id: tid, runner: "r".into() },
            EventKind::EvalCompleted { task_id: tid, exit_code: 0, passed: true },
            EventKind::EvalBenchmarkStarted { suite_name: "s".into(), task_count: 1 },
            EventKind::EvalBenchmarkCompleted {
                suite_name: "s".into(),
                pass_rate: 1.0,
                avg_latency_ms: 2,
                avg_cost_usd: 0.0,
            },
            EventKind::EvalRegressionDetected {
                suite_name: "s".into(),
                metric: "m".into(),
                delta_pct: 0.5,
            },
            EventKind::UndoStashCreated { session_id: sid, stash_ref: "x".into() },
            EventKind::UndoRestored { session_id: sid },
            EventKind::MemoryRetrieved {
                project_id: "p".into(),
                query_hash: "h".into(),
                chunk_count: 1,
                retrieval_ms: 2,
            },
            EventKind::MemoryConflict {
                key: "k".into(),
                agent_role: AgentId::new("coder"),
                previous_agent: None,
            },
            EventKind::StaleVectorsDetected { project_id: "p".into(), stale_count: 1 },
            EventKind::ReindexQueued {
                project_id: "p".into(),
                file_path: "f".into(),
                reason: "r".into(),
            },
            EventKind::EmbeddingModelMismatch {
                stored_version: "1".into(),
                current_version: "2".into(),
            },
            EventKind::EntityExtracted {
                project_id: "p".into(),
                entity_count: 1,
                relation_count: 1,
            },
            EventKind::FactExtracted { project_id: "p".into(), fact_count: 1 },
            EventKind::FactExpired { project_id: "p".into(), fact_id: "f".into() },
            EventKind::ObservabilityTraceStarted { trace_id: "t".into(), service_name: "s".into() },
            EventKind::ObservabilityTraceFinished { trace_id: "t".into(), duration_ms: 1 },
            EventKind::ObservabilityMetricExported {
                metric_name: "m".into(),
                value: 1.0,
                labels: Vec::new(),
            },
            EventKind::ObservabilityExportFailed { exporter: "e".into(), error: "x".into() },
            EventKind::LspServerStarted { project_dir: "d".into(), language: "rust".into() },
            EventKind::LspServerStopped { project_dir: "d".into(), clean: true },
            EventKind::LspServerError { project_dir: "d".into(), error: "x".into() },
            EventKind::OpenAPIDocGenerated { path: "p".into(), endpoint_count: 1 },
            EventKind::SandboxProfileActivated {
                profile: "default".into(),
                tool_name: "fs".into(),
            },
            EventKind::PolicyEvaluated {
                tool_name: "fs".into(),
                verdict: "allow".into(),
                rule_matched: None,
            },
        ];
        for kind in &noise {
            assert_eq!(
                transcript_entry_from_event(kind),
                None,
                "expected None for noise event {kind:?}"
            );
        }
    }

    /// Every ToolCall status mapping produces the correct entry.
    #[test]
    fn tool_call_status_mappings() {
        let started = transcript_entry_from_event(&EventKind::ToolExecutionStarted {
            tool_name: "fs_write".into(),
            input_hash: "abc".into(),
            detail: Some("write main.rs".into()),
        });
        assert_eq!(
            started,
            Some(TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: TranscriptToolStatus::Running,
            })
        );

        let finished_ok = transcript_entry_from_event(&EventKind::ToolExecutionFinished {
            tool_name: "fs_write".into(),
            duration_ms: 5,
            success: true,
            detail: Some("Wrote 42 bytes".into()),
        });
        assert_eq!(
            finished_ok,
            Some(TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "Wrote 42 bytes".into(),
                status: TranscriptToolStatus::Completed,
            })
        );

        let finished_err = transcript_entry_from_event(&EventKind::ToolExecutionFinished {
            tool_name: "shell".into(),
            duration_ms: 5,
            success: false,
            detail: Some("exit code 1".into()),
        });
        assert_eq!(
            finished_err,
            Some(TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: "exit code 1".into(),
                status: TranscriptToolStatus::Failed,
            })
        );

        let timeout = transcript_entry_from_event(&EventKind::ToolTimeout {
            tool_name: "shell".into(),
            timeout_secs: 30,
        });
        assert_eq!(
            timeout,
            Some(TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Failed,
            })
        );

        let approved = transcript_entry_from_event(&EventKind::ApprovalResolved {
            tool_name: "shell".into(),
            approved: true,
        });
        assert_eq!(
            approved,
            Some(TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Allowed,
            })
        );

        let denied = transcript_entry_from_event(&EventKind::ApprovalResolved {
            tool_name: "shell".into(),
            approved: false,
        });
        assert_eq!(
            denied,
            Some(TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Denied,
            })
        );

        let cancelled = transcript_entry_from_event(&EventKind::ApprovalTimeout {
            tool_name: "shell".into(),
            timeout_secs: 60,
        });
        assert_eq!(
            cancelled,
            Some(TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Cancelled,
            })
        );
    }

    /// Thinking, Assistant and Error mappings.
    #[test]
    fn assistant_thinking_error_mappings() {
        let thinking = transcript_entry_from_event(&EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "step one".into(),
        });
        assert_eq!(
            thinking,
            Some(TranscriptEntry::Thinking { agent: "coder".into(), content: "step one".into() })
        );

        let assistant = transcript_entry_from_event(&EventKind::AssistantMessage {
            task_id: task_id(),
            content: "done".into(),
        });
        assert_eq!(assistant, Some(TranscriptEntry::Assistant { content: "done".into() }));

        let error =
            transcript_entry_from_event(&EventKind::ErrorOccurred { message: "boom".into() });
        assert_eq!(error, Some(TranscriptEntry::Error { content: "boom".into() }));
    }

    /// Summarization events map to Summary entries with the live UI wording.
    #[test]
    fn summary_mappings() {
        let sid = Ulid::new();
        let started = transcript_entry_from_event(&EventKind::SummarizationStarted {
            session_id: sid,
            messages_to_summarize: 42,
        });
        assert_eq!(
            started,
            Some(TranscriptEntry::Summary {
                content: "Context compacted: 42 messages summarized".into(),
            })
        );

        let completed = transcript_entry_from_event(&EventKind::SummarizationCompleted {
            session_id: sid,
            summary_len: 1234,
        });
        assert_eq!(
            completed,
            Some(TranscriptEntry::Summary {
                content: "Context compaction complete: 1234 chars".into(),
            })
        );
    }

    /// Representative activity mappings mirror the desktop's live strings.
    #[test]
    fn activity_mappings_mirror_desktop_strings() {
        let tid = task_id();

        let subtask = transcript_entry_from_event(&EventKind::SubTaskCreated {
            task_id: tid,
            role: AgentId::new("coder"),
            description: "implement the fix".into(),
        });
        assert_eq!(
            subtask,
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: format!(
                    "Decomposed task {tid} into specialist subtask: implement the fix"
                ),
            })
        );

        let delegated = transcript_entry_from_event(&EventKind::DelegationDecided {
            parent_id: tid,
            child_id: tid,
            role: AgentId::new("reviewer"),
            reason: "needs review".into(),
        });
        assert_eq!(
            delegated,
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: format!("Delegated subtask {tid} to reviewer: needs review"),
            })
        );

        let routed = transcript_entry_from_event(&EventKind::RoutingDecided {
            task_id: tid,
            role: AgentId::new("coder"),
            provider: "openrouter".into(),
            model: "example/model".into(),
            reason: "configured".into(),
        });
        assert_eq!(
            routed,
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: format!(
                    "Routed coder subtask {tid} to openrouter/example/model: configured"
                ),
            })
        );

        let review = transcript_entry_from_event(&EventKind::ReviewCycleStarted {
            task_id: tid,
            cycle_num: 2,
        });
        assert_eq!(
            review,
            Some(TranscriptEntry::Activity {
                agent: "Reviewer".into(),
                content: format!("Started review cycle 2 for subtask {tid}."),
            })
        );

        let task_started = transcript_entry_from_event(&EventKind::TaskStarted {
            task_id: tid,
            description: "apply the fix".into(),
        });
        assert_eq!(
            task_started,
            Some(TranscriptEntry::Activity {
                agent: "Agent".into(),
                content: format!("Task started: apply the fix (id: {tid})"),
            })
        );

        let task_completed =
            transcript_entry_from_event(&EventKind::TaskCompleted { task_id: tid, success: true });
        assert_eq!(
            task_completed,
            Some(TranscriptEntry::Activity {
                agent: "Agent".into(),
                content: format!("Task completed: {tid} (success: true)"),
            })
        );

        let multi_started = transcript_entry_from_event(&EventKind::MultiAgentModeStarted {
            task_id: tid,
            subtask_count: 3,
            plan_id: None,
        });
        assert_eq!(
            multi_started,
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: format!("Decomposed task {tid} into 3 specialist subtasks."),
            })
        );

        let retry = transcript_entry_from_event(&EventKind::ProviderRetryScheduled {
            session_id: Ulid::new(),
            task_id: tid,
            attempt: 2,
            delay_ms: 3000,
            reason: "429".into(),
            source: "openai".into(),
            retry_after_ms: Some(60_000),
        });
        assert_eq!(
            retry,
            Some(TranscriptEntry::Activity {
                agent: "Provider".into(),
                content: "Provider retry #2 in 3s (openai): 429".into(),
            })
        );
    }

    /// Caller-supplied gate labels override the defaults in the review and
    /// validation activity entries (ADR-58 P2+P3, F8). All other events are
    /// unaffected by the label parameter.
    #[test]
    fn custom_gate_labels_override_review_and_validate_entries() {
        let labels = GateLabels { review: "QA Reviewer".into(), validate: "QA Verifier".into() };
        let tid = task_id();

        let review = transcript_entry_from_event_with_labels(
            &EventKind::ReviewCycleStarted { task_id: tid, cycle_num: 2 },
            &labels,
        );
        assert_eq!(
            review,
            Some(TranscriptEntry::Activity {
                agent: "QA Reviewer".into(),
                content: format!("Started review cycle 2 for subtask {tid}."),
            })
        );

        let reviewed = transcript_entry_from_event_with_labels(
            &EventKind::ReviewCycleCompleted { task_id: tid, cycle_num: 2, verdict: "pass".into() },
            &labels,
        );
        assert_eq!(
            reviewed,
            Some(TranscriptEntry::Activity {
                agent: "QA Reviewer".into(),
                content: format!("Review cycle 2 for subtask {tid}: pass"),
            })
        );

        let escalated = transcript_entry_from_event_with_labels(
            &EventKind::ReviewCycleEscalated { task_id: tid, max_cycles: 3 },
            &labels,
        );
        assert_eq!(
            escalated,
            Some(TranscriptEntry::Activity {
                agent: "QA Reviewer".into(),
                content: format!("Escalated subtask {tid} after 3 review cycles."),
            })
        );

        let validated = transcript_entry_from_event_with_labels(
            &EventKind::ValidationCycleStarted { task_id: tid, cycle_num: 1 },
            &labels,
        );
        assert_eq!(
            validated,
            Some(TranscriptEntry::Activity {
                agent: "QA Verifier".into(),
                content: format!("Started validation cycle 1 for subtask {tid}."),
            })
        );

        let validation_escalated = transcript_entry_from_event_with_labels(
            &EventKind::ValidationEscalated { task_id: tid, max_cycles: 2 },
            &labels,
        );
        assert_eq!(
            validation_escalated,
            Some(TranscriptEntry::Activity {
                agent: "QA Verifier".into(),
                content: format!("Escalated subtask {tid} after 2 validation cycles."),
            })
        );

        // Non-gate activity entries are untouched by the label parameter.
        let routed = transcript_entry_from_event_with_labels(
            &EventKind::RoutingDecided {
                task_id: tid,
                role: AgentId::new("coder"),
                provider: "openrouter".into(),
                model: "example/model".into(),
                reason: "configured".into(),
            },
            &labels,
        );
        assert_eq!(
            routed,
            Some(TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: format!(
                    "Routed coder subtask {tid} to openrouter/example/model: configured"
                ),
            })
        );
    }

    /// Every `TranscriptEntry` variant survives a serde JSON round-trip.
    #[test]
    fn serde_round_trip_all_variants() {
        let entries = vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Assistant { content: "on it".into() },
            TranscriptEntry::Thinking { agent: "coder".into(), content: "hmm".into() },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: TranscriptToolStatus::Running,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Denied,
            },
            TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: "Delegated subtask X to coder: go".into(),
            },
            TranscriptEntry::Error { content: "boom".into() },
            TranscriptEntry::Summary { content: "compacted".into() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: true,
                files: vec!["main.rs".into(), "lib.rs".into()],
                project_root: Some("/tmp/proj".into()),
            },
        ];
        for entry in &entries {
            let json = serde_json::to_string(entry).expect("entry serializes");
            let back: TranscriptEntry = serde_json::from_str(&json).expect("entry deserializes");
            assert_eq!(entry, &back, "round-trip mismatch for {json}");
        }
    }
}
