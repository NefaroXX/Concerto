# Multi-Provider LLM Architecture & Resilience: Comprehensive Research Document

> **Compiled:** August 2026
> **Scope:** Provider abstraction patterns, error handling taxonomies, retry strategies, circuit breakers, fallback chains, and production resilience patterns across major LLM platforms and gateways.
> **Origin:** Independent research thread by sol (augments `docs/ARCHITECTURE-V2.md`).

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Provider Abstraction Architectures](#2-provider-abstraction-architectures)
3. [Error Taxonomy by Provider](#3-error-taxonomy-by-provider)
4. [Retry Strategies & Patterns](#4-retry-strategies--patterns)
5. [Circuit Breakers](#5-circuit-breakers)
6. [Fallback & Graceful Degradation](#6-fallback--graceful-degradation)
7. [Agentic Workflow Resilience](#7-agentic-workflow-resilience)
8. [Streaming-Specific Error Handling](#8-streaming-specific-error-handling)
9. [Gateway Comparison Matrix](#9-gateway-comparison-matrix)
10. [Observability & Metrics](#10-observability--metrics)
11. [Production Implementation Checklist](#11-production-implementation-checklist)
12. [Code Reference Implementations](#12-code-reference-implementations)

---

## 1. Executive Summary

Production LLM systems in 2026 universally face the same challenge: **no single provider is reliable enough to bet everything on**. Rate limits spike without warning, entire regions experience multi-hour outages, models are deprecated, and context windows vary dramatically across providers. The difference between a fragile prototype and a production-grade system is not the model -- it is the **orchestration layer** that sits between your application and the APIs.

This document synthesizes patterns from:
- **LangChain** (50+ providers, BaseChatModel abstraction)
- **Vercel AI SDK** (25+ providers, provider registry, edge-native)
- **LiteLLM Proxy** (100+ providers, OpenAI-compatible unified API)
- **Portkey** (250+ models, enterprise gateway with circuit breakers)
- **Helicone** (observability-first gateway, ~8ms overhead)
- **Bifrost** (Go-based, 11 microsecond overhead at 5,000 RPS)
- **Production engineering guides** from Digital Applied, Grizzly Peak, LearnWithParam, and Statsig (as cited in references)

The core insight across all platforms: **treat provider failures as normal operating conditions, not edge cases.** Build retry logic that is precise, bounded, and classified by error type. Use circuit breakers to prevent cascading failures{0}implement fallback chains that degrade gracefully rather than failing hard.

---

## 2. Provider Abstraction Architectures

### 2.1 LangChain: BaseChatModel Pattern

- **BaseChatModel** defines *what* a chat model does, not *how*; concrete `ChatOpenAI`/`ChatAnthropic`/`ChatGoogleGenerativeAI` implement it.
- Do NOT wrap `BaseChatModel` in your own ABC (wrapping breaks `create_react_agent`, `.with_structured_output()`, `.bind_tools()`); instead use an enum-keyed factory returning real instances.
- Three caller shapes: plain invoke → BaseMessage; structured output → Pydantic; agent → agent result.

### 2.2 Vercel AI SDK: Provider Registry

- Modular provider packages (`@ai-sdk/openai`, `@ai-sdk/anthropic`, `@ai-sdk/google`); core `ai` package is unified (text/stream/tools/structured output).
- Env-var convention: `OPENAI_API_KEY` etc. auto-discovered.
- `createProviderRegistry` registers providers under aliases; `registry.languageModel('openai:gpt-4o')` selects.
- Custom provider = entry point + LM impl + input mapping + result processing + object generation.

### 2.3 LiteLLM: OpenAI-Compatible Proxy

- Single OpenAI-compatible API across 100+ providers; self-hosted Python service.
- Multi-tenancy hierarchy: Orgs → Teams → Users → Keys; per-key rate limits & budgets.
- Router settings: `routing_strategy` (simple-shuffle) and `fallback_strategy` (lowest-cost).
- Limitations: Python GIL caps single-instance concurrency (~500 RPS); enterprise add-ons paid.

### 2.4 Portkey & Enterprise Gateways

- 250+ models, conditional routing (latency/cost/rule), load balancing, automatic retries + circuit breakers, virtual key vault, 50+ guardrails, prompt library.

### 2.5 Supervisor / Router Pattern

- A cheap routed classifier (supervisor) recognizes intent; delegates to specialized sub-agents with own `provider/model/contextWindow/costTier`.
- State management across heterogeneous model boundaries is the hard part (avoid dropping context or duplicating token ingestion).

---

## 3. Error Taxonomy by Provider

### 3.1 OpenAI
| Error | HTTP | Retryable? |
|---|---|---|
| `invalid_request_error` | 400 | NO |
| `authentication_error` | 401 | NO |
| `permission_error` / `tokens_exceeded` | 403 | NO |
| `not_found_error` | 404 | NO |
| `rate_limit_error` | 429 | YES (Retry-After) |
| `server_error` | 500 | YES |
| `content_filter_error` | 400 | NO |

Headers: `x-ratelimit-remaining-requests|tokens`, `x-ratelimit-reset-requests|tokens`, `retry-after`.

### 3.2 Anthropic
- `APIConnectionError`: retryable; `RateLimitError` (429): retryable, read `retry-after`; `OverloadedError` (529): retryable; 5xx retryable; 4xx not.

### 3.3 Google Gemini
- 429 RESOURCE_EXHAUSTED retryable (~90% of complaints; free tier cuts Dec 2025); 503 UNAVAILABLE (MODEL_CAPACITY_EXHAUSTED) retryable, may last long; 500 retryable; 400/403/404 not.

### 3.4 Network
- timeouts / ECONNRESET / DNS / SSL / proxy timeouts: retryable.

## 4. Retry Strategies

- **Full jitter** (recommended): `min(base * 2^attempt, max)` sampled uniform in `[0, cap]`.
- Parameters: base 1s; max 30s; jitter ≤2s; max 4 attempts total.
- Respect `Retry-After` as a floor (+small jitter); never immediately after 429.
- **Never retry**: 4xx; **Always retry**: 429/5xx/timeouts. Conditional: on context-overflow retry only after prompt degradation; streaming resume if possible.

## 5. Circuit Breakers

- Three states CLOSED/OPEN/HALF_OPEN with failure_threshold (5), recovery_timeout (~60s), success_threshold (2).
- **Key = provider+model combo** (GPT4 outage must not block GPT3.5).
- **Half-open probe semaphore**: exactly one probe request; else fail fast — prevents thundering herd re-trip.

## 6. Fallback & Graceful Degradation

- Ordered fallback chains by priority; degrade capability, not availability.
- Prompt degradation ladder: strip few-shot → half system prompt → trunk middle of last user msg.
- **Capability-aware routing**: check tools/vision/json/context before routing.
- Three fallback categories: general (timeouts/5xx), content-policy, **context-window** (degrade prompt first or switch to bigger window).

## 7. Agentic Workflow Resilience

- Tools: wrap calls, structured error returns; idempotent design; circuit breakers for external APIs.
- **Idempotency & run IDs**: stable run id + per-side-effect idempotency key, upsert semantics — prevents duplicate side effects after retry/timeout.
- **Checkpointing**: persist at safe boundaries; durable worklist `WHERE status = pending`.
- **Loop guards**: max steps; loop detector over hashed context; timeout watchdog; token budget.

## 8. Streaming-Specific

- Heartbeat timeout (no data within ~15s ⇒ reconnect) — silent `ECONNRESET` drops.
- Resume = track last chunk/offset; or re-request and skip duplicated prefix.
- Timeout profiles per task type (classification 10s, summarization 30s, generation 60s, long 120s) scaled by estimated duration (≈50 tok/s, ×1.3 buffer).

## 9. Gateway Comparison (selected)

| Feature | LiteLLM | Portkey | Vercel | OpenRouter |
|---|---|---|---|---|
| Deploy | Self | Self/managed | Managed | Managed |
| Providers | 100+ | 250+ | 100+ | 100+ |
| Circuit breakers | Partial | Yes | Auto-failover | Yes |
| Budgets | per key | Yes | Yes | credit balance |
| MCP | Basic | No | No | No |
| Latency | High (Py) | Low | <20ms | Low |

## 10. Observability

- Log: error class+status, attempt+wait, trace/request ID, provider/model/endpoint. Never log bodies (secrets/PII).
- Metrics + alert thresholds: fallback hit rate >10%, retry rate >5%, breaker-open freq >1/h, P99 provider latency >2×, cost/1k, token burn per run.
- Transition logs: `{event: fallback_triggered, cause, from, to, attempt, latency_ms, timestamp}`.

## 11. Production Checklist (Condensed)

1. Read retry logic; if it retries on *any* exception ⇒ bug.
2. Classification: 429/5xx/conn only; 4xx raise after 1.
3. Bounded backoff + jitter; max 4 attempts.
4. Respect `Retry-After`.
5. Circuit breaker per provider+model; half-open = one probe.
6. Fallback chains (3-4 tiers, capability parity).
7. Idempotency keys and in-flight dedup.
8. Kill switches / feature flags.
9. Failure injection tests (fake 429/5xx) per path.
10. Metrics + structured transition logs; cost; DLQ.

## 12. Code Reference Implementations

- §12.1 Resilient client (Python): `RetryConfig`, `CircuitBreaker` + probe semaphore, per provider+model breaker map, `_is_retryable`, `retry-after` parse, in-flight dedup key (`sha256`).
- §12.2 FallbackRouter w/ `_fits_context` + `_degrade_prompt` (strip → half → middle-truncate).
- §12.3 CheckpointManager + LoopDetector (hash window 5) + ResilientAgent (max_steps 50, max_tokens 100k, checkpoint resume, step-boundary save, budget loop, idempotent-safe-boundary).

---

## References & Further Reading

1. AWS Architecture Blog — Exponential Backoff and Jitter
2. Martin Fowler — Circuit Breaker
3. OpenAI / Anthropic / Google API error references
4. LiteLLM docs; Vercel AI SDK docs; LangChain reference; Portkey AI Gateway docs
5. Production guides: Digital Applied; Statsig provider fallbacks; Grizzly Peak LLM error handling (2026).

*Document compiled by the user from production engineering guides, official SDK documentation, gateway architecture references, and community best practices (August 2026). Preserved here for provenance and to feed the ARCHITECTURE-V2 transport-resilience section.*