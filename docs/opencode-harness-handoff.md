# OpenCode handoff — Concerto harness reliability

Checkpoint requested by the user on 2026-11-16. Resume from this document and
the work logs; do not restart the audit or infer that the whole goal is done.

## Start here

The latest candidate fixes root conversation persistence. **Do not publish it
yet:** the final strict cargo-audit gate failed because the registry yanked
check timed out. Every other local gate passed. The runner has terminated;
there is no background validation to wait for. Rerun the unchanged strict audit,
preserving its new log, before deciding whether publication is ready.

The existing draft [PR #379](https://github.com/NefaroXX/Concerto/pull/379) targets
`dev`. Its published head remains `14390eb0a54581ca9deee7562f2d069f974cd8d9`,
with all twelve remote checks verified green. Neither checkpoint below is
pushed. No PR, policy gate, exception list or remote branch was changed for
this handoff.

## Git and worktrees

| Location | Branch | State |
| --- | --- | --- |
| `/home/sol/Projects/Concerto` | `checkpoint/opencode-harness-2026-11-16` | Documentation-only handoff on published `14390eb`; original PR branch remains at `14390eb` |
| `/home/sol/Projects/Concerto-worktrees/h39` | `fix/harness-transcript-continuity` | Local checkpoint `a88e8419ffdfb8f6d771f285f4e8e13681cef037`; root repair, tests, ADR and detailed log |
| `/home/sol/Projects/Concerto-worktrees/h40` | `fix/harness-headless-history` | Local checkpoint `950a722b63299c83de48cce2720a28e33bcba31f`; prepared regressions and H41 fixture correction, not verified |

Dedicated local refs `checkpoint/opencode-h39-2026-11-16` and
`checkpoint/opencode-h40-2026-11-16` preserve those exact checkpoints if the
working topic branches are rebased or their local WIP commits are rewritten.
These are intentional WIP checkpoints, not claims of completed verification.
All three worktrees should be clean after the handoff commit.

The detailed current H39 work log is in the **H39 worktree** at
`docs/harness-reliability-work-log.md`. Its investigation is
`docs/harness-transcript-investigation.md`, and its contract is
`docs/adrs/ADR-87-root-provider-transcript.md` (still Proposed).
H40 preparation is in its own `docs/harness-headless-history-investigation.md`.

## Working requirements

- The user's starting codebase passed. Own failures introduced during this
  work; never dismiss them as pre-existing. Do not ignore warnings or leave
  published CI red. Preserve failing evidence and fix the cause.
- Keep the work log current. Commit and push completed, verified milestones
  through the existing topic PR to `dev`. Do not work on `main`, merge without
  authorization, or force-push a shared branch.
- Match the full CI battery locally before publishing changed code. Then
  verify all twelve remote checks on the **exact new SHA**, including the
  Linux/macOS/Windows native matrix. An old green head is not evidence for a
  new commit.
- Serialize local Cargo compilation/security gates, use `-j1`,
  `RUSTFLAGS="-D warnings"` and `CARGO_INCREMENTAL=0`. Keep
  `CARGO_TARGET_DIR=/home/sol/Projects/Concerto/target` and `TMPDIR=/var/tmp`.
  Never build/test/clone under RAM-backed `/tmp`. Do not repurpose HOME.
- No subagents were authorized. Read repository/subdirectory instructions and
  relevant skills before editing. Do not run another Cargo job while one is
  live. An expired tool observation handle alone does not establish completion;
  inspect processes and logs before restarting.
- The code graph has no useful Concerto index. CCC indexes the main checkout,
  not these isolated worktrees; use fallback searches for unindexed changes.
  Refresh the main index after promotion.

## H39: implemented candidate

The root `AgentLoop` and coordinator dispatch loop now persist completed
assistant turns before tools and each completed tool result before later work.
Reasoning, signatures and Gemini native replay survive store reload. Initial
submissions and inbox-delivered user messages keep their existing persistence
owners; rendered replies/errors remain in the separate typed transcript.

Each original assistant turn owns its reported usage. Completed text-driver
repair requests are counted even if a later request fails. Usage resets between
external runs and remains cumulative across internal continuation rounds.
Settlement no longer writes synthetic assistant summaries with duplicate usage.

`provider_transcript.rs` provides bounded cleanup writes with a fresh cancellation
domain, a ten-second deadline and cancellation on drop. It never retries
ambiguous writes. Context assembly groups recorded tool responses and marks
missing results as request-only `outcome_unknown` responses. It does not execute
historical calls or persist invented results.

Review caught a defect in the first repair: removing summary rows also removed
post-provider verification and stop reasons. Two new regressions failed first.
The final candidate retains these facts as labeled system observations with no
usage; original assistant turns remain intact. Errors/cancellation retain their
reason through the cleanup path. Full verifier output remains in evidence stores.

Key files: `agent_loop.rs`, `coordinator.rs`, `provider_transcript.rs`,
`context_engine.rs`, `runtime_runner.rs`, `conversation/reply.rs`; regressions
are in `agent_loop_transcript_regressions.rs` and
`coordinator_transcript_regressions.rs`. `SqliteSessionStore::connect_path` is
now public for explicit plaintext paths; implementation/schema are unchanged,
and encrypted databases retain their existing guarded open path.

**H39 remains open beyond this root milestone:** independent specialists and
supervised children need agent-scoped durable history. The supervised child's
real `AgentLoop` currently receives neither a session store nor prior messages.
Do not merge specialist histories into the root stream. Runtime metrics/spend
settlement also receives the cancelled run token; its cancellation durability
still needs reproduction and repair. Root message usage tests do not prove
spend-table durability. No exactly-once side-effect/database guarantee is claimed.

## H39 verification: exact results

Final battery state is in main checkout
`target/harness-reliability/h39-final-status.json`:

| Check | Result |
| --- | --- |
| Workspace unit/integration/doc tests | **6,149 passed, 0 failed, 17 ignored; 86 groups** |
| Focused corrected orchestrator suite | 1,915 passed, 0 failed, 1 ignored |
| Explicit released llvm-cov/tarpaulin fixture | Passed |
| Formatting, diff, warning-denied all-target Clippy | Passed |
| Workspace, desktop-only and CLI-only builds | Passed |
| Launcher/cache-report/CLI smokes | Passed |
| Four WASM guests and wasmtimer WASM check | Passed |
| Nine release benchmark smoke targets | Passed |
| Palette and Python checks | Passed; 13 Python tests |
| Cargo-deny | All four gates passed; **67 duplicate warnings remain tracked as H28** |
| Strict cargo-audit | **Failed, exit 1: registry timeout; incomplete verification** |

Audit loaded 1,509 advisories and scanned 1,079 dependencies, then emitted:

```text
error: couldn't check if the package is yanked: registry: request could not be completed in the allotted timeframe
error: cargo-audit reported diagnostics despite exit zero; audit is incomplete or unclean
```

The strict wrapper behaved correctly. Do not bypass the failure with
`--no-yanked`, `--no-fetch`, new exceptions, or an exit-code-only assertion.
The diagnostic is committed in the H39 worktree at
`docs/verification/h39-audit-checkpoint.txt`.

All **1,014 source hashes matched after the runner finished**, before the
checkpoint work-log update and audit evidence file were added. No production
or test source changed during validation. The checkpoint's only subsequent
changes were those documentation/evidence additions.

Preserved artifacts under main `target/harness-reliability/`:

- `h39-final-validate.py`, `h39-final-ci-extra.sh`: exact serial battery.
- `h39-final-source-sha256.json`, `h39-final-validated.patch`: tested source.
- `h39-final-full-*.log`, `h39-final-{desktop,cli,wasm,bench,...}.log`: results.
- `h39-baseline-*`, `h39-request-baseline-*`: expected failing reproductions.
- `h39-attempt1/`, `h39-attempt2/`: corrected Clippy failures, no suppressions.
- `h39-outcome-review-interrupted/`: deliberately stopped earlier candidate;
  no workspace success is claimed for that attempt.
- `h39-runtime-outcome-baseline.log`, `h39-runtime-outcome-repair-2.log`:
  failing then passing verification/stop-reason regressions.
- `pr-body-h39-draft.md`: **unpublished draft with pending-verification text**;
  replace placeholders before using. `h39-commit-message.txt` is a proposed
  final message, not the WIP checkpoint's verification claim.

Do not rerun the old script in place and overwrite these evidence files. First
rerun only the incomplete strict gate on unchanged code, with a new log:

```bash
cd /home/sol/Projects/Concerto-worktrees/h39
env TMPDIR=/var/tmp CARGO_TARGET_DIR=/home/sol/Projects/Concerto/target \
  RUSTFLAGS="-D warnings" CARGO_INCREMENTAL=0 \
  python3 scripts/check-cargo-audit.py \
  > /home/sol/Projects/Concerto/target/harness-reliability/h39-audit-resume-1.log 2>&1
```

Inspect both exit status and log. If a substantive finding appears, fix it and
run the checks required by that change. With unchanged source and a clean
strict audit, retain the already-passing battery evidence; do not repeat it
merely because a tool observation handle expired. Update the log/ADR and PR
description accurately, promote H39 onto `fix/harness-streaming-integrity`,
publish, then watch all twelve checks on the exact head. The prior H38 runs were
`43785417943` (main CI) and `43785417957` (native matrix); these are not H39 runs.

## H40 and H41: prepared, not executed

The H40 branch is still based on `14390eb` plus its WIP checkpoint. Integrate the
verified H39 commit before compiling: the new CLI fixture uses H39's public
`connect_path` API. Do not mistake its current dependency on that missing API
for an implementation regression.

- Two wired helper probes already reproduced the 4,000-character per-message
  and 16,000-character total cutoffs. Original failing evidence is retained.
- `crates/cli/tests/headless_history.rs` is new and **unexecuted**. It seeds a
  real isolated session, starts the actual headless CLI against a bounded
  loopback OpenAI server, checks the JSON/session identity and captures the
  request. It asserts long and older obligations, original assistant role,
  and no duplicate flattened history inside the submitted task. It uses a
  test-only credential, nonce keyring account, Linux XDG directories and a
  proxy bypass for loopback. No live provider is required.
- `coordinator_history_regressions.rs` is **unwired and unexecuted**. Include it
  in the coordinator test module after integration. It captures real requests
  with and without an inbox as a parity control for typed history.
- Production H40 routing is unchanged. `run_multi_agent` attaches typed
  history only when an inbox exists and there is no approved plan; headless
  runs instead receive clipped prose through `multi_agent_task_with_history`.
  Preserve the actual request-level failing baseline before fixing this.
- Ordinary headless runs should get typed history without duplicating it in
  task prose or manufacturing an interactive inbox. Approved-plan runs must
  retain ADR-60 D7's separate verified-artifact/ledger contract, excluding old
  rendered plan history. Review the D7 tests and add appropriate controls.
- H41: `settings_commands.rs` used `cfg(unix)` with only XDG isolation, which
  can reach the real macOS profile through `dirs` 6.0.0. Its prepared correction
  restricts it to Linux, as does the new CLI fixture. This is source-confirmed;
  no mutating test was run against a real macOS profile. Keep Linux behavioral
  coverage and do not claim portable fixture coverage from this correction.

After the actual CLI baseline, implement and verify the routing change, retain
meaningful replacement coverage if deleting the clipped helper/tests, run the
full battery, and publish only after the H39 head is green.

## Remaining findings

Nine findings remain open: H12 event-queue memory bounds; H14 intermittent
checkpoint ToolNotFound; H23 MCP probe timeout cause; H28 unused dependencies /
67 duplicate families; H29 declared Rust minimum; H34 unmarked incomplete
subprocess capture; H39 specialist/process history and cancelled-run spend;
H40 headless history; H41 settings-fixture platform isolation pending verification.
Full status/history is in the work log. Passing tests do not establish overall
harness superiority, and lost historical state cannot be reconstructed.
