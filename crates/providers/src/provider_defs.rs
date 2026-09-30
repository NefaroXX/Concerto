//! Provider definitions: the single source of truth for provider-type metadata
//! (capabilities, known models, defaults, discovery support) plus the shared
//! model-option resolver and provider-readiness validation.
//!
//! These are pure functions consumed by Settings, Chat, the global default
//! control and tests. They never perform I/O and never touch the credential
//! store, so they can be unit-tested without a keychain or network.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use concerto_config::ProviderConfig;

/// Whether a provider type requires an API key to be usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialRequirement {
    Required,
    Optional,
    None,
}

/// Whether a provider type supports model discovery via its API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelDiscoverySupport {
    Supported,
    Unsupported,
}

/// One model in a provider's static catalog, carrying its free-tier
/// classification.
///
/// `free` is derived from the provider catalog's `cost.input == 0` — **never**
/// from the model name. The repo has twice been burned by suffix-based
/// classification (`*-free` / `-contributor`), so the field is explicit and
/// the classifier that builds it is cost-only. A model with no cost data is
/// **not** free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownModel {
    pub id: &'static str,
    /// `true` iff the catalog reports `cost.input == 0` for this model.
    pub free: bool,
}

/// Classify free-ness from a catalog `cost.input` value.
///
/// This is the ONE free-ness rule the [`OPENCODE_FREE_KNOWN`] catalog is built
/// with: `Some(0.0)` is free; a positive cost is not; and **no cost data**
/// (`None`) is not. A model's name is never consulted — `big-pickle` and
/// `grok-code` are free with no `free` marker, and a `-free` suffix on a paid
/// model would still classify as not free.
pub fn is_free_cost(cost_input: Option<f64>) -> bool {
    cost_input == Some(0.0)
}

/// Static metadata describing one provider type.
///
/// Every shipped model ID in [`ProviderDefinition::known_models`] and
/// [`ProviderDefinition::known_model_costs`] MUST be a real API identifier for
/// that provider's API. Live discovery and custom entry cover the long tail,
/// so this catalog is deliberately small and hand-maintained.
#[derive(Debug, Clone)]
pub struct ProviderDefinition {
    pub id: &'static str,
    pub display_name: String,
    pub default_model: Option<&'static str>,
    pub known_models: &'static [&'static str],
    /// Cost-aware catalog for provider types whose free-tier classification is
    /// derived from catalog cost data. Empty for every other type, whose
    /// `known_models` are the whole story.
    ///
    /// When non-empty, its ids are merged with `known_models` by
    /// [`ProviderDefinition::known_model_ids`] and it is authoritative for
    /// free-ness via [`ProviderDefinition::known_model_is_free`].
    pub known_model_costs: &'static [KnownModel],
    pub credential_requirement: CredentialRequirement,
    pub model_discovery: ModelDiscoverySupport,
    pub allows_custom_model: bool,
}

impl ProviderDefinition {
    /// Whether this provider type requires a credential to be usable.
    pub fn requires_credential(&self) -> bool {
        matches!(self.credential_requirement, CredentialRequirement::Required)
    }

    /// Whether this provider type supports live model discovery.
    pub fn supports_discovery(&self) -> bool {
        matches!(self.model_discovery, ModelDiscoverySupport::Supported)
    }

    /// Every known model id, from the plain catalog and the cost-aware one.
    ///
    /// The two are disjoint in practice; callers deduplicate anyway (the
    /// resolver does).
    pub fn known_model_ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.known_models.iter().copied().chain(self.known_model_costs.iter().map(|model| model.id))
    }

    /// Free-tier classification for a known model, when this provider ships
    /// cost metadata. `None` when the model is not in the cost catalog (or the
    /// provider ships none) — callers must treat that as **not free**.
    pub fn known_model_is_free(&self, model: &str) -> Option<bool> {
        self.known_model_costs
            .iter()
            .find(|known| known.id.eq_ignore_ascii_case(model.trim()))
            .map(|known| known.free)
    }
}

/// Defensive cap for discovered model catalogs so a huge response (e.g. OpenRouter)
/// cannot blow up the UI or the cache.
pub const MAX_CACHED_MODELS: usize = 2_000;

/// Hand-maintained, deliberately small catalog of well-known stable model IDs.
/// Do not ship speculative names — live discovery and custom entry cover the rest.
const OPENAI_KNOWN: &[&str] = &["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "gpt-4", "gpt-3.5-turbo"];
const ANTHROPIC_KNOWN: &[&str] = &[
    "claude-3-5-sonnet-latest",
    "claude-3-5-haiku-latest",
    "claude-3-opus-latest",
    "claude-3-haiku-20240307",
];
const GOOGLE_KNOWN: &[&str] = &["gemini-1.5-pro", "gemini-1.5-flash", "gemini-2.0-flash-exp"];
const DEEPSEEK_KNOWN: &[&str] = &["deepseek-chat", "deepseek-reasoner"];
const GROQ_KNOWN: &[&str] =
    &["llama-3.3-70b-versatile", "openai/gpt-oss-120b", "openai/gpt-oss-20b"];
const TOGETHER_KNOWN: &[&str] = &[
    "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "meta-llama/Llama-4-Scout-17B-16E-Instruct",
    "deepseek-ai/DeepSeek-V3",
];
const MISTRAL_KNOWN: &[&str] =
    &["mistral-large-latest", "mistral-medium-latest", "mistral-small-latest", "codestral-latest"];
const XAI_KNOWN: &[&str] = &["grok-4", "grok-4-fast", "grok-2-latest"];
const FIREWORKS_KNOWN: &[&str] = &[
    "accounts/fireworks/models/llama-v3p3-70b-instruct",
    "accounts/fireworks/models/llama-4-maverick",
    "accounts/fireworks/models/deepseek-v3",
];
const CEREBRAS_KNOWN: &[&str] = &["llama-3.3-70b", "gpt-oss-120b", "llama3.1-8b", "qwen-3-32b"];
const COHERE_KNOWN: &[&str] =
    &["command-a-plus-05-2026", "command-a", "command-r-plus-08-2024", "command-r-08-2024"];
// DeepInfra — api.deepinfra.com model IDs
const DEEPINFRA_KNOWN: &[&str] = &[
    "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo",
    "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "Qwen/Qwen3-235B-A22B-Instruct-2507",
    "deepseek-ai/DeepSeek-V3.2",
];
// Perplexity — api.perplexity.ai model IDs. `sonar` stays selectable even
// though its legacy Chat Completions surface is deprecated (2026-09-27).
const PERPLEXITY_KNOWN: &[&str] = &["sonar", "sonar-pro", "sonar-reasoning-pro"];
// SambaNova — api.sambanova.ai model IDs
const SAMBANOVA_KNOWN: &[&str] = &[
    "Meta-Llama-3.3-70B-Instruct",
    "DeepSeek-V3.1",
    "MiniMax-M2.7",
    "MiniMax-M3",
    "gemma-4-31B-it",
    "gpt-oss-120b",
];
// Alibaba DashScope — dashscope.aliyuncs.com compatible-mode model IDs (Qwen)
const DASHSCOPE_KNOWN: &[&str] =
    &["qwen-plus", "qwen-turbo", "qwen-max", "qwen3-max", "qwen3-235b-a22b-instruct"];
// Moonshot AI — api.moonshot.cn model IDs. The discontinued `kimi-k2` family
// is intentionally absent; only live identifiers ship (kimi-k2.5/k2.6/k3 plus
// the moonshot-v1 legacy line still served by the platform).
const MOONSHOT_KNOWN: &[&str] =
    &["kimi-k2.6", "kimi-k2.5", "kimi-k3", "moonshot-v1-8k", "moonshot-v1-32k", "moonshot-v1-128k"];
// Zhipu AI — open.bigmodel.cn model IDs (GLM)
const ZHIPU_KNOWN: &[&str] = &["glm-4.7", "glm-4.6", "glm-4.5", "glm-4-plus", "glm-4-flash-250414"];
// Novita AI is shipped discovery-driven: no stable hand-verifiable model ID
// list exists for its OpenAI-compatible surface, so no static catalog and no
// default model are hard-coded (custom entry + live discovery cover the long
// tail).
const NOVITA_KNOWN: &[&str] = &[];

/// Model IDs served by the OpenCode **Go** relay (`https://opencode.ai/zen/go/v1`),
/// the endpoint behind the `opencode-free` provider type when the
/// `opencode-free-tier` feature is **off** (the shipped default).
///
/// Retrieved live from `GET https://opencode.ai/zen/go/v1/models` on
/// 2026-09-29. The catalog is hand-maintained and refreshed periodically; it
/// is deliberately a snapshot of the *Go* roster — a **distinct relay with a
/// distinct catalog**, not Zen's (`opencode` type, `…/zen/v1/models`): of the
/// old Zen free-tier ids only `longcat-2.5-preview-free` and
/// `space-bunny-free` also appear on Go, while Go serves plenty of non-`free`
/// ids (`minimax-m3`, `glm-5.3`, `mimo-v2.6-pro`, …). Never copy a catalog
/// between the two relays.
///
/// Live discovery is intentionally NOT used for this provider type
/// (`ModelDiscoverySupport::Unsupported`): the Go listing endpoint is ungated
/// and returns the full paid roster too, which would leak paid IDs into a
/// free-only picker.
pub const OPENCODE_GO_KNOWN: &[&str] = &[
    "deepseek-flash",
    "deepseek-v4-flash",
    "deepseek-v4-flash-vision-exp",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
    "glm-5",
    "glm-5.1",
    "glm-5.2",
    "glm-5.3",
    "glm-5.3-flash",
    "gpt-5.6-luna",
    "gpt-6-luna",
    "grok-4.5",
    "grok-4.6",
    "grok-4.7",
    "hy3",
    "hy3-preview",
    "hy4-preview",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "kimi-k3",
    "longcat-2.0",
    "longcat-2.5-preview-free",
    "mimo-v2-omni",
    "mimo-v2-pro",
    "mimo-v2.5",
    "mimo-v2.5-pro",
    "mimo-v2.6-flash",
    "mimo-v2.6-pro",
    "minimax-m2.5",
    "minimax-m2.7",
    "minimax-m3",
    "muse-spark-1.2-contributor",
    "muse-spark-1.3-contributor",
    "omen-alpha",
    "qwen3.5-plus",
    "qwen3.6-plus",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.8-flash",
    "qwen3.8-max",
    "space-bunny-free",
];

/// Cost-aware OpenCode **Zen** catalog, the `opencode-free` roster when the
/// `opencode-free-tier` feature is **on**.
///
/// - **Source**: `https://models.opencode.ai/api.json`, the `opencode`
///   provider entry (`api: "https://opencode.ai/zen/v1"`,
///   `npm: "@ai-sdk/openai-compatible"`). This is the same catalog OpenCode
///   itself fetches (`packages/core/src/models-dev.ts`).
/// - **Retrieved**: 2026-09-30. The catalog is a hand-maintained snapshot and
///   is refreshed periodically; re-fetch and regenerate this constant when the
///   Zen roster changes.
/// - **Free-ness**: `free` is `cost.input == 0` from the catalog — the exact
///   gate upstream applies (`if (value.cost.input === 0) continue`). It is
///   **never** derived from a `-free` / `-contributor` name suffix. The repo
///   has been burned by suffix classification twice; `big-pickle` and
///   `grok-code` are free with no `free` in their names, and any model with no
///   cost data is classified NOT free.
///
/// All 114 catalog models ship here so a keyed request can reach the paid
/// roster; the picker filters to the `free` subset only while keyless (see
/// [`picker_model_options_for`]).
pub const OPENCODE_FREE_KNOWN: &[KnownModel] = &[
    KnownModel { id: "big-pickle", free: true },
    KnownModel { id: "claude-3-5-haiku", free: false },
    KnownModel { id: "claude-fable-5", free: false },
    KnownModel { id: "claude-fable-5-1", free: false },
    KnownModel { id: "claude-haiku-4-5", free: false },
    KnownModel { id: "claude-opus-4-1", free: false },
    KnownModel { id: "claude-opus-4-5", free: false },
    KnownModel { id: "claude-opus-4-6", free: false },
    KnownModel { id: "claude-opus-4-7", free: false },
    KnownModel { id: "claude-opus-4-8", free: false },
    KnownModel { id: "claude-opus-5", free: false },
    KnownModel { id: "claude-opus-5-5", free: false },
    KnownModel { id: "claude-sonnet-4", free: false },
    KnownModel { id: "claude-sonnet-4-5", free: false },
    KnownModel { id: "claude-sonnet-4-6", free: false },
    KnownModel { id: "claude-sonnet-5", free: false },
    KnownModel { id: "claude-sonnet-5-5", free: false },
    KnownModel { id: "deepseek-v4-flash", free: false },
    KnownModel { id: "deepseek-v4-flash-free", free: true },
    KnownModel { id: "deepseek-v4-flash-vision-exp", free: false },
    KnownModel { id: "deepseek-v4-pro", free: false },
    KnownModel { id: "deepseek-v4.1-flash", free: false },
    KnownModel { id: "gemini-3-flash", free: false },
    KnownModel { id: "gemini-3-pro", free: false },
    KnownModel { id: "gemini-3.1-pro", free: false },
    KnownModel { id: "gemini-3.5-flash", free: false },
    KnownModel { id: "gemini-3.5-flash-lite", free: false },
    KnownModel { id: "gemini-3.6-flash", free: false },
    KnownModel { id: "gemini-3.7-flash", free: false },
    KnownModel { id: "gemini-3.8-flash", free: false },
    KnownModel { id: "glm-4.6", free: false },
    KnownModel { id: "glm-4.7", free: false },
    KnownModel { id: "glm-4.7-free", free: true },
    KnownModel { id: "glm-5", free: false },
    KnownModel { id: "glm-5-free", free: true },
    KnownModel { id: "glm-5.1", free: false },
    KnownModel { id: "glm-5.2", free: false },
    KnownModel { id: "glm-5.3", free: false },
    KnownModel { id: "glm-5.3-flash", free: false },
    KnownModel { id: "gpt-5", free: false },
    KnownModel { id: "gpt-5-codex", free: false },
    KnownModel { id: "gpt-5-nano", free: false },
    KnownModel { id: "gpt-5.1", free: false },
    KnownModel { id: "gpt-5.1-codex", free: false },
    KnownModel { id: "gpt-5.1-codex-max", free: false },
    KnownModel { id: "gpt-5.1-codex-mini", free: false },
    KnownModel { id: "gpt-5.2", free: false },
    KnownModel { id: "gpt-5.2-codex", free: false },
    KnownModel { id: "gpt-5.3-codex", free: false },
    KnownModel { id: "gpt-5.3-codex-spark", free: false },
    KnownModel { id: "gpt-5.4", free: false },
    KnownModel { id: "gpt-5.4-mini", free: false },
    KnownModel { id: "gpt-5.4-nano", free: false },
    KnownModel { id: "gpt-5.4-pro", free: false },
    KnownModel { id: "gpt-5.5", free: false },
    KnownModel { id: "gpt-5.5-pro", free: false },
    KnownModel { id: "gpt-5.6-luna", free: false },
    KnownModel { id: "gpt-5.6-sol", free: false },
    KnownModel { id: "gpt-5.6-terra", free: false },
    KnownModel { id: "gpt-6-astra", free: false },
    KnownModel { id: "gpt-6-luna", free: false },
    KnownModel { id: "gpt-6-sol", free: false },
    KnownModel { id: "gpt-6.1-sol", free: false },
    KnownModel { id: "grok-4.5", free: false },
    KnownModel { id: "grok-4.6", free: false },
    KnownModel { id: "grok-4.7", free: false },
    KnownModel { id: "grok-build-0.1", free: false },
    KnownModel { id: "grok-code", free: true },
    KnownModel { id: "hy3-free", free: true },
    KnownModel { id: "hy3-preview-free", free: true },
    KnownModel { id: "kimi-k2", free: false },
    KnownModel { id: "kimi-k2-thinking", free: false },
    KnownModel { id: "kimi-k2.5", free: false },
    KnownModel { id: "kimi-k2.5-free", free: true },
    KnownModel { id: "kimi-k2.6", free: false },
    KnownModel { id: "kimi-k2.7-code", free: false },
    KnownModel { id: "kimi-k3", free: false },
    KnownModel { id: "laguna-s-2.1-free", free: true },
    KnownModel { id: "ling-2.6-flash-free", free: true },
    KnownModel { id: "ling-3.0-flash-fin-free", free: true },
    KnownModel { id: "ling-3.0-flash-free", free: true },
    KnownModel { id: "ling-3.0-tiny-free", free: true },
    KnownModel { id: "longcat-2.0-free", free: true },
    KnownModel { id: "longcat-2.5-preview-free", free: true },
    KnownModel { id: "mimo-v2-flash-free", free: true },
    KnownModel { id: "mimo-v2-omni-free", free: true },
    KnownModel { id: "mimo-v2-pro-free", free: true },
    KnownModel { id: "mimo-v2.5-free", free: true },
    KnownModel { id: "mimo-v2.6-flash-free", free: true },
    KnownModel { id: "minimax-m2.1", free: false },
    KnownModel { id: "minimax-m2.1-free", free: true },
    KnownModel { id: "minimax-m2.5", free: false },
    KnownModel { id: "minimax-m2.5-free", free: true },
    KnownModel { id: "minimax-m2.7", free: false },
    KnownModel { id: "minimax-m3", free: false },
    KnownModel { id: "minimax-m3-free", free: true },
    KnownModel { id: "muse-spark-1.2", free: false },
    KnownModel { id: "muse-spark-1.2-contributor-free", free: true },
    KnownModel { id: "muse-spark-1.3", free: false },
    KnownModel { id: "muse-spark-1.3-contributor-free", free: true },
    KnownModel { id: "nemotron-3-super-free", free: true },
    KnownModel { id: "nemotron-3-ultra-free", free: true },
    KnownModel { id: "nemotron-3.5-lightning-free", free: true },
    KnownModel { id: "north-mini-code-free", free: true },
    KnownModel { id: "qwen3-coder", free: false },
    KnownModel { id: "qwen3.5-plus", free: false },
    KnownModel { id: "qwen3.6-plus", free: false },
    KnownModel { id: "qwen3.6-plus-free", free: true },
    KnownModel { id: "qwen3.8-flash", free: false },
    KnownModel { id: "qwen3.8-max", free: false },
    KnownModel { id: "ring-2.6-1t-free", free: true },
    KnownModel { id: "space-bunny-free", free: true },
    KnownModel { id: "trinity-large-preview-free", free: true },
    KnownModel { id: "x-preview-f-free", free: true },
];

/// Return the [`ProviderDefinition`] for a provider type string.
///
/// Unrecognized types fall back to a permissive "unknown" definition so the UI
/// still works (custom model entry + no discovery). The fallback keeps the app
/// usable for new providers without forcing a code change.
pub fn provider_definition(provider_type: &str) -> ProviderDefinition {
    match provider_type {
        "openai" => ProviderDefinition {
            id: "openai",
            display_name: String::from("OpenAI"),
            default_model: Some("gpt-4o"),
            known_models: OPENAI_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "anthropic" => ProviderDefinition {
            id: "anthropic",
            display_name: String::from("Anthropic"),
            default_model: Some("claude-3-5-sonnet-latest"),
            known_models: ANTHROPIC_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "google" => ProviderDefinition {
            id: "google",
            display_name: String::from("Google"),
            default_model: Some("gemini-1.5-pro"),
            known_models: GOOGLE_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "openrouter" => ProviderDefinition {
            id: "openrouter",
            display_name: String::from("OpenRouter"),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "nim" => ProviderDefinition {
            id: "nim",
            display_name: String::from("NVIDIA NIM"),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "ollama" => ProviderDefinition {
            id: "ollama",
            display_name: String::from("Ollama (local)"),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::None,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "opencode" => ProviderDefinition {
            id: "opencode",
            display_name: String::from("OpenCode Zen"),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        // Free-tier port of OpenCode's own unauthenticated `opencode`
        // provider. The two feature states are genuinely different products:
        //
        // - OFF (shipped default): the **Go** relay sibling, with its own
        //   static catalog and a required key. Byte-identical to before.
        // - ON: targets the **Zen** relay and ships the cost-aware Zen catalog
        //   (free-ness is `cost.input == 0`). The anonymous `Bearer public`
        //   credential this mode used to send was REMOVED (2026-09-30): live
        //   testing proved the relay refuses it server-side (`403
        //   FreeTierError` / `429`). A real key is now required exactly like
        //   `opencode`; the supported route to OpenCode's free models is the
        //   `opencode-local` type. The keyless picker still lists only
        //   zero-cost models.
        #[cfg(not(feature = "opencode-free-tier"))]
        "opencode-free" => ProviderDefinition {
            id: "opencode-free",
            display_name: String::from("OpenCode Zen (free)"),
            // `minimax-m3` takes the Anthropic Messages `/messages` dialect on
            // the Go relay (`minimax-` is an Anthropic prefix in the upstream
            // Go table), escapes the weak-tool-calling heuristic — it
            // tokenizes to `["minimax","m3"]`, and the heuristic only matches
            // whole `mimo`/`mini` tokens (ADR-66 §5 token bounding) — and
            // resolves tool-capable on that path, so it is usable as a
            // tool-requiring default. Ranking it against the other 42 ids
            // would need live probing; name resemblance is never used for
            // capability decisions.
            default_model: Some("minimax-m3"),
            known_models: OPENCODE_GO_KNOWN,
            // The Go relay answers `401 AuthError "Missing API key."` when no
            // `Authorization` header is sent, so this type requires a real
            // credential exactly like `opencode`; it is not keyless.
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            // Live discovery is intentionally unsupported: `GET
            // https://opencode.ai/zen/go/v1/models` is ungated and returns the
            // entire paid roster too, so advertising discovery here would let
            // paid IDs leak into a free-only picker.
            model_discovery: ModelDiscoverySupport::Unsupported,
            allows_custom_model: true,
        },
        #[cfg(feature = "opencode-free-tier")]
        "opencode-free" => ProviderDefinition {
            id: "opencode-free",
            display_name: String::from("OpenCode Zen (free)"),
            // `space-bunny-free` takes the OpenAI-compatible
            // `/chat/completions` dialect on Zen and is a zero-cost catalog
            // entry. It is the default for its cost and dialect, never for its
            // name (name-based free-ness is the regression this repo has fixed
            // twice).
            default_model: Some("space-bunny-free"),
            // The ids come from the cost-aware catalog so the keyed picker can
            // still reach the paid roster; the keyless picker filters it.
            known_models: &[],
            known_model_costs: OPENCODE_FREE_KNOWN,
            // A real key is required: the anonymous `Bearer public` path was
            // removed after live testing showed the relay refuses it. The
            // keyless picker filter still shows only zero-cost models before a
            // key is entered.
            credential_requirement: CredentialRequirement::Required,
            // Discovery stays off: the Zen `/models` endpoint is ungated and
            // would leak paid ids into the keyless picker.
            model_discovery: ModelDiscoverySupport::Unsupported,
            allows_custom_model: true,
        },
        // A local `opencode serve` instance. Unlike the two relay types this
        // one is not an HTTP relay to a hosted service: it is a first-class
        // provider over the server's own API. The model catalog is discovered
        // from `GET /provider` and filtered to `cost.input == 0` (free-ness is
        // COST, never a name suffix), so no static list ships here. The server
        // password is a real credential (the API answers 401 without it), so
        // the requirement is `Required`; discovery is `Supported`.
        "opencode-local" => ProviderDefinition {
            id: "opencode-local",
            display_name: String::from("OpenCode (local server)"),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "deepseek" => ProviderDefinition {
            id: "deepseek",
            display_name: String::from("DeepSeek"),
            default_model: Some("deepseek-chat"),
            known_models: DEEPSEEK_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "groq" => ProviderDefinition {
            id: "groq",
            display_name: String::from("Groq"),
            default_model: Some("llama-3.3-70b-versatile"),
            known_models: GROQ_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "together" => ProviderDefinition {
            id: "together",
            display_name: String::from("Together AI"),
            default_model: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo"),
            known_models: TOGETHER_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "mistral" => ProviderDefinition {
            id: "mistral",
            display_name: String::from("Mistral"),
            default_model: Some("mistral-large-latest"),
            known_models: MISTRAL_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "xai" => ProviderDefinition {
            id: "xai",
            display_name: String::from("xAI"),
            default_model: Some("grok-4"),
            known_models: XAI_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "fireworks" => ProviderDefinition {
            id: "fireworks",
            display_name: String::from("Fireworks"),
            default_model: Some("accounts/fireworks/models/llama-v3p3-70b-instruct"),
            known_models: FIREWORKS_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "cerebras" => ProviderDefinition {
            id: "cerebras",
            display_name: String::from("Cerebras"),
            default_model: Some("llama-3.3-70b"),
            known_models: CEREBRAS_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "cohere" => ProviderDefinition {
            id: "cohere",
            display_name: String::from("Cohere"),
            default_model: Some("command-a-plus-05-2026"),
            known_models: COHERE_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "deepinfra" => ProviderDefinition {
            id: "deepinfra",
            display_name: String::from("DeepInfra"),
            default_model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo"),
            known_models: DEEPINFRA_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "perplexity" => ProviderDefinition {
            id: "perplexity",
            display_name: String::from("Perplexity"),
            default_model: Some("sonar-pro"),
            known_models: PERPLEXITY_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "sambanova" => ProviderDefinition {
            id: "sambanova",
            display_name: String::from("SambaNova"),
            default_model: Some("Meta-Llama-3.3-70B-Instruct"),
            known_models: SAMBANOVA_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "dashscope" => ProviderDefinition {
            id: "dashscope",
            display_name: String::from("Alibaba (Qwen)"),
            default_model: Some("qwen-plus"),
            known_models: DASHSCOPE_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "moonshot" => ProviderDefinition {
            id: "moonshot",
            display_name: String::from("Moonshot (Kimi)"),
            default_model: Some("kimi-k2.6"),
            known_models: MOONSHOT_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "zhipu" => ProviderDefinition {
            id: "zhipu",
            display_name: String::from("Zhipu (GLM)"),
            default_model: Some("glm-4.7"),
            known_models: ZHIPU_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "novita" => ProviderDefinition {
            id: "novita",
            display_name: String::from("Novita AI"),
            default_model: None,
            known_models: NOVITA_KNOWN,
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        _ => ProviderDefinition {
            id: "<unknown>",
            display_name: provider_type.to_string(),
            default_model: None,
            known_models: &[],
            known_model_costs: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Unsupported,
            allows_custom_model: true,
        },
    }
}

/// Recognized provider type ids, in UI display order.
pub const PROVIDER_TYPE_IDS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "openrouter",
    "nim",
    "ollama",
    "opencode",
    "opencode-free",
    "opencode-local",
    "deepseek",
    "groq",
    "together",
    "mistral",
    "xai",
    "fireworks",
    "cerebras",
    "cohere",
    "deepinfra",
    "perplexity",
    "sambanova",
    "dashscope",
    "moonshot",
    "zhipu",
    "novita",
];

/// Discovered model catalog for a provider.
///
/// Kept separate from [`ProviderConfig`]: it is transient, endpoint-scoped
/// discovery data, not user intent. Persisted in the application cache area
/// (Phase 3), keyed by stable provider id and scoped to the actual endpoint
/// via [`ProviderModelCache::api_base_fingerprint`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderModelCache {
    pub provider_id: String,
    pub provider_type: String,
    /// Cheap, non-cryptographic fingerprint of the endpoint this cache came from,
    /// so a cache fetched from one custom endpoint is not silently reused after
    /// the endpoint changes.
    pub api_base_fingerprint: String,
    pub models: Vec<String>,
    pub fetched_at_unix: i64,
}

impl ProviderModelCache {
    /// Compute the endpoint fingerprint used to scope a cache to an API base.
    pub fn fingerprint(api_base: &Option<String>) -> String {
        match api_base {
            Some(base) if !base.trim().is_empty() => format!("v1:{}", base.trim()),
            _ => "v1:<default>".to_string(),
        }
    }

    /// Normalize a discovered model list: trim, drop empties, dedupe, sort
    /// case-insensitively, and cap at [`MAX_CACHED_MODELS`].
    ///
    /// Returns `(models, truncated)` where `truncated` is true when the input
    /// exceeded the cap.
    pub fn normalize(models: Vec<String>) -> (Vec<String>, bool) {
        let mut seen: HashSet<String> = HashSet::with_capacity(models.len());
        let mut out: Vec<String> = Vec::new();
        for m in models {
            let t = m.trim().to_string();
            if t.is_empty() {
                continue;
            }
            if seen.insert(t.to_lowercase()) {
                out.push(t);
            }
        }
        out.sort_by_key(|s| s.to_lowercase());
        let truncated = out.len() > MAX_CACHED_MODELS;
        if truncated {
            out.truncate(MAX_CACHED_MODELS);
        }
        (out, truncated)
    }
}

/// Insert a model into `list` (deduplicated, case-insensitive) unless empty.
fn push_unique(list: &mut Vec<String>, seen: &mut HashSet<String>, model: &str) {
    let t = model.trim().to_string();
    if t.is_empty() {
        return;
    }
    if seen.insert(t.to_lowercase()) {
        list.push(t);
    }
}

/// Build the ordered, deduplicated list of selectable model IDs for a provider.
///
/// Priority (pinned/current choices first, then case-insensitive sorted
/// remainder):
/// 1. Currently selected model (`provider.model`).
/// 2. Provider default model (from `definition`).
/// 3. Static known models (from `definition`).
/// 4. Successfully discovered cached models (from `cache`).
///
/// Rules:
/// - Trim whitespace, drop empties.
/// - Deduplicate exact IDs (case-insensitive).
/// - Preserve the selected model even when it is no longer advertised.
/// - Do not replace static choices merely because a cache exists.
/// - Do not mutate the persisted selection while merely building options.
pub fn model_options_for(
    provider: &ProviderConfig,
    definition: &ProviderDefinition,
    cache: Option<&ProviderModelCache>,
) -> Vec<String> {
    let mut pinned: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Selected model — preserved even if unadvertised.
    let selected = provider.model.trim().to_string();
    if !selected.is_empty() {
        pinned.push(selected.clone());
        seen.insert(selected.to_lowercase());
    }

    // 2. Provider default model.
    if let Some(dm) = definition.default_model {
        push_unique(&mut pinned, &mut seen, dm);
    }

    // 3. Static known models (plain catalog + cost-aware catalog).
    for m in definition.known_model_ids() {
        push_unique(&mut rest, &mut seen, m);
    }

    // 4. Discovered cached models.
    if let Some(cache) = cache {
        for m in &cache.models {
            push_unique(&mut rest, &mut seen, m);
        }
    }

    rest.sort_by_key(|s| s.to_lowercase());
    pinned.append(&mut rest);
    pinned
}

/// Build the picker-facing model list for a provider config: the shared
/// [`model_options_for`] resolution merged with the provider's *additive
/// advertising* candidates — `cached_models` (live discovery) and
/// `extra_models` (config-first gateways).
///
/// This is the single resolver every desktop model picker (Settings provider
/// rows, the global default picker, the chat header) should call, so a model
/// only has to be advertised in one place to become selectable. It is pure —
/// no I/O, no credential lookups — and never mutates the persisted selection.
///
/// `extra_models` entries are model names, not provider ids: plugin-backed
/// providers stay run-only and are intentionally absent from the provider-type
/// picker ([`PROVIDER_TYPE_IDS`]).
pub fn picker_model_options(provider: &ProviderConfig) -> Vec<String> {
    // Historical callers have no credential context; `true` preserves the
    // pre-feature (unfiltered) behaviour exactly.
    picker_model_options_for(provider, true)
}

/// Credential-aware picker resolver: the single merge point for selected,
/// default, static, config-first `extra_models`, and discovered `cached_models`.
///
/// `credential_present` tells the resolver whether a usable key exists. In
/// keyless free-tier mode (feature `opencode-free-tier`, provider
/// `opencode-free`, no key) the returned list is filtered to models the
/// catalog classifies `free == true` — mirroring the upstream client deleting
/// every model with `cost.input !== 0`. Filtering happens here, after the
/// merge, so `extra_models` and discovered `cached_models` are covered too: a
/// discovered paid id cannot leak into a keyless picker.
///
/// With a key present — or for any other provider, or with the feature off —
/// the full merged list is returned, byte-identical to before.
pub fn picker_model_options_for(
    provider: &ProviderConfig,
    credential_present: bool,
) -> Vec<String> {
    let definition = provider_definition(&provider.provider);
    let mut options = model_options_for(provider, &definition, None);
    let mut seen: HashSet<String> = options.iter().map(|m| m.to_lowercase()).collect();
    // Config-declared `extra_models` come before discovered `cached_models`, so
    // the user's spelling wins when the two advertise the same id.
    for model in provider.extra_models.iter().chain(provider.cached_models.iter()) {
        push_unique(&mut options, &mut seen, model);
    }
    apply_free_tier_filter(provider, &definition, credential_present, options)
}

/// Keyless free-tier picker gate, compiled in with `opencode-free-tier`.
#[cfg(feature = "opencode-free-tier")]
fn apply_free_tier_filter(
    provider: &ProviderConfig,
    definition: &ProviderDefinition,
    credential_present: bool,
    options: Vec<String>,
) -> Vec<String> {
    if provider.provider == "opencode-free" && !credential_present {
        return options
            .into_iter()
            .filter(|model| definition.known_model_is_free(model) == Some(true))
            .collect();
    }
    options
}

/// Feature-off no-op: the resolver is byte-identical to before.
#[cfg(not(feature = "opencode-free-tier"))]
fn apply_free_tier_filter(
    _provider: &ProviderConfig,
    _definition: &ProviderDefinition,
    _credential_present: bool,
    options: Vec<String>,
) -> Vec<String> {
    options
}

/// Why a provider cannot yet be used for dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderReadiness {
    Ready,
    MissingRequiredCredential,
    InvalidEndpoint(String),
}

impl ProviderReadiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, ProviderReadiness::Ready)
    }

    /// Human-readable setup guidance for the UI. `None` when ready.
    pub fn setup_message(&self) -> Option<&'static str> {
        match self {
            ProviderReadiness::Ready => None,
            ProviderReadiness::MissingRequiredCredential => Some("Add an API key to finish setup."),
            ProviderReadiness::InvalidEndpoint(_) => {
                Some("API base URL is not a valid http(s) URL.")
            }
        }
    }
}

/// Whether an `api_base` value (when present and non-empty) is a usable http(s) URL.
fn is_valid_endpoint(api_base: &Option<String>) -> bool {
    match api_base {
        Some(base) => {
            let b = base.trim();
            b.is_empty()
                || ((b.starts_with("http://") || b.starts_with("https://"))
                    && b.len() > "https://".len())
        }
        None => true,
    }
}

/// Validate whether a provider is ready for dispatch.
///
/// `credential_present` reflects whether a credential is currently stored for
/// the provider's keyring key. Endpoint validity is checked only when an
/// `api_base` is supplied.
pub fn provider_readiness(
    provider: &ProviderConfig,
    definition: &ProviderDefinition,
    credential_present: bool,
) -> ProviderReadiness {
    // Credential requirement. Models are assigned per agent role (not stored on
    // the provider), so a missing model no longer blocks provider readiness.
    if definition.requires_credential() && !credential_present {
        return ProviderReadiness::MissingRequiredCredential;
    }

    // Endpoint validity (only when supplied).
    if !is_valid_endpoint(&provider.api_base) {
        let bad = provider.api_base.clone().unwrap_or_default().trim().to_string();
        return ProviderReadiness::InvalidEndpoint(bad);
    }

    ProviderReadiness::Ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::ProviderConfig;

    fn pc(
        provider: &str,
        model: &str,
        api_base: Option<&str>,
        keyring_key: &str,
    ) -> ProviderConfig {
        ProviderConfig {
            id: "p1".into(),
            name: "Test".into(),
            provider: provider.into(),
            model: model.into(),
            api_base: api_base.map(|s| s.to_string()),
            timeout_seconds: 30,
            cached_models: Default::default(),
            cached_models_fetched_at: 0,
            keyring_key: keyring_key.into(),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn known_provider_definitions_are_complete() {
        for id in PROVIDER_TYPE_IDS {
            let def = provider_definition(id);
            assert_eq!(def.id, *id);
            assert!(!def.display_name.is_empty());
        }
    }

    /// The feature-off `opencode-free` type is the **Go** relay sibling: its
    /// static Go catalog, a required credential (the Go relay answers
    /// `401 AuthError` without one), discovery disabled, custom models
    /// allowed, and an OpenAI-compat default model (see the arm's comments).
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn opencode_free_definition_is_complete() {
        let def = provider_definition("opencode-free");
        assert_eq!(def.id, "opencode-free");
        assert_eq!(def.display_name, "OpenCode Zen (free)");
        assert_eq!(def.default_model, Some("minimax-m3"));
        // Inverted from the old keyless assertion: the Go relay requires a
        // real API key, so this type is no more keyless than `opencode`.
        assert_eq!(def.credential_requirement, CredentialRequirement::Required);
        assert_eq!(def.model_discovery, ModelDiscoverySupport::Unsupported);
        assert!(def.allows_custom_model);

        // The 43 ids served by `GET https://opencode.ai/zen/go/v1/models`
        // (2026-09-29), quoted verbatim in alphabetical order: this list is
        // the contract for the Go roster, not Zen's — a wrong id here is a
        // silent 404 at request time, so it is pinned byte for byte.
        let expected = [
            "deepseek-flash",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
            "deepseek-v4-pro",
            "deepseek-v4.1-flash",
            "glm-5",
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "glm-5.3-flash",
            "gpt-5.6-luna",
            "gpt-6-luna",
            "grok-4.5",
            "grok-4.6",
            "grok-4.7",
            "hy3",
            "hy3-preview",
            "hy4-preview",
            "kimi-k2.5",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "kimi-k3",
            "longcat-2.0",
            "longcat-2.5-preview-free",
            "mimo-v2-omni",
            "mimo-v2-pro",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "mimo-v2.6-flash",
            "mimo-v2.6-pro",
            "minimax-m2.5",
            "minimax-m2.7",
            "minimax-m3",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3-contributor",
            "omen-alpha",
            "qwen3.5-plus",
            "qwen3.6-plus",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-flash",
            "qwen3.8-max",
            "space-bunny-free",
        ];
        assert_eq!(
            def.known_models,
            expected.as_slice(),
            "the Go catalog must ship exactly the 43 live-retrieved IDs"
        );
        assert_eq!(expected.len(), 43, "the Go relay advertises exactly 43 models");
    }

    /// The feature-on `opencode-free` type is the Zen relay sibling with the
    /// cost-aware catalog and a **required** credential: the anonymous
    /// `Bearer public` path was removed after live testing proved the relay
    /// refuses it server-side. Discovery stays disabled and the default is a
    /// zero-cost Chat Completions model.
    #[cfg(feature = "opencode-free-tier")]
    #[test]
    fn opencode_free_definition_is_complete() {
        let def = provider_definition("opencode-free");
        assert_eq!(def.id, "opencode-free");
        assert_eq!(def.display_name, "OpenCode Zen (free)");
        assert_eq!(def.default_model, Some("space-bunny-free"));
        // A real key is required: the anonymous path was removed.
        assert_eq!(def.credential_requirement, CredentialRequirement::Required);
        assert_eq!(def.model_discovery, ModelDiscoverySupport::Unsupported);
        assert!(def.allows_custom_model);
        // The full 114-model Zen catalog ships, with 34 zero-cost models.
        assert_eq!(def.known_model_costs.len(), 114, "the full Zen catalog ships");
        assert_eq!(def.known_models.len(), 0, "ids come from the cost catalog");
        let free_count = def.known_model_costs.iter().filter(|model| model.free).count();
        assert_eq!(free_count, 34, "the catalog has exactly 34 cost.input == 0 models");
    }

    /// The `opencode-local` type is a first-class, key-required,
    /// discovery-supported provider over a local `opencode serve` instance.
    /// No static catalog ships: the free list is discovered from `/provider`.
    #[test]
    fn opencode_local_definition_is_complete() {
        let def = provider_definition("opencode-local");
        assert_eq!(def.id, "opencode-local");
        assert_eq!(def.display_name, "OpenCode (local server)");
        assert_eq!(def.default_model, None, "the catalog is discovered, not hard-coded");
        assert!(def.known_models.is_empty());
        assert!(def.known_model_costs.is_empty());
        // The server password is a real credential; the API answers 401
        // without it.
        assert_eq!(def.credential_requirement, CredentialRequirement::Required);
        assert_eq!(def.model_discovery, ModelDiscoverySupport::Supported);
        assert!(def.allows_custom_model);
    }

    /// `opencode-local` is registered in the picker type list next to the
    /// other OpenCode types, and `opencode-free` stays immediately after
    /// `opencode`.
    #[test]
    fn opencode_local_is_registered_next_to_opencode() {
        let position = |id: &str| {
            PROVIDER_TYPE_IDS
                .iter()
                .position(|candidate| *candidate == id)
                .unwrap_or_else(|| panic!("{id} must be registered in PROVIDER_TYPE_IDS"))
        };
        assert_eq!(position("opencode-local"), position("opencode-free") + 1);
        assert_eq!(position("opencode-free"), position("opencode") + 1);
    }

    /// The keyless picker lists discovered free models for `opencode-local`:
    /// the connector's `list_models` returns only zero-cost entries and the
    /// picker merges `cached_models` additively.
    #[test]
    fn picker_lists_discovered_free_models_for_opencode_local() {
        let mut provider =
            pc("opencode-local", "big-pickle", None, "opencode-local/server_password");
        provider.cached_models = vec!["big-pickle".into(), "space-bunny-free".into()];
        let options = picker_model_options(&provider);
        assert!(options.contains(&"big-pickle".to_string()));
        assert!(options.contains(&"space-bunny-free".to_string()));
    }

    /// Feature-on: the default is a free, tool-capable Chat Completions model
    /// on the Zen relay.
    #[cfg(feature = "opencode-free-tier")]
    #[test]
    fn opencode_free_default_model_is_usable() {
        use crate::opencode::{api_mode_for, ApiMode, OpenCodeRelay};

        let def = provider_definition("opencode-free");
        let default_model = def.default_model.expect("opencode-free ships a default model");
        assert_eq!(def.known_model_is_free(default_model), Some(true), "the default must be free");
        assert_eq!(
            api_mode_for(OpenCodeRelay::Zen, default_model),
            ApiMode::ChatCompletions,
            "the default rides the OpenAI-compatible Chat Completions dialect on Zen"
        );
        assert!(
            crate::capability::require_tool_support("opencode-free", default_model, None, None)
                .is_ok(),
            "the default must pass the tool-support gate"
        );
    }

    /// The default model is real, routable, and usable for tool-requiring
    /// tasks: it must exist in the catalog, take exactly the dialect the
    /// upstream Go table assigns it (`minimax-*` → Anthropic Messages), and
    /// escape the weak-tier heuristic so its default schema tier is strict.
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn opencode_free_default_model_is_usable() {
        use crate::opencode::{api_mode_for, ApiMode, OpenCodeRelay};

        let def = provider_definition("opencode-free");
        let default_model = def.default_model.expect("opencode-free ships a default model");
        assert!(
            OPENCODE_GO_KNOWN.contains(&default_model),
            "the default `{default_model}` must be part of the Go catalog"
        );
        assert_eq!(
            api_mode_for(OpenCodeRelay::Go, default_model),
            ApiMode::AnthropicMessages,
            "on the Go relay `minimax-*` is served via the Anthropic Messages dialect"
        );
        assert!(
            !crate::adapters::schema_loose::last_resort_weak_tool_calling_model(default_model),
            "the default must not fall to the weak tool-calling tier"
        );
        assert_eq!(
            crate::capability::resolve_tool_schema_mode(
                "opencode-free",
                default_model,
                concerto_config::ToolSchemaMode::default(),
                None,
            ),
            concerto_config::ToolSchemaMode::Strict,
            "an unknown-but-not-weak model defaults to the strict schema tier"
        );
        assert!(
            crate::capability::require_tool_support("opencode-free", default_model, None, None)
                .is_ok(),
            "the default must pass the tool-support gate"
        );
    }

    /// Prints the Go catalog verbatim (run with `--nocapture`) so a reviewer
    /// can diff it against `GET https://opencode.ai/zen/go/v1/models` without
    /// re-probing the network — the same 43 ids the previous test pins.
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn opencode_free_catalog_prints_verbatim_for_review() {
        println!("OPENCODE_GO_KNOWN ({} ids):", OPENCODE_GO_KNOWN.len());
        for id in OPENCODE_GO_KNOWN {
            println!("{id}");
        }
    }

    /// Prints the cost-aware Zen catalog (id + free flag) for review.
    #[cfg(feature = "opencode-free-tier")]
    #[test]
    fn opencode_free_catalog_prints_verbatim_for_review() {
        println!("OPENCODE_FREE_KNOWN ({} ids):", OPENCODE_FREE_KNOWN.len());
        for model in OPENCODE_FREE_KNOWN {
            println!("{}{}", model.id, if model.free { "  [free]" } else { "" });
        }
    }

    /// `opencode-free` is a first-class picker entry, registered immediately
    /// after the paid `opencode` entry.
    #[test]
    fn opencode_free_is_registered_immediately_after_opencode() {
        let position = |id: &str| {
            PROVIDER_TYPE_IDS
                .iter()
                .position(|candidate| *candidate == id)
                .unwrap_or_else(|| panic!("{id} must be registered in PROVIDER_TYPE_IDS"))
        };
        assert_eq!(position("opencode-free"), position("opencode") + 1);
    }

    /// Pin the Go catalog's wire routing against the upstream `opencode-go`
    /// prefix table and confirm every id stays tool-capable on its dialect
    /// (the Responses converter carries native tools since ADR-75).
    ///
    /// This test was updated from the old assertion that *no* Go id routes to
    /// Anthropic: the upstream Go table sends `minimax-*`/`qwen*` to
    /// `/messages` and `gpt-*`/`grok-*`/`muse-spark*` to `/responses`.
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn opencode_free_catalog_routing_is_pinned() {
        use crate::opencode::{api_mode_for, ApiMode, OpenCodeRelay};

        let mut responses: Vec<&str> = Vec::new();
        let mut anthropic: Vec<&str> = Vec::new();
        let mut chat: Vec<&str> = Vec::new();
        for model in OPENCODE_GO_KNOWN {
            match api_mode_for(OpenCodeRelay::Go, model) {
                ApiMode::Responses => responses.push(*model),
                ApiMode::AnthropicMessages => anthropic.push(*model),
                ApiMode::ChatCompletions => chat.push(*model),
            }
            assert!(
                crate::capability::resolve_tool_support("opencode-free", model, None, None),
                "{model} must not be capability-blocked for tool calling"
            );
            assert!(
                crate::capability::require_tool_support("opencode-free", model, None, None).is_ok(),
                "{model} must pass the tool-support gate regardless of wire dialect"
            );
        }

        assert_eq!(
            responses,
            [
                "gpt-5.6-luna",
                "gpt-6-luna",
                "grok-4.5",
                "grok-4.6",
                "grok-4.7",
                "muse-spark-1.2-contributor",
                "muse-spark-1.3-contributor",
            ],
            "exactly the gpt-/grok-/muse-spark Go ids ride the Responses dialect"
        );
        assert_eq!(
            anthropic,
            [
                "minimax-m2.5",
                "minimax-m2.7",
                "minimax-m3",
                "qwen3.5-plus",
                "qwen3.6-plus",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.8-flash",
                "qwen3.8-max",
            ],
            "exactly the minimax-/qwen Go ids ride the Anthropic Messages dialect"
        );
        assert_eq!(chat.len(), 27, "every other Go id rides Chat Completions");
        assert_eq!(
            responses.len() + anthropic.len() + chat.len(),
            OPENCODE_GO_KNOWN.len(),
            "each id lands in exactly one dialect bucket"
        );
        // `omen-alpha` is a live-roster near-miss of `union-alpha`; upstream
        // lists only `union-alpha`, so `omen-alpha` stays on Chat Completions.
        assert!(chat.contains(&"omen-alpha"), "omen-alpha must not be Anthropic");
    }

    /// The feature-on catalog is the real Zen cost catalog: the eight named
    /// free models are present and classified `free`, and the classifier is
    /// cost-based, not name-based (`big-pickle`/`grok-code` are free with no
    /// `free` marker; a synthetic paid model whose name says `free` is not).
    #[cfg(feature = "opencode-free-tier")]
    #[test]
    fn opencode_free_zen_catalog_is_cost_classified() {
        let free_ids: Vec<&str> =
            OPENCODE_FREE_KNOWN.iter().filter(|model| model.free).map(|model| model.id).collect();
        for id in [
            "big-pickle",
            "ling-3.0-flash-fin-free",
            "longcat-2.5-preview-free",
            "mimo-v2.6-flash-free",
            "muse-spark-1.3-contributor-free",
            "nemotron-3-ultra-free",
            "nemotron-3.5-lightning-free",
            "space-bunny-free",
        ] {
            assert!(free_ids.contains(&id), "{id} must be present and classified free");
        }

        let def = provider_definition("opencode-free");
        // Cost-based, not name-based: a model with no `free` marker is free
        // because its catalog cost is zero…
        assert_eq!(def.known_model_is_free("grok-code"), Some(true));
        assert_eq!(def.known_model_is_free("big-pickle"), Some(true));
        // …and a name suffix is never enough: a synthetic record whose name
        // contains `free` but whose cost is non-zero is NOT free.
        let synthetic_paid = KnownModel { id: "definitely-free-model", free: false };
        assert!(!synthetic_paid.free, "a name containing `free` must not imply free");
        // A model absent from the catalog has no cost data and is treated as
        // not free by the picker (`None`, filtered out).
        assert_eq!(def.known_model_is_free("some-unknown-model"), None);
    }

    #[test]
    fn ollama_requires_no_credential() {
        assert_eq!(
            provider_definition("ollama").credential_requirement,
            CredentialRequirement::None
        );
    }

    #[test]
    fn deepseek_definition_is_complete() {
        let def = provider_definition("deepseek");
        assert_eq!(def.id, "deepseek");
        assert_eq!(def.default_model, Some("deepseek-chat"));
        assert!(def.known_models.contains(&"deepseek-reasoner"));
        assert_eq!(def.credential_requirement, CredentialRequirement::Required);
        assert!(def.allows_custom_model);
    }

    /// Every Tier-1 OpenAI-compatible provider is fully defined: key-required,
    /// discovery-supported, custom-model-allowed, with a default model and a
    /// hand-maintained catalog of real API model IDs.
    #[test]
    fn tier1_openai_compatible_definitions_are_complete() {
        for id in ["groq", "together", "mistral", "xai", "fireworks", "cerebras", "cohere"] {
            let def = provider_definition(id);
            assert_eq!(def.id, id);
            assert_eq!(def.credential_requirement, CredentialRequirement::Required);
            assert_eq!(def.model_discovery, ModelDiscoverySupport::Supported);
            assert!(def.allows_custom_model);
            assert!(def.default_model.is_some(), "{id} must have a default model");
            assert!(!def.known_models.is_empty(), "{id} must ship a known-model catalog");
            assert!(
                def.default_model.is_some_and(|default| def.known_models.contains(&default)),
                "{id} default model must be part of its known-model catalog"
            );
        }
    }

    /// Every Tier-2 OpenAI-compatible provider is fully defined: key-required,
    /// discovery-supported, custom-model-allowed. All but the
    /// discovery-driven Novita ship a default model and a hand-maintained
    /// catalog of real API model IDs.
    #[test]
    fn tier2_openai_compatible_definitions_are_complete() {
        for id in ["deepinfra", "perplexity", "sambanova", "dashscope", "moonshot", "zhipu"] {
            let def = provider_definition(id);
            assert_eq!(def.id, id);
            assert_eq!(def.credential_requirement, CredentialRequirement::Required);
            assert_eq!(def.model_discovery, ModelDiscoverySupport::Supported);
            assert!(def.allows_custom_model);
            assert!(def.default_model.is_some(), "{id} must have a default model");
            assert!(!def.known_models.is_empty(), "{id} must ship a known-model catalog");
            assert!(
                def.default_model.is_some_and(|default| def.known_models.contains(&default)),
                "{id} default model must be part of its known-model catalog"
            );
        }
        // Novita: discovery-driven — key-required and discovery-supported, but
        // no static catalog or default model (no speculative IDs shipped).
        let novita = provider_definition("novita");
        assert_eq!(novita.id, "novita");
        assert_eq!(novita.credential_requirement, CredentialRequirement::Required);
        assert_eq!(novita.model_discovery, ModelDiscoverySupport::Supported);
        assert!(novita.allows_custom_model);
        assert!(novita.default_model.is_none());
        assert!(novita.known_models.is_empty());
    }

    #[test]
    fn unknown_provider_falls_back_permissively() {
        let def = provider_definition("brand-new-provider");
        assert_eq!(def.id, "<unknown>");
        assert_eq!(def.model_discovery, ModelDiscoverySupport::Unsupported);
    }

    // ---- model_options_for --------------------------------------------------

    #[test]
    fn static_catalog_used_with_no_cache() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        let opts = model_options_for(&p, &provider_definition("openai"), None);
        assert!(opts.contains(&"gpt-4o".to_string()));
        assert!(opts.contains(&"gpt-4o-mini".to_string()));
        assert!(opts.contains(&"gpt-3.5-turbo".to_string()));
        // selected first
        assert_eq!(opts[0], "gpt-4o");
    }

    #[test]
    fn cached_models_are_merged_not_replaced() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        let cache = ProviderModelCache {
            provider_id: "p1".into(),
            provider_type: "openai".into(),
            api_base_fingerprint: ProviderModelCache::fingerprint(&None),
            models: vec!["custom-discovered-1".into(), "gpt-4o".into()],
            fetched_at_unix: 0,
        };
        let opts = model_options_for(&p, &provider_definition("openai"), Some(&cache));
        // static present
        assert!(opts.contains(&"gpt-4o-mini".to_string()));
        // discovered present and not duplicated
        assert!(opts.contains(&"custom-discovered-1".to_string()));
        assert_eq!(
            opts.iter().filter(|m| *m == "gpt-4o").count(),
            1,
            "selected/cached overlap must not duplicate"
        );
    }

    #[test]
    fn selected_deprecated_model_remains_available() {
        // selected model no longer in static or discovered lists
        let p = pc("openai", "gpt-3.5-turbo-deprecated", None, "openai/api_key");
        let cache = ProviderModelCache {
            provider_id: "p1".into(),
            provider_type: "openai".into(),
            api_base_fingerprint: ProviderModelCache::fingerprint(&None),
            models: vec!["gpt-4o".into()],
            fetched_at_unix: 0,
        };
        let opts = model_options_for(&p, &provider_definition("openai"), Some(&cache));
        assert!(
            opts.contains(&"gpt-3.5-turbo-deprecated".to_string()),
            "selected (unadvertised) model must be preserved"
        );
        assert_eq!(opts[0], "gpt-3.5-turbo-deprecated");
    }

    #[test]
    fn empty_and_duplicate_values_removed() {
        let cache = ProviderModelCache {
            provider_id: "p1".into(),
            provider_type: "openai".into(),
            api_base_fingerprint: ProviderModelCache::fingerprint(&None),
            models: vec!["  ".into(), "DUPLICATE".into(), "duplicate".into(), "DUPLICATE".into()],
            fetched_at_unix: 0,
        };
        let p = pc("openai", "", None, "openai/api_key");
        let opts = model_options_for(&p, &provider_definition("openai"), Some(&cache));
        assert!(!opts.iter().any(|m| m.trim().is_empty()), "empty models must be dropped");
        assert_eq!(opts.iter().filter(|m| m.eq_ignore_ascii_case("duplicate")).count(), 1);
    }

    #[test]
    fn ordering_is_deterministic() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        let opts1 = model_options_for(&p, &provider_definition("openai"), None);
        let opts2 = model_options_for(&p, &provider_definition("openai"), None);
        assert_eq!(opts1, opts2);
    }

    // ---- picker_model_options -----------------------------------------------

    #[test]
    fn picker_options_merge_extra_and_cached_models() {
        let mut p = pc("openai", "gpt-4o", None, "openai/api_key");
        p.extra_models = vec!["gateway-model-a".into(), "  ".into(), "gpt-4o".into()];
        p.cached_models = vec!["discovered-model".into(), "GATEWAY-MODEL-A".into()];
        let opts = picker_model_options(&p);
        // Selected / static resolver output survives.
        assert_eq!(opts[0], "gpt-4o");
        assert!(opts.contains(&"gpt-4o-mini".to_string()));
        // Additive advertising candidates become selectable.
        assert!(opts.contains(&"gateway-model-a".to_string()));
        assert!(opts.contains(&"discovered-model".to_string()));
        // Trimmed, de-duplicated (case-insensitively), empties dropped.
        assert_eq!(opts.iter().filter(|m| m.eq_ignore_ascii_case("gateway-model-a")).count(), 1);
        assert!(!opts.iter().any(|m| m.trim().is_empty()));
    }

    #[test]
    fn picker_options_with_no_additions_match_resolver() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        assert_eq!(
            picker_model_options(&p),
            model_options_for(&p, &provider_definition("openai"), None)
        );
    }

    /// The free-ness classifier is cost-only, never name-based.
    #[test]
    fn free_classification_is_cost_only() {
        assert!(is_free_cost(Some(0.0)), "cost.input == 0 is free");
        assert!(!is_free_cost(Some(0.0001)), "a non-zero cost is not free");
        assert!(!is_free_cost(Some(1.0)), "a positive cost is not free");
        assert!(!is_free_cost(None), "no cost data is NOT free");

        // A name containing `free` cannot make a non-zero-cost model free…
        let paid_with_free_name =
            KnownModel { id: "totally-free-model", free: is_free_cost(Some(2.5)) };
        assert!(!paid_with_free_name.free);
        // …and a zero-cost model with no `free` marker IS free.
        let free_without_marker = KnownModel { id: "grok-code", free: is_free_cost(Some(0.0)) };
        assert!(free_without_marker.free);
    }

    /// Feature-on: keyless `opencode-free` lists only free models; keyed lists
    /// everything; discovered paid ids cannot leak into the keyless list.
    #[cfg(feature = "opencode-free-tier")]
    #[test]
    fn keyless_free_tier_picker_lists_only_free_models() {
        let mut p = pc("opencode-free", "space-bunny-free", None, "opencode-free/api_key");
        let def = provider_definition("opencode-free");

        let keyless = picker_model_options_for(&p, false);
        assert!(keyless.contains(&"space-bunny-free".to_string()), "the free default is listed");
        assert!(!keyless.contains(&"claude-opus-5".to_string()), "a paid model is hidden");
        assert!(
            keyless.iter().all(|m| def.known_model_is_free(m) == Some(true)),
            "keyless picker leaked a non-free model: {keyless:?}"
        );

        // Keyed mode lists the full catalog, including paid ids.
        let keyed = picker_model_options_for(&p, true);
        assert!(keyed.contains(&"claude-opus-5".to_string()), "keyed mode lists paid models");
        assert!(keyed.contains(&"space-bunny-free".to_string()));
        assert!(keyed.len() > keyless.len(), "keyed mode is a superset");

        // A discovered paid id must not reintroduce itself into the keyless
        // picker; an unknown id (no cost data) must not either.
        p.cached_models = vec!["claude-opus-5".into(), "some-custom-paid".into()];
        let keyless = picker_model_options_for(&p, false);
        assert!(!keyless.contains(&"claude-opus-5".to_string()), "discovered paid id leaked");
        assert!(!keyless.contains(&"some-custom-paid".to_string()), "unknown id leaked");
        let keyed = picker_model_options_for(&p, true);
        assert!(keyed.contains(&"claude-opus-5".to_string()), "keyed mode still lists it");
    }

    /// Feature-off: the credential argument is ignored and the resolver is
    /// byte-identical to [`picker_model_options`].
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn picker_ignores_credential_when_feature_off() {
        let p = pc("opencode-free", "minimax-m3", None, "opencode-free/api_key");
        assert_eq!(picker_model_options_for(&p, false), picker_model_options(&p));
        assert_eq!(picker_model_options_for(&p, true), picker_model_options(&p));
    }

    #[test]
    fn large_discovery_results_are_capped_and_flagged() {
        let models: Vec<String> =
            (0..(MAX_CACHED_MODELS + 50)).map(|i| format!("model-{i}")).collect();
        let (norm, truncated) = ProviderModelCache::normalize(models);
        assert!(truncated);
        assert_eq!(norm.len(), MAX_CACHED_MODELS);
    }

    // ---- provider_readiness -------------------------------------------------

    #[test]
    fn missing_model_does_not_block_readiness() {
        // Models are assigned per agent role, so a provider with no model of its
        // own is still ready to dispatch once a role picks a model.
        let p = pc("openai", "", None, "openai/api_key");
        assert_eq!(
            provider_readiness(&p, &provider_definition("openai"), true),
            ProviderReadiness::Ready
        );
    }

    #[test]
    fn missing_required_credential_blocks_readiness() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        assert_eq!(
            provider_readiness(&p, &provider_definition("openai"), false),
            ProviderReadiness::MissingRequiredCredential
        );
    }

    #[test]
    fn ollama_without_key_is_ready() {
        let p = pc("ollama", "llama3", Some("http://localhost:11434"), "ollama/api_key");
        assert_eq!(
            provider_readiness(&p, &provider_definition("ollama"), false),
            ProviderReadiness::Ready
        );
    }

    #[test]
    fn invalid_endpoint_reported() {
        let p = pc("openai", "gpt-4o", Some("not-a-url"), "openai/api_key");
        assert_eq!(
            provider_readiness(&p, &provider_definition("openai"), true),
            ProviderReadiness::InvalidEndpoint("not-a-url".into())
        );
    }

    #[test]
    fn ready_provider_passes() {
        let p = pc("openai", "gpt-4o", None, "openai/api_key");
        assert_eq!(
            provider_readiness(&p, &provider_definition("openai"), true),
            ProviderReadiness::Ready
        );
    }
}
