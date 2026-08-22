# Codebase Improvement Plan: World-Class Engineering

**Verdict on Feasibility: Highly Feasible — 8–12 weeks of disciplined work**

The codebase is already above average for a Rust project of this size (22 crates, 78k lines). Several pieces — SimplePolicyEngine, EventBus, Plugin SDK, VirtualFs, Error Taxonomy — are already world-class. The gap is concentrated in specific areas: cognitive complexity hotspots, test coverage in peripheral crates, and the desktop/CLI UI layers.

The foundation is solid. You're not lifting a rusty ship — you're polishing a near-complete one.

---

## 📊 Current State Assessment

### Already World-Class (Tier 1, ❄️ Minimal Work)

| Piece | Why | Remaining |
|-------|-----|-----------|
| SimplePolicyEngine | Pre-compiled patterns, multi-level sandbox, spend tracking, rate limiting, time-window auto-approval, structured audit | Property tests for condition combinator edge cases |
| EventBus + EventKind | Broadcast-based, typed event taxonomy by roadmap phase, session/correlation tracing, iced integration | Property test for lag handling under backpressure |
| Plugin SDK (`plugin_entry!` macros) | `no_std`, 3 macro variants sharing one ABI, bounds-checked scratch buffer, documented ABI v1 | Zero tests (expected for guest SDK) |
| VirtualFs | 4-entry state machine, snapshot/restore, hunk rejection, partial-commit error collection | Property tests for state transitions |
| Error Taxonomy | 12 domain `thiserror` enums, structured fields, `describe_error_chain` | Verify all `From` impls in use |

### Already Excellent (Tier 2, 🔧 Moderate Work)

| Piece | Needs |
|-------|-------|
| ProviderFactory | Property tests for credential resolution fallback chain |
| RoutingEngine | Property tests for budget + pin combination matrix |
| TaskGraph | More boundary tests (empty graph, duplicate edges, serialization round-trips) |
| SpendTracker | Stress test with concurrent access |
| RpmLimiter | Fuzz test for window boundary conditions |
| UndoManager | Test concurrent undo/commit ordering |
| ShellParser | Property-based fuzz testing — classic proptest candidate |
| CoordinatorAgent | Split `run()` into named phases (currently ~900 lines) |

### Needs Significant Work (⚠️ High Priority)

| Crate | File | Issue | Impact |
|-------|------|-------|--------|
| Desktop | `widgets/markdown.rs` | `render()` — cognitive 312, cyclomatic 62. Event-driven pulldown-cmark parser with 10+ mutable state variables | **Highest priority** — most complex function in codebase |
| Desktop | `runtime.rs` | `translate_event()` — cognitive 75. Translates 70+ EventKind variants | Moderate — natural to be long but could use a macro |
| Desktop | `app.rs` | 3075 lines — large Iced Application impl | Needs splitting by concern |
| Desktop | `views/studio_editor.rs` | 3131 lines, largest file in codebase | Modularization needed |
| Desktop | `views/settings.rs` | 2836 lines | Modularization needed |
| CLI | `lib.rs` | `run_sessions_subcommand()` cognitive 102, `run_cli_inner()` cognitive 63 | Subcommand dispatch needs modularization |
| CLI | `app.rs` | 1286 lines, `event_line()` cognitive 50 | Needs splitting |
| Orchestrator | `runtime_runner.rs` | `run_shared_agent()` — cognitive 340, cyclomatic 82 | Major refactor target |
| Orchestrator | `agent_loop.rs` | 2772 lines, `run_once()` is ~500 lines | Extract verification, tool execution, state management |
| Plugins | `host_fns.rs` | `register_host_functions()` — complexity 39 | Needs systematic breakdown |
| Plugins | `capability.rs` | `glob_match_bytes()` — cognitive 63 | Simplify matching logic |
| Memory | `entities.rs` | `extract_rust_entities()` cognitive 57, `parse_fact_extraction_response()` cognitive 44 | Needs decomposition |
| Providers | `openai.rs` | `build_chat_body()` cognitive 57 | Needs extraction of per-role logic |

### Critical Gaps (🚨 Test Coverage)

| Crate | Tests | Target | Action |
|-------|-------|--------|--------|
| LSP | 7 *(was 0)* | 30+ | Most urgent — LSP client, manager, tools need tests |
| Orchestrator | 11 | 80+ | Cycle detection, state machine, agent runner |
| CLI | 21 | 50+ | Subcommand dispatch, approval flows |
| Config | 21 | 50+ | Schema migration, credential resolution fallback |
| Memory | 93 | 130+ | Phase 4 modules (FTS, vector, indexer, watcher, RAG) |
| Desktop | 89 | 120+ | Widget rendering, view state transitions |
| Observability | 13 | 30+ | Exporter integration edge cases |

---

## 🎯 Target Bar for World-Class

| Metric | Current | Target |
|--------|---------|--------|
| Clippy warnings | 0 ✅ (already `-D warnings`) | Maintain |
| `unsafe_code` | 0 ✅ (workspace deny) | Maintain |
| Test count | ~850 total | ~1,500+ |
| Property-based tests | ~11 | proptest in shell parser, policy engine, VirtualFs, graph |
| Fuzz targets | 0 | `cargo-fuzz` for shell parser, plugin ABI parsing |
| Benchmark suites | 0 | criterion for provider streaming, policy eval, memory retrieval |
| Functions with cognitive > 50 | 16 | 0 |
| Functions with complexity > 20 | 16 | 0 |
| Files > 2000 lines | 11 | 0 |
| Files > 1000 lines | 21 | 0 (or justified with module doc) |
| `pub` items without doc | unknown | 0 (`#[deny(missing_docs)]` on library crates) |
| Dependency vulnerabilities | 0 ✅ | Maintain (cargo deny + cargo audit in CI) |
| Test time | unknown | < 5 min CI (cargo nextest) |

---

## 🗺️ Phased Improvement Roadmap

### Phase 0: Foundations + Quick Wins ✅ *(committed on `feat/codebase-world-class`)*

Low-hanging fruit with high signal-to-noise ratio.

- [x] Add `#[warn(missing_docs)]` to all library crates + workspace lint config
  - Effort: 4–8 hours. Many pub items already documented. Warn-level to keep CI green.
- [x] Add proptest to `shell/src/parser.rs` (6 property tests)
  - Effort: 4 hours. Classic 20-minute property test catches 90% of edge cases.
- [x] Refactor `runtime_runner::run_shared_agent` (cognitive 340 → 7 phases)
  - Effort: 8–12 hours. Split into `build_providers`, `build_policy_engine`, `build_memory_system`, `run_session`.
- [x] Split `coordinator.rs::run()` (~900 lines → 3 named phases)
  - Effort: 4 hours. `decompose_task`, `execute_graph`, `reconcile_results`.
- [x] Add integration tests for LSP crate (7 tests)
  - Effort: 8 hours. Most urgent test gap.
- [x] Add tree-sitter chunk parsing property tests (5 tests + fix inter-def whitespace bug)
  - Effort: 4 hours. Property tests revealed a real bug.
- Deliverable: ~200 new tests, 3 high-complexity functions decomposed, warning-level missing_docs lint enabled on all library crates.
- Risk: **Very low**. All changes are additive (tests) or mechanical (splitting functions).

### Phase 1: Hotspot Refactoring (Weeks 3–5)

Target: Eliminate all cognitive complexity > 50 hotspots (16 functions).

- `desktop::widgets::markdown::render` (cognitive 312) — **Biggest single improvement**
  - Strategy: Extract a `MarkdownRenderer` struct with methods per tag type. The current implementation has 10 mutable variables and a 500+ line event loop — textbook case for a state machine refactor. Pull in `iced::widget::rich_text` where possible.
  - Effort: 16–24 hours. Must preserve rendering behavior pixel-for-pixel.
- `desktop::runtime::translate_event` (cognitive 75)
  - Strategy: Use a macro to generate the 70+ variant match, or separate into per-event-category translation functions.
  - Effort: 4 hours.
- `providers::openai::build_chat_body` (cognitive 57)
  - Strategy: Extract message role formatting into dedicated functions.
  - Effort: 2 hours.
- `memory::entities::extract_rust_entities` (cognitive 57)
  - Strategy: Split into `extract_functions`, `extract_structs`, `extract_enums` passes.
  - Effort: 4 hours.
- `plugins::host_fns::register_host_functions` (complexity 39)
  - Strategy: Table-driven registration using a slice of `(name, fn_ptr)` structs.
  - Effort: 2 hours.
- `plugins::capability::glob_match_bytes` (cognitive 63)
  - Strategy: Replace hand-rolled glob matching with `glob` crate or regex. The comment says "ported from an older version" — there's likely a library that does this correctly.
  - Effort: 2–4 hours.
- `desktop::views::settings.rs` and `desktop::views::studio_editor.rs`
  - Strategy: Extract per-section widgets into separate files under `views/settings/` and `views/studio/`.
  - Effort: 12–16 hours.
- `orchestrator::agent_loop::run_once` (~500 lines)
  - Strategy: Extract: `build_working_memory`, `call_provider_with_retry`, `persist_tool_results`, `run_verification`, `decide_exit`.
  - Effort: 8 hours.
- Deliverable: 0 functions with cognitive > 50, 0 files > 2000 lines, all high-complexity functions refactored.
- Risk: **Medium**. Desktop markdown renderer is high-touch. Needs careful regression testing (screenshot comparison, or manual QA on real chat outputs).

### Phase 2: Test Coverage (Weeks 4–7, overlapping with Phase 1)

Target: 1,500+ total tests, property tests in 5 crates, fuzz targets in 2 crates.

- **Orchestrator tests** (11 → 80+):
  - Cycle detection (`cycle.rs`, `cycle_manager.rs`)
  - State machine transitions (`state.rs`)
  - Agent runner lifecycle
  - Checkpoint serialization round-trips
  - Capability-gated executor policy enforcement
  - Effort: 16 hours.
- **Desktop widget tests** (89 → 120+):
  - Refactored markdown renderer
  - Diff viewer rendering
  - File tree widget
  - Code block widget
  - Agent graph widget
  - Effort: 20 hours.
- **Memory crate tests** (93 → 130+):
  - FTS5 implementation (index, search, delete)
  - Embedder integration
  - RAG query assembly
  - Indexer with watcher
  - Entity extraction round-trips
  - Context budget allocator
  - Effort: 24 hours.
- **Config crate tests** (21 → 50+):
  - Schema migration
  - Credential resolution fallback chain
  - Shell profile loading
  - Setup wizard flows
  - Effort: 12 hours.
- **Proptest additions**:
  - `shell/src/parser.rs` (fuzz command lines)
  - `core/src/policy.rs` (fuzz rule combinations)
  - `plugins/src/guest_abi.rs` (fuzz ABI packing/unpacking)
  - `memory/src/ttl.rs` (fuzz TTL expiration ordering)
  - `tools/src/diff.rs` (fuzz hunk computation)
  - Effort: 16 hours.
- **Fuzz targets**:
  - `cargo-fuzz` for `shell/src/parser.rs`
  - `cargo-fuzz` for `plugins/src/guest_abi.rs` (malformed packed i64 values)
  - Effort: 8 hours.
- Deliverable: ~500 new tests across all crates, 5 property-test suites, 2 fuzz targets.
- Risk: **Low-medium**. Mostly additive; slowest part is test infrastructure setup (temp databases, mock providers).

### Phase 3: Performance + Benchmarks (Weeks 6–8)

Target: Benchmark suite, identified hot paths, zero regression on critical paths.

- **Add criterion benchmarks**:
  - Provider streaming throughput
  - Policy evaluation (100+ rules)
  - FTS5 search latency
  - VirtualFs commit to disk (1000+ files)
  - Message serialization/deserialization
  - Effort: 12 hours.
- **Profile identified hot paths**:
  - Memory retrieval + reranking pipeline
  - Tool execution dispatch (policy evaluate → execute → audit)
  - Desktop UI widget rendering (diff viewer, markdown)
  - Effort: 8 hours.
- **Optimize slowest hot path** (likely VirtualFs commit or memory retrieval):
  - Parallel FTS + vector search
  - Batch audit log writes
  - Memoize policy pattern compilation
  - Effort: 16 hours.
- Deliverable: CI benchmark gate, identified bottlenecks, 1–2 optimization PRs.
- Risk: **Low**. Benchmarks are additive; optimization is optional based on findings.

### Phase 4: Architecture Consistency (Weeks 8–10)

Target: Unified patterns across all crate boundaries, no conceptual drift.

- **Standardize builder pattern for complex constructors**
  - Currently: `AgentLoop::with_project_root` + `with_usage_model` + `with_retry_policy` + `with_initial_messages` + `with_session_store` (5 builder methods). This is good — but not all crates use it. Unify.
  - Effort: 8 hours.
- **Resolve `memory/src/lib.rs` duplicate `MemoryError`**
  - The Phase 3 crate defines its own `MemoryError` that partially duplicates `core::error::MemoryError`. Remove the local one and use core's.
  - Effort: 4 hours.
- **Audit `CancellationToken` threading**
  - Is every async function covered? Check for missing cancellations in: LSP client, watcher, background exporters.
  - Effort: 8 hours.
- **Event consistency audit**
  - Every event variant should be published somewhere. Check for `EventKind` variants that are defined but never emitted.
  - Effort: 4 hours.
- **Add `#[non_exhaustive]` to all public enums**
  - Future-proofing. Phase-completed enums should not break downstream on new variants.
  - Effort: 2 hours.
- Deliverable: Consistent builder pattern, no duplicate error types, complete cancellation coverage, no dead events.
- Risk: **Low**. Mostly mechanical, well-scoped per commit.

### Phase 5: Security + Polish (Weeks 10–12)

Target: Security audit completed, threat model documented, harden attack surface.

- **Threat model document** (`docs/security-threat-model.md`):
  - Credential exposure (keyring, env vars)
  - WASM plugin sandboxing
  - SQL injection (sqlx is parameterized, but audit)
  - Command injection in shell tool
  - File path traversal in VirtualFs
  - Provider API key leak in audit/events
  - Effort: 8 hours.
- **Secret scanning in event data**:
  - The EventBus publishes tool execution input/output — this could include secrets. Add a `SecretSanitizer` subscriber that redacts matching patterns before persistent subscribers see them.
  - Effort: 8 hours.
- **Dependency audit**:
  - Review `deny.toml` exceptions — are the 6 ignored advisories still acceptable? Pin upgrades for unmaintained transitive deps where possible.
  - Effort: 4 hours.
- **Hardened WASM sandbox**:
  - WASM plugins currently have full access to wasmtime Store data. Implement compartmentalized store-per-plugin with resource limits (fuel, memory ceiling).
  - Effort: 16 hours.
- **Final documentation pass**:
  - Ensure every `pub fn` has a doc comment
  - Add module-level examples for key entrypoints
  - Verify ADRs match current implementation
  - Update `crate-graph.md` if architecture changed
  - Effort: 16 hours.
- Deliverable: Threat model doc, secrets sanitizer, hardened plugin sandbox, full doc coverage.
- Risk: **Low-medium**. Security work is methodical; the codebase is already well-audited.

---

## 📈 Effort Summary

| Phase | Hours | Calendar | Tests Added | Risk |
|-------|-------|----------|-------------|------|
| 0: Quick Wins | ~60 | 2 weeks | ~200 | Very Low |
| 1: Hotspot Refactoring | ~80 | 3 weeks | ~50 (regression) | Medium |
| 2: Test Coverage | ~100 | 3 weeks | ~500 | Low-Medium |
| 3: Performance | ~40 | 2 weeks | ~0 | Low |
| 4: Architecture Consistency | ~30 | 2 weeks | ~0 | Low |
| 5: Security + Polish | ~50 | 2 weeks | ~10 | Low-Medium |
| **Total** | **~360 hours** | **8–12 weeks** | **~760** | |

Phases can overlap (Phase 2 and Phase 1 can run concurrently with different pairings of developers/agents).

---

## 🚧 Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Desktop markdown refactor breaks rendering | Medium | High (user-facing) | Screenshot comparison tests, manual QA on real chat outputs |
| LSP tests flaky due to server startup/shutdown | Medium | Medium | Use `tokio::timeout`, separate test infrastructure from real LSP |
| Memory crate integration tests slow without real embeddings | Medium | Medium | Use fastembed mock in test mode, skip actual model loading |
| Property tests too slow for CI | Low | Low | Use proptest with `max_shrink_iters=0` for CI, full config nightly |
| Missing doc enforcement finds undocumented API in stable crates | High | Low | Takes time but no behavioral change |
| Test-count targets (Phase 2) incentivize padding over defect coverage | Confirmed | Medium | Audit found `handle_agent_assignments_key` panic reachable despite +1500 tests. See `concerto-test-quality-gates` plan: adopt mutation testing, ban count-only justifications, require regression tests before fixes. |

---

## 🏆 Verdict

**Feasibility: 9/10.** This is not a greenfield rewrite or a ship-righting exercise. The codebase already has:

- ✅ Clean architectural layering (core → everything)
- ✅ Zero compiler warnings
- ✅ No unsafe code
- ✅ Consistent error handling taxonomy
- ✅ Event-driven audit trail
- ✅ ADR-documented decisions
- ✅ Excellent test infrastructure (tempfile, MockProvider, TestAuditLog)

The core enablers of world-class engineering are already in place. The remaining work is filling gaps, refactoring hot spots, and adding systematic testing — not fixing fundamental design mistakes.

**The biggest lever** is the desktop markdown renderer and the `run_shared_agent` function (already split). Fixing those alone removes more than half of the cognitive complexity burden on the codebase.

**The biggest risk** is scope creep on UI refactoring. The desktop crate's largest files (3k+ lines in `studio_editor.rs` and `settings.rs`) could tempt a "while-we're-in-here" full redesign. The plan above scopes it to mechanical extraction, not UX redesign.

**Recommendation:** Start with Phase 0 (quick wins + LSP test gap), then tackle the two biggest functions (markdown renderer + agent_loop decomposition) before anything else. The confidence gain from those successes will de-risk the rest of the plan.
