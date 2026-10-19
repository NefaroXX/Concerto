//! OpenCode provider (Zen + Go relays).
//!
//! OpenCode operates **two distinct relays** behind one connector:
//!
//! - **Zen** — [`OPENCODE_ZEN_BASE`], the `opencode` provider's default. Serves
//!   the paid/free-tier Zen roster.
//! - **Go** — [`OPENCODE_GO_BASE`], the `opencode-free` provider's default. A
//!   *separate* endpoint with a *separate* model catalog (see
//!   [`crate::provider_defs::OPENCODE_FREE_KNOWN`]): never assume a model id
//!   exists on both relays, and never copy a catalog from one to the other.
//!
//! Neither relay serves completions to a plain unauthenticated client. **Go**
//! answers `401 AuthError "Missing API key."`, so the `opencode-free` provider
//! type always requires a real credential. **Zen**, by contrast, serves its
//! zero-cost roster to an *anonymous* client (`Authorization: Bearer public`,
//! the relay's `"public"` sentinel for "no account"). We present exactly the
//! client identity the real OpenCode client presents — the same
//! `x-opencode-*` identity headers, one stable `ses_` wire session per run
//! (minted opaquely, never the caller's raw ULID), and `opencode/<version>`
//! User-Agent — so the relay classifies Concerto as an OpenCode client instead
//! of manufacturing a fingerprint mismatch. See
//! [`OPENCODE_ANONYMOUS_CREDENTIAL`], [`OPENCODE_SESSION_HEADER`], and
//! [`OpenCodeZenProvider::per_request_headers`].
//! That anonymous path is why the `opencode` provider type can be built with no
//! credential; it is client *identity*, not session registration. A
//! config-supplied `api_base` overrides either default (self-hosted gateways,
//! proxies, tests).
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
use concerto_core::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget, ToolCall};
use concerto_core::CancellationToken;
use concerto_core::SecretString;
use futures::stream::StreamExt;
use reqwest::header::CONTENT_TYPE;
use std::collections::{HashMap, VecDeque};

use crate::adapters::{AnthropicChatDialect, Dialect, ReasoningEcho};
use crate::openai::OpenAiProvider;
use crate::sse::BufferedSseParser;
use crate::sse_state::AnthropicStreamState;

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
/// an anonymous provider type. Kept as a constant so a future relay correction is
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
    /// ([`Self::for_base`]); otherwise `opencode-free` targets Go and every
    /// other type (notably `opencode`) targets Zen. Callers that already
    /// resolved a base through [`OpenCodeZenProvider::resolve_api_base`] use
    /// [`Self::for_base`] directly — the two agree on every base that function
    /// can produce.
    pub(crate) fn resolve(provider_type: &str, api_base: Option<&str>) -> Self {
        match api_base {
            Some(base) => Self::for_base(base),
            None if provider_type == "opencode-free" => Self::Go,
            None => Self::Zen,
        }
    }

    /// Detect the relay from an effective base URL's path.
    ///
    /// Go is the `/zen/go` path; everything else is Zen. This is deliberately
    /// URL-derived so a caller-supplied `api_base` can never silently select
    /// the wrong per-relay table.
    ///
    /// The match is **path-segment-bound**: `zen` and `go` must be two
    /// adjacent whole segments, so sibling paths that merely embed the string
    /// `…/zen/go` — `/zen/gold/v1`, `/zen/go-v2` — stay Zen instead of being
    /// misclassified by a substring test.
    pub(crate) fn for_base(base: &str) -> Self {
        if base.split('/').collect::<Vec<_>>().windows(2).any(|pair| pair == ["zen", "go"]) {
            Self::Go
        } else {
            Self::Zen
        }
    }
}

/// Client-identity header the real OpenCode client sends on every request.
///
/// Upstream reads it (`packages/console/app/src/routes/zen/util/handler.ts`:
/// `input.request.headers.get("x-opencode-session")`) as a session identity;
/// on the paid path it doubles as prompt-cache / backend affinity. Concerto
/// mirrors it from the run's real orchestration session id on keyed requests
/// ([`CompletionRequest::session_id`]); the anonymous free tier mints one
/// opaque client-format wire id per caller session instead (see
/// [`OpenCodeZenProvider::per_request_headers`]). It is **not** an
/// authentication mechanism and authorizes nothing: without a valid
/// `Authorization` the relay still answers `401 AuthError`. Keyed requests
/// with no orchestration session (discovery, tests, standalone legs) omit the
/// header entirely rather than filled with an invented value.
pub(crate) const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Second session-identity spelling the real client sends alongside
/// [`OPENCODE_SESSION_HEADER`].
///
/// The genuine client sends the same session value under both names for the
/// `opencode`/`opencode-go` provider types; Concerto mirrors both so the
/// request is indistinguishable from a real client's.
pub(crate) const OPENCODE_SESSION_ID_HEADER: &str = "x-opencode-session-id";

/// Per-request identifier header the real client sends.
///
/// Upstream sets `x-opencode-request` to the user message id for the turn
/// (`session/llm/request.ts`). Concerto mints one fresh id per outgoing
/// request; it identifies the request, not the caller, and gates nothing.
pub(crate) const OPENCODE_REQUEST_HEADER: &str = "x-opencode-request";

/// Surface header identifying which OpenCode client is calling.
///
/// Upstream sets `x-opencode-client` from `input.flags.client` — `"cli"` for
/// the terminal client, `"desktop"` for the desktop app. This is a
/// construction-time property of the running binary, not per-request
/// identity; see [`set_opencode_client_surface`].
pub(crate) const OPENCODE_CLIENT_HEADER: &str = "x-opencode-client";

/// Surface assumed when the host binary has not declared one.
///
/// The CLI and headless entrypoints keep this; the desktop app calls
/// [`set_opencode_client_surface`] early in startup.
pub(crate) const DEFAULT_OPENCODE_CLIENT: &str = "cli";

/// The relay's `"public"` credential sentinel, normalized to "anonymous".
///
/// `routes/zen/util/handler.ts` treats a `Bearer public` (or `x-api-key:
/// public`) credential as "no account": the request is then served from the
/// zero-cost roster. We send this sentinel instead of an empty `Authorization`
/// so the header is well-formed and the relay's anonymous branch is selected
/// explicitly.
pub(crate) const OPENCODE_ANONYMOUS_CREDENTIAL: &str = "public";

/// Process-wide client surface reported through [`OPENCODE_CLIENT_HEADER`].
///
/// Set once at startup by the host binary (see
/// [`set_opencode_client_surface`]); defaults to
/// [`DEFAULT_OPENCODE_CLIENT`] so headless/CLI callers need no extra setup.
static OPENCODE_CLIENT_SURFACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Declare which OpenCode client surface the running binary presents.
///
/// Call once, before any provider is constructed. The value is process-global
/// because the surface is a property of the binary, not of a request or a
/// provider instance. Subsequent calls are ignored (first write wins) so an
/// embedding host can never be silently overridden by a later component.
pub fn set_opencode_client_surface(surface: &str) {
    let _ = OPENCODE_CLIENT_SURFACE.set(surface.to_string());
}

/// The effective client surface, borrowed for the process lifetime.
pub(crate) fn opencode_client_surface() -> &'static str {
    OPENCODE_CLIENT_SURFACE.get().map(String::as_str).unwrap_or(DEFAULT_OPENCODE_CLIENT)
}

/// Mint one fresh per-request identifier for [`OPENCODE_REQUEST_HEADER`].
///
/// Uses the workspace ULID generator: globally unique, time-ordered, and
/// opaque. The relay treats the value as an opaque message id and gates
/// nothing on it, so no OpenCode-specific shape is required.
pub(crate) fn next_request_id() -> String {
    concerto_core::ids::new_id().to_string()
}

/// Mint one opencode-format `ses_` **wire** session id.
///
/// The anonymous free-tier relay validates the session identity the way the
/// real client mints it (`packages/opencode/src/id/id.ts`): a `ses_` prefix,
/// six hex bytes of a millisecond timestamp shifted left 12 bits and inverted
/// for descending order (plus a within-millisecond counter), and fourteen
/// base62 suffix characters. The caller's orchestration ULID cannot go on the
/// anonymous wire — the relay refuses raw ULIDs — so the provider mints an
/// opaque wire id per caller session and reuses it across that run
/// ([`OpenCodeZenProvider::per_request_headers`]); session-less anonymous
/// completions mint one per request.
pub(crate) fn mint_wire_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    // The low 48 bits — the same window `id.ts` extracts for the hex block.
    const TIMESTAMP_MASK: u64 = 0xFFFF_FFFF_FFFF;

    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    if LAST_MS.load(Ordering::Relaxed) != ms {
        // Mirrors the client's per-millisecond counter reset.
        LAST_MS.store(ms, Ordering::Relaxed);
        COUNTER.store(0, Ordering::Relaxed);
    }
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let descending = !(ms.wrapping_mul(0x1000).wrapping_add(counter));
    let mut id = format!("ses_{:012x}", descending & TIMESTAMP_MASK);
    for _ in 0..14 {
        id.push(BASE62[fastrand::usize(0..62)] as char);
    }
    id
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

/// Upper bound on the anonymous wire-session affinity map. A long-lived
/// provider can serve many runs; cap the retained caller ids so the map stays
/// small. Eviction is safe: it only forfeits cache affinity for stale runs.
const ANONYMOUS_WIRE_SESSION_CAP: usize = 256;

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
    /// Whether this provider targets Zen **without a credential** — the
    /// anonymous free-tier path. Fixed at construction from the (relay, raw
    /// key) pair: `true` only for Zen with an empty key. When set, the
    /// connector sends the [`OPENCODE_ANONYMOUS_CREDENTIAL`] sentinel and
    /// forces `stream: true` (the relay's anonymous branch serves only
    /// streamed completions). Go is never anonymous — the relay rejects
    /// anonymous requests with `401`, and the factory fails closed.
    anonymous: bool,
    /// Anonymous Zen wire session ids keyed by the caller's orchestration
    /// session. The real OpenCode client sends one stable `ses_` id for the
    /// whole session and the relay uses that id for backend/cache affinity
    /// (upstream `session/llm/request.ts`). Concerto must not put the caller's
    /// raw ULID on the anonymous wire, so it mints one opaque `ses_` id per
    /// caller session and reuses it for that run. Session-less anonymous
    /// requests (discovery, standalone legs) still mint one per completion.
    ///
    /// The map is bounded; eviction only causes the next completion for that
    /// run to mint a new wire session, which is a cache miss, never a
    /// correctness issue.
    anonymous_wire_sessions: std::sync::Mutex<HashMap<String, String>>,
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
        // The effective base URL fully determines the relay before any wire
        // decision: the Go base carries the `/zen/go` path, and
        // `resolve_api_base` injects that default for `opencode-free`, so a
        // caller-supplied base pointing at `/zen/go` selects the Go table even
        // when the provider type is Zen.
        let relay = OpenCodeRelay::for_base(&api_base);
        let raw_key = api_key.into();
        // Anonymous is a **Zen-only** property: Go answers `401 AuthError` for
        // an anonymous request, so an empty key there is a misconfiguration the
        // factory already fails closed on — never silently anonymous.
        let anonymous = relay == OpenCodeRelay::Zen && raw_key.expose().trim().is_empty();
        // Substitute the relay's `"public"` anonymous sentinel for the empty
        // key so every leg (including the inner `/models` listing) sends a
        // well-formed credential that selects the relay's anonymous branch.
        let wire_key =
            if anonymous { SecretString::from(OPENCODE_ANONYMOUS_CREDENTIAL) } else { raw_key };
        // The inner connector carries the one construction-time identity header
        // (the client surface); the per-request identity headers are supplied
        // by `per_request_headers` on the completion call so each request can
        // carry its own session id (see `stream_completion`).
        let openai_inner = OpenAiProvider::new(wire_key, model.clone(), timeout_secs)
            .with_api_base(api_base.clone())
            .with_reasoning_echo(ReasoningEcho::Always)
            .with_extra_header(OPENCODE_CLIENT_HEADER, opencode_client_surface())
            .with_anonymous(anonymous);
        Self {
            model,
            timeout_secs,
            api_base,
            relay,
            anonymous,
            anonymous_wire_sessions: std::sync::Mutex::new(HashMap::new()),
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            advertised_tool_support: None,
            openai_inner,
        }
    }

    /// Whether this provider is on the anonymous, credential-less Zen free
    /// tier.
    pub(crate) fn is_anonymous(&self) -> bool {
        self.anonymous
    }

    /// The per-request OpenCode identity headers for one completion.
    ///
    /// Returns the session identity (mirrored under both spellings the real
    /// client uses) plus a fresh per-request id. The client-surface header is
    /// a construction-time property and rides the connector's static extra
    /// headers instead (see [`OPENCODE_CLIENT_HEADER`]).
    ///
    /// **Anonymous** completions never put the caller's orchestration ULID on
    /// the wire: the relay validates the session identity against the client's
    /// own format and refuses raw ULIDs, and it refuses credential-less
    /// completions with no session identity at all (both live-verified
    /// 2026-10-18). The provider mints one opaque opencode-format `ses_` id
    /// per caller session and reuses it for every completion in that run,
    /// mirroring the real client's stable session header (see
    /// [`OpenCodeZenProvider::anonymous_headers_for`]). This helper mints a
    /// fresh id for callers that have no provider instance (keyed paths and
    /// session-less probes never use it for anonymous requests in production).
    /// Keyed requests mirror the caller's real session id when one is supplied
    /// and omit the session headers otherwise — byte-identical to the
    /// pre-patch keyed wire.
    pub(crate) fn per_request_headers(
        session_id: Option<&str>,
        request_id: &str,
        anonymous: bool,
    ) -> Vec<(String, String)> {
        let wire_session = anonymous.then(mint_wire_session_id);
        Self::per_request_headers_with_wire_session(
            session_id,
            request_id,
            anonymous,
            wire_session.as_deref(),
        )
    }

    /// The anonymous-path headers for one completion. A caller session gets
    /// one stable minted wire id for the whole run; a session-less request
    /// mints an id per completion.
    fn anonymous_headers_for(
        &self,
        session_id: Option<&str>,
        request_id: &str,
    ) -> Vec<(String, String)> {
        let wire_session = session_id.map(|caller| self.anonymous_wire_session(caller));
        Self::per_request_headers_with_wire_session(
            session_id,
            request_id,
            true,
            wire_session.as_deref(),
        )
    }

    /// Return the stable anonymous wire session id for one caller session,
    /// minting it on first use. The caller's real ULID is only the in-memory
    /// lookup key; it never reaches the wire.
    fn anonymous_wire_session(&self, caller_session: &str) -> String {
        let mut sessions =
            self.anonymous_wire_sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(id) = sessions.get(caller_session) {
            return id.clone();
        }
        if sessions.len() >= ANONYMOUS_WIRE_SESSION_CAP {
            sessions.clear();
        }
        let id = mint_wire_session_id();
        sessions.insert(caller_session.to_string(), id.clone());
        id
    }

    /// Build the identity headers with an explicit anonymous wire session (or
    /// `None` to mint one for this call).
    fn per_request_headers_with_wire_session(
        session_id: Option<&str>,
        request_id: &str,
        anonymous: bool,
        anonymous_wire_session: Option<&str>,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(3);
        if anonymous {
            let wire_session = match anonymous_wire_session {
                Some(id) => id.to_string(),
                None => mint_wire_session_id(),
            };
            headers.push((OPENCODE_SESSION_HEADER.to_string(), wire_session.clone()));
            headers.push((OPENCODE_SESSION_ID_HEADER.to_string(), wire_session));
        } else if let Some(session) = session_id {
            headers.push((OPENCODE_SESSION_HEADER.to_string(), session.to_string()));
            headers.push((OPENCODE_SESSION_ID_HEADER.to_string(), session.to_string()));
        }
        headers.push((OPENCODE_REQUEST_HEADER.to_string(), request_id.to_string()));
        headers
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

    /// Apply the client-identity headers to a request builder.
    ///
    /// Shared by the two legs this provider owns directly (`/responses`,
    /// `/messages`); the OpenAI-compatible leg gets the same static client
    /// header plus the identical per-request set through the inner connector's
    /// `stream_completion_with_extra_headers`. The client-surface header is
    /// constant (a property of the binary); the session and request headers
    /// come from `per_request_headers` for this specific completion.
    fn apply_relay_headers(
        &self,
        request: reqwest::RequestBuilder,
        per_request: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        per_request.iter().fold(
            request.header(OPENCODE_CLIENT_HEADER, opencode_client_surface()),
            |builder, (name, value)| builder.header(name.as_str(), value.as_str()),
        )
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
        per_request: &[(String, String)],
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
                let r = self
                    .apply_relay_headers(
                        client
                            .post(&url)
                            .bearer_auth(self.api_key().expose())
                            .header(CONTENT_TYPE, "application/json"),
                        per_request,
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_opencode_http_error(
                        status,
                        &text,
                        retry_after,
                        self.is_anonymous(),
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
        per_request: &[(String, String)],
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
                let r = self
                    .apply_relay_headers(
                        client
                            .post(&url)
                            .header("x-api-key", self.api_key().expose())
                            .header("anthropic-version", "2023-06-01")
                            .header(CONTENT_TYPE, "application/json"),
                        per_request,
                    )
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_opencode_http_error(
                        status,
                        &text,
                        retry_after,
                        self.is_anonymous(),
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
        let mut request = request;
        // The relay's anonymous branch serves only streamed completions, so an
        // anonymous Zen request is forced to stream. Paid Zen and Go are left
        // byte-identical — we send the tools the caller asked for either way.
        if self.is_anonymous() {
            request.stream = true;
        }
        let model = self.resolve_model(&request);
        // One fresh per-request id, plus the session identity: keyed requests
        // mirror the run's real session id when the caller supplied one
        // (session-less keyed legs omit it), while anonymous requests carry a
        // minted client-format wire session id because the free-tier relay
        // validates that identity and refuses raw ULIDs. The anonymous wire id
        // is stable for the caller's session so the relay can keep backend/cache
        // affinity across the run, matching the real client.
        let request_id = next_request_id();
        let session_id = request.session_id.map(|id| id.to_string());
        let per_request = if self.is_anonymous() {
            self.anonymous_headers_for(session_id.as_deref(), request_id.as_str())
        } else {
            Self::per_request_headers(session_id.as_deref(), request_id.as_str(), false)
        };
        // The ONE dialect decision: (relay, model) -> wire mode. The relay is
        // fixed at construction from the effective base URL, so the Go relay
        // can never be routed with Zen's table (the 400 bug this fixes).
        match api_mode_for(self.relay, &model) {
            ApiMode::AnthropicMessages => {
                self.stream_completion_anthropic(request, cancel, &per_request).await
            }
            ApiMode::Responses => {
                self.stream_completion_responses(request, cancel, &per_request).await
            }
            ApiMode::ChatCompletions => {
                self.openai_inner
                    .stream_completion_with_extra_headers(request, cancel, &per_request)
                    .await
            }
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
        // Path-segment-bound: a substring hit on a longer segment is not the
        // Go relay.
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/gold/v1"), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/go-v2/v1"), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/gopher"), OpenCodeRelay::Zen);
        assert_eq!(OpenCodeRelay::for_base("https://xzen/go/v1"), OpenCodeRelay::Zen);
        // A trailing slash (the normalized `resolve_api_base` shape) still
        // matches on the adjacent segments.
        assert_eq!(OpenCodeRelay::for_base("https://opencode.ai/zen/go/v1/"), OpenCodeRelay::Go);

        assert_eq!(OpenCodeRelay::resolve("opencode-free", None), OpenCodeRelay::Go);
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
    // Anthropic body rendering tests (dialect integration)
    // -----------------------------------------------------------------------

    #[test]
    fn claude_model_renders_anthropic_body_via_dialect() {
        // Updated from the old `muse_model_renders_anthropic_body_via_dialect`:
        // `muse-spark-*` now routes to the Responses dialect (upstream prefix
        // table), so the Anthropic body fixture uses a genuine Anthropic id.
        let p = OpenCodeZenProvider::new("key".to_string(), "claude-3-5-sonnet".into(), 30);
        let request = CompletionRequest {
            session_id: None,
            stable_system_prefix_bytes: None,
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
            session_id: None,
            stable_system_prefix_bytes: None,
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
            session_id: None,
            stable_system_prefix_bytes: None,
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
            session_id: None,
            stable_system_prefix_bytes: None,
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
            session_id: None,
            stable_system_prefix_bytes: None,
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
        assert_eq!(OpenCodeZenProvider::resolve_api_base("opencode-free", None), OPENCODE_GO_BASE);
        // Only `opencode-free` targets Go; an unrelated type never picks it.
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

    /// The per-request identity header builder mirrors the real client: keyed
    /// requests borrow the caller's session under both spellings (and invent
    /// nothing when the caller has none), while anonymous requests always
    /// carry an opencode-format `ses_` wire id — the free-tier relay refuses
    /// raw ULIDs and session-less completions alike (live-verified
    /// 2026-10-18). This static helper mints per call; the provider's
    /// [`OpenCodeZenProvider::anonymous_headers_for`] reuses one wire id per
    /// caller session.
    #[test]
    fn per_request_headers_mirror_the_client() {
        let headers = OpenCodeZenProvider::per_request_headers(Some("ses_abc"), "msg_xyz", false);
        let pairs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            vec![
                (OPENCODE_SESSION_HEADER, "ses_abc"),
                (OPENCODE_SESSION_ID_HEADER, "ses_abc"),
                (OPENCODE_REQUEST_HEADER, "msg_xyz"),
            ],
            "keyed: the session id is mirrored under both spellings and the request id is sent"
        );

        let session_less = OpenCodeZenProvider::per_request_headers(None, "msg_xyz", false);
        let names: Vec<&str> = session_less.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![OPENCODE_REQUEST_HEADER],
            "keyed, session-less: no session headers are invented"
        );

        for caller_session in [Some("01M57FZD52MN7P652C63EEJXM0"), None] {
            let anonymous =
                OpenCodeZenProvider::per_request_headers(caller_session, "msg_xyz", true);
            let names: Vec<&str> = anonymous.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                names,
                vec![OPENCODE_SESSION_HEADER, OPENCODE_SESSION_ID_HEADER, OPENCODE_REQUEST_HEADER],
                "anonymous: the session identity is always present"
            );
            let wire = anonymous[0].1.as_str();
            assert_eq!(wire, anonymous[1].1, "anonymous: both spellings mirror ONE wire id");
            assert_wire_session_format(wire, "the anonymous minted session id");
            assert_ne!(
                Some(wire),
                caller_session,
                "anonymous: the caller's orchestration ULID never goes on the wire"
            );
        }
    }

    /// One caller session gets one stable anonymous wire id across completions
    /// (mirroring the real client's per-session header), while distinct caller
    /// sessions stay distinct — and the caller's ULID never appears on the
    /// wire. Session-less anonymous requests still mint per completion.
    #[test]
    fn anonymous_wire_session_is_stable_per_caller_session() {
        let provider = OpenCodeZenProvider::new(String::new(), "glm-5.3-flash-free".into(), 5);
        assert!(provider.is_anonymous(), "an empty-key Zen provider is anonymous");

        let caller = "01M57FZD52MN7P652C63EEJXM0";
        let first = provider.anonymous_headers_for(Some(caller), "msg_one");
        let second = provider.anonymous_headers_for(Some(caller), "msg_two");
        let first_wire = first[0].1.clone();
        assert_eq!(first_wire, first[1].1, "both spellings mirror one wire id");
        assert_eq!(first_wire, second[0].1, "one caller session reuses one wire id");
        assert_eq!(second[0].1, second[1].1);
        assert_wire_session_format(&first_wire, "the anonymous stable wire id");
        assert_ne!(first_wire, caller, "the caller ULID never goes on the wire");
        assert_eq!(first[2].1, "msg_one");
        assert_eq!(second[2].1, "msg_two", "the request id still changes per call");

        let other = provider.anonymous_headers_for(Some("01M57FZD52MN7P652C63EEJXM1"), "msg_three");
        assert_ne!(other[0].1, first_wire, "distinct caller sessions get distinct wire ids");

        let session_less_a = provider.anonymous_headers_for(None, "msg_four");
        let session_less_b = provider.anonymous_headers_for(None, "msg_five");
        assert_ne!(
            session_less_a[0].1, session_less_b[0].1,
            "session-less anonymous requests still mint one wire id per completion"
        );
    }

    /// Each call mints a distinct per-request id, so two requests never share
    /// one identifier.
    #[test]
    fn next_request_id_is_unique_per_call() {
        let first = next_request_id();
        let second = next_request_id();
        assert!(!first.is_empty(), "the request id must be non-empty");
        assert_ne!(first, second, "each request gets a fresh id");
    }

    /// The minted wire session id carries the client's exact `ses_` shape
    /// (`id.ts`): twelve hex chars of a descending millisecond timestamp plus
    /// a fourteen-char base62 suffix; each call is fresh.
    #[test]
    fn mint_wire_session_id_has_client_shape_and_is_fresh() {
        let first = mint_wire_session_id();
        let second = mint_wire_session_id();
        assert_ne!(first, second, "each mint is a distinct id");
        assert_wire_session_format(&first, "the minted wire session id");

        // Decode the timestamp block back: it is `~((ms << 12) | counter)`
        // masked to the low 48 bits, so undoing the mask yields the low 36
        // bits of the minting instant's milliseconds.
        let suffix = first.strip_prefix("ses_").expect("the ses_ prefix");
        let encoded = u64::from_str_radix(&suffix[..12], 16).expect("a hex timestamp block");
        let minted_ms_low = (!encoded & 0xFFFF_FFFF_FFFF) >> 12;
        let now_ms_low = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_millis() as u64
            & 0xF_FFFF_FFFF;
        assert!(
            minted_ms_low.abs_diff(now_ms_low) <= 5,
            "the timestamp block must encode the current time: minted={minted_ms_low} \
             now={now_ms_low}"
        );
    }

    /// A keyed provider keeps its key; only an empty-key Zen provider is
    /// anonymous and sends the relay's `"public"` sentinel.
    #[test]
    fn anonymous_zen_uses_public_sentinel() {
        let anonymous = OpenCodeZenProvider::new("", "big-pickle".into(), 5);
        assert!(anonymous.is_anonymous(), "empty key + Zen is the anonymous path");
        assert_eq!(anonymous.api_key().expose(), OPENCODE_ANONYMOUS_CREDENTIAL);

        let paid = OpenCodeZenProvider::new("sk-real", "big-pickle".into(), 5);
        assert!(!paid.is_anonymous(), "a real key keeps the paid path");
        assert_eq!(paid.api_key().expose(), "sk-real");
    }

    /// The Go relay answers `401` for anonymous requests, so an empty key
    /// targeting `/zen/go` must never be treated as the anonymous path.
    #[test]
    fn go_relay_is_never_anonymous() {
        let go = OpenCodeZenProvider::with_api_base(
            "",
            "minimax-m3".into(),
            5,
            format!("{OPENCODE_GO_BASE}/"),
        );
        assert!(!go.is_anonymous(), "Go is never anonymous — the factory fails closed there");
    }

    /// Parse the captured request's header block (lowercased) plus request
    /// line out of raw HTTP bytes.
    fn captured_headers(raw: &[u8]) -> String {
        crate::testing::mock_server::request_headers(raw)
    }

    /// Extract one captured request header's EXACT value (case-insensitive
    /// name match, value verbatim).
    ///
    /// Unlike [`captured_headers`], which lowercases the whole block so plain
    /// `contains` checks stay ergonomic, this preserves case — required to pin
    /// a header *value* to the exact ULID handed to
    /// `CompletionRequest::session_id` (ULIDs are uppercase).
    fn captured_header_value(raw: &[u8], name: &str) -> String {
        let header_end = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("captured request has a header block");
        let headers = String::from_utf8_lossy(&raw[..header_end]);
        headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
            })
            .unwrap_or_else(|| panic!("captured request must carry the {name} header"))
    }

    /// Value-fidelity companion to [`assert_session_header`]: BOTH session
    /// spellings must carry exactly the ULID the request passed as
    /// `CompletionRequest::session_id` — not merely *a* value.
    fn assert_session_header_value(raw: &[u8], expected: concerto_core::ids::Ulid, context: &str) {
        let expected = expected.to_string();
        for name in [OPENCODE_SESSION_HEADER, OPENCODE_SESSION_ID_HEADER] {
            let actual = captured_header_value(raw, name);
            assert_eq!(
                actual, expected,
                "{context}: {name} must equal the exact ULID passed as session_id"
            );
        }
    }

    /// Drive one streaming request against a one-shot mock and return the raw
    /// captured request. The canned response is irrelevant: the assertion
    /// target is the OUTBOUND request, so any completed body works.
    ///
    /// `path_suffix` is appended to the mock base so a caller can point the
    /// provider at the Go relay (`/zen/go/v1`) while still reaching the local
    /// mock; the empty suffix is the Zen-shaped default.
    async fn capture_wire_request_at(model: &str, path_suffix: &str) -> Vec<u8> {
        capture_wire_request_with_key_at(model, path_suffix, "test-key").await
    }

    /// Drive one streaming request with an explicit credential against a
    /// one-shot mock and return the raw captured request. An empty `key`
    /// exercises the anonymous free-tier path (which mints the wire session
    /// identity, so the caller's ULID never reaches the wire).
    ///
    /// The request carries a real session id and a real tool declaration, so
    /// tests can assert the actual identity headers and the *real* tool set on
    /// the wire (the keyed connector substitutes nothing; the anonymous path
    /// only appends `read` and mints the session id).
    async fn capture_wire_request_with_key_at(
        model: &str,
        path_suffix: &str,
        key: &str,
    ) -> Vec<u8> {
        capture_request_with_key_at(model, path_suffix, key, Some(concerto_core::ids::new_id()))
            .await
    }

    /// As [`capture_wire_request_with_key_at`], but with an explicit session
    /// (or `None` for a session-less discovery-style request).
    async fn capture_request_with_key_at(
        model: &str,
        path_suffix: &str,
        key: &str,
        session_id: Option<concerto_core::ids::Ulid>,
    ) -> Vec<u8> {
        let (base, req_rx) = crate::testing::mock_server::spawn(String::new());
        let provider = OpenCodeZenProvider::with_api_base(
            key.to_string(),
            model.into(),
            5,
            format!("{base}{path_suffix}"),
        );
        let request = CompletionRequest {
            session_id,
            stable_system_prefix_bytes: None,
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
            tools: Some(vec![concerto_core::types::ToolDefinition {
                name: "filesystem".into(),
                description: "Read or write a file in the workspace.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
            }]),
            ..Default::default()
        };
        // A zero-byte SSE body ends the stream immediately; whether the
        // parser reports `Ok` or an EOF error is irrelevant here.
        let _ = provider.stream_completion(request, concerto_core::CancellationToken::new()).await;
        req_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the request reaches the mock")
    }

    /// The anonymous path end to end: an empty-key Zen provider sends the
    /// `"public"` sentinel, a `stream: true` body, a minted opencode-format
    /// `ses_` session identity plus the request id, and the caller's *real*
    /// tool declarations alongside the injected `read`. The exact ULID handed
    /// to the request must never reach the wire: the free-tier relay refuses
    /// raw ULIDs (live-verified 2026-10-18).
    #[tokio::test]
    async fn anonymous_wire_sends_public_sentinel_and_real_tools() {
        let session = concerto_core::ids::new_id();
        let raw = capture_request_with_key_at("big-pickle", "", "", Some(session)).await;
        let headers = captured_headers(&raw);
        assert!(
            headers.contains("authorization: bearer public"),
            "anonymous sends the anonymous sentinel: {headers}"
        );
        assert_session_header(&headers, "the anonymous /chat/completions leg");
        let wire_session = captured_header_value(&raw, OPENCODE_SESSION_HEADER);
        assert_wire_session_format(&wire_session, "the anonymous /chat/completions leg");
        assert_eq!(
            wire_session,
            captured_header_value(&raw, OPENCODE_SESSION_ID_HEADER),
            "both session spellings mirror one minted wire id"
        );
        assert_ne!(
            wire_session,
            session.to_string(),
            "the caller's orchestration ULID must never go on the anonymous wire"
        );

        let lower = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(lower.contains("\"stream\":true"), "the anonymous path must stream: {lower}");
        assert!(
            lower.contains("\"name\":\"filesystem\""),
            "the caller's real tool declaration must reach the wire: {lower}"
        );
        assert!(
            lower.contains("\"name\":\"read\""),
            "the injected read declaration must reach the anonymous wire: {lower}"
        );
    }

    /// A session-less KEYED request still sends the request id and the client
    /// surface, but omits the session headers rather than inventing one —
    /// unchanged by the anonymous-path wire work.
    #[tokio::test]
    async fn session_less_keyed_request_omits_session_headers() {
        let raw = capture_request_with_key_at("big-pickle", "", "test-key", None).await;
        let headers = captured_headers(&raw);
        assert!(
            headers.contains("x-opencode-request:"),
            "the request id is always sent: {headers}"
        );
        assert!(
            headers.contains(&format!("x-opencode-client: {DEFAULT_OPENCODE_CLIENT}")),
            "the client surface is always sent: {headers}"
        );
        assert!(
            !headers.contains("x-opencode-session:"),
            "no session header when the request has no session: {headers}"
        );
        assert!(
            !headers.contains("x-opencode-session-id:"),
            "no mirrored session header when the request has no session: {headers}"
        );
    }

    /// Assert a header value has the opencode wire session-id shape: a `ses_`
    /// prefix, twelve hex chars of the masked, inverted `(ms << 12) | counter`
    /// timestamp encoding `id.ts` mints, and fourteen base62 suffix chars.
    fn assert_wire_session_format(value: &str, context: &str) {
        let suffix = value
            .strip_prefix("ses_")
            .unwrap_or_else(|| panic!("{context}: the wire session id must carry ses_: {value}"));
        assert_eq!(suffix.len(), 26, "{context}: 12 hex + 14 base62 expected: {value}");
        assert!(
            suffix[..12].chars().all(|c| c.is_ascii_hexdigit()),
            "{context}: the timestamp block must be hex: {value}"
        );
        assert!(
            suffix[12..].chars().all(|c| c.is_ascii_alphanumeric()),
            "{context}: the suffix must be base62: {value}"
        );
    }

    /// A session-less ANONYMOUS completion still carries the session identity:
    /// the free-tier relay refuses credential-less completions without one
    /// (live-verified 2026-10-18), so the anonymous path mints a fresh
    /// opencode-format wire id even when the caller has no session.
    #[tokio::test]
    async fn anonymous_session_less_request_mints_session_headers() {
        let raw = capture_request_with_key_at("big-pickle", "", "", None).await;
        let headers = captured_headers(&raw);
        assert!(
            headers.contains("authorization: bearer public"),
            "still the anonymous sentinel: {headers}"
        );
        assert!(
            headers.contains("x-opencode-session:"),
            "the anonymous path mints a session identity: {headers}"
        );
        let wire_session = captured_header_value(&raw, OPENCODE_SESSION_HEADER);
        assert_wire_session_format(&wire_session, "the session-less anonymous leg");
        assert_eq!(
            wire_session,
            captured_header_value(&raw, OPENCODE_SESSION_ID_HEADER),
            "both session spellings mirror one minted wire id"
        );
    }

    /// F1 pin: an anonymous request whose model name matches the weak-tool-calling
    /// heuristic (a standalone `mimo` token) must still stream.
    ///
    /// The relay's anonymous branch serves only streamed completions, so the
    /// inner OpenAI-compatible leg's weak-tier transport downgrade must be
    /// suppressed on the anonymous path (the connector forces `stream: true`
    /// before dispatch). Without that, a model carrying a `mimo`/`mini` token
    /// would be downgraded to `stream: false` and refused.
    #[tokio::test]
    async fn anonymous_weak_tier_model_still_streams() {
        // `mimo-v2` tokenizes to `["mimo", "v2"]` (standalone weak-tier token)
        // and routes to Chat Completions on Zen, i.e. through the inner
        // OpenAI-compatible leg where the downgrade lived.
        let raw = capture_wire_request_with_key_at("mimo-v2", "", "").await;
        let lower = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            lower.contains("\"stream\":true"),
            "an anonymous weak-tier model must still stream: {lower}"
        );
        assert!(
            !lower.contains("\"stream\":false"),
            "the weak-tier transport downgrade must never apply on the anonymous path: {lower}"
        );
    }

    /// Drive one streaming request against a Zen-shaped mock base.
    async fn capture_wire_request(model: &str) -> Vec<u8> {
        capture_wire_request_at(model, "").await
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
    ///
    /// Full-path value fidelity: each leg runs with its own caller-chosen
    /// session ULID, and both session spellings must carry that exact value —
    /// plus every leg's per-call `x-opencode-request` id must be distinct, so
    /// three requests can never share one identifier.
    #[tokio::test]
    async fn every_wire_leg_carries_the_session_header() {
        let chat_session = concerto_core::ids::new_id();
        let responses_session = concerto_core::ids::new_id();
        let anthropic_session = concerto_core::ids::new_id();

        // OpenAI-compatible `/chat/completions` (inner provider delegation).
        let chat_raw =
            capture_request_with_key_at("minimax-m3", "", "test-key", Some(chat_session)).await;
        let openai_compat = captured_headers(&chat_raw);
        assert!(
            openai_compat.starts_with("post /chat/completions http/1.1"),
            "the OpenAI-compat leg must hit /chat/completions: {openai_compat}"
        );
        assert_session_header(&openai_compat, "the /chat/completions leg");
        assert_session_header_value(&chat_raw, chat_session, "the /chat/completions leg");

        // OpenAI Responses `/responses`.
        let responses_raw =
            capture_request_with_key_at("muse-v2", "", "test-key", Some(responses_session)).await;
        let responses = captured_headers(&responses_raw);
        assert!(
            responses.starts_with("post /responses http/1.1"),
            "the Responses leg must hit /responses: {responses}"
        );
        assert_session_header(&responses, "the /responses leg");
        assert_session_header_value(&responses_raw, responses_session, "the /responses leg");
        assert!(
            responses.contains("authorization: bearer test-key"),
            "the /responses leg authenticates with the resolved key: {responses}"
        );

        // Anthropic Messages `/messages` (x-api-key instead of Bearer).
        let anthropic_raw = capture_request_with_key_at(
            "claude-3-5-sonnet",
            "",
            "test-key",
            Some(anthropic_session),
        )
        .await;
        let anthropic = captured_headers(&anthropic_raw);
        assert!(
            anthropic.starts_with("post /messages http/1.1"),
            "the Anthropic leg must hit /messages: {anthropic}"
        );
        assert_session_header(&anthropic, "the /messages leg");
        assert_session_header_value(&anthropic_raw, anthropic_session, "the /messages leg");
        assert!(
            anthropic.contains("x-api-key: test-key"),
            "the /messages leg authenticates with the resolved key: {anthropic}"
        );

        // Per-call request ids: each leg mints its own fresh id, so the three
        // captured values are pairwise distinct (and never the session id).
        let request_ids = [
            captured_header_value(&chat_raw, OPENCODE_REQUEST_HEADER),
            captured_header_value(&responses_raw, OPENCODE_REQUEST_HEADER),
            captured_header_value(&anthropic_raw, OPENCODE_REQUEST_HEADER),
        ];
        for (leg, id) in ["chat/completions", "responses", "messages"].into_iter().zip(&request_ids)
        {
            assert!(!id.is_empty(), "the {leg} leg must send a non-empty request id");
        }
        assert_ne!(
            request_ids[0], request_ids[1],
            "the chat and responses legs must mint distinct request ids"
        );
        assert_ne!(
            request_ids[0], request_ids[2],
            "the chat and messages legs must mint distinct request ids"
        );
        assert_ne!(
            request_ids[1], request_ids[2],
            "the responses and messages legs must mint distinct request ids"
        );
    }

    /// Shared request contract for [`assert_session_header`]: the mirrored
    /// session headers, the request id, and the client-surface header are
    /// present, and `X-Session-ID` — which does not exist upstream — is absent.
    fn assert_session_header(headers: &str, context: &str) {
        assert!(
            headers.contains("x-opencode-session:"),
            "{context} must carry x-opencode-session: {headers}"
        );
        assert!(
            headers.contains("x-opencode-session-id:"),
            "{context} must mirror the session id: {headers}"
        );
        assert!(
            headers.contains("x-opencode-request:"),
            "{context} must carry the request id: {headers}"
        );
        assert!(
            headers.contains(&format!("x-opencode-client: {DEFAULT_OPENCODE_CLIENT}")),
            "{context} must carry the client surface: {headers}"
        );
        assert!(
            !headers.contains("x-session-id"),
            "{context} must not send X-Session-ID: {headers}"
        );
    }
}
