# ADR-62: Tool Execution Pipeline — `ToolExecutor`, Policy Gates, and `VirtualFs` Staging

**Status:** Accepted
**Date:** 2026-08-19
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

### 4. `VirtualFs` stages every write

Filesystem mutations from agents land in the `VirtualFs` overlay first:

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
- Per-hunk review keeps large agent edits inspectable; rejected hunks never
  touch disk.
- The audit trail records what *actually* ran, enabling post-hoc forensics.
- Costs: one indirection on every tool call; overlay memory proportional to
  pending changes (bounded by snapshot/commit cadence); policy usability
  depends on sensible starter rules (tracked in live-testing follow-ups).

## Alternatives Considered

- **Direct writes with post-hoc audit:** rejected — review-before-disk is the
  core safety property; undo after damage is not equivalent.
- **Per-tool ad-hoc permission checks inside each tool implementation:**
  rejected — scattered enforcement invites bypass; the executor is the single
  chokepoint (and ADR-60 preserves exactly this property when tools move
  behind the supervisor gate).
- **OS sandbox instead of `VirtualFs`:** complementary, not alternative —
  container isolation remains explicitly deferred; `VirtualFs` provides review
  semantics no sandbox supplies.
- **Allow-by-default with deny rules only:** rejected — silent mutation of
  user projects must require affirmative configuration.

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
(retrospective consolidation — see [README](./README.md)).*
