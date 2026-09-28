# ADR-56: Model-first intent classification — the LLM decides intent; deterministic rules become fallbacks

**Status:** Accepted (2026-08-11) — supersedes two ADR-55 Phase 2c pins, in
    part: Phase 2c §2 (`classifier_enabled` default **false** → **true**) and
    Phase 2c §3 (AskUser-only classifier placement → model-first with two
    deterministic fast paths), both as recorded in ADR-55 §10. **Operating
    status:** the model-first classifier is **retired from run dispatch** by
    the unified-loop decision (ADR-55 §1/§10, 2026-09-09/09-24; ADR-71);
    the surviving intent-classification role is evaluation / authorization-
    input, never dispatch. Read "Current state" before the recorded design.
**Date:** 2026-08-11
**Deciders:** Concerto architecture
**Supersedes:** ADR-55 Phase 2c §2 (default pin) and Phase 2c §3 (placement pin), in part — as recorded.
**Composes with:** ADR-55 (deterministic tier classifier, policy gates, plan
    bindings, audit chain), ADR-26 (audit / correlation-id chain), ADR-52
    (plan artifacts), ADR-44 (session-scoped `VirtualFs`), ADR-37 (capability
    lifecycle), ADR-71 (coordinator supremacy — run shape).

## Current state (read this first)

The LLM intent classifier does **not** run on any dispatch path today. In
citable terms:

- **No invocation:** nothing in the tree calls an LLM intent classifier —
  `intent_classifier` appears only in config-migration history (schema v6→v7
  added its keys, v7→v8 dropped them). The module was deleted in the
  routing-carcass cleanup of 2026-09-24 (the same cleanup that deleted
  `route()` and `cycle_manager`; ADR-55 §1/§10).
- **No config surface:** the `[intent]` classifier keys
  (`classifier_enabled`, `classifier_model`, `classifier_confidence_threshold`)
  were removed at schema **v8** (2026-09-11); loading is unknown-key tolerant
  so old files keep loading. **No threshold invariant survives** — the
  `>= LOW_CONFIDENCE_THRESHOLD` (0.7) validation that bound the re-route band
  is gone with the keys.
- **No auto-grant on the hot path.** The 2026-09-06 amendment that let the
  classifier auto-grant at high confidence is **moot and removed** (its text
  is preserved verbatim in
  [`archive/ADR-55-phase-history.md`](archive/ADR-55-phase-history.md)). Grants
  come only from confirmed user decisions: an Apply/Replan plan decision or a
  user-picked Execute, both through `grant_execute` — or from the
  **exact-objective auto-Apply** of a user-approved, artifact-hash-verified
  plan binding (ADR-55 §4), which is keyed to the binding, never to model or
  classifier output.
- **What survives as intent classification:** the deterministic, pure
  `classify_tier` action classifier in `crates/core/src/authorization.rs`
  (Observe / MutateLocal / Consequential), feeding `IntentVerdict` into the
  policy gate as an **authorization-input and audit-label mechanism** — it
  classifies *actions*, never routes runs, and never grants.
  `LOW_CONFIDENCE_THRESHOLD` (0.7) remains the gate's shared constant.
- **Why it stopped gating dispatch (the reasoning, stated natively):** the
  tool-level path is authoritative. Every non-empty run enters one unified
  agent loop in which the model selects tools and containment is enforced at
  the policy engine — deny-class rules first, the approval sink, and loop
  guards (ADR-55 §1). A model-first *request* classifier interposed between
  the user and that loop could at best save a model call and at worst
  re-introduced exactly what the unified loop removed: a pre-loop decision
  point whose mistakes became dispatch decisions. With the loop in place the
  classifier's remaining honest jobs — evaluating intent offline and feeding
  authorization context — are served by the deterministic tier classifier;
  the added LLM call had no reader.

## Context

Live testing (session `01KZS8AGP512QFR0T2246WEWBJ`) surfaced three recurring
routing failures that the deterministic keyword corpora of ADR-55 cannot fix by
adding rules:

1. **Plain chat falls into the AskUser modal.** "hi, lets work on something" —
   an ordinary conversational opener with no code intent — fell to the
   six-option AskUser modal because the router had no general path to "just
   chat".
2. **A read-only build request was lucky, not guaranteed.** A build request
   ending "Do not touch the filesystem or run any commands until the plan is
   approved." was caught by the negation corpus and routed read-only to Plan.
   The negation corpus is doing safety work a model must also respect — but
   today the corpus is the *only* thing standing between that phrasing and an
   Execute route.
3. **Chat is inherently ambiguous and a keyword corpus cannot represent it.**
   Statements containing intent words — "lets build a game", "i was planning my
   vacation", "we should fix the website" — are hijacked by the keyword corpora
   because `route()` evaluates corpora **before** any LLM. Chat can be about
   literally anything; no finite corpus can distinguish "talk about building a
   house" from "build a house".

The structural point that settled the direction: **the LLM reads the
conversation; the keyword corpus cannot.** Market LLM coding tools (opencode,
Claude Code, and peers) do not use keyword intent classification at all — the
model reads the conversation and tool use is gated at the tool/permission
level, not by pre-classifying the message into a fixed outcome set. ADR-55
Phase 2c deliberately shipped the classifier as an off-by-default wrapper at
the AskUser sink only. This ADR superseded exactly those two pins: the
classifier became the primary decider, and the deterministic rules became
fallbacks and safety nets.

## Decision (as recorded) — and how it operates today

Sections 1–3, 5–7 record the model-first design as accepted (2026-08-11).
Sections 4 and 8 state the decision as it **operates today**; read them with
the "Current state" block above. The model-first design is superseded in
force by the unified-loop decision (ADR-55 §1) — the classifier no longer
runs on dispatch — but the supersession's lesson survives: request
classification is an evaluation/authorization-input concern, and the safety
invariant that no model output can authorize a mutation is **stronger** now
than when this ADR was written.

### 1. Primary decider — the LLM classifies every message except two fast paths (recorded)

When `[intent] classifier_enabled` is true, the LLM classifier becomes the
intent authority for **every** user message except two deterministic fast
paths, which run **before** the classifier:

- **(a) Negation-override** (read-only safety invariant): a user saying
  "don't touch", "do not …", "never …", "without changing …" must never be
  overridden by a model — even a mistaken one.
- **(b) Smalltalk route** (zero-cost chat): pure greetings/pleasantries of at
  most `SMALLTALK_MAX_INPUT_LEN` (48) characters route to a read-only `Answer`
  so "hi" never costs an LLM call.

Every other message — including explicit keyword hits, question-detection
results, and AskUser-remaining ambiguity — was classified by the model when
the classifier was enabled; the deterministic rules no longer
short-circuited the LLM. This reversed the 2c wrapper semantics: the
classifier mounted **after the two fast paths and before** any rule hit,
question detection, or the AskUser sink. As accepted, this section was the
operating decision; see "Current state" for the superseding one.

### 2. Default — `classifier_enabled` flips to true (recorded)

`classifier_enabled` defaults flipped **false → true**; `classifier_model`
stayed `None`-default (fallback to the run's chat model, 2c §2/§9). Spend
cap / reservation semantics were unchanged: reserve-before-call gates the
call and fail-soft on cap-exceeded (2c §6 unchanged). The default flip is
moot in force — schema v8 removed the keys (Current state).

### 3. Demoted fallbacks — the deterministic chain stood as today (recorded)

When the classifier was disabled, unavailable, cancelled, malformed, or
fail-soft, the full deterministic chain — **negation → question → explicit
keywords → smalltalk → AskUser** — stood exactly as today. Offline behavior
was the fallback, never the primary. This chain itself was later removed with
`route()` (ADR-55 §1): the unified loop replaced the chain's dispatch role,
and containment became mechanical (deny-class rules + approval sink + loop
guards) rather than corpus-derived.

### 4. The classifier is not a dispatch-routing gate; no auto-grant on the hot path

The classifier's role is **evaluation / observability and authorization-input**,
not dispatch. Concretely, today:

- **Routing happens in the loop, not before it.** Every non-empty run enters
  the unified agent loop; the coordinator owns run shape; intent signals are
  advisory flavor (ADR-55 §1/§8, ADR-71). There is no pre-loop classification
  step whose output selects a code path — so there is nothing for a
  classifier confidence threshold to re-route, and the recorded
  above-threshold re-route / below-threshold-stands semantics of this ADR
  (and of 2c §3) no longer have an operating site.
- **Authorization input is deterministic.** The only classifier consulted by
  the policy engine is `classify_tier` (`crates/core/src/authorization.rs`),
  which classifies each *policy action* into Observe / MutateLocal /
  Consequential and feeds `IntentVerdict`. That verdict can only upgrade
  `RequireApproval` → `Allow` under a standing user-confirmed grant; it never
  overrides `Deny`, and Consequential actions always prompt (ADR-55 §3).
- **No auto-grant exists from any model output.** The 2026-09-06 auto-grant
  amendment is moot and removed (Current state). The only click-free grant is
  the exact-objective auto-Apply of a user-approved, hash-verified plan
  binding (ADR-55 §4) — the authorization is the binding, never a
  classification.
- **Threshold semantics, recorded for the record.** As accepted, a classifier
  suggestion at or above `classifier_confidence_threshold` re-routed the
  outcome (`RouterOutput.route = LlmClassifier`, confidence replaced — path
  selection only), below-threshold left the deterministic result standing,
  and `classifier_confidence_threshold` was validated
  `>= concerto_core::LOW_CONFIDENCE_THRESHOLD` (0.7) at config load so no
  `[threshold, 0.7)` band could exist. The 2026-09-11 clarification retired
  the surface: no threshold invariant survives. What survives from this
  section's spine is the invariant behind it — classification never grants —
  which §8 states in its current, stronger form.

### 5. Audit chain (recorded; the routed rows are gone)

The pre-replacement router row name was captured **before** the classifier
call, classifier rows kept `rule_matched = "llm_classifier"`,
`verdict = "n/a"`, the JSON envelope `{route, confidence, threshold,
rationale}`, and the shared correlation id (2c §5). Fail-soft rows carried
zero confidence and the literal envelope route `"ask_user"`. Operating state:
the routed `intent_router` grant rows are no longer written and the
`intent_classifier` rows cannot occur — the classifier does not run
(ADR-55 §6). The audit chain that remains is the action rows,
plan-decision rows (`record_plan_decision`, `intent:plan`), and coordinator
decision rows on one `correlation_id` lineage.

### 6. Prompt — utterance-only one-shot JSON classification (recorded)

The classifier prompt was an **utterance-only one-shot** classification: one
system instruction + the raw utterance, demanding a single JSON object
`{route, confidence, rationale}` with `route` ∈ the six-outcome set,
`confidence` 0..1, `rationale` ≤ 512 characters; temperature 0, bounded
output tokens, one bounded non-streaming call. Conservative-outcome guidance
("answer/review over execute when uncertain") was retained. Conversation
context for relative utterances ("apply that") stayed in the known-v2 list.
Recorded only — no operating prompt exists.

### 7. Cost — one bounded LLM call per non-fast-path message (recorded)

With the classifier enabled there was one bounded LLM call per non-fast-path
message, spend-tracked, counted against the session spend cap, gated by
reserve-before-call, fail-soft on cap-exceeded. Operating state: there are
**zero** classifier calls per run — this section's entire cost envelope
retired with the module.

### 8. Security posture — the model classifies, never authorizes

Authorization is exclusively **user-event-driven**; nothing that classifies —
the deterministic tier classifier or any model output — can ever produce a
grant:

- **The model classifies, never authorizes.** A misclassification can at
  worst shape a flavor hint or an offline evaluation; it can never produce an
  unconfirmed mutation, because no model output is on the grant path. This
  invariant is structural, not behavioral: grants enter only through
  user-confirmed decisions (`grant_execute` on Apply / picked Execute) or the
  user-approved, hash-verified plan binding (ADR-55 §4/§5).
- **The tool-level path is authoritative.** Deny-class `AutoDeny` danger
  patterns and `Deny`-is-final run first in the first-match-wins policy
  engine; `Condition::IntentAuthorized` only upgrades `RequireApproval`, never
  overrides `Deny`; Consequential actions always prompt; shell is
  project-bounded (`intent_authorized_shell`) or approval-gated —
  `shell_requires_approval` — and never grantable (ADR-55 §3).
- **No auto-grant derives from classification.** The 2026-09-06 auto-grant is
  removed; the exact-objective auto-Apply is bound to the artifact hash of a
  plan the user approved, consumed on execution, and loud-fails on drift —
  its authority is the user's approval, not a route or a confidence score
  (ADR-55 §4).
- **Read-only is mechanical.** Where a run is declared read-only (Replan,
  dismissal, a denial), `intent_readonly_deny` is a hard, pre-sink denial —
  not a prompt, not a re-route, not an upgrade (ADR-55 §2/§3).

### 9. Supersession scope — exactly two pins, nothing more

This ADR superseded exactly two ADR-55 Phase 2c pins, as recorded in ADR-55
§10: **Phase 2c §2** (`classifier_enabled` default false → true) and
**Phase 2c §3** (AskUser-only placement → model-first with two deterministic
fast paths). Nothing else in ADR-55 was contradicted, and the later unified-
loop decision (ADR-55 §1/§10; ADR-71) superseded this ADR's operating role,
not its pins or its never-grant invariant. Where this ADR is silent, ADR-55
governs.

## Consequences

- **Positive (as accepted).** The model-first experiment proved the market
  direction — the model reads the conversation — and its two lessons carried
  into the unified loop: request classification is not a trustworthy dispatch
  authority, and safety must live at the tool/permission level. The
  never-grant invariant (§8) is now *stronger* than the design that shipped
  it: no classifier path exists at all.
- **Costs / trade-offs.** The classifier's per-message LLM call is gone (no
  cost, but also no offline classifier-supported routing — the unified loop
  replaced both). The recorded design's validation machinery
  (threshold ≥ 0.7, reserve-before-call, envelope audit) is recorded history;
  re-introducing any classifier later must re-justify every one of those
  pieces against the loop, not just the dispatch hookup.
- **Risks.** The surviving risk is the same one this ADR always guarded: a
  future change that lets *any* classification output upgrade authorization.
  ADR-55 §3's injection-point rule and §8's structural invariant are the
  standing defenses; ADR-74's delegation doctrine must not be read as
  authorization-by-classification.

## Verification notes (current state)

- **No dispatch hookups:** `rg "intent_classifier|IntentClassifier"` over
  `crates/` matches only config-migration tests (v7→v8 drop) and a
  legacy-config load test; no module, no call site.
- **Config:** a schema-7 config with classifier keys loads at v8 unchanged
  (unknown-key tolerant) and the keys are ignored.
- **Never-grant:** `grant_execute` is reachable only from an Apply decision
  or a user-picked Execute; the exact-objective auto-Apply consumes a
  hash-verified binding; `Deny` and `intent_readonly_deny` stay final.
- **Consistency:** ADR-55 §10 records the classifier arc (proposal-only →
  2c → model-first → auto-grant experiment → unified-loop retirement →
  deletion) in one place; this ADR records the pins and the invariant.