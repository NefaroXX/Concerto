//! Ollama chat dialect (`POST /api/chat`).
//!
//! This adapter lowers a canonical [`CompletionRequest`] to the wire body of
//! Ollama's `/api/chat` endpoint. The logic here is a verbatim port of the
//! body builder that previously lived in `crate::ollama`; only the seam
//! changed, not the bytes.
//!
//! # Wire contract
//!
//! - Messages keep the canonical roles directly (`system`/`user`/`assistant`/
//!   `tool`), unlike the Anthropic/Gemini families which fold system and tool
//!   messages into other roles.
//! - Assistant messages follow the OpenAI shape: `content` (null when empty)
//!   plus a `tool_calls` array with `id`/`type`/`function{name,arguments}`
//!   where `arguments` is a JSON-encoded string.
//! - Tool messages carry only the **first** tool result (its content rendered
//!   as a string) plus a `tool_call_id` from the first result.
//! - Tools: OpenAI-style `tools:[{"type":"function","function":{name,
//!   description, parameters}}]`.
//! - Options: `options.temperature` only when the request sets a temperature.
//! - Forced tool choices are unsupported: [`ToolChoice::Required`] and
//!   [`ToolChoice::Forced`] are ignored on the wire (with a warning), while
//!   `Auto`/`None` are no-ops.
//!
//! This family never echoes `reasoning_content` back on assistant messages
//! (Ollama has no such field), so the `ReasoningEcho` argument is accepted for
//! the uniform [`Dialect`] signature but has no effect on the wire body.

use concerto_core::types::{CompletionRequest, Role, ToolChoice, ToolDefinition};

use super::schema_sanitize::sanitize_tool_schema;
use super::{Dialect, ReasoningEcho};

/// The Ollama chat dialect (`/api/chat`).
///
/// Stateless unit struct; construct one (or keep one per provider) and call
/// [`Dialect::render_chat_body`] for each completion request. See the module
/// docs for the wire contract it implements.
pub struct OllamaChatDialect;

impl Dialect for OllamaChatDialect {
    fn kind(&self) -> &'static str {
        "ollama"
    }

    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        model: &str,
        _echo: ReasoningEcho,
    ) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| {
                let mut msg = serde_json::json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                        _ => "unknown",
                    },
                });

                match m.role {
                    Role::Assistant => {
                        // Content can be null when there are only tool calls.
                        if m.content.is_empty() {
                            msg["content"] = serde_json::Value::Null;
                        } else {
                            msg["content"] = serde_json::Value::String(m.content.clone());
                        }
                        if let Some(ref tool_calls) = m.tool_calls {
                            let calls: Vec<serde_json::Value> = tool_calls
                                .iter()
                                .map(|tc| {
                                    // Non-object arguments must not serialize to
                                    // the string `"null"` / `"\"ls\""` — the
                                    // OpenAI-style schema rejects that with
                                    // `HTTP 400: function.arguments must be a
                                    // JSON object`.
                                    let arguments = serde_json::to_string(
                                        &crate::protocol::ensure_arguments_object(
                                            tc.arguments.clone(),
                                        ),
                                    )
                                    .unwrap_or_else(|_| "{}".to_string());

                                    serde_json::json!({
                                        "id": tc.id,
                                        "type": "function",
                                        "function": {
                                            "name": tc.name,
                                            "arguments": arguments,
                                        }
                                    })
                                })
                                .collect();
                            if !calls.is_empty() {
                                msg["tool_calls"] = serde_json::Value::Array(calls);
                            }
                        }
                    }
                    Role::Tool => {
                        if let Some(ref results) = m.tool_results {
                            // Multiple tool results become separate messages.
                            // For simplicity, we flatten by taking the first
                            // result's content and using the id as tool_call_id.
                            if let Some(result) = results.first() {
                                let content = match &result.content {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                msg["content"] = serde_json::Value::String(content);
                                msg["tool_call_id"] = serde_json::Value::String(result.id.clone());
                            } else {
                                msg["content"] = serde_json::Value::String(m.content.clone());
                            }
                        } else {
                            msg["content"] = serde_json::Value::String(m.content.clone());
                        }
                    }
                    _ => {
                        msg["content"] = serde_json::Value::String(m.content.clone());
                    }
                }

                msg
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            // The request's `stream` flag is honored on the wire: the
            // weak-model tier (see `schema_loose::non_streaming_transport_active`)
            // forces `false` in the connector so tool-call arguments arrive
            // whole instead of as streamed deltas.
            "stream": request.stream,
        });

        if let Some(temp) = request.temperature {
            body["options"]["temperature"] = serde_json::json!(temp);
        }

        // Ollama does not support forced tool_choice. Warn if caller asked for
        // anything other than Auto/None (which are no-ops).
        if let Some(tc) = &request.tool_choice {
            match tc {
                ToolChoice::Required | ToolChoice::Forced(_) => {
                    tracing::warn!(
                        "Ollama does not support forced tool_choice={tc:?}; falling back to auto"
                    );
                }
                _ => {}
            }
        }

        // Include tool definitions in the request so Ollama can call them.
        if let Some(tools) = &request.tools {
            let ollama_tools = build_ollama_tools(tools);
            if !ollama_tools.is_empty() {
                body["tools"] = serde_json::Value::Array(ollama_tools);
            }
        }

        body
    }
}

// ---------------------------------------------------------------------------
// Family-specific renderers (one canonical value -> the Ollama wire shape).
// Field shape matters for golden tests: keep these builds byte-identical to the
// previous `ollama.rs` implementations.
// ---------------------------------------------------------------------------

/// Convert `ToolDefinition`s into Ollama's OpenAI-style tools JSON array.
///
/// Each tool's `parameters` schema is sanitized via
/// [`sanitize_tool_schema`] to strip draft-2020-12 constructs that
/// forwarder-gateways and free-tier pilots reject on the wire.
fn build_ollama_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            let mut params = t.parameters.clone();
            sanitize_tool_schema(&mut params);
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": params,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{Message, SubmitDesignDocInput, ToolCall, ToolResult};

    fn render(request: &CompletionRequest, model: &str) -> serde_json::Value {
        OllamaChatDialect.render_chat_body(request, model, ReasoningEcho::IfPresent)
    }

    /// The same canonical multi-turn fixture used across the family adapters:
    /// system, user, assistant-with-tool-calls (with captured reasoning),
    /// assistant-with-reasoning, a tool result, and a follow-up user turn,
    /// plus a tool list and a forced tool choice.
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
    fn dialect_kind_is_ollama() {
        assert_eq!(OllamaChatDialect.kind(), "ollama");
    }

    #[test]
    fn stream_flag_is_honored() {
        let streaming = CompletionRequest { stream: true, ..Default::default() };
        assert_eq!(render(&streaming, "llama3.3")["stream"], true);

        // `stream: false` renders verbatim — this is how weak-model
        // completions are requested non-streamed so their tool-call
        // arguments arrive whole.
        let non_streaming = CompletionRequest { stream: false, ..Default::default() };
        assert_eq!(render(&non_streaming, "llama3.3")["stream"], false);
    }

    #[test]
    fn system_messages_keep_system_role() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are a helpful assistant.");
    }

    #[test]
    fn user_messages_keep_user_role() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello!");
    }

    #[test]
    fn assistant_tool_calls_serialize_as_openai_style_function_calls() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().unwrap();
        let assistant = &msgs[2];

        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "Here is the file list.");
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "shell");
        assert_eq!(tcs[0]["function"]["arguments"], serde_json::json!("{\"command\":\"ls\"}"));
        assert!(tcs[0]["function"]["arguments"].is_string(), "arguments must be a JSON string");
    }

    /// A `ToolCall` carrying `Value::Null` arguments must serialize to the
    /// wire-safe string `"{}"` — never `"null"` — mirroring the OpenAI-compat
    /// family (see `openai_compat.rs`).
    #[test]
    fn null_arguments_coerce_to_empty_object_string_on_the_wire() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_null".into(),
                    name: "no_arg_tool".into(),
                    arguments: serde_json::Value::Null,
                }]),
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().unwrap();
        let tcs = msgs[0]["tool_calls"].as_array().unwrap();
        assert!(
            tcs[0]["function"]["arguments"].is_string(),
            "arguments must remain a JSON string on the Ollama wire"
        );
        assert_eq!(
            tcs[0]["function"]["arguments"],
            serde_json::json!("{}"),
            "Null arguments must serialize to the wire-safe string `\"{{}}\"`"
        );
    }

    /// ADR-46 parity: Ollama never echoes `reasoning_content` onto assistant
    /// messages; a reasoning-only assistant renders identically without it.
    #[test]
    fn assistant_reasoning_is_not_echoed() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");

        let serialized = serde_json::to_string(&body).expect("body serializes");
        assert!(
            !serialized.contains("reasoning"),
            "ollama body must not carry reasoning_content: {serialized}"
        );

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "Here");
        assert!(msgs[3].get("tool_calls").is_none(), "no tool calls on the reasoning-only turn");
    }

    /// Ollama flattens tool results by taking the FIRST result's content (as a
    /// plain string, JSON rendered) and its id as `tool_call_id`.
    #[test]
    fn tool_results_use_first_result_as_tool_call_id() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().unwrap();
        let tool_msg = &msgs[4];

        assert_eq!(tool_msg["role"], "tool");
        assert!(tool_msg["content"].is_string(), "content must be a string");
        assert_eq!(tool_msg["content"], "{\"output\":\"file list\"}");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
    }

    #[test]
    fn tools_serialize_as_function_type() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "test_tool");
        assert_eq!(tools[0]["function"]["description"], "A test tool");
        assert_eq!(tools[0]["function"]["parameters"]["properties"]["input"]["type"], "string");
    }

    #[test]
    fn temperature_goes_under_options() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        assert_eq!(body["options"]["temperature"], 0.5);
    }

    #[test]
    fn no_temperature_means_no_options_field() {
        let request = CompletionRequest::default();
        let body = render(&request, "llama3.3");
        assert!(body.get("options").is_none(), "no temperature => no options field");
    }

    /// Forced tool choices are unsupported: Ollama logs a warning and never
    /// emits a `tool_choice` field.
    #[test]
    fn forced_tool_choice_is_dropped() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");
        assert!(body.get("tool_choice").is_none(), "ollama must not emit tool_choice");
    }

    /// Exact serialized body is part of the Ollama wire contract: golden-test
    /// it so a refactor that reorders or renames fields fails loudly.
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
            // Streaming is the default transport; the golden body reflects it.
            stream: true,
            ..Default::default()
        };
        let body = render(&request, "gemma3");
        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            r#"{"messages":[{"content":"You are a test assistant.","role":"system"},{"content":"Hello","role":"user"}],"model":"gemma3","stream":true}"#
        );
    }

    /// VALUE-level parity: the full fixture renders to exactly the Ollama shape
    /// the previous `ollama.rs` builder produced.
    #[test]
    fn full_fixture_value_snapshot() {
        let request = fixture_request();
        let body = render(&request, "llama3.3");

        let expected = serde_json::json!({
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Hello!"},
                {
                    "role": "assistant",
                    "content": "Here is the file list.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"command\":\"ls\"}"
                            }
                        }
                    ]
                },
                {"role": "assistant", "content": "Here"},
                {
                    "role": "tool",
                    "content": "{\"output\":\"file list\"}",
                    "tool_call_id": "call_1"
                },
                {"role": "user", "content": "Run that again."}
            ],
            "model": "llama3.3",
            "stream": true,
            "options": {"temperature": 0.5},
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "test_tool",
                        "description": "A test tool",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "input": {"type": "string"}
                            },
                            "required": ["input"]
                        }
                    }
                }
            ]
        });

        assert_eq!(body, expected);
    }

    /// Contract 9 (Ollama): the real schemars-generated `submit_design_doc`
    /// schema survives the OpenAI-style tools builder. Ollama cannot honor a
    /// forced tool choice (it warns and falls back to auto), so the test only
    /// asserts the tool payload shape.
    #[test]
    fn submit_design_doc_schema_flows_through_tools_builder() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let tools = vec![ToolDefinition {
            name: "submit_design_doc".into(),
            description: "Submit a structured design document.".into(),
            parameters: serde_json::to_value(&schema).unwrap(),
        }];
        let tools_json = build_ollama_tools(&tools);
        assert_eq!(tools_json[0]["type"], "function");
        assert_eq!(tools_json[0]["function"]["name"], "submit_design_doc");
        let params: serde_json::Value =
            serde_json::from_value(tools_json[0]["function"]["parameters"].clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));
    }

    /// Oracle guard F-1: a `Role::Tool` message with multiple results flattens
    /// to a *single* tool message carrying the FIRST result's content and id —
    /// a second result must not leak into the body.
    #[test]
    fn multiple_tool_results_flatten_to_first_result_only() {
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
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 1, "ollama collapses results into one tool message");
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["content"], "first result", "first result's content wins");
        assert_eq!(msgs[0]["tool_call_id"], "call_a", "first result's id wins");
        assert_ne!(
            msgs[0]["content"], "second result",
            "a second distinct result must not appear in the body"
        );
    }

    /// Oracle guard F-2: `tools: Some(vec![])` must not emit a `tools` field
    /// (an empty array would be rejected by Ollama's `/api/chat`).
    #[test]
    fn empty_tools_list_omits_tools_field() {
        let request = CompletionRequest { tools: Some(vec![]), ..Default::default() };
        let body = render(&request, "llama3.3");

        assert!(body.get("tools").is_none(), "empty tools => no tools field");
        assert!(body.get("tool_choice").is_none(), "ollama never emits tool_choice");
    }

    /// Oracle guard F-3: a tool-role message with no `tool_results` still
    /// renders as a tool message with its raw content and no `tool_call_id` —
    /// it must not vanish from the wire history.
    #[test]
    fn tool_message_without_results_keeps_content_and_no_call_id() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: "plain tool output".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["content"], "plain tool output");
        assert!(msgs[0].get("tool_call_id").is_none(), "no tool_results => no tool_call_id field");
    }

    /// Oracle guard F-4: an explicit `temperature: Some(0.0)` still renders
    /// `options.temperature` — zero is a real value, not an "unset" sentinel.
    #[test]
    fn zero_temperature_still_emits_options_field() {
        let request = CompletionRequest { temperature: Some(0.0), ..Default::default() };
        let body = render(&request, "llama3.3");

        assert_eq!(body["options"]["temperature"], 0.0);
    }

    /// Oracle guard F-6: unlike the Anthropic/Gemini families, Ollama keeps
    /// each `Role::System` message as its own message in the array, in order —
    /// there is no "last wins" collapsing.
    #[test]
    fn multiple_system_messages_are_all_kept_in_order() {
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
        let body = render(&request, "llama3.3");
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 2, "both system messages are kept");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "first system");
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["content"], "second system");
    }
}
