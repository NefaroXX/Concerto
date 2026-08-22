# ADR-19: Multi-Agent Orchestration

**Status:** Accepted — routing portion superseded by [ADR-31](ADR-31.md) (model-first selection); remainder remains accepted
**Date:** 2025-09-02
**Deciders:** Concerto architecture
**Phase:** 5  

> **Current implementation note (2026-07-19):** Specialist tasks execute from
> a validated dependency DAG and only dependency-ready tasks are batched.
> Relationships are configurable through `MultiAgentConfig.relationships`.
> LSP tool wrappers exist but are not registered in the default specialist
> runtime. Treat capability tables and fixed-pipeline text below as historical
> design context where they conflict with those facts.

## Context

Phase 5 introduces opt-in multi-agent orchestration: a `CoordinatorAgent` decomposes tasks into subtasks and delegates them to specialist agents (Architect, Researcher, Coder, Reviewer, Validator). This ADR documents the architecture decisions behind that system.

## Decision

### 1. Opt-in, not default-on

Multi-agent mode is controlled by a `--multi-agent` CLI flag, a desktop toggle (via `config.multi_agent.default_enabled`), and the per-session `multi_agent` field. The single-agent path (Phase 3 `AgentLoop`) remains the default and must not regress.

**Rationale:** Multi-agent orchestration adds latency and cost. For simple tasks ("fix this typo", "explain this function"), a single agent is faster and cheaper. The user opts in when the task is complex enough to benefit from specialisation.

### 2. Agent role responsibilities

| Role | Capabilities | Output |
|---|---|---|
| **Architect** | Read-only filesystem, LSP, RAG retrieval | `DesignDoc` — goals, constraints, file list, interface sketch, risks |
| **Researcher** | Read-only filesystem, LSP, RAG retrieval, entity/fact queries | `ResearchReport` — relevant files, code snippets, facts, unknowns |
| **Coder** | Full filesystem (write), shell, git, LSP for diagnostics | File changes via `VirtualFs` + `ToolExecutor` |
| **Reviewer** | Read-only filesystem, git diff, LSP | `ReviewReport` — verdict, issues, suggestions |
| **Validator** | Read-only filesystem, shell (test runner only) | `EvalResult` from `concerto-eval` |

### 3. Shared memory via MemoryStore + EventBus vs. message-passing

**Chosen:** Shared memory via `MemoryStore` + `EventBus`.

**Rationale:**
- Reuses Phase 4 infrastructure (`MemoryStore`, `HybridRetriever`).
- Provides a unified audit trail — every agent action is an `Event` that subscribers (UI, audit log, replay) can observe.
- Message-passing would require building an agent-to-agent protocol, routing layer, and serialisation boundary — all of which duplicate existing event bus functionality.
- The coordinator orchestrates by passing `AgentContext` (with `WorkingMemorySnapshot`) directly to each agent, avoiding the complexity of a message bus.

**Trade-off:** Agents are less decoupled than with message-passing. Mitigated by (a) the `WorkingMemorySnapshot` being immutable at delegation time, and (b) write gates preventing unauthorised mutations.

### 4. Write gates enforced at ToolRegistry/ToolExecutor level

**Chosen:** Read-only agents (Architect, Researcher, Reviewer, Validator) have their `ToolRegistry` constructed with `CapabilitySet::read_only()`. If a write tool is somehow injected into a read-only registry, `ToolRegistry::build` panics in debug and returns an error in release.

**Rationale:** Never trust agent implementations. Structural enforcement at the tool registry level means even a compromised or buggy agent cannot write to disk, run arbitrary shell commands, or mutate git state.

### 5. Serialised long-term memory writes

**Chosen:** `MemoryWriteSerializer` wraps `MemoryStore` with a `tokio::sync::Semaphore(1)`. Only one agent can write to long-term memory at a time. Reads are not serialised.

**Rationale:** Long-term memory (vector store, entity graph) is append-mostly but some operations (entity dedup, fact expiry) are read-modify-write. Without serialisation, concurrent writes from multiple agents could corrupt state. The semaphore is a simple, correct solution. Reads are safe to do concurrently from the underlying store.

### 6. Cycle detection strategy

Two rules:

- **Rule A:** Same `(AgentRole, task_hash)` appears 3× without progress (`FileDeltaTracker` reports no net file change AND no new `MemoryEntry`). Emit `OrchestratorCycleDetected`.
- **Rule B:** `ReviewerAgent` returns the same `Issue.description` hash in 2 consecutive cycles AND `FileDeltaTracker` reports zero net change. Emit `OrchestratorCycleDetected`.

On detection, the coordinator returns `CycleDetected` error. The CLI/desktop offers Continue (resets cycle state), Reset (restart task), or Abort.

**Rationale:** Hardcoded limits (3× for Rule A, 2× for Rule B) rather than configurable thresholds. Simpler to implement and reason about. The continuation mechanism allows the user to override if the agent genuinely needs more cycles.

### 7. Routing strategy

> Routing portion superseded by [ADR-31](ADR-31.md) (model-first selection
> with internal provider routing; archived predecessor [ADR-24](archive/ADR-24.md)).
> Capability tiers and heuristic role ranking are no longer part of Concerto's
> routing design.

The original heuristic routing decision is retained here only as historical
context. ADR-31 replaces it with model-first provider/model pair selection.

### 8. Evaluation loop policy

- **Review loop:** Reviewer critiques Coder output. Coder revises if `NeedsRevision`. Max 3 cycles. Exceeded → `MaxReviewCyclesExceeded`.
- **Validation loop:** Validator runs tests after Coder. If tests fail and cycles < 2, Coder revises with test output. Max 2 cycles. Exceeded → `MaxValidationCyclesExceeded`.

**Rationale:** Hardcoded upper bounds prevent infinite loops. The user is always notified on escalation. These limits are deliberately conservative — they can be increased based on empirical data.

### 9. Crate boundary

**Chosen:** Specialist agents live in `concerto-orchestrator` for Phase 5. Extract to `concerto-agents` in Phase 6 if orchestrator exceeds 10k lines.

**Rationale:** The orchestrator crate is ~1k LOC currently. Premature extraction adds module boundary overhead without benefit. The extract line (10k) is explicit and measurable.

### 10. Task decomposition via LLM planning

The original Phase 5 design used a fixed heuristic pipeline: Architect always runs first, then a Researcher pass is inserted if the design doc has multiple proposed files or risks, and finally a Coder pass. Reviewer and Validator loops run after Coder as needed.

**Revised decision (June 2026):** The `decompose_task` method now first attempts an LLM-driven plan via `TaskPlanner`, which calls the planning provider with a system prompt describing available agent roles and expects a JSON array of `(role, description, depends_on)` entries. If the LLM response can be parsed into a valid DAG, that plan is used directly. Otherwise, the coordinator falls back to the original heuristic pipeline.

**Rationale:**
- The LLM planner produces more flexible task graphs than the fixed pipeline (e.g., parallel Researcher + Coder subtasks, multiple review cycles, or novel role orderings).
- The fallback ensures the system degrades gracefully when the LLM returns unparseable output.
- Only a single new field (`planning_provider: Arc<dyn LlmProvider>`) is added to `CoordinatorAgent` — the TaskPlanner is stateless and lightweight.
- The `TaskPlanner` lives in its own module (`crates/orchestrator/src/planner.rs`) and is `#[cfg(not(test))]` only; tests use the fallback path or `ScriptedCoordinator`.

**Trade-off:** The LLM planner adds one extra provider call per task (latency + cost). Mitigated by (a) using a fast/cheap model for planning when possible, (b) caching the prompt design in the planner's system prompt, and (c) the fallback path costing zero extra calls.

### 12. TaskGraph persistence

**Chosen:** Adjacency list stored as JSON in the `subtasks.graph_json` column (migration `012_subtasks.sql`). `petgraph::DiGraph` is used only for in-memory traversal during execution.

**Rationale:** `petgraph`'s internal representation is opaque and version-specific. An adjacency list is trivial to serialise, diff, and inspect. The conversion via `TaskGraphSerializer` is lossless for the DAG structure we need.

## Consequences

### Positive

- Multi-agent mode shares all Phase 4 infrastructure with single-agent mode — no duplicate code.
- Write gates prevent a compromised agent from causing damage.
- Unified event bus means the UI, audit log, and replay system all observe multi-agent activity without changes.
- Cycle detection prevents infinite loops with clear user-visible escalation.
- Budget-aware routing prevents surprise costs.

### Negative

- Shared memory via `Snapshot + AgentContext` means agents don't see each other's results in real-time during parallel execution. Mitigated by the coordinator feeding previous results into subsequent `AgentContext`s.
- `MemoryWriteSerializer` is a throughput bottleneck for concurrent writes. Acceptable for MVP — revisit if benchmarking shows contention.
- Hardcoded cycle limits (3 review, 2 validation) may need tuning per task type. Addressed by operator override on escalation.

### Risks

- **R-01** (plan invalid graph): Mitigated by `TaskGraphValidator`.
- **R-02** (cost > 3× single-agent): Mitigated by `SpendTracker` + `RoutingEngine` downgrade.
- **R-03** (Coder↔Reviewer infinite loop): Mitigated by Rule B cycle detection.
- **R-04** (read-only agent writes): Prevented structurally at `ToolRegistry` construction.
- **R-05** (memory corruption): Prevented by `MemoryWriteSerializer`.

## References

- [Current architecture](../architecture.md)
- [Current multi-agent guide](../agent-collaboration.md)
- [ADR-10](archive/ADR-10.md) (archived): historical LanceDB vector-store
  decision, superseded by [ADR-63](ADR-63-memory-subsystem.md)
- [ADR-16](ADR-16.md): context overflow strategy
- [ADR-31](ADR-31.md): model-first selection with internal provider routing —
  supersedes the routing portion of this ADR (archived predecessor:
  [ADR-24](archive/ADR-24.md))

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
