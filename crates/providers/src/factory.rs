use std::collections::HashMap;
use std::sync::Arc;

use concerto_config::{
    parse_tool_schema_mode, CredentialStore, ModelSettings, ProviderConfig, ToolSchemaMode,
};
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::RoutingProfile;

use crate::anthropic::AnthropicProvider;
use crate::context_guard::ContextGuardProvider;
use crate::google::GoogleProvider;
use crate::nim::NimProvider;
use crate::ollama::OllamaProvider;
use crate::openai::{OpenAiProvider, ReasoningEcho};
use crate::opencode::OpenCodeZenProvider;
use crate::openrouter::OpenRouterProvider;

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

    /// Resolve the provider configuration that advertises a model.
    ///
    /// A provider config offers a model when the name equals its primary
    /// `model`, appears in `extra_models`, or is offered by the static/cached
    /// catalog paths. `extra_models` is purely additive — it can never shadow
    /// the primary `model` (the primary always wins on exact match).
    ///
    /// An existing route wins when it remains valid; otherwise configuration
    /// order is the deterministic tie-breaker for duplicate model IDs.
    pub fn config_for_model<'a>(
        settings: &'a ModelSettings,
        model: &str,
        preferred_provider_id: Option<&str>,
    ) -> Option<&'a ProviderConfig> {
        let offers_model = |provider: &ProviderConfig| {
            let definition = crate::provider_defs::provider_definition(&provider.provider);
            let mut options = crate::provider_defs::model_options_for(provider, &definition, None);
            options.extend(provider.cached_models.iter().cloned());
            options.extend(provider.extra_models.iter().cloned());
            options.iter().any(|candidate| candidate == model)
        };

        preferred_provider_id
            .and_then(|id| settings.providers.iter().find(|provider| provider.id == id))
            .filter(|provider| offers_model(provider))
            .or_else(|| settings.providers.iter().find(|provider| offers_model(provider)))
    }

    /// Build a single provider from its config.
    ///
    /// Missing credentials and unknown provider types are configuration
    /// errors. Production execution must never silently substitute a mock
    /// model because that makes a failed setup look like a successful run.
    pub fn build(
        config: &ProviderConfig,
        creds: &CredentialStore,
    ) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        if !matches!(
            config.provider.as_str(),
            "anthropic" | "openai" | "opencode" | "google" | "openrouter" | "nim" | "ollama"
        ) {
            return Err(ProviderError::UnsupportedProvider { provider: config.provider.clone() });
        }

        // Ollama doesn't use API keys.
        if config.provider == "ollama" {
            let mut provider = OllamaProvider::new(config.model.clone(), config.timeout_seconds)
                .with_tool_schema_mode(resolve_tool_schema_mode(config));
            if let Some(base) = &config.api_base {
                provider = provider.with_base_url(base.clone());
            }
            let provider: Arc<dyn LlmProvider> = Arc::new(provider);
            return Ok(Self::with_context_guard(provider, &config.model));
        }

        // Key-based providers. The keyring-then-`<PROVIDER>_API_KEY` resolution
        // lives in `ProviderConfig::effective_api_key` so `concerto health`
        // and the run path agree on whether a key is present. The original
        // keyring error is remapped to the pre-existing CredentialMissing
        // variant so downstream behavior is unchanged.
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
                        .with_tool_schema_mode(resolve_tool_schema_mode(config));
                if config.cache_breakpoints {
                    provider = provider.with_cache_breakpoints(true);
                }
                Arc::new(provider)
            }
            "openai" => {
                let mut provider =
                    OpenAiProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config));
                if let Some(base) = &config.api_base {
                    provider = provider.with_api_base(base.clone());
                }
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "opencode" => {
                // OpenCode Zen defaults to `ReasoningEcho::Always` at
                // construction (DeepSeek contract), so the config dial is a
                // no-op here: "always" matches the default, and any other
                // value leaves the current behavior untouched.
                let provider = if let Some(base) = &config.api_base {
                    OpenCodeZenProvider::with_api_base(
                        key,
                        config.model.clone(),
                        config.timeout_seconds,
                        base.clone(),
                    )
                } else {
                    OpenCodeZenProvider::new(key, config.model.clone(), config.timeout_seconds)
                }
                .with_tool_schema_mode(resolve_tool_schema_mode(config));
                Arc::new(provider)
            }
            "google" => {
                // Google/Gemini has no loose-schema path yet; the
                // `tool_schema_mode` dial is tolerated but inert for it.
                Arc::new(GoogleProvider::new(key, config.model.clone(), config.timeout_seconds))
            }
            "openrouter" => {
                let mut provider =
                    OpenRouterProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config));
                if let Some(echo) = reasoning_echo {
                    provider = provider.with_reasoning_echo(echo);
                }
                Arc::new(provider)
            }
            "nim" => {
                let mut provider =
                    NimProvider::new(key, config.model.clone(), config.timeout_seconds)
                        .with_tool_schema_mode(resolve_tool_schema_mode(config));
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
                    _ => (0.005, 500),
                };
                let mut profile = RoutingProfile {
                    provider_config_id: Self::config_id(config),
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    cost_per_1k_tokens,
                    avg_latency_ms,
                    context_window: 8192,
                    supports_tool_calling: true,
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
}
