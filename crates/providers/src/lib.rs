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
pub mod google;
pub mod nim;
pub mod ollama;
pub mod openai;
pub mod opencode;
pub mod openrouter;
pub mod sse;

#[cfg(test)]
pub mod testing;

/// Mock provider for evaluation and testing without real API keys.
/// Public to allow the eval crate to instantiate it directly.
pub mod mock;

// Re-export ModelInfo for convenience
pub use concerto_core::types::ModelInfo;

/// Default HTTP connect timeout (seconds) used where no per-provider config exists.
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Default OpenCode Zen API base URL, used by the `opencode` provider and the
/// model-listing helper.
pub(crate) const OPENCODE_ZEN_API_BASE: &str = "https://opencode.ai/zen/v1";

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

/// Async helper: list available model IDs for a given provider configuration.
///
/// Constructs a temporary provider instance and calls its `list_models` method.
/// Returns an empty `Vec` on any error (network failure, auth, etc.) so callers
/// can gracefully fall back to a free-text model prompt.
pub async fn list_models_for_provider_async(
    provider_type: &str,
    api_key: &str,
    api_base: Option<&str>,
) -> Vec<String> {
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
        "opencode" => {
            let p = opencode::OpenCodeZenProvider::with_api_base(
                api_key.to_string(),
                String::new(),
                DEFAULT_TIMEOUT_SECS,
                api_base.unwrap_or(OPENCODE_ZEN_API_BASE).to_string(),
            );
            p.list_models(cancel.clone()).await
        }
        _ => return Vec::new(),
    };
    result.unwrap_or_default().into_iter().map(|m| m.id).collect()
}

/// Blocking helper: list available model IDs for a given provider configuration.
///
/// Creates a single-threaded tokio runtime internally for the API call.
/// Returns an empty `Vec` on any error so callers can fall back gracefully.
pub fn list_models_for_provider_blocking(
    provider_type: &str,
    api_key: &str,
    api_base: Option<&str>,
) -> Vec<String> {
    use tokio::runtime::Builder;
    let Ok(rt) = Builder::new_current_thread().enable_all().build() else {
        return Vec::new();
    };
    rt.block_on(list_models_for_provider_async(provider_type, api_key, api_base))
}
