# ADR-77: Studio configuration workspace

**Status:** Proposed

**Date:** 2026-10-02

## Context

The Studio gives advisory blueprint stage cards the visual authority of an
execution pipeline. The session quick panel duplicates roster/model controls
and compresses the editor. Single-line prompt inputs obscure long instructions.
ADR-71 and ADR-74 place dispatch authority with the Coordinator; ADR-59 keeps
the global roster and blueprint in the existing merge-aware save path.

## Decision

Make Agents the default workspace, with a searchable roster and a focused
inspector. Put blueprint CRUD in a separate, explicitly advisory workspace.
Provide a configuration guide describing ownership and linking to Settings.
Hide the session quick panel while in Studio without changing its saved state.

Add multiline prompt editing, agent duplication, name/role editing, explicit
removal confirmation, and truthful display of custom lifecycle/model values.
Keep the existing global save and validation seams. Cloning creates a distinct
identity and does not copy relationship or blueprint staffing references.
Deleting an agent removes its staffing and assignment references in the draft.

Agent capabilities are requested tool access, not policy grants. Studio does
not bypass shell profiles, approval rules, the supervisor write gate, or the
Coordinator's authority. Blueprint order and relationship edits are advisory.
No new scheduler, policy engine, or runtime configuration schema is introduced.

## Verification

Desktop state tests cover prompt synchronization, clone identity/model pins,
removal confirmation/reference cleanup, and custom value preservation.
Run formatting, desktop Clippy, and the desktop test suite before pushing.
Native visual verification remains necessary at 1366x768 and narrow windows.
