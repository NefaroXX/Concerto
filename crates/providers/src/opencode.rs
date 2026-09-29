//! OpenCode Zen provider.
//!
//! OpenCode Zen serves multiple model families through a single gateway:
//!
//! - **OpenAI-compatible models** (e.g. `big-pickle`, DeepSeek): routed via
//!   [`OpenAiProvider`] to `POST {base}/chat/completions`.
//! - **Anthropic models** (`claude-*`): routed via the Anthropic Messages API
//!   to `POST {base}/messages` with `x-api-key` + `anthropic-version` headers.
//! - **Responses-API models** (genuine `muse-v*` family members, plus
//!   explicit entries like `muse-spark-*`): routed via the OpenAI Responses
//!   API to `POST {base}/responses`. These models 500 on both
//!   `/chat/completions` and `/messages` upstream.
//!
//! The dialect is chosen per model id. The governing principle is
//! **behavior over taxonomy** (ADR-66 §5 correction, 2026-09-08): the wire
//! dialect follows what the endpoint *does*, not which family a model name
//! resembles. Concretely, in precedence order:
//!
//! 1. **Explicit full-id prefix entries** ([`RESPONSES_API_MODEL_PREFIXES`])
//!    — endpoint behavior observed live (0d511f1: `muse-spark-*` 500s on
//!    `/chat/completions` and only works via `/responses`), even though the
//!    name is not a Muse family member.
//! 2. **Whole family tokens** (ADR-66 §5) — the `claude` token selects the
//!    Anthropic dialect, the Muse rule (`muse` + version segment) selects
//!    the Responses API. Substring matches are forbidden — a name merely
//!    *containing* `muse` or `claude` (e.g. `claudette-*`, `some-muse-model`)
//!    never routes to that family's dialect without an explicit entry.

use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, CompletionUsage, ModelInfo, TokenBudget, ToolCall,
};
use concerto_core::CancellationToken;
use concerto_core::SecretString;
use futures::stream::StreamExt;
use reqwest::header::CONTENT_TYPE;
use std::collections::{HashMap, VecDeque};

use crate::adapters::{AnthropicChatDialect, Dialect, ReasoningEcho};
use crate::openai::OpenAiProvider;
use crate::sse::BufferedSseParser;

/// Default OpenCode Zen API base URL.
const OPENCODE_ZEN_API_BASE: &str = "https://opencode.ai/zen/v1";

/// Stable capability name used by every ADR-66 capability refusal.
pub(crate) const TOOL_CALLING_CAPABILITY: &str = "tool_calling";

/// Detect whether a model name requires the Anthropic Messages API dialect.
///
/// Claude models served by the Zen gateway expect the Anthropic wire format
/// (`POST /messages`, `x-api-key` header, Anthropic SSE events). All other
/// non-Muse models use the OpenAI-compatible dialect.
///
/// Family matching is token-based (ADR-66 §5): a hyphen-separated token must
/// *be* `claude` — a name where `claude` is merely a substring of another
/// token (`claudette-1`, `declaude`, `claudeify-v2`) never matches.
pub(crate) fn needs_anthropic_dialect(model: &str) -> bool {
    tokenize_model_name(model).iter().any(|token| token == "claude")
}
/// Detect whether a model name requires the OpenAI Responses API dialect.
///
/// The dialect follows **endpoint behavior, not family taxonomy** (ADR-66
/// §5 correction): models in [`RESPONSES_API_MODEL_PREFIXES`] are served
/// only via `POST /responses` — they 500 on both `/chat/completions` and
/// `/messages` — regardless of what family their name suggests. Genuine
/// Muse family members (`muse` followed by a `v`-prefixed version segment,
/// e.g. `muse-v2`, `muse-v2.1`, `v3-pro`) hit the same upstream behavior
/// and are matched by the token rule.
///
/// The token rule stays deliberately strict (ADR-66 §5): a `muse` token
/// alone is NOT sufficient — the token immediately after it must be a
/// `v`-prefixed version token. Names that merely contain "muse"
/// (`some-muse-model`, `amuse-v2`, `museum-2`) never match the token rule;
/// they only take the Responses path via an explicit prefix entry, and only
/// when endpoint behavior justifies it.
pub(crate) fn needs_responses_api(model: &str) -> bool {
    // Explicit full-id prefix entries override the token heuristic: they
    // encode observed endpoint behavior (0d511f1), not name taxonomy.
    let lowered = model.to_ascii_lowercase();
    if RESPONSES_API_MODEL_PREFIXES.iter().any(|prefix| lowered.starts_with(prefix)) {
        return true;
    }
    let tokens = tokenize_model_name(model);
    tokens
        .iter()
        .zip(tokens.iter().skip(1))
        .any(|(token, next)| token == "muse" && is_muse_family_segment(next))
}

/// Explicit Responses-API dialect overrides, keyed by **full model-id
/// prefix** (matched case-insensitively against the whole model id).
///
/// These entries exist because the wire dialect follows endpoint behavior,
/// not family taxonomy (ADR-66 §5 correction, 2026-09-08): Zen's
/// `muse-spark-*` catalog family is not Muse, but its models 500 on
/// `/chat/completions` and only work via `POST /responses` (the original
/// fix, 0d511f1). Entries are exact prefixes with a trailing `-` so they
/// respect token boundaries (`muse-sparkless` must not match). Add an entry
/// only for an observed upstream endpoint behavior, never for a name
/// resemblance.
pub(crate) const RESPONSES_API_MODEL_PREFIXES: &[&str] = &["muse-spark-"];

/// Split a model name into lowercase family tokens — the **shared**
/// tokenizer of this crate.
///
/// Model ids are hyphen-delimited (`muse-v2`, `claude-sonnet-4`); the
/// hyphen is the only family separator honored. A token is the whole
/// dash-delimited word, so substring collisions inside larger tokens are
/// impossible. Tokens are owned — callers keep them as a standalone list.
///
/// Every name-based model-name decision in this crate routes through this
/// one function: the dialect rules here ([`needs_anthropic_dialect`],
/// [`needs_responses_api`]) and the tool-schema tier heuristic
/// ([`crate::adapters::schema_loose::last_resort_weak_tool_calling_model`]). There
/// must be exactly one tokenizer — do not copy this logic (ADR-66 §5:
/// family heuristics match whole tokens, never bare substrings).
pub(crate) fn tokenize_model_name(model: &str) -> Vec<String> {
    model.to_ascii_lowercase().split('-').map(str::to_owned).collect()
}

/// Decide whether the token following a `muse` token identifies a genuine
/// Muse family member.
///
/// Known Muse family segments are version tokens: a leading `v` followed by
/// at least one ASCII digit (`v2`, `v2.1`, `v3`, `v3-pro`). Everything else
/// (`spark`, `pro`, `vapor`) is not a known Muse family segment, so such
/// names only reach the Responses dialect through an explicit
/// [`RESPONSES_API_MODEL_PREFIXES`] entry — never via name resemblance.
fn is_muse_family_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.first() == Some(&b'v') && bytes.get(1).is_some_and(u8::is_ascii_digit)
}

/// OpenCode Zen provider that automatically selects the correct wire dialect
/// per model family.
pub struct OpenCodeZenProvider {
    model: String,
    timeout_secs: u64,
    api_base: String,
    /// Tool-schema presentation tier (adaptive tool schemas) for the
    /// provider's own Anthropic-dialect path. `Auto` (default) keeps every
    /// non-weak model on the verbatim strict schema.
    ///
    /// The credential is *not* duplicated here: both wire paths read the
    /// single copy owned by `openai_inner` (see `OpenAiProvider::api_key`),
    /// so a long-lived provider keeps one zero-on-drop buffer, not two.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    /// Provider-advertised per-model tool-calling capability (ADR-66 §3
    /// precedence level 2). `None` when the provider publishes no such
    /// metadata. Beats the last-resort name heuristic for the Anthropic-
    /// dialect path and is forwarded to `openai_inner` for the
    /// OpenAI-compatible path.
    advertised_tool_support: Option<bool>,
    /// Pre-built inner OpenAI provider for OpenAI-compatible models.
    openai_inner: OpenAiProvider,
}

impl OpenCodeZenProvider {
    /// Build a provider targeting the OpenCode Zen endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(api_key, model, timeout_secs, OPENCODE_ZEN_API_BASE.to_string())
    }

    /// Build a provider with an explicit API base URL, overriding the Zen default.
    ///
    /// Useful for self-hosted gateways, proxies, or tests.
    pub fn with_api_base(
        api_key: impl Into<SecretString>,
        model: String,
        timeout_secs: u64,
        api_base: String,
    ) -> Self {
        let openai_inner = OpenAiProvider::new(api_key, model.clone(), timeout_secs)
            .with_api_base(api_base.clone())
            .with_reasoning_echo(ReasoningEcho::Always);
        Self {
            model,
            timeout_secs,
            api_base,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            advertised_tool_support: None,
            openai_inner,
        }
    }

    /// The credential both wire paths authenticate with — a borrow, so
    /// callers never materialize a second copy of the key.
    pub(crate) fn api_key(&self) -> &SecretString {
        self.openai_inner.api_key()
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Applies to the Anthropic Messages path handled here and the
    /// OpenAI-compatible path delegated to the inner provider. The Responses
    /// API path renders tool declarations verbatim (strict schema): its
    /// converter is complete, but loose-schema flattening is not applied there
    /// — a weak model on that dialect simply keeps the verbatim schema.
    ///
    /// Defaults to [`concerto_config::ToolSchemaMode::Auto`]: weak
    /// tool-calling models (name heuristic) get loose schemas and the
    /// connector re-nests dot-notation arguments on the way back; every
    /// other model keeps the verbatim strict schema and byte-identical wire
    /// output. See `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.tool_schema_mode = mode;
        self.openai_inner = self.openai_inner.with_tool_schema_mode(mode);
        self
    }

    /// Set the provider-advertised per-model tool-calling capability
    /// (ADR-66 §3 precedence level 2), forwarded to both wire paths.
    pub fn with_advertised_tool_support(mut self, advertised: Option<bool>) -> Self {
        self.advertised_tool_support = advertised;
        self.openai_inner = self.openai_inner.with_advertised_tool_support(advertised);
        self
    }

    /// Resolve the effective model name for a request.
    fn resolve_model(&self, request: &CompletionRequest) -> String {
        if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    /// Build the Anthropic Messages API request body for the Zen endpoint.
    fn build_anthropic_body(&self, request: &CompletionRequest, model: &str) -> serde_json::Value {
        let dialect = AnthropicChatDialect;
        dialect.render_chat_body(request, model, ReasoningEcho::IfPresent)
    }

    /// Build the Responses API request body for Responses-dialect models.
    ///
    /// Uses the easy input format: an array of `{role, content}` items, with
    /// system messages carried as instructions. Tool declarations are rendered
    /// in the Responses **flat** function shape (see
    /// [`Self::render_responses_tools`]); assistant tool calls and tool
    /// results round-trip as `function_call` / `function_call_output` input
    /// items so the conversation stays replayable across turns.
    fn build_responses_body(request: &CompletionRequest, model: &str) -> serde_json::Value {
        let mut instructions = String::new();
        let mut input: Vec<serde_json::Value> = Vec::new();
        for msg in &request.messages {
            match msg.role {
                concerto_core::types::Role::System => {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&msg.content);
                }
                concerto_core::types::Role::User => {
                    input.push(serde_json::json!({"role": "user", "content": msg.content}));
                }
                concerto_core::types::Role::Assistant => {
                    let has_tool_calls =
                        msg.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
                    if !has_tool_calls {
                        // Tool-free assistant turns keep the historical shape
                        // byte-for-byte.
                        input
                            .push(serde_json::json!({"role": "assistant", "content": msg.content}));
                    } else {
                        if !msg.content.is_empty() {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": msg.content,
                            }));
                        }
                        for call in msg.tool_calls.iter().flatten() {
                            // Responses function-call items carry `arguments`
                            // as a JSON-encoded string, like Chat
                            // Completions.
                            let arguments = serde_json::to_string(
                                &crate::protocol::ensure_arguments_object(call.arguments.clone()),
                            )
                            .unwrap_or_else(|_| "{}".to_string());
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.name,
                                "arguments": arguments,
                            }));
                        }
                    }
                }
                concerto_core::types::Role::Tool => {
                    let has_results =
                        msg.tool_results.as_ref().is_some_and(|results| !results.is_empty());
                    if !has_results {
                        input.push(serde_json::json!({
                            "role": "user",
                            "content": format!("[tool result]\n{}", msg.content),
                        }));
                    } else {
                        for result in msg.tool_results.iter().flatten() {
                            let output = match &result.content {
                                serde_json::Value::String(text) => text.clone(),
                                other => other.to_string(),
                            };
                            input.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": result.id,
                                "output": output,
                            }));
                        }
                    }
                }
                // Future `#[non_exhaustive]` variants: drop rather than fail.
                _ => {}
            }
        }
        let mut body = serde_json::json!({
            "model": model,
            "input": input,
            "stream": true,
        });
        if !instructions.is_empty() {
            body["instructions"] = serde_json::Value::String(instructions);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_output_tokens"] = serde_json::json!(max_tokens);
        }
        // Tool declarations use the Responses flat function shape. Omit the
        // key entirely when there are no tools so text-only requests stay
        // byte-identical to before.
        let tools = Self::render_responses_tools(request);
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }

    /// Render `request.tools` into the OpenAI Responses **flat** function
    /// shape: `{"type":"function","name":...,"description":...,
    /// "parameters":{...JSON Schema...}}`.
    ///
    /// This is deliberately NOT the nested Chat-Completions shape
    /// (`{"type":"function","function":{...}}`); sending the nested form to
    /// `/responses` is rejected upstream. Returns an empty vec when the
    /// request carries no tools.
    fn render_responses_tools(request: &CompletionRequest) -> Vec<serde_json::Value> {
        request
            .tools
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect()
    }

    /// Stream a completion using the OpenAI Responses API dialect.
    ///
    /// This path handles Responses-dialect models (genuine Muse models and
    /// explicit prefix entries like `muse-spark-*`), which the Zen gateway
    /// serves only via `POST /responses` with Responses SSE events
    /// (`response.output_text.delta`, `response.completed`, etc.).
    async fn stream_completion_responses(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        // The Responses body builder now renders `request.tools` natively
        // (flat function shape) and the stream parser accumulates function
        // calls, so there is no capability seam to guard here. The former
        // `CapabilityRefused` guard was removed with ADR-75: it converted a
        // converter gap into a permanent model exclusion.
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "opencode",
            dialect = "responses",
            model = %model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/responses", self.api_base);

        let body = Self::build_responses_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .bearer_auth(self.api_key().expose())
                    .header(CONTENT_TYPE, "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_http_error(status, &text, retry_after));
                }
                Ok(r)
            } => result,
        }?;

        let state = ResponsesStreamState::new();
        let cancel = cancel.clone();

        let s = stream! {
            let mut state = state;
            let mut byte_stream = response.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                if cancel.is_cancelled() {
                    yield Err(ProviderError::Cancelled);
                    break;
                }
                let items = match chunk {
                    Ok(bytes) => {
                        let events = state.parser.push_bytes(&bytes);
                        for event in events {
                            state.handle_event(event);
                        }
                        let mut items = Vec::new();
                        while let Some(item) = state.pending.pop_front() {
                            items.push(item);
                        }
                        items
                    }
                    // Stream-retry: a transport fault
                    // mid-stream is retriable (tools execute only
                    // post-assembly — re-issue is side-effect-free within
                    // the bounded attempt budget); framing/parse failures
                    // inside a healthy stream stay fatal.
                    Err(e) => vec![Err(ProviderError::StreamTransport(format!(
                        "connection dropped mid-stream: {}",
                        describe_error_chain(&e)
                    )))]
                };
                for item in items {
                    yield item;
                }
            }
            while let Some(item) = state.pending.pop_front() {
                yield item;
            }
        }
        .boxed();

        Ok(s)
    }

    /// Stream a completion using the Anthropic Messages API dialect.
    ///
    /// This path handles Claude models that the Zen gateway serves via the
    /// Anthropic wire format (`POST /messages`, Anthropic SSE events).
    async fn stream_completion_anthropic(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "opencode",
            dialect = "anthropic",
            model = %model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/messages", self.api_base);

        // Adaptive tool schemas (weak-model tier): when the resolved model
        // matches the loose tier, rewrite the request's tool definitions in
        // place before the dialect renders the body. Strict models are
        // untouched — their wire output stays byte-identical.
        let mut request = request;
        let resolved_mode = crate::capability::resolve_tool_schema_mode(
            "opencode",
            &model,
            self.tool_schema_mode,
            self.advertised_tool_support,
        );
        let tool_adapted =
            crate::adapters::schema_loose::adaptive_tool_schemas_active(resolved_mode, &model);
        if tool_adapted {
            if let Some(tools) = request.tools.as_mut() {
                crate::adapters::schema_loose::adapt_tool_definitions(tools);
            }
        }

        let body = self.build_anthropic_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .header("x-api-key", self.api_key().expose())
                    .header("anthropic-version", "2023-06-01")
                    .header(CONTENT_TYPE, "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_http_error(status, &text, retry_after));
                }
                Ok(r)
            } => result,
        }?;

        let mut state = AnthropicStreamState::new();
        if tool_adapted {
            state.tool_adapted = true;
        }
        let cancel = cancel.clone();

        let s = stream! {
            let mut state = state;
            let mut byte_stream = response.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                if cancel.is_cancelled() {
                    yield Err(ProviderError::Cancelled);
                    break;
                }
                let items = match chunk {
                    Ok(bytes) => {
                        let events = state.parser.push_bytes(&bytes);
                        for event in events {
                            state.handle_event(event);
                        }
                        let mut items = Vec::new();
                        while let Some(item) = state.pending.pop_front() {
                            items.push(item);
                        }
                        items
                    }
                    // Stream-retry: a transport fault
                    // mid-stream is retriable (tools execute only
                    // post-assembly — re-issue is side-effect-free within
                    // the bounded attempt budget); framing/parse failures
                    // inside a healthy stream stay fatal.
                    Err(e) => vec![Err(ProviderError::StreamTransport(format!(
                        "connection dropped mid-stream: {}",
                        describe_error_chain(&e)
                    )))]
                };
                for item in items {
                    yield item;
                }
            }
            while let Some(item) = state.pending.pop_front() {
                yield item;
            }
        }
        .boxed();

        Ok(s)
    }
}

#[async_trait]
impl LlmProvider for OpenCodeZenProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        if needs_anthropic_dialect(&model) {
            self.stream_completion_anthropic(request, cancel).await
        } else if needs_responses_api(&model) {
            self.stream_completion_responses(request, cancel).await
        } else {
            self.openai_inner.stream_completion(request, cancel).await
        }
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        self.openai_inner.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        self.openai_inner.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        "opencode"
    }

    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        self.openai_inner.test_connection(_cancel.clone()).await
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        self.openai_inner.list_models(_cancel.clone()).await
    }
}

// ---------------------------------------------------------------------------
// Anthropic SSE stream parser (Muse/Claude path).
//
// Mirrors the event handling from `crate::anthropic::AnthropicStreamState` but
// is kept local to this module to avoid widening the public API surface of the
// anthropic connector. Handles the four Anthropic SSE event types:
// `content_block_start`, `content_block_delta`, `content_block_stop`,
// `message_stop`.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AnthropicParseState {
    text_acc: HashMap<usize, String>,
    tool_acc: HashMap<usize, (String, String, String)>,
}

struct AnthropicStreamState {
    parser: BufferedSseParser,
    parse: AnthropicParseState,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    /// Whether the request that produced this stream was rendered with
    /// loose (weak-model) tool schemas. When set, emitted tool-call
    /// arguments are re-nested from dot-notation back into the tools'
    /// original nested shape (see `crate::adapters::schema_loose`).
    tool_adapted: bool,
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

/// One in-flight function-call item in a Responses SSE stream.
///
/// Accumulates the metadata from `response.output_item.added`/`.done` and the
/// argument fragments from `response.function_call_arguments.delta`, then
/// produces exactly one [`ToolCall`] chunk.
#[derive(Default, Clone)]
struct ResponsesToolAccum {
    name: String,
    call_id: String,
    arguments: String,
    /// Whether a `ToolCall` chunk was already emitted for this item, so the
    /// two event shapes (`output_item.done` AND
    /// `function_call_arguments.done`) cannot double-emit the same call.
    emitted: bool,
}

/// Responses-dialect SSE state.
///
/// Handles text streaming (`response.output_text.delta`), function-call
/// items (`response.output_item.added`/`.done`) and their streamed arguments
/// (`response.function_call_arguments.delta`/`.done`), and the terminal
/// `response.completed`/`response.done`. Text-only streams see no tool events
/// and behave byte-identically to before.
struct ResponsesStreamState {
    parser: BufferedSseParser,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    /// In-flight function-call items, keyed by the item id (falling back to
    /// the stream's `output_index` or `item_id` when an id is absent).
    tool_acc: HashMap<String, ResponsesToolAccum>,
    /// Set when a completed function-call item carried unrepairable
    /// arguments. The typed error is deferred to the terminal event so the
    /// stream fails loudly instead of emitting a tool call with empty
    /// arguments (which the executor would run as `{}`).
    tool_parse_error: Option<ProviderError>,
}

impl ResponsesStreamState {
    fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            pending: VecDeque::new(),
            tool_acc: HashMap::new(),
            tool_parse_error: None,
        }
    }

    /// A stable per-item key: the item id when present, else the event's
    /// `item_id`, else the `output_index` (the three identifiers OpenAI
    /// Responses events use to correlate a call with its argument deltas).
    fn item_key(data: &serde_json::Value, item: Option<&serde_json::Value>) -> String {
        if let Some(id) = item.and_then(|item| item["id"].as_str()).filter(|id| !id.is_empty()) {
            return id.to_string();
        }
        if let Some(id) = data["item_id"].as_str().filter(|id| !id.is_empty()) {
            return id.to_string();
        }
        if let Some(index) = data["output_index"].as_i64() {
            return format!("index:{index}");
        }
        "function_call".to_string()
    }

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            self.pending.push_back(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }

        let Some(data) = event.data else { return };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&data) else { return };
        match event.event.as_deref().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(serde_json::Value::as_str) {
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: delta.to_owned(),
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }));
                }
            }
            "response.output_item.added" => self.capture_item(&data, false),
            "response.output_item.done" => self.capture_item(&data, true),
            "response.function_call_arguments.delta" => self.append_arguments_delta(&data),
            "response.function_call_arguments.done" => self.finish_arguments(&data),
            "response.completed" | "response.done" => self.finish(),
            _ => {}
        }
    }

    /// Capture a `function_call` item from `response.output_item.added` or
    /// `.done`. On `.done` the full arguments are authoritative, so the
    /// accumulated fragments are replaced before emitting.
    fn capture_item(&mut self, data: &serde_json::Value, done: bool) {
        let item = &data["item"];
        if item["type"].as_str() != Some("function_call") {
            return;
        }
        let key = Self::item_key(data, Some(item));
        let entry = self.tool_acc.entry(key.clone()).or_default();
        if let Some(name) = item["name"].as_str().filter(|name| !name.is_empty()) {
            entry.name = name.to_string();
        }
        if let Some(call_id) = item["call_id"].as_str().filter(|id| !id.is_empty()) {
            entry.call_id = call_id.to_string();
        }
        if let Some(arguments) = item["arguments"].as_str() {
            if done || entry.arguments.is_empty() {
                entry.arguments = arguments.to_string();
            }
        }
        if done {
            self.emit_tool_call(&key);
        }
    }

    /// Append a `response.function_call_arguments.delta` fragment.
    fn append_arguments_delta(&mut self, data: &serde_json::Value) {
        let key = Self::item_key(data, None);
        let entry = self.tool_acc.entry(key).or_default();
        if let Some(delta) = data["delta"].as_str() {
            entry.arguments.push_str(delta);
        }
    }

    /// Handle `response.function_call_arguments.done`: its `arguments` field
    /// is the complete JSON, so it replaces the accumulated fragments, then
    /// the call is emitted. This is also the emit point for providers that
    /// send argument deltas without an `output_item.done`.
    fn finish_arguments(&mut self, data: &serde_json::Value) {
        let key = Self::item_key(data, None);
        let entry = self.tool_acc.entry(key.clone()).or_default();
        if let Some(arguments) = data["arguments"].as_str() {
            entry.arguments = arguments.to_string();
        }
        if let Some(name) = data["name"].as_str().filter(|name| !name.is_empty()) {
            entry.name = name.to_string();
        }
        self.emit_tool_call(&key);
    }

    /// Emit exactly one [`ToolCall`] chunk for `key`, if the accumulated
    /// arguments can be parsed (directly or after repair). Unrepairable
    /// arguments set [`Self::tool_parse_error`] and are surfaced as a typed
    /// error on the terminal event — never a silent `Value::Null` tool call.
    fn emit_tool_call(&mut self, key: &str) {
        let Some(entry) = self.tool_acc.get(key) else { return };
        if entry.emitted || entry.name.is_empty() {
            return;
        }
        let name = entry.name.clone();
        let id = if entry.call_id.is_empty() { key.to_string() } else { entry.call_id.clone() };
        let arguments = entry.arguments.clone();
        let outcome = crate::tool_args::parse_tool_arguments(&arguments);
        if let Some(entry) = self.tool_acc.get_mut(key) {
            entry.emitted = true;
        }
        match outcome {
            Ok(crate::tool_args::ToolArgumentParse::Value(value)) => {
                let arguments = crate::protocol::ensure_arguments_object(value);
                self.pending.push_back(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: Some(ToolCall { id, name, arguments, ..Default::default() }),
                    is_final: false,
                    usage: None,
                }));
            }
            Ok(crate::tool_args::ToolArgumentParse::Empty) => {
                // Argument-less tool call: coerce to `{}` as the executor
                // contract requires (never `Null`).
                let arguments = crate::protocol::ensure_arguments_object(serde_json::Value::Null);
                self.pending.push_back(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: Some(ToolCall { id, name, arguments, ..Default::default() }),
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

    /// Flush any remaining function-call items (a stream that only sent
    /// `output_item.added`), then emit the terminal chunk — or the deferred
    /// typed error if an argument object was unrepairable.
    fn finish(&mut self) {
        let unemitted: Vec<String> = self
            .tool_acc
            .iter()
            .filter(|(_, entry)| !entry.emitted && !entry.name.is_empty())
            .map(|(key, _)| key.clone())
            .collect();
        for key in unemitted {
            self.emit_tool_call(&key);
        }
        if let Some(error) = self.tool_parse_error.take() {
            self.pending.push_back(Err(error));
            return;
        }
        self.pending.push_back(Ok(CompletionChunk {
            reasoning: None,
            delta: String::new(),
            tool_call: None,
            is_final: true,
            usage: None,
        }));
    }
}

impl AnthropicStreamState {
    fn new() -> Self {
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
    /// The Zen gateway's Anthropic-dialect path mirrors the Anthropic SSE
    /// shape (ADR-48 §4): `message_start` carries
    /// `message.usage.input_tokens`, `message_delta` carries the cumulative
    /// `usage.output_tokens`. Only counts actually present on the wire are
    /// recorded — `None` and `0` are both legitimate reports, so no
    /// coalescing happens here. The `message_start` `output_tokens`
    /// placeholder (`1`) is overwritten by the later real cumulative total.
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

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
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
                            let mut args = crate::protocol::ensure_arguments_object(args_json);
                            // Adaptive tool schemas: re-nest dot-notation
                            // arguments from loose-schema streams before the
                            // executor or the tool-call guard validates
                            // against the nested schema.
                            if self.tool_adapted {
                                crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
                            }
                            self.pending.push_back(Ok(CompletionChunk {
                                reasoning: None,
                                delta: String::new(),
                                tool_call: Some(ToolCall {
                                    id,
                                    name,
                                    arguments: args,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_opencode() {
        let p = OpenCodeZenProvider::new("key".to_string(), "model".to_string(), 30);
        assert_eq!(p.provider_name(), "opencode");
    }

    // -----------------------------------------------------------------------
    // Dialect detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn muse_models_use_the_responses_dialect() {
        // muse-spark-* routes via the explicit prefix entry (Responses), so
        // it must not take the Anthropic dialect either.
        assert!(!needs_anthropic_dialect("muse-spark-1.2-contributor-free"));
        assert!(!needs_anthropic_dialect("Muse-Spark-1.2"));
        assert!(!needs_anthropic_dialect("some-muse-model"));
        assert!(needs_responses_api("MUSE-v2"));
        assert!(needs_responses_api("muse-v2"));
        assert!(needs_responses_api("muse-v3"));
        assert!(needs_responses_api("muse-v2.1"));
        assert!(needs_responses_api("muse-v3-pro"));
    }

    /// ADR-66 §5 correction (2026-09-08): the wire dialect follows endpoint
    /// behavior, not family taxonomy. `muse-spark-*` is not a Muse family
    /// member, but the Zen gateway 500s on `/chat/completions` and only
    /// serves it via `POST /responses` (the original fix, 0d511f1) — the
    /// explicit full-id prefix entry overrides the token heuristic.
    #[test]
    fn responses_prefix_table_routes_muse_spark_by_endpoint_behavior() {
        assert!(
            needs_responses_api("muse-spark-1.3-contributor-free"),
            "explicit prefix entry: muse-spark-* 500s on /chat/completions"
        );
        // Siblings and case-insensitivity of the full-id prefix match.
        assert!(needs_responses_api("Muse-Spark-1.2"));
        assert!(needs_responses_api("muse-spark-1.3"));
        // The entry respects token boundaries: a longer name that merely
        // starts with the prefix's characters (minus the trailing `-`)
        // stays on the OpenAI-compatible dialect.
        assert!(!needs_responses_api("muse-sparkless"));
    }

    /// ADR-66 §5 regression: the Responses token heuristic matches whole
    /// Muse family tokens only. Every near-miss here stays on the
    /// OpenAI-compatible dialect — no explicit prefix entry covers them, so
    /// name resemblance alone must never select the Responses dialect.
    #[test]
    fn muse_near_misses_never_route_to_responses() {
        // `muse` inside another token.
        assert!(!needs_responses_api("some-muse-model"));
        assert!(!needs_responses_api("amuse-v2"));
        assert!(!needs_responses_api("museum-2"));
        assert!(!needs_responses_api("musex-v2"));
        // `muse-` prefix without a known family segment after it and
        // without an explicit prefix entry.
        assert!(!needs_responses_api("muse"));
        assert!(!needs_responses_api("muse-pro"));
        assert!(!needs_responses_api("muse-vapor"));
        assert!(!needs_responses_api("muse-latest"));
        // Empty / unrelated names stay on the OpenAI-compatible dialect.
        assert!(!needs_responses_api(""));
        assert!(!needs_responses_api("big-pickle"));
        assert!(!needs_responses_api("deepseek-v4-flash-free"));
    }

    /// ADR-66 §5 regression: the Anthropic heuristic matches the whole
    /// `claude` family token, never a bare substring.
    #[test]
    fn claude_near_misses_never_route_to_anthropic() {
        assert!(needs_anthropic_dialect("claude-3-5-sonnet"));
        assert!(needs_anthropic_dialect("Claude-3-opus"));
        assert!(needs_anthropic_dialect("claude-4"));
        assert!(needs_anthropic_dialect("claude-sonnet-4"));

        // Substring near-misses must stay on the OpenAI-compatible dialect.
        assert!(!needs_anthropic_dialect("claudette-1"));
        assert!(!needs_anthropic_dialect("declaude"));
        assert!(!needs_anthropic_dialect("claudeify-v2"));
        assert!(!needs_anthropic_dialect("sub-claudeify"));
        assert!(!needs_anthropic_dialect(""));
    }

    #[test]
    fn openai_models_do_not_need_anthropic_dialect() {
        assert!(!needs_anthropic_dialect("big-pickle"));
        assert!(!needs_anthropic_dialect("deepseek-v4-flash-free"));
        assert!(!needs_anthropic_dialect("gpt-4o"));
        assert!(!needs_anthropic_dialect("MiMo-7B"));
    }

    #[test]
    fn empty_model_defaults_to_openai() {
        assert!(!needs_anthropic_dialect(""));
    }

    // -----------------------------------------------------------------------
    // Anthropic SSE parser tests
    // -----------------------------------------------------------------------

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
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
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
            state.pending.drain(..).map(|result| result.unwrap()).collect();
        assert!(
            chunks[..chunks.len() - 1].iter().all(|chunk| !chunk.is_final),
            "only the final chunk is terminal"
        );
        assert_eq!(chunks[0].usage, None, "content deltas carry no usage");
        let terminal = chunks.last().unwrap();
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage,
            Some(CompletionUsage { prompt_tokens: Some(25), completion_tokens: Some(15) })
        );
        crate::testing::assert_terminal_usage_contract(&chunks);
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

    // -----------------------------------------------------------------------
    // Anthropic body rendering tests (dialect integration)
    // -----------------------------------------------------------------------

    #[test]
    fn muse_model_renders_anthropic_body_via_dialect() {
        let p = OpenCodeZenProvider::new(
            "key".to_string(),
            "muse-spark-1.2-contributor-free".into(),
            30,
        );
        let request = CompletionRequest {
            messages: vec![concerto_core::types::Message {
                role: concerto_core::types::Role::User,
                content: "Hello".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = p.build_anthropic_body(&request, "muse-spark-1.2-contributor-free");
        // Anthropic wire format: stream is always true, max_tokens defaults to 4096
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["model"], "muse-spark-1.2-contributor-free");
        // Messages use Anthropic content-array format
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn openai_model_body_not_affected_by_anthropic_path() {
        // big-pickle should not trigger the Anthropic path
        assert!(!needs_anthropic_dialect("big-pickle"));
    }

    // -----------------------------------------------------------------------
    // Responses-dialect tool converter (ADR-75)
    // -----------------------------------------------------------------------

    fn responses_event(event_type: &str, data: &str) -> crate::sse::SseEvent {
        crate::sse::SseEvent {
            event: Some(event_type.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        }
    }

    fn tool_request() -> CompletionRequest {
        CompletionRequest {
            model: "muse-v2".into(),
            messages: vec![concerto_core::types::Message {
                role: concerto_core::types::Role::User,
                content: "list files".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            tools: Some(vec![concerto_core::types::ToolDefinition {
                name: "shell".into(),
                description: "Run a command.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                }),
            }]),
            ..Default::default()
        }
    }

    /// The Responses request body renders tools in the **flat** function
    /// shape (`{"type":"function","name":...,"parameters":...}`), NOT the
    /// nested Chat-Completions shape (`{"function":{...}}`).
    #[test]
    fn responses_body_renders_flat_function_tools() {
        let request = tool_request();
        let body = OpenCodeZenProvider::build_responses_body(&request, "muse-v2");
        let tools = body["tools"].as_array().expect("tools array present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "shell");
        assert_eq!(tools[0]["description"], "Run a command.");
        assert_eq!(tools[0]["parameters"]["required"][0], "command");
        assert!(tools[0].get("function").is_none(), "Responses uses the flat shape");
    }

    /// No tools ⇒ the `tools` key is omitted entirely (text-only body stays
    /// byte-identical to before the converter was completed).
    #[test]
    fn responses_body_omits_tools_key_when_absent() {
        let request = CompletionRequest {
            model: "muse-v2".into(),
            messages: vec![concerto_core::types::Message {
                role: concerto_core::types::Role::User,
                content: "hello".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = OpenCodeZenProvider::build_responses_body(&request, "muse-v2");
        assert!(body.get("tools").is_none(), "no tools => no tools key");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "hello");
    }

    /// Assistant tool calls and tool results round-trip as Responses
    /// `function_call` / `function_call_output` input items.
    #[test]
    fn responses_body_round_trips_tool_call_and_output() {
        let request = CompletionRequest {
            model: "muse-v2".into(),
            messages: vec![
                concerto_core::types::Message {
                    role: concerto_core::types::Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                        ..Default::default()
                    }]),
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                concerto_core::types::Message {
                    role: concerto_core::types::Role::Tool,
                    content: "file-a\nfile-b".into(),
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: "call_1".into(),
                        name: "shell".into(),
                        content: serde_json::json!("file-a\nfile-b"),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };
        let body = OpenCodeZenProvider::build_responses_body(&request, "muse-v2");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["name"], "shell");
        assert_eq!(input[0]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["output"], "file-a\nfile-b");
    }

    /// A scripted Responses SSE stream (output_item.done carrying the
    /// function_call) yields exactly one `ToolCall` with the accumulated
    /// arguments, then the terminal chunk.
    #[test]
    fn responses_stream_emits_tool_call_from_output_item_done() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":""}}"#,
        ));
        state.handle_event(responses_event(
            "response.function_call_arguments.delta",
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#,
        ));
        state.handle_event(responses_event(
            "response.function_call_arguments.delta",
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#,
        ));
        state.handle_event(responses_event(
            "response.output_item.done",
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{\"command\":\"ls\"}"}}"#,
        ));
        state.handle_event(responses_event(
            "response.completed",
            r#"{"type":"response.completed"}"#,
        ));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 2, "one tool call + terminal chunk");
        let call = chunks[0].tool_call.as_ref().expect("tool call emitted");
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "shell");
        assert_eq!(call.arguments, serde_json::json!({"command": "ls"}));
        assert!(chunks[1].is_final);
    }

    /// Argument deltas accumulate to the whole JSON when the stream emits
    /// `function_call_arguments.done` (no `output_item.done`).
    #[test]
    fn responses_stream_accumulates_argument_deltas() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"write","arguments":""}}"#,
        ));
        for delta in
            [r#"{"path":"#.to_string(), r#""a.txt","content":"#.to_string(), r#""hi"}"#.to_string()]
        {
            let data = serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_2",
                "delta": delta,
            })
            .to_string();
            state.handle_event(responses_event("response.function_call_arguments.delta", &data));
        }
        state.handle_event(responses_event(
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_2","arguments":"{\"path\":\"a.txt\",\"content\":\"hi\"}"}"#,
        ));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        let call = chunks[0].tool_call.as_ref().unwrap();
        assert_eq!(call.arguments, serde_json::json!({"path": "a.txt", "content": "hi"}));
    }

    /// Truncated streamed arguments are repaired by `tool_args` before the
    /// `ToolCall` is emitted (never a `Value::Null` with an empty object).
    #[test]
    fn responses_stream_repairs_truncated_arguments() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_3","call_id":"call_3","name":"shell","arguments":""}}"#,
        ));
        state.handle_event(responses_event(
            "response.output_item.done",
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_3","call_id":"call_3","name":"shell","arguments":"{\"command\":\"cargo te"}}"#,
        ));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        let call = chunks[0].tool_call.as_ref().unwrap();
        assert_eq!(call.name, "shell");
        assert_eq!(call.arguments, serde_json::json!({"command": "cargo te"}));
    }

    /// Unrepairable arguments surface as `ProviderError::InvalidResponse` on
    /// the terminal event — never a tool call with silently-empty arguments.
    #[test]
    fn responses_stream_unrepairable_arguments_fail_loudly() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_4","call_id":"call_4","name":"shell","arguments":""}}"#,
        ));
        state.handle_event(responses_event(
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_4","arguments":"this is not json at all"}"#,
        ));
        state.handle_event(responses_event(
            "response.completed",
            r#"{"type":"response.completed"}"#,
        ));

        let chunks: Vec<Result<CompletionChunk, ProviderError>> = state.pending.drain(..).collect();
        assert!(
            chunks.iter().all(|chunk| chunk.as_ref().map_or(true, |c| c.tool_call.is_none())),
            "no tool call may be emitted from unparseable arguments: {chunks:?}"
        );
        let error = chunks.last().unwrap().as_ref().expect_err("terminal chunk is an error");
        assert!(matches!(error, ProviderError::InvalidResponse(_)), "got: {error:?}");
    }

    /// The two event shapes describing the same call (`function_call_arguments
    /// .done` AND `output_item.done`) must not double-emit.
    #[test]
    fn responses_stream_does_not_double_emit() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_item.added",
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_5","call_id":"call_5","name":"shell","arguments":""}}"#,
        ));
        state.handle_event(responses_event(
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_5","arguments":"{\"command\":\"ls\"}"}"#,
        ));
        state.handle_event(responses_event(
            "response.output_item.done",
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_5","call_id":"call_5","name":"shell","arguments":"{\"command\":\"ls\"}"}}"#,
        ));

        let tool_calls: Vec<_> =
            state.pending.iter().flatten().filter(|chunk| chunk.tool_call.is_some()).collect();
        assert_eq!(tool_calls.len(), 1, "the same call must be emitted exactly once");
    }

    /// Text-only Responses streaming is unchanged: text deltas pass through
    /// and a tool-free completion yields only the terminal chunk.
    #[test]
    fn responses_text_only_stream_unchanged() {
        let mut state = ResponsesStreamState::new();
        state.handle_event(responses_event(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
        ));
        state.handle_event(responses_event(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":" world"}"#,
        ));
        state.handle_event(responses_event(
            "response.completed",
            r#"{"type":"response.completed"}"#,
        ));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].delta, "Hello");
        assert_eq!(chunks[1].delta, " world");
        assert!(chunks.iter().all(|chunk| chunk.tool_call.is_none()));
        assert!(chunks[2].is_final);
    }

    /// Contract fixture: the full Responses tool round trip — tools rendered
    /// in, a recorded SSE tool call parsed out, and the result rendered back as
    /// a `function_call_output` on the next request. A converter that silently
    /// drops any leg fails this test.
    #[test]
    fn responses_dialect_tool_round_trip_contract_fixture() {
        // Leg 1 — tools in: the flat function shape is on the wire.
        let first = tool_request();
        let body = OpenCodeZenProvider::build_responses_body(&first, "muse-v2");
        let tools = body["tools"].as_array().expect("tools declared");
        assert_eq!(tools[0]["name"], "shell");
        assert!(tools[0].get("function").is_none(), "flat Responses shape");

        // Leg 2 — tool call out: a recorded SSE function-call stream yields
        // one ToolCall with the accumulated arguments.
        let mut state = ResponsesStreamState::new();
        for (event_type, data) in [
            (
                "response.output_item.added",
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"shell","arguments":""}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{\"command\":"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"\"ls\"}"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_9","call_id":"call_9","name":"shell","arguments":"{\"command\":\"ls\"}"}}"#,
            ),
            ("response.completed", r#"{"type":"response.completed"}"#),
        ] {
            state.handle_event(responses_event(event_type, data));
        }
        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        let call = chunks[0].tool_call.clone().expect("tool call parsed from SSE");
        assert_eq!(call.id, "call_9");
        assert_eq!(call.name, "shell");
        assert_eq!(call.arguments, serde_json::json!({"command": "ls"}));

        // Leg 3 — result back in: the assistant call and its tool result
        // render as function_call / function_call_output items.
        let follow_up = CompletionRequest {
            model: "muse-v2".into(),
            messages: vec![
                concerto_core::types::Message {
                    role: concerto_core::types::Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(vec![call.clone()]),
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                concerto_core::types::Message {
                    role: concerto_core::types::Role::Tool,
                    content: "file-a".into(),
                    tool_calls: None,
                    tool_results: Some(vec![concerto_core::types::ToolResult {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        content: serde_json::json!("file-a"),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            tools: first.tools.clone(),
            ..Default::default()
        };
        let body = OpenCodeZenProvider::build_responses_body(&follow_up, "muse-v2");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_9");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_9");
    }

    /// Inverted from the removed `responses_path_refuses_tool_declarations`:
    /// a tool-carrying request routed to the Responses dialect no longer
    /// refuses. With an unroutable local base it fails at the network
    /// boundary (never `CapabilityRefused`), proving the converter is reached.
    #[tokio::test]
    async fn responses_path_no_longer_refuses_tool_declarations() {
        let p = OpenCodeZenProvider::with_api_base(
            "key".to_string(),
            "muse-v2".into(),
            30,
            "http://127.0.0.1:1".into(),
        );
        let request = tool_request();
        let result = p.stream_completion(request, concerto_core::CancellationToken::new()).await;
        let Err(error) = result else {
            panic!("no server in tests — the request must fail");
        };
        assert!(
            !matches!(error, ProviderError::CapabilityRefused { .. }),
            "tool-carrying Responses request must not be capability-refused: {error:?}"
        );
    }
}
