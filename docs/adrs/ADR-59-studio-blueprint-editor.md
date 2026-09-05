# ADR-59: Studio orchestration editor — one surface, config-owned, full CRUD

**Status:** Accepted — **revised in place 2026-08-15** (maintainer direction:
overwrite, no superseding ADR; this document is the corrected record). The
original 2026-08-14 acceptance and P4 batches 1–3 landed the write seam
(`toml_edit`, atomic `save_blueprint`, target-shadow guard), the apply path,
and the `Blueprint`-model Studio. The revision replaces the closed-kind stage
cards and splash-based init with the **one-surface roster editor** (full CRUD,
locked coordinator, silent auto-seed, single-arm Save) per the ADR-58 revision
of the same date. Revision landed on `dev`: `1052b94`, `1d6d7e2`, `f8a6f79`.
Gates: workspace build, clippy `-D warnings`, fmt, config 192 /
orchestrator 445 / desktop 402 tests, parity 6/6.

**Date:** 2026-08-14 (original), 2026-08-15 (revision)

**Deciders:** Concerto architecture + maintainer direction

**Supersedes:** Nothing in full. Completes the write side of ADR-58's
config-ownership model and extends ADR-57's one-apply-path discipline.

## Context

ADR-58 (revised) establishes: only the coordinator is hardcoded; agents,
stages, blueprints, prompts, assignments, and permissions are config data; the
five specialists are seed templates materialized into the project config; the
config owns the roster once `[multi_agent.custom_agents]` is non-empty or
`[orchestration]` is present; deletions stick.

The original ADR-59 delivered a splash + one-click init, stage cards over the
six closed kinds, and a blueprint-path Save writing the include file. Under
the ADR-58 revision that surface is wrong in four ways: stage kinds are open
strings, not six closed picker options; the splash and separate init step are
ceremony — seeding must be silent; the agent roster was not editable (and
legacy surfaces were hidden, not replaced); and after Save the app
navigated away to a blueprint screen, with no way back and clipped cards.

The Studio is the editor for the config file as the source of truth. It must
edit everything the config owns — agents, stages, relationships — on one
surface, and persist merge-aware and atomically.

## Decision (revised)

1. **One surface, one model.** The Orchestration Studio binds the resolved
   config model (`Blueprint` + `StageDef` + `RelationshipDef` + the agent
   roster) on a single screen: agent roster pane, pipeline stage cards, and
   relationship rows. The legacy `multi_agent` editing tables are removed
   from the Studio; legacy `multi_agent` config remains authoritative only
   while `[orchestration]` is absent (no schema change, no behavior change on
   the legacy path).

2. **Silent auto-seed, no splash.** Opening the Studio calls
   `ensure_orchestration_seeded`: if the config owns no roster, the project
   config layer is seeded with the inline `standard` blueprint and the five
   seed agents, idempotently, preserving all other sections
   (`seed_orchestration_roster`). There is no splash, no separate
   initialization step, no mandatory-order init dance. The previous
   `InitializeBlueprint` message and splash are deleted.

3. **Full CRUD except the coordinator.** All agents are editable: add, edit
   (name, role, model, capabilities, prompt sections, stage, disabled/eval
   flags), and delete — **except the coordinator**, whose row is locked and
   badged (it is the one sanctioned hardcoded actor). Stages support add,
   reorder (move up/down), delete (prunes relationships referencing the
   stage), and **free-text kind editing** with suggestions over the six known
   kinds — unknown kinds are valid and kept verbatim. Relationships support
   add and delete.

4. **Roster ownership; seeds never resurrect.** `load_from_config` shows
   exactly the configured agents when `AppConfig::owns_agent_roster()` is
   true — a seed deleted in the Studio stays deleted across restarts. The
   seeds stand in only for legacy configs that declare neither
   `[orchestration]` nor custom agents. The runtime registry mirrors the same
   rule (`merge_seeds = !owns_agent_roster()`), so the surface and the engine
   can never disagree.

5. **Single-arm Save; no redirect.** Save routes by selection source:
   inline → merge-aware atomic rewrite of the `[orchestration]` section
   (`save_inline_blueprint`, drops dangling name/include selectors);
   include → `persist_include_blueprint`; name/absent → inline materialization.
   The roster is persisted through `save_agent_roster` (merge-aware atomic
   replace of `[multi_agent.custom_agents]`; deletions persist). Save never
   navigates, never switches surfaces; validation gates the write and the
   draft is kept on failure. All writes use `toml_edit` + temp/rename — user
   comments and key order survive.

6. **Validation surfacing.** The Studio calls `validate_blueprint` and maps
   the structured `BlueprintError::Rule { field, code, message }` (codes
   `rule_c`…`rule_j` — the remaining relaxed-rulebook rules, ADR-58 §5) to
   per-field danger outlines, a toolbar badge, and a detail bar. `config_broken`
   renders as a persistent status-bar badge; startup fallback to defaults is
   surfaced with a boot toast. A failed save blocks the write and keeps the
   draft.

7. **Settings → Relationships** (`views/settings/`) is hidden while
   `[orchestration]` is present (no dead second surface); it remains available
   on the legacy path.

8. **Non-goals (unchanged).** TOML diff/preview panel; canvas DAG editor;
   migration runner (P5); post-seed `config.toml` wholesale rewriting.

## Amendment (2026-09-05) — blueprints are advisory in the editor

Revised **in place** (no new ADR number). The Studio's blueprint editing
surface remains roster/config CRUD, but blueprints (including the seeded
`standard` one) are **advisory data**: editing a blueprint never enforces
staffing or dispatch order (ADR-58 amendment 2026-09-05, ADR-35 amendment
2026-09-05). The Studio's authoritative surface is `custom_agents` — add,
edit, disable, delete agents freely; the Coordinator consumes them as context.
`seed_orchestration_roster` behavior is unchanged (it materializes the seeds
as the user's own editable config).

## Consequences

- The config file is the sanctioned user-editable artifact; Studio and
  hand-editing interoperate through the watcher (single reconcile path,
  ADR-57).
- The seed agents become the user's own data after first open — refine or
  delete them; deletions are permanent everywhere.
- The coordinator is visibly the one fixed component (locked row), matching
  the ADR-58 model exactly.
- Unknown stage kinds round-trip through the Studio without being mangled.
- Risks: the merge must never drop user comments or reorder keys (tested);
  watch/UI races are mitigated by the single apply path and content-aware
  reconcile.

## Testing

- Roster persistence: `save_agent_roster` round-trip; deletion persists
  across save → reload (config crate tests).
- Ownership: `load_from_config_keeps_deleted_seed_agents_deleted` (Studio)
  and `config_owned_roster_never_resurrects_seed_agents` (orchestrator
  registry) pin the no-resurrection contract.
- CRUD: add/edit/delete agent; stage add/move/delete with relationship
  pruning; kind free-text accepts unknown kinds; coordinator row is locked.
- Save routing: inline/include/name selection arms; no navigation after
  save; validation failure keeps the draft.
- Merge/atomicity: comments, untouched keys, and key order preserved;
  induced mid-write failure leaves the previous file intact.
- Apply path: include-file content change → `ConfigReloaded` → live
  `resolved_blueprint` updates.
- Gates: fmt; clippy `--workspace --all-targets -D warnings`; config /
  orchestrator / desktop suites; parity 6/6.

## Relationship to prior ADRs

- **ADR-58 (revised)**: implements its config-ownership, open-kind, and
  seed-materialization decisions on the write side.
- **ADR-57**: reuses the watcher + `ConfigReloaded` reconcile; the include
  file is in `TRACKED_NAMES`.
- **ADR-35**: legacy `multi_agent` remains authoritative only while
  `[orchestration]` is absent.

## Deferred / Sequencing

- **P5**: migration runner for existing `multi_agent` configs; export-merge
  hardening.
- **P6**: freeze/stable-surface decisions.
- Diff view and DAG canvas editor: post-P4 stretch, not planned here.
