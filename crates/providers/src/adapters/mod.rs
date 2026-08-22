//! Dialect adapters — the provider-family seam of the provider-first design
//! (Phase 2, `docs/ARCHITECTURE-V2.md`).
//!
//! Providers are dialects of one canonical protocol: a [`Dialect`] lowers a
//! canonical [`CompletionRequest`] onto the exact wire request body that a
//! provider family expects. The owning provider connector keeps everything
//! transport-shaped — retries/backoff, SSE parsing via the shared
//! [`crate::sse::BufferedSseParser`], keepalive chunks, timeouts, cancellation —
//! and asks its dialect only for the JSON payload.
//!
//! Milestone 1 introduced the seam and ported the OpenAI-compatible chat
//! body builder as the first dialect (zero behavior change). Milestone 2 adds
//! Anthropic, Gemini and Ollama dialects, also zero-behavior-change ports of
//! their connectors' body builders. This module depends on `concerto_core`
//! types plus the reasoning-echo policy defined below.

use concerto_core::types::CompletionRequest;

pub mod anthropic;
pub mod google;
pub mod ollama;
pub mod openai_compat;

pub use anthropic::AnthropicChatDialect;
pub use google::GeminiChatDialect;
pub use ollama::OllamaChatDialect;
pub use openai_compat::OpenAiChatDialect;

/// Controls whether collected `reasoning_content` is echoed back to the
/// provider on assistant messages (ADR-46).
///
/// DeepSeek-style endpoints (OpenCode Zen, DeepSeek, NIM) return HTTP 400 when
/// `reasoning_content` is passed back onto an assistant message while the model
/// is in "thinking" mode. This policy decides how we react:
/// - [`ReasoningEcho::IfPresent`]: emit `reasoning_content` only when the
///   underlying assistant message actually carries captured reasoning.
/// - [`ReasoningEcho::Always`]: emit `reasoning_content` (empty string when no
///   reasoning is present) on *every* assistant message. The empty string
///   satisfies the DeepSeek contract when only a tool call exists in history.
///
/// The enum lives here because echoing is an adapter-level decision: every
/// [`Dialect`] takes the policy in [`Dialect::render_chat_body`], and the
/// OpenAI connector re-exports it (`crate::openai::ReasoningEcho`) for callers
/// that construct providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEcho {
    /// Emit the `reasoning_content` JSON field only when reason exists.
    IfPresent,
    /// Emit `reasoning_content` on every assistant message (empty when absent).
    Always,
}

/// A per-provider-family lowering of a canonical [`CompletionRequest`] onto the
/// exact request body that family expects on the wire.
///
/// Implementations are pure and stateless: `render_chat_body` performs no I/O,
/// retries, backoff, or cancellation handling — those stay in the owning
/// provider connector. Stream parsing likewise stays in the connector via the
/// existing [`crate::sse::BufferedSseParser`]; a dialect's scope is what bytes
/// go out and (in later milestones) how wire events reduce back to canonical
/// chunks.
///
/// Reasoning echo ([`ReasoningEcho`], ADR-46) is a dialect concern: whether an
/// assistant message carries `reasoning_content` is gated by the family's echo
/// contract (e.g. DeepSeek mandates `reasoning_content` on every assistant
/// message in a tool-call history, `""` when no reasoning was captured;
/// OpenAI-native never expects it).
pub trait Dialect: Send + Sync {
    /// Stable identifier for this provider family (e.g. `"openai-compat"`).
    fn kind(&self) -> &'static str;

    /// Render the exact JSON request body for a chat completion.
    ///
    /// `model` is the resolved model name (the request's own `model` when set,
    /// otherwise the provider's default), and `echo` is the active
    /// reasoning-echo policy (ADR-46). The returned [`serde_json::Value`] is
    /// sent verbatim by the provider connector. Renderers must stay pure — no
    /// cancellation, timeouts, or transports here.
    fn render_chat_body(
        &self,
        request: &CompletionRequest,
        model: &str,
        echo: ReasoningEcho,
    ) -> serde_json::Value;

    /// Opt-in provider-cache punctuation for a rendered request body.
    ///
    /// The provider tier calls this only when the provider's cache flag is set
    /// (e.g. `AnthropicProvider::cache_breakpoints`). The default is a no-op:
    /// provider families without explicit prompt-cache support (OpenAI-compatible
    /// backends, Gemini, Ollama) leave the body untouched — server-side pooling
    /// handles them, and the engine already guarantees a byte-stable request
    /// head. Families that support explicit prompt caching (Anthropic
    /// `cache_control`) override this to add their breakpoint markers.
    ///
    /// Implementations must be idempotent: applying the markers twice to the
    /// same body must not change it on the second pass.
    fn apply_cache_breakpoints(&self, _body: &mut serde_json::Value) {}
}
