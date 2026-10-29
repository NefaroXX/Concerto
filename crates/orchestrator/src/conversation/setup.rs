//! Shared conversation preflight. Presentation adapters may display its reason,
//! but cannot impose their own provider/model or assignment requirements.

use concerto_config::{AgentModelAssignment, AppConfig, CredentialStore, ProviderConfig};
use concerto_providers::factory::ProviderFactory;
use concerto_providers::provider_defs::{provider_definition, provider_readiness};

use super::ConversationOptions;

pub fn validate_provider_setup(
    providers: &[ProviderConfig],
    assignments: &[AgentModelAssignment],
    provider_id: &str,
    model: &str,
) -> Option<String> {
    if providers.is_empty() {
        return Some("no providers are configured".into());
    }
    let credentials = CredentialStore::new();
    let ready = |provider: &ProviderConfig, selected_model: &str| {
        let mut resolved = provider.clone();
        if !selected_model.trim().is_empty() {
            resolved.model = selected_model.to_owned();
        }
        let definition = provider_definition(&resolved.provider);
        provider_readiness(&resolved, &definition, credentials.exists(&resolved.keyring_key))
            .is_ready()
    };
    let Some(provider) = providers.iter().find(|provider| provider.id == provider_id) else {
        return Some("no active provider is selected".into());
    };
    if !ready(provider, model) {
        return Some(format!(
            "active provider '{}' is not ready (missing model or required API key)",
            provider.name
        ));
    }
    for assignment in assignments {
        let valid = providers
            .iter()
            .find(|provider| provider.id == assignment.provider_config_id)
            .zip(assignment.model_override.as_deref().filter(|model| !model.trim().is_empty()))
            .is_some_and(|(provider, model)| ready(provider, model));
        if !valid {
            return Some(format!(
                "agent role '{}' is assigned to an incomplete provider/model",
                assignment.agent_role
            ));
        }
    }
    None
}

pub(super) fn validate_options(
    config: &AppConfig,
    options: &ConversationOptions,
) -> Option<String> {
    // Legacy/env/plugin routes are resolved by the execution factory. Do not
    // prohibit those routes merely because they have no model-settings rows.
    let settings = config.model_settings.as_ref()?;
    if settings.providers.is_empty() {
        return None;
    }
    let model = options
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .or(settings.global_default_model.as_deref())
        .unwrap_or("");
    let provider = options
        .provider_id
        .as_deref()
        .and_then(|id| settings.providers.iter().find(|provider| provider.id == id))
        .or_else(|| ProviderFactory::config_for_model(settings, model, None))
        .or_else(|| settings.providers.first())?;
    validate_provider_setup(&settings.providers, &settings.agent_assignments, &provider.id, model)
}
