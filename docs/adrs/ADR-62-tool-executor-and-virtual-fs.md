# ADR-62: Tool Execution Pipeline — `ToolExecutor`, Policy Gates, and `VirtualFs` Staging

**Status:** Accepted — **revised in place 2026-10-09** (amendment: the
agent-path `VirtualFs` staging claims in §4 and the Consequences are corrected
for the `filesystem` tool; see the dated **Amendment (2026-10-09)** before
References. The executor-chokepoint, deny-by-default, audit-trail, and
path-confinement decisions are unchanged.)
**Date:** 2026-08-19 (original), 2026-10-09 (amendment)
**Deciders:** Concerto architecture
**Related crates:** `concerto-core`, `concerto-tools`, `concerto-config`, `concerto-sessions`
**Supersedes:** nothing — codifies the tool/policy/filesystem layer that every
other safety ADR (44, 50, 52, 55, 60) assumes.

## Context

Every mutation an agent performs — file writes, shell commands, git
operations, MCP server tools, plugin tools — must cross one auditable,
policy-gated boundary. There is no second path. The same boundary must make
changes reviewable before they reach disk, reversible after the fact, and
explainable in the audit log.

Three cooperating pieces provide this:

1. **`ToolExecutor`** (`crates/core/src/executor.rs`) — the single execution
   chokepoint for registered tools.
2. **`SimplePolicyEngine`** (`crates/core/src/policy.rs`) — first-match-wins
   rule evaluation over typed policy actions.
3. **`VirtualFs`** (`crates/tools/src/virtual_fs.rs`) — an overlay filesystem
   that stages changes for review and supplies diffs.

Requirements:

- Deny-by-default: an unmatched action is denied, never allowed.
- Human confirmation is a real decision channel with audit records.
- Filesystem effects are reviewable per-hunk and reversible.
- The pipeline works identically for single-agent, multi-agent, plugin, and
  MCP-originated calls.

## Decision

### 1. One executor, one gate

`ToolExecutor::execute` looks up the requested tool in the registry, builds a
typed `PolicyAction`, evaluates it through the configured
`SimplePolicyEngine`, enforces spend constraints via the shared
`SpendTracker`, requests approval when the verdict is `RequireApproval`, emits
lifecycle events around execution, and only then runs the tool. Every caller
— agent loop, coordinator specialists, plugins (via host functions), MCP
tools (via the `McpTool` bridge), and eval scenarios — goes through this one
method.

The desktop/CLI coding registry contains the `filesystem` and `shell` tools;
git operations ride the shell/git tooling; LSP tools register alongside them;
MCP and plugin tools register into the same registry at runtime.

### 2. First-match policy rules; deny unmatched

`SimplePolicyEngine` evaluates configured rules in order; the first match
wins; **unmatched actions are denied**. Rule conditions cover tool name
(exact and prefix/glob for namespaced `mcp:*` tools), command patterns,
resolved executables, argv patterns, working directories, URL hosts, and
spend/RPM budgets (structured shell facts per ADR-28/30). Verdicts are
`Allow`, `Deny`, or `RequireApproval`.

Presets ship with the engine:

- the **safe default preset** allows project file reads/listing/existence
  checks, requires approval for filesystem mutation, shell execution, git
  operations, and any unmatched tool, and hard-denies known destructive shell
  patterns (ADR-32);
- the desktop installs an explicit allow-all rule for expert/no-rules mode so
  an empty configuration remains functional — but the shell tool still applies
  its own independent hard denylist before any allow-all configuration.

**Deny is final.** No later mechanism — intent grants (ADR-55), session
approvals, or future policy layers — can upgrade a `Deny`.

### 3. Approval is a decision record

`RequireApproval` verdicts route to an `ApprovalSink`. Desktop dialogs offer
*Allow once / Allow this tool for the session / Deny* with the exact path,
command, or operation displayed (ADR-32). Every resolution writes an audit row
(`record_approval_decision`) sharing the `correlation_id`/`input_hash` chain
with the preceding verdict row, so "what was asked, what was answered" is
always reconstructable. Ack-style prompts (`request_ack`) use the same audited
channel.

### 4. `VirtualFs` staging — review gate for the human review chains, audit/diff record for agent writes

`VirtualFs` is the overlay the review surfaces (desktop diff view, desktop
code-editor staged review, CLI diff review) operate on. **Agent** `filesystem`
tool writes/deletes do **not** land in the overlay first: they materialize to
disk synchronously inside `execute` and update the overlay afterward — for
them the overlay is a **post-disk audit/diff record**, not a pre-disk gate
(see **Amendment (2026-10-09)**). The staged-overlay capabilities below
describe the overlay itself:

- writes create pending entries; reads resolve through the overlay onto disk;
- snapshots capture pre-review state; diffs are computed once by
  `imara-diff` into a shared `DiffResult` consumed by both frontends
  (ADR-05);
- hunk-level accept/reject decisions accumulate in the UI and apply to
  `VirtualFs` atomically on commit; diff review restores a stable pre-review
  snapshot, applies all rejected hunks in one pass, then materializes the
  reviewed result to disk (ADR-33);
- session undo/snapshot support uses git infrastructure where a repository is
  configured;
- all paths are confined to the session/project root via canonicalizing
  path resolution (`resolve_path`); traversal outside the root is rejected.

`VirtualFs` is deliberately a staging/review layer, **not** an OS sandbox or a
backup system (see `SECURITY_BOUNDARIES.md`).

### 5. Tool input contracts are schema-derived where possible

Tools advertise their input contract as JSON Schema derived from Rust structs
via `schemars` (ADR-25) so the advertised schema and the deserialization
target cannot drift; requiredness follows the struct. At the boundary,
deserialization applies a narrow, well-defined lenient coercion set plus the
binary-safe read contract — non-UTF-8 reads return an informative placeholder
instead of failing the call (ADR-50).

### 6. Everything observable

Execution start/finish/timeouts publish typed `EventKind` variants on the
EventBus (ADR-65); policy decisions and approvals append to the append-only
audit log (ADR-40/64) including structured shell facts (resolved executable,
argv, working directory, exit code, duration); tool output surfaces in the
desktop Tool Log and CLI transcript.

## Consequences

- A compromised or buggy agent cannot bypass policy structurally: there is no
  production code path that executes a registered tool without the executor.
- Read-only specialists are enforced by constructing their registries with
  read-only capability sets — write tools are absent, not merely discouraged
  (ADR-19).
- Per-hunk review keeps large **staged** edits inspectable; rejected hunks
  never touch disk **for content that entered the overlay** (the human review
  path). Agent `filesystem` writes/deletes are materialized to disk at
  `execute` time, so hunk rejection cannot gate them — see
  **Amendment (2026-10-09)**.
- The audit trail records what *actually* ran, enabling post-hoc forensics.
- Costs: one indirection on every tool call; overlay memory proportional to
  pending changes (bounded by snapshot/commit cadence); policy usability
  depends on sensible starter rules (tracked in live-testing follow-ups).

## Alternatives Considered

- **Direct writes with post-hoc audit:** rejected — review-before-disk is the
  core safety property for the human overlay path; undo after damage is not
  equivalent. (The agent `filesystem` path is, in practice, direct-write with
  post-hoc audit — the correction recorded in **Amendment (2026-10-09)**; the
  review-before-disk property holds for content that enters the overlay, not
  for the agent tool's own write.)
- **Per-tool ad-hoc permission checks inside each tool implementation:**
  rejected — scattered enforcement invites bypass; the executor is the single
  chokepoint (and ADR-60 preserves exactly this property when tools move
  behind the supervisor gate).
- **OS sandbox instead of `VirtualFs`:** complementary, not alternative —
  container isolation remains explicitly deferred; `VirtualFs` provides review
  semantics no sandbox supplies.
- **Allow-by-default with deny rules only:** rejected — silent mutation of
  user projects must require affirmative configuration.

## Amendment (2026-10-09) — agent `filesystem` writes are not staged before disk; the overlay is their post-disk audit/diff record

Scope: this amendment corrects the `VirtualFs` staging claims in §4 and the
Consequences for the **agent `filesystem` tool path**. It does **not** reopen
the executor/policy decision (§§1–3, 5–6 remain in force), and it leaves the
path-confinement property (the §4 bullet on `resolve_path` and traversal
rejection) and the "not an OS sandbox or a backup system" caveat (§4) intact —
both are accurate. The Context requirement "reviewable before they reach disk,
reversible after the fact" is met by the human review chain; for agent writes
it holds only in the weaker sense of post-hoc audit/diff records, as
corrected below.

### Corrected claim 1 — agent writes do not land in the overlay first

**Original (§4):** *"Filesystem mutations from agents land in the `VirtualFs`
overlay first:"*

**Actual behavior.** The `filesystem` tool's `write` operation calls
`std::fs::write(path.as_std_path(), content)` (`crates/tools/src/filesystem.rs:450`)
and only then updates the shared overlay via `vfs.write(&path, ...)`
(`filesystem.rs:458`); the `ToolOutput` reports `"materialized": true`
(`filesystem.rs:466`). Deletion is synchronous inside the same `execute`:
the overlay is staged (`filesystem.rs:475`) and the disk file is removed
when it exists (`filesystem.rs:477-483`), with the same `"materialized": true`
flag (`filesystem.rs:490`). Neither operation leaves a reviewable pre-disk
window in `execute`: for `write` the overlay update follows the disk write;
for `delete` the overlay is staged and the on-disk file is then removed in the
same call. The overlay is therefore an **audit and diff record** of agent
writes — it can
diff them later (`compute_diffs_from_virtual_fs`,
`crates/tools/src/diff.rs:321`) and retains the pre-image of `Modified`
entries (`crates/tools/src/virtual_fs.rs:88-90`) — but it is **not a pre-disk
gate** for the agent path.

Separate mechanism, not contradicted: the orchestrator write gate (ADR-60)
captures a pre-image hash and appends a `write-applied` WAL entry **before**
the tool runs, and refuses denied/approval-required writes before execution
(`crates/orchestrator/src/gate.rs:14-23`). That is a durability
and attribution record (plus policy denial before execution); it is not
overlay staging and not an automatic rollback, and this amendment does not
speak against it.

### Corrected claim 2 — "rejected hunks never touch disk" holds only for staged content

**Original (Consequences):** *"rejected hunks never touch disk."*

**Actual behavior.** Hunk rejection is an overlay operation
(`VirtualFs::reject_hunks`, `crates/tools/src/virtual_fs.rs:636`), and it
affects disk only through `materialize_paths` (`virtual_fs.rs:698`, disk
writes/removals at `:712-734`). The claim holds **only** where hunks exist to
reject — content staged in the overlay and reviewed before materialization,
i.e. the human review path. It does **not** hold for agent `filesystem`
writes/deletes, which were never staged before disk: at the moment the disk
effect occurred there was no hunk to reject.

### The review-before-disk path that does exist (human/editor review chains)

`VirtualFs::restore` (`crates/tools/src/virtual_fs.rs:743-746`) swaps the
in-memory entries map and performs **no disk I/O**; disk effects are
concentrated in `materialize_paths` (`virtual_fs.rs:712-734`). The
restore → `reject_hunks` → `materialize_paths` chain is real and is the
review surface for staged content:

- desktop diff view: `crates/desktop/src/views/diff.rs:107-119` — `restore`
  at `:107`, `reject_hunks` at `:117`, `materialize_paths` at `:119`;
- desktop code-editor staged-file review: `decide_staged`
  (`crates/desktop/src/views/code_editor/workspace.rs:349-377`;
  `materialize_paths` on accept at `:374`), with the editor's own buffer
  undo/redo restoring text state only (`editor_core.rs:347,359`);
- CLI staged review: `crates/cli/src/app.rs:1829` (self-described "Same guard
  chain, same messages, same `materialize_paths`/`unstage`"), `materialize_paths`
  at `app.rs:1856`; reject only unstages — "the on-disk state is authoritative"
  (`app.rs:1835-1836`).

Caveat in the same split: a plain desktop-editor **Save** writes to disk
directly (`workspace.rs:171`, background `:218`) and clears the overlay entry
(`workspace.rs:178,227`); new-file creation writes directly too
(`editor_core.rs:209,227`). So even "human/editor-originated edits" are not
universally staged before disk: the staged-review chains govern content that
entered the overlay; a bare Save bypasses the overlay entirely.

### No automatic undo/rollback pipeline exists

`Tool::rollback_support()` defaults to `false`
(`crates/core/src/traits/tool.rs:85-87`) and `Tool::rollback()` defaults to
`Err(ToolError::RollbackNotSupported)` (`tool.rs:97-103`; variant defined at
`crates/core/src/error.rs:446-451`). `ToolExecutor`
(`crates/core/src/executor.rs`) has **zero** call sites for either method, and
`RollbackSnapshot` (`crates/core/src/types.rs:294-298`) is never constructed
anywhere in the tree. No reader should infer a tool-level rollback is
available: it is a declared but unwired seam.

`UndoManager`-based session undo exists and uses git infrastructure
(`crates/tools/src/undo.rs:56-57`), but the stash command is
`git stash push -m <msg>` **without** `-u`/`--include-untracked`
(`undo.rs:57`), so untracked files are not captured by a stash-based restore.
`session undo` support must not be read as a complete-workspace restore.

### Forward intent — open gap, no design committed

Durable file checkpoints and an executor-seam pre-image/rollback dispatch are
planned as part of the harness-upgrade work (tracking items H01 contract
schemas, H06 durable checkpoints, H07 executor-seam pre-image/rollback
dispatch). This amendment records the gap so that work starts from the
corrected premise above (the H00 audit finding that prompted this amendment);
it deliberately specifies **no** schemas or interfaces — that is a separate
ADR. Until then, agent file effects are recoverable in practice only through
mechanisms outside this ADR's scope — git history/stashes for tracked files,
and the overlay's retained pre-images while entries remain staged for review —
never through an automatic rollback pipeline.

## References

- Executor and policy: `crates/core/src/executor.rs`,
  `crates/core/src/policy.rs`
- Overlay filesystem and shell tool: `crates/tools/src/virtual_fs.rs`,
  `crates/tools/src/shell.rs`
- Audit persistence: `crates/sessions/src/lib.rs` (audit tables; ADR-40)
- Related: ADR-05 (diff computation), ADR-25 (schema-derived inputs),
  ADR-28→30 (shell facts), ADR-32 (approval UX and safe defaults),
  ADR-40 (append-only audit), ADR-43 (MCP tools under the executor),
  ADR-44 (project-root confinement and consent),
  ADR-50 (coercion/binary-read contract), ADR-55 (intent-gated
  authorization), ADR-62's sibling gate relocation in ADR-60

---

*Decision codified from inception 2025-07-10; document stabilized 2026-08-19
(retrospective consolidation — see [README](./README.md)). Amended in place
2026-10-09: agent `filesystem` writes materialize to disk at `execute` time
and the overlay is their post-disk audit/diff record, not a pre-disk gate
(VirtualFs staging claims corrected — H00 harness-upgrade audit follow-up).*
