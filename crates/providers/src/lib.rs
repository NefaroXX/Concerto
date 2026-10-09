#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-providers` — Multi-provider LLM completions with streaming,
//! tool-call normalization, retry/backoff, fallback chains, and context
//! window budget management.

pub mod budget;
pub mod context_guard;
pub mod factory;
pub mod metered;
pub mod metrics;
pub mod model;
pub mod model_registry;
pub mod model_selector;
pub mod protocol;
pub mod provider_defs;
pub mod registry;
pub mod retry;
pub mod routing;
pub mod tokenizer;

pub mod adapters;
pub mod anthropic;
pub mod capability;
pub mod cerebras;
pub mod cohere;
pub mod dashscope;
pub mod deepinfra;
pub mod deepseek;
pub mod fireworks;
pub mod google;
pub mod groq;
pub mod mistral;
pub mod moonshot;
pub mod nim;
pub mod novita;
pub mod ollama;
pub mod openai;
pub mod opencode;
pub mod openrouter;
pub mod perplexity;
pub mod sambanova;
pub mod sse;
// Shared Anthropic-dialect SSE stream state machine (crate-internal: the
// anthropic and opencode connectors share one copy; not part of the public
// API surface).
mod sse_state;
pub mod together;
pub mod tool_args;
pub mod xai;
pub mod zhipu;

#[cfg(test)]
pub mod testing;

/// Mock provider for evaluation and testing without real API keys.
/// Public to allow the eval crate to instantiate it directly.
pub mod mock;

// Re-export ModelInfo for convenience
pub use concerto_core::types::ModelInfo;

/// Default HTTP connect timeout (seconds) used where no per-provider config exists.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// User-Agent presented to upstream APIs.
///
/// MUST stay opencode-shaped (`opencode/<version>`). Verified 2026-08-13:
/// the OpenCode Zen gateway UA-gates its free-tier pilot models — an
/// anonymous request with `User-Agent: opencode/1.0` gets HTTP 200 for
/// `deepseek-v4-flash-free`/`big-pickle` while the exact same request with a
/// `reqwest`/`curl`/absent UA gets `429 FreeUsageLimitError: Error from
/// provider (Console): Rate limit exceeded` — regardless of API key or
/// account. The 429 is client-identification, not pool exhaustion. Do not
/// replace this with `concerto/<version>`; the free pilots will stop
/// serving.
const DEFAULT_USER_AGENT: &str = "opencode/1.0";

/// Build a `reqwest::Client` with a bounded connect timeout.
///
/// Only the connection-establishment phase is bounded; a legitimately
/// slow but progressing stream is never cut off. Without this, a silently
/// dropped TCP/TLS handshake (e.g. a firewall dropping packets) hangs the
/// request forever with no error and no timeout — the calling `iced::Task`
/// never resolves and the UI shows nothing.
///
/// The timeout is clamped to `[5, 60]` seconds to keep behavior predictable
/// regardless of source-configured values.
pub(crate) fn new_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(timeout_secs.clamp(5, 60)))
        .build()
        .expect("failed to build HTTP client")
}

/// Async helper: list available models for a given provider configuration.
///
/// Constructs a temporary provider instance and calls its `list_models` method.
/// Returns the provider's [`ModelInfo`] entries — **including** any
/// provider-advertised capability metadata ([`ModelInfo::supports_tool_calling`],
/// ADR-66 §3 precedence level 2 / ADR-75) — rather than collapsing them to bare
/// ids. Callers that only need names map `.id`; callers that persist a catalog
/// (`ProviderConfig::record_discovered_models`) keep the advertised capability
/// and thread it into `ProviderFactory::build`, so provider-advertised
/// capability actually reaches capability resolution.
///
/// Returns an empty `Vec` on any error (network failure, auth, etc.) so callers
/// can gracefully fall back to a free-text model prompt.
///
/// The signature cannot report the failure, so an error is logged as a warning
/// (naming the provider type) before it is collapsed — otherwise a failed fetch
/// is indistinguishable from a provider that genuinely publishes zero models.
pub async fn list_models_for_provider_async(
    provider_type: &str,
    api_key: &str,
    api_base: Option<&str>,
) -> Vec<ModelInfo> {
    use concerto_core::traits::LlmProvider;

    let cancel = concerto_core::CancellationToken::new();
    let result = match provider_type {
        "anthropic" => {
            let p = anthropic::AnthropicProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            p.list_models(cancel.clone()).await
        }
        "openai" => {
            let mut p = openai::OpenAiProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "openrouter" => {
            let p = openrouter::OpenRouterProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            p.list_models(cancel.clone()).await
        }
        "nim" => {
            let p = nim::NimProvider::new(api_key.to_string(), String::new(), DEFAULT_TIMEOUT_SECS);
            p.list_models(cancel.clone()).await
        }
        "google" => {
            let p = google::GoogleProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            p.list_models(cancel.clone()).await
        }
        "ollama" => {
            let mut p = ollama::OllamaProvider::new(String::new(), DEFAULT_TIMEOUT_SECS);
            if let Some(base) = api_base {
                p = p.with_base_url(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        // Both OpenCode provider types share the connector; only the default
        // relay differs (`opencode` → Zen, `opencode-free` → Go) and a config
        // `api_base` overrides either.
        "opencode" | "opencode-free" => {
            let p = opencode::OpenCodeZenProvider::with_api_base(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
                opencode::OpenCodeZenProvider::resolve_api_base(provider_type, api_base),
            );
            p.list_models(cancel.clone()).await
        }
        "deepseek" => {
            let p = deepseek::DeepSeekProvider::with_api_base(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
                api_base.unwrap_or(deepseek::DEEPSEEK_API_BASE).to_string(),
            );
            p.list_models(cancel.clone()).await
        }
        "groq" => {
            let mut p =
                groq::GroqProvider::new(api_key.to_string(), String::new(), DEFAULT_TIMEOUT_SECS);
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "together" => {
            let mut p = together::TogetherProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "mistral" => {
            let mut p = mistral::MistralProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "xai" => {
            let mut p =
                xai::XaiProvider::new(api_key.to_string(), String::new(), DEFAULT_TIMEOUT_SECS);
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "fireworks" => {
            let mut p = fireworks::FireworksProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "cerebras" => {
            let mut p = cerebras::CerebrasProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "cohere" => {
            let mut p = cohere::CohereProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "dashscope" => {
            let mut p = dashscope::DashScopeProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "deepinfra" => {
            let mut p = deepinfra::DeepInfraProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "moonshot" => {
            let mut p = moonshot::MoonshotProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "novita" => {
            let mut p = novita::NovitaProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "perplexity" => {
            let mut p = perplexity::PerplexityProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "sambanova" => {
            let mut p = sambanova::SambaNovaProvider::new(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
            );
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        "zhipu" => {
            let mut p =
                zhipu::ZhipuProvider::new(api_key.to_string(), String::new(), DEFAULT_TIMEOUT_SECS);
            if let Some(base) = api_base {
                p = p.with_api_base(base.to_string());
            }
            p.list_models(cancel.clone()).await
        }
        _ => return Vec::new(),
    };
    match result {
        Ok(models) => models,
        Err(error) => {
            // The signature is fixed (every caller receives a plain `Vec`), so
            // the failure has to be visible here: without this log a network,
            // auth, or parse error silently becomes an empty list that is
            // indistinguishable from a provider advertising zero models.
            tracing::warn!(
                provider_type,
                %error,
                "model discovery failed; returning an empty model list"
            );
            Vec::new()
        }
    }
}

/// Blocking helper: list available models for a given provider configuration.
///
/// Creates a single-threaded tokio runtime internally for the API call.
/// Returns an empty `Vec` on any error so callers can fall back gracefully.
/// Preserves provider-advertised capability metadata exactly like
/// [`list_models_for_provider_async`] (the return type is [`ModelInfo`]).
/// The runtime-construction failure is logged as a warning for the same reason
/// [`list_models_for_provider_async`] logs its errors: a collapsed empty list
/// must not be mistaken for a real, empty discovery.
pub fn list_models_for_provider_blocking(
    provider_type: &str,
    api_key: &str,
    api_base: Option<&str>,
) -> Vec<ModelInfo> {
    use tokio::runtime::Builder;
    let Ok(rt) = Builder::new_current_thread().enable_all().build() else {
        tracing::warn!(
            provider_type,
            "model discovery runtime failed to build; returning an empty model list"
        );
        return Vec::new();
    };
    rt.block_on(list_models_for_provider_async(provider_type, api_key, api_base))
}

// ---------------------------------------------------------------------------
// Tests — model discovery
//
// The helpers can only return `Vec<String>`, so the failure mode that matters
// is a *silent* collapse to `[]`. These tests pin the visibility of that
// collapse. `WarnSink` mirrors the one already in `openai::tests`: a minimal
// `tracing::Subscriber` so no `tracing-subscriber` dev-dependency is needed.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod discovery_tests {
    use super::list_models_for_provider_async;

    /// One captured WARN event: its rendered message plus its fields.
    #[derive(Clone, Debug, Default)]
    struct CapturedWarn {
        message: String,
        fields: Vec<(String, String)>,
    }

    impl CapturedWarn {
        fn field(&self, name: &str) -> Option<&str> {
            self.fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
        }
    }

    impl tracing::field::Visit for CapturedWarn {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let rendered = format!("{value:?}");
            if field.name() == "message" {
                self.message = rendered.trim_matches('"').to_string();
            } else {
                self.fields.push((field.name().to_string(), rendered));
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else {
                self.fields.push((field.name().to_string(), value.to_string()));
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields.push((field.name().to_string(), value.to_string()));
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    #[derive(Clone, Default)]
    struct WarnSink {
        warns: std::sync::Arc<std::sync::Mutex<Vec<CapturedWarn>>>,
    }

    impl WarnSink {
        fn warns(&self) -> Vec<CapturedWarn> {
            self.warns.lock().expect("warn capture mutex poisoned").clone()
        }
    }

    impl tracing::Subscriber for WarnSink {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            tracing::Id::from_u64(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }

        fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() != &tracing::Level::WARN {
                return;
            }
            let mut captured = CapturedWarn::default();
            event.record(&mut captured);
            self.warns.lock().expect("warn capture mutex poisoned").push(captured);
        }

        fn enter(&self, _span: &tracing::Id) {}

        fn exit(&self, _span: &tracing::Id) {}
    }

    /// An endpoint nothing listens on: loopback port 1 is unprivileged and
    /// unused, so the connect is refused locally and immediately — no packet
    /// leaves the machine, no external service is contacted, no DNS lookup.
    const UNREACHABLE_BASE: &str = "http://127.0.0.1:1";

    /// A failing discovery still collapses to an empty list (the signature is
    /// fixed), but it must no longer do so *silently*: the underlying error is
    /// logged at WARN with the provider type, so an outage is distinguishable
    /// from a provider that genuinely publishes zero models.
    #[test]
    fn list_models_for_provider_async_warns_when_discovery_fails() {
        let sink = WarnSink::default();
        let handle = sink.clone();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        // `block_on` inside the subscriber scope so the `warn!` — which runs
        // on this thread, after the connect fails — is captured.
        let models = tracing::subscriber::with_default(sink, || {
            runtime.block_on(list_models_for_provider_async(
                "openai",
                "sk-test-key",
                Some(UNREACHABLE_BASE),
            ))
        });

        assert!(models.is_empty(), "the failure still collapses to an empty list");
        let warns = handle.warns();
        assert_eq!(warns.len(), 1, "the collapse must be logged, not silent");
        assert_eq!(
            warns[0].field("provider_type"),
            Some("openai"),
            "the warning must name the provider type"
        );
        assert!(
            warns[0].message.contains("model discovery failed"),
            "the warning must describe the failure: {}",
            warns[0].message
        );
    }
}
