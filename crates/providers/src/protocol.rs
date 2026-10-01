use concerto_core::error::ProviderError;
use concerto_core::types::{CompletionRequest, ToolCall};
use concerto_core::SecretString;

/// A normalized request sent to any provider implementation.
///
/// `api_key` is a [`SecretString`] so the derived `Debug` — and any struct
/// that embeds this one — renders `[REDACTED]` instead of the credential.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub completion: CompletionRequest,
    pub api_base: Option<String>,
    pub api_key: SecretString,
    pub timeout_seconds: u64,
}

impl ProviderRequest {
    pub fn new(completion: CompletionRequest, api_key: String) -> Self {
        Self { completion, api_base: None, api_key: api_key.into(), timeout_seconds: 30 }
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

/// The neutral continuation cue appended when a rendered OpenAI-compatible
/// `messages` array would otherwise be empty or system-only.
///
/// Deliberately short and content-free: this is a protocol floor, not prompt
/// content. A real objective is the caller's job (the coordinator seeds one on
/// the planning path), so this message only has to make the request
/// satisfiable.
pub const CONVERSATION_FLOOR_USER_MESSAGE: &str = "Continue.";

/// Guarantee that an OpenAI-compatible `messages` array is satisfiable before
/// the request goes on the wire.
///
/// A strict OpenAI-compatible gateway rejects a conversation whose `messages`
/// array contains only `system` messages with `HTTP 400 invalid_request_error`.
/// The verified shape matrix against the OpenCode Zen/Go relay
/// (`POST https://opencode.ai/zen/go/v1/chat/completions`) is:
///
/// | `messages`                      | result |
/// |---------------------------------|--------|
/// | `system` only                   | **400** |
/// | `system` + `user`               | 200 |
/// | `user` only                     | 200 |
/// | `system` + `assistant`          | 200 |
/// | `system` + `user` + `assistant` | 200 |
///
/// The rejection is not a size, `temperature`, `max_tokens`, `tool_choice`, or
/// tool-schema problem — a system-only conversation is the single rejected
/// shape. This helper is the provider-level floor that keeps every
/// OpenAI-compatible request satisfiable regardless of caller:
///
/// * A non-empty array that already carries at least one non-`system` message
///   is left **unchanged** (byte-identical to the pre-floor output). This is
///   the overwhelmingly common case.
/// * An empty array, or one containing only `system` messages, gets ONE
///   appended minimal `user` message (see [`CONVERSATION_FLOOR_USER_MESSAGE`])
///   so the request is satisfiable.
///
/// The coordinator is separately fixed to always seed a real objective user
/// turn, so this floor should never fire on the planning path; it exists as
/// defence in depth for every other caller.
///
/// ```
/// use concerto_providers::protocol::ensure_non_system_conversation;
///
/// // The exact shape strict gateways reject: a system-only conversation.
/// let mut messages = vec![serde_json::json!({"role": "system", "content": "objective"})];
/// ensure_non_system_conversation(&mut messages);
///
/// assert_eq!(messages.len(), 2);
/// assert_eq!(messages[0]["role"], "system");
/// assert_eq!(messages[1]["role"], "user");
/// ```
pub fn ensure_non_system_conversation(messages: &mut Vec<serde_json::Value>) {
    let has_non_system = messages
        .iter()
        .any(|message| message.get("role").and_then(serde_json::Value::as_str) != Some("system"));
    if messages.is_empty() || !has_non_system {
        messages.push(serde_json::json!({
            "role": "user",
            "content": CONVERSATION_FLOOR_USER_MESSAGE,
        }));
    }
}

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

#[cfg(test)]
mod tests {
    use super::{
        ensure_arguments_object, ensure_non_system_conversation, ProviderRequest,
        CONVERSATION_FLOOR_USER_MESSAGE,
    };

    /// Build a wire `message` object for the floor tests.
    fn wire_message(role: &str, content: &str) -> serde_json::Value {
        serde_json::json!({ "role": role, "content": content })
    }

    /// The common path: a conversation that already carries a non-system turn
    /// is returned byte-identical — no floor, no reordering, no allocation.
    #[test]
    fn non_system_conversations_pass_through_byte_identical() {
        let cases: Vec<Vec<serde_json::Value>> = vec![
            vec![wire_message("user", "Hello")],
            vec![wire_message("system", "sys"), wire_message("user", "Hello")],
            vec![wire_message("system", "sys"), wire_message("assistant", "hi")],
            vec![wire_message("user", "Hello"), wire_message("assistant", "hi")],
        ];
        for before in cases {
            let mut after = before.clone();
            ensure_non_system_conversation(&mut after);
            assert_eq!(after, before, "non-system conversation must be untouched: {before:?}");
        }
    }

    /// An empty conversation gets exactly one appended floor user turn.
    #[test]
    fn empty_conversation_gets_a_floor_user_turn() {
        let mut messages = Vec::new();
        ensure_non_system_conversation(&mut messages);
        assert_eq!(
            messages,
            vec![wire_message("user", CONVERSATION_FLOOR_USER_MESSAGE)],
            "empty conversation must gain one user turn"
        );
    }

    /// A system-only conversation gets the floor appended AFTER the system
    /// message (order is preserved; the system prompt stays first).
    #[test]
    fn system_only_conversation_gets_a_floor_user_turn_after_it() {
        let mut messages = vec![wire_message("system", "the 14 KB coordinator prompt")];
        ensure_non_system_conversation(&mut messages);
        assert_eq!(messages.len(), 2, "system turn + floor");
        assert_eq!(messages[0], wire_message("system", "the 14 KB coordinator prompt"));
        assert_eq!(messages[1], wire_message("user", CONVERSATION_FLOOR_USER_MESSAGE));
    }

    /// Applying the floor twice is a no-op the second time: after the first
    /// pass the array already carries a user turn, so the common-path rule
    /// leaves it alone.
    #[test]
    fn conversation_floor_is_idempotent() {
        let mut messages = vec![wire_message("system", "sys")];
        ensure_non_system_conversation(&mut messages);
        let once = messages.clone();
        ensure_non_system_conversation(&mut messages);
        assert_eq!(messages, once, "a second floor application must not append again");
    }

    /// A JSON object — the only wire-legal shape — passes through untouched.
    /// Fixture is synthetic — never a real credential. `ProviderRequest`
    /// derives `Debug`, so the redaction has to come from the `SecretString`
    /// field itself, not from a manual impl on this struct.
    #[test]
    fn provider_request_debug_redacts_the_api_key() {
        const SYNTHETIC: &str = "sk-synthetic-provider-request-fixture";
        let request = ProviderRequest::new(
            concerto_core::types::CompletionRequest::default(),
            SYNTHETIC.to_string(),
        );

        assert_eq!(request.api_key.expose(), SYNTHETIC);

        let rendered = format!("{request:?}");
        assert!(!rendered.contains(SYNTHETIC), "api_key leaked into Debug: {rendered}");
        assert!(!rendered.contains("sk-synthetic"), "api_key prefix leaked: {rendered}");
        assert!(rendered.contains("[REDACTED]"), "redaction marker missing: {rendered}");
        // Non-secret fields must remain diagnosable.
        assert!(rendered.contains("timeout_seconds"), "other fields must still render: {rendered}");
    }

    #[test]
    fn objects_pass_through_unchanged() {
        let args = serde_json::json!({"command": "ls"});
        assert_eq!(ensure_arguments_object(args.clone()), args);
    }

    /// Every non-object producer shape coerces to `{}` so the outbound wire
    /// never trips `function.arguments must be a JSON object` — including a
    /// double-encoded arguments *string*, which is why the OpenAI connector
    /// unwraps that class upstream (row #38) instead of losing the payload
    /// here.
    #[test]
    fn non_object_shapes_coerce_to_empty_object() {
        let cases = [
            serde_json::json!("ls"),
            serde_json::json!("\"{\\\"command\\\":\\\"ls\\\"}\""),
            serde_json::json!(["ls"]),
            serde_json::json!(42),
            serde_json::json!(null),
        ];
        for args in cases {
            assert_eq!(
                ensure_arguments_object(args.clone()),
                serde_json::json!({}),
                "shape: {args}"
            );
        }
    }
}
