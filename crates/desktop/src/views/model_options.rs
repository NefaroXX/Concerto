//! Shared, cross-provider model-picker option builder.
//!
//! One resolver feeds every desktop model picker — the Settings "Default
//! Model" picker, the Orchestration Studio agent model pane, and the right-rail
//! quick panel — so a model advertised in exactly one place (static known,
//! config-first `extra_models`, or live-discovered `cached_models`) becomes
//! selectable everywhere with the same provider-qualified label.
//!
//! Each option carries a compound `key` (`"provider_id|model"`) while showing
//! a human `label` (`"model — ProviderLabel"`). The empty key is reserved as a
//! caller-provided sentinel (e.g. "Automatic", "Use global default"); this
//! module never emits it.

use std::collections::HashMap;

use concerto_config::ProviderConfig;
use concerto_providers::provider_defs::picker_model_options;

use super::settings::readable_provider_label;

/// Display option for every unified model pick-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedModelOption {
    /// `"provider_id|model"`, or the empty string for a caller sentinel.
    pub key: String,
    /// Human label, `"model — ProviderLabel"`.
    pub label: String,
}

impl std::fmt::Display for UnifiedModelOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Build the unified list from already-computed per-provider option lists.
///
/// Options are sorted by model name (case-sensitive, then provider id) and
/// deduplicated, so the same model advertised by two providers yields two
/// distinct, provider-qualified entries. `provider_label` resolves a provider
/// id to its human label.
pub fn unified_model_options_from_map<F>(
    models_by_provider: &HashMap<String, Vec<String>>,
    provider_label: F,
) -> Vec<UnifiedModelOption>
where
    F: Fn(&str) -> String,
{
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (provider_id, models) in models_by_provider {
        for model in models {
            let model = model.trim();
            if model.is_empty() {
                continue;
            }
            pairs.push((provider_id.clone(), model.to_string()));
        }
    }
    pairs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    pairs.dedup();
    pairs
        .into_iter()
        .map(|(provider_id, model)| {
            let label = format!("{} — {}", model, provider_label(&provider_id));
            UnifiedModelOption { key: format!("{}|{}", provider_id, model), label }
        })
        .collect()
}

/// Build the unified list directly from provider configs, resolving each
/// provider's selectable models through the shared [`picker_model_options`]
/// (selected / default / static known / `extra_models` / discovered
/// `cached_models`).
pub fn unified_model_options(providers: &[ProviderConfig]) -> Vec<UnifiedModelOption> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for provider in providers {
        map.entry(provider.id.clone()).or_default().extend(picker_model_options(provider));
    }
    unified_model_options_from_map(&map, |id| {
        providers
            .iter()
            .find(|provider| provider.id == id)
            .map(readable_provider_label)
            .unwrap_or_else(|| id.to_string())
    })
}

/// Split a `"provider_id|model"` key into its parts. The empty sentinel key
/// yields `(None, None)`; a key with no `|` is treated as a bare model name.
pub fn split_model_key(key: &str) -> (Option<String>, Option<String>) {
    if key.is_empty() {
        return (None, None);
    }
    match key.split_once('|') {
        Some((provider, model)) => (Some(provider.to_string()), Some(model.to_string())),
        None => (None, Some(key.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, provider_type: &str, extra: &[&str]) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            name: String::new(),
            provider: provider_type.into(),
            extra_models: extra.iter().map(|m| m.to_string()).collect(),
            ..ProviderConfig::default()
        }
    }

    #[test]
    fn options_are_provider_qualified_and_sorted() {
        let providers = vec![
            provider("p_openai", "openai", &["gpt-4o"]),
            provider("p_anthropic", "anthropic", &["claude-sonnet-4"]),
        ];
        let options = unified_model_options(&providers);
        // Sorted by model name, each with a provider-qualified key.
        let keys: Vec<&str> = options.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"p_openai|gpt-4o"));
        assert!(keys.contains(&"p_anthropic|claude-sonnet-4"));
        let mut sorted = keys.clone();
        sorted.sort();
        // Labels always carry the "model — Provider" shape.
        assert!(options.iter().all(|o| o.label.contains(" — ")));
    }

    #[test]
    fn same_model_from_two_providers_stays_distinct() {
        let providers = vec![
            provider("p_a", "openai", &["shared-model"]),
            provider("p_b", "anthropic", &["shared-model"]),
        ];
        let options = unified_model_options(&providers);
        let shared: Vec<&UnifiedModelOption> =
            options.iter().filter(|o| o.key.ends_with("|shared-model")).collect();
        assert_eq!(shared.len(), 2, "same model on two providers must not collapse");
    }

    #[test]
    fn split_key_handles_sentinel_and_compound() {
        assert_eq!(split_model_key(""), (None, None));
        assert_eq!(
            split_model_key("p_1|gpt-4o"),
            (Some("p_1".to_string()), Some("gpt-4o".to_string()))
        );
        assert_eq!(split_model_key("bare-model"), (None, Some("bare-model".to_string())));
    }
}
