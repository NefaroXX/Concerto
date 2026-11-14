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
| H08 | P2 | Compaction counts historical usage as current context size | Fixed and verified; awaiting merge |
| H09 | P2 | Compaction truncates constraints and unresolved work | Open |
| H10 | P2 | Agent-driven benchmark fixture policy denies tools | Fixed and verified; awaiting merge |
| H11 | P2 | Empty benchmark outcomes force a zero pass score | Fixed and verified; awaiting merge |
| H12 | P2 | Durable event subscriber queues have no memory bound | Open |
| H13 | P1 | Structured-response parsing rewrites Unicode before attempting JSON parsing | Fixed and verified; awaiting merge |
| H14 | P2 | Checkpoint startup can degrade into ToolNotFound; CLI test failed intermittently | Open; observed during H02 verification |
| H15 | P1 | Orchestrator read cache trusts matching size/mtime without checking current bytes | Fixed and verified; awaiting merge |
| H16 | P2 | Coverage runners misread reports, lose cancellation and trust incomplete test evidence | Full local battery verified; remote CI pending |
| H17 | P1 | Resumed history lacks restored read-version claims when a new gate is constructed | Fixed and verified; awaiting merge |
| H18 | P1 | Locked Wasmtime 36.0.19 fails fresh security gates on RUSTSEC-2026-0424 | Fixed and verified; awaiting merge |
| H19 | P2 | Action-required minimum tool-call check ignores earlier continuation rounds | Fixed and verified; awaiting merge |
| H20 | P1 | Yanked async-executor 1.14.0 remains locked after an upstream memory-safety regression | Fixed and verified; awaiting merge |
| H21 | P2 | Multi-agent runtime stage reports Complete for partial outputs | Fixed and verified; awaiting merge |
| H22 | P1 | Cache path normalization can select a different file from the filesystem tool | Fixed and verified with H15; awaiting merge |
| H23 | P2 | CLI MCP probe integration test timed out during the H19 workspace run | Open; cause under investigation |
| H24 | P2 | Final provider context guard omits replayable reasoning from its size estimate | Fixed and verified; awaiting merge |
| H25 | P2 | Benchmark required-pattern checks count unreadable files as matching | Fixed and verified; awaiting merge |
| H26 | P2 | Nine non-standard benchmark task manifests contain invalid-length IDs | Fixed and verified; awaiting merge |
| H27 | P1 | Native orchestrator builds fail on macOS (Linux syscall) and Windows (test helper cfgs) | Fixed and verified; awaiting merge |
| H28 | P2 | Unused dependencies and inconsistent versions inflate the dependency graph | Initial cleanup verified on 43a3e79; 67 duplicate families remain under review |
| H29 | P2 | Declared Rust 1.88 minimum contradicts required locked dependencies | Open; dependency manifests verified |
| H30 | P1 | Locked wasmtimer retains unmaintained pin-utils and fails the supply-chain gate | Fixed and verified; awaiting merge |
| H31 | P1 | Bundled SQLite/SQLCipher fails the WAL memory-disclosure security gate | Fixed and verified; awaiting merge |
| H32 | P1 | Locked rustix fails the extended-attribute buffer soundness security gate | Fixed and verified; awaiting merge |
| H33 | P2 | Nested Cargo fixtures can execute each other’s binaries with a shared target directory | Fixed and verified on 43a3e79; awaiting merge |
| H34 | P2 | Bounded subprocess capture can conceal inconsistent test evidence without marking output incomplete | Open; source path traced, execution reproduction pending |
| H35 | P1 | Locked quinn-udp fails fresh raw-lockfile security scan | Patched and locally verified; remote CI pending |
| H36 | P2 | cargo-audit can report incomplete registry checks with exit zero | Strict gate locally verified, including complete real scan; remote CI pending |


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

## 2026-11-04 — H13/H15/H22: baseline failures reproduced, fixes applied

- Published H19 as `e07991c` and updated draft PR #379. The primary tree was
  clean before applying the saved test-only patches. H19's remote native,
  Clippy, formatting, WASM and supply-chain checks are green; full remote
  tests/build/benchmarks are still running.
- H13 baseline: 44 parser tests passed and all three new regressions failed.
  Valid JSON silently changed Unicode keys/values, and typed/repair cases lost
  punctuation. H15/H22 baseline: 12 cache tests passed, four regressions failed
  (same-size/restored-mtime edit, literal backslash, symlink parent and symlink
  escape). These are behavioral failures, not compile failures.
- Applied the isolated production diffs after reproduction, retaining H19's
  accumulated counters. The primary tree is now authoritative; the older
  worktrees must not overwrite subsequent primary fixes. The orchestrator
  suite is running on the combined changes.
- ADR-65 now explicitly supersedes its originally accepted metadata-only risk.
  Cache hits pay for bounded disk-byte validation and retain original fact
  attribution; no disk-I/O saving or atomic write guarantee is claimed.
- All 1,876 orchestrator tests pass after applying H13/H15/H22 (one ignored),
  including every reproduced regression and existing policy/cancellation/cache
  coverage. The full workspace, Clippy, build and supplemental CI pipeline
  follows. No native runtime result is inferred from this Linux suite.
- All twelve remote checks on H19's published head `e07991c` have now passed,
  including Tests and Benchmarks. This does not imply execution of the separate
  Windows-only orchestrator absence-alias regression.
- **H16 tracing:** coverage discovery uses `status().is_ok()` rather than the
  process success bit, so an unavailable Cargo subcommand is classified as
  installed. On this host `cargo llvm-cov --version` exits 101 with “no such
  command”; current discovery would nevertheless select it. Standalone
  `run_coverage` also ignores its cancellation token and blocks on child exit.
  No coverage implementation changes are included in H13/H15/H22.

## 2026-11-04 — H13: verification complete

- The sequential H13/H15/H22 validation pipeline exited 0. Full workspace
  tests passed: 6,053 passed, zero failed, 16 ignored across 85 groups. All
  1,876 orchestrator tests passed (one ignored), including all seven original
  behavioral failures and the cache boundary/cancellation regression.
- Workspace Clippy (all targets, warnings denied), default build, both frontend
  builds, launcher/cache/CLI smoke tests, all four WASM guests and all nine
  benchmark smoke targets passed. Benchmarks establish correctness only.
- Formatting, diff hygiene, palette checks, six Python tests and semantic index
  refresh passed. Fresh cargo-deny and cargo-audit scans passed under the
  existing exception policy. No source edits occurred during the final pipeline.
- H13 is published separately: original valid JSON is tried first, and fallback
  repair preserves string data. The cache fix was tested alongside it and is
  recorded in the following milestone. H23's earlier timeout remains open;
  the successful current suite does not establish its cause.

## 2026-11-04 — H15/H22: verification complete

- Published H13 as `e33045d`. H15/H22 now land together because fresh bytes
  only establish correctness when they belong to the file actually requested.
- Regressions prove rejection of same-length edits with restored mtime,
  Unix literal-backslash aliases, symlink-parent lexical collapse and a path
  replaced with an escaping symlink. Empty, Unicode and exact-64-KiB reads
  remain cacheable; pre-cancelled reads miss. All tests passed in the unchanged
  combined tree and workspace validation recorded above.
- All three production cache callers obtain advisory Allow before validation
  reads disk. Native resolver identity/containment, same-handle metadata and
  bounded current-byte comparison supplement the existing clean-row/hash rules.
  Any uncertainty falls through to ordinary policy-gated execution.
- ADR-65 and the cache API documentation explicitly supersede metadata-only
  freshness. No filesystem lock, atomic-write guarantee or disk-I/O saving is
  claimed. Native Unix regressions ran on Linux; Windows-specific orchestrator
  coverage remains an explicit verification limitation.

## 2026-11-04 — H08: size the current context independently of usage

- Published H13 and H15/H22 as `e33045d` and `2793320` and updated draft PR
  #379. Their full local verification is complete; the new remote jobs are
  running. The primary worktree was clean before starting H08.
- Three new regressions failed against the previous estimator (nine existing
  context-engine tests passed): large historical usage inflated tiny windows,
  zero usage hid large tool payloads, and a persisted tiny conversation was
  unnecessarily checkpointed. The baseline log is retained.
- The estimator now measures current content/tool payloads and replayable
  reasoning, independent of usage counters. Structured tool results replace
  legacy display text for sizing. Saturating arithmetic bounds accumulation;
  the public payload/usage shape and accounting writes are unchanged.
- The persistence regression compares stored rows before and after compaction:
  the existing SQLite adapter restores stored zero usage as unknown, so the
  test does not assume zero-vs-unknown round-trip preservation. Added payload
  coverage checks reasoning and legacy-display duplication. Focused context
  tests are running; no completed verification is claimed yet.
- **H24:** `providers/context_guard.rs::estimate_message_tokens` also omits
  `reasoning_content`, despite OpenAI-compatible adapters replaying it. H08
  corrects the durable compaction estimate, not that final request guard. A
  provider-boundary regression and fix remain required.
- H08's focused context run passed all 29 tests. The provider guard follow-up
  now has two execution-boundary regressions: reject oversized replay reasoning
  before calling the provider, and preserve fitting reasoning even with very
  large historical usage counters. Its baseline is running separately.
- H17 follow-up tracing confirms reads intentionally return `gate_seq = 0`
  without a gate WAL row (`gate.rs::run_read_only`), and `WriteGate::new`
  constructs an empty `ReadVersions` map. A resumed single-agent runtime
  constructs a fresh gate, so replaying that WAL cannot recover read claims.
  Any restoration fix must preserve the actual returned-byte observation,
  not replace it with a post-resume disk hash.

## 2026-11-04 — H24: account for replay reasoning at the provider boundary

- The provider baseline passed seven tests and failed the new oversized-
  reasoning rejection test: the inner provider received a request whose
  reasoning alone exceeded the input budget. The fitting-reasoning case
  already passed and remains a preservation check.
- The final guard now includes reasoning bytes in its existing estimate.
  It retains the current reduction order and returns ContextOverflow before
  dispatch when the request still cannot fit. No reasoning truncation or
  historical-usage-based sizing was introduced.
- Both changes are being verified together through the provider and
  orchestrator suites, then the workspace and supplemental CI checks.
- The affected suites passed: 1,880 orchestrator tests and 594 provider tests,
  with one ignored in each. Formatting/diff checks, palette enforcement, all
  six Python tests, semantic indexing and fresh cargo-deny/cargo-audit scans
  passed. Full workspace validation is running on the unchanged source.
- Prepared two H17 regressions in the separate disk-backed `h17` worktree:
  re-create a gate against the same database/session after an observed read or
  absence, then reject an intervening external edit/create. These are formatted
  drafts only, not compiled or run. They are not part of H08/H24 validation.
- All twelve remote CI checks on published `2793320` now pass, including
  workspace tests/build/benchmarks, native Windows/macOS/Linux jobs and both
  supply-chain gates. H08/H24 remain uncommitted pending the live full local
  pipeline; no remote result is attributed to those newer changes.

## 2026-11-04 — H08: final verification complete

- The sequential H08/H24 pipeline exited 0. Full workspace tests passed:
  6,059 passed, zero failed, 16 ignored across 85 groups. Affected suites passed
  1,880 orchestrator and 594 provider tests (one ignored in each).
- Workspace Clippy (all targets, warnings denied), default workspace build,
  both frontend builds, launcher/cache/CLI smoke checks, all four WASM guests
  and all nine release benchmark smoke targets passed. No performance claim
  is inferred from smoke benchmarks.
- Formatting/diff checks, palette enforcement, six Python script tests, semantic
  index refresh and both fresh supply-chain scans passed. No primary source
  changes occurred during the final pipeline. No Cargo process remains.
- H08 is published separately with the sizing regressions and ADR-48 amendment;
  H24 was tested alongside it and follows as a separate provider fix.
- H17 remains an isolated draft: durable claims keyed by project/session/agent,
  bounded per-session storage, cleanup, explicit legacy reread requirements,
  cancellation-aware lookup and persistence failures that do not fall back to
  disk hashes. Six regression drafts now cover restart, absence, own-write
  advancement/isolation, legacy recovery, unavailable storage and capacity.
  The migration/bounded-upsert SQL passed an in-memory SQLite probe; the Rust
  draft has only been formatted, not compiled or tested. No H17 guarantee is
  claimed from H08/H24 validation.

## 2026-11-04 — H24: final verification complete

- Published H08 as `ba06570`. H24 adds reasoning bytes to the provider guard's
  existing estimate; oversized replay reasoning is rejected before dispatch
  when reduction cannot fit it, while fitting reasoning remains unchanged.
- The new rejection regression failed before the fix; all eight focused guard
  tests pass in the complete 594-test provider suite. The unchanged combined
  H08/H24 tree passed every local check recorded above (6,059 workspace tests).
- This remains a heuristic estimate, not an exact model tokenizer. Neither
  this fix nor H08 claims to preserve every constraint through compaction;
  H09 remains open. Remote checks follow publication of this new head.

## 2026-11-04 — H17: reproduce lost resume claims and apply durable provenance

- Published H08/H24 as `ba06570` and `3dc5f0a` and updated draft PR #379.
  Their full local verification is complete; remote checks follow publication.
- Both H17 baseline regressions failed: a fresh gate over the same database
  stamped the new disk hash instead of the returned-byte hash or observed
  absence from the earlier gate. The test-only baseline log is retained.
- Applied the isolated H17 diff after reproducing the failures, preserving
  H08/H24. The primary tree is now authoritative. Migration 041 stores native
  project/session/agent/path claims with a persistent 4,096-entry limit and
  legacy-session markers; session cleanup removes them. Old sessions with
  unknown provenance require fresh reads instead of guessing current bytes.
- Reads publish success only after provenance commits. Storage failure and
  cancelled lookup cannot revert to arrival-time stamping. Tracked writes
  advance durable claims from known submitted bytes; update failures leave
  conservative prior claims and explicitly report that the write executed.
  Policy/approval and write-ahead decision ordering are unchanged.
- Eight orchestrator regressions now cover restart after read/absence,
  own-write advancement and isolation, legacy recovery, unavailable storage,
  persistent capacity, native project-root aliases and cancellation while
  waiting for a connection. A session-cleanup regression covers the new rows.
  The gate suite is compiling; no Rust verification result is claimed yet.
- ADR-60 documents the new durability boundary, legacy recovery and remaining
  limitations. The in-memory SQLite migration/upsert probe passed, including
  rejection of a new entry at capacity and refresh of an existing entry.
- All 71 gate/in-process-gate tests passed, including the eight new H17
  regressions. The session-cleanup test and broader suites follow.
- Extended the existing native-platform workflow to run the orchestrator gate
  and read-version tests plus session-provenance cleanup on Windows, macOS and
  Linux. This will cover H06's previously unexecuted Windows-only absence-alias
  regression; it is not reported as executed until the new workflow runs.
- The complete affected suites passed: 1,888 orchestrator tests (one ignored)
  and 206 session tests, including cleanup. Clippy found one `question_mark`
  lint in the extracted pure read parser; replaced its `let...else` with `?`
  without changing behavior. The final workspace/Clippy/build/supplemental
  pipeline now runs on that correction. The isolated draft must not overwrite
  primary changes made after application.
- H17's index refresh, formatting/diff checks, palette enforcement, six Python
  tests and both fresh supply-chain scans passed. All twelve remote checks on
  H08/H24's published head `3dc5f0a` also passed; these do not cover H17 yet.
- **H25:** the benchmark scorer leaves `all_match = true` when reading a file
  with required patterns fails (`eval/runner.rs`). A binary or unreadable file
  can therefore satisfy checks whose patterns were never inspected.
- **H26:** nine task manifests across bug-fix/library/CLI/web-API benchmark
  categories have 27-character IDs, incompatible with the 26-character ULID
  field. Inspection confirmed each length; a corpus-loading regression is
  still needed. Standard-suite tests do not exercise these category manifests.
  These benchmark follow-ups are outside the current H17 verification tree.

- H17's full workspace test run passed: 6,068 passed, zero failed, 16 ignored
  across 85 groups. Workspace Clippy with all targets and warnings denied also
  passed. The sequential build/frontend/WASM/benchmark checks are still live;
  no terminal result is claimed for that pipeline yet.
- The H17 default workspace build and launcher/cache-replay smoke checks also
  passed. Frontend-only builds and supplemental CI checks remain in progress.
- Benchmark follow-ups are drafted only in the disk-backed `h10-h11` worktree.
  Five regressions cover the fixture policy/audit, optional empty constraints,
  actual suite completion/test aggregation, unreadable required content and
  loading all six committed categories. The baseline preserves the old
  behavior; a separate draft adds the explicit fixture allow rule, correct
  scoring and nine valid unique IDs. Neither draft is yet compiled or tested.

## 2026-11-04 — H17: final local verification complete

- The sequential final pipeline exited 0. Full workspace tests passed: 6,068
  passed, zero failed, 16 ignored across 85 groups. All-target workspace Clippy
  with warnings denied, the workspace build, both frontend builds, launcher/
  cache/CLI smoke checks, all four WASM guests and all nine release benchmark
  smoke targets passed. No performance improvement is inferred from smoke runs.
- Formatting/diff checks, palette enforcement, six Python tests, semantic-index
  refresh and fresh `cargo deny check`/`cargo audit` passed. The tested primary
  Rust source remained unchanged throughout the final pipeline.
- Publishing the durable ledger, migration 041, nine regressions, ADR amendment
  and expanded native workflow together. Windows/macOS execution of the new
  gate coverage remains unverified until remote CI runs this revision.
- The previous goal turn made concrete progress: H17 verification completed,
  and five benchmark regressions plus an isolated implementation were drafted.
  The broader audit remains active; the remaining open findings are unchanged.

## 2026-11-04 — H10/H11/H25/H26: benchmark validity regressions

- Published H17 as `69c2ac3` and updated draft PR #379 to describe all sixteen
  fixed findings and their limits. Expanded native gate coverage is pending.
- Applied only the benchmark regression/extraction patch to the primary tree.
  Policy construction and score calculation still preserve the defective
  behavior so the new assertions can reproduce each finding. The final fix
  remains isolated until baseline test results have been inspected.
- The four eval regressions failed behaviorally on the baseline: invalid ID
  length, empty-constraint score 0, unreadable-required-content score 1, and
  aggregate score 0 instead of one passing task out of three. The policy
  regression is still compiling; its result is not yet claimed.
- Remote H17 native CI failed before running tests because the unquoted
  `gate::` filter is invalid YAML at the final colon. PyYAML reproduced the
  exact line/column error. Quoted the command; all workflow YAML now parses.
  This workflow-only correction does not change the verified Rust source or
  imply that Windows/macOS gate tests have passed.
- The policy baseline also failed behaviorally (`Deny` instead of `Allow`
  for a filesystem write). Both baseline commands exited 101 with the five
  intended assertion failures, not compilation errors. Logs are retained.
- Applied the separate implementation patch after the baseline: explicit
  audited permissive fixture policy, neutral empty file constraints, failed
  unreadable pattern checks, and nine deterministic valid unique task IDs.
  Five focused regressions are now running on the primary tree.
- Published the native-workflow quoting correction as `9f1eaf9`; its expanded
  Windows/macOS/Linux gate test results remain pending.
- All five focused regressions now pass: four scoring/corpus tests and one
  policy/audit test. The corpus test loads all 22 tasks across six categories;
  the suite regression proves exactly one pass among a valid completed task,
  an incomplete task and a task with failing tests. The policy test confirms
  both filesystem and shell Allow decisions remain in the audit log.
- Starting full workspace tests, all-target Clippy, workspace/frontend builds
  and the supplemental CI smoke checks on this unchanged primary source.
- Benchmark-fix index refresh, palette enforcement, six Python script tests
  and fresh `cargo deny check`/`cargo audit` passed. The full Rust pipeline is
  still running; there is no workspace result for the benchmark fixes yet.
- H17's corrected native workflow is running on Windows/Linux, with macOS
  queued. Remote fmt, Clippy, WASM, palette and deny checks have passed on
  `9f1eaf9`; these checks do not cover the uncommitted benchmark fixes.
- H21 follow-up remains isolated in the `h21` worktree. Both multi-agent
  tails unconditionally publish Complete even for Partial (and the coordinator
  may return AwaitingUser). A shared status-to-stage boundary and three
  observable event/state regression drafts cover unfinished and completed
  results. No H21 Rust compile/test result is claimed yet.
- The benchmark-fix workspace suite passed: 6,073 passed, zero failed,
  16 ignored across 85 groups. Clippy/build/supplemental checks are still live.
- All nine main remote CI checks on H17's corrected head `9f1eaf9` passed.
  The expanded native Linux job also passed both the gate/resume suite and
  session-provenance cleanup. Windows/macOS gate steps remain in progress.
- All-target workspace Clippy passed with warnings denied for the benchmark
  fixes. Workspace/frontend builds and supplemental checks continue.
- H16 follow-up evidence: Context7's llvm-cov documentation confirms explicit
  JSON report-file output; the current code mixes report JSON and test logs.
  Tarpaulin's [released 0.37.5 report implementation](https://github.com/xd009642/tarpaulin/blob/0.37.5/src/report/json.rs)
  writes `tarpaulin-report.json` with `coverage`, `covered` and `coverable`.
  Concerto instead reads stdout and expects `coverage_percent`, defaulting a
  missing field to zero. The existing parser tests use that invented shape.
  This adds concrete report-format evidence to H16; no coverage code has been
  changed or claimed fixed in the benchmark milestone.
- Expanded macOS CI exposed H27: `supervisor.rs:867` calls Linux-only
  `libc::prctl(PR_SET_PDEATHSIG, ...)` under `cfg(unix)`, causing E0425 before
  gate tests could run. The same code exists in published pre-H17 `3dc5f0a`;
  older native jobs never compiled this crate. The failed job log is retained.
- Drafted a targeted platform guard in the isolated H21 worktree (exported
  separately as the H27 patch): keep Unix process-group setup, restrict the
  Linux parent-death syscall to Linux, and document the actual platform
  guarantee. This is not applied to the primary source during its live
  benchmark verification. Native macOS execution remains unverified.

## 2026-11-04 — H10/H11/H25/H26: final local verification complete

- All 6,073 workspace tests passed, with zero failures and 16 ignored across
  85 groups. All-target Clippy with warnings denied, workspace/default and
  both frontend builds, launcher/cache/CLI smoke checks, all four WASM guests,
  formatting/diff checks, palette enforcement, six Python tests, semantic
  indexing and both fresh supply-chain scans passed.
- The original pipeline handle was unavailable after its complete final bench
  log. Process inspection confirmed no Cargo/rustc/linker remained. Earlier
  stages had finished successfully (the sequential pipeline reached its last
  command). Re-ran only that cached final benchmark command to capture an
  authoritative terminal result: exit 0 for all nine targets, also persisted
  to a status artifact. No incomplete run is reported as successful.
- Expanded Windows CI found the other H27 branch: Unix-only rollback tests
  leave seven helper items unused when the orchestrator test crate builds
  with warnings denied. The failed log is retained. The isolated correction
  makes two already-portable rollback tests run on all platforms and limits
  the Unix sleep fixture helper to Unix. Native CI will run the rollback
  suite explicitly; no lint is weakened.
- Publishing the four benchmark fixes as one evaluation-validity milestone.
  H27 portability corrections and the H21 completion-stage draft are still
  separate, unapplied work; broader harness findings remain open.

## 2026-11-04 — CI repair takes priority

- The benchmark commit `9adafff` was pushed while native CI failures were
  unresolved. That was a workflow error: passing Linux checks was not enough
  to treat the branch as ready. Unrelated audit work is stopped until every
  required check on the final revision passes. The broader audit goal remains
  paused at the user's request.
- Applied the native build corrections: Linux-only parent-death syscall guard,
  Unix-only sleep fixture helper, and two portable rollback regressions enabled
  on Windows. Added explicit native rollback test execution. No warning or
  test failure is suppressed, and warnings-as-errors remains enabled.
- Verification must include the exact pushed commit on Linux, macOS and
  Windows. Local success alone will not close this repair.

- Correction to the failure attribution: the user's handoff baseline was
  green. Calling failures exposed after changing CI coverage “pre-existing
  failures” was incorrect. The changed branch and verification configuration
  are this work's responsibility; source age does not excuse a red check.
  Completion requires restoring every required check to green.

## 2026-11-05 — CI warning cleanup

- Updated checkout to v6 in the two required PR workflows to remove the
  Node 20 deprecation warning shown in their job logs. Checked the official
  [checkout migration notes](https://github.com/actions/checkout/blob/v6/README.md)
  against the observed hosted runner version 2.338.2; it exceeds both stated
  runner minimums. Workflow YAML parses successfully.
- The applied repair passed all 1,888 orchestrator library tests (one ignored)
  and workspace all-target Clippy with warnings denied. Both exit codes are
  recorded as 0 in the validation artifact. YAML parsing and formatting/diff
  checks passed. Publishing this repair to run the complete required matrix;
  it is not considered complete until every check on that commit passes.

## 2026-11-05 — CI repair verified on the published revision

- All twelve required GitHub checks passed on `8682c373f7a76d2aa0fcdc80eba3fe8789307a9e`:
  [main CI](https://github.com/NefaroXX/Concerto/actions/runs/41938380129) and
  [native CI](https://github.com/NefaroXX/Concerto/actions/runs/41938380066).
  These results attest that exact source revision, not an earlier green run.
- Inspected native job logs: Linux/macOS each ran 74 gate tests, one session
  cleanup test and eight rollback tests; Windows ran 73, one and six. Both
  resume regressions and both portable rollback tests actually executed and
  passed. Windows/macOS logs contain no warning annotations.
- The full local battery also completed on the same revision: 6,073 workspace
  tests passed, zero failed, 16 ignored across 85 groups; Clippy, formatting,
  workspace/frontend/WASM builds, launcher/cache/CLI smoke checks, nine release
  benchmark smoke targets, palette and six Python tests all exited 0. Both
  security scans exited 0. The final status artifacts record the revision and
  each command's exit code, so an expired process handle does not lose proof.
- Warning review: cargo-deny emitted 74 duplicate-version warnings under the
  repository's explicit `multiple-versions = "warn"` policy. These remain
  visible; no suppression or policy relaxation was added. GitHub's remaining
  inspected annotations were informational Ubuntu runner-image rollout
  notices. This milestone is not described as warning-free.
- The user resumed the broader audit after this repair. New fixes must retain
  this discipline: reproduce, verify locally, publish, then resolve every
  required check before proceeding to another milestone.

## 2026-11-05 — H21: multi-agent completion stage regressions

- Reconfirmed all twelve required checks pass on published revision `8682c37`
  before resuming changes. Both multi-agent runtime tails still emit Complete
  unconditionally, contradicting their Partial/AwaitingUser output status.
- Extracted their existing terminal-stage behavior into `StageTracker::finish`
  without changing its unconditional transition, so regressions can demonstrate
  the defect before correcting it. Added status/event tests for Partial,
  AwaitingUser and Completed, plus a real supervised runtime test whose child
  executable is absent. That test checks the failed-child summary, Partial
  status, and absence of a Complete stage event.
- The baseline regression command is compiling with `RUSTFLAGS="-D warnings"`
  and `CARGO_INCREMENTAL=0`. No passing result or fix is claimed yet. Build
  artifacts remain in this repository's disk-backed `target/` directory.

- Baseline execution finished: four stage tests passed and all three negative
  regressions failed at the expected assertions (exit 101). The failed-child
  runtime emitted `[Execute, Complete]` despite returning Partial; both status
  tests incorrectly advanced to Complete as well. The exact baseline patch,
  output and terminal status are preserved under `target/harness-reliability/`.
- Applied the fix: only Completed advances to Complete. Coordinator and
  supervised runs use their actual returned status; the single-agent runtime
  now uses the same helper while preserving its existing completion rule.
  Successful completion still publishes only one transition. Full validation
  is pending; this milestone has not been published.
- Native CI now explicitly executes the stage-tracker regression filter on
  Linux, macOS and Windows, including the failed-child runtime scenario.
  Workflow YAML parses successfully; all warning policies remain unchanged.
- Fixed-source focused run passed all seven stage tests, including all three
  regressions that failed on the baseline and the successful-completion control.
  Formatting, diff checks and native workflow YAML validation passed. The full
  workspace/CI-equivalent battery is running sequentially with warnings denied;
  `h21-status.json` records each completed command. Commit/push and remote CI
  verification remain pending until that battery succeeds.

## 2026-11-05 — Dependency warning triage during H21 validation

- Traced three duplicate-version warnings (`axum`, `axum-core`, `matchit`) to
  `utoipa-axum = "0.1"`, declared in the workspace and API-server manifests.
  A repository-wide reference search found only these manifest declarations;
  the server builds its routes with `axum::Router`, `utoipa::OpenApi` and
  `utoipa_swagger_ui::SwaggerUi` directly (`crates/api-server/src/routes.rs`).
- Recorded H28 for removal and verification after H21's required checks pass.
  Existing API tests exercise the OpenAPI endpoint, docs-disabled routing,
  and embedded Swagger assets, providing behavior checks for that cleanup.
  No dependency or warning-policy changes are mixed into the running H21
  validation; all 74 warnings remain visible until individually resolved.

## 2026-11-06 — H21: workspace passed; remaining validation resumed

- The full workspace run exited 0: 6,077 passed, zero failed, 16 ignored across
  85 unit/integration/doc-test groups. Confirmed the code, workflow and changelog
  diff still matches the saved validation snapshot; only the work log changed.
- The validation process and its session handle are no longer present. Its
  status file records workspace success but no Clippy result, and the Clippy
  log ends during dependency checking. Preserved that incomplete log and
  resumed the remaining battery from Clippy. An interrupted check is not a pass;
  the already completed workspace result does not need to be inferred or rerun.
- Reclaimed approximately 23 GiB before continuing builds by removing 133
  obsolete generated test executables whose Cargo fingerprints have no warning
  flags. Current `-D warnings` executables, source and validation logs remain.
  The removed-path inventory is saved in `h21-obsolete-test-artifacts.json`;
  available repository disk space increased from 17 to 40 GiB.
- Resumed workspace all-target Clippy completed successfully with warnings
  denied (exit 0). The full workspace build and remaining smoke/security gates
  are now running; publication remains pending their results.
- Full workspace build exited 0. Default launcher/config and cache-report
  smoke checks passed before the frontend feature builds started. Source review
  confirmed blueprint feed labels exclude Complete, so the coordinator's feed
  cannot independently bypass H21's returned-status rule.
- Desktop-only frontend build passed (7m23s). The serial extra-check script
  progressed to the CLI-only build, proving launcher/cache smoke and desktop
  commands returned successfully under `set -euo pipefail`. Saved these substep
  results separately; CLI/WASM builds, benchmark smoke and security scans remain
  pending. No new commit has been pushed.
- CLI-only frontend build, CLI startup/dispatch smoke checks and all four
  wasm32-wasip2 guest builds passed. The serial script has reached the nine
  release benchmark smoke targets. Those and the subsequent palette/Python
  checks and security scans remain pending.

## 2026-11-06 — H29: declared Rust minimum contradicts dependencies

- The workspace declares `rust-version = "1.88"`, but its required SQLx 0.9.0
  and SQLite backend declare Rust 1.94.0. Those packages are used in the passed
  workspace build. Also found higher minimums in opentelemetry-langfuse 0.7.0
  (1.93.1) and tree-sitter 0.27.1 (1.90).
- Verified these values using locked offline Cargo metadata and the downloaded
  dependency manifests. Current CI uses Rust 1.96.0; its green result does not
  establish 1.88 support. H29 requires testing the actual supported minimum and
  aligning the manifest/documentation/CI contract, without claiming an untested
  version. No toolchain or dependency change is mixed into H21 validation.

## 2026-11-07 — H30: remove the dependency failing the security gate

- H21's remaining nine release benchmark smoke targets, palette check and six
  Python tests passed. The full battery then stopped at cargo-deny:
  RUSTSEC-2026-0441 reports `pin-utils` as unmaintained. A separate cargo-audit
  returned zero but emitted the same maintenance warning; that is not a clean
  security result. The dependency path was `iced_futures` → `wasmtimer` 0.4.3
  → `pin-utils` 0.1.0. This advisory is a maintenance finding, not a claimed
  exploit.
- Updated only the locked `wasmtimer` version to 0.4.5, within iced_futures'
  existing compatible requirement. This removes `pin-utils` and uses the
  already-locked `pin-project-lite`; no advisory exceptions or warning-policy
  changes were added. Upstream's [pinned changelog](https://github.com/whizsid/wasmtimer-rs/blob/101a11e164f3e4fad93758147e580bb72ecb79e1/CHANGELOG.md)
  records the replacement in 0.4.4 and an Arc leak fix in 0.4.5.
- Restarted the full local CI-equivalent battery on the combined H21/H30 tree,
  with security scans first and Rust warnings denied. The previous H21 logs
  remain intact; new commands and results use the `h30-` artifact prefix.
  Commit/push and exact-head remote CI verification remain pending.
- Both fresh security scans passed after the lockfile update. Cargo-audit
  reports no warnings; cargo-deny still reports the 74 duplicate-version
  families tracked separately. All seven focused stage tests, formatting and
  diff checks also passed. The complete workspace suite is running.
- The combined H21/H30 tree passed the full local battery: 6,077 workspace
  tests, zero failed, 16 ignored across 85 unit/integration/doc-test groups;
  all-target Clippy and all builds with warnings denied; default launcher,
  cache-report and CLI smoke checks; desktop-only/CLI-only builds; four WASM
  guest builds; nine release benchmark smoke targets; palette checks; six
  Python tests; formatting; and both security scans. Source, workflow and
  lockfile diffs match the validation snapshot. No compiler warning was emitted.
- Added a direct `wasmtimer` wasm32-wasip2 compile check because the four guest
  plugins do not depend on Iced's timer. This checks the updated package itself
  on a WASM target; it does not claim browser runtime testing.

## 2026-11-11 — H31: repair SQLite and SQLCipher together

- On resumption, the entire untracked `target/` directory was absent and the
  extra WASM check's process handle was unknown. Its completion cannot be
  established. The November 7 full-battery results were observed directly and
  remain recorded above, but their raw logs are no longer available. The
  tracked source changes still matched the inspected November 7 diff.
- Refreshed security scans before publication. Cargo-deny failed on
  [RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459.html),
  issued November 9: `libsqlite3-sys` 0.37.0 bundles an affected SQLite engine.
  Database files paired with crafted WAL files can disclose process memory.
  Nothing has been committed or pushed with this failure unresolved.
- Verified the published dependency contracts: SQLx 0.9.0 caps
  `libsqlite3-sys` below 0.38; 0.39.x patches plain SQLite but retains an
  affected SQLCipher amalgamation. Version 0.40.0 uses SQLCipher 4.20.0 with
  SQLite 3.54.1. Updated the workspace requirement and vendored only SQLx's
  0.9.0 SQLite driver to change that dependency requirement. Its Rust source
  is unchanged; provenance, licenses and retirement instructions are included.
- Added a runtime version regression using `SELECT sqlite_version()` so a
  passing generic advisory scan cannot mask an affected SQLCipher build.
  Native CI runs it on Linux/macOS/Windows. Existing encryption feature gates
  remain intact. Dependency resolution changed only the SQLite binding,
  driver source and SQLCipher source package beyond H30's timer update.
- This dependency change invalidates using November 7's full battery as
  verification of the final tree. Fresh security, storage and full-workspace
  verification are required before publication.
- Cargo-deny's advisory check now passes. Its license check initially rejected
  the new source package's SQLite `blessing` and LibTomCrypt `WTFPL` license
  identifiers. Reviewed the shipped component notices and added an exact-version,
  package-scoped license allowance for `sqlcipher-src =420.0.0`; no advisory
  ignore or global license allowance was added. The next cargo-deny run passed
  all four gates. The existing duplicate-version warnings remain visible.
- Native CI also runs the at-rest storage regressions on Linux/macOS, where
  SQLCipher is enabled. Windows retains its existing encryption availability
  behavior and runs the linked-engine version regression. The full session
  suite is rebuilding locally with warnings denied.
- The full session library suite passed: 207 tests, zero failed/ignored,
  including the runtime engine-version regression, encrypted creation,
  wrong-key rejection, plaintext migration, read-only inspection, recovery and
  encrypted audit archive tests. Rust warnings were denied. Cargo-audit also
  passed without warnings; cargo-deny passed with the 74 tracked duplicate
  warnings. The native workflow YAML parses, and every vendored driver source
  file matches the published 0.9.0 bytes.
- Started the full final-tree battery (`h31-validate.py`): direct WASM timer
  compilation, H21 regressions, workspace unit/integration/doc tests, all-target
  Clippy, all builds/frontend variants, launcher/CLI/cache smoke, WASM guests,
  nine release benchmark targets, palette/Python checks and fresh security
  scans. Its statuses are written per completed command. Publication and
  exact-head GitHub verification remain pending.
- The direct WASM timer check passed. After a tool-session handoff, the
  validation coordinator was gone but its orphaned Cargo child was still
  actively compiling the orchestrator target (confirmed through its process,
  compiler children and log descriptor). Kept that build running. A detached
  resume coordinator waits for that exact child to finish, preserves its log,
  and then re-runs the focused command to capture an authoritative exit status
  before continuing the remaining battery. The source checksum manifest still
  matched every file at handoff; no completed check is inferred from a live or
  orphaned process.
- The orphaned build finished, and the detached coordinator captured a fresh
  successful focused run: all seven stage-tracker regressions passed on the
  repaired dependency tree. Formatting and diff checks also passed. The
  coordinator has advanced to the full workspace suite; Clippy, complete build
  and smoke/security gates remain pending. Code search indexing was refreshed
  successfully.
- CI parity review found the Python dataset-export fixture used the default
  temporary directory. That command runs outside Cargo, so Cargo's TMPDIR
  setting does not govern it. Updated this one fixture to use the repository's
  disk-backed `target/` directory, like the cache smoke script. All six Python
  tests passed. Rust/manifests/lockfile remain unchanged during their run; the
  full pipeline will also run the updated Python suite before finishing.

## 2026-11-11 — H31 battery passed; H32 blocks publication

- The detached runner finished all compilation and runtime gates successfully:
  6,078 workspace tests passed, zero failed, 16 ignored across 85 groups;
  all-target Clippy with warnings denied; full workspace build; both frontend
  builds; launcher/cache/CLI smoke; four WASM guest builds; all nine release
  benchmark targets; palette; six Python tests; formatting and diff checks.
  Source checksums still matched apart from this work log.
- The final cargo-deny scan failed on newly issued
  [RUSTSEC-2026-0486](https://rustsec.org/advisories/RUSTSEC-2026-0486.html):
  `rustix` 1.1.4 can mark unwritten extended-attribute buffer memory as
  initialized. The unaffected 0.38 series is separate; the affected 1.x copy
  arrives through multiple transitive paths. No Concerto-specific exploit is
  claimed. The runner stopped at this failure, so its final audit command did
  not run. Earlier audit success does not certify this later advisory state.
- Updated the affected lockfile package to patched `rustix` 1.1.6. Cargo's
  update also reassigned several unrelated Windows dependency edges to older
  already-locked versions. Preserved the previously tested Windows selections:
  the new rustix accepts `windows-sys >=0.52, <0.62`, and locked offline Cargo
  metadata validates the resulting graph. Only rustix's version/checksum and
  references change beyond H30/H31. No advisory suppression was added.
- The full final-tree battery must run on this latest lockfile before
  publication. Earlier successful gates are retained as evidence of H21/H31,
  not substituted for validation of the new dependency tree.
- Fresh cargo-deny and cargo-audit both passed on the rustix 1.1.6 lockfile;
  the direct WASM timer check passed as well. The detached H32 coordinator is
  running the complete battery with Rust warnings denied and TMPDIR explicitly
  set to `/var/tmp`. H31 logs/statuses remain intact. The final PR description
  is prepared locally with validation deliberately marked pending.
- On the rustix-updated tree, all seven stage regressions passed, followed by
  formatting and diff checks. The full workspace suite is running.
- Staged the vendored driver and reviewed all 56 text files (about 387 KiB).
  The staged diff check found trailing spaces in three upstream doc-comment
  lines and extra EOF blank lines in the copied licenses. Removed those and
  updated the provenance note; staged diff checks now pass. A source comparison
  confirms executable Rust is unchanged and only those trailing doc-comment
  spaces differ from the published driver. The validation checksum manifest
  records this documentation-only normalization explicitly.
- Saved H21's three Rust files as local commit `b7f9e65` after the completed
  full-workspace/Clippy checks and the rustix-updated focused regression pass.
  This is not a claim that the bare intermediate commit has current green
  security CI: the dependency repairs, native workflow additions and this log
  remain pending in the integration worktree. No push was made. The final
  combined tree must finish its battery before the series is published.
- Saved the verified Python fixture change as local commit `e2948b7` and the
  SQLite/timer repair as `99c571d`. The latter uses the checksum-verified H31
  lockfile in its commit; the rustix 1.1.6 update remains in the working tree
  for its own commit after validation. Vendored text files use normal 0644
  permissions; no executable source-file flags or build artifacts are included.
  The current integration worktree retains every pending change, and no commits
  have been pushed. Native CI additions and the final verification log remain
  part of the unpublished series.
- Saved native CI coverage as local commit `e0b03e7` after checking the YAML,
  three-platform matrix and SQLCipher platform condition. Its Linux tests were
  exercised by the passing focused/session/workspace suites; Windows/macOS
  execution is still required on the final pushed head. The H32 workspace
  command remains live and is linking integration binaries; process and cgroup
  checks show no OOM event. No restart, warning suppression or push was made.


## 2026-11-12 — H21/H30/H31/H32 complete local verification

- The detached H32 coordinator completed every command with exit zero. The
  final dependency tree passed 6,078 workspace unit/integration/doc tests,
  zero failed and 16 ignored across 85 groups; all-target Clippy with warnings
  denied; full workspace and both single-frontend builds; launcher/cache/CLI
  smoke; all four WASM guest builds; nine release benchmark smoke targets;
  formatting, diff hygiene, palette, six Python tests and both security scans.
  All seven stage-tracker regressions and the direct WASM timer check passed.
- Compared all 994 recorded source hashes against the current tree: only this
  work log differs. Compiler logs contain no warning lines. Cargo-deny still
  reports the tracked duplicate-version families; this is not a claim that
  H28 is resolved or that the entire repository is warning-free.
- Prepared H28 in the disk-backed `Concerto-worktrees/h28` worktree. Removing
  the unused `utoipa-axum` manifest entries removes four packages (the bridge,
  Axum 0.7, axum-core 0.4 and matchit 0.7). Offline metadata resolution preserves
  every retained package's enabled features. No builds were started there
  while the main battery ran. This worktree starts before H32's rustix update;
  its lockfile must not replace the final integration lockfile. API tests and
  final integration verification remain pending. The main worktree under
  validation was not changed by this preparation.
- Publication and Linux/macOS/Windows checks on the final pushed head remain
  required before taking the next change into the integration branch.
- Refreshed both security scans immediately before publication on November
  12: cargo-deny and cargo-audit passed, with no new advisory suppression.


## 2026-11-12 — H28 isolated unused Axum dependency cleanup

- Published the locally verified H21/H30/H31/H32 series as `f8d91bb` in PR
  #379. Main CI run 43127596886 and native CI run 43127596759 both identify
  that exact head. At this checkpoint, formatting, palette, WASM guests and
  cargo-deny passed; the other eight checks remain in progress. No later
  integration milestone has been pushed.
- Advanced the isolated H28 worktree to `f8d91bb` before reapplying the two
  manifest removals and Cargo's minimal lockfile cleanup. The rustix 1.1.6
  security repair is preserved. Locked offline metadata succeeds and confirms
  exactly four removed packages, no added packages and no changed feature sets
  for retained packages. No source import or call uses `utoipa-axum`.
- Started the complete API-server test suite with Rust warnings denied,
  followed by cargo-deny and cargo-audit. Existing tests cover docs-disabled
  routing, OpenAPI, embedded Swagger assets and authentication boundaries.
  Compilation is still running; this entry does not claim passing API tests
  or resolution of the remaining duplicate-version warnings.
- The Axum-only cleanup passed all 63 API-server tests, its binary/doc-test
  targets, cargo-deny and cargo-audit. Cargo-deny duplicate warnings fell from
  74 to 71. Saved that intermediate validation separately before extending
  the same dependency cleanup.
- Reviewed API source, startup and route tests: it does not use its direct
  orchestrator, tower-http, serde or ulid declarations. Removed those four
  declarations, plus the now-unused workspace tower-http declaration. The
  revised lockfile removes five packages in total, adds none, and preserves
  the enabled features of every retained package in workspace metadata.
  Existing authentication and rate-limit middleware remain implemented in
  API source; no route implementation is changed.
- The all-platform normal/build dependency closure reachable from API-server
  shrinks from 707 packages to 444 in resolved metadata. This is a dependency
  count, not a measured compile-time or runtime performance claim. Started
  fresh API tests and a binary build, security scans, and an exact parsed-JSON
  comparison of generated OpenAPI output with the published build. Results
  for this expanded cleanup are still pending.
- The expanded API cleanup passed all 63 tests, its binary build, both
  security scans and parsed-JSON equality of generated OpenAPI output with
  the published binary. Cargo-deny duplicate warnings fell to 70. Saved this
  intermediate validation before the directory dependency consolidation.
- The desktop separately selected dirs 5 while the workspace selected dirs 6.
  Context7 did not index this library; inspected the downloaded published
  sources and pinned upstream sources instead. Linux/macOS lookup code is
  unchanged; the Windows helper changes the null-handle expression for newer
  Windows bindings. Upstream source:
  https://github.com/dirs-dev/dirs-sys-rs/blob/8bcd4aa2c35990d57a2cff2953793525fc42709c/src/lib.rs.
  Switched desktop to the workspace dependency. The remaining dirs 5 edge was
  an unused orchestrator dependency (text occurrences are local variable names
  and comments); removed that declaration as well.
- Froze these manifest/lockfile changes for the complete local battery in the
  isolated worktree. The published integration head remains f8d91bb while its
  required GitHub checks run. No warning policy or advisory exception changes
  are included. Full validation of this combined cleanup is still pending.
- The combined cleanup's initial cargo-deny scan passed with 67 duplicate
  warning families (74 before cleanup), with no other warning categories.
  Initial cargo-audit, direct WASM timer compilation, all seven stage-tracker
  regressions, formatting and diff checks passed. Full workspace validation
  is now running; no source changes have been made since its snapshot.

## 2026-11-12 — H16 execution-path review during verification

- Traced the coverage entry points in `crates/eval/src/lib.rs`. Repository
  searches found no production callers of these public EvalEngine coverage
  methods outside the module; the similarly named desktop method computes
  UI run coverage and is unrelated. Existing coverage tests exercise parsers
  with invented Tarpaulin fields, not the real subprocess/report contract.
- In addition to the recorded process-status, report-file, UTF-8 and
  cancellation defects, discovery probes ambient `cargo` while
  `run_with_coverage` can execute through a configured shell profile.
  Standalone `run_coverage` ignores that profile entirely. Discovery and
  execution need one consistent environment. `run_command` provides bounded
  async execution but does not apply shell profiles, so simply routing all
  coverage through it unchanged would lose that contract.
- Planned execution regressions: nonzero version probes must not select a
  tool; profile-scoped discovery must match execution; report JSON must come
  from a unique run-specific file and remain separate from stdout/stderr;
  zero/ignored-only tests, malformed summaries and a failed test run must not
  pass; Unicode diagnostic truncation must not panic; cancellation before
  discovery and during execution must propagate through all three entry
  points; deadline and report failures must stay explicit. Preserve the
  existing governed-executor fail-closed boundary (no direct coverage spawn).
  No H16 implementation or test result is claimed yet.


## 2026-11-12 — Published head green; H33 fixture isolation repair

- All twelve required GitHub checks passed on exact head
  `f8d91bb4fd45d94e7c3a10aebb1ab1b5e2c57c99`:
  [main CI](https://github.com/NefaroXX/Concerto/actions/runs/43127596886)
  and [native CI](https://github.com/NefaroXX/Concerto/actions/runs/43127596759).
  Updated PR #379 with that evidence. Linux/macOS ran 74 gate, one cleanup,
  one SQLite runtime-version, 25 encrypted-storage, eight rollback and seven
  stage-tracker tests; Windows ran 73, one, one, six and seven respectively
  (encrypted storage remains intentionally unavailable on Windows).
- The H28 local workspace run stopped at `cargo_verification` with one failure.
  Its passing/empty-test fixture executed the separate fixture's deliberately
  failing root/module tests. Both temporary projects used package name
  `h04_fixture`; the isolated-worktree runner's inherited `CARGO_TARGET_DIR`
  made them share the same test artifact path. This is a fixture defect in
  the regression tests added during this work, not an excused baseline issue.
  The original failed log/status remain preserved under `h28-final-*`.
- Assigned each temporary fixture a unique Cargo package identity, preserving
  the real direct/fallback execution paths and parallel testing. Started the
  complete three-test target with the same inherited target directory and
  warnings denied. Its result is pending; the full workspace and remaining
  CI-equivalent gates must complete after this repair before publication.
- H16 preparation is isolated in `Concerto-worktrees/h16`: four parser
  regressions cover the released Tarpaulin report fields, measured zero versus
  absent data, impossible measurements and out-of-range LLVM percentages.
  They have not run; no coverage implementation change is claimed.
- H33 focused verification passed all three Cargo integration tests with
  the same inherited shared target directory and Rust warnings denied. The
  combined tree is proceeding through the full workspace and remaining gates.
- The repaired tree passed the full workspace suite: 6,078 tests,
  0 failed and 16 ignored across 85 unit/integration/doc groups.
  Source hashes match the validation snapshot. Clippy and the remaining
  build/smoke/benchmark/security gates are running; publication remains pending.

- All-target Clippy with warnings denied and the full workspace build passed
  on the repaired H28/H33 tree. Frontend/launcher/WASM/benchmark smoke checks
  and final scans are still running.

## 2026-11-12 — H34: incomplete subprocess output lacks a verdict

- While evaluating process-runner reuse for H16, traced
  `ProcessHandle::read_bounded` in `crates/tools/src/process.rs`: it captures
  the first 10 MiB per stream, drains and discards the rest, and returns only
  a String. Pipe read errors also break the loop without preserving the error.
  `ProcessOutput` carries no completeness flag. The direct coverage code
  currently uses a different, unbounded capture path, so blindly substituting
  this helper would weaken coverage verification.
- `test_output::completed_result` in `crates/eval/src/test_output.rs` accepts
  Cargo exit zero when captured output contains a passing summary and no
  failing/malformed summary. A captured passing prefix with a discarded later
  failure therefore needs explicit incomplete-output handling at the process
  boundary. A nonzero child exit still fails regardless of captured text;
  this concern is successful exits with inconsistent or incomplete evidence.
  This is a source-traced failure path; a process-level regression
  must reproduce it before the repair is claimed. Do not infer output
  completeness from a successful child exit or hide the issue with a larger
  capture limit. Record it separately from H16's Unicode display truncation.
- Both frontend variants, launcher/cache/CLI smoke and all four WASM
  guests passed. The remaining release benchmark targets are compiling.
- Added the three-test Cargo verification integration target to every native
  CI platform with an explicitly shared CARGO_TARGET_DIR. The YAML, platform
  matrix and warning policy validate; the exact command already passed
  locally under that environment. This workflow-only change does not alter
  the Rust/manifests/lockfile under validation and still requires execution
  on the final published head.


## 2026-11-12 — H28/H33 complete local verification

- The repaired combined tree passed the full local battery: 6,078 workspace
  tests, zero failed, 16 ignored across 85 groups; warnings-denied all-target
  Clippy; full workspace and both frontend builds; launcher/cache/CLI smoke;
  four WASM guests; all nine release benchmark smoke targets; formatting,
  palette, six Python tests and fresh cargo-deny/cargo-audit scans.
- Cargo-deny now reports 67 duplicate-version families, down from 74, with
  no other warning category. Cargo-audit reports no warnings. No advisory or
  warning suppression was added. H28 remains open for the remaining dependency
  review; this batch removes 17 packages and adds none.
- Every Rust source, manifest and lockfile hash matches the repaired validation
  snapshot. Subsequent differences are only the changelog, this log and the
  reviewed native-workflow step whose exact command already passed locally.
  The earlier failed workspace run remains recorded; the successful full run
  verifies the fixture repair under the same inherited shared target setting.
- The fixture repair is local commit ad5aea4. The native regression workflow
  and dependency cleanup are being saved separately before publication; the
  final head still needs all required GitHub checks, including the new native
  shared-target integration step. No H16 implementation has been mixed in.


## 2026-11-12 — H16 parser regression draft

- Added four isolated parser regressions for real Tarpaulin percentages,
  measured zero versus missing fields, invalid aggregate measurements, and
  invalid LLVM line/function/branch percentages. Report shape verified against
  https://github.com/xd009642/tarpaulin/blob/0.37.5/src/report/json.rs.
- Formatting has been applied. No baseline execution or implementation fix
  yet: Cargo compilation remains serialized behind the H28/H33 verification.
  This worktree starts at f8d91bb and must incorporate the verified dependency
  cleanup before integration.
- Added three Unix process-level regression tests using a per-fixture
  shell profile and fake Cargo executable. They cover profile-local tool
  discovery/report files, empty and ignored-only suites, and Unicode output.
  Fixtures use disk-backed directories with spaces, avoid global environment
  mutation, and refuse report paths outside their own target directory even
  when exercising the unfixed implementation. Formatting and diff checks
  pass; execution remains queued behind the active H28/H33 battery.

- Advanced this isolated worktree to published 43a3e79, retaining the drafted
  tests and the verified H28/H33 cleanup. Starting baseline execution of the
  seven coverage regressions; no coverage implementation change yet.


## 2026-11-12 — Published cleanup green; H16 failures reproduced

- Both GitHub workflows completed successfully on exact head
  `43a3e7960de997a371cb4aa04ee79b422d734ff0`: main run 43230517229
  and native run 43230517228. All twelve required checks are green, including
  shared-target Cargo fixture verification on Linux, macOS and Windows.
  Updated PR #379 to identify this verified head explicitly.
- The seven H16 regressions compiled without warnings and all seven failed
  against that head before implementation changes. Real Tarpaulin fields
  incorrectly produced zero; missing reports and impossible percentages were
  accepted. Unicode output panicked at a non-character byte boundary.
  Report-file guards rejected the legacy /dev/stdout path with exit 65.
  The empty-suite case stopped at that guard too, so its false-success
  assertion still needs independent execution after report handling is fixed.
- Evidence: target/harness-reliability/h16-baseline.log and status JSON,
  with the baseline source copies and hashes retained alongside them.

## 2026-11-12 — H16 initial implementation and targeted evidence

- Moved coverage execution/parsing into its own module. Runs now discover the
  coverage executable through the same shell profile used for execution, require
  a successful version probe, and share one asynchronous, cancellable deadline.
  Scoped and standalone entrypoints use that path instead of launching a second
  test suite or swallowing coverage/cancellation errors.
- Reports live in unique temporary directories under the project's target/,
  with generated relative paths safe for shell profiles even when the project
  path contains spaces. Reports are read separately from diagnostics; missing,
  malformed, non-regular or oversized reports fail explicitly. Cleanup runs on
  success and early return. Tarpaulin uses its released coverage/count fields;
  LLVM metrics require valid counts/percentages and a single dataset.
- Test success uses the existing complete libtest-summary interpreter and
  character-safe display tail. Empty and ignored-only suites now execute their
  negative assertions successfully. Nonzero commands retain their failure and
  do not attach partial coverage.
- Added a checked entrypoint to the shared process runner so coverage cannot
  accept silently truncated/read-failed output. It retains the existing process
  group cancellation/timeout mechanism and bounded pipe draining. Other legacy
  callers still use their existing capture contract: H34 is not resolved by
  this coverage-specific use of the checked path.
- First repair run: all seven original regressions pass. Expanded eval package
  run: 103 unit tests and three integration tests pass, including discovery/run
  cancellation and timeout, stale/malformed reports, one instrumented run for
  standalone/scoped entrypoints and failure propagation. Process-runner tests
  and warnings-denied targeted Clippy are still running; full CI verification
  and native coverage execution remain pending. Nothing from H16 is published.

- The checked-process suite completed: 23 passed, including both stdout and
  stderr overflow with successful child exits, exact capture boundaries and
  injected pipe-read failure. Targeted Clippy was interrupted without an exit
  record: its coordinator and every compiler child were confirmed absent
  after the execution environment changed. It remains unverified and will
  resume; no compile failure is inferred from the missing process. The same
  interruption stopped llvm-tools installation; both released coverage tool
  archives downloaded successfully with verified GitHub SHA-256 digests.
- Prepared an explicit ignored live-tool contract test for both released
  coverage tools. This requires external binaries and LLVM tools; it is not
  silently skipped as a default passing test. The default regression suite
  remains self-contained. Added its portable parser and Unix process
  regressions to the native matrix; Windows still runs the portable cases.

- The Clippy metadata failure was traced through concerto-config -> figment
  -> pear. Rebuilding only concerto-config and then figment exposed the next
  dependency in that chain; rebuilding pear, pear_codegen and
  proc-macro2-diagnostics restored resolution. No source/dependency-version
  change was made to those packages. The final targeted all-target Clippy
  completed with warnings denied (h16-clippy-pear.log, exit zero). Retained
  all failed logs and the initially rejected config metadata/hash for diagnosis;
  the initial cause of the cache inconsistency is not established.
- Added report-size/type and concurrent-report-path regressions, and extended
  the policy test to standalone/scoped coverage. LLVM requests summary-only
  JSON because only aggregate measurements are consumed. Coverage follows the
  selected tool's supported test types; llvm-cov's stable-Rust run does not
  instrument doctests. Broader package testing is running on these additions.

## 2026-11-12 — H16 released-tool check and full validation start

- The broader package run passed 465 tests: 106 eval units, three Cargo
  verification integration tests and 356 tools units, with zero failures.
  The external-tool contract test is explicitly ignored by default and was
  then run separately; it is not counted as passing in that default run.
- Real cargo-llvm-cov 0.9.1 and cargo-tarpaulin 0.37.5 both executed a real
  fixture test and produced parsed reports (87.5% and 75% line coverage,
  respectively; different instrumentation/measurement semantics, not a
  comparison benchmark). Both runs used project paths containing spaces and
  disk-backed isolated fixture build directories. Tarpaulin used its LLVM
  engine. The LLVM-tools component installation completed successfully.
- The first live test exposed a defect in the new fixture launcher: using
  env!("CARGO") called the bare toolchain Cargo, which cannot interpret
  Tarpaulin's internal +toolchain directive. The fixture now preserves the
  PATH Cargo launcher. The failed live log is retained; after that fixture
  repair, the real-tool test passed for both tools. No production workaround
  was needed. Evidence: h16-live.log and h16-live-launcher.log.
- Starting the full workspace/CI battery from a source hash snapshot. This
  includes warnings-denied Clippy/builds, tests and doctests, frontend/WASM/
  benchmark smoke checks, palette/Python checks and both supply-chain gates.
  Native CI must still run on the final published head. H16 is not yet marked
  fully verified or published; the existing published head remains 43a3e79
  with all twelve required checks green.


## 2026-11-14 — H16 full runtime battery; refreshed security findings

- The H16 coordinator finished. Workspace tests passed: 6,100 passed, zero
  failed, 17 ignored across 86 groups. All-target Clippy with warnings denied,
  full/default and both frontend builds, launcher/cache/CLI smoke, four WASM
  guests, nine release benchmark smokes, formatting, palette and Python checks
  passed. All 998 files match the recorded source snapshot exactly.
- The original audit process returned zero but logged registry timeout errors.
  That is not a complete yanked-package check; its exit code is not treated as
  sufficient verification. The subsequent fresh audit additionally found
  RUSTSEC-2026-0538 in quinn-udp 0.5.15 (raw lockfile gate). Cargo-deny still
  passed. Nothing is published until both gates are fully verified.
- H35: updated quinn-udp to patched 0.5.16. The fetched RustSec advisory
  describes a receive-loop denial of service in the recvmsg paths on affected
  non-Linux/non-Windows targets. Kept all prior dependency selections: Cargo's
  update initially reselected several broad windows-sys requirements, so those
  unrelated changes were removed after checking each published requirement.
  cargo metadata --locked --offline validates the resulting lockfile. Only
  the quinn-udp version/checksum and quinn's resolved edge differ; no new
  package or advisory suppression was added. Metadata node membership alone
  is not proof of an enabled build path.
- H36: cargo-audit 0.22.2 source confirms registry lookup errors are printed
  without entering the returned warnings or failure status. Added
  scripts/check-cargo-audit.py after the existing RustSec CI action: it retains
  the same advisory exceptions, denies warnings, and independently rejects
  error/warning diagnostics even when the process exits zero. Seven real
  child-process regression tests pass; all 13 Python tests pass. The strict
  gate is now being exercised against the real registry.
- Evidence: h16-full-* logs, h16-source-sha256.json, h16-audit-refresh.log,
  h35-resolved.lock (including discarded unrelated resolver changes),
  h35-metadata.json and h36-strict-audit.log. The previous passing runtime
  snapshot does not certify these subsequent lockfile/CI changes; final
  combined validation will run after the security checks complete.

- The patched cargo-deny scan passed. `cargo tree --locked --target all -i
  quinn-udp` reports no enabled reverse dependency, confirming the raw-lockfile
  gate is needed for this optional dependency. The first real strict audit
  rejected a lock-contention warning caused by running it alongside cargo-deny
  against the shared advisory database; it found no remaining vulnerability
  or registry timeout. Both processes completed. Rerunning the strict audit
  alone now, and future local security gates will be serialized. The warning
  was not suppressed or reclassified as success.

- The serialized strict audit completed with exit zero and no error/warning
  diagnostics: freshly fetched database, 1,509 advisories, 1,079 lockfile
  dependencies. Both the advisory scan and registry checks completed. The
  patched cargo-deny scan also passed. Starting final combined validation with
  the minimal quinn patch, strict audit wrapper, 13 Python tests and current
  coverage sources; artifacts use the h16-final-* prefix to preserve the
  earlier runtime-only results and incomplete audit evidence.


## 2026-11-14 — H16/H35/H36 final combined local verification

- Final combined validation completed successfully: 6,100 workspace tests,
  zero failures, 17 explicitly ignored tests across 86 groups; the released
  llvm-cov/Tarpaulin contract test also passed separately. All-target Clippy
  with warnings denied, workspace/frontend builds, launcher/cache/CLI smoke,
  four WASM guest builds, nine release benchmark smoke targets, formatting,
  palette checks and all 13 Python tests passed. Both workflow YAML files parse.
- Both fresh security gates passed. Cargo-deny retains 67 duplicate-version
  warning families and no other warning category; H28 remains open. The strict
  raw-lockfile audit fetched 1,509 advisories and checked 1,079 dependencies
  without errors or warnings, including successful registry checks. No ignore
  or suppression was added.
- All 1,000 source-snapshot files matched after validation. Only this final
  evidence entry is added afterward. The isolated H09 worktree contains two
  drafted durable-compaction regressions, not an implementation, and has not
  run Cargo while this battery was active.
- Saving the quinn dependency repair, coverage repair and stricter audit gate
  in separate commits, followed by their documentation/work-log update. The
  final published head still requires every remote check; local results are
  not presented as native macOS/Windows execution of that new head.

- Saved dependency patch 7d2ab8f, coverage repair 62d1692 and strict audit gate
  e4af9d6 as separate commits. This documentation commit records their shared
  verification evidence; publication and exact-head remote checks follow.
