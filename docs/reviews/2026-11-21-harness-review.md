# Concerto harness reliability review

Reviewed 2026-11-21. Candidate: **`fae4e80d4ec7a3f79323989598801f11cafaa058`**,
[PR #380](https://github.com/NefaroXX/Concerto/pull/380), targeting `dev`.

**Recommendation: hold approval pending the four P1 findings below.** The work
since the handoff substantially improves recovery and verification, and all
twelve GitHub checks are green. However, distinct supervised agents can share
history files, interrupted tool batches can restore invalid conversations,
approval pauses can lose completed specialist exchanges, and failed in-process
specialists can consume tokens without charging the spend tracker. These are
additional gaps, separate from the residuals accepted in the handoff.

## Scope and evidence

This is a focused review of the reliability changes since `14390eb`, especially
H39/H40 history ownership, interruption, accounting and event recording. The
comparison contains 101 changed files. It is not an exhaustive re-audit of every
crate or a comparative benchmark against other harnesses.

The reviewed code is in `/home/sol/Projects/Concerto-worktrees/h40`. The main
checkout remains a documentation branch containing older implementation code;
reviewing only that checkout would miss the current candidate. Source links
below pin the candidate SHA so they remain valid across branch changes.

| Evidence | Result |
| --- | --- |
| Current PR state | Open and unmerged at `fae4e80` |
| [Main CI](https://github.com/NefaroXX/Concerto/actions/runs/44958751032) | Nine successful jobs on the exact candidate SHA |
| [Native matrix](https://github.com/NefaroXX/Concerto/actions/runs/44958750982) | Linux, macOS and Windows successful on the same SHA |
| Retained local workspace log `h40-ci2-test.log` | 6,219 passed, zero failed, 17 ignored across 87 groups |
| New isolated review probes | Compiled with warnings denied; three expected assertion failures exposing R1/R2 |
| R3–R5 | Source-traced findings; end-to-end reproductions remain required |

The local full battery was **not rerun during this review**. Its retained log
belongs to the prior validation; between its recorded code revision `494efd4`
and the reviewed head, only `AGENTS.md` and the work log changed. Both remote
workflow heads were independently checked during this review.

## Findings

### R1 — P1 — Distinct valid agents collide on supervised history and metrics files

**Evidence:** [path construction and sanitization](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/supervised_history.rs#L76-L101),
[agent ID validation](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/config/src/agents.rs#L171-L184),
and [per-agent seed creation](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/runtime_runner.rs#L687-L703).

`review.v1` and `review_v1` are distinct, valid configured IDs. Both become
`review_v1.history.json` and `review_v1.metrics.jsonl`. The parent writes each
seed to that same path, and both children subsequently read/write it. Harvest
then reads the shared files separately under each agent identity.

This can overwrite one agent's seed, lose concurrent appended turns, expose
another agent's conversation, and duplicate or misattribute spend. It is not
the accepted same-role concurrent-dispatch limitation: these are different
configured agents. The isolated probe confirms both IDs pass the production
validator and produce identical paths.

**Fix:** derive filenames from a collision-resistant encoding of the complete
identity, or allocate unique per-agent directories and retain the mapping.
Check for duplicate physical paths before spawning. These files are ephemeral,
so the database identity need not change.

**Required regression:** seed two valid colliding IDs with different histories,
run both children, and assert separate restore requests, appended turns and
exactly-once attributed spend.

### R2 — P1 — Recovery leaves partially answered tool batches invalid

**Evidence:** [tail trimming](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/supervised_history.rs#L104-L114),
[child restoration](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/supervised_history.rs#L190-L200),
and [direct initial-message injection](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/bin/agent_process.rs#L233-L240).

The trimming function only removes messages while the **last message itself**
has `tool_calls`. For the realistic interrupted sequence
`Assistant(calls A, B) → Tool(result A)`, it removes nothing. B remains
unanswered. Adding a terminal runtime observation also leaves the sequence
unchanged. Both counterexamples fail the isolated replay-validity probes.

This history can be harvested and restored into an API request that requires a
response for every call. The root context engine's `restore_tool_responses`
repair does not protect these paths: it is called by `ContextEngine::assemble`,
whereas the child injects the restored file directly. The in-process runner
also uses the same inadequate trimmer when persisting specialist history.

**Fix:** validate complete exchanges by call ID and assemble explicit
unknown-outcome responses for unresolved calls, preserving completed results
and signed assistant state. Reuse the root recovery semantics. Do not discard
evidence of executed side effects or automatically rerun old calls.

**Required regression:** interrupt after the first result of a two-call batch,
with and without a runtime observation; restart through both supervised and
in-process paths; capture the actual next provider request and assert valid
pairing, retained native replay and no repeated side effect.

### R3 — P1 — Approval pause can discard completed specialist exchanges

**Evidence:** [recording at iteration entry](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agents/generic/freeform.rs#L125-L128),
[successful result append](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agents/generic/freeform.rs#L528-L541),
[approval-pause return](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agents/generic/freeform.rs#L577-L579),
and [runner persistence](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agent_runner.rs#L305-L317).

In a freeform specialist turn, let call A successfully change a file and call B
return `PausedAwaitingApproval`. A's result and the original assistant batch
have been added only to the local `messages` vector. The immediate return
skips both the next iteration's `record_from` and the normal final recording at
line 631. `AgentRunner` persists only `context.agent_history.produced()`, so
the completed exchange from that iteration is absent from durable history.

This loses the model-visible evidence of work that actually happened at an
ordinary approval boundary. Other audit records may still exist; they are not
substituted into the next specialist request by this path.

**Fix:** record completed assistant turns and individual results at their
completion boundaries, or ensure every early return transfers the accumulated
delta before returning. Pair this with R2's safe unresolved-call handling.

**Required regression:** a real specialist with two calls, the first performing
a verifiable write and the second pausing for approval. Reopen the store and
capture the next request; A's exact call/result must survive and B must remain
unresolved without being silently executed.

### R4 — P1 — Failed in-process specialists refund spend after consuming tokens

**Evidence:** [provider-error return after previous iterations](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agents/generic/freeform.rs#L163-L180),
[zero-cost error settlement](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/agent_runner.rs#L294-L303),
and [successful-result metrics collection](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/coordinator.rs#L10718-L10723).

A specialist can complete one or more paid provider requests and then fail on
a later request. `AgentRunner` derives actual cost from `Ok(AgentRunResult)`;
every `Err` maps to `0.0` and settles the reservation at zero. The coordinator
collects usage from returned results, so the failed attempt does not provide
its actual usage for aggregate metrics/spend settlement either.

The new failed-child metrics sink repairs the supervised `AgentLoop` path,
but does not cover `GenericExpertAgent` through `AgentRunner`. Consequently,
failed in-process attempts can understate spend and allow further dispatches
against a budget already consumed. Persisting message usage alone does not
repair the live spend tracker.

**Fix:** retain completed-request usage in a run-owned accumulator available on
both success and error. Settle the reservation and persist attributed usage
from it once, independently of the agent's terminal result.

**Required regression:** a priced mock provider completes a measured first
turn and fails the second. Assert the original failure is retained, live spend
includes the first turn, durable metrics/spend are attributed once, and a
subsequent dispatch is refused when that spend exhausts the cap.

### R5 — P2 — Normal runtime recorder overflow does not report incomplete persistence

**Evidence:** [log-only gap observer](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/runtime_runner/recorders.rs#L35-L69),
[guard without an integrity result](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/runtime_runner/recorders.rs#L74-L87),
and [normal runtime wiring](https://github.com/NefaroXX/Concerto/blob/fae4e80d4ec7a3f79323989598801f11cafaa058/crates/orchestrator/src/runtime_runner.rs#L1507-L1514).

The bounded queue fixes memory growth. However, the normal runtime's event and
transcript recorders only log observed drops; their guards cannot return an
integrity failure or a durable degraded status. A run can therefore return its
normal completed outcome while those persisted records have gaps.

The separate `concerto_sessions::EventRecording::finish` does fail closed on
overflow, but it is not the recorder constructed at this normal runtime
boundary. Likewise, CLI `events_dropped` records describe its optional export
subscription, not the independent session/transcript subscriptions. A fast
export consumer does not prove that the database recorder kept up.

**Fix:** propagate per-recorder integrity health to settlement and persist an
explicit incomplete-recording status, or reject any result that requires
complete audit evidence. Keep this distinct from whether the underlying task
itself succeeded. Narrow the H12 completion wording until the normal runtime
behavior is covered.

**Required regression:** stall the real runtime recorder, overflow its queue,
then drain and finish. Assert a caller-visible and durable integrity signal,
including when the optional events export has no drops. Test event and
transcript recorders separately.

## Accepted residuals and documentation

The handoff records acceptance of the scoped duplicate dependencies, the
unreproduced checkpoint root cause with mitigation, the supervised flush crash
window, pre-flush child metrics loss, shared same-role in-process streams and
unbounded persisted agent-history rows. This review does not reclassify those
accepted limits as new findings. R1–R5 concern different failure conditions.

The handoff still contains a paragraph asserting that supervised children get
neither a store nor prior messages and that cancelled settlement remains
unimplemented, despite its newer status sections saying these were repaired.
The candidate's `AGENTS.md` likewise calls specialist/process history unfinished.
Reconcile these historical statements after the correctness work so the next
agent does not mistake old scope notes for current behavior.

## Reproduction and next steps

Run the [isolated probe script](2026-11-21-harness-probes.py) against the pinned
candidate:

```bash
python3 docs/reviews/2026-11-21-harness-probes.py /home/sol/Projects/Concerto-worktrees/h40
```

It extracts the unchanged production bodies for path construction, ID validation
and tail trimming, then compiles them with `rustc --test -D warnings`. The
message type is a minimal field-compatible stand-in; this verifies those
algorithms, **not** the full provider/SQLite/process integration. At the reviewed
head the result is **0 passed, 3 failed, exit 101**, as expected for the reported
counterexamples. Generated source, source hashes and output are under
`target/harness-review-2026-11-21/`. The script pins the SHA to prevent silently
testing another revision. These probes are not added to the regular CI suite.

Fix R1–R4 with the end-to-end regressions above, add R5's integrity propagation,
and then run the full local battery and all twelve remote checks on the new
head. Keep the report's distinction between isolated reproduction and
source-traced findings until those integration cases have been executed.

No production code was changed, no PR was merged, and no provider credentials
or live model requests were used for this review.
