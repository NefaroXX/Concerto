# ADR-60: Concurrent Agent Runtime — Process-per-Agent Supervisor, Event-Sourced Whiteboard, Memory Spine

**Status:** Accepted (see Revision)
**Date:** 2026-08-18
**Deciders:** sol (product owner); architecture review pending per process
**Supersedes:** none (new decision; ADR-35 §4/§5 coordinator contract is redefined below, not silently contradicted)
**Relationship to prior ADRs:** ADR-35 (coordinator contract — amended: coordinator becomes supervisor), ADR-36 (transcript_entries — becomes a projection view of the whiteboard log), ADR-43 (MCP stdio lifecycle — transport precedent), ADR-58/59 (config-owned orchestration topology — orthogonal, unchanged), ADR-55 (intent/authorization — gate scope), ADR-57 (config change propagation — per-process startup config)
**Research input:** `docs/research/concurrent-agent-runtime.md` (2026-08-18)

## Context

### The current architecture (verified against code 2026-08-18)

Concerto runs a single-process, hub-and-spoke loop: one `CoordinatorAgent` per `run_shared_agent` invocation dispatches specialist agents in waves over an in-process `EventBus`. Everything is shared via `Arc`/`Mutex` inside one process (`VirtualFs` overlay, `SpendTracker`, concurrency semaphores). All tool executions pass one policy chokepoint inside that process (`ToolExecutor::execute` → `SimplePolicyEngine::evaluate`, executor.rs:413→447; `FilesystemTool` deliberately ignores its `_policy` parameter, filesystem.rs:170–178, because the executor is the gate).

Four structural facts define the limits of this design:

1. **One agent alive at a time per process.** Waves give *apparent* concurrency, but outcomes are applied sequentially after each wave join (coordinator.rs wave loop). There is no moment where two agents are simultaneously active, publishing, and reacting to each other.
2. **No cross-run continuity.** A fresh `CoordinatorAgent` is constructed per invocation (runtime_runner.rs); `design_doc: Mutex<Option<DesignDoc>>` never persists (coordinator.rs:484); the coordinator's `memory_store` is read-only (only `retrieve` at 1914/2502, zero store callsites); `transcript_entries` is written durably but only ever loaded for UI display (sessions/lib.rs:1777–1825); resume identity `objective_hash = blake3(task.description)` embeds conversation history so the hash never matches after message one (runtime_runner.rs:2644–2645).
3. **GitHub issue #152** (Plan→Execute continuity): Plan produces no checkpoint (no tools execute during planning), so Execute always constructs a fresh coordinator, embeds the rendered plan markdown as prose into a new task description, and re-runs `decompose_task` — the architect re-derives the plan it already produced; earlier writes are checked-for as if unknown; failed commands re-run as if their failure left no trace. Root cause: the system has no concept of "the same unit of work continued".
4. **Provider flakiness is a whole-run risk.** One agent's hanging/failing LLM call endangers the entire process, including the audit trail and the policy gate.

### Why a new architecture is being decided fresh

Per owner decision (2026-08-18): this is a new decision, not recovered intent. No prior ADR or doc covers a shared workspace/whiteboard or concurrency (ADR-58/59 and `docs/research/orchestration-blueprint.md` are about configurable pipeline topology only). The earlier foundational research (`docs/research/concurrent-agent-runtime.md`) was commissioned specifically to establish the mechanisms for this decision.

### Owner decisions (binding)

- **Process-per-agent**, not same-process parallel tasks. Rationale: fault containment — one agent's flaky/hanging LLM call must not take the run down. Extends the existing pattern where `crates/mcp` treats tool servers as separate processes with their own lifecycle over a protocol.
- **Coordinator becomes a process supervisor**: spawn, health-check/heartbeat, restart policy, orphan cleanup, plus a real IPC protocol for all cross-process state (no shared `Arc` shortcut).
- **Scale**: 3–6 agents now; scheduler/subscription model not hardcoded to N; no swarm-scale coordination (sharding, hierarchical supervision) in v1.
- **Not routed through the plugin/WASM boundary**: plugins are a trust-containment boundary (hostile code); agent concurrency is a fault-containment boundary (flaky providers). Kept separate by design.
- **Non-negotiable constraints** (these degrade silently under naive concurrency; they must be designed at the point where whiteboard, supervisor, and write path meet):
  1. Full audit trail with the policy engine as the **sole write gate** across all agent processes — one real chokepoint, not per-process gates "supposed to be equivalent".
  2. Reversibility/checkpointing that correctly attributes concurrent, possibly-interleaved writes **per agent** (not last-action undo).
  3. **Deterministic replay** despite concurrent execution: explicit ordering/attribution captured at write time, not reconstructed after the fact.
- **#152 is coupled, not separate**: the persisted structured state #152 asks for (design doc, task graph, decision/action ledger surviving past one coordinator instance) is a prerequisite of the whiteboard — agents cannot see each other's recent activity if activity does not outlive a single coordinator instance.

## Decision

### D1. Execution model — process-per-agent with a supervisor

- Each specialist agent runs as its own OS process. The coordinator process becomes the **supervisor**: spawn, handshake, heartbeat/liveness/readiness, restart policy (one_for_one with intensity limits and backoff+jitter), orphan cleanup (process groups, `PR_SET_PDEATHSIG` on direct children, SIGTERM → grace → SIGKILL escalation), and the write gate.
- The supervisor owns the policy engine, the event log, the whiteboard projections, and the audit trail. Agents are stateless workers that rehydrate from the log on restart (duoduo pattern: state lives in the log, not the process).
- Heartbeat is emitted from the agent's async loop so in-flight LLM calls never block liveness reporting; readiness (handshake + gate registration) is separate from liveness.
- This supersedes the ADR-35 "coordinator as sole delegator" role: the coordinator retains delegation authority but its mechanism becomes supervision + scheduling, not in-process sequential dispatch.

### D2. IPC — bespoke versioned protocol over stdio JSON-RPC

- Transport: stdio, newline-delimited JSON-RPC — the MCP transport precedent (mcp/client.rs lifecycle: spawn → initialize → messages → graceful stop + reap), but a **bespoke protocol namespace and lifecycle**, not MCP itself (different domain; MCP remains for external tool servers).
- Versioned handshake (semver); both sides fail loudly on mismatch. Error taxonomy defined at the boundary.
- Messages: agent → supervisor: `execute_tool` (with idempotency key), `publish_event`, `retrieve_memory`, `heartbeat`; supervisor → agent: `ack(event_id)`, `gate_decision`, `whiteboard_slice` (subscription push), `shutdown`. All state-bearing traffic goes through this protocol; no shared memory.
- Config is passed at spawn from the existing layered config (ADR-57 precedent), not re-read per event.

### D3. Whiteboard — append-only event log as source of truth

- The whiteboard is an append-only event log (SQLite, extending the existing sessions DB substrate — `session_events` precedent, migration 003; new migration adds gate fields/kind set). Events: findings, decisions, write-applications, failures, plan/design artifacts, consolidations.
- Event schema: `{event_id: uuid, gate_seq: u64 (global, assigned by supervisor), agent_id, agent_seq: u64 (per-agent), kind, scope/topic, causation: {trigger_event_id | hlc}, payload, content_hash, pre_image_hash (for writes)}`.
- Ordering: **central sequencer** (option A in the research brief) — the gate assigns `gate_seq` under a single-writer mutex before persisting. This yields a total order, a consistent-cut coordinate for checkpoints ("everything ≤ seq S"), and Kafka-style single-writer-per-stream semantics. Per-agent cursors are used for recovery, layered on top.
- Subscription: topics/scopes; at-least-once delivery with dedup by `event_id`; per-subscriber persisted cursors; bounded queues with backpressure.
- The raw log is never summarized or deleted (audit-trail requirement); compaction applies to projections only.

### D4. Single write gate (constraint 1)

- **All** agent file/tool writes execute as `execute_tool` requests through the supervisor-side `ToolExecutor`/`SimplePolicyEngine` — the existing executor chokepoint (executor.rs:413→447) relocated into the supervisor, the only policy evaluation in the system.
- Request path: agent → `execute_tool(idempotency_key, ...)` → policy check → assign `gate_seq` → **WAL-before-execute** (persist event, then apply, then ack) → `ack(event_id)`.
- Idempotency: the gate dedups by `event_id`/request key; agents retry safely; replay skips applied writes.
- Fairness: per-agent in-flight limits and weighted round-robin at the gate (no chatty-agent starvation); token/turn budgets enforced as today (SpendTracker moves into the supervisor).
- This also fixes the existing in-process gap where any future in-process writer bypassing `ToolExecutor` would skip policy — with a single gate process there is exactly one enforcement point.

### D5. Attribution, reversibility, deterministic replay (constraints 2 & 3)

- Attribution at write time: every write event carries `agent_id`, `agent_seq`, `gate_seq`, target path, content hash, and **pre-image hash** (captured by the gate before applying) — no reconstruction from history later.
- Reversibility: (i) gate-boundary checkpoints — commit/fsync a snapshot at gate_seq S; restore = snapshot + replay tail; (ii) per-agent revert = restore snapshot + replay excluding that agent's `event_ids`. **Not last-action undo.**
- Deterministic replay: the total-ordered log is the replay input; divergence is a loud failure (replay-diff harness), never silent drift.
- Conflict policy for shared files (v1, 3–6 agents): optimistic `base_version` checks + hunk-aware staging; explicit lock tokens reserved for hot shared files; same-hunk collisions surfaced loudly to the supervisor with a manual-resolution path — never silently dropped (CRDT "conflict-free ≠ intent-free" is rejected as a silent-loss mechanism).

### D6. Memory spine — working era → consolidation → long-term

- Working-memory era: per-agent labeled, token-budgeted blocks (4–8KB always-injected) + a bounded whiteboard slice (subscribed topics, filtered by relevance + recency). The whiteboard log is the episodic tier, never summarized away.
- Consolidation: a supervisor-side consolidation task (sleep-time pattern, triggered by event-count/context thresholds) that produces episodic summaries and semantic facts into the existing hybrid vector+FTS store (SqliteVectorStore), with **bi-temporal metadata** (world-time vs ingestion-time), **invalidate-not-delete**, and **provenance citing event_ids**.
- Progressive disclosure: one disclosure level; retrieval shortlists 5–10 chunks with token caps; summaries are projections, never rewrites of the log (prevents the documented compaction-conflation failure mode).
- This activates the currently dead coordinator write path (coordinator.rs has zero memory store callsites today).

### D7. Issue #152 resolution (coupled, one persistence layer)

- The structured `DesignDoc`, the task graph, and the decision/action ledger are persisted as **whiteboard events keyed by `plan_id`** at Plan-approval time — the durable store #152's fix #1 asks for is the whiteboard, not a parallel system.
- Execute phase: when a task references an approved `plan_id`, the Execute supervisor loads the structured `DesignDoc` object + prior ledger via the gate (not rendered prose), and seeds the carry-forward run state #152's fix #3 asks for (completed subtask results, files touched, failed commands with failure reasons) from the log.
- Silent re-decompose is forbidden (issue fix #4): divergence from the approved plan requires explicit user re-approval.
- One persistence layer total: the event log + its projections (checkpoints, hybrid memory store, transcript views). Explicitly resolves the issue's "do not conflate with the memory subsystem" note: RAG retrieval is *not* the continuity mechanism (the issue is correct on that); the log is. The memory store remains a projection on top of the log, and #152's fix rides the log.
- **Amendment (run-continuity, session-keyed):** the D7 read also fires for explicit `continue`/resume runs that carry no approved-plan binding (a resume after a failed Execute, or a reopened project). It is read-side only: the gate already persists the substrate keyed by the run's session id (`write-applied` rows carry the files touched, `failure` rows the failed commands), and the session's newest hash-verified `plan-approved` payload re-anchors the last approved artifact. No new event kind and no summary row — the log stays the sole source of truth, folded at read time with the same ledger grammar as the plan-keyed path. Gated by the same `plan_binding_source` switch; `legacy` keeps the pre-D7 behavior. A fresh session or empty log seeds nothing (truthful empty state).
- **Amendment (interrupt-safe resume, 2026-09-05):** live acceptance of the evidence-spine build exposed a gap on both sides of the 2026-08-24 amendment: a multi-agent build run, interrupted with Ctrl+C after the coder started, then re-run with `continue`, re-dispatched the **architect** instead of continuing the build. Two decisions close it:
  - **Write side — checkpoint on graceful interrupt.** A graceful stop materializes the orchestration checkpoint before teardown, the same `completed=0` row the stall path writes. The terminal and desktop apps gain a signal/window-close handler (SIGINT/Ctrl+C, app close) that cancels the run and invokes the supervisor's `checkpoint_at_shutdown` (implemented today, exercised only by tests). A hard-killed run leaves no row, a later `continue` finds nothing to resume, and the run re-derives from scratch — the observed failure.
  - **Read side — headless resume from the evidence chain.** When a `continue`/resume request finds **no** checkpoint row (a killed run, or a reopened project with prior whiteboard events), the coordinator seeds dispatch state from the logged evidence instead of re-entering design: the newest hash-verified `plan-approved` payload and the logged researcher/coder gate events determine the next dispatch (verified design + research done → dispatch the **coder**, not the architect). This extends the 2026-08-24 amendment from "fold the ledger prose" to "fold the ledger prose *and* the dispatch cursor". It does **not** relax ADR-65 §7: re-calling the architect or researcher still requires a recorded, evidence-backed `Decision` event; a resume path with no such decision continues the nearest non-redundant step.

### D8. Scope boundaries

- v1: 3–6 agents; scheduler/subscription model expressed with N not hardcoded anywhere; no sharding, no hierarchical supervision.
- Plugin/WASM stays a separate trust boundary. A plugin acting as an agent is a future feature: plugin sandbox talks to the agent supervisor as its own integration; agents are never routed through WASM.
- Configuration-owned rosters (ADR-58/59), the validation rulebook, parity tests, and reversibility guarantees are preserved unchanged.

## Consequences

Positive:
- Crash/fault isolation: a flaky provider takes down one agent process, restarted from the log, never the run or the audit trail.
- Deterministic replay and write-time attribution make the audit trail, reversibility, and testing (replay-diff) real for the first time.
- Run-to-run and phase-to-phase continuity: fixes #152 and the observed symptoms (re-derivation, phantom file-existence checks, repeated failed commands).
- Agents can genuinely cooperate: shared whiteboard subscriptions give peer awareness ("zoom call") on top of the already-shared filesystem.
- The existing durability substrate (sessions DB, `sequence_num` ordering, `session_events`, `transcript_entries`) is extended, not replaced.

Negative / costs:
- IPC overhead (~µs/op over stdio) and gate serialization — acceptable at 3–6 agents; revisited only if profiling demands (option B sequencing is the documented escape hatch).
- Migration cost: runtime_runner construction → supervisor + agent entry points; ToolExecutor/SpendTracker/VirtualFs move behind the gate; EventBus-based observers migrate to log subscriptions.
- `run_review_cycle` must become resumable (it is not today — in-flight review is lost on restart).
- More moving parts: process lifecycle, protocol versioning, heartbeat tuning — the supervision surface is new.
- Single gate = single point of failure by design; its WAL-before-execute durability is mandatory (D4).

## Testing

- Replay-diff harness: run a scenario twice; the total-ordered logs and final projections must be identical (divergence = loud failure).
- Crash-injection suite: kill an agent mid-write, kill the supervisor at gate boundaries; verify WAL-before-execute invariants (no applied-but-unlogged writes) and restart recovery.
- Parallel e2e: two agents concurrently active on one project with a shared file; verify attribution, base_version conflicts surface loudly, and per-agent revert works.
- Parity suite: existing single-process tests continue to pass against the supervisor-backed path (the parity tests are extended, not replaced).
- #152 acceptance: Plan → Execute as two turns must NOT re-invoke the architect with the same objective; earlier writes must be known; failed commands must not re-run unchanged.

## Deferred / Sequencing

1. Thin vertical slice (before any breadth): supervisor + two agent processes + whiteboard log + gate + one consolidation pass, with the replay-diff harness. Proves D3–D6 end-to-end.
2. #152 full fix (D7) on top of the slice — it is a consumer of the whiteboard, not a prerequisite of the slice.
3. **Review-cycle resumability (explicit scope item, consequence of D4/D5):** `run_review_cycle` today is non-resumable — in-flight review state (review target, feedback/retry ledger) is lost on restart (coordinator.rs:2988 comment, :3075). Under the new model this is a defect, not a stylistic gap: a supervisor restart must resume, not discard, an in-flight review. Ship it as part of the slice or immediately after: persist in-flight review state as whiteboard events (review target, feedback ledger, retry counters) and rehydrate it on restart.
4. Scheduler/subscription generalization beyond 6 agents if warranted.

## Migration

- `runtime_runner::run_multi_agent` becomes the supervisor entry; `AgentLoop::run` becomes the agent-process entry (its in-process tool executor replaced by gate requests).
- `VirtualFs` moves to the supervisor; agents get base_version-checked deltas.
- `EventBus` observers (UI, terminal) migrate to log-subscription views.
- `session_events`/`transcript_entries` gain gate fields via new migrations; existing rows remain valid (backward-compatible additive migration).

## Revision

- 2026-08-18 — v1 drafted (Proposed). Pending owner review against `coordinator.rs`, `runtime_runner.rs`, `agent_loop.rs`, `sessions/lib.rs`, `core/executor.rs`, `tools/filesystem.rs`, `memory/vector_store.rs`, `mcp/client.rs` before approval. No code exists for this ADR yet.
- 2026-08-18 — v1.1, Status → Active. Owner review passed on two conditions, both met:
  - D1/D2 claim verification completed: `agent_loop.rs` struct (:35–80) owns every dependency for a run, all injected at construction (`new` :180, `with_project_root` :215, `with_session_store` :292); `run` (:303) → `run_once` (:442) → provider → `execute_single_tool_call` (:1286) → single tool-executor call site (:1365) → `store_task_summary` (:1102) → persistence (:1185/:1232). AgentLoop is the agent-process entry; the slice swaps the executor call site for a gate-proxy client. `mcp/client.rs` confirms the transport precedent: `spawn` (:194, piped stdio, `kill_on_drop`, double-spawn guard :200), `initialize` (:266), JSON-RPC 2.0 newline-delimited framing (:495–511), `stop` (:439, cancel → stdin EOF → GRACE_PERIOD → kill escalation), `Drop` reap (:580); `GRACE_PERIOD = 2s` (:45).
  - `run_review_cycle` resumability promoted from a consequence footnote to an explicit Deferred/Sequencing item (item 3 above).
  - Approved to proceed to the vertical slice (Deferred item 1).
- 2026-08-24 — v1.2, Status Active → Accepted. Implementation complete across
  Phases 1–4 plus follow-up fixes: decisions D1–D8 are implemented and Deferred
  items 1–3 are delivered (thin vertical slice, #152 plan binding, review-cycle
  resumability); Deferred item 4 (scheduler/subscription generalization beyond
  6 agents, real-embedder swap, multi-level disclosure) remains deferred per
  ADR. Implementation commits: `5c9b269` (Phase 1 supervisor wiring),
  `4dc2c67` (Phase 2 whiteboard-verified plan binding), `8e065d8` (Phase 3
  review-cycle resumability), `787df6e` (Phase 4 consolidation and replay
  harness), `b6ce712` (heartbeat liveness anchored on the supervisor clock),
  `f75b4b7` (`PR_SET_PDEATHSIG` orphan cleanup); merged via PR #11 (`83d20cc`).
- 2026-09-01 — v1.3, D4 fairness resolved (implementation notes below). The
  delivered gate satisfies D4's "no chatty-agent starvation" requirement
  structurally — per-agent in-flight isolation over a deliberately
  non-serialized gate — so the weighted-round-robin *mechanism* named in D4
  is superseded, not deferred: there is no cross-agent queue for it to
  schedule, and inventing one (a global execution cap) would be a throughput
  regression rationing a resource nobody contends for. Decision text above is
  unchanged; the notes extend D4 the way the D3/D5/D6 notes do.
- 2026-09-05 — v1.4, run-continuation amendment extended (interrupt-safe
  resume). Live acceptance of the evidence-spine build (hexview smoke test)
  failed: Ctrl+C after the coder started left no checkpoint (no app signal
  handler; `checkpoint_at_shutdown` was test-only), so the `continue` rerun
  re-dispatched the architect instead of continuing the build — and that
  dispatch carried no recorded `Decision` event (ADR-65 §7). The D7
  amendment above now covers both sides; implementation commits are recorded
  here when landed.

### D5 implementation notes — always-on injection & per-target claims (2026-08)

Status of D5 as landed on branch `fix/orchestrator-always-on-base-version`
(HEAD `0785367`, plus uncommitted working-tree changes). These notes extend D5;
the decision text above is unchanged.

- **Always-on stamps, supervised path.** `supervisor.rs::handle_execute_tool`
  now stamps every versioned `GateRequest` with the current pre-image hash(es)
  via `gate::stamp_base_versions` before `services.gate.submit`. D5 conflict
  detection is therefore active by default for supervised writes; previously
  every production write carried `base_version: None` and degraded silently to
  last-writer-wins.
- **In-process parity for the single-agent loop.** Single-process runs
  (CLI/Desktop) now route the legacy `AgentLoop`'s tool calls through the new
  `InProcessGateBackend` (`crates/orchestrator/src/in_process_gate.rs`) — the
  in-process twin of the supervised `GateProxyBackend`. It wraps the same
  `WriteGate` (WAL in the session DB, `FilePreImageReader` rooted at the
  project dir, agent_id `"single-agent"`, max_in_flight 1) built in
  `runtime_runner.rs` from the run's own policy/executor pair and the
  session-DB pool. If the pool cannot be opened, the loop falls back to the
  plain executor with an explicit `tracing::warn!` (never silent). The
  multi-agent/coordinator path deliberately keeps the plain executor —
  specialists are already policy-gated per agent.
- **Per-target claims: the `base_versions` map.** `GateRequest.base_version:
  Option<String>` became `base_versions: BTreeMap<String, String>` (relative
  target path → claimed blake3 hex) with `#[serde(default)]` so pre-change
  wire clients deserialize to an empty map (back-compat). `gate::
  versioned_targets()` is the single source of truth for "what a filesystem op
  mutates": write/delete → `[path]`; move → `[source, destination]` (both are
  mutated — the source is removed); copy → `[destination]` only. The read-only
  `copy` source is covered separately by `attributed_paths()`: its pre-image
  is captured for WAL attribution (audit trail) but it is never
  conflict-checked and never claim-stamped — a copy reads whatever is there,
  so a concurrent source write cannot be lost. `versioned_target` (singular)
  is retained only to keep the whiteboard `pre_image_hash` column's
  single-target-era semantics stable; it is derived from
  `versioned_targets().last()` (write/delete → `path`, move/copy →
  `destination`) and cannot drift.
- **Conflict check and error surface.** `WriteGate::submit` loops declared
  claims vs the per-target captured pre-images; any mismatch (including a
  target that no longer exists) produces `GateError::Conflict` before any WAL
  append — zero whiteboard rows for a conflicted write; it is never logged as
  applied and never silently dropped. Declare-wins is per target: the stamp
  never clobbers a caller-declared claim, even a stale one (the gate surfaces
  the conflict, not the stamp). `GateError::Conflict` maps to
  `IpcErrorCode::Conflict` (`-32005`, `ipc.rs::IpcError::from_gate`); both the
  supervised child (`gate_proxy.rs::gate_proxy_to_tool_error` /
  `gate_rejection_message`) and the in-process backend surface the
  byte-identical retryable tool-error string, so agents see one error surface.
  A pre-image read failure during stamping leaves no claim for that target
  plus a `tracing::debug!` (observable degradation, never silent); the
  in-process backend also emits a `tracing::warn!` when it observes a
  `GateError::Conflict`.
- **Known limitation (by design, documented not fixed).** Fresh-create
  concurrent creates (target never existed → no prior state) remain
  last-writer-wins: there is no prior version to conflict with.
- **Audit note.** The `supervisor_conflict` e2e suite is heavy (~24 min)
  because of the kill-9/sleep crash-injection tests; runtime is being watched.

### D3 implementation notes — whiteboard subscription push (2026-08)

Completing the supervisor → agent `whiteboard_slice` push surface (D2) and the
D3 subscription machinery (topics, at-least-once, per-subscriber persisted
cursors, bounded queues) on top of the delivered vertical slice. These notes
extend D3 and the D2 message list additively; the decision text above is
unchanged.

- **Wire surface (protocol 0.2.0, additive).** Two new `IpcMethod` variants,
  kebab-case on the wire:
  - `WhiteboardSlice` — supervisor → agent `IpcNotification`
    (`params: { subscription_id: String, events: Vec<WhiteboardEvent>,
    end_gate_seq: u64 }`). `events` is the contiguous, total-ordered run of
    whiteboard events for the subscribed scopes with `gate_seq >
    cursor_gate_seq` up to `end_gate_seq`.
  - `AckWhiteboard` — agent → supervisor request (`params: { end_gate_seq:
    u64 }`, empty result). The supervisor persists
    `cursor_gate_seq = max(cursor_gate_seq, end_gate_seq)`.
  - Protocol `PROTOCOL_VERSION` bumps `0.1.0 → 0.2.0`; both sides reject on
    mismatch exactly as today (additive change, no renegotiation logic).
- **Registration.** The handshake payload gains a **wire-optional**
  `subscriptions: Option<Vec<WhiteboardScope>>` field on
  `IpcParams::Handshake` (scope = a `WhiteboardKind` topic list). `Option`
  is the back-compat mechanism by construction: the Handshake params carry
  no `#[serde(default)]` today, so a required field would reject every
  older handshake; an absent field on the wire deserializes to `None` — the
  same additive contract as `#[serde(default)]` on
  `GateRequest.base_versions`. Scopes are config-owned at spawn (ADR-58/59
  roster convention): `SupervisorConfig` per-agent scope list → spawn env →
  the agent process declares them in the handshake. Subscriber identity is
  the registered `agent_id`; a restarted agent re-registers with the same
  id and rehydrates its cursor.
- **Cursor persistence.** New session-DB migration `027_whiteboard_subscriptions`:
  `(subscriber_id TEXT PRIMARY KEY, scopes TEXT NOT NULL, cursor_gate_seq
  INTEGER NOT NULL DEFAULT 0)`. The cursor is a `gate_seq` consistent-cut
  coordinate ("everything ≤ cursor already acked").
- **Delivery and backpressure.** A supervisor-side `SubscriptionManager`
  observes each gate append; for every subscriber whose scopes match, the
  new event run is enqueued to that subscriber's bounded mailbox (64 events
  / 256 KiB). The persisted cursor advances **only on the agent's
  `AckWhiteboard`** — never at enqueue. If a mailbox is full, the manager
  skips enqueue (delivery stalls; the cursor has not advanced, so nothing is
  lost). On registration/restart the manager drains from the persisted
  cursor and resumes pushing. Agent-side dedup is free: slices are
  contiguous in `gate_seq`, so the agent keeps a per-subscription high-water
  mark and ignores any event with `gate_seq ≤ high-water` (crash between
  apply and ack → redelivery is idempotent by construction).
- **Client restructure — concurrency model, not an additive task.**
  `GateProxyClient` today is a strictly single-reader sequential client:
  `request()` reads stdio *inline* (owning the framing carry buffer),
  answers supervisor-initiated heartbeat pings out-of-band while awaiting a
  response, rejects any other supervisor-initiated request with
  `InvalidRequest`, and silently skips unknown notifications; the process
  holds it as one `Arc<Mutex<..>>` with at most one request in flight.
  stdin has exactly one reader, so the design below is a restructure of
  that model, not a second task bolted onto the existing loop:
  - a single background reader task becomes the **sole owner of stdin and
    the carry buffer**, and takes over id correlation and heartbeat
    answering (both move out of `request()`);
  - `request()` becomes send-then-await on a per-request oneshot that the
    reader resolves by correlating response id — the one-in-flight
    invariant is preserved by the caller loop's sequential await, as today;
  - outbound writes serialize behind a single writer (stdout has one
    writer; requests, acks, and outbound heartbeats share it);
  - `WhiteboardSlice` notifications are dispatched to the bounded slice
    channel; any other unknown notification keeps today's silent-skip
    behavior; unknown supervisor-initiated requests still get the
    `InvalidRequest` error response;
  - the consumer applies the slice to working memory (D6) and acks via the
    same serialized outbound path; `whiteboard_slice()` exposes the
    consumer channel.
  - Back-compat anchor: the supervisor side of the protocol is unchanged;
    the existing `supervisor_agent_process` tests pin the request/response,
    heartbeat, and rejection behavior across the restructure.
- **Testing obligations (land with the slice).** e2e: (1) publisher → subscriber
  ordering — two agents on one run, subscriber receives the publisher's
  write events in `gate_seq` order; (2) crash/recovery — kill -9 the
  subscriber between slice delivery and ack, restart, verify no loss and no
  duplicate apply (high-water dedup); (3) backpressure — mailbox overflow
  drops delivery, cursor stalls, resume-from-cursor re-delivers.
- **Deferred, documented not built.** Topic *hierarchy* and
  filter-by-relevance/recency (D6 disclosure), cross-subscriber
  backpressure signalling, and schedule-driven pushes all remain behind
  Deferred item 4. The raw log remains append-only and untouched; slices are
  projections.

### D6 implementation notes — consolidation (2026-08)

Minimal thin-slice consolidator as landed in Phase 4 (`787df6e`). These notes
extend D6; the decision text above is unchanged.

- **Out-of-band trigger, never blocking the gate.** The supervisor's write-path
  handlers count appends; every `CONSOLIDATION_TRIGGER_APPENDS` (= 16) appends
  detach ONE consolidation pass onto the tokio runtime, fire-and-forget. The
  gated-write / publish reply path never awaits indexing work; triggers are
  coalesced while a pass is in flight (at most one pass runs at a time).
- **Fold into the hybrid store, bi-temporal.** Each pass folds foldable events
  (`Decision`, `PlanApproved`, `ReviewState`) into `SqliteVectorStore` with
  bi-temporal metadata (world-time from the folded events vs ingestion-time of
  the projection). The raw log is never summarized away.
- **Content-derived ids + bookmark watermark → idempotent.** Chunk ids derive
  deterministically from project + group + watermark `gate_seq`, so a crash
  between storing a chunk and recording its `Consolidation` bookmark converges
  on re-run instead of duplicating; each pass records ONE bookmark event whose
  watermark is where the next pass resumes.
- **Invalidate-not-delete with provenance.** A newer projection tombstones the
  previous chunk (rows retained) and cites `superseded_event_ids` /
  `superseded_chunk_ids` in its own provenance, keeping the audit trail
  unbroken.
- **Deterministic feature-hash placeholder embedder.** Projections are
  retrievable without a downloaded model via bag-of-tokens feature-hash vectors
  (`model_id: "feature-hash"`); swapping in real embeddings later changes no
  contract and stays behind Deferred item 4.
- **Disclosure clamp.** The supervisor's `retrieve-memory` shortlist clamps to
  `DISCLOSURE_MAX_CHUNKS` (= 10) chunks — one disclosure level, per D6.
- **Files:** `crates/orchestrator/src/consolidation.rs` (`Consolidator`,
  constants above), `crates/orchestrator/src/supervisor.rs` (write-path trigger,
  retrieval clamp), `crates/memory/src/vector_store.rs` (`SqliteVectorStore`
  projection target).

### D4 implementation notes — gate fairness (2026-09)

Closing the "weighted round-robin fairness across agents (D4)" deferral that
the write-gate module docs carried since the vertical slice. These notes
extend D4; the decision text above is unchanged.

- **The requirement, verified against the delivered gate.** D4's fairness
  requirement is "no chatty-agent starvation". The delivered `WriteGate`
  satisfies it structurally: every agent owns a private FIFO in-flight
  limiter (cap = `max_in_flight_per_agent`, 1 in production), demand beyond
  the cap parks on the agent's *own* semaphore, agents never contend for one
  another's permits, and tool execution runs concurrently across agents.
  There is no cross-agent queue inside the gate, so one agent's backlog can
  neither delay nor reorder a sibling's write. Pinned by
  `per_agent_limiter_bounds_concurrency_but_agents_are_independent`
  (`crates/orchestrator/src/gate.rs`): while a chatty agent holds its permit
  with two writes parked behind it, a sibling's write completes within a
  bounded wait and is sequenced (`gate_seq`) ahead of the parked backlog.
- **Backpressure and caller shape.** In-flight gated ops are bounded per
  agent (total bound: roster size × cap) and no other queue exists. The
  wired callers add no buffering: the supervised child is a strictly
  sequential one-request-at-a-time client (`gate_proxy.rs`), and the
  in-process loop awaits each tool call, so each agent holds at most one
  write in flight in practice; backpressure propagates to the agent loop
  instead of accumulating at the gate. The supervisor's steady-state loop
  additionally drains each agent at `MAX_EVENTS_PER_TICK` per tick —
  per-agent fair by construction.
- **The one cross-agent serialization point** is the whiteboard WAL append
  (`gate_seq` assignment under SQLite's `BEGIN IMMEDIATE` write lock).
  Appends are single short transactions; contention is bounded by the pool's
  `busy_timeout` and surfaces as a `GateError::Whiteboard` error — never as
  silent deferral or reordering.
- **Why weighted round-robin is superseded, not shipped.** WRR presupposes a
  serialized gate queue it can fairly interleave (the research brief's
  "single-writer thread" gate). The implemented gate is deliberately
  non-serialized: per-agent isolation plus concurrent cross-agent execution
  was the chosen concurrency model (the module docs' "agents do not block
  one another" contract). Scheduling WRR there would first require
  inventing contention — a global execution cap serializing what today runs
  concurrently — a throughput regression that rations a resource nobody
  contends for, to fix a failure mode (cross-agent starvation) that
  structurally cannot occur. The mechanism is therefore rejected; the
  requirement it served is met and pinned by test.
