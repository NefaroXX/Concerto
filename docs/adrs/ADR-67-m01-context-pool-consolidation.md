# ADR-67: M-01 — Consolidate context-overflow pools under a single owner per pool

**Status:** Accepted

Amends [ADR-16](./ADR-16.md) (tiered budget with LLM summarization) and
composes with [ADR-48](./ADR-48-context-engine.md) (ContextEngine v2) and
[ADR-65](./ADR-65-evidence-spine.md) (facts on the append-only chain).
Supersedes: none.

**Date:** 2026-09-18

**Deciders:** Concerto architecture + maintainer direction

## Context

ADR-16 introduced a tiered budget allocator with three pools (RAG, Working,
Conversation). The implementation spread ownership across unrelated modules:

- **RAG pool:** `truncate_to_rag_limit` lives in `short_term.rs:40-177` as
  a free function operating on raw `Message` slices — no pool owner, no
  coordination with the ContextEngine.
- **Working/Conversation pools:** `ContextEngine` owns assembly but
  `ContextGuardProvider` independently evicts conversation entries, creating
  a double-clip (agent_loop:1003 clips, guard:84 clips again) where the
  effective budget is unknowable.
- **Summarization:** `SummarizeOldest` is a fallback loser — no code path
  selects it today, but it carries tests and surface area that obscure the
  real decision path (`NoOp`).
- **Observability gap:** the generic budget-overflow loop in
  `generic.rs:402` either silently drops entries or has an undocumented
  contract — unclear whether it is a correct implementation or a latent
  bug.

The net effect: two independent clip sites per pool, an orphaned
summarizer, and no single module accountable for "what happens when a pool
overflows."

## Decision

### 1. One owner per pool

Each budget pool has exactly one module responsible for enforcement:

| Pool | Owner | Responsibility |
|---|---|---|
| RAG | `ContextEngine` | Calls `truncate_to_rag_limit` internally; removes the free function from `short_term.rs`. |
| Working | `ContextEngine` | Clip + eviction logic consolidated here; removes `ContextGuardProvider` as an independent clip site. |
| Conversation | `ContextEngine` | Summarization or truncation as configured; single entry point. |

### 2. Remove `SummarizeOldest` — keep `NoOp`

`SummarizeOldest` is deleted. The `NoOp` summarizer remains as the
default — it documents the intentional decision to truncate rather than
summarize. LLM-based summarization may return as a configurable option
behind a feature flag in a future ADR; this ADR does not block that.

### 3. Double-clip eliminated

With `ContextGuardProvider` removed as an independent clip site, the
ContextEngine is the sole arbiter of budget enforcement. The agent-loop
clip (agent_loop:1003) delegates to ContextEngine rather than clipping
independently.

### 4. `generic.rs:402` — document or fix

The overflow handling at `generic.rs:402` is either a correct
implementation of the budget contract or a latent defect. This ADR
requires: inspect the code, add a doc-comment stating the invariant, and
if the invariant is violated, fix it. The doc-comment is the deliverable
for this ADR; a behavioral fix may require a follow-up ADR if the scope
is non-trivial.

### 5. Effort classification

This is a **gate-only S effort**: the changes are confined to context
assembly and budget enforcement, touch a small surface area, and do not
alter external behavior beyond eliminating the double-clip.

## Consequences

- **Single audit point.** Budget violations are diagnosable from one
  module instead of two.
- **Test surface reduced.** `SummarizeOldest` tests are deleted; the
  remaining tests exercise the real decision path.
- **Agent loop simplified.** The clip call in agent_loop:1003 becomes a
  ContextEngine method call, removing direct message-manipulation logic
  from the loop.
- **`short_term.rs` slimmed.** The free `truncate_to_rag_limit` function
  moves into ContextEngine or is replaced by a ContextEngine-owned
  equivalent.

## Follow-ups (not in this ADR)

- **`rag_pct` configurability.** Making the RAG pool percentage
  configurable via `[context]` config — a small config-surface addition.
- **Estimator deduplication.** The token estimator may be called
  multiple times for the same message across pool boundaries; a dedup
  cache or shared estimator would reduce redundant work.

## Acceptance criteria

- **A1** — `ContextGuardProvider` no longer clips independently; all
  budget enforcement routes through `ContextEngine`.
- **A2** — `SummarizeOldest` is deleted; `NoOp` is the default.
- **A3** — `truncate_to_rag_limit` no longer exists as a free function
  in `short_term.rs`.
- **A4** — `generic.rs:402` has a doc-comment stating the invariant;
  if the invariant is violated, the code is fixed.
- **A5** — Agent-loop clip delegates to ContextEngine; no independent
  message manipulation in the loop.

---

*Last updated: 2026-09-18*
