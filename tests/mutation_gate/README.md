# Mutation gate contract fixtures (Q03)

These are black-box acceptance checks for Q01 v1, contract
`CX/20261005-q01-v1/001`. OpenCode owns `scripts/mutation-gate.sh` (Q02).
Codex owns this directory. The harness runs the supplied runner unchanged in
disposable, committed Git workspaces with the same pilot paths and package
names as Concerto. It does not test Concerto's production policy or parser
semantics; the later pilot runs their real package tests.

## Run against an exact Q02 checkout

Use Linux, Bash, Git, Python 3.9+, GNU `stat`, Rust, and the pinned tool:

```bash
cargo install cargo-mutants --version 27.1.0 --locked
bash tests/mutation_gate/run-contract-tests.sh \
  --runner /absolute/path/to/Q02-checkout/scripts/mutation-gate.sh \
  --work-dir /absolute/disk/path/outside/both/checkouts/q03-run-001
```

Use Rust 1.96.0 for the proposed CI pilot. The work directory must not exist;
its parent must exist on a disk-backed filesystem outside `/tmp` and both
source checkouts. Every rerun needs a new directory. Evidence is retained;
the harness never deletes a previous run. The runner's byte hash is recorded
and checked again after execution. Keep the Q02 Git head alongside the report
when handing off a result.

The entry point accepts only the agreed `--runner` and `--work-dir` arguments
(plus `--help`). Candidate repositories and their commits are created by the
harness. No workspace manifests, production code, CI files, or supplied
runner are edited. Fixture Cargo workspaces have no external dependencies
and run offline after installation of the tools.

## What counts as evidence

- **Real tool:** a strongly asserted Boolean function catches mutations;
  a positive-only test leaves a constant-true mutation alive; an intentionally
  broken unmutated test fails baseline. A real Cargo build-script process
  signals readiness, then hangs so global deadline, SIGINT, SIGTERM, and child
  cleanup paths can be exercised. These cases require real `cargo-mutants`.
- **Simulated tool:** a PATH-local shim emits the pinned upstream artifact
  shape for timeout, build-timeout, baseline-timeout, missed-plus-timeout,
  zero/all-unviable, malformed/missing artifact, version mismatch, internal
  error, and invalid-diff cases. It also checks command scope and configured
  caps. These cases verify wrapper control flow; they are not mutation proof.
- **Git/input guards:** documentation-only and out-of-pilot skips, deletion,
  rename, multiple selected files/packages, mixed uncovered files, dirty and
  untracked inputs, invalid base, non-repository cwd, caller-relative output
  paths, hidden mutant configuration, and unsafe/reused output directories.
  Odd Rust filenames contain spaces, a newline, Unicode and shell/glob
  metacharacters. A diverged base branch detects incorrect two-dot diffs.

The wrapper report must identify the contract, tool, Git commits, selected and
uncovered files, raw exits, counts, completeness, duration, and artifacts.
A passing report must reconcile with successful raw baseline evidence and a
caught viable mutant for **each** selected file. A global count alone is
insufficient. Unknown counts, missed mutants, timeouts, unviable-only results,
and explicit skips never become a successful completed mutation run.

An output-path guard may reject the request before creating a report; those
cases verify exit 1 and preservation of source/prior evidence without asking
the runner to write through an unsafe path. Other report-writing failure
cases require `gate-report.json` and `console.log`.

The simulated CLI covers the invocations used by this pilot. If Q02 uses a
different valid pinned-tool invocation, review the shim against upstream
27.1.0 before attributing a simulation failure to Q02. The shim is kept in this
directory, never installed, and is used only in cases labeled
`simulated_tool`. Its source references the upstream serialization code.

## Results and bounds

`WORK/contract-results.json` records each case's mode, command, fixture SHAs,
expected/observed result, and evidence locations. Per-case directories retain
the runner log, gate report, raw artifacts, fixture Git repository and shim
call trace. Harness exits:

| Exit | Meaning |
| --- | --- |
| 0 | Every case passed, including the real mutation and process cases |
| 1 | A contract check failed |
| 2 | Setup/required real execution was blocked; never an acceptance pass |

Missing Rust or the pinned tool blocks the real cases and prevents exit 0;
simulation results cannot replace them. The harness has a 720-second overall
budget. Individual real runner invocations use at most 120 seconds (10 for
the deadline case); simulated cases use 10 seconds, with the Q01 cleanup
allowance plus a small watchdog margin. Time exhaustion cannot turn a partial
suite green. Runner process groups and the known fixture child are cleaned
up on failure after any cleanup violation has been recorded.

## Harness self-checks

These validate fixture construction and the report oracle, without claiming
that Q02 works or that any real mutant was caught:

```bash
PYTHONDONTWRITEBYTECODE=1 \
Q03_SELF_TEST_ROOT=/absolute/existing/disk/directory \
  python3 -m unittest discover -s tests/mutation_gate -p test_harness.py -v
bash -n tests/mutation_gate/run-contract-tests.sh
```

The self-checks exercise clean candidate commits, a genuine diverged Git base,
odd filenames, source/symlink preservation, per-file outcome attribution,
contradictory/raw missed evidence, report type/path/count validation,
non-destructive work-directory handling and process-group cleanup.

## Initial validation handoff — 2026-10-05

Base: `288caeefe821479c734af36f32f12f486925b4af`.
Claim: `Codex/cx-20261005-Q03-01/Q03/001`, acknowledged by OpenCode in
`OC/sol-20261005-01/003`.

Observed locally: harness self-checks pass; Bash syntax and Python compilation
pass. Real Rust fixtures and the supplied Q02 runner have **not been run**.
The handoff names Q02 head `05e24f60114a1dd703b5a3d6391387dd1f492c5e`, but
GitHub returned 404 for both that commit's file and `fix/Q02-mutation-runner`.
OpenCode needs to push that exact branch/head before Q04 can review it.
This environment also has no Cargo/Rust installation; the Rust distribution
download connection timed out. No real mutation or full integration result
is claimed from the self-checks.

Next: fetch the published Q02 head, run this harness using its absolute script
path in an equipped environment, retain the complete result directory, and
request reciprocal review at the exact Q03/Q02 heads. Authors repair their
own scopes. Q03's claim remains held through review/integration and explicit
release.
