//! Runtime-observed verification transported through specialist settlements.

use super::*;
use concerto_core::types::{VerificationOutcome, VerificationSummary};

const MAX_RECORDS: usize = 128;
const COVERAGE_COMMAND: &str = "specialist-verification-coverage";
const BUDGET_COMMAND: &str = "specialist-verification-budget";

pub(super) fn restored(task: &SubTask, context: &AgentContext) -> Vec<VerificationSummary> {
    let mut records = Vec::new();
    if let Some(previous) = context.previous_results.iter().rev().find(|r| r.task_id == task.id) {
        for record in &previous.verification {
            insert(&mut records, record.clone());
        }
    }
    records
}

/// Only an executed native verification operation can populate this ledger.
/// Unmaterialized, incomplete or malformed envelopes cannot provide evidence.
pub(super) fn collect(
    records: &mut Vec<VerificationSummary>,
    tool: &str,
    canonical_tool: Option<&str>,
    output: &serde_json::Value,
    event_id: Option<&str>,
) {
    if canonical_tool.unwrap_or(tool) != crate::verify::VERIFY_RUN_TOOL_NAME {
        return;
    }
    let scope = format!(
        "{}:{}",
        output.get("scope").and_then(|value| value.as_str()).unwrap_or("unknown"),
        output.get("verifier").and_then(|value| value.as_str()).unwrap_or("unknown")
    );
    let decoded = output
        .get("records")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<VerificationSummary>>(value).ok());
    let Some(mut observed) = decoded else {
        incomplete(
            records,
            "native verify.run returned malformed records",
            &scope,
            COVERAGE_COMMAND,
        );
        return;
    };
    let complete = output.get("overlay_materialized").and_then(|v| v.as_bool()) == Some(true)
        && output.get("truncated_records").and_then(|v| v.as_bool()) == Some(false)
        && output.get("total_records").and_then(|v| v.as_u64()) == Some(observed.len() as u64);
    if complete {
        records.retain(|record| record.command != COVERAGE_COMMAND || record.scope != scope);
    }
    for record in &mut observed {
        if complete {
            if let Some(id) = event_id.filter(|id| !id.is_empty()) {
                if !record.evidence_ids.iter().any(|existing| existing == id) {
                    record.evidence_ids.push(id.to_string());
                }
            } else {
                // Write evidence alone does not prove that this check ran.
                record.evidence_ids.clear();
            }
        } else {
            record.content_fingerprint.clear();
            record.evidence_ids.clear();
        }
        insert(records, record.clone());
    }
    if !complete {
        incomplete(
            records,
            "verify.run coverage was incomplete or overlay state was pending",
            &scope,
            COVERAGE_COMMAND,
        );
    }
}

fn incomplete(records: &mut Vec<VerificationSummary>, detail: &str, scope: &str, command: &str) {
    let record = VerificationSummary::error("project", command, detail)
        .with_verifier(crate::verify::VERIFY_RUN_TOOL_NAME)
        .with_scope(scope);
    if let Some(existing) = records.iter_mut().find(|r| r.command == command && r.scope == scope) {
        *existing = record;
    } else {
        if records.len() >= MAX_RECORDS {
            records.pop();
        }
        records.push(record);
    }
}

fn insert(records: &mut Vec<VerificationSummary>, record: VerificationSummary) {
    if let Some(existing) = records.iter_mut().find(|r| {
        r.path == record.path
            && r.command == record.command
            && r.verifier == record.verifier
            && r.scope == record.scope
    }) {
        *existing = record;
    } else if records.len() < MAX_RECORDS - 1 {
        records.push(record);
    } else {
        incomplete(
            records,
            "specialist verification ledger exceeded its record budget",
            "project",
            BUDGET_COMMAND,
        );
    }
}

pub(crate) fn has_failure(records: &[VerificationSummary]) -> bool {
    records.iter().any(|record| {
        record.inconsistent
            || matches!(
                record.outcome,
                Some(
                    VerificationOutcome::Failed
                        | VerificationOutcome::Error
                        | VerificationOutcome::Cancelled
                )
            )
    })
}

/// Latest runtime observation per check, ordered independently of task map order.
pub(crate) fn merged(results: &[&AgentRunResult]) -> Vec<VerificationSummary> {
    let mut observed: Vec<_> =
        results.iter().flat_map(|r| r.verification.iter().cloned()).collect();
    observed.sort_by_key(|record| record.checked_at);
    let mut records = Vec::new();
    for record in observed {
        insert(&mut records, record);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(record: VerificationSummary) -> serde_json::Value {
        serde_json::json!({"records": [record], "overlay_materialized": true,
            "total_records": 1, "truncated_records": false})
    }

    #[test]
    fn native_failed_check_supersedes_pass_and_requires_observed_check_event() {
        let mut records = Vec::new();
        let passed = VerificationSummary::passed("a.rs", "check", "ok")
            .with_verifier("native")
            .with_scope("a.rs");
        collect(&mut records, "mcp:fake:verify.run", None, &envelope(passed.clone()), Some("fake"));
        assert!(records.is_empty());
        collect(&mut records, "verify.run", None, &envelope(passed), Some("actual-check"));
        assert_eq!(records[0].evidence_ids, vec!["actual-check"]);
        let failed = VerificationSummary::failed("a.rs", "check", "broken")
            .with_verifier("native")
            .with_scope("a.rs");
        collect(&mut records, "verify.run", None, &envelope(failed), None);
        assert_eq!(records.len(), 1);
        assert!(has_failure(&records));
        assert!(records[0].evidence_ids.is_empty());
    }

    #[test]
    fn a_complete_recheck_replaces_the_same_scopes_incomplete_coverage() {
        let mut records = Vec::new();
        let mut output = envelope(VerificationSummary::passed("a.rs", "check", "ok"));
        output["overlay_materialized"] = serde_json::json!(false);
        collect(&mut records, "verify.run", None, &output, Some("pending-check"));
        assert!(has_failure(&records));
        output["overlay_materialized"] = serde_json::json!(true);
        collect(&mut records, "verify.run", None, &output, Some("materialized-check"));
        assert!(!has_failure(&records));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].evidence_ids, vec!["materialized-check"]);
    }

    #[test]
    fn incomplete_envelope_and_record_budget_remain_explicit_failures() {
        let mut records = Vec::new();
        for index in 0..MAX_RECORDS + 2 {
            insert(&mut records, VerificationSummary::passed(format!("{index}.rs"), "check", "ok"));
        }
        assert!(records.len() <= MAX_RECORDS);
        assert!(has_failure(&records));
        let mut output = envelope(
            VerificationSummary::passed("a.rs", "check", "ok").with_content_fingerprint("bytes"),
        );
        output["truncated_records"] = serde_json::json!(true);
        collect(&mut records, "verify.run", None, &output, Some("check"));
        assert!(has_failure(&records));
    }
}
