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
use concerto_core::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget, ToolCall};
use concerto_core::CancellationToken;
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

/// Split a model name into lowercase family tokens.
///
/// Model ids are hyphen-delimited (`muse-v2`, `claude-sonnet-4`); the
/// hyphen is the only family separator honored. A token is the whole
/// dash-delimited word, so substring collisions inside larger tokens are
/// impossible. Tokens are owned — callers keep them as a standalone list.
fn tokenize_model_name(model: &str) -> Vec<String> {
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
    api_key: String,
    model: String,
    timeout_secs: u64,
    api_base: String,
    /// Tool-schema presentation tier (adaptive tool schemas) for the
    /// provider's own Anthropic-dialect path. The OpenAI-compatible path
    /// delegates to `openai_inner`, which carries its own copy. `Auto`
    /// (default) keeps every non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    /// Pre-built inner OpenAI provider for OpenAI-compatible models.
    openai_inner: OpenAiProvider,
}

impl OpenCodeZenProvider {
    /// Build a provider targeting the OpenCode Zen endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(api_key, model, timeout_secs, OPENCODE_ZEN_API_BASE.to_string())
    }

    /// Build a provider with an explicit API base URL, overriding the Zen default.
    ///
    /// Useful for self-hosted gateways, proxies, or tests.
    pub fn with_api_base(
        api_key: String,
        model: String,
        timeout_secs: u64,
        api_base: String,
    ) -> Self {
        let openai_inner = OpenAiProvider::new(api_key.clone(), model.clone(), timeout_secs)
            .with_api_base(api_base.clone())
            .with_reasoning_echo(ReasoningEcho::Always);
        Self {
            api_key,
            model,
            timeout_secs,
            api_base,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            openai_inner,
        }
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Applies to both wire paths: the Anthropic Messages path handled here
    /// and the OpenAI-compatible path delegated to the inner provider. The
    /// Responses API path (Responses-dialect models, e.g. Muse and
    /// `muse-spark-*`) carries no tool declarations at all, so there is
    /// nothing to adapt there.
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
    /// system messages carried as instructions.
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
                    input.push(serde_json::json!({"role": "assistant", "content": msg.content}));
                }
                concerto_core::types::Role::Tool => {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": format!("[tool result]\n{}", msg.content),
                    }));
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
        body
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
        // ADR-66 §2(b) fail-loud seam: the Responses body builder carries no
        // tool declarations at all, so a tool-carrying request routed here
        // would be silently degraded to text-only. Refuse instead — the
        // harness routes tool tasks to a capable path (native or the ADR-66
        // §4 text-fallback driver) or fails before spend.
        if request.tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
            return Err(ProviderError::CapabilityRefused {
                provider: "opencode".to_string(),
                model,
                capability: TOOL_CALLING_CAPABILITY.to_string(),
            });
        }
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
                    .bearer_auth(&self.api_key)
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
                    // ADR-55 Phase 2e stream-retry: a transport fault
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
        let tool_adapted = crate::adapters::schema_loose::adaptive_tool_schemas_active(
            self.tool_schema_mode,
            &model,
        );
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
                    .header("x-api-key", &self.api_key)
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
                    // ADR-55 Phase 2e stream-retry: a transport fault
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
}

struct ResponsesStreamState {
    parser: BufferedSseParser,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
}

impl ResponsesStreamState {
    fn new() -> Self {
        Self { parser: BufferedSseParser::new(), pending: VecDeque::new() }
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
            "response.completed" | "response.done" => {
                self.pending.push_back(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }));
            }
            _ => {}
        }
    }
}

impl AnthropicStreamState {
    fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            parse: AnthropicParseState::default(),
            pending: VecDeque::new(),
            tool_adapted: false,
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
                    let args_json = if args_str.trim().is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null)
                    };
                    let mut args = crate::protocol::ensure_arguments_object(args_json);
                    // Adaptive tool schemas: re-nest dot-notation arguments
                    // from loose-schema streams before the executor or the
                    // tool-call guard validates against the nested schema.
                    if self.tool_adapted {
                        crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
                    }
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(ToolCall { id, name, arguments: args }),
                        is_final: false,
                        usage: None,
                    }));
                }
            }
            "message_stop" => {
                self.pending.push_back(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }));
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

    // -----------------------------------------------------------------------
    // Anthropic body rendering tests (dialect integration)
    // -----------------------------------------------------------------------

    #[test]
    fn muse_model_renders_anthropic_body_via_dialect() {
        let p =
            OpenCodeZenProvider::new("key".into(), "muse-spark-1.2-contributor-free".into(), 30);
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

    /// ADR-66 §2(b) fail-loud seam: a tool-carrying request routed to the
    /// Responses dialect must error before any network I/O — never silently
    /// drop the tool declarations.
    #[tokio::test]
    async fn responses_path_refuses_tool_declarations() {
        let p = OpenCodeZenProvider::new("key".into(), "muse-v2".into(), 30);
        let request = CompletionRequest {
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
                name: "filesystem".into(),
                description: "File ops.".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]),
            ..Default::default()
        };
        let result = p.stream_completion(request, concerto_core::CancellationToken::new()).await;
        let Err(error) = result else {
            panic!("tool-carrying Responses request must be refused");
        };
        match error {
            ProviderError::CapabilityRefused { provider, model, capability } => {
                assert_eq!(provider, "opencode");
                assert_eq!(model, "muse-v2");
                assert_eq!(capability, "tool_calling");
            }
            other => panic!("expected CapabilityRefused, got: {other:?}"),
        }
    }

    /// A tool-free request to a Muse model does NOT hit the refusal seam
    /// (the guard must not fire on absent or empty tool lists).
    #[tokio::test]
    async fn responses_path_accepts_tool_free_request_guard_only() {
        let p = OpenCodeZenProvider::with_api_base(
            "key".into(),
            "muse-v2".into(),
            30,
            // Unroutable local port: the connection fails fast and locally,
            // keeping this test network-free.
            "http://127.0.0.1:1".into(),
        );
        let request = CompletionRequest {
            model: "muse-v2".into(),
            messages: Vec::new(),
            tools: None,
            ..Default::default()
        };
        let result = p.stream_completion(request, concerto_core::CancellationToken::new()).await;
        let Err(error) = result else {
            panic!("no server in tests — the request must fail");
        };
        assert!(
            !matches!(error, ProviderError::CapabilityRefused { .. }),
            "tool-less request must not be capability-refused: {error:?}"
        );
    }
}
