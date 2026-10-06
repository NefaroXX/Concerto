# Codebase Improvement Plan: World-Class Engineering

> **Status: active engineering plan, retained at the project owner's request.**
> **Reconciled: 2026-10-05 (Africa/Johannesburg).** Source baseline:
> [`dev` at `c340a5107b874caf3bb517dd06c51f6692736ad4`](https://github.com/NefaroXX/Concerto/commit/c340a5107b874caf3bb517dd06c51f6692736ad4).
> The revised plan merged in [#192](https://github.com/NefaroXX/Concerto/pull/192)
> and was removed by [#193](https://github.com/NefaroXX/Concerto/pull/193).
> That removal was not intended; this revision restores the same path with
> the current implementation status and next-task contract.
> [DEFERRED.md](../DEFERRED.md) remains the single deferred-work register.
> This plan specifies engineering order, verification and collaboration;
> it does not replace that register or reinstate the old phase estimates.

## Assessment and evidence limits

Concerto has a substantial policy, event, persistence, provider, and testing
foundation. Several original refactoring targets are already implemented.
The remaining engineering burden is concentrated in orchestration module
size, enforceable test-quality gates, cancellation contracts, portable frontend
interaction parity, and verification of security boundaries.

The old "already world-class", zero-vulnerability, zero-warning, complexity,
LOC, and test-count assertions were not current measurements. They are
retired, along with the blanket 1,500-test target and hour/week estimates.
Passing configured checks is evidence for those checks, not a security or
coverage certification.

The 2026-10-05 review compared source and merged work against the prior plan
and Miro queue, then checked the five additional commits through the baseline
above. Those five changed agent guidance and ignore rules, not application code.
Source files, current CI, the parity/security guides and ADR index were checked;
hotspot counts below were re-measured from current blobs.

Baseline [CI](https://github.com/NefaroXX/Concerto/actions/runs/37311877951),
[native Linux/Windows/macOS tests](https://github.com/NefaroXX/Concerto/actions/runs/37311878217)
and [Windows build](https://github.com/NefaroXX/Concerto/actions/runs/37311878100)
completed successfully. These are upstream results for the source baseline.
This documentation restoration does not claim locally executed Rust builds,
mutation testing, fuzzing, profiling, desktop interactions or keychain checks.
Interactive release evidence remains separate from automated CI.

## What changed since the original plan

| Original item | Current status | Evidence / remaining work |
|---|---|---|
| Phase 0 groundwork | Landed, with documentation-lint drift | Shell-parser and tree-sitter property suites exist; runtime/coordinator phases have named helpers. Reviewed library roots now use `#![allow(missing_docs)]`, rather than the proposed warning. See [Cargo.toml](../../Cargo.toml), [parser](../../crates/shell/src/parser.rs), [tree-sitter](../../crates/memory/src/treesitter.rs). |
| Markdown `render()` state machine | Implemented | [markdown.rs](../../crates/desktop/src/widgets/markdown.rs) has `MarkdownRenderer`, tag/event handlers, cached `MarkdownDoc`, and truncated-render tests. Preserve these; do not schedule the original rewrite again. Visual fidelity still needs UI evidence when rendering changes. |
| Settings / former Studio editor modularization | Implemented or superseded; size reduction incomplete | [settings/](../../crates/desktop/src/views/settings/) and [code_editor/](../../crates/desktop/src/views/code_editor/) are the current modules; the old `studio_editor.rs` and `views/studio/` paths are absent. Large settings state/view modules and `orchestration_studio.rs` remain. |
| Rust entity extraction helpers | Implemented | [entities.rs](../../crates/memory/src/entities.rs) delegates to `try_extract_fn/struct/enum/trait/impl`. Other parsers should be assessed independently, without reusing the old complexity scores. |
| OpenAI `build_chat_body()` refactor | Original target superseded | [openai.rs](../../crates/providers/src/openai.rs) uses `Dialect` / `OpenAiChatDialect` and `render_body`; body ownership is in [adapters/](../../crates/providers/src/adapters/). Review current stream/dialect contracts rather than refactoring a retired function. |
| Plugin host-function registration | Handlers extracted | [host_fns.rs](../../crates/plugins/src/host_fns.rs) has separate async `host_*` functions and explicit ABI registration. A heterogeneous function-pointer table is not a required improvement. Security of the handlers is a separate concern below. |
| `run_shared_agent`, coordinator, agent-loop phases | Helpers landed; structural debt remains | Named setup, execution, retry, restore, and reconciliation helpers exist. Their containing modules have grown substantially; this is partial completion, not a renewed claim that all complexity disappeared. |
| Duplicate `MemoryError` | Closed | [memory/src/lib.rs](../../crates/memory/src/lib.rs) re-exports core's error. Remaining same-name public errors are `SessionError` and `EvalError`, deliberately deferred as API changes in [TODO audit cleanups](../TODO.md#audit-cleanups-deferredmd-row-34). |
| LSP test infrastructure | Present | [LSP integration tests](../../crates/lsp/tests/) cover client, manager, and tools; source modules also contain tests. The old "7 tests / most urgent empty crate" assessment is obsolete. Pending-I/O cancellation still needs contract coverage. |
| Criterion benchmarks | Implemented | Seven targets: policy, serialization, FTS search, vector retrieval, task graph, provider streaming, and VirtualFs. See [benchmark inventory](#performance-and-fuzzing). |
| Fuzz targets | Implemented | [fuzz/Cargo.toml](../../fuzz/Cargo.toml) registers `shell_parser` and `guest_abi`. Normal CI does not run them. Target presence is not fuzz-run evidence. |
| Threat model and event secret sanitization | Implemented | [Threat model](../security-threat-model.md), [SecretSanitizer](../../crates/core/src/sanitizer.rs), and its [EventBus wiring](../../crates/core/src/event.rs) exist. Accepted residuals remain. |
| WASM sandbox / extension authorization | Security implementation merged; release evidence still scoped | [host.rs](../../crates/plugins/src/host.rs) now installs allocation-time `StoreLimits`, with fuel/epoch and bounded-output controls. [#172](https://github.com/NefaroXX/Concerto/pull/172) merged; [#191](https://github.com/NefaroXX/Concerto/pull/191) restored reviewed W7/platform fixes dropped by the stale-head merge. Host effects use the shared executor/VirtualFs; MCP deadlines include transport waits. Preserve regressions and collect native UI/keychain evidence separately. |
| Test-quality / mutation gate | Not enforced | [mutation-gate.sh](../../scripts/mutation-gate.sh) echoes "Would run" and executes no `cargo mutants` command. Neither it nor the panic sweep is invoked by current CI. This is the recommended next task. |
| Desktop ↔ CLI parity | Settings and management shipped; interaction parity remains | [#189](https://github.com/NefaroXX/Concerto/pull/189) implements validated config, agent/provider/extension/credential/profile/preference commands. [Parity matrix](../desktop-cli-parity.md) identifies remaining staged review, live Coordinator control and Studio projections. Audit that matrix rather than creating another settings inventory. |
| Native shell and human-owned security | Merged in [#190](https://github.com/NefaroXX/Concerto/pull/190) | Native program/argv requests, user-global revision-checked security, central preparation/policy/approval and fail-closed container requirements are implemented. [Security guide](../native-shell-security.md) states protected-path, resource and platform limits. Profiles are compatibility settings; no implicit interpreter is required. |
| Editor workspace | Merged in [#169](https://github.com/NefaroXX/Concerto/pull/169), with [#185](https://github.com/NefaroXX/Concerto/pull/185) exit guard | [workspace.rs](../../crates/desktop/src/views/code_editor/workspace.rs) and [workspace_view.rs](../../crates/desktop/src/views/code_editor/workspace_view.rs) supply tabs and selected-file review. Do not duplicate the redesign; retain dirty-tab save-failure behavior and collect live UI evidence. |
| Evidence / resume / obligation hardening | Merged, preserve in refactors | [#177](https://github.com/NefaroXX/Concerto/pull/177) persists execution_mode; [#180](https://github.com/NefaroXX/Concerto/pull/180) distinguishes tool outcomes; [#181](https://github.com/NefaroXX/Concerto/pull/181) limits dismissal by kind/evidence/freshness; [#182](https://github.com/NefaroXX/Concerto/pull/182) fixes pinned precedence; [#183](https://github.com/NefaroXX/Concerto/pull/183) and [#184](https://github.com/NefaroXX/Concerto/pull/184) harden obligation guards and evidence. |

## Measured structural hotspots

These are physical line counts, including comments, blank lines and tests,
from the pinned UTF-8 blobs (`len(content.splitlines())`). The last column
locates the main inline test module; it is not a production-LOC measurement,
because some files have other inline tests or helpers. No fresh cognitive or
cyclomatic analysis was run.

| File | Total lines | Main test module starts |
|---|---:|---:|
| [orchestrator/coordinator.rs](../../crates/orchestrator/src/coordinator.rs) | 41,667 | 17,928 |
| [orchestrator/runtime_runner.rs](../../crates/orchestrator/src/runtime_runner.rs) | 9,534 | 5,471 |
| [desktop/app.rs](../../crates/desktop/src/app.rs) | 9,244 | 5,158 |
| [orchestrator/agent_loop.rs](../../crates/orchestrator/src/agent_loop.rs) | 8,253 | 3,323 |
| [desktop/views/orchestration_studio.rs](../../crates/desktop/src/views/orchestration_studio.rs) | 7,208 | 4,631 |
| [cli/app.rs](../../crates/cli/src/app.rs) | 3,998 | 2,627 |
| [desktop/views/settings/state.rs](../../crates/desktop/src/views/settings/state.rs) | 3,666 | 2,707 |
| [cli/lib.rs](../../crates/cli/src/lib.rs) | 2,944 | 1,687 |
| [desktop/views/settings/mod.rs](../../crates/desktop/src/views/settings/mod.rs) | 2,735 | 2,553 |
| [plugins/capability.rs](../../crates/plugins/src/capability.rs) | 2,120 | 1,321 |

Moving tests helps navigation but does not, alone, resolve oversized production
modules. Extract by ownership and behavior, preserve public paths and private
invariants, and avoid a single replacement file containing all the complexity.
Line thresholds are review prompts, not universal pass/fail targets.

## Current gates and their limits

[ci.yml](../../.github/workflows/ci.yml) runs formatting, workspace/all-targets
Clippy with warnings denied, workspace unit/integration/doc tests via
`cargo test`, builds, default and single-frontend CLI feature combinations and startup smoke commands,
benchmark smoke runs, four WASM guest builds, cargo-deny, and palette checks.
It pins Rust 1.96.0; the workspace declares MSRV 1.88. The old claim that CI
uses nextest and independently runs cargo-audit is incorrect. An MSRV build,
mutation enforcement, fuzz execution and measured coverage are not present
in that workflow. [native-shell.yml](../../.github/workflows/native-shell.yml)
adds core/config/tools/shell test coverage on Linux, Windows and macOS;
a successful platform run still does not prove interactive consent or GUI flows.

`unsafe_code = "deny"` remains a workspace rule. Dependency hygiene is
exception-managed: [deny.toml](../../deny.toml) contains documented advisory
exceptions, including Wasmtime 24.0.13, with several review dates of
2026-10-01. Re-review reachability/version assumptions before a dependency
change; do not describe a passing exception-aware check as "zero
vulnerabilities", or remove exceptions without understanding them.

The [contribution test-quality rules](../../CONTRIBUTING.md)
require behavioral justification, adversarial cases, regression-before-fix,
and independent review for hardening/quality work. Use the current contribution rules and [TESTING.md](../../TESTING.md),
rather than the removed research tier or an unresolvable quality-gates guide.
[AGENTS.md](../../AGENTS.md) is restored in this baseline; executable workflows
remain authoritative where its CI descriptions lag the implementation.
[panic-path-sweep.sh](../../scripts/panic-path-sweep.sh) is a manual report
with a historical checklist, not an automated proof of panic freedom.

## Revised work order

| Priority | Work | Completion condition |
|---|---|---|
| P1 — next engineering task | Make the mutation gate executable and enforce a bounded pilot in CI (Q01–Q08) | Actual mutations run; missed mutants fail; setup/baseline/timeout outcomes are distinct; retained reports and independent adversarial review prove the signal. |
| P1 — release evidence | Verify shipped extension/editor/native-shell behavior in real clients (V01) | Exact source/OS/client and observed UI/keychain/process results recorded; do not reopen merged implementation solely because a live check is outstanding. |
| P2 — structural debt | Map coordinator seams (R01), then extract/review one test slice (R02/R03) after Q08 | Tests preserve outcome, dismissal, obligation, resume, approval, cancellation and accounting contracts; mechanical movement remains separate from behavior changes. |
| P2 — contract defects | Audit blocked-I/O cancellation and compaction contracts (A01) | Already-cancelled and mid-flight waits have bounded exit/cleanup and durable-state evidence. Follow [DEFERRED row 34](../DEFERRED.md#outstanding) and [TODO audit cleanups](../TODO.md#audit-cleanups-deferredmd-row-34). |
| P2 — interaction parity | Audit remaining portable interactions (P01), then agree one implementation slice (P02) | Existing settings/management remain closed. First candidate: staged unified-diff review through the shared overlay service, including accept/reject and narrow-terminal evidence. |
| P2 — documentation | Reconcile targeted STATUS/TODO/DEFERRED statements (D01) | Correct native CPU-setting, module/cancellation and delivered parity/console claims without closing Windows/aggregate-resource or live-testing residuals. |
| P3 — frontend decomposition | Extract app, CLI dispatch, Studio and settings responsibilities | State/stale-reply contracts, palette checks and representative UI verification protect each changed view. Avoid bundling another redesign. |
| P3 — measured maintenance | Profile before optimizing; scoped docs/MSRV/error compatibility work | A measured bottleneck or contract justifies the change; public error renames require a compatibility decision. |

Extension security, the editor workspace, ADR allocation, settings parity and
native shell security have merged. New work starts from fresh dev; existing
source and current PRs are checked before assigning overlapping changes.

## Next task: replace the mutation dry run with an executable gate

**Purpose:** establish a trustworthy signal before large orchestration extractions.
[mutation-gate.sh](../../scripts/mutation-gate.sh) still prints "Would run" and
executes no cargo-mutants command. The task remains unstarted.

**Implementation branches:** fresh dev with the Miro task ID, for example
`fix/Q02-mutation-runner`, `test/Q03-mutation-fixtures` and
`ci/Q06-mutation-pilot`. One isolated branch/worktree per writer.

**Proposed pilot:** [core/policy.rs](../../crates/core/src/policy.rs),
[core/shell_security.rs](../../crates/core/src/shell_security.rs) and
[shell/parser.rs](../../crates/shell/src/parser.rs). Q01 must settle supported
targets, tool version, base handling, fixture/report interface, exit classes
and CI budget before either implementation starts. Config, plugins, MCP and
tools/native_process coverage is outside this proposal and must be reported
truthfully until an acknowledged expansion is measured.

| Task | Intended writer | Writable scope | Gate |
|---|---|---|---|
| Q01 | Codex; OpenCode acknowledges | Pilot/interface decision in Miro | OC00 proves OpenCode access; both agree the contract |
| Q02 | OpenCode | scripts/mutation-gate.sh only | Q01 ACK and exclusive claim |
| Q03 | Codex | new tests/mutation_gate/** only | Q01 ACK and exclusive claim |
| Q04 / Q05 | Codex reviews runner; OpenCode reviews fixtures | Read-only exact Q02/Q03 heads | Both artifacts in Review; authors repair |
| Q06 | Codex | new .github/workflows/mutation-gate.yml and docs/mutation-quality-gate.md | Q04/Q05 accepted; agreed integration base |
| Q07 | OpenCode | Read-only Q06 and CI artifacts | Exact Q06 head and actual CI evidence |
| Q08 | Codex | Integration candidate for dev | Q07 accepted; required checks and reviewed candidate content |

Existing ci.yml is outside the initial writable scope. Keep its independent
jobs and the native platform workflow intact; any shared-file expansion needs
an explicit scope decision.

1. Parse `--base <ref>` explicitly, validate it and select changed eligible
   Rust source without word-splitting. Preserve command exit codes. Never hide
   an invalid base with the current `git diff ... || git diff HEAD` fallback.
2. Execute real `cargo mutants` on the agreed supported packages/files. Use
   isolated fixtures or safe runner workspaces; do not mutate another writer's
   checkout in place. Documentation-only/no-eligible diffs skip visibly;
   out-of-pilot production changes are reported as uncovered.
3. Pin the tool version, resolve the actual PR base with sufficient checkout
   history, bound time/cost and retain reports/logs even when the gate fails.
4. Missed mutants fail. Setup/tool failure, failing baseline, timeout and
   inconclusive results are distinct from successful mutation detection.
   Exclusions require an owner and reason; no silent success path.
5. An actual controlled Rust fixture proves a killed mutation passes and a
   surviving behavioral mutation fails. Exercise invalid base, missing tool,
   unsupported source, no eligible changes and timeout reporting as contracts,
   rather than merely asserting that a command string was assembled.
6. Document enforced versus advisory checks and measure runtime before
   widening coverage. Report behaviors/branches detected, not test totals.

**Acceptance:** actual mutation artifacts; a controlled surviving mutant makes
CI nonzero; failure classes and uncovered changes cannot masquerade as coverage;
independent adversarial review and ordinary required checks are recorded at the
candidate's exact SHA before merge. This pilot does not itself prove the later
coordinator extraction: each extraction also needs focused contract tests and
appropriate mutation evidence.

## Codex / OpenCode coordination

The [Miro queue](https://miro.com/app/board/uXjVEel-qWk=/?moveToWidget=3458764685914585078),
[mailbox](https://miro.com/app/board/uXjVEel-qWk=/?moveToWidget=3458764685914585825)
and [scope ledger](https://miro.com/app/board/uXjVEel-qWk=/?moveToWidget=3458764685914651075)
coordinate external coding agents. They do not change Concerto's runtime roles.

- At session start/resume, refresh dev, current PRs, task dependencies, mailbox
  and claims. Reserved is a future assignment, not an active writer lock.
- Claim an exact branch/base/file set with a unique agent/session/task token.
  The counterpart ACKs disjointness or transfer; silence is not agreement.
  Miro reservations are cooperative, not atomic locks.
- One writer per scope; review the other's exact SHA read-only. Authors repair
  their branches. Scope expansion or ownership transfer needs DECISION/ACK;
  transfer also preserves the original work and records RELEASE.
- Handoffs state base/head, files, behavior, commands, observed results,
  limitations and whether the claim is held or released.
- Confirm that the merge candidate contains every reviewed fix. #191 had to
  restore reviewed changes omitted by #172's stale-head merge. A merged flag
  alone is insufficient; a changed/rebased head needs new review evidence.
- Code tasks become Done only after merge into dev. Retired duplicate records
  carry no work authority. No automatic notifications or background worker
  are implied by a board task.

OpenCode's Miro access/ACK remains OC00 and is unverified at this reconciliation.
Q01 is proposed, not an accepted interface. Feature/fix work continues through
PRs to dev under repository instructions. The owner explicitly requested
restoration onto dev. GitHub branch protection requires a PR for delivery;
that transport requirement does not change the retention instruction.

## Follow-on: coordinator decomposition in small slices

The first refactor should preserve `CoordinatorAgent`'s public surface and
existing checkpoints/events. A practical sequence, to verify against the
fresh source before implementation:

1. Move the main test module into private test files grouped by dispatch,
   planning/recovery, approvals, resume, and completion. Preserve test names
   and meaningful assertions. This is preparation, not the entire refactor.
2. Extract pure run-shape / argument / decision parsing helpers with narrow
   visibility and failure-case contracts.
3. Extract resume/checkpoint and planning-recovery responsibilities, then
   dispatch/fallback/settlement responsibilities, one slice per review.
   Keep coordinator orchestration and shared ownership explicit.
4. Protect known seams: fail-closed preparation/write/approval gates,
   cancellation without terminal-failure reclassification, bounded retries,
   checkpoint continuity and persisted execution_mode; ToolOutcome tri-state,
   evidence-limited dismissal with opened_gate_seq, pinned precedence,
   obligation/zero-work guards, and exactly-once usage/settlement accounting.
5. Run focused orchestrator tests and meaningful mutation probes per slice;
   complete required workspace checks before merge. Do not judge success
   only by a shorter parent file.

Cross-crate ownership, persistent formats or security-boundary changes require
an ADR before implementation. Mechanical private-module extraction should
preserve existing decisions rather than invent a new architecture.

## Performance and fuzzing

The current Criterion inventory is:

| Package | Target |
|---|---|
| core | [policy](../../crates/core/benches/policy.rs), [serde](../../crates/core/benches/serde.rs) |
| memory | [fts_search](../../crates/memory/benches/fts_search.rs), [vector_retrieval](../../crates/memory/benches/vector_retrieval.rs) |
| orchestrator | [task_graph](../../crates/orchestrator/benches/task_graph.rs) |
| providers | [provider_streaming](../../crates/providers/benches/provider_streaming.rs) |
| tools | [virtual_fs](../../crates/tools/benches/virtual_fs.rs) |

PR CI compiles and smoke-runs benchmarks with `--test`; it makes no timing
assertion. [bench-baseline.yml](../../.github/workflows/bench-baseline.yml)
runs weekly/manually, compares cached Criterion baselines with a 25% mean
regression tolerance, and captures a baseline on a cache miss. It is an
investigation signal, not a per-PR timing blocker. Fresh profiling and
optimization evidence remain unverified by this reconciliation.

Retain [shell-parser](../../fuzz/fuzz_targets/shell_parser.rs) and
[guest-ABI](../../fuzz/fuzz_targets/guest_abi.rs) fuzz targets. A later bounded
scheduled/manual job should store corpus/crash artifacts and promote every
reproduced crash into a deterministic regression test. Parser/tree-sitter
property suites already exist; policy combinations, VirtualFs transitions,
TTL ordering and diff properties should be added only where a named invariant
and adversarial input justify them.

## Security and architecture residuals

- **Extension enforcement is shipped:** [host.rs](../../crates/plugins/src/host.rs),
  [host_fns.rs](../../crates/plugins/src/host_fns.rs) and
  [MCP client](../../crates/mcp/src/client.rs) contain allocation limits,
  shared-executor effects and full-lifecycle deadlines from #172/#191.
  Preserve fail-closed paths, resolved-path execution and per-run contexts.
  Residual new-file path races, native consent/keychain and platform release
  evidence still need their own scoped treatment; merge is not a certification.
- **Native shell boundaries:** direct argv execution and human-owned security
  are implemented. protected_paths constrains the built-in filesystem tool,
  not arbitrary host programs or writable container mounts. External program
  writes bypass VirtualFs staging. Host mode has ambient OS permissions;
  unsupported isolation/resources fail closed.
- **CPU and platform limits:** [shell_security.cpu_seconds](../../crates/core/src/shell_security.rs)
  exists and [native process execution](../../crates/tools/src/native_process.rs)
  applies it through container per-process ulimit. It is not an aggregate CPU
  allowance or Windows/cgroup-v2 completion. Native Windows cancellation
  reaches only the direct child; do not infer MCP tree-cleanup guarantees for
  that path. See [native security limits](../native-shell-security.md) and
  [DEFERRED rows 36/45](../DEFERRED.md#outstanding); D01 must reconcile stale
  claims that no operator-facing CPU key exists.
- **Cancellation remains partial:** [LSP client](../../crates/lsp/src/client.rs)
  checks tokens around reader loops while blocked reads remain an audit lead;
  [ContextOverflowStrategy](../../crates/core/src/traits/context_overflow.rs)
  has implementations accepting _cancel. Audit executor waits, cleanup and
  persistence through named already-cancelled/mid-flight cases before changes.
  Accepting a token alone does not prove prompt cancellation.
- **Audit controls:** encryption/rate limiting remain opt-in. Keep the
  [threat model](../security-threat-model.md) and
  [security boundaries](../../SECURITY_BOUNDARIES.md) scoped to real enforcement.
- **API consistency:** MemoryError unification is done. SessionError/EvalError
  renames, constructor standardization and non_exhaustive require caller and
  compatibility review rather than workspace-wide mechanical edits.
- **Events and docs:** verify live publishers, consumers and replay per event
  family. Do not revive removed event variants to meet a count target. Restore
  documentation warnings in measured stable-module slices, rather than
  switching every crate directly to denial.

## Shipped work and maintenance

| Work | Status in the pinned source | Follow-up |
|---|---|---|
| [#172](https://github.com/NefaroXX/Concerto/pull/172), [#186](https://github.com/NefaroXX/Concerto/pull/186), [#191](https://github.com/NefaroXX/Concerto/pull/191) | Extension implementation and restored reviewed fixes present | Preserve regressions; collect scoped native consent/keychain results in V01 |
| [#169](https://github.com/NefaroXX/Concerto/pull/169), [#185](https://github.com/NefaroXX/Concerto/pull/185) | Editor workspace and dirty-tab exit guard present | Live editor evidence remains separate |
| [#174](https://github.com/NefaroXX/Concerto/pull/174), [#175](https://github.com/NefaroXX/Concerto/pull/175), [#180](https://github.com/NefaroXX/Concerto/pull/180)–[#188](https://github.com/NefaroXX/Concerto/pull/188) | Question identity, outcome/evidence/obligation and review hardening present | Preserve current contracts during R01–R04 |
| [#177](https://github.com/NefaroXX/Concerto/pull/177) | Task execution_mode persists through sessions | Migration 036 and resume compatibility remain protected |
| [#189](https://github.com/NefaroXX/Concerto/pull/189) | Settings/management CLI parity present | P01/P02 cover remaining interactions |
| [#190](https://github.com/NefaroXX/Concerto/pull/190) | Native shell and global security present | Preserve documented platform/protected-path/resource limits |

The former ADR78 collision is resolved: editor uses ADR78, extension security
ADR79, CLI parity ADR80 and native shell ADR81 in the [current index](../adrs/README.md).
Do not schedule a renumbering task for that resolved collision.

Before implementation, re-read fresh dev and pending work rather than relying
on this snapshot. DEFERRED/TODO still contain older statements; check them
against source and preserve one deferred-work authority. Update this active
plan when an item lands, re-measure hotspots after refactors and record
verified contracts and remaining limits. The owner explicitly requested
retention of this file; directory cleanup must preserve it unless the owner
changes that instruction.

## Normalization track (NORM) - owner-requested

Appended 2026-10-06 from `dev` at `0d07038` as a pure, add-only section at the
end of this file, deliberately isolated from the open
[D01 reconciliation (#202)](https://github.com/NefaroXX/Concerto/pull/202),
which edits this plan's header and status lines; appending here avoids a merge
conflict with that PR. This section sets conventions, order and gates for
normalization work. It claims no completed normalization, no merged slices and
no new measurements.

**Exemplar and five conventions.**
[shell/parser.rs](../../crates/shell/src/parser.rs) (320 lines) is the
reference module. Slices in this track apply: (1) verb-first function names;
(2) `thiserror` error enums with every failure mode a named variant, plus
`# Errors` sections on fallible APIs; (3) `//!` module/crate docs with doctest
examples; (4) in-file failure-case tests, including proptest properties where
a property is named; (5) size bars — modules <= 500 lines, functions <= ~100
lines, zero `unwrap`/`expect` in library (non-test) code. Convention (3) is a
target, not a status claim: the exemplar carries no doctest and doctests are
near-absent tree-wide today.

**Monolith order — smallest first; each slice moves < 500 lines:**
(1) tools staged helpers; (2) desktop
[code_editor/](../../crates/desktop/src/views/code_editor/); (3) chat
transcript merge; (4) sessions splits; (5) orchestrator giants last
(`coordinator.rs`, `runtime_runner.rs`, `agent_loop.rs` — sizes in the
hotspot table above). Never replace one monolith with one new giant file.
Named slices so far: slice 2 is the desktop staged-helper move (~120 moved
lines); slice 3 is the chat transcript merge (~80 moved lines).

**Spaghetti singles — one relocation target each:**

- S1: `StagedReview` finds its home in
  [tools/diff.rs](../../crates/tools/src/diff.rs), post-#203;
- S2: chat entries find their home in
  [views/chat.rs](../../crates/desktop/src/views/chat.rs);
- S3: plan-approval truth in
  [plan_approval.rs](../../crates/orchestrator/src/plan_approval.rs);
- S4: `parse_tool_blocks` finds its home under
  [agents/](../../crates/orchestrator/src/agents/) — inventory label; confirm
  the exact symbol before moving it.

The inventory references an S5 with no target named in this request; it stays
unassigned and nothing moves under that label until it is.

**Remnants rule.** Each `allow(dead_code)` (8 files at this base) is justified
or removed individually — no blanket sweeps. The
`unimplemented!("not expected in this test")` wall inside
[runtime_runner.rs](../../crates/orchestrator/src/runtime_runner.rs)'s test
module is kept; it is a test sentinel, not dead product code. `TODO`
pointers are kept — registers own the work. `dbg!` is already zero in
`crates/`; keep it zero. The `unwrap`/`expect` ban stays scoped to library
(non-test, non-`main`) code per [AGENTS.md](../../AGENTS.md).

**Slice gates.** Every slice is behavior-preserving and lands as exactly one
PR: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
-D warnings` and the focused crate tests run green, with independent review of
the exact head before merge. Dependencies: coordinator slices are blocked by
[#200](https://github.com/NefaroXX/Concerto/pull/200) (R02 resume-test
extraction) and staged-review normalization by
[#203](https://github.com/NefaroXX/Concerto/pull/203) (the CLI twin of P02) —
which is why S1 is explicitly post-#203.

**V01 stays deferred.** Live release evidence for shipped
extension/editor/native-shell behavior remains open exactly as stated in the
work-order table above. Nothing in this normalization track claims live UI,
keychain or process proof for any slice.

