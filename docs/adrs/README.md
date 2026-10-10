# Architecture Decision Records

Numbered, append-only record of Concerto's architecture decisions. Each ADR is
a standalone Markdown document in this directory (`ADR-NN[-slug].md`);
superseded records are not retained: the number stays as a short pointer to its
successor so historical citations resolve, and nothing else is kept.
## Status legend

| Status | Meaning |
|---|---|
| **Accepted** | Decision is in force. |
| **Accepted — amended/extended by ADR-NN** | In force as modified by the named ADR(s); read both. |
| **Accepted (revised in place)** | Corrected by maintainer direction; no superseding ADR exists. |
| **Deferred** | Design is recorded for future work; not implemented. |
| **Active** | Approved and in force; may carry in-document revisions. |
| **Superseded** | Replaced by a later ADR; the number is a pointer to the successor only, and no full text is retained. |

## Consolidation note (2026-08-22)

This set was retrospectively consolidated on 2026-08-22: every ADR now carries
an explicit `**Status:**` and `**Date:**` header; the founding ADRs (01–08,
11, 12, 14, 16, 22) carry dates from their original July–August 2025 design
window plus a *"Last updated: 2026-08-22 (retrospective consolidation)"*
footer; superseded records (10, 21, 24, 27, 28) were reduced to stubs pointing
at their successors; and three
codification ADRs (61–63) were added documenting long-standing layers that had
no dedicated record.

**Numbering.** Numbers are stable and never reused. The gaps at 09, 13, 15, 17,
18 and 51 are permanently retired — those ADRs were withdrawn and their
numbers are not reassigned. 42 was consolidated in place.

Dates state when a decision was made or its document stabilized; consolidation
footers mark exactly which files were touched. Two deliberate exceptions to
early dating: **ADR-43** stays at 2026-08-04 because it pins MCP protocol
revision `2025-11-25` (an earlier date would contradict its own content), and
the 2026 remediation wave (33–63) keeps its genuine recent dates. Numbers 09,
13, 15, 17, 18, and 51 are unused; nothing was deleted to create those gaps.

## Active ADRs

| ADR | Title | Date | Status | Summary |
|---|---|---|---|---|
| [01](./ADR-01.md) | Git Library — `gitoxide` (`gix`) | 2025-07-10 | Accepted | Pure-Rust git integration (stash rollback, diffs, git tools); avoids the libgit2/C dependency. |
| [02](./ADR-02.md) | Correlation / Event IDs — ULID | 2025-07-10 | Accepted | Time-sortable, URL-safe IDs across the event bus, sessions, audit log, and API. |
| [03](./ADR-03.md) | Configuration System — `figment` | 2025-07-11 | Accepted | Layered config: hardcoded defaults → TOML → env vars, with `schema_version` checks. |
| [04](./ADR-04.md) | Secure Credential Storage — `keyring` | 2025-07-11 | Accepted | OS-native credential storage; env-var-backed store for CI/tests. Secrets never touch disk config. |
| [05](./ADR-05.md) | Diff Computation — `imara-diff` | 2025-07-12 | Accepted | Fast unified diffs for `VirtualFs` previews and git diffs; per-hunk accept/reject. |
| [06](./ADR-06.md) | Filesystem Watch — `notify` | 2025-07-12 | Accepted | Cross-platform native file watching for live memory re-indexing. |
| [07](./ADR-07.md) | Terminal UI — `ratatui` + `crossterm` | 2025-07-13 | Accepted | Pure-Rust TUI for the CLI, driven by the shared `EventBus`. |
| [08](./ADR-08.md) | Desktop UI — `iced`, No Tauri Fallback | 2025-07-13 | Accepted — final | Native Rust GUI only; no Electron/Tauri/web fallback, deliberately settled up front. |
| [11](./ADR-11.md) | Multi-Instance File Locking — `fd-lock` | 2025-07-21 | Accepted | One data-directory write lock across processes; SQLite WAL for concurrent readers. |
| [12](./ADR-12.md) | Embedding Versioning | 2025-08-04 | Accepted | Model-tagged embeddings with re-index-on-upgrade flow; prevents cross-model vector mixing. |
| [14](./ADR-14.md) | Plugin Architecture — WASM with Capability-Secure Host ABI | 2025-07-14 | Accepted | WASM plugins (tool/provider/memory adapter) behind a linear-memory host ABI with declared capabilities. |
| [16](./ADR-16.md) | Context Overflow Strategy — Tiered Budget with LLM Summarization | 2025-07-15 | Accepted (updated for Phase 4) | Token-budget-aware context management with tiered eviction and summarization. |
| [19](./ADR-19.md) | Multi-Agent Orchestration | 2025-09-02 | Accepted — routing portion superseded by [ADR-31](./ADR-31.md); cycle-terminal and planner-authority points partially superseded by [ADR-71](./ADR-71-coordinator-supremacy.md) | Opt-in coordinator + specialist agents sharing `EventBus` memory; DAG-based task scheduling. |
| [20](./ADR-20.md) | Rich Iced Desktop UI — Architecture, Theming & Accessibility | 2026-06-26 | Accepted | Routed desktop architecture, theming system, and accessibility targets. |
| [22](./ADR-22.md) | Hybrid Retriever Ranking — Reciprocal Rank Fusion (RRF) | 2025-08-06 | Accepted | RRF (`k = 60`) combines vector and BM25 ranks without score normalization. |
| [23](./ADR-23.md) | Baseline Architecture Overview — Knowledge-Graph Snapshot | 2026-07-12 | Accepted | Descriptive (not normative) snapshot of the as-built workspace structure. |
| [25](./ADR-25.md) | Derive tool input JSON Schemas from Rust types | 2026-07-15 | Accepted | Rust types are the single source of truth for tool input contracts. |
| [26](./ADR-26.md) | Fault containment and recovery in multi-agent runs | 2026-07-16 | Accepted | Per-subtask retry and isolation instead of run-wide failure propagation. |
| [29](./ADR-29.md) | AI-native shell runtime and policy-gated execution | 2026-07-18 | Accepted | Structured, context-aware command execution — no terminal text scraping. |
| [30](./ADR-30.md) | Unified Agent Shell Selection | 2026-07-19 | Accepted — supersedes part of [ADR-28](./ADR-28.md) | One shell-resolution path across CLI and desktop. |
| [31](./ADR-31.md) | Model-first selection with internal provider routing | 2026-07-20 | Accepted — supersedes [ADR-24](./ADR-24.md) | The session's provider/model pair is authoritative; no capability-tier ranking. |
| [32](./ADR-32.md) | Explicit provider failures and safe interactive policy defaults | 2026-07-20 | Accepted | Visible, typed provider errors; conservative default approval prompts. |
| [33](./ADR-33.md) | Shared frontend project and runtime context | 2026-07-23 | Accepted | Common project/session context across desktop and CLI frontends. |
| [34](./ADR-34.md) | Durable orchestration runtime | 2026-07-27 | Accepted | Persisted runs and checkpoints for multi-agent orchestration. |
| [35](./ADR-35.md) | Tag-driven agent orchestration with Coordinator-first architecture | 2026-08-01 (rev. 2026-09-27) | Accepted (revised in place) — §8 self-execution exhaustion condition amended by [ADR-74](./ADR-74-delegation-doctrine-and-ladder-hold.md) | `AgentStage` tags are an open affinity vocabulary the Coordinator weighs; every dispatch, review, validation, and self-execution (roster exhaustion, §8) is a Coordinator decision. |
| [36](./ADR-36.md) | Durable typed session transcript | 2026-08-01 | Accepted — complete | Typed, replayable transcript entries persisted in SQLite. |
| [37](./ADR-37.md) | Plugin Capability Grant Lifecycle — TTL, Hash Pinning, Revocation | 2026-07-26 | Accepted (renumbered 2026-08-02) | Time-bounded, pinned, revocable capability grants instead of indefinite approvals. |
| [38](./ADR-38.md) | Async WASM Host Functions | 2026-08-02 | Accepted — implemented | wasmtime `async_support` for plugin host calls. |
| [39](./ADR-39.md) | Embedder Degradation Handling | 2026-08-02 | Accepted | Stale-marking, backoff pause, explicit degradation events, FTS-only fallback with notice. |
| [40](./ADR-40.md) | Audit Log is Append-Only and Outlives Session Pruning | 2026-08-02 | Accepted — §Decision item 3 superseded by [ADR-73](./ADR-73-audit-encryption-and-retention.md); items 1/2/4 in force | The audit trail survives session deletion by design. |
| [41](./ADR-41.md) | Spend surfaces in the status bar; no Dashboard page | 2026-08-03 | Accepted | Cost/spend visibility inline; no separate dashboard surface. |
| [42](./ADR-42.md) | Coordinator resilience: failure-class fallback ladder | 2026-08-04 | Accepted — amended by [ADR-45](./ADR-45.md), extended by [ADR-35](./ADR-35.md) | Failure classes map to an escalation ladder (retry → provider switch → takeover). |
| [43](./ADR-43-skills-mcp-and-extension-manager.md) | Skills, MCP client, and extension manager | 2026-08-04 | Accepted | Local instruction packs (never execute code) + stdio MCP tools, all policy-gated. |
| [44](./ADR-44.md) | Project-root confinement and consent gating | 2026-08-05 | Accepted | Filesystem access confined to user-consented roots. |
| [45](./ADR-45.md) | Ladder provider switch, retry configurability, and coordinator takeover | 2026-08-07 | Accepted — amends [ADR-42](./ADR-42.md) | Configurable retries; tier-2 dispatch becomes full agent runs on the planning provider. |
| [46](./ADR-46-reasoning-as-data.md) | Reasoning content as first-class data | 2026-08-07 | Accepted | Model reasoning streams are captured as structured data, not flattened prose. |
| [47](./ADR-47-message-parts.md) | Canonical message parts (deferred, flat model retained) | 2026-08-08 | Deferred | Parts structure recorded for future adoption; flat message model stays. |
| [48](./ADR-48-context-engine.md) | ContextEngine v2 — deterministic context assembly | 2026-08-07 | Accepted | Reproducible, budget-aware context assembly pipeline. |
| [49](./ADR-49-config-first-catalog.md) | Config-first model catalog — providers as data | 2026-08-08 | Accepted | Providers/models come from config data, not compiled-in tiers. |
| [50](./ADR-50-tool-coercion-and-binary-read-contract.md) | Tool coercion + binary read contract | 2026-08-08 | Accepted — implemented | Argument coercion rules and bounded binary reads at the tool boundary. |
| [52](./ADR-52-orchestration-safety-gates.md) | Orchestration safety gates — global run cap, plan artifacts, exit gate | 2026-08-08 | Accepted — implemented | Hard caps and durable plan artifacts bound multi-agent runs. |
| [53](./ADR-53-dialect-plugins-and-plugin-heartbeat.md) | Dialect plugins and plugin heartbeat (Phase 6) | 2026-08-08 | Accepted — implemented | Shell dialects as plugins; liveness heartbeat for plugin health. |
| [54](./ADR-54-memory-stub-store-hardening.md) | Stub Global Memory, Self-Heal Stores, Identify All Failures | 2026-08-08 | Accepted | Stub-backed global memory with self-healing stores after live-test failures. |
| [55](./ADR-55-intent-routing-and-authorization.md) | Intent routing and intent-gated authorization — one loop, deterministic containment | 2026-08-09 | Accepted — Phase 2c classifier pins partially superseded by [ADR-56](./ADR-56-model-first-intent-classification.md); outcome→topology and planner-roster points partially superseded by [ADR-71](./ADR-71-coordinator-supremacy.md); addenda 1d–2e consolidated in place 2026-09-28  | Mutation gate, plan agreement, deterministic containment; phase-by-phase record in the consolidated decisions in [ADR-55](./ADR-55-intent-routing-and-authorization.md). |
| [56](./ADR-56-model-first-intent-classification.md) | Model-first intent classification — the LLM decides intent; deterministic rules become fallbacks | 2026-08-11 | Accepted — classifier retired from run dispatch (2026-09-09) and deleted (2026-09-24); supersedes two ADR-55 Phase 2c pins, in part | Recorded design + current state: the classifier runs on no dispatch path; the deterministic tier classifier (`classify_tier`) is the surviving gate input. |
| [57](./ADR-57-config-change-propagation.md) | Config change propagation without restart | 2026-08-13 | Accepted | Desktop watcher + reconcile helper; per-run reload in the CLI. |
| [58](./ADR-58-configurable-orchestration.md) | Configurable orchestration — config owns the pipeline | 2026-08-13 (rev. 2026-08-15) | Accepted (revised in place) — registry-is-roster point partially superseded by [ADR-71](./ADR-71-coordinator-supremacy.md) | Table-driven stage topology from config; only the coordinator is hardcoded. |
| [59](./ADR-59-studio-blueprint-editor.md) | Studio orchestration editor — one surface, config-owned, full CRUD | 2026-08-14 (rev. 2026-08-15) | Accepted (revised in place) | Single-surface roster editor with locked coordinator and atomic saves. |
| [60](./ADR-60-concurrent-agent-runtime.md) | Concurrent Agent Runtime — Process-per-Agent Supervisor | 2026-08-18 (rev. 2026-09-05) | Accepted | Process-per-agent supervision, event-sourced whiteboard, and a durable memory spine; gate fairness is per-agent in-flight isolation (weighted round-robin considered and rejected); amends the [ADR-35](./ADR-35.md) coordinator contract, [ADR-36](./ADR-36.md) transcripts become log projections. Supersedes none. |
| [61](./ADR-61-provider-layer-and-factory.md) | Provider Layer — `LlmProvider` Trait, Factory, Transport Hardening | 2026-08-18 | Accepted | One provider execution contract, one construction path, uniform transport behavior. |
| [62](./ADR-62-tool-executor-and-virtual-fs.md) | Tool Execution Pipeline — `ToolExecutor`, Policy Gates, `VirtualFs` | 2026-08-19 (rev. 2026-10-09) | Accepted (revised in place) — agent-path staging claims amended 2026-10-09 (see the ADR's **Amendment**) | Single auditable, policy-gated mutation boundary; the overlay is a pre-disk review surface for the human review chains and a post-disk audit/diff record for agent `filesystem` writes. |
| [63](./ADR-63-memory-subsystem.md) | Memory Subsystem — SQLite Hybrid Vector/FTS Retrieval | 2026-08-19 | Accepted — supersedes [ADR-10](./ADR-10.md) | Offline hybrid semantic + lexical retrieval over SQLite with local embeddings. |
| [64](./ADR-64-timeline-zero-waste-orchestration.md) | Timeline-driven zero-waste orchestration | 2026-09-02 | Proposed — compiled-scheduler authority partially superseded by [ADR-71](./ADR-71-coordinator-supremacy.md); the §3 pure-verdict reuse oracle is codified as compatible | Durable timeline + role-agnostic semantic keys + pre-dispatch resolver; plan reuse, gap-driven research, file capsules, agent-removability. |
| [65](./ADR-65-evidence-spine.md) | Evidence spine — facts, claims, and decisions on one append-only chain | 2026-09-04 | Accepted — evidence-scheduler point partially superseded by [ADR-71](./ADR-71-coordinator-supremacy.md); design-doc quarantine advisory | Append-only evidence chain over the whiteboard log; tool facts, workspace snapshot, read cache, resume, resource facts. |
| [66](./ADR-66-harness-tool-call-guarantee.md) | Harness-level tool-call guarantee — every model drives tools or fails loud | 2026-09-07 | Accepted | Tool use is a loop invariant; fail-loud at selection/request/parse seams; universal text-fallback driver; per-model capability resolution. |
| [67](./ADR-67-m01-context-pool-consolidation.md) | M-01 — Consolidate context-overflow pools under a single owner per pool | 2026-09-18 | Accepted | One ContextEngine owns all budget pools; removes double-clip and removed `SummarizeOldest` from production (`NoOp` retained as the default). Note: `SummarizeOldest` was later re-introduced as an opt-in, unwired library strategy in `crates/memory/src/short_term.rs` — see `docs/DEFERRED.md` row 31. Gate-only S effort. |
| [68](./ADR-68-h04-session-ack-breaking-param.md) | H-04 — Breaking parameter change for `request_ack` | 2026-09-18 | Accepted | New `request_ack` signature with session_id validation; PendingAck gains session_id; 6 impls + 5 doubles; desktop single-slot queues-or-rejects-busy. |
| [69](./ADR-69-symbolic-cascade.md) | Symbolic cascade — link store, scoring, and observability in slices | 2026-09-18 | Accepted | Three slices: link store (M 3-5d), scoring+decay (M 5-8d), Mermaid+UI+eval (S-M 3-5d); fail-open, degree/TTL caps, cost-gated progression. |
| [70](./ADR-70-project-agents-md-context-injection.md) | Project AGENTS.md context injection | 2026-09-20 | Accepted | Global + per-project AGENTS.md injected into every prompt path (skills → AGENTS → environment card); project-over-global, bounded/truncated, fail-soft, opt-in, coordinator maintenance nudge (text only). |
| [71](./ADR-71-coordinator-supremacy.md) | Coordinator Supremacy — the coordinator is the sole master of a run | 2026-09-24 | Accepted | Coordinator sole master: instructions run until done / intervention / coordinator error; all agent errors to coordinator; four-class terminal taxonomy (Coordinator decision / intervention / Coordinator error / immutable safety terminals); hardcoded cycle terminals → coordinator-owned guards; planner + registry advisory-only; no intent topology branching; compiled schedulers revoked except the resolver-as-reuse-oracle; scoped partial supersession of ADR-19/55/58/64/65. |
| [72](./ADR-72-containerized-sandbox-profile.md) | Containerized sandbox profile — OS-level isolation via a container runtime | 2026-09-26 | Accepted — implemented (`d00582b`/`3ae6ea5`/`fdf4800`); in force for `Containerized` only; `ReadOnlyFs`/`NetworkIsolated` still denied; Windows not supported, fails closed | Opt-in docker/podman `PATH` detection + fail-closed policy admission (named denial rules) + shell routing through `<runtime> run`; requires a `CommandRouting::Containerized` producer marker; project root bind-mounted read-write, operator-supplied image, no implicit pull, `--network none`; selection programmatic only. |
| [73](./ADR-73-audit-encryption-and-retention.md) | Audit-Log Encryption at Rest and Bounded Retention | 2026-09-26 | Accepted — implemented (`7351128`) | `sessions.db` (including the append-only `audit_log`) encrypted with SQLCipher, opted in and fail-closed; aged rows archived into a keyed archive and then deleted, verified before delete, configured via `[audit]`; answers ADR-40 §Decision item 3. |
| [74](./ADR-74-delegation-doctrine-and-ladder-hold.md) | Delegation Doctrine — delegate by default, hold a rung before demoting it | 2026-09-27 | Accepted — implemented (`5a22405`/`269c344`/`eefd45e`/`115f85c`) | Delegation is the coordinator default; self-execution only on roster exhaustion, enforced by a named `delegation-required` policy refusal with a `CoordinatorSelfImplementing` record; `can_cover` makes agent coverage config data and puts the agent axis before provider escalation; a throttled planning rung is **held** (30 s × 2, 3 recovery rounds) and retried, with the fallback as a **bridge**, not a demotion. Refines ADR-71, ADR-42/45, ADR-58; amends ADR-35 §8's exhaustion condition. |
| [75](./ADR-75-tool-argument-integrity-and-capability-resolution.md) | Tool-argument integrity and capability-driven schema resolution | 2026-09-29 | Accepted | One model-agnostic parse-and-repair entry point (`tool_args`) replaces the silent `Value::Null` tool-argument path (which ran tools with `{}`); unrepairable arguments fail loudly; the `"free"` price-tier name hint is removed; and the tool-schema/transport tier now follows the ADR-66 §3 precedence chain (`dial > advertised > family table > last-resort heuristic`), defaulting unknown models to the optimistic streamed/strict end. |
| [76](./ADR-76-conversational-turns-vs-action-required-orchestration.md) | Conversational turns vs. action-required orchestration | 2026-10-01 | Accepted | Adds `TaskExecutionMode::CoordinatorDecides` (coordinator owns the turn; may answer directly or delegate) between `AnswerOnly` and `ActionRequired`; the runtime entry classifies the run shape structurally (Apply / resume / forced-single-agent / otherwise) with no word router; only `ActionRequired` arms the mandatory specialist-dispatch guards (as amended by the 2026-10-04 in-place addendum: the combined `dispatch_guard_arms` also arms on open graph obligations or a promised plan with no code artifact, and the guarantee covers declared/graphed work only), so ordinary conversation completes without manufactured delegation while real work still cannot close on prose alone. Refines ADR-71 and ADR-74. |
| [77](./ADR-77-studio-configuration-workspace.md) | Studio configuration workspace | 2026-10-02 | Proposed | Separates agent configuration from advisory blueprints; adds focused multiline editing and truthful runtime configuration boundaries. |
| [78](./ADR-78-editor-workspace.md) | Native editor workspace | 2026-10-03 | Proposed | Session-local document tabs, scoped LSP replies, bottom Problems panel and selected-file staged review. |
| [79](./ADR-79-extension-security-enforcement.md) | Extension authorization and host execution | 2026-10-03 | Accepted | Discovery never creates grants; runtime activation requires deny-by-default capability approval matched to the current binary/scope/TTL; plugin file and shell effects use the shared executor, VirtualFs, and shell profile; MCP deadlines include transport waits; guest growth is bounded and workers are torn down with the server. |
| [80](./ADR-80-cli-settings-and-studio-parity.md) | CLI settings and Studio parity | 2026-10-03 | Accepted | Parity applies to capabilities rather than whole graphical pages; adds validated scoped config commands, canonical agent edits, provider/extension/shell management and portable display preferences. |
| [81](./ADR-81-native-shell-security.md) | Native shell execution and user-owned security settings | 2026-10-02 | Accepted for implementation | Native argv execution, protected global settings, shared client approvals, and fail-closed container requirements; partially supersedes ADR-30. |
| [82](./ADR-82-harness-contract-surface.md) | Harness contract surface — typed failures, verification outcomes, environment manifest, checkpoints, and memory evidence | 2026-10-09 | Proposed | Extends the existing type surface rather than five greenfield contracts: derived `ToolFailure` (shell envelope intact), additive verification outcomes over `VerificationSummary` (skip/no-manifest/eval errors become honest records), descriptive `EnvironmentManifest` (never an authorization token), durable file checkpoints (sidecar migration 037; restore is an authorized mutation), and a read-time `MemoryEvidence` composite; one canonical effect key fixes alias-write accounting escapes; opt-in completion-honesty gate. |

> **Partial supersession note (2026-09-24):**
> [ADR-71](./ADR-71-coordinator-supremacy.md) makes **scoped partial
> supersession** declarations against ADR-19, ADR-55, ADR-58, ADR-64, and
> ADR-65 on exactly the points revoked in its conflict table (§3 of the ADR;
> summarized in those rows' Status cells above). These are partial, not full,
> supersessions — none of the five files is moved to anywhere; nothing of
> current design is deleted. Read ADR-71 before relying on the revoked points.

> **Scoped partial supersession note (2026-09-26):**
> [ADR-73](./ADR-73-audit-encryption-and-retention.md) supersedes **only**
> ADR-40's §Decision item 3, which read "audit retention remains a future
> policy question" and asked for exactly that ADR. Item 3 is restated in
> [ADR-40](./ADR-40.md) in its settled form, so it is read there. ADR-40's other
> clauses — append-only, detach-don't-delete, migration rebuild — remain in
> force and ADR-40 is **not** archived. The distinction item 3 drew is kept:
> retention is a time-based policy, not a session-lifecycle one.

> **Refinement note (2026-09-27):**
> [ADR-74](./ADR-74-delegation-doctrine-and-ladder-hold.md) **refines
> [ADR-71](./ADR-71-coordinator-supremacy.md) and does not contradict it.**
> Supremacy settled *who* decides a run (the Coordinator, alone); ADR-74
> settles *what the Coordinator may decide with* — delegation is the default,
> self-execution is an enumerated roster-exhaustion case, and the agent axis is
> consulted before provider escalation. Every mechanism it adds is still a
> Coordinator decision: a prompt doctrine the Coordinator follows, a
> policy refusal the Coordinator can only resolve by dispatching, and
> Coordinator-owned recovery that holds or bridges. It adds no compiled
> authority that selects an agent, orders work, or ends a run — the exact thing
> ADR-71 §3 revoked. It also **amends ADR-35 §8** on one point only: the
> self-execution trigger is now roster exhaustion (empty / disabled-unavailable /
> delegation-attempted-and-failed) rather than *stage absence*, because gating on
> which stage is staffed would constrain operator-chosen rosters. ADR-35 §8's
> shared-executor guardrails, the `coordinator-self-execute` sentinel, and the
> reserved `coordinator` id are unchanged. ADR-42/ADR-45 are refined, not
> superseded — and one ADR-45-era invariant is deliberately **reversed**: the
> ladder no longer refuses to reassign a hard-failed subtask to a same-stage
> peer; that peer is now the preferred takeover target.

## Superseded stubs (live, no full text retained)

These five files exist in `docs/adrs/` and are listed here so that reading the
directory accounts for every file. Each is a short stub that points at its
successor and records no current design. **None is active guidance**, none
should be cited by line number, and **no full text is preserved** — the
superseded text was not retained when the `docs/adrs/archive/` tier was removed.
If you need the reasoning behind a superseded decision, it survives in the
successor ADR's Context section or in the commit history.

| Stub in this directory | Title carried by the stub | Superseded by |
|---|---|---|
| [ADR-10](./ADR-10.md) | Vector Store — LanceDB | [ADR-63](./ADR-63-memory-subsystem.md) |
| [ADR-21](./ADR-21.md) | WASM Plugin Implementation | [ADR-14](./ADR-14.md); async host functions in [ADR-38](./ADR-38.md) |
| [ADR-24](./ADR-24.md) | Deterministic Provider/Model Routing | [ADR-31](./ADR-31.md) |
| [ADR-27](./ADR-27.md) | Integrated Desktop Terminal Lifecycle | [ADR-30](./ADR-30.md) |
| [ADR-28](./ADR-28.md) | Shell & Process Toolchain | [ADR-30](./ADR-30.md) |

`ADR-55`'s phase history (addenda 1d–2e) and `ADR-56`'s amendment blocks are
not stubs: they were consolidated into the live ADR-55/ADR-56 files in place on
2026-09-28, with their still-in-force decisions restated there.

## Reading order suggestions

- New to the codebase: [23](./ADR-23.md) (as-built baseline) →
  [62](./ADR-62-tool-executor-and-virtual-fs.md) (safety boundary) →
  [61](./ADR-61-provider-layer-and-factory.md) (providers) →
  [63](./ADR-63-memory-subsystem.md) (memory).
- Orchestration lineage: [19](./ADR-19.md) → [34](./ADR-34.md) →
  [35](./ADR-35.md) → [42](./ADR-42.md)/[45](./ADR-45.md) →
  [58](./ADR-58-configurable-orchestration.md)/[59](./ADR-59-studio-blueprint-editor.md) →
  [60](./ADR-60-concurrent-agent-runtime.md).
- Routing lineage: [24 (superseded)](./ADR-24.md) →
  [31](./ADR-31.md) → [49](./ADR-49-config-first-catalog.md) →
  [56](./ADR-56-model-first-intent-classification.md).
