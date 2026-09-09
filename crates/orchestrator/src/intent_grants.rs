//! Session-scoped, non-durable intent grants (ADR-55 §4, Phase 2d §1).
//!
//! ADR-55 Phase 2d: routing **is** the decision. A high-confidence route to
//! one of the five action-grantable outcomes auto-grants the same
//! `filesystem`/`git` scopes a confirmed `Apply` holds today — no
//! [`ApprovalSink`] call, no dialog, no modal (2d §1). The hard read-only
//! invariants are unchanged (2d §2): the negation-override rule never grants,
//! and a zero-confidence `AskUser` route never grants — with the classifier
//! off it still opens the AskUser modal (ADR-56 §3, byte-identical offline
//! chain), with a real classification it lands as a read-only answer-only run.
//!
//! Grants are created fresh per `run_shared_agent` call, so they are bound to
//! a single run/session by construction and re-apply automatically through
//! routing on every resume (non-durable — nothing is persisted; 2d §4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use concerto_core::intent::{RequestedOutcome, RouterOutput, RouterRoute};
use concerto_core::traits::approval::ApprovalSink;
use concerto_core::types::PolicyAction;
use concerto_core::{CancellationToken, IntentAuthorization, LOW_CONFIDENCE_THRESHOLD};

/// One granted tool-class scope, bound to the confirmed requested outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEntry {
    /// The confirmed requested outcome that produced this grant.
    pub intent: RequestedOutcome,
    /// The tool class covered (`"filesystem"` | `"git"`). Shell is never
    /// grantable (ADR-55 §2 shell scope hole), and Consequential actions are
    /// decided before the grant is ever consulted.
    pub scope: &'static str,
}

/// Run-scoped grant store.
///
/// Created fresh per run: grants are per-plan, session-scoped, non-durable,
/// and revoked by `Stop` / a changed objective (ADR-55 §4) — all of which is
/// achieved structurally here because the store is dropped when the run ends
/// and never persisted.
#[derive(Debug, Default)]
pub struct IntentGrantStore {
    grants: Mutex<Vec<GrantEntry>>,
}

impl IntentGrantStore {
    /// Create an empty, run-scoped store. Grants enter only via [`Self::grant`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a grant covering `scope` for the confirmed `intent`.
    ///
    /// Callers are the run loop, after either a high-confidence route
    /// (auto-grant, ADR-55 Phase 2d §1) or an explicit user decision in the
    /// AskUser modal (the classifier-off chain, ADR-56 §3) — never from
    /// routing or classification alone below the threshold.
    pub fn grant(&self, intent: RequestedOutcome, scope: &'static str) {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).push(GrantEntry { intent, scope });
    }

    /// True while an active grant covers `scope`.
    pub fn covers(&self, scope: &str) -> bool {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|grant| grant.scope == scope)
    }

    /// True when no grant is active.
    pub fn is_empty(&self) -> bool {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Revoke every grant (semantics of `Stop` / changed objective). The store
    /// is dropped at run end anyway; this makes the revocation explicit.
    pub fn revoke_all(&self) {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Authorization state source for one run (ADR-55 §2/§4).
///
/// `is_read_only_intent` starts `true`: a run stays read-only until routing
/// auto-grants (ADR-55 Phase 2d §1) or the user confirms a mutating intent
/// through the AskUser modal (the classifier-off chain). Only the run loop
/// changes it.
pub struct SessionIntentAuth {
    store: Arc<IntentGrantStore>,
    read_only: AtomicBool,
}

impl SessionIntentAuth {
    /// Wrap a run-scoped store. The run starts read-only until confirmed.
    pub fn new(store: Arc<IntentGrantStore>) -> Self {
        Self { store, read_only: AtomicBool::new(true) }
    }

    /// Set whether the current run is a read-only-intent run. Called once per
    /// run from the routing step, before any tool executes.
    pub fn set_read_only(&self, read_only: bool) {
        self.read_only.store(read_only, Ordering::Relaxed);
    }

    /// Current read-only-intent flag of this run.
    pub fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    /// The run-scoped store this provider consults.
    pub fn store(&self) -> Arc<IntentGrantStore> {
        self.store.clone()
    }
}

impl IntentAuthorization for SessionIntentAuth {
    fn is_read_only_intent(&self) -> bool {
        self.is_read_only()
    }

    fn grant_covers(&self, action: &PolicyAction<'_>) -> bool {
        self.store.covers(action.tool_name)
    }
}

/// The audit `rule_matched` value for a routing path (ADR-55 §5.2).
///
/// `RuleHit` carries the deterministic corpus name (`execute_keyword`, ...);
/// the Phase-1 classifier and the ask path use stable synthetic names that the
/// executor's `record_routing_decision` doc already reserves.
pub fn router_route_name(route: &RouterRoute) -> &'static str {
    match route {
        RouterRoute::RuleHit { rule } => rule,
        RouterRoute::LlmClassifier => "llm_classifier",
        RouterRoute::AskUser => "ask_user",
        _ => "unknown",
    }
}

/// The routing **path kind** for audit envelopes (ADR-55 Phase 2d §5/A2):
/// the [`RouterRoute`] variant name, distinguishing a deterministic corpus
/// hit from the LLM classifier path (`RuleHit` | `LlmClassifier`).
pub fn router_path_name(route: &RouterRoute) -> &'static str {
    match route {
        RouterRoute::RuleHit { .. } => "RuleHit",
        RouterRoute::LlmClassifier => "LlmClassifier",
        RouterRoute::AskUser => "AskUser",
        _ => "Unknown",
    }
}

/// The audit `user_response` value for a requested outcome (ADR-55 §5.2).
pub fn outcome_name(outcome: RequestedOutcome) -> &'static str {
    match outcome {
        RequestedOutcome::Answer => "Answer",
        RequestedOutcome::Diagnose => "Diagnose",
        RequestedOutcome::Review => "Review",
        RequestedOutcome::Plan => "Plan",
        RequestedOutcome::Execute => "Execute",
        RequestedOutcome::Verify => "Verify",
        _ => "Unknown",
    }
}

/// Grant the two in-scope, mutate-local tool classes for a **confirmed**
/// Execute: filesystem local mutations and git local mutations (ADR-55 §2).
///
/// Shared by the paths that carry a confirmed Execute decision: an `Apply`
/// decision on a stored plan binding (see
/// `crate::plan_approval::apply_plan_decision`) and a picked `Execute` in the
/// AskUser modal. Mutate-local, in-scope grants never cover shell (never
/// grantable) and never cover Consequential/destructive actions (decided
/// before the grant is consulted), so this helper can never widen them.
pub fn grant_execute(store: &IntentGrantStore) {
    store.grant(RequestedOutcome::Execute, "filesystem");
    store.grant(RequestedOutcome::Execute, "git");
}

/// Grant the two in-scope, mutate-local tool classes for `intent`
/// (ADR-55 Phase 2d §1): the same `filesystem`/`git` scopes a confirmed
/// `Apply` holds, bound to the routed outcome instead of `Execute`.
///
/// Only the auto-grant route ([`is_auto_grant_route`]) and the AskUser-modal
/// grant path reach this; shell stays never-grantable and Consequential
/// actions are decided before the grant is ever consulted (ADR-55 §Decision 2
/// capability tiers — the auto-grant only upgrades `RequireApproval`, never
/// overrides `Deny`).
pub fn grant_outcome(store: &IntentGrantStore, intent: RequestedOutcome) {
    store.grant(intent, "filesystem");
    store.grant(intent, "git");
}

/// The five action-grantable outcomes (ADR-55 Phase 2d §1).
fn is_action_grantable_outcome(outcome: RequestedOutcome) -> bool {
    matches!(
        outcome,
        RequestedOutcome::Execute
            | RequestedOutcome::Plan
            | RequestedOutcome::Verify
            | RequestedOutcome::Review
            | RequestedOutcome::Diagnose
    )
}

/// ADR-55 Phase 2d §1: does `routing` decide the run's authority on its own?
///
/// True exactly when the outcome is one of the five action-grantable
/// outcomes, the confidence is at or above [`LOW_CONFIDENCE_THRESHOLD`], and
/// the route is a deterministic rule hit **other than the negation override**
/// or the LLM classifier path. The negation corpus keeps first-match-wins
/// priority ahead of any model output (ADR-56 §1a), so a permissive model can
/// never make a read-only request writable — a `negation_override` hit never
/// grants even though it carries high confidence. `AskUser` (confidence
/// `0.0`) never qualifies (2d §2 hard read-only), and neither does
/// smalltalk (routes to `Answer`, outside the grantable set).
pub fn is_auto_grant_route(routing: &RouterOutput) -> bool {
    if routing.confidence < LOW_CONFIDENCE_THRESHOLD
        || !is_action_grantable_outcome(routing.outcome)
    {
        return false;
    }
    match routing.route {
        RouterRoute::RuleHit { rule } => rule != "negation_override",
        RouterRoute::LlmClassifier => true,
        _ => false,
    }
}

/// ADR-55 Phase 2d §1/§3: is `routing` a confident Execute that auto-grants
/// (and may therefore auto-Apply a stored plan binding)?
pub fn is_confident_auto_grant_execute(routing: &RouterOutput) -> bool {
    is_auto_grant_route(routing) && routing.outcome == RequestedOutcome::Execute
}

/// The `{rule, confidence, route, outcome}` envelope of an auto-granted
/// routing audit row (ADR-55 Phase 2d §5/A2): `rule`/`route` name the path
/// that granted (`router_route_name` / [`router_path_name`]), `confidence`
/// is the decision-time confidence, and `outcome` the effective outcome.
pub fn auto_grant_envelope(routing: &RouterOutput) -> String {
    serde_json::json!({
        "rule": router_route_name(&routing.route),
        "route": router_path_name(&routing.route),
        "outcome": outcome_name(routing.outcome),
        "confidence": routing.confidence,
    })
    .to_string()
}

/// ADR-55 Phase 2e §2: the permission envelope a routed run executes under.
///
/// The router keeps only the safety job: it decides whether the run may act,
/// never which code path runs. Every non-empty run enters the unified agent
/// loop; the envelope is what the loop's prompt and the policy engine key
/// off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnvelope {
    /// Hard read-only: no grants exist, the policy engine denies every
    /// mutation, and the loop's prompt does not ask the model to act.
    /// Reached by a task-level prohibition (`negation_override`), an
    /// unresolved `AskUser` ambiguity, or a gate denial
    /// (`declined`/`dismissed`).
    ReadOnly,
    /// Acting: the confirmed grants hold exactly as the gate granted them
    /// (`auto_granted`, or a user-granted Execute). Writes stay governed by
    /// the policy engine, never by a branch.
    Acting,
}

impl RunEnvelope {
    /// Derive the envelope from the intent gate's confirmation value
    /// (`auto_granted` | `granted` | `declined` | `dismissed` | `n/a`).
    ///
    /// A `negation_override` route and an unresolved `AskUser` route never
    /// produce a granting confirmation ([`apply_intent_gate`] audits them
    /// `"n/a"`), so they land [`RunEnvelope::ReadOnly`] here by construction
    /// — the hard read-only invariant (2e §2) does not need its own clause.
    pub fn from_confirmation(confirmation: &str) -> Self {
        if matches!(confirmation, "granted" | "auto_granted") {
            Self::Acting
        } else {
            Self::ReadOnly
        }
    }

    /// True when the run may act within its grants.
    pub fn is_acting(self) -> bool {
        self == Self::Acting
    }
}

/// ADR-55 Phase 2e §2: the non-binding flavor hint for a routed outcome.
///
/// One system-prompt line rendered from the outcome the router settled on.
/// Pure: the helper maps an outcome to text and nothing else. Prompt-building
/// callers append it, log it alongside the routing row, and NEVER branch on
/// it — no keyword may select a tool-less path, and the unified loop decides
/// tool use from the model's own output.
pub fn flavor_hint(outcome: RequestedOutcome) -> &'static str {
    match outcome {
        RequestedOutcome::Execute => "the user asked for a change; implement it",
        RequestedOutcome::Plan => "the user seems to want a plan; design before changing",
        RequestedOutcome::Verify => {
            "the user seems to want verification; prefer checking over changing"
        }
        RequestedOutcome::Review => "the user seems to want a review; critique rather than modify",
        RequestedOutcome::Diagnose => {
            "the user seems to want a diagnosis; investigate and explain before changing"
        }
        RequestedOutcome::Answer => "the user seems to want an answer",
        _ => "",
    }
}

/// Apply the ADR-55 gate (Phase 2d) to a routed request and return the run's
/// EFFECTIVE outcome plus the audit confirmation value (`auto_granted` |
/// `granted` | `declined` | `dismissed` | `n/a`).
///
/// - **Auto-grant (2d §1):** a route that satisfies [`is_auto_grant_route`]
///   (one of the five action-grantable outcomes at `>=`
///   [`LOW_CONFIDENCE_THRESHOLD`] via a non-negation rule hit or the LLM
///   classifier) grants `filesystem`+`git` outright and is audited as
///   `"auto_granted"` — no [`ApprovalSink`] call, no dialog, no modal. The
///   negation-override rule and `AskUser` never reach this arm (2d §2 hard
///   read-only).
/// - **AskUser modal (ADR-56 §3, byte-identical offline chain):** when
///   `allow_ask_user_modal` is true — the classifier disabled, unavailable,
///   or fail-soft (no real classification) — the ambiguous input still opens
///   the six-outcome modal: a picked `Execute` grants (`"granted"`), any
///   other pick re-routes read-only (`"declined"`), a dismissed dialog
///   degrades to `Answer` read-only (`"dismissed"`).
/// - **AskUser, classified (2d §2):** when a real classification happened and
///   the caller declined to re-route (below threshold), the zero-confidence
///   input lands as a read-only answer-only run (`"n/a"`, no prompt): the
///   user escalates by rephrasing with clearer intent, never by clicking.
/// - Deterministic read-only outcomes never prompt and are audited as
///   `"n/a"`.
///
/// Capability tiers are untouched (ADR-55 §Decision 2): the grant only
/// upgrades `RequireApproval`, never overrides `Deny`, and never covers
/// Consequential actions. A read-only run hard-denies all mutation —
/// filesystem, shell, and git — even under session auto-approve (B-1).
pub async fn apply_intent_gate(
    routing: &RouterOutput,
    approval_sink: &dyn ApprovalSink,
    store: &IntentGrantStore,
    auth: &SessionIntentAuth,
    cancel: CancellationToken,
    allow_ask_user_modal: bool,
) -> (RequestedOutcome, &'static str) {
    // ADR-55 Phase 2d §1: routing is the decision — grant instead of prompt.
    if is_auto_grant_route(routing) {
        grant_outcome(store, routing.outcome);
        auth.set_read_only(false);
        return (routing.outcome, "auto_granted");
    }
    let (effective, confirmation) = match routing.outcome {
        // AskUser route: the deterministic router found nothing conclusive.
        // The modal survives only where the deterministic chain stands alone
        // (classifier off/unreachable/fail-soft — ADR-56 §3); a real
        // classification that was not re-routed lands read-only answer-only.
        RequestedOutcome::Answer
            if routing.route == RouterRoute::AskUser && allow_ask_user_modal =>
        {
            match approval_sink
                .request_intent_confirmation(
                    "I could not confidently tell what you want. Pick the intent for this run \
                     (read-only choices stay read-only):"
                        .to_string(),
                    &[
                        RequestedOutcome::Answer,
                        RequestedOutcome::Diagnose,
                        RequestedOutcome::Review,
                        RequestedOutcome::Plan,
                        RequestedOutcome::Execute,
                        RequestedOutcome::Verify,
                    ],
                    cancel.clone(),
                )
                .await
            {
                Some(RequestedOutcome::Execute) => (RequestedOutcome::Execute, "granted"),
                Some(other) => (other, "declined"),
                // Dismissed (prompt shown, no answer): audited as "dismissed"
                // and degraded to a safe read-only Answer.
                None => (RequestedOutcome::Answer, "dismissed"),
            }
        }
        outcome => (outcome, "n/a"),
    };

    let granted = matches!((effective, confirmation), (RequestedOutcome::Execute, "granted"));
    auth.set_read_only(!granted);
    if granted {
        grant_execute(store);
    }
    (effective, confirmation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::CapabilitySet;
    use concerto_core::{
        IntentVerdict, RULE_CONSEQUENTIAL, RULE_INTENT_AUTHORIZED, RULE_INTENT_READONLY_DENY,
        RULE_OBSERVE, RULE_SHELL_REQUIRES_APPROVAL, RULE_UN_GRANTED,
    };
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
        PolicyAction {
            tool_name,
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        }
    }

    #[test]
    fn grant_store_covers_and_revokes() {
        let store = IntentGrantStore::new();
        assert!(store.is_empty());

        store.grant(RequestedOutcome::Execute, "filesystem");
        assert!(store.covers("filesystem"));
        assert!(!store.covers("git"), "a filesystem grant never covers git");
        assert!(!store.is_empty());

        store.revoke_all();
        assert!(store.is_empty());
        assert!(!store.covers("filesystem"), "revoked grants never cover");
    }

    #[test]
    fn session_starts_read_only_and_denies_writes() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());

        assert!(auth.is_read_only(), "a run starts read-only by default");
        // `write` is a MutateLocal filesystem op (not in the destructive/consequential set).
        let input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let write = action("filesystem", &input);
        assert_eq!(
            auth.verdict(&write),
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
            "a filesystem mutation in a read-only run is a final pre-sink denial"
        );

        // A read still flows (Observe -> Allow) even in a read-only run.
        let read_input = serde_json::json!({"operation": "read", "path": "src/main.rs"});
        let read = action("filesystem", &read_input);
        assert_eq!(auth.verdict(&read), IntentVerdict::Allow { rule: RULE_OBSERVE });
    }

    #[test]
    fn confirmed_execute_grants_fs_and_git_mutations() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false);

        // `write` is a MutateLocal filesystem op (not in the destructive/consequential set).
        let input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let write = action("filesystem", &input);
        // No grant yet: MutateLocal on a grantable class -> un_granted prompt.
        assert_eq!(auth.verdict(&write), IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED });

        store.grant(RequestedOutcome::Execute, "filesystem");
        assert_eq!(
            auth.verdict(&write),
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
            "an in-scope grant upgrades RequireApproval -> Allow for filesystem"
        );

        // Git local mutations need their own grant.
        let commit_input = serde_json::json!({"operation": "commit"});
        let commit = action("git", &commit_input);
        assert_eq!(
            auth.verdict(&commit),
            IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED },
            "a filesystem grant never covers git"
        );
        store.grant(RequestedOutcome::Execute, "git");
        assert_eq!(
            auth.verdict(&commit),
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
            "an in-scope grant upgrades RequireApproval -> Allow for git local mutation"
        );
    }

    #[test]
    fn shell_mutation_is_never_grantable() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false);
        store.grant(RequestedOutcome::Execute, "shell");

        let input = serde_json::json!({"cmd": "rm -rf target"});
        let shell = action("shell", &input);
        assert_eq!(
            auth.verdict(&shell),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "shell mutations stay under approval even with a (never-created) shell grant"
        );
    }

    #[test]
    fn consequential_actions_are_never_covered_by_grants() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false);
        store.grant(RequestedOutcome::Execute, "git");

        let push_input = serde_json::json!({"operation": "push"});
        let push = action("git", &push_input);
        assert_eq!(
            auth.verdict(&push),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "blanket grants never cover Consequential egress"
        );

        let delete_input = serde_json::json!({"operation": "delete", "path": "src/main.rs"});
        let delete = action("filesystem", &delete_input);
        assert_eq!(
            auth.verdict(&delete),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "destructive filesystem ops are Consequential and never auto-covered"
        );
    }

    #[test]
    fn audit_name_mappings_are_stable() {
        assert_eq!(router_route_name(&RouterRoute::AskUser), "ask_user");
        assert_eq!(router_route_name(&RouterRoute::LlmClassifier), "llm_classifier");
        assert_eq!(
            router_route_name(&RouterRoute::RuleHit { rule: "execute_keyword" }),
            "execute_keyword"
        );
        assert_eq!(router_path_name(&RouterRoute::AskUser), "AskUser");
        assert_eq!(router_path_name(&RouterRoute::LlmClassifier), "LlmClassifier");
        assert_eq!(router_path_name(&RouterRoute::RuleHit { rule: "execute_keyword" }), "RuleHit");
        assert_eq!(outcome_name(RequestedOutcome::Execute), "Execute");
        assert_eq!(outcome_name(RequestedOutcome::Plan), "Plan");
        assert_eq!(outcome_name(RequestedOutcome::Verify), "Verify");
    }

    // ------------------------------------------------------------------
    // apply_intent_gate (ADR-55 Phase 2d §1/§2/§4; ADR-56 §3)
    // ------------------------------------------------------------------

    /// Approval sink stub that records how many times the intent-confirmation
    /// surface was offered, what option sets were offered, and what the
    /// "user" chose.
    struct StubIntentSink {
        confirmed: Mutex<Option<RequestedOutcome>>,
        calls: AtomicUsize,
        prompts: Mutex<Vec<Vec<RequestedOutcome>>>,
    }

    impl StubIntentSink {
        fn new(confirmed: Option<RequestedOutcome>) -> Self {
            Self {
                confirmed: Mutex::new(confirmed),
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ApprovalSink for StubIntentSink {
        async fn request_approval(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> concerto_core::ApprovalDecision {
            concerto_core::ApprovalDecision::Deny
        }
        async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}
        async fn request_ack(&self, _message: &str, _cancel: CancellationToken) -> bool {
            true
        }
        async fn request_intent_confirmation(
            &self,
            _question: String,
            options: &[RequestedOutcome],
            _cancel: CancellationToken,
        ) -> Option<RequestedOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().unwrap_or_else(|e| e.into_inner()).push(options.to_vec());
            *self.confirmed.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    fn project_dir() -> PathBuf {
        std::env::temp_dir().join("concerto-intent-grants-test")
    }

    /// A2 (ADR-55 Phase 2d §6): a `build <objective>` request routes Execute
    /// at confidence >= 0.7 via `RuleHit` → the run loop AUTO-GRANTS
    /// `filesystem`+`git` — no approval sink call, no dialog, no modal.
    #[tokio::test]
    async fn a2_confident_execute_auto_grants_without_dialog() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Plan));

        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        assert_eq!(routing.outcome, RequestedOutcome::Execute, "premise: Execute route");
        assert!(
            routing.confidence >= LOW_CONFIDENCE_THRESHOLD,
            "premise: confidence at or above the threshold"
        );
        assert!(is_auto_grant_route(&routing), "premise: routing decides on its own");

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), false)
                .await;

        assert_eq!(effective, RequestedOutcome::Execute, "the routed Execute stands");
        assert_eq!(confirmation, "auto_granted", "the auto decision is audited as auto_granted");
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            0,
            "no approval sink call, no modal, no click (2d §1)"
        );
        assert!(!auth.is_read_only(), "the auto-granted run is mutation-capable");
        assert!(store.covers("filesystem"), "fs mutations are in scope");
        assert!(store.covers("git"), "git local mutations are in scope");
    }

    /// A2 (audit half): the auto-grant envelope carries the
    /// `{rule, confidence, route, outcome}` story the ADR names.
    #[test]
    fn a2_auto_grant_envelope_names_rule_route_and_confidence() {
        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        let envelope: serde_json::Value =
            serde_json::from_str(&auto_grant_envelope(&routing)).expect("valid envelope JSON");
        assert_eq!(envelope["rule"], "execute_keyword");
        assert_eq!(envelope["route"], "RuleHit");
        assert_eq!(envelope["outcome"], "Execute");
        assert!(
            envelope["confidence"].as_f64().unwrap_or(0.0) >= f64::from(LOW_CONFIDENCE_THRESHOLD),
            "the envelope carries the decision-time confidence"
        );
    }

    /// A3 (ADR-55 Phase 2d §2): `don't build <objective>` hits the negation
    /// corpus first-match-wins → hard read-only: zero grants, no prompt, no
    /// spend, even though the negation route carries confidence 0.9.
    #[tokio::test]
    async fn a3_negation_override_never_grants_or_prompts() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("don't build accord", project_dir());
        assert!(
            matches!(routing.route, RouterRoute::RuleHit { rule: "negation_override" }),
            "premise: the negation corpus wins over the execute keyword"
        );
        assert!(
            !is_auto_grant_route(&routing),
            "a negation-override hit never auto-grants, whatever its confidence"
        );

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), true).await;

        assert_eq!(effective, RequestedOutcome::Answer);
        assert_eq!(confirmation, "n/a", "no prompt was shown");
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            0,
            "the negation path never opens any dialog"
        );
        assert!(auth.is_read_only(), "the run stays hard read-only");
        assert!(store.is_empty(), "zero grants");
    }

    /// A4 (ADR-55 Phase 2d §2): `hmm` routes `AskUser` at confidence 0.0.
    /// With a real classification behind it (no modal allowed), the
    /// zero-confidence input lands as a read-only answer-only run: zero
    /// grants, zero writes, no click — the user escalates by rephrasing.
    #[tokio::test]
    async fn a4_ask_user_zero_confidence_lands_read_only_answer_only() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("hmm", project_dir());
        assert_eq!(routing.route, RouterRoute::AskUser, "premise: ambiguous input");
        assert_eq!(routing.outcome, RequestedOutcome::Answer);
        assert_eq!(routing.confidence, 0.0, "premise: zero confidence");
        assert!(!is_auto_grant_route(&routing), "AskUser never auto-grants (2d §2)");

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), false)
                .await;

        assert_eq!(effective, RequestedOutcome::Answer, "read-only answer-only run");
        assert_eq!(confirmation, "n/a", "no prompt, no dismissal — just read-only");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "no modal at the zero-confidence path");
        assert!(auth.is_read_only());
        assert!(store.is_empty(), "zero grants");
    }

    /// A2-family coverage: all five action-grantable outcomes auto-grant
    /// fs+git from a confident rule hit — Plan, Verify, Review, Diagnose —
    /// each without any sink call (2d §1).
    #[tokio::test]
    async fn a2_all_five_outcomes_auto_grant_at_confidence() {
        for (input, expected) in [
            ("plan the refactor", RequestedOutcome::Plan),
            ("run the tests", RequestedOutcome::Verify),
            ("review the parser code", RequestedOutcome::Review),
            ("diagnose the failing build", RequestedOutcome::Diagnose),
        ] {
            let store = Arc::new(IntentGrantStore::new());
            let auth = SessionIntentAuth::new(store.clone());
            let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

            let routing = concerto_core::intent::route(input, project_dir());
            assert_eq!(routing.outcome, expected, "premise for {input:?}");
            assert!(
                routing.confidence >= LOW_CONFIDENCE_THRESHOLD,
                "premise confidence for {input:?}"
            );

            let (effective, confirmation) =
                apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), false)
                    .await;
            assert_eq!(effective, expected);
            assert_eq!(confirmation, "auto_granted", "{input:?} auto-grants (2d §1)");
            assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "{input:?} never prompts");
            assert!(!auth.is_read_only(), "{input:?} run is mutation-capable");
            assert!(store.covers("filesystem") && store.covers("git"));
        }
    }

    /// ADR-56 §4 flip (ADR-55 Phase 2d §1): a classifier-rerouted Execute at
    /// or above the threshold auto-grants exactly like a rule-hit Execute —
    /// the confirmation dialog is gone from the re-routed path too.
    #[tokio::test]
    async fn llm_classifier_reroute_auto_grants_at_threshold() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Plan));

        // Reconstruct the post-classifier routing state the wrapper produces
        // when it re-routes an AskUser ambiguity into a confident Execute.
        let mut routing = concerto_core::intent::route("hmm", project_dir());
        routing.outcome = RequestedOutcome::Execute;
        routing.confidence = 0.92;
        routing.route = RouterRoute::LlmClassifier;
        assert!(is_auto_grant_route(&routing), "the LlmClassifier path grants (2d §1)");

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), false)
                .await;

        assert_eq!(effective, RequestedOutcome::Execute);
        assert_eq!(confirmation, "auto_granted");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "no dialog on the classifier path");
        assert!(!auth.is_read_only());
        assert!(store.covers("filesystem") && store.covers("git"));
    }

    /// A small-talk-routed greeting ("hi, lets work on something") yields a
    /// deterministic read-only `Answer` via `RuleHit { rule: "smalltalk" }`,
    /// NOT `RouterRoute::AskUser` — so `apply_intent_gate` takes the
    /// `outcome => (outcome, "n/a")` arm and never calls
    /// `request_intent_confirmation`. Smalltalk is outside the grantable set.
    #[tokio::test]
    async fn gate_never_prompts_for_smalltalk_greetings() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("hi, lets work on something", project_dir());
        assert_eq!(routing.outcome, RequestedOutcome::Answer);
        assert!(matches!(routing.route, RouterRoute::RuleHit { rule: "smalltalk" }));

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), true).await;

        assert_eq!(effective, RequestedOutcome::Answer);
        assert_eq!(confirmation, "n/a");
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            0,
            "smalltalk routes read-only Answer and never opens the confirmation dialog"
        );
        assert!(auth.is_read_only());
        assert!(store.is_empty());
    }

    /// Offline/classifier-disabled chain (ADR-56 §3): with the modal allowed,
    /// an ambiguous input still opens the six-outcome AskUser modal — picked
    /// Execute grants, a picked read-only outcome re-routes, a dismissed
    /// dialog degrades to a read-only Answer. Byte-identical to the
    /// pre-2d behavior.
    #[tokio::test]
    async fn gate_ask_user_prompts_with_all_outcomes() {
        let project = project_dir();

        // Picking Execute on an ambiguous request grants (explicit user
        // authorization in the classifier-off chain).
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("zzzzzzzzzz", project.clone());
        assert_eq!(routing.route, RouterRoute::AskUser);
        assert_eq!(routing.outcome, RequestedOutcome::Answer);

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), true).await;

        assert_eq!(effective, RequestedOutcome::Execute);
        assert_eq!(confirmation, "granted");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "AskUser prompts exactly once");
        assert_eq!(
            sink.prompts.lock().unwrap_or_else(|e| e.into_inner())[0],
            vec![
                RequestedOutcome::Answer,
                RequestedOutcome::Diagnose,
                RequestedOutcome::Review,
                RequestedOutcome::Plan,
                RequestedOutcome::Execute,
                RequestedOutcome::Verify,
            ],
            "ambiguous input offers all six outcomes"
        );
        assert!(!auth.is_read_only());
        assert!(store.covers("filesystem") && store.covers("git"));

        // Picking a read-only outcome re-routes and never grants.
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Diagnose));
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), true).await;
        assert_eq!(effective, RequestedOutcome::Diagnose);
        assert_eq!(confirmation, "declined");
        assert!(auth.is_read_only());
        assert!(store.is_empty());

        // Dismissing the dialog degrades to a safe read-only Answer.
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(None);
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), true).await;
        assert_eq!(effective, RequestedOutcome::Answer, "dismissed ask degrades to Answer");
        assert_eq!(confirmation, "dismissed", "a shown-but-dismissed ask is audited as dismissed");
        assert!(auth.is_read_only());
        assert!(store.is_empty());
    }

    /// Capability tiers are untouched (ADR-55 §Decision 2): the auto-grant
    /// only upgrades `RequireApproval` — shell stays approval-gated and
    /// Consequential actions (push, delete) are decided before the grant is
    /// ever consulted, never blanket-covered.
    #[tokio::test]
    async fn a2_auto_grant_never_covers_shell_or_consequential() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());

        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        let sink = StubIntentSink::new(None);
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new(), false)
                .await;
        assert_eq!(confirmation, "auto_granted");
        assert_eq!(effective, RequestedOutcome::Execute);
        assert!(!auth.is_read_only());

        let push_input = serde_json::json!({"operation": "push"});
        let push = action("git", &push_input);
        assert_eq!(
            auth.verdict(&push),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "Consequential egress is never blanket-covered by the auto-grant"
        );
        let shell_input = serde_json::json!({"cmd": "rm -rf target"});
        let shell = action("shell", &shell_input);
        assert_eq!(
            auth.verdict(&shell),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "shell stays approval-gated (never grantable)"
        );
        let delete_input = serde_json::json!({"operation": "delete", "path": "src/main.rs"});
        let delete = action("filesystem", &delete_input);
        assert_eq!(
            auth.verdict(&delete),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "destructive ops are Consequential and never auto-covered"
        );
    }
}
