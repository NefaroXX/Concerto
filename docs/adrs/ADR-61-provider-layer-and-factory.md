# ADR-61: Provider Layer — `LlmProvider` Trait, Factory Construction, and Transport Hardening

**Status:** Accepted
**Date:** 2026-08-18
**Deciders:** Concerto architecture
**Related crates:** `concerto-core`, `concerto-providers`, `concerto-config`
**Supersedes:** nothing new — codifies the provider-execution layer that
[ADR-31](./ADR-31.md) (model-first selection) and [ADR-49](./ADR-49-config-first-catalog.md)
(config-first catalog) sit on top of; supersedes-in-part the archived
[ADR-24](./archive/ADR-24.md) framing of provider/model pairing.

## Context

Concerto is provider-plural by design: a local-first coding agent must work
with hosted frontier models, OpenAI-compatible gateways, and fully offline
local runtimes without changing agent behavior. The workspace therefore needs
one execution contract that every model backend implements, one construction
path that turns config data into running provider instances, and one transport
layer that behaves identically no matter which backend serves a request.

The contract lives in `concerto-core`
(`crates/core/src/traits/provider.rs`) so every crate can depend on it without
depending on any specific vendor adapter. The implementations and the
machinery around them live in `concerto-providers`. Selection — which model,
resolved through which provider configuration — is deliberately a separate
concern owned by ADR-31/ADR-49; this ADR covers everything below that decision:
identity, streaming, construction, normalization, metering, retry, and
overflow guarding.

Requirements:

1. **Streaming-first.** Every completion path streams chunks as they arrive;
   UI reveal, cancellation, and budget checks all consume the stream.
2. **Uniform identity.** A provider exposes its identity, available model
   metadata, connection-test capability, and declared context window.
3. **Config-driven construction.** Building a provider requires only config
   data plus a credential source — never code changes or provider-ID switches.
4. **Explicit failure.** Missing credentials, unknown provider types, and bad
   endpoints are typed errors (ADR-32); mock providers are unreachable from
   production construction.
5. **Cancellation everywhere.** Every async operation threads a
   `CancellationToken`; backoff sleeps observe it.

## Decision

### 1. One trait, defined in core

`LlmProvider` (`crates/core/src/traits/provider.rs`) defines provider identity,
streaming completion, connection testing, and model metadata. Consumers depend
on the trait, never on concrete adapters. The orchestrator, eval harness, API
server, and both frontends are written against `Arc<dyn LlmProvider>`.

### 2. Concrete adapters live in `concerto-providers`

The crate ships seven first-class adapters sharing one streaming interface:

- **OpenAI**, **Anthropic**, **Google Gemini**, **OpenRouter**, **Ollama**,
  **NVIDIA NIM**, and the **OpenCode Zen** compatible endpoint family.

Each adapter owns its wire dialect; everything shareable — SSE parsing,
timeout boundaries, usage extraction — lives once in shared modules rather
than being copied per vendor.

### 3. Factory construction is the only production path

`ProviderFactory` (`crates/providers/src/factory.rs`) turns a
`ProviderConfig` plus a `CredentialStore` into a ready provider instance:

- **Model-to-config resolution**: `config_for_model` treats a provider's
  primary `model`, discovery-cached `cached_models`, and user-declared
  `extra_models` as offering candidates; extras never shadow the primary route
  (ADR-49).
- **Key resolution contract**: `effective_api_key` reads the OS keyring first
  (service `concerto`, per-provider account names — ADR-04), then falls back to
  the `<PROVIDER>_API_KEY` environment variable. When neither yields a key, the
  original keyring error surfaces — construction fails loudly instead of
  degrading (ADR-32).
- **No implicit fallbacks.** There is no default provider, no first-provider
  fallback, and no mock substitution outside tests. If resolution cannot
  produce a usable pair, dispatch stops with a visible configuration error
  (ADR-31).
- **Per-provider knobs ride the config record**: `reasoning_echo`
  (`always` / `if-present`, ADR-46) and `cache_breakpoints` (Anthropic
  prompt-cache markers, ADR-48) are serde-default fields on `ProviderConfig`,
  not provider-ID hard-coded switches.

### 4. OpenAI-compatible protocol normalization

The OpenAI-compatible chat-completions shape is the interchange format inside
the provider crate: adapters for OpenRouter, Ollama (OpenAI mode), NVIDIA NIM,
and OpenCode Zen normalize onto it, and reasoning content is carried as an
additive field on messages/chunks (ADR-46) until a canonical parts model ships
(ADR-47 records it as deferred). Novel wire shapes are handled by dialect
plugins (ADR-53), which re-render the canonical request body inside WASM while
transport stays host-side.

### 5. Token metering and honest accounting

Every completion reports token usage when the backend provides it; estimates
(bytes/4 heuristic) fill gaps and drive planning. Metered usage feeds the
shared `SpendTracker` (one accounting implementation across single-agent and
multi-agent runs — ADR-64) and the per-call spend records surfaced by ADR-41.
`tiktoken-rs` provides tokenizer support for budget estimation where an exact
tokenizer for the target model is unavailable.

### 6. Retry wrapping is centralized and cancellation-aware

Transport retry/backoff wraps individual logical requests, not whole runs:

- finite attempt count and elapsed-time ceiling from `RetryConfig`;
- separate time-to-first-byte and stream-idle deadlines;
- `Retry-After` header support;
- every wait observes the run's `CancellationToken`;
- permanent failures (authentication, cancellation) are never retried;
- a specialist run is never replayed as transport recovery (ADR-26/34).

Retry policy defaults live centrally so single- and multi-agent modes behave
identically (ADR-32).

### 7. Context guard as the final backstop

`ContextGuardProvider` (`crates/providers/src/context_guard.rs`) wraps every
provider as the last-stage enforcement of the context budget (ADR-48):
budget = capacity − output reserve − safety margin; deterministic reduction
(clip marked blocks, drop oldest conversation groups, insert compaction
summaries), then a typed `ContextOverflow` error. In-loop LLM summarization
does not exist on this path.

## Consequences

- Adding a provider means adding one adapter module plus config documentation;
  no orchestration, policy, or frontend changes.
- Users can point config at a brand-new OpenAI-compatible endpoint and obtain
  a catalog-visible model without recompiling (ADR-49 exit criterion).
- Construction failures are visible and cannot masquerade as model output
  (ADR-32).
- The retry boundary is uniform: a slow or flaky endpoint produces bounded,
  cancellable waits and typed errors — recoverable context for the multi-agent
  ladder (ADR-42/45), never silent hangs.
- Model metadata (context windows, tool-call support, prices) can go stale;
  explicit role assignments remain authoritative, and compatibility metadata
  blocks incompatible routes rather than ranking models (ADR-31).

## Alternatives Considered

- **Per-crate provider traits (orchestrator defines its own interface):**
  rejected — it would invert the dependency direction and force
  `concerto-providers` to know about orchestration concerns.
- **Capability-tier model ranking (archived ADR-24 lineage):** rejected —
  subjective numeric tiers made valid selections fail and decoupled model
  choice from the serving provider; objective compatibility metadata
  (tool-call support, context window) does the same job honestly.
- **Direct HTTP calls from the orchestrator:** rejected — metering, retry,
  normalization, and guard behavior would fragment across call sites.
- **gRPC-sidecar provider processes:** rejected — unnecessary process
  lifecycle for in-process library adapters; the process boundary is reserved
  for fault containment (ADR-60), not transport isolation.

## References

- Trait: `crates/core/src/traits/provider.rs`
- Adapters, factory, routing, guard: `crates/providers/src/factory.rs`,
  `crates/providers/src/routing.rs`, `crates/providers/src/context_guard.rs`
- Config surface: `crates/config/src/schema.rs` (`ProviderConfig`,
  `RetryConfig`)
- Credentials: `crates/config` keyring store (ADR-04)
- Related: ADR-31 (model-first selection), ADR-32 (explicit failures),
  ADR-46 (reasoning echo), ADR-47 (deferred parts model),
  ADR-48 (context engine/guard), ADR-49 (config-first catalog),
  ADR-50 (tool coercion — the tool-side sibling contract),
  ADR-53 (dialect plugins)

---

*Decision codified from inception 2025-07-10; document stabilized 2026-08-18
(retrospective consolidation — see [README](./README.md)).*
