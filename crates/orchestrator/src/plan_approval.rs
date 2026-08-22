//! ADR-55 Phase 1d: process-scoped plan binding registry and decision helper.
//!
//! §2 defines a registry keyed strictly by `(session_id, objective_hash)`
//! (newest-wins per key) that holds the most recent approved plan for an
//! objective as free text. The single-agent run consults it when an
//! action-required Execute request matches a stored plan: instead of the
//! generic intent confirmation it asks a real, audited
//! [`ApprovalSink::request_plan_approval`] question and translates the user's
//! `Apply` / `Replan` decision into the same authority rules the generic gate
//! uses (a confirmed Execute grants filesystem + git; anything else keeps the
//! run read-only).
//!
//! The registry is process-scoped (ADR-55 §2): it is populated after every
//! gate-enabled, *successful* Plan run and consulted before a later Execute
//! run of the same objective within the process. Plan text is capped at
//! [`MAX_PLAN_TEXT_BYTES`] and stored on a char boundary.
//!
//! ADR-55 §1 (pending): every binding carries an artifact hash — the blake3
//! fingerprint of its plan text captured at creation ([`PlanBinding::new`]).
//! Before a binding arms the Apply/Replan dialog it is re-verified
//! ([`verified_binding`]): a text that no longer matches its hash, or a
//! legacy durable row without a hash, falls through to the generic intent
//! gate instead of being shown (fail-soft, never blocking on storage).
//!
//! Live-fix (restart-safe approvals): the registry is mirrored to durable
//! storage in the session database (`concerto-sessions` `plan_bindings`,
//! same `(session_id, objective_hash)` key, newest-wins) by the run loop.
//! When an approval phrase offers no in-memory hit, the durable row is
//! rehydrated with [`PlanBinding::restored`] so "i approve the plan" after
//! an app restart still arms the real Apply/Replan dialog. A registry miss
//! falls through to the unchanged generic gate path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RouterOutput};
use concerto_core::CancellationToken;
use time::OffsetDateTime;

use crate::intent_grants::{grant_execute, IntentGrantStore};

/// Upper bound for stored plan text (16 KiB), enforced by [`plan_text_cap`].
pub const MAX_PLAN_TEXT_BYTES: usize = 16 * 1024;

/// One stored plan, bound to the exact objective it implements (ADR-55 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBinding {
    plan_id: String,
    objective_hash: String,
    source_revision: Option<String>,
    plan_text: String,
    /// blake3 fingerprint of `plan_text` captured at creation (ADR-55 §1
    /// pending: diff-vs-artifact). `None` only on bindings rehydrated from
    /// durable rows written before migration 025 — those are unverifiable.
    artifact_hash: Option<String>,
    created_at: OffsetDateTime,
}

impl PlanBinding {
    /// Create a binding. `plan_text` is capped by [`plan_text_cap`] so callers
    /// can pass an uncompressed final message; the cap is applied defensively
    /// here too to keep every registry entry within bounds no matter the
    /// caller. The artifact hash (blake3 of the capped text) is captured here
    /// so the dialog can later verify the text against its artifact.
    pub fn new(
        plan_id: String,
        objective_hash: String,
        source_revision: Option<String>,
        plan_text: String,
    ) -> Self {
        let plan_text = plan_text_cap(&plan_text);
        let artifact_hash = Some(plan_artifact_hash(&plan_text));
        Self {
            plan_id,
            objective_hash,
            source_revision,
            plan_text,
            artifact_hash,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    /// Rehydrate a binding from durable storage (live-fix: restart-safe
    /// Apply/Replan dialogs). Mirrors [`Self::new`]'s cap, but preserves the
    /// ORIGINAL `created_at` so newest-wins ordering survives a restart —
    /// a rehydrated binding must age like the binding it replaces. The
    /// stored `artifact_hash` rides along as-is: `None` marks a legacy row
    /// that is unverifiable at dialog arming.
    pub fn restored(
        plan_id: String,
        objective_hash: String,
        source_revision: Option<String>,
        plan_text: String,
        artifact_hash: Option<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            plan_id,
            objective_hash,
            source_revision,
            plan_text: plan_text_cap(&plan_text),
            artifact_hash,
            created_at,
        }
    }

    /// Stable identifier for this binding; surfaced in the audit `intent:plan`
    /// rows and the Apply/Replan dialog.
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    /// The objective this plan implements (the input hash it was keyed on).
    pub fn objective_hash(&self) -> &str {
        &self.objective_hash
    }

    /// The git revision the plan was created at, when known.
    pub fn source_revision(&self) -> Option<&str> {
        self.source_revision.as_deref()
    }

    /// The stored plan text, already capped by [`MAX_PLAN_TEXT_BYTES`].
    pub fn plan_text(&self) -> &str {
        &self.plan_text
    }

    /// The artifact hash (blake3 of the plan text at creation), when the
    /// binding carries one. `None` marks an unverifiable legacy binding.
    pub fn artifact_hash(&self) -> Option<&str> {
        self.artifact_hash.as_deref()
    }

    /// Does the stored plan text still match the artifact hash it was bound
    /// under at creation (ADR-55 §1 pending)? Re-hashes the current text and
    /// compares. A missing hash (legacy durable row) never verifies.
    pub fn artifact_verifies(&self) -> bool {
        match self.artifact_hash() {
            Some(stored) => stored == plan_artifact_hash(self.plan_text()),
            None => false,
        }
    }

    /// When the plan was recorded, UTC.
    pub fn created_at(&self) -> OffsetDateTime {
        self.created_at
    }
}

/// blake3 fingerprint of a plan's text — the `artifact_hash` captured on every
/// binding so the Apply/Replan dialog can verify the plan text it shows
/// against the creation-time artifact before arming (ADR-55 §1 pending).
pub fn plan_artifact_hash(plan_text: &str) -> String {
    blake3::hash(plan_text.as_bytes()).to_hex().to_string()
}

/// Process-scoped plan binding registry (ADR-55 §2).
///
/// Keyed by `(session_id, objective_hash)`; inserting a new plan for the same
/// key replaces the previous one (newest-wins). All operations are
/// synchronization-free from the caller's perspective.
#[derive(Debug, Default)]
pub struct PlanApprovalRegistry {
    /// session_id -> objective_hash -> binding
    bindings: Mutex<HashMap<Ulid, HashMap<String, PlanBinding>>>,
}

impl PlanApprovalRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `binding`, newest-wins per `(session_id, objective_hash)`.
    ///
    /// An empty (whitespace-only or capped-to-empty) plan is never stored — a
    /// binding must carry an actionable plan.
    pub fn insert(&self, session_id: Ulid, binding: PlanBinding) {
        if binding.plan_text().trim().is_empty() {
            return;
        }
        self.bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id)
            .or_default()
            .insert(binding.objective_hash.clone(), binding);
    }

    /// Strict `(session_id, objective_hash)` lookup. `None` on either key miss.
    pub fn pending(&self, session_id: Ulid, objective_hash: &str) -> Option<PlanBinding> {
        let bindings = self.bindings.lock().unwrap_or_else(|e| e.into_inner());
        bindings.get(&session_id)?.get(objective_hash).cloned()
    }

    /// Newest binding stored for `session_id` across every objective.
    ///
    /// ADR-55 Phase 2b: a planning-only run binds the rendered plan under the
    /// *current* input's hash, so a later natural-language approval ("i approve
    /// the plan") — which hashes differently — surfaces the just-rendered plan
    /// through this lookup rather than a strict-hash miss.
    ///
    /// Ordering is `created_at`, with a deterministic `plan_id` tie-break:
    /// two bindings recorded within the same clock tick must not resolve
    /// differently across runs (HashMap iteration order is unspecified).
    pub fn latest_for_session(&self, session_id: Ulid) -> Option<PlanBinding> {
        let bindings = self.bindings.lock().unwrap_or_else(|e| e.into_inner());
        bindings
            .get(&session_id)?
            .values()
            .max_by_key(|binding| (binding.created_at(), binding.plan_id().to_owned()))
            .cloned()
    }

    /// Remove and return the binding for `(session_id, objective_hash)`, if any.
    pub fn remove(&self, session_id: Ulid, objective_hash: &str) -> Option<PlanBinding> {
        let mut bindings = self.bindings.lock().unwrap_or_else(|e| e.into_inner());
        let removed = bindings.get_mut(&session_id)?.remove(objective_hash)?;
        // Drop the now-empty session bucket to avoid unbounded growth.
        if bindings.get(&session_id).is_some_and(HashMap::is_empty) {
            bindings.remove(&session_id);
        }
        Some(removed)
    }

    /// Drop every binding for `session_id` (e.g. `Stop` / session end).
    pub fn clear_session(&self, session_id: Ulid) {
        self.bindings.lock().unwrap_or_else(|e| e.into_inner()).remove(&session_id);
    }
}

/// Does the stored plan text still match the artifact hash it was bound under
/// at creation (ADR-55 §1 pending: diff-vs-artifact)?
///
/// Re-hashes the current text and compares against the stored hash. `None`
/// (a legacy durable row without a hash) never verifies — per ADR-55 the
/// verification is load-bearing, so an unverifiable binding is treated as no
/// binding rather than silently shown.
pub fn verified_binding(binding: PlanBinding) -> Option<PlanBinding> {
    if binding.artifact_verifies() {
        return Some(binding);
    }
    tracing::warn!(
        plan_id = %binding.plan_id(),
        artifact_hash = ?binding.artifact_hash(),
        "plan binding artifact verification failed (missing or mismatched artifact hash); \
         treating as no binding — falling through to the generic intent gate",
    );
    None
}

/// Process-scoped singleton registry (ADR-55 §2: "in-memory, held in the
/// process"). Frontends and the run loop share one registry so a plan created
/// by one run can be applied by a later run of the same objective.
static REGISTRY: OnceLock<Arc<PlanApprovalRegistry>> = OnceLock::new();

/// Access the process-scoped registry, initialising it on first use.
pub fn plan_registry() -> &'static Arc<PlanApprovalRegistry> {
    REGISTRY.get_or_init(|| Arc::new(PlanApprovalRegistry::new()))
}

/// Live-fix recovery (restart-safe Apply/Replan dialog): rehydrate the
/// session's newest DURABLE binding (concerto-sessions `plan_bindings`) into
/// the in-process registry.
///
/// Called when phrase arming finds no in-memory binding — the ordinary case
/// after an app restart between a planning run and the user's "i approve the
/// plan" — so the approval still surfaces the real dialog instead of
/// silently re-planning. Returns the re-seeded binding (also inserted under
/// the `(session_id, objective_hash)` key, newest-wins) or `None` when no
/// durable row exists. Fail-soft: a storage error logs and returns `None`,
/// leaving the run to fall through to the unchanged generic intent gate. The
/// rehydrated binding is also verified against its artifact hash (ADR-55 §1
/// pending): a tampered or legacy unverifiable row logs and returns `None`
/// and is never re-seeded into the registry.
pub async fn rehydrate_durable_binding(
    store: &dyn concerto_sessions::SessionStore,
    session_id: Ulid,
    cancel: CancellationToken,
) -> Option<PlanBinding> {
    match store.load_newest_plan_binding(session_id, cancel).await {
        Ok(Some(record)) => {
            let binding = PlanBinding::restored(
                record.plan_id,
                record.objective_hash,
                record.source_revision,
                record.plan_text,
                record.artifact_hash,
                record.created_at,
            );
            // ADR-55 §1 (pending): only a binding whose plan text still
            // matches its creation-time artifact hash may arm the dialog. A
            // tampered or legacy (unverifiable) row falls through to the
            // generic intent gate and is never re-seeded into the registry.
            let binding = verified_binding(binding)?;
            plan_registry().insert(session_id, binding.clone());
            tracing::info!(
                %session_id,
                plan_id = %binding.plan_id(),
                "rehydrated durable plan binding after restart"
            );
            Some(binding)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                %error,
                "durable plan binding lookup failed; falling through to the generic intent gate"
            );
            None
        }
    }
}

/// Cap plan text at [`MAX_PLAN_TEXT_BYTES`], truncating on a char boundary so
/// a multibyte character is never split in half. Empty text stays empty.
pub fn plan_text_cap(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    if text.len() <= MAX_PLAN_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_PLAN_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Translate the user's plan-binding decision into the run's effective outcome
/// and audit confirmation (ADR-55 Phase 1d).
///
/// - `Some(Apply)` → the stored plan is authorized; effective `Execute`
///   carries the same in-scope filesystem + git grants as a confirmed Execute
///   (via [`grant_execute`]); audit `"granted"`.
/// - `Some(Replan)` → the stored plan is discarded and the objective planned
///   anew, read-only, no grants; effective `Plan`, audit `"declined"`.
/// - `None` (dialog dismissed / no confirmation surface) → conservative
///   default: keep the routed Execute but stay read-only with no grants; no
///   mutation slips through; audit `"dismissed"`.
///
/// [`RequestedOutcome`] and [`RouterOutput`] keep the return and the decision
/// in the intent vocabulary shared with the router. The caller (runtime_runner)
/// is responsible for flipping the run's read-only flag from the returned
/// confirmation, mirroring [`crate::intent_grants::apply_intent_gate`].
pub fn apply_plan_decision(
    decision: Option<PlanDecision>,
    store: &Arc<IntentGrantStore>,
    _routing: &RouterOutput,
) -> (RequestedOutcome, &'static str) {
    match decision {
        Some(PlanDecision::Apply) => {
            grant_execute(store);
            (RequestedOutcome::Execute, "granted")
        }
        Some(PlanDecision::Replan) => (RequestedOutcome::Plan, "declined"),
        // `PlanDecision` is `#[non_exhaustive]`; an unknown future variant is
        // handled conservatively like a dismissed dialog (read-only, no grant).
        Some(_) => (RequestedOutcome::Execute, "dismissed"),
        None => (RequestedOutcome::Execute, "dismissed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn objective_hash() -> String {
        "0123456789abcdef0123456789abcdef".to_owned()
    }

    fn binding(objective: &str, plan_id: &str) -> PlanBinding {
        PlanBinding::new(
            plan_id.to_owned(),
            objective.to_owned(),
            None,
            "step 1: read the code".into(),
        )
    }

    fn routing_execute() -> RouterOutput {
        concerto_core::intent::route("implement the login feature", PathBuf::from("/tmp"))
    }

    #[test]
    fn registry_pending_requires_both_keys() {
        let registry = PlanApprovalRegistry::new();
        let session = Ulid::new();
        let hash = objective_hash();
        registry.insert(session, binding(&hash, "p1"));

        assert!(registry.pending(session, &hash).is_some(), "exact key pair resolves");
        assert!(
            registry.pending(Ulid::new(), &hash).is_none(),
            "different session misses even with a matching objective hash"
        );
        assert!(
            registry.pending(session, "fedcba9876543210fedcba9876543210").is_none(),
            "same session, different objective misses"
        );
    }

    #[test]
    fn registry_insert_is_newest_wins_per_key() {
        let registry = PlanApprovalRegistry::new();
        let session = Ulid::new();
        let hash = objective_hash();
        registry.insert(session, binding(&hash, "p1"));
        registry.insert(session, binding(&hash, "p2"));

        let pending = registry.pending(session, &hash).unwrap();
        assert_eq!(pending.plan_id(), "p2", "the newest plan replaces the previous one");
    }

    #[test]
    fn registry_remove_and_clear_session() {
        let registry = PlanApprovalRegistry::new();
        let session = Ulid::new();
        registry.insert(session, binding(&objective_hash(), "p1"));

        assert_eq!(registry.remove(session, &objective_hash()).unwrap().plan_id(), "p1");
        assert!(registry.pending(session, &objective_hash()).is_none());

        registry.insert(session, binding(&objective_hash(), "p2"));
        registry.clear_session(session);
        assert!(registry.pending(session, &objective_hash()).is_none());
    }

    #[test]
    fn registry_rejects_empty_plan_text() {
        let registry = PlanApprovalRegistry::new();
        let session = Ulid::new();
        let hash = objective_hash();
        let blank = PlanBinding::new("p1".into(), hash.clone(), None, "   \n  ".into());
        registry.insert(session, blank);
        assert!(registry.pending(session, &hash).is_none(), "whitespace-only plans are not stored");
    }

    #[test]
    fn registry_latest_for_session_returns_newest_across_objectives() {
        let registry = PlanApprovalRegistry::new();
        let session = Ulid::new();
        let other = Ulid::new();
        registry.insert(session, binding("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "p-early"));
        registry.insert(session, binding(&objective_hash(), "p-late"));
        registry.insert(other, binding("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "p-other"));

        let latest = registry.latest_for_session(session).expect("session has bindings");
        assert_eq!(
            latest.plan_id(),
            "p-late",
            "the most recently inserted binding wins across objectives"
        );
        // The interleaved insert for `other` must not disturb the session's
        // own order, nor leak into it.
        let latest_other = registry.latest_for_session(other).unwrap();
        assert_eq!(latest_other.plan_id(), "p-other");
        assert!(
            registry.latest_for_session(Ulid::new()).is_none(),
            "a session with no bindings misses"
        );
    }

    #[test]
    fn plan_text_cap_preserves_short_and_truncates_on_char_boundary() {
        assert_eq!(plan_text_cap(""), "");
        let short = "a short plan";
        assert_eq!(plan_text_cap(short), short);

        // 16 KiB of 'a' plus a 2-byte multibyte char pushes past the cap; the
        // truncation must land on the boundary before the multibyte char.
        let mut text = "a".repeat(MAX_PLAN_TEXT_BYTES);
        text.push('\u{00e9}'); // é is 2 bytes in UTF-8.
        let capped = plan_text_cap(&text);
        assert!(capped.len() <= MAX_PLAN_TEXT_BYTES, "never exceeds the cap");
        assert!(capped.is_char_boundary(capped.len()), "truncation lands on a char boundary");
        assert_eq!(capped, "a".repeat(MAX_PLAN_TEXT_BYTES), "multibyte char is dropped whole");
    }

    #[test]
    fn plan_binding_caps_plan_text_defensively() {
        let binding = PlanBinding::new(
            "p1".into(),
            objective_hash(),
            None,
            "x".repeat(MAX_PLAN_TEXT_BYTES + 1),
        );
        assert!(binding.plan_text().len() <= MAX_PLAN_TEXT_BYTES);
    }

    // ------------------------------------------------------------------
    // ADR-55 §1 (pending): the dialog's plan text is verified against the
    // creation-time artifact hash before arming.
    // ------------------------------------------------------------------

    #[test]
    fn new_computes_artifact_hash_over_capped_text() {
        let text = "step 1: read the code";
        let binding = PlanBinding::new("p1".into(), objective_hash(), None, text.into());
        assert_eq!(
            binding.artifact_hash(),
            Some(plan_artifact_hash(text).as_str()),
            "a fresh binding carries the blake3 fingerprint of its plan text"
        );
        assert!(verified_binding(binding.clone()).is_some(), "a fresh binding always verifies");
    }

    #[test]
    fn tampered_plan_text_fails_verification_and_falls_through() {
        let binding =
            PlanBinding::new("p1".into(), objective_hash(), None, "step 1: read the code".into());
        // Alter the stored text behind the accessor's back: the hash still
        // covers the ORIGINAL text, so verification must fail and the
        // binding must fall through (no dialog).
        let mut tampered = binding.clone();
        tampered.plan_text = "step 1: read the code and delete everything".into();
        assert!(!tampered.artifact_verifies(), "altered plan text never verifies");
        assert!(verified_binding(tampered).is_none(), "tampered binding falls through");
        assert!(binding.artifact_verifies(), "the untouched binding still verifies");
    }

    #[test]
    fn restored_without_hash_is_unverifiable() {
        // A legacy durable row (pre-migration 025) carries no artifact hash.
        let legacy = PlanBinding::restored(
            "p1".into(),
            objective_hash(),
            None,
            "step 1: read the code".into(),
            None,
            OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .unwrap_or_else(|_| OffsetDateTime::now_utc()),
        );
        assert!(legacy.artifact_hash().is_none(), "legacy row has no hash");
        assert!(
            verified_binding(legacy).is_none(),
            "an unverifiable legacy binding falls through to the generic gate"
        );
    }

    #[test]
    fn restored_with_matching_hash_verifies() {
        let text = "step 1: read the code";
        let restored = PlanBinding::restored(
            "p1".into(),
            objective_hash(),
            None,
            text.into(),
            Some(plan_artifact_hash(text)),
            OffsetDateTime::now_utc(),
        );
        assert!(restored.artifact_verifies());
        assert!(verified_binding(restored).is_some());
    }

    #[test]
    fn plan_artifact_hash_is_deterministic_and_sensitive() {
        let text = "step 1: read the code";
        assert_eq!(plan_artifact_hash(text), plan_artifact_hash(text));
        assert_ne!(plan_artifact_hash(text), plan_artifact_hash(&format!("{text} ")));
        assert_eq!(plan_artifact_hash("").len(), 64, "blake3 hex output is 64 chars");
    }

    #[test]
    fn apply_grant_authorizes_fs_and_git_like_confirmed_execute() {
        let store = Arc::new(IntentGrantStore::new());
        let routing = routing_execute();

        let (effective, confirmation) =
            apply_plan_decision(Some(PlanDecision::Apply), &store, &routing);
        assert_eq!(effective, RequestedOutcome::Execute);
        assert_eq!(confirmation, "granted");
        assert!(store.covers("filesystem"), "an Apply decision grants local fs mutations");
        assert!(store.covers("git"), "an Apply decision grants local git mutations");
    }

    #[test]
    fn replan_stays_read_only_without_grants() {
        let store = Arc::new(IntentGrantStore::new());
        let routing = routing_execute();

        let (effective, confirmation) =
            apply_plan_decision(Some(PlanDecision::Replan), &store, &routing);
        assert_eq!(effective, RequestedOutcome::Plan, "replan re-routes the run to Plan");
        assert_eq!(confirmation, "declined");
        assert!(store.is_empty(), "replan never grants");
    }

    #[test]
    fn dismissed_decision_keeps_execute_read_only() {
        let store = Arc::new(IntentGrantStore::new());
        let routing = routing_execute();

        let (effective, confirmation) = apply_plan_decision(None, &store, &routing);
        assert_eq!(effective, RequestedOutcome::Execute, "dismissed keeps the routed intent");
        assert_eq!(confirmation, "dismissed");
        assert!(store.is_empty(), "a missing response never grants");
    }
}
