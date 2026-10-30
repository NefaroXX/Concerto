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
| H03 | P1 | VFS reads shadow external disk changes | Open |
| H04 | P1 | Scoped Rust verification can pass with zero tests | Open |
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

H01–H02 review: [draft PR #379](https://github.com/NefaroXX/Concerto/pull/379).

Next milestone: H03, distinguish cached disk originals from pending overlay
edits so external changes do not silently disappear from reads.
