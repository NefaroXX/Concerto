# H39/H40 investigation — 2026-11-15

Investigation in the isolated `h39` worktree, now based on published H38 commit
`14390eb`. The five baseline tests are wired and have run. All failed at the
expected loss assertions, with production implementation unchanged.

H39: `AgentLoop::persist_run_success` and `persist_run_partial` store summary
messages. `decide_exit` assigns the full vector to `initial_messages` only.
Coordinator dispatch history is local to `run_dispatch_session`; shared runtime
writes terminal summary messages. Existing store tests prove serialization of
messages explicitly passed to the store, not production transcript capture.

Prepared two real-loop regressions: a tool-call response and matching tool
result must survive both successful settlement and a later provider error.
The signature assertion uses the existing ToolCall signature field, so this
is independent of the new Gemini native-parts representation.

Potential H40: `runtime_runner::multi_agent_task_with_history` clips every
message at 4,000 characters and omits old messages after 16,000 characters.
`run_multi_agent` selects it when there is no conversation inbox and no approved
plan. The interactive inbox branch gets full typed history; headless requests
therefore have different instruction continuity. Reproduce loss of an explicit
constraint after the per-message cutoff and loss of an older obligation before
changing this path. Approved-plan semantics require separate review.

Next design work: distinguish provider transcript from visible UI/event
transcript, define durable append identity/cursors, avoid duplicate user input
and usage accounting, isolate coordinator and specialist histories, retain
completed exchanges on errors/cancellation, and verify restart through actual
runtime history assembly. Do not implement summary substitution as full replay.

Prepared H40 regressions for both cutoffs in `history_continuity_regressions.rs`;
they are also unwired and unexecuted until H38 verification releases Cargo.

## Persistence and display boundaries inspected

- `conversation.rs::receive` appends delivered user messages before exposing
  them to the model; a new journal must not append those inputs a second time.
- `conversation.rs` loads `ProjectSessionManager::load_recent_messages` before
  each shared runtime run. That method currently delegates to `load_messages`.
- `runtime_runner_persistent.rs` assembles compaction from the same store before
  dispatch, then maintains checkpoints from it after settlement. Compaction
  cannot recover provider messages that the runner never stored.
- `runtime_runner.rs::run_multi_agent` stores the initial user input and a
  terminal summary (or failure summary). Its inbox branch passes typed history
  to the coordinator; the other branch uses the clipped task-description
  embedding above. Approved-plan runs intentionally use a separate artifact.
- `conversation/reply.rs` also stores visible status replies as assistant
  messages. If full coordinator tool-call history is stored, these must not
  become duplicate model turns between an assistant call and its tool result.
- CLI restoration prefers the separate typed `transcript` table. Only legacy
  sessions without entries fall back to `messages`, skipping tool/system roles
  and blank assistant content. Native provider history and visible transcript
  already have distinct representation paths; preserve that separation.
- Current single-agent tests explicitly assert aggregate run usage on a terminal
  summary row. A per-provider-turn journal needs revised accounting evidence:
  do not count the same usage on both original turns and a summary mirror.

Design remains open. A correct repair needs tests for delivery ordering,
coordinator status replies, partial/failing settlement, usage totals, restart,
and isolated specialist histories. Saving all specialists into the root
conversation would introduce unrelated tool-call/result sequences and is not a
valid shortcut. Persisting only at successful settlement would still lose
completed exchanges when the next provider request fails.

## Required integration evidence before closing H39/H40

1. Run the prepared baseline regressions without implementation changes and
   preserve their failing output and source hashes. The success case must
   actually complete; the failure case must reach the second provider request.
2. After repair, restart from persisted history through context assembly and
   capture the next provider request. Assert roles, text, call IDs, matching
   results, reasoning and native replay, rather than only checking stored rows.
3. Exercise a coordinator status reply followed by work: the reply must remain
   visible once, and the provider call/result sequence must remain coherent.
4. Compare input usage and output usage with actual provider reports; do not
   duplicate whole-run usage on a synthetic summary after recording turns.
5. Repeat with a delivered follow-up, a later provider error and cancellation.
   Persist completed exchanges; never commit an unfinished provider attempt.
6. H40's helper-level probes establish the old cutoffs only. Final regression
   coverage must capture the headless provider request, especially if the fix
   removes the string-based history helper in favor of typed messages. Include
   the interactive path as a parity control and inspect approved-plan behavior.
7. Preserve the complete UI transcript separately. Isolate specialist histories
   by task/agent so their internal calls cannot enter the coordinator's stream.

No repair implementation is committed here; these are test and design requirements.

## Additional coordinator baseline probe

Prepared `coordinator_transcript_regressions.rs` with a real coordinator and
session store: a signed status-reply tool call followed by a final answer must
produce one visible status reply and persist the original call immediately
followed by its matching result. This checks the display/model-history boundary
directly. Like the other probes it remains unwired and unexecuted while H38's
full local battery owns Cargo compilation.

Further source inspection confirms `conversation::reply` currently appends a
plain assistant display message to the same messages store, while coordinator
dispatch never appends its original assistant/result sequence. A repair must
coordinate both writes; simply adding persistence to dispatch would create an
extra assistant message between a call and its result.

## Candidate repair boundary (not yet selected or implemented)

Prefer reusing the existing canonical `messages` store and separate visible
`transcript` recorder before introducing another journal schema. Append original
root-agent turns and executed results at completed-exchange boundaries, replacing
summary-only settlement. Keep specialist histories isolated. Status-reply events
already feed the visible recorder; their plain assistant mirror must not pollute
the canonical exchange. Initial user input and inbox-delivered follow-ups already
have persistence owners and must not be appended twice.

This needs an explicit policy for interruption during a multi-call exchange:
completed side effects must remain evidenced, unmatched calls must not replay as
completed results, and restart must never automatically repeat a tool merely to
fill a missing result. Persisting only after the entire run succeeds is excluded
by the prepared second-provider-failure regression. Text-driver repair messages
and internal continuation instructions also need ordering and duplication tests.

Session-wide token totals and spend queries use `sessions` / `spend_records`,
separately from message rows (`sessions/src/lib.rs` update around 1354 and spend
query around 1610). That permits per-turn usage on canonical assistant messages
without retaining a second aggregate summary row, but the runtime metric writes
and existing accounting tests must still be checked together. No accounting
behavior has been changed in this preparation.


## Baseline results — 2026-11-16

All three transcript regressions failed at the missing original provider-turn
assertion after their setup/outcome checks passed. Both headless history probes
failed at the missing-constraint assertions. Neither command had a compilation
error. The two commands returned 101, and all 1,013 baseline source hashes
matched. Evidence: main repository `target/harness-reliability/h39-baseline-*`.
The earlier preparation notes describe sequencing before this baseline run.

Next: strengthen restart coverage through `ContextEngine::assemble` and a
captured provider request, then repair persistence ownership and headless
history selection. The completed-exchange failure cases are proven; cancellation
mid-batch and interrupted-call replay still require explicit test/design work.
Do not infer those guarantees from the five baseline probes.


The request-level single-agent probes now also fail as expected: a fresh loop
receives history from `ContextEngine::assemble` with no supplied fallback and
captures its actual provider request. Original completed exchanges are missing
after both successful settlement and the later provider error. The fixture
includes native Gemini state. These runs use an in-memory SQLite store; do not
label them as an on-disk database or process restart. Preserved evidence uses
`h39-request-baseline-*` in the main target directory.


## Root-conversation repair — 2026-11-16

The selected boundary is documented in [ADR-87](adrs/ADR-87-root-provider-transcript.md).
The disk-reopen probes now pass, including native replay state and exactly-once
per-turn usage accounting. Completed tool results survive cancellation. Recovery
adds unknown-outcome responses only to assembled requests, preserving original
stored evidence. Rendered status replies are verified through the actual
transcript recorder and no longer insert assistant mirrors into model history.

H40's baseline probes now live in the separate `h40` worktree. H39 remains open
for independent specialist/supervised histories: the supervised child constructs
its real `AgentLoop` without a session store or prior messages. Parent summary
persistence cannot substitute for that missing agent-scoped history. Full root
milestone verification and exact-head CI are recorded in the work log, not
inferred from focused regression passes.
