# Completion evidence provenance

The Coordinator now keeps its decision-session run ID through graph execution
and the completion gate. Resume seeds the ID from the existing checkpoint;
a fresh invocation clears the previous attribution before planning. No session
or checkpoint schema changes are required.

Completion tool facts must name the current run. Historical rows without a run
ID remain inspectable but cannot satisfy a new run's acceptance gate. Successful
facts must also agree with any recorded outcome (`ok`), process exit code, and
typed failure envelope. Evidence details cite the backing event ID. Canonical
tool identities are honored, including aliases of `shell`.

The command classifier rejects help/listing/no-run invocations, arbitrary npm
scripts, format-only writes, and compound shell expressions. This is conservative
execution evidence, not a substitute for a harness result. Unsupported command
spellings require a validator or an explicit Coordinator exemption.

Exemptions carry the current run ID in their existing Decision payload and must
be attributed to the Coordinator. Another run in the same session cannot inherit
an old exemption. If a bounded evidence scan has more than 4,096 records, the
gate reports Partial rather than accepting an incomplete prefix. This deliberately
fails closed; a future run-filtered query can improve long-session availability.

Existing acceptance policy is otherwise preserved: an accepted validator, a
successful mutation of a declared deliverable, a recognized successful command,
or a scoped exemption may support acceptance. A mutation proves production,
not passing verification. Validator outcomes still use their existing contract.

## Harness handoff and next slice

Codex owns these completion consumers and run attribution. OpenCode owns the
canonical `VerificationSummary` producers and harness adapters. Multi-agent
`AgentRunResult` currently carries no canonical verification ledger; changing
that shared contract requires a coordinated handoff. The next integration must
transport the existing typed records (including unavailable/skipped/error),
bind them to the current workspace generation and checked bytes, and invalidate
passes after later mutations. This patch does not claim those freshness checks.

Reserved files: `crates/orchestrator/src/completion_evidence.rs`, the completion
and run-attribution seams of `coordinator.rs`, and the scoped decision writer in
`bypass_decision.rs`. Shared core types, configuration, sessions, runtime
construction, CLI and Studio are untouched. This branch is independent of
the context/handoff work in PR #312.

Regression fixtures cover current versus foreign/unscoped facts, contradictory
outcomes, exit failures, typed failures, misleading command invocations, actual
dispatch-context attribution through completion, legacy successes, and an
exemption followed by a fresh run in the same session.
