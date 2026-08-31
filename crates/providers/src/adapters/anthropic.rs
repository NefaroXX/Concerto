//! Anthropic Messages API chat dialect (`POST /v1/messages`).
//!
//! This adapter lowers a canonical [`CompletionRequest`] to the wire body of
//! Anthropic's Messages API. The logic here is a verbatim port of the body
//! builder that previously lived in `crate::anthropic`; only the seam changed,
//! not the bytes.
//!
//! # Wire contract
//!
//! - System prompt: a top-level `system` string (the last `Role::System`
//!   message wins); system messages are not part of `messages`.
//! - Messages: `user`/`assistant` roles with a `content` *array* of typed
//!   blocks — `text`, `tool_use` (from assistant `tool_calls`) and
//!   `tool_result` (from `Role::Tool` messages, which are sent as `user`).
//! - Tools: native Anthropic shape (`name`, `description`, `input_schema`).
//! - `tool_choice`: `{"type":"auto"|"none"|"any"}` or `{"type":"tool",
//!   "name": ...}` for a forced tool. Only set when the request carries one.
//! - `max_tokens` is mandatory and defaults to 4096; `stream` is hardcoded
//!   `true`.
//!
//! This family never echoes `reasoning_content` back on assistant messages
//! (Anthropic has no such field), so the `ReasoningEcho` argument is accepted
//! for the uniform [`Dialect`] signature but has no effect on the wire body.

use concerto_core::types::{CompletionRequest, Role, ToolChoice, ToolDefinition};
use serde_json::json;

use super::schema_sanitize::sanitize_tool_schema;
use super::{Dialect, ReasoningEcho};

/// The Anthropic Messages API chat dialect.
///
/// Stateless unit struct; construct one (or keep one per provider) and call
/// [`Dialect::render_chat_body`] for each completion request. See the module
/// docs for the wire contract it implements.
pub struct AnthropicChatDialect;

impl Dialect for AnthropicChatDialect {
    fn kind(&self) -> &'static str {
        "anthropic"
    }

    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        model: &str,
        _echo: ReasoningEcho,
    ) -> serde_json::Value {
        let mut system_prompt: Option<String> = None;
        let mut msgs = Vec::new();
        for msg in &request.messages {
            if let Role::System = msg.role {
                system_prompt = Some(msg.content.clone());
                continue;
            }

            let role_str = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "user",
                _ => "user",
            };
            let mut content = Vec::new();

            if !msg.content.is_empty() {
                content.push(json!({"type": "text", "text": msg.content }));
            }

            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
            }

            if let Some(tool_results) = &msg.tool_results {
                for tr in tool_results {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tr.id,
                        "content": tr.content,
                    }));
                }
            }
            msgs.push(json!({
                "role": role_str,
                "content": content,
            }));
        }

        // The base body order mirrors the previous `anthropic.rs` builder:
        // model, messages, stream, max_tokens, then the optional fields.
        let mut body = json!({
            "model": model,
            "messages": msgs,
            "stream": true,
            "max_tokens": request.max_tokens.unwrap_or(4096),
        });
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(system) = system_prompt {
            body["system"] = json!(system);
        }
        if let Some(tools) = &request.tools {
            body["tools"] = json!(build_anthropic_tools(tools));
        }

        // Map generic ToolChoice to Anthropic's native format.
        if let Some(tc) = &request.tool_choice {
            body["tool_choice"] = build_anthropic_tool_choice(tc);
        }

        body
    }

    /// Mark the two Anthropic prompt-cache breakpoints on the rendered body
    /// (ADR-48 decision 3: prefix discipline, opt-in extension point):
    ///
    /// 1. The top-level `system` prompt is wrapped as a single typed text block
    ///    object with an ephemeral `cache_control`, so the system prompt joins
    ///    the cached prefix (Anthropic accepts a single content block object
    ///    for `system`). Unchanged when `system` is absent or already wrapped.
    /// 2. The FIRST `user` message's FIRST `text` block gains an ephemeral
    ///    `cache_control` — Anthropic's documented pattern for caching the
    ///    conversation prefix. Subsequent user turns are left unmarked.
    ///
    /// `tool_use`/`tool_result` blocks are deliberately untouched (out of scope
    /// for this milestone). The method is idempotent: each breakpoint is only
    /// written when not already present, so applying it twice yields the same
    /// body the second time.
    fn apply_cache_breakpoints(&self, body: &mut serde_json::Value) {
        // 1. System prompt. Only a plain string is wrapped; an already-wrapped
        //    (object) system value is left alone, which also keeps the call
        //    idempotent on repeat application.
        if let Some(text) = body.get("system").and_then(|s| s.as_str()) {
            body["system"] = json!({
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            });
        }

        // 2. First user message, first text block only.
        let mut user_seen = false;
        if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for msg in messages {
                if msg["role"] != "user" {
                    continue;
                }
                if user_seen {
                    continue;
                }
                user_seen = true;
                if let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                    if let Some(first_text) = blocks.iter_mut().find(|b| b["type"] == "text") {
                        if first_text.get("cache_control").is_none() {
                            first_text["cache_control"] = json!({"type": "ephemeral"});
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Family-specific renderers (one canonical value -> the Anthropic wire shape).
// Field shape matters for golden tests: keep these builds byte-identical to the
// previous `anthropic.rs` implementations.
// ---------------------------------------------------------------------------

/// Convert `ToolDefinition`s into Anthropic's native tools JSON array
/// (`name`, `description`, `input_schema`).
///
/// Each tool's `input_schema` is sanitized via [`sanitize_tool_schema`]
/// to strip draft-2020-12 constructs that some Anthropic models reject.
fn build_anthropic_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            let mut params = t.parameters.clone();
            sanitize_tool_schema(&mut params);
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": params,
            })
        })
        .collect()
}

/// Map the generic `ToolChoice` to Anthropic's native `tool_choice` forms.
fn build_anthropic_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Forced(name) => json!({"type": "tool", "name": name}),
        _ => json!({"type": "auto"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{Message, Role, SubmitDesignDocInput, ToolCall, ToolResult};

    fn render(request: &CompletionRequest, model: &str) -> serde_json::Value {
        AnthropicChatDialect.render_chat_body(request, model, ReasoningEcho::IfPresent)
    }

    /// Render a request and apply the cache breakpoints, mirroring the
    /// provider connector's body-building path.
    fn render_with_breakpoints(request: &CompletionRequest, model: &str) -> serde_json::Value {
        let mut body = render(request, model);
        AnthropicChatDialect.apply_cache_breakpoints(&mut body);
        body
    }

    /// A plain text-only message with every optional field empty.
    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    /// A canonical multi-turn fixture: system, user, assistant-with-tool-calls
    /// (with captured reasoning), assistant-with-reasoning, a tool result, and
    /// a follow-up user turn, plus a tool list and a forced tool choice.
    fn fixture_request() -> CompletionRequest {
        CompletionRequest {
            model: String::new(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: "You are a helpful assistant.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Hello!".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Assistant,
                    content: "Here is the file list.".into(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                    }]),
                    tool_results: None,
                    reasoning_content: Some("I'll list the files.".into()),
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Assistant,
                    content: "Here".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: Some("compact reasoning".into()),
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::Tool,
                    content: String::new(),
                    tool_calls: None,
                    tool_results: Some(vec![ToolResult {
                        id: "call_1".into(),
                        name: "shell".into(),
                        content: serde_json::json!({"output": "file list"}),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Run that again.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            tools: Some(vec![ToolDefinition {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input": {"type": "string"}
                    },
                    "required": ["input"]
                }),
            }]),
            tool_choice: Some(ToolChoice::Forced("test_tool".into())),
            temperature: Some(0.5),
            max_tokens: Some(4096),
            stream: true,
        }
    }

    #[test]
    fn dialect_kind_is_anthropic() {
        assert_eq!(AnthropicChatDialect.kind(), "anthropic");
    }

    #[test]
    fn stream_is_always_true() {
        let request = CompletionRequest { stream: false, ..Default::default() };
        let body = render(&request, "claude-3-5-sonnet");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn max_tokens_defaults_to_4096() {
        let request = CompletionRequest::default();
        let body = render(&request, "claude-3-5-sonnet");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn max_tokens_is_set_when_specified() {
        let request = CompletionRequest { max_tokens: Some(2048), ..Default::default() };
        let body = render(&request, "claude-3-5-sonnet");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn temperature_is_set_when_specified() {
        let request = CompletionRequest { temperature: Some(0.5), ..Default::default() };
        let body = render(&request, "claude-3-5-sonnet");
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn system_renders_as_top_level_string_not_a_message() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");

        assert_eq!(body["system"], "You are a helpful assistant.");
        let msgs = body["messages"].as_array().expect("should have messages");
        assert!(
            msgs.iter().all(|m| m["role"] != "system"),
            "system messages must not appear in the messages array"
        );
    }

    #[test]
    fn user_messages_render_text_content_blocks() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "Hello!");
    }

    #[test]
    fn assistant_tool_calls_render_tool_use_blocks_after_text() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");
        let msgs = body["messages"].as_array().unwrap();
        let blocks = msgs[1]["content"].as_array().unwrap();

        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Here is the file list.");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "shell");
        assert_eq!(blocks[1]["input"], serde_json::json!({"command": "ls"}));
    }

    /// ADR-46 parity: Anthropic never echoes `reasoning_content`. An assistant
    /// message that carries captured reasoning renders exactly as if it did
    /// not — the text block only.
    #[test]
    fn assistant_reasoning_is_not_echoed() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");

        let serialized = serde_json::to_string(&body).expect("body serializes");
        assert!(
            !serialized.contains("reasoning"),
            "anthropic body must not carry reasoning_content: {serialized}"
        );

        // The reasoning-only assistant message below the tool-use turn keeps a
        // bare text block.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "assistant");
        let content = msgs[2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Here");
    }

    #[test]
    fn tool_results_render_as_tool_result_blocks_with_user_role() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");
        let msgs = body["messages"].as_array().unwrap();

        assert_eq!(msgs[3]["role"], "user", "tool results fold into a user message");
        let content = msgs[3]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_1");
        assert_eq!(content[0]["content"], serde_json::json!({"output": "file list"}));
    }

    #[test]
    fn tools_serialize_with_input_schema() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");

        let tools = body["tools"].as_array().expect("should have tools array");
        let tool = &tools[0];
        assert_eq!(tool["name"], "test_tool");
        assert_eq!(tool["description"], "A test tool");
        assert!(tool.get("input_schema").is_some(), "anthropic uses input_schema");
        assert_eq!(tool["input_schema"]["properties"]["input"]["type"], "string");
    }

    #[test]
    fn tool_choice_auto_any_none_and_forced_forms() {
        for (choice, expected) in [
            (ToolChoice::Auto, serde_json::json!({"type": "auto"})),
            (ToolChoice::None, serde_json::json!({"type": "none"})),
            (ToolChoice::Required, serde_json::json!({"type": "any"})),
            (
                ToolChoice::Forced("submit_design_doc".into()),
                serde_json::json!({"type": "tool", "name": "submit_design_doc"}),
            ),
        ] {
            let request = CompletionRequest {
                tools: Some(vec![ToolDefinition {
                    name: "submit_design_doc".into(),
                    description: "Submit a structured design document.".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]),
                tool_choice: Some(choice),
                ..Default::default()
            };
            let body = render(&request, "claude-3-5-sonnet");
            assert_eq!(body["tool_choice"], expected);
        }
    }

    #[test]
    fn tool_choice_absent_means_no_field() {
        let mut request = fixture_request();
        request.tool_choice = None;
        let body = render(&request, "claude-3-5-sonnet");
        assert!(body.get("tool_choice").is_none(), "no tool_choice => no field");
    }

    /// Exact serialized body (keys, values, ordering) is part of the Anthropic
    /// wire contract: golden-test it so a refactor that reorders or renames
    /// fields fails loudly.
    #[test]
    fn golden_serialized_body_system_and_user() {
        let request = CompletionRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: "You are a test assistant.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::User,
                    content: "Hello".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };
        let body = render(&request, "claude-3-5-sonnet");
        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            r#"{"max_tokens":4096,"messages":[{"content":[{"text":"Hello","type":"text"}],"role":"user"}],"model":"claude-3-5-sonnet","stream":true,"system":"You are a test assistant."}"#
        );
    }

    /// VALUE-level parity: the full fixture (all roles, tool calls, reasoning,
    /// tools, tool_choice) renders to exactly the Anthropic shape the previous
    /// `anthropic.rs` builder produced.
    #[test]
    fn full_fixture_value_snapshot() {
        let request = fixture_request();
        let body = render(&request, "claude-3-5-sonnet");

        let expected = serde_json::json!({
            "max_tokens": 4096,
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "Hello!"}]
                },
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Here is the file list."},
                        {
                            "type": "tool_use",
                            "id": "call_1",
                            "name": "shell",
                            "input": {"command": "ls"}
                        }
                    ]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Here"}]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "call_1",
                            "content": {"output": "file list"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "Run that again."}]
                }
            ],
            "model": "claude-3-5-sonnet",
            "stream": true,
            "system": "You are a helpful assistant.",
            "temperature": 0.5,
            "tool_choice": {"type": "tool", "name": "test_tool"},
            "tools": [
                {
                    "name": "test_tool",
                    "description": "A test tool",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"}
                        },
                        "required": ["input"]
                    }
                }
            ]
        });

        assert_eq!(body, expected);
    }

    /// Contract 9 (Anthropic): the real schemars-generated
    /// `submit_design_doc` schema survives the native tools builder
    /// (`input_schema`), and the forced tool choice maps to Anthropic's
    /// `{"type":"tool","name":...}`.
    #[test]
    fn submit_design_doc_schema_flows_through_tools_builder() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let tools = vec![ToolDefinition {
            name: "submit_design_doc".into(),
            description: "Submit a structured design document.".into(),
            parameters: serde_json::to_value(&schema).unwrap(),
        }];
        let tools_json = build_anthropic_tools(&tools);
        assert_eq!(tools_json[0]["name"], "submit_design_doc");
        let params: serde_json::Value =
            serde_json::from_value(tools_json[0]["input_schema"].clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));

        let choice = build_anthropic_tool_choice(&ToolChoice::Forced("submit_design_doc".into()));
        assert_eq!(choice, serde_json::json!({"type": "tool", "name": "submit_design_doc"}));
    }

    /// Oracle guard F-1: multiple tool results fold into a *single* user-role
    /// message whose `content` array carries one `tool_result` block per
    /// result, each with its own `tool_use_id`.
    #[test]
    fn multiple_tool_results_fold_into_one_user_message() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![
                    ToolResult {
                        id: "call_a".into(),
                        name: "tool_a".into(),
                        content: serde_json::json!("first result"),
                    },
                    ToolResult {
                        id: "call_b".into(),
                        name: "tool_b".into(),
                        content: serde_json::json!("second result"),
                    },
                ]),
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, "claude-3-5-sonnet");
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 1, "one folded user message");
        assert_eq!(msgs[0]["role"], "user", "tool results fold into a user message");
        let content = msgs[0]["content"].as_array().expect("content is an array");
        assert_eq!(content.len(), 2, "one tool_result block per result");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_a");
        assert_eq!(content[0]["content"], serde_json::json!("first result"));
        assert_eq!(content[1]["type"], "tool_result");
        assert_eq!(content[1]["tool_use_id"], "call_b");
        assert_eq!(content[1]["content"], serde_json::json!("second result"));
    }

    /// Oracle guard F-2 (Anthropic parity quirk): `tools: Some(vec![])` still
    /// emits an empty `"tools": []` array on the body — intentionally *different*
    /// from OpenAI and Ollama (which omit the field) and Gemini (which omits the
    /// array). This pins the historical builder's behavior.
    #[test]
    fn empty_tools_list_still_emits_tools_array() {
        let request = CompletionRequest { tools: Some(vec![]), ..Default::default() };
        let body = render(&request, "claude-3-5-sonnet");

        assert!(body.get("tools").is_some(), "anthropic keeps the tools field even when empty");
        assert_eq!(body["tools"], serde_json::json!([]));
    }

    /// Oracle guard F-3: a tool-role message with empty content and *no*
    /// `tool_results` renders as a user message with an empty `content` array —
    /// it must not disappear from the wire history.
    #[test]
    fn tool_message_without_results_renders_empty_user_message() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, "claude-3-5-sonnet");
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user", "tool never has its own anthropic role");
        assert_eq!(msgs[0]["content"], serde_json::json!([]), "empty content array");
    }

    /// Oracle guard F-4: an explicit `temperature: Some(0.0)` still renders the
    /// field — zero is a real value, not an "unset" sentinel.
    #[test]
    fn zero_temperature_still_emits_field() {
        let request = CompletionRequest { temperature: Some(0.0), ..Default::default() };
        let body = render(&request, "claude-3-5-sonnet");

        assert_eq!(body["temperature"], 0.0);
    }

    /// Oracle guard F-6: when several `Role::System` messages are present the
    /// *last* one wins the top-level `system` field, matching the previous
    /// builder's accumulate-and-overwrite loop.
    #[test]
    fn last_system_message_wins() {
        let request = CompletionRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: "first system".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                Message {
                    role: Role::System,
                    content: "second system".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };
        let body = render(&request, "claude-3-5-sonnet");

        assert_eq!(body["system"], "second system", "last system message wins");
        let msgs = body["messages"].as_array().expect("should have messages");
        assert_eq!(msgs.len(), 0, "system messages never appear in the messages array");
    }

    /// ADR-48 decision 3 / Phase 3 M3: the system prompt is wrapped as a single
    /// typed text block with an ephemeral `cache_control`, and the FIRST user
    /// message's FIRST text block is marked. Subsequent user turns (including a
    /// tool-result fold-in and a later plain user) stay unmarked.
    #[test]
    fn cache_breakpoints_mark_system_and_first_user_block() {
        let request = fixture_request();
        let body = render_with_breakpoints(&request, "claude-3-5-sonnet");

        assert_eq!(
            body["system"],
            json!({
                "type": "text",
                "text": "You are a helpful assistant.",
                "cache_control": {"type": "ephemeral"},
            }),
            "system must be wrapped as a single cache-marked text block"
        );

        let msgs = body["messages"].as_array().expect("should have messages");
        // First user message: its first block is a text block and gets the mark.
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(
            msgs[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"}),
            "first user's first text block carries the prefix breakpoint"
        );

        // The other user-role messages (tool result, follow-up user) are
        // NOT marked — only the first user message participates.
        for idx in [3usize, 4] {
            let content = msgs[idx]["content"].as_array().expect("content array");
            for block in content {
                assert!(
                    block.get("cache_control").is_none(),
                    "user message {idx} must not carry a breakpoint: {block}"
                );
            }
        }
        // tool_use / tool_result blocks are out of scope and stay untouched.
        let tool_use = &msgs[1]["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert!(tool_use.get("cache_control").is_none());
    }

    /// ADR-48 prefix discipline: applying the breakpoints twice yields the
    /// identical body (both value and serialized-byte identity), so repeated
    /// passes can never double-wrap a block or reorder the JSON.
    #[test]
    fn cache_breakpoints_are_idempotent() {
        let request = fixture_request();
        let mut once = render_with_breakpoints(&request, "claude-3-5-sonnet");
        let twice = once.clone();
        AnthropicChatDialect.apply_cache_breakpoints(&mut once);

        assert_eq!(once, twice, "second application must be a no-op");
        assert_eq!(
            serde_json::to_string(&once).expect("serializes"),
            serde_json::to_string(&twice).expect("serializes"),
            "serialized identity preserved across the second application"
        );
    }

    /// No system prompt => no `system` key is synthesized; the first user
    /// message is still marked.
    #[test]
    fn cache_breakpoints_system_absent_noop_on_system() {
        let request = CompletionRequest {
            messages: vec![
                msg(Role::User, "hello"),
                msg(Role::Assistant, "hi"),
                msg(Role::User, "again"),
            ],
            ..Default::default()
        };
        let mut body = render(&request, "claude-3-5-sonnet");
        assert!(body.get("system").is_none(), "fixture must have no system");
        AnthropicChatDialect.apply_cache_breakpoints(&mut body);

        assert!(
            body.get("system").is_none(),
            "no system key may be added beyond the original absence"
        );
        let msgs = body["messages"].as_array().expect("should have messages");
        assert_eq!(
            msgs[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"}),
            "first user message must still be marked"
        );
        assert!(
            msgs[2]["content"][0].get("cache_control").is_none(),
            "second user message must stay unmarked"
        );
    }
    /// ADR-48 decision 3 (prefix discipline): two consecutive conversations
    /// with a shared message prefix must render that prefix byte-identically,
    /// both with and without cache breakpoints. The breakpoint lands on the
    /// first user message, which sits inside the shared prefix, so it never
    /// perturbs the stable window.
    #[test]
    fn message_prefix_stable_across_consecutive_turns() {
        // Short = [system, user1, assistant1, user2]; extended = short +
        // [assistant2, user3]. Both render the system prompt out-of-band as the
        // top-level `system` string, so the messages-array common prefix is
        // [user1, assistant1, user2] — anything the short render emits must be
        // byte-identical to the extended render's leading messages.
        let all = vec![
            msg(Role::System, "You are a test assistant."),
            msg(Role::User, "First user turn."),
            msg(Role::Assistant, "First assistant reply."),
            msg(Role::User, "Second user turn."),
            msg(Role::Assistant, "Second assistant reply."),
            msg(Role::User, "Third user turn."),
        ];
        let short = CompletionRequest { messages: all[..4].to_vec(), ..Default::default() };
        let extended = CompletionRequest { messages: all.clone(), ..Default::default() };

        for breakpoints in [false, true] {
            let mut short_body = render(&short, "claude-3-5-sonnet");
            let mut extended_body = render(&extended, "claude-3-5-sonnet");
            if breakpoints {
                AnthropicChatDialect.apply_cache_breakpoints(&mut short_body);
                AnthropicChatDialect.apply_cache_breakpoints(&mut extended_body);
            }
            let short_msgs = short_body["messages"].as_array().expect("messages");
            let extended_msgs = extended_body["messages"].as_array().expect("messages");
            assert!(
                short_msgs.len() < extended_msgs.len(),
                "extended render must carry more messages than the short render"
            );
            assert_eq!(
                serde_json::to_string(short_msgs).expect("serializes"),
                serde_json::to_string(&extended_msgs[..short_msgs.len()]).expect("serializes"),
                "rendered messages must be byte-identical up to the short render's length \
                 (breakpoints = {breakpoints})",
            );
            assert_eq!(
                extended_msgs.len(),
                all.len() - 1,
                "extended render must keep the full conversation (system is out-of-band)"
            );
        }
    }
}
