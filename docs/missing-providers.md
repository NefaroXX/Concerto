# Missing LLM Providers — Implementation Inventory

Analysis date: 2026-07-24

## Currently implemented (7 providers)

| # | Provider | Module | API Format |
|---|---|---|---|
| 1 | OpenAI | `openai.rs` | Native |
| 2 | Anthropic | `anthropic.rs` | Native |
| 3 | Google Gemini | `google.rs` | Native |
| 4 | OpenRouter | `openrouter.rs` | Thin wrapper around OpenAI |
| 5 | NVIDIA NIM | `nim.rs` | Thin wrapper around OpenAI |
| 6 | Ollama | `ollama.rs` | Native |
| 7 | OpenCode Zen | `opencode.rs` | Thin wrapper around OpenAI |

## Implementation pattern

Every OpenAI-compatible provider follows the same ~50-line thin-wrapper pattern.
The files to touch per provider:

| File | Change |
|---|---|
| `crates/providers/src/{name}.rs` | New wrapper struct delegating to `OpenAiProvider` with custom `api_base` |
| `crates/providers/src/provider_defs.rs` | Add match arm + `PROVIDER_TYPE_IDS` entry |
| `crates/providers/src/factory.rs` | Import + `matches!` guard + build arm |
| `crates/providers/src/lib.rs` | `pub mod` + `list_models_for_provider_async` arm |
| `crates/providers/src/budget.rs` | Context capacities for known models |
| `docs/models.md` | Update supported provider IDs table |

The desktop Settings UI picks up new providers automatically since it reads
`PROVIDER_TYPE_IDS` at compile time.

## Major providers NOT yet implemented

### Tier 1 — High-impact (all OpenAI-compatible)

| Provider | API Base URL | Why add it |
|---|---|---|
| **DeepSeek** | `https://api.deepseek.com` | Cheapest frontier models (V4 Flash at $0.14/$0.28 per MTok). 1M context. Huge popularity. Already partially referenced in `budget.rs`. |
| **Groq** | `https://api.groq.com/openai/v1` | Ultra-low latency (LPU hardware, 5-10x faster than GPU). Free tier available. Major OSS inference platform. |
| **Together AI** | `https://api.together.xyz/v1` | 200+ open-source models (Llama, Qwen, DeepSeek, Mistral). Very competitive pricing. |
| **Mistral AI** | `https://api.mistral.ai/v1` | European open-weight leader (Mistral Large 3, Codestral). EU data residency. |
| **xAI (Grok)** | `https://api.x.ai/v1` | Grok 4 models with 2M context. Fast-growing. |
| **Fireworks AI** | `https://api.fireworks.ai/inference/v1` | Production-grade open-weight inference. Strong enterprise SLAs. |
| **Cerebras** | `https://api.cerebras.ai/v1` | Wafer-scale inference, 2,100 tok/s on Llama 70B. Free tier. |
| **Cohere** | `https://api.cohere.ai/compatibility/v1` | Enterprise RAG/embeddings leader. Command A+ models. |

### Tier 2 — Notable but smaller market share

| Provider | API Base URL | Notes |
|---|---|---|
| **DeepInfra** | `https://api.deepinfra.com/v1/openai` | Stable inference, dedicated endpoints |
| **Perplexity** | `https://api.perplexity.ai` | Fast OSS model access, includes web search |
| **SambaNova** | `https://api.sambanova.ai/v1` | Cloud inference platform |
| **Alibaba (Qwen)** | `https://dashscope.aliyuncs.com/compatible-mode/v1` | Qwen3 models via DashScope API |
| **Moonshot AI (Kimi)** | `https://api.moonshot.cn/v1` | Kimi K2/K3 models |
| **Zhipu AI (GLM)** | `https://open.bigmodel.cn/api/paas/v4` | GLM-5 models |
| **Novita AI** | Various per-endpoint | Serverless GPUs, low cost |

### Tier 3 — Full SDK / Agent-runtime providers

These are not simple API endpoints. They are full agent SDKs with their own
runtime, tool invocation, session lifecycle, and streaming. Adding them requires
integrating the native SDK (Node.js, Python, Go, .NET, etc.) via FFI or a sidecar
process — fundamentally more complex than a thin API wrapper.

| Provider | SDK Languages | Integration Approach |
|---|---|---|
| **GitHub Copilot** | Node.js, Python, Go, .NET, Java | `github.com/github/copilot-sdk`. Full agent runtime. Supports BYOK. Requires Copilot subscription or BYOK API keys. |
| **Amazon Bedrock** | AWS SDK (all major langs) | AWS SDK integration |
| **Azure OpenAI** | Azure SDK + OpenAI-compat | REST API with Azure auth headers |
| **Google Vertex AI** | GCP SDK + OpenAI-compat | GCP SDK integration |
| **IBM watsonx** | Custom API | Enterprise LLM platform |

### Tier 4 — Retired / Shutting down

| Provider | Notes |
|---|---|
| **GitHub Models** | Closed to new customers June 2026; existing users migrate to Azure AI Foundry or Copilot's token-metered API |

## Key insight

Virtually all major providers are **OpenAI-compatible** on their chat completions
surface (Tiers 1 & 2). The existing `OpenAiProvider` with a custom `api_base` can
already talk to all Tier 1 and Tier 2 providers — users just need to know the
base URL and model ID.

GitHub Copilot is an exception: it exposes a full agent SDK (not a raw
completions API), making it a fundamentally different integration category
alongside AWS Bedrock, Azure OpenAI, and Vertex AI.

What's missing for first-class support on OpenAI-compatible providers:

1. **Named provider types** — proper branding, UI discovery, default model hints
2. **`provider_defs.rs` entries** — `ProviderDefinition`, display name, known models, `PROVIDER_TYPE_IDS`
3. **`factory.rs` build arms** — direct construction by name
4. **`lib.rs` model listing** — `list_models_for_provider_async` entries
5. **`budget.rs` context windows** — known model capacities + pricing data
6. **Tool call verification** — each provider's OpenAI-compatible surface needs testing

## Recommended implementation order

**Batch 1 — Market leaders:**
1. DeepSeek (cheapest frontier, already partially in budget.rs)
2. Groq (speed leader, free tier)
3. Together AI (broad OSS coverage)

**Batch 2 — Next tier:**
4. Mistral AI
5. xAI (Grok)
6. Fireworks AI

**Batch 3 — Nice-to-have:**
7. Cerebras
8. Cohere
9. DeepInfra

**Batch 4 — Full SDK integration (higher effort):**
10. GitHub Copilot (agent SDK integration, not a simple API wrapper)