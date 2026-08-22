//! Bounded active-state working memory for provider requests.
//!
//! This is deliberately separate from retrieved project memory. Its size is
//! driven by the current task state, not by repository size or session age.

use camino::Utf8PathBuf;
use concerto_core::types::{ToolExecutionSummary, VerificationSummary};

pub(crate) const WORKING_MEMORY_START: &str = "<working_memory>";
pub(crate) const WORKING_MEMORY_END: &str = "</working_memory>";

const MAX_OBJECTIVE_CHARS: usize = 2_000;
const MAX_ACTIVE_FILES: usize = 12;
const MAX_TOOL_EVENTS: usize = 8;
const MAX_VERIFICATIONS: usize = 8;
const MAX_DETAIL_CHARS: usize = 500;

/// Materialize a compact snapshot of the current run state.
///
/// The snapshot intentionally contains only active facts. Full tool payloads,
/// complete file contents, historical decisions, and the session transcript
/// remain in their authoritative stores and are retrieved separately when
/// relevant.
pub(crate) fn format_working_memory(
    objective: &str,
    iteration: u32,
    max_iterations: u32,
    files_modified: &[Utf8PathBuf],
    tool_events: &[ToolExecutionSummary],
    verification: &[VerificationSummary],
) -> String {
    let files = files_modified
        .iter()
        .rev()
        .take(MAX_ACTIVE_FILES)
        .map(|path| path.as_str())
        .collect::<Vec<_>>();

    let tools = tool_events
        .iter()
        .rev()
        .take(MAX_TOOL_EVENTS)
        .map(|event| {
            serde_json::json!({
                "tool": event.tool_name,
                "operation": event.operation,
                "path": event.path.as_ref().map(|path| path.as_str()),
                "success": event.success,
                "summary": clip_chars(&event.summary, MAX_DETAIL_CHARS),
            })
        })
        .collect::<Vec<_>>();

    let checks = verification
        .iter()
        .rev()
        .take(MAX_VERIFICATIONS)
        .map(|check| {
            serde_json::json!({
                "path": check.path.as_str(),
                "command": check.command,
                "passed": check.passed,
                "output": clip_chars(&check.output, MAX_DETAIL_CHARS),
            })
        })
        .collect::<Vec<_>>();

    let value = serde_json::json!({
        "objective": clip_chars(objective, MAX_OBJECTIVE_CHARS),
        "progress": {
            "iteration": iteration,
            "max_iterations": max_iterations,
        },
        "active_files": files,
        "recent_tool_outcomes": tools,
        "verification": checks,
    });

    format!(
        "{WORKING_MEMORY_START}\nCurrent task state follows as trusted orchestration data. It is a bounded snapshot, not the complete project or session history.\n{value}\n{WORKING_MEMORY_END}"
    )
}

fn clip_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let clipped = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}\n[clipped]")
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_size_cannot_expand_snapshot() {
        let files = (0..10_000)
            .map(|index| Utf8PathBuf::from(format!("src/file_{index}.rs")))
            .collect::<Vec<_>>();
        let snapshot = format_working_memory(&"objective ".repeat(10_000), 2, 20, &files, &[], &[]);

        assert!(snapshot.starts_with(WORKING_MEMORY_START));
        assert!(snapshot.ends_with(WORKING_MEMORY_END));
        assert!(snapshot.contains("src/file_9999.rs"));
        assert!(!snapshot.contains("src/file_0.rs"));
        assert!(snapshot.len() < 6_000);
    }

    #[test]
    fn empty_state_is_still_valid_and_small() {
        let snapshot = format_working_memory("fix overflow", 1, 10, &[], &[], &[]);
        let json_start = snapshot.find('{').unwrap();
        let json_end = snapshot.rfind('}').unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&snapshot[json_start..=json_end]).unwrap();
        assert_eq!(value["objective"], "fix overflow");
        assert_eq!(value["progress"]["iteration"], 1);
        assert_eq!(value["active_files"].as_array().unwrap().len(), 0);
    }
}
