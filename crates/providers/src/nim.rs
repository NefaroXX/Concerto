use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

const NIM_API_BASE: &str = "https://integrate.api.nvidia.com/v1";

pub struct NimProvider {
    inner: OpenAiProvider,
}

impl NimProvider {
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(NIM_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner, [
        with_reasoning_echo,
        with_tool_schema_mode => [
            /// Set the tool-schema presentation mode (adaptive tool schemas),
            /// forwarded to the inner OpenAI-compatible connector. Defaults to
            /// [`concerto_config::ToolSchemaMode::Auto`]. See
            /// `crate::adapters::schema_loose`.
        ],
        with_advertised_tool_support => [
            /// Forward the provider-advertised per-model tool-calling capability
            /// (ADR-66 §3 precedence level 2) to the inner OpenAI-compatible
            /// provider, so an advertised flag beats the last-resort name heuristic.
        ],
    ]);
}

openai_wrapper_forwarders!(llm NimProvider, inner,
    name: "nim",
    capacity: budget(4_000),
    // Representative 70B pricing: ~$0.00099/1K tokens (in+out combined).
    // Actual cost varies by model; callers should consult NIM pricing for precision.
    cost: per_1k(0.00099),
);
