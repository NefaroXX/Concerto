//! Generator for the forwarding surface shared by the thin OpenAI-compatible
//! wrapper providers.
//!
//! The wrappers in this crate (`CerebrasProvider`, `GroqProvider`,
//! `SambaNovaProvider`, …) are all the same shape: a struct holding an inner
//! [`crate::openai::OpenAiProvider`], a matching set of `with_*` builder
//! forwarders, and an [`concerto_core::traits::LlmProvider`] impl whose
//! methods (apart from the advertised name and the pricing estimate) delegate
//! to that inner provider. [`openai_wrapper_forwarders!`] generates that
//! repeated surface so each wrapper file only states what is genuinely
//! provider-specific: the endpoint, the name, the pricing, and any doc note
//! that deviates from the canonical wording.
//!
//! Declared before the provider modules in `lib.rs` under `#[macro_use]`, so
//! every wrapper module in the crate can invoke it.

// The canonical docs emitted by this macro carry intra-doc links
// (`[`ReasoningEcho::IfPresent`]`) whose spans resolve against this
// definition site, so the linked names must be in scope here even though no
// code item references them.
#[allow(unused_imports)]
use crate::openai::ReasoningEcho;

/// Generates the repeated forwarding surface of a thin OpenAI-compatible
/// wrapper provider.
///
/// Three invocation shapes:
///
/// - `openai_wrapper_forwarders!(inner)` — the four `with_*` builder
///   forwarders with canonical docs, for the common wrapper. Invoked
///   **inside** the wrapper's inherent `impl`.
/// - `openai_wrapper_forwarders!(inner, [method, …])` — an explicit method
///   subset; each entry is optionally followed by `=> [docs]` to override
///   the canonical doc comment for that method (the body is still generated).
///   Use this to skip methods a wrapper does not forward (for example
///   `with_api_base`) or to keep a provider-specific doc note.
/// - `openai_wrapper_forwarders!(llm Wrapper, inner, name: …, capacity: …,
///   cost: …)` — the [`LlmProvider`] forwarding impl, invoked at module
///   level. `capacity: forward` delegates `context_capacity` to the inner
///   provider; `capacity: budget(rate)` uses the crate-wide name-based
///   budget table instead. `cost:` is either `per_mtok(input, output)`
///   (per-million-token pricing) or `per_1k(rate)` (flat combined per-1k
///   pricing); the pricing rationale comment belongs at the invocation site,
///   next to the rates.
///
/// Doc comments passed at an invocation site are emitted verbatim, so every
/// wrapper keeps its exact rustdoc.
macro_rules! openai_wrapper_forwarders {
    // ------------------------------------------------------------------
    // `LlmProvider` forwarding impl (invoked at module level).
    // ------------------------------------------------------------------
    (
        llm $wrapper:ty,
        $inner:ident,
        name: $name:expr,
        capacity: $cap:ident $(($caparg:expr))?,
        cost: $cost:ident($($costargs:expr),* $(,)?)
        $(,)?
    ) => {
        #[async_trait]
        impl LlmProvider for $wrapper {
            async fn stream_completion(
                &self,
                request: CompletionRequest,
                cancel: CancellationToken,
            ) -> Result<CompletionStream, ProviderError> {
                self.$inner.stream_completion(request, cancel).await
            }

            openai_wrapper_forwarders!(@context_capacity, $inner, $cap $(, $caparg)?);
            openai_wrapper_forwarders!(@approximate_cost, $cost $(, $costargs)*);

            fn provider_name(&self) -> &'static str {
                $name
            }

            async fn test_connection(
                &self,
                _cancel: CancellationToken,
            ) -> Result<(), ProviderError> {
                self.$inner.test_connection(_cancel.clone()).await
            }

            async fn list_models(
                &self,
                _cancel: CancellationToken,
            ) -> Result<Vec<ModelInfo>, ProviderError> {
                self.$inner.list_models(_cancel.clone()).await
            }
        }
    };

    // ------------------------------------------------------------------
    // Builder forwarders (invoked inside the wrapper's inherent `impl`).
    // ------------------------------------------------------------------

    // The common case: all four forwarders, canonical docs.
    ($inner:ident) => {
        openai_wrapper_forwarders!(
            $inner,
            [
                with_api_base,
                with_reasoning_echo,
                with_tool_schema_mode,
                with_advertised_tool_support
            ]
        );
    };

    // Explicit method selection; docs are canonical unless overridden.
    ($inner:ident, [$($method:ident $(=> [$(#[$doc:meta])*])? ),+ $(,)?]) => {
        $(openai_wrapper_forwarders!(@forwarder, $method, $inner $(=> [$(#[$doc])*])?);)+
    };

    // ------------------------------------------------------------------
    // Per-method emitters: canonical docs, then doc-override variants.
    // ------------------------------------------------------------------
    (@forwarder, with_api_base, $inner:ident) => {
        /// Override the API base URL (self-hosted gateways, proxies, or tests).
        pub fn with_api_base(mut self, api_base: String) -> Self {
            self.$inner = self.$inner.with_api_base(api_base);
            self
        }
    };
    (@forwarder, with_api_base, $inner:ident => [$(#[$doc:meta])*]) => {
        $(#[$doc])*
        pub fn with_api_base(mut self, api_base: String) -> Self {
            self.$inner = self.$inner.with_api_base(api_base);
            self
        }
    };

    (@forwarder, with_reasoning_echo, $inner:ident) => {
        /// Set the reasoning-content echo policy (ADR-46), forwarded to the inner
        /// OpenAI-compatible connector. Defaults to [`ReasoningEcho::IfPresent`].
        pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
            self.$inner = self.$inner.with_reasoning_echo(echo);
            self
        }
    };
    (@forwarder, with_reasoning_echo, $inner:ident => [$(#[$doc:meta])*]) => {
        $(#[$doc])*
        pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
            self.$inner = self.$inner.with_reasoning_echo(echo);
            self
        }
    };

    (@forwarder, with_tool_schema_mode, $inner:ident) => {
        /// Set the tool-schema presentation mode (adaptive tool schemas),
        /// forwarded to the inner OpenAI-compatible connector. Defaults to
        /// [`concerto_config::ToolSchemaMode::Auto`].
        pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
            self.$inner = self.$inner.with_tool_schema_mode(mode);
            self
        }
    };
    (@forwarder, with_tool_schema_mode, $inner:ident => [$(#[$doc:meta])*]) => {
        $(#[$doc])*
        pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
            self.$inner = self.$inner.with_tool_schema_mode(mode);
            self
        }
    };

    (@forwarder, with_advertised_tool_support, $inner:ident) => {
        /// Forward the provider-advertised per-model tool-calling capability
        /// (ADR-66 §3 precedence level 2) to the inner OpenAI-compatible
        /// provider.
        pub fn with_advertised_tool_support(mut self, advertised: Option<bool>) -> Self {
            self.$inner = self.$inner.with_advertised_tool_support(advertised);
            self
        }
    };
    (@forwarder, with_advertised_tool_support, $inner:ident => [$(#[$doc:meta])*]) => {
        $(#[$doc])*
        pub fn with_advertised_tool_support(mut self, advertised: Option<bool>) -> Self {
            self.$inner = self.$inner.with_advertised_tool_support(advertised);
            self
        }
    };

    // ------------------------------------------------------------------
    // `LlmProvider` method fragments, dispatched from the entry arm above.
    // ------------------------------------------------------------------
    (@context_capacity, $inner:ident, forward) => {
        fn context_capacity(&self, model: &str) -> TokenBudget {
            self.$inner.context_capacity(model)
        }
    };
    (@context_capacity, $inner:ident, budget, $rate:expr) => {
        fn context_capacity(&self, model: &str) -> TokenBudget {
            crate::budget::budget_for_model(model, $rate)
        }
    };

    (@approximate_cost, per_mtok, $input_rate:expr, $output_rate:expr) => {
        fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
            let input_cost = (tokens_in as f64 / 1_000_000.0) * $input_rate;
            let output_cost = (tokens_out as f64 / 1_000_000.0) * $output_rate;
            input_cost + output_cost
        }
    };
    (@approximate_cost, per_1k, $rate:expr) => {
        fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
            ((tokens_in + tokens_out) as f64 / 1_000.0) * $rate
        }
    };
}
