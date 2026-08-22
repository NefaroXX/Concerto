# Orchestration Blueprint: From the five-stage hardcode to a fully configurable pipeline

**Date:** 2026-08-13
**Status:** Research / input — not an ADR; ADR-58+ will follow once direction is approved.
**Scope:** `docs/research/orchestration-blueprint.md` only. No source, `Cargo.toml`, or ADR changes were made. Every `file:line` anchor below is **at time of writing**; implementation will move them.

**Abstract.** Concerto's agents pipeline is effectively hardcoded: a five-stage vocabulary (`design`/`research`/`implement`/`review`/`validate`) whose stage set, stage semantics, pipeline order, relationship taxonomy, observability feed, and identity prompts are all baked into `concerto-core`/`concerto-orchestrator`/`concerto-config` source rather than derived from configuration. This document audits that hardcode density, synthesizes prior art from eight agent/workflow frameworks and six studio products, and proposes the **Orchestration Blueprint**: a config-as-data pipeline where a typed stage registry (six closed engine archetypes plus open custom registration), an authority model that grants file-write capability strictly from stage kind (narrowing-only overrides), a data-defined pipeline with a load-time validation rulebook, and data-driven relationship/observability/persona tables replace every hardcoded seam discovered in §3. Oracle review is folded into §5; §6 lists the open decisions the user must confirm before a superseding ADR is drafted.

---

## 2. Context & motivation

The original product vision was an "intelligent orchestrator" with a "fully configurable orchestration studio" (tool-calling is policy-gated; agents themselves are the product surface). `docs/ARCHITECTURE-V2.md` states the governing principle: **"Everything is config data"** — providers, models, roles, tools, permissions, budgets. `docs/STATUS.md` separately calls out "configurable directed relationships and cycle limits; per-role provider/model assignments." ADR-49 already ships the everything-is-data precedent (providers as data feed the model catalog); ADR-35 replaced hardcoded role IDs with *stage tags*, but only as a vocabulary — the semantics behind those tags stayed hardcoded.

The five-stage model was a good first attempt. It shipped, however, as **hardcoded semantics with an inferior-era implementation**: exactly five strings have engine meaning, every other string silently degrades to "freeform run-once," and the coordinator branches on those strings in ~20 places. The drift is visible in the inverted authority layer: capability flags and stage tags are configured side by side, but the engine decides writing rights by role-tag identity (see §3), so config is ceremony around a hardcoded machine.

Users today edit configs directly; the CLI has no orchestration-editing surface (CLI QoL is out of scope here, deferred). In practice the **config file is the source of truth and the Studio — when it exists at full fidelity — is its editor**. This blueprint makes the config file able to *express* the pipeline, then makes the Iced Studio (desktop) the editor for it.

## 3. Audit: the hardcode map

All anchors are at time of writing.

- **Stage set** — `AgentStage` exposes exactly five consts and a closed `is_known()`: `crates/core/src/types.rs:763–808`. An unknown tag is not an error; it is folded into the freeform bucket: `crates/orchestrator/src/planner.rs:346–358`. Adding a stage means editing core types; there is no registry.
- **Stage semantics** — the planner partitions agents by `is_research` / `is_implement` / `(design|review|validate)` / custom (`planner.rs:327–362`), renders roles with hardcoded labels "Researcher"/"Coder"/"custom agent" (`planner.rs:222–227`), and decides plan-artifact ownership purely on `is_implement` (`planner.rs:509–548`). The coordinator carries ~20 `is_*`/`stage_of` dispatch sites: `coordinator.rs:2555`, `2715–2724`, `2793–2796`, `2880–2911`, `3279`, `3817–3945`. Hardcoded self-execution and self-verification personas with a "coordinator" identity: `coordinator.rs:692–717` (`self_implement_agent`), `736–749` (`self_verify` availability), `3833`, `3900`, `4419–4457`. The `"coordinator-self-execute"` provider sentinel is written and asserted in multiple spots: `coordinator.rs:843`, `1564`, `5589` (and more in tests).
- **Pipeline order** — `run()`/`execute_graph()` control flow is source-ordered: `coordinator.rs:3368–4641`, with literal event-key strings constraining what a stage may emit (`crates/core/src/event.rs:315–350`: `agent_handoff`, `ReviewCycleStarted/Completed/Escalated`, `ValidationCycleStarted`, `ValidationEscalated`, `RoutingDecided`). Order, cycle caps, and terminal conditions are not data.
- **Relationships** — a **closed 4-variant enum** `AgentRelationship { Supervises, ProvidesContextTo, ReportsTo, OwnsDesign }`: `crates/orchestrator/src/relationship.rs:15–24`. A closed string match maps config strings to variants (`runtime_runner.rs:86–94`): anything else becomes `None` — silent. The built-in defaults are a hardcoded table (`relationship.rs:139–171`), and the config crate keeps a *second* source of truth for the same shape (`crates/config/src/schema.rs:1425–1431`).
- **Observability feed** — stage→`RunStage` mapping for the status/progress feed is hardcoded: `crates/orchestrator/src/runtime_runner.rs:3262–3281`.
- **Identities/prompts as data that is still hardcoded** — `builtin_agent_seeds()` embeds `system_instructions` verbatim (`crates/config/src/schema.rs:1506–1622`); the Studio mirrors them, including a coordinator prompt naming the five specialists by hand (`crates/desktop/src/views/orchestration_studio.rs:466–585`); the Studio's 5-edge preset is literal (`587–626`); the stage picker only lists the five known stages (`1994–2001`); and the relationship "kind" input is **free-text** (`1713–1764`) — the Studio must already fight the closed string match.
- **Latent fork list** (strings that a configurable catalog must key on *flags* later, not on tags): `state.rs:128` (Rule-B review cycle detection keyed on `is_review`); `coordinator.rs:380–383` (DecisionCategory mapping hardcoded design/validate vs implement|review); `cost.rs:13–22` (token table by literal role ID); `cli/src/health.rs:32` (`DEFAULT_TOOL_CALLING_ROLES = ["researcher","coder","validator"]`); `registry.rs:142` (validator special-casing); the eval harness's `is_validate` keys; and the closed `EventKind` enum itself.
- **Pinned tests** (~30 sites) serialize today's semantics and will need deliberate, staged migration: `registry.rs:836–1189`, `planner.rs:1030/1038/4923`, `coordinator.rs:5446–5717`, `schema.rs:1752–1840`, plus eval-harness tests.

**Density verdict.** `coordinator.rs` is by far the most coupled module (stage dispatch, relationship resolution, personality fallbacks, and observability all meet there). The **single unblocking change** is a config-driven pipeline registry — a `stage_order` + per-stage `stage_lifecycle` table — because every other hardcode (relationships, feeds, personas) either keys off the stage set or shares the coordinator's control flow.

**Seam summary** (the seven hardcode seams and their blueprint owner):

| # | Seam | Anchor (at time of writing) | Today | Blueprint owner |
|---|---|---|---|---|
| 1 | Stage set | `core/src/types.rs:763–808` | 5 consts + closed `is_known`; unknown ⇒ freeform | §5.1 registry (6 closed kinds, open custom) |
| 2 | Stage semantics | `planner.rs:327–362` / `coordinator.rs` ~20 sites | `is_*` dispatch | §5.1 kind flags, §5.3 pipeline [P2+P3] |
| 3 | Pipeline order | `coordinator.rs:3368–4641` + `event.rs:315–350` | source-ordered control flow | §5.3 `stage_order` + lifecycle table |
| 4 | Relationships | `relationship.rs:15–24, 139–171`; `runtime_runner.rs:86–94`; `schema.rs:1425–1431` | closed enum + silent `None` | §5.4 open registry, closed semantics, one schema |
| 5 | Observability feed | `runtime_runner.rs:3262–3281` | hardcoded stage→`RunStage` | §5.6 feed binding as data |
| 6 | Identities/prompts | `schema.rs:1506–1622`; `orchestration_studio.rs:466–585, 587–626, 1994–2001, 1713–1764` | verbatim system prompts; free-text kinds | §5.5 fallbacks as config, §5.11 typed pickers |
| 7 | Latent forks | `state.rs:128`; `coordinator.rs:380–383`; `cost.rs:13–22`; `cli/health.rs:32`; `registry.rs:142`; eval keys | tag-keyed decisions | key on catalog flags / closed feeds, never tags |

## 4. Prior art synthesis

Condensed from research; source URLs in §7. One paragraph per card, then the pattern matrix and the pitfall checklist.

- **LangGraph** (MIT) — graphs are built *as code*, while state, checkpoints, and config are *data*; the "blueprint"/agent definition is explicitly not a serialized artifact. Authoritative for "graph is a capability, not a config file" — industry velocity products (e.g., for LangChain-based crews) have to *generate* the graph code from config, a tell that code-as-graph does not scale to user-editable pipelines.
- **CrewAI** — agents and tasks are declared in YAML and loaded at runtime; it explicitly *rejects forward references* (an agent may only reference tasks already defined) — an anti-pattern for flexible graphs — and its `Process` enum is **closed** (`sequential`/`hierarchical`), the same closed-enum trap Concerto's stages are in.
- **AutoGen (Microsoft, CC-BY/ MIT core)** — components are data **with per-component `version` / `component_version` fields**, and code escape hatches exist for anything data cannot express. The lesson: version artifacts, keep a data-first default, accept escape hatches rather than smuggling logic into config.
- **OpenAI Agents SDK** — agents are data; *tool-level guardrails are decoupled from topology* (guardrails attach to the agent/tool, not to the graph), which validates Concerto's "policy engine as runtime enforcer" rather than baked-into-stage behavior; handoffs are the primitive.
- **Google ADK** (Apache-2.0) — aggressively config-first, then **retreated from config-first control flow** to imperative app classes for `topology`/`model`/`agent` when configuration became a bottleneck. This is the strongest warning sign in the survey: config-first failed *there* because their configs carried control flow. Concerto must keep config free of control flow (§5.3, §5.9).
- **Haystack (Apache-2.0)** — a **serializable DAG** deserialized at load time with strict validation: deserialization **allowlist**, load-time schema checks, and `max_runs_per_component` (a per-node loop cap). Direct template for §5.3's bounded-loop rulebook and §5.9's allowlist.
- **Semantic Kernel Process framework** (experimental, MIT) — event-driven **steps** with step-state **`.V1` versioning** and explicit "experimental" labeling; step-state snapshots make processes resumable. Supports versioning the *state machine*, not just the schema (§5.10).
- **Claude Code subagents** — freeform markdown roles with no stage binding; routing relies on description matching and is known to **drift** (a subagent stops being selected). Lesson inverted: Concerto's *explicit stage binding* is the fix — routing by declared stage, never by description.
- **Workflow engines Prefect / Temporal** — config references code **by hash/commit**; deployments are **versioned and pinned**, rollback = re-point the reference. The pattern that makes "yesterday's blueprint still runs" a mechanical fact. Relevant to Concerto's worker-side code that config only references (§5.9, §5.10).
- **Studios n8n / LangFlow / Dify / PromptFlow / Copilot Studio** — canvas-based editors have **converged** on the same UX (node palette, drag edges, run-one-node, live panel), which is comforting but not differentiating. Notable divergences: PromptFlow keeps **dual text/visual single-source** (YAML is the truth, the canvas renders it — validates Concerto's "config is truth, Studio edits it"); Dify supports run-one-node simulation (§5.12 P6); n8n ships **templates** as the onboarding primitive (→ blueprints). Licenses matter for code we may mirror: n8n **fair-code** (source-available), LangFlow **MIT** (restrictions on the brand), Flowise **Apache-2.0**, Dify **Apache-2.0**, PromptFlow **MIT**.

### 4.1 Pattern matrix (fit: 1–5, Concerto-specific)

| Pattern | Fit | Evidence / notes |
|---|---|---|
| Typed stage registry + lifecycle flags | **5** | CrewAI rejects it, ADK retreated from it, LangGraph refuses to serialize it — yet all three ship *some* fixed pipeline. Concerto's five tags are the smallest useful registry; the fix is open registration, closed semantics (§5.1). |
| Role-as-data with capabilities | **5** | ADR-35 + ADR-49 already steer here; OpenAI SDK's per-agent tool guardrails show the runtime must remain the enforcer (§5.2). |
| Event → stage binding | **4** | Semantic Kernel's event-driven steps are exactly this; our closed `EventKind` pins it (§5.6). |
| Relationship taxonomy as data | **4** | **None of the surveyed frameworks fill this with data.** n8n/Dify edges are untyped or type-lite; Concerto's 4-kind typed edges + defaults is a differentiator if the *kinds* become open but their engine semantics stay closed (§5.4). |
| Templates / presets | **5** | n8n onboarding templates; blueprints in §5.8. |
| Schema versioning | **5** | AutoGen `component_version`; SK `.V1` process step-state; Prefect/Temporal pinned deployments (§5.10). |
| Validation-at-load | **5** | Haystack allowlist + load-time checks; Concerto's load-time rulebook (§5.3). |
| Runtime guardrails | **5** | OpenAI tool guardrails decoupled from topology = policy engine stays the enforcer (§5.2, §5.9). |
| Freeform node-graph config | **3–4** | Haystack proves DAG-as-config works with the right validation; **deferred** to P6 (§5.12). |
| Visual canvas editor | **3** | Studio products converge here; not a differentiator, and editing raw graphs is the highest-churn UX. **Deferred** (§5.11 rail-first). |

### 4.2 Pitfalls checklist (10)

1. **YAML/TOML hell** — strings-typed configs drift from the schema; prefer typed tables + `deny_unknown_fields`.
2. **Invalid graphs** — cycle/order errors at load, not mid-run (§5.3 rulebook).
3. **Control flow smuggled into config** — ADK's retreat; conditions stay a closed predicate catalog, never scripts (§5.3, §5.9).
4. **Serializer trap** — "blueprint as artifact" fails when the artifact can't round-trip; keep the registry authoritative (§5.10 export-with-merge).
5. **Unbounded runs** — per-stage `max_cycles` + engine cap, or loops escape (§5.3 rulebook e/f, ADR-52).
6. **Capability drift** — write-grant and stage tag must share one source; otherwise §5.2's authority model rots.
7. **Unsafe config deserialization** — load untrusted config must go through an allowlist (Haystack) — non-overridable (§5.9).
8. **Description-drift routing** — Claude Code subagents; Concerto routes by declared stage, never description (§4, §5.5).
9. **Schema explosion** — one field per conceivable option; blueprints compress it (§5.8, §5.11).
10. **False determinism** — "default behavior preserved" must be proven by tests, not asserted (§5.10 ~30 pinned tests).

## 5. Proposed design: the Orchestration Blueprint

The agreed proposal, with oracle review folded in. Where a decision came from the oracle review, the change is labeled. Three load-bearing principles run through it: **config expresses, engine decides** (open registration for what users vary, closed catalogs for everything with safety consequences — §5.1, §5.9); **one authority per question** (exactly one place answers "may this actor write here" — §5.2); and **legacy equivalence by test, not assertion** (the default blueprint reproduces today's behavior until migration is deliberate — §5.10).

### 5.1 Stage registry — open registration, closed semantics

A stage block is configuration data: `{tag, label, kind, version, flags}` where `kind` is `builtin | custom` and `flags` are drawn from a **fixed engine-capability catalog** (not freeform). The catalog is the six closed engine archetypes derived from today's five, **plus a sixth closed kind per oracle review**:

| Kind | Semantics (closed) |
|---|---|
| `Research` | Context gathering; no writes. (Today: `research`.) |
| `Planning` | Plan/design-doc production only. (Today: `design`.) |
| `Execution` | **Owns `plan.files`**; write-granted. (Today: `implement`.) |
| `Review` | Iterative gate with a cycle cap. (Today: `review`.) |
| `Acceptance` | Final gate; if unstaffed, falls back to a persona. (Today: `validate`.) |
| `RunOnce/Freeform` | **New explicit kind per oracle**: today's unknown-tag no-lifecycle behavior becomes a first-class kind. |

Custom stages are **flag composites** drawn from this catalog (e.g., "documentation" = `Planning` + no-write). Engine behaviors that carry safety consequences — file writes, shell, gates, cycle caps — are reachable only through the catalog. **Unknown tags become hard load errors**, not silent freeform — the current `planner.rs:346–358` behavior survives only as the explicit `RunOnce` kind.

### 5.2 Authority model — the load-bearing fix (oracle Q1)

Today an agent's write right is effectively decided by role-tag identity inside the coordinator plus per-agent capability flags that are never reconciled with the stage. The blueprint replaces this with a single, layered authority:

1. **Stage kind → default capability mask** (closed catalog): `Research`/`Review`/`Acceptance`/`RunOnce` default to **no** `fs_write`/`shell`; `Execution` defaults to `fs_write` + `shell`.
2. **Agent capability config = a narrowing override only.** Widening beyond the assigned stage-kind mask is a **load error** — unless the agent is also staffed in an `Execution`-kind stage (explicit multi-role case).
3. **Policy engine = runtime enforcer** on the *resolved per-agent mask*; there is no other path to a write.
4. **Planner = routing/artifact assignment only** — it assigns roles and ownership (`plan.files`), it never grants capability.

This **deletes the legacy tool-calling write route** and **dissolves the ADR-35 all-false-capabilities contradiction**: the built-in seeds' all-false capability blocks stop being a lie about what a non-`Execution` staffed agent may do — they become the correct, stage-derived defaults. There is exactly **one effective authority** answering "may this agent write files in this stage": the resolved mask, enforced by the policy engine.

### 5.3 Pipeline definition

The pipeline is data: an **ordered stage list** plus per-stage `{condition, max_cycles, feed}` where `condition` is a **predicate-name from the closed catalog**, `max_cycles` bounds that stage's loop, and `feed` binds the stage to an observability feed (see §5.6). Constraints:

- **Binary single-primary-`Execution` rule for v1**: exactly one primary `Execution`-kind stage. Multi-executor flows need artifact partitioning — that is new engine semantics and is **rejected in v1** (§6, decision 5).
- **≥ 1 terminal kind** (`Acceptance` or `RunOnce`) must be reachable.
- **No control flow in config** — no conditionals over arbitrary expressions, no scripts; predicates come from the closed catalog.
- **Load-time validation rulebook (oracle, a–j):**
  - (a) exactly one primary `Execution`-kind stage;
  - (b) ≥ 1 terminal kind reachable;
  - (c) an unstaffed non-`Acceptance` gate without a fallback persona → reject;
  - (d) a stage's fallback id must differ from any agent staffed in that same stage (no self-fallback);
  - (e) `max_cycles = 0` → reject;
  - (f) the sum of stage caps is bounded under the engine global maximum (ADR-52 run cap);
  - (g) unknown stage tag in the registry → hard load error;
  - (h) `feed` label must be a member of the **closed feed catalog** (§5.6);
  - (i) the stage `condition` predicate must be evaluable from the stage's semantics flags (no arbitrary code);
  - (j) reserved-name collisions are rejected (e.g., a custom stage tagged `coordinator` colliding with the sentinel provider identity).

### 5.4 Relationship kinds

- The registry of kinds is **open** (config adds a kind name), but each registered kind **references closed engine semantics**: `approval-gate`, `context-flow`, `delegation` — the current `Supervises`/`ProvidesContextTo`/`ReportsTo`/`OwnsDesign` become data rows over those semantics.
- The defaults table (`relationship.rs:139–171`) becomes configuration data, loaded into the same registry.
- `schema.rs:1425–1431` (the config-crate mirror of the shape) is updated **in lockstep** with the runtime, so the two sources of truth become one.
- Keep the `#[non_exhaustive] AgentRelationship` enum as the stable API surface: the registry maps strings → flags **without extending the enum**. The free-text Studio input (`orchestration_studio.rs:1713–1764`) is replaced by a typed picker over the open registry (§5.11).

### 5.5 Fallback agents as config (oracle Q2)

Per-stage default persona used when the stage is unstaffed (replacing `self_implement_agent`/`self_verify` and the "coordinator" identity at `coordinator.rs:692–717, 736–749, 3833, 3900, 4419–4457`):

- Fallback records are config **with explicit capability masks defaulting to the stage-kind mask — no widening** (mirrors §5.2).
- The sentinel tag (`coordinator-self-execute`) is validated/reserved, not free-text (rulebook (j)).
- Fallback prompts render **only when the stage is actually unstaffed** — never pre-injected into staffed runs.
- Stage labels/descriptions render through the **fixed role template** (no free inline prompts per stage beyond the template). *Prompt-injection surface noted:* per-stage prompts multiply the injection surface; mitigated by (a) config being local, (b) the consistency policy that prompts never replace the tool gate, and (c) the fixed template.

### 5.6 Observability binding

- The stage→`RunStage` feed map (`runtime_runner.rs:3262–3281`) becomes **data** bound per stage.
- `EventKind` stays closed; custom stages emit a **generic `StageStarted` event or map to nearest-kind** (`RunStage`). No new literal event keys per stage; the audits/replay feed bind through the same table.
- Sessions audit/replay consumes the same feed binding, so replay output matches what the UI showed.

### 5.7 Artifact contract (oracle Q6b)

`Execution`-kind **custom** stages need an explicit `files` / plan-delta field on the stage block so plan semantics do not fork: ownership, `expected_artifacts`, and the planner's `is_implement` artifact logic (`planner.rs:509–548`) all key off this field for any custom `Execution` stage, never off the tag string.

### 5.8 Named blueprints vs single pipeline (open decision)

Preset/variant pipelines (`standard`, `tdd`, `docs-only`, `research-only`) are modeled as **named blueprints** — a pipeline + stage set + relationship defaults bound to a name — rather than one global pipeline with many flags. This is the schema-explosion guard (§4.2). **Open decision 3** (§6) asks whether to commit to blueprints now or keep a single pipeline and add named presets later.

### 5.9 Non-overridables (engine-owned, never config)

The following are engine invariants and are deliberately **not** in the blueprint config:

- policy/approval engine **and** preset definitions (selection is wireable; definitions are not);
- eval engine and its keys (incl. `is_validate`-style harness keys, §3 latent forks);
- `VirtualFs` sandbox, shell/git gating (ADR-44 project-root consent stays engine-owned);
- `CancellationToken` threading;
- tool security validation and the **deserialization allowlist and its resolution**;
- termination/loop-cap enforcement (θ caps, ADR-52 global run cap);
- cycle-detection keying (`role, hash`);
- the `ready_tasks` dispatch unit (only `ready_tasks()` enters a batch — `TaskGraph` contract);
- the sentinel provider mechanism (`coordinator-self-execute`, `coordinator.rs:843/1564/5589` semantics, kept as an engine label).

### 5.10 Migration & versioning

- `schema_version` on the orchestration section + **per-stage `version`** (AutoGen `component_version` / SK `.V1` pattern) so a blueprint pins the semantics it was written against.
- `serde` `deny_unknown_fields` on the orchestration section — typo-proof from day one (pitfall 1/7).
- The **default blueprint reproduces current behavior byte-for-byte**, including the `RunOnce` kind for today's unknown tags.
- The **~30 pinned tests pass initially and migrate later** (registry.rs:836–1189, planner.rs:1030/1038/4923, coordinator.rs:5446–5717, schema.rs:1752–1840, eval) — no silent behavior change.
- Config and Studio **SQLite undo-snapshots are versioned**; a blueprint change is a snapshot diff, giving rollback (Prefect/Temporal pinning pattern).
- **Studio apply and the ADR-57 watcher reload MUST share ONE apply path.** ADR-57's watcher (`0f01fa7`) and CLI per-run reload (`99b6a89`) landed 2026-08-13; a second, divergent apply path is the top migration risk. One `apply_blueprint()` entry point for Studio and watcher.
- **Export-to-config merges, never rewrites.** TOML round-trips lose comments; prefer the blueprint living in **its own include file** (TOML `include` / merged at load) so Studio writes a clean, mergeable section rather than the user's whole config file (open decision 4).

### 5.11 Studio UX (Iced) — three tiers

- **Simple** — template/blueprint picker (standard / tdd / docs-only / research-only); per-stage on/off; agent↔stage-tag mapping. The onboarding tier, n8n-templates-style (§4).
- **Advanced** — stage cards on a **timeline rail (rail-first; canvas deferred)**; lifecycle-flag toggles for the fixed catalog flags; **typed relationship picker** over the open registry (replacing free-text, `orchestration_studio.rs:1713–1764`); a **live validation panel** streaming the §5.3 rulebook; per-agent prompt/capability/model editor; a preset gallery; **export-with-merge + diff view**.
- **Graph** — read-only DAG map (drawing the §5.3 ordered list + edges); canvas editing later. There is already an agent-graph view in desktop, so the read-only tier reuses it.
- Undo is the **SQLite snapshot** layer (§5.10), not widget state.

### 5.12 Phasing

| Phase | Work |
|---|---|
| **P1** | Blueprint data model + load-time validation + **legacy equivalence** (all tests green on the default blueprint). |
| **P2 + P3 (merged)** | **Table-driven coordinator rewrite**: stage and relationship dispatch in one pass (replacing `coordinator.rs:3817–3945, 4419–4457`), configurable fallback personas, eval-harness migration. |
| **P4** | Studio Advanced tier (rail, typed pickers, validation panel, export-with-merge). |
| **P5** | Studio Simple tier + migration runner + export-merge hardening. |
| **P6 (deferred)** | Graph/DAG config support + run-one-stage simulation (Dify-style). |

**Rationale for the P2+P3 merge:** relationship semantics live in the same control flow as stage dispatch (`coordinator.rs:3817–3945`, `4419–4457`) and resolve through the same string match (`runtime_runner.rs:86–94`). Splitting them into two phases means touching the coordinator twice around the same seams — one pass is cheaper and less risky.

## 6. Open decisions for the user (confirm before ADR-58)

Numbered; each must be explicitly confirmed before a superseding ADR is drafted. ADR-57 is the newest ADR (accepted 2026-08-13); the next ADR number is 58.

1. **Authority model adoption** — stage-kind → capability masks with narrowing-only overrides, **including deleting the legacy tool-calling write route**. Behavior change: built-in seeds' writes are re-granted by the `Execution` stage mask, not by their config flags.
2. **Six-kind closed catalog** — including the new `RunOnce/Freeform` kind, and **hard errors on unknown stage tags** (replacing silent freeform).
3. **Named blueprints vs single pipeline** — commit to blueprints in P1, or ship a single pipeline and add named presets later (§5.8)?
4. **Blueprint in its own include-file** with export-with-merge (Studio writes a clean section; user config comments preserved), or managed only as an in-place section?
5. **Single primary `Execution` rule in v1** — exactly one primary `Execution` stage; multi-executor artifact partitioning deferred (§5.3, §5.7).
6. **Phasing commitment P1 → P5** with **P2+P3 merged** into a single table-driven coordinator rewrite (§5.12); P6 deferred.

## 7. Sources

Framework / engine docs:

- LangGraph — https://langchain-ai.github.io/langgraph/
- CrewAI — https://docs.crewai.com/
- AutoGen — https://microsoft.github.io/autogen/
- OpenAI Agents SDK — https://openai.github.io/openai-agents-python/
- Google ADK — https://google.github.io/adk-docs/
- Haystack — https://docs.haystack.deepset.ai/
- Semantic Kernel Process framework — https://learn.microsoft.com/en-us/semantic-kernel/frameworks/agent/processes/
- Claude Code subagents — https://docs.anthropic.com/en/docs/claude-code/sub-agents
- Prefect — https://docs.prefect.io/
- Temporal — https://docs.temporal.io/

Studio / editors:

- n8n — https://docs.n8n.io/
- LangFlow — https://docs.langflow.org/
- Flowise — https://docs.flowiseai.com/
- Dify — https://docs.dify.ai/
- PromptFlow — https://microsoft.github.io/promptflow/
- Microsoft Copilot Studio — https://learn.microsoft.com/en-us/microsoft-copilot-studio/

Internal references quoted in §2/§3 (repo-local, at time of writing):

- `docs/ARCHITECTURE-V2.md` (§3 "Everything is config data")
- `docs/STATUS.md` ("configurable directed relationships and cycle limits")
- `docs/adrs/ADR-35.md` (stage-tag vocabulary, §5 lifecycle), `docs/adrs/ADR-52-orchestration-safety-gates.md` (plan artifacts, run caps), `docs/adrs/ADR-44.md` (project-root consent), `docs/adrs/ADR-49-config-first-catalog.md` (providers-as-data precedent), `docs/adrs/ADR-57-config-change-propagation.md` (watcher + CLI reload)
- Code anchors captured in §3.