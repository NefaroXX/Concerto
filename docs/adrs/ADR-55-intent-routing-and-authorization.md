# ADR-55: Intent routing and intent-gated authorization (three-generation gate)

> **Partially superseded (2026-08-11) by
> [ADR-56](ADR-56-model-first-intent-classification.md):** the Phase 2c §2
> `classifier_enabled` default pin (false → true) and the Phase 2c §3 classifier
> placement pin (AskUser-only → model-first with two deterministic fast paths).
> Every other decision in this ADR remains in force.

**Status:** Accepted (2026-08-09) — flipped per the Phase 1e addendum §6.
    Phase 1 (intent gate as the only routing path) landed on `dev` at
    `8606ce9` (code) and `d9e352c` (addendum). Items deferred to Phase 2
    (LLM classifier proposal-only, shell argv/cwd containment, v2
    persistence) are tracked in the Phase 1e addendum §4.
**Date:** 2026-08-09
**Deciders:** Concerto architecture
**Supersedes:** (Phase 1) the `AgentMode` (Build/Chat/Plan) prompt-level picker
    as the de-facto read-only guarantee — the picker is removed only after the
    mutation gate and plan-approval UX land (§Decision 8).
**Composes with:** ADR-52 (durable plan artifacts: `plan_id`,
    `objective_hash`, `source_revision`), ADR-46/ADR-48 (reasoning-as-data,
    ContextEngine — unchanged), ADR-44 (session-scoped `VirtualFs`), ADR-26
    (audit/correlation-id chain), ADR-37 (capability lifecycle).

## Context

Today the only thing separating "read-only" agent behavior from "mutates the
codebase" is the **prompt level** mode picker:

- `AgentMode { Build, Chat, Plan }` (`crates/core/src/types.rs`) drives
  `is_action_required()` (true only for `Build`) and a `system_prompt()` that
  *tells* the model not to use tools in Chat/Plan mode. Nothing in the policy
  engine knows the mode.
- The mode is persisted in config (`AppConfig.mode: AgentMode`,
  `crates/config/src/schema.rs`), surfaced by a desktop picker
  (`crates/desktop/src/views/chat.rs` → `SetMode` → `save_config`,
  `crates/desktop/src/app.rs`) and a CLI `SettingsField::InteractionMode`
  (`crates/cli/src/app.rs`) that feeds `config.mode` into `RequestBuilder`.
- `crates/orchestrator/src/runtime_runner.rs` maps the mode to
  `AgentTask::new_action_required` vs `AgentTask::new`; the multi-agent path
  dispatches **independently of mode** (its only branch is
  `!req.force_single_agent`).

Structural facts this ADR builds on (all verified in the current tree):

- **The policy gate is real and default-strict.** `PolicyPresets::default_rules()`
  and `strict()` gate every `filesystem` / `shell` / `git` operation as
  `RequireApproval`, with `AutoDeny` danger patterns ordered first in the
  first-match-wins `SimplePolicyEngine` (never silently relaxed). `Build` picks
  up real enforcement only because the executor's `RequireApproval` path asks
  an `Arc<dyn ApprovalSink>`.
- **The read-only guarantee is prompt-only.** Chat/Plan produce an
  `AgentTask::new` (no tool requirement) and a prompt that instructs *not* to
  use tools, but the policy engine doesn't know it — a model that ignores the
  prompt can still open the same `RequireApproval` dialogs as `Build`. This is
  the pre-existing gap the third-generation gate (§Decision 2) closes.
- **Authorization state does not exist today.** `SimplePolicyEngine` composes
  over static `rules` plus optional infrastructure trackers (spend, RPM); there
  is no intent/authorization hook. Approval is per-call: `record_approval_decision`
  (`crates/core/src/executor.rs`) writes a `rule_matched = "user_approval"`
  audit row sharing a `correlation_id`/`input_hash` with the preceding
  `RequireApproval` verdict; `request_ack` (`crates/core/src/traits/approval.rs`)
  returns a `bool` and writes **no audit row** (used today e.g. for the
  not-a-git-repo undo warning in `agent_loop.rs`).
- **Plan-binding artifacts already exist** (ADR-52): the checkpoint/plan
  machinery persists `plan_id`, `objective_hash`, `source_revision`
  (`crates/sessions/src/lib.rs`, `crates/orchestrator/src/checkpoint.rs`), and
  auto-resume already compares `checkpoint.objective_hash == input_hash`
  (`crates/orchestrator/src/runtime_runner.rs`). This ADR binds authorization
  to those identifiers rather than inventing new ones.
- **Events are additive.** `EventKind` (`crates/core/src/event.rs`) is a
  `#[non_exhaustive]` struct-variant enum with serde renames; consumers match
  with wildcard arms, so new variants are safe.
- **Sessions never stored a mode** (schema holds `provider`/`model` only), so
  mode removal has nothing to migrate in the session DB.

A design review produced **five blocking findings**: (1) routing and
authorization were conflated in one type; (2) there was no capability tier
between "ask every time" and "trust the model"; (3) plan acceptance was an LLM
announcement, not a user decision; (4) grants had no lifetime/revocation
semantics; (5) the new decision channels bypassed the audit. The decisions
below resolve each.

## Decision

Route user intent, authorize mutations from that intent, and never let the
classifier grant authority. Ten decisions:

### 1. Split types — the router never carries authorization

```rust
RouterOutput { outcome: RequestedOutcome, scope: TaskScope,
               confidence: f32, route: RuleHit | LlmClassifier | AskUser }
```

Authorization is **exclusively user-event-driven** and owned by the run loop,
not by routing or classification:

```rust
AuthorizationState { granted: bool, plan_id: Option<String>,
                     binds: (objective_hash, source_revision),
                     granted_at: Option<OffsetDateTime> }
```

**Hard rule:** the classifier (and the deterministic router) can *classify*,
never *grant*. No model output — chat or classifier — can upgrade
`RequireApproval` to `Allow`. `AuthorizationState` transitions only in response
to confirmed user decisions (§2, §3, §4).

### 2. Third-generation gate — three capability tiers

- **Observe** (auto): reads, inspection, planning — no authorization needed.
- **Mutate-local** (authorizable *within scope*): file edits, undoable local
  changes inside `TaskScope` — grantable via §3/§4 user confirmation.
- **Consequential** (never covered by blanket authorization): `git push`
  network egress, destructive/reverting operations, secrets access. Always
  prompts; blanket authorization can never cover these.

`SimplePolicyEngine` gains a new pure `Condition::IntentAuthorized { scope }`,
injected **after the `AutoDeny` danger patterns and before the
`RequireApproval` defaults** (first-match-wins): it upgrades
`RequireApproval` → `Allow` only when `AuthorizationState` grants the matching
scope. `Allow` is also final for denials — **authorization only upgrades
`RequireApproval`, never overrides `Deny`**. The matched rule flows into the
existing audit row as `rule_matched = "intent_authorized"` (reusing the
verdict-row/decision-row correlation pair from `record_approval_decision`). The
engine is backed by `Arc<dyn IntentAuthorization>` (a state source, not a
decision maker) — keeping the engine itself deterministic.

**Shell scope hole (Phase 1):** `resolve_path`
(`crates/tools/src/common.rs`) confines only the `filesystem` tool's paths;
shell `argv`/`cwd` do not pass through it. Scope-bound shell authorization is
deferred until shell argv/cwd get the same containment.

### 3. Plan agreement — an explicit dialog, not an announcement

Accepting a plan is a real user decision: the run loop shows an "Apply it?"
dialog implemented as `ApprovalSink` calls (audited, blocking), binding
`plan_id`, `objective_hash`, and `source_revision`. The diff shown to the user
is verified against the expected plan artifacts (ADR-52 `PlanArtifact` /
checkpoint fields). The dialog is a decision record, never an LLM prose offer.

### 4. Grants — session-scoped, non-durable, re-confirmed on resume

Grants are **per-plan and session-scoped**: they are revoked by `Stop`, by a
changed objective (new `objective_hash` — the same comparison auto-resume
already performs), and never cross session boundaries. Grants are
**non-durable** — on every auto-resume of a same-input run the mutation
boundary re-confirms them through the same dialog channel (§3). There is no
disk-persisted "trust this tool forever."

### 5. Audit hardening — close the `request_ack` gap first

1. **First:** `request_ack` becomes an audited decision channel — calls record
   `ForceContinue` / `Aborted` outcomes through the same
   `record_approval_decision` machinery (same `correlation_id` chain), so a
   non-git-repo continuation or any ack prompt is in the audit.
2. **Then:** add intent-classification records (router decision + classifier
   rationale) and plan-approval records, reusing the `correlation_id` chain.

### 6. Router — deterministic rules + negation corpus, LLM only for ambiguity

- Deterministic rules and conversation context decide first; a **negation
  corpus wins over positive keywords** (positive matches are suppressed by
  negation).
- The LLM classifier runs **only** on ambiguity remaining after the rules;
  `confidence: f32` is used for path selection only — **low (`< 0.7`) →
  read-only + ask** (never authorization, §1).
- Output set: Answer / Diagnose / Review / Plan / Execute / **Verify** (new,
  added to the user's original set).
- Relative utterances ("do it", "apply that") resolve via conversation context.
- **Every routing decision is snapshotted to audit** (Phase 1; the type and the
  event land in Phase 0).

### 7. Stages — transient `RunStage`, additive event

```rust
RunStage { Understand, Inspect, Plan, Execute, Verify, Complete }
```

`RunStage` is **transient** (never persisted); the machine lives in
`runtime_runner` (the shared single/multi-agent entry), not inside the agent
loop. Progress publishes an **additive `RunStageChanged`** event kind and
drives the status chip (Responding / Inspecting / Planning / Editing / Testing /
Waiting / Blocked).

### 8. Mode removal order — the gate lands before the picker disappears

The mutation gate (§2) and the plan-approval UX (§3) must ship **before** the
Build/Chat/Plan picker is removed. Today the picker is the de-facto read-only
guarantee; removing it first would regress to an unconditionally-gated prompt-only
model. Removal touches desktop (`views/chat.rs` picker, `app.rs` `SetMode`
config write), CLI (`SettingsField::InteractionMode`), and the config schema
(migration). **Sessions never stored a mode, so there is nothing to migrate
there.**

### 9. Classifier runtime — same model, spend-tracked, toggleable

The classifier uses the same chat model by default, routed through
`SpendTracker`/RPM like any call. A config toggle disables the classifier
entirely, forcing **read-only + ask** for any request that would have been
classified (§6).

### 10. Phases

- **Phase 0 (now, additive):** split types (`RouterOutput`, `AuthorizationState`),
  deterministic router + negation corpus, `RunStage` + additive `RunStageChanged`
  event, `request_ack` audit hardening. All additive; ships without changing
  default behavior.
- **Phase 1:** the `Condition::IntentAuthorized { scope }` gate, plan-approval UX,
  classifier compliance (§6/§9 confidence + spend), shell argv/cwd containment,
  then the mode-picker removal (§8).
- **Phase 2:** status chip UX, orchestration depth per outcome, session
  persistence/migrations, and durable audit coverage for the new records.
  **Complete as of 2026-08-11** — Phase 2a (status chip UX) and Phase 2b
  (orchestration depth per outcome) landed earlier (`df5c2e7`, `5818642`);
  the remaining v2 items (shell argv/cwd containment, schema-derived audit
  columns, dialog plan-text verification, optional LLM classifier) landed
  2026-08-11 via `3cb251d`/`21d4d3e`/`44f2deb`/`9ec27f3`.

## Consequences

- **Positive.** The read-only guarantee stops being prompt-only: external write
  authority is granted by confirmed user decisions bound to an immutable
  (objective, revision) scope, never by model classification. Consequential
  actions remain unconditionally gated; denial is final. Every decision surface
  (approval, ack, routing, plan acceptance) lands in the audit with a shared
  `correlation_id`.
- **Negative / trade-offs.** Grants are deliberately non-durable, so a resumed
  same-input run re-prompts at the mutation boundary — a usability cost paid for
  safety. The classifier is an added model call (spend-tracked, toggleable).
  Three capability tiers mean an operator may find the "prompt once per plan"
  flow more ceremony than the current prompt-per-call flow for small edits.
- **Risks.** The gate's power comes from the injection point: the new condition
  must sit **after** `AutoDeny` danger patterns and **before** the blanket
  `RequireApproval` defaults, and must never be able to flip a `Deny` to
  `Allow`. Shell scope cannot authorize cross-root writes until `argv`/`cwd`
  containment lands (Phase 1) — until then the gate authorizes fs-tool scope
  only.
- **Migration.** Sessions carry no mode, so nothing to migrate there. Config
  schema requires the mode-removal sequence guarded by §8. All Phase 0 additions
  are additive (`serde(default)`, `#[non_exhaustive]`, new event variants with
  wildcard consumers).

## Open questions with assumed defaults

These six were asked and are recorded as **assumed defaults — REVOCABLE**. Each
is a one-line default-shift if the reviewer disagrees.

1. **Grant lifetime:** per-plan, session-scoped grants. Assumed: grants are
   revoked by `Stop` or a changed objective.
2. **Prompting granularity:** prompts remain for in-scope Mutate-local only;
   Consequential always prompts. Assumed: no prompt-silencing beyond Mutate-local
   scope.
3. **Overridable denials:** `Deny` is never overridable (not even by a future
   grant).
4. **Plan dialog binding:** explicit, verified against expected plan artifacts,
   binding `(plan_id, objective_hash, source_revision)`.
5. **Resume behavior:** grants are re-confirmed at the mutation boundary on
   every auto-resume of a same-input run (no persistence).
6. **Classifier runtime:** same chat model, spend-tracked through
   `SpendTracker`/RPM, disable-able via config toggle forcing read-only when off.

## Review notes

- The type split (§1) is the load-bearing decision: every earlier design that
  let routing carry authorization either trusted the classifier or duplicated
  state. Keeping `AuthorizationState` in the run loop makes "who granted what,
  when, bound to which revision" a single auditable value.
- The three-tier gate is deliberately conservative: Observe needs nothing,
  Mutate-local is grantable only in scope, and Consequential sits outside what
  any grant can reach. This is the ceiling, not the floor — Phase 1 may tighten
  it (e.g. scope-bound shell).
- `request_ack` hardening first (§5) is intentional ordering: the new dialog
  channels reuse the same record path, so hardening the existing bool-returning
  ack before adding plan/classifier records keeps one audit story instead of
  three.

## Addendum (Phase 1d)

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

## Addendum (Phase 1e)

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

## Addendum (Phase 2b)

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

## Addendum (Phase 2c)

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
