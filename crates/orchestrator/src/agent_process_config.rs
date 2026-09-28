//! Supervised agent-process configuration contract — ADR-60 S5 (DEFERRED row #49).
//!
//! The parent (supervisor/runtime) resolves *what this process should run* once,
//! and this module is the single, shared definition of how that configuration
//! travels to the child and how the child rebuilds the real provider from it.
//! It exists so the resolve-once/stamp-the-child/rebuild-in-child contract has
//! exactly one implementation instead of drifting between the parent and the
//! child binary.
//!
//! ## Why config-by-value, not by-env-indirection
//!
//! The child cannot simply call [`concerto_config::load_config`] itself: the
//! parent may have been started with an explicit `global_path`, a different
//! `CONCERTO_*` environment, or plugin providers that only exist in the parent.
//! Passing the *effective* [`AppConfig`] over the process boundary guarantees
//! the child resolves the same provider the parent would have, with no hidden
//! re-resolution drift.
//!
//! ## Secret boundary
//!
//! [`AppConfig`] carries provider metadata only — API keys live in the
//! keychain/`CONCERTO_*_API_KEY` env, never in the config value. The child
//! rebuilds the provider through [`concerto_providers::factory::ProviderFactory`],
//! which resolves credentials from the OS keychain exactly like the parent.
//! Secrets therefore never ride argv, env or IPC frames, and a missing
//! credential fails closed with a named error (never a silent mock fallback).

use std::sync::Arc;

use concerto_config::{AppConfig, CredentialStore, ProviderConfig};
use concerto_core::error::OrchestratorError;
use concerto_core::traits::provider::LlmProvider;
use concerto_providers::factory::ProviderFactory;
use concerto_providers::mock::MockProvider;

/// Environment variable naming the provider wiring the child should use.
///
/// [`MOCK_PROVIDER`] is an explicit, documented opt-in (tests and fixtures
/// only); every other value — including unset — selects the real
/// config/credential path. The mock is never the implicit default.
pub const PROVIDER_ENV: &str = "CONCERTO_PROVIDER";

/// Environment variable carrying the parent's resolved [`AppConfig`] as JSON.
///
/// The parent stamps the *effective* config (global + project + env layers
/// already merged) so the child's provider resolution cannot drift from the
/// parent's. It carries no secrets (see the module docs).
pub const CONFIG_ENV: &str = "CONCERTO_AGENT_CONFIG_JSON";

/// Explicit opt-in value for [`PROVIDER_ENV`] selecting the scripted mock.
pub const MOCK_PROVIDER: &str = "mock";

/// A named, fail-closed provider-unavailable error.
///
/// Every provider resolution failure on the supervised path funnels through
/// here so the child entry can report one recognizable class (and tests can
/// pin it) instead of a silent mock substitution.
#[derive(Debug, thiserror::Error)]
pub enum AgentProcessProviderError {
    /// The parent did not stamp a config and none could be loaded locally.
    #[error(
        "agent-process: no provider config available (CONCERTO_AGENT_CONFIG_JSON not set by \
         the supervisor and local config load failed: {reason}); refusing to fall back to mock"
    )]
    MissingConfig { reason: String },
    /// The stamped config was present but not valid JSON / not an `AppConfig`.
    #[error("agent-process: CONCERTO_AGENT_CONFIG_JSON is not a valid AppConfig: {reason}")]
    MalformedConfig { reason: String },
    /// No provider is configured in the effective config.
    #[error("agent-process: {reason}")]
    NoProviderConfigured { reason: String },
    /// The provider built, but its credential is missing.
    #[error("agent-process: provider credential missing for '{provider}'")]
    CredentialMissing { provider: String },
    /// The provider type is unsupported.
    #[error("agent-process: provider '{provider}' is not supported")]
    UnsupportedProvider { provider: String },
    /// The provider failed to build for any other reason.
    #[error("agent-process: provider build failed: {reason}")]
    Build { reason: String },
    /// The provider script JSON for the mock was malformed.
    #[error("agent-process: CONCERTO_MOCK_SCRIPT_JSON is not a valid chunk script: {reason}")]
    MockScript { reason: String },
}

impl AgentProcessProviderError {
    /// Map a [`concerto_core::error::ProviderError`] onto the named taxonomy.
    fn from_provider_error(error: concerto_core::error::ProviderError) -> Self {
        use concerto_core::error::ProviderError;
        match error {
            ProviderError::CredentialMissing { provider } => Self::CredentialMissing { provider },
            ProviderError::UnsupportedProvider { provider } => {
                Self::UnsupportedProvider { provider }
            }
            other => Self::Build { reason: other.to_string() },
        }
    }
}

/// Parse the parent-stamped [`AppConfig`], or fail with a named error.
///
/// `raw` is the value of [`CONFIG_ENV`]: `None` (unset/empty) or invalid JSON
/// both fail closed rather than reaching for a mock.
pub fn parse_app_config(raw: Option<&str>) -> Result<AppConfig, AgentProcessProviderError> {
    let raw = match raw {
        Some(raw) if !raw.trim().is_empty() => raw,
        _ => {
            return Err(AgentProcessProviderError::MissingConfig {
                reason: format!("{CONFIG_ENV} is unset"),
            })
        }
    };
    serde_json::from_str::<AppConfig>(raw)
        .map_err(|error| AgentProcessProviderError::MalformedConfig { reason: error.to_string() })
}

/// Resolve the provider configuration the child should build.
///
/// Mirrors the parent's default-provider resolution (the same precedence the
/// single-agent path uses in `runtime_runner::resolve_provider`): the global
/// default model's provider, else the first configured provider, else the
/// legacy single-provider config. Never a mock.
pub fn resolve_provider_config(
    config: &AppConfig,
) -> Result<(ProviderConfig, Option<String>), AgentProcessProviderError> {
    if let Some(settings) = &config.model_settings {
        if let Some(default_model) =
            settings.global_default_model.as_deref().filter(|model| !model.trim().is_empty())
        {
            if let Some(provider) = ProviderFactory::config_for_model(settings, default_model, None)
            {
                let model = default_model.to_owned();
                return Ok((provider.clone(), Some(model)));
            }
        }
        if let Some(provider) = settings.providers.first() {
            let model = provider.model.trim().to_owned();
            return Ok((provider.clone(), (!model.is_empty()).then_some(model)));
        }
    }
    if let Some(provider) = &config.primary_provider_config {
        let model = provider.model.trim().to_owned();
        return Ok((provider.clone(), (!model.is_empty()).then_some(model)));
    }
    Err(AgentProcessProviderError::NoProviderConfigured {
        reason: "no provider configured in model_settings or primary_provider_config".to_owned(),
    })
}

/// Build the real provider from a resolved config and the credential store.
///
/// Credentials resolve through [`CredentialStore::new`] (OS keychain, with the
/// `CONCERTO_*_API_KEY` env fallback the factory already implements) — exactly
/// the parent's behavior. A missing key is [`AgentProcessProviderError::CredentialMissing`].
pub fn build_provider(
    config: &ProviderConfig,
    model_override: Option<&str>,
    creds: &CredentialStore,
) -> Result<Arc<dyn LlmProvider>, AgentProcessProviderError> {
    let mut config = config.clone();
    if let Some(model) = model_override.filter(|model| !model.trim().is_empty()) {
        config.model = model.to_owned();
    }
    ProviderFactory::build(&config, creds).map_err(AgentProcessProviderError::from_provider_error)
}

/// Build the scripted mock provider from `CONCERTO_MOCK_SCRIPT_JSON`.
///
/// Only reached through the explicit [`MOCK_PROVIDER`] opt-in.
pub fn build_mock_provider(
    script_json: Option<&str>,
) -> Result<MockProvider, AgentProcessProviderError> {
    match script_json {
        Some(script_json) if !script_json.trim().is_empty() => serde_json::from_str::<
            Vec<Vec<concerto_core::types::CompletionChunk>>,
        >(script_json)
        .map(MockProvider::scripted)
        .map_err(|error| AgentProcessProviderError::MockScript { reason: error.to_string() }),
        _ => Ok(MockProvider::default()),
    }
}

/// Whether `provider_env` selects the explicit mock opt-in.
pub fn selects_mock(provider_env: Option<&str>) -> bool {
    provider_env.map(str::trim).is_some_and(|value| value.eq_ignore_ascii_case(MOCK_PROVIDER))
}

/// A minimal `AppConfig` carrying exactly one provider, for tests and fixtures.
///
/// Kept beside the contract so tests exercise the same serialization the
/// parent stamps (ruling out a "tests build it differently" drift).
pub fn single_provider_config(provider: ProviderConfig) -> AppConfig {
    let mut config = AppConfig::default();
    let mut settings = concerto_config::ModelSettings::default();
    if !provider.model.trim().is_empty() {
        settings.global_default_model = Some(provider.model.clone());
    }
    settings.providers = vec![provider];
    config.model_settings = Some(settings);
    config
}

impl From<AgentProcessProviderError> for OrchestratorError {
    fn from(error: AgentProcessProviderError) -> Self {
        OrchestratorError::AgentLoopError(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    /// Serialises access to the process-global `CONCERTO_DEEPSEEK_API_KEY`
    /// env var across the tests in this module. `cargo test` runs tests in
    /// parallel threads within one process, so the `real_provider_*` tests that
    /// set the key race `missing_credential_*` which removes it — an
    /// intermittent false failure. Mirrors the crate-wide
    /// `CONCERTO_API_KEY_LOCK` convention in `crates/api-server/src/auth.rs`.
    static DEEPSEEK_API_KEY_LOCK: Mutex<()> = Mutex::new(());

    fn provider_config(provider: &str, model: &str) -> ProviderConfig {
        ProviderConfig {
            id: format!("{provider}-main"),
            name: format!("{provider} main"),
            provider: provider.to_owned(),
            model: model.to_owned(),
            keyring_key: format!("{provider}/api_key"),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn unset_config_env_is_a_named_missing_error() {
        let error = parse_app_config(None).expect_err("must fail closed");
        assert!(matches!(error, AgentProcessProviderError::MissingConfig { .. }));
        assert!(error.to_string().contains("refusing to fall back to mock"));
    }

    #[test]
    fn blank_config_env_is_a_named_missing_error() {
        let error = parse_app_config(Some("   ")).expect_err("blank must fail closed");
        assert!(matches!(error, AgentProcessProviderError::MissingConfig { .. }));
    }

    #[test]
    fn malformed_config_env_is_a_named_malformed_error() {
        let error = parse_app_config(Some("{ not json")).expect_err("malformed must fail");
        assert!(matches!(error, AgentProcessProviderError::MalformedConfig { .. }));
    }

    #[test]
    fn stamped_config_parses_and_resolves_the_default_model_provider() {
        let config = single_provider_config(provider_config("deepseek", "deepseek-chat"));
        let json = serde_json::to_string(&config).expect("config serializes");
        let parsed = parse_app_config(Some(&json)).expect("round-trip");
        let (resolved, model) = resolve_provider_config(&parsed).expect("one provider resolves");
        assert_eq!(resolved.provider, "deepseek");
        assert_eq!(model.as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn no_provider_is_a_named_error_not_a_mock() {
        let config = AppConfig::default();
        let error = resolve_provider_config(&config).expect_err("no provider must fail");
        assert!(matches!(error, AgentProcessProviderError::NoProviderConfigured { .. }));
    }

    #[test]
    fn mock_is_selected_only_by_the_explicit_opt_in() {
        assert!(selects_mock(Some("mock")));
        assert!(selects_mock(Some(" MOCK ")));
        assert!(!selects_mock(None), "unset must NOT select the mock");
        assert!(!selects_mock(Some("")));
        assert!(!selects_mock(Some("deepseek")));
    }

    #[test]
    fn mock_builds_without_a_script_and_rejects_a_bad_script() {
        assert!(build_mock_provider(None).is_ok());
        match build_mock_provider(Some("[not-a-script]")) {
            Err(AgentProcessProviderError::MockScript { .. }) => {}
            Err(other) => panic!("expected MockScript, got {other}"),
            Ok(_) => panic!("a malformed script must fail"),
        }
    }

    #[test]
    fn missing_credential_is_a_named_credential_error() {
        let _lock = DEEPSEEK_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::remove_var("CONCERTO_DEEPSEEK_API_KEY");
        let config = provider_config("deepseek", "deepseek-chat");
        let error = build_provider(&config, None, &CredentialStore::from_env())
            .err()
            .expect("missing credential must fail closed");
        assert!(matches!(error, AgentProcessProviderError::CredentialMissing { .. }));
        assert!(error.to_string().contains("credential missing"));
    }

    #[test]
    fn real_provider_builds_with_an_env_backed_credential() {
        let _lock = DEEPSEEK_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_DEEPSEEK_API_KEY", "sk-synthetic-fixture");
        let config = provider_config("deepseek", "deepseek-chat");
        let built = build_provider(&config, None, &CredentialStore::from_env());
        std::env::remove_var("CONCERTO_DEEPSEEK_API_KEY");
        match built {
            Ok(provider) => assert_eq!(provider.provider_name(), "deepseek"),
            Err(error) => panic!("build must succeed: {error}"),
        }
    }

    #[test]
    fn unsupported_provider_is_a_named_error() {
        let config = provider_config("not-a-real-provider", "x");
        let error = build_provider(&config, None, &CredentialStore::from_env())
            .err()
            .expect("unsupported must fail");
        assert!(matches!(error, AgentProcessProviderError::UnsupportedProvider { .. }));
    }

    #[test]
    fn model_override_applies_to_the_built_provider() {
        let _lock = DEEPSEEK_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_DEEPSEEK_API_KEY", "sk-synthetic-fixture");
        let config = provider_config("deepseek", "deepseek-chat");
        let built =
            build_provider(&config, Some("deepseek-reasoner"), &CredentialStore::from_env());
        std::env::remove_var("CONCERTO_DEEPSEEK_API_KEY");
        match built {
            Ok(provider) => assert_eq!(provider.provider_name(), "deepseek"),
            Err(error) => panic!("build with override must succeed: {error}"),
        }
    }

    #[test]
    fn single_provider_config_stamps_a_default_model() {
        let config = single_provider_config(provider_config("openai", "gpt-4o"));
        let settings = config.model_settings.expect("model settings");
        assert_eq!(settings.global_default_model.as_deref(), Some("gpt-4o"));
    }
}
