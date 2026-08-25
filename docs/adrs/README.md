# Architecture Decision Records

Numbered, append-only record of Concerto's architecture decisions. Each ADR is
a standalone Markdown document in this directory (`ADR-NN[-slug].md`);
superseded records are retained under [`archive/`](./archive/) and reduced to
stubs in place — nothing is ever deleted.

## Status legend

| Status | Meaning |
|---|---|
| **Accepted** | Decision is in force. |
| **Accepted — amended/extended by ADR-NN** | In force as modified by the named ADR(s); read both. |
| **Accepted (revised in place)** | Corrected by maintainer direction; no superseding ADR exists. |
| **Deferred** | Design is recorded for future work; not implemented. |
| **Active** | Approved and in force; may carry in-document revisions. |
| **Superseded** | Replaced by a later ADR; archived copy kept for history. |
| **Archived** | Historical record only; not current guidance. |

## Consolidation note (2026-08-22)

This set was retrospectively consolidated on 2026-08-22: every ADR now carries
an explicit `**Status:**` and `**Date:**` header; the founding ADRs (01–08,
11, 12, 14, 16, 22) carry dates from their original July–August 2025 design
window plus a *"Last updated: 2026-08-22 (retrospective consolidation)"*
footer; superseded records (10, 21, 24, 27, 28) were reduced to stubs pointing
at their successors with full text preserved under `archive/`; and three
codification ADRs (61–63) were added documenting long-standing layers that had
no dedicated record.

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
| [19](./ADR-19.md) | Multi-Agent Orchestration | 2025-09-02 | Accepted — routing portion superseded by [ADR-31](./ADR-31.md) | Opt-in coordinator + specialist agents sharing `EventBus` memory; DAG-based task scheduling. |
| [20](./ADR-20.md) | Rich Iced Desktop UI — Architecture, Theming & Accessibility | 2026-06-26 | Accepted | Routed desktop architecture, theming system, and accessibility targets. |
| [22](./ADR-22.md) | Hybrid Retriever Ranking — Reciprocal Rank Fusion (RRF) | 2025-08-06 | Accepted | RRF (`k = 60`) combines vector and BM25 ranks without score normalization. |
| [23](./ADR-23.md) | Baseline Architecture Overview — Knowledge-Graph Snapshot | 2026-07-12 | Accepted | Descriptive (not normative) snapshot of the as-built workspace structure. |
| [25](./ADR-25.md) | Derive tool input JSON Schemas from Rust types | 2026-07-15 | Accepted | Rust types are the single source of truth for tool input contracts. |
| [26](./ADR-26.md) | Fault containment and recovery in multi-agent runs | 2026-07-16 | Accepted | Per-subtask retry and isolation instead of run-wide failure propagation. |
| [29](./ADR-29.md) | AI-native shell runtime and policy-gated execution | 2026-07-18 | Accepted | Structured, context-aware command execution — no terminal text scraping. |
| [30](./ADR-30.md) | Unified Agent Shell Selection | 2026-07-19 | Accepted — supersedes part of [ADR-28](archive/ADR-28.md) | One shell-resolution path across CLI and desktop. |
| [31](./ADR-31.md) | Model-first selection with internal provider routing | 2026-07-20 | Accepted — supersedes [ADR-24](archive/ADR-24.md) | The session's provider/model pair is authoritative; no capability-tier ranking. |
| [32](./ADR-32.md) | Explicit provider failures and safe interactive policy defaults | 2026-07-20 | Accepted | Visible, typed provider errors; conservative default approval prompts. |
| [33](./ADR-33.md) | Shared frontend project and runtime context | 2026-07-23 | Accepted | Common project/session context across desktop and CLI frontends. |
| [34](./ADR-34.md) | Durable orchestration runtime | 2026-07-27 | Accepted | Persisted runs and checkpoints for multi-agent orchestration. |
| [35](./ADR-35.md) | Tag-driven agent orchestration with Coordinator-first architecture | 2026-08-01 (rev. 2026-08-13) | Accepted (revised in place) | `AgentStage` tags drive coordinator algorithms; open-vocabulary custom agents. |
| [36](./ADR-36.md) | Durable typed session transcript | 2026-08-01 | Accepted — complete | Typed, replayable transcript entries persisted in SQLite. |
| [37](./ADR-37.md) | Plugin Capability Grant Lifecycle — TTL, Hash Pinning, Revocation | 2026-07-26 | Accepted (renumbered 2026-08-02) | Time-bounded, pinned, revocable capability grants instead of indefinite approvals. |
| [38](./ADR-38.md) | Async WASM Host Functions | 2026-08-02 | Accepted — implemented | wasmtime `async_support` for plugin host calls. |
| [39](./ADR-39.md) | Embedder Degradation Handling | 2026-08-02 | Accepted | Stale-marking, backoff pause, explicit degradation events, FTS-only fallback with notice. |
| [40](./ADR-40.md) | Audit Log is Append-Only and Outlives Session Pruning | 2026-08-02 | Accepted | The audit trail survives session deletion by design. |
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
| [55](./ADR-55-intent-routing-and-authorization.md) | Intent routing and intent-gated authorization (three-generation gate) | 2026-08-09 | Accepted — partially superseded by [ADR-56](./ADR-56-model-first-intent-classification.md) | Mutation gate and authorization generations; two classifier pins superseded by ADR-56. |
| [56](./ADR-56-model-first-intent-classification.md) | Model-first intent classification | 2026-08-11 | Accepted | The LLM decides intent; deterministic rules remain as fast paths/fallbacks. |
| [57](./ADR-57-config-change-propagation.md) | Config change propagation without restart | 2026-08-13 | Accepted | Desktop watcher + reconcile helper; per-run reload in the CLI. |
| [58](./ADR-58-configurable-orchestration.md) | Configurable orchestration — config owns the pipeline | 2026-08-13 (rev. 2026-08-15) | Accepted (revised in place) | Table-driven stage topology from config; only the coordinator is hardcoded. |
| [59](./ADR-59-studio-blueprint-editor.md) | Studio orchestration editor — one surface, config-owned, full CRUD | 2026-08-14 (rev. 2026-08-15) | Accepted (revised in place) | Single-surface roster editor with locked coordinator and atomic saves. |
| [60](./ADR-60-concurrent-agent-runtime.md) | Concurrent Agent Runtime — Process-per-Agent Supervisor | 2026-08-18 (rev. 2026-08-24) | Accepted | Process-per-agent supervision, event-sourced whiteboard, and a durable memory spine; amends the [ADR-35](./ADR-35.md) coordinator contract, [ADR-36](./ADR-36.md) transcripts become log projections. Supersedes none. |
| [61](./ADR-61-provider-layer-and-factory.md) | Provider Layer — `LlmProvider` Trait, Factory, Transport Hardening | 2026-08-18 | Accepted | One provider execution contract, one construction path, uniform transport behavior. |
| [62](./ADR-62-tool-executor-and-virtual-fs.md) | Tool Execution Pipeline — `ToolExecutor`, Policy Gates, `VirtualFs` | 2026-08-19 | Accepted | Single auditable, policy-gated mutation boundary with staged, reversible writes. |
| [63](./ADR-63-memory-subsystem.md) | Memory Subsystem — SQLite Hybrid Vector/FTS Retrieval | 2026-08-19 | Accepted — supersedes [ADR-10](archive/ADR-10.md) | Offline hybrid semantic + lexical retrieval over SQLite with local embeddings. |

## Archived ADRs ([`archive/`](./archive/))

Full historical texts, retained verbatim; not active guidance.

| ADR | Title | Superseded by / consolidated into |
|---|---|---|
| [ADR-10](./archive/ADR-10.md) | Vector Store — LanceDB | [ADR-63](./ADR-63-memory-subsystem.md) — LanceDB removed entirely in pre-release cleanup; SQLite is the only vector store. |
| [ADR-21](./archive/ADR-21.md) | WASM Plugin Implementation — Runtime, Host ABI, Capability Model | [ADR-14](./ADR-14.md) (living design); async host functions in [ADR-38](./ADR-38.md). |
| [ADR-24](./archive/ADR-24.md) | Deterministic Provider/Model Routing | [ADR-31](./ADR-31.md) — capability-tier removal record, refined into model-first selection. |
| [ADR-27](./archive/ADR-27.md) | Integrated desktop terminal lifecycle | [ADR-30](./ADR-30.md) (shell selection); terminal lifecycle consolidated into [ADR-20](./ADR-20.md). |
| [ADR-28](./archive/ADR-28.md) | Shell Profiles and Integrated Toolchain | [ADR-30](./ADR-30.md) (unified shell selection, carries surviving profile/config decisions); runtime in [ADR-29](./ADR-29.md). |
| [ADR-42](./archive/ADR-42.md) | Coordinator resilience: failure-class fallback ladder (original) | The active [ADR-42](./ADR-42.md) replaced this file's content in place; original retained here. |

## Reading order suggestions

- New to the codebase: [23](./ADR-23.md) (as-built baseline) →
  [62](./ADR-62-tool-executor-and-virtual-fs.md) (safety boundary) →
  [61](./ADR-61-provider-layer-and-factory.md) (providers) →
  [63](./ADR-63-memory-subsystem.md) (memory).
- Orchestration lineage: [19](./ADR-19.md) → [34](./ADR-34.md) →
  [35](./ADR-35.md) → [42](./ADR-42.md)/[45](./ADR-45.md) →
  [58](./ADR-58-configurable-orchestration.md)/[59](./ADR-59-studio-blueprint-editor.md) →
  [60](./ADR-60-concurrent-agent-runtime.md).
- Routing lineage: [24 (archived)](./archive/ADR-24.md) →
  [31](./ADR-31.md) → [49](./ADR-49-config-first-catalog.md) →
  [56](./ADR-56-model-first-intent-classification.md).
