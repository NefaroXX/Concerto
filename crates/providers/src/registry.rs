use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};

use concerto_core::types::RoutingProfile;
use concerto_core::types::{CompletionRequest, TokenBudget};
use concerto_core::CancellationToken;

/// Holds available providers with their associated model names.
pub struct ProviderRegistry {
    providers: Vec<RegisteredProvider>,
}

struct RegisteredProvider {
    provider: Box<dyn LlmProvider>,
    model: String,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self { providers: Vec::new() }
    }

    /// Register a provider with an optional model name.
    ///
    /// If `model` is `None`, the provider's `provider_name()` is used as the
    /// model identifier for routing purposes.
    pub fn register(&mut self, provider: Box<dyn LlmProvider>, model: Option<String>) {
        let model = model.unwrap_or_else(|| provider.provider_name().to_string());
        self.providers.push(RegisteredProvider { provider, model });
    }

    pub fn get(&self, name: &str) -> Option<&dyn LlmProvider> {
        self.providers
            .iter()
            .find(|p| p.provider.provider_name() == name)
            .map(|p| p.provider.as_ref())
    }

    pub fn all(&self) -> Vec<&dyn LlmProvider> {
        self.providers.iter().map(|p| p.provider.as_ref()).collect()
    }

    /// Return the model name for a given provider, if registered.
    pub fn model_for(&self, provider_name: &str) -> Option<&str> {
        self.providers
            .iter()
            .find(|p| p.provider.provider_name() == provider_name)
            .map(|p| p.model.as_str())
    }

    /// Return routing profiles for all registered providers.
    ///
    /// Each profile carries cost and latency data derived from the provider's
    /// pricing and typical response time:
    ///
    /// | Provider   | Cost per 1k tokens (avg) |
    /// |------------|--------------------------|
    /// | openai     | ~$0.006                  |
    /// | anthropic  | ~$0.009 (sonnet)         |
    /// | google     | ~$0.005                  |
    /// | openrouter | ~$0.003                  |
    /// | nim        | ~$0.001                  |
    /// | ollama     | ~$0.000                  |
    pub fn routing_profiles(&self) -> Vec<RoutingProfile> {
        self.providers
            .iter()
            .map(|rp| {
                let name = rp.provider.provider_name();
                let (cost_per_1k_tokens, avg_latency_ms) = match name {
                    "openai" => (0.006, 800),
                    "anthropic" => (0.009, 1200),
                    "google" => (0.005, 600),
                    "openrouter" => (0.003, 1000),
                    "ollama" => (0.000, 200),
                    "nim" => (0.001, 400),
                    _ => (0.005, 500),
                };
                RoutingProfile {
                    provider_config_id: name.to_string(),
                    provider: name.to_string(),
                    model: rp.model.clone(),
                    cost_per_1k_tokens,
                    avg_latency_ms,
                    context_window: 8192,
                    supports_tool_calling: true,
                    base_url: None,
                    description: None,
                }
            })
            .collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps a primary and fallback provider.
pub struct FallbackProvider {
    primary: Box<dyn LlmProvider>,
    fallback: Box<dyn LlmProvider>,
}

impl FallbackProvider {
    pub fn new(primary: Box<dyn LlmProvider>, fallback: Box<dyn LlmProvider>) -> Self {
        Self { primary, fallback }
    }

    pub fn primary(&self) -> &dyn LlmProvider {
        self.primary.as_ref()
    }

    pub fn fallback(&self) -> &dyn LlmProvider {
        self.fallback.as_ref()
    }
}

#[async_trait]
impl LlmProvider for FallbackProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        match self.primary.stream_completion(request.clone(), cancel.clone()).await {
            Ok(stream) => Ok(stream),
            Err(err) => {
                if matches!(err, ProviderError::AuthFailure | ProviderError::Cancelled) {
                    return Err(err);
                }
                tracing::warn!(
                    primary = self.primary.provider_name(),
                    fallback = self.fallback.provider_name(),
                    error = %err,
                    "primary provider failed; trying fallback"
                );
                self.fallback.stream_completion(request, cancel).await
            }
        }
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        self.primary.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        self.primary.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        "fallback"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use concerto_core::error::ProviderError;
    use concerto_core::traits::{CompletionStream, LlmProvider};
    use concerto_core::types::CompletionChunk;
    use concerto_core::types::{CompletionRequest, TokenBudget};
    use concerto_core::CancellationToken;
    use futures::StreamExt;

    #[derive(Clone, Copy)]
    enum MockBehavior {
        Success,
        AuthFailure,
        Cancelled,
        OtherError,
    }

    struct MockProvider {
        name: &'static str,
        behavior: MockBehavior,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            use futures::stream;
            match self.behavior {
                MockBehavior::Success => {
                    let chunk = CompletionChunk {
                        delta: "ok".into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: true,
                        usage: None,
                    };
                    let s = stream::once(async move { Ok(chunk) });
                    Ok(Box::pin(s))
                }
                MockBehavior::AuthFailure => Err(ProviderError::AuthFailure),
                MockBehavior::Cancelled => Err(ProviderError::Cancelled),
                MockBehavior::OtherError => Err(ProviderError::Other("other error".into())),
            }
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(8000, 4000)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        fn provider_name(&self) -> &'static str {
            self.name
        }
    }

    #[test]
    fn register_and_get() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            Box::new(MockProvider { name: "openai", behavior: MockBehavior::Success }),
            Some("gpt-4".into()),
        );
        assert!(reg.get("openai").is_some());
        assert!(reg.get("anthropic").is_none());
    }

    #[test]
    fn routing_profiles_have_real_data() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            Box::new(MockProvider { name: "openai", behavior: MockBehavior::Success }),
            Some("gpt-4".into()),
        );
        reg.register(
            Box::new(MockProvider { name: "anthropic", behavior: MockBehavior::Success }),
            Some("claude-sonnet-4".into()),
        );
        reg.register(
            Box::new(MockProvider { name: "ollama", behavior: MockBehavior::Success }),
            None,
        );

        let profiles = reg.routing_profiles();
        assert_eq!(profiles.len(), 3);

        // OpenAI should have non-zero estimated cost.
        let openai = profiles.iter().find(|p| p.provider == "openai").unwrap();
        assert!(openai.cost_per_1k_tokens > 0.0);
        assert_eq!(openai.model, "gpt-4");

        // Anthropic should be the most expensive configured remote provider.
        let anthropic = profiles.iter().find(|p| p.provider == "anthropic").unwrap();
        assert!(anthropic.cost_per_1k_tokens > openai.cost_per_1k_tokens);

        // Ollama should have zero estimated API cost.
        let ollama = profiles.iter().find(|p| p.provider == "ollama").unwrap();
        assert_eq!(ollama.cost_per_1k_tokens, 0.0);
        // No model given → uses provider name
        assert_eq!(ollama.model, "ollama");
    }

    #[test]
    fn model_for_returns_registered_model() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            Box::new(MockProvider { name: "openai", behavior: MockBehavior::Success }),
            Some("gpt-4-turbo".into()),
        );
        assert_eq!(reg.model_for("openai"), Some("gpt-4-turbo"));
        assert_eq!(reg.model_for("anthropic"), None);
    }

    #[tokio::test]
    async fn fallback_primary_success() {
        let primary = Box::new(MockProvider { name: "primary", behavior: MockBehavior::Success });
        let fallback =
            Box::new(MockProvider { name: "fallback", behavior: MockBehavior::OtherError });
        let fb = FallbackProvider::new(primary, fallback);
        let req = CompletionRequest::default();
        let cancel = CancellationToken::new();
        let stream = fb.stream_completion(req, cancel).await.expect("should succeed");
        let mut s = stream;
        let first = s.next().await.unwrap().expect("chunk ok");
        assert_eq!(first.delta, "ok");
    }

    #[tokio::test]
    async fn fallback_primary_retryable_uses_fallback() {
        let primary =
            Box::new(MockProvider { name: "primary", behavior: MockBehavior::OtherError });
        let fallback = Box::new(MockProvider { name: "fallback", behavior: MockBehavior::Success });
        let fb = FallbackProvider::new(primary, fallback);
        let req = CompletionRequest::default();
        let cancel = CancellationToken::new();
        let stream = fb.stream_completion(req, cancel).await.expect("fallback should succeed");
        let mut s = stream;
        let first = s.next().await.unwrap().expect("chunk ok");
        assert_eq!(first.delta, "ok");
    }

    #[tokio::test]
    async fn fallback_primary_auth_failure_no_fallback() {
        let primary =
            Box::new(MockProvider { name: "primary", behavior: MockBehavior::AuthFailure });
        let fallback = Box::new(MockProvider { name: "fallback", behavior: MockBehavior::Success });
        let fb = FallbackProvider::new(primary, fallback);
        let req = CompletionRequest::default();
        let cancel = CancellationToken::new();
        match fb.stream_completion(req, cancel).await {
            Err(err) => assert!(matches!(err, ProviderError::AuthFailure)),
            Ok(_) => panic!("expected auth failure error"),
        }
    }

    #[tokio::test]
    async fn fallback_primary_cancelled_no_fallback() {
        let primary = Box::new(MockProvider { name: "primary", behavior: MockBehavior::Cancelled });
        let fallback = Box::new(MockProvider { name: "fallback", behavior: MockBehavior::Success });
        let fb = FallbackProvider::new(primary, fallback);
        let req = CompletionRequest::default();
        let cancel = CancellationToken::new();
        match fb.stream_completion(req, cancel).await {
            Err(err) => assert!(matches!(err, ProviderError::Cancelled)),
            Ok(_) => panic!("expected cancelled error"),
        }
    }
}
