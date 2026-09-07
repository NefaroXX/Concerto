//! Universal text-fallback tool driver (ADR-66 §4).
//!
//! Providers whose wire path cannot express tool declarations (e.g.
//! Zen-served genuine `muse-v*` models on the Responses dialect) must never
//! silently degrade a tool-requiring task to text-only output. This module
//! implements the harness-level prompt-based driver that engages
//! **automatically** for such providers: tool schemas are injected into the
//! system prompt, the model is asked to emit structured tool-call blocks,
//! and a strict parser reduces those blocks to canonical [`ToolCall`]s.
//!
//! # Contract
//!
//! - **Native is always preferred.** The driver engages only when
//!   [`fallback_engaged`] resolves the provider/model to no native tool
//!   support (the same ADR-66 §3 resolution used at selection). Plugin
//!   providers are excluded: they are hard-gated to AnswerOnly tasks with
//!   an explicit error, so the driver must not bypass that gate.
//! - **Strict parser, bounded repair.** A turn carrying tool-call block
//!   markers but unparseable content is a defect to repair: the caller
//!   re-prompts with [`repair_message`] up to
//!   [`MAX_REPAIR_ATTEMPTS`] times, then fails the run loudly — never a
//!   silent completion.
//! - **Labeled, not invisible.** Fallback turns are labeled in the
//!   transcript via lifecycle events and in the audit log
//!   (`tool_driver` rows) by the integrating loops.
//! - **No tool declarations on the wire.** Driver-driven requests carry
//!   `tools: None` — the driver's prompt section replaces the wire
//!   declarations — so fail-loud request-build seams (Responses dialect,
//!   plugin protocol) can never fire on a fallback-driven turn.

use concerto_core::types::{Message, Role, ToolCall, ToolDefinition};

/// Maximum repair-by-reprompt attempts per driver turn before the run
/// fails loudly (ADR-66 §2(c): bounded repair, then run-level failure).
pub const MAX_REPAIR_ATTEMPTS: u32 = 2;

/// Block markers the model is instructed to use for tool calls.
pub const BLOCK_OPEN: &str = "<tool_calls>";
pub const BLOCK_CLOSE: &str = "</tool_calls>";

/// The outcome of strictly parsing one driver turn.
#[derive(Debug, Clone)]
pub enum DriverTurn {
    /// The turn carried one or more well-formed tool-call blocks; the
    /// parsed calls (with generated driver ids) are ready for execution.
    ToolCalls(Vec<ToolCall>),
    /// The turn carried no tool-call block at all: a final answer in plain
    /// text. The integrating loop's own completion semantics decide whether
    /// that answer satisfies the task (e.g. the action-required gate).
    FinalAnswer(String),
    /// The turn carried block markers but the content could not be reduced
    /// to valid tool calls: repair-by-reprompt is required.
    Malformed { reason: String },
}

/// The prompt-based tool driver for one run: the advertised tools plus the
/// strict parser state.
pub struct TextToolDriver {
    tools: Vec<ToolDefinition>,
    /// Sequential call-id counter for generated `td_*` ids.
    next_id: u64,
}

impl TextToolDriver {
    /// Build a driver for the given advertised tools.
    pub fn new(tools: Vec<ToolDefinition>) -> Self {
        Self { tools, next_id: 0 }
    }

    /// Whether the driver can drive anything: it needs at least one tool.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The system-prompt section injected into every fallback-driven
    /// request: the contract, the full tool schemas, and the block format.
    pub fn prompt_section(&self) -> String {
        let mut section = String::from(
            "# Tool driver (text mode)\n\n\
             Your environment does not support native tool calls for this model, so tools are \
             driven through text. You have access to the following tools:\n\n",
        );
        for tool in &self.tools {
            section.push_str(&format!(
                "- `{}`: {}\n  Parameters schema: {}\n",
                tool.name,
                tool.description,
                serde_json::to_string(&tool.parameters)
                    .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string()),
            ));
        }
        section.push_str(
            "\n## How to call a tool\n\n\
             When you decide to use tools, reply with EXACTLY one block of this form \
             (and nothing else inside the block):\n\n\
             <tool_calls>\n\
             [{\"name\": \"<tool name>\", \"arguments\": {<arguments matching the tool's \
             schema>}}]\n\
             </tool_calls>\n\n\
             Rules:\n\
             - `name` must be one of the listed tool names.\n\
             - `arguments` must be a JSON object matching that tool's parameters schema.\n\
             - You may include brief reasoning before the block, but the block itself must \
             be parseable JSON.\n\
             - After each block you will receive the tool results as `[tool result]` \
             messages; then continue with the next step (another block).\n\
             - When the task is complete and no more tools are needed, reply in plain text \
             only — no `<tool_calls>` block — stating the final answer.\n",
        );
        section
    }

    /// Strip the wire tool declarations and inject the prompt section into
    /// a request about to be sent in fallback mode.
    pub fn augment_request(&self, request: &mut concerto_core::types::CompletionRequest) {
        request.tools = None;
        let section = self.prompt_section();
        match request.messages.iter_mut().find(|message| message.role == Role::System) {
            Some(system_message) => {
                system_message.content.push_str("\n\n");
                system_message.content.push_str(&section);
            }
            None => {
                request.messages.insert(
                    0,
                    Message {
                        role: Role::System,
                        content: section,
                        tool_calls: None,
                        tool_results: None,
                        reasoning_content: None,
                        tokens_in: None,
                        tokens_out: None,
                    },
                );
            }
        }
    }

    /// Strictly parse one model turn.
    ///
    /// - No block markers → [`DriverTurn::FinalAnswer`] (the whole text).
    /// - Well-formed block(s) → [`DriverTurn::ToolCalls`] with generated
    ///   `td_*` ids; the first parseable block wins; unknown tool names or
    ///   non-object arguments are defects.
    /// - Markers with unparseable content → [`DriverTurn::Malformed`].
    pub fn parse_turn(&mut self, text: &str) -> DriverTurn {
        let Some(open_index) = text.find(BLOCK_OPEN) else {
            return DriverTurn::FinalAnswer(text.to_string());
        };
        let after_open = &text[open_index + BLOCK_OPEN.len()..];
        let Some(close_offset) = after_open.find(BLOCK_CLOSE) else {
            return DriverTurn::Malformed {
                reason: "a <tool_calls> block was opened but never closed".to_string(),
            };
        };
        let raw = after_open[..close_offset].trim();
        // Tolerate an accidental markdown fence around the JSON; the JSON
        // inside must still be strict.
        let raw = raw
            .strip_prefix("```json")
            .or_else(|| raw.strip_prefix("```"))
            .unwrap_or(raw)
            .strip_suffix("```")
            .unwrap_or(raw)
            .trim();
        if raw.is_empty() {
            return DriverTurn::Malformed { reason: "the <tool_calls> block is empty".to_string() };
        }

        let calls: Vec<ParsedCall> = match serde_json::from_str(raw) {
            Ok(calls) => calls,
            Err(error) => {
                return DriverTurn::Malformed {
                    reason: format!("the <tool_calls> block is not a valid JSON array: {error}"),
                };
            }
        };
        if calls.is_empty() {
            return DriverTurn::Malformed {
                reason: "the <tool_calls> block contains no calls".to_string(),
            };
        }

        let mut tool_calls = Vec::with_capacity(calls.len());
        for call in calls {
            let ParsedCall { name, arguments } = call;
            if !self.tools.iter().any(|tool| tool.name == name) {
                return DriverTurn::Malformed { reason: format!("unknown tool name '{name}'") };
            }
            let arguments = arguments.unwrap_or_else(|| serde_json::json!({}));
            if !arguments.is_object() {
                return DriverTurn::Malformed {
                    reason: format!("arguments for '{name}' must be a JSON object"),
                };
            }
            let id = format!("td_{}", self.next_id);
            self.next_id += 1;
            tool_calls.push(ToolCall { id, name, arguments });
        }
        DriverTurn::ToolCalls(tool_calls)
    }

    /// The repair prompt for a malformed turn: re-states the format and the
    /// concrete defect so the model can correct it.
    pub fn repair_message(reason: &str) -> Message {
        Message {
            role: Role::User,
            content: format!(
                "Your previous reply was not a valid tool-call block: {reason}. \
                 Reply with EXACTLY one block of the form\n\
                 <tool_calls>\n\
                 [{{\"name\": \"<tool name>\", \"arguments\": {{...}}}}]\n\
                 </tool_calls>\n\
                 with `name` taken from the listed tools and `arguments` a JSON object \
                 matching that tool's schema. If the task is complete and no tools are \
                 needed, reply in plain text only.",
            ),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }
}

/// One parsed tool-call entry from the block: name + optional arguments.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedCall {
    name: String,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

/// Decide whether the ADR-66 §4 text-fallback driver engages for a run.
///
/// Engages automatically when the provider lacks native tool support for
/// the model (the ADR-66 §3 resolution, no override at loop level) AND the
/// provider is not plugin-backed — plugin providers are hard-gated to
/// AnswerOnly tasks with an explicit error (decision (a)), so the fallback
/// must not bypass that gate. Unknown models attempt native first and are
/// never fallback-driven from selection time.
pub fn fallback_engaged(provider: &str, model: &str) -> bool {
    if concerto_providers::capability::is_plugin_backed(provider) {
        return false;
    }
    !concerto_providers::capability::resolve_tool_support(provider, model, None, None)
}

/// Merge two provider-reported usage reports.
///
/// ADR-66 §4: text-fallback repair turns consume additional tokens; merging
/// the reports keeps spend accounting complete so fallback repairs never
/// hide spend.
pub fn merge_usage(
    a: Option<concerto_core::types::CompletionUsage>,
    b: Option<concerto_core::types::CompletionUsage>,
) -> Option<concerto_core::types::CompletionUsage> {
    use concerto_core::types::CompletionUsage;
    match (a, b) {
        (Some(a), Some(b)) => Some(CompletionUsage {
            prompt_tokens: add_optional_tokens(a.prompt_tokens, b.prompt_tokens),
            completion_tokens: add_optional_tokens(a.completion_tokens, b.completion_tokens),
        }),
        (None, b) => b,
        (a, None) => a,
    }
}

fn add_optional_tokens(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn driver() -> TextToolDriver {
        TextToolDriver::new(vec![
            ToolDefinition {
                name: "filesystem".into(),
                description: "File ops.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "echo".into(),
                description: "Echo.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        ])
    }

    #[test]
    fn plain_text_is_a_final_answer() {
        let mut driver = driver();
        match driver.parse_turn("All done — the file is written.") {
            DriverTurn::FinalAnswer(text) => {
                assert_eq!(text, "All done — the file is written.");
            }
            other => panic!("expected FinalAnswer, got: {other:?}"),
        }
    }

    #[test]
    fn well_formed_block_parses_to_tool_calls() {
        let mut driver = driver();
        let text = "I will write the file.\n<tool_calls>\n\
                    [{\"name\": \"filesystem\", \"arguments\": {\"operation\": \"write\", \
                    \"path\": \"a.txt\", \"content\": \"hi\"}}]\n</tool_calls>";
        match driver.parse_turn(text) {
            DriverTurn::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[0].id, "td_0");
                assert_eq!(calls[0].arguments["operation"], "write");
            }
            other => panic!("expected ToolCalls, got: {other:?}"),
        }
        // The id counter advances across turns.
        match driver.parse_turn("<tool_calls>\n[{\"name\": \"echo\"}]\n</tool_calls>") {
            DriverTurn::ToolCalls(calls) => {
                assert_eq!(calls[0].id, "td_1", "ids are sequential per driver");
                assert_eq!(
                    calls[0].arguments,
                    json!({}),
                    "absent arguments coerce to an empty object"
                );
            }
            other => panic!("expected ToolCalls, got: {other:?}"),
        }
    }

    #[test]
    fn multiple_valid_calls_parse_in_order() {
        let mut driver = driver();
        let text = "<tool_calls>\n\
                    [{\"name\": \"filesystem\", \"arguments\": {}}, \
                    {\"name\": \"echo\", \"arguments\": {\"x\": 1}}]\n\
                    </tool_calls>";
        match driver.parse_turn(text) {
            DriverTurn::ToolCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "filesystem");
                assert_eq!(calls[1].name, "echo");
            }
            other => panic!("expected ToolCalls, got: {other:?}"),
        }
    }

    #[test]
    fn malformed_blocks_are_repairable_defects() {
        let mut driver = driver();
        let cases = [
            ("<tool_calls>\n[{\"name\": \"filesystem\", }]\n</tool_calls>", "valid JSON"),
            ("<tool_calls>\nnot json at all\n</tool_calls>", "valid JSON"),
            ("<tool_calls>\n</tool_calls>", "empty"),
            ("<tool_calls>\nno closing marker", "never closed"),
            (
                "<tool_calls>\n[{\"name\": \"definitely_not_a_tool\", \"arguments\": {}}]\n</tool_calls>",
                "unknown tool",
            ),
            (
                "<tool_calls>\n[{\"name\": \"echo\", \"arguments\": \"not-an-object\"}]\n</tool_calls>",
                "non-object arguments",
            ),
        ];
        for (text, label) in cases {
            assert!(
                matches!(driver.parse_turn(text), DriverTurn::Malformed { .. }),
                "case '{label}' must be Malformed: {text}"
            );
        }
    }

    #[test]
    fn fenced_block_json_is_tolerated() {
        let mut driver = driver();
        let text =
            "<tool_calls>\n```json\n[{\"name\": \"echo\", \"arguments\": {}}]\n```\n</tool_calls>";
        match driver.parse_turn(text) {
            DriverTurn::ToolCalls(calls) => assert_eq!(calls[0].name, "echo"),
            other => panic!("expected ToolCalls, got: {other:?}"),
        }
    }

    #[test]
    fn repair_message_restates_the_defect() {
        let message = TextToolDriver::repair_message("the block is empty");
        assert_eq!(message.role, Role::User);
        assert!(message.content.contains("the block is empty"));
        assert!(message.content.contains(BLOCK_OPEN));
        assert!(message.content.contains(BLOCK_CLOSE));
    }

    /// The augmented request carries no tool declarations (the fail-loud
    /// wire seams must never fire on a fallback-driven request) and the
    /// prompt section lands in the system message.
    #[test]
    fn augment_request_strips_tools_and_injects_section() {
        let driver = driver();
        let mut request = concerto_core::types::CompletionRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: "base system prompt".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "do the thing".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            tools: Some(vec![ToolDefinition {
                name: "filesystem".into(),
                description: String::new(),
                parameters: json!({}),
            }]),
            ..Default::default()
        };
        driver.augment_request(&mut request);
        assert!(request.tools.is_none(), "fallback-driven requests carry no wire tools");
        let system = request
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .expect("system message present");
        assert!(system.content.starts_with("base system prompt"));
        assert!(system.content.contains("## How to call a tool"));
        assert!(system.content.contains("`filesystem`"));
    }

    /// Without an existing system message, the driver inserts one.
    #[test]
    fn augment_request_inserts_system_message_when_missing() {
        let driver = driver();
        let mut request = concerto_core::types::CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                content: "do the thing".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            tools: None,
            ..Default::default()
        };
        driver.augment_request(&mut request);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, Role::System);
        assert!(request.messages[0].content.contains("## How to call a tool"));
    }

    /// ADR-66 §4 engagement: engages for known tool-less families
    /// (Zen-served Muse models), never for plugin providers (hard gate,
    /// decision (a)), never for capable models, and never for near-misses.
    #[test]
    fn fallback_engagement_follows_capability_resolution() {
        assert!(fallback_engaged("opencode", "muse-v2"));
        assert!(fallback_engaged("opencode", "muse-v3"));
        assert!(
            !fallback_engaged("opencode", "muse-spark-1.3-contributor-free"),
            "the ADR-66 near-miss keeps native (OpenAI-compatible) tools"
        );
        assert!(!fallback_engaged("openai", "gpt-4o"));
        assert!(!fallback_engaged("anthropic", "claude-sonnet-4"));
        assert!(
            !fallback_engaged("plugin:my-llm", "any"),
            "plugin providers are hard-gated to AnswerOnly — the driver must not bypass it"
        );
        // Unknown models attempt native first (provider default).
        assert!(!fallback_engaged("opencode", "big-pickle"));
    }
}
