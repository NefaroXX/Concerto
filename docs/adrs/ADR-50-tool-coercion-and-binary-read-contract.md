# ADR-50: Tool coercion + binary read contract

**Status:** Accepted (2026-08-08) — implemented in commit `c43e6d8`
    ("lenient tool-arg coercion and binary-safe reads"), **Phase 1** of the
    provider-first redesign plan (tool resilience, fix classes 2&3).
**Date:** 2026-08-08
**Deciders:** Concerto architecture
**Supersedes:** Phase 1 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`) — the tool-resilience items (failure classes
    2 and 3) this ADR finalizes.
**Composes with:** ADR-42/45 and ADR-52 (fallback ladder / orchestration
    safety gates) — tool failures stay *recoverable* context for the ladder to
    act on, and the coercion/read contract changes what a tool returns, not
    how the ladder retries. ADR-43 (MCP tool policy-gating) unchanged.

## Context

Two of the four recorded run failures in the redesign were tool-boundary
errors, not provider errors:

- **Failure class 2 (wrong-typed tool args).** `git.rs` deserialized `GitInput`
  straight from JSON: `invalid type: string "5", expected u32` when the model
  sent a string for an `integer` schema field. The strict parser was the
  contract at `Tool::execute` entry.
- **Failure class 3 (binary read hygiene).** `virtual_fs.rs` used
  `read_to_string`, so reading a binary file produced
  `Tool filesystem failed: stream did not contain valid UTF-8` — a hard error
  that polluted context and killed the turn.

Both are salvageable at the execution boundary without touching the provider
or the loop.

## Decision

Adopt a **lenient-at-the-boundary, strict-internally** tool contract plus an
**informative, never-failing** binary read:

1. **Arg coercion at `Tool::execute` entry.** Git and filesystem tools
   attempt strict deserialization first; on failure they run a
   **well-defined coercion** step instead of erroring:
   - number-vs-string (`"5"` → `5` for integer fields; `max_count`
     accepts a numeric string),
   - string→string-array for `paths` (split on whitespace / newline / comma),
   - scalar→string for string fields.
   Only fields with a defined lenient coercion are touched; objects and
   malformed values are never silently coerced away. Coercion is silent at
   today's boundary parser; a model-facing warn-note and the plan's §5 config
   switch (`tools: { git: { "coerce": true }, ... }`) are the documented future
   surface, not part of this ADR's shipped behavior.
2. **Binary/read hygiene.** `virtual_fs.rs` reads raw bytes and decodes as
   UTF-8: valid text returns verbatim; non-UTF-8 content returns an
   informative placeholder —
   `[binary file: N bytes — contents not decoded]` (the plan's "binary file
   (N bytes)" contract) — **never a hard error**, so reading or staging a
   binary file cannot fail a tool call. Raw bytes are deliberately omitted.
3. **Strict wins everywhere else.** Coercion applies only at the tool
   boundary parser, not to the schema, output, or audit path; the strict
   contract is preserved for everything not in the targeted coercion set
   (git tool args, filesystem read params).

## Consequences

- **Positive.** Failure classes 2 and 3 stop killing turns: type-mismatched
  args and binary-file reads become handled values instead of hard errors. No
  provider, loop, or persistence path changed. A string-typed `"5"` no longer
  aborts a git call; a binary file no longer aborts a filesystem call.
- **Negative.** Coercion is heuristic — a string like `"5"` being turned into
  `5` can mask a genuinely wrong argument; the scope is deliberately narrow
  (numbered fields, path lists, scalars), and today's coercion is silent at
  the boundary (no model-facing note yet — the plan's §5 warn-note/config
  surface remains future work). Binary files return a placeholder rather than
  the bytes; reading binary as text is not supported (schema-level
  `read_binary` knob remains a future config option, §5).
- **Risk.** Over-broad coercion could let a bad tool call pass when it should
  fail loudly — mitigated by only coercing fields with well-defined
  semantics, and by tests exercising each coercion and its rejection path.
- **Migration.** None. Both changes are within the tools crates; no config
  removal, no schema change, no persistence change.

## Review notes

- These are the failure-class 2 and 3 fixes from §0 of the plan; the exit gate
  is the per-tool coercion + binary-read tests (`coerce_git_input`,
  `coerce_filesystem_input`, `VirtualFs` read placeholder) landed with
  `c43e6d8`.