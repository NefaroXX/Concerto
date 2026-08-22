# Concurrent Agent Runtime — Foundational Research Brief

Date: 2026-08-18
Status: Research input for ADR-60 (Active). Mechanism notes below predate the
implementation; the D5 conflict policy landed as always-on per-target
`base_versions` claims — see the D5 implementation-notes addendum in
`docs/adrs/ADR-60-concurrent-agent-runtime.md`.
Scope: mechanisms, not product surveys — persistent working memory, event sourcing for agent runtimes, concurrent-agent scheduling under a single write gate, progressive-disclosure context, process supervision/IPC (Rust)

## 1. Purpose

Concerto is converting from a single-process, hub-and-spoke orchestrator loop (one coordinator that sequentially dispatches specialist agents in waves over a shared in-memory `EventBus`) to a **process-per-agent runtime**: each agent is its own OS process; the coordinator becomes a **process supervisor**; agents cooperate through a **shared, event-sourced whiteboard** (persistent, durable, subscribable); and a **memory spine** carries state from live working memory through consolidation into long-term hybrid vector+FTS storage.

This brief documents the mechanisms the new architecture must be built on, with 2025–2026 sources, and is explicitly **not** a survey of commercial products. It is coupled to GitHub issue #152 (Plan→Execute continuity): the persisted structured state #152 requires (design doc, task graph, decision/action ledger surviving past one coordinator instance) is a **prerequisite** of the whiteboard, not a separate workstream.

Design targets that every mechanism below must serve (user decisions, non-negotiable):

1. Separate OS processes per agent; coordinator = supervisor with heartbeat/health-check, restart policy, orphan cleanup, and a real IPC protocol.
2. **One real write gate**: all agent file/tool writes flow through a single policy-engine chokepoint in the supervisor process — not per-process gates that are "supposed to be equivalent".
3. Reversibility/checkpointing that attributes concurrent, possibly-interleaved writes **per agent** (not last-action undo).
4. **Deterministic replay** despite concurrent execution: ordering/attribution captured at write time, never reconstructed after the fact.
5. Full audit trail with the policy engine as the sole write gate.
6. Incremental/progressive-disclosure context: no full transcripts in prompts; working set + retrieval + summaries, token-budgeted.
7. Scale target 3–6 agents now; scheduler/subscription model not hardcoded to N; no swarm-scale coordination (sharding, hierarchical supervision) in v1.
8. **Not** routed through the plugin/WASM boundary: plugins are a trust-containment boundary (hostile code); agent concurrency is a fault-containment boundary (flaky providers). Different mechanisms, kept separate. A plugin acting as an agent is a future feature with its own integration.

## 2. Ground truth: the architecture we are replacing (verified 2026-08-18)

| Fact | Location |
|---|---|
| Fresh `CoordinatorAgent` per `run_shared_agent` invocation; `design_doc: Mutex<Option<DesignDoc>>` scoped to one instance, never persisted | runtime_runner.rs (construction), coordinator.rs:484 |
| Execute phase task = rendered markdown of the plan embedded in prose; `decompose_task` re-run from scratch | runtime_runner.rs (Execute task assembly), coordinator.rs:2018 (non-checkpoint branch) |
| `decompose_or_restore` (1919) vs `execute_graph` (2104); wave scheduling runs ready subtasks concurrently **within the process**, then applies outcomes sequentially | coordinator.rs |
| Coordinator `memory_store` is **read-only**: only `retrieve` calls (1914, 2502); zero `store` callsites. Only production memory write is single-agent `store_task_summary → ChunkType::SessionSummary` | coordinator.rs:472/675/690/1914/2502; agent_loop.rs:1132 |
| Resume identity: `objective_hash = blake3(task.description)` compared at runtime_runner.rs:2644–2645; description embeds conversation history → hash **never matches after message 1**; Plan produces no checkpoint at all (no tools run during planning) | runtime_runner.rs:2644; coordinator.rs:2174 |
| `transcript_entries` (per-turn typed detail incl. tool calls) written durably but loaded only for UI/audit display, never into next-run context; `messages` table has `tool_calls`/`tool_results` columns always NULL in production appends | sessions/lib.rs (messages 855–914, transcript_entries 1777–1825); agent_loop.rs:1180–1273 |
| `EventBus` is in-process broadcast + durable mpsc subscriber; no transport; `stage_feed` filters by `session_id` because one process hosts many sessions | core/src/event.rs:707/882/887; runtime_runner.rs:3366/3389 |
| Single policy chokepoint exists **in-process**: `ToolExecutor::execute` → `SimplePolicyEngine::evaluate` (executor.rs:413→447); `FilesystemTool` intentionally ignores `_policy` (filesystem.rs:170–178) because the executor is the gate | core/executor.rs:413/447; tools/filesystem.rs:175–178 |
| Shared mutable in-process state: `VirtualFs` overlay, `SpendTracker` (RwLock), concurrency semaphores, `expected_artifacts` — all `Arc`/`Mutex`, all lost across processes | virtual_fs.rs:124; policy.rs:720; agent_runner.rs:82–84; coordinator.rs:480 |
| `run_review_cycle` is non-resumable (no checkpoint in flight) | coordinator.rs:2988/3075 |
| Lossless ordering primitives already exist at the DB layer: `sequence_num = COALESCE(MAX(sequence_num),0)+1` on messages/transcript_entries/session_events | sessions/lib.rs:787/1220/1800 |
| MCP stdio child lifecycle precedent: single child per server, initialize handshake, GRACE_PERIOD=45, stop+reap | mcp/client.rs:17/194/307/371 |

Implications: the single-process assumption is explicit in code (in-process bus, Arc-shared stores, sequential outcome application), and the two cross-process handoff points today are (a) outcome→observer events and (b) memory writes — both ride in-process machinery. The durable log substrate (`session_events`, `transcript_entries`, deterministic `sequence_num`) already exists and is the natural foundation for the whiteboard.

## 3. Persistent working-memory architectures

### Mechanisms (established, 2025–2026)

**Era/tier model** — the field converged on working → episodic → semantic (→ procedural) tiers. Working memory = token-budgeted, always-in-context state; episodic = timestamped event records with provenance; semantic = consolidated facts; procedural = skills/policies retrieved on demand. Consolidation (episodic→semantic) is the recognized weak link.

- **Letta/MemGPT memory blocks**: labeled, size-limited, in-context blocks, agent-editable (`memory_replace/insert/rethink`), persisted individually so parts of the context window are editable; guidance: <50k chars, <20 blocks/agent. Letta's "sleep-time" agents are the canonical **consolidation-as-a-separate-process** mechanism: a background agent (or reflection subagent) rewrites/consolidates memory between steps (default every N=5 steps), keeping the primary loop low-latency.
- **Mem0**: add() = context lookup → LLM fact extraction → hash dedup → embedding → entity extraction → storage; search() = parallel vector+BM25+graph scoring → fusion. v3 is **add-only**: old and new facts coexist, with temporal metadata (event/state/plan/preference/relationship, ongoing/completed) written at extraction and scored at retrieval; v2 had LLM conflict resolution that invalidates superseded memories.
- **Graphiti/Zep (temporal knowledge graph)**: three subgraphs (episode / semantic entity / community); **bi-temporal model** — `t_valid`/`t_invalid` (world time) vs `t_created`/`t_expired` (ingestion time); contradictions **invalidate, never delete**; episodes are provenance ground truth; retrieval fuses time + full-text + semantic + graph.
- **Anthropic Memory API / context-engineering doctrine** (memory tool + `compact_20260112` + `clear_tool_uses` + `clear_thinking`): memory = structured note-taking **outside** the window; compaction = lossy in-window compression; clearing = dropping re-fetchable tool results. Progressive disclosure: put only the right slice of context in the window, rest retrievable.
- **LangGraph v1-era persistence**: checkpointer (short-term, thread-scoped state; **per-node `checkpoint_writes`** so a node's durable writes survive a sibling's failure) vs store (long-term, cross-thread KV). Threads are the cursor unit.
- **2026 research systems**: HMAT (working = 4096-token block rewritten per step; episodic = vector + importance weighting; semantic = periodic consolidation; +14–23% on WebArena/SWE-bench); Virtual Context (OS-style paging: tag page tables, prefetch, demand paging, LRU eviction, compaction at 70%/85% budget); Agentic Context Management (five primitives; quadratic→linear cost; context-rot resistance).

### Failure modes

- **Recall drift / context rot**: recursive summarization compounds fidelity loss ("telephone game"); long-running agents drift from their own history.
- **Compaction-as-truth conflation** (documented real incident, Jul 2026): Claude Code compaction summaries recorded partial stdout of timed-out commands as confirmed results — false positives propagated across sessions. **Summaries must be views/projections, never rewrites of the raw log.**
- **Stale memories**: agents fail to convert retrieved evidence into current-state judgment (recognition ≠ recall).
- **Consolidation fragility**: episodic→semantic consolidation needs explicit rules or LLM-driven passes — both "fragile and hard to validate" (2026 survey).
- **Block budget creep**: always-injected working state grows into a monolith unless overflow triggers consolidation.
- **One disclosure level is enough**: first controlled study (Jul 2026) — a deeper second routing level "never helps and sometimes breaks accuracy"; gains appear only when the agent would otherwise navigate raw documents poorly.

### Recommendations

1. Era model implemented as **files/logs, not DB blobs**: working-memory era = per-agent labeled, token-budgeted blocks (Letta-style) always injected; episodic = the whiteboard event log itself (never summarized away); semantic = consolidated facts in the existing hybrid vector+FTS spine (SqliteVectorStore, RRF fusion, local embeddings) with **bi-temporal metadata** (Graphiti-style) and **invalidate-not-delete**.
2. **Consolidation as a process, not a thread**: a dedicated consolidation agent/task (sleep-time pattern) with write access through the same single gate, triggered by event-count/context-threshold. Keeps the primary loop low-latency (MemGPT's known flaw: in-band memory ops destabilize the loop).
3. **Write-time metadata**: temporal/type metadata captured at extraction time (Mem0 rationale — same as our "attribution at write time" constraint).
4. **Provenance-first**: every consolidated fact cites its episode/event IDs (Graphiti). The spine is a projection; the log is truth — prevents the compaction-conflation failure mode.
5. **Budgets to encode**: working memory 4–8KB always-injected; retrieval shortlists 5–10 chunks; token caps per assembly; one disclosure level; drop lowest-priority layers first.

## 4. Event sourcing for agent runtimes

### Mechanisms

- **Log-as-truth**: append-only event log; current state = deterministic projection; behaviors react to log/state changes and emit new events. Variants differ in who writes (single runtime writer vs per-agent streams), what is recorded (only side effects vs also decisions), and how projections are built (in-memory graph, DB, filesystem).
- **WAL-before-execute**: write the canonical event **first**, then apply, then ack (duoduo; Claude Code's "log before act" for user messages). Gives replayability and crash-safe recovery: a crash never yields applied-but-unlogged writes.
- **Typed event schema families**: typed action logs (LLM_REQUEST/LLM_RESPONSE/TOOL_CALL/TOOL_RESULT/CHECKPOINT/POLICY_DECISION — Solana Garden/Harbor), transcript logs (Claude Code JSONL: per-line typed entries, uuid+parentUuid chains for forking, compact_boundary markers), hash-chained logs (Eventloom: SHA-256 chain), per-user monotonic sequences (provenance-log, crash-safe idempotent batches).

### Ordering under concurrency — the spectrum

- **Per-sender order only** (Erlang semantics): "if A sends S1 then S2 to B, S1 is guaranteed not to arrive after S2." Nothing else.
- **Causal order via logical clocks**: Lamport happened-before + tie-break gives a consistent total order; **HLC** (Hybrid Logical Clocks) stays close to wall-clock while preserving causality (CockroachDB/MongoDB).
- **Centralized sequencer / single-writer-per-stream** (Kafka): order guaranteed within a partition; a partition consumed by ≤1 consumer per group; exactly-once = idempotent producer (producer ID + per-message sequence) + transactional offset checkpointing.

### Deterministic replay, snapshots, idempotency

- Record-replay literature: a **total order is not always necessary** — commutative events replay in any order (2× lower overhead at 131k cores); but for **our** purposes file writes are non-commutative, so a total order at the gate is the simple sound choice.
- Replay divergence must be a **loud failure** (Temporal `DeterminismViolationError`; Claude Lab `ReplayDivergenceError` by seq), not silent drift.
- Snapshotting = periodic projection checkpoint (DB state or filesystem snapshot + log tail), **never log summarization** (compaction recursion trap); compensating events for wrong writes.
- Idempotency: event IDs + request-key matching; at-least-once delivery + dedup by event ID is the robust default ("exactly-once" is a composite claim — idempotent production + atomic offset checkpointing; marketing collapses the two).
- Backpressure: unbounded subscription queues blow up supervisors; bounded queues + cursor-based catch-up.
- Wall-clock timestamps from multiple processes are **not** a total order (clock skew) — never order by received-at wall time; use gate seq or HLC.

### Recommendations

1. **Event schema**: `{event_id: uuid, gate_seq: u64 (global, assigned by supervisor), agent_id, agent_seq: u64 (per-agent), kind, scope/topic, causation: {trigger_event_id | hlc}, payload, content_hash, pre_image_hash (for writes)}`. Whiteboard non-write events (findings, decisions, summaries) order by **HLC**; writes order by **gate_seq**.
2. **Sequencing options**:
   - **A. Central sequencer (recommended)**: the write gate assigns `gate_seq` under a single-writer mutex before persisting → natural sound total order, consistent-cut coordinate for checkpoints ("everything ≤ seq S"), Kafka-style single-writer-per-stream semantics. Cost: the gate is a serialization point — fine at 3–6 agents (~µs/op over stdio).
   - **B. Per-agent cursors + HLC merge**: agents journal locally with agent-local seqs; supervisor merges by (HLC, agent_id). Removes the gate from the hot path but forces causal reconstruction and makes "consistent checkpoint" harder. Only justified if the gate becomes a throughput bottleneck.
3. **Idempotent replay**: gate applies writes with event_id dedup (Kafka PID+seq analogue); agents retry request/ack with idempotency keys; replay divergence → loud error.
4. **Snapshotting**: periodic projection checkpoint (filesystem/DB state + log tail), not event summarization; compensating events for wrong writes.
5. **Delivery**: whiteboard = topics; at-least-once with dedup by event_id; per-subscriber cursors persisted; bounded queues with backpressure.
6. **Keep raw episodes forever** (or hash-anchored to cold storage) — audit-trail and provenance requirement; summaries are projections.

## 5. Concurrent-agent scheduling with a single write gate

### Mechanisms

- **Centralized write serialization**: single-writer-per-partition discipline (Kafka idempotent producer); single actor/mailbox serializing mutations; WAL-before-execute ordering (duoduo). Variants: FIFO vs priority/fair-queueing, per-producer concurrency limits, batching.
- **Write-time attribution**: every mutation tagged producer ID + sequence (Kafka PID+seq; provenance-log per-user monotonic sequence; Solana Garden content-hash entries). This is the primitive that makes per-agent undo and interleaved replay possible.
- **Conflict handling for shared files** (active 2026 problem space for concurrent coding agents):
  - *Isolation*: git worktrees per agent (safe, slow, defers conflicts until context is gone).
  - *Locks*: pessimistic file locks with tokens + timeouts + canonical acquisition order + deadlock-avoidance; optimistic concurrency via `base_version` check (write fails if file changed since read; three-way merge vs last-write-wins).
  - *Hunk-level staging*: file-level granularity is wrong — two agents editing different hunks of the same file should not conflict; same-hunk edits are genuinely unresolvable automatically (tier model: different files → different hunks → same lines → manual).
  - *CRDT deterministic merge*: LWW scalars + commutative sets; "conflict-free, not intent-free" — the losing writer is silently dropped unless surfaced.
- **Checkpoint semantics for concurrent processes**:
  - *Per-agent cursors*: LangGraph threads (`thread_id` + `checkpoint_id`, per-node `checkpoint_writes`); Kafka consumer offsets.
  - *Global snapshot*: filesystem snapshot + log tail (duoduo rehydrate; Temporal checkpoint + event history).
  - **Consistent-cut insight**: with a central gate seq, "all gate events ≤ S" is a consistent cut across agents; per-agent cursors give agent-local recovery; both together are cheap.
- **Supervision** (OTP lineage): one_for_one / one_for_all / rest_for_one / escalate; restart intensity `{MaxRestarts, PeriodSeconds}`; heartbeat vs liveness vs readiness probes; backoff with jitter; failure-storm guard; process groups / cgroup v2 for orphan cleanup; SIGTERM → grace → SIGKILL escalation.

### Failure modes

- **Last-action undo is unsound under interleaving**: undoing agent A's last write can clobber agent B's concurrent write. Reversibility requires checkpoint-at-boundary + replay, not inverse operations.
- **Fairness/starvation**: naive FIFO gate lets a chatty agent starve others — per-agent in-flight limits + weighted round-robin or quotas; throttle by token/turn budgets.
- **Deadlocks in file locks**: canonical acquisition order + timeouts; abandoned locks need expiry.
- **Lock granularity mismatch**: file-level locks block unrelated hunks; for 3–6 agents prefer optimistic `base_version` + hunk-aware staging over pessimistic whole-file locks (reserve locks for hot shared files).
- **Orphan cleanup gaps on Linux**: `PR_SET_PDEATHSIG` covers only direct children (grandchildren survive); a supervising process that dies without cleanup leaks the tree; `setsid` escapes process-group containment; cgroup v2 is the kernel-level guarantee.
- **Health-check false positives**: a busy agent (long LLM call) misses heartbeat deadlines → wedged-vs-slow ambiguity. Heartbeats must be emitted by the async loop, not blocked by in-flight LLM/tool work; readiness separate from liveness.
- **Restart storms**: one_for_all restarts are wrong when agents hold independent state — use one_for_one + dependency-ordered recovery (rest_for_one) for shared-spine dependencies.
- **Single gate = single point of failure**: acceptable in v1 (supervisor is already the coordinator), but the gate's own durability (WAL-before-execute) must be crash-safe or the audit trail has holes.

### Recommendations

1. **Gate design**: supervisor-side policy engine is the sole writer. Request path: agent → JSON-RPC `write`/`execute` request (with idempotency key) → policy check → assign `gate_seq` → persist event → apply write → ack with `event_id`. Single-writer thread or tokio mutex; **WAL-before-execute** so a gate crash never produces applied-but-unlogged writes.
2. **Attribution at write time** (mandatory fields on every write event): `agent_id`, `agent_seq`, `gate_seq`, target path, content hash, **pre-image hash** (captured by the gate before applying — makes per-agent revert possible without reconstructing history).
3. **Reversibility**: two mechanisms — (i) gate-boundary checkpoints (commit/fsync a snapshot at gate_seq S; restore = snapshot + replay tail); (ii) per-agent revert = restore snapshot + replay excluding that agent's event_ids. **Not last-action undo.**
4. **Conflict policy for v1 (3–6 agents)**: optimistic `base_version` checks + hunk-aware staging; explicit lock token for hot shared files; same-hunk collisions surfaced to the supervisor (loud, manual-resolution path), never silently dropped.
5. **Supervision**: one_for_one with intensity limits; heartbeat from the async loop; readiness separate from liveness; restart with backoff+jitter; orphan cleanup via process groups + PDEATHSIG + SIGTERM→grace→SIGKILL; per-agent concurrency limits + fairness at the gate.

## 6. Incremental / progressive-disclosure context

Mechanisms and doctrine (Topic 1 sources): working set (always-injected, 4–8KB budget) vs retrieved (on-demand, shortlists 5–10 chunks, token caps) vs archived (long-term store); summary cascades at **one disclosure level** (a second level hurts accuracy); dedup across agents (shared whiteboard events should not be re-injected verbatim into every agent); compaction only as in-window compression with the raw log kept as truth; drop lowest-priority layers first under budget pressure.

Applied to our multi-agent case: each agent's prompt = role instructions + working-memory blocks + a **bounded whiteboard slice** (relevant topics, filtered by subscription + recency + priority) + retrieved memory chunks + its own recent tool results. Never the full transcript; never the full event log. Token caps enforced at assembly time.

## 7. Rust IPC and process supervision

- **Transport**: stdio + newline-delimited JSON-RPC is the proven in-repo precedent (MCP client lifecycle: spawn → initialize handshake → messages → graceful stop + reap; GRACE_PERIOD=45s; double-spawn guard). For 3–6 agents over localhost this is simple, debuggable, and sufficient; serde_json is fine at this scale (switch to a binary codec only if profiling demands it).
- **Protocol**: bespoke versioned protocol in the same transport shape (NOT MCP itself — different domain, different lifecycle); explicit version in handshake; both sides fail loudly on version mismatch.
- **Heartbeat/liveness**: agent emits heartbeat from its async loop (not blocked by in-flight LLM calls); supervisor distinguishes liveness (process alive + heartbeat) from readiness (handshake complete, gate registered).
- **Restart**: one_for_one, intensity-limited, backoff + jitter; state comes from the log (stateless rehydration — duoduo pattern), so restarts are cheap.
- **Orphan cleanup**: process groups; PDEATHSIG on direct children; SIGTERM → grace → SIGKILL escalation; cgroup v2 noted as the kernel-level guarantee if we later need stronger containment.
- **Config propagation**: per-process startup config from the existing layered config system (ADR-57 precedent) at spawn time, not re-read per event.

## 8. Synthesis — how the pieces fit

```
                 ┌────────────────────────────────────────────┐
                 │           SUPERVISOR PROCESS               │
                 │  coordinator/scheduler + policy gate       │
                 │  ┌─────────────┐   ┌───────────────────┐   │
                 │  │ write gate  │──▶│ event log (truth) │   │
                 │  │ seq+policy  │   │ + projections     │   │
                 │  └─────────────┘   └───────────────────┘   │
                 └───────┬────────────────────┬───────────────┘
          IPC (stdio    JSON-RPC)             │ WAL-before-execute
        ┌───────────────┴────────┐    ┌───────┴────────┐
        ▼                        ▼    ▼                ▼
   AGENT PROCESS A          AGENT PROCESS B     CONSOLIDATION TASK
   (architect)              (coder)             (sleep-time pattern:
   LLM calls, tools via     whiteboard pub/     episodic summaries,
   gate, heartbeat          sub, memory        facts → hybrid store)
```

- **Whiteboard** = the event log + topic projections + subscriptions. Agents publish findings/decisions/writes; they see each other's recent activity via their subscribed slice — the "zoom call".
- **Write path** = gate: policy check → gate_seq → persist → apply → ack. One chokepoint, total order, attribution at write time.
- **Memory spine** = working-memory era (per-agent blocks + whiteboard slice) → consolidation task → episodic summaries + facts in the hybrid vector+FTS store (bi-temporal, invalidate-not-delete, provenance-cited).
- **#152** = structured state (DesignDoc, task graph, decision/action ledger) persisted as whiteboard events keyed by `plan_id`; Execute loads the structured object + ledger instead of re-deriving from prose; silent re-decompose forbidden.

## 9. Open questions for ADR-60

1. Reuse `session_events` (extend its kind set + add gate fields) vs a new `whiteboard_events` table? (Recommendation: extend — one log, existing ordering primitives, migration 003 precedent.)
2. Where does the filesystem live — supervisor-owned single VirtualFs with per-agent delta views, vs per-agent worktrees? (Recommendation: supervisor-owned overlay + optimistic base_version; worktrees deferred.)
3. Does the consolidation task run as a fourth process type or as a supervisor-side task with gate access? (Recommendation: supervisor-side task — simpler, still isolated from agents.)
4. Heartbeat cadence and restart intensity defaults (suggest: 10s heartbeat, 3 restarts/60s, backoff 1s→30s with jitter).
5. Protocol versioning scheme (semver in handshake) and error taxonomy for the IPC boundary.

## 10. Sources

Working memory: Letta context hierarchy / memory blocks / sleep-time agents (docs.letta.com, letta.com/blog); Mem0 pipeline + v3 add-only + paper (docs.mem0.ai, arxiv.org/abs/2504.19413); Graphiti/Zep bi-temporal (arxiv.org/abs/2501.13956, github.com/getzep/graphiti, deepwiki); Anthropic Memory API + context-engineering cookbook + progressive disclosure (platform.claude.com/docs/agents-and-tools/tool-use/memory-tool, platform.claude.com/cookbook/tool-use-context-engineering-context-engineering-tools, anthropic.com/engineering/effective-context-engineering-for-ai-agents); LangGraph persistence/checkpointers (docs.langchain.com/oss/python/langgraph/persistence); HMAT (clawrxiv.org/papers/2026.00008); Virtual Context (virtual-context.com/paper); ACM context primitives (arxiv.org/html/2607.21503); A-MEM (arxiv.org/abs/2502.12110); MemOS (arxiv.org/abs/2507.03724); MemoryOS (emnlp 2025); surveys: Memory for Autonomous LLM Agents (arxiv.org/html/2603.07670v1), Governed Memory for Multi-Agent Workflows (arxiv.org/abs/2603.17787); compaction-conflation incident (arxiv.org/abs/2607.13071); STALE (arxiv.org/html/2605.06527); disclosure-levels study (arxiv.org/html/2607.17598).

Event sourcing: ActiveGraph "The Log is the Agent" (arxiv.org/html/2605.21997); Eventloom (syndicalt.github.io/eventloom); provenance-log (npmjs.com/package/provenance-log); duoduo WAL-before-execute (github.com/openduo/duoduo); AGNT5 journal/replay (agnt5.com/docs/concepts/event-sourcing-and-replay.md); Temporal (docs.temporal.io/workflow-execution); Kafka delivery semantics + KIP-98 (docs.confluent.io/kafka/design/delivery-semantics.html, cwiki.apache.org KIP-98); Erlang ordering semantics (erlang.org/docs/22/apps/erts/communication.html); HLC (usenix.org hotcloud15-demirbas); Lamport (lamport.org/pubs/time-clocks.pdf); deterministic replay: zylos.ai/research/2026-04-26-replayable-agent-runtimes, solana.garden/guides/llm-agent-deterministic-replay, claudelab.net Claude Agent SDK replay postmortem, agrepl (arxiv.org/pdf/2607.16200v1); Claude Code transcript internals (claude-wiki.com/session-persistence.html); partial-order replay (charm.cs.illinois.edu 14-21); Confluent blackboard-as-topic (confluent.io/blog/event-driven-multi-agent-systems).

Scheduling/supervision: OTP supervision (erlang.org/doc/design_principles/sup_princ.html; zylos.ai 2026-03-16 supervisor trees, 2026-03-02 self-healing/StuckDetector, 2026-02-20 health monitoring); auton lifecycle (github.com/atemerev/auton); microagent-guide (github.com/mkkotcherla/microagent-guide); Callipso hunk-level conflict tiers (callipso.dev/blog/parallel-agent-conflict-detection); puppyone MUT optimistic concurrency (puppyone.ai/en/docs/conflict); Fastio lock tokens (fast.io/resources/ai-agent-concurrent-editing); continuum isolation spectrum (github.com/CambrianTech/continuum/issues/508); grite WAL-in-git-ref CRDT (dev.to/dipankar_sarkar).

Context: budget/retrieval guidance (sitepoint.com/ai-agent-memory-guide, dataworkers.io avoid-context-bloat), block budget creep (zylos.ai 2026-06-30).

## 11. Status

Research complete 2026-08-18. Feeds ADR-60 (Active). No code decisions in this document; mechanisms here are the constraint set the ADR chooses among. The v1 conflict-mechanism implementation is the always-on, per-target `base_versions` design recorded in the ADR-60 D5 implementation-notes addendum.
