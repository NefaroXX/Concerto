//! `ModelRegistry` — stores and looks up `ModelProfile`s built from
//! `RoutingProfile`s and optional config-level overrides.

use std::collections::HashMap;

use concerto_core::types::RoutingProfile;

use crate::model::ModelProfile;

/// Manages a collection of `ModelProfile`s, built from `RoutingProfile`s
/// and optional metadata overrides from config.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    profiles: HashMap<(String, String), ModelProfile>,
}

impl ModelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `ModelProfile` by stable provider config ID and model name.
    pub fn register(&mut self, profile: ModelProfile) {
        let key = (profile.profile.provider_config_id.clone(), profile.model_name().to_string());
        self.profiles.insert(key, profile);
    }

    /// Look up an exact configured provider/model pair.
    pub fn get(&self, provider_config_id: &str, model_name: &str) -> Option<&ModelProfile> {
        self.profiles.get(&(provider_config_id.to_string(), model_name.to_string()))
    }

    /// Return all registered profiles.
    pub fn all(&self) -> Vec<&ModelProfile> {
        self.profiles.values().collect()
    }

    /// Build a registry from a list of `RoutingProfile`s, wrapping each
    /// into a `ModelProfile` with defaults for missing metadata.
    ///
    /// Config-level overrides (`ModelProfileOverride`) are expected to have
    /// already been applied to the `RoutingProfile` fields (see
    /// `ProviderFactory::build_profiles()`).
    pub fn from_profiles(routing_profiles: Vec<RoutingProfile>) -> Self {
        let mut reg = Self::new();
        for rp in routing_profiles {
            let context_window = rp.context_window;
            let supports_tool_calling = rp.supports_tool_calling;
            let base_url = rp.base_url.clone();
            let description = rp.description.clone();
            reg.register(ModelProfile {
                profile: rp,
                context_window,
                supports_tool_calling,
                base_url,
                description,
            });
        }
        reg
    }
}
