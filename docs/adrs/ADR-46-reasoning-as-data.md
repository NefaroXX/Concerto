# ADR-46: Reasoning content as first-class data

**Status:** Accepted (2026-08-07)
**Date:** 2026-08-07
**Deciders:** Concerto architecture
**Supersedes:** Phase 0 of the reasoning-detection cleanup
    (`docs/ARCHITECTURE-V2.md`)

## Context

DeepSeek-family hosted endpoints (OpenCode Zen, deepseek, Nvidia NIM) stream a
`reasoning_content` field alongside `content` on OpenAI-compatible chat
completions, and they reject a follow-up assistant message with HTTP 400 when
`reasoning_content` is passed back **while the model is still in "thinking"
mode** — yet require it (or its empty-string form) on assistant messages that
carry only a tool call. Concerto currently discards streamed reasoning and
never persists it, which loses the chain-of-thought for replay, audit, and
subsequent turns. The two requirements that pull in opposite directions are:

1. **Capture**: reasoning must be read off the stream, carried on the message
   value, and persisted so later turns, replay and memory can consume it.
2. **Echo**: the same reasoning must NOT be blindly echoed to every provider.
   A provider that rejects the field while in thinking mode needs a different
   echo policy than one that requires it.

## Decision

Make reasoning a first-class field on both the message and the streaming
boundary, capture it exactly once at the stream boundary, persist it, and
re-emit it to the provider under an explicit, configurable echo policy.

### 1. Model as data

- `concerto_core::types::Message` gains
  `#[serde(default)] pub reasoning_content: Option<String>`. The serde default
  keeps legacy persisted JSON (without the key) deserializable.
- `completion_core::types::CompletionChunk` gains `reasoning: Option<String>`.
  Reasoning may arrive in many small SSE deltas, so `CompletionChunk::reasoning`
  is a per-delta fragment; the collector concatenates them.
- Phase 2 (not in this ADR's scope) will split the OpenAI dialect so reasoning
  becomes a full **Part** on the wire (like Anthropic's `thinking` blocks).
  Until then it stays an `Option<String>` on the native types.

### 2. Capture at the stream boundary

- `OpenAiStreamState` accumulates each incoming `reasoning_content` delta into a
  per-turn `reasoning_buffer` and flushes it once, immediately before the
  terminal chunk, as a single `CompletionChunk` with `reasoning =
  Some(...)`. Empty reasoning never produces a chunk.
- `prompts::collect_stream`, `collect_stream_with_timeouts` and
  `complete_provider_request` change their return from
  `(String, Vec<ToolCall>)` to `(String, Option<String>, Vec<ToolCall>)`,
  concatenating non-`None` reasoning fragments and normalizing an empty buffer
  to `None`. Every caller propagates it onto the assistant `Message` it builds.

### 3. Echo policies

- `OpenAiProvider` gains a `reasoning_echo` field (default `IfPresent`):
  - `IfPresent`: emit `reasoning_content` JSON only when the underlying
    assistant message carries reasoning.
  - `Always`: emit `reasoning_content` on **every** assistant message, using an
    empty string when no reasoning exists — the form DeepSeek-backed endpoints
    accept.
- OpenCode Zen wires `ReasoningEcho::Always` on its inner `OpenAiProvider`.

### 4. Persistence

- New migration `sessions/migrations/022_reasoning_content.sql`:
  `ALTER TABLE messages ADD COLUMN reasoning_content TEXT NULL;`
- `save_message` / `append_messages` insert `reasoning_content` (SQL `NULL` when
  the option is `None`); `load_messages` selects it back onto
  `Message.reasoning_content`. Old rows decode with `None`.

## Consequences

- **Positive**: chain-of-thought survives streaming, is recoverable from a
  session, and can be re-sent to providers that require it; DeepSeek-family
  endpoints get the empty-string echo their contract expects on tool-call-only
  turns, avoiding the HTTP 400 hard stop.
- **Negative**: a reasoning field now threads through message construction
  everywhere; a few extra bytes per persisted assistant message. The provider
  wire format still carries reasoning inline rather than as a content part.
- **Risks**: other providers (Anthropic, Google, Ollama, NIM variants) do
  **not** yet populate `reasoning_content` on the stream — they currently emit
  `None` — so reasoning is populated only on the OpenAI-compatible path today.
  This is intentionally deferred to the Phase 2 dialect split. The echo policy
  default (`IfPresent`) means most providers are unaffected; only OpenCode Zen
  opts into `Always`.
- **Migration**: additive; the new message column is nullable and legacy rows
  load as `None`, so no backfill or data rewrite is required.

## Review notes

- The `IfPresent`/`Always` split exists precisely because a provider family
  (DeepSeek) *rejects* reasoning in thinking mode but *requires* the field's
  presence (or empty) on tool-call turns — a single policy cannot satisfy both.
- `CompletionChunk` carries the raw per-delta reasoning; normalization to a
  single `Option<String>` happens in `collect_*`, keeping the chunk type a
  faithful record of the wire while the message type is the normalized model.