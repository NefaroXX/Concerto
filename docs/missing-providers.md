# Missing LLM Providers — Implementation Inventory

Analysis date: 2026-07-24 · Updated: 2026-09-19

## Currently implemented (22 providers)

All 22 named provider types are registered in `provider_defs.rs:346-369`
(`PROVIDER_TYPE_IDS`). Original 7 + DeepSeek + 7 Tier-1 + 7 Tier-2.

| # | Provider | Type ID | API Format |
|---|---|---|---|
| 1 | OpenAI | `openai` | Native |
| 2 | Anthropic | `anthropic` | Native |
| 3 | Google Gemini | `google` | Native |
| 4 | OpenRouter | `openrouter` | Thin wrapper around OpenAI |
| 5 | NVIDIA NIM | `nim` | Thin wrapper around OpenAI |
| 6 | Ollama | `ollama` | Native |
| 7 | OpenCode Zen | `opencode` | Thin wrapper around OpenAI |
| 8 | DeepSeek | `deepseek` | Thin wrapper around OpenAI |
| 9 | Groq | `groq` | Thin wrapper around OpenAI |
| 10 | Together AI | `together` | Thin wrapper around OpenAI |
| 11 | Mistral AI | `mistral` | Thin wrapper around OpenAI |
| 12 | xAI (Grok) | `xai` | Thin wrapper around OpenAI |
| 13 | Fireworks AI | `fireworks` | Thin wrapper around OpenAI |
| 14 | Cerebras | `cerebras` | Thin wrapper around OpenAI |
| 15 | Cohere | `cohere` | Thin wrapper around OpenAI |
| 16 | DeepInfra | `deepinfra` | Thin wrapper around OpenAI |
| 17 | Perplexity | `perplexity` | Thin wrapper around OpenAI |
| 18 | SambaNova | `sambanova` | Thin wrapper around OpenAI |
| 19 | Alibaba (Qwen) | `dashscope` | Thin wrapper around OpenAI |
| 20 | Moonshot AI (Kimi) | `moonshot` | Thin wrapper around OpenAI |
| 21 | Zhipu AI (GLM) | `zhipu` | Thin wrapper around OpenAI |
| 22 | Novita AI | `novita` | Thin wrapper around OpenAI |

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
`PROVIDER_TYPE_IDS` at compile time. Each thin-wrapper also includes the
flat proxy tool-call Fix 1 fallback from `openai.rs:326`.

## ~~Major providers NOT yet implemented~~ — Tier 1 & 2 DONE

All Tier-1 and Tier-2 providers from the original analysis are now implemented
(see table above). The "Key insight" and "Recommended implementation order"
sections below are historical and no longer apply to Tier 1/2.

### Tier 3 — Full SDK / Agent-runtime providers (still open)

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

### Tier 4 — Retired / Shutting down (historical)

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

## ~~Recommended implementation order~~ (historical — Tier 1/2 all done)

**Batch 1 — Market leaders:** ✅ DeepSeek, ✅ Groq, ✅ Together AI
**Batch 2 — Next tier:** ✅ Mistral AI, ✅ xAI (Grok), ✅ Fireworks AI
**Batch 3 — Nice-to-have:** ✅ Cerebras, ✅ Cohere, ✅ DeepInfra, ✅ Perplexity, ✅ SambaNova, ✅ DashScope, ✅ Moonshot, ✅ Zhipu, ✅ Novita
**Batch 4 — Full SDK integration (still open):**
10. GitHub Copilot (agent SDK integration, not a simple API wrapper)