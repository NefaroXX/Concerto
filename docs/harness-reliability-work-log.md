# Harness reliability work log

Audit baseline: `8c11c4a` (2026-10-29). Scope: the twelve initial findings from
the source audit and local probes, plus follow-up findings recorded below.
Changes land in verified, reviewable milestones.
This log distinguishes completed fixes from findings still requiring work;
passing repository tests alone does not close a finding.

## Findings and progress

| ID | Priority | Finding | Status |
| --- | --- | --- | --- |
| H01 | P1 | SSE events lost across network chunk boundaries | Fixed and verified; awaiting merge |
| H02 | P1 | Ordinary file writes rewrite valid Unicode content | Fixed and verified; awaiting merge |
| H03 | P1 | VFS reads shadow external disk changes | Fixed and verified; awaiting merge |
| H04 | P1 | Scoped Rust verification can pass with zero tests | Fixed and verified; awaiting merge |
| H05 | P1 | Premature provider EOF accepted as successful completion | Open |
| H06 | P1 | Default write versions are captured after the agent's read | Open |
| H07 | P1 | Verification errors can accompany Completed status | Open |
| H08 | P2 | Compaction counts historical usage as current context size | Open |
| H09 | P2 | Compaction truncates constraints and unresolved work | Open |
| H10 | P2 | Agent-driven benchmark fixture policy denies tools | Open |
| H11 | P2 | Empty benchmark outcomes force a zero pass score | Open |
| H12 | P2 | Durable event subscriber queues have no memory bound | Open |
| H13 | P1 | Structured-response parsing rewrites Unicode before attempting JSON parsing | Open; source traced during H02 |
| H14 | P2 | Checkpoint startup can degrade into ToolNotFound; CLI test failed intermittently | Open; observed during H02 verification |
| H15 | P1 | Orchestrator read cache trusts matching size/mtime without checking current bytes | Open; source traced during H03 |
| H16 | P2 | Coverage-only Cargo runners trust exit status and truncate output at byte offsets | Open; source traced during H04 |

## 2026-10-29 — H01: preserve streaming events across chunks

Branch: `fix/harness-streaming-integrity`.

- **Failure:** `push_bytes` kept its pending event in a local variable. A chunk
  ending after a complete field line discarded that field before the blank
  dispatch line arrived. A CRLF pair split between chunks also introduced a
  false blank line, which could dispatch an incomplete event.
- **Change:** keep pending event fields and CRLF continuation state on the
  parser. Consume each byte once and reuse the current line buffer. Decode UTF-8
  only after a complete line arrives; keepalive comments remain immediate
  activity signals without resetting the pending event.
- **Regression coverage:** exact expected events at every two-chunk split and
  every uniform chunk size (including byte-by-byte delivery), for LF, CR and
  CRLF. Fixtures include named events, IDs, multiline data, Unicode, comments
  and multiple events. Separate tests assert no dispatch before the blank line
  and no false blank line from a split CRLF, including intervening empty chunks.
- **Protocol reference:** [WHATWG SSE parsing and interpretation](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream).
- **Verification:** all four new regression tests fail against the baseline
  parser; all 14 parser tests pass against the change. See the completion entry
  below for workspace checks.
- **Remaining:** H05 is a separate provider-completion contract fix; preserving
  parsed events does not establish that a response reached its terminal marker.

## 2026-10-30 — H01: workspace verification

- `cargo test --workspace --locked`: 5,995 passed, zero failed, 16 ignored
  across 84 test groups (including integration and doc tests).
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check`, `git diff --check`, and
  `bash scripts/check-hardcoded-colors.sh`: passed.
- `python3 -m unittest discover -s scripts/tests`: six passed.
- `cargo deny check`: advisories, bans, licenses and sources passed with the
  existing repository policy; no dependency or exception changes.
- `cargo audit`: fetched the advisory database and scanned the raw lockfile,
  but registry requests for yanked-package status timed out on both attempts.
  Do not interpret its exit status as complete yanked-package verification.
- `cargo audit --no-fetch --no-yanked`: passed using the freshly fetched
  database (1,386 advisories, 1,096 dependencies). This confirms the advisory
  scan only; it does not replace the incomplete registry check above.
- The codebase knowledge graph was refreshed after the parser change.
- `cargo build --workspace --locked`: passed.

## Baseline verification

- `cargo fmt --all -- --check`: passed.
- Library tests across core, eval, MCP, orchestrator, providers, sessions and
  tools: 3,608 passed, two ignored.
- Additional audit probes reproduced H01–H04 and H08–H11. H05–H07 and H12 were
  source-traced findings; implementation work must add focused regressions.
- Tests use the configured disk-backed `/var/tmp` scratch directory. Putting
  all scratch files inside this repository invalidates the eval test that
  requires a directory without a Cargo manifest in its ancestors.

## 2026-10-30 — H02: preserve file-write content

- **Failure:** the filesystem tool and VFS globally replace Unicode
  punctuation in supplied content. Curly quotes inside valid JSON or Rust
  strings become unescaped ASCII delimiters; other replacements silently
  change data. Appending can also rewrite the existing prefix.
- **Change:** remove punctuation rewriting from filesystem text writes and VFS
  writes/appends. Both text and byte channels now preserve supplied content;
  the text path moves its owned string into the overlay after writing the same
  bytes to disk. Document the verbatim contract in the API/schema and clarify
  that the shared normalizer is a lossy repair utility.
- **Regression coverage:** three replacement tests assert exact bytes for valid
  JSON and source literals on disk and in the overlay; original/replacement
  content through staging and commit; and append to both original and modified
  entries without changing the prefix. Fixtures include all punctuation the
  old write path replaced, non-breaking spaces, and Japanese text.
- **Verification:** all three regressions failed against the previous
  implementation. `cargo test -p concerto-tools --locked` passed all 340 tests;
  formatting and diff checks passed. Workspace Clippy (all targets, warnings
  denied) and `cargo build --workspace --locked` passed. The workspace retry
  (`cargo test --workspace --locked --no-fail-fast`) passed: 5,995 tests, zero
  failed, 16 ignored across 84 groups, including integration and doc tests.
  The first fixed-code run exposed a test setup mistake (materialization
  retains the staged entry); the overwrite test now explicitly loads the
  committed original into a fresh VFS before checking modification semantics.
- **Index:** refreshed the semantic index with `ccc index`. The graph refresh
  completed, but its fast-mode exclusions include `crates/tools`; graph results
  alone should not be treated as proof that this crate's index is current.
- **Follow-up H13:** `parse_json_value` and `parse_json_substring` in
  `crates/orchestrator/src/prompts.rs` normalize the whole response before
  parsing, including characters inside valid JSON strings. This requires a
  separate parser fix and regression coverage; H02 covers filesystem/VFS
  preservation of the content supplied to those APIs.
- **Workspace failure / H14:** the first H02 workspace run failed in
  `cli_checkpoint_create_then_inspect_share_the_service_payloads`: create
  succeeded, then inspect returned `tool not found: checkpoint_inspect`.
  Nineteen subsequent CLI tests failed on the poisoned environment lock.
  The checkpoint test passed immediately when run alone with the same binary.
  Source trace: `runtime_runner::build_audit_sink` returns a missing pool after
  initialization errors, and `build_standalone_harness_with_memory` then skips
  registering checkpoint tools. The original database-init error was not
  captured in the test output; the intermittent trigger remains unconfirmed.
  This path does not write file contents through the APIs changed in H02.
  The full CLI rerun passed all 314 tests, and the workspace retry passed as
  recorded above, without code changes between runs. H14 remains open.

H01–H04 review: [draft PR #379](https://github.com/NefaroXX/Concerto/pull/379).

## 2026-10-30 — H03: refresh clean reads without losing staged intent

Implementation baseline: `0e2e778`.

- **Failure:** unbounded, byte and bounded reads treat cached `Original`
  entries as authoritative after external edits or deletion. The filesystem
  tool also falls back to disk for staged deletions because `exists` is false.
- **Design:** clean originals must read current disk state; staged text, bytes
  and deletions keep precedence. A full hunk rejection is a pending write of
  restored content, so it must not become a refreshable disk cache before it
  is materialized. Preserve the existing no-diff result for that restoration.
- **Change:** text, byte and bounded reads bypass clean cached originals;
  filesystem reads refresh those observations. Existence checks distinguish
  staged deletion from a cached disk observation. Appends and transfers use
  current sources, with UTF-8 validation before appending to a disk file.
  Full rejection retains a `Modified` entry whose current content equals its
  original, preserving the pending restoration without reporting a diff.
  ADR-62 §4 documents this distinction using existing serialized variants.
  Materialization continues to retain staged review state; explicit unstaging
  returns disk authority, as for other pending edits.
- **Regression coverage:** ten tests cover external edits/deletion, bounded
  size enforcement, exact binary reads/copies, current append/move preimages,
  rejection of a binary replacement during append, all staged entry kinds,
  snapshot restoration, and full hunk rejection before materialization.
- **Verification:** seven of the initial eight regression tests failed
  against the baseline; the existing staged-precedence behavior passed.
  An intermediate focused run passed all 350 tools tests, including the two
  additional boundary tests. Final review added assertions that a no-op hunk
  rejection leaves clean observations refreshable, invalid hunk selections
  retain their out-of-bounds error, and bounded tool reads refresh an existing
  observation to the exact returned text. The final-source workspace run
  (`cargo test --workspace --locked --no-fail-fast -j 2`) passed: 6,005 tests,
  zero failed, 16 ignored across 84 groups, including integration and doc tests.
  `cargo build --workspace --locked -j 2`, formatting and diff checks passed.
  `cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings`
  passed against the final source. No dependencies or policy rules changed.
- **Earlier workspace run:** both `supervisor_slice_crash` tests failed at
  `whiteboard_pool` setup with `PoolTimedOut`, before exercising their test
  behavior. The remaining targets completed. Both tests passed in an isolated
  rerun with the same test binary; the intermittent setup trigger remains
  unconfirmed. The final-source workspace run passed both tests. No timeouts
  were increased and no tests were disabled to accommodate this failure.
- **Intermediate revalidation:** another run, overlapping the rebuild after
  final review, failed in two supervisor tests: replay timed out waiting for
  whiteboard events (`supervisor_conflict`), and a healthy-silent-agent test
  observed one restart instead of zero (`supervisor_loop`). Desktop and eval
  doc tests also failed because their referenced tools rlib was absent during
  overlapping compilation. These results are not a valid final verification
  pass. The final-source workspace test run, after the build completed,
  passed all four targets. The supervisor failures remain observations with
  unconfirmed triggers; neither is treated as fixed by H03. Future verification
  must serialize Cargo test/build/Clippy operations against one target directory
  and freeze source before the final run.
- **Index:** refreshed the semantic index with `ccc index` after the final
  source changes. The graph server was unavailable (transport closed).
- **Follow-up H15:** `read_cache/serve.rs::maybe_serve_read` checks current disk
  size and millisecond mtime, then hashes only the cached content. A same-size
  external edit retaining/restoring the timestamp can therefore serve stale
  bytes without invoking the filesystem tool. This separate orchestrator cache
  defect is source-traced and still needs a focused regression/fix; H03 fixes
  reads that reach the VFS/tool, not every end-to-end cache path.
- **Remaining:** H06 still governs optimistic write-version capture; these
  fresh reads do not make subsequent writes atomic against external changes.
  Read-only VFS accessors do not rebind recorded originals to write preimages;
  write/preimage consistency needs separate coverage with H06.

H04 continues below. H15 also needs attention before claiming end-to-end
read freshness.

## 2026-10-30 — H04: require meaningful Rust test evidence

Implementation baseline: `1724124`.

- **Failure:** `scoped_args` guesses Rust test-name filters from source paths.
  Crate roots become `lib`/`main`, renamed modules do not follow file names,
  and multiple module names become one space-containing filter. Even a filter
  that matches unit tests can omit unrelated integration tests. `run_with_args`
  accepts exit code zero without checking whether any tests executed.
- **Design:** use the full Cargo suite at the resolved manifest root until a
  reliable dependency-to-test mapping exists. Preserve the actual exit code,
  but require a successful libtest summary with at least one passed test before
  setting `EvalResult.passed`. Empty, ignored-only, missing, or malformed
  evidence cannot establish a pass. Parse available output before reducing it
  to a bounded, Unicode-safe display tail, for both governed and direct runs.
- **Reference:** [Cargo test filtering](https://doc.rust-lang.org/cargo/commands/cargo-test.html)
  filters test names, not source dependencies (checked through Context7).
- **Regression coverage:** real Cargo fixtures for crate-root/multi-file/renamed
  module filters, integration failures, empty/ignored/passing/doc-only suites,
  and ancestor-manifest fallback; governed executor cases for mixed summaries,
  missing/malformed output, nonzero exits, and Unicode output beyond the tail.
- **Verification:** all three new integration tests failed against the baseline:
  a guessed root filter hid failing tests, a real empty suite returned a pass,
  and governed zero-test output returned a pass. The first fixed-code eval run
  passed 83 unit tests and all three integration tests. Final review tightened
  summary parsing so truncated/malformed trailing fields cannot count as proof;
  the stricter-parser regression run passed all three tests. A formatting
  check requested one line wrap, which was applied. Final workspace results
  are recorded in the completion entry below.
- **Compatibility:** Cargo's actual exit code and existing serialized result
  shape remain unchanged. `passed = false` can now accompany exit code zero,
  with an explanation in the output tail. Custom Rust harnesses without libtest
  summaries need a future explicit adapter; their successful exit alone is
  insufficient. Non-Cargo runner pass/fail rules are unchanged. The graph
  server remains unavailable (transport closed); source discovery used local
  files. The semantic index was refreshed with `ccc index`.
- **Follow-up H16:** `run_with_coverage` directly runs llvm-cov/tarpaulin and
  builds results from exit status, with a byte-offset output slice that can
  split UTF-8. Those optional coverage-only paths need their own execution
  tests; H04 covers ordinary full/scoped/fallback Cargo test runs (including
  the test result reused by `run_scoped_with_coverage`).

## 2026-10-31 — H04: verification complete

- `cargo test --workspace --locked --no-fail-fast -j 2`: 6,006 passed, zero
  failed, 16 ignored across 85 groups, including integration and doc tests.
- `cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings` and
  `cargo build --workspace --locked -j 2`: passed. These checks ran sequentially
  after the workspace test run, with no intervening source changes.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- All three H04 regressions failed before the fix and passed after it. The net
  workspace count increases by one: three new integration tests, and three old
  Cargo-filter unit tests consolidated into one fallback matrix.
- No dependency, policy, cancellation or serialization contracts changed.
  H16 remains open for optional coverage-only runners. The existing supply-chain
  results and registry-timeout caveat from H01 remain applicable; those checks
  were not repeated for this dependency-free change.

Next milestone: H05, reject provider EOF without valid completion evidence.
