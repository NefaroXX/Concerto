# Completion evidence provenance

The Coordinator now keeps its decision-session run ID through graph execution
and the completion gate. Its checkpoint sequence also continues across that
boundary. Resume seeds both from the existing checkpoint;
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

## Canonical specialist verification

PR #343 adds the backward-compatible `AgentRunResult.verification` collection.
Native `verify.run` observations, including failures, skipped checks and errors,
travel through specialist settlements, checkpoints and bounded handoffs. The
actual check's whiteboard event backs a transported record; write evidence alone
does not establish that a check ran. Pending overlays and truncated envelopes
remain explicit incomplete coverage. A complete recheck can resolve the same
scope's incomplete materialization record.

The existing `strict_verification_gate` remains opt-in. When enabled for an
action-required task requiring verification, the Coordinator checks canonical
records against the shared verifier clock and current checked bytes. Missing,
failed, incomplete or stale checks prevent Completed. A refused settle leaves
work held for continuation and re-enters the Coordinator decision loop on
resume; it cannot become a permanent all-nodes-complete dead end.

Workspace checks carry a versioned source-tree fingerprint. The service compares
complete bounded source bytes before and after checking; check caches also key
on those bytes. A write during checking removes the record's evidence-grade
fingerprint. Later mutations invalidate old checks. File and workspace reads are
bounded, cancellable and reject unknown coverage. Existing run attribution and
legacy C-06 policy remain in place when strict checking is disabled.

The implementation and acceptance index is
[`orchestration-completion-checklist.md`](orchestration-completion-checklist.md).
The PR's CI checks establish validation; the new test uses a production
specialist and native tools with a scripted provider, not a live-model claim.
