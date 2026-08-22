# ADR-27: Integrated desktop terminal lifecycle

> **Archived** — superseded by [ADR-30](../ADR-30.md) (shell-resolution
> portion) and [ADR-20](../ADR-20.md) (terminal lifecycle, consolidated into
> the desktop-UI ADR 2026-08-22). See [docs/adrs/README.md](../README.md) for
> the current index. Retained verbatim as the historical record; not active
> guidance.

**Status:** Superseded by ADR-30 (shell resolution) and ADR-20 (terminal lifecycle)

## Context

Concerto's desktop application needs a project-scoped interactive terminal so a
user can inspect, build, and run generated code without leaving the application.
The terminal is a long-lived child process with an asynchronous PTY event stream,
not a stateless view. Treating it as a standalone widget without application
message routing leaves its backend events unhandled and allows a shell exit or
spawn error to destabilise the desktop session.

The active project folder may also change while Concerto is running. A terminal
must never silently continue in the previous project after that switch.

## Decision

The desktop terminal is a page-routed view backed by `iced_term` and owned by
the root `App` state.

- Terminal creation is lazy. No shell process is spawned until the terminal page
  is first opened.
- The original shell-resolution decision used `COMSPEC`/`SHELL`; ADR-30 now
  requires the terminal to use the canonical agent execution profile.
- The PTY working directory is always the canonical active project folder.
- Changing the active project restarts an already-started terminal in the new
  folder. A terminal that has never been opened remains lazy.
- Each restart receives a new terminal ID so Iced replaces the old subscription
  and listens to the new PTY receiver.
- Terminal events are routed through a namespaced desktop message and handled by
  the terminal state. Shell exit, failed spawn, or a closed PTY becomes visible
  terminal-page state with a local Restart action; it never closes the Concerto
  window or fails an agent run.
- Terminal colors are derived from `AppTheme` semantic palette values. Theme
  changes update a live terminal without hard-coded UI colors.
- The child process is shut down by dropping the backend when the terminal is
  restarted or the application exits.

## Consequences

- Users get a persistent interactive shell scoped to the same project used by
  sessions, memory, and agent tools.
- Ordinary terminal failures are contained and recoverable without restarting
  Concerto.
- The terminal remains alive while another page is selected, preserving shell
  state and running commands until the project changes or the user restarts it.
- `iced_term` follows Iced's unstable widget API, so Iced upgrades must validate
  this integration and its subscription lifecycle together.
- A project change intentionally terminates commands running in the old project's
  terminal to prevent commands and paths from crossing project boundaries.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
