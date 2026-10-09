//! Truthful specialist termination over the existing outcome/ledger contracts.
//!
//! This projection carries no raw tool arguments, credentials, or executable
//! replay instructions. It is serialized in a NeedsRevision reason, which the
//! existing graph checkpoint already preserves. It records graceful settlement,
//! not an exact process snapshot or a replacement for the harness write journal.

use std::collections::BTreeMap;

use concerto_core::types::{AgentContext, AgentOutcome, SubTask};
use concerto_core::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const CONTINUATION_MARKER: &str = "Specialist continuation v1: ";
const MAX_RECORDED_CALLS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordedCall {
    pub call_id: String,
    pub tool: String,
    pub operation_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutstandingCall {
    pub call_id: String,
    pub tool: String,
    pub code: String,
    pub operation_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionContinuation {
    pub schema_version: u32,
    pub task_id: String,
    pub agent_id: String,
    pub stop_code: String,
    pub turns: u32,
    pub successful_calls: Vec<RecordedCall>,
    pub outstanding_calls: Vec<OutstandingCall>,
    pub omitted_successful_calls: usize,
    #[serde(default)]
    pub omitted_outstanding_calls: usize,
}

/// Runtime-observed progress, never inferred from a model's completion prose.
#[derive(Debug, Default)]
pub(crate) struct ExecutionState {
    turns: u32,
    final_answer: bool,
    unavailable_executor: bool,
    successful_calls: Vec<RecordedCall>,
    omitted_successful_calls: usize,
    omitted_outstanding_calls: usize,
    failures: BTreeMap<String, OutstandingCall>,
    rejected_arguments: BTreeMap<String, OutstandingCall>,
}

impl ExecutionState {
    /// Restore only this task's latest runtime settlement. A new dispatch
    /// gets a fresh turn budget, but unresolved operations do not disappear
    /// merely because the Coordinator continued the same task.
    pub fn for_task(task: &SubTask, context: &AgentContext) -> Self {
        let note = context.previous_results.iter().rev().find(|result| result.task_id == task.id)
            .and_then(|result| match &result.outcome {
                AgentOutcome::NeedsRevision { reason } => continuation(reason),
                _ => None,
            })
            .filter(|note| note.task_id == task.id.to_string());
        let Some(note) = note else { return Self::default() };
        let mut state = Self {
            omitted_successful_calls: note.omitted_successful_calls,
            omitted_outstanding_calls: note.omitted_outstanding_calls,
            ..Self::default()
        };
        for call in note.successful_calls {
            if state.successful_calls.len() == MAX_RECORDED_CALLS {
                state.successful_calls.remove(0);
                state.omitted_successful_calls = state.omitted_successful_calls.saturating_add(1);
            }
            state.successful_calls.push(call);
        }
        for call in note.outstanding_calls {
            let calls = if call.code == "invalid_tool_arguments" {
                &mut state.rejected_arguments
            } else {
                &mut state.failures
            };
            if calls.len() >= MAX_RECORDED_CALLS && !calls.contains_key(&call.operation_hash) {
                state.omitted_outstanding_calls = state.omitted_outstanding_calls.saturating_add(1);
            } else {
                calls.insert(call.operation_hash.clone(), call);
            }
        }
        state
    }

    pub fn begin_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    pub fn final_answer(&mut self) {
        self.final_answer = true;
    }

    pub fn unavailable_executor(&mut self) {
        self.unavailable_executor = true;
    }

    pub fn rejected(&mut self, call_id: &str, tool: &str, args: &Value) {
        let key = if has_resource(args) { operation_hash(tool, args) } else { tool.to_owned() };
        if self.rejected_arguments.len() >= MAX_RECORDED_CALLS
            && !self.rejected_arguments.contains_key(&key)
        {
            self.omitted_outstanding_calls = self.omitted_outstanding_calls.saturating_add(1);
            return;
        }
        self.rejected_arguments.insert(
            key.clone(),
            OutstandingCall {
                call_id: bounded(call_id),
                tool: bounded(tool),
                code: "invalid_tool_arguments".into(),
                operation_hash: key,
            },
        );
    }

    pub fn failed(&mut self, call_id: &str, tool: &str, args: &Value, code: &str) {
        let operation_hash = operation_hash(tool, args);
        // Valid arguments repaired a prior schema rejection, even when the
        // execution itself failed. Its actual failure replaces that rejection.
        self.rejected_arguments.remove(tool);
        self.rejected_arguments.remove(&operation_hash);
        if self.failures.len() >= MAX_RECORDED_CALLS && !self.failures.contains_key(&operation_hash) {
            self.omitted_outstanding_calls = self.omitted_outstanding_calls.saturating_add(1);
            return;
        }
        self.failures.insert(
            operation_hash.clone(),
            OutstandingCall {
                call_id: bounded(call_id),
                tool: bounded(tool),
                code: bounded(code),
                operation_hash,
            },
        );
    }

    pub fn succeeded(&mut self, call_id: &str, tool: &str, args: &Value) {
        let operation_hash = operation_hash(tool, args);
        self.failures.remove(&operation_hash);
        self.rejected_arguments.remove(tool);
        self.rejected_arguments.remove(&operation_hash);
        if self.successful_calls.len() == MAX_RECORDED_CALLS {
            self.successful_calls.remove(0);
            self.omitted_successful_calls = self.omitted_successful_calls.saturating_add(1);
        }
        self.successful_calls.push(RecordedCall {
            call_id: bounded(call_id),
            tool: bounded(tool),
            operation_hash,
        });
    }

    /// A final model answer is necessary, but unresolved runtime failures or
    /// a missing executor prevent it from becoming a successful settlement.
    pub fn outcome(&self, task: &SubTask, summary: &str) -> AgentOutcome {
        let (code, reason) = if self.unavailable_executor {
            ("specialist-no-executor", "requested tool work has no execution backend")
        } else if !self.final_answer {
            ("specialist-turn-limit", "the execution limit was reached before a final answer")
        } else if !self.failures.is_empty()
            || !self.rejected_arguments.is_empty()
            || self.omitted_outstanding_calls > 0
        {
            ("specialist-unresolved-tools", "tool operations remain unresolved")
        } else if summary.trim().is_empty() {
            ("specialist-empty-completion", "the model returned no completion summary")
        } else {
            return AgentOutcome::Success;
        };
        let continuation = ExecutionContinuation {
            schema_version: 1,
            task_id: task.id.to_string(),
            agent_id: task.role.to_string(),
            stop_code: code.into(),
            turns: self.turns,
            successful_calls: self.successful_calls.clone(),
            outstanding_calls: self
                .failures
                .values()
                .chain(self.rejected_arguments.values())
                .cloned()
                .collect(),
            omitted_successful_calls: self.omitted_successful_calls,
            omitted_outstanding_calls: self.omitted_outstanding_calls,
        };
        let detail = serde_json::to_string(&continuation).unwrap_or_default();
        AgentOutcome::NeedsRevision {
            reason: format!("{reason}. {CONTINUATION_MARKER}{detail}"),
        }
    }
}

/// Consume the existing diagnosis; do not create a competing ToolFailure
/// schema or classify native failures from their formatted Display text here.
pub(crate) fn tool_failure_payload(error: &ToolError) -> Value {
    let diagnosis = crate::failure_diagnosis::diagnose_tool(error);
    let recovery = if diagnosis.retryable && diagnosis.same_agent_viable {
        "correct_and_retry"
    } else if diagnosis.replan_required || diagnosis.same_agent_viable {
        "change_action"
    } else {
        "return_to_coordinator"
    };
    serde_json::json!({
        "error": "tool_execution_failed",
        "code": diagnosis.code,
        "message": diagnosis.evidence,
        "retryable": diagnosis.retryable,
        "recovery": recovery,
        "diagnosis": diagnosis.tool_summary(),
    })
}

/// A retry repairs only the same native operation on the same resource. File
/// contents may change as part of correction; a successful sibling operation
/// never erases another file's error. Opaque tools require identical arguments.
fn operation_hash(tool: &str, args: &Value) -> String {
    let operation = match tool {
        "filesystem" | "write_file" | "edit_file" | "delete_file" | "create_file"
        | "modify_file" | "write" if has_resource(args) => serde_json::json!({
            "tool": tool,
            "operation": args.get("operation"),
            "path": args.get("path"),
            "file_path": args.get("file_path"),
            "file": args.get("file"),
            "source": args.get("source"),
            "target": args.get("target"),
            "paths": args.get("paths"),
            "destination": args.get("destination"),
        }),
        _ => serde_json::json!({ "tool": tool, "args": args }),
    };
    blake3::hash(operation.to_string().as_bytes()).to_hex().to_string()
}

fn has_resource(args: &Value) -> bool {
    ["path", "file_path", "file", "source", "target", "paths", "destination"]
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_null()))
}

fn bounded(value: &str) -> String {
    value.chars().take(256).collect()
}

pub(crate) fn continuation(reason: &str) -> Option<ExecutionContinuation> {
    let (_, raw) = reason.split_once(CONTINUATION_MARKER)?;
    let note: ExecutionContinuation = serde_json::from_str(raw).ok()?;
    (note.schema_version == 1).then_some(note)
}

/// Display the actionable reason; keep machine progress in the persisted
/// result and model handoff rather than dumping its JSON into chat panels.
pub(crate) fn display_reason(reason: &str) -> String {
    reason.split_once(CONTINUATION_MARKER).map_or(reason, |(text, _)| text).trim().to_owned()
}

pub(crate) fn unfinished(outcome: &AgentOutcome) -> bool {
    matches!(outcome, AgentOutcome::NeedsRevision { reason } if continuation(reason).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::AgentId;

    fn task() -> SubTask {
        SubTask::new(Ulid::new(), AgentId::new("coder"), "Repair a file")
    }

    #[test]
    fn exhausted_execution_preserves_successes_without_claiming_completion() {
        let mut state = ExecutionState::default();
        state.begin_turn();
        state.succeeded("write-1", "filesystem", &serde_json::json!({
            "operation": "write", "path": "a.rs", "content": "changed"
        }));
        let AgentOutcome::NeedsRevision { reason } = state.outcome(&task(), "working") else {
            panic!("execution without a final answer must remain unfinished");
        };
        let note = continuation(&reason).expect("continuation survives serialization");
        assert_eq!(note.stop_code, "specialist-turn-limit");
        assert_eq!(note.successful_calls[0].call_id, "write-1");
        assert!(!reason.contains("changed"));
    }

    #[test]
    fn successful_sibling_cannot_resolve_another_resources_failure() {
        let mut state = ExecutionState::default();
        state.failed("bad", "filesystem", &serde_json::json!({
            "operation": "write", "path": "a.rs"
        }), "tool-failed");
        state.succeeded("other", "filesystem", &serde_json::json!({
            "operation": "write", "path": "b.rs"
        }));
        state.final_answer();
        assert!(matches!(state.outcome(&task(), "done"), AgentOutcome::NeedsRevision { .. }));
        state.succeeded("fixed", "filesystem", &serde_json::json!({
            "operation": "write", "path": "a.rs", "content": "repaired"
        }));
        assert_eq!(state.outcome(&task(), "done"), AgentOutcome::Success);
    }

    #[test]
    fn argument_repair_requires_a_valid_operation_from_the_rejected_tool() {
        let mut state = ExecutionState::default();
        state.rejected("bad", "filesystem", &Value::Null);
        state.succeeded("other", "shell", &serde_json::json!({"command": "true"}));
        state.final_answer();
        assert!(matches!(state.outcome(&task(), "done"), AgentOutcome::NeedsRevision { .. }));
        state.succeeded("fixed", "filesystem", &serde_json::json!({
            "operation": "write", "path": "a.rs"
        }));
        assert_eq!(state.outcome(&task(), "done"), AgentOutcome::Success);
    }

    #[test]
    fn policy_denial_has_no_unconditional_retry_instruction() {
        let payload = tool_failure_payload(&ToolError::PolicyDenied { rule: "read-only".into() });
        assert_eq!(payload["code"], "policy-denied");
        assert_eq!(payload["retryable"], false);
        assert_eq!(payload["recovery"], "change_action");
    }

    #[test]
    fn empty_answer_and_missing_executor_are_not_success() {
        let mut state = ExecutionState::default();
        state.final_answer();
        assert!(matches!(state.outcome(&task(), " "), AgentOutcome::NeedsRevision { .. }));
        state.unavailable_executor();
        assert!(matches!(state.outcome(&task(), "done"), AgentOutcome::NeedsRevision { .. }));
    }

    #[test]
    fn progress_remains_bounded_and_omitted_failures_prevent_success() {
        let mut state = ExecutionState::default();
        for index in 0..(MAX_RECORDED_CALLS + 1) {
            let args = serde_json::json!({"path": format!("{index}.rs")});
            state.succeeded(&index.to_string(), "write_file", &args);
            state.failed(&index.to_string(), "filesystem", &args, "tool-failed");
        }
        state.final_answer();
        let AgentOutcome::NeedsRevision { reason } = state.outcome(&task(), "done") else {
            panic!("record limits cannot erase outstanding work");
        };
        let note = continuation(&reason).unwrap();
        assert_eq!(note.successful_calls.len(), MAX_RECORDED_CALLS);
        assert_eq!(note.outstanding_calls.len(), MAX_RECORDED_CALLS);
        assert_eq!(note.omitted_successful_calls, 1);
        assert_eq!(note.omitted_outstanding_calls, 1);
        for index in 0..(MAX_RECORDED_CALLS + 1) {
            state.succeeded("fixed", "filesystem", &serde_json::json!({"path": format!("{index}.rs")}));
        }
        assert!(matches!(state.outcome(&task(), "done"), AgentOutcome::NeedsRevision { .. }));
    }

    #[test]
    fn resource_scoped_argument_rejection_survives_sibling_success() {
        let mut state = ExecutionState::default();
        state.rejected("invalid", "write_file", &serde_json::json!({"path": "a.rs"}));
        state.succeeded("sibling", "write_file", &serde_json::json!({"path": "b.rs"}));
        state.final_answer();
        assert!(matches!(state.outcome(&task(), "done"), AgentOutcome::NeedsRevision { .. }));
        state.succeeded("fixed", "write_file", &serde_json::json!({"path": "a.rs", "content": "fixed"}));
        assert_eq!(state.outcome(&task(), "done"), AgentOutcome::Success);
    }
}
