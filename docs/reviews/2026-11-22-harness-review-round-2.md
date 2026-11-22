# Harness reliability review round 2

Reviewed **PR #380 at `90afd73207472c9da9c65d67d81b813ab664d4bd`**, comparing
the five follow-up commits with round one's `fae4e80`. This report follows
[the first review](2026-11-21-harness-review.md); it is a targeted review of
those corrections and their integration paths, not a new whole-codebase audit.

**Recommendation: hold approval.** Three original findings are resolved.
Failed-specialist accounting is improved but still loses paid text-tool
repair requests. A separate attempt-isolation defect can double-charge
fallback usage. Runtime recording integrity remains unresolved.

## Findings requiring changes

### R4 follow-up — P1 — Failed text-tool repair still loses paid usage

**Location:** [freeform.rs:189–242](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/agents/generic/freeform.rs#L189)
and [generic.rs:594–725](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/agents/generic.rs#L594).

The new usage accumulator runs only after `freeform_driver_turn` succeeds.
That helper can make up to five repair requests after an initial paid
completion. If a repair request fails, or all completed replies remain
malformed, it returns an error before the accumulator sees any of them.
`AgentRunner` then settles zero for those requests and skips their failed-attempt
spend row because `measured_requests` is zero.

The review regressions execute the real `GenericSpecialistAgent` with a
scripted provider reporting 1,000 input tokens, 200 output tokens and $0.012
per completed request:

| Scenario | Completed paid requests | Expected cost | Recorded cost |
| --- | ---: | ---: | ---: |
| Native tool turn, then provider authentication failure | 1 | $0.012 | $0.012 |
| Malformed text-tool reply, then repair request authentication failure | 1 | $0.012 | $0 |
| Malformed replies exhaust all five repair attempts | 6 | $0.072 | $0 |

The native control passes. Both text-tool cases fail their usage assertions;
the original error still propagates correctly. This is a remaining case of
R4, not a claim that the native-path fix failed. The consequence remains
understated session spend and budget available for further dispatches despite
consumed provider usage.

**Required correction:** account for each completed provider request at its
completion boundary, including internal repair requests. Use that accumulator
for both successful and failed settlement without counting merged repair
usage a second time. Add priced-provider regressions through the real
specialist and runner, including durable attribution and the subsequent budget
check; the current runner regression manually calls `record_usage` and cannot
detect this missing producer call.

### R6 — P2 — Cloned fallback contexts can charge earlier failed attempts again

**Location:** [agent_runner.rs:262–300](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/agent_runner.rs#L262),
[AgentHistory shared accumulator](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/core/src/types.rs#L2344)
and [fallback context reuse](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/coordinator.rs#L6308).

`AgentHistory::clone` shares its usage accumulator. The runner creates a fresh
history only after a successful session-store history load. With no attached
store, it retains the caller's accumulator; the history-load error branch also
does not replace it. The coordinator's fallback ladder passes clones of one
context into successive attempts.

Consequently, two failed attempts using that context can settle the first
attempt's cost twice: the first settles its own $0.01, while the second reads
the cumulative $0.02 and settles all of it again. The resulting $0.03 total
overstates $0.02 of actual usage and can refuse a later dispatch prematurely.
A successful store load masks this defect by replacing the accumulator, so
this finding concerns the optional-store/degraded-history path.

**Verification:** the isolated regression reproduced **$0.03 settled for two
$0.01 attempts**. It uses the real `AgentRunner`, its default absent store,
and the candidate's `UsageThenFailAgent` with cloned contexts. The
history-load-error branch is source-traced, not separately fault-injected.

**Required correction:** initialize attempt-owned usage and produced-message
state unconditionally at runner entry, preserving only the intended seed.
Do not rely on a successful persistence read to establish accounting scope.
Test sequential fallback attempts, concurrent clones, and history-load failure
as well as the normal attached-store path.

### R5 — P2 — Normal runtime recording gaps still have no durable status

**Location:** [recorders.rs:35–87](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/runtime_runner/recorders.rs#L35)
and [normal runtime recorder construction](https://github.com/NefaroXX/Concerto/blob/90afd73207472c9da9c65d67d81b813ab664d4bd/crates/orchestrator/src/runtime_runner.rs#L1534).

Unchanged from round one. Queue overflow is logged by `GapLog`; recorder
guards return no integrity result to settlement. An execution can therefore
finish with incomplete persisted events/transcript and no durable incomplete
recording status. The optional CLI event export's `events_dropped` reports its
own subscription, not the session recorder's queue. The separate benchmark
recorder's fail-closed behavior does not cover these normal runtime recorders.

**Required correction:** propagate recorder health and persist an explicit
recording-integrity outcome, separately from task execution outcome. Test
overflow of each runtime recorder while the optional event export keeps up.
This finding is source-confirmed; no new overflow stress test was run here.

## Findings resolved

| Round-one finding | Status and evidence |
| --- | --- |
| R1 — distinct valid agent IDs share files | Closed. Bounded readable prefixes now include a stable hash of the complete ID; preparation checks for duplicate paths before writing. `colliding_agent_ids_get_separate_paths_histories_and_metrics` verifies separate seeds, append/harvest and attributed metrics for `review.v1` and `review_v1`. Duplicate-path exclusion is also tested. |
| R2 — interrupted multi-tool batches remain invalid | Closed. Both specialist restore and supervised child restore use `restore_tool_responses`; completed results remain intact, missing outcomes are request-only, and raw child evidence is harvested without trimming. Tests cover partial batches, trailing observations, repair idempotence, repaired seed offsets, and unchanged durable history. |
| R3 — approval pause drops completed exchanges | Closed for the reported path. The freeform pause exit records the current delta before returning. The real specialist fixture executes a write, pauses the next tool, and retains the assistant batch plus the write result; the unresolved call is repaired only for replay. |
| R4 — failed specialist refunds all consumed usage | Partially resolved. Native completion usage now survives a later error. The runner test verifies attributed durable spend and cap refusal; the independent real-specialist native control passes. The text-tool repair cases above remain open. |
| R5 — runtime recorder overflow is log-only | Open, unchanged. |

The new path-isolation test simulates child file operations through the real
preparation/harvest helpers; it does not launch two child processes. The
approval-pause fixture inspects the real specialist's produced history and
repair result; it does not itself reopen SQLite and capture the next provider
request. Existing runner/store tests exercise persistence separately. Those
limits do not invalidate the source-level corrections, but should not be
described as complete process-restart coverage.

## Verification

- Independently checked all **12 GitHub checks passed on `90afd732…`**.
  The [main CI run](https://github.com/NefaroXX/Concerto/actions/runs/44975965542)
  covers formatting, clippy, workspace tests, build, benchmark smoke, WASM,
  cargo-deny, cargo-audit and palette/script checks. The
  [native-shell run](https://github.com/NefaroXX/Concerto/actions/runs/44975965575)
  covers Linux, macOS and Windows. Both workflow records identify the reviewed
  head SHA; this does not reuse round one's green results.
- Independently reran the unchanged candidate's orchestrator library suite:
  **1,960 passed, zero failed, one ignored**. The ignored test is the explicitly
  marked expensive `fault_injection::large_fanout_with_cycles` stress scenario.
  Command: `cargo test -j1 -p concerto-orchestrator --lib`, with
  `RUSTFLAGS="-D warnings"`, `CARGO_INCREMENTAL=0`, `TMPDIR=/var/tmp` and the
  main repository's disk-backed `target/`. No compiler warnings were reported.
- The full local CI battery was not rerun in this review. Remote green CI and
  the existing suite do not cover the new counterexamples.
- The four review probes compiled with warnings denied: **one control passed,
  three safety assertions failed** (exit 101), reproducing the two accounting
  defects above. These are additional failing regressions, not failures in
  the unchanged candidate's existing test suite. The source was restored and
  the candidate worktree verified clean after the probe run.
- [Probe runner](2026-11-22-harness-probes.py),
  [specialist fixtures](2026-11-22-harness-probes.rs), and
  [runner fixture](2026-11-22-harness-runner-probes.rs) are review artifacts,
  outside Cargo's normal test discovery. The runner pins the candidate SHA,
  requires a clean worktree, temporarily adds test-module declarations and
  restores the original source in `finally`. Production bodies are unchanged.
  Invoke `python3 docs/reviews/2026-11-22-harness-probes.py /path/to/candidate`
  while its worktree and the shared Cargo target are idle.
- Retained logs and source/fixture hashes live under
  `target/harness-review-2026-11-22/`: `orchestrator-lib.log`, `probes.log` and
  `probes.json`. The first three-probe run is retained as `probes-initial.*`.
  The specialist probes use the actual loop with a scripted priced provider;
  the runner probe uses the actual runner with a usage-reporting mock agent.
  They are complementary boundary tests, not a single end-to-end scenario.

Previously accepted residuals from the handoff remain unchanged: supervised
flush/crash windows, same-role concurrent history, stored-history growth and
the documented supply-chain exceptions. This report does not reopen them or
claim a warning-free supply-chain policy. No production fix, PR comment,
approval or merge was performed as part of this review.
