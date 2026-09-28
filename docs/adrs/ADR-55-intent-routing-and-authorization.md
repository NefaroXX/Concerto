# ADR-55: Intent routing and intent-gated authorization — one loop, deterministic containment

> **Partially superseded (2026-08-11) by
> [ADR-56](ADR-56-model-first-intent-classification.md):** the Phase 2c §2
> `classifier_enabled` default pin (false → true) and the Phase 2c §3
> classifier placement pin (AskUser-only → model-first with two deterministic
> fast paths) — both now recorded as history in §10 below. **Partially
> superseded (2026-09-24) by [ADR-71](ADR-71-coordinator-supremacy.md):**
> outcome→topology branching (what was Phase 1e §2 here) and the Phase 2b §1
> planner-roster contract — the coordinator owns run shape; intent signals are
> advisory. Every other decision in this ADR remains in force. The
> phase-by-phase record of how this decision landed (addenda 1d–2e, live-fix
> rounds, acceptance lists) was removed on 2026-09-28 with the rest of the
> research/archive tiers; the load-bearing outcome of each phase is stated in
> this ADR's decision sections and in [ADR-56](./ADR-56-model-first-intent-classification.md).

**Status:** Accepted
**Date:** 2026-08-09
**Deciders:** Concerto architecture
**Supersedes:** the `AgentMode` (Build/Chat/Plan) prompt-level picker as the
    de-facto read-only guarantee — removed (Phase 1e) once the mutation gate
    and plan-approval UX landed (§7).
**Composes with:** ADR-52 (durable plan artifacts: `plan_id`,
    `objective_hash`, `source_revision`), ADR-46/ADR-48 (reasoning-as-data,
    ContextEngine — unchanged), ADR-44 (session-scoped `VirtualFs`), ADR-26
    (audit/correlation-id chain), ADR-37 (capability lifecycle), ADR-60
    (process-per-agent runtime, single write gate), ADR-65 (evidence spine),
    ADR-66 (harness tool-call guarantee), ADR-71 (coordinator supremacy — run
    shape), ADR-74 (delegation doctrine).

## Context

The problem this decision solves: the only thing separating "read-only" agent
behavior from "mutates the codebase" was the **prompt-level mode picker**.

- `AgentMode { Build, Chat, Plan }` drove `is_action_required()` and a
  `system_prompt()` that *told* the model not to use tools in Chat/Plan mode.
  Nothing in the policy engine knew the mode — a model that ignored the prompt
  could still open the same approval dialogs as `Build`.
- The multi-agent path dispatched **independently of mode**.
- There was no authorization state: `SimplePolicyEngine` composed over static
  rules plus optional infrastructure trackers; approval was per-call.

Structural facts the decision builds on (verified in the current tree):

- **The policy gate is real and default-strict.** `PolicyPresets::default_rules()`
  and `strict()` gate every `filesystem` / `shell` / `git` operation as
  `RequireApproval`, with `AutoDeny` danger patterns ordered first in the
  first-match-wins `SimplePolicyEngine` (never silently relaxed).
- **Plan-binding artifacts already exist** (ADR-52): the checkpoint/plan
  machinery persists `plan_id`, `objective_hash`, `source_revision`, and
  auto-resume already compares `checkpoint.objective_hash == input_hash`.
  This ADR binds authorization to those identifiers rather than inventing new
  ones.
- **Events are additive.** `EventKind` is a `#[non_exhaustive]` struct-variant
  enum with serde renames; consumers match with wildcard arms.
- **Sessions never stored a mode**, so mode removal has nothing to migrate in
  the session DB.

A design review produced **five blocking findings**: (1) routing and
authorization were conflated in one type; (2) there was no capability tier
between "ask every time" and "trust the model"; (3) plan acceptance was an LLM
announcement, not a user decision; (4) grants had no lifetime/revocation
semantics; (5) the new decision channels bypassed the audit. The decisions
below resolve each.

## Decision

Every non-empty run enters **one unified agent loop**; intent never selects a
code path; mutations are authorized by confirmed user decisions enforced at
the policy engine; and nothing that classifies can ever grant. Nine
decisions:

### 1. One loop — intent never selects a code path

Every non-empty run enters the unified agent loop (`run_text_only` is
deleted; single-agent vs coordinator selection is unchanged —
`force_single_agent` is the only remaining explicit mode switch). The model is
an agent in the environment: it chooses tools itself and acts on tool
feedback in the loop (Anthropic "Building Effective Agents" augmented-LLM
pattern; OpenCode's single loop with per-tool permission rules — no request
classification). Chat is simply what the loop does when the model uses no
tools. The coordinator owns run shape (ADR-71); intent signals are advisory
flavor, never topology.

**Containment is mechanical and deterministic — enforced where actions
happen, not by pre-classifying the message:**

1. **Deny-class rules run first and are final.** `AutoDeny` danger patterns
   and any `Deny` verdict sit first in the first-match-wins policy engine; a
   `Deny` can never be upgraded to `Allow` by any grant or authority.
2. **The approval sink is the grant channel.** Every write outside a standing
   grant prompts. The coordinator's own dispatch carries
   `orchestrator_authority`, which the gate maps to
   `rule_matched = "coordinator_authority"` for non-Consequential actions,
   while Consequential actions keep their approval path (§3).
3. **Loop guards bound runaway behavior** — zero-work guard, cycle budget
   (`CycleBudgetTracker` in `crates/orchestrator/src/cycle.rs` — "detects
   repeated identical tool calls"), continuation caps, spend fuses, and the
   empty-completion honesty rule (a loop ending with empty final text still
   synthesizes the explanatory completion). Cycle detection was consolidated
   here from the removed `cycle_manager`; no separate cycle module exists.

**Considered and rejected: auto-grant-then-branch (issue #27).** An earlier
design made `route()` + classifier output *the decision*: high-confidence
outcomes auto-granted and the routed outcome selected the run's code path
before any model was consulted. Three consecutive smoke failures shared one
mechanism, not three causes — a `verify` subordinate clause hijacked builds
into tool-less chat, `don't` inside constraints hijacked into the read-only
sink, and a `Prompt:` label glued to the leading verb blinded whole-token
matching. Narrowing instances cannot close a mechanism that generates them,
so the mechanism was replaced. The rejection's reasoning survives as this
paragraph; its vocabulary (`auto_granted`) survives only in gate-test
fixtures. `route()` and its keyword corpora were later deleted from
`crates/core/src/intent.rs` (2026-09-24); the module now retains only the
intent vocabulary (`RequestedOutcome`, `TaskScope`, `RouterOutput`,
`RouterRoute`, `RunStage`, `PlanDecision`, `LOW_CONFIDENCE_THRESHOLD`) and the
audit rule-name round-trip.

### 2. Split types — routing never carries authorization

`RouterOutput { outcome, scope, confidence, route }` and the authorization
state are separate types:

```rust
AuthorizationState { granted: bool, plan_id: Option<String>,
                     binds: (objective_hash, source_revision),
                     granted_at: Option<OffsetDateTime> }
```

Implemented as the run-scoped `IntentGrantStore` plus `SessionIntentAuth`
(`crates/orchestrator/src/intent_grants.rs`): a run starts read-only and the
run loop flips it once per run; grants enter only via confirmed decisions.
**Hard rule:** nothing that classifies — the deterministic tier classifier
(§3) or any model output — can ever *grant*. `IntentAuthorization` is a
verdict *source* the engine consults, not a decision maker; the engine stays
deterministic. Under full local agency the run envelope is `Acting` for every
non-empty run (`runtime_runner.rs`: no route-derived grant, no
read-only-from-routing); `RunEnvelope::ReadOnly` remains reachable where a
run is declared read-only (a Replan/dismissed plan decision, a denial), and
there the `intent_readonly_deny` verdict stays a hard, pre-sink deny.

**Read-only declaration semantics (where a run is read-only, e.g. Replan):**
only a **task-level prohibition** — a phrase standing alone or preceding an
explicit action keyword — declares read-only. A requirement clause that
appears *after* an explicit action request ("build X … do NOT read it as
UTF-8", "… must not panic") constrains the artifact the user asked for; it
does not prohibit the action, passes through as ordinary task text, and never
demotes the run. Reassurance markers (`don't panic`, `don't worry`) never
veto in any position. Unclear input gets bounded in-loop clarification
(≤1 turn), zero writes.

### 3. Three capability tiers — the only gate

- **Observe** (auto): reads, inspection, planning — no authorization needed.
- **Mutate-local** (authorizable *within scope*): file edits, undoable local
  changes — grantable via §4/§5 user confirmation.
- **Consequential** (never covered by blanket authorization): `git push`
  network egress, destructive/reverting operations, secrets access,
  install/publish, force-flags. Always prompts.

`classify_tier` (`crates/core/src/authorization.rs`) is the pure, deterministic
action classifier feeding `IntentVerdict`; the policy engine's
`Condition::IntentAuthorized` (injected **after `AutoDeny`, before the
`RequireApproval` defaults** — first-match-wins) applies the verdict:
`Allow` upgrades `RequireApproval` → `Allow` with the verdict's rule in the
audit row; `RequireApproval` keeps the approval path; `Deny` is final and
never surfaced to the approval sink. Backed by `Arc<dyn IntentAuthorization>`
(state source, not decision maker).

**Shell scope — project-bounded, shipped.** The Phase 1b shell argv/cwd
containment (`crates/tools/src/containment.rs`: `cd`/`pushd` targets, path
tokens, `xargs` pipelines, redirect writes, git `-C`, empty-root
disablement) confines the shell tool. Under an Acting run, a shell call
inside those bounds upgrades to `Allow` with its own audited row
(`rule_matched = "intent_authorized_shell"`) exactly when
`is_project_bounded_shell` proves the call resolves inside the project root —
no network, no denylist/Consequential match, no escape in command text / `cd`
/ redirect targets. Everything outside keeps `shell_requires_approval`. Shell
is never grantable.

**Delegation scope (2026-09-10).** Under the same Acting run, the
Coordinator's `call_specialist` dispatch — the only dispatch tool the
coordinator's decision loop policy-evaluates — upgrades to `Allow` with its
own distinct, individually auditable row
(`rule_matched = "intent_authorized_delegation"`). Cause-only-no-effect: this
authorizes the dispatch, never the specialist's own work — every specialist
tool call stays policy+grant-gated, spend/task caps still bound the fan-out,
and a read-only run denies delegation outright.

### 4. Plan agreement — binding-governed, explicit or auto-Apply

Accepting a plan is a real user decision, not an LLM proclamation.

- **Bindings are durable.** `plan_bindings` (migration 023:
  session_id, objective_hash, plan_id, plan_text, source_revision,
  created_at_ms; UNIQUE(session, objective); newest-wins UPSERT; rows deleted
  by `delete_session`) mirrors the in-process registry
  (keyed `(session_id, objective_hash)`, newest-wins, re-planning replaces).
  `plan_text` is the rendered plan, capped at 16 KiB — never a completion
  placeholder; an empty final message prevents failure bindings. A lookup
  miss yields the generic Execute — never a stale plan gate.
- **Apply/Replan dialog:** a confirming
  `ApprovalSink::request_plan_approval` call (audited, blocking) showing
  "plan made at rev X, current rev Y". Apply = `grant_execute` (fs+git),
  audited `"granted"`, and the run executes the **approved plan text**
  (`approved_plan_task_description` / `build_run_task`), never the approval
  phrase. Replan = read-only answer-only run planned anew, no grants.
  Dismiss = read-only, no mutation possible.
- **Exact-objective auto-Apply** (what survives of the click-tax removal):
  when a stored binding exists for **this exact objective** and verifies
  against its artifact hash (`resolve_auto_apply_binding` —
  in-process registry, then a restart-safe durable leg), the run auto-Applies
  it: no dialog, no keyword, audited `auto_apply` via the `record_plan_decision`
  seam, and the row is consumed in both stores so a later run cannot re-apply
  an executed plan. **Loud-fail on drift** — a binding whose artifact hash
  moved fails the run as `Unrecoverable` (never silent re-decompose; ADR-60
  D7 whiteboard rehydration adds a revision divergence guard). A different
  objective never auto-Applies; a missing/unverifiable row falls through to
  the unified loop (fail-soft).
- **Checkpoint precedence (Phase 2b).** Planning-only runs never write or
  clear the orchestration checkpoint, so an in-flight partial Execute's
  crash-recovery checkpoint survives; an Apply clears/suppresses a stale
  checkpoint so the approved objective executes the approved plan, never a
  silently resumed partial graph.

### 5. Grants — session-scoped, non-durable, re-confirmed

Grants are **per-plan and run-scoped**: created fresh per `run_shared_agent`
call, dropped at run end, revoked by `Stop` or a changed objective (new
`objective_hash`) — never cross session boundaries, never persisted. There is
no disk-persisted "trust this tool forever"; a resumed same-input run
re-confirms at the mutation boundary. The only grant is `grant_execute`
(filesystem + git) on a **confirmed** Execute: an Apply decision (§4) or a
user-picked Execute. Shell and Consequential are outside every grant.

### 6. Audit — every decision surface on one correlation chain

1. `request_ack` is an audited decision channel — `ForceContinue` / `Aborted`
   outcomes flow through the same `record_approval_decision` machinery (same
   `correlation_id` chain).
2. Plan decisions use the `record_plan_decision` seam: synthetic
   `tool_name = "intent:plan"`, decision in `rule_matched`/`verdict`,
   `input_hash = objective_hash`, `user_response` JSON
   `{ plan_id, source_revision }`; variants `apply` / `replan` / `dismissed` /
   `auto_apply`.
3. The routed `intent_router` grant rows are **no longer written** — the
   routed-authority decision they recorded is gone. Action rows
   (`record_approval_decision`, executor action rows), plan-decision rows, and
   the coordinator's own decision rows remain the audit trail. Every grant
   carries its rule name (`intent_authorized`, `intent_authorized_shell`,
   `intent_authorized_delegation`, `coordinator_authority`, `un_granted`,
   `consequential`, `intent_readonly_deny`).

### 7. Mode removal — the gate landed before the picker disappeared

The mutation gate (§3) and the plan-approval UX (§4) shipped **before** the
Build/Chat/Plan picker was removed — removing it first would have regressed to
an unconditionally-gated prompt-only model. The removal deleted the desktop
picker, `SetMode`, `SettingsField::InteractionMode`, and `AgentMode` itself;
the gate is now the **always-on** authorization path for single- and
multi-agent runs alike (the shared executor's `SessionIntentAuth` starts
read-only). Config schema v6 dropped `mode` and `[intent] enabled`; existing
configs with `enabled = false` silently lost the opt-out (the key is ignored
at load) — deliberate: the gate is mandatory. Sessions never stored a mode, so
there was nothing to migrate.

### 8. Outcomes are flavor hints — prompt mapping, never path selection

`RequestedOutcome { Answer, Diagnose, Review, Plan, Execute, Verify }` stays
in the vocabulary and feeds exactly:
- one system-prompt line ("the user seems to want verification; prefer
  checking over changing"), and
- the audit/logging row.

It never branches the run, never selects a topology (ADR-71), and never — by
itself — grants anything. Execute → Build prompt, Plan → Plan prompt, all
other outcomes → Chat prompt; prompt texts remain core consts shared with the
eval-runner. The deterministic keyword corpora that once "routed" these
outcomes were deleted with `route()` (§1); the negation corpus's *semantic*
survives only as the read-only declaration rule in §2.

### 9. RunStage — transient, additive

`RunStage { Understand, Inspect, Plan, Execute, Verify, Complete }` is
transient (never persisted); progress publishes an additive
`RunStageChanged` event and drives the status chip. Stage feeds must not
advance to Execute during planning-only runs.

### 10. The LLM classifier — considered, shipped, retired

The model-first intent classifier arc, one decision:

1. **Proposal-only** — the classifier was an added model call at the
   router's AskUser sink, optional, off by default (§9 as first designed).
2. **Shipped as a real component (Phase 2c, schema v7)** — mounted only at
   the AskUser sink, fail-soft, one bounded non-streaming call,
   reserve-before-call spend, JSON envelope `{route, confidence, rationale}`,
   confidence threshold validated `>= LOW_CONFIDENCE_THRESHOLD` (0.7) so no
   configuration could create a band where a re-routed Execute missed the
   gate's confirmation.
3. **Model-first (ADR-56)** — became the primary decider for every
   non-fast-path message (default flipped true), with negation and smalltalk
   as deterministic fast paths.
4. **Auto-grant experiment rejected** — "the classifier may auto-grant at
   high confidence" (2026-09-06) was the dispatch-grant variant of §1's
   rejected design; the unified-loop decision retired it from the hot path
   (2026-09-09).
5. **Retired and removed** — the config surface was dropped at schema v8
   (2026-09-11; unknown-key tolerant, so old files keep loading; **no
   threshold invariant survives it**), and the `intent_classifier` module was
   deleted in the routing-carcass cleanup (2026-09-24, same cleanup as
   `route()` and `cycle_manager`).

**Surviving role, in citable terms:** nothing in the tree invokes an LLM
intent classifier on any path — `intent_classifier` appears only in config
migration history (v6→v7→v8). Intent classification survives as the
deterministic, pure `classify_tier` action classifier in
`crates/core/src/authorization.rs` (Observe / MutateLocal / Consequential),
which feeds `IntentVerdict` into the policy gate — an **authorization-input
and audit-label mechanism**, never a dispatch gate, and it can never grant.
`LOW_CONFIDENCE_THRESHOLD` (0.7) remains the gate's shared constant. Details:
[ADR-56](ADR-56-model-first-intent-classification.md).

## Consequences

- **Positive.** The read-only guarantee is no longer prompt-only and no
  longer keyword-derived: external write authority comes from confirmed user
  decisions bound to an immutable (objective, revision) scope, enforced
  mechanically at the policy engine (deny-class first, approval sink,
  coordinator authority, cycle/spend/continuation guards). Consequential
  actions remain unconditionally gated; denial is final. Every decision
  surface lands in the audit with a shared `correlation_id`.
- **Negative / trade-offs.** Grants are deliberately non-durable, so a
  resumed same-input run re-prompts at the mutation boundary — a usability
  cost paid for safety. The unified loop gives the model full local agency,
  so safety rests on the guards and the approval sink rather than on
  restricting what the model may attempt; runaway behavior is bounded by the
  loop guards, not prevented. Keyword routing is gone, so there is no
  cheap offline "answer" for inputs a model cannot see.
- **Risks.** The gate's power comes from the injection point — `IntentAuthorized`
  must sit **after** `AutoDeny` and **before** the blanket `RequireApproval`
  defaults and must never flip a `Deny`. The coordinator authority branch
  deliberately never auto-allows Consequential. Auto-Apply's safety rests on
  the binding being exact-objective and artifact-hash-verified; the
  fail-soft legs must never widen into a grant.
- **Migration.** Sessions carry no mode; config schema history is
  v5→v6 (mode/`enabled` dropped) → v7 (classifier keys) → v8 (classifier keys
  dropped); all retained keys are additive (`serde(default)`,
  `#[non_exhaustive]`, unknown-key tolerant).

## Open questions with assumed defaults

These were asked and are recorded as **assumed defaults — REVOCABLE**. Each
is a one-line default-shift if a reviewer disagrees; all remain in force:

1. **Grant lifetime:** per-plan, run-scoped grants. Assumed: revoked by
   `Stop` or a changed objective; never persisted.
2. **Prompting granularity:** prompts remain for in-scope Mutate-local only;
   Consequential always prompts. Assumed: no prompt-silencing beyond
   Mutate-local scope.
3. **Overridable denials:** `Deny` is never overridable (not even by a future
   grant).
4. **Plan dialog binding:** explicit, verified against expected plan
   artifacts, binding `(plan_id, objective_hash, source_revision)`; the
   exact-objective auto-Apply is the only click-free grant path.
5. **Resume behavior:** grants are re-confirmed at the mutation boundary on
   every auto-resume of a same-input run (no persistence).

## Review notes

- The type split (§2) is load-bearing: every earlier design that let routing
  carry authorization either trusted the classifier or duplicated state.
  Keeping `AuthorizationState` run-scoped makes "who granted what, when, bound
  to which revision" a single auditable value.
- The three-tier gate is deliberately conservative: Observe needs nothing,
  Mutate-local is grantable only in scope, and Consequential sits outside
  what any grant can reach.
- The one-loop decision (§1) is the ceiling, not the floor: deterministic
  containment (deny-class first, approval sink, loop guards) replaces
  keyword dispatch because the keyword mechanism itself was the defect — a
  mechanism-level failure cannot be fixed instance-by-instance.