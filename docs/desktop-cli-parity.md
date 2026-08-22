# Desktop ↔ CLI Parity Plan

**Status: complete as of 2026-08-03.** Every item in this plan (R2–R4 and
P1–P7) is implemented in source; R1 is tracked on a separate branch and is not
duplicated here. This document is retained as the historical record and the
regression checklist. The fine-grained pending-work list lives in
[TODO.md](TODO.md).

## Objective

Full feature parity between Desktop and CLI frontends, except where the
feature is fundamentally GUI-only and cannot be usefully rendered in a TUI.

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

**File**: `crates/cli/src/app.rs` — function `event_line()` (line 1123)

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

## GUI-Only (stay Desktop)

| Feature | Reason |
|---------|--------|
| `AgentGraph` page | Visual directed graph — TUI can't render usefully |
| `DiffViewer` page | Side-by-side diff — TUI lacks horizontal space |
| `Terminal` page | CLI IS a terminal |
| `OrchestrationStudio` | Complex drag-drop + code editor — GUI-only |
| `Editor` | Studio code editor — GUI-only |
| Dashboard CSV export (removed) | Dropped with the Dashboard page (ADR-41): CSV export no longer exists in the desktop UI, so file export is CLI shell piping — the original parity note now describes reality |
| Theme switching / font size | Not applicable to TUI |
| Screenshot capture | Not applicable to terminal |
| Toast notifications | Not applicable to TUI |
| Circuit tick animation | Visual animation — not applicable |
| In-chat code block copy button | Not applicable to TUI |

---

## Execution Order

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
