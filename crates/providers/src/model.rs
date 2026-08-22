//! `ModelProfile` — a fully-resolved model descriptor wrapping `RoutingProfile`
//! with computed metadata for per-agent dispatch decisions.

use concerto_core::types::RoutingProfile as CoreRoutingProfile;

/// A fully-resolved model profile with computed metadata.
///
/// Wraps `RoutingProfile` with additional runtime-derived fields such as
/// context window size, tool-calling support, and local/remote distinction.
/// Used by `ModelSelector` and `AgentRunner` for per-agent model dispatch.
#[derive(Debug, Clone)]
pub struct ModelProfile {
    /// The underlying routing profile (cost, latency, provider, model).
    pub profile: CoreRoutingProfile,
    /// Maximum context window size in tokens.
    pub context_window: u32,
    /// Whether the model supports tool/function calling.
    pub supports_tool_calling: bool,
    /// Optional custom API base URL.
    pub base_url: Option<String>,
    /// Optional human-readable description.
    pub description: Option<String>,
}

impl ModelProfile {
    /// Returns `true` if this model runs locally (Ollama or custom base URL).
    pub fn is_local(&self) -> bool {
        self.profile.provider == "ollama" || self.base_url.is_some()
    }

    /// Provider type string (e.g. "openai", "anthropic").
    pub fn provider(&self) -> &str {
        &self.profile.provider
    }

    /// Model name string (e.g. "gpt-4", "claude-3-opus").
    pub fn model_name(&self) -> &str {
        &self.profile.model
    }

    /// Cost per 1k tokens in USD.
    pub fn cost_per_1k_tokens(&self) -> f64 {
        self.profile.cost_per_1k_tokens
    }

    /// Average latency in milliseconds.
    pub fn avg_latency_ms(&self) -> u64 {
        self.profile.avg_latency_ms
    }
}
