# Codex handoff — cache/audit/benchmark work

Repository: NefaroXX/Concerto. Branch: `feat/cache-audit-benchmarks`.
Draft PR: https://github.com/NefaroXX/Concerto/pull/326. Base: `dev`.
Read `AGENTS.md`, then `docs/cache-benchmarks.md`. Continue on this branch.

## Implemented

- Additive optional provider cache/reasoning counts, inclusive Anthropic input,
  OpenAI trailing usage handling, and Google cached input/reasoning accounting.
- Durable per-attempt started/finished records with request/session/task identity,
  timing, content-free fingerprints and honest missing usage/cost handling.
- Shared persisted session cache report for CLI, desktop and eval, plus explicit
  versioned model reporting prices. Existing spend-cap enforcement is unchanged.
- Audit-backed single-agent eval runtime and retained failed suite results.
- Opt-in stable system boundary hint, deterministic tools and native Anthropic
  moving history markers within the provider budget.
- Offline comparison and local reviewed benchmark/training-candidate exports,
  tests and usage documentation. No paid live experiments or training performed.

## Validation and current limits

Six offline comparison/export tests, formatting/diff checks and desktop palette
check passed locally. The current local checkout has OpenSSL, D-Bus, SQLite and
the WASM target installed; workspace Rust validation can run here. The earlier
web workspace lacked development prerequisites and produced malformed zero-length
dependency archive members; do not keep retrying full builds in that environment.

CI on checkpoint `4d592d4` failed because two config migrations omitted the new
field. `aafd726` fixed production constructors; subsequent CI found three explicit
test fixtures also missing it. Checkpoint `d8f319b` fixes those fixtures and adds
regression tests for durable draining, interrupted attempts and TTL-only usage.
Its latest CI passed workspace tests, builds, benchmark smoke runs, WASM, supply
chain, formatting and palette checks. Clippy failed because the desktop cache
report enlarged nested message enums; Windows failed two existing spawn-error
tests that assumed Unix's `NotFound` rather than Windows's `NotADirectory`.

The follow-up boxes the desktop report payload and checks preservation of the
native OS spawn error using isolated missing directories. A headless desktop
smoke test loads durably recorded evidence, renders the report across all themes,
and preserves unknown cost for unfinished attempts. The CI build job also runs
`scripts/cache-report-smoke.py` against the freshly built launcher using an
isolated synthetic SQLite session. It checks configured pricing, duplicate and
unfinished attempts, text/JSON output and legacy sessions without telemetry.
These checks do not replace an interactive desktop smoke test. Inspect the
latest PR checks before merge; do not infer success from older runs.

This is an implemented reporting/export slice, not completion of every research
phase in the original plan. Explicit remaining work:

1. Resolve any latest CI failures; run workspace tests/clippy on a fully provisioned
   checkout and smoke-test desktop/CLI with a recorded session.
2. Review compatibility with concurrent harness and desktop/CLI changes before
   merging into dev. No migrations or plugin wire ABI changes were introduced.
3. Add a production-policy/multi-agent benchmark runtime using the complete effective
   configuration if testing policies or specialists. Current eval is explicitly a
   permissive single-agent fixture, not that runtime.
4. Repair/curate representative benchmark cases (some existing IDs are invalid
   ULIDs, and standard fixtures lack expected outcomes), strengthen external graders,
   and link starting snapshot/definition fingerprints directly into root reports.
5. Time-out results currently retain unknown telemetry/session linkage; durable
   records survive separately. Join them reliably if timeout accounting is required.
6. Automatic optional transcript capture, provider-native diagnostics/session
   affinity, specialist views and advanced TTL settings remain separate work.
   The candidate exporter requires selected visible content from a human-reviewed
   JSONL input; audit summaries are not training examples.
7. Run repeated/interleaved baseline comparisons only after agreeing a concrete
   paid-call budget. Establish a measured baseline before enabling the new opt-in
   cache policy. No savings or correctness benefit has yet been measured live.

## Next local checks

```sh
git fetch origin
git switch feat/cache-audit-benchmarks
git pull --ff-only
python3 -m unittest discover -s scripts/tests -v
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/cache-report-smoke.py ./target/debug/concerto
```

Keep the PR draft until validation and scope review are complete. Do not merge,
start paid experiments, upload datasets or train a model as part of this handoff.
