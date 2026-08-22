# Concerto Architecture V2 — Provider-First Redesign (Researched Plan)

Status: **Draft for review** · Date: 2026-08-07 · Owner: sol
Companion research: three external deep-dives (opencode internals, reasoning wire
protocols, production harness orchestration/context) + current-code inventory.
This document supersedes the ad-hoc patch approach: it is the plan for making
Concerto *the* configurable, provider-agnostic multi-agent environment, with
`reasoning` as first-class data, tool schemas that adapt per provider, and a
context engine that manages token budgets like production harnesses do.

## 0. Why this exists (the four failure classes we actually hit)

From a real recorded run (strigil task: Architect on NIM Nemotron, everything else
on OpenCode big-pickle/deepseek/openrouter), four distinct failure classes, each
now confirmed in code AND in upstream ecosystem research:

| # | Failure observed | Root cause (code) | Root cause (ecosystem) |
|---|---|---|---|
| 1 | `400: The 'reasoning_content' in the thinking mode must be passed back to the API` on every retry (Coder = opencode big-pickle) | `reasoning_content` appears **nowhere** in the crate: `openai.rs:handle_event` drops `delta.reasoning_content`; `core/types.rs Message` has no field; `build_assistant_message` never emits it | DeepSeek's documented contract: once tools are involved, **every** assistant message in history must carry `reasoning_content` (or `""`) or the API returns 400. opencode fixed this via "unconditional injection" (PRs #24146/#24250/#24443, `resolved = extracted \|\| existing \|\| ""`); Concerto has none of it |
| 2 | `Tool git failed: invalid type: string "5", expected u32` (Reviewer) | `git.rs line 463` strict `serde_json::from_value::<GitInput>`; schema says `integer`, model sent string | Harnesses (Claude/opencode) coerce/repair tool args; SDKs implement `experimental_repairToolCall`. We do not |
| 3 | `Tool filesystem failed: stream did not contain valid UTF-8` (Researcher) | `virtual_fs.rs` uses `read_to_string` → hard error on binary | Real harnesses return "binary file (N bytes)" or decode; failing the model pollutes context |
| 4 | Planning-phase model death killed whole run | design-stage `.await?` bypassing ladder (fixed this session, commit 4f83c51) | The ladder now covers planning; BUT if the *request itself* is protocol-broken (failure 1), no ladder can help |

**Synthesis**: failures 1–3 are architectural, not incidental. Failure 4's fix
stays, but the architecture must make such failures self-healing and configurable.

---

## 1. Architecture principles

1. **Providers are dialects of one canonical protocol.** One internal model:
   `Part`s, tool calls as data, reasoning as parts. A per-provider *dialect
   adapter* lowers canonical→wire and parses wire→canonical. No provider-specific
   logic may leak into the loop, planner, or sessions layers.
2. **Reasoning is first-class data.** Capture, store, replay verbatim. Never
   rebuild it from content. Echo per a **capability matrix**, not provider ID.
3. **Everything is config data.** Any provider/model/role/tool/permission/budget
   is expressible in config — including entirely-new OpenAI-compatible endpoints
   and NIM/DeepSeek/OpenRouter variants — without recompiling* (*plugins for
   non-dialect providers; see §6).
4. **Policy is a hard gate, not a hint.** (Claude Code doc: "rules enforced by
   client"; prompt shapes behavior.) We already do this via SimplePolicyEngine;
   extend to deny→ask→allow ordering, doom-loop guards, timeout/step caps.
5. **Context is budgeted, not unlimited.** Compaction, prompt-cache-stable
   prefixes, head+tail+summarize-middle, append-only audit. Keep model-relevant
   guidance (skills, AGENTS) as a *stable prefix* — never interleave volatile data.
6. **Isolation for subagents** (fresh window; only a summary returns) — the
   highest-leverage quality feature observed across Claude Code, opencode,
   OpenHands, LangGraph supervisor.
7. **Everything observable & replayable** (event bus, session/checkpoint layer
   unchanged in spirit, audit trail append-only).

---

## 2. Target architecture (layer diagram)

```
                    ┌────────────────────────────────────────────┐
                    │  CONFIG (single source of truth)            │
                    │  providers, models, roles, tools,           │
                    │  permissions, budgets, compaction, MCP      │
                    └───────────────┬────────────────────────────┘
                                    │
   ┌────────────────────────────────▼──────────────────────────────────┐
   │ LLM RUNTIME (crates/llm)                                          │
   │  • Model catalog (models.dev-merged config)                       │
   │  • Capability matrix (reasoning format, thinking mode,            │
   │    tool styles, cache, budget limits, timeout)                    │
   │  • Dialect adapters: openai-chat / openai-compat / anthropic-     │
   │    messages / gemini / ollama / nim / openrouter / bedrock        │
   │  • Canonical IR: parts, messages, tools                           │
   │  • Request lowering  (content-parts → wire)  w/ reasoning echo    │
   │  • Stream reducer   (wire → canonical events, reasoning capture)  │
   │  • Retry/backoff, timeouts, SSE, keepalive, auth/keyring          │
   └───────────────┬────────────────────────────▲──────────────────────┘
                   │                            │
   ┌───────────────▼───────────────┐  ┌─────────┴──────────────┐
   │ CONTEXT ENGINE (crates/*)     │  │ TOOL LAYER (crates/*)  │
   │ token budgeter & trimmer       │  │ canonical ToolSchema   │
   │ prompt-cache prefix discipline │  │ per-dialect projection │
   │ compaction (head+tail+summary)│  │ coercion/repair        │
   │ audit: append-only             │  │ sandbox gates          │
   └───────────────┬───────────────┘  └─────────┬──────────────┘
                   └───────────┬────────────────┘
                ┌──────────────▼───────────────────┐
                │ ORCHESTRATION (crates/orchestrator)
                │  loop · planner-as-data · ladder  │
                │  (incl. 4f83c51 design-stage fix) │
                └───────────────────────────────────┘
```

### 2.1 LLM runtime (`crates/llm` new — extraction of provider-facing logic)

- **Model catalog**: `catalog()` combines (a) built-in constants (current registry.rs
  budgets) and (b) a user config `models: [...]` — matching opencode's
  `models.dev → merge config/env → resolve`. At least: id, provider, api type
  (`native` or `sdk`-style enum), context_budget, cost per MTok, temperature
  default, max_tokens, capabilities (`reasoning`, `thinking` modes, `tool_choice`
  styles, `cache`), timeout.
- **Canonical IR**:
  - `Part` sealed enum: `Text { text }`, `Reasoning { text }`, `ToolCall { id, name, args }`,
    `ToolResult { id, name, content }`, `Thinking { text, signature? }`
    (Anthropic includes `signature`, Gemini `thoughtSignature`), `RedactedThinking { data }`,
    `File { data, mime }` (future).
  - `Message { role, parts: Vec<Part> }` (replaces flat `String content +
    tool_calls/tool_results` on `core/types.rs:Message`).
  - `Request { model, messages, tools, tool_choice, temperature, max_tokens,
    thinking: Option<ThinkingOpts> }` where `ThinkingOpts = { enabled, effort, budget }`.
  - `Chunk` events: `Delta(text)`, `ReasoningDelta(text)`, `ToolCallDelta`,
    `Completed { usage }`, `Error`, `KeepAlive`.
- **Dialect adapter trait** (new): `Dialect::render_request(&Request) -> wire bytes` and
  `Dialect::parse_stream(bytes) -> Stream<Item=Event>`.
  - Implement for: OpenAI chat (/chat/completions with `reasoning_content` & `thinking`
    integration), OpenAI-compatible (DeepSeek/NIM/Together/Zen variations = same dialect
    family, overridable baseURL/headers), Anthropic Messages (thinking blocks +
    signature), Gemini (thought parts/signature, thinkingConfig), Ollama (`thinking`
    field, SSE), OpenRouter (`reasoning` + `reasoning_details`), Bedrock/Vertex (later).
  - This *replaces* `openai.rs`'s custom builder+parser with reusable components but
    keeps the tested transport pieces (`sse.rs`, timeouts, retry) — a port, not a rewrite.
- **Reasoning echo policy** (the fix): at render time, per-message:
  `resolved_reasoning = captured_reasoning || prev.reasoning || ""` for models whose
  matrix `echo_reasoning: Must|Optional|Reject`. Emit `reasoning_content` on every
  assistant message for Must (DeepSeek tool turns); emit nothing for OpenAI-native;
  never emit for models that reject unknown fields (scope matrix). "Capture-on-stream,
  echo-verbatim" guarantees byte-fidelity; do nothing from prose.
- **Cost/tokens**: `usag`, metering per `tokenizer.rs` (unchanged idea), plus
  `usage.output_tokens_details.reasoning_tokens` parsing where available.

### 2.2 Context engine (`crates/context` new, or extend `context_compaction`)

Existing `context_compaction.rs` is *checkpoint-based*, `trigger_tokens=16_000`,
summarize middle — good bones. Upgrade:

- Token budgeter: given model context window, `headroom` for output, `buffer`
  (opencode: keep 8k tokens, buffer 20k), trigger when `estimate > context −
  max(output, buffer)`.
- `Trimmer`: keep system+stable-prefix (unchanged) + head (first N) + user-relevant
  tail; summarize middle into `<summary of prior conversation>` (LLM cost = 4k out).
  Append-only audit layer keeps the full original (like OpenHands condensation events,
  LangGraph "your own table vs checkpointer").
- Prompt-cache discipline: freeze the system/tools prefix; dynamic content only in
  message tail; mark `cacheControl` breakpoints at end of static prefix (Anthropic),
  provider-agnostic: keep prefix byte-identical across turns.
- `request_too_large` → preflight trim vs post-`ContextOverflow` compaction,
  matching opencode's `compactAfterOverflow` (only pre-assistant-start).

### 2.3 Tool layer

- **Canonical tool schema** (already schemars-based). Add a **coercion/repair** step
  at `Tool::execute` entry (like AI SDK `repairToolCall`): number-vs-string,
  string-array coerce (split whitespace/newline), `-1`→default? plus a warn-note
  returned to the model (not an error) when adopted, and preserve strict contract
  elsewhere. Initial targeted set: git tool args, filesystem read params.
- **Binary/read hygiene** (`virtual_fs.rs`): replace `read_to_string` hard-fail with
  "binary file (N bytes)" informative result (optionally hex/base64 snippet via config),
  no UTF-8 exception to the model.
- **Sandbox**: deny→ask→allow ordering (unmatched rules = config), bare-tool-removed-
  from-context, `TimeoutCap`, `MaxStepsCap`, doom-loop guard (same tool+args N times = 3).
- MCP: keep namespacing `mcp:<server>:<tool>`, collocation=check, policy-gating;
  align blob caps (opencode ~10 MB limit is a good default) & MIME allowlist.

### 4. Orchestration

- Already done this session: planning-phase ladder (design stage) inside
  `run_design_stage_with_recovery` (4f83c51). **Do not regress.**
- Extend ladder/termination to "model dead" fixes via the new runtime (ladder
  targets default model, not a scanning).
- **Plan-as-data**: keep plan + DesignDoc; render to Markdown; persist in
  `.concerto/plans/<slug>.md`; make it editable + observed (TDB list of TODO-like).
- **Subagent isolation**: ensure specialist runs use a **fresh `AgentContext` window**
  (already fresh per-subtask in coordinator) and only the *summary/result* returns to
  the coordinator message history (verify `previous_results` doesn't leak full
  transcripts — review artifact contracts).
- Loop guards: `max_steps`, `max_subtask_attempts`, doom-loop, per-role
  `CancellationToken` (already threaded).

### 5. Config surface — "freedom of config" as a feature

Today config is partial (RetryConfig, SkillsConfig, McpConfig, ProviderConfig,
MultiAgentConfig, ModelPinConfig, PolicyConfig… in `crates/config/src/schema.rs`).
Target config (`concerto.json`):

```jsonc
{
  "models": [ { "id": "opencode/big-pickle", "provider": "opencode", "api": "openai-compat",
                "context": 200000, "capabilities": {"reasoning": "always-echo", "tools": "function"},
                "price": {"in": 0.0, "out": 0.0} } ],
  "providers": [ { "id": "opencode", "type": "openai-compatible",
                   "base_url": "https://opencode.ai/zen/v1", "env_key": "OPENCODE_API_KEY",
                   "headers": {"Authorization": "Bearer ${OPENCODE_API_KEY}"} } ],
  "roles": [ {"role": "architect", "model": "nvidia/nemotron-ultra-550b", "tools": [...],
               "capabilities": {...}, "max_steps": 32} ],
  "fallback": { "default_model": "opencode/big-pickle", "tier1b": true, "tier2": true },
  "context": { "buffer_tokens": 20000, "keep_tokens": 8000, "compaction": "auto" },
  "tools": { "git": {"coerce": true}, "filesystem": {"binary": "note"} },
  "permissions": { "default": "ask", "allow": [...], "deny": [...] },
  "budget": { "max_cost_usd": 5.0 }
}
```

.Enforced via existing `figment` layering (defaults → config file → env) and
keyring. Migration from today's fields is mechanical (schema serde defaults).

### 6. Plugin/extension path (later)

- Native dialect adapters = compile-time. Plugin (WASM) host already exists for
  tool+provider+memory plugins (`crates/plugins`). Expose new dialect as a plugin
  hook (`stream_completion`), matching opencode's plugin provider seam. — Phase 4+.

---

## 7. Phased implementation plan (each phase: goal · scope · files · exit tests)

**Execution status:** Phase 0 ✅ `109e3b6` · Phase 1 ✅ `c43e6d8` · Phase 2 ✅
`b38d3ab`/`6429485`/`be43294` · Phase 3 ✅ `0f615c2`+`0e9deb5`/`35b02ed`/
`b9e670b` (ADR-48: ContextEngine, real token accounting, cache breakpoints) ·
Phase 4 ✅ `8436462` (health + shared tier-1 precedence; config-only endpoint
realized in P2 M3 + this docs section) · Phase 5 ✅ `84b1a1a` (ADR-52
orchestration safety gates: global dispatch cap, durable plan artifacts,
multi-failure exit gate; subagent-isolation audit found no gaps — see §7) —
**deferred feature config surface** (post-Phase-5, same style as the Phase-4
flattening): per-role fallback disable, per-tier retry/backoff counts,
ladder-locked tier targeting.

### Phase 0 — Reasoning capture repair (MVP, no new deps) — ~1-2 days
**Goal**: kill failure class 1 (the dominant producer of unusable runs) with the
proven minimum: preserve `reasoning_content` and echo it for OpenAI-compatible
providers.
- `core/types.rs`: `Message` gains `reasoning_content: Option<String>` (serde default
  keeps old rows), `CompletionChunk` gains `reasoning: Option<String>`.
- `providers/openai.rs`: `handle_event` accumulates `delta.reasoning_content` into
  chunk; `build_assistant_message` emits `reasoning_content` when
  `request.model` profile says echo (default: echo if present in history, per-model
  override in catalog).
- Thread through `prompts.rs` collectors: return `(String text, String reasoning,
  Vec<ToolCall>)`; `complete_provider_request` callers propagate; `agent_loop.rs`
  and `agents/generic.rs` attach to assistant Message.
- Sessions: `messages` table add `reasoning_content` column (migration), replay
  loads it; state/checkpoint serialize.
- Tests: unit (parse delta → chunk → message → re-render echo), integration
  (fake OpenAI stream with reasoning_content delta → second request contains
  `reasoning_content` field), persistence round-trip.
- Exit gate: `cargo test -p concerto-core -p concerto-providers -p
  concerto-orchestrator` green; clippy/fmt clean; the exact "400 reasoning_content"
  scenario unit-testable.

### Phase 1 — Tool resilience (fix classes 2&3) — ~1 day
- `tools/git.rs` (and `filesystem.rs`): coercion at execute(): string→number,
  string→array (with model-facing warn), strict stays strict.
- `tools/virtual_fs.rs`: `read` returns `Ok("binary file {size} bytes — not decoded")`
  for non-UTF8; optional `read_binary: "base64|hex|note"` config.
- Tests per tool; exit gate clippy + crate tests.

### Phase 2 — Dialect adapter split (the big one; STEADY, testable)
1. Introduce `crates/llm` (or `crates/providers/adapters`) with `Dialect` trait;
   port OpenAI-chat as first dialect reusing SseParser/timeouts/retry; concourse
   canonical IR types (`Message{parts}`, `Request`, `Chunk` events).
2. Rebase `orchestrator` + `prompts.rs` onto the new IR (keep flat text conv for
   skill-injection paths until context engine). This desugars failure class 1 into
   a pure adapter concern.
3. Add Anthropic + Gemini + Ollama dialects (parity with current printers).
4. Move `provider_defs.rs` budgets → catalog, wire catalog merge from config
   (user-overridable).
- Exit gate: all existing provider tests (message builder snapshots, tool-call
  round-trips) pass without behavioral regress; new adapters have streamed
  golden tests.

### Phase 3 — Context engine & compaction v2
- Reuse `context_compaction.rs` bones: budgeter, token estimate, run trimmer
  (head + summary + tail), audit table, cache-breakpoint responses.
- Config knobs exposed; default = current behavior.
- Exit gate: simulated long-run test asserts bounded input tokens + preserved recall.

### Phase 4 — Config-first wire-up
- New `concerto.json` schema + migrations; `ProviderConfig`+`ModelPinConfig`+
  `MultiAgentConfig` flatten into catalog; credentials via keyring/env; `concerto
  health` shows resolved model/provider.
- Exit gate: a user (README-documented) can point to a *brand-new* open-OpenAI-compatible
  endpoint via config only.

### Phase 5 — Orchestration polish & subagent isolation review
- ✅ **Subagent window / isolation audit** — no gaps: specialists are isolated
  by capability-gated executor policy (`AgentCapabilities::fs_write`), a session-scoped `VirtualFs`, and a
  fresh `AgentContext` per task (only the summary returns to the coordinator);
  per-attempt caps already bound retry/fallback work.
- ✅ **Planner-as-data (ADR-52)** — `TaskPlanner::plan` returns a
  `PlanOutcome` carrying a serializable `PlanArtifact` (plan id, task text,
  per-subtask role / dependencies / expected artifacts), persisted
  pretty-printed to `<app_data_dir>/plans/plan-<plan_id>.json` via
  `concerto_sessions::plans::PlansManager`; `plan_id` is carried on
  `MultiAgentModeStarted` (additive `Option<String>`); restore rewrites the
  artifact from the resumed graph (idempotent).
- ✅ **Global step cap / doom guard** — `MultiAgentConfig.max_total_iterations`
  (`Option<usize>`, default unlimited; `Some(0)` = off) caps model
  dispatches (ready batches + ladder tiers) at the `execute_graph` batch
  boundary; exhaustion exits through the ladder's `Partial` machinery.
  Planner/design and review/validation loops are not counted.
- ✅ **Exit gate e2e** — coordinator test
  `multi_failure_exit_gate_run_still_completes`: 5 roles, 2 fail first
  (hard `LimitReached` + budget), both rescued by the tier-1 ladder, run
  exits `Completed`. The "2 of N models intentionally failing, output still
  completes" gate is institutionalized at the unit level (no chaos harness).
- **Deferred feature config surface** (post-Phase-5, see the audit note in the
  execution status; same one-line style as the Phase-4 flattening deferral):
  - per-role fallback disable (a per-role "no-fallback" switch beyond the
    global `default_model_fallback`);
  - per-tier retry/backoff counts (per-tier attempt/backoff tuning beyond the
    shared `RetryConfig`);
  - ladder-locked tier targeting (pinning a ladder tier to a specific
    provider/model class rather than the run-wide default).
- Exit gate (as planned): e2e multi-failure run completing — realized at unit
  level by the `multi_failure_exit_gate_run_still_completes` test above.

### Phase 6 (later) — plugins for novel dialects, heartbeats, merge leftovers.

---

## 8. Decisions to ratify (write ADRs before code)

Decision 0 (this doc) supersedes ad-hoc patching deposits. Status:
- ✅ ADR-46: Reasoning-as-data + capability-echo matrix (files: core types,
  openai adapter, catalog).
- ⏳ ADR-47: Canonical Message parts (replaces flat string) — deferred: parts
  remained flat in Phase 2; revisit with the parts/caching work
  (docs/adrs/ADR-47-message-parts.md).
- ✅ ADR-48: ContextEngine v2 (compaction/trimmer/prefix discipline) — this change.
- ✅ ADR-49: Config-first catalog (models/providers as data) — landed in Phase 2
  M3: extra models + reasoning echo; `concerto health` surfaces the resolved
  catalog. Full flattening of `ProviderConfig`/`ModelPinConfig`/
  `MultiAgentConfig` into one catalog schema remains deferred
  (docs/adrs/ADR-49-config-first-catalog.md).
- ✅ ADR-50: Tool coercion + binary read contract
  (docs/adrs/ADR-50-tool-coercion-and-binary-read-contract.md).
- ✅ ADR-52: Orchestration safety gates (global dispatch cap, durable plan
  artifacts, multi-failure exit gate) — Phase 5 M1, landed in `84b1a1a`. The
  §7 subagent-isolation audit found no gaps beyond those three items. Deferred
  feature-config surface (`per-role fallback disable`, `per-tier
  retry/backoff counts`, `ladder-locked tier targeting`) is recorded in §7,
  not ratified here.

Old ADRs 42/45 (fallback ladder) remain valid; this work composes to them.

---

## 9. Research provenance (concise)

Primary sources consulted: DeepSeek Thinking Mode guide (api-docs.deepseek.com),
Anthropic extended-thinking + tool workflows + errors, Gemini thinking &
thought-signature docs, OpenRouter reasoning-tokens + reasoning_details semantics,
Ollama thinking docs + issue #8529, NVIDIA NIM reasoning-model docs, Vercel AI SDK
reasoning + provider docs, openai-python shared/reasoning params,
missive+openai/codex config reference, ag-sst/opencode source (`catalog.ts`,
`openai-chat.ts`, `transform.ts` lineage PRs #24146/#24250/#24443/#24442,
compaction.ts, session/runner/*), OpenHands condenser docs + PRs, OpenClaw
compaction docs, LangGraph checkpointer/summarization docs, Claude Code
context-window & permissions & skills docs, Codex AGENTS.md doc (+ `project_doc_max_bytes`
32KiB), Agent Skills spec, Aider repomap, AutoGen docs. See companion reports for
URLs; this doc cites the distilled conclusions.

## 10. Risks & open items

- **Ph2 adapter split risk**: regressions on Claude/Gemini/Ollama printers — mitigated
  with golden stream tests before removal of the old builders (keep both, `#[cfg(not)]`
  rollback).
- **Reasoning echo instability (DeepSeek)**: current API is lenient according to
  some July 2026 tests but docs still mandate. Safe posture: always echo verbatim
  (or `""`); no user cost.
- **Message/Parts rollout touches persistence format** — keep `serde(default)` +
  legacy column fallback so old DBs replay.
- **Anthropic budget_tokens deprecation** — new models need `thinking.adaptive`
  shape; catalog carries per-model thinking knob rather than one Global.
- **NIM/OpenCode gateway echo requirements remain UNVERIFIED from primary sources**;
  design NIM as `echo: Optional` (livens on echo-verbatim if the model emits it).

---

## 10.5 Transport resilience (complements this doc — companion research)

Companion: `docs/research/multi-provider-resilience.md` (user-authored survey of
LangChain/AI SDK/LiteLLM/Portkey + production guides, Aug 2026). Audited against
current code; it does **not** duplicate my design — it covers the *transport*
half (what happens when the wire breaks) while this doc covers the *fidelity*
half (what bytes must survive). Findings of the audit:

### Already solid in Concerto (verified anchors)
| Pattern | Where |
|---|---|
| Classified retry decisions (429/5xx yes; 4xx no) | `crates/core/src/retry.rs:50 classify_provider_error`; `error.rs:250` |
| Exp backoff, cap, jitter (full-jitter `[0,delay]`) | `providers/retry.rs:239-298`; config `RetryConfig` (initial/multiplier/max_delay/jitter) |
| `Retry-After` / `retry-after-ms` respected | `providers/retry.rs:128 parse_retry_after`; `core retry.rs` |
| Stream heartbeats & idle re-set | SSE keepalive chunks (`openai.rs:231`), `Retry::stream_idle_timeout_seconds` |
| Timeouts cap, attempt cap, elapsed fuse | `RetryConfig{ max_attempts, max_elapsed_seconds, ttfb, idle }` |
| Fallback chain | `providers/routing.rs:258` + coordinator tiers (2.4/2.5 in this doc) |
| Loop guards & step caps | `orchestrator/agent_loop.rs:459 max_iterations`; `cycle.rs`; `cycle_manager` |
| Tool errors as structured context | `tools/*` return values; `orchestrator/agents/generic.rs:462` |
| Context-overflow → degrade/retry | `context_guard.rs`, `ContextOverflow` error |
| Idempotency keys on requests | `providers/retry.rs` & OpenAI SDK headers where applicable |

### Gaps to close (new items — fold into phases below / future)
1. **Circuit breaker** — per provider+model key, CLOSED→OPEN→HALF_OPEN,
   half-open probe semaphore, open-route fail-fast. NOT present today.
2. **In-flight dedup (idempotency)** — same (provider,model,prompt-hash) in parallel
   → single flight. Add to `providers/retry.rs` or new connector.
3. **Content-policy / token burn watch** — budget kill-switch per run (`cost.rs` +
   `TokenBudget` exist; add run-level cap config).
4. **Gateway benchmark pruning** — keep native NIM/Ollama direct-connection
   default; the config-first catalog's "provider = openAI-compatible `base_url` +
   env-key" already covers LiteLLM-style gateways without a new runtime. Do not
   build a proxy layer unless benchmarks demand it.

## 11. Immediate next steps (after your sign-off)

1. Ratify decision + ADR; 2. dispatch Phase 0 (reasoning capture) to coder now
   (it can land this week and unblock real-world usage); 3. Phase 1 (tool
   coercion/binary) next; 4. Phase 2 (dialect split) as the sustained effort;
   5. circuit breaker + budget kill-switch land with Phase 2/3 (transport gaps).