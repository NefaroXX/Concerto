# Codebase Improvement Plan: World-Class Engineering

> **Reconciled: 2026-10-04 (Africa/Johannesburg).** Source baseline:
> [`dev` at `345c429d20af5bd2840475f022a935829e483a77`](https://github.com/NefaroXX/Concerto/commit/345c429d20af5bd2840475f022a935829e483a77),
> committed 2026-10-03 UTC.
> This replaces the 2026-08-06 assessment and its 8–12 week estimate.
> Status means present in that snapshot; an open PR is not shipped.
> [DEFERRED.md](../DEFERRED.md) remains the deferred-work register.
> This document reconciles the engineering programme and specifies the next
> task; it does not create a second authority for deferred product features.

## Assessment and evidence limits

Concerto has a substantial policy, event, persistence, provider, and testing
foundation. Several original refactoring targets are already implemented.
The remaining engineering burden is concentrated in orchestration module
size, enforceable test-quality gates, cancellation contracts, frontend
configuration parity, and verification of security boundaries.

The old "already world-class", zero-vulnerability, zero-warning, complexity,
LOC, and test-count assertions were not current measurements. They are
retired, along with the blanket 1,500-test target and hour/week estimates.
Passing configured checks is evidence for those checks, not a security or
coverage certification.

This reconciliation inspected the repository tree, the affected source
modules, manifests, tests, fuzz targets, CI workflows, and open PRs through
GitHub. It did not execute Rust builds, fuzzing, mutation testing, profiling,
or desktop UI checks. Existing
[CI](https://github.com/NefaroXX/Concerto/actions/runs/37155188939) and
[Windows build](https://github.com/NefaroXX/Concerto/actions/runs/37155188918)
runs for the baseline commit completed successfully; those are upstream
results, not checks run for this document.

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
| WASM sandbox | Partial; important fixes pending | [host.rs](../../crates/plugins/src/host.rs) supplies per-plugin stores, fuel, epochs, and disables memory64. It configures static-memory reservation but does not install an allocation-time `StoreLimits` limiter in this snapshot. PR #172 addresses this and other boundary defects; do not mark complete before merge and verification. |
| Test-quality / mutation gate | Not enforced | [mutation-gate.sh](../../scripts/mutation-gate.sh) echoes "Would run" and executes no `cargo mutants` command. Neither it nor the panic sweep is invoked by current CI. This is the recommended next task. |
| Desktop ↔ CLI parity | Historical scope too narrow | [Parity plan](../desktop-cli-parity.md) calls Studio GUI-only. Portable orchestration/security settings require their own inventory and CLI surfaces; a visual layout exemption is not a configuration exemption. |

## Measured structural hotspots

These are physical line counts, including comments, blank lines and tests,
from the pinned UTF-8 blobs (`len(content.splitlines())`). The last column
locates the main inline test module; it is not a production-LOC measurement,
because some files have other inline tests or helpers. No fresh cognitive or
cyclomatic analysis was run.

| File | Total lines | Main test module starts |
|---|---:|---:|
| [orchestrator/coordinator.rs](../../crates/orchestrator/src/coordinator.rs) | 40,185 | 17,533 |
| [orchestrator/runtime_runner.rs](../../crates/orchestrator/src/runtime_runner.rs) | 9,532 | 5,469 |
| [desktop/app.rs](../../crates/desktop/src/app.rs) | 9,196 | 5,109 |
| [orchestrator/agent_loop.rs](../../crates/orchestrator/src/agent_loop.rs) | 8,244 | 3,314 |
| [desktop/views/orchestration_studio.rs](../../crates/desktop/src/views/orchestration_studio.rs) | 7,207 | 4,631 |
| [cli/app.rs](../../crates/cli/src/app.rs) | 3,836 | 2,465 |
| [desktop/views/settings/state.rs](../../crates/desktop/src/views/settings/state.rs) | 3,529 | 2,615 |
| [cli/lib.rs](../../crates/cli/src/lib.rs) | 2,772 | 1,575 |
| [desktop/views/settings/mod.rs](../../crates/desktop/src/views/settings/mod.rs) | 2,721 | 2,539 |
| [plugins/capability.rs](../../crates/plugins/src/capability.rs) | 2,079 | 1,280 |

Moving tests helps navigation but does not, alone, resolve oversized production
modules. Extract by ownership and behavior, preserve public paths and private
invariants, and avoid a single replacement file containing all the complexity.
Line thresholds are review prompts, not universal pass/fail targets.

## Current gates and their limits

[ci.yml](../../.github/workflows/ci.yml) runs formatting, workspace/all-targets
Clippy with warnings denied, workspace unit/integration/doc tests via
`cargo test`, builds, CLI feature combinations and startup smoke commands,
benchmark smoke runs, four WASM guest builds, cargo-deny, and palette checks.
It pins Rust 1.96.0; the workspace declares MSRV 1.88. The old claim that CI
uses nextest and independently runs cargo-audit is incorrect. An MSRV build,
mutation enforcement, fuzz execution and measured coverage are not present
in that workflow.

`unsafe_code = "deny"` remains a workspace rule. Dependency hygiene is
exception-managed: [deny.toml](../../deny.toml) contains documented advisory
exceptions, including Wasmtime 24.0.13, with several review dates of
2026-10-01. Re-review reachability/version assumptions before a dependency
change; do not describe a passing exception-aware check as "zero
vulnerabilities", or remove exceptions without understanding them.

The [contribution test-quality rules](../../CONTRIBUTING.md)
require behavioral justification, adversarial cases, regression-before-fix,
and independent review for hardening/quality work. The separately named
`docs/concerto-test-quality-gates.md` is absent from this snapshot; use the
actual contribution rules rather than an unresolvable reference.
[panic-path-sweep.sh](../../scripts/panic-path-sweep.sh) is a manual report
with a historical checklist, not an automated proof of panic freedom.

## Revised work order

| Priority | Work | Completion condition |
|---|---|---|
| P0 — existing safety work | Resolve and validate pending extension-security work in PR #172 | Approved plugin effects reach shared policy/VirtualFs; allocation/output limits and MCP full-lifecycle deadlines/teardown are tested; native checks and independent adversarial review complete. Reconcile after merge. |
| P1 — next new engineering task | Make the mutation gate executable and enforce a bounded pilot in CI | Real mutations run, useful failures propagate, reports persist, and a deliberate behavioral mutation is caught. Detailed scope below. |
| P2 — structural debt | Decompose coordinator, then runtime runner / agent loop | Characterization evidence protects dispatch, approvals, cancellation, resume, completion, and accounting; production responsibilities have named module owners. Separate mechanical movement from behavioral fixes. |
| P2 — contract defects | Complete cancellation audit / compaction contracts | Test already-cancelled and mid-flight cancellation while blocked on locks/I/O; cleanup, durable-state preservation, and bounded exit are observed. Follow [DEFERRED row 34](../DEFERRED.md#outstanding) / [TODO audit cleanups](../TODO.md#audit-cleanups-deferredmd-row-34). |
| P2 — product parity | Inventory portable Desktop settings and implement CLI access | Each setting has canonical config ownership, scope/default/validation, CLI read/write path, and round-trip/restart evidence. Include orchestration, policies, shell, plugins/MCP, memory, and display controls where applicable. Recheck any existing parity branch before starting. |
| P3 — remaining frontend decomposition | Extract app, CLI dispatch, Studio and settings responsibilities | State transitions and stale async replies have contract tests; changed desktop views pass palette checks and representative dark/light, narrow/wide UI checks. Avoid bundling a new redesign. |
| P3 — measured performance / maintenance | Profile before optimizing; improve docs and compatibility checks | A measured bottleneck justifies each optimization; public API docs and MSRV checks are enabled in scoped increments; error renames require an explicit compatibility decision. |

Security work already in review should not be duplicated. The next *new*
task is P1; its work can proceed while PR #172 completes review.

## Next task: replace the mutation dry run with an executable gate

**Suggested branch:** `fix/mutation-quality-gate`, freshly based on `dev`.
**Purpose:** establish a trustworthy signal before large orchestration
extractions. A script that exits successfully after printing commands provides
no evidence that tests detect wrong behavior.

**Scope:** [scripts/mutation-gate.sh](../../scripts/mutation-gate.sh) and
[ci.yml](../../.github/workflows/ci.yml), plus focused gate fixtures and
contribution/testing instructions.

1. Parse `--base <ref>` explicitly, validate the ref, and determine changed
   eligible Rust source safely. Support file names without word-splitting;
   preserve runner exit codes. Do not hide an invalid base with the current
   `git diff ... || git diff HEAD` fallback.
2. Run `cargo mutants` for the selected supported package/files. Start with a
   bounded core/shell pilot and record its eligible paths; expanding the pilot
   is a later measured decision. A documentation-only diff must skip clearly.
   Changed production files outside the pilot must be reported as uncovered,
   not described as mutation-verified.
3. Pin the tool/version used by CI, bound runtime, and retain mutation outcomes
   and logs as artifacts. Use checkout history sufficient to resolve the
   PR's actual base. Define CI event/ref handling explicitly.
4. Make missed mutants fail. Distinguish baseline-test failure, setup/tool
   failure, timeout/inconclusive results, and successful mutation detection.
   Document any exclusions with an owner and rationale; no blanket silent
   suppression of failures or "would run" path.
5. Prove the gate against an isolated fixture or controlled mutation:
   the unchanged contract passes; a surviving behavioral mutation fails;
   killing that mutation passes. Also exercise invalid base, missing tool,
   unsupported/out-of-pilot source, no eligible changes, and timeout reporting.
6. Update contribution instructions to describe what is now enforced versus
   advisory. Keep the rest of the established CI jobs intact.

**Acceptance criteria**

- A CI report demonstrates actual mutation execution in the pilot and names
  the behavior/branch tested; test totals are not the success metric.
- The controlled surviving mutation makes the gate nonzero, and its report
  explains the failure.
- Source changes outside the pilot and inconclusive work cannot masquerade
  as full coverage. Normal builds/tests remain independently visible.
- Runtime/cost is measured before widening the pilot.
- Independent adversarial review is completed before this quality branch
  merges, as required by CONTRIBUTING.md.

This task does not rename errors, rewrite the coordinator, change policy
semantics, or add a workspace-wide test-count requirement.

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
4. Protect known seams: fail-closed write/approval gates, cancellation without
   terminal-failure reclassification, bounded retries, checkpoint continuity,
   evidence identifiers, and exactly-once usage/settlement accounting.
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

- **Pending extension boundary work:** in this `dev` snapshot,
  `load_discovered_plugins` / `load_and_configure_plugins` in
  [runtime_runner.rs](../../crates/orchestrator/src/runtime_runner.rs) and
  [host_fns.rs](../../crates/plugins/src/host_fns.rs) still need the approved
  authority/shared-executor enforcement described by PR #172.
  [MCP request deadlines](../../crates/mcp/src/client.rs) begin after stdin
  locking/writing; full-lifecycle deadlines and teardown are pending there.
- **Cancellation remains partial:** accepting a token is not sufficient.
  [LSP client](../../crates/lsp/src/client.rs) checks cancellation around its
  reader loop, but pending reads are not raced against cancellation.
  [ContextOverflowStrategy](../../crates/core/src/traits/context_overflow.rs)
  includes implementations accepting `_cancel`; this contract and the
  [executor](../../crates/core/src/executor.rs) need scoped review.
- **Threat-model closure has residuals:** audit encryption/rate limiting are
  opt-in; Windows/container and shell CPU-enforcement limitations remain.
  `cpu_budget_secs` is absent from the inspected
  [configuration schema](../../crates/config/src/schema.rs).
  Consult [DEFERRED rows 36 and 45](../DEFERRED.md#outstanding), not an
  "all security complete" label.
- **API consistency:** `MemoryError` unification is done. Do not mechanically
  unify `SessionError` / `EvalError`, standardize all constructors, or add
  `non_exhaustive` to every enum without reviewing callers and compatibility.
- **Event coverage:** audit live publishers, consumers and replay semantics
  per event family. Do not reintroduce removed review/escalation variants just
  to satisfy an "every variant must emit" rule.
- **Documentation:** restore warnings/docs in selected stable modules after
  measuring their missing-doc surface. Avoid switching every crate directly
  to denial and breaking CI without a remediation slice.

## In-flight work and maintenance

Open at reconciliation; these are dependencies/context, not completed items:

| PR | Scope | Implication |
|---|---|---|
| [#172](https://github.com/NefaroXX/Concerto/pull/172) | Extension security and UI, `fix/extensions-security-ui` | Recheck resource limits, approved authority, MCP lifecycle and UI after merge. Its validation/native-review residuals remain recorded in the PR. |
| [#169](https://github.com/NefaroXX/Concerto/pull/169) | Tabbed editor workspace, `feat/editor-workspace` | Do not duplicate its editor redesign or characterize its new layout as present on this baseline. |
| [#174](https://github.com/NefaroXX/Concerto/pull/174) | World-model OpenProblem decision-scoped question keys | Preserve this behavior when future coordinator/world-model work rebases; it is not yet baseline behavior. |

ADRs #78 are claimed independently by both #169 and #172; reconcile their
numbers/index before both land, following the repository's no-reuse rule.

Before the next implementation, fetch fresh `dev`, inspect pending/merged work,
and recheck the relevant DEFERRED/TODO entries against source. In particular,
those registers were last reconciled on 2026-09-28 and are pointers, not proof
that a claim remains true. Update this document's snapshot/status when an item
lands; preserve deferred-work ownership in DEFERRED.md. Re-measure hotspots
after refactors and report verified contracts and remaining limits, not a
"world-class" score.
