# Cache accounting and benchmark reports

Provider usage preserves missing counts as unknown. Prompt input is inclusive:
Anthropic ordinary input, reads, and writes sum to the total; OpenAI-compatible
prompt totals already include reads. Reasoning is an output subset. Reads from
the filesystem cache are unrelated to provider prompt-cache reuse.

Each orchestrated provider attempt is recorded as content-free session events.
Reports reconstruct those events, deduplicate attempt IDs, include retries and
failures, and flag starts without finishes. Interrupted calls with no terminal
usage retain unknown counts/cost. Telemetry goes to durable consumers without
evicting UI progress/approval events. Old sessions remain readable with no cache
measurement coverage. Provider telemetry does not alter security audit rows.

## Session reports

```sh
concerto sessions cache SESSION_ID
concerto sessions cache SESSION_ID --json > session-cache.json
```

Desktop session spend view uses the same report calculation. Token reuse is the
ratio of summed reads to summed inclusive input on the same valid calls; coverage
is cache-reporting calls divided by all recorded attempts. Empty denominators are
N/A. JSON includes per-provider/model summaries, timing samples and raw attempts.
Desktop exposes a compact summary; detailed timing/attempt views are in JSON.

## Explicit reporting prices

Optional TOML entries are keyed by the exact `provider / model` string in the
report. All rates are USD per million tokens. The numbers below are artificial
test rates, not current vendor prices; operators supply an identified effective
price version for their actual route/model.

```toml
[cache_pricing."fixture / model"]
version = "fixture-v1"
input = 3.0
output = 15.0
cache_read = 0.3
cache_write_5m = 3.75
cache_write_1h = 6.0
```

With a configured table, missing model prices or required usage components leave
cost unknown. TTL writes are subsets, never charged twice. For APIs that charge
cache writes at ordinary input rates, set `writes_charged_as_input = true`; that
explicit mode needs inclusive input, reported reads and output. Reporting prices
do not replace the existing spend-cap enforcement estimator. Without a table,
recorded costs remain explicitly labelled legacy nominal estimates without cache
discounts. Pricing changes take effect when reports are loaded; estimates are not
provider invoices. Invalid/nonfinite/negative rates produce no estimate.

## Opt-in cache boundaries

`[context] cache_stable_prefix = true` preserves stable system instructions ahead
of volatile state and makes tool ordering deterministic. A typed byte boundary
hint splits the first system message into native Anthropic text blocks without
changing instruction roles. Provider `cache_breakpoints = true` enables native
Anthropic markers within the four-marker budget, preserving manual markers and
moving the final eligible conversation boundary through tool-result loops.
Other routes receive no new unsupported cache fields. Defaults remain off.
Fingerprints describe changes; they do not prove a provider-side cache miss.

## Benchmark runs and comparison

```sh
cargo run -p concerto-eval-runner -- \
  --suite=crates/eval/benchmark_tasks/library_with_tests \
  --config=benchmark.toml --variant=baseline --cache-preparation=operator-described \
  > baseline.json
python3 scripts/cache-benchmark.py compare baseline.json candidate.json \
  --output comparison.json
```

The Rust command with `--config` makes real, potentially paid provider calls.
Run it only with a deliberately chosen budget. The Python commands are offline.
The runner uses a single-agent, permissive fixture with null memory and ten
iterations; it is not a production-policy or specialist comparison. Persisted
run metadata includes provider config and starting workspace fingerprints.
Root report metadata records variant, price fingerprint and operator-labelled
cache preparation. No claim of guaranteed cold/warm state is made.

The offline comparison reports verified success, cost of all measured attempts
per verified success, cache ratios, coverage, median/p95 timing and missing data.
It suppresses token comparisons for incomplete usage, and cost comparisons for
incomplete costs or different price versions. Task IDs/descriptions and runtime
mode must match. Starting snapshots must still be checked against persisted run
manifests; the tool does not claim to establish causal savings. Interleave repeated
runs on identical starting workspaces before accepting a tuning change.

Legacy `avg_cost_usd`/`avg_tokens` fields can be incomplete subtotals; inspect the
telemetry and comparison completeness fields. Failed/time-out cases stay in the
suite report. Time-outs may lose the report's session link while durable events
are retained separately. Fixtures without expected outcomes are not verified
successes, even if tests happen to pass. Some existing fixture IDs are invalid
ULIDs; curate valid case definitions before running them.

## Reviewed dataset exports

```sh
python3 scripts/cache-benchmark.py export-cases PATH_TO_CURATED_SUITE \
  --project-id PROJECT_LINEAGE --output case-export
python3 scripts/cache-benchmark.py export-examples reviewed.jsonl \
  --output candidate-examples.json
```

Case export bundles UTF-8 snapshots, SHA-256 provenance, expected outcomes and
grader version. It requires nonempty expected outcomes and snapshots within the
suite; rejects symlinks, credential files/patterns and unsafe paths; excludes build
and repository internals. Outputs must be new paths. Review bundled project data
before sharing. An outcome definition is provenance, not evidence it passed.

Training candidates must be explicitly curated JSONL with nonempty `project_id`,
`task_id`, `session_id`, `visible_input`, `visible_output`, and `verification`;
both `approved_for_export` and `externally_verified` must be true. Other fields
are discarded. Approval/verification are curator assertions and are not
independently proved by the exporter. Missing/incomplete examples are rejected.
Content is deduplicated within task/project lineage and common credential patterns
are redacted; arbitrary sensitive data requires human review. Entire project
lineages receive one deterministic train/held-out split, so related tasks and
sessions cannot cross it. Small datasets may have an empty split.

Audit summaries cannot reconstruct training transcripts. This exporter consumes
explicitly selected visible content; automatic transcript capture is not added.
Exports remain local candidates requiring review. Nothing is uploaded or trained.

## Verification

```sh
python3 -m unittest discover -s scripts/tests -v
cargo fmt --all -- --check
cargo test -p concerto-core cache_usage
cargo test -p concerto-providers
cargo test -p concerto-sessions cache::tests
cargo test -p concerto-orchestrator provider_telemetry
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
