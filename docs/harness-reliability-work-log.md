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
| H05 | P1 | Premature provider EOF accepted as successful completion | Fixed and verified; awaiting merge |
| H06 | P1 | Default write versions are captured after the agent's read | Fixed and verified; awaiting merge |
| H07 | P1 | Verification errors can accompany Completed status | Fixed and verified; awaiting merge |
| H08 | P2 | Compaction counts historical usage as current context size | Open |
| H09 | P2 | Compaction truncates constraints and unresolved work | Open |
| H10 | P2 | Agent-driven benchmark fixture policy denies tools | Open |
| H11 | P2 | Empty benchmark outcomes force a zero pass score | Open |
| H12 | P2 | Durable event subscriber queues have no memory bound | Open |
| H13 | P1 | Structured-response parsing rewrites Unicode before attempting JSON parsing | Draft prepared in isolated worktree; baseline reproduction pending |
| H14 | P2 | Checkpoint startup can degrade into ToolNotFound; CLI test failed intermittently | Open; observed during H02 verification |
| H15 | P1 | Orchestrator read cache trusts matching size/mtime without checking current bytes | Draft prepared in isolated worktree; baseline reproduction pending |
| H16 | P2 | Coverage-only Cargo runners trust exit status and truncate output at byte offsets | Open; source traced during H04 |
| H17 | P1 | Resumed history lacks restored read-version claims when a new gate is constructed | Open; source traced during H06 |
| H18 | P1 | Locked Wasmtime 36.0.19 fails fresh security gates on RUSTSEC-2026-0424 | Fixed and verified; awaiting merge |
| H19 | P2 | Action-required minimum tool-call check ignores earlier continuation rounds | Fixed and verified locally; remote CI pending |
| H20 | P1 | Yanked async-executor 1.14.0 remains locked after an upstream memory-safety regression | Fixed and verified; awaiting merge |
| H21 | P2 | Multi-agent runtime stage reports Complete for partial outputs | Open; source traced during H07 |
| H22 | P1 | Cache path normalization can select a different file from the filesystem tool | Open; source traced during H15 preparation |
| H23 | P2 | CLI MCP probe integration test timed out during the H19 workspace run | Open; cause under investigation |

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

H01–H06 review: [draft PR #379](https://github.com/NefaroXX/Concerto/pull/379).

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

H05 continues below.

## 2026-10-31 — H05: require provider completion evidence

Implementation baseline: `236c496`.

- **Failure:** both orchestrator stream collectors accept EOF without seeing a
  canonical final chunk. Buffered text/tool calls can therefore escape a
  truncated attempt as a successful response. The plugin completion host call
  also accepts EOF without checking completion.
- **Design:** require a final chunk before returning assembled output; classify
  premature EOF as the existing retryable `StreamTransport` error. Keep
  consuming the stream to preserve existing error, timeout and metering
  semantics. Retries assemble into fresh buffers; tools execute only after
  successful assembly. Preserve terminal-only usage and legitimate empty
  completions with an explicit final chunk.
- **Compatibility:** Gemini STOP is valid completion evidence even without an
  OpenAI-style DONE marker. Its adapter must emit a final chunk on clean EOF
  after STOP, preserve trailing usage, and never turn cancellation/transport
  failure into successful completion. OpenAI already accepts finish_reason at
  EOF; an EOF with neither finish_reason nor DONE is rejected by the collector.
- **Sources:** [OpenAI streaming events](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events),
  [Gemini finishReason](https://ai.google.dev/api/generate-content#FinishReason).
  The graph server has no index for this project; discovery used local sources
  and the refreshed semantic index.
- **Reproduction:** all three orchestrator regressions failed on the baseline:
  EOF was accepted as a completion, a truncated write reached the filesystem,
  and repeated empty streams were classified Completed. The plugin host
  regression also failed: partial provider text escaped as a successful WASM
  response.
- **Change:** both orchestrator collectors now require a final chunk before
  returning assembled output. The plugin host returns its existing ABI error
  on premature EOF. Gemini's decoder distinguishes clean EOF after STOP from
  missing completion evidence, emits terminal usage once, and preserves
  cancellation/transport failures. Its decoder is factored from HTTP setup so
  these boundaries are tested without network or credentials. Existing complete
  test-provider fixtures now emit final chunks; deliberate truncation scripts
  remain incomplete.
- **Regression coverage:** focused regressions cover collector EOF,
  cancellation, late errors, valid empty completions, bounded retry/exhaustion,
  no tool side effects on incomplete attempts, the WASM host boundary, and
  Gemini STOP/EOF, trailing usage, duplicate DONE, missing/partial markers and
  midstream cancellation/transport failure.
- **Intermediate checks:** all ten focused regressions passed. The first
  affected-library run found one more intended-complete scripted provider in
  the runtime stage tests that omitted a terminal marker; its fixture now
  includes one. The new WASM regression initially asserted the error at stream
  consumption, but this plugin reports it during stream creation; the assertion
  now checks that boundary and the specific incomplete-stream error. A test-only
  tuple equality assertion was also replaced with field checks because
  `ToolCall` does not implement `PartialEq`. None of these corrections relaxes
  the production completion requirement.
- **Affected suites:** orchestrator, providers and plugins library tests passed
  (2,562 passed, two ignored); all 55 plugin integration tests passed. These
  runs use local scripted streams and WAT fixtures, without live provider
  requests or credentials.

## 2026-10-31 — H05: verification complete

- `cargo test --workspace --locked --no-fail-fast -j 2`: 6,016 passed, zero
  failed, 16 ignored across 85 groups, including integration and doc tests.
- `cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings` and
  `cargo build --workspace --locked -j 2`: passed. Cargo checks ran sequentially
  with no intervening source changes.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Ten new regressions cover the shared collectors, actual agent-loop retries
  and filesystem side effects, Gemini wire decoding, and the WASM completion
  host. Existing complete-response fixtures now provide explicit final chunks.
- The semantic index was refreshed with `ccc index`. The graph server remains
  reachable but has no Concerto project index.
- No dependencies, serialized result shapes, policy gates or retry limits
  changed. The plugin host returns its existing ABI error rather than adding
  guest retries. No live provider requests were made. The H01 supply-chain
  results and registry-timeout caveat remain applicable; those checks were not
  repeated for this dependency-free change.

H06 continues below.

## 2026-11-01 — H06: bind default versions to observed reads

Implementation baseline: `e8df4a6`.

- **Failure:** both gated backends stamp missing claims at write arrival.
  An external edit between read and write therefore becomes the accepted base.
- **Design:** retain versions derived from successful filesystem
  read output, scoped by session and agent, and prefer those observations to
  arrival-time stamping. Explicit claims retain their existing precedence.
  Keep observations bounded without silently evicting stale-write protection.
- **Scope:** the in-process gate and supervisor share this seam. Neither gated
  backend enables the advisory cache-serve hook; plain-executor fallback and
  H15 read-cache freshness remain separate limitations. This change does not
  make compare-and-write atomic against external processes.
- **Discovery:** the graph server is reachable but still has no Concerto index;
  semantic search and local source inspection traced the shared path.

- **Reproduction:** both initial regressions failed against `e8df4a6`: the
  stamped hash matched the external edit rather than the returned read, and a
  stale writer was accepted after another agent reread the changed file.
- **Implementation:** the shared gate retains bounded, session/agent-scoped
  hashes of returned filesystem content. Claims prefer those observations;
  successful text writes advance only the writer's own known base. Absent-path
  observations protect subsequent creates. Unknown post-write bytes require a
  reread rather than a potentially racy post-write disk hash.

- **Binary compatibility:** filesystem reads now return an additive
  `content_hash` field from the same raw read as their display content. Staged
  byte entries hash their raw bytes under the VFS lock; binary placeholders
  are never used as the base version. Bounded reads retain their existing
  allocation limit and UTF-8 behavior.
- **Initial verification:** the first 11 regressions passed. The expanded
  binary-read test initially lacked a qualified `camino::Utf8PathBuf` name;
  that test-only compile error was corrected before rerunning the suites.

- **Follow-up H17:** `runtime_runner.rs` constructs a new `WriteGate` for a
  resumed single-agent run while restoring conversation/checkpoint context.
  The read ledger is deliberately run-local and is not reconstructed from that
  history. Resumed writers need fresh tool reads or explicit claims until
  durable observation restoration is implemented and tested.

- **Affected-suite verification:** all 13 regressions passed (12 orchestrator,
  one tools). Full affected library suites passed: 1,854 orchestrator tests
  with one ignored, and 351 tools tests. Coverage includes both real backend
  paths, failed reads, independent sessions/agents, explicit claims, each
  mutated target, read recovery, own-write advancement, absence/create/delete,
  binary and staged reads, capacity exhaustion, and symlink retargeting.

- **Review correction:** the first workspace test run passed 6,029 tests with
  zero failures and 16 ignored, and Clippy passed. Review then found that
  hunk staging can change the submitted content without showing the merged
  bytes to the agent. Advancing the ledger to that unseen merged version would
  allow a subsequent stale write to erase the external edit. A dedicated
  regression will pin this sequence; staged merges must require a fresh read.
  These initial broad results do not verify that final correction.

- **Merge reproduction and correction:** the new real-gate sequence failed
  against the initial implementation: a disjoint merge preserved the external
  edit, but a second default-claimed write based on the agent's original view
  was accepted. The ledger now marks hunk-staged content as requiring a reread,
  rather than advancing to bytes the agent has not observed. The regression
  also verifies that reading the merged content restores successful editing.

- **Corrected affected suites:** all 14 regressions passed, followed by 1,855
  orchestrator library tests (one ignored) and 351 tools library tests.
  Workspace tests, Clippy and build are being repeated because the staged-merge
  correction changed production behavior after the first broad pass.

- **Path-identity correction:** review found that raw `PathBuf` comparison does
  not fold Windows case/verbatim aliases, especially for an observed absence
  without an existing file to canonicalize. Ledger keys now use the filesystem
  module's existing host-specific comparison rules. A native-host wrapper test
  and a Windows-only absence-alias test accompany the existing pure Windows
  normalization tests. The in-flight workspace repeat was interrupted before
  source edits; final verification restarts on this corrected source.

- **Environment interruption:** the goal transition removed the active tool
  session, and process inspection confirmed no Cargo process remained. The
  final-v2 log stopped during integration tests without a completed suite.
  Verification was restarted from the existing disk-backed artifacts; that
  interrupted run is not counted as a pass.


## 2026-11-01 — H06: source verification complete

- `cargo test --workspace --locked --no-fail-fast -j 2`: 6,031 passed, zero
  failed, 16 ignored across 85 groups, including integration and doc tests.
- `cargo clippy --workspace --all-targets --locked -j 2 -- -D warnings` and
  `cargo build --workspace --locked -j 2`: passed sequentially on final source.
- `cargo fmt --all -- --check`, `git diff --check`, the UI palette check, and
  all six Python script tests passed. The semantic index was refreshed.
- All 15 Linux regressions passed. Native Windows execution of the added
  absence-alias case remains unverified; existing pure Windows normalization
  tests passed as part of the tools suite.
- Both original stale-read regressions failed before implementation. The
  staged-merge regression also failed before its correction, then passed along
  with reread recovery. Interrupted broad runs were not counted as passes.
- **New blocker H18:** fresh `cargo deny check` and `cargo audit` both failed on
  [RUSTSEC-2026-0424](https://rustsec.org/advisories/RUSTSEC-2026-0424), published
  after the earlier supply-chain pass. Wasmtime 36.0.19 is affected; the same
  LTS line is patched in 36.0.20. No suppression was added. H06 is committed
  separately; publishing waits for the dependency repair and verification.
- H17 remains open: gate-lifetime observations are not restored with resumed
  history. Plain-executor fallback and non-atomic external file races also
  remain outside H06's guarantees, as recorded in ADR-60.

Next: patch H18, then continue H07's truthful verification/completion handling.

## 2026-11-01 — H18: patch the Wasmtime LTS security dependency

- **Reproduction:** fresh `cargo deny check` and `cargo audit` both rejected
  Wasmtime 36.0.19 for
  [RUSTSEC-2026-0424](https://rustsec.org/advisories/RUSTSEC-2026-0424).
  The upstream advisory describes unchecked memory growth when transferring
  borrowed component resources. Concerto does not enable the component model;
  this is a dependency-gate repair, not a demonstrated Concerto exploit.
- **Change:** raised the manifest minimum to `~36.0.20` and regenerated the
  lockfile with `cargo update -p wasmtime --precise 36.0.20`. Only 29 matching
  Wasmtime/Cranelift/Pulley packages changed; their dependency edges did not.
  Kept the existing feature selection, memory64 restriction, and intentional
  unrelated advisory exceptions. No new suppression was added.
- **Initial verification:** both fresh supply-chain gates now pass. Feature
  inspection confirms async/Cranelift with no component-model, Winch or pooling
  allocator. Plugin and workspace verification is running serially against
  the patched dependency before publication.

- **Plugin suite:** 195 tests passed (128 unit, 55 WASM integration, 12
  policy integration); one doc test is intentionally ignored. Existing
  resource-limit, capability and host-call tests cover the retained sandbox.
- **H07 investigation:** the shared test adapter turns all harness failures
  into `eval_result: None`, and finalization consults only `EvalResult.passed`.
  Typed sweep/eval failures are therefore ignored by completion. Missing
  harness configuration currently uses the same setup-error variant as real
  configuration faults; the repair must distinguish absence from failure and
  cover both live completion events and persisted task status. No H07 source
  changes are included in the dependency milestone.
- **H07 continuation edge:** `merge_run_progress` retains earlier verification
  records but copies the latest round's completion status after finalization
  has already published its event. A latest-round-only fix could therefore
  miss unresolved earlier failures. Regression coverage must include a failed
  check carried across an iteration-cap continuation and a successful recheck
  that supersedes it, without contradictory live/persisted completion events.
- **H07 contract constraints:** `VerificationSummary::effective_outcome` is
  explicitly display-only; completion must use typed outcomes and the existing
  evidence rules rather than promoting legacy `passed` flags. Preserve the
  ordinary answer-only completion case, distinguish an unsupported harness
  from a real setup/I/O failure, and ensure cancellation during evaluation
  cannot publish successful completion. The optional strict freshness gate
  and basic truthful status need separate regression coverage.


## 2026-11-03 — H18: final workspace checks and CI parity

- The previous goal turn made concrete progress: patched dependencies,
  passing security gates and plugin tests, then a complete workspace suite.
  Current-state inspection confirms the full run finished: 6,031 passed,
  zero failed, 16 ignored across 85 groups; Clippy with warnings denied and
  the workspace build also finished successfully. No Cargo process remained.
- Fresh `cargo deny check` and `cargo audit` passed again on November 3.
- Remaining CI checks include single-frontend builds, launcher/cache replay
  smoke tests, WASM guest builds and benchmark smoke runs. The launcher/cache
  smoke passed. The first desktop-only build was deliberately interrupted
  with SIGINT after confirming its process group, to reclaim 25 GB of
  incremental caches before exhausting disk space. Only repository
  `target/debug/incremental` was removed after all compiler processes stopped;
  sources, logs and compiled outputs were retained. The interrupted build is
  not counted as a pass, and the extra checks resume from compiled artifacts.

- **Additional CI coverage:** desktop-only and CLI-only builds passed, as did
  default-launcher commands, cache-replay smoke, CLI startup/routing smoke and
  all four `wasm32-wasip2` guest builds. The nine benchmark smoke targets are
  compiling in the release profile; no timing/performance claim is made.
- **Checkpoint:** H18 is committed locally as `9793148`; H06 is `80047b8`.
  Publication waits for the remaining benchmark gate.

## 2026-11-03 — H07: isolated regression and implementation draft

- The prior goal turn made concrete progress: committed H18 and prepared H07
  regressions in disk-backed worktree `Concerto-worktrees/h07`, branch
  `fix/harness-verification-status`. No concurrent Cargo operation shares the
  active target directory. H18's source tree remains unchanged for its checks.
- The initial regression patch is saved for baseline reproduction before
  applying production changes. It checks harness denial/missing runner/timeout,
  verification cancellation, live and persisted completion events, unresolved
  failures across continuation rounds, recovery after rechecking, missing
  harness classification and required versus optional verification.
- The isolated implementation draft distinguishes unavailable harnesses from
  execution failures, reconciles same-run checks before terminal events, and
  refuses completion for failures or unmet required verification. Rechecks
  must match the verifier/scope; a skipped/unavailable attempt cannot erase an
  unresolved failure. Diagnostic messages and persisted summaries explain a
  downgrade. The optional strict freshness gate remains separate.
- These H07 changes are formatted but **not yet compiled or tested**. After
  H18's benchmark gate finishes, reproduce the initial failures on the baseline
  and then verify the implementation. No H07 fix is claimed yet.

- **Follow-up H19:** `run` carries audited file mutations across rounds, but
  `decide_exit` compares `min_tool_calls` only with the latest round's tool
  count. An action-required continuation that already wrote files can therefore
  fail `ExecutionRequiredButNoTools` when its final round only answers. This
  source-traced case still needs reproduction and its own fix; H07's
  continuation regression uses coordinator-decides mode to isolate the
  independent verification-state defect.

## 2026-11-03 — H18: verification complete

- All nine discovered benchmark targets compiled in the release profile and
  passed their `--test` smoke runs. The sequential extra-CI script exited 0;
  process inspection confirmed no Cargo operation remained. These are
  correctness/compilation smoke tests, not measured performance comparisons.
- Final checks also passed: formatting, diff hygiene, UI palette restrictions
  and all six Python script tests. Together with 6,031 workspace tests,
  warnings-denied Clippy, default/single-frontend builds, runtime smoke tests,
  WASM guest builds and fresh deny/audit scans, local CI coverage is complete.
- H06 and H18 are ready to publish to draft PR #379. The native Windows-only
  H06 case remains unexecuted locally; no Windows runtime validation is claimed.
- The preceding goal turn made progress: completed the live benchmark wait and
  drafted the H07 implementation/regressions in isolation. Next, reproduce H07
  on the unfixed baseline, then verify the corrected implementation.

## 2026-11-03 — H07: reproduced and implementation applied

- Published H06/H18 and completed verification notes to PR #379 at
  `e216aeb`. The preceding goal turn made concrete progress: completed all
  remaining CI checks, published the verified milestones, and prepared H07.
- All five initial H07 regressions failed against the unfixed implementation
  (zero passed, five failed). Failures demonstrated false Completed outcomes
  for verification denial/cancellation and an earlier continuation failure,
  plus incorrect Error-versus-Unavailable classification.
- Applied the isolated implementation to the main topic branch after that
  reproduction. Focused verification is now running; the additional tests
  cover cancelled whole runs, invalid project roots, cache/record mapping,
  legacy/inconsistent records, verifier/scope identity and skipped rechecks.
- Basic completion checks consume typed results produced during the current
  run. They do not assert freshness or full coverage; the existing opt-in
  strict gate still checks fingerprints, generation and provenance.

## 2026-11-03 — H07 checks and remote CI follow-ups

- All 19 focused completion tests passed, including the five baseline
  regressions. The broader affected-library run passed the eval suite and
  1,860 orchestrator tests, with four fixture failures and one ignored test.
  Those fixtures asserted Completed despite lacking required verification.
- Driver-only fixtures now explicitly make verification optional while still
  checking written bytes. The execution-summary fixture asserts honest Partial
  status; stage tracking covers both optional and required verification and
  must omit Complete when the required check never passed. Production
  verification requirements were not disabled to accommodate these fixtures.
- Remote checks on `e216aeb` caught a platform-specific H06 test-fixture bug:
  staged bytes used a raw temporary-directory key instead of the resolved key
  used by `FilesystemTool::execute`. Windows verbatim paths and macOS temp-path
  symlinks made these different entries. The fixture now resolves the path
  through the production helper; its Linux regression passes. Native reruns
  are still required; this correction changes no production behavior.
- **H20:** remote cargo-deny rejected async-executor 1.14.0 as yanked. The
  [registry](https://crates.io/api/v1/crates/async-executor) confirms the yank
  on November 2; upstream's [changelog](https://github.com/smol-rs/async-executor/blob/master/CHANGELOG.md)
  identifies a use-after-free during executor drop, fixed in 1.14.1. Earlier
  local registry scans missed this yank; their pass is not proof of current
  registry state. Update only this transitive patch, then rerun both gates.

## 2026-11-03 — H07: completion-stage boundary and final checks

- The second affected-suite run passed 1,863 orchestrator tests but exposed
  an actual runtime bug: a Partial output still emitted the Complete stage.
  The single-agent runtime now advances to Complete only for Completed output;
  the regression checks both optional and required verification contracts.
  The workspace suite, Clippy and build are running on this correction.
- Fresh `cargo deny check` and `cargo audit` both exited 0 after updating only
  async-executor's locked version/checksum to 1.14.1. No advisory exceptions
  or dependency edges changed. This patch addresses H20's upstream regression.
- **H21:** the coordinator and supervised runtime tails also unconditionally
  emit Complete for partial outcomes (`runtime_runner.rs`). They need their
  own multi-agent regression coverage; the single-agent H07 fix does not close
  this independently observed presentation defect.
- Published-head CI is now fully settled: tests, build, Clippy, formatting,
  benchmarks, WASM, palette and cargo-audit passed; native Windows/macOS and
  cargo-deny failed for the H06 fixture and H20 issues recorded above. The next
  push must rerun those failing gates before claiming remote verification.
- **H19 tracing detail:** the ActionRequired prose-response branch in
  `process_provider_response` also tests only the current round's mutation
  count. Its retry instruction can demand a redundant write before the
  later minimum-tool-count check fails. Regression coverage must exercise
  both boundaries, not just change `decide_exit`.

## 2026-11-03 — H07/H20: workspace verification passed

- The preceding goal turn made concrete progress: fixed the single-agent
  completion-stage boundary, passed the focused and affected suites, and
  recorded H21 plus H19's second affected boundary. The existing verification
  process has now exited 0; no restart was needed.
- Final workspace tests: 6,041 passed, zero failed, 16 ignored across 85 groups,
  including integration and doc tests. Workspace Clippy (all targets, warnings
  denied) and the default workspace build also passed.
- Formatting, diff hygiene, palette enforcement, six Python script tests and
  the semantic index refresh passed. The supplemental CI script is now checking
  launcher/cache behavior, both frontend builds, four WASM guests and all nine
  benchmark smoke targets. These pending checks are not counted as passes.
- Prepared (not yet executed) H19 regression drafts outside the source tree:
  a one-iteration round cap followed by a prose finish after one/two real
  writes, and a minimum of two calls accumulated over separate rounds. The
  latter also checks whether an intermediate round cap incorrectly enforces
  a minimum intended for the finished task. Apply after H07 publication and
  reproduce against the baseline before implementing the continuation fix.

## 2026-11-04 — H07/H20: local CI coverage complete

- The preceding goal turn made progress: confirmed 6,041 passing workspace
  tests, warnings-denied Clippy and the workspace build; completed the launcher
  smoke and desktop-only build, and prepared H19 regression drafts.
- Current-state inspection confirms the supplemental script has finished:
  desktop-only and CLI-only builds, launcher/cache-replay and CLI startup
  smoke tests, all four WASM guests, and all nine release benchmark smoke
  targets passed. The benchmark log contains all nine executed targets and
  their successful cases, with no reported failure; no Cargo process remains.
  No performance measurement or comparison is claimed.
- Final formatting and diff checks passed. H07 now prevents failed/unmet
  verification from producing a successful single-agent result or Complete
  stage. H20 removes the yanked, vulnerable async-executor version. The H06
  test uses the production canonical path key on each platform.
- These are separate reviewable changes. Publication and native Windows/macOS
  plus cargo-deny reruns follow; local Linux passes do not prove native results.
- Remaining findings retain their scope, including H19's continuation counts
  and H21's multi-agent stage reporting. Overall harness superiority is not
  established by these fixes or by passing repository tests.

## 2026-11-04 — H19: continuation progress reproduced and corrected

- Published H06's native fixture correction (`353622b`), H20's dependency patch
  (`aa942b8`) and H07 (`f7af55c`) to draft PR #379. The source tree was clean
  before applying the H19 regression draft. Remote cargo-deny and macOS native
  checks now pass; Windows and remaining CI jobs are still running.
- The draft initially needed a borrow correction before it could compile;
  that compile failure is not behavioral reproduction. The corrected baseline
  ran 22 action-required tests: 20 passed, and both new regressions failed with
  `ExecutionRequiredButNoTools` despite real successful file writes.
- The loop now supplies the accumulated output to each continuation round.
  Prose completion and the minimum-call check both see prior execution, while
  returned counts remain round-local to avoid double counting. Intermediate
  caps with progress may continue toward the task's minimum; a run with no
  tool calls still fails, and a premature final answer below the minimum fails.
- All 22 focused action-required tests pass after the correction. Additional
  coverage now checks a premature final answer, isolation between separate
  runs on one loop, and retained/repaired verification failures in actual
  action-required continuations. Broader validation follows.
- **Remote follow-up:** all three native-shell jobs (Windows, macOS, Ubuntu),
  cargo-deny, cargo-audit, Clippy, formatting, palette and WASM passed on
  `f7af55c`. The H06 canonical fixture and H20 yank failures are resolved on
  the formerly failing gates. Full remote tests/build/benchmark jobs remain
  in progress. The Windows-only orchestrator ledger test is not part of the
  native-shell workflow, so these passes do not claim coverage of that case.
- The expanded H19 orchestrator suite passes: 1,868 tests, zero failures,
  one ignored. Full workspace tests, Clippy and build are now running.
- **H15 preparation:** drafted a deterministic regression that rewrites a
  cached file with different bytes of the same length, then restores its mtime
  using `FileTimes`. It is outside the source tree and not yet executed.
  The 64 KiB cache bound allows bounded byte validation; a future fix must
  compare current disk bytes, honor cancellation and retain the policy gate.
- **H22:** `canonical_project_path` rewrites backslashes to slashes on every
  platform before the cache re-stats a path. `FilesystemTool` resolves native
  paths, where Unix backslashes are literal filename characters. A cache row
  for `dir/file.txt` can therefore answer a request for the different file
  `dir\file.txt`; hashing only the normalized path's current bytes would not
  fix this discrepancy. A Unix regression draft is prepared alongside H15;
  neither draft has yet been executed. Cache validation must use the same
  resolved, contained path as actual tool execution.
- **Published milestone verified remotely:** every check on `f7af55c` is
  green, including full tests, default build, benchmark smoke, Clippy, fmt,
  WASM, palette, both supply-chain gates and all three native-shell platforms.
  H07/H20 are verified and awaiting merge; H19 remains uncommitted under its
  own workspace validation.
- **H13 preparation:** located both public JSON parser entrypoints in
  `prompts.rs`; both normalize the entire response before any parse attempt.
  Prepared strict/fenced/prose and typed-candidate regressions covering Unicode
  keys, quoted punctuation, nonbreaking hyphens, escaped text and emoji. These
  drafts are not yet applied or executed. Existing malformed-input repair
  behavior also needs review so a parser fix does not silently alter values.
- H13's prepared cases also cover mixed smart-quote delimiters with ordinary
  JSON strings, smart-quoted strings containing nonbreaking hyphens, and
  nonbreaking whitespace inside versus outside values. Repair should be
  restricted to structural delimiters/whitespace, preserving string contents;
  simply moving the existing lossy normalizer later would leave that hazard.

## 2026-11-04 — H19: workspace MCP probe failure under investigation

- The workspace run reported an MCP probe timeout in
  `explicit_mcp_probe_connects_and_stops_without_persisting_enable_switches`
  (`settings_commands.rs`); that target passed 10 tests and failed one.
  `--no-fail-fast` is still running the remaining targets. H19 is not yet
  claimed fully verified, and its Clippy/build tail will not run after failure.
- **H23:** this Python stdio fixture configures a three-second probe timeout.
  The request path installs its pending response before writing, so inspection
  has not established a response-registration race. No runtime or timeout
  change is justified yet. Reproduce the focused case and its integration
  target after the active Cargo run ends, preserving this first failure.

## 2026-11-04 — H23: focused reruns pass without changes

- The preceding goal turn made concrete progress: finished the first H19
  workspace run, recorded its one failure, inspected the probe path and
  launched targeted reproduction. That run passed 6,044 tests, failed one,
  and ignored 16 across 85 groups; its Clippy/build tail did not execute.
- Current-state inspection confirms the focused probe passed unchanged in
  0.07 seconds, and its complete 11-test integration target passed in 0.40
  seconds. The process has exited and no Cargo operation remains.
- The earlier timeout remains an intermittent observation with unproven cause;
  no timeout, runtime or fixture change was made to hide it. H23 remains open.
  Rerun the complete workspace suite, then the remaining H19 CI checks.
- H13's implementation draft is now isolated in disk-backed worktree
  `Concerto-worktrees/h13` (`fix/harness-json-integrity`, based on `f7af55c`).
  It tries original candidates first and limits fallback repair to smart quote
  delimiters and nonbreaking whitespace outside strings; string punctuation
  is preserved. The initial test-only patch is saved separately for baseline
  reproduction. This draft is formatted but has not been compiled or tested;
  the primary H19 source remains unchanged during its verification.
- The unchanged full-workspace rerun passed: 6,045 tests, zero failures,
  16 ignored across 85 groups. This includes the previously failing MCP probe;
  H23 remains open because a pass does not establish its original cause.
- Fresh cargo-deny and cargo-audit both passed on November 4 (1,403 advisories
  in the fetched audit database). The sequential Clippy/build/supplemental
  pipeline is still running; incomplete checks are not counted as passes.
- H15/H22 now have an isolated draft in `Concerto-worktrees/h15`
  (`fix/harness-cache-integrity`, based on `f7af55c`). It compares requested
  and cached paths through the filesystem tool's contained resolver, validates
  cached hashes, and compares a bounded read of current bytes. Cancellation
  is checked around resolution/read work; later external edits remain possible.
- All three production cache callers check advisory Allow before the draft's
  new disk-byte validation. Non-Allow or a cache miss still reaches normal
  policy-checked execution. Draft tests cover restored mtimes, literal Unix
  backslashes, symlink-parent resolution, symlink escape, cache-size boundaries
  and cancellation. The initial test-only patch is saved for reproduction.
- These H15/H22 changes are formatted but not compiled or tested. Apply their
  diff after H19 publication; do not copy their whole `agent_loop.rs` over the
  primary tree, because this older worktree does not contain the H19 fix.

## 2026-11-04 — H19: verification complete and ready to publish

- The sequential final-check pipeline exited 0. The full workspace rerun
  passed 6,045 tests with zero failures and 16 ignored; Clippy with warnings
  denied, default workspace build, both frontend builds, launcher/cache and
  CLI smoke tests, all four WASM guests and all nine benchmark smoke targets
  passed. The benchmark run is a correctness smoke, not a performance claim.
- Formatting/diff checks, palette enforcement, all six Python script tests,
  semantic index refresh, and both fresh supply-chain scans passed. Process
  inspection confirms no Cargo operation remains.
- H19 is ready to publish as one logical fix with its regressions and changelog.
  The earlier MCP timeout is preserved under H23; its cause remains unproven.
  H13/H15/H22 isolated drafts are not included in this verified milestone.
