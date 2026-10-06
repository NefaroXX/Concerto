//! Shared structured tool-call block parsing for ADR-66 §4 text-fallback
//! turns (NORM S4).
//!
//! Both integrating loops — the single-agent loop [`crate::agent_loop::AgentLoop`]
//! and the multi-agent specialist [`crate::agents::GenericSpecialistAgent`] —
//! resolve a fallback turn the same way: scan the turn text for a
//! `<tool_calls>` block ([`TextToolDriver::parse_turn`]) and, when that block
//! is malformed, either request one bounded repair or fail the run loudly.
//! This module is the single home of that parse-plus-bound decision, so the
//! repair bound is stated once; each loop keeps its own event/audit/reprompt
//! I/O, its own error strings, and its own attempt counter.

use crate::tool_driver::{DriverTurn, TextToolDriver, MAX_REPAIR_ATTEMPTS};
use concerto_core::types::ToolCall;

/// The bounded decision for one structured tool-call block parse.
#[derive(Debug)]
pub(crate) enum BlockDecision {
    /// Well-formed block(s): the parsed calls are ready for execution.
    Ready(Vec<ToolCall>),
    /// No block markers: a final answer in plain text.
    FinalAnswer(String),
    /// Markers with unparseable content and repairs still within the bound:
    /// re-prompt for one more attempt.
    Repair { reason: String },
    /// Markers with unparseable content and the repair bound exhausted: the
    /// caller fails the run loudly (ADR-66 §2(c)) — never a silent completion.
    Exhausted { reason: String },
}

/// Parse `text` for structured tool-call blocks under the shared repair bound.
///
/// `attempts` is the number of repair attempts already made for this turn;
/// once it reaches [`MAX_REPAIR_ATTEMPTS`], a further malformed block is
/// [`BlockDecision::Exhausted`] instead of another repair. The scan itself is
/// the strict block parser in [`TextToolDriver::parse_turn`] (first parseable
/// block wins; unknown tool names and non-object arguments are defects).
pub(crate) fn parse_tool_blocks(
    driver: &mut TextToolDriver,
    text: &str,
    attempts: u32,
) -> BlockDecision {
    match driver.parse_turn(text) {
        DriverTurn::ToolCalls(calls) => BlockDecision::Ready(calls),
        DriverTurn::FinalAnswer(final_text) => BlockDecision::FinalAnswer(final_text),
        DriverTurn::Malformed { reason } if attempts >= MAX_REPAIR_ATTEMPTS => {
            BlockDecision::Exhausted { reason }
        }
        DriverTurn::Malformed { reason } => BlockDecision::Repair { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::ToolDefinition;
    use serde_json::json;

    fn driver() -> TextToolDriver {
        TextToolDriver::new(vec![ToolDefinition {
            name: "echo".into(),
            description: "Echo.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }])
    }

    /// A well-formed block parses to executable calls.
    #[test]
    fn parses_well_formed_block_to_calls() {
        let text = "<tool_calls>\n[{\"name\": \"echo\", \"arguments\": {\"x\": 1}}]\n</tool_calls>";
        match parse_tool_blocks(&mut driver(), text, 0) {
            BlockDecision::Ready(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "echo");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// No block markers: the whole text is a final answer.
    #[test]
    fn plain_text_is_a_final_answer() {
        match parse_tool_blocks(&mut driver(), "All done.", 0) {
            BlockDecision::FinalAnswer(text) => assert_eq!(text, "All done."),
            other => panic!("expected FinalAnswer, got {other:?}"),
        }
    }

    /// Malformed content inside markers requests the next repair attempt.
    #[test]
    fn malformed_block_requests_repair() {
        let text = "<tool_calls>\nnot json\n</tool_calls>";
        match parse_tool_blocks(&mut driver(), text, 0) {
            BlockDecision::Repair { reason } => assert!(reason.contains("JSON"), "{reason}"),
            other => panic!("expected Repair, got {other:?}"),
        }
    }

    /// A truncated block (opened, never closed) is repairable, not final.
    #[test]
    fn truncated_block_requests_repair() {
        let text = "<tool_calls>\n[{\"name\": \"echo\"}]";
        match parse_tool_blocks(&mut driver(), text, 1) {
            BlockDecision::Repair { reason } => {
                assert!(reason.contains("never closed"), "{reason}")
            }
            other => panic!("expected Repair, got {other:?}"),
        }
    }

    /// The bound: one attempt below [`MAX_REPAIR_ATTEMPTS`] still repairs;
    /// at and past the bound the decision is a loud failure.
    #[test]
    fn repair_bound_is_exhausted_loudly() {
        let text = "<tool_calls>\nnot json\n</tool_calls>";
        assert!(matches!(
            parse_tool_blocks(&mut driver(), text, MAX_REPAIR_ATTEMPTS - 1),
            BlockDecision::Repair { .. }
        ));
        assert!(matches!(
            parse_tool_blocks(&mut driver(), text, MAX_REPAIR_ATTEMPTS),
            BlockDecision::Exhausted { .. }
        ));
        assert!(matches!(
            parse_tool_blocks(&mut driver(), text, MAX_REPAIR_ATTEMPTS + 1),
            BlockDecision::Exhausted { .. }
        ));
    }
}
