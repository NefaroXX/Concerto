# ADR-58: Configurable orchestration — config owns the pipeline; only the coordinator is hardcoded

**Status:** Accepted — **revised in place 2026-08-15** (maintainer direction:
overwrite, no superseding ADR; this document is the corrected record). The
original 2026-08-13 acceptance and the P1 (PR #148) / P2+P3 (PR #149) landings
delivered the table-driven runtime that is still in force; the revision below
**reverses** the original D1–D3 and D5 decisions (closed stage kinds, widening
= load error, exactly-one-primary, byte-identical mandate, rulebook (a)/(b)/
(d)/(i)) that contradicted the governing principle. Revision landed on `dev`:
`1052b94` (open kinds + config-owned roster), `1d6d7e2` (auto-seed + single-arm
Save), `f8a6f79` (one-surface Studio CRUD + seed-free rosters). Gates: full
workspace build, clippy `--workspace --all-targets -D warnings`, fmt clean,
config 192 / orchestrator 445 / desktop 402 tests, parity 6/6.

**Date:** 2026-08-13 (original), 2026-08-15 (revision)

**Deciders:** Concerto architecture + maintainer direction

**Supersedes:** Nothing in full. This ADR **amends** ADR-35 in part (see
Relationship to prior ADRs). Every other settled ADR remains in force.

## Context

The governing principle is "everything is config data"
(`docs/ARCHITECTURE-V2.md`). The product directive: a pipeline with a fixed,
source-baked stage vocabulary is a pipeline of **hardcoded agents** — even
though ADR-35 replaced hardcoded role IDs with stage tags, it data-fied only
the vocabulary; the semantics behind those tags stayed in source. Users edit
configs directly, so in practice **the config file is the source of truth and
the Studio is its editor**.

The original ADR-58 went partway: it made the pipeline data-defined, but kept
stage kinds a **closed catalog of six engine kinds** with "closed semantics",
kept a 10-rule rulebook that *enforced pipeline shape* (exactly one primary
Execution stage, terminal reachability, widening = load error), and bound the
whole thing with a "byte-identical defaults" mandate. That still left a
pipeline of hardcoded agents — the six kinds were hardcoded semantics, and
config was ceremony around them.

The maintainer direction (2026-08-15) is unambiguous and load-bearing:

- **Only the coordinator is hardcoded — deliberately.** The coordinator
  engine is the one sanctioned hardcode: it is the dispatch loop, and it is
  aware of all configured agents, roles, permissions, and how to call them.
  Everything it needs arrives as config data.
- **Everything else is config data**: agents, stages, blueprints, prompts,
  assignments, and permissions.
- The five specialists (architect / researcher / coder / reviewer / validator)
  are **seed templates** — known-good starting data that is **materialized
  into the project config file** on first run, then refined by the user.
- Full CRUD for every agent **except the coordinator**, whose row is locked.
- Deletions **stick**: once the config owns the roster, deleted seeds must
  never come back — not in the Studio, not at runtime.
- The named blueprint catalog (standard / tdd / docs-only / research-only) is
  hardcoded **seed data, demoted to seed data** — a known-good starting point,
  not a closed catalog.

## Decision (revised)

### 1. The coordinator is the only hardcoded component (sanctioned)

The coordinator engine — dispatch loop, cycle detection, `ready_tasks` batch
contract, `CancellationToken` threading, policy-engine enforcement, sentinel
identities — is hardcoded **by design** and is not configurable. It consumes
the resolved config and is aware of every configured agent, role, stage,
permission, and relationship. Everything upstream of the coordinator is data.

### 2. Stage kinds are open strings; six known kinds are vocabulary

`StageDef.kind` is an **open `String`**, not an enum. The six kinds from the
original catalog remain as a closed **vocabulary** (`StageKind::parse` /
`as_str`: `research`, `planning`, `execution`, `review`, `acceptance`,
`run_once`) used for:

- known-kind detection (`StageDef::known_kind()`);
- defaults, never enforcement: `is_gate()` (review/acceptance), `is_terminal()`
  (acceptance/run_once), `default_max_cycles()` (review 3 / acceptance 2 /
  else 1), `default_capability_mask()` (only `execution` grants
  `fs_write` + `shell`);
- Studio suggestions and planner/cost heuristics (`typical_tokens` falls back
  to the legacy role table for unknown kinds).

An **unknown kind is valid config**: it dispatches generically and
panic-free (proven by tests), grants no write capabilities by default, and
runs with the engine defaults. Stage semantics are data, never a closed enum.

### 3. The config owns the roster; seeds never merge back

`AppConfig::owns_agent_roster()` is true when `[multi_agent.custom_agents]` is
non-empty **or** `[orchestration]` is present. Ownership semantics:

- When the config **owns** the roster, the config **is** the roster: the
  runtime registry registers exactly the configured agents
  (`merged_agent_configs(_, merge_seeds = false)` in the orchestrator), and
  the Studio surface shows exactly the configured agents. Deleted seeds stay
  deleted — at runtime and across restarts.
- When the config declares **neither** (legacy embedded default), the five
  seed templates stand in as today's behavior.
- Regression tests pin both directions: `config_owned_roster_never_resurrects_seed_agents`
  (registry) and `load_from_config_keeps_deleted_seed_agents_deleted` (Studio).

### 4. Seeds are templates, materialized into the project config

`seed_orchestration_roster(config_path)` (config crate, `saving.rs`) writes,
idempotently and preserving all other sections: an inline `[orchestration]`
`standard` blueprint and the five seed agents under
`[multi_agent.custom_agents]`. The desktop app calls it **silently** when the
Studio opens (`ensure_orchestration_seeded`), and the seeds then become the
user's own editable config. There is no splash and no separate init step.

### 5. Relaxed rulebook: guardrails, not shape enforcement

`validate_blueprint` keeps only integrity and safety gates:

- (c) an unstaffed non-`Acceptance` gate without a fallback persona is
  rejected (`Acceptance` is exempt — unstaffed it falls back by engine design;
  unknown kinds are never gates);
- (d) a stage's fallback persona id must differ from any staffed agent
  (no self-fallback; fallback capability flags are plain flags — the old
  narrowing/widening check is removed);
- (e) `max_cycles = 0` is rejected;
- (f) the sum of stage cycle caps must not exceed the ADR-52 global maximum,
  when set;
- (g) stage tags are unique and non-empty;
- (j) stage tags must not collide with reserved engine names;
- (h) `feed` stays a closed `FeedLabel` enum — an unknown label is a hard
  parse error.

**Removed rules:** (a) exactly-one primary `Execution` — primary is a plain
declarative flag; (b) terminal reachability — a pipeline may end on any
stage; (d-old) fallback capability narrowing/widening — flags are plain
flags; (i) `OnGateCycle` requires a gate kind — any stage may declare it, the
engine resolves semantics. `validate_custom_agents` similarly drops the B3
widening hard error, and its B4 unknown-stage-tag check applies only when
`[orchestration]` is present.

### 6. Named blueprint catalog is seed data

The named blueprints (`standard`, `tdd`, `docs-only`, `research-only`) are
hardcoded **seed data** for known-good pipelines; the Studio's default write
materializes the inline `standard` blueprint. They are a starting point the
user refines — not a closed catalog, and not a constraint on what the config
may express.

### 7. Full CRUD for every agent except the coordinator

The Studio edits agents, stages, and relationships on one surface:

- agents: add, edit, delete; the **coordinator row is locked** (badge + no
  delete/edit);
- stages: add, reorder, delete (prunes relationships), free-text kind editing
  with suggestions over the six known kinds;
- relationships: add, delete;
- persisted merge-aware and atomically via `toml_edit` — comments and key
  order survive (`save_agent_roster`, `save_inline_blueprint` in `saving.rs`);
- a single-arm Save routes by selection source (inline / include / name) and
  **never navigates or switches surfaces**; validation gates the write and the
  draft is kept on failure.

### 8. Non-overridables stay engine-owned

Unchanged: policy/approval engine and preset definitions; eval engine and
keys; `VirtualFs` sandbox and shell/git gating; `CancellationToken` threading;
tool security validation and the deserialization allowlist; termination and
loop-cap enforcement; cycle-detection keying; the `ready_tasks` dispatch unit;
the sentinel provider mechanism (`coordinator-self-execute`); intent
classification (ADR-55/56). The coordinator sentinel identity itself is
engine-owned — it is the one hardcoded actor by design.

## Consequences

- **Config file is the source of truth.** Agents, stages, blueprints, prompts,
  assignments, and permissions are data; deleting an agent in the Studio
  deletes it everywhere, permanently.
- **Unknown stage kinds work.** Users can define custom stages without the
  engine rejecting them; they are generic, write-free by default, and
  panic-free.
- **Seeds become templates, not law.** The shipped specialists are the
  starting point; after first run they are the user's own config rows.
- **The coordinator is the seam.** The one intentional hardcode is the
  coordinator engine, which is config-aware by construction.
- **Legacy configs keep working.** A config with neither `[orchestration]` nor
  custom agents keeps today's seeded behavior until the Studio materializes
  the roster.
- **Costs / risks.** Validation is a guardrail, not a shape check — malformed
  pipelines may load and fail at runtime rather than at load; the Studio is
  the mitigation (structured per-field rulebook errors, badge surfacing).
  Forward-compat stays on `schema_version`.

## Testing

- Known-kind vocabulary: parse/as_str round-trips; unknown kinds dispatch
  generically (`unknown_kind_stage_dispatch_is_generic_and_panic_free`,
  planner unknown-kind tests).
- Ownership: `config_owned_roster_never_resurrects_seed_agents` (registry),
  `load_from_config_keeps_deleted_seed_agents_deleted` (Studio); parity tests
  6/6 pin byte-identical runtime tables on the default blueprint.
- Rulebook: per-rule unit tests for (c)/(d)/(e)/(f)/(g)/(j) and the removed
  rules' acceptance.
- Persistence: `save_agent_roster` / `save_inline_blueprint` merge tests
  (comments, order, deletion persistence), atomicity tests.
- Gates per slice: workspace build, clippy `-D warnings`, fmt, config /
  orchestrator / desktop suites, parity.

## Relationship to prior ADRs

- **ADR-35 — amended, not superseded.** Stage tags stay the architecture;
  `GenericSpecialistAgent` stays. Corrections: (1) authority — writes are
  decided by resolved capability masks enforced by the policy engine, the
  planner routes only; (2) stage vocabulary — unknown tags are valid open
  kinds (previously silent freeform; `run_once` is the explicit known kind
  carrying today's freeform semantics); (3) unstaffed-gate semantics —
  rulebook (c) replaces "deletion skips the gate" with a fallback requirement
  for non-`Acceptance` gates.
- **ADR-42/45** — fallback ladder and `coordinator-self-execute` sentinel stay
  engine-owned.
- **ADR-49** — no conflict; config-driven model/provider assignment extends to
  per-agent pins.
- **ADR-52** — rule (f) stays a static load-time bound on stage cycle caps;
  termination/loop-cap enforcement stays engine-owned.
- **ADR-55/56** — intent classification stays engine-owned (non-overridables).
- **ADR-57** — Studio Save and the watcher reload converge on the same
  reconcile path (single-arm Save writes config, reload applies it).

## Deferred / Sequencing

| Phase | Work |
|---|---|
| **Done** | Table-driven runtime (P2+P3, PR #149); open kinds + config-owned roster (`1052b94`); auto-seed + single-arm Save (`1d6d7e2`); one-surface Studio CRUD + seed-free rosters (`f8a6f79`). |
| **P5 (pending)** | Migration runner for legacy `multi_agent` configs; export-merge hardening. |
| **P6 (deferred)** | Graph/DAG config support; run-one-stage simulation; freeze decisions. |

Explicitly deferred: TOML diff view of include changes; canvas DAG editor;
multi-executor artifact partitioning (primary stays a plain flag).
