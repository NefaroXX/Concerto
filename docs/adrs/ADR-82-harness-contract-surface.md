# ADR-82: Harness Contract Surface — Typed Failures, Verification Outcomes, Environment Manifest, Checkpoints, and Memory Evidence

**Status:** Proposed — revised 2026-10-10 to address review items R1–R6 and two
small corrections (context Q1 and Confidence validation); still awaiting
acceptance. This is the H01 scoping decision of the harness-upgrade effort:
**extend the existing type surface rather than introduce five parallel
greenfield contracts**, because the H00 audit found most of the required
infrastructure already exists. It records design only; no source change lands
with it. The `#[deprecated]`, sidecar-table, migration-037, and `[agent]`
config items below are **planned**, not implemented.

**Date:** 2026-10-09 (revised 2026-10-10)

**Deciders:** Concerto architecture

**Related crates:** `concerto-core` (vocabularies), `concerto-orchestrator`
(collector, gate, single-agent loop), `concerto-sessions` (sidecar table and
retention), `concerto-shell` (envelope conversion), `concerto-tools`
(checkpoint side, write gate), `concerto-config` (`[agent]` key)

**Supersedes:** nothing. **Extends** ADR-62 (the executor boundary and the
corrected `VirtualFs` staging facts), ADR-65 (the evidence spine — facts,
`resource_facts`, supersession), ADR-71 (Coordinator supremacy — conformance
stated in its own section below), and ADR-73 (audit encryption and retention).
**Preserves** ADR-60: restoring a checkpoint is an authorized mutation (the
supervisor/gate contract), never a bypass.

## Context

Five questions every agent currently cannot answer reliably:

1. **What can I do, and what persists across a run?** There is **no unified
   environment declaration**: no single bounded call exposes the effective
   tool set, execution backend, sandbox profile, project root, or which
   effects survive. Some of these facts are exposed piecemeal today — the
   registry, health surfaces, the snapshot digest — but never as one bounded
   declaration.
2. **What will this change, and what is undoable?** There is no durable
   pre-image record and no automatic rollback pipeline.
3. **If it fails, what precisely failed?** Failures are flattened to prose at
   more than one boundary.
4. **What evidence establishes that the outcome worked?** Verification can
   record a skip as a pass, or record nothing at all.
5. **What prior knowledge can I trust?** Memory evidence carries no separated
   truth/relevance/confidence/freshness axes.

The H00 audit established these are genuinely unanswerable today, in specific
cited ways — not merely under-documented:

- **A skip is recorded as a pass.** When a `.js`/`.ts` file's `package.json`
  exists but has no `"test"` script, the loop pushes a `VerificationSummary`
  with `passed: true` and the output `"skipped (no test script in
  package.json)"` (`crates/orchestrator/src/agent_loop.rs:3155-3161`).
- **Some verifications record nothing at all.** The `"rs"` arm only records when
  `Cargo.toml` exists (`agent_loop.rs:3080-3082`, no `else`) and the
  `"js"|"ts"|"jsx"|"tsx"` arm only records when `package.json` exists
  (`agent_loop.rs:3117-3119`, no `else`): a `.rs` without `Cargo.toml` or a
  `.js` without `package.json` yields **no `VerificationSummary` record**
  (`agent_loop.rs:3080-3163`).
- **Evaluation errors collapse to `None`.** `run_evaluation` returns
  `self.eval.run_scoped_in_dir(...).await.ok()` (`agent_loop.rs:1632-1639`)
  — a harness error becomes an absent value, indistinguishable from "not run".
- **Model-boundary errors are flattened to prose.** A tool execution failure is
  rendered into a `"TOOL_RESULT … status: error"` string and a
  `serde_json::json!({ "error": "execution_failed", "message": e.to_string() })`
  payload (`agent_loop.rs:2820-2831`) — the typed error is discarded at the
  boundary.
- **Retry classification parses prose.** `shell_repair.rs:176-193` classifies
  failure cause by substring markers (`"gatedenied"`, `"requireapproval"`,
  `"policydenied"`, `"blocked"`, `"denied"`, `"cancelled"`, plus
  "not found"/"no such file" heuristics) rather than a typed code.
- **Restore is not a durable byte restore.** `VirtualFs::restore`
  (`crates/tools/src/virtual_fs.rs:743-746`) swaps the in-memory entries map
  only. Git-stash undo issues `git stash push -m <msg>` with **no**
  `-u`/`--include-untracked` (`crates/tools/src/undo.rs:56-57`).
- **Rollback is a declared but unwired seam.** `RollbackSnapshot` is defined
  (`crates/core/src/types.rs:293-297`) but never constructed;
  `Tool::rollback_support()` defaults to `false`
  (`crates/core/src/traits/tool.rs:85-103`) and `Tool::rollback()` defaults to
  `Err(ToolError::RollbackNotSupported)`, with **zero overrides and zero call
  sites** in the tree.

The scoping decision is therefore: extend what exists, add the missing typed
fields, and fix one live accounting defect — do not fork five parallel
contracts.

## Decision

### 0. Extend, don't fork

The five contracts below reuse existing types wherever the type already exists
and only add what is missing. In particular `ToolFailure` does **not** fork the
shell envelope, `VerificationResult` is an extension of `VerificationSummary`,
and `MemoryEvidence` is a read-time projection, not a stored record.

### 1. `ToolFailure` — typed failures replace prose classification

The failure vocabulary lives in `concerto-core`: `ToolFailure`, `ToolStatus`,
`ToolDiagnostic`, a code grammar, a **recovery-disposition vocabulary**, and a
`schema_version`. It does **not** fork the shell envelope:
`concerto_shell::CommandStatus` (`crates/shell/src/model.rs:11`),
`concerto_shell::Diagnostic` (`model.rs:47`), and
`concerto_shell::CommandResult` (`model.rs:126`) already exist with stable
codes. The conversion `CommandResult::to_tool_failure()` lives **in the shell
crate** (planned — it does not exist in the tree today), because only a crate
that depends on both core and shell may name both (see the layering rule
below). A `ToolFailure` records:

- `code`, `category`, `disposition`;
- bounded `detail` (4 KiB cap);
- `resource` (`path` | `pointer`);
- `valid_values`;
- sanitized `message`;
- `opaque: bool`;
- `origin`, `correlation_id`, `evidence_id`.

**Typed codes are the derivation source — prose classification is the
fallback, never the other way around.** Native failures map through an
explicit, **exhaustive `match` over `ToolError` variants** — the enum is
`#[non_exhaustive]` (`crates/core/src/error.rs:362-364`) with the variants at
`:364-470` — to typed codes and recovery dispositions. The unknown-variant arm
falls back to `Opaque`. The mapping:

| `ToolError` variant | `code` | `category` | `disposition` |
|---|---|---|---|
| `PolicyDenied { rule }` | `policy_denied` | policy | `do-not-re-issue` |
| `PausedAwaitingApproval { .. }` | `awaiting_approval` | policy | `awaiting-decision` |
| `ExecutionFailed { message }` | `execution_failed` | execution | prose payload → routed through the compatibility predicate below |
| `NotARepository { .. }` | `not_a_repository` | environment | `do-not-re-issue` |
| `Timeout { .. }` | `timeout` | execution | `bounded-retry-with-backoff` |
| `Cancelled` | `cancelled` | control | `do-not-re-issue-same-intent` |
| `VirtualFsConflict { reason }` | `virtual_fs_conflict` | workspace | containment → `do-not-re-issue`; else `retryable-after-rebase` |
| `RollbackNotSupported` | `rollback_not_supported` | environment | `do-not-re-issue` |
| `LspError { .. }` | `lsp_error` | execution | `bounded-retry-with-backoff` |
| `Io(kind)` | `io_error` | execution | structural `ErrorKind`s (below) → `do-not-re-issue`; else `bounded-retry-with-backoff` |
| unknown variant (`#[non_exhaustive]`) | `opaque` | opaque | compatibility predicate (below) |

**The disposition vocabulary is not retryable/not-retryable.** It distinguishes
the cases that a boolean collapses:

- `do-not-re-issue` — deterministic: the identical action with identical input
  and identical workspace state cannot succeed (`PolicyDenied`,
  `NotARepository`, `RollbackNotSupported`, structural `Io` kinds —
  `NotFound`, `PermissionDenied`, `InvalidInput`, `AlreadyExists`,
  `NotADirectory`, `IsADirectory` — and containment-flagged
  `VirtualFsConflict`).
- `do-not-re-issue-same-intent` — **cancelled**: a deliberate stop; the agent
  must not silently re-issue the same intent, only a new user instruction may
  (distinct from a deterministic refusal).
- `awaiting-decision` — **approval**: the action is parked awaiting the
  operator; resolution is the answer, not a retry (`PausedAwaitingApproval`).
- `bounded-retry-with-backoff` — **timeout** and transient execution faults:
  may resolve on a later attempt, retried with backoff and a bound
  (`Timeout`, non-structural `Io`, `LspError`).
- `retryable-after-rebase` — a genuine overlay conflict (a concurrent edit) may
  resolve after the agent reconciles the workspace (`VirtualFsConflict` with a
  non-containment `reason`).
- `awaiting-decision` and `do-not-re-issue-same-intent` are neither
  "retryable" nor "deterministic"; they get their own values so a consumer
  never has to guess.

**The existing `is_deterministic_failure` predicate survives only as a
compatibility fallback for opaque failures** — unknown third-party, plugin,
and MCP error text, and the prose `ExecutionFailed { message }` /
`VirtualFsConflict { reason }` payloads that arrive without a typed code.
`is_deterministic_failure_message` (`crates/core/src/error.rs:539-542`) is
substring matching over `DETERMINISTIC_FAILURE_MARKERS` (`error.rs:485-522`)
and `DETERMINISTIC_CONTAINMENT_MARKERS` (`error.rs:529-530`); the full
predicate is at `error.rs:557-583`. It is **not** the derivation source for
native failures: the per-variant table above is. The fallback's residual use is
explicitly listed in the slice-2 verification below, and typed codes are the
retirement path for it. Slice 2 must demonstrate that the two agree where the
predicate is uncontroversial (the structural arms) and that opaque text never
masquerades as a native code.

### 2. `EnvironmentManifest` — descriptive, never an authorization token

A greenfield descriptive type in core, with the collector in the orchestrator.
It is collected lazily and cached per run with freshness stamps — **never
probed per prompt**. Fields: run identity; os/arch; execution backend; sandbox
profile; project root plus root hash; registered **and canonical** tool names;
executable observations; descriptive policy scope; persistence classes; restore
capabilities; `collected_at`; `revision`; freshness. It is persisted as an
additive `WhiteboardKind::EnvironmentObserved` (the enum is at
`crates/sessions/src/whiteboard.rs:48`; new kinds are additive kebab-case, per
the ADR-65 §1 precedent).

**Load-bearing invariant: the manifest is descriptive and is never an
authorization token.** No code path branches authorization on it; the policy
engine never reads it; and there is no conversion from the manifest into a
policy type. It answers the operator's "what can this run do" question; it does
not grant, deny, or widen anything (ADR-71 §1.1: the Coordinator owns the
decision, and a static description cannot become authority).

### 3. `VerificationResult` — an extension of `VerificationSummary`, with an outcome that is authoritative

`VerificationSummary` exists (`crates/core/src/types.rs:1307-1316`, with
`passed: bool`). `VerificationResult` **extends** it with
`outcome: Option<VerificationOutcome>`, where the outcome is one of
`Passed | Failed | Skipped | Unavailable | Error | Cancelled`. This is what
kills the real defects in the Context section: a missing npm `test` script no
longer records `passed: true` (`agent_loop.rs:3155-3161`), and
`.rs`-without-`Cargo.toml` / `.js`-without-`package.json` no longer produce
*no record at all* (`agent_loop.rs:3080-3163`). `EvalResult` (`types.rs:1431`)
remains as the harness payload nested in `detail`. Invalidation of a prior
result cites the existing `resource_facts` rows and the ADR-65 supersession
rules (`resource_facts.rs`; ADR-65's F-SUPERSEDE / F-WORKSPACE-SUPERSEDE)
rather than introducing a new invalidation mechanism.

**Legacy records (absent `outcome`) are display data, not trusted evidence.**
Historical rows already contain `passed: true` for skipped checks, and no
automatic reclassification can distinguish them — a deserialized legacy record
must **not** silently become passing evidence. The rule is stated plainly: **a
missing `outcome` never counts as passing evidence.** Such records deserialize
with `None` and are preserved for display in transcripts and audit trails, but
any consumer that treats a record as *verification evidence* (completion
gates, acceptance checks, the evidence spine) requires an `outcome`; a
`None`-outcome record is `Unavailable` for evidence purposes until a fresh
check records one.

**Where `outcome` exists, it is authoritative over `passed`** — the opposite
ordering would let a stale boolean override the new truth. Inconsistent
combinations are resolved by `outcome` **and surfaced, never silently
normalized**: both raw values are retained and an explicit
`inconsistent: bool` (set when `outcome` and `passed` disagree, e.g.
`outcome: Failed` with a legacy `passed: true`) is recorded so the mismatch is
visible to consumers and auditors instead of being collapsed away.

**Freshness basis.** A consumer may only treat a record as fresh passing
evidence when the full field set is present and jointly satisfies the check:
verifier identity (`verifier`), checked scope (`scope`), checked state
(`generation` **and** `content_fingerprint` — state is not freshness without
both), an **explicit check timestamp** (`checked_at`, added by this ADR), and
evidence references (`evidence_ids`). Any of these missing or stale degrades
the record to `Unavailable` for evidence purposes; the fields are named here
as a set so the consumer check is one bounded predicate, not an open-ended
probe.

### 4. `CheckpointRecord` — durable pre-image bytes, restore as an authorized mutation

A greenfield type in core; the durable pre-image bytes live in a
`concerto-sessions` sidecar table (migration **037**, the next free number —
`crates/sessions/migrations/` currently tops out at
`036_tasks_execution_mode.sql`). It reuses the existing pre-image hashing at
the write gate: `stamp_base_versions` reads the pre-image through
`gate.pre_image.read` and hashes it with blake3
(`crates/orchestrator/src/gate.rs:1705`). **Restore is itself an authorized,
policy-gated, ownership-checked, idempotent mutation** — it goes through the
same executor/gate boundary as any other write (ADR-60, ADR-62), never around
it. `excluded` explicitly covers DB / network / process classes; these effects
are **not** reversed, and a file checkpoint makes no such claim.
`RollbackSnapshot` (`crates/core/src/types.rs:293-297`) and
`Tool::rollback` / `Tool::rollback_support`
(`crates/core/src/traits/tool.rs:85-103`) are marked `#[deprecated]`, **not
removed** — they are dead today (zero overrides, zero call sites) and removal is
deferred to the checkpoint work so the deprecation is breaking-change-free.

**Checkpoint bytes have their own budget.** They are **not** bounded by
`CACHE_LIMIT_BYTES`, whose own doc comment scopes it to the evidence path —
"maximum cached read content size", "keeping the cache a small, bounded side
table" (`crates/sessions/src/resource_facts.rs:47-53`) — and whose alias
`tool_facts::MAX_HASH_BYTES` (`crates/orchestrator/src/tool_facts.rs:50`) is
the hashing budget. A checkpoint store is a different store with a different
purpose (restoration, not read dedupe), so it gets **its own named limit** —
a planned `CHECKPOINT_BLOB_LIMIT_BYTES` — and a **byte-oriented
representation**: the sidecar column is a BLOB, never a text field, and the
limit is a byte bound on that column. A small text-only restore slice (≤ 64
KiB) remains acceptable **only as an explicitly scoped partial milestone** —
it does not represent the byte budget of the full store (see the milestone
note in Consequences).

**The scoped slice is not binary/untracked-file restoration.** It restores
≤ 64 KiB text pre-images only. Oversized and binary targets stay hash-only:
the blake3 hash is a **verifier, not a source** — it detects drift but cannot
reconstruct a deleted before-image, and no hash-based reconstruction is ever
claimed. Untracked-file restore inherits the git-stash gap (`undo.rs:56-57`)
until the streaming/block-based store lands. The plan's "exact restore
including binary files" goal is **not** met by this ADR.

**Retention is coupled to checkpoint lifecycle — this is an invariant of this
decision.** Retention must **preserve the bytes needed by active checkpoints,
or explicitly expire those checkpoints**; it must never delete the bytes while
continuing to advertise the checkpoint as restorable. Concretely:
prune-eligibility is gated on checkpoint status, and once a checkpoint's bytes
are pruned the checkpoint **transitions to `Expired`** — the restorable claim
is withdrawn from that instant, the record remains for audit with its `Expired`
status, and no consumer may treat it as restorable. A checkpoint advertised as
restorable is never prune-eligible; a checkpoint whose retention is explicitly
released (operator action or a terminal state) becomes `Expired` first, and
only then may its bytes be collected.

### 5. `MemoryEvidence` — a read-time derived composite

`MemoryEvidence` is a **read-time derived composite, not a stored record**. The
epistemic axes — truth status, relevance, evidence/corroboration score,
confidence, and freshness — stay separate and non-interchangeable; they are
never collapsed into one number. Truth uses a new core vocabulary enum (see the
layering rule: the real `FactStatus` lives in
`crates/orchestrator/src/world_model.rs:301`, which core may not name). A
hardened `Confidence` enum (`Unknown | Known { value, source }`) enforces its
invariants **at the boundary, not only at one constructor**: the field(s)
holding the numeric value are **private**, so the only construction paths are
the validated ones, and the `Deserialize` impl is custom (planned) to **reject
NaN, out-of-range, and unknown-collapsing inputs at deserialization** — a
hostile or stale payload never reaches an invalid `Confidence`. `Unknown` has
**no numeric accessor**, and renders as the literal word — **never `0.0`**.
The composite carries `snapshot_generation` / `observed_at` drift stamps so a
consumer can detect that the composite has drifted from its sources;
detection, not prevention.

## Dependency-layering rule (a rule of this decision)

**`concerto-core` owns vocabularies; crates above core own conversions into
them.** A core type may **never** name a type from `tools`, `shell`, `sessions`,
`memory`, or `orchestrator`. This is a real architectural constraint, verified
against the workspace graph:

- `crates/core/Cargo.toml` declares **no workspace (`concerto-*`) dependencies**
  — its `[dependencies]` are third-party crates only.
- `crates/shell/Cargo.toml:12-15` depends on `concerto-config`,
  `concerto-core`, `concerto-tools`, and `concerto-sessions` — so the shell
  crate **may** name both core and tools, which is why the
  `CommandResult::to_tool_failure()` conversion belongs there.
- `crates/memory/Cargo.toml:12` depends only on `concerto-core`.
- `crates/orchestrator/Cargo.toml:12-18` depends on core, tools, providers,
  sessions, memory, eval, and lsp — and **not** on `concerto-shell`.

Consequences: `ToolStatus` / `ToolDiagnostic` mirror the *shape* of shell's
`CommandStatus` / `Diagnostic` **without naming them**; and
`MemoryEvidence.truth` uses a new core vocabulary enum because the real
`FactStatus` lives in the orchestrator (`world_model.rs:301`). Any design that
makes core name a downstream type is rejected on this rule.

## Live defect found: the canonical effect key

The `write` tool presents to policy as `("filesystem", {operation: "write"})`:
`WriteTool::policy_view` returns `("filesystem".to_string(), canonical_input)`
(`crates/tools/src/filesystem.rs:715-718`) and registers under the name
`"write"` (`filesystem.rs:684-686`). The policy engine therefore sees a
canonical filesystem write. But several accounting seams key on the **registered
name** instead, so aliased writes escape accounting. The executor's own comment
states the split: policy evaluates the canonical view, while "execution and
audit below still use the registered name and the caller's input"
(`crates/core/src/executor.rs:764-766`).

Three escape sites, plus one audit site that already covers the alias:

1. **Audit completion row.** `AuditEntry { tool_name: tool_name.to_owned(), … }`
   records the registered name (`crates/core/src/executor.rs:197-198`), not the
   canonical `("filesystem", {operation:"write"})` identity.
2. **Single-agent fact writer.** `is_file_affecting_tool` matches
   `"write_file" | "delete_file" | "edit_file" | "create_file" |
   "modify_file"`, or `"filesystem"` with a mutating operation
   (`crates/orchestrator/src/tool_facts.rs:574-578`). There is **no `"write"`
   arm**, so an aliased write produces no invalidation and no pre-image column.
3. **Write gate short-circuit.** The gate keys on `req.tool` at
   `versioned_targets` (`crates/orchestrator/src/gate.rs:555`),
   `is_read_only_request` (`gate.rs:613`), `attributed_paths` (`gate.rs:633`),
   and the hunk-attempt check (`gate.rs:943`). A request whose `req.tool` is
   `"write"` yields no versioned targets — hence no pre-image, no base-version
   conflict check, and no ownership acquire.
4. **Progress accounting — covered, but fragile.** `is_audited_mutation_event`
   (`crates/orchestrator/src/agent_loop/output_progress.rs:93-110`) **already
   matches `"write"` explicitly in its tool-name list** (`output_progress.rs:101-104`),
   so this site is **not** a fourth alias-write escape. What remains fragile is
   the `summary.starts_with("Wrote ")` / `"Deleted "` Display-parse fallback
   (`output_progress.rs:109`), which classifies normalized writes whose raw
   `operation` field was absent from prose; that fallback is to be removed as
   prose parsing, while the tool-name arm stays.

The per-site patch at `crates/orchestrator/src/coordinator.rs:1216-1218`
(`tool == "write" || crate::tool_facts::is_file_affecting_tool(tool, args)`) is
evidence that the grammar has already drifted: each seam patches the alias
individually instead of sharing one canonical key.

**Fix (planned).** Introduce one `ToolExecutor::canonical_effect` service so
every accounting seam keys on the canonical effect, not the registered name.

- **Both execution paths canonicalize at request assembly, before
  `stamp_base_versions` — same load-bearing ordering, both backends.**
  Supervised: `handle_execute_tool` must canonicalize before the stamp call
  (`crates/orchestrator/src/supervisor.rs:1841`), because the child process
  cannot see the registry and `stamp_base_versions` keys on `versioned_targets`
  and returns early when that list is empty (`gate.rs:1705-1709`). In-process:
  the `GateRequest` is assembled from the registered `tool_name` verbatim
  (`crates/orchestrator/src/in_process_gate.rs:221-232`, the name at `:224`)
  and `stamp_base_versions` runs immediately after (`in_process_gate.rs:238`);
  the canonical-effect resolution must be inserted between assembly and stamp.
  On both paths the canonical identity is resolved **before** pre-image
  hashing, so the stamped targets, the audit row, and the fact writer all see
  the canonical operation.
- **Record both identities.** An audit row recording that the caller invoked
  `write` is not wrong; accounting *solely* by that name is the problem. The
  audit and evidence rows carry the **requested tool identity and the canonical
  effect both** — attribution by the former, accounting by the latter.
- **Canonicalization is not a reroute.** `policy_view` is an
  accounting/attribution view, never a substitution of the registered
  implementation that executes: the executing implementation remains the
  registered one, and the canonical effect only determines how that execution
  is accounted, versioned, and attributed.

**Flag this as an IPC/contract review point**: the canonical identity must be
resolved on the supervisor side of the child boundary, not inferred from what
the child sends.

## Accepted limitations

These are real ceilings of this decision, not hedges:

1. **Exact byte restore is not guaranteed above 64 KiB.** The sidecar's own
   planned budget constant (`CHECKPOINT_BLOB_LIMIT_BYTES`) defaults to 64 KiB
   and is **not** an alias of the evidence-path `CACHE_LIMIT_BYTES`
   (`crates/sessions/src/resource_facts.rs:47-53`). Oversized and binary
   targets are hash-only: the blake3 hash is a verifier, never a
   reconstruction source — restoring a deleted before-image from its hash
   alone is not possible and not claimed. Restore degrades to `Manual`, and
   hash verification **detects drift but does not recover bytes**. The
   in-memory gate pre-image cache holds more per entry
   (`PRE_IMAGE_CACHE_MAX_ENTRY_BYTES = 4 MiB`,
   `crates/orchestrator/src/gate.rs:444-446`) than the durable sidecar, so a
   crash-recovered restore can restore *less* than the live gate could stage.
   The ≤ 64 KiB text slice is a **partial milestone**; the streaming/block-based
   pre-image store that completes binary restore is deferred. **The plan's
   "exact restore including binary files" goal is NOT met by this ADR.**
2. **DB / network / process effects are never reversed.** A file checkpoint
   makes no such claim (`excluded` covers these classes).
3. **Git-stash restore inherits the untracked-file gap** — `undo.rs:56-57`
   stashes without `-u`.
4. **`MemoryEvidence` is a read-time projection and can be stale.** The drift
   stamps make staleness detectable, not preventable.
5. **`Confidence` / newtype separation is discipline-enforced alongside
   boundary enforcement.** Private fields and a rejecting `Deserialize` bound
   the construction paths; constructors, docs, and tests back them. A caller
   holding a raw `f64` can still deliberately build the wrong value — no type
   system can stop that, but no deserialization path can reach an invalid
   value either.
6. **Manifest probes are point-in-time.** A tool installed mid-run is invisible
   until the next explicit re-probe.

## ADR-71 conformance

A reviewer flagged tension between a completion-honesty gate and Coordinator
supremacy. Read against ADR-71:

- §1.1 "**One run, one master.** The coordinator owns every execution decision
  for a run: what work exists, what is dispatched, in what order, to which
  agent, and when the run stops."
- §1.2 "**An instruction runs until (a) the work is done, (b) the user
  intervenes, or (c) the coordinator errors.** These are the only run-level
  endings."
- §1.3 "**All agent errors flow to the coordinator.** Every failure, denial,
  conflict, and guard finding surfaces to the Coordinator as a result to be
  routed — never around it."
- §2 classifies **verifier failure** and **acceptance-gate failure** as
  **Coordinator-error-class** terminals that surface to the Coordinator as
  results to be routed, never around it (§1.3). They *already* end a run only
  after the Coordinator routes them.

Coordinator supremacy governs **who chooses subsequent work and termination**;
it does not require inaccurate verification reporting. Returning an unmet-check
result to the Coordinator is **compatible** with that authority: a
completion-honesty `Blocked` is **evidence surfacing, not a compiled dispatch
rule** — it selects no agent and orders no work; it declines to report `Done`
for a task whose own `require_verification` contract was unmet, and `Blocked`
is a returned status the caller routes — the same shape as the loop's existing
accepted block for "no file-changing tool call succeeded". Reporting `Done`
without the required verification would itself violate §1.2 ("the work is
done").

**Two states, kept distinct.** "**Task finished**" and "**required verification
passed**" are different states, and the completion record must be able to
report the former without claiming the latter. `Done` implies the task's work
is complete; it does not, by itself, attest that required checks passed. The
verification outcome (this ADR §3) and the completion record are separate
fields, and the gate only refuses the *claim* of verified completion when the
required evidence is absent — it never pretends the work is unfinished when it
is, and never pretends the verification passed when it did not.

**Default-off is a rollout posture, not an authority requirement — and it does
not satisfy H04 by itself.** The gate is opt-in and defaults off, scoped to the
single-agent action-required path only, so no run outcome tightens silently
and the multi-agent Coordinator path is unchanged. That posture eases
migration, and the opt-in flag stays. But **H04 remains incomplete until the
required checks demand fresh passing evidence**: default-off means the gate
cannot yet guarantee honest completion claims in production runs, so the slice
list below marks the *enforcement* (not the opt-in key) as the completing step.
Nothing here selects, orders, or ends a run behind the Coordinator.

## Compatibility & migration

Compatibility is **per contract, defined and tested** — there is no blanket
"`#[serde(default)]` handles everything" guarantee. Each contract below states
its own supported-version set and migration behavior explicitly, following the
checkpoint's named-versions precedent: the checkpoint reader accepts
**specific** versions — current v4 loads as-is, legacy v3 and v2 migrate
in-memory, anything else is rejected (`crates/orchestrator/src/checkpoint.rs`,
`from_json` + `migrate_schema` at `:503-541`) — not "every version ≤ current".
`#[serde(default)]` is characterized accurately: it fills **missing fields on
read**; it does **not** validate schema versions and does **not** preserve
every Rust source API. The shell envelope is the existence proof — `CommandResult`
derives `Deserialize` with a `schema_version` field and **no enforcement at the
struct level** (`crates/shell/src/model.rs:124-142`) — so each new contract
adds the enforcement the envelope lacks.

Per contract:

- **`ToolFailure`.** Supported set: current `schema_version` plus the versions
  the conversion explicitly migrates. Wire form: additive fields, unknown
  fields ignored; unknown schema versions rejected. Rust API: `ToolStatus` /
  `ToolDiagnostic` / `ToolFailure` — new fields additive; new enum variants
  under `#[non_exhaustive]` (the shell envelope's own enums already follow
  this: `CommandStatus` at `model.rs:8-11`, `DiagnosticSeverity` at
  `model.rs:36-39`); deprecation posture is deprecate-don't-remove (see §4).
- **`VerificationResult`.** Backward-read: a legacy `VerificationSummary`
  (no `outcome`) deserializes with `None` — display-only, never passing
  evidence (§3). Forward-read: older binaries ignore the new fields. `outcome`
  is authority over `passed`; mismatches are flagged, not normalized (§3).
- **`EnvironmentManifest`.** One additive `WhiteboardKind`
  (`EnvironmentObserved`) — the enum at `whiteboard.rs:48`; new kinds additive
  kebab-case. No `SCHEMA_VERSION` bump for the config key (below).
- **`CheckpointRecord`.** Follows the graph-checkpoint named-versions precedent
  (`checkpoint.rs:503-541`) for the record envelope; the sidecar table is
  migration 037. A pruned-bytes checkpoint is `Expired`, never silently
  restorable (§4).
- **`MemoryEvidence`.** Read-time projection — its version surface is a
  documented read-time schema with the same named-version policy; the
  `Confidence` `Deserialize` rejects invalid inputs at the boundary (§5).
- **`[agent] strict_verification_gate = false`** is additive under the existing
  `Option<T> + #[serde(default)]` section pattern, so it needs **no
  `SCHEMA_VERSION` bump**. `AppConfig` has no `deny_unknown_fields` (stated at
  `crates/config/src/schema.rs:28`), so older binaries ignore the section. It is
  deliberately **not** placed under `[orchestration]`: that section
  deserializes `deny_unknown_fields` (`OrchestrationConfig`,
  `crates/config/src/blueprint.rs:876-877`; nested blocks are likewise closed —
  `CapabilityMask` at `blueprint.rs:65`, `StageFlags` at `blueprint.rs:179`), so
  an unknown key there would break older binaries instead of being ignored.
- **Exactly one migration: 037** (the checkpoint sidecar table). No other schema
  change is made by this ADR.

Each contract's slice verification below includes an explicit compatibility
test: round-trip of the current version, rejection of unknown future versions,
and legacy-decode of the named legacy versions (or the `None`-outcome case for
`VerificationResult`).

## ADR-73 interaction

ADR-73 encrypts the **whole `sessions.db`** ("Scope: the whole `sessions.db` —
sessions, transcripts **and** the append-only `audit_log`. There is no partial
encryption of the log alone", `ADR-73-audit-encryption-and-retention.md` §1,
lines 78-79). That encryption is **operator-controlled**: the
`encrypt_at_rest` flag defaults to `false` and encrypts `sessions.db` with
SQLCipher only when an operator enables it (`ADR-73` §3 table, line 143). A new
sidecar table in that database is therefore **encrypted when database
encryption is enabled** — the same guarantee and the same operator control as
every other table in the database; it is not unconditionally "auto-encrypted
with no extra work": the guarantee holds only when an operator enables
`encrypt_at_rest`. However, `prune_audit`
only archives and deletes `audit_log` rows (the archive schema covers only
`audit_log`, `crates/sessions/src/audit_retention.rs:40-89`; the delete targets
`audit_log` only, `:267`), so an unextended sidecar would **orphan pre-image
bytes indefinitely** — encrypted, but never aged out.

**Decision:** add the sidecar table **and extend the retention job** to cover
it, so checkpointed pre-images age out on the same policy as the audit rows
rather than growing without bound. **Justification:** the sidecar is derived
data, not the durable reference (hashes are); leaving it to grow unbounded would
re-introduce an unbounded store inside an ADR-73-bounded database — and the
lifecycle coupling of §4 (prune-eligibility gated on checkpoint status,
pruned bytes ⇒ `Expired`) is enforced by the same extension.

**Tracked gap:** "retention extension slipping" is carried as an explicit gap
for H01 — the sidecar may land before the retention extension does, and while it
is unextended the orphan risk above is live. This ADR records the gap rather
than claiming the extension is done.

## Deliberate behavior changes

Named explicitly, not slipped in:

- npm-without-test-script now records `Skipped` (was `passed: true`,
  `agent_loop.rs:3155-3161`).
- `.rs`-without-`Cargo.toml` and `.js`-without-`package.json` now record
  `Unavailable` (was no record at all, `agent_loop.rs:3080-3163`).
- Evaluation errors now produce typed records (was `None`,
  `agent_loop.rs:1632-1639`).
- The `decide_exit` block (`agent_loop.rs:1682`) is **opt-in only**, gated by
  `[agent] strict_verification_gate`.
- Legacy verification records without `outcome` stop being treated as passing
  evidence (they remain display data) — §3.
- Unknown schema versions stop being described as "≤ current accepted": each
  contract names its supported versions — Compatibility section.

## Implementation slices (ordered, each independently shippable)

Slice 1 is the **directed next implementation step** — the reviewer directed
implementing the canonical-effect correction first, and the slice list below
starts there. Slices 4 and 5 are **partial milestones, not completion** of the
corresponding upgrades (see the milestone note in Consequences).

1. **Canonical effect key**, including parity on **both** execution paths: the
   supervised `handle_execute_tool`-before-`stamp_base_versions` ordering
   (`supervisor.rs:1841`) **and** the in-process request-assembly ordering
   (`in_process_gate.rs:221-232` before `:238`). Verify: aliased write produces
   a versioned target, pre-image, conflict check, and audit row identical to a
   direct `filesystem` write; the audit row records **both** the requested
   name and the canonical effect; the executing implementation is still the
   registered one.
2. **`ToolFailure` envelope** (core vocabulary + shell conversion). Verify:
   `CommandResult -> ToolFailure` round-trips codes; the per-variant
   disposition table matches the structural `ToolError` arms exactly; opaque
   text (unknown variants, plugin/MCP errors, `ExecutionFailed` prose) routes
   through the compatibility predicate and is marked `opaque`, never
   masquerading as a native code; **compatibility test**: round-trip,
   unknown-version rejection, legacy-decode.
3. **Additive verification outcomes** — **no run-outcome change**. Verify:
   legacy records deserialize with `None` and are **display-only — a legacy
   `passed: true` on a skipped check does not become trusted evidence by
   deserializing**; skip/no-manifest/error cases carry
   `Skipped`/`Unavailable`/`Error`; `outcome` wins over `passed` and
   inconsistencies are flagged, not normalized; `checked_at` is recorded;
   **compatibility test**: legacy `VerificationSummary` decode, current
   round-trip, inconsistent-combination handling.
4. **Opt-in `strict_verification_gate`**. Depends on slice 3. Verify: default
   off leaves `decide_exit` behavior unchanged; enabled reports `Blocked` for an
   unmet `require_verification` contract. **Partial milestone**: enforcement of
   *fresh passing evidence* (the `outcome` + freshness fields of §3 feeding the
   gate) is the completing step for H04 and is planned separately.
5. **`CheckpointRecord` + sidecar (037) + retention extension + deprecations**.
   Depends on slice 1. Verify: pre-image bytes recover a ≤ 64 KiB text file;
   oversized/binary degrade to `Manual` and are never claimed restorable;
   restore is policy-gated and idempotent; `prune_audit`-style aging covers
   the sidecar **with prune-eligibility gated on checkpoint status** (bytes
   pruned ⇒ `Expired`, restorable claim withdrawn); the byte budget is
   `CHECKPOINT_BLOB_LIMIT_BYTES`, not `CACHE_LIMIT_BYTES`; **compatibility
   test**: named-version policy (current as-is, named legacy versions migrate,
   unknown rejected — per `checkpoint.rs:503-541`). **Partial milestone**: the
   ≤ 64 KiB text slice does not complete binary/untracked-file restoration;
   the streaming/block-based pre-image store is the completing step and is
   planned separately.
6. **`EnvironmentManifest`**. Verify: collected once per run with freshness
   stamps; no authorization branch reads it; persisted as
   `WhiteboardKind::EnvironmentObserved`; **compatibility test**: round-trip,
   additive-kind behavior, unknown-version rejection.
7. **`MemoryEvidence` axes**. Verify: `Unknown` renders as the literal word and
   exposes no numeric accessor; drift stamps detect divergence;
   **compatibility test**: `Confidence` deserialization rejects
   NaN/out-of-range/unknown-collapsing inputs at the boundary, and a valid
   round-trip preserves `Unknown` and `Known` distinctly.

Dependencies: **4 depends on 3**; **5 depends on 1**.

## Non-goals

- No policy-semantics change.
- No compiled dispatch or scheduling (ADR-71 §3).
- No world-model projection rule for verification staleness — H01 ships the
  data fields only.
- No cross-process restore protocol.
- No confidence calibration.
- No `resource_facts` schema changes.
- No blanket cross-contract compatibility promise — each contract defines its
  own supported-version set (Compatibility section).

## Alternatives considered

- **(a) Five greenfield parallel contracts.** Rejected: duplicates existing
  types (`VerificationSummary`, the shell `CommandResult`/`CommandStatus`/
  `Diagnostic` envelope) and forks the shell envelope from the failure
  vocabulary.
- **(b) Retiring `RollbackSnapshot` now.** Rejected: a removal is a breaking
  change beyond H01's extend-don't-fork mandate; deprecate instead and remove
  with the checkpoint work.
- **(c) Folding pre-image bytes into `WriteApplied` JSON payloads.** Rejected:
  the log is append-only and never rewritten, so 64 KiB blobs would bloat IPC on
  every append; and bytes are repudiable derived data while the hash is the
  durable reference.
- **(d) Making the verification gate default-on.** Rejected: it would silently
  tighten run outcomes and pre-empt Coordinator authority (ADR-71 §1.2).

## Consequences

- One canonical effect key closes the alias-write accounting escapes
  structurally instead of by per-site patching — ordered identically on both
  execution paths, recording both the requested identity and the canonical
  effect, without rerouting execution.
- Typed failures replace prose classification: the per-variant map is the
  derivation source, the substring predicate survives only as the opaque
  fallback, and dispositions distinguish cancelled / awaiting-decision /
  bounded-retry / do-not-re-issue instead of collapsing into a boolean.
- Verification outcomes become honest without changing any run outcome until an
  operator opts in; legacy records stay visible but never count as passing
  evidence, and `outcome` outranks `passed` with mismatches surfaced.
- Failure types survive the shell boundary in a form core can speak, without
  core naming a downstream crate.
- Checkpointed pre-images are bounded by their own byte budget and by the
  ADR-73 retention policy once the extension lands — with prune-eligibility
  gated on checkpoint status — and the gap is tracked until it does.
- H04's honesty gate is opt-in today and incomplete until required checks
  demand fresh passing evidence; "task finished" and "required verification
  passed" remain distinct states the completion record can report separately.

**Milestone scoping note (required by review).** Two items in this ADR are
**explicitly partial milestones, not completion** of the corresponding
upgrades:

- The **≤ 64 KiB checkpoint slice** (slice 5) restores small text pre-images
  only; it does **not** satisfy the binary/untracked-file restoration goal.
  The completing future work is the **streaming/block-based pre-image store**
  for binary restore (deferred, tracked as a gap).
- The **default-off verification gate** (slice 4) is a migration posture; H04
  is **incomplete until required checks demand fresh passing evidence**. The
  completing future work is **enforced fresh-passing-evidence** at the gate
  (the `outcome` + freshness fields of §3 feeding `decide_exit`), planned as a
  separate step.

Costs / risks: a 64 KiB restore ceiling (limitation 1); a retention extension
that can slip; `Confidence` discipline that remains backed by boundary
enforcement rather than a proof (limitation 5); and per-contract compatibility
tests that must each be written and kept green (Compatibility section).

## References

- Failure vocabulary / retry predicate & typed mapping:
  `crates/core/src/error.rs:362-470` (`ToolError`, `#[non_exhaustive]`),
  `:485-522` (`DETERMINISTIC_FAILURE_MARKERS`), `:529-530`
  (`DETERMINISTIC_CONTAINMENT_MARKERS`), `:539-542`
  (`is_deterministic_failure_message`), `:557-583`
  (`ToolError::is_deterministic_failure`),
  `crates/core/src/types.rs:1307-1316`, `crates/shell/src/model.rs:8-11,36-39,124-142`
- Loop verification + error flattening: `crates/orchestrator/src/agent_loop.rs`
  (`:1632-1639`, `:1682`, `:2820-2831`, `:3080-3163`, `:3155-3161`)
- Retry prose classification: `crates/orchestrator/src/shell_repair.rs:176-193`
- Undo / overlay restore / rollback seam: `crates/tools/src/undo.rs:56-57`,
  `crates/tools/src/virtual_fs.rs:743-746`,
  `crates/core/src/traits/tool.rs:85-103`, `crates/core/src/types.rs:293-297`
- Canonical effect key and gate: `crates/core/src/executor.rs:197-198,764-766`,
  `crates/tools/src/filesystem.rs:684-686,715-718`,
  `crates/orchestrator/src/tool_facts.rs:574-578`,
  `crates/orchestrator/src/gate.rs:444-446,555,613,633,943,1705-1709`,
  `crates/orchestrator/src/coordinator.rs:1216-1218`,
  `crates/orchestrator/src/agent_loop/output_progress.rs:93-110,101-104,109`,
  `crates/orchestrator/src/supervisor.rs:1841`,
  `crates/orchestrator/src/in_process_gate.rs:221-232,238`
- Checkpoint versioning precedent: `crates/orchestrator/src/checkpoint.rs:503-541`
- Retention / sidecar bounds: `crates/sessions/src/resource_facts.rs:47-53`,
  `crates/orchestrator/src/tool_facts.rs:50`,
  `crates/sessions/src/audit_retention.rs:40-89,116,189,267`,
  `crates/sessions/migrations/` (037 next free)
- Config layering: `crates/config/src/schema.rs:28`,
  `crates/config/src/blueprint.rs:65,179,876-877`
- Workspace graph: `crates/core/Cargo.toml`, `crates/shell/Cargo.toml:12-15`,
  `crates/memory/Cargo.toml:12`, `crates/orchestrator/Cargo.toml:12-18`
- Related ADRs: ADR-60 (restore is an authorized mutation), ADR-62 (executor
  boundary; amended 2026-10-09), ADR-65 (evidence spine; facts/claims/decisions
  and `resource_facts` supersession), ADR-71 (Coordinator supremacy §1.1/§1.2/
  §1.3/§2), ADR-73 (audit encryption and retention; §1 scope at lines 78-79,
  `encrypt_at_rest` default at line 143)

---

*Proposed 2026-10-09 (revised 2026-10-10): H01 harness-upgrade scoping
decision. Design only — nothing in this ADR is implemented by it.*