# ADR-47: Canonical message parts (deferred, flat model retained)

**Status:** Deferred (2026-08-08) — the canonical parts structure is recorded
    as the intended target but **not** implemented; the retained interim shape
    is the flat text/content model on `concerto_core::types::Message` today.
**Date:** 2026-08-08
**Deciders:** Concerto architecture
**Supersedes:** Phase 2 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`) — the canonical-IR "message parts" item that
    this decision ratifies, then defers.
**Composes with:** ADR-46 (reasoning-as-data) — `reasoning_content` stays a
    flat `Option<String>` and only becomes a full `Part` during the dialect
    split this ADR defers.

## Context

The V2 redesign defines a canonical IR with structured message content
(`docs/ARCHITECTURE-V2.md` §2.1):

- `Part` sealed enum: `Text { text }`, `Reasoning { text }`,
  `ToolCall { id, name, args }`, `ToolResult { id, name, content }`,
  `Thinking { text, signature? }`, `RedactedThinking { data }`,
  `File { data, mime }` (future).
- `Message { role, parts: Vec<Part> }` — explicitly stated to **replace** the
  flat `String content + tool_calls/tool_results` on
  `concerto_core::types::Message`.

Today the code still ships the flat shape that this ADR would replace:

- `Message { role, content: String, tool_calls: Option<Vec<ToolCall>>,
  tool_results: Option<Vec<ToolResult>> }` plus the additive
  `reasoning_content: Option<String>` (ADR-46) and
  `tokens_in` / `tokens_out` (ADR-48) fields
  (`crates/core/src/types.rs`).

ADR-46 explicitly outsources the wire-level part split: "the OpenAI dialect so
reasoning becomes a full **Part** on the wire (like Anthropic's `thinking`
blocks) … Until then it stays an `Option<String>` on the native types." That
split is exactly the Phase 2 work deferred here.

The plan's own Phase 2 text keeps the flat conversation:
"Rebase `orchestrator` + `prompts.rs` onto the new IR (**keep flat text conv
for skill-injection paths until context engine**)." Phase 3 (ADR-48) assembled
on the flat message shape and froze its flat rendering path
(`ADR-48` decision: "`reasoning_content` stays a flat `Option<String>`");
Phase 5 (ADR-52) shipped orchestration safety gates on the same flat shape.

## Decision

**Ratify the parts structure as the canonical message contract, but do not ship
it now: record it as deferred.** The flat message shape is retained unchanged
until a consumer actually requires richer content.

The decision is therefore:

- The `Part`/`parts` IR from §2.1 of the plan is the **accepted future
  contract**, not a rejected alternative — this ADR keeps the design on the
  record so it is not re-litigated later.
- **No parts migration ships with Phase 1–5 work** (dialect split, Context
  Engine, config-first catalog, orchestration gates). All of that landed on
  the flat model without behavioral regression.
- A parts migration is a **persistence-format change** (session message rows,
  checkpoints, replay), so it must not land as a side effect of an unrelated
  feature — it needs a dedicated migration window with `serde(default)` and a
  legacy-column fallback so old DBs replay.

### Why deferred

- **Priorities.** ADR-48 (context engine) and ADR-52 (orchestration safety
  gates) addressed higher-value gaps on the flat model; restructuring the
  message type added risk without a change to those decisions.
- **No migration driver.** The flat `content` + `tool_calls`/`tool_results` +
  `reasoning_content` shape already carries everything the production loop
  reads and writes. Transferring it to `parts` changes serialization,
  session storage, replay, and every consumer without delivering a new
  capability.

### Trigger to reopen

Reopen when a consumer proposes/needs **richer message content that the flat
shape cannot express**, for example:

- tool-use message bodies where `content` and `tool_calls`/`tool_results`
  must carry structured per-call content (multi-part tool results);
- multi-modal or typed parts: image/file parts (`File { data, mime }`),
  structured `Thinking` with signature / `RedactedThinking` data on
  Anthropic/Gemini paths;
- any consumer that would otherwise have to smuggle structured data into the
  single `content: String`.

When any of these land, ship the full parts migration (§2.1 contract) together
with the persistence migration in one scoped change and update ADR-46's
"reasoning becomes a Part" line at the same time.

## Consequences

- **Positive.** The design stays on the record; no backward-compat break
  (nothing changed); the flat model remains the stable, tested contract for
  all current consumers; priorities stay on ADR-46/ADR-49/ADR-50/ADR-52 work.
- **Negative.** Rich message content (per-call tool bodies, images,
  thinking-with-signature) is unavailable until shipped. Every dialect still
  lowers flat content to its wire form.
- **Risks.** The planned parts migration is one of the largest serialization
  surface changes in the codebase — `docs/ARCHITECTURE-V2.md` §10 risks:
  "Message/Parts rollout touches persistence format — keep `serde(default)` +
  legacy column fallback so old DBs replay." Because it is deferred, the flat
  shape must stay byte-identical so nothing silently starts depending on a
  half-migrated message model.
- **Migration.** None today. When shipped: additive, `serde(default)` + legacy
  column fallback, one scoped pending window (see Decision).

## Review notes

- Composes upstream: ADR-46 (reasoning stays flat until this ships), ADR-48
  (context assembly keeps flat head/tail; `reasoning_content` not copied into
  checkpoints — unaffected by parts), ADR-52 (orchestration gates are
  message-shape-agnostic). ADR-42/45 (fallback ladder) unchanged.