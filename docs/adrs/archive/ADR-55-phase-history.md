# ADR-55 phase history — addenda 1d–2e (verbatim record)

> **Status:** Archived — historical record only; not current guidance.
>
> This file preserves, verbatim, the phase-by-phase addenda that accreted on
> [ADR-55](../ADR-55-intent-routing-and-authorization.md) after its original
> body, plus the three amendment blocks that accreted on
> [ADR-56](../ADR-56-model-first-intent-classification.md). On 2026-09-28 the
> two ADRs were consolidated in place so each reads as one continuous
> decision: every still-in-force decision was folded into the live body of the
> ADR it governs, and fully superseded addenda (notably Phase 2d's
> auto-grant-then-branch, superseded the same week by Phase 2e) were removed
> from the live files. In-force decisions live in the live ADRs; this file is
> the decision trail and the implementation evidence (commits, live-fix
> rounds, acceptance lists) that produced them.
>
> **Supersession map, in one line per block:**
>
> - **1d** (plan-approval machinery) → folded into ADR-55 §4/§5/§6; durable
>   `plan_bindings` (2b §7) supersedes the process-scoped registry as the
>   operative store.
> - **1e** (picker removal, gate always-on) → ADR-55 §7.
> - **2b** (planning-role orchestration, rounds 1–5) → Rounds 1–5 are
>   implementation evidence; the planning-role path and checkpoint precedence
>   fold into ADR-55 §4; the planner-roster contract point is superseded by
>   ADR-71; the §12 arming-fallback chain was superseded by the
>   exact-objective auto-Apply (ADR-55 §4).
> - **2c** (classifier component) → the never-grant invariant and threshold
>   binding fold into ADR-55 §10 / ADR-56; the config keys were retired at
>   schema v8.
> - **2d** (auto-grant-then-branch) → **superseded as dispatch logic by 2e**
>   the same week; the rejection reasoning is preserved in ADR-55 §1;
>   the exact-objective auto-Apply (§3) survived into ADR-55 §4.
> - **2e** (unified agent loop) → ADR-55 §§1–3/§8; the `cycle_manager`
>   reconciliation note (removal, 2026-09-24) is folded into ADR-55 §1.
> - **ADR-56 amendments** (2026-09-06 auto-grant — moot and removed;
>   2026-09-09 retirement; 2026-09-11 config-surface removal) → ADR-56
>   "Current state" / §4 / §8.
>
> Nothing here is live guidance; nothing here was deleted.

---

## ADR-55, Addendum (Phase 1d) — verbatim

Phase 1d plan-approval decisions, reviewed as a settled sequence in the
intent-gate commit range `6ae5a92..b8f1a1d`. Items marked **pending** land with
1e (v2). **Status stays Proposed:** this ADR flips to Accepted only after the
re-scoped Phase 1 lands, including 1e (classifier compliance and shell
argv/cwd containment moved to Phase 2).

### 1. §3 diff-verification deferred to 1e — pending

ADR-55 §3's "diff shown to the user is verified against the expected plan
artifacts (ADR-52 `PlanArtifact` / checkpoint fields)" is **not satisfiable for
single-agent in 1d**: `PlanArtifact` is multi-agent-only, and single-agent plans
are answer-only runs with no artifact. 1d ships the load-bearing binding
contract — `plan_id`, `objective_hash`, `source_revision` — plus `plan_text` in
the dialog. **Pending (1e/v2):** `PlanArtifact` write + diff-vs-artifact
verification. The dialog remains a real `ApprovalSink` call (audited, blocking),
as §3 demands.

### 2. Binding registry — process-scoped, keyed by (session_id, objective_hash)

Process-scoped in-memory registry, keyed strictly by
`(session_id, objective_hash)`; newest-wins per key (re-planning replaces).
Insert happens **post-run**, only when a Plan-effective run returns `Ok` with a
non-empty final message; `plan_text` is capped at 16 KiB. A lookup miss yields
the generic Execute prompt — never a stale plan gate. This is a **session-level
decision record**, distinct from §4's run-scoped grant object: bindings
deliberately survive across runs; grants do not.

### 3. Interception — where the gate would prompt, only with a pending binding

"Apply it?" fires exactly where `apply_intent_gate` would prompt —
`routing.outcome == Execute`, confidence above the threshold, mode
action-required capable — and only when a pending binding exists. **Apply** =
audited authority replacing the generic confirmation (grants fs+git like a
confirmed Execute). **Replan** = read-only answer-only run routed to Plan;
binding replaced by the new plan. **Dismiss** (`None`) = read-only answer-only,
effective Execute, audited `"dismissed"`. No mutation is possible without an
explicit Apply.

### 4. Audit — `record_plan_decision` seam, no schema change in 1d

New `record_plan_decision` seam: synthetic `tool_name = "intent:plan"`, decision
in `rule_matched`/`verdict`, `input_hash = objective_hash`, and `user_response`
JSON `{ plan_id, source_revision }`. **Pending (1e/v2):** schema-derived columns
+ artifact persistence.

### 5. Source revision — git HEAD via the pre-existing helper

Single-agent captures git HEAD via the pre-existing `current_source_revision`
helper (`git rev-parse HEAD`); `"unknown"` for non-git worktrees. The dialog
shows "plan made at rev X, current rev Y" when both are known.

### 6. Known v2 items — noted, not decided

- Relative utterances ("apply that") need conversation-context resolution for
  the binding lookup.
- Multi-binding per objective (keying by `objective_hash`).
- A shared `correlation_id` between the routing and plan-decision audit records.
- Stop-eviction policy wired to a future `Stop` hook (`clear_session` exists,
  unwired).

---

## ADR-55, Addendum (Phase 1e) — verbatim

Phase 1e gate-required decisions, oracle-reviewed and settled: picker removal
with the gate as the only routing path (§1), gate coverage of all runs (§2),
outcome → prompt mapping (§3), Phase 1 re-scope (§4), the config breaking
change (§5), and the flip acceptance (§6). **Status stays Proposed** until the
flip lands (§6).

### 1. Picker removal, gate-required — the gate is the only routing path

The Build/Chat/Plan picker is removed from desktop (chat picker + `SetMode` +
config write), CLI (`SettingsField::InteractionMode`), and the config schema
(`AppConfig.mode`); `AgentMode` is deleted from core. The intent gate is now
the **only** routing path and is always-on: the `[intent] enabled` toggle is
removed. Unclassified/ambiguous prompts land in the AskUser dialog (all six
outcomes). `force_single_agent` remains config-driven as the sole
execution-style lever (single-agent gate vs multi-agent coordinator
governance).

### 2. Gate covers all runs — B1 amendment

The gate now governs **single- and multi-agent runs alike** (amending the
Context's "multi-agent path dispatches independently of mode"). The shared
executor's `SessionIntentAuth` starts read-only, so the gate prompt is the
only way multi-agent mutations stay authorized. Multi-agent task shape derives
from the same effective outcome: `Execute` + `!read_only` → full topology;
otherwise text-only / coordinator-only; `Plan` stays text-only with the Plan
prompt — there is no planning-role path. Consequences: multi-agent
unclassified inputs now hit the AskUser dialog (previously zero prompts);
multi-agent `Execute` runs may hit the plan-binding Apply/Replan path (1d §3);
multi-agent `Plan` runs now produce plan bindings.

### 3. Outcome → prompt mapping

`system_prompt_for(RequestedOutcome)`: `Execute` → Build prompt, `Plan` → Plan
prompt, all other outcomes (including AskUser and the wildcard arm) → Chat
prompt. Prompt texts are preserved as core consts; eval-runner shares the
Build const.

### 4. Phase 1 re-scope — B2

Phase 1's original scope included classifier compliance (§6/§9) and shell
argv/cwd containment (§2's shell scope hole) — both are **deferred to Phase 2**
(v2). Rationale: deterministic routing plus the user-confirmation gate is the
load-bearing security boundary; the LLM classifier remains proposal-only. The
two 1d-pending items close as v2: the §3 diff-vs-`PlanArtifact` verification
(1d §1) and the schema-derived audit columns (1d §4).

### 5. Breaking change — silent opt-out loss

Existing config files with `[intent] enabled = false` silently lose the
opt-out: the key is ignored at load (no `deny_unknown_fields`),
`SCHEMA_VERSION` bumps 5 → 6, and v6 drops the field. No kill switch is
provided — deliberate, per the gate-required decision (§1).

### 6. Acceptance — the Status flip

The ADR flips **Status → Accepted** (recorded with the landing commit SHA) once
this addendum and the 1e code land; the flip is a separate docs commit.

---

## ADR-55, Addendum (Phase 2b) — verbatim

Phase 2b planning-role orchestration decisions, oracle-reviewed and settled:
planning-only orchestration depth (§1), checkpoint precedence (§2), plan
deliverable & binding (§3), cost envelope & failure semantics (§4), non-goals
(§5), and the acceptance test list (§6). **Status stays Accepted** — the ADR
already flipped at 1e §6; this addendum records the Phase 2b scope with no
further flip.

### Orchestration depth per outcome — planning-role path (2b)

### 1. Scope — planning-only orchestration depth

Multi-agent `Plan` runs now run the **real coordinator** in a new
`OrchestrationDepth::PlanningOnly` mode — a builder field on `CoordinatorAgent`
(default `Full`; `CoordinatorAgent::run` signature unchanged). Planning-only
executes memory retrieval + the design stage (full recovery ladder) +
`TaskPlanner` with the **FULL registered-agent roster** — the planner contract
requires implement roles and at least one Coder task, which settles that
registry-subsetting was refuted — plus graph validation (dependency-resolvability
only). It then RETURNs the rendered plan — design-doc summary + per-subtask
role/description/dependencies — as the run's final message and persists the
`PlanArtifact` (`persist_plan_artifact`, ADR-52), closing the first half of the
1d §1 pending item. No `execute_graph`, no review, no validation throughout;
zero tool grants beyond the run's read-only intent gate. Every other
non-action-required outcome keeps the text-only branch unchanged.

### 2. Checkpoint precedence — load-bearing

Planning-only passes `None` for resume and never writes or clears the session's
orchestration checkpoint, so an in-flight partial Execute run's crash-recovery
checkpoint is preserved. The 1d Apply path CLEARS/suppresses a stale checkpoint
so an Execute re-run of the approved objective re-plans from the approved plan
instead of silently resuming the old partial graph. Regression test mandatory.

### 3. Plan deliverable & binding

The binding's `plan_text` is the rendered plan — not a completion placeholder;
the empty-final-message guard already prevents failure bindings. `plan_id`
references the persisted `PlanArtifact` id where available (1d §4 audit
consistency). The Plan stage is emitted; the 2a stage feed must NOT advance to
Execute during planning-only runs. Regression test: Plan→Complete, never
Execute.

### 4. Cost envelope & failure semantics

Nominal cost is 2 model calls (Architect + planner ≤ 2048 out-tokens). The
failure path is the existing design recovery ladder (`max_subtask_attempts` +
escalation + fallback tiers); a design failure returns Partial with no binding.
Planning-only still publishes `MultiAgentModeCompleted` at its terminal (S3).

### 5. Non-goals — explicit

Verify / Review / Diagnose / Answer remain text-only: those outcomes operate on
existing work, the coordinator fabricates nothing, and a review/verification of
nothing is dishonest and costly. No registry-subset matrix (refuted by the
planner contract). Single-agent Plan behavior is unchanged — a full answer-only
`AgentLoop`.

### 6. Acceptance — oracle test list

- **T1** — plan rendered + binding equality.
- **T2** — zero tool grants in planning-only.
- **T3** — stage Plan→Complete, never Execute.
- **T4** — stale checkpoint ignored by planning-only, cleared by Apply (regression).
- **T5** — coordinator dispatches only architect + planner.
- **T6** — failure envelope Partial / no binding.
- **T7** — planner fallback still renders a plan.
- **T8** — multi-agent Execute path unchanged.
- **T9** — approval follow-up (live-fix): a natural-language approval of the
  rendered plan ("i approve the plan", "apply it", ...) arms the same audited
  Apply/Replan dialog through the session-wide newest binding (M3 binds the
  rendered plan under the *current* input's hash, and `plan_approval`'s new
  `latest_for_session` resolves across objectives), instead of re-triggering a
  fresh planning run. Exact-objective replay also arms the dialog under any
  non-`Answer` routing (the router re-classifies a `plan1:` prompt as
  `Diagnose`, and a replay must not silently re-analyze). Change-execution
  phrasing ("apply the fix") without a matching objective never arms it.
- **T10** — fallback durability: the heuristic-pipeline fallback (planner JSON
  parse failure) persists a `PlanArtifact` built from the generated graph, so
  planning-only runs bind a real `plan_id` and `plans/` is never left empty
  (observed live before the fix).
### 7. Durable bindings + router elevation (live-fix round 2)

Round-2 live evidence (data dirs `/mnt/Temp/concerto`, `/mnt/Temp/concerto2`,
two-message sessions "plan: …spec…" → "I approve" / "i approve the plan"):

- **Artifact durability fixed** (T10) — `plans/plan-*.json` persisted in both
  runs. The remaining failures were routing + binding durability.
- **Router ate the plan request**: the 6-gate verdict spec contains negation
  wording ("don't use unwrap…") AND the word "error" (an exit-code contract),
  so `negation_override` demoted the `plan:` prompt through
  `read_only_outcome`'s Diagnose check → a *Diagnose* run, no plan rendered,
  no binding. Fix: plan-family keywords (`plan`/`plans`/`planning`/`proposal`/
  `blueprint`/`roadmap` — deliberately excluding `design`, which appears in
  "without changing the design of X") elevate to `Plan` above the Diagnose/
  Review checks in the read-only branch. Planning is read-only by
  construction, so the elevation never grants write access.
- **Bare approvals now phrases**: "I approve" (without "the plan") routed
  `AskUser` and the generic intent dialog surfaced "could not identify
  intent" — the phrase list had no bare forms. Added `i approve`, `yes`,
  `approved`, `go ahead`, `proceed`, … with a negation guard (`don't`,
  `not yet`, `never`, …) so denials never arm the dialog. Still binding-gated:
  a plan must exist for the session.
- **Bindings are now durable**: the process-scoped registry alone lost the
  binding across app restarts (T9's premise was in-process). Migration 023
  adds `plan_bindings` (session_id, objective_hash, plan_id, plan_text,
  source_revision, created_at_ms; UNIQUE (session, objective); newest-wins
  UPSERT; rows deleted by `delete_session`). Both M3 sites mirror every
  insert; an Apply deletes the row + registry entry so a later bare "yes"
  cannot re-arm an executed plan; phrase arming falls back to the durable row
  via `rehydrate_durable_binding` (restart-safe). Fail-soft throughout —
  storage errors never fail a run.

### 8. Non-goals, kept and added

As §5 — plus: `design`/`architecture for` stay out of the negation-elevated
corpus; bare approvals remain binding-gated and consumption-cleared; durable
binding persistence is best-effort (a missing row degrades to the pre-round
behavior, never to a grant).

### 9. Acceptance — additions

- **T11** — negation + diagnose-word prompt that explicitly plans ("plan: …no
  unsafe… exit code 2 = error") routes `Plan` (0.9, `negation_override`).
- **T12** — "I approve" / "yes" arm the dialog only when a session binding
  exists; "don't approve the plan", "not yet" never do.
- **T13** — durable round trip: `save_plan_binding` → restart →
  `rehydrate_durable_binding` → phrase arming arms the dialog; Apply clears
  the row (delete returns `(Ok(true))`, second delete `Ok(false)`).
- **T14** — non-repo git hint: `open_repo` classifies a non-git directory as
  `NotARepository` with an actionable hint so the coder stops looping git
  calls (observed live: repeated `git|Allow|observe` → ExecutionError rows).

### 10. Live-fix round 3: M3 — Apply executes the approved plan; planner empty-response hardening

Round-3 live evidence (data dir `/mnt/Temp/concerto`, a stored-plan exercise
closed with a bare "i approve"; fixes `bff3e85` deployed in the tested binary):

- **Plan → approval loop verified end-to-end.** With `bff3e85`, a bare
  "i approve" armed the real Apply/Replan dialog — audit
  `intent:plan | apply | {"plan_id":"01KZPZKVQAEQRNK9Q7WVVJRJE2",...}` — the
  durable `plan_bindings` row was consumed (table count 0 after Apply), and the
  router no longer demoted `plan:` (routing row `Plan` with `negation_override`,
  no Diagnose).
- **Observed execution-half failure.** The apply-ack run's coordinator subtasks
  were literally `Implement: i approve <conversation_history>…`; the Coder
  "completed with no file changes"; Reviewer flagged Critical (zero workspace
  entries); validation could not run; revision queued without review.
  Concurrently, `concerto.log` showed twice:
  `Task planning failed: MultiAgentPlanFailed { reason: "JSON parse error: expected a JSON array of plan items, got: " }`
  — an empty provider response on the planner call (deepseek-v4-flash-free) → the
  heuristic fallback built degenerate subtasks from the task text.
- **Two root causes.** (a) The run task description was built from `req.input`
  (the approval phrase) — `apply_plan` only suppressed stale checkpoints; (b)
  empty-content planner responses fell through to the JSON parser.
- **Fix (commit `b228837`).** `runtime_runner.rs` captures the consumed
  `PlanBinding` (clone) **before** registry/durable consumption on Apply and
  builds the run task from the stored plan text via the pure helpers
  `approved_plan_task_description` / `build_run_task` (`apply_plan` →
  action-required task describing the approved plan; non-apply routing
  unchanged; `req.input` still recorded in transcript + audit). `planner.rs`
  treats empty/whitespace planner output as a retriable failure — retry once
  with the same prompt, warn with provider + attempt, and on a second empty
  return `MultiAgentPlanFailed { reason: "planner returned an empty response (no content) after 2 attempts" }`
  so the coordinator heuristic fallback still engages.
- **Verification.** Full workspace gate green (2511 tests, fmt + clippy
  `-D warnings` clean, 25 crates), four new tests. Security review: grant scope
  unchanged; executed artifact == approved artifact (same binding cloned from
  the dialog's text); capture-before-delete ordering sound with no await between
  decision and removal; no re-arm after Apply (registry + durable delete;
  post-run insert gated on effective outcome == Plan).
- **Non-blocking follow-ups.** A manual planner retry emits no bus event
  (UI-invisible); a failed durable delete leaves a re-arm window requiring a
  fresh explicit approval; Replan still re-plans from the approval phrase as
  objective (round-2 quirk); the Windows drive-letter arc (`C:\Verdict` shows a
  ⌀ placeholder) and `..` path-traversal containment friction are
  environment-side, not routing defects.

**Acceptance — additions:**

- **T15** — the Apply run's task is the approved plan, not the approval phrase.
- **T16** — an empty planner response is retried once and then fails with a
  clear reason rather than a positional parse error.

### 11. Live-fix round 4: binding-driven Apply/Replan arming

Round-4 live evidence (data dir `/mnt/Temp/concerto`, a `plan:` run closed with
a follow-up "execute"; fix `ba41f2d` —
`fix(orchestrator): arm Apply/Replan dialog for confident Execute from durable session binding`):

- **Arming was phrase- and hash-only.** The router classified the follow-up
  "execute" as a confident Execute (`Execute | ask_user | granted`), but with
  no phrase/hash match nothing armed: no `intent:plan | apply` audit row
  appeared, and `plan_bindings` stayed at count 1 — plan
  `01KZQ4VAB3VDEMRSACFYK0W5TS`, objective hash `132befb4…` — while the
  session's newest durable plan sat unused.
- **What the run did instead.** The coordinator re-planned from the raw
  "execute" input; the LLM planner returned empty — observed live twice:
  `planner returned an empty response; retrying once provider=opencode
  attempt=1`, then the explicit `MultiAgentPlanFailed` reason — the heuristic
  fallback built degenerate subtasks (`Implement: execute …`), the Coder issued
  zero write tool calls (audit shows observe/read/probe rows only), the
  Reviewer flagged Critical twice ("workspace root contains no files at all"),
  two `provider stream-idle timed out after 120s` were logged, and the run
  ended "Task failed: provider stream-idle timed out after 120s".
- **Fix (commit `ba41f2d`).** A third arming fallback in `run_shared_agent`:
  `bound.is_none() && is_confident_execute(&routing)` — outcome Execute +
  confidence >= `LOW_CONFIDENCE_THRESHOLD`, the exact predicate the generic
  gate uses — → `store.load_newest_plan_binding(session_id)` →
  `arm_binding_for_confident_execute` (pure mapping to `PlanBinding::restored`,
  preserving the original objective hash, plan text, source revision,
  `created_at`) → re-seed the in-process registry → the same audited
  Apply/Replan dialog. Apply consumes the row with the same
  (session, original-objective) key in both stores and executes the approved
  plan text (R3 `build_run_task`); Replan stays read-only and keeps the durable
  row; a missing row or storage error falls through to the generic gate
  (fail-soft). The dialog question is reworded to name the plan id — it no
  longer claims "for this objective", since §11 may load a session-newest plan
  from an earlier objective. Security posture unchanged: identical grants to a
  generic granted Execute (fs+git via `grant_execute`; shell never grantable),
  and the dialog shows the actual stored plan text — strictly more informative
  than a bare confirmation.
- **Verification.** Full workspace gate green (2514 tests — 2511 + 3 new; fmt
  + clippy `-D warnings` clean, 25 crates). Oracle review: approve, no blocking
  issues — grant-equivalence, same-key consumption, Replan row retention,
  fail-soft races, predicate consistency, exact Execute guard.

**Acceptance — additions:**

- **T17** — a confident Execute with no phrase/hash binding but a durable
  session-newest row arms the Apply/Replan dialog with the stored plan, and
  Apply executes that plan's text; a missing row or storage error falls through
  to the generic intent gate.

**Deferred — agent-specific, explicitly out of scope for this fix:** Coder
write attempts under a correct task; provider stream-idle timeouts on large
contexts; planner empty-response behavior of the configured model (the retry +
clear failure already handle it, and the heuristic fallback carries planning);
Windows Git-bash friction (`/dev/null`, `..`, drive-letter arc);
eval-harness "no config file found"; reviewer/revision polish.

### 12. Live-fix round 5: binding-driven arming for bare execution directives

Round-5 live evidence (data dir `/mnt/Temp/concerto`, a `plan:` run closed with
the bare follow-ups "execute" and "approve"; session `01KZRHY1J5EVMX4D240K60M7MT`;
fix landed in `4778ea3`):

- **Bare directives never reached the §11 path.** After a `plan:` run bound
  plan `01KZRJ0XRRPYTRFQ2SZ9ZYD74E` (objective hash `132befb4471cfa15`) under
  the session-newest durable row — never consumed — the user typed bare
  "execute" and bare "approve". The router classified both as AskUser (audit
  rows `intent_router | granted | ask_user | Execute`): `EXECUTE_KEYWORDS`
  had no base-form entries ("execute"/"run"/"apply"/"approve"), so the
  deterministic Execute rule never fired and the classification fell to the
  AskUser path — `is_confident_execute` (§11) could not engage.
- **What the user got instead.** The generic AskUser list modal — "I could
  not confidently tell what you want. Pick the intent for this run" — with all
  six outcomes, instead of the stored-plan Apply/Replan dialog, even though a
  durable session-newest binding existed and was unarmed (no
  `intent:plan | apply` audit row).
- **Fix (vocabulary, both files).** `crates/core/src/intent.rs`:
  `EXECUTE_KEYWORDS` gains the base forms `execute`, `run`, `apply`, `approve`
  (exact word-boundary matching, no inflections — `running`/`applying`/
  `approving`/`approved` deliberately excluded), and `VERIFY_KEYWORDS` gains
  the run-family verify phrasings `run tests`, `run the test`, `run the test
  suite`, `run cargo test`, `run the build` — so "run tests" / "run cargo
  test" stay Verify, not Execute. `crates/orchestrator/src/runtime_runner.rs`:
  `is_plan_approval_phrase` gains `run the plan`, `run plan` (still gated by
  the binding-existence guard and the NEGATIONS guard). The router's
  documented priority order is unchanged: negation → question → Verify → Plan
  → Review → Diagnose → Execute, with `NEGATION_PHRASES` still overriding
  everything.
- **Security invariant.** This adds **no new grant surface**: the vocabulary
  only lets a bare directive reach the existing §11 arming fallback, which
  still lands in the user-facing Apply/Replan confirm dialog
  (`request_plan_approval`); dismissal stays read-only; grants are identical
  to a generic granted Execute (fs+git via `grant_execute`, shell never
  grantable); fail-soft behavior unchanged — a missing row or storage error
  still falls through to the generic intent gate (`apply_intent_gate`).
- **Accepted tradeoffs (brief).** (a) "run X" phrasings not in the verify
  list ("run the server", "run the numbers") now route Execute —
  user-confirmable only, never auto-execute; (b) bare "not" is not in the
  NEGATIONS guard, so "not run the plan" would arm via the phrase path —
  accepted, since adding bare "not" would wrongly kill "not sure, looks
  good"; (c) §11 reads only the durable row — a fail-soft durable-save
  failure with only an in-memory binding falls back to the generic gate (rare,
  by design); (d) the §12 tests are unit-level; a run-level integration test
  of the arming wiring is future work.
- **Verification.** Full workspace gate green (2514 tests; fmt + clippy
  `-D warnings` clean, 25 crates). Cross-reference to §11's addendum: the
  §11 fallback chain (`bound.is_none() && is_confident_execute` →
  `store.load_newest_plan_binding` → `arm_binding_for_confident_execute`,
  preserving the ORIGINAL objective hash / plan text / source revision /
  `created_at`) is unchanged; §12 only guarantees that a bare directive
  actually routes as a confident Execute so that fallback can fire.

**Acceptance — additions:**

- **T18** — bare directive words route to the Execute rule and arm the dialog
  from a durable session-newest binding, by name:
  `bare_execute_directives_route_to_execute` ("execute"/"run"/"apply"/
  "approve" → Execute, `execute_keyword`, confidence 0.8),
  `bare_directives_keep_priority_and_negation_semantics` (negation coverage:
  "don't execute"/"don't run"/"do not approve"/"never apply" → Answer via
  `negation_override`; "run the tests"/"run tests"/"run cargo test" stay
  Verify; "run the plan" stays Plan) and
  `directive_compounds_and_inflections_never_route_to_execute`
  (runner/running/runbook/runway/application/applying/approving/approved stay
  AskUser) in `crates/core/src/intent.rs`, plus the orchestrator
   `bare_execute_arms_dialog_from_durable_binding` in
   `crates/orchestrator/src/runtime_runner.rs` — a bare "execute" routes
   Execute at or above `LOW_CONFIDENCE_THRESHOLD` and
   `arm_binding_for_confident_execute` restores the ORIGINAL plan objective,
   plan text, source revision, and `created_at` from the durable row.

---

## ADR-55, Addendum (Phase 2c) — verbatim

Phase 2c classifier decisions: the LLM classifier becomes a real, optional
Phase-2 runtime component (previously proposal-only, §6/§9) — config home
(§1–§2), routing placement and fail-soft semantics (§3), the never-grant
invariant (§4), audit (§5), spend/failure semantics (§6), tests (§7), and
non-goals (§8). **Status stays Accepted** — the ADR already flipped at 1e §6;
this addendum records the 2c scope, no further flip.

### 1. Scope & status — a real, optional Phase-2 component

The classifier is no longer a placeholder, but stays **optional and off by
default** (an added model call). It is a wrapper around the deterministic
router: `route()` (`crates/core/src/intent.rs`) stays pure and unchanged; the
classifier mounts only at the router's AskUser sink (routing step d). Status
stays Accepted — this addendum records the 2c scope, not a new flip
(landed at `9ec27f3`).

### 2. Config home — new `[intent]` section, schema 6 → 7

A new `[intent]` section (`IntentConfig`, `crates/config/src/schema.rs`) with
three classifier keys only:

- `classifier_enabled: bool` — default **false** (conservative; an added model call).
- `classifier_model: Option<String>` — default `None` = same chat model per §9.
- `classifier_confidence_threshold: f32` — default **0.7**, **validated at
  config load to be `>= concerto_core::LOW_CONFIDENCE_THRESHOLD`** (the
  deterministic constant, defined `crates/core/src/intent.rs`, used by the gate
  in `crates/orchestrator/src/intent_grants.rs`; `concerto-config` already
  depends on `concerto-core`) — a config error otherwise, mirroring
  `RetryConfig::validate` (`crates/config/src/schema.rs`). Binding the threshold
  to the gate's constant (not a literal) keeps §4's "same confirmation
  machinery" claim true even if the constant is ever raised: no configured
  threshold can create a `[threshold, LOW_CONFIDENCE_THRESHOLD)` band where a
  classifier Execute re-route would miss the gate's arm-1 dialog
  (`is_confident_execute`) and land in the read-only wildcard instead.

`SCHEMA_VERSION` bumps **6 → 7**; `migrate_v6_to_v7` inserts the section with
defaults when absent (insert-only; fill mirrors `migrate_v3_to_v4`, bump mirrors
`migrate_v4_to_v5`/`migrate_v5_to_v6`). **Re-adding `[intent]` does NOT restore
the `mode`/`enabled` keys dropped at v6** — they stay removed (1e §1: gate
always-on); v7 adds only classifier keys and `enabled` is not resurrected as a
gate toggle. `IntentConfig` stays **additive** (no `deny_unknown_fields` on
`AppConfig` — `crates/config/src/schema.rs` comment) so stale keys keep loading.
Rationale: 1e deleted the section; a toggle needs a home, and narrowly-scoped
keys avoid re-litigating the gate-required decision.

### 3. Routing placement — AskUser only, bounded, fail-soft

The classifier runs **only** when the deterministic router + negation corpus
produce `RouterRoute::AskUser` (ambiguity remaining). It never replaces a rule
hit and never runs for negation-override results (read-only by construction,
§6). On AskUser, if `classifier_enabled` and a model is available: **one
non-streaming provider call** through the normal provider stack, with
`SpendTracker`/RPM accounting like any model call and a `CancellationToken`
threaded. Output JSON `{route, confidence, rationale}`, `route` ∈ the
six-outcome set. If `confidence >= threshold` → re-route to the suggested route:
`RouterOutput.route = RouterRoute::LlmClassifier` (the placeholder becomes
real), and the AskUser `0.0` confidence is replaced by the classifier's — path
selection only, so the gate's `is_confident_execute`/threshold checks operate on
the replaced value. If confidence `< threshold`, parse failure, provider error,
or cancellation → **fail-soft to AskUser unchanged** (read-only + ask, `0.0`).
No retries beyond the provider's normal retry budget (one call, bounded).

### 4. Never-grant invariant — load-bearing

The classifier can classify, never grant. Its output **never upgrades
authorization**: any suggested route — including Execute — passes through the
exact confirmation machinery as today (intent gate dialog `apply_intent_gate`;
plan-binding Apply/Replan; read-only outcomes unchanged). It cannot produce
grants, cannot bypass the intent gate, and cannot produce a route the
deterministic router could not produce (six-outcome set; mutation routes still
require confirmation). **Deny is final** — the classifier never runs for a Deny
(the AskUser sink never yields one; `AutoDeny` danger patterns untouched) and
cannot downgrade one. `AuthorizationState` transitions stay user-event-driven
(§1).

### 5. Audit — reuse `record_routing_decision`, JSON envelope, chained correlation_id

**Reuse the existing `record_routing_decision` seam** (`intent_router` channel,
`crates/core/src/executor.rs`) — no sibling seam, no schema change. Exact
fields for the **classifier row**: `tool_name = "intent_router"`,
`rule_matched = "llm_classifier"` (the name `router_route_name` already
reserves for `RouterRoute::LlmClassifier`),
`verdict = "n/a"` (no confirmation solicited; a fail-soft re-ask is a separate
AskUser routing decision, itself recorded), `user_response` = JSON envelope
`{"route": "<suggested outcome>", "confidence": <f32>, "threshold": <configured
value, default 0.7>, "rationale": "<≤512 chars>"}`. **Disambiguation from the
router row:** the existing router-decision record at
`runtime_runner.rs:2317-2328` must keep recording the **pre-replacement
deterministic route name** (`"ask_user"` for a classifier-eligible event, not
`router_route_name(&routing.route)` post-replacement) — so the two rows per
event are distinguished by `rule_matched` (`"ask_user"` vs `"llm_classifier"`),
never by envelope shape alone. **`correlation_id`
chaining:** one correlation_id per routing event, created **at classifier start**
and threaded into the existing `record_routing_decision` call — replacing the
fresh `Ulid::new()` at `runtime_runner.rs:2320` — so the router-decision and
classifier records share it. This **resolves the 1d §6 known item for
routing↔classifier records**; the routing↔plan-decision half stays pending.
**No new columns:** migration 024
already added `plan_id`/`source_revision` to `audit_log`; the classifier needs
neither — the envelope suffices.

### 6. Spend & failure semantics

The classifier call is spend-tracked on the same channel as any model call and
counts against the session spend cap, using the codebase's **reserve-before-call**
semantics (as `agent_runner.rs` does): `check_and_add` ("Atomically reserve
spend after checking all configured caps", `policy.rs:767-771`) is called
**before** the classifier call — if the reserve fails (cap exceeded), the call
never happens and AskUser stands; after the call, `settle_reservation`
(`policy.rs:791`) records actual spend without re-checking the cap (retained
over cap, same property as `record`'s doc at `policy.rs:806-808`). There is no
mid-call discard path — the cap check gates the call up front. **Ordering
requirement:** the per-session spend **carry-forward must be recorded before the
classifier runs** (the current gate sequence records it at `runtime_runner.rs:2336`,
after routing), so a session already over cap from a prior run cannot fire a
classifier call. `CancellationToken` threaded through the call. Missing
provider/config → classifier disabled with a debug log, AskUser unchanged (§3
fail-soft).

### 7. Test & acceptance checklist

- Deterministic-router regression: existing intent tests unchanged (`route()`
  stays pure; the classifier is a wrapper, never inside `route()`).
- Classifier-enabled unit tests with a stub classifier: confidence above/below
  threshold, malformed JSON, provider error, cancellation, spend-cap-exceeded.
- Audit records present with the correlation_id chain: the router row keeps the
  **pre-replacement** route name (`"ask_user"`) and the classifier row carries
  `rule_matched = "llm_classifier"` — exactly two rows per classifier-eligible
  event, distinguished by `rule_matched`.
- Negation corpus still wins over classifier output (a negation-override input
  never reaches the classifier).
- **`llm_classifier_is_never_produced_in_phase_0` is superseded.** Replacement
  contract: `RouterRoute::LlmClassifier` is produced **only** via the 2c
  classifier path — a wrapper around `route()` — never by `route()` itself; the
  replacement asserts (a) `route()` over the corpus never yields it, (b) the
  wrapper yields it exactly when re-routing an AskUser input above threshold.

### 8. Non-goals — explicitly out

Conversation-context resolution of relative utterances ("apply that") stays in
the known-v2 list (1d §6); no multi-binding per objective; no classifier-driven
grant persistence; no streaming classification.

### 9. Acceptance

- **C1** — `[intent]` classifier keys load at schema 7; v6 configs migrate with
  the section defaulted (classifier off); `mode`/`enabled` stay absent;
  `classifier_confidence_threshold < 0.7` is rejected at load (config error).
- **C2** — the classifier runs only for AskUser-remaining ambiguity; rule hits
  and negation-override results never reach it.
- **C3** — above-threshold classification re-routes with classifier confidence;
  below-threshold / parse-failure / provider-error / cancellation /
  cap-exceeded all fail-soft to AskUser (read-only + ask, no grant).
- **C4** — every classifier invocation writes one audit row on the
  `intent_router` channel with `rule_matched = "llm_classifier"`, the JSON
  envelope, and the router decision's correlation_id (the router row keeps the
  pre-replacement route name); no new columns.
- **C5** — classifier spend is reserve-before-call (`check_and_add` gates the
  call; `settle_reservation` records actual spend), the session spend
  carry-forward is recorded before the classifier runs, and a cap-exceeded
  classifier request never routes anywhere (the call never fires, AskUser
  stands).
- **C6** — the superseding test contract for
  `llm_classifier_is_never_produced_in_phase_0` lands (§7).

---

## ADR-55, Addendum (Phase 2d) — Automatic intent gating: the Coordinator decides, no clicks (issue #27) — verbatim

> **Superseded as dispatch logic by Phase 2e below** (2026-09-09). This block
> is preserved verbatim for the rejected-design record; its §3 (exact-objective
> hash-verified auto-Apply) survived into the live decision (ADR-55 §4), its
> §2/§2a semantics into ADR-55 §2, and its §1/§4/§5/§6 are revoked.

Issue #27 lands the click-tax removal: `route()` + classifier result **is** the
decision, grants are automatic at high confidence, and the interactive
plan/execute confirmation dialog is deleted from the hot path. This addendum
supersedes, **in part**:

- **Decision §1 "hard rule"** — "the classifier (and the deterministic router)
  can *classify*, never *grant*": replaced by high-confidence auto-grant (§1
  below) for the five action-grantable outcomes. The AskUser zero-confidence
  path never grants (§2).
- **Decision §3** — "Plan agreement — an explicit dialog": the
  `ApprovalSink` Apply-it dialog is deleted from the hot path; routing +
  auto-grant replaces the user click.
- **Decision §4** — "re-confirmed on resume": grants stay non-durable, but
  re-confirmation happens through routing (auto re-grant), never a dialog.
- **1d §3 interception** — "No mutation is possible without an explicit Apply":
  auto-Apply from a hash-verified binding replaces the Apply/Replan/Dismiss
  clicks. Replan remains reachable only via an explicit new Plan request.

**Unchanged and load-bearing (compose with, do not revoke):** Decision §2
capability tiers (`Consequential` never covered by blanket authorization;
`IntentAuthorized` only upgrades `RequireApproval`, never overrides `Deny`);
Decision §5 audit chain; 1d §2 binding registry (process-scoped, keyed by
`(session_id, objective_hash)`, newest-wins, 16 KiB `plan_text` cap); 1d §4
`record_plan_decision` seam; 1d §5 source-revision identity; 1e §2
gate-covers-all-runs; 2b checkpoint precedence; ADR-56 §1a negation fast path
(read-only can never be upgraded to writable by any model output or rule).
No whiteboard / ledger / checkpoint persistence / supervisor changes; no new
LLM calls beyond the existing classifier (ADR-56 §7).

### 1. Auto-grant — routing is the decision

For outcomes `Execute | Plan | Verify | Review | Diagnose` with
`confidence >= concerto_core::LOW_CONFIDENCE_THRESHOLD` (0.7) reached via
`RouterRoute::RuleHit` **or** `RouterRoute::LlmClassifier`, the run loop
auto-grants in `IntentGrantStore` with the same `filesystem`/`git` scopes a
confirmed `Apply` holds today. No `ApprovalSink` call, no dialog, no modal.
**Scope amendment (2026-09-09):** the auto-grant's scopes are now
`filesystem` + `git` + **project-bounded `shell`** — a shell command is
covered by the acting grant only while structured command facts prove its
working directory and every path-like token resolve inside the session
project root and no denylist/Consequential/network rule already matched
(`is_project_bounded_shell`; everything outside that scope keeps the
existing `shell_requires_approval` approval path). The shell scope hole in
§Decision 2 remains for anything outside these bounds.
**Scope amendment (2026-09-10, delegation under Acting grants):** the same Acting grant also covers the
Coordinator's orchestration/delegation surface — `call_specialist`, the only dispatch tool the
coordinator's decision loop policy-evaluates — so the run-scoped authorization upgrades its
otherwise-`un_granted` `RequireApproval` to `Allow` under the distinct, individually auditable
`intent_authorized_delegation` row. Cause-only-no-effect: this authorizes the dispatch, never the
specialist's own work — every specialist tool call stays policy+grant-gated, spend/task caps
still bound the fan-out, and a read-only run denies delegation outright.
The classifier wrapper remains mounted after the two fast paths (ADR-56 §1)
and its threshold validation (>= 0.7, no band creation, ADR-56 §4) is
unchanged — the invariant shift is only *what happens after a high-confidence
route*: grant instead of prompt.

### 2. AskUser and negation — hard read-only invariant

`AskUser` (confidence `0.0`) and `NegationOverride` (`don't`, `never`,
`without touching`, ...) remain **hard read-only**: no grant, no tool write,
no spend. The negation read-only invariant continues to rest on the
`NEGATION_PHRASES` corpus running first-match-wins ahead of any model (ADR-56
§1a) — a permissive model can never make a read-only request writable. A
zero-confidence input that needs action lands as a read-only answer-only run
with an audit row; the user escalates by rephrasing with clearer intent, not
by clicking a modal.

**2a. Negation trigger — task-level prohibition vs. constraint clause
(2026-09-06)**

`negation_override` fires only for a **task-level prohibition**: the matched
phrase either stands alone (short input) or precedes any explicit action
keyword (`verify`/`plan`/`review`/`diagnose`/`execute`). A requirement clause
that appears *after* an explicit action request — "build X … do NOT read it as
UTF-8", "… must not panic", "… don't touch the parser" — constrains the
artifact the user asked for; it does not prohibit the action. It must not
demote the run: it passes through as ordinary task text so the constraint
reaches the executor in the prompt. Reassurance markers (`don't panic`,
`don't worry`, `don't forget`) never fire the veto in any position.

The hard read-only wall is unchanged **once fired**: negation still beats every
model and rule (ADR-56 §1a), and a genuine prohibition ("don't do it",
"just answer", "no changes", "don't build accord") vetoes exactly as before.
This narrows only *when the veto triggers*; it cannot make a read-only request
writable — a prohibition-first or prohibition-only message still grants
nothing (§1 auto-grant composes with the wall unchanged). Standalone `"stop"`
is deliberately **not** a corpus member — "stop the service and restart it"
is an action request — and lands `AskUser` (0.0), still hard read-only (§2).

### 3. Plan→Execute auto-Apply — hash-verified binding

When a plan-approved binding exists (1d §2; inserted post-run on a
Plan-effective `Ok` run) and the next input routes `Execute` at confidence,
the run auto-`Apply`s the persisted `DesignDoc`: `artifact_hash` verified,
**loud-fail on drift** (never silent re-decompose — 2b checkpoint precedence
and ADR-65 §7 resume semantics unchanged). No `approve the plan` click.

### 4. Resume auto re-grant

On resume, grants re-apply automatically through routing (Decision §4
non-durability retained, dialog channel removed): a high-confidence
action-required route re-grants; an `AskUser`-routed resume stays read-only.
No modal at the resume boundary.

### 5. Audit — observable, not blocking

Every auto decision writes to `sessions.db:audit_log` labeled
`intent_router: auto_granted` with `{rule, confidence, route}` plus a
`session_events` `RoutingDecided` record. `record_plan_decision` (1d §4)
gains auto variants (`auto_apply` / `auto_granted`). Denial, negation, and
AskUser paths keep their existing audit rows.

### 6. Acceptance (issue #27)

- **A1** — `cargo test -p concerto-orchestrator --lib` + `cargo clippy -D
  warnings` green; `intent` tests green.
- **A2** — `build accord` → immediate Execute run, no click modal;
  `audit_log` shows `auto_granted` + `RuleHit|LlmClassifier` +
  `confidence >= 0.7`.
- **A3** — `don't build accord` → `NegationOverride` → read-only, zero
  writes.
- **A4** — `hmm` (`AskUser` `0.0`) → zero writes, zero grants.
- **A5** — `plan: X` then `execute` (no `approve` click) → auto-`Apply` from
  the persisted `DesignDoc`, hash-verified.
- **A6 (revised by Phase 2e below)** — "Build a Rust CLI tool called hexview
  … do NOT read it as a UTF-8 string … must not panic … don't panic …
  verify it" (with or without a `Prompt:`-style glued label) → Acting
  envelope → the unified loop builds and verifies with tools → files
  written. Outcomes are flavor hints only; no keyword may select a
  tool-less path.
- **A7** — A3 plus "don't do it", "just answer", "no changes" → `NegationOverride`
  read-only, zero writes, zero grants. Standalone "stop" (deliberately not a
  corpus member; "stop the service" is an action request) → `AskUser` 0.0,
  also hard read-only (§2), zero writes, zero grants.

---

## ADR-55, Addendum (Phase 2e) — Unified agent loop: the router grants envelopes, the model shapes the run (2026-09-09) — verbatim

> This addendum superseded Phase 2d §§1/3–4 as dispatch logic and became the
> live decision; it is folded into ADR-55 §§1–3/§8, with the reconciliation
> note (cycle_manager removal) folded into §1.

Supersedes Phase 2d §§1/3–4 *as dispatch logic* (auto-grant-then-branch).
Three consecutive smoke failures share one mechanism, not three causes:
deterministic keyword routing choosing the run's code path before any model
is consulted (a `verify` subordinate clause hijacks builds into tool-less
chat; `don't` inside constraints hijacks into the read-only sink; a
`Prompt:` label glued to the leading verb blinds whole-token matching).
Narrowing instances (#43, #44, #46) cannot close a mechanism that generates
them. Grounding: Anthropic "Building Effective Agents" (augmented LLM —
the model selects tools; agents are LLMs using tools on environmental
feedback in a loop) and OpenCode (one loop, `permission` rules keyed by
tool at action time, no request classification).

**Unchanged and load-bearing:** Decision §2 tiers (the *only* gate); §5
audit chain; §2a prohibition trigger as narrowed by #43 (now an *envelope*
trigger, not a path selector); ADR-60/65; ADR-66.

### 1. One loop

Every non-empty run enters the unified agent loop (single-agent vs
coordinator selection unchanged — out of scope). The `run_text_only`
branch is deleted. Chat is what the loop does when the model uses no tools
(≈ one text-only call in cost, zero forks).

### 2. Router keeps the safety job only

`route()` still runs (cheap, auditable) and decides only the permission
envelope: `ReadOnly` (task-level prohibition; empty/zero-confidence) or
`Acting` (everything else, grant scopes exactly as today). Outcomes become
non-binding flavor hints — one system-prompt line (e.g. "the user seems to
want verification; prefer checking over changing"), logged with the routing
row, never branching.

### 3. Enforcement at grants; modal leaves the hot path

`ReadOnly` = `IntentAuthorized` never set; the policy engine denies writes
as today. Prohibition inputs get read-capable answers (better UX, identical
safety). Unclear input gets bounded in-loop clarification (iteration caps
bind it).

### 4. Classifier retired from dispatch

The ADR-56 classifier leaves the run hot path (saves a call + latency per
run); deterministic safety rules + flavor scan remain (see ADR-56 amendment
2026-09-09).

### 5. Guards that stay

Zero-work guard, cycle detection, continuation caps, spend fuses, and the
empty-completion honesty rule (Phase 2d Fix B — a loop run ending with
empty final text still synthesizes the explanatory completion).
Identical-tool-call repetition coverage is verified in `cycle_manager`,
with a minimal same-call guard added only if absent.
> Reconciliation note (2026-09-24): `cycle_manager` was removed in the
> routing-carcass cleanup; cycle detection lives in `cycle.rs` (`CycleTracker`,
> "detects repeated identical tool calls"), and its coverage (e.g.
> `sixth_call_with_same_input_fires_cycle_detected`) is verified there.

### 6. Narrow normalization (glue-strip)

Routing normalization detaches a leading glued `Label:` iff alphabetic
length > 1 (excludes `C:` drives) and the remainder begins with an explicit
action keyword as a whole token (excludes `https://…`). Near-miss
regression tests pin the exclusions.

### 7. Acceptance (Phase 2e)

- **A8** — `don't build accord` → ReadOnly envelope → answer, zero writes,
  zero grants.
- **A9** — `hmm` → bounded in-loop clarification (≤1 turn), zero writes.
- **A10** — `verify the fix` → Acting envelope → loop verifies *with
  tools*; writes governed by policy, not by branch.
- **A11** — full workspace green; `run_text_only` deleted; no text-only
  branch; no dispatch-time classifier call; routing docs updated.

---

## ADR-56, Amendment (2026-09-06) — the classifier may auto-grant at high confidence (issue #27, superseded in part) — verbatim

> **Moot and removed.** Superseded by the 2026-09-09 retirement and the
> unified-loop decision; preserved for the rejected-design record.

ADR-55 Addendum (Phase 2d) lands automatic intent gating: `route()` +
classifier result is the decision, and high-confidence outcomes auto-grant.
This amendment supersedes, **in part**, two §4/§8 invariants as finalized by
2d:

- **§4 "The classifier never grants"** — "a re-routed Execute still passes
  through the confirmation dialog and grants machinery (arm-1 gate; the 2c §4
  never-grant invariant is unchanged)": replaced by 2d §1 — a re-routed
  `Execute` (or `Plan`/`Verify`/`Review`/`Diagnose`) at
  `confidence >= classifier_confidence_threshold` **auto-grants** with the
  standard `filesystem`/`git` scopes; no confirmation dialog.
- **§8 "The model classifies, never authorizes"** — "it can never produce an
  unconfirmed mutation": a high-confidence misclassification (>= 0.7) can now
  produce an unconfirmed mutation. This is the deliberate, accepted trade of
  issue #27 (the click added no information above the threshold) and is
  bounded — never silent — by:

  1. the negation fast path (§1a) runs before any model and is an absolute
     read-only veto; `NEGATION_PHRASES` remains load-bearing;
  2. `AskUser` (confidence `0.0`) never auto-grants — hard read-only (2d §2);
  3. `AutoDeny` danger patterns and `Deny`-is-final run first in the policy
     engine; `IntentAuthorized` only upgrades `RequireApproval`, never
     overrides `Deny` (ADR-55 §Decision 2);
  4. `Consequential` tier and unscoped actions are outside any blanket grant
     (ADR-55 §Decision 2) — the auto-grant carries exactly the
     `filesystem`/`git` scopes a confirmed `Apply` holds today;
  5. spend cap + reserve-before-call (§7) gate the classifier call itself;
  6. every auto decision is audited (`intent_router: auto_granted` +
     `RoutingDecided`, 2d §5) — observable, not blocking.

**Unchanged:** §1a/§1b fast paths and their precedence; §3 offline fallback
chain (classifier off/unreachable → byte-identical deterministic chain with
the AskUser modal standing — the modal survives only where no confidence
exists, not as a click tax on confident routes); §4 threshold validation
(`classifier_confidence_threshold >= LOW_CONFIDENCE_THRESHOLD` at config
load; no `[threshold, 0.7)` band); §5 audit chain; §6 utterance-only prompt;
§7 cost semantics.

---

## ADR-56, Amendment (2026-09-09) — classifier retired from run dispatch (unification, ADR-55 Phase 2e) — verbatim

> Folded into ADR-56 "Current state" / §4 / §8 and ADR-55 §10.

The classifier leaves the run hot path: §1 "primary decider" and §4
"reroute at threshold" no longer operate on run dispatch (saves one bounded
LLM call + latency per run). What remains: deterministic safety rules +
flavor scan in `route()`; the `intent_classifier` module itself, retained
for future eval/classification UX and marked off-hot-path (no dispatch
hookups). §8 (classifies, never authorizes) holds wherever the module is
used. The 2026-09-06 auto-grant amendment is moot on the hot path — grants
derive from the deterministic envelope (ADR-55 Phase 2e §§2–3), not from
classifier output.

---

## ADR-56, Clarification (2026-09-11) — classifier config surface removed — verbatim

> Folded into ADR-56 "Current state" and ADR-55 §10; the module itself was
> later deleted in the 2026-09-24 routing-carcass cleanup.

With the classifier off the hot path, its dedicated config surface
(`[intent].classifier_enabled`, `[intent].classifier_model`,
`[intent].classifier_confidence_threshold`, ADR-55 Phase 2c §2 as adopted
by §2 here) serves no reader and is removed from the schema (unknown-key
tolerant: old files keep loading). The `intent_classifier` module itself
stays for eval/future UX per the 2026-09-09 amendment. ADR-55 Phase 2c §2's
schema-6→7 history stands as record; no threshold invariant survives it.