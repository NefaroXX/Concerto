# ADR-07: Terminal UI — `ratatui` + `crossterm`

**Status:** Accepted
**Date:** 2025-07-13
**Deciders:** Concerto architecture

## Context

The project needs a lightweight, cross‑platform command‑line interface. A web UI would add a server and browser dependencies, while an Electron app would pull in a heavyweight JavaScript runtime and increase binary size. A terminal UI can run in any console, works over SSH, and keeps the single‑binary distribution goal.

## Decision

Use `ratatui` (formerly `tui-rs`) for the UI widget layer and `crossterm` for terminal backend handling. Both are pure Rust, have active maintenance, and support Windows, macOS, and Linux terminals.

## Consequences

- The CLI will run an async event loop driven by `tokio`. UI events are translated into `EventBus` messages, allowing the same event model used by the desktop UI.
- Shared state between the CLI and the Iced desktop UI is communicated via the existing `EventBus` (`tokio::sync::broadcast`).
- Adding a terminal UI introduces a dependency on `crossterm` which may require platform‑specific terminal capabilities (e.g., ANSI support). This is acceptable for the target audience.
- Future UI extensions must respect the `EventBus` contract; the CLI will not directly call core logic.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*

