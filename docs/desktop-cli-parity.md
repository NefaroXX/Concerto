# Desktop ↔ CLI Parity Plan

**Status: settings and configuration parity expanded 2026-10-03; interaction
parity remains in progress.** The August R2–R4/P1–P7 checklist below is a
historical milestone, not proof of complete parity. [ADR-78](adrs/ADR-78-cli-settings-and-studio-parity.md)
corrects the former blanket exclusions. See [CLI settings reference](cli-settings.md).

## Objective

Parity applies to capabilities, settings, data and actions. Only graphical
presentation is exempt. A Studio layout does not exclude agent configuration;
a graph has textual data; desktop appearance preferences can be set from CLI.

## Implemented settings and management parity

| Desktop capability | CLI equivalent | Behavior |
|---|---|---|
| Shared settings | `config show/get/keys/set/unset/path` | Effective/global reads, global/project writes, TOML values, indexed records, file input, dry-run previews |
| Providers, default model, assignments, profiles | `providers add/remove/refresh`; `config set model_settings…` | Same provider routes and discovery; in-use removal requires reassignment |
| API keys | `credentials status/set/delete` | OS keychain; hidden prompt or stdin; values never accepted as arguments |
| Policies, order, time windows | `config set policy…` | Existing policy engine and approval boundaries |
| Retry, memory, context, spend | `config set retry…/memory…/context…/session_spend_cap_usd` | Same load-time validation and runtime readers |
| Shell profiles and selection | `shell list/test/select`; `config set shell_settings…` | Same canonical profile and argument vectors |
| Managed Bash | `shell managed install/remove/verify/export/import` | Existing service and integrity checks; import does not copy binaries |
| Skills settings and lifecycle | `config set skills…`; `extensions skills list/show/create/edit/remove/enable/disable` | Existing SkillManager; deletion keeps recoverable manifest backups |
| MCP configuration and CRUD | `config set mcp.enabled`; `extensions mcp list/show/add/set/remove/enable/disable` | Same server records; environment values redacted |
| MCP connection probe | `extensions mcp probe ID` | Explicit spawn/initialize/list/stop; bounded/cancellable; no saved switch changes |
| Plugins | `plugin installed/install/remove/revoke`; `plugin list` for grants; `config set plugins…` | Existing installer; replacement revokes stale grants; consent remains required |
| Project AGENTS.md | `config set project_context…` | Same injection and advisory nudge controls |
| Studio agent settings | `agents list/show/import/set/clone/remove` | Global canonical per-agent files; prompts, lifecycle, coverage, models and capability requests |
| Advisory blueprints | `blueprint list/show/select/import`; `config set orchestration.blueprint.inline…` | Same resolver; global only; Coordinator retains dispatch authority |
| Desktop theme and font size | `preferences show/set ui_theme/set ui_font_size` | Existing preference store and instance lock; effective next Desktop startup |
| CLI display | `config set display…` | Same flag/env precedence; CLI theme and desktop theme preferences are separate settings |
| Roots, audit, updates, observability | `config set project_roots/audit…/updates…/observability…` | Existing readers and startup/run semantics |
| TUI quick settings | 19 fields with scrolling selection | Adds memory, retention, retries, extensions, context, Git init, motion, title, theme, shell and update check |

Standard builds now include both frontends. Commands automatically select CLI
mode without a setup wizard or provider. CLI-only builds remain available via
`--no-default-features --features cli`. `--desktop` plus a CLI command is an error.

### Scope, validation and runtime semantics

- Reads default to the effective config; `--global` excludes project/env
  overrides. Writes default to global; `--project-scope` changes only the
  selected project's `.concerto.toml`. `--project DIR` selects the project.
- Roster, blueprint and legacy model pins remain global only. Agent commands
  write canonical agent files, never an ignored inline `custom_agents` array.
- Document edits preserve unrelated comments/keys, reject unknown new keys,
  validate through the existing load seam before writing, and replace files
  atomically. Agent reference cleanup uses one document edit. The existing
  multi-file roster save is not a transaction across all files: an OS write
  failure can still require recovery/retry.
- `--dry-run` returns redacted JSON and writes nothing. `--file` accepts one
  TOML value for multiline/structured settings. Numeric indexes address
  existing records; replace the full array or use lifecycle commands to add
  and remove records. `unset` restores defaults/inheritance.
- Changes generally apply to the next run; startup preferences apply after
  restart. Config writes cannot remotely control a running Desktop instance.
  Higher-priority project/env/startup flags can override saved values.
- Exposing config does not make unwired runtime knobs operational: scheduling
  concurrency limits remain a separate runtime issue. Agent capabilities are
  access requests, not policy grants. Policy and write gates remain authoritative.

## Remaining portable interaction work

These are real gaps, not GUI-only exclusions.

| Priority | Capability | Planned CLI acceptance |
|---|---|---|
| P1 | Staged change review | TUI unified diff and accept/reject through the shared overlay/review service; narrow-terminal checks |
| P1 | Live Coordinator pause/resume/decisions | Same in-process control handles and checkpoint semantics; cross-process control requires a shared authenticated API |
| P1 | Runtime Studio inspection | Text/JSON views of world model, decisions, diagnoses, suitability, evidence and topology through existing projections |
| P2 | Rich memory explorer | Search/detail/delete through the shared memory service; graph/explain commands cover only part of Desktop |
| P2 | Structured TUI editors | Roster/provider/policy/MCP inspectors and multiline prompts backed by validated command services |
| P2 | Editor operations | File/context selection, search, diagnostics, patch/review, attachments and external-editor integration |
| P2 | Terminal workflow | Launch selected profile with the same toolchain/environment; only terminal-page layout is exempt |

### Regression checks

Config tests cover comments, invalid/unknown edits without writes, scope and
inheritance, dry runs, indexed records, authoritative roster files and redaction.
Process tests cover fresh installs, agents, MCP CRUD and a real stdio handshake,
skill-pack lifecycle and recoverable deletion, providers, blueprints and desktop
preferences. TUI selection rendering is checked at 24×8, 40×12, 80×24 and 120×40.
Launcher routing skips startup-option values. Formatting, tests and Clippy are
required before handoff.

Branch validation on 2026-10-03: formatting, workspace Clippy with warnings
denied, dependency checks and 5,052 workspace unit/integration/doc tests passed
(12 existing ignores). One unchanged Linux CPU-watchdog test was excluded after
also failing in isolation: this sandbox's process IDs differ from `/proc` IDs,
preventing its process-group accounting. A live TUI smoke check covered settings
navigation, persistence and clean exit.

---

## ✓ Achieved (from Issue #58 work)

| Item | Status | Notes |
|------|--------|-------|
| CLI uses `ProjectSessionManager` | Done | Both frontends resolve sessions through the same path |
| Structured `Vec<Message>` history | Done | No more text-flattening / MAX_HISTORY_TURNS |
| Shared `ServicesBuilder` / `RequestBuilder` | Done | Single construction path for both frontends |
| Unified `ContextOverflowStrategy` trait | Done | Core trait, `SummarizeOldest` wired in `runtime_runner` |
| CLI `conversation_history` field removed | Done | No unbounded in-memory history cache |

---

## Regressions to Fix (shared, both frontends affected)

| # | Item | Status | File(s) |
|---|------|--------|---------|
| R1 | Token-budget-aware context cap replacing deleted `history_limit` | Deferred — handled on a separate branch; not duplicated here | `orchestrator/session_manager.rs` |
| R2 | `ProviderSummarizer` cancellation leak (fresh `CancellationToken` instead of run's) | Done — `ProviderSummarizer::new` accepts the run's token | `orchestrator/services/summarizer.rs:42-44` |
| R3 | Integration test asserting convergent request-building path | Done | `orchestrator/tests/parity.rs` |
| R4 | CI grep-check: fail if frontend crates contain `Vec<Message>` field or `MAX_HISTORY` constant outside display code | Done | Former self-hosted CI (grep-check not carried into `.github/workflows/ci.yml`) |

---

## Portable Features (CLI should have these)

### P1 — Event rendering (CLI ignores events Desktop shows)

| Event | Desktop | CLI | Source |
|-------|---------|-----|--------|
| `AgentThought` | In chat | Done — rendered in `event_line()` | `cli/src/app.rs:1139` |
| `ShellOutputChunk` | In chat | Done | `cli/src/app.rs:1170` |
| `SubTaskCreated` | Agent activity inline | Done | `cli/src/app.rs:1140` |
| `SubTaskCompleted` | Agent activity inline | Done | `cli/src/app.rs:1143` |
| `SubTaskFailed` | Agent activity inline | Done | `cli/src/app.rs:1155` |
| `SpendUpdated` | Live cost display | Done | `cli/src/app.rs:1175` |
| `IndexingCompleted` | Shown | Done | `cli/src/app.rs:1190` |
| `SessionSaved` | Shown | Done | `cli/src/app.rs:1198` |

**File**: `crates/cli/src/app.rs` — function `event_line()` (line 1530)

### P2 — Session list / resume screen

Done — `Screen::Sessions` variant (`crates/cli/src/app.rs:683`) loads the
session list and resumes on Enter (key handling at `:634`).

### P3 — Provider/model inline picker

Done — `SettingsField::Provider` cycles configured providers
(`crates/cli/src/app.rs:70`, applied at `:923`).

### P4 — Agent model assignments in settings

Done — `Screen::AgentAssignments` (`crates/cli/src/app.rs:760`, key handling at
`:636`).

### P5 — Tool log modal overlay

Done — `Screen::ToolLog` (`crates/cli/src/app.rs:688`, key handling at `:635`)
using the same centered-overlay pattern as `draw_approval_modal()`
(`crates/cli/src/ui.rs:328`).

### P6 — Memory status in status bar

Done — `draw_status_bar()` renders a `mem: N` chunk count when memory is
populated (`crates/cli/src/ui.rs:239`, `:250`).

### P7 — Project directory switching

Done — `switch_project()` (`crates/cli/src/app.rs:229`) plus the interactive
project picker invoked from the status screen (`:654`).

---

## Presentation differences (historical exclusions corrected)

| Feature | Reason |
|---------|--------|
| `AgentGraph` canvas | Canvas layout is graphical; topology/status data remains portable |
| `DiffViewer` side-by-side layout | Unified terminal diffs/review actions remain portable and planned |
| `Terminal` panel | Embedded layout differs; configured profile/environment operations remain portable |
| `OrchestrationStudio` layout | Agent and blueprint commands implemented; live inspectors planned |
| `Editor` layout | File/search/diagnostic/context operations remain portable and planned |
| Dashboard CSV export (removed) | Dropped with the Dashboard page (ADR-41): CSV export no longer exists in the desktop UI, so file export is CLI shell piping — the original parity note now describes reality |
| Theme switching / font size | Both configurable in CLI; desktop font size affects Desktop |
| Screenshot capture | Not applicable to terminal |
| Toast notifications | Popup layout differs; warnings and completion notices remain portable |
| Circuit tick animation | Visual animation — not applicable |
| In-chat code block copy button | Not applicable to TUI |

---

## Historical August execution order

All steps are merged on `dev` except R1, which is tracked on a separate branch:

1. **R2** — Cancellation leak fixed
2. **R3** — Integration test added
3. **R4** — CI tripwire added
4. **P1** — Event rendering
5. **P2** — Session list/resume screen
6. **P3** — Provider picker
7. **P4** — Agent model assignments
8. **P5** — Tool log modal
9. **P6** — Memory status
10. **P7** — Project dir switching

---

## Notes

- R1 (token-budget-aware context cap) is being addressed on a separate branch
  and is not duplicated here. Once parity is properly set up, fixing R1 will
  benefit both frontends simultaneously.
- All changes in this plan must preserve `cargo test --workspace` passing.
- Every complete step gets its own atomic commit.
