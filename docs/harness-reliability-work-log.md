# Harness reliability work log

Audit baseline: `8c11c4a` (2026-10-29). Scope: the twelve findings from the
source audit and local probes. Changes land in verified, reviewable milestones.
This log distinguishes completed fixes from findings still requiring work;
passing repository tests alone does not close a finding.

## Findings and progress

| ID | Priority | Finding | Status |
| --- | --- | --- | --- |
| H01 | P1 | SSE events lost across network chunk boundaries | Fixed and verified; awaiting merge |
| H02 | P1 | Ordinary file writes rewrite valid Unicode content | Open |
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

Next milestone: H02, preserve file-write content with regressions for Unicode
inside valid JSON and source literals.
