# ADR-70: Project AGENTS.md context injection

**Status:** Accepted

Composes with [ADR-43](./ADR-43-skills-mcp-and-extension-manager.md) (skills
are local instruction packs injected into prompts) and the custom-ai-shell
plan's OS/shell identity card (`docs/custom-ai-shell-plan.md`, Phase C),
which defines the environment-card slot that AGENTS.md precedes. Supersedes:
none.

**Date:** 2026-09-20

**Deciders:** Concerto architecture + maintainer direction

## Context

The AGENTS.md convention (a per-repository instruction file, plus a
user-global one) is the de-facto way real projects, teams, and users
communicate durable working rules to AI agents. Concerto ignores both files
today:

- Skills ([ADR-43](./ADR-43-skills-mcp-and-extension-manager.md)) cover
  reusable, versioned instruction packs, but they are opt-in artefacts —
  nothing reads a project's `AGENTS.md` at run time, so the most common
  carrier of team conventions never reaches the model.
- The single-agent `PromptBuilder` and the coordinator dispatch assembly have
  different assembly code paths, so any new context section must be wired into
  **both** to keep prompts consistent.

Requirements distilled from the lived convention:

1. **Both sources.** The user-global AGENTS.md (platform config dir) and the
   project's `<root>/AGENTS.md` must both be readable and injected.
2. **Deterministic precedence.** On conflict the project file wins; the prompt
   must say so explicitly rather than silently concatenating.
3. **Bounded.** AGENTS.md files can be arbitrarily large; the injected section
   must be cut at a hard, per-file character budget with an explicit
   truncation marker.
4. **Never crash the loop.** A missing, unreadable, or malformed AGENTS.md is
   routine (fresh clones, permissions, disabled feature) — the agent loop must
   never fail on it.
5. **Explicit opt-in.** AGENTS.md content is injected into every prompt of a
   run; reading a user's files into every conversation should not happen by
   default.

## Decision

Ship project AGENTS.md context injection as a run-scoped orchestrator feature,
gated on an explicit `[project_context]` config section (ADR-70 — additive
serde-default only, no schema migration).

### 1. Two sources, project-over-global, both named

`ProjectContext` reads the user-global `AGENTS.md` — from
`ProjectContextConfig.global_path`, or the platform default
`~/.config/concerto/AGENTS.md` (POSIX) / `%APPDATA%\concerto\AGENTS.md`
(Windows) via `default_global_agents_path` — plus the per-project
`<root>/AGENTS.md`. When both are present the injected section names both
sources (`### <path>` headings, global first) and emits a precedence line:
"the project AGENTS.md overrides the global AGENTS.md on conflict."

### 2. Bounded budget, per-file truncation with a marker

Each source is truncated independently to `max_bytes` (default 32 KiB, see
`DEFAULT_PROJECT_CONTEXT_MAX_BYTES`); a truncation marker is appended when
content is cut. The whole assembled section never exceeds the budget.

### 3. Run-scoped: one instance per run, one refresh, cheap reads

`ProjectContext` needs the run's project dir, so the runtime
(`run_shared_agent`) constructs one instance per run from
`services.config.project_context` and `req.project_dir` and calls `refresh()`
once at startup. `section()` just clones a pre-formatted `String`; no
filesystem work happens in the prompt hot path. This is deliberately
different from the process-scoped `SkillsContext` (ADR-43) — the desktop UI
refresh story does not apply to AGENTS.md.

### 4. Fail-soft contract (the loop never crashes on AGENTS.md)

- A source that is absent or empty is skipped silently.
- An unreadable source logs at debug/`tracing::warn!` in the runtime, keeps
  the previous section, and surfaces `Err` so the caller decides how to
  surface it.
- A disabled configuration performs no filesystem work at all.

### 5. Opt-in config, validated at load time

`[project_context]` defaults off (`enabled = false`). Because the section is
injected into every prompt of a run, activation is explicit. Load-time
validation rejects `update_frequency = 0` (would silently disable the nudge
cadence) and `max_bytes = 0` (would collapse every source) through
`ConfigError::InvalidValue`, mirroring `retry`/`memory` validation.

### 6. Coordinator maintenance nudge (advisory, never a file write)

When `auto_update_agents_md = true` AND the project AGENTS.md is present, the
coordinator's decision loop injects `PROJECT_CONTEXT_NUDGE` — a bounded
user message before the dispatch request — every `update_frequency`-th
(decision-loop) dispatch (default 1 = every dispatch). The nudge is text
only: it points the orchestrated agent at the policy-gated filesystem tool to
refresh a stale AGENTS.md. The coordinator itself never edits AGENTS.md.

### 7. Wire points: both prompt paths, same ordering

The section is injected **between the skills section and the environment
card** in both assemblies so prompt order is identical everywhere:

- Single-agent: `PromptBuilder::with_project_context` (`prompts.rs`),
  threaded from `execute_agent_loop` via `run_shared_agent`.
- Multi-agent: `CoordinatorAgent::with_project_context` (`coordinator.rs`),
  threaded from `run_multi_agent` via `run_shared_agent`, rendered in
  `render_dispatch_system_prompt` between the skills section and the
  environment card.

## Consequences

- **Consistent prompt anatomy.** Skills → project AGENTS.md → OS/shell
  identity card holds across single-agent and coordinator paths, so the model
  sees project rules in the same position regardless of dispatch mode.
- **Fail-soft everywhere.** Missing/unreadable AGENTS.md degrades to logging;
  the feature can be safely enabled on any repo.
- **Opt-in privacy posture.** No user file is read into prompts unless
  `enabled = true`; the nudge only fires when the project file actually
  exists and the maintenance toggle is on.
- **Bounded tokens.** 32 KiB per source, section-global budget, explicit
  markers — no prompt-size surprise from a 5 MB AGENTS.md.
- **No UI surface in this ADR.** Settings-facing toggles are `false` for all
  knobs here; the coordinator nudge is text only. Follow-up work could add a
  Settings section.

## Acceptance criteria

- **C1** — With `[project_context] enabled = true`, an `AGENTS.md` in the
  project root is injected into single-agent and coordinator dispatch prompts,
  between the skills section and the environment card.
- **C2** — Global + project both present: section names both sources, orders
  global first, and states project-over-global precedence.
- **C3** — A source larger than `max_bytes` is truncated with the marker and
  the assembled section never exceeds the budget.
- **C4** — Missing files are silent; an unreadable file logs and keeps the
  previous section; a disabled config performs zero filesystem work; none of
  these fail the run.
- **C5** — Load-time validation rejects `update_frequency = 0` and
  `max_bytes = 0`.
- **C6** — The coordinator nudge fires only when `auto_update_agents_md` is on
  AND the project AGENTS.md is present, at the configured cadence, as advisory
  text only — no file write.
- **C7** — `cargo test -p concerto-orchestrator --lib` and
  `cargo test -p concerto-config` pass; fmt/clippy clean on the touched
  crates.

---

*Last updated: 2026-09-20*