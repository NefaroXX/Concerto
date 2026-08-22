# OVERVIEW
Multi-agent orchestrator coordinating specialist agents with write gates and cycle detection.

## STRUCTURE
```
orchestrator/
├── lib.rs              # crate exports
├── agent_loop.rs       # single-agent execution path (future agent-process entry, ADR-60)
├── agent_runner.rs     # agent execution runner
├── coordinator.rs      # multi-agent dependency-ready coordination
├── runtime_runner.rs   # shared runtime construction and frontend entry
├── supervisor.rs       # process-supervision core (ADR-60 S4→S5): lifecycle, steady-state loop,
│   │                   #   write-path dispatch (gate/whiteboard/memory) via SupervisorServices;
│   │                   #   Completed: clean exit 0 after handshake is terminal, never restarted
├── exec_backend.rs     # ToolExecutionBackend seam (executor call-site abstraction for tools)
├── gate_proxy.rs       # client/backend/memory facade used by the agent-process child (ADR-60 S5)
├── gate.rs             # single write gate (ADR-60 D4): policy eval, WAL-before-execute,
│   │                   #   replay dedup, pre-image hashing
├── ipc.rs              # supervisor/agent stdio protocol (ADR-60 D2/D3): JSON-RPC 2.0 framing
├── bin/mock_agent.rs   # mock-agent fixture for supervisor integration tests (env knobs)
├── bin/agent_process.rs # real single-agent AgentLoop as supervised child (ADR-60 S5): env contract
├── tests/supervisor_agent_process.rs  # e2e: real agent-process driven through the supervisor write gate
├── agents/             # one generic specialist agent (all roles are config seeds, ADR-35)
│   ├── mod.rs          # exports (GenericSpecialistAgent)
│   └── generic.rs      # Freeform / DesignDoc / ResearchReport / ReviewReport modes + eval-runner
├── graph.rs            # execution graph construction
├── state.rs            # orchestrator state model
├── conflict.rs         # conflict resolution
├── cycle.rs            # cycle detection
├── cycle_manager.rs    # cycle handling
├── planner.rs          # task planning
├── memory_serial.rs    # memory serialization
├── testing.rs          # test harness
├── registry.rs         # agent registry
├── relationship.rs     # configurable directed collaboration rules
├── session_manager.rs  # run/session lifecycle helpers
├── prompts.rs          # built-in prompts
├── delta.rs            # state deltas
├── cost.rs             # cost tracking
└── hash.rs             # content hashing
```

## WHERE TO LOOK
| Concern | File(s) |
|--------|---------|
| Single agent loop | `agent_loop.rs` |
| Agent runner | `agent_runner.rs` |
| Coordinator logic | `coordinator.rs` |
| Runtime construction | `runtime_runner.rs` |
| Specialist agents | `agents/*.rs` |
| Execution graph | `graph.rs` |
| State model | `state.rs` |
| Conflict resolution | `conflict.rs` |
| Cycle detection | `cycle.rs`, `cycle_manager.rs` |
| Write gates | `gate.rs` (`WriteGate` — single write chokepoint, ADR-60 D4; optimistic `base_version` conflict checks, ADR-60 D5); legacy capability gating via `AgentCapabilities::fs_write` (executor policy) |
| Supervisor | `supervisor.rs` (`Supervisor`, `SupervisorConfig`, `SupervisorServices`) + `ipc.rs` protocol |
| Supervised agent-process (ADR-60 S5) | `src/bin/agent_process.rs` + `gate_proxy.rs` + `exec_backend.rs` |
| Testing utilities | `testing.rs`; supervisor fixtures in `bin/mock_agent.rs`, `tests/supervisor_agent_process.rs` (real agent-process e2e), `tests/supervisor_*.rs` |
| Agent registry | `registry.rs` |
| Relationships | `relationship.rs` |
| Task planning | `planner.rs` |
| Memory serialization | `memory_serial.rs` |

## CONVENTIONS
- **Agent trait** - specialists implement `ExpertAgent` and are resolved by
  `AgentRegistry`
- **Cancellation** - every long operation accepts `CancellationToken`
- **Errors** - propagate `OrchestratorError`/domain errors; preserve recoverable
  failures as retry context or blocked/partial outcomes
- **Logging** - `tracing` with per-agent targets (`orchestrator::agent::<name>`)
- **Cost tracking** - all paths share the session/core `SpendTracker`; provider
  metrics are retained on agent results
- **Testing** - `testing.rs` provides integration test scaffolding

## ANTI-PATTERNS
- Avoid `unwrap` or `expect` outside tests; every fallible call returns error
- Do not place heavy computation in coordinator; delegate to specialists
- Never write files directly; Coder uses `ToolExecutor`/filesystem tools and
  other specialists remain write-gated
- Do not dispatch all planned roles at once; only `TaskGraph::ready_tasks()` may
  enter a batch
- Skip global mutable state; keep all mutable data in `OrchestratorState`
- Refrain from deep nesting in `conflict.rs`; use early returns
- Do not import entire `agents` module; import only needed agents
- Do not bypass cycle detection; all agent runs go through `cycle_manager`
