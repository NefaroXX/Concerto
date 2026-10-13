# Harness measurement and release gates (H14)

This document is the measurement plan for the harness-upgrade effort's final
section ("Measurement and release gates"). It records, per plan metric, which of
four **honest buckets** carries it, and where. No number is invented: a metric
with no in-tree assertion, no benchmark, and no runnable procedure is written
down as **declared-unmeasured**, not estimated.

The reconciled implementation plan (H00–H13 status + live task register) is
[`docs/harness-upgrade-plan.md`](harness-upgrade-plan.md).

## Bucket legend

| Bucket | Meaning | Fails the build when it regresses? |
| --- | --- | --- |
| **assert** | An in-tree test that fails if the property regresses. | Yes (`cargo test`) |
| **benchmark** | A criterion benchmark that records latency/bytes. **No pass/fail on absolute values** — it is a baseline for future comparison. | Compiles + smoke-runs in the CI `bench` job; timing compared only weekly in `bench-baseline.yml` |
| **live-procedure** | Needs a real model + operator. The procedure and fixture corpus are written here; the result is **not** faked and **not** claimed. | No |
| **declared-unmeasured** | No honest bucket exists yet. Stated plainly. | No |

The benchmarks are **not wired into CI by this change**. They are ordinary
`[[bench]]` targets; the existing auto-discovery in `.github/workflows/ci.yml`
(`cargo bench --workspace --bench <name> -- --test`) and `bench-baseline.yml`
would pick them up if/when an owner chooses to run them there. Until then they
are run on demand.

## Metric → bucket map

| Plan metric | Bucket | Where |
| --- | --- | --- |
| Skipped/unavailable checks incorrectly reported as success (target **zero**) | **assert** | `crates/core/src/types.rs:3505` (`no_non_passing_outcome_is_ever_passing_evidence`); `crates/orchestrator/src/verify.rs:3295` (`gate_ledger_never_counts_a_non_passing_outcome_as_fresh`) |
| Restore correctness | **assert** (pre-existing H06/H07) | `crates/orchestrator/tests/file_checkpoints.rs:388` (`write_checkpoint_mutate_restore_is_byte_exact`), `:1085` (`binary_checkpoint_mutate_restore_is_byte_exact`), `:1194` (`corrupt_chunk_set_restores_partial_without_success_claim`) |
| Restore conflict behavior | **assert** (pre-existing H06/H07) | `crates/orchestrator/tests/file_checkpoints.rs:458` (`ownership_conflict_restore_is_refused_and_surfaces`), `:946` (`git_stash_restore_onto_conflicting_tree_fails_loudly`) |
| Retained-bytes accounting | **assert** (new) | `crates/sessions/src/file_checkpoints.rs:1479` (`retained_bytes_account_for_every_banked_path`) |
| Source attribution + stale-memory handling across sessions | **assert** (pre-existing H11/H12) | `crates/memory/tests/h12_acceptance.rs` (`run_acceptance`), `crates/memory/src/system.rs:2270` (`evidence_recall_ttl_expiry_stales`), `:2567` (`evidence_recall_dirty_seam_observation_stales`), `crates/memory/src/evidence.rs:766` (`evident_projection_carries_source_status_and_why`) |
| Parity across declared execution backends and clients | **assert** (pre-existing H13) | `crates/orchestrator/tests/harness_equivalence.rs:191` (`memory_recall_service_matches_direct_store_recall`), `:241` (`environment_describe_is_equivalent_across_standalone_builds`), `:280` (`checkpoint_lifecycle_round_trips_through_the_standalone_service`); scope declared in `docs/harness-access.md` |
| Redundant verification runs (coalescing) | **assert** (pre-existing H05) + **benchmark** premise | `crates/orchestrator/src/verify.rs` `project_wide_spawns_coalesce_on_generation`; restated as a bench premise in `crates/orchestrator/benches/verification.rs` |
| Verification latency | **benchmark** | `crates/orchestrator/benches/verification.rs` |
| Restore latency + retained bytes at representative sizes | **benchmark** | `crates/orchestrator/benches/checkpoint_restore.rs` |
| Environment-probing calls before the first useful action | **live-procedure** (declared-unmeasured here) | "Live procedure" below; fixtures under `crates/eval/benchmark_tasks/harness_behavior/probe_locate_scale` |
| Identical failed-call retries | **live-procedure** (declared-unmeasured here) | "Live procedure" below; fixture `.../retry_signature_change` |
| Elapsed time to fresh passing evidence | **live-procedure** (declared-unmeasured here) | "Live procedure" below; fixture `.../verify_fix_is_even` |

## In-tree assertions (run these)

```bash
cargo test -p concerto-core        no_non_passing_outcome_is_ever_passing_evidence
cargo test -p concerto-orchestrator gate_ledger_never_counts_a_non_passing_outcome_as_fresh
cargo test -p concerto-sessions    retained_bytes_account_for_every_banked_path
```

The **target-zero property** is asserted at both levels the plan names:

- **Taxonomy** (`concerto-core`): every non-`Passed` `VerificationOutcome`
  (`Failed`, `Skipped`, `Unavailable`, `Error`, `Cancelled`) — even stamped with
  the complete R2 freshness basis — is never `is_passing_evidence()` and never
  reports as a pass. `Skipped` and `Unavailable` are the historically dangerous
  cases (a missing npm `test` script used to record `passed: true`; a missing
  manifest used to record nothing). The control at the end proves the assertion
  is not vacuous.
- **Gate ledger** (`concerto-orchestrator`): the same enumeration through
  `evaluate_gate_ledger`, the predicate behind the opt-in strict verification
  gate. No non-`Passed` outcome counts as a fresh or typed pass, so a required
  check can never be satisfied by an absence/failure/skip.

`is_passing_evidence` itself requires `outcome == Passed` **and** agreement of
the raw `passed` flag **and** the current schema version **and** the full
freshness basis (verifier, scope, generation, content fingerprint, explicit
`checked_at`, evidence ids) — see `crates/core/src/types.rs:1632`.

## Benchmarks (run on demand; not CI-wired here)

```bash
# Smoke-run (compile + one execution each; the repo's own bench-check pattern)
cargo bench -p concerto-orchestrator --bench verification -- --test
cargo bench -p concerto-orchestrator --bench checkpoint_restore -- --test

# Record a baseline (criterion writes target/criterion/)
cargo bench -p concerto-orchestrator --bench verification
cargo bench -p concerto-orchestrator --bench checkpoint_restore
```

- **`verification.rs`** runs a project-wide check through the production
  `VerifierService` with an in-process stub spawner and counts executions via
  the public `CountingSpawner`. The coalescing premise (a repeat project check
  adds no spawn and no harness run) is asserted before timing; the cold vs
  coalesced arms are then benchmarked. Hermetic: tempdir workspace, no network,
  no container runtime, no real `cargo`.
- **`checkpoint_restore.rs`** times `FileCheckpointStore` create and sidecar
  reassembly (`CheckpointStore::load_bytes`) at small-text, multi-chunk binary
  within budget, and large multi-chunk sizes. It asserts, as the premise, that
  **bytes retained equal bytes written**. Hermetic: a file-backed tempdir SQLite
  database with the real migrations.

Both benches state no pass/fail on absolute values; the recorded numbers are the
baseline. Reproduction protocol: same machine class, same toolchain, default
criterion settings, `target/criterion/` retained (or the weekly baseline
cache), compare means with the 25% tolerance used by `bench-baseline.yml`.

## Live procedure — agent-behavior metrics

These three metrics are **declared-unmeasured in this milestone**: they need a
real model and operator, and the result must not be invented. What follows is
the procedure and the fixture corpus for the run that would measure them.

### Fixture corpus

`crates/eval/benchmark_tasks/harness_behavior/` (driven by the existing
`eval-runner`; no second runner is built). Each fixture is mechanically valid:
its test fails in the committed state and passes after the documented reference
fix (verified without a model — see "Mechanical validation" below).

| Fixture | Shape | Metric it serves |
| --- | --- | --- |
| `probe_locate_scale` | The needed constant (`SCALE`) lives in a different module than the function under change; the test fails (`todo!`) until the agent explores and finds it. | environment-probing calls before the first useful action |
| `retry_signature_change` | The in-tree test asserts a `Result` return, so the committed panicking `u16` signature does not compile against it; the agent's first edit is likely to fail and be retried. | identical failed-call retries |
| `verify_fix_is_even` | A seeded correctness bug with a failing test. | elapsed time to fresh passing evidence |

Reference fixes (for validating the fixture, not to be committed into the
snapshot): `probe_locate_scale/src/calc.rs` →
`crate::config::SCALE * (a + b)`; `retry_signature_change/src/lib.rs` →
`pub fn parse_port(s: &str) -> Result<u16, std::num::ParseIntError> { s.parse() }`;
`verify_fix_is_even/src/lib.rs` → `n % 2 == 0`.

### Which agent / model / config

- Agent: the single-agent loop (`concerto-orchestrator::AgentLoop`) driven by
  the existing `eval-runner` live leg (`crates/eval-runner/src/main.rs`,
  `live_runtime_benchmark_leg`), or the equivalent CLI/desktop single-agent run.
- Provider/model: a real provider via the existing env contract —
  `CONCERTO_LIVE_PROXY`, `CONCERTO_LIVE_PROXY_KEY`,
  `CONCERTO_LIVE_PROXY_MODEL`. Fix **one** model and **one** config for the
  whole comparison; record both.
- Suite: `CONCERTO_LIVE_EVAL_SUITE=crates/eval/benchmark_tasks/harness_behavior`.
- Config: record the effective config hash (`[agent]`, policy preset, shell
  profile). Do not vary it across repeated runs of one comparison.

```bash
CONCERTO_LIVE_PROXY=https://host/v1 \
CONCERTO_LIVE_PROXY_KEY=… \
CONCERTO_LIVE_PROXY_MODEL=… \
CONCERTO_LIVE_EVAL_SUITE=crates/eval/benchmark_tasks/harness_behavior \
cargo test -p concerto-eval-runner live_runtime_benchmark_leg -- --ignored --nocapture
```

### What to record, and from where

The shipped live leg reports only `task_count` and `pass_rate`; it does **not**
persist the per-call sequence or the verification timestamps. The three
sequence-dependent metrics therefore require an observation surface beyond the
leg's summary. Two honest options, in preference order:

1. **CLI/desktop session (audit trail).** Run the same fixtures through a real
   single-agent session, which persists the append-only audit log, then read the
   tool-call sequence with:

   ```bash
   concerto audit <session-id> --json            # every row: timestamp, tool, operation, failed
   concerto audit <session-id> --failed --json   # retry-relevant rows only
   ```

   - *environment-probing calls before the first useful action*: count audit
     rows from run start until the first **successful file-affecting write**
     (`write`/`delete`/`edit`/`filesystem` mutation). Read-only discovery
     (`read`/`list`/`search`/`environment.describe`) counts as probing.
   - *identical failed-call retries*: count maximal runs of consecutive
     `--failed` rows with the same tool + canonical input, before any success.
   - *elapsed time to fresh passing evidence*: timestamp of the first
     verification proving the required check (the `verify.run` audit row, or the
     typed `AgentOutput.verification[]` record's `checked_at` when the operator
     can capture the run output) minus run start. If the typed `checked_at`
     and the freshness verdict are not captured, **mark this unmeasured** — the
     audit-row timestamp alone is a coarse proxy, not the metric.

2. **Eval-runner observation export (future).** Adding a read-only export of
   the run's tool-event sequence and `checked_at` stamps to `eval-runner`'s
   `AgentRunResult` would let option 1 be measured from the harness directly.
   This is a small, additive change owned by the eval track, not by H14; it is
   **not** built here, and building it would not be a "second runner".

### Repetition

Run each fixture **N ≥ 5** times against the fixed model/config. Report the
median and the min–max range, never a single sample. Discard and re-run a sample
only for an identified environmental fault (record why). Live-leg flakes are
triaged, not silently retried (see the live-flake quarantine item in
`docs/DEFERRED.md` row 17).

### Mechanical validation (performed here, no model)

Each fixture was copied to a scratch directory under the repo's `target/`,
`[workspace]`-isolated as the harness does, and run `cargo test` in the
committed state (all fail: `probe_locate_scale` and `verify_fix_is_even` on the
assertion, `retry_signature_change` on a compile error) and after applying the
reference fix (all pass). This validates the **fixture**, not the model run.

## Cross-platform honesty audit

The plan: "Cross-platform claims require platform evidence; passing a mock
container test is not proof of actual isolation or restoration." Claims the
harness work makes, checked against their evidence:

| Claim | Where | Evidence / scoping | Verdict |
| --- | --- | --- | --- |
| Sandbox `Linux`/`macOS` availability derives from the same `PATH` probe; parity claimed only where implemented | `docs/adrs/ADR-83-sandbox-backend-decision.md` §4, §6 | Explicitly scoped: "honest parity, not assumed parity"; macOS Docker-Desktop VM spawn-time differences named as a residual. | **Scoped** — no fix needed |
| Windows is `UnsupportedByDecision`; the probe is never consulted | ADR-83 §3; `crates/core/src/sandbox.rs` (`SANDBOX_WINDOWS_DECISION`) | Proven by test with a FOUND runtime still reporting the decision (`available_runtime_is_honored_on_linux_macos_but_not_on_windows`). | **Backed** (decision, not a probe claim) |
| Sandbox declares isolation only and restores nothing (`NO_SANDBOX_RESTORE_MILESTONE`) | ADR-83 §2; `crates/core/src/sandbox.rs:274` | No restore is claimed; the routing marker is explicitly a producer assertion (ADR-72 §2 residual restated). | **Scoped** — matches the plan's warning |
| No independent proof of confinement beyond the audited argv | ADR-83 Residuals | Stated as a residual. | **Honest** |
| "Containerized is implemented and enforced on Linux/macOS" | `docs/STATUS.md` (sandbox bullet) | Inherited from ADR-72, not harness-authored; mechanism-level, cites ADR-72. No macOS **sandbox** platform run exists in CI (`native-shell.yml` runs macOS for native shell only; the container path is exercised by unit tests with a fake `docker` on `PATH`, not a real runtime). | **Report-only** (pre-existing, out of harness scope): no macOS container-enforcement platform evidence is claimed as proof of isolation. Not reshaped here. |
| `docs/harness-access.md` | H13 | Makes no OS/platform claim. | **N/A** |

No harness-authored cross-platform claim was found that overstates platform
evidence, so no documentation-scope fix was required. The one pre-existing claim
(`docs/STATUS.md`) is reported, not reshaped.

## Not claimed / numbers refused

- No live model run was performed for this milestone. The three agent-behavior
  metrics (probing, retries, time-to-fresh-evidence) are **declared-unmeasured**.
- No absolute latency or byte number is asserted anywhere; the benchmarks record
  a baseline for comparison only.
- No claim about actual container isolation or restoration; the sandbox
  descriptor explicitly restores nothing (ADR-83).
- No macOS/Windows platform-run evidence is claimed for the container sandbox.
- No cost/token/throughput numbers are reported for the H14 work.
