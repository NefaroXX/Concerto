//! `opencode-local` — talks to a local `opencode serve` instance instead of
//! the Zen HTTP relay.
//!
//! # Why this exists
//!
//! A previous attempt (`2f63274`) reached OpenCode's free models by
//! impersonating OpenCode's anonymous client over HTTP
//! (`Authorization: Bearer public`). Live testing proved the relay refuses it
//! server-side (`403 FreeTierError` for 7 of the 8 zero-cost models, `429` for
//! the rest). The mechanism that actually works is a headless `opencode serve`
//! process: all eight free models return completions and tool calling works.
//!
//! # Contract (verified live)
//!
//! - **Auth**: HTTP Basic, username `opencode`, password the server's
//!   `OPENCODE_SERVER_PASSWORD`. Unauthenticated and Bearer requests get `401`.
//! - **Models**: `GET {base}/provider` → `{all, default, connected}`. `all` is
//!   an array of provider objects; the `opencode` entry carries `models`, a map
//!   of id → model with `cost: {input, output}`, `tool_call`, `limit`, …
//! - **Create session**: `POST {base}/session` body `{"directory": "<abs>"}` →
//!   `{id}`.
//! - **Send message**: `POST {base}/session/{id}/message` body
//!   `{"providerID":"opencode","modelID":"<model>","parts":[{"type":"text",
//!   "text":"…"}]}` → `{info, parts, tokens?}` where each part has `type` in
//!   `text` | `reasoning` | `tool` | `step-start` | `step-finish` | ….
//!
//! # Free-ness is cost, never a name
//!
//! [`list_models`](OpenCodeLocalProvider::list_models) returns only entries
//! whose `cost.input == 0` (via [`crate::provider_defs::is_free_cost`]). A
//! `-free` suffix never makes a paid model free, and a zero-cost model with no
//! `free` marker (`big-pickle`) is still free.
//!
//! # Tool calling: what this connector can and cannot do
//!
//! The server's message endpoint does **not** accept client tool schemas. Its
//! `tools` request field is a `{toolName: bool}` map that the server applies as
//! **session permissions** over its *own* tool registry (see upstream
//! `packages/opencode/src/session/prompt.ts`: the map is turned into
//! `{permission, action: allow|deny, pattern: "*"}` rules), so Concerto's
//! [`ToolDefinition`](concerto_core::types::ToolDefinition)s are never sent.
//! The server's tools also execute in the server's own session/directory —
//! bypassing Concerto's sandbox, permission checks, and audit trail — so they
//! are not Concerto's tool loop.
//!
//! Consequently [`list_models`](OpenCodeLocalProvider::list_models) reports
//! `supports_tool_calling: false` for every model, even when the server
//! advertises `tool_call: true`. That is deliberate: it selects the ADR-66 §4
//! prompt-text fallback driver, which drives Concerto's tool loop itself by
//! rendering the tools into the prompt and parsing the model's
//! `<tool_calls>` block. The server is used as a **text/reasoning completion
//! backend only**.
//!
//! `tool` parts the server returns (it may have executed its own tools) are
//! still mapped to [`CompletionChunk::tool_call`] so the wire is not silently
//! dropped. Because the fallback driver discards provider-native tool calls in
//! favour of its parsed text calls, they do not drive a second execution.
//!
//! **Limitation**: because the `tools` map only sets permissions, the connector
//! cannot reliably disable the server's own toolset. If the model chooses to
//! call a server tool, the server executes it in the configured directory. Run
//! `opencode serve` with an agent/config that exposes no tools when strict
//! isolation is required.
//!
//! # Conversation history
//!
//! Each completion creates a fresh server session, so the connector must carry
//! the whole conversation itself. The server's own `SystemPrompt` builds only
//! its environment/model prompt and its `toModelMessages` ignores the request's
//! `system` field, so the system prompt and the full message history are folded
//! into the single `text` part (see [`render_conversation`]). A lone user
//! message is sent verbatim so simple requests stay clean.

use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, CompletionUsage, Message, ModelInfo, Role, TokenBudget,
    ToolCall,
};
use concerto_core::{CancellationToken, SecretString};
use futures::stream;
use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use serde_json::Value;

/// Default base URL of a local `opencode serve` instance.
///
/// Matches the server's own default (`opencode serve --port 4096`).
pub const OPENCODE_LOCAL_DEFAULT_BASE: &str = "http://127.0.0.1:4096";

/// The HTTP Basic username the server expects. Upstream defaults to `opencode`
/// (`packages/opencode/src/server/auth.ts`).
const OPENCODE_BASIC_USER: &str = "opencode";

/// The provider id the server exposes for its own models on `GET /provider`.
const OPENCODE_PROVIDER_ID: &str = "opencode";

/// Response-side token reservation used with the shared capacity table.
const DEFAULT_RESERVED_FOR_RESPONSE: u64 = 4_000;

/// First-class provider for a local `opencode serve` instance.
///
/// See the module docs for the wire contract, the cost-only free-model rule,
/// and the tool-calling determination.
pub struct OpenCodeLocalProvider {
    /// The server password (HTTP Basic). Zero-on-drop and redacting.
    password: SecretString,
    /// Configured model id; a request-level `model` overrides it.
    model: String,
    timeout_secs: u64,
    /// Base URL of the server, without a trailing slash.
    api_base: String,
}

impl OpenCodeLocalProvider {
    /// Build a provider for the default local server.
    pub fn new(password: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(password, model, timeout_secs, OPENCODE_LOCAL_DEFAULT_BASE.to_string())
    }

    /// Build a provider with an explicit base URL (tests, non-default ports).
    pub fn with_api_base(
        password: impl Into<SecretString>,
        model: String,
        timeout_secs: u64,
        api_base: String,
    ) -> Self {
        Self { password: password.into(), model, timeout_secs, api_base }
    }

    /// The base URL this provider builds its request paths from.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Attach HTTP Basic authentication.
    fn basic(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(OPENCODE_BASIC_USER, Some(self.password.expose()))
    }

    fn base(&self) -> &str {
        self.api_base.trim_end_matches('/')
    }

    fn provider_url(&self) -> String {
        format!("{}/provider", self.base())
    }

    fn session_url(&self) -> String {
        format!("{}/session", self.base())
    }

    fn message_url(&self, session_id: &str) -> String {
        format!("{}/session/{}/message", self.base(), session_id)
    }

    /// The single actionable connection failure: the most common user error is
    /// simply not having started the server.
    fn connection_error(&self, error: &reqwest::Error) -> ProviderError {
        ProviderError::Other(format!(
            "opencode-local: cannot reach the OpenCode server at {} ({}). \
             Start it with `opencode serve --port 4096` and set OPENCODE_SERVER_PASSWORD \
             to the same value the server was started with.",
            self.api_base,
            describe_error_chain(error),
        ))
    }

    /// Decode a JSON response, mapping auth and HTTP failures consistently for
    /// both the session and message endpoints.
    async fn decode_json(
        response: reqwest::Response,
        endpoint: &str,
    ) -> Result<Value, ProviderError> {
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ProviderError::AuthFailure);
        }
        if !status.is_success() {
            let retry_after = crate::retry::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::HttpStatus {
                status: status.as_u16(),
                retry_after,
                message: text,
            });
        }
        response.json().await.map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "opencode-local: invalid `{endpoint}` JSON: {}",
                describe_error_chain(&error)
            ))
        })
    }

    /// Build the `POST /session/{id}/message` body.
    ///
    /// `providerID`/`modelID` are top-level per the verified server contract;
    /// the whole conversation is rendered into a single text part (see the
    /// module docs for why the `system` field is not used).
    fn build_message_body(request: &CompletionRequest, model: &str) -> Value {
        serde_json::json!({
            "providerID": OPENCODE_PROVIDER_ID,
            "modelID": model,
            "parts": [{
                "type": "text",
                "text": render_conversation(&request.messages),
            }],
        })
    }
}

/// Resolve the working directory handed to the server session.
///
/// The `LlmProvider` trait carries no working-directory context and the factory
/// has no such mechanism, so the process's current directory is used. The
/// server executes any of its own tools relative to this directory.
fn working_directory() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

/// Render Concerto's messages into the single text part the server accepts.
///
/// A lone user message is sent verbatim; otherwise each message is labelled by
/// role so the model can follow the conversation. This is required because the
/// server ignores the request's `system` field and keeps no history across the
/// fresh session created per completion.
fn render_conversation(messages: &[Message]) -> String {
    if messages.len() == 1 && messages[0].role == Role::User {
        return messages[0].content.clone();
    }
    let mut out = String::new();
    for message in messages {
        let label = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool result",
            // Future `#[non_exhaustive]` variants: drop rather than fail.
            _ => continue,
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push('[');
        out.push_str(label);
        out.push_str("]\n");
        out.push_str(&message.content);
        for call in message.tool_calls.iter().flatten() {
            out.push_str(&format!(
                "\n<tool_call name=\"{}\">{}</tool_call>",
                call.name, call.arguments
            ));
        }
    }
    out
}

/// Parse the `GET /provider` body into the zero-cost model catalog.
///
/// Free-ness is `cost.input == 0` only. `supports_tool_calling` is populated
/// from the entry's `tool_call` field here; the transport gate that suppresses
/// it lives in [`OpenCodeLocalProvider::list_models`].
fn parse_free_models(provider_json: &Value) -> Result<Vec<ModelInfo>, ProviderError> {
    let providers = provider_json.get("all").and_then(Value::as_array).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "opencode-local: `/provider` response is missing the `all` provider array".to_string(),
        )
    })?;
    let opencode = providers
        .iter()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some(OPENCODE_PROVIDER_ID))
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "opencode-local: `/provider` did not list the `opencode` provider".to_string(),
            )
        })?;
    let models = opencode.get("models").and_then(Value::as_object).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "opencode-local: the `opencode` provider has no `models` object".to_string(),
        )
    })?;

    let mut free: Vec<ModelInfo> = Vec::new();
    for (id, entry) in models {
        let cost_input =
            entry.get("cost").and_then(|cost| cost.get("input")).and_then(Value::as_f64);
        if !crate::provider_defs::is_free_cost(cost_input) {
            continue;
        }
        let tool_call = entry.get("tool_call").and_then(Value::as_bool).unwrap_or(false);
        free.push(ModelInfo {
            id: id.clone(),
            name: Some(id.clone()),
            owned_by: Some(OPENCODE_PROVIDER_ID.to_string()),
            supports_tool_calling: Some(tool_call),
        });
    }
    free.sort_by_key(|model| model.id.to_lowercase());
    Ok(free)
}

/// Read an integer token count that may arrive as a JSON integer or float.
fn token_count(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(count) = value.as_u64() {
        return Some(count);
    }
    value.as_f64().filter(|count| *count >= 0.0).map(|count| count as u64)
}

/// Populate usage from the assistant message's `tokens` object.
///
/// The server puts `tokens` on `info` (the assistant message); a top-level
/// `tokens` is accepted too. Empty is not a measurement: when neither count is
/// present, `None` is returned rather than `Some(0, 0)`.
fn parse_usage(json: &Value) -> Option<CompletionUsage> {
    let tokens =
        json.get("info").and_then(|info| info.get("tokens")).or_else(|| json.get("tokens"))?;
    let prompt_tokens = token_count(tokens.get("input"));
    let completion_tokens = token_count(tokens.get("output"));
    if prompt_tokens.is_none() && completion_tokens.is_none() {
        return None;
    }
    Some(CompletionUsage { prompt_tokens, completion_tokens })
}

/// Parse a tool part's arguments.
///
/// The server exposes the model's raw argument string on a pending state
/// (`state.raw`) and the parsed object on a completed one (`state.input`). A
/// string form is routed through [`crate::tool_args::parse_tool_arguments`] so
/// malformed arguments fail loudly instead of becoming `Null`; an object is
/// passed through [`crate::protocol::ensure_arguments_object`].
fn parse_tool_part_arguments(part: &Value, tool_name: &str) -> Result<Value, ProviderError> {
    let state = part.get("state");
    if let Some(raw) = state.and_then(|state| state.get("raw")).and_then(Value::as_str) {
        if !raw.trim().is_empty() {
            return parse_raw_arguments(raw, tool_name);
        }
    }
    if let Some(input) = state.and_then(|state| state.get("input")) {
        return match input {
            Value::String(raw) => parse_raw_arguments(raw, tool_name),
            Value::Object(_) => Ok(crate::protocol::ensure_arguments_object(input.clone())),
            Value::Null => Ok(serde_json::json!({})),
            other => parse_raw_arguments(&other.to_string(), tool_name),
        };
    }
    if let Some(raw) = part.get("arguments").and_then(Value::as_str) {
        return parse_raw_arguments(raw, tool_name);
    }
    Ok(serde_json::json!({}))
}

fn parse_raw_arguments(raw: &str, tool_name: &str) -> Result<Value, ProviderError> {
    match crate::tool_args::parse_tool_arguments(raw) {
        Ok(crate::tool_args::ToolArgumentParse::Value(value)) => {
            Ok(crate::protocol::ensure_arguments_object(value))
        }
        Ok(crate::tool_args::ToolArgumentParse::Empty) => Ok(serde_json::json!({})),
        Err(error) => Err(ProviderError::InvalidResponse(format!(
            "opencode-local: unparseable tool-call arguments for '{tool_name}': {error}"
        ))),
    }
}

/// Map one server `tool` part to a canonical [`ToolCall`].
fn tool_call_from_part(part: &Value) -> Result<ToolCall, ProviderError> {
    let name = part.get("tool").and_then(Value::as_str).unwrap_or_default().to_string();
    let id = part
        .get("callID")
        .and_then(Value::as_str)
        .or_else(|| part.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let arguments = parse_tool_part_arguments(part, &name)?;
    Ok(ToolCall { id, name, arguments, ..Default::default() })
}

/// Map the server's response `parts` to canonical [`CompletionChunk`]s.
///
/// `text` → text, `reasoning` → `reasoning`, `tool` → a [`ToolCall`] with
/// parsed arguments; `step-start`/`step-finish`/`file`/`patch`/… carry no
/// completion content and are skipped. Usage is attached to the terminal chunk
/// only (ADR-48 §4). A malformed tool argument surfaces as
/// [`ProviderError::InvalidResponse`] rather than a `Null` argument.
fn map_response_parts(json: &Value) -> Result<Vec<CompletionChunk>, ProviderError> {
    let parts = json.get("parts").and_then(Value::as_array).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "opencode-local: message response has no `parts` array".to_string(),
        )
    })?;
    let usage = parse_usage(json);

    let mut chunks: Vec<CompletionChunk> = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        chunks.push(CompletionChunk {
                            delta: text.to_string(),
                            reasoning: None,
                            tool_call: None,
                            is_final: false,
                            usage: None,
                        });
                    }
                }
            }
            Some("reasoning") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        chunks.push(CompletionChunk {
                            delta: String::new(),
                            reasoning: Some(text.to_string()),
                            tool_call: None,
                            is_final: false,
                            usage: None,
                        });
                    }
                }
            }
            Some("tool") => {
                chunks.push(CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: Some(tool_call_from_part(part)?),
                    is_final: false,
                    usage: None,
                });
            }
            _ => {}
        }
    }

    chunks.push(CompletionChunk {
        delta: String::new(),
        reasoning: None,
        tool_call: None,
        is_final: true,
        usage,
    });
    Ok(chunks)
}

#[async_trait]
impl LlmProvider for OpenCodeLocalProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = if request.model.trim().is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        };
        let client = crate::new_client(self.timeout_secs);

        // 1. Create a fresh session bound to the working directory.
        let session_body = serde_json::json!({ "directory": working_directory() });
        let session_response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = self
                .basic(client.post(self.session_url()))
                .header(CONTENT_TYPE, "application/json")
                .json(&session_body)
                .send() => result.map_err(|error| self.connection_error(&error))?,
        };
        let session_json = Self::decode_json(session_response, "/session").await?;
        let session_id =
            session_json.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(
                || {
                    ProviderError::InvalidResponse(
                        "opencode-local: `/session` response has no `id`".to_string(),
                    )
                },
            )?;

        // 2. Post the message and read the complete response.
        let body = Self::build_message_body(&request, &model);
        let message_response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = self
                .basic(client.post(self.message_url(&session_id)))
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send() => result.map_err(|error| self.connection_error(&error))?,
        };
        let json = Self::decode_json(message_response, "/session/{id}/message").await?;

        let chunks = map_response_parts(&json)?;
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        // The `/provider` `limit` field is only reachable through an async
        // call; this trait method is synchronous and the connector is
        // stateless, so the shared model-capacity table (with its conservative
        // fallback) is used instead.
        crate::budget::budget_for_model(model, DEFAULT_RESERVED_FOR_RESPONSE)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        // Only zero-cost models are exposed by `list_models`, so every
        // completion through this provider is free.
        0.0
    }

    fn provider_name(&self) -> &'static str {
        "opencode-local"
    }

    async fn test_connection(&self, cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = self.basic(client.get(self.provider_url())).send() => {
                result.map_err(|error| self.connection_error(&error))?
            }
        };
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(ProviderError::AuthFailure);
        }
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ProviderError::Other(format!(
                "opencode-local: `/provider` returned {}",
                response.status()
            )))
        }
    }

    async fn list_models(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = self.basic(client.get(self.provider_url())).send() => {
                result.map_err(|error| self.connection_error(&error))?
            }
        };
        let json = Self::decode_json(response, "/provider").await?;

        let mut models = parse_free_models(&json)?;
        // Transport gate: the HTTP API cannot carry Concerto's tool schemas
        // (its `tools` map only toggles permissions over the server's own
        // registry), so no model is usable for Concerto's native tool loop.
        // Reporting `false` selects the ADR-66 §4 prompt-text fallback driver,
        // which drives Concerto's tools itself. See the module docs.
        for model in &mut models {
            model.supports_tool_calling = Some(false);
        }
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{CompletionChunk, Message, Role};
    use futures::TryStreamExt;
    use serde_json::json;
    use std::time::Duration;

    /// Password used by every HTTP test; the expected Basic value is pinned.
    const TEST_PASSWORD: &str = "test-server-password";
    /// `base64("opencode:test-server-password")`.
    const TEST_BASIC: &str = "b3BlbmNvZGU6dGVzdC1zZXJ2ZXItcGFzc3dvcmQ=";

    fn user_message(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    fn request(model: &str, messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest { model: model.to_string(), messages, ..Default::default() }
    }

    fn provider(base: String) -> OpenCodeLocalProvider {
        OpenCodeLocalProvider::with_api_base(TEST_PASSWORD, "big-pickle".to_string(), 5, base)
    }

    /// A recorded `/provider` fixture: the `opencode` provider with the eight
    /// zero-cost models from the verified contract, plus the two classification
    /// traps — a paid model whose name says `free`, and a model with no cost
    /// data at all. `other` is an unrelated provider that must be ignored.
    fn provider_fixture() -> Value {
        json!({
            "all": [
                {"id": "other", "models": {"ignored": {"cost": {"input": 0.0}}}},
                {"id": "opencode", "models": {
                    "big-pickle": {"cost": {"input": 0.0}, "tool_call": true, "limit": {"context": 200000}},
                    "ling-3.0-flash-fin-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "longcat-2.5-preview-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "mimo-v2.6-flash-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "muse-spark-1.3-contributor-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "nemotron-3-ultra-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "nemotron-3.5-lightning-free": {"cost": {"input": 0.0}, "tool_call": true},
                    "space-bunny-free": {"cost": {"input": 0.0}, "tool_call": false},
                    "totally-free-model": {"cost": {"input": 2.5}, "tool_call": true},
                    "claude-opus-5": {"cost": {"input": 15.0}, "tool_call": true},
                    "no-cost-data": {"tool_call": true}
                }}
            ],
            "default": {},
            "connected": []
        })
    }

    /// The eight zero-cost ids, sorted as `parse_free_models` returns them.
    const FREE_IDS: [&str; 8] = [
        "big-pickle",
        "ling-3.0-flash-fin-free",
        "longcat-2.5-preview-free",
        "mimo-v2.6-flash-free",
        "muse-spark-1.3-contributor-free",
        "nemotron-3-ultra-free",
        "nemotron-3.5-lightning-free",
        "space-bunny-free",
    ];

    // ---- model list parsing ------------------------------------------------

    #[test]
    fn free_filter_is_cost_based_and_tool_call_populates_support() {
        let models = parse_free_models(&provider_fixture()).expect("fixture parses");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, FREE_IDS, "only the zero-cost models are returned");

        let big_pickle = models.iter().find(|model| model.id == "big-pickle").unwrap();
        assert_eq!(
            big_pickle.supports_tool_calling,
            Some(true),
            "tool_call: true populates supports_tool_calling"
        );
        let space_bunny = models.iter().find(|model| model.id == "space-bunny-free").unwrap();
        assert_eq!(space_bunny.supports_tool_calling, Some(false));

        // A name containing `free` with a non-zero cost is NOT free.
        assert!(
            !ids.contains(&"totally-free-model"),
            "a paid model whose name says free must not be free"
        );
        // A model with no cost data is not free.
        assert!(!ids.contains(&"no-cost-data"), "no cost data means not free");
    }

    /// Runnable demonstration: prints the free-model list discovered from the
    /// recorded `/provider` fixture (no live server required).
    ///
    /// Run with:
    /// `cargo test -p concerto-providers --lib print_discovered_free_models -- --nocapture`
    #[test]
    fn print_discovered_free_models() {
        let models = parse_free_models(&provider_fixture()).expect("fixture parses");
        println!(
            "opencode-local free models from the recorded /provider fixture ({}):",
            models.len()
        );
        for model in &models {
            println!("  {} (tool_call={:?})", model.id, model.supports_tool_calling);
        }
    }

    // ---- HTTP: auth, request shape, response mapping -----------------------

    /// Build a two-response script (session then message) for `spawn_scripted`.
    fn session_then_message(message: Value) -> Vec<(u16, String)> {
        vec![(200, json!({"id": "ses_test"}).to_string()), (200, message.to_string())]
    }

    #[tokio::test]
    async fn basic_auth_header_is_exactly_opencode_password() {
        let (base, requests) = crate::testing::mock_server::spawn_scripted(vec![(
            200,
            json!({"all": [{"id": "opencode", "models": {}}]}).to_string(),
        )]);
        let provider = provider(base);
        provider.list_models(CancellationToken::new()).await.expect("list_models");

        let raw = requests.recv_timeout(Duration::from_secs(5)).expect("one request captured");
        // `request_headers` lowercases the whole block, which corrupts the
        // case-sensitive base64 value; read the raw bytes instead.
        let text = String::from_utf8_lossy(&raw);
        let auth = text
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .expect("an Authorization header is present");
        let value = auth.split_once(':').expect("header has a value").1.trim();
        assert_eq!(
            value,
            format!("Basic {TEST_BASIC}"),
            "Basic auth must be base64(\"opencode:<password>\")"
        );
    }

    #[tokio::test]
    async fn message_body_has_provider_model_and_parts_shape() {
        let message = json!({"info": {"tokens": {"input": 3, "output": 4}}, "parts": [{"type": "text", "text": "hi"}]});
        let (base, requests) =
            crate::testing::mock_server::spawn_scripted(session_then_message(message));
        let provider = provider(base);
        let stream = provider
            .stream_completion(
                request("big-pickle", vec![user_message("hello")]),
                CancellationToken::new(),
            )
            .await
            .expect("stream");
        let _ = stream.try_collect::<Vec<_>>().await.expect("collect");

        // Request 1: session creation.
        let session_raw = requests.recv_timeout(Duration::from_secs(5)).expect("session request");
        let session_headers = crate::testing::mock_server::request_headers(&session_raw);
        assert!(
            session_headers.starts_with("post /session http/1.1"),
            "the first request creates a session: {session_headers}"
        );
        let session_body = crate::testing::mock_server::request_body(session_raw);
        assert!(session_body.get("directory").and_then(Value::as_str).is_some());

        // Request 2: the message.
        let message_raw = requests.recv_timeout(Duration::from_secs(5)).expect("message request");
        let message_headers = crate::testing::mock_server::request_headers(&message_raw);
        assert!(
            message_headers.starts_with("post /session/ses_test/message http/1.1"),
            "the second request posts to the created session: {message_headers}"
        );
        let body = crate::testing::mock_server::request_body(message_raw);
        assert_eq!(body["providerID"], "opencode");
        assert_eq!(body["modelID"], "big-pickle");
        assert_eq!(body["parts"][0]["type"], "text");
        assert_eq!(body["parts"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn response_parts_map_to_chunks_and_usage_is_terminal() {
        let message = json!({
            "info": {"tokens": {"input": 11, "output": 22}},
            "parts": [
                {"type": "step-start"},
                {"type": "reasoning", "text": "thinking"},
                {"type": "text", "text": "answer"},
                {"type": "tool", "callID": "call_1", "tool": "bash", "state": {"status": "completed", "input": {"command": "ls"}}},
                {"type": "step-finish"}
            ]
        });
        let (base, _requests) =
            crate::testing::mock_server::spawn_scripted(session_then_message(message));
        let provider = provider(base);
        let stream = provider
            .stream_completion(
                request("big-pickle", vec![user_message("go")]),
                CancellationToken::new(),
            )
            .await
            .expect("stream");
        let chunks: Vec<CompletionChunk> = stream.try_collect().await.expect("collect");

        let text: String = chunks.iter().map(|chunk| chunk.delta.as_str()).collect();
        assert_eq!(text, "answer");
        assert_eq!(chunks.iter().find_map(|chunk| chunk.reasoning.as_deref()), Some("thinking"));

        let call =
            chunks.iter().find_map(|chunk| chunk.tool_call.as_ref()).expect("tool call chunk");
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments, json!({"command": "ls"}));

        let terminal = chunks.last().expect("terminal chunk");
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage,
            Some(CompletionUsage { prompt_tokens: Some(11), completion_tokens: Some(22) })
        );
        // ADR-48 §4: usage only on the terminal chunk.
        assert_eq!(chunks.iter().filter(|chunk| chunk.usage.is_some()).count(), 1);
    }

    #[tokio::test]
    async fn malformed_tool_arguments_surface_an_error_not_null() {
        let message = json!({
            "parts": [
                {"type": "tool", "callID": "call_bad", "tool": "bash",
                 "state": {"status": "pending", "raw": "this is not json", "input": {}}}
            ]
        });
        let (base, _requests) =
            crate::testing::mock_server::spawn_scripted(session_then_message(message));
        let provider = provider(base);
        let error = provider
            .stream_completion(
                request("big-pickle", vec![user_message("go")]),
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("malformed arguments must fail the request");
        match error {
            ProviderError::InvalidResponse(message) => {
                assert!(message.contains("unparseable tool-call arguments"), "{message}");
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unauthenticated_is_an_auth_failure() {
        let (base, _requests) =
            crate::testing::mock_server::spawn_scripted(vec![(401, "{}".to_string())]);
        let provider = provider(base);
        let error =
            provider.list_models(CancellationToken::new()).await.expect_err("401 must fail");
        assert!(matches!(error, ProviderError::AuthFailure), "got {error:?}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_server_command() {
        // Loopback port 1 is unused: the connect is refused locally and
        // immediately, no packet leaves the machine.
        let provider = OpenCodeLocalProvider::with_api_base(
            TEST_PASSWORD,
            "big-pickle".to_string(),
            5,
            "http://127.0.0.1:1".to_string(),
        );
        let error = provider
            .list_models(CancellationToken::new())
            .await
            .expect_err("unreachable server must fail");
        let message = error.to_string();
        assert!(
            message.contains("opencode serve --port 4096"),
            "the error must name the server command: {message}"
        );
        assert!(message.contains("OPENCODE_SERVER_PASSWORD"), "{message}");
    }

    #[tokio::test]
    async fn list_models_suppresses_native_tools_for_the_transport() {
        // The server advertises tool_call: true, but the transport cannot
        // carry Concerto's schemas, so the connector must report false to
        // engage the ADR-66 §4 fallback driver.
        let (base, _requests) = crate::testing::mock_server::spawn_scripted(vec![(
            200,
            provider_fixture().to_string(),
        )]);
        let provider = provider(base);
        let models = provider.list_models(CancellationToken::new()).await.expect("list_models");
        assert!(!models.is_empty());
        assert!(
            models.iter().all(|model| model.supports_tool_calling == Some(false)),
            "the transport must not advertise native tool support: {models:?}"
        );
    }

    // ---- pure helpers ------------------------------------------------------

    #[test]
    fn lone_user_message_is_sent_verbatim() {
        let rendered = render_conversation(&[user_message("just this")]);
        assert_eq!(rendered, "just this");
    }

    #[test]
    fn system_and_history_are_folded_into_the_text_part() {
        let mut assistant = user_message("prior answer");
        assistant.role = Role::Assistant;
        let messages = vec![
            Message { role: Role::System, content: "be terse".into(), ..user_message("") },
            user_message("first"),
            assistant,
            user_message("second"),
        ];
        let rendered = render_conversation(&messages);
        assert!(rendered.contains("[system]\nbe terse"), "{rendered}");
        assert!(rendered.contains("[user]\nfirst"), "{rendered}");
        assert!(rendered.contains("[assistant]\nprior answer"), "{rendered}");
        assert!(rendered.trim_end().ends_with("[user]\nsecond"), "{rendered}");
    }

    #[test]
    fn usage_absent_is_none_not_zero() {
        assert_eq!(parse_usage(&json!({"info": {"tokens": {}}})), None);
        assert_eq!(parse_usage(&json!({})), None);
        assert_eq!(
            parse_usage(&json!({"tokens": {"input": 0, "output": 0}})),
            Some(CompletionUsage { prompt_tokens: Some(0), completion_tokens: Some(0) })
        );
    }
}
