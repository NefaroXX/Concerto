//! Model/provider pinning coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24C): the three `legacy_pins_from_config`
//! model-override tests, the `EnvVarGuard` RAII env-var restore, and the five
//! custom-agent `provider_id` pin-resolution tests move verbatim out of
//! `runtime_runner::runtime_runner_tests`, so test names and assertions are
//! unchanged. `EnvVarGuard` moves WITH the tests (never duplicated) — its sole
//! user is `custom_agent_provider_id_wins_over_default_when_it_offers_the_model`
//! inside this span. `use super::super::*;` keeps the parent
//! (`runtime_runner_impl`) items in scope; this cluster touches none of the old
//! test-mod imports (its config types are imported per-test).

use super::super::*;

// ------------------------------------------------------------------
// legacy_pins_from_config (per-agent model pins in the fallback chain)
// ------------------------------------------------------------------

#[test]
fn custom_agent_model_override_feeds_legacy_pins_without_assignments() {
    // Config with NO `model_settings.agent_assignments` but a custom
    // agent pinning a model on the "coder" role: the resolved model must
    // come from the override (via the legacy-pin fallback), not the
    // provider default.
    let multi_agent = concerto_config::MultiAgentConfig {
        custom_agents: vec![concerto_config::CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("coder-model-x".into()),
            ..Default::default()
        }],
        ..Default::default()
    };

    let pins = legacy_pins_from_config(&Some(multi_agent));

    assert_eq!(
        pins.get(&AgentId::new("coder")),
        Some(&"coder-model-x".to_string()),
        "per-agent model pin must feed the legacy fallback used when no assignment exists"
    );
}

#[test]
fn legacy_pins_preserve_model_pins_and_skip_unset_overrides() {
    // Explicit `model_pins` survive; custom agents without a non-empty
    // `model_override` add no pin.
    let multi_agent = concerto_config::MultiAgentConfig {
        model_pins: std::collections::HashMap::from([(
            AgentId::new("researcher"),
            "researcher-model".to_string(),
        )]),
        custom_agents: vec![
            concerto_config::CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                model_override: Some("   ".into()),
                ..Default::default()
            },
            concerto_config::CustomAgentConfig {
                id: "docs".into(),
                name: "Docs".into(),
                role: "docs-writer".into(),
                model_override: None,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let pins = legacy_pins_from_config(&Some(multi_agent));

    assert_eq!(
        pins.get(&AgentId::new("researcher")),
        Some(&"researcher-model".to_string()),
        "explicit model_pins are preserved"
    );
    assert!(
        !pins.contains_key(&AgentId::new("coder")),
        "whitespace-only model_override is skipped"
    );
    assert!(
        !pins.contains_key(&AgentId::new("docs-writer")),
        "custom agent without model_override adds no pin"
    );
}

/// Maintainer decision 2026-09: the coordinator always follows the run's
/// global default model. Even when a legacy `model_pins` entry names
/// `coordinator`, resolution ignores it — the pin stays parsed (user data
/// preserved) but is inert for the coordinator.
#[test]
fn coordinator_model_ignores_configured_pins() {
    let multi_agent = concerto_config::MultiAgentConfig {
        model_pins: std::collections::HashMap::from([(
            AgentId::new("coordinator"),
            "pinned-coordinator-model".to_string(),
        )]),
        ..Default::default()
    };
    let pins = legacy_pins_from_config(&Some(multi_agent));
    assert_eq!(
        pins.get(&AgentId::new("coordinator")),
        Some(&"pinned-coordinator-model".to_string()),
        "the legacy pin is still parsed (user data preserved)"
    );
    assert_eq!(
        resolve_coordinator_model(&pins, "global-default-model"),
        "global-default-model",
        "the coordinator must ignore the pin and use the global default"
    );
}

// ------------------------------------------------------------------
// Custom-agent provider pin resolution (role serving pipe)
// ------------------------------------------------------------------

/// Sets an env var and restores the previous value (or removes it) on drop.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var(self.key, previous),
            None => std::env::remove_var(self.key),
        }
    }
}

/// A custom agent's `provider_id` must select the serving pipe for its
/// role when no `agent_assignments` entry exists — even when the run's
/// global default resolves to a different provider. Historically the pin
/// was ignored, so a google-pinned role was dispatched to the default
/// (nim) provider carrying a Google model id → HTTP 404 on every dispatch.
#[test]
fn custom_agent_provider_id_wins_over_default_when_it_offers_the_model() {
    use concerto_config::{CustomAgentConfig, ModelSettings, MultiAgentConfig, ProviderConfig};

    let google = ProviderConfig {
        id: "google".into(),
        provider: "google".into(),
        model: "gemini-2.5-flash-lite".into(),
        keyring_key: "google_pin_test".into(),
        cached_models: vec!["gemini-2.5-flash-lite".into()],
        ..Default::default()
    };
    let nim = ProviderConfig {
        id: "nim".into(),
        provider: "nim".into(),
        model: "nim-default-model".into(),
        keyring_key: "nim_pin_test".into(),
        ..Default::default()
    };
    let settings = ModelSettings {
        providers: vec![nim.clone(), google],
        global_default_model: Some("nim-default-model".into()),
        ..Default::default()
    };
    let multi_agent = MultiAgentConfig {
        custom_agents: vec![CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("gemini-2.5-flash-lite".into()),
            provider_id: Some("google".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let agent_configs = build_agent_config_map(&Some(multi_agent.clone()));
    let legacy_pins = legacy_pins_from_config(&Some(multi_agent));
    let role = AgentId::new("coder");
    let model = legacy_pins.get(&role).cloned().unwrap_or_default();
    assert_eq!(model, "gemini-2.5-flash-lite", "legacy pin model resolution unchanged");

    // No `agent_assignments` entry: the custom-agent provider pin decides.
    let chosen = resolve_role_provider_config(&settings, &agent_configs, &role, None, &model, &nim);
    let provider_id = ProviderFactory::config_id(chosen);
    assert_eq!(provider_id, "google", "custom-agent provider pin must be honoured");
    assert_ne!(
        provider_id,
        ProviderFactory::config_id(&nim),
        "the default (nim) provider must not serve a google-pinned role"
    );

    // The recorded provider-pin map carries the honoured id, not the
    // default, so RoutingDecided events report google.
    let provider_pins = HashMap::from([(role.clone(), provider_id)]);
    assert_eq!(provider_pins.get(&role).map(String::as_str), Some("google"));

    // And the built provider is Google, not Nim.
    let _guard = EnvVarGuard::set("CONCERTO_GOOGLE_PIN_TEST", "test-key");
    let store = CredentialStore::from_env();
    let mut resolved_config = chosen.clone();
    resolved_config.model = model;
    let built =
        ProviderFactory::build(&resolved_config, &store).expect("google provider must build");
    assert_eq!(built.provider_name(), "google");
    assert_ne!(built.provider_name(), "nim");
}

/// A stale custom-agent `provider_id` (provider removed) keeps the existing
/// silent fallback to the default provider — no error.
#[test]
fn stale_custom_agent_provider_id_falls_back_to_default() {
    use concerto_config::{CustomAgentConfig, ModelSettings, MultiAgentConfig, ProviderConfig};

    let nim = ProviderConfig {
        id: "nim".into(),
        provider: "nim".into(),
        model: "nim-default-model".into(),
        keyring_key: "nim_pin_test".into(),
        ..Default::default()
    };
    let settings = ModelSettings {
        providers: vec![nim.clone()],
        global_default_model: Some("nim-default-model".into()),
        ..Default::default()
    };
    let multi_agent = MultiAgentConfig {
        custom_agents: vec![CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("gemini-2.5-flash-lite".into()),
            provider_id: Some("removed-provider".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let agent_configs = build_agent_config_map(&Some(multi_agent.clone()));
    let legacy_pins = legacy_pins_from_config(&Some(multi_agent));
    let role = AgentId::new("coder");
    let model = legacy_pins.get(&role).cloned().unwrap_or_default();

    let chosen = resolve_role_provider_config(&settings, &agent_configs, &role, None, &model, &nim);
    assert_eq!(
        ProviderFactory::config_id(chosen),
        "nim",
        "a stale provider_id must fall back to the default provider without erroring"
    );
}

/// A live custom-agent provider that does not offer the resolved model is
/// ignored, keeping the default provider (never a guaranteed 404 pipe).
#[test]
fn custom_agent_provider_without_the_model_falls_back_to_default() {
    use concerto_config::{CustomAgentConfig, ModelSettings, MultiAgentConfig, ProviderConfig};

    let openai = ProviderConfig {
        id: "openai".into(),
        provider: "openai".into(),
        model: "gpt-4o".into(),
        keyring_key: "openai_pin_test".into(),
        ..Default::default()
    };
    let nim = ProviderConfig {
        id: "nim".into(),
        provider: "nim".into(),
        model: "nim-default-model".into(),
        keyring_key: "nim_pin_test".into(),
        ..Default::default()
    };
    let settings = ModelSettings {
        providers: vec![openai, nim.clone()],
        global_default_model: Some("nim-default-model".into()),
        ..Default::default()
    };
    let multi_agent = MultiAgentConfig {
        custom_agents: vec![CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("gemini-2.5-flash-lite".into()),
            provider_id: Some("openai".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let agent_configs = build_agent_config_map(&Some(multi_agent.clone()));
    let legacy_pins = legacy_pins_from_config(&Some(multi_agent));
    let role = AgentId::new("coder");
    let model = legacy_pins.get(&role).cloned().unwrap_or_default();

    let chosen = resolve_role_provider_config(&settings, &agent_configs, &role, None, &model, &nim);
    assert_eq!(
        ProviderFactory::config_id(chosen),
        "nim",
        "a custom provider that does not offer the model must not serve the role"
    );
}

/// With no custom-agent provider pin the behavior is unchanged: the run
/// default provider serves the role.
#[test]
fn no_custom_agent_provider_keeps_default_provider() {
    use concerto_config::{CustomAgentConfig, ModelSettings, MultiAgentConfig, ProviderConfig};

    let google = ProviderConfig {
        id: "google".into(),
        provider: "google".into(),
        model: "gemini-2.5-flash-lite".into(),
        keyring_key: "google_pin_test".into(),
        ..Default::default()
    };
    let nim = ProviderConfig {
        id: "nim".into(),
        provider: "nim".into(),
        model: "nim-default-model".into(),
        keyring_key: "nim_pin_test".into(),
        ..Default::default()
    };
    let settings = ModelSettings {
        providers: vec![google, nim.clone()],
        global_default_model: Some("nim-default-model".into()),
        ..Default::default()
    };
    // A custom agent with a model pin but no provider pin.
    let multi_agent = MultiAgentConfig {
        custom_agents: vec![CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("nim-default-model".into()),
            provider_id: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let agent_configs = build_agent_config_map(&Some(multi_agent.clone()));
    let legacy_pins = legacy_pins_from_config(&Some(multi_agent));
    let role = AgentId::new("coder");
    let model = legacy_pins.get(&role).cloned().unwrap_or_default();

    let chosen = resolve_role_provider_config(&settings, &agent_configs, &role, None, &model, &nim);
    assert_eq!(
        ProviderFactory::config_id(chosen),
        "nim",
        "no custom-agent provider pin keeps the run default provider"
    );
}

/// ADR-31: an explicit, valid `agent_assignments` entry wins over a
/// custom-agent `provider_id` when both are present.
#[test]
fn explicit_assignment_wins_over_custom_agent_provider_pin() {
    use concerto_config::{CustomAgentConfig, ModelSettings, MultiAgentConfig, ProviderConfig};

    let assignment_provider = ProviderConfig {
        id: "anthropic".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet".into(),
        keyring_key: "anthropic_pin_test".into(),
        ..Default::default()
    };
    let google = ProviderConfig {
        id: "google".into(),
        provider: "google".into(),
        model: "gemini-2.5-flash-lite".into(),
        keyring_key: "google_pin_test".into(),
        ..Default::default()
    };
    let nim = ProviderConfig {
        id: "nim".into(),
        provider: "nim".into(),
        model: "nim-default-model".into(),
        keyring_key: "nim_pin_test".into(),
        ..Default::default()
    };
    let settings = ModelSettings {
        providers: vec![assignment_provider.clone(), google, nim.clone()],
        global_default_model: Some("nim-default-model".into()),
        ..Default::default()
    };
    let multi_agent = MultiAgentConfig {
        custom_agents: vec![CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            model_override: Some("gemini-2.5-flash-lite".into()),
            provider_id: Some("google".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let agent_configs = build_agent_config_map(&Some(multi_agent));

    let chosen = resolve_role_provider_config(
        &settings,
        &agent_configs,
        &AgentId::new("coder"),
        Some(&assignment_provider),
        "claude-sonnet",
        &nim,
    );
    assert_eq!(
        ProviderFactory::config_id(chosen),
        "anthropic",
        "an explicit assignment must win over the custom-agent provider pin"
    );
}
