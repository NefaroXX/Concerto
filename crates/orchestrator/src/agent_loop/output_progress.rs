//! Track pure run-output progress across continuation rounds (NORM S11).
//!
//! Every item here takes `AgentOutput` / `ToolExecutionSummary` values and
//! returns derived values: the progress fingerprint the no-convergence check
//! compares, the cross-round accumulator merge, the audited file-change count
//! that reconciles the ActionRequired completion check with the audited write
//! path, and the auto-continuation instruction appended to the conversation.
//! No event bus, checkpoint, policy, session store, or async I/O appears in
//! any body — call sites in `agent_loop.rs` pass all context explicitly.

use concerto_core::types::{AgentOutput, ToolExecutionSummary};
/// Snapshot of progress used to detect non-convergence across continuation
/// rounds (two identical fingerprints in a row ⇒ no real forward motion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProgressFingerprint {
    pub(super) files_modified: usize,
    pub(super) tool_call_count: u32,
    pub(super) passed_verifications: usize,
    pub(super) failed_verifications: usize,
    pub(super) final_message_len: usize,
}

impl ProgressFingerprint {
    pub(super) fn from_output(output: &AgentOutput) -> Self {
        Self {
            files_modified: output.files_modified.len(),
            tool_call_count: output.tool_call_count,
            passed_verifications: output.verification.iter().filter(|v| v.passed).count(),
            failed_verifications: output.verification.iter().filter(|v| !v.passed).count(),
            final_message_len: output.final_message.len(),
        }
    }
}

pub(super) fn merge_run_progress(accumulated: &mut AgentOutput, latest: &AgentOutput) {
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

/// Number of successful, audited file-changing tool calls recorded in
/// `output.tool_events` (completion-fix counter reconciliation, 2026-09-09).
///
/// The ActionRequired completion check consults the in-loop counter
/// (`file_changing_tool_count`), which is SCOPED to one continuation round
/// and keys off the raw arguments' tool name / `operation` field — while the
/// audited write path (tool summaries, tool-fact rows) accumulates across
/// rounds and records what the tool normalized. A run that wrote a file in
/// an earlier round then ended Blocked on "no file-changing tool call
/// succeeded" reported a disagreement between the two views (live smoke
/// evidence). This reconciles the check with the audited write path: the
/// cross-round audit counts as file-changing evidence too.
pub(super) fn audited_file_changes(output: &AgentOutput) -> u32 {
    output
        .tool_events
        .iter()
        .filter(|event| event.success && is_audited_mutation_event(event))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// True when a tool-execution summary names a mutating tool or operation.
///
/// ADR-82 slice 1: when the summary carries the canonical policy-view identity
/// it is **authoritative** — a `write` alias resolves to its
/// `("filesystem", "write")` effect and is classified structurally by
/// [`crate::tool_facts::is_file_affecting_tool`], not by a per-site name arm or
/// a prose marker. Summaries without a canonical identity (legacy records, or
/// the supervised child, which cannot see the registry) fall back to the
/// operation field and then the registered-name grammar.
///
/// The former `"Wrote "`/`"Deleted "` Display-parse fallback is **removed**:
/// classification reads structured identity only, never prose.
fn is_audited_mutation_event(event: &ToolExecutionSummary) -> bool {
    if let Some(tool) = event.canonical_tool.as_deref() {
        return crate::tool_facts::is_file_affecting_tool(
            tool,
            event.canonical_operation.as_deref(),
        );
    }
    if event
        .operation
        .as_deref()
        .is_some_and(|op| matches!(op, "write" | "delete" | "move" | "copy"))
    {
        return true;
    }
    matches!(
        event.tool_name.as_str(),
        "write" | "write_file" | "delete_file" | "edit_file" | "create_file" | "modify_file"
    )
}

/// Hard completion condition: verification must actually pass when the task
/// requires it. Without this, a capped or partially-failed run could be
/// reported as `Done`.
/// Instruction appended to the conversation when a run is auto-continued,
/// so the model resumes the same task instead of re-summarizing.
pub(super) fn continuation_instruction(reason: &str) -> String {
    format!(
        "Continue the same task. Previous run stopped because: {reason}. \
             Do not summarize. Continue using tools until the job is done, \
             verification passes, user input is required, or a real blocker is found."
    )
}

#[cfg(test)]
mod tests {
    use concerto_core::ids::Ulid;
    use concerto_core::types::{AgentCompletionStatus, VerificationSummary};
    use concerto_core::TaskId;

    use super::*;

    /// Minimal `AgentOutput` with every field explicitly empty/zero, so each
    /// test states only the field it exercises.
    fn empty_output() -> AgentOutput {
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
            completion_status: AgentCompletionStatus::Partial,
            provider_metrics: vec![],
            checkpoint_json: None,
        }
    }

    /// Build one tool-execution summary for the classification tests (no
    /// canonical identity — the legacy shape).
    fn event(
        tool_name: &str,
        operation: Option<&str>,
        success: bool,
        summary: &str,
    ) -> ToolExecutionSummary {
        ToolExecutionSummary {
            tool_name: tool_name.to_string(),
            operation: operation.map(str::to_string),
            path: None,
            success,
            summary: summary.to_string(),
            canonical_tool: None,
            canonical_operation: None,
        }
    }

    /// Build one summary carrying an ADR-82 slice-1 canonical identity.
    fn canonical_event(
        tool_name: &str,
        canonical_tool: &str,
        canonical_operation: Option<&str>,
    ) -> ToolExecutionSummary {
        ToolExecutionSummary {
            tool_name: tool_name.to_string(),
            operation: None,
            path: None,
            success: true,
            summary: String::new(),
            canonical_tool: Some(canonical_tool.to_string()),
            canonical_operation: canonical_operation.map(str::to_string),
        }
    }

    /// Empty boundary: an output with no tool events counts zero audited
    /// file changes, so the ActionRequired reconciliation stays blocked on
    /// an empty run rather than miscounting.
    #[test]
    fn audited_file_changes_is_zero_for_empty_tool_events() {
        assert_eq!(audited_file_changes(&empty_output()), 0);
    }

    /// Count only SUCCESSFUL audited mutations: a failed mutation and a
    /// successful read must not contribute, while a successful mutation does.
    #[test]
    fn audited_file_changes_counts_only_successful_mutations() {
        let mut output = empty_output();
        output.tool_events = vec![
            event("write", Some("write"), true, "Wrote 10 bytes"),
            event("write", Some("write"), false, "permission denied"),
            event("read", Some("read"), true, "Read 10 bytes"),
        ];
        assert_eq!(audited_file_changes(&output), 1);
    }

    /// Operation-field grammar: mutating operations classify as mutations and
    /// non-mutating or unknown operations do not, regardless of tool name.
    #[test]
    fn mutation_class_from_operation_field_covers_each_mutating_verbs() {
        for op in ["write", "delete", "move", "copy"] {
            assert!(is_audited_mutation_event(&event("fs", Some(op), true, "")), "op={op}");
        }
        for op in ["read", "list", "unknown"] {
            assert!(!is_audited_mutation_event(&event("fs", Some(op), true, "")), "op={op}");
        }
        assert!(!is_audited_mutation_event(&event("fs", None, true, "no marker here")));
    }

    /// Tool-name grammar for the filesystem mutation tools, and the boundary
    /// that a near-miss name is not counted.
    #[test]
    fn mutation_class_from_tool_name_rejects_near_miss_names() {
        for name in
            ["write", "write_file", "delete_file", "edit_file", "create_file", "modify_file"]
        {
            assert!(is_audited_mutation_event(&event(name, None, true, "")), "name={name}");
        }
        assert!(!is_audited_mutation_event(&event("read_file", None, true, "")));
        assert!(!is_audited_mutation_event(&event("write_file_v2", None, true, "")));
    }

    /// ADR-82 slice 1: the canonical policy-view identity is authoritative and
    /// classifies the `write` alias structurally — no name arm, no prose
    /// marker. A non-filesystem canonical name with a mutating operation still
    /// classifies via the shared grammar.
    #[test]
    fn mutation_class_prefers_canonical_identity_for_alias_writes() {
        // A `write` alias: raw args name no operation and the name grammar is
        // deliberately bypassed because a canonical identity is present.
        assert!(is_audited_mutation_event(&canonical_event("write", "filesystem", Some("write"))));
        assert!(is_audited_mutation_event(&canonical_event("apply", "filesystem", Some("delete"))));
        assert!(is_audited_mutation_event(&canonical_event("write_file", "write_file", None)));
        // Canonical read identity never counts, even with a write-ish summary.
        let mut read = canonical_event("filesystem", "filesystem", Some("read"));
        read.summary = "Wrote 42 bytes".to_owned();
        assert!(!is_audited_mutation_event(&read));
    }

    /// ADR-82 slice 1: the prose-marker fallback is removed — a summary that
    /// merely *looks* like a write in prose (and carries no canonical identity,
    /// operation, or write-tool name) is never classified as a mutation.
    #[test]
    fn mutation_class_ignores_prose_markers() {
        assert!(!is_audited_mutation_event(&event("fs", None, true, "Wrote 42 bytes")));
        assert!(!is_audited_mutation_event(&event("fs", None, true, "Deleted a.txt")));
        assert!(!is_audited_mutation_event(&event("fs", None, true, "reads Wrote 42 bytes")));
    }

    /// Empty boundary: merging an empty latest round adds nothing — no
    /// duplicate files, no verification rows, no tool calls.
    #[test]
    fn merge_run_progress_with_empty_latest_is_a_no_op() {
        let mut acc = AgentOutput {
            files_modified: vec![camino::Utf8PathBuf::from("a.rs")],
            tool_call_count: 2,
            final_message: "partial".into(),
            ..empty_output()
        };
        merge_run_progress(&mut acc, &empty_output());
        assert_eq!(acc.files_modified.len(), 1);
        assert_eq!(acc.tool_call_count, 2);
        assert!(acc.verification.is_empty());
        // The merge always takes the latest final message, empty or not.
        assert_eq!(acc.final_message, "");
    }

    /// Empty boundary: a blank output fingerprints to all-zero counters, and
    /// two blank outputs fingerprint equal (no progress signal either way).
    #[test]
    fn progress_fingerprint_of_empty_output_is_all_zero_and_stable() {
        let a = ProgressFingerprint::from_output(&empty_output());
        let b = ProgressFingerprint::from_output(&empty_output());
        assert_eq!(
            a,
            ProgressFingerprint {
                files_modified: 0,
                tool_call_count: 0,
                passed_verifications: 0,
                failed_verifications: 0,
                final_message_len: 0,
            }
        );
        assert_eq!(a, b);
    }

    /// Verification counters split passed from failed on the same output.
    #[test]
    fn progress_fingerprint_splits_passed_and_failed_verifications() {
        let mut output = empty_output();
        output.verification = vec![
            VerificationSummary {
                path: "a.py".into(),
                command: "py_compile".into(),
                passed: true,
                output: String::new(),
            },
            VerificationSummary {
                path: "b.py".into(),
                command: "py_compile".into(),
                passed: false,
                output: String::new(),
            },
        ];
        let fp = ProgressFingerprint::from_output(&output);
        assert_eq!(fp.passed_verifications, 1);
        assert_eq!(fp.failed_verifications, 1);
    }

    /// Boundary: an empty stop reason still yields the full instruction —
    /// the template is never dropped and the model is still told to resume
    /// the same task.
    #[test]
    fn continuation_instruction_with_empty_reason_keeps_the_template() {
        let instruction = continuation_instruction("");
        assert!(instruction.contains("Continue the same task"));
        assert!(instruction.contains("Do not summarize"));
        assert!(instruction.contains("because: ."));
    }

    /// Boundary: reason text is embedded verbatim, including whitespace the
    /// caller passed, so the stop cause reaches the model unchanged.
    #[test]
    fn continuation_instruction_preserves_the_reason_verbatim() {
        let reason = "iteration cap hit after 10 iterations";
        let instruction = continuation_instruction(reason);
        assert!(instruction.contains(reason));
        assert_eq!(instruction.matches(reason).count(), 1);
    }
}
