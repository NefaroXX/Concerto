# Specialist execution and orchestration handoff

This change targets `dev` at `005a18c1a6a4e365c7995b4f1eb31f1b37ef4c04`.
It builds on the current Coordinator, graph, tool facts and checkpoint contracts.
The Coordinator continues to choose the specialist and any continuation.

## Behavior

- Freeform specialists require a final nonempty answer and no unresolved observed
  tool failures before returning `Success`. Reaching the turn limit, requesting
  tools without an executor, or leaving rejected arguments unresolved returns
  `NeedsRevision` with a structured continuation.
- Successful edits, file lists, call counts and spend remain in the settled
  result. Tool failures use the existing diagnosis's retry/recovery advice.
  Cancellation and approval pauses retain their existing control flow.
- A successful operation resolves a prior failure only for the same tool and
  operation/resource. An unrelated successful edit cannot erase another file's
  failure. Opaque tools require the same arguments; the runtime does not infer
  that a different command repaired the failure.
- Unfinished execution stays `NeedsRevision` in the graph and blocks dependent
  work. The Coordinator explicitly continues it with `call_specialist.task_id`,
  retaining the node, its original dependencies and its prior settlement.
  `update_obligations` may retarget or revise this held node. This grants no
  artifact ownership; any ownership transfer still uses the existing gate.
- A completed review's ordinary `NeedsRevision` recommendation still settles
  its review node. The continuation marker distinguishes unfinished execution.
- Prompts include dispatch identity, workspace generation and expected outputs.
  Handoffs prioritize the current task, its parent and its direct dependencies,
  retain the latest result per task, and identify omitted dependencies. Summaries
  remain agent claims; write success remains distinct from verification.

## Compatibility and limits

`ExecutionContinuation` is an orchestration-private projection serialized after
`Specialist continuation v1: ` in the existing `NeedsRevision.reason`. Existing
graph checkpoint rows already preserve the result and held status; no core enum,
configuration field, session migration or checkpoint version is added. Human
event messages omit the machine projection. Prompt projections carry bounded
call excerpts and explicit omission counts; persisted progress is also bounded.

The projection carries call IDs, tool names and operation hashes, never raw tool
arguments, file contents or executable replay instructions. A continuation must
inspect the live workspace. Successful calls are not instructions to replay.
An omitted outstanding operation keeps settlement incomplete.

This records graceful settlement, not a process snapshot or a durable crash
recovery journal. Provider errors, cancellation and approval pauses continue to
use the existing failure/control-flow mechanisms. Durable undo and richer typed
failure/verification evidence remain responsibilities of the harness upgrade.

## Parallel implementation boundary

| Owner | Files and responsibilities |
| --- | --- |
| Codex orchestration branch | `agents/generic/freeform.rs`, `agents/generic/prompt.rs`, `agents/execution_state.rs`, `agents/task_contract.rs`, task dispatch/settlement in `coordinator.rs`, human result events in `agent_runner.rs`, corresponding regression tests |
| OpenCode harness upgrade | Environment/tool-failure/verification/checkpoint/memory-evidence schemas and services; executor/IPC error transport; verification adapters; durable undo/restore; experiment/database/container harnesses; memory evidence |
| Handoff before editing | Shared core types, config schema, session migrations, runtime construction, CLI resume, and harness integrations in the orchestration files above |

`generic.rs` delegates freeform execution and prompt construction to the new
modules. Its evaluator and typed submission paths stay in the parent file.
OpenCode H05 should start from this extraction rather than copying the old
freeform loop or building a second validator path.

OpenCode H02 should replace the existing diagnosis-to-tool-result adapter when
its canonical `ToolFailure` is available. Do not add a second error taxonomy.
When H01 provides a canonical rich result schema, migrate this private
continuation projection and its checkpoint reader together; do not keep two
independent progress stores. H04's verification-evidence completion checks must
retain the held-task/dependency distinction introduced here.

Merge or cherry-pick this branch before touching its reserved orchestration
areas. Continue the harness on its own branch. No shared schema or runtime
construction files are changed by this orchestration slice.

## Validation

Regression fixtures cover actual retained writes, turn exhaustion, corrected
operations, unresolved sibling failures, schema rejection, missing executors,
unresolved failures across continuation attempts, bounded progress, dependency
blocking, checkpoint round trips, same-node continuation, owner retargeting and
completed review recommendations. Formatting, compilation and tests must be
confirmed by the repository's Rust CI; the editing environment has no Rust
toolchain.
