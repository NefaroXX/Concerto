# Harness access and configuration (H13)

This document declares the release coverage for the H13 "common access and
configuration" item and records where each shared setting lives. It is the
explicit declaration the plan requires; anything not listed here is
**out of scope** for this release, not silently missing.

## Operations

One service seam — `concerto_orchestrator::harness_service::HarnessService`
(`crates/orchestrator/src/harness_service.rs`) — owns every harness operation:

| Operation | Tool implementation | Path |
| --- | --- | --- |
| `environment_describe` | `EnvironmentDescribeTool` | `ToolExecutor::execute` |
| `checkpoint_create` | `CheckpointCreateTool` | `ToolExecutor::execute` |
| `checkpoint_inspect` | `CheckpointInspectTool` | `ToolExecutor::execute` |
| `checkpoint_restore` | `restore_file_checkpoint` | `ToolExecutor::restore_checkpoint` |
| `checkpoint.prune` (CLI-only carve-out, see below) | `run_checkpoint_prune` (CLI dispatch) | direct `SqliteSessionStore::prune_file_checkpoints` — operator-invoked, delete-only of window-expired rows, same precedent as `sessions prune` |
| `verify_run` | `VerifyRunTool` | `ToolExecutor::execute` |
| `memory_recall` | `MemoryRecallTool` | `ToolExecutor::execute` |
| `memory_inspect` | `MemoryInspectTool` | `ToolExecutor::execute` |

Every adapter calls `HarnessService`; no adapter touches a filesystem path, a
SQLite pool, or a `FileCheckpointStore` directly — with one explicit,
operator-scoped exception: `checkpoint.prune` (CLI-only, see below) connects
to the session store directly to delete window-expired sidecar rows, mirroring
the pre-existing `sessions prune` precedent. It is delete-only, never runs
automatically, and is listed in the operations table as its own row rather
than hidden inside the service claim. `checkpoint_restore` is a
service-level operation because a restore must itself issue gated `filesystem`
writes; it calls the executor's dedicated `restore_checkpoint` pipeline (one
policy decision + audit row per resource). Restore is operator/frontend-scoped
by design, not a structural impossibility: rewriting workspace state is never
something an agent invokes on itself mid-run.

## Declared coverage

| Adapter path | Status | Proof |
| --- | --- | --- |
| In-process single-agent | covered except `checkpoint_restore` (operator/frontend-scoped, see below) | `harness_service::tests`, `vertical_slice` |
| In-process coordinator | covered except `checkpoint_restore` (operator/frontend-scoped, see below) | `resume_with_memory_still_grounds` (evidence-bearing recall) |
| Supervised agents | covered (memory recall evidence) | `ipc::MemoryChunk` status fields + `supervisor` |
| CLI JSON | covered | `concerto-cli` `harness` subcommand |
| Desktop | covered | `concerto-desktop` harness panel |
| External API / MCP facade | **out of scope** | see below |

In-process agents reach the operations through the run's shared
registry/executor pair, exactly like every other tool; the service is the seam
frontends call. "Equivalent semantics and evidence" means the same operation
through any covered adapter yields the same `HarnessInvocation.data` payload
(asserted by the equivalence tests).

Adapter proof map:

- In-process / standalone service: `tests/harness_equivalence.rs`
  (`memory_recall` equals a direct `retrieve_with_evidence`; the standalone
  `environment_describe` payload is stable across builds; the checkpoint
  lifecycle round-trips real bytes and records its evidence).
- Supervised: `gate_proxy::tests::wire_standing_rebuilds_evidence_bearing_chunks`
  (the wire's status/source/why/degraded/interrupted rebuild the same
  `EvidentChunk` evidence) plus the `ipc` wire round-trip/legacy-decode tests.
- CLI JSON: `concerto-cli::dispatch::harness` tests invoke the real service
  end-to-end for `environment_describe` and the checkpoint lifecycle and
  assert the `--json` payload is the service shape. `memory_recall`/`memory_inspect`
  CLI dispatch is covered by unit tests and by the shared service seam; a full
  CLI `memory_recall` end-to-end run is **not claimed** here because the first
  recall triggers `fastembed` model resolution (a network download) in this
  environment — the same embedder every in-process recall uses.
- Desktop: `concerto-desktop::views::harness` tests (request building,
  completion recording incl. refusals, and rendering the service payload).

## External access — deliberate non-coverage

"External agents should receive narrow, run/project-scoped API access."

No external path meets the bar in this release, and a thin unauthenticated
endpoint was **not** shipped to claim coverage. The `api-server`'s `AppState`
is session/task/spend-oriented and holds no `ToolExecutor` and no run-scoped
executor. Routing a harness operation through it would either build a second,
ungoverned executor (violating the binding constraint that every operation
keeps its normal policy/audit path) or accept a caller-supplied project root
with no run binding — exactly the wide, unscoped access the plan forbids. The
server's API-key auth authenticates a client, not a run, so it cannot scope an
operation to the caller's own run/project.

A correct external surface needs its own run-scoped
transport/authentication/lifecycle decision — the same class of decision the
plan defers for the MCP facade under ADR-43. It is therefore declared out of
scope here. The constant `EXTERNAL_ACCESS_NON_COVERAGE` in
`harness_service.rs` carries the same reasoning at the code site.

## Shared settings (H13 §5)

One settings home: the additive `[harness]` config section
(`crates/config/src/schema.rs`, `HarnessConfig`). Every key is
client/operator-controlled: the harness controls *how much* the local
operator's own harness retains and *which* verifier it prefers — none of it
grants an agent new authority, widens the policy engine, or is read by any
authorization path.

| Key | Meaning | Controlled by | Why |
| --- | --- | --- | --- |
| `harness.checkpoint_retention_days` | Retention window for checkpoint byte rows | client/operator | Local disk-lifecycle choice; `0`/unset keeps today's keep-all behavior. Applied only by the operator-invoked `concerto harness checkpoint prune` |
| `harness.recall_limit` | Default `memory_recall` `top_k` | client/operator | Local disclosure bound (validated `1..=50`); callers may request less |
| `harness.verify_selector` | Default verifier selection for `verify_run` | client/operator | Operator preference, not an authorization decision |
| `harness.declared_capabilities` | Descriptive capability declarations surfaced by `environment_describe` | client/operator | Descriptive labels only; never an admission gate |

**Security-impacting configuration stays outside this section**: policy rules
(`[policy]`), the native shell security ceiling (`[shell_security]`), and the
project-root allowlist (`project_roots`) remain where they are. The `[harness]`
keys never feed `SimplePolicyEngine`, the write gate, or the sandbox ceiling.
