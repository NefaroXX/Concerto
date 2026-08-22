# ADR-56: Model-first intent classification — the LLM decides intent; deterministic rules become fallbacks

**Status:** Accepted (2026-08-11) — supersedes two ADR-55 Phase 2c pins, in part:
    Phase 2c §2 (`classifier_enabled` default **false** → **true**) and Phase 2c
    §3 (AskUser-only classifier placement → model-first with two deterministic
    fast paths). Nothing else in ADR-55 is contradicted: the deterministic
    router's rule set and evaluation order, the policy gates, grants,
    confirmation dialogs, plan bindings, audit §5 correlation-id chain, and
    Phase 1e gate-always-on all remain in force.
**Date:** 2026-08-11
**Deciders:** Concerto architecture
**Supersedes:** ADR-55 Phase 2c §2 (default pin) and Phase 2c §3 (placement pin), in part.
**Composes with:** ADR-55 (deterministic router, policy gates, grants,
    confirmation dialogs, plan bindings, audit chain), ADR-26 (audit /
    correlation-id chain), ADR-52 (plan artifacts), ADR-44 (session-scoped
    `VirtualFs`), ADR-37 (capability lifecycle)

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

The structural point that settles the direction: **the LLM reads the
conversation; the keyword corpus cannot.** Market LLM coding tools (opencode,
Claude Code, and peers) do not use keyword intent classification at all — the
model reads the conversation and tool use is gated at the tool/permission
level, not by pre-classifying the message into a fixed outcome set. ADR-55
Phase 2c deliberately shipped the classifier as an off-by-default wrapper at
the AskUser sink only. This ADR supersedes exactly those two pins: the
classifier becomes the primary decider, and the deterministic rules become
fallbacks and safety nets.

## Decision

Model-first intent classification: when `[intent] classifier_enabled` is true,
the LLM classifier is the intent authority for every user message except two
deterministic fast paths. Deterministic routing remains the read-only safety
net and the offline fallback. Nine decisions:

### 1. Primary decider — the LLM classifies every message except two fast paths

When `[intent] classifier_enabled` is true, the LLM classifier becomes the
intent authority for **every** user message except two deterministic fast
paths, which run **before** the classifier:

- **(a) Negation-override** (read-only safety invariant): a user saying
  "don't touch", "do not …", "never …", "without changing …" must never be
  overridden by a model — even a mistaken one. The `NEGATION_PHRASES` corpus
  keeps its first-match-wins priority (`crates/core/src/intent.rs`), exactly as
  today.
- **(b) Smalltalk route** (zero-cost chat): pure greetings/pleasantries of at
  most `SMALLTALK_MAX_INPUT_LEN` (48) characters route to a read-only `Answer`
  so "hi" never costs an LLM call. Smalltalk continues to be length-bounded so
  a long message that merely opens with a greeting is not swallowed.

Every other message — including explicit keyword hits ("build", "plan",
"execute", …), question-detection results, and AskUser-remaining ambiguity —
is classified by the model when the classifier is enabled. The deterministic
rules no longer short-circuit the LLM; their internal evaluation order in
`route()` is otherwise unchanged.

This reverses the ADR-55 Phase 2c §1/§3 wrapper semantics: the classifier is no
longer mounted **only** at the AskUser sink; it is mounted **after the two fast
paths and before** any rule hit, question detection, or the AskUser sink.
`route()` itself stays pure and unchanged — the classifier remains a wrapper
around it (as in 2c §1/§4). The change is where the wrapper mounts, not the
router's internals.

### 2. Default — `classifier_enabled` flips to true

`classifier_enabled` defaults flip **false → true**. The 2c §2 "conservative;
an added model call" framing is superseded: the added call is now the intended
primary path, not an opt-in extra. `classifier_model` stays `None`-default —
it falls back to the run's chat model (2c §2, §9 unchanged). Spend cap /
reservation semantics are unchanged: reserve-before-call gates the call and
fail-soft on cap-exceeded (2c §6 unchanged).

### 3. Demoted fallbacks — the deterministic chain stands exactly as today

When the classifier is disabled, unavailable (no provider / no config),
cancelled, produces malformed output, or fails soft, the full deterministic
chain — **negation → question → explicit keywords → smalltalk → AskUser** — in
`route()`'s documented order stands exactly as today. Offline behavior is the
fallback, never the primary: with the classifier off, a run behaves
byte-for-byte like an ADR-55 deterministic-router-only run (an ambiguous
request lands on the AskUser sink unchanged). There is no behavioral regression
when the classifier is off or unreachable.

### 4. Confidence semantics — reroute at threshold, never grant

- A classifier suggestion at or above `classifier_confidence_threshold`
  re-routes the outcome to the suggested route: `RouterOutput.route =
  LlmClassifier`, confidence replaced — path selection only, as 2c §3.
- Below-threshold → the deterministic routing result stands (which may be
  Execute → the confirmation dialog, or AskUser → the modal).
- **The classifier never grants:** a re-routed Execute still passes through
  the confirmation dialog and grants machinery (arm-1 gate; the 2c §4
  never-grant invariant is unchanged).
- `classifier_confidence_threshold` stays validated
  `>= concerto_core::LOW_CONFIDENCE_THRESHOLD` (0.7) at config load — no
  configured threshold can create a `[threshold, LOW_CONFIDENCE_THRESHOLD)`
  band (2c §2 invariant retained).

### 5. Audit chain unchanged

- The pre-replacement router row name is captured **before** the classifier
  call (already implemented), so the trail always records the deterministic
  outcome the classifier was asked about.
- Classifier rows keep `rule_matched = "llm_classifier"`,
  `verdict = "n/a"`, the JSON envelope `{route, confidence, threshold,
  rationale}`, and share the router-decision row's correlation id (2c §5
  unchanged).
- Fail-soft rows (malformed, provider-error, cancelled, cap-exceeded)
  carry zero confidence and the literal envelope route `"ask_user"` as a
  schema-stable "no classification to report" marker; the actual
  pre-replacement route name is recorded on the caller's router-decision
  row, never fabricated here. Below-threshold is **not** fail-soft: it is a
  real classification (envelope carries the classified route + confidence)
  that the caller declines to re-route.

### 6. Prompt — utterance-only one-shot JSON classification

The classifier prompt stays an **utterance-only one-shot** classification:
one system instruction + the raw utterance, demanding a single JSON object
`{route, confidence, rationale}` with `route` ∈ the six-outcome set
(Answer / Diagnose / Review / Plan / Execute / Verify), `confidence` 0..1, and
`rationale` ≤ 512 characters. Temperature 0, bounded output tokens, one
bounded non-streaming call (2c §3 unchanged). Conservative-outcome guidance
("answer/review over execute when uncertain") is retained. **Future option,
noted not decided:** adding conversation context for relative-utterance
resolution ("apply that") stays in the known-v2 list (1d §6, 2c §8).

### 7. Cost — one bounded LLM call per non-fast-path message

With the classifier enabled there is one bounded LLM call per non-fast-path
message. It is spend-tracked on the same channel as any model call, counted
against the session spend cap, and gated by **reserve-before-call** — a
cap-exceeded reservation means the call never happens, fail-soft (2c §6
unchanged). Missing provider/config → classifier disabled with a debug log.

### 8. Security posture — the model classifies, never authorizes

Authorization is unchanged and remains exclusively user-event-driven:
confirmation dialog for Execute + `IntentGrantStore` grants + `SessionIntentAuth`
read-only + `SimplePolicyEngine` / `VirtualFs` tool gates. **The model
classifies, never authorizes.** A model misclassification can at worst produce
an unwanted confirmation dialog or a conservative outcome — it can never
produce an unconfirmed mutation. The negation fast path (§1a) guarantees
read-only stays read-only even against a permissive model, so the read-only
invariant rests on the `NEGATION_PHRASES` corpus, not on model behavior.
`AutoDeny` danger patterns, `Deny`-is-final, and the first-match-wins engine
order are untouched (ADR-55 §Decision 2).

### 9. Supersession scope — exactly two pins, nothing more

This ADR supersedes exactly two ADR-55 Phase 2c pins:

- **Phase 2c §2:** `classifier_enabled` default **false → true**.
- **Phase 2c §3:** AskUser-only placement → **model-first with two
  deterministic fast paths**.

Nothing else in ADR-55 is contradicted: the deterministic router's corpora and
evaluation order, the policy gates (`Condition::IntentAuthorized`), grants
(§Decision 4), confirmation dialogs (§Decision 3 / 1d), plan bindings
(1d / 2b), audit §5 correlation-id chain, and Phase 1e gate-always-on all
remain in force. Where this ADR is silent, ADR-55 governs.

## Consequences

- **Positive.** Chat works like market tools: plain utterances ("hi, lets work
  on something", "i was planning my vacation", "we should fix the website")
  route sensibly instead of falling into the AskUser modal or being hijacked
  by keyword corpora. No corpus treadmill for the infinite ambiguity of chat —
  the model reads the conversation. Deterministic behavior is preserved
  offline: with the classifier off or fail-soft, today's chain stands
  unchanged. Gate and security are unaffected: the classifier never grants,
  and the negation fast path keeps read-only a hard invariant.
- **Costs / trade-offs.** One bounded LLM call per non-fast-path message when
  the feature is enabled (spend-tracked, reserve-before-call, against the
  session cap). Smalltalk and negation exceptions exist so zero-cost chat and
  read-only safety never depend on a model call. Classifier quality depends on
  the configured model; a wrong read misroutes — bounded to an unwanted
  confirmation dialog or a conservative outcome. Keyword corpora retain only
  fallback / deny-rule value when the classifier is enabled; their role becomes
  the read-only safety net and the offline fallback.
- **Risks.** The model-first path adds model spend surface to the default
  configuration; the spend cap and reserve-before-call semantics bound it. The
  two fast paths are load-bearing (read-only invariant and zero-cost chat) and
  must run before the classifier; reordering them would regress either.

## Verification notes

- Fast paths beat the classifier: a negation-override input and a
  ≤48-char smalltalk input never reach the provider even with the classifier
  enabled.
- Every other message with the classifier enabled reaches the classifier —
  keyword hits, questions, and AskUser alike.
- Default flip: a config without `[intent]` now yields `classifier_enabled =
  true`; `classifier_model` still defaults to the run's model.
- Fail-soft chain: disabled / malformed / provider-error / cancelled /
  cap-exceeded all leave the deterministic result standing, byte-identical to
  today's behavior.
- Never-grant: a re-routed Execute still hits the arm-1 confirmation dialog;
  `AutoDeny`/`Deny`-is-final untouched.
- Audit: two rows per classifier-eligible event — the router row keeps the
  pre-replacement route name, the classifier row keeps
  `rule_matched = "llm_classifier"` with the shared correlation id.