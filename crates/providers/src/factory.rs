use std::collections::HashMap;
use std::sync::Arc;

use concerto_config::{
    parse_tool_schema_mode, CredentialStore, ModelSettings, ProviderConfig, ToolSchemaMode,
};
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::RoutingProfile;

use crate::anthropic::AnthropicProvider;
use crate::cerebras::CerebrasProvider;
use crate::cohere::CohereProvider;
use crate::context_guard::ContextGuardProvider;
use crate::dashscope::DashScopeProvider;
use crate::deepinfra::DeepInfraProvider;
use crate::deepseek::DeepSeekProvider;
use crate::fireworks::FireworksProvider;
use crate::google::GoogleProvider;
use crate::groq::GroqProvider;
use crate::mistral::MistralProvider;
use crate::moonshot::MoonshotProvider;
use crate::nim::NimProvider;
use crate::novita::NovitaProvider;
use crate::ollama::OllamaProvider;
use crate::openai::{OpenAiProvider, ReasoningEcho, UsageRequest};
use crate::opencode::OpenCodeZenProvider;
use crate::openrouter::OpenRouterProvider;
use crate::perplexity::PerplexityProvider;
use crate::sambanova::SambaNovaProvider;
use crate::together::TogetherProvider;
use crate::xai::XaiProvider;
use crate::zhipu::ZhipuProvider;

/// Resolve the `[providers.*] tool_schema_mode` dial for provider construction.
///
/// Lenient like the `reasoning_echo` dial: unset/empty/`"auto"` resolve to
/// [`ToolSchemaMode::Auto`] silently; any other unrecognized value warns and
/// falls back to `Auto` instead of failing the build, keeping configs
/// forward-compatible.
fn resolve_tool_schema_mode(config: &ProviderConfig) -> ToolSchemaMode {
    let raw = config.tool_schema_mode.as_deref();
    let mode = parse_tool_schema_mode(raw);
    if let Some(raw) = raw {
        let normalized = raw.trim().to_ascii_lowercase();
        if mode == ToolSchemaMode::Auto && !matches!(normalized.as_str(), "" | "auto") {
            tracing::warn!(
                provider = %config.provider,
                value = %raw,
                "unknown tool_schema_mode; falling back to \"auto\""
            );
        }
    }
    mode
}

/// Builds `Arc<dyn LlmProvider>` instances from config definitions.
pub struct ProviderFactory;

impl ProviderFactory {
    /// Return the stable runtime ID for a provider configuration.
    pub fn config_id(config: &ProviderConfig) -> String {
        if config.id.is_empty() {
            format!("prov_{}", config.provider)
        } else {
            config.id.clone()
        }
    }

    /// Whether one provider configuration advertises a model.
    ///
    /// A provider config offers a model when the name equals its primary
    /// `model`, appears in `extra_models` or `cached_models`, or is offered by
    /// the static catalog paths. `extra_models` is purely additive — it can
    /// never shadow the primary `model` (the primary always wins on exact
    /// match). This is the single offer predicate behind
    /// [`ProviderFactory::config_for_model`].
    pub fn config_offers_model(provider: &ProviderConfig, model: &str) -> bool {
        let definition = crate::provider_defs::provider_definition(&provider.provider);
        let mut options = crate::provider_defs::model_options_for(provider, &definition, None);
        options.extend(provider.cached_models.iter().cloned());
        options.extend(provider.extra_models.iter().cloned());
        options.iter().any(|candidate| candidate == model)
    }

    /// Resolve the provider configuration that advertises a model.
    ///
    /// See [`ProviderFactory::config_offers_model`] for the offer predicate.
    /// An existing route wins when it remains valid; otherwise configuration
    /// order is the deterministic tie-breaker for duplicate model IDs.
    pub fn config_for_model<'a>(
        settings: &'a ModelSettings,
        model: &str,
        preferred_provider_id: Option<&str>,
    ) -> Option<&'a ProviderConfig> {
        preferred_provider_id
            .and_then(|id| settings.providers.iter().find(|provider| provider.id == id))
            .filter(|provider| Self::config_offers_model(provider, model))
            .or_else(|| {
                settings
                    .providers
                    .iter()
                    .find(|provider| Self::config_offers_model(provider, model))
            })
    }

    /// Build a single provider from its config.
    ///
    /// Missing credentials and unknown provider types are configuration
    /// errors. Production execution must never silently substitute a mock
    /// model because that makes a failed setup look like a successful run.
    ///
    /// Provider-advertised tool-calling capability captured during model
    /// discovery ([`ProviderConfig::advertised_tool_support_for`], ADR-66 §3
    /// precedence level 2 / ADR-75) is threaded into the connector, so a
    /// provider that advertises tool support is believed and advertised
    /// absence selects the fallback/loose tier. Callers with a capability flag
    /// from an out-of-band listing can use
    /// [`ProviderFactory::build_with_capabilities`] to supply it directly.
    pub fn build(
        config: &ProviderConfig,
        creds: &CredentialStore,
    ) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        Self::build_with_capabilities(
            config,
            creds,
            config.advertised_tool_support_for(&config.model),
        )
    }

    /// Build a single provider, threading a provider-**advertised** per-model
    /// tool-calling capability into the connector (ADR-66 §3 precedence level
    /// 2, ADR-75).
    ///
    /// `advertised` is the value from the provider's own model listing
    /// ([`concerto_core::types::ModelInfo::supports_tool_calling`]) for the
    /// configured model, when the caller has one. It participates in the
    /// tool-schema/transport tier resolution so an advertised capability beats
    /// the last-resort name heuristic. `None` means the caller knows of no
    /// advertised metadata, leaving the family table and the conservative name
    /// heuristic to decide.
    pub fn build_with_capabilities(
        config: &ProviderConfig,
        creds: &CredentialStore,
        advertised_tool_support: Option<bool>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        if !matches!(
            config.provider.as_str(),
            "anthropic"
                | "openai"
                | "opencode"
                | "opencode-free"
                | "google"
                | "openrouter"
                | "nim"
                | "ollama"
                | "deepseek"
                | "groq"
                | "together"
                | "mistral"
                | "xai"
                | "fireworks"
                | "cerebras"
                | "cohere"
                | "deepinfra"
                | "perplexity"
                | "sambanova"
                | "dashscope"
                | "moonshot"
                | "zhipu"
                | "novita"
        ) {
            return Err(ProviderError::UnsupportedProvider { provider: config.provider.clone() });
        }

        // Ollama doesn't use API keys.
        if config.provider == "ollama" {
            let mut provider = OllamaProvider::new(config.model.clone(), config.timeout_seconds)
                .with_tool_schema_mode(resolve_tool_schema_mode(config))
                .with_advertised_tool_support(advertised_tool_support);
            if let Some(base) = &config.api_base {
                provider = provider.with_base_url(base.clone());
            }
            let provider: Arc<dyn LlmProvider> = Arc::new(provider);
            return Ok(Self::with_context_guard(provider, &config.model));
        }

        // `opencode-free-tier` port of OpenCode's unauthenticated `opencode`
        // provider: a key is optional. When one resolves it is used verbatim;
        // when none does the connector carries the literal `public`
        // credential (see `crate::credential`). This returns BEFORE the shared
        // key resolution so a keyless config cannot fail closed. Compiled out
        // entirely when the feature is off, where `opencode-free` keeps the
        // Go-relay, required-credential behaviour below.
        #[cfg(feature = "opencode-free-tier")]
        if config.provider == "opencode-free" {
            let key = config.effective_api_key(creds).unwrap_or_default();
            let base =
                OpenCodeZenProvider::resolve_api_base(&config.provider, config.api_base.as_deref());
            let provider = OpenCodeZenProvider::with_api_base(
                key,
                config.model.clone(),
                config.timeout_seconds,
                base,
            )
            .with_free_tier(true)
            .with_tool_schema_mode(resolve_tool_schema_mode(config))
            .with_advertised_tool_support(advertised_tool_support);
            let provider: Arc<dyn LlmProvider> = Arc::new(provider);
            return Ok(Self::with_context_guard(provider, &config.model));
        }

        // Key-based providers. The keyring-then-`<PROVIDER>_API_KEY` resolution
        // lives in `ProviderConfig::effective_api_key` so `concerto health`
        // and the run path agree on whether a key is present. The original
        // keyring error is remapped to the pre-existing CredentialMissing
        // variant so downstream behavior is unchanged.
        //
        // `opencode-free` deliberately takes this path too: the Go relay
        // answers `401 AuthError "Missing API key."` when no `Authorization`
        // header is sent, so a build with an empty credential would produce a
        // provider that can never complete a request. It used to short-circuit
        // *before* this resolution with an empty credential — that is exactly
        // the silent-unusable failure this resolution prevents.
        let key =
            config.effective_api_key(creds).map_err(|_| ProviderError::CredentialMissing {
                provider: if config.name.trim().is_empty() {
                    config.provider.clone()
                } else {
                    config.name.clone()
                },
            })?;

        // ADR-46 reasoning echo is a per-config dial for OpenAI-compatible
        // providers: `"always"` forces `reasoning_content` on every assistant
        // message (required by DeepSeek-style endpoints), `"if-present"` (and
        // `None`) keep the provider-built-in default. Unknown values are
        // warned about and treated as unset, never a hard error.
        let reasoning_echo = parse_reasoning_echo(config.reasoning_echo.as_deref());

        let provider: Arc<dyn LlmProvider> = match config.provider.as_str() {
            "anthropic" => {
                let mut provider =
                    AnthropicProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if config.cache_breakpoints {
                    provider = provider.with_cache_breakpoints(true);
                }
                Arc::new(provider)
            }
            "openai" => {
                let mut provider =
                    OpenAiProvider::new(key, config.model.clone(), config.timeout_seconds)
                        // ADR-48 §4: OpenAI reports usage on a streamed
                        // response only when asked for it, so without this
                        // opt-in every per-message usage column stays 0 on
                        // the OpenAI connector.
                        .with_usage_request(UsageRequest::IncludeStreamUsage)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            // Both OpenCode provider types share `OpenCodeZenProvider`; they
            // differ in the default relay (`opencode` → Zen, `opencode-free`
            // → Go) and in the static catalog. The credential comes from the
            // shared resolution above — the Go relay rejects keyless requests
            // with `401 AuthError`, so building one with an empty key would
            // only defer the failure to the first network call.
            "opencode" | "opencode-free" => {
                // OpenCode Zen defaults to `ReasoningEcho::Always` at
                // construction (DeepSeek contract), so the config dial is a
                // no-op here: "always" matches the default, and any other
                // value leaves the current behavior untouched.
                let base = OpenCodeZenProvider::resolve_api_base(
                    &config.provider,
                    config.api_base.as_deref(),
                );
                let provider = OpenCodeZenProvider::with_api_base(
                    key,
                    config.model.clone(),
                    config.timeout_seconds,
                    base,
                )
                .with_tool_schema_mode(resolve_tool_schema_mode(config))
                .with_advertised_tool_support(advertised_tool_support);
                Arc::new(provider)
            }
            "google" => {
                // ADR-66 §4 family: the Gemini connector now carries the
                // loose-schema path for weak models (same schema_loose
                // family as OpenAI/Ollama), so the `tool_schema_mode` dial
                // is live here.
                Arc::new(
                    GoogleProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support),
                )
            }
            "openrouter" => {
                let mut provider =
                    OpenRouterProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "nim" => {
                let mut provider =
                    NimProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "deepseek" => {
                // DeepSeek defaults to `ReasoningEcho::Always` at construction
                // (ADR-46 reasoning contract), so the config dial is a no-op
                // here — mirroring the OpenCode Zen arm.
                let provider = if let Some(base) = &config.api_base {
                    DeepSeekProvider::with_api_base(
                        key,
                        config.model.clone(),
                        config.timeout_seconds,
                        base.clone(),
                    )
                } else {
                    DeepSeekProvider::new(key, config.model.clone(), config.timeout_seconds)
                }
                .with_tool_schema_mode(resolve_tool_schema_mode(config))
                .with_advertised_tool_support(advertised_tool_support);
                Arc::new(provider)
            }
            "groq" => {
                let mut provider =
                    GroqProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "together" => {
                let mut provider =
                    TogetherProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "mistral" => {
                let mut provider =
                    MistralProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "xai" => {
                let mut provider =
                    XaiProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "fireworks" => {
                let mut provider =
                    FireworksProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "cerebras" => {
                let mut provider =
                    CerebrasProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "cohere" => {
                let mut provider =
                    CohereProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "deepinfra" => {
                let mut provider =
                    DeepInfraProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "perplexity" => {
                let mut provider =
                    PerplexityProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "sambanova" => {
                let mut provider =
                    SambaNovaProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "dashscope" => {
                let mut provider =
                    DashScopeProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "moonshot" => {
                let mut provider =
                    MoonshotProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "zhipu" => {
                let mut provider =
                    ZhipuProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "novita" => {
                let mut provider =
                    NovitaProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config))
                        .with_advertised_tool_support(advertised_tool_support);
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            other => {
                return Err(ProviderError::UnsupportedProvider { provider: other.to_string() });
            }
        };

        Ok(Self::with_context_guard(provider, &config.model))
    }

    fn with_context_guard(
        provider: Arc<dyn LlmProvider>,
        default_model: &str,
    ) -> Arc<dyn LlmProvider> {
        Arc::new(ContextGuardProvider::new(provider, default_model))
    }

    /// Build all providers from `ModelSettings`, returning a map of
    /// `provider_config.id` -> `Arc<dyn LlmProvider>`.
    pub fn build_all(
        settings: &ModelSettings,
        creds: &CredentialStore,
    ) -> Result<HashMap<String, Arc<dyn LlmProvider>>, ProviderError> {
        settings
            .providers
            .iter()
            .map(|config| {
                let id = Self::config_id(config);
                Self::build(config, creds).map(|provider| (id, provider))
            })
            .collect()
    }

    /// Resolve the provider and model name for a given agent role.
    ///
    /// Returns `(provider, model_name)`:
    /// - If `role` has a matching `AgentModelAssignment`, uses its
    ///   `provider_config_id` (with optional `model_override`).
    /// - Without an explicit assignment, returns `None`.
    pub fn resolve_for_role(
        settings: &ModelSettings,
        providers: &HashMap<String, Arc<dyn LlmProvider>>,
        role: &str,
    ) -> Option<(Arc<dyn LlmProvider>, String)> {
        if let Some(assignment) = settings.agent_assignments.iter().find(|a| a.agent_role == role) {
            let provider_id = &assignment.provider_config_id;
            if let Some(provider) = providers.get(provider_id) {
                let model = assignment
                    .model_override
                    .clone()
                    .or_else(|| {
                        settings
                            .providers
                            .iter()
                            .find(|provider| Self::config_id(provider) == *provider_id)
                            .map(|provider| provider.model.clone())
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                return Some((provider.clone(), model));
            }
        }

        None
    }

    /// Build `RoutingProfile` entries from `ModelSettings` providers.
    ///
    /// Each provider config is converted to a single `RoutingProfile` using
    /// the same cost/latency mapping as `ProviderRegistry::routing_profiles`.
    /// After building defaults, optional overrides from
    /// `settings.model_profile_overrides` (keyed by `pc.id`) are applied.
    ///
    /// Profile cardinality stays one-per-provider: `extra_models` is a model
    /// *resolution* concept (which model names the provider offers), not a
    /// routing concept — it does not create additional profiles.
    pub fn build_profiles(settings: &ModelSettings) -> Vec<RoutingProfile> {
        settings
            .providers
            .iter()
            .map(|config| {
                let (cost_per_1k_tokens, avg_latency_ms) = match config.provider.as_str() {
                    "openai" => (0.006, 800),
                    "anthropic" => (0.009, 1200),
                    "google" => (0.005, 600),
                    "openrouter" => (0.003, 1000),
                    "ollama" => (0.000, 200),
                    "nim" => (0.001, 400),
                    "opencode" => (0.005, 600),
                    // DeepSeek V4 Flash: $0.14/$0.28 per MTok blended ≈
                    // $0.0002/1k tokens — the cheapest frontier tier.
                    "deepseek" => (0.0002, 800),
                    // Tier-1 OpenAI-compatible providers (blended 3:1
                    // in:out representative flagship pricing, see the docs
                    // table in `ProviderRegistry::routing_profiles`).
                    "groq" => (0.0006, 150),
                    "together" => (0.001, 600),
                    "mistral" => (0.0008, 700),
                    "xai" => (0.003, 800),
                    "fireworks" => (0.0009, 400),
                    "cerebras" => (0.0009, 250),
                    "cohere" => (0.004, 900),
                    // Tier-2 OpenAI-compatible providers (blended 3:1 in:out
                    // representative flagship pricing, see the docs table in
                    // `ProviderRegistry::routing_profiles`).
                    "deepinfra" => (0.0002, 600),
                    "perplexity" => (0.006, 500),
                    "sambanova" => (0.0008, 700),
                    "dashscope" => (0.0006, 700),
                    "moonshot" => (0.003, 700),
                    "zhipu" => (0.001, 600),
                    "novita" => (0.0003, 800),
                    _ => (0.005, 500),
                };
                let mut profile = RoutingProfile {
                    provider_config_id: Self::config_id(config),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    cost_per_1k_tokens,
                    avg_latency_ms,
                    context_window: 8192,
                    // ADR-66 §3 / ADR-75: per-model capability resolution.
                    // The explicit-config override (level 1) is applied below
                    // from `model_profile_overrides`, so it is passed as
                    // `None` here; provider-advertised metadata (level 2),
                    // captured from the last discovery listing, is consulted
                    // next and wins over the optimistic default (level 3).
                    supports_tool_calling: crate::capability::resolve_tool_support(
                        &config.provider,
                        &config.model,
                        None,
                        config.advertised_tool_support_for(&config.model),
                    ),
                    base_url: config.api_base.clone(),
                    description: None,
                };
                if let Some(override_config) =
                    settings.model_profile_overrides.get(&Self::config_id(config))
                {
                    if let Some(cost) = override_config
                        .cost_per_1k_tokens
                        .filter(|cost| cost.is_finite() && *cost >= 0.0)
                    {
                        profile.cost_per_1k_tokens = cost;
                    }
                    if let Some(latency) = override_config.avg_latency_ms {
                        profile.avg_latency_ms = latency;
                    }
                    if let Some(context_window) = override_config.context_window {
                        profile.context_window = context_window;
                    }
                    if let Some(supports_tool_calling) = override_config.supports_tool_calling {
                        profile.supports_tool_calling = supports_tool_calling;
                    }
                    if let Some(base) = &override_config.base_url {
                        profile.base_url = Some(base.clone());
                    }
                    if let Some(description) = &override_config.description {
                        profile.description = Some(description.clone());
                    }
                }
                profile
            })
            .collect()
    }
}

/// Parse a configured `reasoning_echo` value (`ProviderConfig::reasoning_echo`)
/// into the ADR-46 echo policy.
///
/// `"always"` → [`ReasoningEcho::Always`]; `"if-present"` →
/// [`ReasoningEcho::IfPresent`]; `None` (unset) → `None`, leaving the
/// provider's built-in default untouched. Unknown values log a warning and
/// fall back to `None` so configs stay lenient (never a hard error).
fn parse_reasoning_echo(value: Option<&str>) -> Option<ReasoningEcho> {
    match value {
        Some("always") => Some(ReasoningEcho::Always),
        Some("if-present") => Some(ReasoningEcho::IfPresent),
        Some(other) => {
            tracing::warn!(
                value = %other,
                "unknown reasoning_echo value ({other}); falling back to the provider default",
            );
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::AgentModelAssignment;

    fn test_creds() -> CredentialStore {
        CredentialStore::from_env()
    }

    #[test]
    fn test_build_all_empty_providers() {
        let settings = ModelSettings::default();
        let result = ProviderFactory::build_all(&settings, &test_creds()).unwrap();
        assert!(result.is_empty(), "expected empty map for no providers");
    }

    #[test]
    fn test_build_all_uses_provided_ids() {
        let mut settings = ModelSettings::default();
        let providers = vec![
            ProviderConfig {
                id: "my-openai".into(),
                name: "My OpenAI".into(),
                provider: "ollama".into(),
                model: "gpt-4".into(),
                api_base: None,
                keyring_key: "openai/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            },
            ProviderConfig {
                id: "my-anthropic".into(),
                name: "My Anthropic".into(),
                provider: "ollama".into(),
                model: "claude-3".into(),
                api_base: None,
                keyring_key: "anthropic/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            },
        ];
        settings.providers = providers;
        let result = ProviderFactory::build_all(&settings, &test_creds()).unwrap();

        assert_eq!(result.len(), 2, "expected two providers");
        assert!(result.contains_key("my-openai"), "expected key 'my-openai'");
        assert!(result.contains_key("my-anthropic"), "expected key 'my-anthropic'");
    }

    #[test]
    fn test_build_all_generates_id_when_empty() {
        let mut settings = ModelSettings::default();
        let providers = vec![ProviderConfig {
            id: "".into(),
            name: "Local".into(),
            provider: "ollama".into(),
            model: "qwen".into(),
            api_base: None,
            keyring_key: "ollama/api_key".into(),
            timeout_seconds: 30,
            cached_models: Default::default(),
            cached_models_fetched_at: 0,
            ..ProviderConfig::default()
        }];
        settings.providers = providers;
        let result = ProviderFactory::build_all(&settings, &test_creds()).unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains_key("prov_ollama"), "expected 'prov_ollama'");
    }

    /// Phase 3 M3: an anthropic config with `cache_breakpoints = true` builds
    /// without error. `LlmProvider` exposes no downcast seam, so the flag value
    /// itself is asserted in the provider unit tests
    /// (`with_cache_breakpoints_toggles_apply`) — this test pins the
    /// config→provider wiring only.
    #[test]
    fn build_anthropic_with_cache_breakpoints_config() {
        let config = ProviderConfig {
            id: "anthropic-cached".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
            keyring_key: "anthropic/api_key".into(),
            cache_breakpoints: true,
            ..ProviderConfig::default()
        };
        std::env::set_var("CONCERTO_ANTHROPIC_API_KEY", "sk-test-cache");
        let provider = ProviderFactory::build(&config, &test_creds()).unwrap();
        std::env::remove_var("CONCERTO_ANTHROPIC_API_KEY");

        assert_eq!(provider.provider_name(), "anthropic");
    }

    /// A `deepseek` config builds through the factory: the env-backed test
    /// keyring resolution maps `keyring_key = "deepseek/api_key"` to
    /// `CONCERTO_DEEPSEEK_API_KEY`.
    #[test]
    fn build_deepseek_provider() {
        let config = ProviderConfig {
            id: "deepseek-main".into(),
            name: "DeepSeek Main".into(),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            keyring_key: "deepseek/api_key".into(),
            ..ProviderConfig::default()
        };
        std::env::set_var("CONCERTO_DEEPSEEK_API_KEY", "sk-test-deepseek");
        let provider = ProviderFactory::build(&config, &test_creds()).unwrap();
        std::env::remove_var("CONCERTO_DEEPSEEK_API_KEY");

        assert_eq!(provider.provider_name(), "deepseek");
    }

    /// Every Tier-1 OpenAI-compatible provider builds through the factory with
    /// its env-backed test keyring key (`<PROVIDER>_API_KEY`, provider
    /// uppercased via `ProviderConfig::effective_api_key`).
    #[test]
    fn build_tier1_openai_compatible_providers() {
        let cases = [
            ("groq", "groq-main", "llama-3.3-70b-versatile", "CONCERTO_GROQ_API_KEY"),
            (
                "together",
                "together-main",
                "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "CONCERTO_TOGETHER_API_KEY",
            ),
            ("mistral", "mistral-main", "mistral-large-latest", "CONCERTO_MISTRAL_API_KEY"),
            ("xai", "xai-main", "grok-4", "CONCERTO_XAI_API_KEY"),
            (
                "fireworks",
                "fireworks-main",
                "accounts/fireworks/models/llama-v3p3-70b-instruct",
                "CONCERTO_FIREWORKS_API_KEY",
            ),
            ("cerebras", "cerebras-main", "llama-3.3-70b", "CONCERTO_CEREBRAS_API_KEY"),
            ("cohere", "cohere-main", "command-a-plus-05-2026", "CONCERTO_COHERE_API_KEY"),
        ];
        for (provider, id, model, env_key) in cases {
            let config = ProviderConfig {
                id: id.into(),
                name: "Tier-1".into(),
                provider: provider.into(),
                model: model.into(),
                keyring_key: format!("{provider}/api_key"),
                ..ProviderConfig::default()
            };
            std::env::set_var(env_key, format!("sk-test-{provider}"));
            let built = ProviderFactory::build(&config, &test_creds());
            std::env::remove_var(env_key);

            assert_eq!(
                built.expect("factory build succeeds").provider_name(),
                provider,
                "built provider_name must match the config provider type"
            );
        }
    }

    /// The Tier-1 routing profiles carry explicit (blended) costs rather than
    /// the built-in `_` fallback, in lockstep with
    /// `ProviderRegistry::routing_profiles`.
    #[test]
    fn build_profiles_tier1_explicit_costs() {
        let providers = [
            ("groq", "llama-3.3-70b-versatile", 0.0006),
            ("together", "meta-llama/Llama-3.3-70B-Instruct-Turbo", 0.001),
            ("mistral", "mistral-large-latest", 0.0008),
            ("xai", "grok-4", 0.003),
            ("fireworks", "accounts/fireworks/models/llama-v3p3-70b-instruct", 0.0009),
            ("cerebras", "llama-3.3-70b", 0.0009),
            ("cohere", "command-a-plus-05-2026", 0.004),
        ];
        let settings = ModelSettings {
            providers: providers
                .iter()
                .map(|(provider, model, _)| ProviderConfig {
                    provider: (*provider).into(),
                    model: (*model).into(),
                    ..ProviderConfig::default()
                })
                .collect(),
            ..ModelSettings::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        assert_eq!(profiles.len(), providers.len());
        for ((provider, _, expected_cost), profile) in providers.iter().zip(&profiles) {
            assert_eq!(profile.provider, *provider);
            assert_eq!(profile.cost_per_1k_tokens, *expected_cost);
            assert!(
                profile.supports_tool_calling,
                "{provider} defaults to native tool calling (ADR-66 §3)"
            );
        }
    }

    /// Every Tier-2 OpenAI-compatible provider builds through the factory with
    /// its env-backed test keyring key (`<PROVIDER>_API_KEY`, provider
    /// uppercased via `ProviderConfig::effective_api_key`).
    #[test]
    fn build_tier2_openai_compatible_providers() {
        let cases = [
            (
                "deepinfra",
                "deepinfra-main",
                "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo",
                "CONCERTO_DEEPINFRA_API_KEY",
            ),
            ("perplexity", "perplexity-main", "sonar-pro", "CONCERTO_PERPLEXITY_API_KEY"),
            (
                "sambanova",
                "sambanova-main",
                "Meta-Llama-3.3-70B-Instruct",
                "CONCERTO_SAMBANOVA_API_KEY",
            ),
            ("dashscope", "dashscope-main", "qwen-plus", "CONCERTO_DASHSCOPE_API_KEY"),
            ("moonshot", "moonshot-main", "kimi-k2.6", "CONCERTO_MOONSHOT_API_KEY"),
            ("zhipu", "zhipu-main", "glm-4.7", "CONCERTO_ZHIPU_API_KEY"),
            // Novita is discovery-driven, so a custom model id is configured.
            ("novita", "novita-main", "custom-model", "CONCERTO_NOVITA_API_KEY"),
        ];
        for (provider, id, model, env_key) in cases {
            let config = ProviderConfig {
                id: id.into(),
                name: "Tier-2".into(),
                provider: provider.into(),
                model: model.into(),
                keyring_key: format!("{provider}/api_key"),
                ..ProviderConfig::default()
            };
            std::env::set_var(env_key, format!("sk-test-{provider}"));
            let built = ProviderFactory::build(&config, &test_creds());
            std::env::remove_var(env_key);

            assert_eq!(
                built.expect("factory build succeeds").provider_name(),
                provider,
                "built provider_name must match the config provider type"
            );
        }
    }

    /// The Tier-2 routing profiles carry explicit (blended) costs rather than
    /// the built-in `_` fallback, in lockstep with
    /// `ProviderRegistry::routing_profiles`.
    #[test]
    fn build_profiles_tier2_explicit_costs() {
        let providers = [
            ("deepinfra", "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo", 0.0002),
            ("perplexity", "sonar-pro", 0.006),
            ("sambanova", "Meta-Llama-3.3-70B-Instruct", 0.0008),
            ("dashscope", "qwen-plus", 0.0006),
            ("moonshot", "kimi-k2.6", 0.003),
            ("zhipu", "glm-4.7", 0.001),
            ("novita", "custom-model", 0.0003),
        ];
        let settings = ModelSettings {
            providers: providers
                .iter()
                .map(|(provider, model, _)| ProviderConfig {
                    provider: (*provider).into(),
                    model: (*model).into(),
                    ..ProviderConfig::default()
                })
                .collect(),
            ..ModelSettings::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        assert_eq!(profiles.len(), providers.len());
        for ((provider, _, expected_cost), profile) in providers.iter().zip(&profiles) {
            assert_eq!(profile.provider, *provider);
            assert_eq!(profile.cost_per_1k_tokens, *expected_cost);
            assert!(
                profile.supports_tool_calling,
                "{provider} defaults to native tool calling (ADR-66 §3)"
            );
        }
    }

    /// The DeepSeek routing profile carries an explicit (cheap) cost rather
    /// than the built-in `_` fallback, in lockstep with
    /// `ProviderRegistry::routing_profiles`.
    #[test]
    fn build_profiles_deepseek_explicit_cost() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                ..ProviderConfig::default()
            }],
            ..ModelSettings::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].provider, "deepseek");
        assert_eq!(profiles[0].cost_per_1k_tokens, 0.0002);
    }

    #[test]
    fn config_for_model_preserves_a_valid_preferred_route() {
        let settings = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: "first".into(),
                    provider: "ollama".into(),
                    cached_models: vec!["shared-model".into()],
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    id: "preferred".into(),
                    provider: "ollama".into(),
                    cached_models: vec!["shared-model".into()],
                    ..ProviderConfig::default()
                },
            ],
            ..ModelSettings::default()
        };

        let resolved =
            ProviderFactory::config_for_model(&settings, "shared-model", Some("preferred"));
        assert_eq!(resolved.map(|provider| provider.id.as_str()), Some("preferred"));
    }

    #[test]
    fn test_resolve_for_role_matching_assignment() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "p1".into(),
                name: "P1".into(),
                provider: "openai".into(),
                model: "gpt-4".into(),
                api_base: None,
                keyring_key: "openai/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            agent_assignments: vec![AgentModelAssignment {
                agent_role: "coder".into(),
                provider_config_id: "p1".into(),
                model_override: None,
            }],
            ..Default::default()
        };

        let provider: Arc<dyn LlmProvider> = Arc::new(crate::mock::MockProvider::default());
        let mut providers = HashMap::new();
        providers.insert("p1".into(), provider);

        let result = ProviderFactory::resolve_for_role(&settings, &providers, "coder");
        assert!(result.is_some(), "expected Some for coder role");
        let (_, model) = result.unwrap();
        assert_eq!(model, "gpt-4", "expected model from provider config");
    }

    #[test]
    fn test_resolve_for_role_with_model_override() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "p1".into(),
                name: "P1".into(),
                provider: "openai".into(),
                model: "gpt-4".into(),
                api_base: None,
                keyring_key: "openai/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            agent_assignments: vec![AgentModelAssignment {
                agent_role: "coder".into(),
                provider_config_id: "p1".into(),
                model_override: Some("gpt-4-turbo".into()),
            }],
            ..Default::default()
        };

        let provider: Arc<dyn LlmProvider> = Arc::new(crate::mock::MockProvider::default());
        let mut providers = HashMap::new();
        providers.insert("p1".into(), provider);

        let result = ProviderFactory::resolve_for_role(&settings, &providers, "coder");
        assert!(result.is_some());
        let (_, model) = result.unwrap();
        assert_eq!(model, "gpt-4-turbo", "expected overridden model");
    }

    #[test]
    fn test_resolve_for_role_without_assignment_does_not_use_global_default() {
        let settings = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: "fast".into(),
                    name: "Fast".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                    api_base: None,
                    keyring_key: "openai/api_key".into(),
                    timeout_seconds: 30,
                    cached_models: Default::default(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    id: "main".into(),
                    name: "Main".into(),
                    provider: "openai".into(),
                    model: "gpt-4".into(),
                    api_base: None,
                    keyring_key: "openai/api_key".into(),
                    timeout_seconds: 30,
                    cached_models: Default::default(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
            ],
            ..Default::default()
        };

        let fast_provider: Arc<dyn LlmProvider> = Arc::new(crate::mock::MockProvider::default());
        let main_provider: Arc<dyn LlmProvider> = Arc::new(crate::mock::MockProvider::default());
        let mut providers = HashMap::new();
        providers.insert("fast".into(), fast_provider);
        providers.insert("main".into(), main_provider);

        let result = ProviderFactory::resolve_for_role(&settings, &providers, "planner");
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_for_role_without_assignment_does_not_use_first_provider() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "only".into(),
                name: "Only".into(),
                provider: "openai".into(),
                model: "gpt-4".into(),
                api_base: None,
                keyring_key: "openai/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            ..Default::default()
        };

        let provider: Arc<dyn LlmProvider> = Arc::new(crate::mock::MockProvider::default());
        let mut providers = HashMap::new();
        providers.insert("only".into(), provider);

        let result = ProviderFactory::resolve_for_role(&settings, &providers, "any-role");
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_for_role_no_providers_returns_none() {
        let settings = ModelSettings::default();
        let providers = HashMap::new();

        let result = ProviderFactory::resolve_for_role(&settings, &providers, "any-role");
        assert!(result.is_none(), "expected None when no providers exist");
    }

    #[test]
    fn test_build_all_returns_hard_error_for_unsupported_provider() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "test".into(),
                name: "Test".into(),
                provider: "unconfigured-test-provider".into(),
                model: "gpt-4".into(),
                api_base: None,
                keyring_key: "unconfigured-test-provider/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            ..Default::default()
        };

        let result = ProviderFactory::build_all(&settings, &test_creds());
        assert!(matches!(result, Err(ProviderError::UnsupportedProvider { .. })));
    }

    #[test]
    fn build_profiles_applies_model_metadata_overrides() {
        let mut settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "openrouter-glm".into(),
                name: "GLM".into(),
                provider: "openrouter".into(),
                model: "z-ai/glm-5.2".into(),
                api_base: None,
                keyring_key: "openrouter/api_key".into(),
                timeout_seconds: 30,
                cached_models: Default::default(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            ..Default::default()
        };
        settings.model_profile_overrides.insert(
            "openrouter-glm".into(),
            concerto_config::ModelProfileOverride {
                cost_per_1k_tokens: Some(0.0),
                avg_latency_ms: Some(750),
                context_window: Some(128_000),
                supports_tool_calling: Some(true),
                ..Default::default()
            },
        );
        let profiles = ProviderFactory::build_profiles(&settings);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].provider_config_id, "openrouter-glm");
        assert_eq!(profiles[0].cost_per_1k_tokens, 0.0);
        assert_eq!(profiles[0].avg_latency_ms, 750);
        assert_eq!(profiles[0].context_window, 128_000);
    }

    #[test]
    fn config_for_model_returns_none_when_model_not_found() {
        let settings = ModelSettings::default();
        let resolved = ProviderFactory::config_for_model(&settings, "nonexistent-model", None);
        assert!(resolved.is_none());
    }

    #[test]
    fn config_for_model_preferred_route_ignored_when_no_match() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "only".into(),
                provider: "ollama".into(),
                model: "llama3".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let resolved = ProviderFactory::config_for_model(&settings, "llama3", Some("preferred"));
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "only");
    }

    #[test]
    fn config_for_model_matches_by_cached_models() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "provider-a".into(),
                provider: "openai".into(),
                model: "gpt-4".into(),
                cached_models: vec!["gpt-4".into(), "gpt-4-turbo".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let resolved = ProviderFactory::config_for_model(&settings, "gpt-4-turbo", None);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "provider-a");
    }

    #[test]
    fn config_offers_model_checks_primary_extra_and_cached_models() {
        let config = ProviderConfig {
            id: "gateway".into(),
            provider: "openai".into(),
            model: "primary".into(),
            extra_models: vec!["alias".into()],
            cached_models: vec!["discovered".into()],
            ..Default::default()
        };

        assert!(ProviderFactory::config_offers_model(&config, "primary"));
        assert!(ProviderFactory::config_offers_model(&config, "alias"));
        assert!(ProviderFactory::config_offers_model(&config, "discovered"));
        assert!(!ProviderFactory::config_offers_model(&config, "not-offered"));
    }

    #[test]
    fn config_for_model_matches_by_extra_models() {
        // `extra_models` advertises additional models on one provider config:
        // resolution finds it, and the primary `model` still matches too.
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "gateway".into(),
                provider: "openai".into(),
                model: "primary".into(),
                extra_models: vec!["alias-a".into(), "alias-b".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let resolved = ProviderFactory::config_for_model(&settings, "alias-b", None);
        assert_eq!(resolved.map(|provider| provider.id.as_str()), Some("gateway"));
        // Primary model still offered (extra_models never shadows it).
        let resolved = ProviderFactory::config_for_model(&settings, "primary", None);
        assert_eq!(resolved.map(|provider| provider.id.as_str()), Some("gateway"));
    }

    #[test]
    fn config_for_model_extra_models_do_not_shadow_primary_route() {
        // Two configs advertise the same extra model, but the primary model
        // of the second config must still win when it is also a candidate of
        // another config — first-match order stays deterministic.
        let settings = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: "first".into(),
                    provider: "openai".into(),
                    model: "shared".into(),
                    ..Default::default()
                },
                ProviderConfig {
                    id: "second".into(),
                    provider: "openai".into(),
                    model: "other".into(),
                    extra_models: vec!["shared".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let resolved = ProviderFactory::config_for_model(&settings, "shared", None);
        assert_eq!(resolved.unwrap().id, "first", "primary model keeps first-match priority");
    }

    #[test]
    fn parse_reasoning_echo_accepts_known_and_rejects_unknown() {
        assert_eq!(parse_reasoning_echo(Some("always")), Some(ReasoningEcho::Always));
        assert_eq!(parse_reasoning_echo(Some("if-present")), Some(ReasoningEcho::IfPresent));
        assert_eq!(parse_reasoning_echo(None), None);
        // Unknown values are tolerated (warned) and treated as unset.
        assert_eq!(parse_reasoning_echo(Some("sometimes")), None);
    }

    #[test]
    fn config_for_model_default_returns_first_match() {
        let settings = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: "first".into(),
                    provider: "ollama".into(),
                    model: "same-model".into(),
                    ..Default::default()
                },
                ProviderConfig {
                    id: "second".into(),
                    provider: "ollama".into(),
                    model: "same-model".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let resolved = ProviderFactory::config_for_model(&settings, "same-model", None);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "first");
    }

    #[test]
    fn build_profiles_empty_settings_returns_empty() {
        let settings = ModelSettings::default();
        let profiles = ProviderFactory::build_profiles(&settings);
        assert!(profiles.is_empty());
    }

    #[test]
    fn build_profiles_without_overrides() {
        let settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "test".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].provider_config_id, "test");
        assert_eq!(profiles[0].model, "gpt-4o");
    }

    /// Building profiles with empty settings returns an empty vector.
    #[test]
    fn build_profiles_empty_settings() {
        let settings = ModelSettings::default();
        let profiles = ProviderFactory::build_profiles(&settings);
        assert!(profiles.is_empty(), "empty settings should produce no profiles");
    }

    /// ADR-66 §3: `build_profiles` resolves `supports_tool_calling` per
    /// model — a Zen-served genuine Muse model (Responses dialect, no tool
    /// declarations) resolves to `false`, and so does the `muse-spark-*`
    /// family via the explicit Responses dialect prefix entry (ADR-66 §5
    /// correction: endpoint behavior, not taxonomy). Other providers keep
    /// the provider default.
    ///
    /// Inverted from the old assertion that the Responses-dialect models have
    /// no tool support: the converter was completed (ADR-75), so they resolve
    /// natively like every other HTTP model — no name decides capability.
    #[test]
    fn build_profiles_resolves_tool_support_per_model() {
        let settings = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: "zen-muse".into(),
                    provider: "opencode".into(),
                    model: "muse-v2".into(),
                    ..Default::default()
                },
                ProviderConfig {
                    id: "zen-spark".into(),
                    provider: "opencode".into(),
                    model: "muse-spark-1.2-contributor-free".into(),
                    ..Default::default()
                },
                ProviderConfig {
                    id: "openai-main".into(),
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        let by_id = |id: &str| {
            profiles
                .iter()
                .find(|profile| profile.provider_config_id == id)
                .unwrap_or_else(|| panic!("missing profile {id}"))
        };
        assert!(
            by_id("zen-muse").supports_tool_calling,
            "the Responses converter now carries native tools"
        );
        assert!(
            by_id("zen-spark").supports_tool_calling,
            "muse-spark-* rides the Responses dialect, whose converter now carries tools"
        );
        assert!(by_id("openai-main").supports_tool_calling);
    }

    /// ADR-66 §3 precedence level 1: the explicit-config override wins over
    /// the per-model resolution in `build_profiles`.
    #[test]
    fn build_profiles_explicit_override_wins() {
        let mut settings = ModelSettings {
            providers: vec![ProviderConfig {
                id: "zen-muse".into(),
                provider: "opencode".into(),
                model: "muse-v2".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        settings.model_profile_overrides.insert(
            "zen-muse".into(),
            concerto_config::ModelProfileOverride {
                supports_tool_calling: Some(true),
                ..Default::default()
            },
        );
        let profiles = ProviderFactory::build_profiles(&settings);
        assert!(
            profiles[0].supports_tool_calling,
            "explicit override must beat the optimistic default"
        );
    }

    /// ADR-75: `build` reads provider-advertised metadata from the config's
    /// discovery cache, so advertised capability actually reaches the
    /// connector. `build_with_capabilities` stays the out-of-band entry point.
    #[test]
    fn build_reads_config_advertised_metadata() {
        // Ollama needs no credential, so the test never touches the keyring.
        let mut config = ProviderConfig {
            id: "local".into(),
            provider: "ollama".into(),
            model: "mimo-v2.5".into(),
            ..Default::default()
        };
        // A weak-looking name advertised as tool-capable.
        config.cached_model_tool_support.insert("mimo-v2.5".into(), true);
        assert_eq!(config.advertised_tool_support_for("mimo-v2.5"), Some(true));

        // `build` must apply it (rather than the name heuristic) and still
        // construct a working provider.
        let creds = test_creds();
        let provider = ProviderFactory::build(&config, &creds)
            .expect("provider builds with config-advertised metadata");
        assert_eq!(provider.provider_name(), "ollama");
    }

    /// ADR-75: `build_profiles` believes provider-advertised metadata over
    /// the optimistic default, in both directions.
    #[test]
    fn build_profiles_honors_advertised_metadata() {
        let mut advertised_absent = ProviderConfig {
            id: "declared-absent".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        };
        advertised_absent.cached_model_tool_support.insert("gpt-4o".into(), false);

        let mut advertised_present = ProviderConfig {
            id: "declared-present".into(),
            provider: "openai".into(),
            model: "mimo-v2.5".into(),
            ..Default::default()
        };
        advertised_present.cached_model_tool_support.insert("mimo-v2.5".into(), true);

        let settings = ModelSettings {
            providers: vec![advertised_absent, advertised_present],
            ..Default::default()
        };
        let profiles = ProviderFactory::build_profiles(&settings);
        let by_id = |id: &str| {
            profiles
                .iter()
                .find(|profile| profile.provider_config_id == id)
                .unwrap_or_else(|| panic!("missing profile {id}"))
        };
        assert!(
            !by_id("declared-absent").supports_tool_calling,
            "advertised absence must beat the optimistic default"
        );
        assert!(
            by_id("declared-present").supports_tool_calling,
            "advertised support must beat the weak-looking name"
        );
    }

    /// Plugin-backed provider configs (the plugins crate registers
    /// `plugin:<id>` providers outside `ProviderFactory::build`, but
    /// profiles may still be built for them) resolve to no tool support via
    /// the provider default (decision (a): gated to AnswerOnly tasks).
    #[test]
    fn capability_resolution_plugin_backed_defaults_false() {
        assert!(!crate::capability::resolve_tool_support("plugin:my-llm", "any-model", None, None));
    }

    /// ADR-75: `build_with_capabilities` threads provider-advertised
    /// tool-calling metadata into the connector, and `build` reads the
    /// config's discovery cache by default. Both construct a working provider
    /// for a free-route model without panicking.
    #[test]
    fn build_with_capabilities_threads_advertised_support() {
        // Ollama needs no credential, so the test exercises only the tier
        // threading and never touches the keyring.
        let config = ProviderConfig {
            id: "local".into(),
            provider: "ollama".into(),
            model: "space-bunny-free".into(),
            ..Default::default()
        };
        let creds = test_creds();
        let with_flag = ProviderFactory::build_with_capabilities(&config, &creds, Some(true))
            .expect("provider builds with advertised metadata");
        assert_eq!(with_flag.provider_name(), "ollama");
        let without_flag =
            ProviderFactory::build(&config, &creds).expect("plain build remains a None delegate");
        assert_eq!(without_flag.provider_name(), "ollama");
    }

    // ------------------------------------------------------------------
    // `opencode-free` credential requirement
    // ------------------------------------------------------------------

    /// Serializes the tests that mutate the `opencode-free` env fallback:
    /// env vars are process-global and cargo runs tests in parallel, so a
    /// build that must see the variable set and a build that must see it
    /// clear would otherwise race. An async-aware mutex (rather than
    /// [`std::sync::Mutex`]) because one of the two tests holds it across
    /// `.await`, which `clippy::await_holding_lock` rejects. Mirrors
    /// `CONFIG_ENV_LOCK` (concerto-desktop) and `THEME_ENV_LOCK`
    /// (concerto-cli).
    static OPENCODE_FREE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Remove `name` for the duration of a test and restore whatever was
    /// there afterwards, even across a panic — env vars are process-global,
    /// so a leaked edit would race with tests running in parallel.
    struct RestoreEnvVar {
        name: &'static str,
        saved: Option<String>,
    }

    impl RestoreEnvVar {
        fn without(name: &'static str) -> Self {
            let saved = std::env::var(name).ok();
            std::env::remove_var(name);
            Self { name, saved }
        }

        /// Set `name` to `value` for the duration of a test and restore
        /// whatever was there afterwards (present or absent), even across a
        /// panic. Same race-avoidance contract as [`Self::without`].
        fn set(name: &'static str, value: &str) -> Self {
            let saved = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self { name, saved }
        }
    }

    impl Drop for RestoreEnvVar {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// The exact env fallback name [`ProviderConfig::effective_api_key`]
    /// looks up for an `opencode-free` config: the name is built from
    /// `provider.to_uppercase()`, so the hyphen survives —
    /// `OPENCODE-FREE_API_KEY`, *not* `OPENCODE_FREE_API_KEY`. It can only be
    /// set programmatically (a shell refuses to export a hyphenated
    /// variable), so the keyring/settings-UI path stays the primary way to
    /// credential this provider.
    const OPENCODE_FREE_ENV_KEY: &str = "OPENCODE-FREE_API_KEY";

    /// An `opencode-free` config with no resolvable credential anywhere: a
    /// unique keyring account (so no `CONCERTO_*` env var can satisfy it) and
    /// no [`OPENCODE_FREE_ENV_KEY`] (cleared by the caller's [`RestoreEnvVar`]).
    fn uncredentialed_opencode_free_config() -> ProviderConfig {
        ProviderConfig {
            id: "opencode-free-main".into(),
            name: "OpenCode Zen (free)".into(),
            provider: "opencode-free".into(),
            model: "minimax-m3".into(),
            keyring_key: "test-uncredentialed-opencode-free/api_key".into(),
            ..ProviderConfig::default()
        }
    }

    /// INVERTED (2026-09-29): `opencode-free` used to build with no
    /// credential at all, short-circuiting before key resolution. The Go
    /// relay answers `401 AuthError "Missing API key."` when a request carries
    /// no `Authorization` header, so that build succeeded only to fail on the
    /// first network call — the silent-unusable failure. It now resolves the
    /// key through the shared path and fails closed with `CredentialMissing`,
    /// exactly like every other key-based type.
    ///
    /// Feature-off only: with `opencode-free-tier` ON, keyless is the whole
    /// point (see `build_opencode_free_keyless_free_tier_builds`).
    #[cfg(not(feature = "opencode-free-tier"))]
    #[test]
    fn build_opencode_free_without_credential_fails_closed() {
        // `blocking_lock` is safe here: this is a plain `#[test]`, outside
        // any async execution context.
        let _env = OPENCODE_FREE_ENV_LOCK.blocking_lock();
        let _restored = RestoreEnvVar::without(OPENCODE_FREE_ENV_KEY);
        let config = uncredentialed_opencode_free_config();
        let creds = test_creds();

        assert!(config.effective_api_key(&creds).is_err(), "no key may resolve for this config");

        let Err(error) = ProviderFactory::build(&config, &creds) else {
            panic!("a credential-less opencode-free config must not build");
        };
        assert!(
            matches!(error, ProviderError::CredentialMissing { .. }),
            "build must fail closed with CredentialMissing, got {error:?}"
        );

        // ADR-75 capability threading takes the same credential path: it must
        // not become a keyless back door.
        let Err(error) = ProviderFactory::build_with_capabilities(&config, &creds, Some(true))
        else {
            panic!("build_with_capabilities must fail closed too");
        };
        assert!(
            matches!(error, ProviderError::CredentialMissing { .. }),
            "capability build must fail closed with CredentialMissing, got {error:?}"
        );
    }

    /// Feature-on: a keyless `opencode-free` config builds (that is the free
    /// tier), and its requests carry the literal `Bearer public` credential
    /// that the server maps to its anonymous path.
    #[cfg(feature = "opencode-free-tier")]
    #[tokio::test]
    async fn build_opencode_free_keyless_free_tier_builds() {
        let _env = OPENCODE_FREE_ENV_LOCK.lock().await;
        let _restored = RestoreEnvVar::without(OPENCODE_FREE_ENV_KEY);
        let (base, req_rx) = crate::testing::mock_server::spawn(String::new());
        let mut config = uncredentialed_opencode_free_config();
        config.api_base = Some(base.clone());
        let creds = test_creds();

        assert!(config.effective_api_key(&creds).is_err(), "no key may resolve for this config");

        let provider = ProviderFactory::build(&config, &creds)
            .expect("a keyless opencode-free config must build in free-tier mode");
        assert_eq!(provider.provider_name(), "opencode");

        provider
            .test_connection(concerto_core::CancellationToken::new())
            .await
            .expect("the connection test must reach the overridden api_base");
        let raw = req_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the mock endpoint must capture exactly one request");
        let headers = crate::testing::mock_server::request_headers(&raw);
        assert!(
            headers.contains("authorization: bearer public"),
            "keyless free-tier requests must carry the literal `public` credential: {headers}"
        );
    }

    /// With a credential the `opencode-free` arm builds through the shared
    /// `OpenCodeZenProvider` connector (`provider_name()` stays `opencode`),
    /// honors a config `api_base` override, and puts the `x-opencode-session`
    /// affinity header on the wire — with no `X-Session-ID`, which does not
    /// exist upstream and must never come back.
    #[tokio::test]
    async fn opencode_free_with_credential_builds_and_honours_api_base() {
        let _env = OPENCODE_FREE_ENV_LOCK.lock().await;
        let _key = RestoreEnvVar::set(OPENCODE_FREE_ENV_KEY, "test-go-relay-key");
        // One request only: `test_connection` issues exactly one `GET
        // /models`, which is what the mock captures.
        let (base, req_rx) = crate::testing::mock_server::spawn(String::new());
        let mut config = uncredentialed_opencode_free_config();
        config.api_base = Some(base.clone());
        let creds = test_creds();

        assert!(
            config.effective_api_key(&creds).is_ok(),
            "the env credential must satisfy the shared resolution"
        );

        let provider = ProviderFactory::build(&config, &creds)
            .expect("an opencode-free config with a credential must build");
        assert_eq!(provider.provider_name(), "opencode");

        provider
            .test_connection(concerto_core::CancellationToken::new())
            .await
            .expect("the connection test must reach the overridden api_base");

        let raw = req_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the mock endpoint must capture exactly one request");
        let headers = crate::testing::mock_server::request_headers(&raw);

        assert!(
            headers.starts_with("get /models http/1.1"),
            "the config api_base must be honored, got request line: {headers}"
        );
        assert!(
            headers.contains("authorization: bearer test-go-relay-key"),
            "the resolved credential must authenticate the request: {headers}"
        );
        assert!(
            headers.contains("x-opencode-session:"),
            "every OpenCode request must carry the affinity header: {headers}"
        );
        assert!(
            !headers.contains("x-session-id"),
            "X-Session-ID is not an upstream header and must never be sent: {headers}"
        );
    }

    /// Credential enforcement does not narrow to OpenCode: a `zhipu` config
    /// with no resolvable key still fails closed with `CredentialMissing`
    /// (`ollama` remains the only keyless type).
    #[test]
    fn keyless_non_opencode_provider_still_requires_credential() {
        let _restored = RestoreEnvVar::without("ZHIPU_API_KEY");
        let config = ProviderConfig {
            id: "zhipu-keyless".into(),
            name: "Zhipu".into(),
            provider: "zhipu".into(),
            model: "glm-4.7".into(),
            keyring_key: "test-keyless-zhipu/api_key".into(),
            ..ProviderConfig::default()
        };
        let creds = test_creds();

        assert!(config.effective_api_key(&creds).is_err(), "no key may resolve for this config");

        let Err(error) = ProviderFactory::build(&config, &creds) else {
            panic!("only ollama may build without a key");
        };
        assert!(
            matches!(error, ProviderError::CredentialMissing { .. }),
            "build must fail closed with CredentialMissing, got {error:?}"
        );
    }
}
