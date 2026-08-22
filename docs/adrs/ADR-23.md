# ADR-23: Baseline Architecture Overview — Knowledge-Graph Snapshot

**Status:** Accepted
**Date:** 2026-07-12
**Deciders:** Concerto architecture

## Context

The Concerto workspace has grown to 22 crates, including the `test-plugin-wasm` / `test-provider-plugin-wasm` / `test-adapter-plugin-wasm` guests and the `eval` / `eval-runner` benchmark crates. To give future ADRs and refactors a stable, objective reference of the *as-built* structure, this ADR records a snapshot of the codebase knowledge graph (generated via `codebase-memory-mcp` on 2026-07-10, at commit `a77d3ae`).

This is a *descriptive* baseline, not a normative one. It captures where code actually lives, which crates depend on which, and which symbols are the most central. Future decisions should reconcile against this baseline rather than against memory.

**Indexed corpus (full mode, semantic edges on):**

| Metric | Count |
|--------|-------|
| Total nodes | 5,657 |
| Total edges | 16,739 |
| Rust files | 240 |
| TOML files | 42 |
| SQL migrations | 11 |
| Languages | Rust, TOML, SQL, Bash, YAML |

Excluded from index: `.git`, `target`.

## Decision

Adopt the following graph-derived map as the canonical baseline architecture reference.

### 1. Crate inventory (by graph node weight)

| Crate | Nodes | Role (from layer analysis) |
|-------|-------|----------------------------|
| `orchestrator` | 358 | Entry/orchestration (outbound calls only) |
| `memory` | 336 | Internal (RAG, prefs, vector/FTS) |
| `desktop` | 280 | Entry (Iced frontend) |
| `core` | 280 | Core (traits, policy, event bus, errors) |
| `providers` | 236 | Entry (LLM provider adapters) |
| `tools` | 176 | Core (fs/shell/git, VirtualFs) |
| `eval` | 175 | Entry (benchmark harness + tasks) |
| `plugins` | 169 | Entry (WASM runtime + capability model) |
| `sessions` | 112 | Internal (SQLite persistence) |
| `config` | 93 | Entry (layered config + credentials) |
| `cli` | 91 | Entry (ratatui TUI) |
| `api-server` | 32 | Axum HTTP server (routes defined) |
| `lsp` | 26 | Internal (LSP client) |
| `observability` | 21 | Internal (OTEL/Langfuse/Prometheus) |
| `api-types` | 15 | Shared request/response types |

### 2. Layering

The graph's layer inference classifies crates as:

- **Core foundation** — `core` (fan-in 35, fan-out 0) and `tools` (fan-in 165, fan-out 14). These are depended on widely and depend on almost nothing.
- **API surface** — `api-server`, plus `json`-tagged route definitions. Exposes HTTP routes (`/sessions`, `/health`, `/metrics`, `/todos`, `/openapi.json`).
- **Entry points** — `cli`, `desktop`, `eval`, `orchestrator`, `plugins`, `providers`, `config`: each has `main`/entry logic and only makes outbound calls.
- **Internal** — `api-server`, `concerto` (binary glue), `eval-runner`, `memory`: service crates with low standalone fan-in/out in this snapshot.

### 3. Cross-crate call boundaries (top edges by call count)

| From → To | Calls | Meaning |
|-----------|-------|---------|
| `plugins` → `tools` | 47 | Plugins delegate fs/shell/git to the tool layer |
| `memory` → `tools` | 38 | Memory indexes/reads files via tools |
| `orchestrator` → `core` | 35 | Orchestrator built on core traits/policy/events |
| `desktop` → `tools` | 21 | UI executes tools directly |
| `eval` → `tools` | 19 | Benchmarks drive the agent tool layer |
| `providers` → `tools` | 16 | Providers may resolve local file context |
| `tools` → `memory` | 14 | Tools inform/query memory |
| `cli` → `tools` | 12 | TUI tool execution |
| `cli` → `memory` | 12 | TUI reads user prefs |
| `config` → `tools` | 12 | Config resolves paths via tools |

**Takeaway:** `tools` and `core` are the gravitational centers. `tools` in particular is the convergence point for every consumer (plugins, memory, desktop, eval, providers, cli, config). Any change to the `tools` crate's public surface has the widest blast radius in the workspace.

### 4. Highest-fan-in symbols (hotspots)

| Symbol | Qualified name | Fan-in |
|--------|----------------|--------|
| `ToastManager::push` | `crates/desktop/src/ui/feedback.rs` | 67 |
| `VirtualFsSnapshot::len` | `crates/tools/src/virtual_fs.rs` | 65 |
| `VirtualFsSnapshot::is_empty` | `crates/tools/src/virtual_fs.rs` | 63 |
| `VirtualFsEntry::path` | `crates/tools/src/virtual_fs.rs` | 54 |
| `VirtualFs::new` | `crates/tools/src/virtual_fs.rs` | 48 |
| `PrefKey::as_str` | `crates/memory/src/prefs.rs` | 48 |
| `VirtualFs::write` | `crates/tools/src/virtual_fs.rs` | 46 |
| `SimplePolicyEngine::new` | `crates/core/src/policy.rs` | 40 |
| `UserPrefsStore::get` | `crates/memory/src/prefs.rs` | 40 |
| `VirtualFs::insert` | `crates/tools/src/virtual_fs.rs` | 32 |

**Takeaway:** The `VirtualFs` abstraction (`crates/tools/src/virtual_fs.rs`) is the single most depended-on type in the system — it appears 5× in the top-10 hotspots. `VirtualFs` and its snapshot are the de-facto shared state model across the agent loop, UI, plugins, and eval. Changes here warrant extra review and tests.

### 5. Architectural clusters (Leiden community detection)

The graph surfaces 12 cohesive clusters (cohesion > 0.63). Representative top nodes per cluster:

| Cluster | Members | Cohesion | Representative nodes |
|---------|---------|----------|----------------------|
| 107 | 144 | 0.76 | `len`, `is_empty`, `push`, `as_str`, `get` |
| 128 | 83 | 0.82 | `new`, `default`, `grant_session`, `check_shell_allowed`, `check_url_allowed` |
| 96 | 72 | 0.90 | `new`, `run`, `always_approve`, `run_shared_agent`, `new_action_required` |
| 97 | 61 | 0.81 | `insert`, `make_entry`, `select`, `search`, `mock_profiles` |
| 109 | 59 | 0.81 | `path`, `new`, `tool_and_dir`, `execute`, `session_for` |
| 384 | 56 | 0.87 | `default`, `new`, `count_tokens`, `build_chat_body` |
| 884 | 50 | 0.95 | `new`, `make_action`, `eval_cond`, `evaluate`, `empty_input` |
| 87 | 48 | 0.90 | `by_name`, `new`, `view`, `route_event` |
| 227 | 43 | 0.80 | `init_memory_system`, `test_session`, `index_file_stores_chunks…` |
| 187 | 39 | 0.79 | `new`, `default`, `from`, `validate_project_dir` |
| 162 | 38 | 0.92 | `new`, `push`, `run_concurrent_increments`, `get` |
| 86 | 36 | 0.63 | `write`, `register_host_functions`, `read`, `virtual_fs_reject_hunk` |

Clusters 884 (eval/condition engine), 162 (concurrent state), and 87 (router/view) are the most internally cohesive (≥0.90), indicating well-bounded subsystems. Cluster 86 (virtual_fs + host fns) is the loosest (0.63) — expected, since it bridges the plugin ABI to the tool layer.

### 6. Entry points

11 `main` functions exist across binaries and examples: `api-server`, `cli`, `concerto`, `core` (event_loop_demo), `desktop`, `eval-runner`, `eval`, and the four `eval/benchmark_tasks/*/src/main.rs` task harnesses.

### 7. HTTP routes (api-server / axum)

- `GET /health`, `GET /healthz`
- `GET /openapi.json`, `GET /v1/openapi.json`, `GET /v1/docs`
- `ANY /sessions`, `/sessions/{id}`, `/sessions/{id}/tasks`, `/sessions/{id}/tasks/{tid}/stream`, `/sessions/{id}/spend`
- `ANY /metrics`
- `ANY /todos`, `/todos/:id`

## Consequences

### Positive

- Objective, reproducible baseline of the architecture as it exists today, decoupled from tribal knowledge.
- Identifies `tools` (and `VirtualFs` specifically) as the highest-leverage crate/type for review priority.
- Pinpoints `core` + `tools` as the stable foundation that other crates should continue to build on.

### Negative

- A snapshot freezes a moment in time; it will drift. Re-run the indexer (full mode) and regenerate before major refactors.
- Graph layer inference is heuristic; "entry" vs "internal" labels reflect call-direction only, not intent.

### Risks

- The hotspots table over-indexes on `VirtualFs` because the snapshot includes many test helpers (`mock_profiles`, `test_session`). Treat fan-in as relative signal, not absolute mandate.
- `tools` being the universal convergence point is both a strength (single source of truth) and a risk (single point of failure / change blast radius). Consider an explicit `tools` stability policy in a follow-up ADR.

## Related ADRs

- ADR-14 / ADR-21: Plugin architecture and WASM host ABI (explains `plugins → tools` boundary and cluster 86).
- ADR-22: Hybrid retriever RRF (explains `memory` ranking internals, cluster 227).
- `docs/architecture.md`, `docs/crate-graph.md`: human-authored architecture docs — this ADR is the machine-derived complement.
