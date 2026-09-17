//! Provider abstraction (rig-aligned, ADR pending for the rig adapter
//! layer — see Risk Register: "thin adapter layer between your traits and
//! theirs").
//!
//! **Deviation from the roadmap's pseudocode, flagged for oracle review:**
//! the roadmap's draft signature returns `impl Stream<Item = ...>` directly
//! from an `async fn` in the trait. That shape only compiles for *static*
//! dispatch (`impl LlmProvider`) — `Tool::execute` already takes
//! `&dyn PolicyEngine`, `ExpertAgent::run` takes `Arc<dyn MemoryStore>`, and
//! a provider registry will need `Vec<Box<dyn LlmProvider>>` in Phase 1 to
//! hold OpenAI/Anthropic/Google/OpenRouter/Ollama side by side. `impl
//! Trait` in that return position is not object-safe, so this version
//! returns a boxed, pinned stream instead. If this trade-off is wrong,
//! raise it as ADR-19 rather than silently special-casing the registry.
//!
//! `#[async_trait]` is used for the same object-safety reason: native
//! async-fn-in-traits (stable since Rust 1.75) does not produce a
//! dyn-compatible vtable without it. `async-trait` is **not** in the
//! roadmap's tech stack table — that's an intentional gap to close in
//! Phase 0, not an oversight.

use crate::error::ProviderError;
use crate::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget};
use crate::CancellationToken;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

pub type CompletionStream =
    Pin<Box<dyn Stream<Item = Result<CompletionChunk, ProviderError>> + Send>>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stream a completion as canonical [`CompletionChunk`]s.
    ///
    /// ADR-48 §4 — every connector must attach provider-reported wire usage
    /// to the terminal chunk only (via `CompletionChunk::usage`): `Some`
    /// when the provider reports counts, `None` when the wire carries no
    /// usable counts. Empty stays `None`; `None`/`0` are never coalesced;
    /// intermediate chunks always carry `usage: None`. See the full contract
    /// on [`crate::types::CompletionUsage`].
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError>;

    /// Provider context budget for `model`.
    ///
    /// Returns a [`TokenBudget`]; the prompt-side bound for context assembly
    /// and RAG truncation is [`TokenBudget::available`] (`capacity` minus the
    /// response reservation), never the raw total `capacity`.
    fn context_capacity(&self, model: &str) -> TokenBudget;

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64;

    fn provider_name(&self) -> &'static str;

    /// Verify the provider connection by making a lightweight API call.
    ///
    /// The default implementation returns `Ok(())`. Providers that support
    /// connection testing should override this with a minimal ping (e.g.
    /// listing models or a tiny completion).
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        Ok(())
    }

    /// List available models from this provider.
    ///
    /// Returns a vector of [`ModelInfo`] entries. The default implementation
    /// returns an empty vector — providers that support model listing should
    /// override this (OpenAI, Anthropic, Google, Ollama, etc.).
    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}
