# ADR-08: Desktop UI — `iced`, No Tauri Fallback

**Status:** Accepted — final, per Guiding Principle 5
**Date:** 2025-07-13
**Deciders:** Concerto architecture

## Context

This project's core differentiator list includes "Native Rust + Iced
desktop — pure Rust end to end; no Electron, no Tauri, no JavaScript
runtime." That's a constraint on the *kind* of project this is, not just a
UI-framework preference: a Tauri/web-frontend fallback would mean
maintaining a JS/TS frontend alongside the Rust backend, splitting the
plugin ABI (Phase 7) and the event/observability model (Phase 0, Phase 8)
across two language ecosystems, and reintroducing exactly the dependency
surface (Node toolchain, npm supply chain, a webview runtime) the project
exists to avoid.

This ADR exists specifically so this decision is not re-opened mid-project
when Iced's widget development (custom diff viewer, agent graph view, etc.)
turns out to be more time-intensive than a web-based UI library would be.
The Risk Register already prices that in: "Iced widget development slower
than expected for Phase 6 — High likelihood, Medium impact — mitigation:
budget 2× time estimate, ship minimal-but-correct UI in Phase 3, polish
incrementally in Phase 6."

## Decision

`iced` is the only desktop UI framework. No Tauri fallback, no Electron, no
web frontend, ever, without a new ADR that explicitly supersedes this one.

## Consequences

- Single-binary distribution stays simple — no embedded webview runtime, no
  Node toolchain in the build pipeline, no separate frontend dependency
  lockfile to audit alongside `Cargo.lock`.
- Custom widgets (diff viewer, agent graph, markdown rendering) must be
  hand-built in Iced rather than reached for off-the-shelf from a mature web
  component ecosystem. Phase 3 ships functional-but-plain versions
  deliberately, with polish deferred to Phase 6, specifically to absorb this
  cost without blocking the MVP.
- The CLI (`ratatui`) remains a fully independent, equally first-class
  interface — not a fallback for when Iced is "too slow," but a parallel
  surface over the same `api-types` contract.
- If Iced widget development proves genuinely intractable for a specific
  view (not just slower than hoped), the correct response is a narrower ADR
  scoping an exception for that one view — not a quiet drift back toward a
  web frontend.

## Alternatives Considered

- **Tauri (Rust backend + web frontend):** faster custom-widget development
  via the web ecosystem, but reintroduces a JS/TS surface this project is
  explicitly defined against, and splits the plugin ABI and event model
  across two runtimes.
- **egui:** immediate-mode, also pure Rust, simpler mental model for some
  widget types — not chosen because Iced's Elm-style architecture maps more
  directly onto this project's already-event-driven core (`EventBus`,
  `Event`/`EventKind`), reducing the translation layer between backend
  events and UI state updates.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
