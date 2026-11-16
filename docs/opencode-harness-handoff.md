# OpenCode handoff — Concerto harness reliability

Checkpoint requested by the user on 2026-11-16, updated after the H39 root
milestone and H40/H41 were published and verified. Resume from this document
and the work logs; do not restart completed work or infer that the whole goal
is done.

## Start here

**H39 root, H40 and H41 are published and verified.** H39: strict cargo-audit
passed (exit 0, 1,509 advisories, 1,079 dependencies) and the squashed commit
`2dbf25a` plus ADR-acceptance commit `72c414a` passed all twelve exact-head
checks; ADR-87 is Accepted for the root boundary. H40: headless multi-agent
runs now receive typed history (no clipped actual-task prose) with the ADR-60
D7 approved-plan boundary pinned by a routing control. H41: the settings CLI
fixture is Linux-gated to match its XDG-only isolation. The squashed H40/H41
commit `91bf006` passed the 12/12 local battery and all twelve exact-head
checks; the documentation-record commit `d4faeb4` is the verified PR head and
also passed all twelve. Evidence: `target/harness-reliability/h40-final-*`,
`h40-attempt1/`, and the work log.

[PR #379](https://github.com/NefaroXX/Concerto/pull/379) targets `dev`, is open
and draft, and its head is `d4faeb4` (H40/H41 publication plus the docs-record
commit). Do not merge without authorization. Remaining work is the
rest of H39: specialist/process-scoped durable history and cancelled-run spend
settlement.

## Git and worktrees

| Location | Branch | State |
| --- | --- | --- |
| `/home/sol/Projects/Concerto` | `checkpoint/opencode-harness-2026-11-16` | Local documentation-only handoff branch; PR branch is at the H40/H41 head |
| `/home/sol/Projects/Concerto-worktrees/h39` | `fix/harness-transcript-continuity` | `72c414a`; H39 root milestone published on `fix/harness-streaming-integrity`; ADR-87 Accepted |
| `/home/sol/Projects/Concerto-worktrees/h40` | `fix/harness-headless-history` | H40/H41 published: code `91bf006`, docs-record commit on top; local battery 12/12, exact-head 12/12 on `91bf006` |

Dedicated local refs `checkpoint/opencode-h39-2026-11-16` (pre-squash WIP
`a88e841`), `checkpoint/opencode-h40-2026-11-16` (`950a722`) and
`checkpoint/opencode-h40-rebased` (`4ea2018`) preserve the checkpoints after
each topic branch was squashed for publication. All worktrees are clean.

The detailed current H39 work log is in the **H39 worktree** at
`docs/harness-reliability-work-log.md`. Its investigation is
`docs/harness-transcript-investigation.md`, and its contract is
`docs/adrs/ADR-87-root-provider-transcript.md` (Accepted for the root boundary).
H40/H41 is recorded in `docs/harness-headless-history-investigation.md`.

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
- Subagents are authorized (user direction, 2026-11-16); the orchestrator may
  delegate review, docs and verification. Read repository/subdirectory
  instructions and relevant skills before editing. Do not run another Cargo
  job while one is
  live. An expired tool observation handle alone does not establish completion;
  inspect processes and logs before restarting.
- The code graph has no useful Concerto index. CCC indexes the main checkout,
  not these isolated worktrees; use fallback searches for unindexed changes.
  Refresh the main index after promotion.

## H39: published root milestone

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
| Strict cargo-audit | **Passed on rerun: exit 0, 1,509 advisories, 1,079 dependencies; no yanked-check timeout** |

The first strict run failed with a registry yanked-check timeout; that
diagnostic is retained at `docs/verification/h39-audit-checkpoint.txt`. Rerunning
only that gate on the unchanged candidate passed, without `--no-yanked`,
`--no-fetch`, new exceptions or an exit-code-only assertion. Log:
`target/harness-reliability/h39-audit-resume-1.log`, committed copy at
`docs/verification/h39-audit-resume-1.txt`.

Source integrity was rechecked against the battery snapshot: **1,013 of 1,014
hashes match** `h39-final-source-sha256.json`. The single difference is
`docs/harness-reliability-work-log.md`, updated after the battery finished (the
two audit evidence files are the other post-run additions). No production or
test source changed, so the battery was not repeated.

The squashed commit `2dbf25a` was pushed to `fix/harness-streaming-integrity`;
all twelve exact-head checks passed in runs `43910428429` (format, clippy,
tests, build, benchmarks, WASM, cargo-deny, cargo-audit, palette) and
`43910428481` (Linux/macOS/Windows native). The ADR-acceptance commit `72c414a`
passed all twelve again in runs `43912024136` and `43912024403`. The PR #379
body now carries these exact results.

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
- `pr-body-h39-draft.md`: the published PR #379 body as of `72c414a`.
  `h39-commit-message.txt` was the final squashed commit message.

Do not rerun the old battery script in place and overwrite these evidence
files. If a later change to H39 is made, run only the checks that change
requires, then the full CI battery, and verify all twelve remote checks on the
exact new SHA. Do not inherit the green result from `2dbf25a`/`72c414a`.

## H40 and H41: published and verified

- H40's request-level baseline is preserved in
  `target/harness-reliability/h40-baseline-headless-history.log`: the real
  headless CLI's provider request carried a system message plus one user
  message with `<conversation_history>` prose; the long-preamble constraint and
  older obligation were gone and the prior assistant turn had no role. Earlier
  harness-only failures are kept as `h40-baseline-cli*.log`.
- Production repair: `run_multi_agent` attaches `conversation_history` as
  typed messages whenever no approved plan is bound, independent of the
  interactive inbox; the submitted task description is the caller's objective.
  Approved-plan runs retain ADR-60 D7 and get no transcript. The decision is
  isolated in `conversation_history_for_run` and pinned by
  `conversation_history_is_injected_only_without_an_approved_plan`.
- The clipped-prose `multi_agent_task_with_history` helper and its helper-only
  tests were deleted. Replacement coverage: the loopback headless CLI fixture
  (durable constraints, one assistant-role prior reply, no flattened marker in
  the task) and the coordinator parity control with/without an inbox.
- H41 is the Linux-only gate on `settings_commands.rs` (and the new CLI
  fixture). The macOS hazard is source-confirmed, never reproduced against a
  real profile, and portable fixture coverage is not claimed.
- Evidence: first battery `81fe15a` 11/12 (transient audit yanked-check timeout,
  preserved as `h40-final-full-audit.first-attempt.log`, resumed strict gate
  passed); final battery `8f2853e` 12/12 in 34 m 02 s with 1,019/1,019 source
  hashes. Published as `91bf006`; all twelve exact-head checks passed (CI
  `43942883154`, native `43942883053`). The docs-record commit `d4faeb4` is the
  current head and also passed all twelve checks (CI `43944565310`, native
  `43944565347`).

## Remaining findings

Seven findings remain open: H12 event-queue memory bounds; H14 intermittent
checkpoint ToolNotFound; H23 MCP probe timeout cause; H28 unused dependencies /
67 duplicate families; H29 declared Rust minimum; H34 unmarked incomplete
subprocess capture; H39 specialist/process history and cancelled-run spend.
Full status/history is in the work log. Passing tests do not establish overall
harness superiority, and lost historical state cannot be reconstructed.
