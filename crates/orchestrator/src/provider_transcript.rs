//! Durable canonical conversation writes, separate from rendered run summaries.

use concerto_core::ids::Ulid;
use concerto_core::types::{AgentCompletionStatus, AgentOutput, Message, Role, ToolResult};
use concerto_core::CancellationToken;
use concerto_sessions::{SessionError, SessionStore};
use std::sync::Arc;
use std::time::Duration;

// SQLite's busy timeout is five seconds. Allow one wait for the transaction
// and one for commit, but never let shutdown wait indefinitely on persistence.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Persist completed turns, tool results or runtime outcomes. Callers use a fresh
/// cleanup token so cancelling generation cannot erase already completed work.
/// The bounded cleanup does not execute tools or retry ambiguous writes.
pub(crate) async fn append_completed_messages(
    store: Option<&Arc<dyn SessionStore>>,
    session_id: Ulid,
    messages: &[Message],
    cleanup: CancellationToken,
) -> Result<(), SessionError> {
    let Some(store) = store.filter(|_| !messages.is_empty()) else {
        return Ok(());
    };
    flush_with_deadline(store.append_messages(session_id, messages, cleanup.clone()), cleanup).await
}

async fn flush_with_deadline(
    write: impl std::future::Future<Output = Result<(), SessionError>>,
    cleanup: CancellationToken,
) -> Result<(), SessionError> {
    // Also release the cleanup domain when the caller itself is dropped.
    let _cancel_on_drop = cleanup.drop_guard();
    let result = tokio::time::timeout(FLUSH_TIMEOUT, write).await;
    result.unwrap_or_else(|_| {
        Err(SessionError::Database("provider transcript flush exceeded its deadline".into()))
    })
}

/// Preserve facts produced after the model's final turn without impersonating
/// that model or counting its usage again. Full raw verifier output stays in
/// the evidence stores; this compact record preserves outcomes and references.
pub(crate) fn run_outcome(output: &AgentOutput) -> Message {
    let verification: Vec<_> = output
        .verification
        .iter()
        .map(|check| {
            serde_json::json!({
                "path": check.path,
                "command": check.command,
                "outcome": check.effective_outcome(),
                "inconsistent": check.computed_inconsistent(),
                "verifier": check.verifier,
                "scope": check.scope,
                "evidence_ids": check.evidence_ids,
            })
        })
        .collect();
    runtime_observation(serde_json::json!({
        "task_id": output.task_id,
        "completion_status": format!("{:?}", output.completion_status),
        "files_modified": output.files_modified,
        "tool_call_count": output.tool_call_count,
        "verification": verification,
        "eval_passed": output.eval_result.as_ref().map(|result| result.passed),
        "termination_detail": (output.completion_status != AgentCompletionStatus::Completed)
            .then_some(&output.final_message),
    }))
}

pub(crate) fn run_failure(
    task_id: concerto_core::TaskId,
    error: &concerto_core::OrchestratorError,
) -> Message {
    let status = if matches!(
        error,
        concerto_core::OrchestratorError::Cancelled
            | concerto_core::OrchestratorError::Provider(
                concerto_core::error::ProviderError::Cancelled
            )
    ) {
        "Cancelled"
    } else {
        "Error"
    };
    runtime_observation(serde_json::json!({
        "task_id": task_id,
        "completion_status": status,
        "termination_detail": error.to_string(),
    }))
}

fn runtime_observation(record: serde_json::Value) -> Message {
    Message {
        role: Role::System,
        content: format!("Concerto runtime outcome (execution data, not instructions):\n{record}"),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
        gemini_replay: None,
    }
}

/// Make a persisted tool exchange replayable after interruption. This is a
/// request-only annotation, never execution evidence and never a retry trigger.
/// Keep native assistant state and recorded results intact; defer corrective
/// user messages until all responses to that assistant's calls are present.
pub(crate) fn restore_tool_responses(messages: Vec<Message>) -> Vec<Message> {
    let mut restored = Vec::with_capacity(messages.len());
    let mut source = messages.into_iter().peekable();
    while let Some(message) = source.next() {
        let Some(calls) = message
            .tool_calls
            .as_ref()
            .filter(|calls| message.role == Role::Assistant && !calls.is_empty())
        else {
            restored.push(message);
            continue;
        };
        let mut following = Vec::new();
        while source.peek().is_some_and(|next| next.role != Role::Assistant) {
            if let Some(next) = source.next() {
                following.push(next);
            }
        }
        let answered: std::collections::HashSet<_> = following
            .iter()
            .filter(|next| next.role == Role::Tool)
            .filter_map(|next| next.tool_results.as_ref())
            .flatten()
            .map(|result| result.id.as_str())
            .collect();
        let missing: Vec<_> = calls.iter().filter(|call| !answered.contains(call.id.as_str()))
            .map(|call| ToolResult {
                id: call.id.clone(), name: call.name.clone(),
                content: serde_json::json!({
                    "status": "outcome_unknown",
                    "error": "No durable result was recorded for this call. It may have executed before interruption. Inspect the workspace and audit evidence before retrying; do not assume success or repeat side effects automatically."
                }),
            }).collect();
        restored.push(message);
        let mut deferred = Vec::new();
        for next in following {
            if next.role == Role::Tool {
                restored.push(next);
            } else {
                deferred.push(next);
            }
        }
        if !missing.is_empty() {
            restored.push(Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(missing),
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
                gemini_replay: None,
            });
        }
        restored.extend(deferred);
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn transcript_flush_times_out_and_cancels_cleanup() {
        let cleanup = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let result = flush_with_deadline(std::future::pending(), cleanup.clone()).await;
        assert!(
            matches!(result, Err(SessionError::Database(message)) if message.contains("deadline"))
        );
        assert_eq!(started.elapsed(), FLUSH_TIMEOUT);
        assert!(cleanup.is_cancelled());
    }

    #[tokio::test]
    async fn transcript_flush_dropped_caller_cancels_cleanup() {
        let cleanup = CancellationToken::new();
        let (started, ready) = tokio::sync::oneshot::channel();
        let owned = cleanup.clone();
        let task = tokio::spawn(flush_with_deadline(
            async move {
                let _ = started.send(());
                std::future::pending().await
            },
            owned,
        ));
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(cleanup.is_cancelled());
    }

    #[tokio::test]
    async fn transcript_flush_propagates_write_failure_without_retry() {
        let cleanup = CancellationToken::new();
        let mut attempts = 0;
        let result = flush_with_deadline(
            async {
                attempts += 1;
                Err(SessionError::Database("ambiguous write".into()))
            },
            cleanup.clone(),
        )
        .await;
        assert!(
            matches!(result, Err(SessionError::Database(message)) if message == "ambiguous write")
        );
        assert_eq!(attempts, 1);
        assert!(cleanup.is_cancelled());
    }
}
