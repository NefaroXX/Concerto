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
use std::collections::{HashMap, VecDeque};

use crate::adapters::{Dialect, OpenAiChatDialect};
use crate::sse::BufferedSseParser;

/// Re-export of the reasoning-echo policy (ADR-46).
///
/// The canonical enum lives in `crate::adapters` — echo is a dialect concern,
/// and the policy is part of the [`Dialect::render_chat_body`] signature. This
/// re-export keeps the historical `crate::openai::ReasoningEcho` path working
/// for code that names it next to [`OpenAiProvider`] (e.g. `crate::opencode`).
pub use crate::adapters::ReasoningEcho;

/// Re-export of the usage-request policy (ADR-48 §4), mirroring
/// [`ReasoningEcho`]: the enum names an endpoint contract, so it lives beside
/// the dialect (`crate::adapters::openai_compat`) and is re-exported here for
/// callers configuring [`OpenAiProvider`].
pub use crate::adapters::UsageRequest;

pub struct OpenAiProvider {
    api_key: SecretString,
    api_base: String,
    model: String,
    timeout_secs: u64,
    reasoning_echo: ReasoningEcho,
    /// Tool-schema presentation tier (adaptive tool schemas). Resolved per
    /// request against the actual model name; `Auto` (default) keeps every
    /// non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    /// Provider-advertised per-model tool-calling capability (ADR-66 §3
    /// precedence level 2; `ModelInfo::supports_tool_calling`). `None` when
    /// the provider publishes no capability metadata. When set, it beats the
    /// last-resort name heuristic in [`crate::capability::
    /// resolve_tool_schema_mode`].
    advertised_tool_support: Option<bool>,
    /// How this connector asks the endpoint to report usage (ADR-48 §4).
    /// Defaults to [`UsageRequest::Off`] — the wire body stays byte-identical
    /// to the pre-wiring output until a construction site opts an endpoint in.
    usage_request: UsageRequest,
    /// Extra request headers attached to every wire call this connector makes
    /// (model listing, connection test, and `/chat/completions`), in insertion
    /// order. Empty by default, so every other provider's wire output stays
    /// byte-identical. Wrappers set it for relay-specific client-identity
    /// headers — `OpenCodeZenProvider` uses it for `x-opencode-session`
    /// (backend/prompt-cache affinity, see `crate::opencode`).
    extra_headers: Vec<(String, String)>,
    /// Whether this connector is serving the OpenCode free tier.
    ///
    /// `false` for every provider by default, which keeps the wire output
    /// byte-identical. When `true`, a `403 FreeTierError` gets its own honest
    /// [`ProviderError::FreeTierRefused`] state instead of collapsing to a
    /// generic auth failure — see [`crate::credential`]. The anonymous
    /// `Bearer public` credential this once implied was removed (the relay
    /// refuses it server-side). Set only by `OpenCodeZenProvider` under the
    /// `opencode-free-tier` feature.
    free_tier: bool,
    dialect: OpenAiChatDialect,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            api_key: api_key.into(),
            api_base: "https://api.openai.com/v1".to_string(),
            model,
            timeout_secs,
            reasoning_echo: ReasoningEcho::IfPresent,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            advertised_tool_support: None,
            usage_request: UsageRequest::Off,
            extra_headers: Vec::new(),
            free_tier: false,
            dialect: OpenAiChatDialect,
        }
    }

    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base;
        self
    }

    /// Mark this connector as serving the OpenCode free tier, enabling the
    /// dedicated `403 FreeTierError` mapping (see [`crate::credential`]).
    ///
    /// Only `OpenCodeZenProvider` sets this, under the `opencode-free-tier`
    /// feature. The default is `false`, so every other provider — and the
    /// feature-off OpenCode path — is unchanged. Feature-gated so the
    /// feature-off build has no unused method.
    #[cfg(feature = "opencode-free-tier")]
    pub(crate) fn with_free_tier(mut self, free_tier: bool) -> Self {
        self.free_tier = free_tier;
        self
    }

    /// The credential this connector authenticates with, borrowed.
    ///
    /// Callers that also need the key (e.g. `OpenCodeZenProvider`, which owns
    /// this provider as its OpenAI-compatible inner path) read it through
    /// here instead of keeping a second long-lived copy of the secret.
    pub(crate) fn api_key(&self) -> &SecretString {
        &self.api_key
    }

    /// Set the reasoning-content echo policy (ADR-46).
    ///
    /// Defaults to [`ReasoningEcho::IfPresent`]. DeepSeek-backed endpoints such
    /// as OpenCode Zen should set [`ReasoningEcho::Always`] so assistant
    /// messages in a tool-call history never carry reasoning that the API
    /// rejects.
    pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
        self.reasoning_echo = echo;
        self
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Defaults to [`concerto_config::ToolSchemaMode::Auto`]: weak
    /// tool-calling models (name heuristic) get loose schemas (flattened
    /// nested objects, enum descriptions, argument examples) and the
    /// connector re-nests dot-notation arguments on the way back; every
    /// other model keeps the verbatim strict schema and byte-identical wire
    /// output. See `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.tool_schema_mode = mode;
        self
    }

    /// Set the provider-advertised per-model tool-calling capability
    /// (ADR-66 §3 precedence level 2).
    ///
    /// When the provider's listing API publishes `ModelInfo::
    /// supports_tool_calling`, this flag participates in the tool-schema tier
    /// resolution (see [`crate::capability::resolve_tool_schema_mode`]) so an
    /// advertised capability beats the last-resort model-name heuristic. `None`
    /// (the default) means the provider publishes no such metadata.
    pub fn with_advertised_tool_support(mut self, advertised: Option<bool>) -> Self {
        self.advertised_tool_support = advertised;
        self
    }

    /// Set the usage-request policy (ADR-48 §4).
    ///
    /// Defaults to [`UsageRequest::Off`]: no usage-request member is written
    /// and the wire body stays byte-identical to the pre-wiring output. The
    /// connector still captures a `usage` object whenever the endpoint
    /// reports one — a missing report keeps `None`, never an error.
    pub fn with_usage_request(mut self, mode: UsageRequest) -> Self {
        self.usage_request = mode;
        self
    }

    /// The active usage-request policy (ADR-48 §4), exposed for tests and for
    /// connectors that wrap this one (e.g. `crate::openrouter`).
    pub fn usage_request(&self) -> UsageRequest {
        self.usage_request
    }

    /// Attach one extra header to every request this connector builds.
    ///
    /// Crate-private by design: only a wrapping connector needs it —
    /// `OpenCodeZenProvider` sends `x-opencode-session` on the OpenAI-compatible
    /// and model-listing legs through here, so the transport is not duplicated.
    /// [`Self::extra_headers`] starts empty, so no other provider's wire
    /// output changes.
    pub(crate) fn with_extra_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Fold the configured extra headers into a request builder.
    ///
    /// Applied to every wire call (connection test, model listing, chat
    /// completion) so a client-identity header never silently skips a leg.
    fn apply_extra_headers(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        self.extra_headers
            .iter()
            .fold(request, |builder, (name, value)| builder.header(name.as_str(), value.as_str()))
    }

    /// Render the exact wire body for `request`: the dialect's payload with
    /// this connector's usage-request policy applied (ADR-48 §4).
    ///
    /// Called by [`LlmProvider::stream_completion`] *after* any adaptive
    /// tool-schema rewrite, so the streaming flag applied matches the one
    /// actually sent. Pure — no I/O — which keeps the body testable without a
    /// transport.
    fn render_body(&self, request: &CompletionRequest, model: &str) -> serde_json::Value {
        let mut body = self.dialect.render_chat_body(request, model, self.reasoning_echo);
        self.usage_request.apply(&mut body, request.stream);
        body
    }
}

// ---------------------------------------------------------------------------
// Request-body rendering now lives in `crate::adapters::openai_compat`
// (`OpenAiChatDialect`); stream parsing remains here in the connector.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Synthetic tool-call slot used by the content-embedded envelope fallback
/// (proxy Fix 2 STRICT). Real proxy indexes are small wire integers from the
/// `tool_calls` array; this sentinel, combined with the empty-`partial_tools`
/// gate, guarantees the synthesized call can never collide with a real one.
const ENVELOPE_CALL_INDEX: usize = usize::MAX;

struct OpenAiStreamState {
    parser: BufferedSseParser,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    partial_tools: HashMap<usize, PartialToolCall>,
    /// Whether the request that produced this stream was rendered with
    /// loose (weak-model) tool schemas. When set, emitted tool-call
    /// arguments are re-nested from dot-notation back into the tools'
    /// original nested shape before the executor or the tool-call guard
    /// sees them (see `crate::adapters::schema_loose`).
    tool_adapted: bool,
    /// Accumulated `reasoning_content` deltas for the current turn (ADR-46).
    ///
    /// DeepSeek-style endpoints stream reasoning incrementally across many
    /// SSE events; the per-turn buffer is drained into a single final chunk so
    /// the collected assistant message can carry the full reasoning text.
    reasoning_buffer: String,
    /// Provider-reported usage observed before the stream ends (ADR-48 §4).
    ///
    /// OpenAI-compatible endpoints surface `usage` in a trailing chunk (either
    /// alongside `finish_reason` or in a separate chunk with empty `choices`
    /// when `stream_options.include_usage` is set). The first observed usage
    /// is attached to the terminal chunk only.
    usage: Option<CompletionUsage>,
    /// Accumulated `content` deltas for the current turn (proxy Fix 2 STRICT —
    /// content-embedded tool-call envelope).
    ///
    /// Content is buffered instead of streamed eagerly so the fallback can
    /// judge the COMPLETE turn text with a strict full-text parse (no substring
    /// extraction). Non-envelope content is drained as plain text at the
    /// terminal point — identical `delta` output, just aggregated at turn end
    /// instead of per-delta (mirrors how reasoning and tool args already
    /// buffer to the terminal point).
    content_buffer: String,
}

impl OpenAiStreamState {
    fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            pending: VecDeque::new(),
            partial_tools: HashMap::new(),
            tool_adapted: false,
            reasoning_buffer: String::new(),
            usage: None,
            content_buffer: String::new(),
        }
    }

    /// Capture a provider-reported `usage` object (top-level `usage` member of
    /// an SSE event, as OpenAI/DeepSeek/OpenRouter emit it). The first
    /// observation wins; usage is only ever attached to the terminal chunk.
    ///
    /// The payload → [`CompletionUsage`] mapping is the family-shared
    /// [`crate::adapters::openai_compat::map_usage`], so this connector
    /// applies the same fail-soft rules as every other OpenAI-compatible
    /// gateway: no `usage`, no counts, or a non-integer count all keep
    /// `None` — never an error, never a fabricated `0`.
    fn capture_usage(&mut self, parsed: &serde_json::Value) {
        if self.usage.is_some() {
            return;
        }
        self.usage = crate::adapters::openai_compat::map_usage(parsed);
    }

    /// Emit a final chunk carrying the accumulated reasoning (if any) before
    /// the stream's terminal chunk, so `is_final` stays on the last chunk.
    fn emit_reasoning_if_any(&mut self) {
        if !self.reasoning_buffer.is_empty() {
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: Some(std::mem::take(&mut self.reasoning_buffer)),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
        }
    }

    /// Strict, full-text-only parse of the turn's accumulated `content` as a
    /// proxy tool-call envelope (Fix 2 STRICT).
    ///
    /// Accepted — the ENTIRE content must decode to exactly one JSON object of
    /// one of these shapes (full-text parse only, no substring extraction):
    ///
    ///   * `{"name": "<string>", "arguments": { … }}`       — object arguments
    ///   * `{"name": "<string>", "arguments": "<json …>"}`  — string arguments
    ///   * `{"name": "<string>", "input": { … }}`           — `input` alias
    ///     (object only, consistent with the Fix 1 flat path)
    ///   * a canonical wire envelope whose `name`/`arguments` live under
    ///     `function` (row #38: "missing name with inferable intent" — the
    ///     name is looked up at top level first, then under `function`)
    ///   * one bounded double-encoding layer: the content is a JSON *string*
    ///     that itself contains one of the above envelopes (row #38 — proxies
    ///     that re-serialize the envelope)
    ///
    /// An optional string `id` member is carried through when present. Anything
    /// else — trailing/leading prose (the parse must consume the whole
    /// payload), JSON arrays, missing `name`/`function`, wrong member types,
    /// `input` as a non-object — returns `None` and the content stays plain
    /// text with zero behavioral change. No heuristic text mining here: loose
    /// recovery from free text is the tool guard's / driver's job.
    fn parse_content_envelope(content: &str) -> Option<PartialToolCall> {
        let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
        // One bounded double-encoding layer: the strictness invariant is
        // unchanged — the complete content still has to decode (through at
        // most this one extra string layer) to exactly one envelope object.
        let value = match parsed {
            serde_json::Value::String(inner) => serde_json::from_str(&inner).ok()?,
            other => other,
        };
        let object = value.as_object()?;
        let name = object
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                object.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str())
            })?
            .to_string();
        let arguments = match object.get("arguments") {
            Some(serde_json::Value::Object(inner)) => {
                serde_json::Value::Object(inner.clone()).to_string()
            }
            Some(serde_json::Value::String(inner)) => inner.clone(),
            // `arguments` present but not Object|String → reject the envelope.
            Some(_) => return None,
            // `arguments` absent → `input` alias (object only, Fix 1-consistent).
            None => match object.get("input") {
                Some(serde_json::Value::Object(inner)) => {
                    serde_json::Value::Object(inner.clone()).to_string()
                }
                Some(_) => return None,
                // No top-level arguments member at all → the nested canonical
                // `function.arguments` (Object|String, same rules as top level).
                None => match object.get("function")?.get("arguments")? {
                    serde_json::Value::Object(inner) => {
                        serde_json::Value::Object(inner.clone()).to_string()
                    }
                    serde_json::Value::String(inner) => inner.clone(),
                    _ => return None,
                },
            },
        };
        let id = object.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        Some(PartialToolCall { id, name, arguments })
    }

    /// Terminal resolution of the buffered `content` (proxy Fix 2 STRICT).
    ///
    /// Invoked from both terminal paths (`[DONE]` and `finish_reason`) before
    /// tools/reasoning/final are emitted:
    ///
    ///   * real tool deltas observed (`partial_tools` non-empty) → the
    ///     buffered content is drained as plain text; the fallback never
    ///     competes with real deltas (no double-emit);
    ///   * otherwise a strict envelope match synthesizes a [`PartialToolCall`]
    ///     at [`ENVELOPE_CALL_INDEX`], emitted by the EXISTING
    ///     [`Self::emit_tool_call`] pipeline (the shared tool-argument
    ///     integrity parse/repair, plus loose-schema un-flattening);
    ///   * otherwise the content is drained as plain text — same `delta`
    ///     payload as before, aggregated at turn end instead of per-delta.
    ///
    /// OpenRouter/NIM inherit this fallback through their `OpenAiProvider`
    /// inner — no wrapper edits.
    fn flush_content(&mut self) {
        if self.content_buffer.is_empty() {
            return;
        }
        if !self.partial_tools.is_empty() {
            self.emit_buffered_content();
            return;
        }
        let content = std::mem::take(&mut self.content_buffer);
        if let Some(envelope) = Self::parse_content_envelope(&content) {
            self.partial_tools.insert(ENVELOPE_CALL_INDEX, envelope);
        } else {
            self.pending.push_back(Ok(CompletionChunk {
                delta: content,
                reasoning: None,
                tool_call: None,
                is_final: false,
                usage: None,
            }));
        }
    }

    /// Drain the turn's buffered `content` as one plain-text chunk (real tool
    /// deltas won, or the content is not a strict envelope).
    fn emit_buffered_content(&mut self) {
        let content = std::mem::take(&mut self.content_buffer);
        self.pending.push_back(Ok(CompletionChunk {
            delta: content,
            reasoning: None,
            tool_call: None,
            is_final: false,
            usage: None,
        }));
    }

    /// Emit the accumulated arguments for a finished tool call.
    ///
    /// Arguments are parsed through [`crate::tool_args::parse_tool_arguments`]:
    /// valid JSON passes through untouched; the real truncation/proxy-format
    /// corruption classes (unbalanced braces, an unterminated string, a
    /// trailing comma, single-quoted keys, trailing garbage, double-encoding)
    /// are repaired deterministically and model-agnostically. Empty
    /// accumulated arguments become `Null` silently (the tool genuinely
    /// received none). Unrepairable arguments yield an explicit
    /// [`ProviderError::InvalidResponse`] on the stream — a tool call with
    /// silently-empty arguments is never emitted, because the executor would
    /// otherwise run the tool with `{}`.
    ///
    /// Row #38: a slot that never received a name is dropped here with a
    /// payload-free `warn!` instead of emitting an empty-name call the
    /// executor could only reject downstream (explicit rejection, never
    /// silent mangling).
    fn emit_tool_call(&mut self, index: usize) {
        if let Some(ptc) = self.partial_tools.remove(&index) {
            if ptc.name.is_empty() {
                tracing::warn!(
                    index = index,
                    raw_len = ptc.arguments.len(),
                    "emit_tool_call: dropping tool call with no name (unclassifiable proxy shape)."
                );
                return;
            }
            // Empty accumulated arguments are a legitimate argument-less tool
            // call. Any other payload must parse (after repair) or the stream
            // surfaces a typed error — never a silent `Null`/`{}`.
            let parsed = if ptc.arguments.trim().is_empty() {
                Ok(serde_json::Value::Null)
            } else {
                crate::tool_args::parse_tool_arguments(&ptc.arguments).map(
                    |outcome| match outcome {
                        crate::tool_args::ToolArgumentParse::Value(value) => value,
                        crate::tool_args::ToolArgumentParse::Empty => serde_json::Value::Null,
                    },
                )
            };
            let mut args = match parsed {
                Ok(args) => args,
                Err(error) => {
                    tracing::warn!(
                        tool_name = %ptc.name,
                        raw_len = error.raw_len,
                        parse_error = %error,
                        "emit_tool_call: unrepairable tool arguments; failing the stream loudly."
                    );
                    self.pending.push_back(Err(ProviderError::InvalidResponse(format!(
                        "provider returned unparseable tool-call arguments for '{}': {}",
                        ptc.name, error
                    ))));
                    return;
                }
            };
            // Row #38: undo proxy double-encoding — `arguments` that parse to a
            // JSON string containing more JSON (a re-serialized object) unwrap
            // to the inner value instead of reaching the executor as a string
            // (which `ensure_arguments_object` would coerce to `{}`).
            args = crate::tool_args::unwrap_argument_string_layers(args);
            // Adaptive tool schemas: when the request was rendered with loose
            // (weak-model) schemas, the model answers in the flattened
            // dot-notation shape — re-nest before the executor or the
            // tool-call guard validates against the original nested schema.
            if self.tool_adapted {
                crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
            }
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: Some(ToolCall {
                    id: ptc.id,
                    name: ptc.name,
                    arguments: args,
                    ..Default::default()
                }),
                is_final: false,
                usage: None,
            }));
        }
    }

    fn emit_remaining_tools(&mut self) {
        let mut indices: Vec<_> = self.partial_tools.keys().copied().collect();
        indices.sort();
        for idx in indices {
            self.emit_tool_call(idx);
        }
    }

    /// Feed a non-streamed completion body through the stream reducer.
    ///
    /// A `stream: false` response carries the full assistant turn in
    /// `choices[].message` (complete `tool_calls` entries, no `index`)
    /// instead of the streamed `choices[].delta` fragments. Rewriting the
    /// body into the delta shape lets the existing reducer handle both
    /// transports: reasoning capture (ADR-46), argument-string coercion to
    /// objects, loose-schema un-flattening, and `usage` attached to the
    /// terminal chunk all apply unchanged.
    fn handle_non_stream_body(&mut self, mut parsed: serde_json::Value) {
        if let Some(choices) = parsed.get_mut("choices").and_then(serde_json::Value::as_array_mut) {
            for choice in choices.iter_mut() {
                let Some(choice_object) = choice.as_object_mut() else { continue };
                let Some(message) = choice_object.remove("message") else { continue };
                let mut delta = message;
                if let Some(tool_calls) =
                    delta.get_mut("tool_calls").and_then(serde_json::Value::as_array_mut)
                {
                    for (index, call) in tool_calls.iter_mut().enumerate() {
                        if let Some(call_object) = call.as_object_mut() {
                            call_object.insert("index".to_owned(), serde_json::Value::from(index));
                        }
                    }
                }
                choice_object.insert("delta".to_owned(), delta);
            }
        }
        self.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(parsed.to_string()),
            id: None,
            keepalive: false,
        });
        self.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some("[DONE]".to_string()),
            id: None,
            keepalive: false,
        });
    }

    /// Reduce ONE `tool_calls` wire element (or a legacy `function_call`
    /// envelope) into its partial-tool slot.
    ///
    /// Supported shapes — standard OpenAI `function` wrapper, the legacy /
    /// proxy `function_call` wrapper key, and the Fix 1 flat shape
    /// (`{id, name, arguments}` on the tool-call object). Row #38 relaxes the
    /// Fix 1 gate the way real proxies actually fragment calls: a flat `name`
    /// is taken whenever present (it may arrive in a different delta from the
    /// arguments), while flat `arguments` still require a *known* name — this
    /// object's flat `name`, its wrapper's `name`, or one accumulated in an
    /// earlier delta — so an unrelated payload can never yield a nameless
    /// call from nameless fragments. Nothing is ever extracted from `content`
    /// or free text here.
    fn apply_tool_call(&mut self, tc: &serde_json::Value) {
        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let partial = self.partial_tools.entry(index).or_default();
        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
            partial.id = id.to_string();
        }

        // ── Structured path: `function` (canonical) or `function_call`
        // (legacy/proxy alias) wrapper ──
        let mut wrapper_args = false;
        if let Some(func) = tc.get("function").or_else(|| tc.get("function_call")) {
            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    partial.name = name.to_string();
                }
            }
            match func.get("arguments") {
                // String fragments accumulate across deltas.
                Some(serde_json::Value::String(args)) => {
                    partial.arguments.push_str(args);
                    wrapper_args = true;
                }
                // Object arguments are a complete one-shot (non-conforming
                // proxy): replace rather than append, so fragments and a
                // whole object can never concatenate into corrupt JSON.
                Some(serde_json::Value::Object(obj)) => {
                    partial.arguments = serde_json::Value::Object(obj.clone()).to_string();
                    wrapper_args = true;
                }
                // Absent / null / wrong type → nothing to accumulate here.
                Some(_) | None => {}
            }
        }

        // ── Proxy fallback: flat format (name / arguments directly on the
        // tool-call object, no `function` wrapper) ──
        if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
            if !name.is_empty() {
                partial.name = name.to_string();
            }
        }
        // The wrapper already contributed this element's arguments (no double
        // push), or no name is known yet (never emit nameless fragments).
        if wrapper_args || partial.name.is_empty() {
            return;
        }
        match tc.get("arguments") {
            Some(serde_json::Value::String(args)) => partial.arguments.push_str(args),
            Some(serde_json::Value::Object(obj)) => {
                partial.arguments = serde_json::Value::Object(obj.clone()).to_string()
            }
            // `input` alias: object only (Fix 1-consistent); anything else
            // (number, null, string `input`) is ignored, not coerced.
            None => {
                if let Some(obj) = tc.get("input").and_then(|v| v.as_object()) {
                    partial.arguments = serde_json::Value::Object(obj.clone()).to_string();
                }
            }
            Some(_) => {}
        }
    }

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            // Liveness signal (SSE comment line): emit an empty chunk so the
            // stream stays active and the orchestrator idle timeout does not
            // fire during long keep-alive-only periods.
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }
        let data = match event.data {
            Some(d) => d,
            None => return,
        };

        if data == "[DONE]" {
            self.flush_content();
            self.emit_remaining_tools();
            self.emit_reasoning_if_any();
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: self.usage.take(),
            }));
            return;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return,
        };

        // OpenAI-compatible endpoints surface `usage` either alongside the
        // final choice or in a dedicated chunk with empty `choices` (when
        // `stream_options.include_usage` is set). Capture it now so the
        // terminal chunk can carry it (ADR-48 §4).
        self.capture_usage(&parsed);

        let Some(choices) = parsed["choices"].as_array() else {
            return;
        };
        let Some(choice) = choices.first() else {
            return;
        };

        if let Some(delta) = choice["delta"].as_object() {
            // Capture DeepSeek-style `reasoning_content`, which is streamed
            // incrementally across SSE deltas (ADR-46).
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                self.reasoning_buffer.push_str(reasoning);
            }

            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                // Buffer the turn's content instead of streaming it eagerly:
                // the Fix 2 STRICT fallback needs the COMPLETE text to judge
                // whether it is a tool-call envelope (full-text parse only).
                // Non-envelope content is drained as plain text at the
                // terminal point — identical output.
                self.content_buffer.push_str(content);
            }

            if let Some(tc_arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tc_arr {
                    self.apply_tool_call(tc);
                }
            } else if let Some(function_call) = delta.get("function_call") {
                // Legacy single-call transport (pre-`tool_calls` wire): the
                // whole call sits at `delta.function_call` with no index or id.
                // Fold it into the same pipeline as one implicit call at
                // index 0 (row #38 — "nested under an unexpected key").
                let synthetic = serde_json::json!({ "index": 0usize, "function": function_call });
                self.apply_tool_call(&synthetic);
            }
        }

        if let Some(finish_reason) = choice["finish_reason"].as_str() {
            if !finish_reason.is_empty() && finish_reason != "null" {
                self.flush_content();
                self.emit_remaining_tools();
                self.emit_reasoning_if_any();
                self.pending.push_back(Ok(CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: None,
                    is_final: true,
                    usage: self.usage.take(),
                }));
            }
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/models", self.api_base);
        let resp = self
            .apply_extra_headers(client.get(&url).bearer_auth(self.api_key.expose()))
            .send()
            .await
            .map_err(|e| {
                ProviderError::Other(format!(
                    "openai connection failed: {}",
                    describe_error_chain(&e)
                ))
            })?;
        if resp.status().is_success() {
            Ok(())
        } else if resp.status().as_u16() == 401 {
            Err(ProviderError::AuthFailure)
        } else {
            Err(ProviderError::Other(format!("openai returned {}", resp.status())))
        }
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/models", self.api_base);
        let resp = self
            .apply_extra_headers(client.get(&url).bearer_auth(self.api_key.expose()))
            .send()
            .await
            .map_err(|e| {
                ProviderError::Other(format!(
                    "openai list_models failed: {}",
                    describe_error_chain(&e)
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "openai list_models returned {status}: {text}"
            )));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            ProviderError::Other(format!(
                "openai list_models parse failed: {}",
                describe_error_chain(&e)
            ))
        })?;

        let models = json["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_string();
                        let owned_by = v["owned_by"].as_str().map(String::from);
                        Some(ModelInfo {
                            id: id.clone(),
                            name: Some(id),
                            owned_by,
                            supports_tool_calling: None,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(models)
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let span = tracing::info_span!("openai_stream_completion", model = %self.model);
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/chat/completions", self.api_base);

        let model =
            if request.model.is_empty() { self.model.clone() } else { request.model.clone() };

        // Adaptive tool schemas + non-streamed transport (weak-model tier):
        // when the resolved model matches the loose tier, rewrite the
        // request's tool definitions in place before the dialect renders the
        // body AND request the completion non-streamed so tool-call
        // arguments arrive whole. Strict models are untouched — their wire
        // output stays byte-identical (streamed).
        let mut request = request;
        let resolved_mode = crate::capability::resolve_tool_schema_mode(
            self.provider_name(),
            &model,
            self.tool_schema_mode,
            self.advertised_tool_support,
        );
        let tool_adapted =
            crate::adapters::schema_loose::non_streaming_transport_active(resolved_mode, &model);
        if tool_adapted {
            if let Some(tools) = request.tools.as_mut() {
                crate::adapters::schema_loose::adapt_tool_definitions(tools);
                tracing::debug!(
                    model = %model,
                    tools = tools.len(),
                    "weak-model loose tool schemas applied (flattened + examples)"
                );
            }
            request.stream = false;
            tracing::info!(
                model = %model,
                "weak tool-calling model: requesting non-streamed completion (stream=false)"
            );
        }
        let non_streamed = !request.stream;

        let body = self.render_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                self.apply_extra_headers(
                    client
                        .post(&url)
                        .bearer_auth(self.api_key.expose())
                        .header("Content-Type", "application/json")
                        .json(&body),
                )
                .send()
                .await
                .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))
            } => result,
        }?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::retry::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(crate::credential::map_opencode_http_error(
                status,
                &text,
                retry_after,
                self.free_tier,
            ));
        }

        let mut state = OpenAiStreamState::new();
        if tool_adapted {
            state.tool_adapted = true;
        }

        // Non-streamed responses (weak-model tier, or any request rendered
        // with `stream: false`) carry a single JSON completion object instead
        // of an SSE event stream: read the whole body and reduce it through
        // the same stream state so downstream chunk shapes stay identical.
        if non_streamed {
            let body_text = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = response.text() => result.map_err(|e| {
                    ProviderError::Network(format!(
                        "failed to read response body: {}",
                        describe_error_chain(&e)
                    ))
                })?,
            };
            let parsed: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
                ProviderError::Other(format!("invalid JSON in non-streamed completion body: {e}"))
            })?;
            state.handle_non_stream_body(parsed);
            let s = stream! {
                let mut state = state;
                while let Some(item) = state.pending.pop_front() {
                    yield item;
                }
            }
            .boxed();
            return Ok(s);
        }

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
                        // Drain ALL pending items from this chunk, not just one
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
            // Drain any remaining pending items at end of stream
            while let Some(item) = state.pending.pop_front() {
                yield item;
            }
        }
        .boxed();

        Ok(s)
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        crate::budget::budget_for_model(model, 4_000)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 2.50;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 10.00;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(data: &str) -> crate::sse::SseEvent {
        crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        }
    }

    /// Build an SSE `delta.content` event carrying `content` verbatim as a
    /// JSON *string* (escaping handled by serde), so hand-rolled `\"` /
    /// `\\` escapes are never needed in fixtures.
    fn content_event(content: &str) -> crate::sse::SseEvent {
        sse(&serde_json::json!({"choices": [{"delta": {"content": content}}]}).to_string())
    }

    fn drain(state: &mut OpenAiStreamState) -> Vec<CompletionChunk> {
        state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect()
    }

    /// Unwrap a result sequence produced by [`tool_call_turn`] for the
    /// success-path tests (any `Err` fails the test with its message).
    fn ok_chunks(chunks: Vec<Result<CompletionChunk, ProviderError>>) -> Vec<CompletionChunk> {
        chunks.into_iter().map(|result| result.expect("chunk emitted")).collect()
    }

    /// Assert that an event sequence left the content as verbatim plain text
    /// with ONE data chunk, no synthesized tool call, and a terminal chunk.
    fn assert_plain_text(state: &mut OpenAiStreamState, expected: &str) {
        let chunks = drain(state);
        assert!(chunks.iter().all(|c| c.tool_call.is_none()), "no tool call synthesized");
        let text: Vec<&str> =
            chunks.iter().filter(|c| !c.delta.is_empty()).map(|c| c.delta.as_str()).collect();
        assert_eq!(text, vec![expected], "content delivered verbatim as plain text");
        assert!(chunks.last().unwrap().is_final, "terminal chunk present");
    }

    /// ADR-46: a streamed delta carrying `reasoning_content` is captured into a
    /// `CompletionChunk::reasoning`, accumulated across deltas, and emitted
    /// once on stream end.
    #[test]
    fn stream_reasoning_content_is_captured() {
        let mut state = OpenAiStreamState::new();

        // First reasoning delta.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(r#"{"choices":[{"delta":{"reasoning_content":"step one"}}]}"#.to_string()),
            id: None,
            keepalive: false,
        });
        // Second incremental reasoning delta.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(
                r#"{"choices":[{"delta":{"reasoning_content":" and step two"}}]}"#.to_string(),
            ),
            id: None,
            keepalive: false,
        });
        // No reasoning chunks should be emitted while streaming (still buffered).
        assert!(state.pending.is_empty());

        // End the stream: the accumulated reasoning is flushed before the final chunk.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some("[DONE]".to_string()),
            id: None,
            keepalive: false,
        });

        assert_eq!(state.pending.len(), 2, "reasoning chunk + final chunk");
        let reasoning_chunk = state.pending.pop_front().unwrap().unwrap();
        assert_eq!(reasoning_chunk.reasoning.as_deref(), Some("step one and step two"));
        assert!(!reasoning_chunk.is_final, "reasoning chunk is not the terminal chunk");
        assert!(reasoning_chunk.delta.is_empty());

        let final_chunk = state.pending.pop_front().unwrap().unwrap();
        assert!(final_chunk.is_final);
        assert!(final_chunk.reasoning.is_none());
    }

    /// Parser parity: a scripted SSE stream carrying reasoning deltas, tool-call
    /// argument fragments and a trailing `[DONE]` reduces to exactly three
    /// chunks in order — the tool call, the reasoning accumulated into one
    /// chunk, then the final chunk. Guards the wire→canonical reducer against
    /// accidental drift that would break reason/tool round-trips.
    #[test]
    fn stream_parser_accumulates_reasoning_tools_and_done_in_order() {
        let mut state = OpenAiStreamState::new();

        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // Reasoning delta (DeepSeek-style, streamed separately from content).
        state.handle_event(event(
            r#"{"choices":[{"delta":{"reasoning_content":"think step one"}}]}"#,
        ));
        // Tool-call delta fragment: id, name and the argument prefix.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
        ));
        // Tool-call argument fragment (JSON string accumulated across deltas).
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        ));
        // Deltas buffer; nothing emitted mid-stream.
        assert!(state.pending.is_empty(), "deltas buffer; nothing emitted mid-stream");

        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 3, "tool-call chunk + reasoning chunk + final chunk");

        // 0) Tool call with arguments parsed from the JSON argument fragments.
        let tool = chunks[0].tool_call.as_ref().expect("tool-call chunk emitted");
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(!chunks[0].is_final);
        assert!(chunks[0].reasoning.is_none());

        // 1) Reasoning accumulated across deltas into ONE chunk.
        assert_eq!(chunks[1].reasoning.as_deref(), Some("think step one"));
        assert!(chunks[1].delta.is_empty());
        assert!(chunks[1].tool_call.is_none());

        // 2) Final chunk.
        assert!(chunks[2].is_final);
        assert!(chunks[2].reasoning.is_none());
        assert!(chunks[2].tool_call.is_none());
    }

    /// Empty accumulated arguments (proxy Fix 3) become `Null` — they never
    /// take the retry chain and never fire the diagnostic `warn!`.
    #[test]
    fn stream_empty_arguments_emit_null_without_warn() {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(event(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
            ));
            state.handle_event(event("[DONE]"));
        });

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::Value::Null, "empty args emit Null");
        assert_eq!(tool.name, "shell");
        assert_eq!(sink.warns().len(), 0, "empty args must not emit a parse-failure warning");
    }

    /// A raw string delivered as valid JSON (`"ls"`) parses successfully, so
    /// Fix 3's success path returns the parsed value directly (no `{ }`
    /// coercion) and emits no diagnostic.
    #[test]
    fn stream_non_object_string_arguments_pass_through() {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(event(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"\"ls\""}}]}}]}"#,
            ));
            state.handle_event(event("[DONE]"));
        });

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!("ls"), "valid JSON passes through verbatim");
        assert_eq!(sink.warns().len(), 0, "no warning on a parseable payload");
    }

    /// A well-formed object argument parses directly and passes through unchanged.
    #[test]
    fn stream_object_arguments_are_preserved() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Proxy flat format (proxy-tool-call-fix doc failure mode #1): the proxy
    /// emits `{id, name, arguments}` directly on the tool-call object with no
    /// `function` wrapper, and the string arguments arrive in incremental
    /// fragments. The flat string fallback accumulates them exactly like the
    /// `function.arguments` path, and `emit_tool_call` still normalizes to a
    /// JSON object.
    #[test]
    fn stream_flat_tool_call_with_string_arguments() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // First flat fragment: id, name and the argument prefix.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","arguments":"{\"command\":"}]}}]}"#,
        ));
        // Second flat fragment: name re-sent, arguments continue.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"name":"shell","arguments":"\"ls\"}"}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Proxy flat format with the complete `arguments` delivered as a JSON
    /// object (one-shot). The object serializes directly and is parsed back to
    /// the same object by `emit_tool_call` — no chunk accumulation involved.
    #[test]
    fn stream_flat_tool_call_with_object_arguments() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","arguments":{"command":"ls"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Proxy flat format using the `input` alias instead of `arguments` for the
    /// complete JSON object (one-shot). Recognized by the same flat fallback.
    #[test]
    fn stream_flat_tool_call_with_input_alias() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","input":{"command":"ls"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Adaptive tool schemas: when the request was rendered with loose
    /// (weak-model) schemas, dot-notation arguments emitted by the model are
    /// re-nested into the tool's original nested shape before leaving the
    /// connector. Without the flag the arguments pass through untouched, so
    /// strict models keep byte-identical behavior.
    #[test]
    fn stream_tool_adaptation_unflattens_dotted_arguments() {
        // Serialize through `json!` so argument strings are correctly escaped
        // inside the SSE data payload.
        let event = |data: String| crate::sse::SseEvent {
            event: None,
            data: Some(data),
            id: None,
            keepalive: false,
        };
        let start_event = || {
            event(
                serde_json::json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function",
                         "function": {"name": "runner", "arguments": ""}}]}}]
                })
                .to_string(),
            )
        };
        let args_event = |arguments: &str| {
            event(
                serde_json::json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": arguments}}]}}]
                })
                .to_string(),
            )
        };
        let dotted_args = r#"{"config.mode":"fast","config.retries":2}"#;

        // Adapted stream: dotted keys are re-nested.
        let mut adapted = OpenAiStreamState::new();
        adapted.tool_adapted = true;
        adapted.handle_event(start_event());
        adapted.handle_event(args_event(dotted_args));
        adapted.handle_event(event("[DONE]".to_string()));
        let chunks: Vec<CompletionChunk> =
            adapted.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast", "retries": 2}}),
            "dotted arguments must be re-nested on adapted streams"
        );

        // Non-adapted stream: identical wire input passes through unchanged.
        let mut strict = OpenAiStreamState::new();
        strict.handle_event(start_event());
        strict.handle_event(args_event(dotted_args));
        strict.handle_event(event("[DONE]".to_string()));
        let chunks: Vec<CompletionChunk> =
            strict.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config.mode": "fast", "config.retries": 2}),
            "strict streams must pass arguments through untouched"
        );
    }

    /// ADR-48 §4: a trailing `usage` object (OpenAI `stream_options.include_usage`
    /// style, in a chunk with empty `choices`) is captured and attached to the
    /// terminal chunk only.
    #[test]
    fn stream_captures_usage_on_final_chunk() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // Content delta (no usage on intermediate chunks).
        state.handle_event(event(r#"{"choices":[{"delta":{"content":"hello"}}]}"#));

        // Dedicated usage chunk with empty choices (include_usage style).
        state.handle_event(event(
            r#"{"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}}"#,
        ));
        // And again at the real end — the first observation wins.
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 2, "content chunk + final chunk");
        assert_eq!(chunks[0].usage, None, "usage is only reported on the terminal chunk");
        assert!(chunks[1].is_final);
        assert_eq!(
            chunks[1].usage,
            Some(CompletionUsage { prompt_tokens: Some(42), completion_tokens: Some(7) })
        );
    }

    /// ADR-48 §4: the legacy wire shape embeds `usage` alongside the final
    /// choice (`finish_reason`) in the same chunk.
    #[test]
    fn stream_captures_usage_embedded_in_final_choice() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let final_chunk = chunks.iter().find(|chunk| chunk.is_final).expect("final chunk");
        assert_eq!(
            final_chunk.usage,
            Some(CompletionUsage { prompt_tokens: Some(9), completion_tokens: Some(3) })
        );
    }

    /// ADR-48 §4: a `usage` object with no token counts is ignored — it is not
    /// a measurement and must not be surfaced as one.
    #[test]
    fn stream_ignores_usage_without_counts() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };
        state.handle_event(event(r#"{"choices":[],"usage":{}}"#));
        state.handle_event(event("[DONE]"));
        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.last().unwrap().usage, None);
    }

    // -- ADR-48 §4: usage-request wiring -----------------------------------

    /// The constructor must not opt any endpoint in: every construction site
    /// that was not explicitly covered keeps byte-identical wire bodies.
    #[test]
    fn usage_request_defaults_to_off() {
        let provider = OpenAiProvider::new("test-key".to_string(), "test-model".into(), 15);
        assert_eq!(provider.usage_request(), UsageRequest::Off, "default policy must be Off");
    }

    #[test]
    fn with_usage_request_sets_policy() {
        let provider = OpenAiProvider::new("test-key".to_string(), "test-model".into(), 15)
            .with_usage_request(UsageRequest::IncludeStreamUsage);
        assert_eq!(provider.usage_request(), UsageRequest::IncludeStreamUsage);
    }

    /// The rendered wire body carries the policy: a streamed request opts in,
    /// a non-streamed request stays byte-identical, and the default policy
    /// writes nothing at all.
    #[test]
    fn render_body_applies_usage_request_policy() {
        use concerto_core::types::{Message, Role};

        let message = || Message {
            role: Role::User,
            content: "Hello".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let streamed =
            CompletionRequest { messages: vec![message()], stream: true, ..Default::default() };
        let non_streamed =
            CompletionRequest { messages: vec![message()], stream: false, ..Default::default() };

        let opted_in = OpenAiProvider::new("test-key".to_string(), "test-model".into(), 15)
            .with_usage_request(UsageRequest::IncludeStreamUsage);
        let body = opted_in.render_body(&streamed, "test-model");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stream"], true, "the stream flag itself is untouched");

        let body = opted_in.render_body(&non_streamed, "test-model");
        assert!(body.get("stream_options").is_none(), "non-streamed bodies stay untouched");
        assert_eq!(body["stream"], false);

        let default = OpenAiProvider::new("test-key".to_string(), "test-model".into(), 15);
        let body = default.render_body(&streamed, "test-model");
        assert!(body.get("stream_options").is_none(), "the default policy writes nothing");
    }

    /// A non-streamed completion body (`stream: false`, the weak-model
    /// transport) reduces to the same canonical chunk shapes as a streamed
    /// response: content, ONE whole tool-call arguments object, the
    /// accumulated reasoning, and a terminal chunk carrying `usage`.
    #[test]
    fn non_stream_body_reduces_to_whole_tool_call_and_final() {
        let mut state = OpenAiStreamState::new();
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Running the tests.",
                        "reasoning_content": "thinking it through",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"command\": \"cargo test\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 5}
            }"#,
        )
        .expect("fixture parses");

        state.handle_non_stream_body(body);

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();

        let content = chunks.iter().find(|c| !c.delta.is_empty()).expect("content chunk");
        assert_eq!(content.delta, "Running the tests.");

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "shell");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"command": "cargo test"}),
            "arguments must arrive as ONE whole object, not truncated deltas"
        );

        let reasoning = chunks.iter().find_map(|c| c.reasoning.clone()).expect("reasoning chunk");
        assert_eq!(reasoning, "thinking it through");

        let final_chunk = chunks.iter().find(|c| c.is_final).expect("terminal chunk");
        assert_eq!(
            final_chunk.usage,
            Some(CompletionUsage { prompt_tokens: Some(11), completion_tokens: Some(5) })
        );
    }

    /// On the weak-model tier (`tool_adapted`), whole arguments emitted by a
    /// non-streamed completion still get dot-notation keys re-nested before
    /// leaving the connector — same as on the streamed transport.
    #[test]
    fn non_stream_body_unflattens_dotted_arguments_when_adapted() {
        let mut state = OpenAiStreamState::new();
        state.tool_adapted = true;
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "runner",
                                "arguments": "{\"config.mode\": \"fast\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect("fixture parses");

        state.handle_non_stream_body(body);

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast"}}),
            "dotted arguments must be re-nested on the non-streamed transport too"
        );
    }

    /// A non-streamed body without a `message` (degenerate response) must not
    /// panic or emit phantom content: only the `[DONE]` terminator is
    /// produced.
    #[test]
    fn non_stream_body_without_message_yields_final_only() {
        let mut state = OpenAiStreamState::new();
        state.handle_non_stream_body(serde_json::json!({"choices": []}));
        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 1, "only the final chunk");
        assert!(chunks[0].is_final);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Proxy Fix 2 STRICT — content-embedded tool-call envelope
    // ─────────────────────────────────────────────────────────────────────

    /// Accepted shape (a): the proxy embeds a complete tool call as a JSON
    /// object in the content text. Because the ENTIRE content is exactly the
    /// envelope, it becomes a real tool call and the raw text is NOT echoed as
    /// a separate content chunk (no text+call double representation).
    #[test]
    fn content_envelope_object_arguments_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert!(chunks.iter().all(|c| c.delta.is_empty()), "envelope must not be echoed as text");
        assert!(chunks.last().unwrap().is_final);
    }

    /// Accepted shape (b): `arguments` delivered as a JSON *string* flows
    /// through the same `emit_tool_call` parse (Fix 3) as every other
    /// shape.
    #[test]
    fn content_envelope_string_arguments_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":"{\"command\":\"ls\"}"}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must land as a JSON object");
    }

    /// Accepted shape (c): the `input` alias for arguments (object only),
    /// consistent with the Fix 1 flat path.
    #[test]
    fn content_envelope_input_alias_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","input":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
    }

    /// A synthesized envelope still runs through the EXISTING emit pipeline,
    /// so loose-schema un-flattening applies on adapted streams exactly as it
    /// does to real deltas.
    #[test]
    fn content_envelope_unflattens_when_adapted() {
        let mut state = OpenAiStreamState::new();
        state.tool_adapted = true;
        state.handle_event(content_event(
            r#"{"name":"runner","arguments":{"config.mode":"fast","config.retries":2}}"#,
        ));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "runner");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast", "retries": 2}}),
            "dotted arguments must be re-nested on adapted streams"
        );
    }

    /// NEG: prose that merely contains a JSON-ish example must stay plain text
    /// — the full-text parse cannot consume the whole payload (no substring
    /// mining).
    #[test]
    fn content_prose_with_json_example_stays_text() {
        let mut state = OpenAiStreamState::new();
        let prose =
            r#"For example, call {"name": "shell", "arguments": {"command": "ls"}} to list files."#;
        state.handle_event(content_event(prose));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, prose);
    }

    /// NEG: an envelope followed by trailing prose — the full-text parse
    /// fails, so the whole content stays text.
    #[test]
    fn content_envelope_with_trailing_prose_stays_text() {
        let mut state = OpenAiStreamState::new();
        let content = r#"{"name": "shell", "arguments": {"command": "ls"}} and that's it"#;
        state.handle_event(content_event(content));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, content);
    }

    /// NEG: a mixed turn — content text whose entire payload LOOKS like an
    /// envelope PLUS real structured tool deltas. The real deltas win: the
    /// content stays plain text and exactly one tool call (the real one) is
    /// emitted. No envelope synthesis, no double-emit.
    #[test]
    fn content_text_with_real_tool_deltas_prefers_real_deltas() {
        let mut state = OpenAiStreamState::new();
        let envelope_text = r#"{"name":"shell","arguments":{"command":"ls"}}"#;
        state.handle_event(sse(&serde_json::json!({
            "choices": [{"delta": {
                "content": envelope_text,
                "tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                "function": {"name": "runner", "arguments": ""}}]
            }}]
        })
        .to_string()));
        state.handle_event(sse(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": r#"{"suite":"demo"}"#}}]}}]
        })
        .to_string()));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let text: Vec<&str> =
            chunks.iter().filter(|c| !c.delta.is_empty()).map(|c| c.delta.as_str()).collect();
        assert_eq!(text, vec![envelope_text], "content stays plain text with real tool deltas");
        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(
            calls.len(),
            1,
            "exactly one tool call — the real delta, no synthesized envelope"
        );
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "runner");
        assert_eq!(calls[0].arguments, serde_json::json!({"suite": "demo"}));
        assert!(chunks.last().unwrap().is_final);
    }

    /// NEG: a top-level JSON array is not an envelope.
    #[test]
    fn content_array_stays_text() {
        let mut state = OpenAiStreamState::new();
        let content =
            serde_json::json!([{"name": "shell", "arguments": {"command": "ls"}}]).to_string();
        state.handle_event(content_event(&content));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, &content);
    }

    /// NEG: an object without a string `name` is not an envelope.
    #[test]
    fn content_object_missing_name_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"arguments":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"arguments":{"command":"ls"}}"#);
    }

    /// NEG: an object without any arguments/`input` member is not an envelope.
    #[test]
    fn content_object_missing_arguments_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell"}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell"}"#);
    }

    /// NEG: `arguments` must be Object|String — a number is rejected.
    #[test]
    fn content_wrong_argument_type_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":42}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell","arguments":42}"#);
    }

    /// NEG: `input` mirrors Fix 1 — object only, so a string `input` is
    /// rejected.
    #[test]
    fn content_input_alias_string_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","input":"{\"command\":\"ls\"}"}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell","input":"{\"command\":\"ls\"}"}"#);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Proxy Fix 3 — diagnostic logging + retry in `emit_tool_call`
    // ─────────────────────────────────────────────────────────────────────

    /// The Fix 3 diagnostic fields extracted from a captured WARN event:
    /// `tool_name`, `raw_len` (the payload length, never the raw payload) and
    /// `parse_error` (the first strict-parse failure).
    #[derive(Clone, Debug, Default)]
    struct CapturedWarn {
        tool_name: Option<String>,
        raw_len: Option<u64>,
        parse_error: Option<String>,
    }

    #[derive(Default)]
    struct FieldCapture {
        tool_name: Option<String>,
        raw_len: Option<u64>,
        parse_error: Option<String>,
    }

    impl From<FieldCapture> for CapturedWarn {
        fn from(c: FieldCapture) -> Self {
            Self { tool_name: c.tool_name, raw_len: c.raw_len, parse_error: c.parse_error }
        }
    }

    impl tracing::field::Visit for FieldCapture {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "tool_name" => self.tool_name = Some(value.to_string()),
                "parse_error" => self.parse_error = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            if field.name() == "raw_len" {
                self.raw_len = Some(value);
            }
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            if field.name() == "raw_len" {
                self.raw_len = Some(value as u64);
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            // `%field` (Display) values reach `record_debug` Display-wrapped,
            // so their `{:?}` representation is the formatted text.
            let formatted = format!("{value:?}");
            match field.name() {
                "tool_name" => self.tool_name = Some(formatted),
                "parse_error" => self.parse_error = Some(formatted),
                _ => {}
            }
        }
    }

    /// Minimal `tracing::Subscriber` that collects WARN events while a closure
    /// runs under `tracing::subscriber::with_default`, so Fix 3's diagnostic
    /// — and its absence on the empty/valid/recovered paths — can be asserted
    /// without pulling `tracing-subscriber` into dev-dependencies.
    #[derive(Clone, Default)]
    struct WarnSink {
        warns: std::sync::Arc<std::sync::Mutex<Vec<CapturedWarn>>>,
    }

    impl WarnSink {
        fn warns(&self) -> Vec<CapturedWarn> {
            self.warns.lock().expect("warn capture mutex poisoned").clone()
        }
    }

    impl tracing::Subscriber for WarnSink {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            tracing::Id::from_u64(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }

        fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() == &tracing::Level::WARN {
                let mut capture = FieldCapture::default();
                event.record(&mut capture);
                self.warns.lock().expect("warn capture mutex poisoned").push(capture.into());
            }
        }

        fn enter(&self, _span: &tracing::Id) {}

        fn exit(&self, _span: &tracing::Id) {}
    }

    /// Reduce one whole tool-call turn — a single `arguments` fragment for
    /// `index 0` / `call_1` / `shell`, then `[DONE]` — under a [`WarnSink`]
    /// subscriber, returning the emitted chunks (including any `Err`) and the
    /// captured warnings.
    fn tool_call_turn(arguments: &str) -> (Vec<Result<CompletionChunk, ProviderError>>, WarnSink) {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let start = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "shell", "arguments": arguments}}]}}]
        })
        .to_string();
        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(sse(&start));
            state.handle_event(sse("[DONE]"));
        });
        (state.pending.drain(..).collect(), sink)
    }

    /// Fix 3 — malformed arguments that cannot be repaired: no tool call is
    /// emitted with empty arguments (that would silently execute the tool with
    /// `{}`); the stream instead surfaces an explicit `InvalidResponse` error,
    /// and the `tracing::warn!` carries `tool_name`, `raw_len` and
    /// `parse_error`.
    #[test]
    fn stream_malformed_args_fail_loudly_with_warn() {
        let payload = "this is not json";
        let (chunks, sink) = tool_call_turn(payload);

        let error = chunks
            .iter()
            .find_map(|item| item.as_ref().err())
            .expect("unrecoverable args must fail the stream, not emit a tool call");
        match error {
            ProviderError::InvalidResponse(message) => {
                assert!(message.contains("shell"), "error names the tool: {message}");
            }
            other => panic!("expected InvalidResponse, got: {other:?}"),
        }
        assert!(
            chunks.iter().all(|chunk| match chunk {
                Ok(chunk) => chunk.tool_call.is_none(),
                Err(_) => true,
            }),
            "no tool call may be emitted for unrepairable arguments"
        );

        let warns = sink.warns();
        assert_eq!(warns.len(), 1, "exactly one diagnostic warning");
        assert_eq!(warns[0].tool_name.as_deref(), Some("shell"));
        assert_eq!(warns[0].raw_len, Some(payload.len() as u64), "raw_len, not raw payload");
        assert!(warns[0].parse_error.is_some(), "first strict-parse error attached");
    }

    /// A truncated streamed fragment (unbalance + unterminated string) is
    /// repaired deterministically and yields the intended argument object —
    /// no error, no warning.
    #[test]
    fn stream_truncated_args_are_repaired() {
        let (chunks, sink) = tool_call_turn(r#"{"command": "cargo test"#);
        let chunks = ok_chunks(chunks);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "cargo test"}));
        assert_eq!(sink.warns().len(), 0, "repair succeeds without a warning");
    }

    /// Fix 3 — single-quote fixup recovery: the strict parse fails on
    /// single-quoted JSON, the deterministic repair rescues it into a real
    /// object, and no warning fires.
    #[test]
    fn stream_single_quote_args_are_recovered() {
        let (chunks, sink) = tool_call_turn("{'command': 'ls'}");
        let chunks = ok_chunks(chunks);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "recovery succeeds silently");
    }

    /// Fix 3 — double-wrap/trim recovery: the payload carries surrounding
    /// whitespace (exercising the trim re-parse) AND single quotes (the fixup
    /// re-parse); the chained retries on the trimmed text recover the object.
    #[test]
    fn stream_double_wrapped_trim_args_are_recovered() {
        let (chunks, sink) = tool_call_turn("  {'command': 'ls'}  ");
        let chunks = ok_chunks(chunks);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "recovery succeeds silently");
    }

    /// NEG — a valid JSON payload passes through byte-for-byte with no warning:
    /// Fix 3 must not perturb the success path.
    #[test]
    fn stream_valid_json_arguments_unchanged() {
        let (chunks, sink) = tool_call_turn(r#"{"command":"ls"}"#);
        let chunks = ok_chunks(chunks);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "no warning on a valid payload");
    }

    /// NEG — legitimate apostrophes inside double-quoted string values are
    /// valid JSON, so the strict parse succeeds and the single-quote repair
    /// never runs (the apostrophe-corruption risk identified in the spec is
    /// mitigated by construction). The payload is preserved verbatim.
    #[test]
    fn stream_string_values_with_apostrophes_are_preserved() {
        let (chunks, sink) = tool_call_turn(r#"{"message": "it's fine"}"#);
        let chunks = ok_chunks(chunks);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"message": "it's fine"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "valid JSON with apostrophes is untouched");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Row #38 — sanitized proxy fixture corpus.
    //
    // Every payload here is SYNTHESIZED from a shape class seen across
    // OpenAI-compatible gateways (flat fragmentation, legacy keys,
    // double-encoding, content envelopes, weak-model flat bodies). No API
    // keys, no PII, no verbatim provider responses — shapes only.
    // ─────────────────────────────────────────────────────────────────────

    /// Expected reduction outcome for one corpus fixture.
    enum CorpusExpect {
        /// Exactly one tool call with this name/arguments; nothing echoed as
        /// text (envelope/content shapes must not double-represent).
        Tool { name: &'static str, arguments: serde_json::Value },
        /// No tool call — the content stays verbatim plain text.
        Text,
        /// Explicitly rejected: no tool call AND no text (the nameless drop
        /// fires exactly one payload-free diagnostic).
        Dropped,
    }

    /// One sanitized proxy tool-call shape plus its expected outcome.
    struct CorpusFixture {
        label: &'static str,
        /// Ordered `choices[0].delta` payloads (a `[DONE]` is appended), or
        /// one whole `stream: false` body when `whole_body` is set.
        events: Vec<serde_json::Value>,
        whole_body: bool,
        tool_adapted: bool,
        expect: CorpusExpect,
    }

    impl CorpusFixture {
        fn stream(
            label: &'static str,
            events: Vec<serde_json::Value>,
            expect: CorpusExpect,
        ) -> Self {
            Self { label, events, whole_body: false, tool_adapted: false, expect }
        }

        /// The weak-model (Mimo-class) transport: one whole body, loose
        /// dot-notation arguments re-nested on the way out.
        fn weak_whole_body(
            label: &'static str,
            events: Vec<serde_json::Value>,
            expect: CorpusExpect,
        ) -> Self {
            Self { label, events, whole_body: true, tool_adapted: true, expect }
        }

        fn tool(name: &'static str, arguments: serde_json::Value) -> CorpusExpect {
            CorpusExpect::Tool { name, arguments }
        }
    }

    /// A `delta` event carrying a tool-call array.
    fn tc_delta(tool_calls: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"choices": [{"delta": {"tool_calls": tool_calls}}]})
    }

    /// A `delta` event carrying `content` verbatim.
    fn content_delta(content: &str) -> serde_json::Value {
        serde_json::json!({"choices": [{"delta": {"content": content}}]})
    }

    /// The sanitized corpus: each shape parses to the correct `ToolCall` or
    /// is explicitly rejected — never silently mangled.
    fn proxy_corpus() -> Vec<CorpusFixture> {
        let shell_args = "{\"command\":\"ls\"}"; // {"command":"ls"}
        let args_head = "{\"command\":"; // {"command":
        let args_tail = "\"ls\"}"; // "ls"}
        let envelope = r#"{"name":"shell","arguments":{"command":"ls"}}"#;
        // Re-serializations of the payload above (stringified-JSON
        // double-encoding): `"{\"name\":…}"` as content, `"{\"command\":…}"`
        // as an arguments value.
        let double_envelope = format!("\"{}\"", envelope.replace('"', "\\\""));
        let double_args = format!("\"{}\"", shell_args.replace('"', "\\\""));

        vec![
            // -- Accept: Fix 1 relaxed — flat name and flat arguments split
            //    across DIFFERENT deltas (previously: nameless drop) ---------
            CorpusFixture::stream(
                "flat-name-then-arguments-split-across-deltas",
                vec![
                    tc_delta(serde_json::json!([{"index": 0, "id": "call_f", "name": "shell"}])),
                    tc_delta(serde_json::json!([{"index": 0, "arguments": args_head}])),
                    tc_delta(serde_json::json!([{"index": 0, "arguments": args_tail}])),
                ],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: mixed nesting — `function.name` with a flat
            //    top-level `arguments` (previously: name kept, args lost) ----
            CorpusFixture::stream(
                "wrapper-name-with-flat-arguments",
                vec![tc_delta(serde_json::json!([{"index": 0, "id": "call_m",
                    "function": {"name": "shell"}, "arguments": shell_args}]))],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: the WHOLE call nested under the unexpected legacy
            //    `delta.function_call` key (no index, no tool_calls array) ---
            CorpusFixture::stream(
                "legacy-delta-function-call-key",
                vec![serde_json::json!({"choices": [{"delta": {"function_call":
                    {"name": "shell", "arguments": shell_args}}}]})],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: `function.arguments` delivered as an object instead
            //    of an incremental string (non-conforming proxy) -------------
            CorpusFixture::stream(
                "wrapper-object-arguments-one-shot",
                vec![tc_delta(serde_json::json!([{"index": 0, "id": "call_w2",
                    "function": {"name": "shell", "arguments": {"command": "ls"}}}]))],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: stringified-JSON double-encoded arguments — the
            //    arguments string parses to ANOTHER JSON string that holds the
            //    object; unwrap instead of shipping a string downstream ------
            CorpusFixture::stream(
                "double-encoded-arguments-string",
                vec![tc_delta(serde_json::json!([{"index": 0, "id": "call_d",
                    "function": {"name": "shell", "arguments": double_args}}]))],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: envelope fragmented across content deltas — the
            //    strict whole-turn parse judges the REASSEMBLED text --------
            CorpusFixture::stream(
                "content-envelope-split-across-deltas",
                vec![
                    content_delta("{\"name\":\"shell\",\"argu"),
                    content_delta("ments\":{\"command\":\"ls\"}}"),
                ],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: double-encoded content envelope (one string layer) --
            CorpusFixture::stream(
                "content-envelope-double-encoded",
                vec![content_delta(&double_envelope)],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: no top-level `name` — inferable from the nested
            //    canonical `function` wrapper (whole-turn strictness holds) --
            CorpusFixture::stream(
                "content-envelope-nested-function-name",
                vec![content_delta(
                    r#"{"id":"call_w","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}"#,
                )],
                CorpusFixture::tool("shell", serde_json::json!({"command": "ls"})),
            ),
            // -- Accept: Mimo-class weak-model path — flat, id-less, whole
            //    non-streamed body with loose dot-notation arguments ---------
            CorpusFixture::weak_whole_body(
                "mimo-class-flat-whole-body-loose-arguments",
                vec![serde_json::json!({"choices": [{"message": {"role": "assistant",
                    "tool_calls": [{"name": "runner",
                        "arguments": {"config.mode": "fast", "config.retries": 2}}]},
                    "finish_reason": "tool_calls"}]})],
                CorpusFixture::tool(
                    "runner",
                    serde_json::json!({"config": {"mode": "fast", "retries": 2}}),
                ),
            ),
            // -- Reject (stay text): prose around / instead of the envelope --
            CorpusFixture::stream(
                "content-leading-prose-then-envelope",
                vec![content_delta(&format!("Sure: {envelope}"))],
                CorpusExpect::Text,
            ),
            // -- Reject (drop): a tool-call element that never carries a name
            //    anywhere — explicit diagnostic, no nameless ToolCall ships ---
            CorpusFixture::stream(
                "tool-call-element-without-name",
                vec![tc_delta(serde_json::json!([{"index": 0, "arguments": shell_args}]))],
                CorpusExpect::Dropped,
            ),
        ]
    }

    /// Run one fixture through the stream reducer (whole-body fixtures go
    /// through the non-streamed transport) under a warn-capturing subscriber.
    fn run_corpus_fixture(f: &CorpusFixture) -> (Vec<CompletionChunk>, WarnSink) {
        let mut state = OpenAiStreamState::new();
        state.tool_adapted = f.tool_adapted;
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        tracing::subscriber::with_default(subscriber, || {
            if f.whole_body {
                state.handle_non_stream_body(f.events[0].clone());
            } else {
                for event in &f.events {
                    state.handle_event(sse(&event.to_string()));
                }
                state.handle_event(sse("[DONE]"));
            }
        });
        (drain(&mut state), sink)
    }

    /// Fixture-driven corpus: every shape either parses to the correct
    /// `ToolCall` (no text echo, no diagnostics) or is explicitly rejected
    /// (text kept, or nameless drop with exactly one diagnostic).
    #[test]
    fn proxy_fixture_corpus_parses_or_rejects() {
        for f in proxy_corpus() {
            let (chunks, sink) = run_corpus_fixture(&f);
            assert!(chunks.last().map(|c| c.is_final).unwrap_or(false), "{}", f.label);
            let calls: Vec<&ToolCall> =
                chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
            let text: String = chunks.iter().map(|c| c.delta.as_str()).collect();
            match &f.expect {
                CorpusExpect::Tool { name, arguments } => {
                    assert_eq!(calls.len(), 1, "{}: exactly one tool call", f.label);
                    assert_eq!(calls[0].name, *name, "{}: tool name", f.label);
                    assert_eq!(&calls[0].arguments, arguments, "{}: arguments", f.label);
                    assert!(text.is_empty(), "{}: must not be echoed as text", f.label);
                    assert_eq!(sink.warns().len(), 0, "{}: clean parse is silent", f.label);
                }
                CorpusExpect::Text => {
                    assert!(calls.is_empty(), "{}: must stay plain text", f.label);
                    let expected: String = f
                        .events
                        .iter()
                        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
                        .collect();
                    assert_eq!(text, expected, "{}: content verbatim", f.label);
                    assert_eq!(sink.warns().len(), 0, "{}: text path is silent", f.label);
                }
                CorpusExpect::Dropped => {
                    assert!(calls.is_empty(), "{}: nameless call must be dropped", f.label);
                    assert!(text.is_empty(), "{}: drop does not echo text", f.label);
                    assert_eq!(sink.warns().len(), 1, "{}: one explicit diagnostic", f.label);
                }
            }
        }
    }

    /// Fix 2 STRICTNESS REGRESSION (row #38): extending the accepted envelope
    /// alias set (nested `function`, one double-encoding layer) must NOT
    /// loosen the whole-turn semantics — anything that is not exactly one
    /// envelope object still rejects, and the canonical shapes still accept.
    #[test]
    fn fix2_strict_envelope_semantics_hold() {
        let rejects: &[&str] = &[
            r#"{"name":"shell","arguments":{"command":"ls"}} and that's it"#, // trailing prose
            "Sure: {\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}",  // leading prose
            r#"[{"name":"shell","arguments":{"command":"ls"}}]"#,             // array
            r#"{"arguments":{"command":"ls"}}"#,                              // no name/function
            r#"{"name":"shell"}"#,                                            // no arguments
            r#"{"name":"shell","arguments":42}"#,                             // wrong type
            r#"{"name":"shell","arguments":null}"#,                           // null arguments
            r#"{"name":"shell","input":"{\"command\":\"ls\"}"}"#,             // string input
            r#"{"function":{"name":"shell"}}"#,                               // nested, no args
            r#""{\"name\":\"shell\"} trailing""#,                             // bad double layer
            "{}",
            "",
        ];
        for content in rejects {
            assert!(
                OpenAiStreamState::parse_content_envelope(content).is_none(),
                "must reject: {content}"
            );
        }

        let accepts: &[(&str, &str)] = &[
            (r#"{"name":"shell","arguments":{"command":"ls"}}"#, "shell"),
            (r#"{"name":"shell","arguments":"{\"command\":\"ls\"}"}"#, "shell"),
            (r#"{"name":"shell","input":{"command":"ls"}}"#, "shell"),
            (r#"{"function":{"name":"shell","arguments":{"command":"ls"}}}"#, "shell"),
            (
                r#"{"id":"c","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}"#,
                "shell",
            ),
        ];
        for (content, name) in accepts {
            let parsed = OpenAiStreamState::parse_content_envelope(content)
                .unwrap_or_else(|| panic!("must accept: {content}"));
            assert_eq!(parsed.name, *name, "envelope: {content}");
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Row #38 — pairwise verification against a REAL OpenAI-compatible
    // proxy. Ignored by default AND env-gated: CI never sees a network.
    // ─────────────────────────────────────────────────────────────────────

    /// Endpoint configuration for the live harness. Present only when both
    /// `CONCERTO_LIVE_PROXY` (base URL) and `CONCERTO_LIVE_PROXY_KEY` are set;
    /// `CONCERTO_LIVE_PROXY_MODEL` defaults to a cheap tool-calling model.
    struct LiveProxy {
        base: String,
        key: SecretString,
        model: String,
    }

    impl LiveProxy {
        fn from_env() -> Option<Self> {
            let base = std::env::var("CONCERTO_LIVE_PROXY").ok()?;
            let key = std::env::var("CONCERTO_LIVE_PROXY_KEY").ok()?;
            if base.trim().is_empty() || key.trim().is_empty() {
                return None;
            }
            let model = std::env::var("CONCERTO_LIVE_PROXY_MODEL")
                .unwrap_or_else(|_| "gpt-4o-mini".to_string());
            Some(Self { base: base.trim_end_matches('/').to_string(), key: key.into(), model })
        }

        /// A forced single-tool request: `tool_choice` pins the outcome so the
        /// assertion is deterministic across endpoints.
        fn request_body(&self, stream: bool) -> serde_json::Value {
            serde_json::json!({
                "model": self.model,
                "stream": stream,
                "messages": [{"role": "user", "content": "Run `pwd` via the shell tool."}],
                "tools": [{"type": "function", "function": {
                    "name": "shell",
                    "description": "Run a shell command.",
                    "parameters": {"type": "object",
                                   "properties": {"command": {"type": "string"}},
                                   "required": ["command"]}
                }}],
                "tool_choice": {"type": "function", "function": {"name": "shell"}},
            })
        }
    }

    /// Wire-shape classes the corpus covers; anything else means the proxy
    /// emitted a shape we have NO fixture for (add one, then re-run).
    const LIVE_KNOWN_CLASSES: &[&str] =
        &["function-wrapped", "flat", "legacy-function_call", "content-embedded"];

    /// Classify tool-bearing wire shapes in one decoded payload (SSE event or
    /// whole body) so the harness fails loudly on an uncovered shape class.
    fn classify_wire_calls(
        container: &serde_json::Value,
        classes: &mut std::collections::BTreeSet<&'static str>,
    ) {
        let Some(calls) = container.get("tool_calls").and_then(|v| v.as_array()) else {
            return;
        };
        for tc in calls {
            if tc.get("function").is_some() || tc.get("function_call").is_some() {
                classes.insert("function-wrapped");
            } else if tc.get("name").is_some() {
                classes.insert("flat");
            } else {
                classes.insert("UNCLASSIFIED");
            }
        }
    }

    fn classify_wire_shape(
        payload: &serde_json::Value,
        classes: &mut std::collections::BTreeSet<&'static str>,
    ) {
        let Some(choice) =
            payload.get("choices").and_then(|v| v.as_array()).and_then(|v| v.first())
        else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            classify_wire_calls(delta, classes);
            if delta.get("function_call").is_some() {
                classes.insert("legacy-function_call");
            }
        }
        if let Some(message) = choice.get("message") {
            classify_wire_calls(message, classes);
        }
    }

    /// Assert the pairwise outcome: exactly one correctly parsed tool call,
    /// and every observed wire-shape class is covered by the corpus.
    fn assert_live_outcome(
        chunks: &[CompletionChunk],
        mut classes: std::collections::BTreeSet<&'static str>,
        leg: &str,
    ) {
        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(
            calls.len(),
            1,
            "{leg}: expected exactly one parsed tool call, got {} (classes: {classes:?})",
            calls.len()
        );
        assert_eq!(calls[0].name, "shell", "{leg}: tool name survives parsing");
        assert!(
            calls[0].arguments.is_object(),
            "{leg}: arguments must land as an object: {}",
            calls[0].arguments
        );
        if classes.is_empty() && !calls.is_empty() {
            // No structured tool-call wire shape was observed, yet a call
            // parsed — the proxy embedded it in `content`.
            classes.insert("content-embedded");
        }
        for class in &classes {
            assert!(
                LIVE_KNOWN_CLASSES.contains(class),
                "{leg}: proxy emitted tool-call shape `{class}` with no corpus fixture — \
                 add a sanitized fixture to `proxy_corpus()`"
            );
        }
    }

    /// POST one chat-completion request to the live endpoint; a non-2xx
    /// status panics WITH the response body (test-only diagnostics), and a
    /// cancelled token yields `None` so the legs exit early.
    async fn live_post(
        proxy: &LiveProxy,
        stream: bool,
        cancel: &CancellationToken,
    ) -> Option<reqwest::Response> {
        let client = crate::new_client(60);
        tokio::select! {
            _ = cancel.cancelled() => None,
            result = client
                .post(format!("{}/chat/completions", proxy.base))
                .bearer_auth(proxy.key.expose())
                .json(&proxy.request_body(stream))
                .send() => {
                let response = result.expect("live proxy request sent");
                let status = response.status();
                assert!(
                    status.is_success(),
                    "live proxy HTTP {status}: {}",
                    response.text().await.unwrap_or_default()
                );
                Some(response)
            }
        }
    }

    /// Streamed leg: replay raw SSE bytes through the same parser + reducer
    /// the connector uses, classifying the wire shape as it arrives.
    async fn live_streamed_leg(
        proxy: &LiveProxy,
        cancel: CancellationToken,
    ) -> (Vec<CompletionChunk>, std::collections::BTreeSet<&'static str>) {
        let Some(response) = live_post(proxy, true, &cancel).await else {
            return (Vec::new(), std::collections::BTreeSet::new());
        };

        let mut state = OpenAiStreamState::new();
        let mut parser = BufferedSseParser::new();
        let mut classes = std::collections::BTreeSet::new();
        let mut byte_stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => break,
                next = byte_stream.next() => next,
            };
            let Some(next) = next else { break };
            let bytes = next.expect("live proxy stream readable");
            for event in parser.push_bytes(&bytes) {
                if let Some(data) = event.data.as_deref() {
                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(data) {
                        classify_wire_shape(&payload, &mut classes);
                    }
                }
                state.handle_event(event);
            }
        }
        (drain(&mut state), classes)
    }

    /// Non-streamed leg: the `stream: false` body reduces through
    /// `handle_non_stream_body` — the weak-model transport.
    async fn live_non_streamed_leg(
        proxy: &LiveProxy,
        cancel: CancellationToken,
    ) -> (Vec<CompletionChunk>, std::collections::BTreeSet<&'static str>) {
        let Some(response) = live_post(proxy, false, &cancel).await else {
            return (Vec::new(), std::collections::BTreeSet::new());
        };
        let body_text = tokio::select! {
            _ = cancel.cancelled() => {
                return (Vec::new(), std::collections::BTreeSet::new());
            }
            text = response.text() => text.expect("live proxy body readable"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&body_text).expect("live proxy body is JSON");
        let mut classes = std::collections::BTreeSet::new();
        classify_wire_shape(&parsed, &mut classes);
        let mut state = OpenAiStreamState::new();
        state.handle_non_stream_body(parsed);
        (drain(&mut state), classes)
    }

    /// Pairwise verification (row #38): replay a forced tool call against a
    /// real OpenAI-compatible proxy in BOTH transports and assert the parse
    /// outcome plus corpus coverage of the observed wire-shape class.
    ///
    /// Never runs in CI: `#[ignore]` plus the env gate below (a `--ignored`
    /// run without env skips instead of failing).
    ///
    /// ```text
    /// CONCERTO_LIVE_PROXY=https://host/v1     # endpoint base URL
    /// CONCERTO_LIVE_PROXY_KEY=...             # per-session key, never committed
    /// CONCERTO_LIVE_PROXY_MODEL=...           # optional (default gpt-4o-mini)
    /// cargo test -p concerto-providers live_proxy_pairwise -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "live network: set CONCERTO_LIVE_PROXY + CONCERTO_LIVE_PROXY_KEY, run with --ignored"]
    async fn live_proxy_pairwise_tool_call_parsing() {
        let Some(proxy) = LiveProxy::from_env() else {
            eprintln!("skipped: CONCERTO_LIVE_PROXY / CONCERTO_LIVE_PROXY_KEY not set");
            return;
        };
        let cancel = CancellationToken::new();

        let (streamed, stream_classes) = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            live_streamed_leg(&proxy, cancel.clone()),
        )
        .await
        .expect("streamed leg timed out");
        assert_live_outcome(&streamed, stream_classes, "streamed");

        let (whole, body_classes) = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            live_non_streamed_leg(&proxy, cancel),
        )
        .await
        .expect("non-streamed leg timed out");
        assert_live_outcome(&whole, body_classes, "non-streamed");
    }
}
