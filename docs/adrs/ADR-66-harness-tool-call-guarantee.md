# ADR-66: Harness-level tool-call guarantee — every model drives tools or fails loud

**Status:** Accepted

Composes with ADR-61 (provider layer + factory), ADR-55 (intent gate — the
same silent-degradation class fixed by PR #43), and ADR-60 (agent runtime).
Amends ADR-61's implicit assumption that capability is uniform per provider.

**Date:** 2026-09-07

**Deciders:** Concerto architecture + maintainer direction

## Context

The pre-smoke review (2026-09-07) established that tool calling is a
per-provider accident, not a harness guarantee:

- Plugin-backed providers hardcode `tool_call: None` (`provider_host.rs`) —
  any tool-requiring task on them silently produces text.
- OpenCode Zen Muse models (Responses API dialect) omit tool declarations
  entirely (`opencode.rs`) — silent text-only degradation, auto-selected by
  model name.
- `supports_tool_calling: true` is hardcoded per provider; there is no
  per-model capability detection and no JSON-in-text fallback anywhere.
- Google has no loose-schema path, so weak Gemini models stall on strict
  schemas.
- Live hazard: `needs_responses_api()` matches `model.contains("muse")`,
  which also matches the `muse-spark-*` family — not Muse models. Served via
  Zen they would lose all tool declarations.

User-facing consequence: prompt/provider gymnastics ("use a native-tool
provider, avoid Zen-served muse-spark, avoid plugin providers") to get a
build. No other harness requires this, and it is unacceptable here: the
failure mode is always *silent* — a run that looks complete but did nothing.

## Decision

### 1. Tool use is a loop invariant, not a provider feature

Any run whose effective outcome requires tools (Execute, Plan, and any
coordinator dispatch) MUST either drive tools or fail with an explicit,
user-visible error naming the provider, the model, and the missing
capability. Silent text-only degradation on a tool-requiring task is a
defect of the same severity as a silent write.

### 2. Fail-loud contract at three seams

(a) **Selection:** resolving a tool-requiring task to a provider/model
without tool support is a hard error *before any spend*.
(b) **Request building:** a request carrying tool declarations that the wire
path cannot express (Responses API path, plugin protocol without tool ops)
must error — never send tool-less.
(c) **Response parsing:** a tool-requiring turn yielding no parseable tool
call and no final answer fails the turn loudly after bounded repair attempts
(structured error back to the loop → bounded retry → run-level failure),
never a silent completion.

### 3. Per-model capability resolution

`supports_tool_calling` is resolved per model at selection time with this
precedence: explicit config override > `list_models` capability flags (where
advertised) > built-in family table > provider default. Unknown models
attempt native first with automatic fallback (§4) — never silent text.

### 4. Universal text-fallback driver

A harness-level prompt-based tool driver (tool schemas injected into the
system prompt, structured tool-call blocks requested, strict parser with
repair-by-reprompt, bounded attempts) engages **automatically** when the
provider lacks native support. Native is always preferred; the fallback is
automatic, never configured. Fallback turns are labeled in the transcript
and audit (`tool_driver: fallback`) so the behavior stays observable.

### 5. Heuristic hygiene

Substring family heuristics must match whole family tokens, never bare
substrings: `muse-spark-*` must never classify as Muse. Heuristics sit last
in the §3 precedence and every heuristic ships with regression tests pairing
it against its known near-misses.

## Consequences

- Plugin providers: implement tool ops in the plugin protocol + host, or the
  factory gates them to AnswerOnly tasks with an explicit error. Silent text
  on tool tasks is removed either way.
- Zen Muse path: implement Responses-API tools, or refuse tool-requiring
  tasks loudly at selection time.
- Google weak models gain the loose-schema adapter path (same family as the
  OpenAI/Ollama adapters).
- Every capability refusal and every fallback engagement writes audit rows —
  observable and diagnosable, never silent.
- Fallback costs extra turns (parse + repair); bounded and labeled, and
  strictly cheaper than a silently useless run.

## Acceptance criteria

- **A1** — provider × {native, fallback} matrix covered by tests; plugin and
  Zen-Muse tool tasks either work or error before spend — never silent text.
- **A2** — `muse-spark-*` (and documented near-misses) never route to the
  Muse/Responses path; regression tests pin each heuristic.
- **A3** — capability-resolution precedence tested (override > flags >
  table > default); unknown model → native attempt + automatic fallback.
- **A4** — the Execute smoke test produces files identically on OpenAI-compat,
  Anthropic, Google, Ollama, and Zen non-Muse models; refusals name
  provider/model/capability.
- **A5** — fallback turns labeled in transcript + audit; bounded-repair
  exhaustion fails the run loudly.
