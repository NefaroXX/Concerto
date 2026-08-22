# ADR-48: ContextEngine v2 — deterministic context assembly

**Status:** Accepted (2026-08-07)
**Date:** 2026-08-07
**Deciders:** Concerto architecture
**Supersedes:** Phase 3 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`)
**Composes with:** ADR-46 (reasoning-as-data), ADR-45/42 (fallback ladder) —
    these remain valid and unchanged.

## Context

The single-agent loop builds its provider request from in-memory messages that
are seeded from the runtime session: `runtime_runner` →
`context_compaction::refresh_and_materialize` → `SessionStore::load_messages`,
bounded as **durable checkpoints + a recent uncompacted tail**. The active
history is deterministic and never mutates the source transcript.
`CompactionPolicy` defaults are `trigger_tokens=16_000`,
`retain_user_turns=4`, `minimum_user_turns=6`
(`crates/orchestrator/src/context_compaction.rs`).

Facts established by a read-only audit of the current code:

- Compaction creates checkpoint summaries as bounded system messages that
  include tool-call and tool-result excerpts; source messages are never
  modified. `reasoning_content` is deliberately **not** copied into checkpoints.
- Token accounting: the `messages.tokens_in`/`tokens_out` columns exist but are
  always written as `0`. Estimates use `bytes div_ceil(4) + 4` (≈ 4 bytes per
  token plus a per-message overhead).
- `ContextGuardProvider` is the per-request backstop
  (`crates/providers/src/context_guard.rs`): budget = `capacity −
  output_reserve − safety_margin`; it applies a deterministic reduction — clip
  marked blocks (retrieved memory, working memory, prior results, changed-file
  context, …), drop the oldest user/assistant conversation groups with a
  compaction-summary insertion, clip old optional messages, then fail with
  `ProviderError::ContextOverflow`.
- In-run LLM overflow summarization (`SummarizeOldest`) is **disabled** in the
  production runtime (audit C-03, `runtime_runner.rs:1539`) and stays disabled.
- There is no provider-cache-breakpoint awareness anywhere — no `cache_control`
  (Anthropic), no pooled-prefix handling. DeepSeek-family pooled caching and
  Anthropic prompt caching reward consecutive turns that share a
  **byte-identical** prefix.
- Multi-agent specialists already see only per-task prompts
  (`working_memory` + `previous_results`), never a long transcript. That design
  is **unchanged** by this ADR.

## Decision

Ship ContextEngine v2: **deterministic, no-LLM-in-loop** context assembly with
(1) a pure-function planner, (2) two-stage budgeting, (3) prefix discipline,
(4) honest token accounting, (5) an audit event trail, (6) additive config, and
(7) an explicit kept-behavior list. In-loop LLM summarization stays forbidden.

### 1. Deterministic, no-LLM-in-loop context assembly

Assembly is a pure function `(persisted transcript, budget config, request) →
rendered request`, shaped as `head (system/fixed-instruction block) +
summary/checkpoints + bounded tail`. No model call, no streaming, no mutation.

- **Why.** A failed in-loop summarizer must never be able to break a run or
  corrupt history. The current checkpoint machinery already implements this;
  v2 surfaces it as a configurable engine.
- **Trade-off.** Deterministic summaries are lossy where an LLM rewrite might
  preserve more; accepted — full history is appended-only for audit/replay, so
  the fidelity loss is at the *window*, never the *source*.

### 2. Two-stage budgeting

a. **Planning stage (assembly time).** A deterministic planner runs over the
   adaptive token estimate before the request is sent. When the estimate
   exceeds the configured trigger, **structural compaction** happens first —
   keep the recent tail and the checkpoint frontier, insert or reuse
   checkpoints. This lands in the existing
   `refresh_and_materialize` / `maintain_after_run` cadence.
b. **Enforcement stage (backstop).** `ContextGuardProvider` stays wired as the
   final safety net for residual misses. If the request still overflows after
   planning, the guard reduces deterministically or returns
   `ContextOverflow`.

Defaults preserve current behavior: `16000` trigger tokens /
`4` retained user turns / `6` minimum user turns.

- **Why**: plan first avoids burning a provider round-trip on a doomed request;
   the backstop still catches estimate-vs-capacity drift and adapter quirks.
- **Trade-off**: two estimational passes; benign because both share the same
   estimator and the second is exit-only.

### 3. Prefix discipline (cache-friendly stable head)

The rendered head must be **byte-identical across consecutive turns on the same
session whenever its inputs are unchanged**, giving a stable window for
server-side prefix caching (DeepSeek-family pooled caching, Anthropic prompt
caching). Dynamic content (memory, working context, results) is injected into
the tail, never interleaved into the head. Anthropic `cache_control`
breakpoint markers are an **opt-in extension point**, not in M0.

- **Why**: server-side prefix caching prices by byte identity; keeping dynamic
  data out of the head maximizes cache reuse without new wire features.
- **Trade-off**: ordering constraints on prompt assembly; volatile additions
  must be versioned so a changed seed visibly invalidates the cached prefix
  instead of silently re-rendering a different head.

### 4. Token accounting becomes real

Provider-reported usage (`usage.*`/`tokens_*` from the completion response) is
recorded per message **when available** and preferred over the estimate; the
estimate remains the fallback. Nothing in this ADR implements the write path —
the contract is the policy ("real when present, else estimate"); the
implementation lands as a follow-up milestone.

- **Why**: `tokens_in`/`tokens_out` are inert (always `0`); real usage lets
  planning calibrate its estimator and spend/replay become trustworthy.
- **Trade-off**: usage arrives at turn end, so planning still relies on
  estimates for the next request; only historical accounting is exact.

### 5. Auditability

Every compaction / overflow action emits an EventBus event with: reason,
before/after token estimates, and turns kept (Darwin-consistent, replayable).
The transcript and checkpoints already persist; add an explicit event stream so
humans and dashboards can review the space.

- **Why**: "everything observable & replayable" is a stated V2 property; a
  context trim without a trace is a silent behavior change.
- **Trade-off**: one extra event per trim — negligible volume.

### 6. Config surface (`[context]`)

Additive `[context]` table with knobs defaulting to the current policy:
`trigger_tokens`, `retain_user_turns`, `minimum_user_turns`, and an optional
`cache_stable_prefix` flag. Additive serde-default only — no config removals,
no breaking migration. Defaults = current behavior.

- **Why**: Phase 3 commits to "config knobs exposed; default = current
  behavior" — this is that surface, on serde defaults so old configs load
  unchanged.
- **Trade-off**: configurable knobs must be kept in sync with the engine
  constants; because the defaults match today, the first ship is a no-op.

### 7. Kept behavior (explicit)

- Single-agent **tail seeding** from the session (bounded checkpoints + recent
  tail) stays the active path.
- Multi-agent specialist prompts (`working_memory`/`previous_results`) are
  untouched.
- Tool calls/results are preserved in checkpoints.
- `reasoning_content` stays persisted in the transcript but is **not** copied
  into checkpoints.
- System messages are always preserved.

These are frozen contract pieces: any change to them is a behavior change that
must be documented as a new ADR. Keeping them explicit scopes the v2 work to
the seven decisions above.

## Consequences

- **Positive.** Bounded, reproducible requests; no new in-loop LLM risk; shared
  stable prefix for provider-side caching; honest accounting when available;
  audit trail for every context change; config that defaults to today; Phase 3
  of the redesign lands with no regressions on the current 16000/4/6 behavior.
- **Negative.** Deterministic summaries may not capture nuance an LLM rewrite
  would; real usage writes are deferred to a follow-up milestone, so planning
  continues on estimates.
- **Risks.** Prefix-stability relies on a stable head actually staying stable —
  covered by an integration test asserting the head is byte-identical across
  two consecutive turns on the same session.
- **Migration.** Additive only. `[context]` defaults match the current
  policy; persisted data and transcript layout are unchanged.

## Review notes

- ADR-45/42 (fallback ladder) and ADR-46 (reasoning-as-data) are explicitly
  orthogonal: failures identified in this plan are testability/context errors,
  handled here; protocol or provider failures stay on the proven ladder.
- The multi-agent coordinator is deliberately out of scope; its isolation
  guarantees (fresh windows, summaries returned) already meet the redesign's
  principle 6.