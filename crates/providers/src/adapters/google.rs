//! Google Gemini chat dialect (`generateContent` / `streamGenerateContent`).
//!
//! This adapter lowers a canonical [`CompletionRequest`] to the wire body of
//! Google's Gemini `generateContent` endpoint. The logic here is a verbatim
//! port of the body builder that previously lived in `crate::google`; only the
//! seam changed, not the bytes.
//!
//! # Wire contract
//!
//! - `contents`: a flat array of entries with `role` (`"user"`/`"model"`) and
//!   `parts`.
//! - System prompt: top-level `system_instruction` with
//!   `{"parts":[{"text": ...}]}`; the last `Role::System` message wins.
//! - User parts: `{"text": ...}`. Tool results are appended as `user` parts
//!   `{"functionResponse":{"name":..., "response":...}}` — one entry per tool
//!   result, matched by *function name*, not call id.
//! - Assistant parts: `{"text": ...}`; when the assistant message carries
//!   `tool_calls` the `parts` array is *replaced* by
//!   `{"functionCall":{"name":...,"args":...}}` entries (the text part is
//!   dropped, matching the historical builder).
//! - Tools: `tools:[{"functionDeclarations":[{name, description,
//!   parameters}]}]`. `generationConfig` carries `temperature`,
//!   `maxOutputTokens` and, when tools are present, a `functionCallingConfig`
//!   mapping the canonical [`ToolChoice`] to Gemini `AUTO`/`NONE`/`ANY` modes.
//! - There is no top-level `stream` field: the connector selects the streaming
//!   variant through the URL (`:streamGenerateContent?alt=sse`).
//!
//! This family never echoes `reasoning_content` back on assistant messages
//! (Gemini has no such field), so the `ReasoningEcho` argument is accepted for
//! the uniform [`Dialect`] signature but has no effect on the wire body.

use concerto_core::types::{CompletionRequest, Role, ToolChoice, ToolDefinition};

use super::{Dialect, ReasoningEcho};

/// The Google Gemini chat dialect (`generateContent`).
///
/// Stateless unit struct; construct one (or keep one per provider) and call
/// [`Dialect::render_chat_body`] for each completion request. See the module
/// docs for the wire contract it implements.
pub struct GeminiChatDialect;

impl Dialect for GeminiChatDialect {
    fn kind(&self) -> &'static str {
        "gemini"
    }

    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        _model: &str,
        _echo: ReasoningEcho,
    ) -> serde_json::Value {
        // Note: unlike the chat-completions families, Gemini does not send the
        // model name in the request body — it is part of the endpoint URL.
        let mut contents: Vec<serde_json::Value> = Vec::new();
        let mut system_instruction: Option<String> = None;

        for msg in &request.messages {
            match msg.role {
                Role::System => {
                    system_instruction = Some(msg.content.clone());
                }
                Role::User => {
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{"text": msg.content}]
                    }));
                }
                Role::Assistant => {
                    let part = serde_json::json!({"text": msg.content});
                    let mut entry = serde_json::json!({
                        "role": "model",
                        "parts": [part]
                    });
                    if let Some(ref tcs) = msg.tool_calls {
                        let tool_calls: Vec<serde_json::Value> = tcs
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "functionCall": {
                                        "name": tc.name,
                                        "args": tc.arguments
                                    }
                                })
                            })
                            .collect();
                        entry["parts"] = serde_json::json!(tool_calls);
                    }
                    contents.push(entry);
                }
                Role::Tool => {
                    // Gemini uses functionResponse to relay tool results back.
                    // The `name` must be the function/tool name (from
                    // ToolResult.name), not the tool call ID — Gemini matches
                    // this against the function declaration sent in the
                    // `tools` array.
                    if let Some(results) = &msg.tool_results {
                        for result in results {
                            contents.push(serde_json::json!({
                                "role": "user",
                                "parts": [{
                                    "functionResponse": {
                                        "name": result.name,
                                        "response": result.content
                                    }
                                }]
                            }));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
        });

        if let Some(si) = system_instruction {
            body["system_instruction"] = serde_json::json!({
                "parts": [{"text": si}]
            });
        }

        let mut gen_config = serde_json::json!({});
        if let Some(temp) = request.temperature {
            gen_config["temperature"] = serde_json::json!(temp);
        }
        if let Some(max_t) = request.max_tokens {
            gen_config["maxOutputTokens"] = serde_json::json!(max_t);
        }
        if let Some(tools) = &request.tools {
            // Send tool/function declarations so Gemini knows what functions
            // exist.
            let decls = build_google_tool_declarations(tools);
            if !decls.is_empty() {
                body["tools"] = serde_json::json!([{
                    "functionDeclarations": decls
                }]);
            }

            // Map generic ToolChoice to Gemini's functionCallingConfig.
            gen_config["functionCallingConfig"] = build_google_function_calling_config(
                request.tool_choice.as_ref().unwrap_or(&ToolChoice::Auto),
            );
        }
        if gen_config.as_object().is_some_and(|o| !o.is_empty()) {
            body["generationConfig"] = gen_config;
        }

        body
    }
}

// ---------------------------------------------------------------------------
// Family-specific renderers (one canonical value -> the Gemini wire shape).
// Field shape matters for golden tests: keep these builds byte-identical to the
// previous `google.rs` implementations.
// ---------------------------------------------------------------------------

/// Convert `ToolDefinition`s into Gemini's `functionDeclarations` entries.
fn build_google_tool_declarations(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// Map the generic `ToolChoice` to Gemini's `functionCallingConfig`.
fn build_google_function_calling_config(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({"mode": "AUTO"}),
        ToolChoice::None => serde_json::json!({"mode": "NONE"}),
        ToolChoice::Required => serde_json::json!({"mode": "ANY"}),
        ToolChoice::Forced(name) => serde_json::json!({
            "mode": "ANY",
            "allowedFunctionNames": [name]
        }),
        _ => serde_json::json!({"mode": "AUTO"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{Message, SubmitDesignDocInput, ToolCall, ToolResult};

    fn render(request: &CompletionRequest) -> serde_json::Value {
        GeminiChatDialect.render_chat_body(request, "gemini-2.0-flash", ReasoningEcho::IfPresent)
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
    fn dialect_kind_is_gemini() {
        assert_eq!(GeminiChatDialect.kind(), "gemini");
    }

    /// Gemini encodes streaming through the URL (`:streamGenerateContent?alt=sse`),
    /// so the body never carries a `stream` field.
    #[test]
    fn body_has_no_stream_field() {
        let request = CompletionRequest { stream: true, ..Default::default() };
        let body = render(&request);
        assert!(body.get("stream").is_none(), "gemini body must not carry stream");
    }

    #[test]
    fn no_model_field_in_body() {
        let request = CompletionRequest::default();
        let body = render(&request);
        assert!(body.get("model").is_none(), "gemini body must not carry model");
    }

    #[test]
    fn system_renders_as_system_instruction() {
        let request = fixture_request();
        let body = render(&request);
        assert_eq!(
            body["system_instruction"],
            serde_json::json!({"parts": [{"text": "You are a helpful assistant."}]})
        );
        let contents = body["contents"].as_array().unwrap();
        assert!(
            contents.iter().all(|c| c["role"] != "system"),
            "system messages must not appear in contents"
        );
    }

    #[test]
    fn user_messages_render_user_parts() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"], serde_json::json!([{"text": "Hello!"}]));
    }

    /// When an assistant message has tool calls, Gemini replaces the parts
    /// array with `functionCall` entries — the text flash is dropped, matching
    /// the previous `google.rs` builder.
    #[test]
    fn assistant_tool_calls_replace_parts_with_function_call() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"],
            serde_json::json!([{
                "functionCall": {
                    "name": "shell",
                    "args": {"command": "ls"}
                }
            }])
        );
    }

    /// ADR-46 parity: Gemini never echoes `reasoning_content` onto assistant
    /// parts; a reasoning-only assistant renders as a bare text part.
    #[test]
    fn assistant_reasoning_is_not_echoed() {
        let request = fixture_request();
        let body = render(&request);

        let serialized = serde_json::to_string(&body).expect("body serializes");
        assert!(
            !serialized.contains("reasoning"),
            "gemini body must not carry reasoning_content: {serialized}"
        );

        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents[2],
            serde_json::json!({
                "role": "model",
                "parts": [{"text": "Here"}]
            })
        );
    }

    #[test]
    fn tool_results_render_function_response_parts() {
        let request = fixture_request();
        let body = render(&request);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents[3],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "shell",
                        "response": {"output": "file list"}
                    }
                }]
            })
        );
    }

    #[test]
    fn tools_serialize_as_function_declarations() {
        let request = fixture_request();
        let body = render(&request);

        let tools = body["tools"].as_array().expect("should have tools array");
        let declarations =
            tools[0]["functionDeclarations"].as_array().expect("should have functionDeclarations");
        let decl = &declarations[0];
        assert_eq!(decl["name"], "test_tool");
        assert_eq!(decl["description"], "A test tool");
        assert_eq!(decl["parameters"]["properties"]["input"]["type"], "string");
        // Unlike OpenAI, Gemini declarations carry no `type: "function"` wrapper.
        assert!(tools[0].get("functionDeclarations").is_some());
        assert!(decl.get("input_schema").is_none(), "gemini uses parameters, not input_schema");
    }

    #[test]
    fn function_calling_config_maps_all_modes() {
        for (choice, expected_mode) in
            [(ToolChoice::Auto, "AUTO"), (ToolChoice::None, "NONE"), (ToolChoice::Required, "ANY")]
        {
            let request = CompletionRequest {
                tools: Some(vec![ToolDefinition {
                    name: "test_tool".into(),
                    description: "A test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]),
                tool_choice: Some(choice),
                ..Default::default()
            };
            let body = render(&request);
            assert_eq!(body["generationConfig"]["functionCallingConfig"]["mode"], expected_mode);
        }

        // Strictly no other keys for non-forced modes: no allowedFunctionNames.
        let request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "test_tool".into(),
                description: "A test tool".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            body["generationConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "NONE"})
        );

        // Forced tool pins the allowed function name under ANY.
        let request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "submit_design_doc".into(),
                description: "Submit a structured design document.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            tool_choice: Some(ToolChoice::Forced("submit_design_doc".into())),
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            body["generationConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "ANY", "allowedFunctionNames": ["submit_design_doc"]})
        );
    }

    /// Without tools there is no `functionCallingConfig` and no `tools` field;
    /// temperature/max tokens alone keep `generationConfig` on the body.
    #[test]
    fn generation_config_is_absent_without_options_or_tools() {
        let request = CompletionRequest::default();
        let body = render(&request);
        assert!(body.get("generationConfig").is_none());
        assert!(body.get("tools").is_none());
    }

    /// With tools present but zero declarations, Gemini still attaches the
    /// `functionCallingConfig` but omits the empty `tools` field (parity with
    /// the previous builder).
    #[test]
    fn empty_tools_still_sets_function_calling_config() {
        let request = CompletionRequest { tools: Some(vec![]), ..Default::default() };
        let body = render(&request);
        assert!(body.get("tools").is_none(), "empty declarations => no tools field");
        assert_eq!(
            body["generationConfig"]["functionCallingConfig"],
            serde_json::json!({"mode": "AUTO"})
        );
    }

    /// Exact serialized body is part of the Gemini wire contract: golden-test
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
            ..Default::default()
        };
        let body = render(&request);
        assert_eq!(
            serde_json::to_string(&body).expect("body serializes"),
            r#"{"contents":[{"parts":[{"text":"Hello"}],"role":"user"}],"system_instruction":{"parts":[{"text":"You are a test assistant."}]}}"#
        );
    }

    /// VALUE-level parity: the full fixture renders to exactly the Gemini shape
    /// the previous `google.rs` builder produced.
    #[test]
    fn full_fixture_value_snapshot() {
        let request = fixture_request();
        let body = render(&request);

        let expected = serde_json::json!({
            "contents": [
                {"role": "user", "parts": [{"text": "Hello!"}]},
                {
                    "role": "model",
                    "parts": [{
                        "functionCall": {"name": "shell", "args": {"command": "ls"}}
                    }]
                },
                {"role": "model", "parts": [{"text": "Here"}]},
                {
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": "shell",
                            "response": {"output": "file list"}
                        }
                    }]
                },
                {"role": "user", "parts": [{"text": "Run that again."}]}
            ],
            "system_instruction": {
                "parts": [{"text": "You are a helpful assistant."}]
            },
            "tools": [
                {
                    "functionDeclarations": [
                        {
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
                    ]
                }
            ],
            "generationConfig": {
                "temperature": 0.5,
                "maxOutputTokens": 4096,
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": ["test_tool"]
                }
            }
        });

        assert_eq!(body, expected);
    }

    /// Contract 9 (Google Gemini): the real schemars-generated
    /// `submit_design_doc` schema survives the function-declaration builder,
    /// and the forced tool choice maps to Gemini's `functionCallingConfig`
    /// with `mode: ANY` + the allowed function name.
    #[test]
    fn submit_design_doc_schema_flows_through_tools_builder() {
        let schema = schemars::schema_for!(SubmitDesignDocInput);
        let tools = vec![ToolDefinition {
            name: "submit_design_doc".into(),
            description: "Submit a structured design document.".into(),
            parameters: serde_json::to_value(&schema).unwrap(),
        }];
        let decls = build_google_tool_declarations(&tools);
        assert_eq!(decls[0]["name"], "submit_design_doc");
        let params: serde_json::Value =
            serde_json::from_value(decls[0]["parameters"].clone()).unwrap();
        assert_eq!(params["required"], serde_json::json!(["interface_sketch"]));

        let config =
            build_google_function_calling_config(&ToolChoice::Forced("submit_design_doc".into()));
        assert_eq!(config["mode"], "ANY");
        assert_eq!(config["allowedFunctionNames"], serde_json::json!(["submit_design_doc"]));
    }

    /// Oracle guard F-1: multiple tool results flatten into one `contents`
    /// entry per result, each keyed by the result's *function name* (Gemini
    /// matches `functionResponse.name` against the declared functions, not the
    /// call id).
    #[test]
    fn multiple_tool_results_flatten_into_entries() {
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
        let body = render(&request);
        let contents = body["contents"].as_array().expect("should have contents");

        assert_eq!(contents.len(), 2, "one entry per tool result");
        assert_eq!(
            contents[0],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "tool_a",
                        "response": "first result"
                    }
                }]
            })
        );
        assert_eq!(
            contents[1],
            serde_json::json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "tool_b",
                        "response": "second result"
                    }
                }]
            })
        );
    }

    /// Oracle guard F-3: a tool-role message with no `tool_results` emits
    /// *no* contents entry — Gemini has no "bare tool message" shape.
    #[test]
    fn tool_message_without_results_emits_no_entry() {
        let request = CompletionRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: "discarded".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = render(&request);
        let contents = body["contents"].as_array().expect("should have contents");

        assert_eq!(contents.len(), 0, "no tool_results => no contents entry");
    }

    /// Oracle guard F-4: a numeric zero is a set value — `temperature` and
    /// `max_tokens` explicitly set to `0` still render `generationConfig`
    /// fields.
    #[test]
    fn zero_temperature_and_max_tokens_render_in_generation_config() {
        let request =
            CompletionRequest { temperature: Some(0.0), max_tokens: Some(0), ..Default::default() };
        let body = render(&request);

        assert_eq!(body["generationConfig"]["temperature"], 0.0);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 0);
    }

    /// Oracle guard F-6: when several `Role::System` messages are present the
    /// *last* one wins `system_instruction`, matching the previous builder's
    /// overwrite loop.
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
        let body = render(&request);

        assert_eq!(
            body["system_instruction"],
            serde_json::json!({"parts": [{"text": "second system"}]}),
            "last system message wins"
        );
    }
}
