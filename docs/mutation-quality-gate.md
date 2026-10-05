# Mutation quality gate (Q06 CI pilot)

An advisory, diff-scoped mutation-testing check for the Q01 pilot file set,
wired into GitHub Actions by `.github/workflows/mutation-gate.yml`.

| Item | Value |
| --- | --- |
| Contract | `CX/20261005-q01-v1/001` (accepted Q01 v1, part 6) |
| Runner (read-only input) | `scripts/mutation-gate.sh` @ **Q02 `d004eb7`** |
| Harness (read-only input) | `tests/mutation_gate/**` @ **Q03 `579e475`** |
| Claim | `OpenCode/sol-20261005-01/Q06/001` |
| Scope of this change | two new files: this document and `.github/workflows/mutation-gate.yml` |

## Enforced vs advisory — read this first

**This workflow is advisory. It is not a required status check, and nothing in
this change makes it one.**

- `.github/workflows/ci.yml` and `.github/workflows/native-shell.yml` are
  untouched: same triggers, same job names, same semantics.
- Required status checks live in repository branch-protection settings, not in
  workflow files. **No branch-protection or required-check change is made or
  implied here.** A red `Mutation gate (pilot)` run is visible to reviewers and
  to Q07, but it does not block a merge today.
- Enforcement is a separate, later decision. It requires the Q07 measurements
  below plus an explicit decision to add the check to the required set — and
  even then the settings change is made deliberately, not as a side effect of
  editing a workflow.

| | Enforced today | Advisory (this change) |
| --- | --- | --- |
| `CI` jobs (fmt, clippy, test, build, bench, wasm, deny, ui-colors) | unchanged | — |
| `Mutation gate (pilot)` fixtures job | no | **yes** |
| `Mutation gate (pilot)` pilot job | no | **yes** |
| Branch protection / required checks | unchanged | unchanged |

## What the workflow runs

Linux only, single `ubuntu-latest` runner, **no matrix, no platform fan-out**.

| Job | Timeout | What it does |
| --- | --- | --- |
| `fixtures` | **15 min** | Runs the Q03 harness (`tests/mutation_gate/run-contract-tests.sh`) against the Q02 runner in disposable fixture repos: caught, missed, baseline, deadline, cancellation, plus the setup/artifact edges. |
| `pilot` | **45 min** | Runs the Q02 runner on the pilot files this change touches, diff-scoped against the PR base, with `--budget-seconds 1800`. |

Runner-minute ceiling: `15 + 45 = 60` runner-minutes per workflow run. Each
job carries its own `timeout-minutes`, so the pair cannot exceed the cap
regardless of which job is slow.

**Triggers**

- `workflow_dispatch` — manual runs. Base is `origin/<default branch>`
  (`origin/dev`).
- `pull_request` on `main`/`dev` with a `paths` filter: the three pilot files,
  plus the gate's own inputs (`scripts/mutation-gate.sh`,
  `tests/mutation_gate/**`, this workflow) so a change to the gate is exercised
  by the run that reads it.

**Checkout and workspace layout**

- `fetch-depth: 0` on the pilot job: the runner resolves
  `merge-base(<base>, HEAD)`, so the PR base commit must exist locally. The PR
  base is `github.event.pull_request.base.sha`; manual runs fall back to
  `origin/<default branch>`.
- The repository is checked out to `$GITHUB_WORKSPACE/repo`. Report and
  scratch directories live at `$GITHUB_WORKSPACE/mutation-gate/{report,scratch}`
  — **workspace-relative and disk-backed (default `GITHUB_WORKSPACE`), never
  under `/tmp`**. The subdirectory checkout is what lets those directories be
  workspace-relative *and* outside the git checkout at the same time, because
  the Q02 runner and the Q03 harness both reject directories inside the
  checkout they drive (and the harness also rejects the checkout's ancestors).
- The pilot run also asserts after the fact that
  `git status --porcelain --untracked-files=all` is empty: the candidate
  checkout must never be mutated in place.

**Pins and determinism**

| Control | How it is pinned |
| --- | --- |
| Rust | `dtolnay/rust-toolchain@1.96.0` (matches `rust-toolchain.toml` and CI) |
| cargo-mutants | `cargo install --locked --version 27.1.0`, then a step asserts the installed binary reports exactly `cargo-mutants 27.1.0` |
| One mutation worker | `CARGO_MUTANTS_JOBS=1` in workflow `env` — cargo-mutants 27.1.0 reads `-j/--jobs` from that variable. The runner builds the tool command itself and does not pass `--jobs`, so the environment is the equivalent hook. |
| Deterministic ordering | cargo-mutants 27.1.0 runs mutants in source order — `--no-shuffle` is its documented default — and the runner passes `--no-config`, so no config file can enable shuffling. |

**Runner caps** (enforced by the runner at `d004eb7`, asserted by the pilot
job's preflight before anything runs):

- global wall clock `--budget-seconds 1800`, then `KILL_GRACE_SECONDS=30` of
  cleanup before the child is killed;
- per-build `MUTANTS_BUILD_TIMEOUT=600` (`--build-timeout`) and per-test
  `MUTANTS_TEST_TIMEOUT=300` (`--timeout`);
- `--baseline run` (the unmutated tree is tested first);
- **no `--in-place`** — cargo-mutants works on copied mutation trees, and the
  workflow re-verifies that the candidate checkout is byte-for-byte untouched;
- reports and logs are uploaded with `actions/upload-artifact` on **success and
  failure** (`if: always()`): artifact `mutation-gate-pilot-report` (pilot) and
  `mutation-gate-fixtures-session` (fixtures).

The preflight is a contract lock: if the pilot trio, a budget, the tool pin or
the baseline/no-in-place flags drift in the runner, the job fails before it
runs and says that a **new contract version plus an explicit ACK** is required.

## How to read `gate-report.json`

The runner writes `<report-dir>/gate-report.json`. In CI it is in the
`mutation-gate-pilot-report` artifact (alongside `console.log`,
`artifacts/<package>/{mutants,outcomes}.json` — the raw cargo-mutants
outcomes — and `scratch/tool-run-*.log`), and its headline fields are echoed
into the run's step summary.

| Field | Meaning |
| --- | --- |
| `schema_version` | Report schema (currently `1`). |
| `contract_id` | `CX/20261005-q01-v1/001`. |
| `tool_version` | Pinned tool version actually used (`27.1.0`). |
| `base_sha`, `head_sha`, `merge_base_sha` | The refs the diff was taken over. |
| `selected_files` | Pilot files the change touches — these were mutated. |
| `uncovered_files` | `.rs` files the change touches that are **not** in the pilot set (including deleted pilot files). Reported, never failed. |
| `skipped_files` | Non-`.rs` paths in the diff (docs, config, workflows). |
| `coverage_scope` | Always `pilot_only`. |
| `status`, `reason` | The verdict and its explanation. |
| `exit_code` | Process exit code — the same value the workflow sees. |
| `tool_exit_codes` | cargo-mutants exit codes per package run. |
| `counts` | `{generated, caught, missed, timeout, unviable}`; any key is `null` when artifacts could not be parsed. |
| `complete` | `true` only for a final, fully parsed verdict. Setup errors, cancellations and budget cuts are `false`. |
| `duration_seconds`, `budget_seconds` | Measured run time against the budget it was given. |
| `artifact_paths` | Files the report directory contains (excluding the report itself). |

`.gate-state/` (decision.env, meta.env, per-run state) is a hidden directory
and `actions/upload-artifact@v4` skips hidden files by default, so it is not in
the artifact; everything needed for the verdict is in `gate-report.json`.

**Rule: exhaustion is inconclusive, never success.** Budget exhaustion yields
`status: inconclusive`, `reason: budget_exceeded`, `exit_code: 3`,
`complete: false`. The same holds for a workflow-level cancel or timeout: the
job goes red. No path maps an exhausted or interrupted run to a pass.

## Failure classes

| Exit | `status` | Typical `reason` | What it means / what to do |
| ---: | --- | --- | --- |
| `0` | `passed` | `all_mutants_caught` | Every pilot mutant was caught. Also the exit for `skipped_no_rust_changes` (`no_rust_changes_since_merge_base`) and `skipped_out_of_pilot` (`rust_changes_present_but_none_selected_for_the_pilot`) — a skip means the gate had **nothing in scope**, not that mutation resistance was proven. |
| `1` | `setup_error` | usage errors, `base_ref_invalid`, `worktree_not_clean`, `report_dir_*`, `scratch_dir_*`, `cargo_mutants_not_found`, `cargo_mutants_version_mismatch`, `tool_exit_1/5/6/70` | The gate never reached a verdict. Fix the environment or the ref; never read it as a pass. |
| `2` | `missed` | `missed_mutants (N missed)` | At least one mutant survived the test suite — a real quality signal on a pilot file. Strengthen tests, then re-run. |
| `3` | `inconclusive` | `budget_exceeded`, `runs_incomplete`, `artifact_parse_failed`, `cargo_mutants_timeout (exit 3)`, `mutant_timeouts (N timeout)`, `no_viable_mutants: <file>`, `tool_unexpected_exit_N`, `external_cancelled (SIGINT/SIGTERM)` | No verdict. Re-run or investigate. **Never a success**, however many mutants were caught before the cut. |
| `4` | `baseline_failed` | `baseline_tests_failed (exit 4)` | The unmutated tree already fails tests. Fix the baseline before any mutant result means anything. |
| `130` | `inconclusive` | `external_cancelled (SIGINT)` | Interrupted (Ctrl-C, or the Actions runner's interrupt on cancellation). Re-run. |
| `143` | `inconclusive` | `external_cancelled (SIGTERM)` | Terminated (job `timeout-minutes`, or `cancel-in-progress` superseding the run). Re-run; if it is the 45-minute cap, that is a measurement input for Q07. |

One non-contract exit exists in the runner's own failure handling: a report
that cannot be generated exits `70`. It is not one of the seven classes above
and must be treated as a failure, never as a pass.

## Pilot scope and out-of-pilot reporting

The pilot target set is fixed by Q01 and never inferred from the diff:

1. `crates/core/src/policy.rs`
2. `crates/core/src/shell_security.rs`
3. `crates/shell/src/parser.rs`

For every path in the diff, the runner classifies it into exactly one list:

| List | Contents | Effect on the verdict |
| --- | --- | --- |
| `selected_files` | Pilot `.rs` files that still exist at HEAD | Mutated; their results decide pass/missed |
| `uncovered_files` | `.rs` files outside the pilot set, plus pilot files deleted by the change | **Reported only** — never fails the gate |
| `skipped_files` | Non-`.rs` paths | Ignored |

`coverage_scope: "pilot_only"` is a scope statement, not a score: a pass claims
mutation resistance for the touched pilot files and **nothing else**. Out-of-
pilot Rust changes are visible in `uncovered_files` so reviewers can see
exactly what the gate did not cover; they are not silently dropped and they do
not turn the run red.

## Runtime caps — proposed, not measured

Every number below is a **proposal from the contract**, chosen so that the
inner budget fires before the outer one (setup ≈ 5 min + 1800 s budget + 30 s
grace < 45 min job cap). None of them has been measured on a GitHub-hosted
runner. Do not quote them as observed performance.

| Cap | Proposed value | Enforced by |
| --- | --- | --- |
| Fixtures job | 15 min | `timeout-minutes` in this workflow |
| Pilot job | 45 min | `timeout-minutes` in this workflow |
| Workflow total | ≤ 60 runner-min | the two job caps (15 + 45) |
| Global runner budget | 1800 s | `--budget-seconds 1800` |
| Budget kill grace | 30 s | runner `KILL_GRACE_SECONDS` |
| Per-build timeout | 600 s | runner `MUTANTS_BUILD_TIMEOUT` |
| Per-test timeout | 300 s | runner `MUTANTS_TEST_TIMEOUT` |
| Workers | 1 | `CARGO_MUTANTS_JOBS=1` |
| Ordering | source order | cargo-mutants 27.1.0 default (`--no-shuffle`) |
| Rust / cargo-mutants | 1.96.0 / 27.1.0 | toolchain action, `cargo install --locked`, verify step |

No cargo build cache is configured, so every run is a cold run (a warm-cache
variant can be added later, but it changes what "cold/warm" means and therefore
belongs to the same measurement exercise as everything else below).

## Before this gate may be enforced (Q07)

Q07, not Q06, owns the go/no-go. Before anyone adds this check to the required
set, Q07 must:

1. **Measure cold and warm runtime** on GitHub-hosted runners for both jobs
   (including `cargo install --locked` of the pinned tool), and confirm the
   proposed 15 min / 45 min / 60 runner-minute caps against real numbers.
2. **Verify every verdict class end to end** in CI:
   - *positive* — a pilot change whose mutants are all caught → exit `0`,
     `status: passed`;
   - *missed* — a surviving mutant → exit `2`;
   - *baseline* — a failing unmutated tree → exit `4`;
   - *timeout* — budget or per-test exhaustion → exit `3`;
   - *cancel* — interruption → exit `130`/`143`;
   - *uncovered* — an out-of-pilot `.rs` change → reported in
     `uncovered_files`, gate still conclusive.
3. Confirm that an exhausted budget is reported as inconclusive and **never**
   as success, and that cancellation leaves a red check rather than a green one.

Until all of that is recorded, the workflow stays advisory.

## Changing a target or a budget

The pilot file set and every budget/timeout in the table above are part of
contract `CX/20261005-q01-v1/001`. Changing either requires **a new contract
version and an explicit ACK** — an edit to `scripts/mutation-gate.sh` alone is
not enough, and this workflow will refuse to run against a runner whose locked
caps do not match (see the preflight step) so that CI cannot silently follow a
contract it was never told about.

## Known limitations

- **Merge order matters.** This branch adds only the two files above. The
  workflow drives `scripts/mutation-gate.sh` @ `d004eb7` and
  `tests/mutation_gate/**` @ `579e475`, which are not on the base branch yet.
  Until Q02 and Q03 are on the ref under test, both jobs fail their preflight
  with an explicit message (the base branch carries a pre-Q02 stub of
  `scripts/mutation-gate.sh`, which the preflight distinguishes by contract
  id). A gate that cannot run must not report success.
- Single Linux runner only. No Windows/macOS coverage is claimed, and no
  coverage beyond the three pilot files is claimed.
- The `paths` filter means the workflow does not run on PRs that touch neither
  the pilot trio nor the gate's own inputs — that is intentional, not a pass.
- Nothing here is measured yet; see "Runtime caps" above.

## References

- Contract: `CX/20261005-q01-v1/001` (Q01 v1, part 6)
- Q02 runner head: `d004eb7` (`scripts/mutation-gate.sh`)
- Q03 harness head: `579e475` (`tests/mutation_gate/**`)
- `.github/workflows/ci.yml`, `.github/workflows/native-shell.yml` (read-only
  here — unchanged)
- `Cargo.toml` / `rust-toolchain.toml` (read-only here — unchanged)
- `AGENTS.md`, `TESTING.md`, `docs/architecture.md`
