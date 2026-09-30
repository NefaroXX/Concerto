//! OpenCode provider (Zen + Go relays).
//!
//! OpenCode operates **two distinct relays** behind one connector:
//!
//! - **Zen** — [`OPENCODE_ZEN_BASE`], the `opencode` provider's default. Serves
//!   the paid/free-tier Zen roster.
//! - **Go** — [`OPENCODE_GO_BASE`], the `opencode-free` provider's default. A
//!   *separate* endpoint with a *separate* model catalog (see
//!   [`crate::provider_defs::OPENCODE_GO_KNOWN`]): never assume a model id
//!   exists on both relays, and never copy a catalog from one to the other.
//!
//! By default neither relay serves completions to an unauthenticated client —
//! Go answers `401 AuthError "Missing API key."`, Zen's free-tier gate answers
//! `403 FreeTierError` for models that require an OpenCode-signed session — so
//! neither provider type is keyless. A config-supplied `api_base` overrides
//! either default (self-hosted gateways, proxies, tests).
//!
//! # `opencode-free-tier` feature
//!
//! With the non-default `opencode-free-tier` feature enabled, the
//! `opencode-free` type becomes Concerto's port of OpenCode's own
//! unauthenticated `opencode` provider: it targets the **Zen** relay, ships
//! the cost-aware Zen catalog (free-ness is `cost.input == 0`, never a name
//! suffix), and a keyless request carries the literal `public` credential that
//! the server maps back to the anonymous IP-rate-limited path. When a real key
//! is present it is used instead and the full catalog is available. With the
//! feature OFF every path here is byte-identical to the shipped Go-relay
//! behaviour. See `crate::credential` and `crate::provider_defs`.
//!
//! Each relay dispatches on the **lowercased full model-id prefix** — the
//! authoritative contract is the upstream consumer's
//! `_OPENCODE_API_MODE_PREFIXES` table (`NousResearch/hermes-agent`,
//! `hermes_cli/models.py`), which this module mirrors exactly. The first
//! matching prefix wins; the default is the OpenAI-compatible Chat Completions
//! path:
//!
//! - **OpenAI-compatible** (`big-pickle`, DeepSeek, Kimi, GLM, …): routed via [`OpenAiProvider`] to `POST {base}/chat/completions`.
//! - **Anthropic Messages**: `POST {base}/messages` with `x-api-key` + `anthropic-version`; Zen prefixes `claude-*`, `union-alpha`, `qwen*`; Go prefixes `minimax-*`, `qwen*`, `union-alpha`.
//! - **OpenAI Responses**: `POST {base}/responses`; both relays use `gpt-*`, `grok-*`, `muse-spark*`.
//!
//! The dialect is chosen per **(relay, model id)** — see [`api_mode_for`]. The
//! two relays have genuinely different tables: `claude-*` is Anthropic on Zen
//! but falls through to Chat Completions on Go, and `minimax-*` is Anthropic
//! on Go but Chat Completions on Zen. Treating one relay's rules as universal
//! is the bug this table fixes. The table is deliberately **prefix-based**
//! because that is the upstream relay contract (`str.startswith`) — do not
//! "tidy" it into token matching or add/remove hyphens.
//!
//! The governing principle remains **behavior over taxonomy** (ADR-66 §5
//! correction, 2026-09-08): the wire dialect follows what the endpoint *does*,
//! not which family a model name resembles. Genuine `muse-v*` family members
//! keep their token-bounded Responses rule (the upstream table does not list
//! them, but Zen serves them only via `/responses`); `muse-spark*` is covered
//! by the prefix table. Whole-token matching is preserved for the Muse family
//! so a name merely *containing* `muse` (`some-muse-model`, `amuse-v2`) never
//! routes to Responses.

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

/// Default OpenCode **Zen** relay base URL — the `opencode` provider's target.
///
/// This is the ONE definition of the Zen base in this workspace (the previous
/// duplicate in `lib.rs` was removed so the two can never diverge).
pub(crate) const OPENCODE_ZEN_BASE: &str = "https://opencode.ai/zen/v1";

/// Default OpenCode **Go** relay base URL — the `opencode-free` provider's
/// target.
///
/// Go (`/zen/go`) is a **distinct relay with a distinct model catalog**, not a
/// path alias of Zen (`/zen`): `GET https://opencode.ai/zen/go/v1/models`
/// returns a roster that shares no assumptions with Zen's. Completions on Go
/// answer `401 AuthError` without a valid API key, so `opencode-free` is *not*
/// a keyless provider type. Kept as a constant so a future relay correction is
/// a one-line change, never a scattered edit.
pub(crate) const OPENCODE_GO_BASE: &str = "https://opencode.ai/zen/go/v1";

/// Which OpenCode relay a provider targets.
///
/// Both relays share one connector ([`OpenCodeZenProvider`]) but have
/// **different per-model wire tables**, so the relay must participate in the
/// dialect decision (see [`api_mode_for`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCodeRelay {
    /// `https://opencode.ai/zen/v1` — the `opencode` provider's default.
    Zen,
    /// `https://opencode.ai/zen/go/v1` — the `opencode-free` provider's default.
    Go,
}

impl OpenCodeRelay {
    /// The ONE provider-type → relay mapping, defined next to [`Self::for_base`]
    /// and the [`OPENCODE_ZEN_BASE`] / [`OPENCODE_GO_BASE`] constants.
    ///
    /// A caller-supplied `api_base` is authoritative and detected from its URL
    /// ([`Self::for_base`]); otherwise [`Self::default_for`] picks the relay.
    /// Callers that already resolved a base through
    /// [`OpenCodeZenProvider::resolve_api_base`] use [`Self::for_base`]
    /// directly — the two agree on every base that function can produce.
    pub(crate) fn resolve(provider_type: &str, api_base: Option<&str>) -> Self {
        match api_base {
            Some(base) => Self::for_base(base),
            None => Self::default_for(provider_type),
        }
    }

    /// The relay a provider type targets when no `api_base` overrides it.
    ///
    /// With the `opencode-free-tier` feature OFF, `opencode-free` targets the
    /// Go relay exactly as shipped. With the feature ON, it is the
    /// unauthenticated free-tier port of OpenCode's own `opencode` provider,
    /// which speaks to the Zen relay whose catalog carries the `cost` metadata
    /// free-ness is derived from. Every other type targets Zen.
    fn default_for(provider_type: &str) -> Self {
        if provider_type == "opencode-free" {
            #[cfg(feature = "opencode-free-tier")]
            const FREE_TIER_RELAY: OpenCodeRelay = OpenCodeRelay::Zen;
            #[cfg(not(feature = "opencode-free-tier"))]
            const FREE_TIER_RELAY: OpenCodeRelay = OpenCodeRelay::Go;
            return FREE_TIER_RELAY;
        }
        Self::Zen
    }

    /// Detect the relay from an effective base URL's path.
    ///
    /// Go is the `/zen/go` path; everything else is Zen. This is deliberately
    /// URL-derived so a caller-supplied `api_base` can never silently select
    /// the wrong per-relay table.
    pub(crate) fn for_base(base: &str) -> Self {
        if base.contains("/zen/go") {
            Self::Go
        } else {
            Self::Zen
        }
    }
}

/// Client-identity header OpenCode's relay reads on every request.
///
/// Upstream reads `x-opencode-session`
/// (`packages/console/app/src/routes/zen/util/handler.ts`:
/// `input.request.headers.get("x-opencode-session")`) as a **backend /
/// prompt-cache affinity** token — it routes a process's requests to a warm
/// backend so a conversation keeps its cache.
///
/// **This is NOT an authentication mechanism.** It carries no credential, it
/// does not satisfy the relay's API-key check (`401 AuthError` still applies
/// without a valid `Authorization`), and it does NOT grant free-tier access —
/// Zen's free-tier gate (`403 FreeTierError`) rejects requests regardless of
/// this header. The name `X-Session-ID` does not exist upstream and must never
/// be reintroduced.
pub(crate) const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Process-stable value of [`OPENCODE_SESSION_HEADER`].
///
/// Initialised exactly once per process so every request a process issues
/// carries the same affinity token — the header only keeps a conversation's
/// prompt cache warm if the value is stable for the whole session. Generated
/// from the already-direct `fastrand` dependency (no new crate), guarded by
/// [`OnceLock`] so concurrent first use cannot produce two different values.
static OPENCODE_SESSION_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The stable, opaque, per-process `x-opencode-session` value.
///
/// Opaque by construction (`concerto-<128 random bits>`) — it identifies
/// nothing and authorizes nothing; see [`OPENCODE_SESSION_HEADER`].
pub(crate) fn opencode_session_id() -> &'static str {
    OPENCODE_SESSION_ID
        .get_or_init(|| format!("concerto-{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..)))
}

/// Stable capability name used by every ADR-66 capability refusal.
pub(crate) const TOOL_CALLING_CAPABILITY: &str = "tool_calling";

/// The wire dialect OpenCode serves a model with.
///
/// Replaces the former implicit `needs_anthropic_dialect` /
/// `needs_responses_api` bool pair with one exhaustive decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiMode {
    /// `POST {base}/chat/completions` — OpenAI-compatible Chat Completions.
    ChatCompletions,
    /// `POST {base}/responses` — OpenAI Responses API.
    Responses,
    /// `POST {base}/messages` — Anthropic Messages API.
    AnthropicMessages,
}

/// Go-relay prefix table — mirrors `opencode-go` in the upstream consumer's
/// `_OPENCODE_API_MODE_PREFIXES` (`hermes_cli/models.py:2453-2470`).
///
/// Each entry is `(prefixes, mode)`; the first matching prefix wins, exactly
/// as upstream iterates its tuple. Note `muse-spark` has **no trailing
/// hyphen** upstream: it is a prefix, not a whole token.
const GO_API_MODE_PREFIXES: &[(&[&str], ApiMode)] = &[
    (&["gpt-", "grok-", "muse-spark"], ApiMode::Responses),
    (&["minimax-", "qwen", "union-alpha"], ApiMode::AnthropicMessages),
];

/// Zen-relay prefix table — mirrors `opencode-zen` in the upstream consumer's
/// `_OPENCODE_API_MODE_PREFIXES`.
///
/// Order matters: `claude-`/`union-alpha` are checked before the Responses
/// prefixes, and `qwen` last — the first matching prefix wins. `claude-` and
/// `union-alpha` are Anthropic here but **not** on Go; `minimax-` is Anthropic
/// on Go but **not** here.
const ZEN_API_MODE_PREFIXES: &[(&[&str], ApiMode)] = &[
    (&["claude-", "union-alpha"], ApiMode::AnthropicMessages),
    (&["gpt-", "grok-", "muse-spark"], ApiMode::Responses),
    (&["qwen"], ApiMode::AnthropicMessages),
];

/// Resolve the wire dialect for a `(relay, model id)` pair.
///
/// Matching is on the **lowercased full model id** with `starts_with`, and the
/// first matching prefix wins. This mirrors the upstream relay contract
/// exactly (`hermes_cli/models.py:2453-2470`), which dispatches on
/// `str.startswith` rather than family tokens: `gpt-5.6-luna` is Responses
/// while `omen-alpha` is Chat Completions even though both carry an `-alpha`
/// segment. The default is [`ApiMode::ChatCompletions`].
///
/// `omen-alpha` is intentionally NOT treated as Anthropic: upstream lists only
/// `union-alpha`, even though the live Go roster carries `omen-alpha`. That id
/// therefore rides Chat Completions until upstream changes — do not "fix" the
/// discrepancy here.
///
/// After the per-relay prefix table, the token-bounded Muse family rule is
/// preserved for Zen (ADR-66 §5 correction): genuine `muse-v*` members are
/// served only via `/responses` even though upstream's prefix list does not
/// name them. Whole-token matching keeps `some-muse-model`/`amuse-v2` on Chat
/// Completions. The Go table/roster has no genuine `muse-*` ids, so the rule
/// is Zen-scoped to match upstream's Go dispatch.
pub(crate) fn api_mode_for(relay: OpenCodeRelay, model: &str) -> ApiMode {
    let normalized = model.to_ascii_lowercase();
    let table = match relay {
        OpenCodeRelay::Go => GO_API_MODE_PREFIXES,
        OpenCodeRelay::Zen => ZEN_API_MODE_PREFIXES,
    };
    for (prefixes, mode) in table {
        if prefixes.iter().any(|prefix| normalized.starts_with(*prefix)) {
            return *mode;
        }
    }
    if relay == OpenCodeRelay::Zen && is_genuine_muse_family(model) {
        return ApiMode::Responses;
    }
    ApiMode::ChatCompletions
}

/// Split a model name into lowercase family tokens — the **shared**
/// tokenizer of this crate.
///
/// Model ids are hyphen-delimited (`muse-v2`, `claude-sonnet-4`); the
/// hyphen is the only family separator honored. A token is the whole
/// dash-delimited word, so substring collisions inside larger tokens are
/// impossible. Tokens are owned — callers keep them as a standalone list.
///
/// Every name-based model-name decision in this crate routes through this
/// one function: the Muse family rule in [`api_mode_for`] and the tool-schema
/// tier heuristic
/// ([`crate::adapters::schema_loose::last_resort_weak_tool_calling_model`]). There
/// must be exactly one tokenizer — do not copy this logic (ADR-66 §5:
/// family heuristics match whole tokens, never bare substrings).
///
/// Note: the OpenCode **wire prefix table** in [`api_mode_for`] deliberately
/// does NOT use this tokenizer — it mirrors the upstream relay's prefix
/// contract, which is `str.startswith` on the full id.
pub(crate) fn tokenize_model_name(model: &str) -> Vec<String> {
    model.to_ascii_lowercase().split('-').map(str::to_owned).collect()
}

/// Whether `model` is a genuine Muse family member (token-bounded).
///
/// A `muse` token immediately followed by a `v`-prefixed version token (`v2`,
/// `v2.1`, `v3`, `v3-pro`) selects the Responses dialect on Zen (ADR-66 §5
/// correction, 0d511f1). A bare `muse` token (`muse-pro`, `muse-latest`) or a
/// name merely *containing* `muse` (`some-muse-model`, `amuse-v2`, `museum-2`)
/// never matches — substring collisions are impossible because matching is on
/// whole hyphen-delimited tokens.
fn is_genuine_muse_family(model: &str) -> bool {
    let tokens = tokenize_model_name(model);
    tokens
        .iter()
        .zip(tokens.iter().skip(1))
        .any(|(token, next)| token == "muse" && is_muse_version_segment(next))
}

/// Whether a token is a Muse version segment: a leading `v` followed by at
/// least one ASCII digit (`v2`, `v2.1`, `v3`, `v3-pro`). Everything else
/// (`spark`, `pro`, `vapor`) is not a version segment.
fn is_muse_version_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.first() == Some(&b'v') && bytes.get(1).is_some_and(u8::is_ascii_digit)
}

/// OpenCode provider (Zen or Go relay) that automatically selects the correct
/// wire dialect per model family.
pub struct OpenCodeZenProvider {
    model: String,
    timeout_secs: u64,
    api_base: String,
    /// Which relay [`Self::api_base`] targets. Derived once at construction
    /// from the effective base URL ([`OpenCodeRelay::for_base`]) and consulted
    /// by [`Self::stream_completion`] to pick the relay's dialect table.
    relay: OpenCodeRelay,
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
    /// Whether this provider is serving OpenCode's unauthenticated free tier
    /// (see [`crate::credential`]). Mirrored into `openai_inner` so the
    /// OpenAI-compatible leg agrees with the Responses/Anthropic legs.
    free_tier: bool,
    /// Pre-built inner OpenAI provider for OpenAI-compatible models.
    openai_inner: OpenAiProvider,
}

impl OpenCodeZenProvider {
    /// Resolve the effective API base URL for an OpenCode-family provider
    /// config.
    ///
    /// A config-supplied `api_base` always wins verbatim (self-hosted
    /// gateways, proxies, tests); otherwise the relay default for the provider
    /// type applies — **Go** ([`OPENCODE_GO_BASE`]) for `opencode-free`,
    /// **Zen** ([`OPENCODE_ZEN_BASE`]) for `opencode`. Both factory arms and
    /// the model-listing helper route through this one rule so the two
    /// provider types can never disagree about which relay they target.
    ///
    /// The provider-type → relay mapping itself lives in exactly one place,
    /// [`OpenCodeRelay::resolve`]; this function only turns the chosen relay
    /// into its base URL.
    pub(crate) fn resolve_api_base(provider_type: &str, api_base: Option<&str>) -> String {
        match api_base {
            Some(base) => base.to_string(),
            None => match OpenCodeRelay::resolve(provider_type, None) {
                OpenCodeRelay::Go => OPENCODE_GO_BASE.to_string(),
                OpenCodeRelay::Zen => OPENCODE_ZEN_BASE.to_string(),
            },
        }
    }

    /// Build a provider targeting the OpenCode Zen endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(api_key, model, timeout_secs, OPENCODE_ZEN_BASE.to_string())
    }

    /// Build a provider with an explicit API base URL, overriding the relay
    /// default (Zen for `new`).
    ///
    /// Useful for self-hosted gateways, proxies, or tests.
    pub fn with_api_base(
        api_key: impl Into<SecretString>,
        model: String,
        timeout_secs: u64,
        api_base: String,
    ) -> Self {
        // Every wire path this provider owns — `/responses`, `/messages`, and
        // the inner OpenAI-compatible `/chat/completions` + `/models` legs —
        // carries the affinity header, so the relay sees one stable session
        // across all three dialects (see `OPENCODE_SESSION_HEADER`).
        let openai_inner = OpenAiProvider::new(api_key, model.clone(), timeout_secs)
            .with_api_base(api_base.clone())
            .with_reasoning_echo(ReasoningEcho::Always)
            .with_extra_header(OPENCODE_SESSION_HEADER, opencode_session_id());
        // The effective base URL fully determines the relay: the Go base
        // carries the `/zen/go` path, and `resolve_api_base` injects that
        // default for `opencode-free`, so a caller-supplied base pointing at
        // `/zen/go` selects the Go table even when the provider type is Zen.
        let relay = OpenCodeRelay::for_base(&api_base);
        Self {
            model,
            timeout_secs,
            api_base,
            relay,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            advertised_tool_support: None,
            free_tier: false,
            openai_inner,
        }
    }

    /// Enable OpenCode's unauthenticated free-tier wire behaviour on this
    /// provider and its inner OpenAI-compatible connector.
    ///
    /// Set by the factory only for `opencode-free` under the
    /// `opencode-free-tier` feature. Default `false` keeps both the feature-off
    /// OpenCode path and every other provider byte-identical. Feature-gated so
    /// the feature-off build has no unused method.
    #[cfg(feature = "opencode-free-tier")]
    pub(crate) fn with_free_tier(mut self, free_tier: bool) -> Self {
        self.free_tier = free_tier;
        self.openai_inner = self.openai_inner.with_free_tier(free_tier);
        self
    }

    /// The base URL this provider builds its request paths from.
    ///
    /// Crate-private accessor for callers outside this module (the relay a
    /// provider actually targets is otherwise invisible from the
    /// `LlmProvider` trait). Kept `#[cfg(test)]` until such a caller exists —
    /// the crate denies dead code.
    #[cfg(test)]
    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    /// The credential both wire paths authenticate with — a borrow, so
    /// callers never materialize a second copy of the key.
    pub(crate) fn api_key(&self) -> &SecretString {
        self.openai_inner.api_key()
    }

    /// The three-state wire credential for a request (see `crate::credential`).
    fn wire_credential(&self) -> crate::credential::WireCredential<'_> {
        crate::credential::resolve_wire_credential(self.api_key().expose(), self.free_tier)
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
        // One credential decision per request; the error mapping reuses it so
        // the anonymous free-tier path is recognised consistently.
        let credential = self.wire_credential();
        let anonymous = credential.is_anonymous();

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .bearer_auth(credential.expose())
                    .header(CONTENT_TYPE, "application/json")
                    // Backend/prompt-cache affinity, not authentication —
                    // see `OPENCODE_SESSION_HEADER`.
                    .header(OPENCODE_SESSION_HEADER, opencode_session_id())
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::credential::map_opencode_http_error(
                        status,
                        &text,
                        retry_after,
                        self.free_tier,
                        anonymous,
                    ));
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
        // The Anthropic dialect carries the same three-state credential in
        // `x-api-key` (see `crate::credential`): a real key verbatim, the
        // literal `public` in keyless free-tier mode, or the historical empty
        // value when free-tier mode is off.
        let credential = self.wire_credential();
        let anonymous = credential.is_anonymous();

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .header("x-api-key", credential.expose())
                    .header("anthropic-version", "2023-06-01")
                    .header(CONTENT_TYPE, "application/json")
                    // Backend/prompt-cache affinity, not authentication —
                    // see `OPENCODE_SESSION_HEADER`.
                    .header(OPENCODE_SESSION_HEADER, opencode_session_id())
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::credential::map_opencode_http_error(
                        status,
                        &text,
                        retry_after,
                        self.free_tier,
                        anonymous,
                    ));
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
        // The ONE dialect decision: (relay, model) -> wire mode. The relay is
        // fixed at construction from the effective base URL, so the Go relay
        // can never be routed with Zen's table (the 400 bug this fixes).
        match api_mode_for(self.relay, &model) {
            ApiMode::AnthropicMessages => self.stream_completion_anthropic(request, cancel).await,
            ApiMode::Responses => self.stream_completion_responses(request, cancel).await,
            ApiMode::ChatCompletions => self.openai_inner.stream_completion(request, cancel).await,
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
    // Dialect detection: the relay-aware upstream prefix table
    // -----------------------------------------------------------------------

    /// Shorthand for the two relays under test.
    const GO: OpenCodeRelay = OpenCodeRelay::Go;
    const ZEN: OpenCodeRelay = OpenCodeRelay::Zen;

    /// Pins the **Go** relay table verbatim (`opencode-go` in upstream
    /// `hermes_cli/models.py`): `gpt-`/`grok-`/`muse-spark` → Responses,
    /// `minimax-`/`qwen`/`union-alpha` → Anthropic, everything else → Chat
    /// Completions. This is the relay the 400 bug came from: `gpt-*` and
    /// `grok-*` were previously sent to `/chat/completions`.
    #[test]
    fn go_relay_prefix_table_is_pinned() {
        for model in ["gpt-5.6-luna", "gpt-6-luna", "grok-4.7", "muse-spark-1.3-contributor"] {
            assert_eq!(api_mode_for(GO, model), ApiMode::Responses, "{model}");
        }
        for model in ["minimax-m3", "minimax-m2.7", "qwen3.8-max", "union-alpha"] {
            assert_eq!(api_mode_for(GO, model), ApiMode::AnthropicMessages, "{model}");
        }
        // Everything else — including `omen-alpha`, which upstream does NOT
        // list (only `union-alpha`) — stays on Chat Completions.
        for model in [
            "space-bunny-free",
            "kimi-k3",
            "glm-5.3",
            "deepseek-v4-pro",
            "mimo-v2.6-pro",
            "longcat-2.5-preview-free",
            "hy3",
            "omen-alpha",
        ] {
            assert_eq!(api_mode_for(GO, model), ApiMode::ChatCompletions, "{model}");
        }
    }

    /// Pins the **Zen** relay table verbatim (`opencode-zen`): `claude-`/
    /// `union-alpha` → Anthropic, `gpt-`/`grok-`/`muse-spark` → Responses,
    /// `qwen` → Anthropic, else Chat Completions. The two relays genuinely
    /// disagree: `minimax-` is Anthropic on Go but Chat here, and `claude-` is
    /// Anthropic here but Chat on Go.
    #[test]
    fn zen_relay_prefix_table_is_pinned() {
        assert_eq!(api_mode_for(ZEN, "claude-opus-5-5"), ApiMode::AnthropicMessages);
        assert_eq!(api_mode_for(ZEN, "gpt-5.5"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "qwen3.8-max"), ApiMode::AnthropicMessages);
        assert_eq!(api_mode_for(ZEN, "big-pickle"), ApiMode::ChatCompletions);
        assert_eq!(api_mode_for(ZEN, "union-alpha"), ApiMode::AnthropicMessages);
        assert_eq!(api_mode_for(ZEN, "muse-spark-1.2-contributor"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "grok-4.7"), ApiMode::Responses);

        // Relay disagreement, both directions.
        assert_eq!(api_mode_for(ZEN, "minimax-m3"), ApiMode::ChatCompletions);
        assert_eq!(api_mode_for(GO, "claude-opus-5-5"), ApiMode::ChatCompletions);
    }

    /// Matching lowercases the full id first, so a mixed-case id resolves
    /// exactly like its lowercase form.
    #[test]
    fn dialect_matching_is_case_insensitive() {
        for (relay, model, expected) in [
            (GO, "MUSE-SPARK-1.3-CONTRIBUTOR", ApiMode::Responses),
            (GO, "MiniMax-M3", ApiMode::AnthropicMessages),
            (GO, "QWEN3.8-MAX", ApiMode::AnthropicMessages),
            (GO, "GPT-6-Luna", ApiMode::Responses),
            (ZEN, "Claude-Opus-5-5", ApiMode::AnthropicMessages),
            (ZEN, "GPT-5.5", ApiMode::Responses),
            (ZEN, "Big-Pickle", ApiMode::ChatCompletions),
        ] {
            assert_eq!(api_mode_for(relay, model), expected, "{model}");
        }
    }

    /// A caller-supplied `api_base` that points at the `/zen/go` path selects
    /// the Go table; any other base selects Zen. Provider type only supplies
    /// the default base when no override is given.
    #[test]
    fn custom_api_base_selects_relay_from_url() {
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/go/v1"), OpenCodeRelay::Go);
        assert_eq!(OpenCodeRelay::for_base("https://proxy.internal/zen/go"), OpenCodeRelay::Go);
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/v1"), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::for_base("http://127.0.0.1:9"), OpenCodeRelay::Zen);

        // The free-tier port targets Zen when the feature is on; the shipped
        // default keeps `opencode-free` on the Go relay.
        #[cfg(not(feature = "opencode-free-tier"))]
        assert_eq!(OpenCodeRelay::resolve("opencode-free", None), OpenCodeRelay::Go);
        #[cfg(feature = "opencode-free-tier")]
        assert_eq!(OpenCodeRelay::resolve("opencode-free", None), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::resolve("opencode", None), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::resolve("openai", None), OpenCodeRelay::Zen);
        // An explicit non-Go override wins over the provider-type default...
        assert_eq!(
            OpenCodeRelay::resolve("opencode-free", Some("https://opencode.ai/zen/v1")),
            OpenCodeRelay::Zen
        );
        // ...and a custom `/zen/go` base selects Go even for `opencode`.
        assert_eq!(
            OpenCodeRelay::resolve("opencode", Some("https://opencode.ai/zen/go/v1")),
            OpenCodeRelay::Go
        );
    }

    /// ADR-66 §5 correction (2026-09-08): `muse-spark*` is not a Muse family
    /// member, but the Zen gateway 500s on `/chat/completions` and only serves
    /// it via `POST /responses` (the original fix, 0d511f1). The upstream
    /// prefix table names `muse-spark` (no trailing hyphen), so the match is a
    /// raw prefix — `muse-sparkless` matches upstream too. Do not "tidy" this
    /// into a token-bounded rule.
    #[test]
    fn muse_spark_prefix_is_endpoint_behavior() {
        for model in ["muse-spark-1.3-contributor-free", "Muse-Spark-1.2", "muse-spark-1.3"] {
            assert_eq!(api_mode_for(ZEN, model), ApiMode::Responses, "{model}");
        }
        assert_eq!(
            api_mode_for(ZEN, "muse-sparkless"),
            ApiMode::Responses,
            "the upstream table is prefix-based: `muse-spark` matches `muse-sparkless`"
        );
    }

    /// ADR-66 §5 regression: the token-bounded Muse family rule matches whole
    /// Muse tokens only. Every near-miss here stays on the OpenAI-compatible
    /// dialect — no prefix entry covers them, so name resemblance alone must
    /// never select the Responses dialect.
    #[test]
    fn muse_near_misses_never_route_to_responses() {
        // `muse` inside another token.
        assert_ne!(api_mode_for(ZEN, "some-muse-model"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "amuse-v2"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "museum-2"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "musex-v2"), ApiMode::Responses);
        // `muse-` without a known version segment after it.
        assert_ne!(api_mode_for(ZEN, "muse"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "muse-pro"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "muse-vapor"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "muse-latest"), ApiMode::Responses);
        // Empty / unrelated names stay on the OpenAI-compatible dialect.
        assert_ne!(api_mode_for(ZEN, ""), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "big-pickle"), ApiMode::Responses);
        assert_ne!(api_mode_for(ZEN, "deepseek-v4-flash-free"), ApiMode::Responses);
    }

    /// Genuine `muse-v*` family members keep the Responses dialect on Zen via
    /// the token-bounded rule (the upstream prefix table does not name them,
    /// but Zen serves them only via `/responses`).
    #[test]
    fn genuine_muse_family_uses_responses_on_zen() {
        assert_eq!(api_mode_for(ZEN, "MUSE-v2"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "muse-v2"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "muse-v3"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "muse-v2.1"), ApiMode::Responses);
        assert_eq!(api_mode_for(ZEN, "muse-v3-pro"), ApiMode::Responses);
        // The Go relay has no genuine muse-* ids; unknown ones fall through to
        // Chat Completions exactly as upstream's Go table dictates.
        assert_eq!(api_mode_for(GO, "muse-v2"), ApiMode::ChatCompletions);
    }

    /// ADR-66 §5 regression: `claude-` matches as a prefix on Zen, so a name
    /// where `claude` is merely a substring (`claudette-1`, `declaude`,
    /// `claudeify-v2`) never routes to Anthropic.
    #[test]
    fn claude_near_misses_never_route_to_anthropic() {
        for model in ["claude-3-5-sonnet", "Claude-3-opus", "claude-4", "claude-sonnet-4"] {
            assert_eq!(api_mode_for(ZEN, model), ApiMode::AnthropicMessages, "{model}");
        }
        // Substring near-misses must stay on the OpenAI-compatible dialect.
        for model in ["claudette-1", "declaude", "claudeify-v2", "sub-claudeify", ""] {
            assert_ne!(api_mode_for(ZEN, model), ApiMode::AnthropicMessages, "{model}");
        }
    }

    #[test]
    fn openai_models_do_not_need_anthropic_dialect() {
        assert_ne!(api_mode_for(ZEN, "big-pickle"), ApiMode::AnthropicMessages);
        assert_ne!(api_mode_for(ZEN, "deepseek-v4-flash-free"), ApiMode::AnthropicMessages);
        assert_ne!(api_mode_for(ZEN, "MiMo-7B"), ApiMode::AnthropicMessages);
        // `gpt-` is Responses, never Anthropic.
        assert_eq!(api_mode_for(ZEN, "gpt-4o"), ApiMode::Responses);
    }

    #[test]
    fn empty_model_defaults_to_openai() {
        assert_eq!(api_mode_for(ZEN, ""), ApiMode::ChatCompletions);
        assert_eq!(api_mode_for(GO, ""), ApiMode::ChatCompletions);
    }

    /// Runnable demonstration (run with `--nocapture`) printing the resolved
    /// dialect for every id named in the upstream table contract, so the whole
    /// table is visible in one place.
    #[test]
    fn dialect_table_demonstration_prints_for_review() {
        let go_roster = [
            "gpt-5.6-luna",
            "gpt-6-luna",
            "grok-4.7",
            "muse-spark-1.3-contributor",
            "minimax-m3",
            "minimax-m2.7",
            "qwen3.8-max",
            "union-alpha",
            "space-bunny-free",
            "kimi-k3",
            "glm-5.3",
            "deepseek-v4-pro",
            "mimo-v2.6-pro",
            "longcat-2.5-preview-free",
            "hy3",
            "omen-alpha",
        ];
        let zen_roster = ["claude-opus-5-5", "gpt-5.5", "qwen3.8-max", "big-pickle"];
        println!("=== opencode-free (Go relay) ===");
        for model in go_roster {
            println!("  {model:<34} -> {:?}", api_mode_for(GO, model));
        }
        println!("=== opencode (Zen relay) ===");
        for model in zen_roster {
            println!("  {model:<34} -> {:?}", api_mode_for(ZEN, model));
        }
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
    fn claude_model_renders_anthropic_body_via_dialect() {
        // Updated from the old `muse_model_renders_anthropic_body_via_dialect`:
        // `muse-spark-*` now routes to the Responses dialect (upstream prefix
        // table), so the Anthropic body fixture uses a genuine Anthropic id.
        let p = OpenCodeZenProvider::new("key".to_string(), "claude-3-5-sonnet".into(), 30);
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
        let body = p.build_anthropic_body(&request, "claude-3-5-sonnet");
        // Anthropic wire format: stream is always true, max_tokens defaults to 4096
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["model"], "claude-3-5-sonnet");
        // Messages use Anthropic content-array format
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn openai_model_body_not_affected_by_anthropic_path() {
        // big-pickle should not trigger the Anthropic path
        assert_ne!(api_mode_for(ZEN, "big-pickle"), ApiMode::AnthropicMessages);
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

    // -----------------------------------------------------------------------
    // Relay selection (Zen vs Go) and the session-affinity header
    // -----------------------------------------------------------------------

    /// The two relay bases are pinned, distinct, and resolved from the
    /// provider type; a config-supplied `api_base` always wins for either
    /// type (self-hosted gateways, proxies, tests).
    #[test]
    fn resolve_api_base_picks_relay_by_provider_type() {
        assert_eq!(OPENCODE_ZEN_BASE, "https://opencode.ai/zen/v1");
        assert_eq!(OPENCODE_GO_BASE, "https://opencode.ai/zen/go/v1");
        assert_ne!(
            OPENCODE_ZEN_BASE, OPENCODE_GO_BASE,
            "Zen and Go are distinct relays, not path aliases"
        );

        assert_eq!(OpenCodeZenProvider::resolve_api_base("opencode", None), OPENCODE_ZEN_BASE);
        // `opencode-free` targets Go by default; the `opencode-free-tier`
        // feature moves it to Zen (OpenCode's own free-tier relay).
        #[cfg(not(feature = "opencode-free-tier"))]
        assert_eq!(OpenCodeZenProvider::resolve_api_base("opencode-free", None), OPENCODE_GO_BASE);
        #[cfg(feature = "opencode-free-tier")]
        assert_eq!(OpenCodeZenProvider::resolve_api_base("opencode-free", None), OPENCODE_ZEN_BASE);
        // An unrelated type never picks Go.
        assert_eq!(OpenCodeZenProvider::resolve_api_base("openai", None), OPENCODE_ZEN_BASE);

        let override_base = "http://127.0.0.1:9";
        assert_eq!(
            OpenCodeZenProvider::resolve_api_base("opencode", Some(override_base)),
            override_base
        );
        assert_eq!(
            OpenCodeZenProvider::resolve_api_base("opencode-free", Some(override_base)),
            override_base
        );
    }

    /// `api_base()` reports exactly the base the provider builds its request
    /// paths from — the relay default, or the override when one is given.
    #[test]
    fn api_base_accessor_reflects_relay_and_override() {
        let zen = OpenCodeZenProvider::new("key".to_string(), "minimax-m3".into(), 30);
        assert_eq!(zen.api_base(), OPENCODE_ZEN_BASE);

        let overridden = OpenCodeZenProvider::with_api_base(
            "key".to_string(),
            "minimax-m3".into(),
            30,
            "http://127.0.0.1:9".into(),
        );
        assert_eq!(overridden.api_base(), "http://127.0.0.1:9");
    }

    /// The affinity value is generated exactly once per process: identical on
    /// every call (cache affinity only works with a stable token), namespaced
    /// to this client, and opaque — it identifies nothing and authorizes
    /// nothing (see [`OPENCODE_SESSION_HEADER`]).
    #[test]
    fn session_id_is_stable_and_opaque() {
        let first = opencode_session_id();
        let second = opencode_session_id();
        assert_eq!(first, second, "one value per process, not one per call");

        let prefix = "concerto-";
        assert!(first.starts_with(prefix), "namespaced to this client: {first}");
        let bits = &first[prefix.len()..];
        assert_eq!(bits.len(), 32, "128 random bits, hex-encoded: {first}");
        assert!(bits.chars().all(|c| c.is_ascii_hexdigit()), "hex only: {first}");
        assert!(!first.contains(' '), "the header value must stay a single token");
    }

    /// Parse the captured request's header block (lowercased) plus request
    /// line out of raw HTTP bytes.
    fn captured_headers(raw: &[u8]) -> String {
        crate::testing::mock_server::request_headers(raw)
    }

    /// Drive one streaming request against a one-shot mock and return the raw
    /// captured request. The canned response is irrelevant: the assertion
    /// target is the OUTBOUND request, so any completed body works.
    ///
    /// `path_suffix` is appended to the mock base so a caller can point the
    /// provider at the Go relay (`/zen/go/v1`) while still reaching the local
    /// mock; the empty suffix is the Zen-shaped default.
    async fn capture_wire_request_at(model: &str, path_suffix: &str) -> Vec<u8> {
        let (base, req_rx) = crate::testing::mock_server::spawn(String::new());
        let provider = OpenCodeZenProvider::with_api_base(
            "test-key".to_string(),
            model.into(),
            5,
            format!("{base}{path_suffix}"),
        );
        let request = CompletionRequest {
            model: model.into(),
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
        // A zero-byte SSE body ends the stream immediately; whether the
        // parser reports `Ok` or an EOF error is irrelevant here.
        let _ = provider.stream_completion(request, concerto_core::CancellationToken::new()).await;
        req_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the request reaches the mock")
    }

    /// Drive one streaming request against a Zen-shaped mock base.
    async fn capture_wire_request(model: &str) -> Vec<u8> {
        capture_wire_request_at(model, "").await
    }

    /// Drive one streaming request with an explicit credential and free-tier
    /// flag. `free_tier` is consumed only when the feature is compiled in.
    async fn capture_wire_request_with_credential(
        model: &str,
        key: &str,
        free_tier: bool,
    ) -> Vec<u8> {
        let (base, req_rx) = crate::testing::mock_server::spawn(String::new());
        let provider = OpenCodeZenProvider::with_api_base(key.to_string(), model.into(), 5, base);
        #[cfg(feature = "opencode-free-tier")]
        let provider = provider.with_free_tier(free_tier);
        #[cfg(not(feature = "opencode-free-tier"))]
        let _ = free_tier;
        let request = CompletionRequest {
            model: model.into(),
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
        let _ = provider.stream_completion(request, concerto_core::CancellationToken::new()).await;
        req_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the request reaches the mock")
    }

    /// Feature-on, keyless: the Chat Completions leg carries exactly
    /// `Authorization: Bearer public` — the literal sentinel the server maps
    /// back to its anonymous path.
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn keyless_free_tier_chat_leg_sends_bearer_public() {
        let headers =
            captured_headers(&capture_wire_request_with_credential("minimax-m3", "", true).await);
        assert!(
            headers.contains("authorization: bearer public"),
            "keyless free-tier request must carry `Bearer public`: {headers}"
        );
    }

    /// Feature-on, keyed: the real key wins over the anonymous sentinel.
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn keyed_free_tier_chat_leg_sends_the_real_key() {
        let headers = captured_headers(
            &capture_wire_request_with_credential("minimax-m3", "sk-live", true).await,
        );
        assert!(
            headers.contains("authorization: bearer sk-live"),
            "a real key must be sent verbatim: {headers}"
        );
        assert!(!headers.contains("bearer public"), "the sentinel must never accompany a key");
    }

    /// Feature-on, keyless: the Responses leg carries the same sentinel.
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn keyless_free_tier_responses_leg_sends_bearer_public() {
        let headers =
            captured_headers(&capture_wire_request_with_credential("muse-v2", "", true).await);
        assert!(headers.starts_with("post /responses http/1.1"), "{headers}");
        assert!(
            headers.contains("authorization: bearer public"),
            "the Responses leg must carry `Bearer public`: {headers}"
        );
    }

    /// Feature-on, keyless: the Anthropic Messages leg carries the sentinel
    /// in `x-api-key`.
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn keyless_free_tier_anthropic_leg_sends_public_api_key() {
        let headers = captured_headers(
            &capture_wire_request_with_credential("claude-3-5-sonnet", "", true).await,
        );
        assert!(headers.starts_with("post /messages http/1.1"), "{headers}");
        assert!(
            headers.contains("x-api-key: public"),
            "the Anthropic leg must carry `x-api-key: public`: {headers}"
        );
    }

    /// Runnable demonstration (feature `opencode-free-tier`): prints, for a
    /// keyless `opencode-free` provider, the picker's model list and the exact
    /// auth header each wire leg carries.
    ///
    /// Run with:
    /// `cargo test -p concerto-providers --features opencode-free-tier \
    ///  opencode_free_tier_demo -- --nocapture`
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn opencode_free_tier_demo() {
        let config = concerto_config::ProviderConfig {
            id: "demo".into(),
            name: "OpenCode Zen (free)".into(),
            provider: "opencode-free".into(),
            model: "space-bunny-free".into(),
            keyring_key: "demo/api_key".into(),
            ..Default::default()
        };
        let keyless = crate::provider_defs::picker_model_options_for(&config, false);
        println!("opencode-free keyless picker ({} models, all cost.input == 0):", keyless.len());
        for model in &keyless {
            println!("  {model}");
        }

        println!();
        println!("exact auth header per wire leg (keyless, no key):");
        for (leg, model) in [
            ("POST /chat/completions", "space-bunny-free"),
            ("POST /responses", "muse-v2"),
            ("POST /messages", "claude-3-5-sonnet"),
        ] {
            let headers =
                captured_headers(&capture_wire_request_with_credential(model, "", true).await);
            let auth = headers
                .lines()
                .find(|line| line.starts_with("authorization:") || line.starts_with("x-api-key:"))
                .unwrap_or("<none>");
            println!("  {leg} ({model}) -> {auth}");
        }
    }

    /// Feature-off: a keyless request keeps the shipped empty credential and
    /// must never be upgraded to the `public` sentinel.
    #[cfg(not(feature = "opencode-free-tier"))]
    #[tokio::test]
    async fn feature_off_keyless_never_sends_public() {
        let headers =
            captured_headers(&capture_wire_request_with_credential("minimax-m3", "", false).await);
        assert!(
            !headers.contains("bearer public"),
            "the `public` sentinel is feature-gated and must not appear: {headers}"
        );
        assert!(
            headers.contains("authorization: bearer \r\n"),
            "the shipped empty credential shape is preserved: {headers}"
        );
    }

    /// The 400-bug fix, end to end: a base pointing at the Go `/zen/go` path
    /// must select the Go table, so `minimax-*`/`qwen*` reach `/messages` and
    /// `gpt-*`/`grok-*` reach `/responses`. The same id on the Zen base takes
    /// Zen's table, proving the relay (not the model name alone) selected it.
    #[tokio::test]
    async fn go_relay_api_base_dispatches_with_the_go_table() {
        let go_minimax =
            captured_headers(&capture_wire_request_at("minimax-m3", "/zen/go/v1").await);
        assert!(
            go_minimax.starts_with("post /zen/go/v1/messages http/1.1"),
            "minimax-* is Anthropic on Go: {go_minimax}"
        );
        // Credentials and the affinity header are unaffected by the relay.
        assert_session_header(&go_minimax, "the Go /messages leg");
        assert!(
            go_minimax.contains("x-api-key: test-key"),
            "the Go /messages leg authenticates with the resolved key: {go_minimax}"
        );
        let go_gpt = captured_headers(&capture_wire_request_at("gpt-5.6-luna", "/zen/go/v1").await);
        assert!(
            go_gpt.starts_with("post /zen/go/v1/responses http/1.1"),
            "gpt-* is Responses on Go: {go_gpt}"
        );
        assert_session_header(&go_gpt, "the Go /responses leg");
        assert!(
            go_gpt.contains("authorization: bearer test-key"),
            "the Go /responses leg authenticates with the resolved key: {go_gpt}"
        );
        let go_chat =
            captured_headers(&capture_wire_request_at("deepseek-v4-pro", "/zen/go/v1").await);
        assert!(
            go_chat.starts_with("post /zen/go/v1/chat/completions http/1.1"),
            "everything else is Chat Completions on Go: {go_chat}"
        );

        // Same id, Zen base: minimax-* stays Chat Completions.
        let zen_minimax = captured_headers(&capture_wire_request("minimax-m3").await);
        assert!(
            zen_minimax.starts_with("post /chat/completions http/1.1"),
            "minimax-* is Chat Completions on Zen: {zen_minimax}"
        );
    }

    /// Every wire leg this provider owns carries the affinity header, and the
    /// upstream-nonexistent `X-Session-ID` never appears on any of them.
    #[tokio::test]
    async fn every_wire_leg_carries_the_session_header() {
        // OpenAI-compatible `/chat/completions` (inner provider delegation).
        let openai_compat = captured_headers(&capture_wire_request("minimax-m3").await);
        assert!(
            openai_compat.starts_with("post /chat/completions http/1.1"),
            "the OpenAI-compat leg must hit /chat/completions: {openai_compat}"
        );
        assert_session_header(&openai_compat, "the /chat/completions leg");

        // OpenAI Responses `/responses`.
        let responses = captured_headers(&capture_wire_request("muse-v2").await);
        assert!(
            responses.starts_with("post /responses http/1.1"),
            "the Responses leg must hit /responses: {responses}"
        );
        assert_session_header(&responses, "the /responses leg");
        assert!(
            responses.contains("authorization: bearer test-key"),
            "the /responses leg authenticates with the resolved key: {responses}"
        );

        // Anthropic Messages `/messages` (x-api-key instead of Bearer).
        let anthropic = captured_headers(&capture_wire_request("claude-3-5-sonnet").await);
        assert!(
            anthropic.starts_with("post /messages http/1.1"),
            "the Anthropic leg must hit /messages: {anthropic}"
        );
        assert_session_header(&anthropic, "the /messages leg");
        assert!(
            anthropic.contains("x-api-key: test-key"),
            "the /messages leg authenticates with the resolved key: {anthropic}"
        );
    }

    /// Shared request contract for [`assert_session_header`]: the affinity
    /// header is present and `X-Session-ID` — which does not exist upstream —
    /// is absent.
    fn assert_session_header(headers: &str, context: &str) {
        assert!(
            headers.contains("x-opencode-session:"),
            "{context} must carry x-opencode-session: {headers}"
        );
        assert!(
            !headers.contains("x-session-id"),
            "{context} must not send X-Session-ID: {headers}"
        );
    }
}
