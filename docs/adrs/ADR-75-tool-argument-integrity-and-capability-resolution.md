# ADR-75: Tool-argument integrity and capability-driven schema resolution

**Status:** Accepted
**Date:** 2026-09-29
**Deciders:** Concerto architecture
**Composes with:** ADR-66 (harness tool-call guarantee — §3 capability
precedence and §5 heuristic hygiene are the direct ancestors of this decision),
ADR-50 (tool coercion + binary read contract — the lenient-at-the-boundary
precedent), ADR-61 (provider layer + factory — the connector boundary this ADR
tightens). Amends ADR-66 §5's heuristic set for the tool-schema tier and
ADR-49/ADR-31's implicit assumption that a route/price tier predicts model
capability.

## Context

The `ToolSchemaMode::Auto` default decided whether a model received loose
(flattened) tool schemas and the non-streamed transport by **substring-matching
its name**:

```rust
pub fn is_weak_tool_calling_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ["mimo", "free", "mini"].iter().any(|hint| lower.contains(hint))
}
```

That predicate gated two coupled adaptations (loose schemas; `stream: false`).
Three defects were verified in the current tree:

1. **The `free` hint is a price tier, not a capability signal.** The same model
   weights are served behind `:free` routes; the distinction is commercial, not
   technical. Counterexample: `space-bunny-free` is MiniMax M3.1, a strong
   tool-caller, silently degraded by its route name. Any future model whose
   name contains `free`/`mini`/`mimo` inherits the same silent degradation with
   no evidence of an actual tool-calling problem.

2. **The failure the tier is meant to prevent was never detected.** When
   streamed tool-call arguments arrive truncated, the accumulated JSON fails to
   parse and was silently discarded: `serde_json::from_str(&args_str)
   .unwrap_or(serde_json::Value::Null)` in `opencode.rs`, `anthropic.rs`, and
   (behind a nominal "repair" whose first branch re-parsed the identical string
   that had already failed) `openai.rs`.

3. **The `Null` then executed a tool with no arguments.**
   `protocol::ensure_arguments_object` coerces `Null` to `{}`, and the executor
   ran it. A truncated tool call therefore performed a tool invocation with
   empty arguments — a correctness **and** safety defect, not merely a quality
   one.

Industry research confirms the root cause of streamed-argument truncation is
model/quantization behavior, not the route or price tier, and that the
community-standard capability channel is model-ID-keyed metadata
(LiteLLM's `function_calling` flag; OpenRouter's `supported_parameters`
including `"tools"`). Concerto already has that channel:
`ModelInfo::supports_tool_calling` (ADR-66 §3 precedence level 2).

## Decision

### 1. Detect and repair at the point of failure (model-agnostic)

Every connector parses accumulated tool arguments through **one** entry point,
`concerto_providers::tool_args::parse_tool_arguments`. It accepts valid JSON
untouched, and on failure attempts deterministic, vendor-neutral repair for the
real truncation/corruption modes:

- unbalanced `{`/`[` (append the missing closers, respecting nesting and
  string state);
- an unterminated string literal (close the quote);
- a trailing comma before `}`/`]`;
- trailing garbage after a complete top-level value (truncate to the value);
- single-quoted keys (the old `openai.rs` fixup, moved here).

Repair is validated by a real `serde_json` parse before it is trusted, so a
wrong guess falls through to the next mode rather than producing a corrupt
object. Nothing in the repair is model- or vendor-specific.

An empty/whitespace payload returns a distinct, non-error `Empty` outcome (the
tool genuinely received no arguments). A payload that remains unparseable
returns a `ToolArgumentParseError` carrying a bounded excerpt and the
underlying parse error. **`Value::Null` is never returned silently.**

### 2. Unrepairable arguments fail loudly — never a `Null` tool call

Connectors surface an unrepairable argument object through the existing typed
error channel (`ProviderError::InvalidResponse`, already classified as
recoverable by the retry layer), so the turn retries or fails loudly. Emitting a
tool call with silently-empty arguments is removed: it could only run a
destructive tool with `{}`. The orchestrator's tool-guard corrective path
remains the in-conversation fallback when a turn still produces empty
arguments.

Proxy double-encoding unwrapping (the row-#38 `MAX_ARGUMENT_STRING_LAYERS`
loop) is preserved as a separate bounded step
(`unwrap_argument_string_layers`), so a legitimate raw string argument such as
`"ls"` is still returned exactly as emitted.

### 3. Stop predicting capability from the price tier

`"free"` is removed from the name heuristic. The remaining hints (`mimo`,
`mini`) are explicitly labelled a **last-resort default**, not a capability
fact, and are documented as sitting below every level of the §3 precedence
chain. The `*-free` regression class (`space-bunny-free`, `qwen3-coder:free`,
…) is pinned as NOT weak.

### 4. Route the tier through the capability precedence — and remove name-based capability

The tool-schema/transport tier now consults the **same** precedence chain as the
dispatch gate, via `capability::resolve_tool_schema_mode`:

1. explicit `tool_schema_mode` (`Strict`/`Loose`) — user intent, always wins;
2. provider-advertised `ModelInfo::supports_tool_calling` — wins over the name
   heuristic;
3. the last-resort name heuristic (`mimo`/`mini`) — **presentation only**;
4. optimistic default `Strict` (verbatim schema, streamed transport).

The decisive property is the **default**: an unknown model with no advertised
metadata resolves to **`Strict`** — the optimistic/reliable end, consistent
with ADR-66 §3's "unknown models attempt native first, never silent text".
Defaulting unknown models to weak was the root cause of the misclassification.

Provider-advertised capability participates through
`ProviderFactory::build(config, creds)`, which reads the per-model flag captured
during discovery (`ProviderConfig::advertised_tool_support_for`) and threads it
into every connector's `with_advertised_tool_support` builder;
`ProviderFactory::build_with_capabilities(config, creds, advertised)` remains
the out-of-band entry point. `list_models_for_provider_async`/`_blocking` return
`ModelInfo` (id **plus** advertised capability) rather than bare ids, and
`ProviderConfig::record_discovered_models` persists the flag, so it survives
into the next run.

#### 4a. The built-in family table is removed; the name never decides capability

The Zen Responses dialect previously blacklisted `muse-v*`/`muse-spark-*` via a
hardcoded family table (`family_table_supports_tools` returned
`Some(false)`), because the Responses request builder emitted no `tools` field
and the stream parser handled no function-call events. That converted a
**converter gap into a permanent model exclusion**. This ADR completes the
Responses converter instead:

- the request body renders `request.tools` in the Responses flat function shape
  and round-trips `function_call` / `function_call_output` input items;
- the SSE parser accumulates `response.output_item.added`/`.done` and
  `response.function_call_arguments.delta`/`.done` into one `ToolCall`, parsing
  the final arguments through `tool_args::parse_tool_arguments`;
- the `CapabilityRefused` guard on tool-carrying Responses requests is deleted.

With the converter complete, the family table is gone. Dispatch capability has
**no name input at all**: `explicit declaration > provider-advertised metadata >
optimistic provider default` (plugin-backed providers remain the one
`false` default — their wire protocol has no tool ops). The `mimo`/`mini`
substring heuristic is renamed `last_resort_weak_tool_calling_model` and can
only select the loose presentation tier, never mark a model incapable.

## Consequences

- Strong models served behind `:free`/`mini`-looking routes are no longer
  silently degraded; unknown models keep the streamed, verbatim path.
- Truncated arguments are repaired when possible; when not, the turn fails
  loudly instead of executing a tool with empty arguments.
- The config dial (`tool_schema_mode`) remains the level-1 override and is
  unchanged for users.
- The remaining `mimo`/`mini` substring hints can still misclassify a model
  whose name happens to contain those tokens (e.g. `minimax`). This is
  accepted: the hints are last-resort, the advertised channel and explicit dial
  both override them, and the detect-and-repair path is model-agnostic.
- A new provider listing that advertises `supports_tool_calling` immediately
  improves tier accuracy without touching the heuristic.

## Acceptance criteria

- **A1** — one public parse entry point (`tool_args::parse_tool_arguments`);
  valid JSON untouched; each repair mode unit-tested; unrepairable input is an
  `Err`; empty input is a non-error `Empty`; the double-encoded unwrap retains a
  test.
- **A2** — no connector returns `Value::Null` silently on the tool-argument
  string path; unrepairable arguments surface `ProviderError::InvalidResponse`.
- **A3** — `"free"` removed from the heuristic; a `*-free` model is pinned as
  NOT weak; docs correcting the free-tier claim.
- **A4** — `resolve_tool_schema_mode` resolves
  `dial > advertised > family table > heuristic`, and defaults unknown models to
  `Strict`; regression tests pin each precedence level.
- **A5** — `cargo check --workspace`, clippy `-D warnings`, and the provider
  test suite are green.
