# Specialist continuation in the desktop

The Coordinator continues an existing task with the same task ID. This dispatch
publishes `SubTaskStarted` without another `SubTaskCreated`. Previously the
desktop translated Started into a thought, leaving a held graph row and settled
chat card looking inactive during the continuation.

The desktop bridge now preserves Started as a typed local lifecycle event.
Routing reactivates the exact task's graph row and chat card, updates its current
specialist after reassignment, and keeps prior logs and exclusively owned
transcript entries. The original card position stays fixed. Repeated creation
events update the existing graph row/card rather than duplicating the row or
moving old work later in the conversation. Different task IDs remain separate
even when they use the same specialist.

Blocked settlement closes the chat card's running indicator. A subsequent
Started reopens it. Completion replaces the current graph summary with the actual
completion outcome. The graph details show the current summary so an unfinished
reason is visible before opening logs.

Graph persistence already carries stable task IDs, states, and logs. A restored
held row reactivates in place; if no creation or saved graph exists, Started
creates an addressable placeholder row/card for subsequent lifecycle events.
Chat entry ownership remains transient, as before; this change does not invent
historical ownership for a transcript restored after restart.

## Scope and validation

Only desktop event translation/routing and the graph/chat views change, plus
this documentation. Backend events, shared harness schemas, checkpoint/resume
wiring, runtime construction, verification policy and CLI are unchanged.
The change is independent of PRs #312 and #315 and branches from pinned dev
`0d13b6aa9a9ecf5f73cd9c9f7a485ba7ecf41e63`.

Regression coverage exercises backend lifecycle translation through both views,
held and blocked continuation without another creation, retargeting, graph
save/load, history and card-anchor retention, distinct same-role tasks, and
Started without saved creation. Rust CI is required because the editing
environment has no Rust toolchain.
