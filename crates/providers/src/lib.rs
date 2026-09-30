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
pub(crate) mod credential;
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
pub mod opencode_local;
pub mod openrouter;
pub mod perplexity;
pub mod sambanova;
pub mod sse;
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
        // A local `opencode serve` instance: discovery reads `/provider` and
        // returns the zero-cost models it advertises. The credential is the
        // server password. Callers that only hold a keyring-backed key pass it
        // as `api_key`; when that is empty (e.g. an `OPENCODE_SERVER_PASSWORD`-
        // only setup), the provider-scoped env fallback is resolved here so the
        // desktop picker still populates.
        "opencode-local" => match credential::opencode_local_password(api_key) {
            Some(password) => {
                let p = opencode_local::OpenCodeLocalProvider::with_api_base(
                    password,
                    String::new(),
                    DEFAULT_TIMEOUT_SECS,
                    api_base.unwrap_or(opencode_local::OPENCODE_LOCAL_DEFAULT_BASE).to_string(),
                );
                p.list_models(cancel.clone()).await
            }
            None => {
                tracing::warn!(
                    provider_type,
                    "no OPENCODE_SERVER_PASSWORD or OPENCODE_LOCAL_API_KEY is set; \
                     returning an empty model list"
                );
                Ok(Vec::new())
            }
        },
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

/// Whether a usable credential is currently resolvable for `provider` on the
/// model-discovery path.
///
/// Keyring-first, matching [`ProviderConfig::api_key`](concerto_config::ProviderConfig::api_key).
/// For `opencode-local` the exportable env fallbacks
/// (`OPENCODE_LOCAL_API_KEY`, `OPENCODE_SERVER_PASSWORD`) are also honoured, so
/// an env-only local-server setup is discovery-ready and the picker populates
/// without a manual step. Other providers keep the keyring-only behaviour their
/// discovery path expects.
pub fn provider_credential_present(
    provider: &concerto_config::ProviderConfig,
    store: &concerto_config::CredentialStore,
) -> bool {
    let keyring = provider.api_key(store).map(|key| !key.expose().is_empty()).unwrap_or(false);
    keyring
        || (provider.provider == "opencode-local"
            && credential::opencode_local_password("").is_some())
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
    use super::{list_models_for_provider_async, provider_credential_present};

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

    /// The `opencode-local` discovery path resolves the server password from
    /// `OPENCODE_SERVER_PASSWORD` when the caller passes an empty credential,
    /// and actually hits `GET /provider`. This is the desktop picker's path: it
    /// resolves the credential keyring-only, so an env-password-only setup used
    /// to build but never populate the picker.
    #[test]
    fn opencode_local_discovery_uses_the_env_password_and_hits_provider() {
        use crate::testing::mock_server::{request_headers, spawn_scripted};
        use crate::testing::RestoreEnvVar;

        let _lock = crate::credential::OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _local = RestoreEnvVar::without(crate::credential::OPENCODE_LOCAL_API_KEY_ENV);
        let _server = RestoreEnvVar::set(crate::credential::OPENCODE_SERVER_PASSWORD_ENV, "pw");

        let (base, requests) = spawn_scripted(vec![(
            200,
            serde_json::json!({
                "all": [{"id": "opencode", "models": {
                    "big-pickle": {"cost": {"input": 0}},
                    "paid-model": {"cost": {"input": 3}}
                }}]
            })
            .to_string(),
        )]);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let models =
            runtime.block_on(list_models_for_provider_async("opencode-local", "", Some(&base)));
        assert_eq!(models.len(), 1, "only the zero-cost model is returned: {models:?}");
        assert_eq!(models[0].id, "big-pickle");

        let raw =
            requests.recv_timeout(std::time::Duration::from_secs(5)).expect("one request captured");
        let headers = request_headers(&raw);
        assert!(headers.starts_with("get /provider http/1.1"), "{headers}");
        // The env password must have been used for Basic auth.
        let text = String::from_utf8_lossy(&raw);
        let auth = text
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .expect("an Authorization header is present");
        assert_eq!(
            auth.split_once(':').expect("header value").1.trim(),
            "Basic b3BlbmNvZGU6cHc=",
            "Basic auth must be base64(\"opencode:pw\")"
        );
    }

    /// The discovery-readiness gate honours the `opencode-local` env fallback
    /// (so an env-only setup auto-discovers) while other providers keep the
    /// keyring-only semantics their discovery path expects.
    #[test]
    fn provider_credential_present_honours_the_local_env_fallback() {
        use crate::testing::RestoreEnvVar;
        use concerto_config::{CredentialStore, ProviderConfig};

        let _lock = crate::credential::OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _local = RestoreEnvVar::without(crate::credential::OPENCODE_LOCAL_API_KEY_ENV);
        let _server = RestoreEnvVar::without(crate::credential::OPENCODE_SERVER_PASSWORD_ENV);
        let store = CredentialStore::from_env();

        let local = ProviderConfig {
            provider: "opencode-local".into(),
            keyring_key: "test-discovery-present/server_password".into(),
            ..ProviderConfig::default()
        };
        assert!(
            !provider_credential_present(&local, &store),
            "no credential anywhere must not be ready"
        );

        let _set = RestoreEnvVar::set(crate::credential::OPENCODE_SERVER_PASSWORD_ENV, "pw");
        assert!(
            provider_credential_present(&local, &store),
            "the server-password env var must make the provider discovery-ready"
        );

        // A stray env var is not a keyring credential for other providers.
        let openai = ProviderConfig {
            provider: "openai".into(),
            keyring_key: "test-discovery-present/openai".into(),
            ..ProviderConfig::default()
        };
        assert!(
            !provider_credential_present(&openai, &store),
            "non-local providers keep keyring-only readiness"
        );
    }
}
