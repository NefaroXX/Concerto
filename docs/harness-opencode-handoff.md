# OpenCode handoff — harness reliability, 2026-11-24

The user asked Codex to finish the current major milestone and hand work back
to OpenCode. Continue from the implementation worktree below. Do not start
again from the older review checkout or restore the old remote branch.

## Checkout and publication

- Implementation: `/home/sol/Projects/Concerto-worktrees/r5`
- Branch: `fix/harness-recording-integrity`
- Draft PR: <https://github.com/NefaroXX/Concerto/pull/381>, targeting `dev`.
- Final production/test commit: `dac153bdf127cc6a2624790149c53e1501f48ad2`.
- Initial publication checkpoint: `f45008e6b77f06006fcbf769bdf113413fe33417`
  (adds changelog and recovery documentation only).
- Main checkout `/home/sol/Projects/Concerto` is the separate review branch
  `review/harness-round-2-2026-11-22`, based on review commit `7f546f5`. This
  handoff is committed there. Its review reports and
  reproduction probes are under `docs/reviews/`; it is not the fixed candidate.
- `/home/sol/Projects/Concerto-worktrees/h40` retains the earlier accounting
  checkpoint `88020d0`. PR #380's remote branch was externally rewound to
  `90afd73`; the reason is unknown. Preserve it. PR #381 carries the complete
  candidate without overwriting that branch. Neither PR has been merged.
- Fetched `dev` was `b58b6a6`; a read-only merge-tree check was conflict-free
  and identical to the candidate tree. Recheck if `dev` advances.

## Major work completed

The full finding ledger and dated evidence are in
[the implementation work log](https://github.com/NefaroXX/Concerto/blob/f45008e6b77f06006fcbf769bdf113413fe33417/docs/harness-reliability-work-log.md).
The latest review required three corrections:

1. **R4 — paid repair usage:** account at every completed request boundary,
   including internal text-tool repair requests. Failed/exhausted repair now
   retains usage; successful repair does not price merged usage twice.
   Real specialist/runner/SQLite regressions verify attributed spend after
   reopen and refusal of subsequent dispatch after the budget cap.
2. **R6 — attempt isolation:** each runner invocation gets fresh usage and
   produced-message state while preserving its intended seed, independently
   of session-store presence or history-load success. Sequential fallback,
   concurrent clones and load-failure tests cover the former double charge.
3. **R5 — normal recorder integrity:** event-log and display-transcript
   recorders return health and persist `RecordingFinished` directly outside
   the bounded queue. Outcomes distinguish drops, failed writes, worker
   failure and failed health persistence. Per-session queue filtering avoids
   unrelated traffic; close-before-drain freezes the queue/drop count.
   Writes are bounded at 10 seconds; worker drain at 30 seconds. Cancelled
   cleanup cannot detach the draining worker.

`AgentOutput.recording_integrity` is independent of task completion. Headless
JSON/reports expose `recording.status` (`complete`, `incomplete`, `unknown`)
and the two outcomes. Conversation summaries show incomplete-recording
warnings while preserving the raw model reply. Error/timeout responses with
no `AgentOutput` explicitly report unknown. Recording status covers the
event log and display transcript, not provider-history, spend or policy-audit
persistence.

Key implementation/test paths:

- `crates/core/src/recording.rs`, `event.rs`, `types.rs`
- `crates/orchestrator/src/agent_runner.rs` and `agent_runner/tests/accounting.rs`
- `crates/orchestrator/src/agents/generic.rs`, `generic/freeform.rs`,
  `generic/tests/accounting.rs`
- `crates/orchestrator/src/runtime_runner/recorders.rs`,
  `recorders/health.rs`, `runtime_runner/tests/recording_integrity.rs`
- `crates/cli/src/dispatch/run.rs`, `crates/cli/tests/headless_history.rs`

Security checks also required patched `url 2.5.9` (RUSTSEC-2026-0592) and
locked `cookie 0.18.3` (RUSTSEC-2026-0594). No new advisory exception or direct
cookie dependency was added. Retain both feature-resolved cargo-deny and the
raw-lockfile audit: they catch different dependency sets.

## Verification evidence

Local final battery: **all 18 stages passed** with Rust 1.96.0, warnings
denied, locked dependencies and serial Cargo jobs. Workspace tests:
**6,250 passed, zero failed, 17 ignored**, across 87 unit/integration/doc groups.
Also passed: formatting, all-target clippy, workspace build, desktop-only and
CLI-only builds, launcher/config/cache/CLI smoke, all four WASM guests,
benchmark smoke, palette and Python checks, deny, audit and the complete-audit
wrapper with live advisory/registry checks.

The portable command/result manifest and tested source hashes are committed in
[the verification record](verification/2026-11-24-recording-local.json).

Retained evidence is local (ignored build artifacts), under
`/home/sol/Projects/Concerto/target/harness-reliability/`:

- `round2-recording-release-ci-results.json` — terminal exit codes for 18 stages
- `round2-recording-release-ci-*.log` — complete command output
- `round2-recording-release-source.json` — 24 source/manifest fingerprints
- `round2-recording-release-ci.py` — serialized local driver
- `round2-recording-focused.log` — 95 focused regression tests passed
- `round2-recording-cli-roundtrip.log` — actual CLI/SQLite boundary assertion

After the power interruption, all 24 hashes matched, both working trees were
clean, and no Cargo/validation process remained. The local test-log totals were
independently re-counted. Do not restart a completed battery solely because a
terminal was lost; inspect process state and result files first.

Remote certification of the publication checkpoint is being checked in:

- Main CI: <https://github.com/NefaroXX/Concerto/actions/runs/45443176392>
- Native Linux/macOS/Windows: <https://github.com/NefaroXX/Concerto/actions/runs/45443176381>

Both workflow records identify `f45008e6b77f06006fcbf769bdf113413fe33417`.
At **2026-11-24 17:11 UTC**, seven checks passed: formatting, clippy,
benchmarks, deny, audit, WASM and palette. Five are still running: workspace
tests, debug build and native Linux/macOS/Windows. **No failure was reported;
full remote certification remains pending.** This is the first follow-up for
OpenCode. Re-read PR #381's head and all twelve checks; diagnose any failure
before rerunning it. The handoff lives on the review branch so publishing it
does not cancel or replace these running candidate checks.

## Remaining scope and next steps

1. Confirm PR #381's current head, all twelve check results and mergeability.
   Finish the existing CI watch and record its outcome in the work log.
   Keep it draft until review is complete. Do not merge or force-push.
2. Review R4/R5/R6 against the second review's concrete counterexamples.
   Earlier R1/R2/R3 were already closed in that review. This candidate fixes
   the tracked blockers; green CI is not proof of global harness superiority.
3. Decide with the user what comes after this milestone. Previously accepted
   limits are unchanged: supervised crash-before-flush loss windows,
   concurrent same-role in-process history sharing, stored history growth
   despite the 512-message replay bound, and documented upstream/platform
   dependency duplicates/exceptions. Do not silently reclassify those as new
   regressions or remove safeguards to simplify verification.

Coverage limits: overflow tests exercise the actual recorder guards with
paused mock ordinary writes and real SQLite outcome persistence. Their fast
observer is a separate durable subscription, not the optional CLI export file
writer. The real offline multi-agent CLI fixture separately proves complete
recording and exact stdout-to-SQLite outcome matching after process exit.
Error/timeout cleanup can persist reports without attaching them to a returned
output; callers correctly see unknown in that case.

## Working rules and operational cautions

- Keep the work log current. Own and repair failures; do not dismiss them as
  pre-existing. Do not suppress warnings or incomplete security checks.
- Use topic branches and atomic verified commits; push completed milestones.
  Full local CI-equivalent validation precedes publication; all twelve remote
  checks must belong to the published head. Documentation-only handoff changes
  need a diff check and their own current remote-check verification.
- Cargo environment: `TMPDIR=/var/tmp`, `CARGO_INCREMENTAL=0`,
  `RUSTFLAGS="-D warnings"`, `CONCERTO_TEST_MODE=1`, and
  `CARGO_TARGET_DIR=/home/sol/Projects/Concerto/target`. Serialize local Cargo
  and supply-chain jobs; use `--locked -j1`. Never build/test under `/tmp`.
- The shared target is currently built for **r5**. Switching worktrees with
  overlapping Cargo identities previously reused stale workspace artifacts.
  Clear affected workspace package artifacts or use a separate disk-backed
  target before switching; do not patch source to accommodate stale artifacts.
- Historical failures remain retained: cross-worktree artifact mismatch,
  registry timeouts rejected by the complete-audit wrapper, and the cookie
  advisory. Only the final release-prefixed battery is the complete pass.
- CCC is initialized in r5. Always run it from that worktree and confirm the
  printed project path. An uninitialized parent lookup previously selected
  `/home/sol`; do not index that parent or erase its existing data. The scoped
  refresh was intentionally stopped for handoff at 742 indexed files / 22,040
  chunks, with no indexing errors before stopping; it is **not complete**.
  `ccc daemon stop` ended the daemon and index client (the client reports
  connection lost / exit 1 as a consequence). Data/settings were retained.
  Resume with `ccc index` from r5 when needed. No local Cargo, validation
  driver or CCC indexing process remains running at handoff.
- Respect the root AGENTS.md, policy gates, cancellation contracts and pinned
  dependency exceptions. No subagents were used for these follow-ups.
