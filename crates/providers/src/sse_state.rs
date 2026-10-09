//! Shared Anthropic-dialect SSE stream state machine.
//!
//! [`AnthropicStreamState`] turns the buffered [`BufferedSseParser`] events of
//! the Anthropic Messages wire dialect into [`CompletionChunk`]s, handling the
//! four event types `content_block_start`, `content_block_delta`,
//! `content_block_stop`, and `message_stop`.
//!
//! Two connectors speak that dialect and share this one implementation so
//! their event handling can never drift apart:
//!
//! - `crate::anthropic` (`api.anthropic.com/v1/messages`)
//! - `crate::opencode` (the Zen/Go relay's Anthropic-Messages path)
//!
//! The module is intentionally crate-private: moving the machine out of
//! `crate::anthropic` must not widen that connector's public API surface.
//!
//! No dialect switch exists here on purpose. The two former copies were
//! verified byte-equivalent modulo doc wording, a blank line, and one local
//! variable rename (`args_json`/`args`) — there is no anthropic-vs-opencode
//! behavioral difference to parameterize.

use std::collections::{HashMap, VecDeque};

use concerto_core::error::ProviderError;
use concerto_core::types::{CompletionChunk, CompletionUsage, ToolCall};

use crate::sse::BufferedSseParser;

#[derive(Default)]
struct AnthropicParseState {
    text_acc: HashMap<usize, String>,
    tool_acc: HashMap<usize, (String, String, String)>,
}

/// The Anthropic-dialect SSE state machine (see the module docs).
///
/// `parser`, `pending`, and `tool_adapted` are crate-visible because each
/// connector drives them from its own streaming loop; everything else is
/// internal to this module.
pub(crate) struct AnthropicStreamState {
    pub(crate) parser: BufferedSseParser,
    parse: AnthropicParseState,
    pub(crate) pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    /// Whether the request that produced this stream was rendered with
    /// loose (weak-model) tool schemas. When set, emitted tool-call
    /// arguments are re-nested from dot-notation back into the tools'
    /// original nested shape (see `crate::adapters::schema_loose`).
    pub(crate) tool_adapted: bool,
    /// Provider-reported usage merged from the events that carry it (ADR-48
    /// §4): `message_start` reports `message.usage.input_tokens`, and
    /// `message_delta` reports the cumulative `usage.output_tokens`. Attached
    /// to the `message_stop` terminal chunk only.
    usage: Option<CompletionUsage>,
    /// Set when a completed tool_use block carried unrepairable arguments.
    /// The typed error is deferred to `message_stop` so the stream still
    /// terminates with a real error instead of silently emitting a tool call
    /// with empty arguments (which the executor would run as `{}`).
    tool_parse_error: Option<ProviderError>,
}

impl AnthropicStreamState {
    pub(crate) fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            parse: AnthropicParseState::default(),
            pending: VecDeque::new(),
            tool_adapted: false,
            usage: None,
            tool_parse_error: None,
        }
    }

    /// Merge provider-reported token counts into the accumulated usage.
    ///
    /// Anthropic splits usage across events: `message_start` carries
    /// `message.usage.input_tokens`, `message_delta` carries the cumulative
    /// `usage.output_tokens`, and newer-API `content_block_stop` events can
    /// carry a full usage object. Only counts actually present on the wire
    /// are recorded — `None` and `0` are both legitimate reports, so no
    /// coalescing happens here (ADR-48 decision 4). The `message_start`
    /// `output_tokens` placeholder (`1`) is overwritten by the later real
    /// cumulative total.
    fn capture_usage(&mut self, data: &serde_json::Value) {
        let input_tokens = data["message"]["usage"]["input_tokens"].as_u64();
        let output_tokens = data["usage"]["output_tokens"].as_u64();
        if input_tokens.is_none() && output_tokens.is_none() {
            return;
        }
        let usage = self.usage.get_or_insert_with(CompletionUsage::default);
        if let Some(input) = input_tokens {
            usage.prompt_tokens = Some(input);
        }
        if let Some(output) = output_tokens {
            usage.completion_tokens = Some(output);
        }
    }

    pub(crate) fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            // Liveness signal (SSE comment line): emit an empty chunk so the
            // stream stays active and the orchestrator idle timeout does not
            // fire during long keep-alive-only periods.
            self.pending.push_back(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }
        let data_str = match event.data {
            Some(d) => d,
            None => return,
        };

        let data: serde_json::Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(_) => return,
        };
        self.capture_usage(&data);

        let event_type = event.event.as_deref().unwrap_or("");

        match event_type {
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let ctype = data["content_block"]["type"].as_str().unwrap_or("");
                if ctype == "text" {
                    self.parse.text_acc.insert(index, String::new());
                } else if ctype == "tool_use" {
                    let id = data["content_block"]["id"].as_str().unwrap_or("").to_string();
                    let name = data["content_block"]["name"].as_str().unwrap_or("").to_string();
                    self.parse.tool_acc.insert(index, (id, name, String::new()));
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                    if let Some(acc) = self.parse.text_acc.get_mut(&index) {
                        acc.push_str(text);
                    }
                }
                if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                    if let Some((_id, _name, args)) = self.parse.tool_acc.get_mut(&index) {
                        args.push_str(partial);
                    }
                }
            }
            "content_block_stop" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                if let Some(text) = self.parse.text_acc.remove(&index) {
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: text,
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }));
                } else if let Some((id, name, args_str)) = self.parse.tool_acc.remove(&index) {
                    match crate::tool_args::parse_tool_arguments(&args_str) {
                        Ok(outcome) => {
                            let args_json = match outcome {
                                crate::tool_args::ToolArgumentParse::Value(value) => value,
                                crate::tool_args::ToolArgumentParse::Empty => {
                                    serde_json::Value::Null
                                }
                            };
                            let mut args_json = crate::protocol::ensure_arguments_object(args_json);
                            // Adaptive tool schemas: re-nest dot-notation
                            // arguments from loose-schema streams before the
                            // executor or the tool-call guard validates
                            // against the nested schema.
                            if self.tool_adapted {
                                crate::adapters::schema_loose::unflatten_tool_arguments(
                                    &mut args_json,
                                );
                            }
                            self.pending.push_back(Ok(CompletionChunk {
                                reasoning: None,
                                delta: String::new(),
                                tool_call: Some(ToolCall {
                                    id,
                                    name,
                                    arguments: args_json,
                                    ..Default::default()
                                }),
                                is_final: false,
                                usage: None,
                            }));
                        }
                        Err(error) => {
                            tracing::warn!(
                                tool_name = %name,
                                raw_len = error.raw_len,
                                parse_error = %error,
                                "tool args unrepairable; failing the stream loudly"
                            );
                            self.tool_parse_error = Some(ProviderError::InvalidResponse(format!(
                                "provider returned unparseable tool-call arguments for '{name}': {error}"
                            )));
                        }
                    }
                }
            }
            "message_stop" => {
                if let Some(error) = self.tool_parse_error.take() {
                    self.pending.push_back(Err(error));
                } else {
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: None,
                        is_final: true,
                        usage: self.usage.take(),
                    }));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — the machine's behavior, moved here verbatim from the two connectors
// that used to carry a private copy each (the two copies of
// `stream_captures_usage_on_final_chunk` collapsed into one).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_default_is_empty() {
        let state = AnthropicParseState::default();
        assert!(state.text_acc.is_empty());
        assert!(state.tool_acc.is_empty());
    }

    #[test]
    fn anthropic_stream_text_only() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        ));
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].delta, "Hello world");
        assert!(!chunks[0].is_final);
        assert!(chunks[1].is_final);
    }

    #[test]
    fn anthropic_stream_tool_use() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_1","name":"shell"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        ));
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 2);
        let tc = chunks[0].tool_call.as_ref().unwrap();
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "shell");
        assert_eq!(tc.arguments, serde_json::json!({"command": "ls"}));
        assert!(chunks[1].is_final);
    }

    #[test]
    fn anthropic_stream_keepalive_emits_empty_chunk() {
        let mut state = AnthropicStreamState::new();
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: None,
            id: None,
            keepalive: true,
        });
        assert_eq!(state.pending.len(), 1);
        let chunk = state.pending.pop_front().unwrap().unwrap();
        assert!(chunk.delta.is_empty());
        assert!(!chunk.is_final);
    }

    #[test]
    fn anthropic_stream_tool_empty_args_coerce_to_object() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_2","name":"noop"}}"#,
        ));
        // No argument deltas — empty tool call.
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        let tc = chunks[0].tool_call.as_ref().unwrap();
        assert!(tc.arguments.is_object(), "empty args must coerce to object");
        assert_eq!(tc.arguments, serde_json::json!({}));
    }

    /// ADR-48 §4: `message_start` input_tokens and `message_delta`
    /// output_tokens are merged and attached to the `message_stop` terminal
    /// chunk only; intermediate content chunks carry no usage.
    #[test]
    fn stream_captures_usage_on_final_chunk() {
        let mut state = AnthropicStreamState::new();
        let event = |event_type: &str, data: &str| crate::sse::SseEvent {
            event: Some(event_type.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-4","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#,
        ));
        state.handle_event(event(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        ));
        state.handle_event(event(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ));
        state.handle_event(event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
        ));
        state.handle_event(event("message_stop", r#"{"type":"message_stop"}"#));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert!(
            chunks[..chunks.len() - 1].iter().all(|chunk| !chunk.is_final),
            "only the final chunk is terminal"
        );
        assert_eq!(chunks[0].usage, None, "content deltas carry no usage");
        let terminal = chunks.last().expect("terminal chunk");
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage,
            Some(CompletionUsage { prompt_tokens: Some(25), completion_tokens: Some(15) })
        );
        crate::testing::assert_terminal_usage_contract(&chunks);
    }

    /// A stream that reports usage but never reaches `message_stop` must not
    /// emit anything: the terminal chunk (the only one carrying usage) is
    /// produced by `message_stop` alone.
    #[test]
    fn message_delta_without_message_stop_emits_nothing() {
        let mut state = AnthropicStreamState::new();
        let event = crate::sse::SseEvent {
            event: Some("message_delta".to_string()),
            data: Some(r#"{"type":"message_delta","usage":{}}"#.to_string()),
            id: None,
            keepalive: false,
        };
        state.handle_event(event);
        assert!(state.usage.is_none(), "counts-less usage must stay None");
        assert!(
            state.pending.is_empty(),
            "message_delta alone emits no chunk; usage surfaces on message_stop"
        );
    }

    /// ADR-48 §4: usage objects with no token counts are not measurements
    /// and must not be surfaced as one (mirrors the OpenAI capture rule).
    #[test]
    fn stream_ignores_usage_without_counts() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // Both usage-bearing event shapes arrive with empty usage objects:
        // no counts means no measurement, so usage stays `None`.
        state.handle_event(event(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-4","content":[],"stop_reason":null,"stop_sequence":null,"usage":{}}}"#,
        ));
        state.handle_event(event(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{}}"#,
        ));
        state.handle_event(event("message_stop", r#"{"type":"message_stop"}"#));

        assert!(state.usage.is_none(), "counts-less usage must stay None");
        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.unwrap()).collect();
        let terminal = chunks.last().unwrap();
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage, None,
            "counts-less usage must not surface on the terminal chunk"
        );
        crate::testing::assert_terminal_usage_contract(&chunks);
    }
}
