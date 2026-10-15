# Orchestration completion checklist

This tracks the six-stage orchestration backlog continued in PR #343. The
earlier merged runtime and harness work remains the foundation. This branch
finishes its specialist, completion and client integration. It is awaiting
the PR's required CI checks; implementation alone is not a validation result.

| Stage | Delivered behavior | Evidence |
| --- | --- | --- |
| 1. Honest outcomes | A failed native check cannot become specialist success because the tool returned successfully. The opt-in strict Coordinator gate requires fresh canonical evidence and keeps refused completion actionable. | `agents/generic/verification.rs`; `verify.rs`; `coordinator/tests/specialist_execution.rs::strict_verification_refusal_keeps_a_settled_node_actionable_on_continue` |
| 2. Task and result contracts | `AgentRunResult.verification` transports existing typed records and defaults empty for older checkpoints. Task-relevant handoffs include bounded check outcomes and explicit omissions. | `core/src/types.rs`; `agents/task_contract.rs`; `agents/generic/verification.rs` |
| 3. Continuation and questions | Interrupted specialists preserve the same task, successful call identities, files and observed checks. CLI resume starts the saved run. Desktop waits for the backend's resume decision. Replies consume scoped questions and preserve the original objective; Continue keeps questions pending. | `agents/execution_state.rs`; `runtime_runner/operator_input.rs`; `cli/src/lib.rs`; `cli/src/app.rs`; `desktop/src/app/run_dispatch.rs` |
| 4. Shared runtime guarantees | Native checks retain the existing executor, policy and approval path. Checks bind to stable source bytes, generation and real check events. Exact successful mutation replays are skipped; verification is rerun. Saved graphs and strict verification use the Coordinator runtime rather than a backend that cannot honor those contracts. | `registry.rs`; `runtime_runner.rs`; `verify.rs`; `agents/generic/freeform.rs` |
| 5. Evidence-driven delegation | The Coordinator continues or retargets held work through existing dispatch tools. It retains the graph's dependency position and can re-enter its decision loop after a verification refusal. No scheduler or new permission mechanism is introduced. | `coordinator.rs`; existing continuation/dependency tests in `coordinator/tests/specialist_execution.rs`; H11 evidence recall |
| 6. Visibility and acceptance | Canonical checks reach completed, partial and awaiting-user outputs. CLI and desktop use the structured run summary, and the persisted transcript retains it. The recovery fixture uses the production specialist, filesystem, verifier and SQLite checkpoint. | `AgentOutput::summary`; `agents/generic/tests/recovery.rs::real_specialist_recovers_failed_check_from_disk_without_repeating_effects` |

The recovery fixture writes HTML with a missing reference, observes a failed
existence check, interrupts, closes and reopens SQLite, restores the same graph
and task, deliberately replays the original write, repairs the file, and runs a
fresh native check. Audit rows must show the original write exactly once. A
later source mutation must invalidate the passing evidence. Its provider is
scripted; its specialist, tools, verification and persistence are production
implementations. It makes no claim about live-model reliability.

The strict verification gate remains opt-in. Workspace fingerprints use the
existing cache/build exclusions and are bounded to 8,192 entries and 64 MiB of
source bytes. Unknown, unreadable, cancelled or oversized coverage cannot prove
completion. The specialist ledger retains at most 128 records; overflow is an
explicit error. The continuation retains at most 64 successful call identities
and reports omissions. An exact replay means the same task, call ID, tool and
operation hash; intentional new work uses a new call identity.

External harness transports, live-provider flake policy, and the declared
sandbox/restore capability ceilings remain the separate scopes documented in
`harness-access.md`, `harness-upgrade-plan.md`, and ADR-83. They are not expanded
by this integration. Hard process death inside an unsettled operation still
uses the existing recovery journal; the specialist continuation is a settled
progress record, not a process-memory snapshot.
