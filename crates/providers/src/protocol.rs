use concerto_core::error::ProviderError;
use concerto_core::types::{CompletionRequest, ToolCall};

/// A normalized request sent to any provider implementation.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub completion: CompletionRequest,
    pub api_base: Option<String>,
    pub api_key: String,
    pub timeout_seconds: u64,
}

impl ProviderRequest {
    pub fn new(completion: CompletionRequest, api_key: String) -> Self {
        Self { completion, api_base: None, api_key, timeout_seconds: 30 }
    }
}

/// Coerce a tool-call `arguments` value to a JSON object before it reaches an
/// OpenAI-compatible wire format.
///
/// OpenAI-compatible providers (OpenAI, OpenRouter, DeepSeek, Nvidia NIM,
/// OpenCode Zen, Ollama, ...) serialize assistant tool-call arguments as a JSON
/// *string* and reject any string whose contents are not a JSON object with
/// `HTTP 400: function.arguments must be a JSON object`. Producers can hand us
/// a non-object `arguments` — e.g. `null` from an empty accumulated argument
/// fragment, a raw string from a non-conforming upstream, or an array — which
/// would serialize to `"null"` / `"\"ls\""` on the wire and trip that strict
/// schema. This function returns the input unchanged when it is already a JSON
/// object, otherwise it coerces to `{}` so the wire always carries a valid
/// object. A warning is logged the first time a coercion happens per process
/// (with a truncated snippet of the original value); later coercions proceed
/// silently.
pub fn ensure_arguments_object(args: serde_json::Value) -> serde_json::Value {
    if args.is_object() {
        return args;
    }
    // Latch the warning: one per process is enough to flag the failure mode
    // without spamming logs for every bad tool call in a long multi-agent run.
    let _ = ARGUMENTS_COERCION_WARNED.get_or_init(|| {
        let snippet: String = serde_json::to_string(&args)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        tracing::warn!(
            original = %snippet,
            "coerced non-object tool-call arguments to `{{}}` to enforce `HTTP 400: function.arguments must be a JSON object` compliance"
        );
    });
    serde_json::json!({})
}

/// Process-wide latch so the `ensure_arguments_object` coercion warning fires
/// only once per process.
static ARGUMENTS_COERCION_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// A normalized response from a provider.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Events during a streaming completion.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StreamEvent {
    Chunk { delta: String },
    ToolCallDelta { id: String, name: String, arguments: String },
    Done { tokens_in: u64, tokens_out: u64 },
    Error(ProviderError),
}
