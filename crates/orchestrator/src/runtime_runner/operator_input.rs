//! Answers resume a scoped user question; tool approvals retain their own channel.

use concerto_core::{Decision, DecisionCategory, DecisionId, WorkingMemorySnapshot};

use crate::checkpoint::GraphCheckpoint;

pub(super) const ANSWER_DECISION: &str = "Operator answered the pending question";

/// The caller validates session and project before consuming any answer.
pub(super) fn consume(checkpoint: &mut GraphCheckpoint, input: &str) -> Option<String> {
    if checkpoint.pending_approval.is_some()
        || super::is_resume_request(input)
        || input.trim().is_empty()
        || input.trim() == checkpoint.objective.trim()
    {
        return None;
    }
    let question = checkpoint.pending_user_input.take()?;
    let now = time::OffsetDateTime::now_utc();
    let memory = checkpoint.working_memory.get_or_insert_with(|| WorkingMemorySnapshot {
        id: concerto_core::ids::Ulid::new(),
        session_id: checkpoint.session_id,
        decisions: Vec::new(),
        task_tree: Vec::new(),
        created_at: now,
    });
    let answer = if input.chars().count() > 16_000 {
        format!(
            "{} [context truncated; full answer is in the user transcript]",
            input.chars().take(16_000).collect::<String>()
        )
    } else {
        input.to_string()
    };
    memory.decisions.push(Decision {
        id: DecisionId(concerto_core::ids::Ulid::new()),
        session_id: checkpoint.session_id,
        task_id: Some(checkpoint.root_task_id),
        what: ANSWER_DECISION.to_string(),
        why: format!("User response to: {}", question.chars().take(4_000).collect::<String>()),
        outcome: Some(answer),
        category: DecisionCategory::Other,
        confidence: 1.0,
        superseded_by: None,
        created_at: now,
    });
    Some(question)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{self, CheckpointContext, CheckpointScope, CheckpointStage};
    use std::collections::HashMap;

    fn checkpoint() -> GraphCheckpoint {
        let session = concerto_core::types::SessionContext::new(
            concerto_core::ids::Ulid::new(),
            std::path::PathBuf::from("."),
        );
        let context = concerto_core::types::AgentContext::new(session);
        let scope = CheckpointScope {
            run_id: concerto_core::ids::Ulid::new(),
            session_id: context.session.session_id,
            root_task_id: concerto_core::types::TaskId::new(),
            project_id: context.session.project_id.0.clone(),
            objective: "Build the report".into(),
            objective_hash: blake3::hash(b"Build the report").to_hex().to_string(),
            source_revision: None,
            sequence_num: 9,
        };
        let mut checkpoint = checkpoint::build_checkpoint(
            &scope,
            CheckpointStage::Executing,
            None,
            &context.working_memory,
            &crate::graph::TaskGraph::new(),
            &HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &CheckpointContext::default(),
        );
        checkpoint.pending_user_input = Some("Which format?".into());
        checkpoint
    }

    #[test]
    fn answer_is_durable_without_changing_the_original_task(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut checkpoint = checkpoint();
        let identity =
            (checkpoint.run_id, checkpoint.root_task_id, checkpoint.objective_hash.clone());
        assert!(consume(&mut checkpoint, "Continue").is_none());
        assert_eq!(checkpoint.pending_user_input.as_deref(), Some("Which format?"));
        assert_eq!(consume(&mut checkpoint, "Use Markdown"), Some("Which format?".into()));
        assert!(checkpoint.pending_user_input.is_none());
        let restored = GraphCheckpoint::from_json(&serde_json::to_string(&checkpoint)?)?;
        assert_eq!((restored.run_id, restored.root_task_id, restored.objective_hash), identity);
        assert_eq!(restored.objective, "Build the report");
        let answer = restored
            .working_memory
            .and_then(|memory| memory.decisions.into_iter().last())
            .ok_or_else(|| std::io::Error::other("answer was not preserved"))?;
        assert_eq!(answer.what, ANSWER_DECISION);
        assert_eq!(answer.outcome.as_deref(), Some("Use Markdown"));
        Ok(())
    }

    #[test]
    fn ordinary_text_cannot_answer_a_pending_tool_approval() {
        let mut checkpoint = checkpoint();
        checkpoint.pending_approval = Some(concerto_core::types::PendingApprovalInfo {
            tool_name: "shell".into(),
            detail: "run command".into(),
            input_hash: "hash".into(),
            correlation_id: "approval-id".into(),
            timeout_secs: 0,
        });
        assert!(consume(&mut checkpoint, "yes").is_none());
        assert_eq!(checkpoint.pending_user_input.as_deref(), Some("Which format?"));
        assert_eq!(
            checkpoint.pending_approval.as_ref().map(|approval| approval.correlation_id.as_str()),
            Some("approval-id")
        );
    }
}
