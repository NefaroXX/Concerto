//! Session-scoped, non-durable intent grants (ADR-55 §4).
//!
//! The router classifies; the classifier never grants (ADR-55 §1). Only a
//! confirmed user decision can create a grant. This module owns the run-scoped
//! grant store and the [`IntentAuthorization`] provider that feeds
//! [`SimplePolicyEngine::with_intent_auth`] under `Condition::IntentAuthorized`.
//!
//! Grants are created fresh per `run_shared_agent` call, so they are bound to a
//! single run/session by construction and are re-confirmed on every resume
//! (non-durable — nothing is persisted).

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
    /// Callers are the run loop, exclusively after a user confirmation
    /// (ADR-55 §1/§4) — never from routing or classification.
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
/// `is_read_only_intent` starts `true`: a run stays read-only until the user
/// confirms a mutating intent through the approval sink — the conservative
/// reading when no confirmation surface is available (a `None` response never
/// lets a mutation slip through). Only the run loop changes it.
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
/// Shared by the two paths that carry a confirmed Execute decision: a picked
/// `Execute` in [`apply_intent_gate`] and a picked `Apply` on a stored plan
/// binding (see `crate::plan_approval::apply_plan_decision`). Mutate-local,
/// in-scope grants never cover shell (never grantable) and never cover
/// Consequential/destructive actions (decided before the grant is consulted),
/// so this helper can never widen them.
pub fn grant_execute(store: &IntentGrantStore) {
    store.grant(RequestedOutcome::Execute, "filesystem");
    store.grant(RequestedOutcome::Execute, "git");
}

/// Apply the ADR-55 gate to a routed request and return the run's EFFECTIVE
/// outcome — what the user actually confirmed — plus the audit confirmation
/// value (`granted` | `declined` | `dismissed` | `n/a`). `n/a` means the gate
/// never prompted (deterministic read-only outcomes); `dismissed` means a
/// prompt WAS shown but the confirmation surface returned no answer.
///
/// - `Execute` with confidence >= [`LOW_CONFIDENCE_THRESHOLD`] asks the sink;
///   `Some(Execute)` grants filesystem + git (explicit user authorization), a
///   picked read-only outcome re-routes the run to that outcome (read-only,
///   no grant), and `None` (dialog dismissed) keeps the router's Execute but
///   read-only, and is audited as `"dismissed"` (not `"n/a"` — a prompt was
///   shown).
/// - [`RouterRoute::AskUser`] (ambiguous input) asks with all six outcomes; a
///   picked `Execute` grants, any other pick re-routes read-only, and a
///   dismissed dialog degrades to `Answer` read-only, audited as `"dismissed"`.
/// - Deterministic read-only outcomes never prompt and are audited as `"n/a"`.
///
/// Authority is the user decision only: deterministic routing and (later) the
/// LLM classifier can classify but never grant (ADR-55 §1). A read-only run
/// hard-denies all mutation — filesystem, shell, and git — even under session
/// auto-approve (B-1).
pub async fn apply_intent_gate(
    routing: &RouterOutput,
    approval_sink: &dyn ApprovalSink,
    store: &IntentGrantStore,
    auth: &SessionIntentAuth,
    cancel: CancellationToken,
) -> (RequestedOutcome, &'static str) {
    let (effective, confirmation) = match routing.outcome {
        RequestedOutcome::Execute if routing.confidence >= LOW_CONFIDENCE_THRESHOLD => {
            match approval_sink
                .request_intent_confirmation(
                    format!(
                        "The request looks like it wants to change code (router confidence \
                         {:.2}). Confirm the run's mutation scope, or pick a read-only \
                         outcome.",
                        routing.confidence,
                    ),
                    &[
                        RequestedOutcome::Execute,
                        RequestedOutcome::Plan,
                        RequestedOutcome::Review,
                        RequestedOutcome::Answer,
                    ],
                    cancel.clone(),
                )
                .await
            {
                Some(RequestedOutcome::Execute) => (RequestedOutcome::Execute, "granted"),
                Some(other) => (other, "declined"),
                // The dialog WAS shown and the user dismissed it: audit it as
                // "dismissed", not "n/a" (which is reserved for paths that
                // never prompted).
                None => (RequestedOutcome::Execute, "dismissed"),
            }
        }
        // AskUser route: the deterministic router found nothing conclusive
        // (Phase 0 yields outcome Answer with confidence 0.0). Ask instead of
        // guessing — read-only unless the user picks Execute.
        RequestedOutcome::Answer if routing.route == RouterRoute::AskUser => {
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
        assert_eq!(outcome_name(RequestedOutcome::Execute), "Execute");
        assert_eq!(outcome_name(RequestedOutcome::Plan), "Plan");
        assert_eq!(outcome_name(RequestedOutcome::Verify), "Verify");
    }

    // ------------------------------------------------------------------
    // apply_intent_gate (ADR-55 §1/§2/§4/§6)
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

    #[tokio::test]
    async fn gate_grants_only_on_confirmed_execute() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        assert_eq!(routing.outcome, RequestedOutcome::Execute);

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

        assert_eq!(effective, RequestedOutcome::Execute, "confirmed Execute stays Execute");
        assert_eq!(confirmation, "granted");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "exactly one confirmation prompt");
        assert_eq!(
            sink.prompts.lock().unwrap_or_else(|e| e.into_inner())[0],
            vec![
                RequestedOutcome::Execute,
                RequestedOutcome::Plan,
                RequestedOutcome::Review,
                RequestedOutcome::Answer,
            ],
            "execute confirmation offers the mutation + read-only alternatives"
        );
        assert!(!auth.is_read_only(), "a confirmed Execute run is mutation-capable");
        assert!(store.covers("filesystem"), "fs mutations are in scope");
        assert!(store.covers("git"), "git local mutations are in scope");
    }

    #[tokio::test]
    async fn gate_declines_when_user_picks_a_read_only_outcome() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Plan));

        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

        assert_eq!(effective, RequestedOutcome::Plan, "picking Plan re-routes the run to Plan");
        assert_eq!(confirmation, "declined");
        assert!(auth.is_read_only(), "redirecting to a read-only outcome keeps the run read-only");
        assert!(store.is_empty(), "no grant was created");
    }

    #[tokio::test]
    async fn gate_without_a_confirmation_surface_stays_read_only() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        // `None` is the conservative reading: no sink implements the surface.
        let sink = StubIntentSink::new(None);

        let routing = concerto_core::intent::route("implement the login feature", project_dir());
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

        assert_eq!(
            effective,
            RequestedOutcome::Execute,
            "dismissed dialog keeps the routed intent"
        );
        assert_eq!(
            confirmation, "dismissed",
            "a shown-but-dismissed prompt is audited as dismissed"
        );
        assert!(auth.is_read_only(), "a missing response never lets a mutation slip through");
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn gate_never_prompts_for_read_only_outcomes() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("review the parser code", project_dir());
        assert_eq!(routing.outcome, RequestedOutcome::Review);

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

        assert_eq!(effective, RequestedOutcome::Review);
        assert_eq!(confirmation, "n/a");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "read-only outcomes never prompt");
        assert!(auth.is_read_only());
        assert!(store.is_empty());
    }

    /// A small-talk-routed greeting ("hi, lets work on something") yields a
    /// deterministic read-only `Answer` via `RuleHit { rule: "smalltalk" }`,
    /// NOT `RouterRoute::AskUser` — so `apply_intent_gate` takes the
    /// `outcome => (outcome, "n/a")` arm (the AskUser modal fires only for
    /// `RequestedOutcome::Answer if routing.route == RouterRoute::AskUser`)
    /// and never calls `request_intent_confirmation`.
    #[tokio::test]
    async fn gate_never_prompts_for_smalltalk_greetings() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("hi, lets work on something", project_dir());
        assert_eq!(routing.outcome, RequestedOutcome::Answer);
        assert!(matches!(routing.route, RouterRoute::RuleHit { rule: "smalltalk" }));

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

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

    #[tokio::test]
    async fn gate_ask_user_prompts_with_all_outcomes() {
        let project = project_dir();

        // Picking Execute on an ambiguous request grants (explicit user
        // authorization — the classifier/rule path itself can never grant).
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(Some(RequestedOutcome::Execute));

        let routing = concerto_core::intent::route("zzzzzzzzzz", project.clone());
        assert_eq!(routing.route, RouterRoute::AskUser);
        assert_eq!(routing.outcome, RequestedOutcome::Answer);

        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;

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
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;
        assert_eq!(effective, RequestedOutcome::Diagnose);
        assert_eq!(confirmation, "declined");
        assert!(auth.is_read_only());
        assert!(store.is_empty());

        // Dismissing the dialog degrades to a safe read-only Answer.
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let sink = StubIntentSink::new(None);
        let (effective, confirmation) =
            apply_intent_gate(&routing, &sink, &store, &auth, CancellationToken::new()).await;
        assert_eq!(effective, RequestedOutcome::Answer, "dismissed ask degrades to Answer");
        assert_eq!(confirmation, "dismissed", "a shown-but-dismissed ask is audited as dismissed");
        assert!(auth.is_read_only());
        assert!(store.is_empty());
    }
}
