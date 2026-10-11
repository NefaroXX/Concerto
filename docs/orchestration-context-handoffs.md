# Specialist context and handoff integration

This orchestration slice starts from `dev` at
`da81c0c11f1c1e4c1c03dab040ae685e9826ea23`, after the harness failure,
verification-outcome, environment-manifest and chunked-file-checkpoint work.

## Behavior

- Handoffs retain the latest settlement per task and prioritize the current
  task's continuation, its parent, dependencies in declared order, then recent
  siblings. A long dependency list cannot evict the task's own repair history.
- Prompt projections include the objective, deliverable, run requirement and
  completion-reporting expectations. Run requirements remain Coordinator
  context; they are not new per-specialist execution gates.
- Each handoff separates returned outcome, unfinished execution, artifact
  production and verification attribution. No handoff summary attests passing
  verification. Superseded settlements and omitted task results are counted
  separately. Resource paths are either complete or explicitly omitted.
- Optional context has a 64 KiB UTF-8 wire-text budget, independent of the
  existing provider token guard. Handoffs are assembled first. Section ceilings
  are 32 KiB for handoffs, 8 KiB each for run memory, capsule, environment and
  retrieved memory, 4 KiB for the snapshot, and 24 KiB for changed-file context.
  Required instructions, skills, task text, constraints and output contracts
  remain outside this optional-context budget.
- Oversized sections become labeled JSON excerpts; exhausted sections are
  explicitly omitted in the context-budget report. The report itself is small
  metadata outside the data budget. This is a byte bound, not an exact model
  token allocation or a configurable provider-window budget.
- Review prompts read at most 16 listed file entries, deduplicate resolved
  paths, bound file reads, and identify the source task and blake3 hash of each
  displayed excerpt. A full-file flag applies only when all observed file bytes
  are shown. Omitted and unavailable entries remain visible in the scope report.
  These observations identify current bytes; they are not diff reviews or
  verification evidence, and later edits can make them stale.

## Ownership and follow-up

Codex owns `agents/context_packet.rs`, `agents/task_contract.rs` and
`agents/generic/prompt.rs` for this slice. The only module-registration change
is in `agents/mod.rs`. Shared core types, configuration, sessions, executor/IPC,
runtime construction, CLI resume and Studio files are untouched.

OpenCode can continue harness schemas and services independently. Coordinate
before changing these prompt consumers. The existing environment-manifest block
is reused, not recollected, and the existing private execution continuation and
checkpoint reader retain their wire format. Handoff schema version 2 is a prompt
projection only; it is not a new durable result store.

Further work still needs canonical artifact-bound verification in multi-agent
acceptance, durable specialist execution state, preserved-question handling and
Studio/CLI evidence visibility. These context changes do not claim to complete
those integrations or to enforce natural-language completion criteria.

## Validation

Regression coverage includes continuation priority under dependency pressure,
latest-result selection, aggregate and escaped multibyte wire-size bounds,
complete resource-path preservation, explicit omissions, required instruction
retention, bounded review reads, excerpt fingerprints, deduplication and
unavailable-file scope reporting. Rust compile, formatter, clippy and workspace
regressions are checked by repository CI; the editing environment has no Rust
toolchain.
