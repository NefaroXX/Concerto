# ADR-41: Spend surfaces in the status bar; no Dashboard page

**Status:** Accepted (2026-08-03)
**Date:** 2026-08-03
**Deciders:** Concerto architecture

## Context

The Phase 6 Dashboard page duplicated what Chat already offered and
misrepresented what the runtime actually produced:

- **Redundant with Chat.** The Dashboard's recent-sessions list repeated the
  session list already available in Chat's empty state, and its cost summary
  was the only page-scoped surface for spend.
- **Live cost display was dead.** The status-bar cost readout showed $0.000 in
  production runs because `SpendUpdated` was defined in the event taxonomy but
  never emitted by the orchestrator. The UI had no live spend signal to render.
- **Per-call spend records were never written.** The schema already had a
  per-call spend record table, but the runtime never persisted a row per
  provider call, so no listable spend history existed.

## Decision

1. **Remove the Dashboard page.** `Page::Dashboard` is gone; the desktop no
   longer routes to it. Chat owns the recent-sessions list (rendered in its
   empty state), and spend surfaces move into the status bar and an overlay.
2. **Live session spend in the status-bar chip.** The chip shows the current
   session's cost and is palette-colored against the session cap: warning at
   ≥80% of the cap, danger at ≥100%.
3. **Spend Log modal.** A Spend Log modal reuses the existing SubView overlay
   pattern (the same mechanism as Diff / Agent Graph / Tool Log) and lists the
   persisted per-call spend records.
4. **Runtime publishes spend events.** The orchestrator emits the existing
   `SpendUpdated` and cap events after each settled provider call, giving the
   status bar a real live signal.
5. **Runtime persists spend records.** One `SpendRecord` is written per
   settled provider call. Multi-agent spend records carry the root task id.
   Failed runs record no spend record (spend is settled only on success).
6. **Daily-total output is stubbed.** The daily-total field is present but
   always `None` until daily tracking is enabled; it is a placeholder, not a
   feature.

## Consequences

- CSV export is dropped from the desktop UI; the CLI remains the file-export
  path (per `desktop-cli-parity.md`, file export stays CLI shell piping).
- The Ctrl+Shift+S shortcut is removed along with the page.
- Multi-agent spend records carry the root task id, so per-call history stays
  attributable to the originating run.
- Failed runs record no spend record; only settled provider calls appear in
  the Spend Log.
- The spend backend (`SpendTracker`, `SpendRecord`, policy checks, session
  caps) is unchanged — this ADR only moves the surfaces and wires the events
  and persistence that already existed in the taxonomy and schema.
