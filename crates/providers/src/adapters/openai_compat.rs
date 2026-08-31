//! OpenAI-compatible chat dialect.
//!
//! This adapter lowers a canonical [`CompletionRequest`] to the wire body of
//! the `POST /chat/completions` OpenAI-compatible family — the same shape used
//! by OpenAI, OpenRouter, Nvidia NIM, OpenCode Zen, DeepSeek, Together, and
//! other gateways. The logic here is a verbatim port of the body builder that
//! previously lived in `crate::openai`; only the seam changed, not the bytes.
//!
//! # DeepSeek reasoning contract
//!
//! DeepSeek-style endpoints (OpenCode Zen, DeepSeek, NIM) return HTTP 400 when
//! an assistant message that produced tool calls is re-sent to the API without
//! `reasoning_content` while the model is in "thinking" mode. The documented
//! contract: once tool calls are in history, **every** assistant message must
//! carry `reasoning_content` — or an empty string (`""`) when no reasoning was
//! captured. [`ReasoningEcho::Always`] implements exactly that (empty string
//! satisfies the contract when only a tool call exists), while
//! [`ReasoningEcho::IfPresent`] emits the field only when captured reasoning is
//! present. Captured reasoning is always echoed verbatim; it is never rebuilt
//! from prose.

use concerto_core::types::{CompletionRequest, Message, Role, ToolChoice, ToolDefinition};

use super::schema_sanitize::sanitize_tool_schema;
use super::{Dialect, ReasoningEcho};

/// The OpenAI-compatible chat dialect (`/chat/completions`).
///
/// Stateless unit struct; construct one (or keep one per provider) and call
/// [`Dialect::render_chat_body`] for each completion request. See the module
/// docs for the DeepSeek reasoning contract this dialect implements.
pub struct OpenAiChatDialect;

impl Dialect for OpenAiChatDialect {
    fn kind(&self) -> &'static str {
        "openai-compat"
    }

    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        model: &str,
        echo: ReasoningEcho,
    ) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .flat_map(|m| match m.role {
                Role::System => vec![build_system_message(m)],
                Role::User => vec![build_user_message(m)],
                Role::Assistant => vec![build_assistant_message(m, echo)],
                Role::Tool => build_tool_messages(m),
                _ => vec![],
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }

        if let Some(max_t) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_t);
        }

        if let Some(ref tools) = request.tools {
            let openai_tools = build_openai_tools(tools);
            if !openai_tools.is_empty() {
                body["tools"] = serde_json::Value::Array(openai_tools);
                body["tool_choice"] =
                    build_tool_choice(request.tool_choice.as_ref().unwrap_or(&ToolChoice::Auto));
            }
        }

        body
    }
}

// ---------------------------------------------------------------------------
// Message-level renderers (one canonical message -> zero or more wire messages).
// Field order of the emitted JSON matters for golden tests: keep these builds
// byte-identical to the previous `openai.rs` implementations.
// ---------------------------------------------------------------------------

/// Convert a system message to its OpenAI JSON representation.
fn build_system_message(m: &Message) -> serde_json::Value {
    serde_json::json!({
        "role": "system",
        "content": m.content,
    })
}

/// Convert a user message to its OpenAI JSON representation.
fn build_user_message(m: &Message) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": m.content,
    })
}

/// Convert an assistant message to OpenAI JSON, optionally including
/// `tool_calls` and (per the echo policy) `reasoning_content`.
fn build_assistant_message(m: &Message, echo: ReasoningEcho) -> serde_json::Value {
    let mut msg = serde_json::json!({
        "role": "assistant",
        "content": if m.content.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(m.content.clone())
        },
    });

    match echo {
        ReasoningEcho::IfPresent => {
            if let Some(reasoning) = &m.reasoning_content {
                msg["reasoning_content"] = serde_json::Value::String(reasoning.clone());
            }
        }
        ReasoningEcho::Always => {
            msg["reasoning_content"] =
                serde_json::Value::String(m.reasoning_content.clone().unwrap_or_default());
        }
    }

    if let Some(ref tool_calls) = m.tool_calls {
        let calls: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|tc| {
                // Non-object arguments (e.g. `Value::Null` from a plugin, MCP
                // bridge, or memory adapter producer) must not serialize to the
                // string `"null"` / `"\"ls\""` — upstream rejects that with
                // `HTTP 400: function.arguments must be a JSON object`.
                let arguments = serde_json::to_string(&crate::protocol::ensure_arguments_object(
                    tc.arguments.clone(),
                ))
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

    msg
}

/// Convert a tool-role message into zero or more OpenAI tool-result JSON
/// objects. Each entry in `tool_results` becomes its own message.
fn build_tool_messages(m: &Message) -> Vec<serde_json::Value> {
    if let Some(ref results) = m.tool_results {
        results
            .iter()
            .map(|result| {
                let content = match &result.content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };

                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": result.id,
                    "content": content,
                })
            })
            .collect()
    } else {
        vec![serde_json::json!({
            "role": "tool",
            "content": m.content,
        })]
    }
}

/// Convert a list of `ToolDefinition` into the OpenAI tools JSON array.
///
/// Each tool's `parameters` schema is sanitized via
/// [`sanitize_tool_schema`] to strip draft-2020-12 constructs that
/// forwarder-gateways and free-tier pilots reject on the wire (`$defs`,
/// `$ref`, `$schema`, `prefixItems`).
fn build_openai_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            let mut params = tool.parameters.clone();
            sanitize_tool_schema(&mut params);
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": params,
                }
            })
        })
        .collect()
}

/// Convert a `ToolChoice` into the OpenAI `tool_choice` JSON value.
fn build_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::Forced(name) => serde_json::json!({
            "type": "function",
            "function": { "name": name },
        }),
        _ => serde_json::json!("auto"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{SubmitDesignDocInput, ToolCall, ToolResult};

    fn make_test_request_with_tools() -> CompletionRequest {
        CompletionRequest {
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
            ..Default::default()
        }
    }

    fn render(request: &CompletionRequest, echo: ReasoningEcho) -> serde_json::Value {
        OpenAiChatDialect.render_chat_body(request, "gpt-4o", echo)
    }

    fn assistant_message(
        content: &str,
        tool_calls: Option<Vec<ToolCall>>,
        reasoning: Option<String>,
    ) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls,
            tool_results: None,
            reasoning_content: reasoning,
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn dialect_kind_is_openai_compat() {
        assert_eq!(OpenAiChatDialect.kind(), "openai-compat");
    }

    #[test]
    fn tools_serialize_as_function_type() {
        let request = make_test_request_with_tools();
        let body = render(&request, ReasoningEcho::IfPresent);

        let tools = body["tools"].as_array().expect("should have tools array");
        assert_eq!(tools[0]["type"], "function", "tool type must be 'function'");
    }

    #[test]
    fn tools_serialize_with_function_fields() {
        let request = make_test_request_with_tools();
        let body = render(&request, ReasoningEcho::IfPresent);

        let tools = body["tools"].as_array().expect("should have tools array");
        let tool = &tools[0];

        assert_eq!(tool["function"]["name"], "test_tool");
        assert_eq!(tool["function"]["description"], "A test tool");
        assert!(tool["function"]["parameters"].is_object(), "parameters should be a JSON object");
        assert_eq!(tool["function"]["parameters"]["properties"]["input"]["type"], "string");
    }

    #[test]
    fn assistant_tool_calls_serialize_with_arguments_string() {
        let request = CompletionRequest {
            messages: vec![assistant_message(
                "",
                Some(vec![ToolCall {
                    id: "call_123".into(),
                    name: "test_tool".into(),
                    arguments: serde_json::json!({"input": "hello"}),
                }]),
                None,
            )],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);

        let msgs = body["messages"].as_array().expect("should have messages");
        let assistant_msg = &msgs[0];

        assert_eq!(assistant_msg["role"], "assistant");
        let tool_calls = assistant_msg["tool_calls"].as_array().expect("should have tool_calls");

        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "test_tool");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            serde_json::json!("{\"input\":\"hello\"}"),
            "arguments must be a JSON string"
        );
        // Verify it's actually a string, not an object.
        assert!(
            tool_calls[0]["function"]["arguments"].is_string(),
            "arguments must be a JSON string"
        );
    }

    #[test]
    fn tool_result_messages_serialize_with_role_and_tool_call_id() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: String::new(),
                tool_calls: None,
                tool_results: Some(vec![ToolResult {
                    id: "call_123".into(),
                    name: "test_tool".into(),
                    content: serde_json::json!("operation completed"),
                }]),
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);

        let msgs = body["messages"].as_array().expect("should have messages");
        let tool_msg = &msgs[0];

        assert_eq!(tool_msg["role"], "tool", "role must be 'tool'");
        assert_eq!(tool_msg["tool_call_id"], "call_123", "must have tool_call_id");
        assert!(tool_msg.get("content").is_some(), "must have content field");
    }

    #[test]
    fn system_messages_serialize_with_role_system() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::System,
                content: "You are a helpful assistant.".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are a helpful assistant.");
    }

    #[test]
    fn user_messages_serialize_with_role_user() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                content: "Hello!".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Hello!");
    }

    #[test]
    fn assistant_messages_without_tools_serialize() {
        let request = CompletionRequest {
            messages: vec![assistant_message("I'll help you.", None, None)],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "I'll help you.");
    }

    #[test]
    fn assistant_empty_content_renders_null() {
        let request = CompletionRequest {
            messages: vec![assistant_message("", None, None)],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");
        assert!(msgs[0]["content"].is_null(), "empty assistant content must render as JSON null");
    }

    #[test]
    fn stream_is_set_in_body() {
        let request = CompletionRequest { stream: true, ..Default::default() };
        let body = render(&request, ReasoningEcho::IfPresent);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn model_is_set_in_body() {
        let request = CompletionRequest::default();
        let body =
            OpenAiChatDialect.render_chat_body(&request, "gpt-4-turbo", ReasoningEcho::IfPresent);
        assert_eq!(body["model"], "gpt-4-turbo");
    }

    #[test]
    fn max_tokens_is_set_when_specified() {
        let request = CompletionRequest { max_tokens: Some(4096), ..Default::default() };
        let body = render(&request, ReasoningEcho::IfPresent);
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn temperature_is_set_when_specified() {
        let request = CompletionRequest { temperature: Some(0.5), ..Default::default() };
        let body = render(&request, ReasoningEcho::IfPresent);
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn assistant_tool_calls_without_arguments_still_serialize() {
        let request = CompletionRequest {
            messages: vec![assistant_message(
                "",
                Some(vec![ToolCall {
                    id: "call_empty".into(),
                    name: "no_arg_tool".into(),
                    arguments: serde_json::json!({}),
                }]),
                None,
            )],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().unwrap();
        let tcs = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["function"]["name"], "no_arg_tool");
        assert!(tcs[0]["function"]["arguments"].is_string());
    }

    /// A `ToolCall` carrying `Value::Null` arguments (from a plugin, MCP
    /// bridge, or memory-adapter producer) must still serialize to a wire-safe
    /// argument string: `"{}"`, never the string `"null"` which upstream
    /// rejects with `HTTP 400: function.arguments must be a JSON object`.
    #[test]
    fn null_arguments_coerce_to_empty_object_string_on_the_wire() {
        let request = CompletionRequest {
            messages: vec![assistant_message(
                "",
                Some(vec![ToolCall {
                    id: "call_null".into(),
                    name: "no_arg_tool".into(),
                    arguments: serde_json::Value::Null,
                }]),
                None,
            )],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().unwrap();
        let tcs = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["function"]["name"], "no_arg_tool");
        assert!(
            tcs[0]["function"]["arguments"].is_string(),
            "arguments must remain a JSON string on the OpenAI-compatible wire"
        );
        assert_eq!(
            tcs[0]["function"]["arguments"],
            serde_json::json!("{}"),
            "Null arguments must serialize to the wire-safe string `\"{{}}\"`"
        );
    }

    /// Building a chat body with a system message includes the system role and
    /// keeps message order (system first, user second).
    #[test]
    fn build_result_body_preserves_system_then_user_order() {
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
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system", "first message should be system");
        assert_eq!(msgs[0]["content"], "You are a test assistant.");
        assert_eq!(msgs[1]["role"], "user", "second message should be user");
        assert_eq!(msgs[1]["content"], "Hello");
    }

    /// The exact serialized body (keys, values, field order) is part of the
    /// wire contract for the OpenAI-compatible family: golden-test it so a
    /// refactor that reorders or renames fields fails loudly.
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
        let body = render(&request, ReasoningEcho::IfPresent);
        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            r#"{"messages":[{"content":"You are a test assistant.","role":"system"},{"content":"Hello","role":"user"}],"model":"gpt-4o","stream":true}"#
        );
    }

    /// Contract 9 (OpenAI + the OpenAI-compatible family: OpenRouter, Nvidia
    /// NIM and OpenCode Zen all delegate to `OpenAiProvider::stream_completion`
    /// → dialect rendering): the real schemars-generated `submit_design_doc`
    /// schema passes through the tool builder untouched, and the forced tool
    /// choice pins the function name.
    #[test]
    fn submit_design_doc_schema_flows_through_tool_body() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "submit_design_doc".into(),
                description: "Submit a structured design document.".into(),
                parameters: serde_json::to_value(&schema).unwrap(),
            }]),
            tool_choice: Some(ToolChoice::Forced("submit_design_doc".into())),
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);

        let tools = body["tools"].as_array().expect("should have tools array");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "submit_design_doc");
        let params: serde_json::Value =
            serde_json::from_value(tools[0]["function"]["parameters"].clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));
        assert_eq!(body["tool_choice"]["function"]["name"], "submit_design_doc");
    }

    /// `tool_choice` renders the OpenAI string forms (`required`, `none`) when
    /// tools are present on the request.
    #[test]
    fn tool_choice_required_and_none_render_as_strings() {
        for (choice, expected) in [(ToolChoice::Required, "required"), (ToolChoice::None, "none")] {
            let request = CompletionRequest {
                tools: Some(vec![ToolDefinition {
                    name: "test_tool".into(),
                    description: "A test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]),
                tool_choice: Some(choice),
                ..Default::default()
            };
            let body = render(&request, ReasoningEcho::IfPresent);
            assert_eq!(body["tool_choice"], serde_json::json!(expected));
        }
    }

    /// Without a `tool_choice` the body falls back to `auto` (default), and
    /// with no tools at all the `tools`/`tool_choice` fields are absent.
    #[test]
    fn tool_choice_absent_means_auto_and_no_tools_means_no_field() {
        let with_tools = make_test_request_with_tools();
        let body = render(&with_tools, ReasoningEcho::IfPresent);
        assert_eq!(body["tool_choice"], serde_json::json!("auto"));

        let without_tools = CompletionRequest::default();
        let body = render(&without_tools, ReasoningEcho::IfPresent);
        assert!(body.get("tools").is_none(), "no tools => no tools field");
        assert!(body.get("tool_choice").is_none(), "no tools => no tool_choice field");
    }

    /// ADR-046: `build_assistant_message` emits `reasoning_content` only when
    /// present under `IfPresent`, and always (empty string when absent) under
    /// `Always`.
    #[test]
    fn reasoning_echo_policies_control_json_field() {
        let with_reasoning = assistant_message("answer", None, Some("reasoning".into()));
        let without_reasoning = assistant_message("answer", None, None);

        // IfPresent + Some => emit field.
        let body = build_assistant_message(&with_reasoning, ReasoningEcho::IfPresent);
        assert_eq!(body["reasoning_content"], "reasoning");

        // IfPresent + None => omit field.
        let body = build_assistant_message(&without_reasoning, ReasoningEcho::IfPresent);
        assert!(body.get("reasoning_content").is_none());

        // Always + Some => emit field verbatim.
        let body = build_assistant_message(&with_reasoning, ReasoningEcho::Always);
        assert_eq!(body["reasoning_content"], "reasoning");

        // Always + None => emit empty string (satisfies DeepSeek contract).
        let body = build_assistant_message(&without_reasoning, ReasoningEcho::Always);
        assert_eq!(body["reasoning_content"], "");
    }

    /// ADR-046: a `Message` with reasoning renders into request JSON that
    /// carries `reasoning_content`.
    #[test]
    fn render_round_trip_carries_reasoning_content() {
        let request = CompletionRequest {
            messages: vec![assistant_message(
                "answer",
                Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "some_tool".into(),
                    arguments: serde_json::json!({}),
                }]),
                Some("reasoning".into()),
            )],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::Always);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["reasoning_content"], "reasoning");
        assert!(msgs[0]["tool_calls"].is_array(), "tool calls preserved");
    }

    /// A full assistant tool-use turn followed by tool results and a user reply
    /// must render its messages in order, with the tool messages carrying the
    /// matching `tool_call_id` and the assistant message keeping `null` content
    /// plus a `tool_calls` array.
    #[test]
    fn tool_call_then_results_render_in_order() {
        let request = CompletionRequest {
            messages: vec![
                assistant_message(
                    "",
                    Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                    }]),
                    None,
                ),
                Message {
                    role: Role::Tool,
                    content: String::new(),
                    tool_calls: None,
                    tool_results: Some(vec![ToolResult {
                        id: "call_1".into(),
                        name: "shell".into(),
                        content: serde_json::json!("file list"),
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
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);

        assert_eq!(msgs[0]["role"], "assistant");
        assert!(msgs[0]["content"].is_null(), "tool-use assistant content must be null");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!("{\"command\":\"ls\"}")
        );

        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_1");
        assert_eq!(msgs[1]["content"], "file list");

        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"], "Run that again.");
    }

    /// Oracle guard F-1: a `Role::Tool` message with *multiple* results must
    /// flatten into one wire message per result, in order, each tagged with its
    /// own `tool_call_id` — never collapsed into a single message.
    #[test]
    fn multiple_tool_results_flatten_into_separate_messages() {
        let request = CompletionRequest {
            messages: vec![
                assistant_message(
                    "",
                    Some(vec![
                        ToolCall {
                            id: "call_a".into(),
                            name: "tool_a".into(),
                            arguments: serde_json::json!({"input": "a"}),
                        },
                        ToolCall {
                            id: "call_b".into(),
                            name: "tool_b".into(),
                            arguments: serde_json::json!({"input": "b"}),
                        },
                    ]),
                    None,
                ),
                Message {
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
                },
            ],
            ..Default::default()
        };
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 3, "assistant + two flattened tool messages");
        assert_eq!(msgs[0]["role"], "assistant");
        let calls = msgs[0]["tool_calls"].as_array().expect("assistant carries both tool calls");
        assert_eq!(calls.len(), 2, "assistant keeps both tool calls");

        assert_eq!(msgs[1]["role"], "tool", "first result is its own message");
        assert_eq!(msgs[1]["tool_call_id"], "call_a");
        assert_eq!(msgs[1]["content"], "first result");

        assert_eq!(msgs[2]["role"], "tool", "second result is its own message");
        assert_eq!(msgs[2]["tool_call_id"], "call_b");
        assert_eq!(msgs[2]["content"], "second result");
    }

    /// Oracle guard F-2: `tools: Some(vec![])` must behave like "no tools" —
    /// the OpenAI body carries neither a `tools` nor a `tool_choice` field
    /// (an empty array would draw a 400 from the API).
    #[test]
    fn empty_tools_list_omits_tools_and_tool_choice() {
        let request = CompletionRequest { tools: Some(vec![]), ..Default::default() };
        let body = render(&request, ReasoningEcho::IfPresent);

        assert!(body.get("tools").is_none(), "empty tools => no tools field");
        assert!(body.get("tool_choice").is_none(), "empty tools => no tool_choice field");
    }

    /// Oracle guard F-3: a tool-role message with *no* `tool_results` still
    /// renders (content only, no `tool_call_id`) instead of vanishing.
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
        let body = render(&request, ReasoningEcho::IfPresent);
        let msgs = body["messages"].as_array().expect("should have messages");

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["content"], "plain tool output");
        assert!(msgs[0].get("tool_call_id").is_none(), "no tool_results => no tool_call_id field");
    }

    /// Oracle guard F-4: a numeric zero is a set value, not an "absent"
    /// sentinel — `temperature` and `max_tokens` explicitly set to `0` still
    /// render their body fields.
    #[test]
    fn zero_temperature_and_max_tokens_still_emit_fields() {
        let request =
            CompletionRequest { temperature: Some(0.0), max_tokens: Some(0), ..Default::default() };
        let body = render(&request, ReasoningEcho::IfPresent);

        assert_eq!(body["temperature"], 0.0, "explicit 0 temperature must be present");
        assert_eq!(body["max_tokens"], 0, "explicit 0 max_tokens must be present");
    }

    /// ADR-48 decision 3: the OpenAI-compatible family relies on server-side
    /// prefix pooling and a byte-stable head guaranteed by the engine — the
    /// dialect's cache-breakpoint method is a no-op and must leave the body
    /// byte-for-byte unchanged.
    #[test]
    fn apply_cache_breakpoints_is_noop_for_openai_compat() {
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
        let mut body = render(&request, ReasoningEcho::IfPresent);
        let before = serde_json::to_string(&body).expect("body serializes");
        OpenAiChatDialect.apply_cache_breakpoints(&mut body);

        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            before,
            "openai-compat body must be byte-identical after apply_cache_breakpoints",
        );
        assert!(
            body.as_object().is_some_and(|obj| !obj.contains_key("cache_control")),
            "no cache_control may appear anywhere in an openai-compat body"
        );
    }
}
