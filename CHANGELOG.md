# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Always-on write-conflict detection (ADR-60 D5):** versioned gated writes
  now carry per-target `base_versions` claims — `GateRequest.base_version`
  (single) became a `base_versions` map with `#[serde(default)]` wire
  back-compat and `versioned_targets()` as the single source of truth (move
  claims source+destination, copy claims destination only, copy source
  attributed-but-not-claim-stamped). The supervisor stamps claims at request
  arrival (`stamp_base_versions`), so mismatches surface as retryable tool
  errors (`IpcErrorCode::Conflict` `-32005`) with zero whiteboard rows instead
  of silent last-writer-wins; the legacy single-agent loop now runs through an
  in-process `WriteGate` (`InProcessGateBackend`) with the same semantics,
  falling back to the plain executor with a `warn!` when the session DB pool
  is unavailable. Fresh-create concurrent creates remain last-writer-wins by
  design.
- Configurable multi-agent relationship manager with validated directed rules,
  dependency-ready task scheduling, and per-role provider/model assignments
- Integrated desktop terminal and canonical shell profiles shared by agents,
  validation, and terminal execution
- Host shell discovery plus explicit custom profiles; removed unavailable
  placeholder presets and duplicate discovered-shell UI
- `concerto-shell` typed command runtime foundation with structured results,
  recoverable statuses, context/effect metadata, read-only built-ins, and
  policy-gated external execution adapters
- Categorized evaluation tasks for bug fixes, libraries with tests, CLI tools,
  and small web APIs
- Public test report/checklist and reconciled provider, multi-agent, policy,
  shell, architecture, status, security, and roadmap documentation
- Status-first desktop session UX with quick-start prompts, resumable recent
  sessions, structured multi-agent phase rows, authoritative completion cards,
  compact agent assignments, project Git status, and real view switching
- Plugin capability grant lifecycle (ADR-37): 30-day grant expiry, SHA-256 WASM
  binary hash pinning, and revocation via `concerto plugin list|revoke` and the
  desktop Settings > Plugins section
- `concerto sessions prune` — maintenance-only, opt-in session pruning: requires
  `--older-than <days>` and keeps the newest `--keep <n>` sessions (default 5),
  with `--dry-run` preview and `--all-projects` (default scopes to the current
  project); sessions mapped active are always skipped; the session and its
  dependent rows (messages, events, spend, tasks, checkpoints, transcript) are
  deleted in one transaction, printing exactly what it did; the audit-log
  decision trail is preserved (ADR-40) — its `session_id` link is detached via
  `ON DELETE SET NULL` (migration 021) instead of the rows being deleted

#### Phase 9: Provider Additions & Polish
- OpenCode Zen provider implementation (7th LLM provider) — streaming completions,
  tool-call normalization, credential config via keyring
- Centralized retry/backoff infrastructure — exponential backoff + jitter,
  retry-after handling, cancellation, and an optional elapsed-time fuse
- Agent-loop robustness fixes: cycle detection edge cases, cancellation path hardening,
  budget-aware dispatch refinements
- Desktop screenshot capture feature
- WASM tool plugin Phase 7.1 closure & hardening (capability scoping, shell_exec host function)
- Prompt override UI and advanced settings sections in desktop
- Pre-release remediation: token accounting, memory wiring, CLI subcommands
- **Plugin capability grant lifecycle (ADR-37):** grants expire after 30 days,
  pinned to SHA-256 hash of the approved WASM binary; legacy grants get 24-hour
  migration window. CLI `plugin list` / `plugin revoke` subcommands. Desktop
  Settings Plugins section with revocation UI.
- SHA-256 manifest hash pinning for WASM plugins — binary change invalidates grants
  and forces re-approval on next load.

#### ADR-35 Phase 4: Pipeline Topology & Capability Gating
- `CustomAgentConfig.disabled` flag for runtime topology control: disabled
  built-in specialists and custom agents are absent from the registry and
  from provider/model resolution (deterministic topology: coordinator, then
  built-ins, then enabled custom agents)
- `AgentCapabilities.eval` capability gating the Validator's eval engine,
  with upgrade-safe defaults (existing configs keep validation enabled) and
  a fail-fast "validation disabled" error when off
- Provider/model resolution and tool-calling routing now follow the
  configured runtime topology instead of a hardcoded role list; routing
  accepts a per-role tool-calling set via `with_tool_calling_roles`
- Config entries targeting the Coordinator (constructed in code only) are
  surfaced as load-time warnings instead of silently dropped
- Orchestration Studio: Disabled and Eval Engine toggles per agent, plus a
  validator guardrail warning when eval is off

#### Orchestration Studio UI overhaul
- Pipeline view rebuilt as a real graph canvas: agents as nodes, labeled
  hand-off edges, a collapsed "+ Add hand-off" form, and section_card groupings
- Library gains a collapsed "+ Add agent" form; disabled agents show a
  "Disabled" badge in the list and are dimmed on the pipeline graph; the
  Coordinator shows a "protected" badge in the inspector
- Permissions pane presets are styled and show a "Current preset" caption; the
  toolbar "Load preset…" pick-list is the single reset entry point (the
  duplicate "Reset to standard" control was removed)
- RemoveAgent now guards the Coordinator id at the message level, with a
  regression test; save success/failure feedback moved from inline toolbar
  text to App-level toast notifications

#### Spend surfaces: status-bar chip, Spend Log modal, per-call records (ADR-41)
- Live session spend in the status-bar chip, palette-colored at ≥80% (warning)
  and ≥100% (danger) of the session cap
- Spend Log modal (SubView overlay) listing persisted per-call spend records
- The runtime now publishes the existing `SpendUpdated`/cap events after each
  settled provider call and persists one `SpendRecord` per call (multi-agent
  records carry the root task id)
- Daily-total spend output stubbed (field present, always `None`) until daily
  tracking is enabled

### Fixed
- Orchestration Studio no longer rebuilds its graph, loads a tokenizer, or
  writes the full configuration during every UI edit; configuration is saved
  only on explicit request, with visible dirty/success/failure state
- Orchestration Studio now provides bounded, scrollable agent and pipeline
  views; functional relationship add/edit/delete controls; pre-mutation cycle,
  duplicate, and endpoint validation; and synchronized per-agent model choices
- Multi-agent Coordinator now respects task dependencies instead of dispatching
  every specialist at once
- Multi-agent fault containment: specialist provider calls now use configured retry/backoff;
  Coder tool failures are returned to the model for correction instead of immediately
  aborting the run; recoverable subtasks are retried with failure context; exhausted
  recovery is reported as an actionable blocker rather than `INTERNAL_ERROR`
- Multi-agent validation now preserves the selected project context and re-runs validation
  after automatic Coder repairs instead of returning after the first repair attempt
- Filesystem reads now identify missing paths and tell the agent how to create them;
  network-isolated policy profiles permit local shell commands while still blocking
  network-reaching commands
- Token accounting fix for streaming mode (MeteredProvider counts per-chunk delta)
- Memory wiring — working memory snapshot population from real store data
- Desktop gaps from Phase 6 audit: diff viewer hunk Accept/Reject, virtualized diff rows,
  agent graph pan/zoom, markdown rendering (tables, blockquotes), session list wiring
- CLI startup, project root resolution, policy denial handling, subcommand routing
- Shell allowlist escape vulnerability, timing attack in auth, URL parser
- Session-scoped capability grant persistence, text-input focus tracking
- Theme selection persistence across restarts
- EventBus AssistantMessage routing through CLI and desktop
- **Policy engine hardening** (`concerto-core`): precompiled glob regexes cached per
  pattern (no per-evaluation recompilation); structured audit decision recorded at all
  four verdict sites (sandbox, spend, rule, default-deny) for complete audit coverage;
  `evaluate` uses a single `chrono::Utc::now()` timestamp; `validate()` now fails fast on
  uncompilable `CommandPattern`/`SecretPattern` regexes; `ReadOnlyFs` and `NetworkIsolated`
  sandbox heuristics use whole-word matching (no false positives on paths/words like
  `my_http_notes.txt` or `overwrite`); invalid globs are logged instead of silently
  skipped during precompilation
- **Shell tool (`concerto-tools`)**: `ShellConfig` gains an explicit `allow_all` flag;
  `validate_command()` consults the denylist before any allow-all bypass, and
  `ShellConfig::allow_all()` no longer uses `expect()`
- Multi-agent and policy execution now share one spend tracker rather than
  maintaining independent accounting paths
- **Mutex poisoning (production `unwrap()` elimination):** 11 `lock().unwrap()`
  and `.last().unwrap()` calls removed across `coordinator.rs` (4),
  `capability_dialog.rs` (4), `app.rs` (2), `ui.rs` (1), `circuit_background.rs` (1)
  — replaced with `unwrap_or_else` / `unwrap_or` / `if let`.
- **Git path validation:** all git operations now use canonicalized `resolved`
  path after the security boundary check, preventing symlink traversal.
- **Git commit hook quoting:** commit-message punctuation no longer causes
  shell interpretation errors in git hooks (desktop commit dialog).
- **Memory re-index watcher debounce:** 1s debounce on the file watcher with a
  bounded, deduplicated queue and a dropped-hint counter plus rate-limited
  warning on overflow, instead of unbounded queue growth.
- **Size-bounded application log rotation** (`concerto-cli`): rotating app log
  writer with 5 MiB max per file and 2 backups, fail-safe to stderr;
  `concerto logs show` is unchanged.
- **Embedder degradation handling (ADR-39):** when local embeddings fail, no
  zero-vector rows are written on any vector backend (including LanceDB under
  `--features lancedb`); per-project health with exponential backoff (5s→120s);
  semantic search degrades to full-text-only with a user-visible notice (new
  `EmbedderDegraded` event rendered in the CLI) and recovers automatically.
  Also fixes pre-existing LanceDB feature drift (five store methods missing
  `CancellationToken`) and a `spend_summary` COALESCE integer-affinity bug that
  would 500 the `GET /sessions/{id}/spend` API endpoint.
- **Bounded, observed durable event-bus backlog** (`concerto-core`): durable
  subscribers (spend tracking, audit, replay) stay lossless but warn at 4096
  pending events and latch a lag health flag at 65536, surfaced via
  `EventBus::durable_health()`; dead subscribers are pruned on publish and
  subscribe; a one-time warning fires above 32 subscribers.
- **WASM host-function async** (ADR-38, `concerto-plugins`): wasmtime async
  support so host calls no longer block the plugin executor; epoch-deadline
  math corrected so WASM interruption actually fires, and capability-grant
  TTL is enforced per call with a runtime revocation signal
- **Stream and I/O robustness** (`concerto-orchestrator`, `concerto-providers`,
  `concerto-tools`): agent-loop/summarizer streams bounded with first-byte and
  idle timeouts (never retrying after output); lossless UTF-8 streaming decode
  with a WHATWG-compliant SSE parser; git CLI fallback bounded with
  timeout/cancellation and process-group kill on abort

### Security

- Shell `allow_all` mode can no longer bypass the denylist, and its construction no
  longer relies on `expect()` (removed a startup panic path)
- Plugin capability grant lifecycle (ADR-37): 30-day TTL prevents indefinite
  approval; SHA-256 hash pinning detects binary tampering; `plugin revoke`
  provides explicit user revocation.

### Changed
- Removed subjective capability-tier routing. Explicit provider/model pins are
  authoritative; unassigned routing uses objective tool support and budget data
- Memory documentation and runtime contract now identify SQLite FTS5/vector
  hybrid retrieval, local fastembed embeddings, and line/sliding-window chunks
- Multi-agent exhausted recovery reports blocked/partial outcomes and preserves
  useful progress instead of converting ordinary failures to `INTERNAL_ERROR`
- Iced upgraded from 0.13 to 0.14 (MSRV 1.85 → 1.88)
- Cargo workspace formatting applied across all crates
- Mainline integration of remote feature branches (multi-agent wiring, OpenCode Zen provider, live-test routing/control) into `main`
- `architecture.md` reconciled with 22-crate workspace (was 20), updated plugin
  section to reflect provider/memory adapter plugin support.
- Dashboard page removed (ADR-41): recent sessions now live in Chat's empty
  state; the Dashboard's token-tracking charts and CSV export are gone from
  the desktop UI (the CLI remains the file-export path); the Ctrl+Shift+S
  shortcut is removed
- `crate-graph.md` updated to 22-crate diagram with `test-provider-plugin-wasm`
  and `test-adapter-plugin-wasm` nodes.
- Documentation rework: ADRs renamed to uniform `ADR-NN.md` filenames with
  normalized headers and stale content updated (LanceDB feature gate, Iced 0.14,
  async WASM host functions implemented, superseded statuses); `ROADMAP.md`
  rewritten with the ADR index extended through ADR-40; new `docs/TODO.md`
  pending-work ledger; resolved audit/spotcheck reports retired and historical
  implementation notes archived; `STATUS.md`, `architecture.md`, `README.md`,
  `desktop-cli-parity.md`, `STUB-FINDINGS.md`, and `hybrid-ui-plan.md`
  reconciled with the source tree.

## [0.1.0-alpha] - 2026-07-04

### Added

#### Phase 8: Observability & Evaluation
- Prometheus metrics endpoint (`/metrics`) with counter/histogram/descriptions
- EventBus subscriber in observability exporter processing 6 event kinds
- `ObservabilityManager` coordinating Prometheus exporter lifecycle
- 10-task standard benchmark suite (task1–task10) in `concerto-eval`
- `EvalRunner` with AgentFactory pattern (avoids circular dep with orchestrator)
- Regression detection with 5% threshold in `cargo-eval --baseline`
- 3 multi-agent benchmark scenarios (add-feature, refactor-module, bugfix-hidden-cause)
- `cargo-eval` CLI binary with `--suite` and `--baseline` flags

#### Phase 8: API Stabilization
- utoipa OpenAPI annotations on all API routes
- `/v1/` route prefix with legacy redirect (301)
- `GET /openapi.json` serving generated OpenAPI 3.1 spec
- Swagger UI at `/v1/docs` (gated behind `CONCERTO_API_DOCS=1`)
- Bearer token auth middleware with constant-time comparison
- API key enforcement on non-localhost binds

#### Phase 8: Types & Events
- `ObservabilityError`, `SandboxError`, `EvalError` error types
- Phase 8 `EventKind` variants: observability, eval, API, sandbox, auto-update
- `CostInfo`, `EvalTask`, `BenchmarkResult`, `BenchmarkReport`, `SuiteResult` types
- `SandboxProfile` enum (None, ReadOnlyFs, NetworkIsolated, Containerized)
- `BenchmarkMetric` enum with regression tracking metrics

#### Phase 8: Config & Packaging
- `ObservabilityConfig` with `prometheus_port`, `service_name` fields
- `UpdatesConfig` for startup update check configuration
- Schema migration support for v3 config format
- Release workflow building `.tar.gz` for ubuntu + macos (strip debug symbols)
- Non-blocking startup update check in CLI

#### Phase 7: WASM Plugin System
- WASM plugin types, manifest format, capability system
- Plugin manager with lifecycle management (load, instantiate, drop)
- Capability approval dialog in desktop UI
- Guest SDK (`concerto-plugin-sdk`) for writing WASM plugins
- `shell_exec` host function with `CapabilityScope` enforcement
- Integration tests for plugin loading and execution
- Example plugin (`test-plugin-wasm`) demonstrating tool plugin pattern

#### Phase 6: Rich Desktop UI (Iced 0.14)
- Full-featured markdown chat with GFM table and blockquote rendering
- Syntax-highlighted diff viewer with per-hunk Accept/Reject
- Agent DAG visualization with pan/zoom
- Tool activity log (real-time event subscription)
- Memory explorer with entity and fact browsing
- Settings UI with theme selection, provider config, multi-agent toggle
- Dashboard with token tracking, spend CSV export, charts
- Session list wired to real SessionStore data
- Screenshot capture feature

#### Phase 5: Multi-Agent Orchestration
- `CoordinatorAgent` with 5 specialist agents (Architect, Researcher, Coder, Reviewer, Validator)
- TaskPlanner with hierarchical task graph decomposition
- Write gates preventing specialist cross-talk in shared memory
- Cycle detection with configurable max iteration budget
- Budget-aware routing engine with per-role provider assignment
- Review/validation loops with configurable max cycles
- Multi-agent config schema with spend cap multiplier
- `AgentContext` with working memory, retrieved chunks, budget tracking

#### Phase 4: Layered Memory
- Local retrieval pipeline with line/sliding-window chunking, `fastembed`
  embeddings, SQLite vector storage, and FTS5 hybrid search via reciprocal-rank
  fusion. (Earlier roadmap text named tree-sitter/LanceDB; those were not the
  active implementation.)
- Hybrid search combining FTS + vector similarity (RRF, k=60)
- Entity extraction with LLM summarization
- Fact extraction with language-specific patterns
- Re-index watcher for file system changes
- Staleness detection for embeddings
- Working memory snapshotting for agent context
- Cross-project isolation (vector leakage prevention)
- Migration support with schema version tracking

#### Phase 3: MVP Agent Loop
- Single-agent `AgentLoop` orchestrator
- Basic session persistence (SQLite `SqliteSessionStore`)
- Context budgeting with `TokenBudget` tracking
- `EvalEngine` with test runner auto-detection (Cargo, npm, pytest, make)
- First-run setup wizard with provider configuration
- Iced desktop UI (basic chat interface)
- ratatui CLI with keyboard-driven workflow
- Session undo via git stash snapshots

#### Phase 2: Tools & Policy
- `SimplePolicyEngine` with composable rule conditions
- `VirtualFs` overlay for safe file operations
- Shell tool with command allowlisting
- Git tool (clone, status, diff, commit, log)
- Policy presets (default, permissive, strict)
- Time-window-based auto-approval rules
- Spend tracking with per-session USD/token caps
- SpendGuard enforcing budget limits
- Rate limiting per provider
- Audit log (append-only, SQLite-backed)
- Session replay from event stream
- Plugin API types and capability set definitions

#### Phase 1: Core Engine
- `LlmProvider` trait with streaming support
- Provider implementations: OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama
- `ProviderFactory` for multi-provider routing and selection
- Session persistence with CRUD operations
- Token counting with `tiktoken-rs`
- Cost tracking per call/session
- Model routing profiles with cost, latency, and objective compatibility metadata
- Connection testing for all providers

#### Phase 0: Foundations
- Cargo workspace with 18 crates
- `EventBus` with typed event dispatch (broadcast channel)
- `CoreError` taxonomy with domain-specific error types
- `CancellationToken` threaded through all async operations
- Policy engine trait and foundations
- ADR index (22+ Architectural Decision Records)
- CI pipeline (fmt, clippy, test, deny, audit)
- GitHub mirror release workflow and CI scaffolding

### Fixed
- EventBus lag handling with warning logging
- `SimplePolicyEngine::evaluate()` now correctly wires SpendTracker + RpmLimiter
- `TimeWindowCondition` honors configured timezone via `chrono-tz` (was hardcoded to UTC)
- Agent loop state machine transitions for error recovery
- Memory stale embedding detection and re-index queuing
- Browser/SSO login hanging in desktop (timeout with cancellation)
- Desktop session-switching clears stale agent state
- WASM plugin sandbox resource limits (fuel, memory, call depth)
- Vendor Swagger UI assets to eliminate build-time network dependency
- SSE stream uses BroadcastStream for proper event delivery

### Changed
- `ObservabilityExporter` subscribes to `EventBus` instead of polling
- `EvalRunner` uses `AgentFactory` closure pattern (no circular dep on orchestrator)
- Config schema v3 with `ObservabilityConfig`, `UpdatesConfig`, `ModelSettings`
- MSRV raised to 1.88 for `iced` 0.14 and `wasmtime` compatibility
- `PolicyEngine::evaluate()` signature includes `EstimatedCostUsd` for spend cap checking
- Memory embedder uses local `fastembed` inference; provider-native embedding
  endpoints are not integrated

### Removed
- Stub/dead code across multiple crates identified in pre-release audit
- Comment-only placeholder widget modules in desktop

### Known Issues
- OpenTelemetry OTLP, Langfuse HTTP, and Prometheus metrics exporters are all implemented
  and version-aligned; enable via `ObservabilityConfig`
- Plugin system: only tool plugins are supported; Provider and MemoryAdapter
  plugin descriptors reserved for later phases
- WASM sandbox not yet tested against path traversal, shell injection, or URL bypass attacks
- Memory uses local `fastembed`; provider-native embedding endpoints are not
  integrated
- LSP client/tool abstractions exist but are not registered by default runtimes
- No `.deb`/`.rpm` packaging — only `.tar.gz` binary distribution

## [0.0.0] - 2026-06-01

### Added
- Initial project scaffolding and workspace setup
