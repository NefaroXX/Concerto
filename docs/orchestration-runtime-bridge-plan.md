# ADR-58 P2+P3 — Runtime–Blueprint Bridge Design Plan

Status: **draft** (pre-implementation design; no code changes yet).

Companion to `docs/adrs/ADR-58-configurable-orchestration.md` (accepted) and
`docs/research/orchestration-blueprint.md` (research). P1 — blueprint data
model, load-time validation, legacy equivalence, and the frontend parity test —
has landed on `dev` (PR #148, squash `6691f15`; branch commits `7c78abd`,
`58447d2`, `a887a34`, `f8e6e71`). This document is the P2+P3 **runtime-bridge**
design: how the coordinator and runtime consume the *resolved* blueprint as
their single dispatch authority, byte-identical on the default `standard`
blueprint, with stage and relationship dispatch rewritten in one pass
(ADR-58 D6, `docs/research/orchestration-blueprint.md` §5.12).

Every code anchor below was re-verified against the current tree.

---

## 1. Attachment point

### 1.1 Where the resolved blueprint lives today

- `validate_config` (`crates/config/src/lib.rs:193–218`) resolves a
  `ResolvedBlueprint` on **every** load path — the `[orchestration]` selection
  when present, `OrchestrationConfig::default().resolve(...)` otherwise — and
  applies the load-time extensions (B3 widening cap, B4 unknown-stage-tag hard
  error) plus the ten-rule rulebook. Rule (f) is bound by the ADR-52
  `max_total_iterations` cap when configured and is vacuous otherwise.
- The `ResolvedBlueprint` is currently a **function-local**: after validation
  it is dropped. `AppConfig.orchestration: Option<OrchestrationConfig>`
  (`crates/config/src/schema.rs:423`) holds only the *selection*, never the
  resolution.
- Every consumer (registry seeds, coordinator dispatch, tool-calling
  classification, feeds, relationship cycle caps) lives downstream in
  `crates/orchestrator`, constructed in the multi-agent frontend task of
  `crates/orchestrator/src/runtime_runner.rs` (~`RuntimeRunner` build, e.g.
  `agent_configs`, `tool_calling_roles`, `AgentRegistry::build_with_roles_for_project`
  at `runtime_runner.rs:3078`, `CoordinatorAgent::new(...)` at `runtime_runner.rs:3124`).

### 1.2 Decision: attach the resolved blueprint to the config object

Add a **derived, serde-skipped** field on `AppConfig`:

```rust
/// ADR-58 P2+P3: the validated, resolved blueprint captured at load. Derived
/// state — never round-trips through config files (serde-skip); populated
/// exactly once in `validate_config`, consumed by the runtime facade.
/// `ResolvedBlueprint` must implement `Default`: `skip` fills the field with
/// `Default::default()` on deserialize, and the manual `AppConfig::default()`
/// fills it on construction.
#[serde(skip)]
pub resolved_blueprint: ResolvedBlueprint,
```

- **Single writer:** `validate_config` fills it at the exact spot resolution
  already happens (`lib.rs:214–218`). All of Studio, CLI, API, and the ADR-57
  watcher funnel through `validate_config`, so this is the ADR-58 "one apply
  path" seam — a second divergent resolve path never exists.
- **Single reader-of-record:** the multi-agent frontend in `runtime_runner.rs`
  reads `services.config.resolved_blueprint` once per run and builds the
  facade (§2). The single-agent loop (`agent_loop.rs`) is untouched — the
  blueprint governs orchestration only.
- **Alternatives rejected:** threading `ResolvedBlueprint` through
  `RuntimeServices`/`ServicesBuilder` was considered; rejected because the
  config crate already owns resolution and every runtime site already receives
  `AppConfig`, so an attached derived field is the narrowest change that keeps
  one writer.
- **Serde + equality hygiene (review F8/Q1):** the field is `#[serde(skip)]`
  and `ResolvedBlueprint` must implement `Default` (`skip` fills the field
  with `Default::default()` on deserialize; the manual `AppConfig::default()`,
  `schema.rs:426–454`, fills it on construction). AppConfig's **derived
  `PartialEq` (`schema.rs:302`) must NOT include the field** — a manual
  `PartialEq` impl ignoring it, or store it as
  `Option<Arc<ResolvedBlueprint>>`. No-leak verified: `save_config` writes via
  `toml::to_string_pretty` (`config/src/lib.rs:370`), figment seeds via
  `Serialized::defaults` (`lib.rs:112`), and extraction (`lib.rs:150`) — all
  skip the field, so it never reaches a written config.

**Anchor:** the P1 code explicitly defers this step: `AgentCapabilities::effective()`
doc (`crates/config/src/schema.rs:1540–1546`) states consumers of the *raw*
config "resolve `None` via `effective()` **until the P2+P3 rewrite wires them
through the resolved blueprint**". That comment is the P1→P2+P3 handoff
contract.

## 2. Typed lookup surface + per-site replacement table

### 2.1 Typed surface: `BlueprintFacade`

Live in **`crates/config`** (next to `ResolvedBlueprint`) so runtime,
coordinator, and tests query it without importing orchestration internals. A
read-only query wrapper over `ResolvedBlueprint` — each method is a lookup /
derivation, no new orchestration semantics.

Proposed API (names draft; no public-API freeze yet):

```rust
impl BlueprintFacade {
    pub fn new(resolved: &ResolvedBlueprint) -> Self;
    pub fn stage_by_tag(&self, tag: &str) -> Option<&ResolvedStage>;
    pub fn stage_for_agent(&self, id: &AgentId) -> Option<&ResolvedStage>; // staffing search over def.agents
    pub fn primary_execution_stage(&self) -> Option<&ResolvedStage>;
    pub fn stage_kind(&self, tag: &str) -> Option<StageKind>;
    pub fn is_gate(&self, tag: &str) -> bool;        // Review | Acceptance kind
    pub fn is_terminal(&self, tag: &str) -> bool;    // Acceptance | RunOnce kind
    pub fn is_execution(&self, tag: &str) -> bool;   // Execution kind
    pub fn feed_for(&self, tag: &str) -> Option<RunStage>;                     // ResolvedStage.effective_feed
    pub fn max_cycles(&self, from: &AgentId, to: &AgentId, kind_default: u32) -> u32; // relationship_defaults + StageKind::default_max_cycles
    pub fn tool_calling_roles(&self, agent_configs: &HashMap<AgentId, CustomAgentConfig>) -> HashSet<AgentId>;
    pub fn effective_capabilities_for(&self, seed: &CustomAgentConfig, id: &AgentId) -> ResolvedCapabilities; // seed effective() + staffing stage write mask
}
```

`effective_capabilities_for` generalizes exactly the overlay the P1 parity test
computes (mask `fs_write`/`shell` over the seed's `effective()`,
`tests/parity.rs:204–221`): coder → `(f,t,t,f,f,t)`, the other four →
`(f,f,f,f,f,t)` on the default blueprint.

### 2.2 Per-site replacement table

Every hardcoded consultation replaced by the facade (all line numbers current):

| # | Site today | Hardcode today | Replaced by facade |
|---|---|---|---|
| R1 | `coordinator.rs:3495` review-gate participant | `first_agent_for_stage(REVIEW)` | identical mechanism — registry seeded from blueprint staffing; meaning tied to `Review` kind |
| R2 | `coordinator.rs:3513` review cap | `relationships.max_cycles(&reviewer,&implement,3)` | `relationship_defaults` kind rows + `Review.default_max_cycles() == 3` |
| R3 | `coordinator.rs:3817–3919` validate gate | `first_agent_for_stage(VALIDATE)` + `max_cycles(...,2)` + self-verify fallback | `Acceptance`-kind gate + fallback persona (R8) + `Acceptance.default_max_cycles() == 2` |
| R4 | `coordinator.rs:4407–4457` decompose roster | `ids_for_stage` + hardcoded `coordinator` persona entry | `Execution`-kind staffing + config fallback persona |
| R5 | `runtime_runner.rs:200–254` `tool_calling_roles_for` | seed-stage classification (research/implement/validate) + custom-any-cap | facade `tool_calling_roles` preserving the full legacy disjunction, not a pure kind-mask (§4 Q5 pin) |
| R6 | `runtime_runner.rs:3244–3281` stage feed | `is_implement` ids + `ValidationCycleStarted` → `RunStage` | `feed_map` binding per stage; cycle events map to the gate stage's feed |
| R7 | `planner.rs:330–363` partitions | `is_design/is_research/is_implement/is_review/is_validate` | `StageKind` classifications + `primary_execution` |
| R8 | `planner.rs:490–517` role resolution + `expected_artifacts` | `is_implement` + `raw.files` | `Execution`-kind staffing + `ExecutionFilesDef` (`files` field, blueprint §5.7) |
| R9 | `registry.rs:131–179` seed build closure | seed `capabilities` + `is_validate` eval attach | resolved per-agent capabilities (kind mask overlay) + eval attach keyed to verify semantics (F4) |
| R10 | `planner.rs:521–524` "plan must contain at least one implementation task" | `implement_agent_ids` emptiness | primary `Execution`-kind stage staffing emptiness, message enriched with stage label |
| R11 | `state.rs:128` Rule B | `is_review` | facade `is_gate(stage_tag)` on the **gate being executed**, not the role's registered stage — the coordinator sentinel is never registered, so `stage_of` → `None` → Rule B silently disabled when a Review gate renders the fallback. Default-safe (review staffed) but pinned |
| R12 | `crates/cli/src/health.rs:32` `DEFAULT_TOOL_CALLING_ROLES` | literal `["researcher","coder","validator"]` | derived from facade `tool_calling_roles` (last literal caller deleted) |
| R13 | `cost.rs:13–29` `AgentCostEstimator::typical_tokens` | role-name match | stage-kind-keyed with role fallthrough for custom/freeform (heuristic; see §4 Q5 pin) |

## 3. Sentinel / fallback wiring

- **Engine-owned, never blueprint config** (ADR-58 §5.9): the
  `coordinator-self-execute` sentinel provider mechanism, `CancellationToken`
  threading, policy engine + presets, eval engine + `is_validate`-style harness
  keys, `VirtualFs`, termination/loop caps, cycle-detection keying, and the
  `ready_tasks` dispatch unit all stay non-overridables.
- **Fallback personas become config** (`FallbackPersonaDef`,
  `blueprint.rs:191–220`): per-stage record with `id`, `label`,
  `system_instructions` (rendered **only** when unstaffed), and capability
  flags that default to the stage-kind mask and may only narrow.
- **Ships on `standard` (overclaim fixed, review F5):** the default blueprint
  ships `coordinator_fallback()` (`blueprint.rs:866`) on the **review and
  validate stages only** (`blueprint.rs:670`, `677`). The implement stage
  ships `fallback: None` (`blueprint.rs:851`), and
  `coordinator_self_implement_fallback()` (`blueprint.rs:890`) is a
  `#[cfg(test)]` placeholder that `standard_blueprint()` does **not** include.
  Batch 3 must promote the placeholder to production to support
  unstaffed-`Execution` blueprints.
- **Capability flags (review F1, major):** `FallbackPersonaDef.capabilities`
  is `StageFlags` = `{fs_write, shell}` only (`blueprint.rs:135–140`), but
  today's `self_implement_agent` hardcodes `fs_read`/`git`/`lsp` =
  `Some(true)` (`coordinator.rs:708–715`, roster `4445–4457`). The
  Execution-kind sentinel render supplies **engine-owned** `fs_read`/`git`/`lsp`
  = `true` defaults (analogous to the eval attach for Acceptance); extend the
  B5 faithful-mirror requirement (ADR-58 finding B5) from instruction sections
  to capability flags. Reachable regression today: disabling the coder seed →
  unstaffed Execution → persona without `fs_read`/`git`/`lsp` → tool-policy
  denials (pinned by `coordinator_self_executes_when_no_implement_agent_is_registered`,
  `coordinator.rs:5547`).
- **Sentinel resolution by gate context (review F2, major):** which fallback
  renders is decided by **which gate is unstaffed** — never by id-keyed
  lookup. `coordinator_fallback()` carries id `"coordinator"` (review+validate)
  and the Execution placeholder id is `"coordinator-self-execute"`, but today's
  runtime emits `"coordinator"` for both self-implement and self-verify
  (subtask role `coordinator.rs:3833`, events `coordinator.rs:1161`). Pin:
  keep emitting `"coordinator"` for the self-implement sentinel — it matches
  today's events and subtask roles.
- **Feed binding preserves the coordinator-self-implement path (review F4):**
  the R6 per-stage feed must keep the `role == "coordinator" && unstaffed-
  implement → Execute` special case today's feed task has
  (`runtime_runner.rs:3272–3277`), or sentinel-role subtasks carry their
  Execution-stage tag.
- **Replacement of the hardcoded personas** (`coordinator.rs:692–717`
  `self_implement_agent`, `736–749` `self_verify_agent`): those two builders
  become *renders of the stage block's fallback* in front of the same
  `GenericSpecialistAgent` machinery. The reserved `coordinator` id still
  cannot be registered (`registry.rs:111` skips it), so the sentinel identity
  stays collision-free.
- **Gate loops unchanged in shape:** `first_agent_for_stage` first; when empty
  and `self_execute_available()` / `self_verify_available()`, coordinate the
  fallback persona. The `run_coordinator_self_verify` routine
  (`coordinator.rs:902–922`) stays engine-owned; only the persona it runs
  becomes config.
- **Sentinel validation:** `RESERVED_BLUEPRINT_NAMES`
  (`blueprint.rs:43`) reserves `coordinator` / `coordinator-self-execute`;
  rule (j) rejects a custom stage tag that collides. ADR-35 §8 trigger-1
  (stage absence + executor ⇒ self-impl) is preserved by the default blueprint's
  Execution-stage fallback, byte-identical.

## 4. Latent forks → catalog flags

Pre-blueprint strings that key on stage/tag identity; each is re-keyed to a
closed catalog kind or flag (never a raw tag):

| Fork | Today | Bridge to |
|---|---|---|
| F1 | **Legacy tool-calling write route** — writes decided by role-tag identity | **deleted** (D1); writes flow through the `Execution`-stage mask enforced by the policy engine; seeds re-granted via the mask |
| F2 | `DEFAULT_TOOL_CALLING_ROLES` (`cli/src/health.rs:32`) literal | facade `tool_calling_roles` (R12) |
| F3 | Stage feed (`runtime_runner.rs:3262–3281`) hardcoded on `SubTaskCreated`(implement)/`ValidationCycleStarted` | per-stage `feed_map` (R6); `EventKind` stays closed; custom stages emit generic `StageStarted` or nearest-kind |
| F4 | Eval-harness key `registry.rs:142` `is_validate` | `Acceptance`-kind verify semantics (eval engine + harness keys are non-overridables) |
| F5 | Rule B cycle detection `state.rs:128` `is_review` | `Review`-kind gate flag — pinned: `is_gate(stage_tag)` at the **gate being executed**, not the role's registered stage (review F3, R11) |
| F6 | Planner artifact ownership `planner.rs:510–517` `is_implement` | `Execution` kind + `ExecutionFilesDef.files` (R8) |
| F7 | `None` / `run_once` stage-tag semantics (implicit freeform) | explicit `RunOnce` kind (`config/src/lib.rs:236–293`); **unknown tags are hard load errors** (deliberate D2 break, retag guidance emitted) |
| F8 | Display strings with stage identity — `coordinator.rs:3497–3510` "No review-stage agent registered; review skipped"; `transcript.rs:208/214` `agent:"Reviewer"`; `failures.rs` cycle messages | route through `StageDef.label`; "skip via deletion" no longer reachable under rulebook (c) (fallback required for unstaffed non-Acceptance gates) |
| F9 | Legacy `multi_agent.relationships` closed-string validation (`config/src/lib.rs:318–338`) | open `RelationshipDef` registry over closed semantics (ADR-58 §4); closed validation retained only while `[orchestration]` is absent |

**Q5 pinned — facade tool-calling lookup is the full disjunction, not a pure
kind-mask** (`runtime_runner.rs:214–220`, `238–246`): builtin seeds require
tool calling when the seed's stage tag is research/implement/validate **or**
the effective capabilities include `fs_write`/`shell`; custom agents with any
effective capability → `true` (they hold the shared executor); explicitly
capability-free → `false`; `coordinator` skipped (`225–227`). The existing
tests already pin this (`runtime_runner.rs:3899–3978`).

**F7 (review) — facade relationship lookup keeps the hard error on unmatched
legacy kinds:** today `validate_multi_agent_relationships` runs unconditionally
at `config/src/lib.rs:222`. The facade must not silently drop a typo'd legacy
relationship under `[orchestration]` — either keep the load-time hard error
for unknown kinds, or explicitly document the behavior change in release
notes. (Distinct from table fork F7 above.)

## 5. Test contract

**Byte-identical — must stay green on the default `standard` blueprint (the
P1 pinned suite + the new parity test), never edited for convenience:**

- `tests/parity.rs`: `default_blueprint_resolves_to_runtime_tables` (5/5:
  stage masks, per-agent resolved caps, collaboration rows, feed bindings,
  gate caps) plus the CLI/Desktop contract tests.
- `registry.rs` (ADR-58 cites `836–1189`): `default_registry_registers_all_five_seeds`,
  `coder_seed_is_generic_backed_freeform`, `validator_seed_attaches_eval_engine_and_runs_it`,
  `eval_disabled_validate_stage_agent_fails_fast_through_registry`,
  `stage_less_user_override_preserves_seed_stage`, pre-ladder override
  inheritance (`1026`, `1146–1168`), `merge_inherits_unset_fields_from_seed_but_keeps_explicit_ones`
  (`1193`), `ids_for_stage_filters_by_declared_stage` (`534`).
- `planner.rs` (`831–1049`): `custom_role_is_accepted_as_freeform_agent_id`,
  `lifecycle_managed_roles_are_rejected_from_plan`,
  `custom_implement_stage_agent_is_treated_as_coder`, `unknown_role_is_rejected`,
  `custom_lifecycle_stage_role_is_rejected`, `no_implement_agent_fails_fast_before_llm_call`,
  `coordinator_self_role_counts_as_implement_stage_agent`.
- `coordinator.rs`: `decompose_task_rejects_empty_design_doc` (`4923`),
  `decompose_task_accepts_non_empty_design_doc` (`5717`),
  `zero_file_implement_guard_short_circuits_once_per_lineage` (`5267`),
  `zero_file_research_stage_agent_is_unaffected` (`5446`),
  `no_implement_agent_returns_partial_with_clear_error` (`5508`),
  `coordinator_self_executes_when_no_implement_agent_is_registered` (`5547`),
  `custom_implement_stage_agent_triggers_review` (`5635`),
  `build_task_without_validator_is_not_silently_accepted` (`5686`),
  `coordinator_self_verifies_build_task_when_no_validator_registered` (`8721`),
  `non_build_task_without_validator_is_vacuously_accepted` (`8891`),
  `planning_only_dispatch_limited_to_design_stage` (`9010`), design-stage
  retry/ladder tests (`7797–7920`) and ladder tests (`6425`, `7210`, `7535`,
  `7613`, `7668`).
- `runtime_runner.rs` stage-tracker tests (`5785`, `5848`, `5906`).
- `crates/config`: B3/B4 unknown-tag + widening tests (`lib.rs:948–989`), the
  rulebook (a)–(j) tests, `run_once_*`, `named_blueprints_all_validate_and_resolve`,
  `rule_j_rejects_reserved_stage_tag`, and schema legacy-freeform / mixed-case
  normalization (`schema.rs:1823–1830`).

**Must-adapt — owned by the rewrite, updated only where user-visible output
changed in the same batch:**

- `coordinator.rs:3817–3945` table-driven review/validation gates;
  `coordinator.rs:4419–4457` decompose roster; `coordinator.rs:692–717` &
  `736–749` personas → fallback config.
- `runtime_runner.rs:200–254` `tool_calling_roles_for` → facade;
  `runtime_runner.rs:86–94` `configured_relationship` string match → open
  registry kind rows; `runtime_runner.rs:3244–3281` feed.
- **New test (review Q4):** a review-cycle → Verify chip-advance test. Today
  only `ValidationCycleStarted` advances Verify
  (`runtime_runner.rs:3278–3280`); P1 binds `review → Verify`
  (`blueprint.rs:668`), so the review-gate chip change is deliberate.
  Feed-only; replay binds the same table.
- `planner.rs:510–517` artifact ownership keyed to `ExecutionFilesDef` (only if
  a message changed — none do on default).
- The review-skip message (F8) changes **only** on a non-default blueprint that
  omits the review fallback — never on default.

## 6. Batch breakdown

Four batches, each keeping the default blueprint byte-identical and ending with
the pinned suite + parity green. CLI, Desktop, and API are untouched until the
last batch (they only observe events).

1. **Attachment + typed surface** — `AppConfig.resolved_blueprint` (derived,
   serde-skipped, excluded from `PartialEq`) filled in `validate_config`;
   `BlueprintFacade` in the config crate; wire it into `runtime_runner`
   multi-agent construction. Purely additive, no behavior change. Two
   sequencing guards land here: (1) `debug_assert!` at `Coordinator::stage_of`
   (`coordinator.rs:1140`) and `first_agent_for_stage` (`coordinator.rs:1150`)
   comparing the registry answer against `facade.stage_for_agent(id)` / facade
   staffing — active once the facade lands, and every replaced site R1–R11
   funnels through these two methods; (2) extend `tests/parity.rs` with a
   registry↔blueprint cross-check on `standard`: walk
   `registry.ids_for_stage(tag)` for each canonical tag and assert equality
   with the resolved blueprint `def.agents` per stage.
2. **Registry + planner + tool-calling from resolved data** — specialist seeds
   and custom agents built from resolved per-agent capabilities and staffing;
   decompose/role resolution and artifact ownership off `StageKind` /
   `ExecutionFilesDef`; delete the legacy tool-calling write route (R5 → facade,
   F1/F2/F6/F9).
3. **Table-driven coordinator** — review/validate gate dispatch and decompose
   roster through the facade; fallback personas rendered from
   `FallbackPersonaDef` (replacing `self_implement_agent` /
   `self_verify_agent`), **promoting `coordinator_self_implement_fallback()`
   from `#[cfg(test)]` to production** (review F5) so unstaffed-`Execution`
   blueprints work; feed map (F3/R6); cycle caps from `relationship_defaults`
   + kind defaults (R2/R3/R11).
4. **Observability + harness + contract migration** — stage-aware event feed
   binding so replay matches the UI; eval-harness keying to `Acceptance` kind
   (F4); last `DEFAULT_TOOL_CALLING_ROLES` caller deleted (R12); cost heuristic
   re-keyed (R13); full pinned suite + parity re-run; must-adapt assertions
   touched only where messages/labels actually changed.

## 7. Open questions & risks

### Resolved at review (oracle trap-review verdict)

1. **Q1 — derived-field storage shape: resolved.** `#[serde(skip)]` +
   `Default` (review F8): the skipped field, with `ResolvedBlueprint:
   Default` so deserialize fills it and the manual `AppConfig::default()`
   (`schema.rs:426–454`) fills it on construction. AppConfig's **derived
   `PartialEq` (`schema.rs:302`) must not include the field** — a manual
   `PartialEq` ignoring it, or store it as `Option<Arc<ResolvedBlueprint>>`.
   No leak into written configs (verified): `save_config` uses
   `toml::to_string_pretty` (`config/src/lib.rs:370`), figment seeds via
   `Serialized::defaults` (`lib.rs:112`), and extraction (`lib.rs:150`) — all
   skip the field.
2. **Q2 — freeform/B3/B4 load checks: closed, already live.** B3/B4 run on
   **every** load path (`config/src/lib.rs:193–222`); canonical rejection test
   `b4_unknown_agent_stage_tag_rejected_with_guidance` (`lib.rs:951–971`);
   retag guidance in the B4 error (`lib.rs:273–278`); builtin seed ids exempt
   from B3/B4 (`lib.rs:262–264`). No shipped config/preset is affected —
   named blueprints define stages only. All "will newly fail" framing removed.
3. **Q3 — `ReportsTo` semantics: accepted → `Delegation`.** In the open
   catalog (`RelationshipSemantics::Delegation`, same family the default
   `supervises` rows already use); preserves the closed-list semantics, zero
   delta on the default blueprint today, and is forward-compatible for custom
   relationship rows.
4. **Q4 — review→Verify chip change: deliberate.** Today only
   `ValidationCycleStarted` advances Verify (`runtime_runner.rs:3278–3280`);
   P1 binds `review → Verify` (`blueprint.rs:668`). An explicit test asserting
   the review-cycle → Verify chip advance is added to the contract (§5).
   Feed-only; replay binds the same table.
5. **Q5 — tool-calling lookup: disjunction preserved.** The facade
   `tool_calling_roles` reproduces the full legacy disjunction (§4 pin) — not
   a pure kind-mask.

### Residual risks

- **Review findings F1–F3 are only reachable on custom blueprints** — the
  default path staffs every gate, so `standard` stays byte-identical.
- **Config becomes the pipeline authority.** A facade bug or a bad named
  blueprint changes runtime topology. Mitigated by the rulebook at load and by
  the parity test running in CI.
- **Pinned-test churn.** The ~30 pinned tests asserting hardcoded
  labels/strings must be re-verified after batch 3; keep them on the default
  blueprint (byte-identical) rather than rewriting assertions wholesale.
- **The `run_once`/freeform break is load-time, not runtime** — default
  behavior unchanged; only *unknown tags* fail to load (D2, already live via
  B4). Retag path must be documented in release notes.
- **Two sources of truth during transition (batches 1–3).** Legacy hardcodes
  and the facade coexist; the parity test is the only guard that they agree.
  Each batch must stay behind a single resolve seam so any drift fails parity
  instead of silently forking.
- **Replay/audit must match the UI.** F3/F6 change feed emission; sessions
  audit/replay consume the same feed binding (`transcript.rs`), so the feed
  table must be the one place emission and replay derive from.
- **Renamed-stage literal lookups remain (post-P2+P3).** ~~A renamed
  primary-`Execution` tag now plans and decomposes correctly, but sibling
  literals remain: `execute_task` replan (`coordinator.rs:2136/2140`
  `IMPLEMENT`), the zero-file short-circuit (`2900` `REVIEW`), and the
  self-verify fix-pair (`4136` `IMPLEMENT`). A renamed tag silently loses
  post-failure replan/fix loops, and a renamed Review/Validate *tag* skips
  its gate. Out of scope for batches 1-4 (§6) — track as a follow-up phase.~~
  **Resolved in `d4b5c37` (issue #150):** all of these sites now resolve by
  stage *kind* through `BlueprintFacade` (`first_stage_of_kind`,
  `kind_stage_tag`, `role_in_kind_stage`), covering the full literal
  inventory from §6 R-table claims plus feeds, gate labels,
  `coordinator_self_implements`, and the ConsoleDecisionCategory ledger.
  Canonical-tag fallbacks remain only when no facade is attached or a kind
  is absent from the pipeline (byte-identical on `standard`, pinned by the
  existing execution-stage/parity tests). Regression tests: facade
  `first_stage_of_kind`, coordinator `kind_stage_tag` +
  renamed-acceptance fallback persona, runtime gate labels on renamed tags.
- **Custom Acceptance-kind tag eval coverage.** The façade-attach parity
  path is exercised on the **standard** `validate` tag *and* on a renamed
  acceptance tag (`ship`, custom fallback persona) by
  `acceptance_fallback_resolves_renamed_acceptance_stage` (`coordinator.rs`
  tests, part of `d4b5c37`) — proving kind-keying diverges from legacy-tag
  keying exactly where the issue predicted, while standard-tag coverage
  still pins the legacy path.