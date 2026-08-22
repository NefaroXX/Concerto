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

/// Static metadata describing one provider type.
///
/// Every shipped model ID in [`ProviderDefinition::known_models`] MUST be a real
/// API identifier for that provider's API. Live discovery and custom entry cover
/// the long tail, so this catalog is deliberately small and hand-maintained.
#[derive(Debug, Clone)]
pub struct ProviderDefinition {
    pub id: &'static str,
    pub display_name: String,
    pub default_model: Option<&'static str>,
    pub known_models: &'static [&'static str],
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
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "anthropic" => ProviderDefinition {
            id: "anthropic",
            display_name: String::from("Anthropic"),
            default_model: Some("claude-3-5-sonnet-latest"),
            known_models: ANTHROPIC_KNOWN,
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "google" => ProviderDefinition {
            id: "google",
            display_name: String::from("Google"),
            default_model: Some("gemini-1.5-pro"),
            known_models: GOOGLE_KNOWN,
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "openrouter" => ProviderDefinition {
            id: "openrouter",
            display_name: String::from("OpenRouter"),
            default_model: None,
            known_models: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "nim" => ProviderDefinition {
            id: "nim",
            display_name: String::from("NVIDIA NIM"),
            default_model: None,
            known_models: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "ollama" => ProviderDefinition {
            id: "ollama",
            display_name: String::from("Ollama (local)"),
            default_model: None,
            known_models: &[],
            credential_requirement: CredentialRequirement::None,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        "opencode" => ProviderDefinition {
            id: "opencode",
            display_name: String::from("OpenCode Zen"),
            default_model: None,
            known_models: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Supported,
            allows_custom_model: true,
        },
        _ => ProviderDefinition {
            id: "<unknown>",
            display_name: provider_type.to_string(),
            default_model: None,
            known_models: &[],
            credential_requirement: CredentialRequirement::Required,
            model_discovery: ModelDiscoverySupport::Unsupported,
            allows_custom_model: true,
        },
    }
}

/// Recognized provider type ids, in UI display order.
pub const PROVIDER_TYPE_IDS: &[&str] =
    &["anthropic", "openai", "google", "openrouter", "nim", "ollama", "opencode"];

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

    // 3. Static known models.
    for m in definition.known_models {
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

    #[test]
    fn ollama_requires_no_credential() {
        assert_eq!(
            provider_definition("ollama").credential_requirement,
            CredentialRequirement::None
        );
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
