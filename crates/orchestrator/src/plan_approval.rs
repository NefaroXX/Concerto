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
//!
//! ADR-60 D7 (issue #152): an approved plan is ALSO persisted as a
//! content-addressed whiteboard event ([`WhiteboardKind::PlanApproved`])
//! keyed by `plan_id` — the whiteboard log is the sole source of truth for
//! Plan→Execute continuity; no projected table exists (oracle review
//! 2026-08-22, comment 1). The payload carries the structured DesignDoc when
//! one was produced, the capped plan text it hashes, and the artifact/source
//! identity needed to verify integrity before rehydration
//! ([`load_approved_plan`]). An Execute run of an approved plan rehydrates
//! that verified state instead of re-deriving the plan from rendered prose,
//! and any divergence from what the user approved is a loud failure — silent
//! re-decompose is forbidden.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RouterOutput};
use concerto_core::types::DesignDoc;
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::intent_grants::{grant_execute, IntentGrantStore};
use concerto_sessions::whiteboard::{
    append_whiteboard_event, compute_content_hash, load_whiteboard_events_by_plan,
    NewWhiteboardEvent, WhiteboardEvent, WhiteboardKind,
};

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

// ===========================================================================
// ADR-60 D7 (issue #152): whiteboard plan binding — write + rehydrate.
//
// The whiteboard `plan_id`-keyed event is the SOLE source of truth for
// Plan→Execute continuity (oracle comment 1): no projected table, no parallel
// store. Write ordering at approval time is (1) whiteboard event commits
// (BEGIN IMMEDIATE, WAL-durable on return), then (2) the in-memory registry
// insert, then (3) the `plan_bindings` durable mirror — a crash between the
// steps leaves the LOG ahead of the projections, never behind, so an Execute
// read either sees a fully verified artifact or degrades to the legacy prose
// path with a warn. Every Execute-phase tool application is ordered after the
// plan event by construction: the event lands during the Plan turn, strictly
// before any Execute dispatch, and Execute-phase tool writes keep their own
// gate WAL-before-execute invariant unchanged.
// ===========================================================================

/// Content-addressed payload of a [`WhiteboardKind::PlanApproved`] whiteboard
/// event (ADR-60 D7). The row's own `content_hash` fingerprints this payload;
/// `artifact_hash` additionally fingerprints `plan_text` via
/// [`plan_artifact_hash`], so tampering with either layer is detectable at
/// rehydration time ([`load_approved_plan`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanApprovedPayload {
    /// The plan this approval binds (mirrored into the row's `plan_id`
    /// column; duplicated in-payload so the payload alone is self-describing).
    pub plan_id: String,
    /// The objective the approved plan implements.
    pub objective_hash: String,
    /// blake3 of the capped plan text at creation (`plan_artifact_hash`).
    pub artifact_hash: String,
    /// Git revision the plan was created at, when known.
    pub source_revision: Option<String>,
    /// Structured DesignDoc when the planning run produced one (multi-agent
    /// planning-only depth). `None` for single-agent plans whose capped text
    /// IS the artifact.
    #[serde(default)]
    pub design_doc: Option<DesignDoc>,
    /// The capped plan text the hash covers.
    pub plan_text: String,
    /// Approval instant, unix epoch milliseconds UTC.
    pub created_at_ms: i64,
}

/// Carry-forward state an approved-plan Execute seeds from the whiteboard
/// ledger (ADR-60 D7 fix #3: completed subtask results, files touched, failed
/// commands with their failure reasons). Extracted defensively from whatever
/// ledger events exist for the plan — a first Execute simply carries empty
/// lists, which is the truthful state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanLedger {
    /// Descriptions of subtasks already completed under this plan.
    pub completed_subtasks: Vec<String>,
    /// File paths mutated by gated writes under this plan.
    pub files_touched: Vec<String>,
    /// Recorded failure reasons (failed commands/tools) that must not be
    /// re-run unchanged.
    pub failed_commands: Vec<String>,
}

/// Everything an approved-plan Execute run rehydrates from the whiteboard:
/// the verified binding plus the structured doc and carry-forward ledger.
/// (`DesignDoc` carries no `PartialEq`, so equality is compared through its
/// canonical JSON where tests need it.)
#[derive(Debug, Clone)]
pub struct ApprovedPlanContext {
    /// The binding the user approved (hash-verified against the log).
    pub binding: PlanBinding,
    /// Structured DesignDoc when the planning run produced one; `None` means
    /// the verified plan text is the artifact.
    pub design_doc: Option<DesignDoc>,
    /// Carry-forward state folded from the plan's ledger events.
    pub ledger: PlanLedger,
}

/// Unix epoch milliseconds for an approval timestamp.
fn offset_ms(at: OffsetDateTime) -> i64 {
    at.unix_timestamp() * 1000 + i64::from(at.millisecond())
}

/// Append the content-addressed `plan-approved` whiteboard event for
/// `binding` (ADR-60 D7 write side). Fail-soft by contract: the caller logs
/// a returned error and the run proceeds — Execute then falls back to the
/// legacy prose path rather than reading an artifact that was never durably
/// attested.
pub async fn append_plan_approved_event(
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
    binding: &PlanBinding,
    design_doc: Option<&DesignDoc>,
) -> Result<WhiteboardEvent, concerto_sessions::SessionError> {
    let payload = PlanApprovedPayload {
        plan_id: binding.plan_id().to_owned(),
        objective_hash: binding.objective_hash().to_owned(),
        artifact_hash: binding.artifact_hash().unwrap_or_default().to_owned(),
        source_revision: binding.source_revision().map(ToOwned::to_owned),
        design_doc: design_doc.cloned(),
        plan_text: binding.plan_text().to_owned(),
        created_at_ms: offset_ms(binding.created_at()),
    };
    let payload = serde_json::to_value(&payload).map_err(|error| {
        concerto_sessions::SessionError::Serialization(format!(
            "plan-approved payload serialization: {error}"
        ))
    })?;
    append_whiteboard_event(
        pool,
        &NewWhiteboardEvent {
            // Idempotency key (ULID, same convention as the supervisor's
            // publish path); a retry mints a fresh key because a re-approval
            // is a new fact, not a replay.
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::PlanApproved,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: Some(binding.plan_id().to_owned()),
            causation: None,
            payload,
            // Not a write: no pre-image (the gate fills this column for
            // write events only).
            pre_image_hash: None,
            created_at: offset_ms(OffsetDateTime::now_utc()),
        },
    )
    .await
}

/// Load and VERIFY the approved plan's whiteboard state for an Execute run
/// (ADR-60 D7 read side + divergence guard).
///
/// Return contract:
/// - `Ok(Some(ctx))` — the newest approval matches `binding`, its payload is
///   internally content-addressed, and the ledger has been folded.
/// - `Ok(None)` — no `plan-approved` events exist for the binding's plan id
///   (a pre-D7 binding): the caller degrades to the legacy prose path with a
///   warn. Missing state is never invented.
/// - `Err(reason)` — DIVERGENCE between the log and what the user approved:
///   a second approval of the same plan id with different content (an
///   injected change after approval), a payload that fails its own artifact
///   hash, or a payload/binding mismatch. The caller must fail the run
///   loudly — silent re-decompose is forbidden (oracle comment 3).
pub async fn load_approved_plan(
    pool: &sqlx::SqlitePool,
    binding: &PlanBinding,
) -> Result<Option<ApprovedPlanContext>, String> {
    let plan_id = binding.plan_id();
    let events = load_whiteboard_events_by_plan(pool, plan_id)
        .await
        .map_err(|error| format!("approved plan lookup failed for {plan_id}: {error}"))?;
    let approvals: Vec<&WhiteboardEvent> =
        events.iter().filter(|event| event.kind == WhiteboardKind::PlanApproved).collect();
    if approvals.is_empty() {
        return Ok(None);
    }

    // The load is gate_seq-ordered, so `approvals` is oldest→newest. Every
    // approval under one plan id must carry IDENTICAL content: a second
    // approval with a different hash means the artifact changed under this
    // plan id after the user approved it (identical replays are fine).
    let mut reference_payload: Option<PlanApprovedPayload> = None;
    for approval in &approvals {
        let payload: PlanApprovedPayload = serde_json::from_value(approval.payload.clone())
            .map_err(|error| {
                format!(
                    "approved plan {plan_id} carries an unreadable plan-approved payload \
                     (event {}): {error}",
                    approval.event_id
                )
            })?;
        match &reference_payload {
            None => reference_payload = Some(payload),
            Some(first) if first.artifact_hash == payload.artifact_hash => {}
            Some(first) => {
                return Err(format!(
                    "divergence detected for approved plan {plan_id}: whiteboard holds two \
                     plan-approved artifacts with different content hashes ({}/{}); explicit \
                     user re-approval is required before execution",
                    first.artifact_hash, payload.artifact_hash,
                ));
            }
        }
    }
    let payload = match reference_payload {
        Some(payload) => payload,
        // Unreachable in practice (the empty-list check above returned), but
        // a missing payload degrades instead of panicking.
        None => return Ok(None),
    };

    // Content addressing: the payload's text must hash to the hash it
    // declares, and both must match the binding the user actually approved.
    if plan_artifact_hash(&payload.plan_text) != payload.artifact_hash {
        return Err(format!(
            "divergence detected for approved plan {plan_id}: payload text does not match its \
             declared artifact_hash; explicit user re-approval is required before execution"
        ));
    }
    if binding.artifact_hash() != Some(payload.artifact_hash.as_str()) {
        return Err(format!(
            "divergence detected for approved plan {plan_id}: whiteboard artifact hash {} does \
             not match the approved binding hash {}; explicit user re-approval is required \
             before execution",
            payload.artifact_hash,
            binding.artifact_hash().unwrap_or("<none>"),
        ));
    }
    if payload.objective_hash != binding.objective_hash() {
        return Err(format!(
            "divergence detected for approved plan {plan_id}: whiteboard objective hash {} does \
             not match the approved binding objective {}; explicit user re-approval is required \
             before execution",
            payload.objective_hash,
            binding.objective_hash(),
        ));
    }

    Ok(Some(ApprovedPlanContext {
        binding: binding.clone(),
        design_doc: payload.design_doc,
        ledger: fold_ledger(&events),
    }))
}

/// Fold a plan's non-approval events into the carry-forward ledger.
/// Extraction is defensive: every writer (gate, agent-process children) uses
/// its own payload shape, so fields are probed and absent ones skipped — a
/// malformed sibling event never blocks continuity.
///
/// `pub(crate)` so the run-continuity reader (`runtime_runner`) folds the
/// session's gate-written events with the exact same extraction rules as the
/// approved-plan read — one ledger grammar, two read paths.
pub(crate) fn fold_ledger(events: &[WhiteboardEvent]) -> PlanLedger {
    let mut ledger = PlanLedger::default();
    for event in events {
        match event.kind {
            WhiteboardKind::SubtaskCompleted => {
                if let Some(description) =
                    string_field(&event.payload, &["description", "summary", "task_id"])
                {
                    ledger.completed_subtasks.push(description);
                }
            }
            WhiteboardKind::Failure => {
                if let Some(failure) = string_field(&event.payload, &["error", "reason"]) {
                    let tool =
                        string_field(&event.payload, &["tool"]).unwrap_or_else(|| "tool".into());
                    ledger.failed_commands.push(format!("{tool}: {failure}"));
                }
            }
            WhiteboardKind::WriteApplied => {
                // Gated writes carry per-target pre-images keyed by relative
                // path — exactly the "files touched" record (D5 attribution).
                if let Some(pre_images) =
                    event.payload.get("pre_images").and_then(|v| v.as_object())
                {
                    for path in pre_images.keys() {
                        ledger.files_touched.push(path.clone());
                    }
                } else if let Some(path) = string_field(&event.payload, &["path", "target"]) {
                    ledger.files_touched.push(path);
                }
            }
            // Findings/decisions/task-graph rows inform agents via their own
            // surfaces; they are not carry-forward facts.
            _ => {}
        }
    }
    ledger
}

/// Probe `payload` for the first present string field among `keys`.
fn string_field(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(|value| value.as_str()).map(ToOwned::to_owned))
}

// ===========================================================================
// ADR-60 Deferred 3: `run_review_cycle` resumability — write + rehydrate.
//
// An in-flight review cycle used to die with the process (coordinator.rs
// historically noted "there is no call to save"): a restart either lost the
// review or ran a duplicate second review of the same deliverable. The
// whiteboard is the durable fix, exactly as for D7 above: one shared event
// kind ([`WhiteboardKind::ReviewState`]) with ONE payload serialization,
// defined here so every future consumer (the coordinator today; any
// supervisor-side review participant later) reads and writes the same shape.
//
// Write ordering is WAL-before-invoke (oracle 2026-08-23 comment 2): the
// caller awaits [`append_review_state_event`] and only then spawns the
// reviewer or continues the loop — the snapshot commits (BEGIN IMMEDIATE,
// WAL-durable on return) before any model call that could be lost.
//
// Each event is a FULL self-contained snapshot: plan id, review target,
// complete feedback ledger, retry counter, and the `gate_seq` cursor of the
// previous snapshot in the cycle group (oracle answer: minimal
// plan_id+target stashes were rejected — resuming from scratch would repeat
// paid-for reviewer work). Rehydration validates before trusting
// (oracle comment 1): each row's canonical content hash is recomputed and a
// structurally inconsistent ledger (counters that disagree with the entries,
// cycles out of range, a cursor pointing past the row itself) is rejected —
// a corrupt state degrades to a fresh cycle with a warn, never to silently
// trusting injected data. Idempotency (oracle comment 3): the newest valid
// snapshot decides — an already-terminal cycle is reported as resolved
// instead of resumed, and a same-process terminal never suppresses a sibling
// subtask's own fresh review.
// ===========================================================================

/// One recorded reviewer verdict inside a review cycle's feedback ledger.
///
/// Only non-terminal verdicts are recorded: a pass or an escalation is the
/// terminal event's `status`, while every needs-revision verdict consumes one
/// cycle slot and queues one implement revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFeedbackEntry {
    /// The review cycle (1-based) whose reviewer returned this verdict.
    pub cycle_num: u32,
    /// Verdict string; `"needs-revision"` today.
    pub verdict: String,
    /// The revision reason, when the reviewer gave one.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Terminal / progress status carried by a [`WhiteboardKind::ReviewState`]
/// snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewCycleStatus {
    /// The reviewer invocation for cycle `ledger.len() + 1` is about to start
    /// (or was in flight when the process died — its result is unknown).
    Started,
    /// Cycle `ledger.len()` returned needs-revision and the implement
    /// revision was queued; the next reviewer invocation is one revision
    /// later.
    RevisionQueued,
    /// The reviewer passed — the cycle group is settled.
    Completed,
    /// `max_cycles` was reached unresolved — the cycle group is settled.
    Escalated,
}

impl ReviewCycleStatus {
    /// Whether this status settles the cycle group (no further reviewer work).
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Escalated)
    }
}

/// Content payload of a [`WhiteboardKind::ReviewState`] whiteboard event
/// (ADR-60 Deferred 3). The row's own `content_hash` fingerprints this
/// payload via the log's canonical hashing, which [`load_review_resume`]
/// recomputes on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStatePayload {
    /// The approved plan this review runs under (mirrored into the row's
    /// `plan_id` column; duplicated in-payload so the payload alone is
    /// self-describing).
    pub plan_id: String,
    /// Session that wrote the snapshot — distinguishes a previous process
    /// attempt's settled review from a sibling subtask of the live run.
    pub session_id: String,
    /// The implement-stage role whose output is under review.
    pub implement_role: String,
    /// Human-readable review target (the implement subtask description).
    pub review_target: String,
    /// Stable identity of the target within the plan:
    /// [`review_target_identity`] of `(implement_role, review_target)`.
    /// For approved-plan runs the decomposition comes from the seeded
    /// DesignDoc, so this survives a restart byte-identically.
    pub review_target_hash: String,
    /// Snapshot status.
    pub status: ReviewCycleStatus,
    /// Cycle cap recorded when the snapshot was written.
    pub max_cycles: u32,
    /// Needs-revision verdicts issued so far (`== feedback_ledger.len()`).
    pub retry_count: u32,
    /// FULL feedback ledger snapshot — every needs-revision verdict so far,
    /// oldest first.
    pub feedback_ledger: Vec<ReviewFeedbackEntry>,
    /// `gate_seq` of the previous snapshot in this cycle group (`0` = none);
    /// the consistent-cut coordinate linking the snapshots in order. Must
    /// never exceed the storing row's own `gate_seq` (the future cannot be
    /// cited).
    pub gate_seq_cursor: u64,
    /// Snapshot instant, unix epoch milliseconds UTC.
    pub created_at_ms: i64,
}

impl ReviewStatePayload {
    /// Structural validation applied before a loaded snapshot is trusted.
    ///
    /// Checks the invariants every writer in this module upholds:
    /// - the target hash matches what the caller is resuming;
    /// - ledger cycles are strictly ascending from 1 and within the recorded
    ///   cap, and every entry is a needs-revision verdict;
    /// - `retry_count == feedback_ledger.len()`;
    /// - open statuses leave at least one cycle unspent, `Escalated` spends
    ///   them all, and `Completed` is reachable from any count;
    /// - the cursor does not cite a log position after the row itself.
    fn is_internally_consistent(
        &self,
        target_hash: &str,
        row_gate_seq: u64,
        spent_cycles: u32,
    ) -> bool {
        if self.review_target_hash != target_hash || self.gate_seq_cursor > row_gate_seq {
            return false;
        }
        if self.retry_count as usize != self.feedback_ledger.len() {
            return false;
        }
        for (index, entry) in self.feedback_ledger.iter().enumerate() {
            // `u32::try_from` cannot fail for a realistic ledger length, but
            // a cast would silently wrap — compare without casting.
            let expected = match u32::try_from(index + 1) {
                Ok(value) => value,
                Err(_) => return false,
            };
            if entry.cycle_num != expected
                || entry.cycle_num > self.max_cycles
                || entry.verdict != "needs-revision"
            {
                return false;
            }
        }
        match self.status {
            ReviewCycleStatus::Started | ReviewCycleStatus::RevisionQueued => {
                spent_cycles < self.max_cycles
            }
            ReviewCycleStatus::Completed => true,
            ReviewCycleStatus::Escalated => spent_cycles == self.max_cycles,
        }
    }
}

/// Stable identity of a review target within a plan: blake3 over
/// `implement_role \0 description`. Deterministic across restarts so the
/// rehydrated lookup finds exactly the crashed cycle group.
pub fn review_target_identity(implement_role: &str, review_target: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(implement_role.as_bytes());
    hasher.update(&[0]);
    hasher.update(review_target.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Append one [`WhiteboardKind::ReviewState`] snapshot and return the stored
/// row (its log-assigned `gate_seq` becomes the caller's next cursor).
///
/// WAL-before-invoke contract: the caller must await this BEFORE spawning the
/// reviewer or continuing cycle logic, so a crash can only ever land between
/// durable snapshots. Idempotency note: each transition mints a fresh
/// `event_id` because it is a NEW fact (a distinct checkpoint), not a replay
/// of an earlier one.
pub async fn append_review_state_event(
    pool: &sqlx::SqlitePool,
    payload: &ReviewStatePayload,
) -> Result<WhiteboardEvent, concerto_sessions::SessionError> {
    let payload_json = serde_json::to_value(payload).map_err(|error| {
        concerto_sessions::SessionError::Serialization(format!(
            "review-state payload serialization: {error}"
        ))
    })?;
    append_whiteboard_event(
        pool,
        &NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::ReviewState,
            scope: String::new(),
            session_id: Some(payload.session_id.clone()),
            plan_id: Some(payload.plan_id.clone()),
            causation: None,
            payload: payload_json,
            // Not a write: no pre-image (the gate fills this column for
            // write events only).
            pre_image_hash: None,
            created_at: offset_ms(OffsetDateTime::now_utc()),
        },
    )
    .await
}

/// What an entering review cycle should do, decided from the durable
/// whiteboard state for `(plan_id, review_target_hash)`.
#[derive(Debug, Clone)]
pub enum ReviewResume {
    /// No trustworthy prior state — start at cycle 1 (pre-Phase 3 behavior).
    Fresh,
    /// An open cycle group exists: continue at `resume_cycle`
    /// (`feedback_ledger.len() + 1`) with the persisted counters, injecting
    /// the full ledger into the reviewer context. `from_gate_seq` chains the
    /// cursor for the next snapshot.
    Resume {
        resume_cycle: u32,
        retry_count: u32,
        feedback_ledger: Vec<ReviewFeedbackEntry>,
        from_gate_seq: u64,
    },
    /// The cycle group already settled in a previous process attempt — the
    /// recorded outcome stands and NO second review may run (oracle
    /// comment 3). A terminal snapshot written by the CURRENT session is
    /// deliberately NOT honored here: within one live run it belongs to a
    /// sibling subtask with an identical target description, which deserves
    /// its own fresh review.
    Resolved { status: ReviewCycleStatus, feedback_ledger: Vec<ReviewFeedbackEntry> },
}

/// Load and VALIDATE the durable review-cycle state for a target (ADR-60
/// Deferred 3 read side).
///
/// Every [`WhiteboardKind::ReviewState`] row under `plan_id` is verified
/// against its own canonical content hash (rejecting stale/corrupt injection,
/// oracle comment 1) and structurally validated; rejected rows are skipped
/// with a warn, and if none survive the answer is [`ReviewResume::Fresh`] —
/// degraded-but-safe, never silently trusting bad data. Among surviving rows
/// the newest `gate_seq` decides between resume and resolved.
///
/// `Err` means the LOOKUP itself failed (storage error); the caller degrades
/// to a fresh cycle with a warn rather than failing the run — continuity
/// bookkeeping never blocks a review the way divergence-guarded plan
/// artifacts legitimately do.
pub async fn load_review_resume(
    pool: &sqlx::SqlitePool,
    plan_id: &str,
    target_hash: &str,
    current_session_id: &str,
) -> Result<ReviewResume, String> {
    let events = load_whiteboard_events_by_plan(pool, plan_id)
        .await
        .map_err(|error| format!("review-state lookup failed for {plan_id}: {error}"))?;

    let mut valid: Vec<(u64, Option<String>, ReviewStatePayload)> = Vec::new();
    for event in events.iter().filter(|event| {
        event.kind == WhiteboardKind::ReviewState && event.plan_id.as_deref() == Some(plan_id)
    }) {
        // Layer 1 — content addressing: rebuild the canonical fields from the
        // stored row and recompute the hash. A mutated payload (stale copy,
        // manual edit, partial write surfaced by another bug) fails here and
        // is dropped instead of driving a wrong resume.
        let rebuilt = NewWhiteboardEvent {
            event_id: event.event_id.clone(),
            agent_id: event.agent_id.clone(),
            kind: event.kind,
            scope: event.scope.clone(),
            session_id: event.session_id.clone(),
            plan_id: event.plan_id.clone(),
            causation: event.causation.clone(),
            payload: event.payload.clone(),
            pre_image_hash: event.pre_image_hash.clone(),
            created_at: event.created_at,
        };
        let hash_verified =
            matches!(compute_content_hash(&rebuilt), Ok(hash) if hash == event.content_hash);
        if !hash_verified {
            tracing::warn!(
                event_id = %event.event_id,
                "review-state row failed its content-hash verification; ignoring it \
                 (ADR-60 Deferred 3)"
            );
            continue;
        }
        // Layer 2 — structure: deserialize and check writer invariants.
        let payload: ReviewStatePayload = match serde_json::from_value(event.payload.clone()) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    event_id = %event.event_id,
                    %error,
                    "review-state row carries an unreadable payload; ignoring it"
                );
                continue;
            }
        };
        let spent_cycles = u32::try_from(payload.feedback_ledger.len()).unwrap_or(u32::MAX);
        if !payload.is_internally_consistent(target_hash, event.gate_seq, spent_cycles) {
            tracing::warn!(
                event_id = %event.event_id,
                "review-state row is structurally inconsistent with its cycle history; \
                 ignoring it (ADR-60 Deferred 3)"
            );
            continue;
        }
        valid.push((event.gate_seq, event.session_id.clone(), payload));
    }

    // Newest surviving snapshot wins (gate_seq order from the loader).
    let Some((row_gate_seq, row_session, latest)) = valid.last() else {
        return Ok(ReviewResume::Fresh);
    };

    if latest.status.is_terminal() {
        if row_session.as_deref() == Some(current_session_id) {
            tracing::debug!(
                plan_id = %plan_id,
                "terminal review-state snapshot belongs to this session (sibling subtask \
                 with an identical target); starting a fresh review"
            );
            return Ok(ReviewResume::Fresh);
        }
        return Ok(ReviewResume::Resolved {
            status: latest.status,
            feedback_ledger: latest.feedback_ledger.clone(),
        });
    }

    let resume_cycle = match u32::try_from(latest.feedback_ledger.len() + 1) {
        Ok(cycle) => cycle,
        Err(_) => return Ok(ReviewResume::Fresh),
    };
    Ok(ReviewResume::Resume {
        resume_cycle,
        retry_count: latest.retry_count,
        feedback_ledger: latest.feedback_ledger.clone(),
        from_gate_seq: *row_gate_seq,
    })
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

    // ------------------------------------------------------------------
    // ADR-60 D7 (#152): whiteboard plan binding — write + verified read.
    // ------------------------------------------------------------------

    /// File-backed pool with production PRAGMAs and every session migration
    /// applied (file-backed so the append path's BEGIN IMMEDIATE locking is
    /// exercised for real, mirroring whiteboard.rs's own test helper).
    async fn d7_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        use sqlx::pool::PoolOptions;
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("plan_approval_test.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn design_doc() -> DesignDoc {
        DesignDoc {
            goals: vec!["add plan continuity".to_owned()],
            constraints: vec!["no new dependencies".to_owned()],
            proposed_files: vec![camino::Utf8PathBuf::from("src/continuity.rs")],
            interface_sketch: "load_approved_plan(pool, binding)".to_owned(),
            risks: vec!["payload tampering".to_owned()],
        }
    }

    fn approved_binding() -> PlanBinding {
        PlanBinding::new(
            "plan-d7".into(),
            objective_hash(),
            Some("abc1234".into()),
            "step 1: build continuity\nstep 2: verify it".to_owned(),
        )
    }

    #[tokio::test]
    async fn plan_approved_event_round_trips_with_design_doc() {
        let (_dir, pool) = d7_pool().await;
        let binding = approved_binding();
        let doc = design_doc();

        let stored = append_plan_approved_event(&pool, Ulid::new(), &binding, Some(&doc))
            .await
            .expect("append approval");
        assert_eq!(stored.kind, WhiteboardKind::PlanApproved);
        assert_eq!(stored.plan_id.as_deref(), Some(binding.plan_id()));

        // The verified read returns the structured doc and an empty ledger
        // (nothing executed under this plan yet — truthful carry-forward).
        let ctx = load_approved_plan(&pool, &binding).await.expect("verified load");
        let ctx = ctx.expect("approval exists");
        assert_eq!(ctx.binding, binding);
        assert_eq!(
            serde_json::to_value(&ctx.design_doc).expect("doc serializes"),
            serde_json::to_value(Some(&doc)).expect("expected doc serializes"),
            "the structured DesignDoc survives the whiteboard round trip"
        );
        assert_eq!(ctx.ledger, PlanLedger::default());
    }

    #[tokio::test]
    async fn plan_without_whiteboard_events_degrades_to_none() {
        let (_dir, pool) = d7_pool().await;
        // A pre-D7 binding (approved before this slice) has no events.
        let loaded = load_approved_plan(&pool, &approved_binding()).await.expect("load");
        assert!(loaded.is_none(), "missing state is reported as None, never invented");
    }

    #[tokio::test]
    async fn injected_second_approval_loud_fails() {
        // Oracle comment 3: a plan change injected AFTER `plan-approved` but
        // BEFORE Execute must loud-fail without re-approval. Simulate it by
        // appending a second approval with different content under the SAME
        // plan id.
        let (_dir, pool) = d7_pool().await;
        let binding = approved_binding();

        // First (legitimate) approval carries the structured doc...
        let doc = design_doc();
        append_plan_approved_event(&pool, Ulid::new(), &binding, Some(&doc))
            .await
            .expect("first approval");

        // ...then a second approval of DIFFERENT content under the same id.
        let injected = PlanBinding::new(
            binding.plan_id().to_owned(),
            binding.objective_hash().to_owned(),
            None,
            "step 1: build continuity\nstep 2: DELETE EVERYTHING".to_owned(),
        );
        append_plan_approved_event(&pool, Ulid::new(), &injected, None)
            .await
            .expect("injected approval");

        let error = load_approved_plan(&pool, &binding).await.expect_err("must loud-fail");
        assert!(
            error.contains("re-approval"),
            "divergence names the required re-approval: {error}"
        );
    }

    #[tokio::test]
    async fn binding_hash_mismatch_loud_fails() {
        // The whiteboard artifact and the durable binding disagree — e.g. the
        // binding row was rewritten after approval. Never re-decompose.
        let (_dir, pool) = d7_pool().await;
        let binding = approved_binding();
        append_plan_approved_event(&pool, Ulid::new(), &binding, None).await.expect("approval");

        let other = PlanBinding::new(
            "plan-d7".into(),
            objective_hash(),
            None,
            "an entirely different plan".to_owned(),
        );
        let error = load_approved_plan(&pool, &other).await.expect_err("must loud-fail");
        assert!(error.contains("artifact hash"), "names the hash mismatch: {error}");
    }

    #[tokio::test]
    async fn tampered_payload_text_loud_fails() {
        // Content addressing: payload text must hash to its declared
        // artifact_hash. Forged through a raw event with inconsistent fields.
        let (_dir, pool) = d7_pool().await;
        let binding = approved_binding();
        let forged_payload = serde_json::json!({
            "plan_id": binding.plan_id(),
            "objective_hash": binding.objective_hash(),
            "artifact_hash": plan_artifact_hash(binding.plan_text()),
            "source_revision": null,
            "design_doc": null,
            "plan_text": "totally different text than what was hashed",
            "created_at_ms": 1,
        });
        concerto_sessions::whiteboard::append_whiteboard_event(
            &pool,
            &concerto_sessions::whiteboard::NewWhiteboardEvent {
                event_id: "forged-1".into(),
                agent_id: "attacker".into(),
                kind: WhiteboardKind::PlanApproved,
                scope: String::new(),
                session_id: None,
                plan_id: Some(binding.plan_id().to_owned()),
                causation: None,
                payload: forged_payload,
                pre_image_hash: None,
                created_at: 1,
            },
        )
        .await
        .expect("forged row stored");

        let error = load_approved_plan(&pool, &binding).await.expect_err("must loud-fail");
        assert!(error.contains("artifact_hash"), "content-addressing failure is named: {error}");
    }

    #[tokio::test]
    async fn ledger_folds_completed_files_and_failures() {
        use concerto_sessions::whiteboard::WhiteboardKind as Kind;
        let (_dir, pool) = d7_pool().await;
        let binding = approved_binding();
        append_plan_approved_event(&pool, Ulid::new(), &binding, None).await.expect("approval");

        // Ledger events from earlier supervised work under the SAME plan id:
        // one completed subtask, two gated file writes, one failed tool.
        async fn ledger_event(
            pool: &sqlx::SqlitePool,
            event_id: &str,
            kind: Kind,
            payload: serde_json::Value,
        ) {
            concerto_sessions::whiteboard::append_whiteboard_event(
                pool,
                &concerto_sessions::whiteboard::NewWhiteboardEvent {
                    event_id: event_id.to_owned(),
                    agent_id: "agent-a".into(),
                    kind,
                    scope: String::new(),
                    session_id: None,
                    plan_id: Some("plan-d7".to_owned()),
                    causation: None,
                    payload,
                    pre_image_hash: None,
                    created_at: 2,
                },
            )
            .await
            .expect("ledger event stored");
        }
        ledger_event(
            &pool,
            "done-1",
            Kind::SubtaskCompleted,
            serde_json::json!({ "task_id": "01HQ", "status": "completed" }),
        )
        .await;
        ledger_event(
            &pool,
            "write-1",
            Kind::WriteApplied,
            serde_json::json!({
                "tool": "filesystem.write",
                "input": { "path": "src/lib.rs" },
                "pre_images": { "src/lib.rs": "hash-a", "src/main.rs": "hash-b" },
            }),
        )
        .await;
        ledger_event(
            &pool,
            "fail-1",
            Kind::Failure,
            serde_json::json!({ "tool": "shell", "error": "cargo build exited 101" }),
        )
        .await;

        // A stranger binding under the same objective hash must NOT verify
        // against plan-d7's artifact (different text → different hash).
        let ctx = load_approved_plan(&pool, &binding).await.expect("load").expect("approval");
        assert_eq!(
            ctx.ledger.completed_subtasks,
            vec!["01HQ".to_owned()],
            "completed subtask ids fold into the ledger"
        );
        assert_eq!(
            ctx.ledger.files_touched.len(),
            2,
            "both gated write targets are recorded: {:?}",
            ctx.ledger.files_touched
        );
        assert!(
            ctx.ledger.files_touched.contains(&"src/lib.rs".to_owned())
                && ctx.ledger.files_touched.contains(&"src/main.rs".to_owned()),
            "files touched come from the write attribution record"
        );
        assert_eq!(
            ctx.ledger.failed_commands,
            vec!["shell: cargo build exited 101".to_owned()],
            "failed commands carry their failure reasons"
        );

        // Unrelated plans never leak into this ledger.
        let stranger =
            PlanBinding::new("plan-other".into(), objective_hash(), None, "x".to_owned());
        let other = load_approved_plan(&pool, &stranger).await.expect("load");
        assert!(other.is_none(), "a different plan id sees nothing");
    }

    // ------------------------------------------------------------------
    // ADR-60 Deferred 3: run_review_cycle resumability.
    // ------------------------------------------------------------------

    const REVIEW_TARGET: &str = "implement the login feature";

    fn review_payload(
        status: ReviewCycleStatus,
        session: &str,
        ledger: Vec<ReviewFeedbackEntry>,
    ) -> ReviewStatePayload {
        ReviewStatePayload {
            plan_id: "plan-d7".into(),
            session_id: session.into(),
            implement_role: "coder".into(),
            review_target: REVIEW_TARGET.into(),
            review_target_hash: review_target_identity("coder", REVIEW_TARGET),
            status,
            max_cycles: 3,
            retry_count: u32::try_from(ledger.len()).unwrap_or(u32::MAX),
            feedback_ledger: ledger,
            gate_seq_cursor: 0,
            created_at_ms: 1,
        }
    }

    fn revision_entry(cycle: u32, reason: &str) -> ReviewFeedbackEntry {
        ReviewFeedbackEntry {
            cycle_num: cycle,
            verdict: "needs-revision".to_owned(),
            reason: Some(reason.to_owned()),
        }
    }

    async fn append_review_snapshot(pool: &sqlx::SqlitePool, payload: &ReviewStatePayload) {
        append_review_state_event(pool, payload).await.expect("review snapshot stored");
    }

    #[tokio::test]
    async fn crash_between_start_and_verdict_resumes_not_duplicates() {
        // The Phase 3 acceptance shape: a snapshot commits (WAL-before-
        // invoke), the reviewer's verdict is lost to a crash, and a restart
        // must continue from the durable position instead of re-running the
        // settled cycles.
        let (_dir, pool) = d7_pool().await;
        let crashed_session = Ulid::new().to_string();

        // Attempt 1: cycle 1 STARTED, then the process died before any
        // verdict landed — exactly one open snapshot exists.
        append_review_snapshot(
            &pool,
            &review_payload(ReviewCycleStatus::Started, &crashed_session, Vec::new()),
        )
        .await;
        let resumed = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "fresh-session",
        )
        .await
        .expect("load");
        match resumed {
            ReviewResume::Resume { resume_cycle, retry_count, feedback_ledger, .. } => {
                assert_eq!(resume_cycle, 1, "no verdicts were recorded");
                assert_eq!(retry_count, 0);
                assert!(feedback_ledger.is_empty());
            }
            other => panic!("expected Resume, got {other:?}"),
        }

        // Attempt 1 (earlier cycles): revision verdict + queued revision +
        // cycle-2 start all committed before the second crash. The restart
        // continues at cycle 2 with the FULL ledger — it does not restart at
        // cycle 1 and re-ask an already-answered reviewer question.
        append_review_snapshot(
            &pool,
            &review_payload(
                ReviewCycleStatus::RevisionQueued,
                &crashed_session,
                vec![revision_entry(1, "missing error path")],
            ),
        )
        .await;
        append_review_snapshot(
            &pool,
            &review_payload(
                ReviewCycleStatus::Started,
                &crashed_session,
                vec![revision_entry(1, "missing error path")],
            ),
        )
        .await;
        let resumed = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "fresh-session",
        )
        .await
        .expect("load");
        match resumed {
            ReviewResume::Resume { resume_cycle, retry_count, feedback_ledger, .. } => {
                assert_eq!(resume_cycle, 2, "cycle 1 was fully settled pre-crash");
                assert_eq!(retry_count, 1);
                assert_eq!(
                    feedback_ledger,
                    vec![revision_entry(1, "missing error path")],
                    "the full feedback ledger survives the crash"
                );
            }
            other => panic!("expected Resume, got {other:?}"),
        }

        // A DIFFERENT target under the same plan never inherits this state.
        let other = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", "implement the logout feature"),
            "fresh-session",
        )
        .await
        .expect("load");
        assert!(matches!(other, ReviewResume::Fresh), "targets are isolated: {other:?}");
    }

    #[tokio::test]
    async fn completed_review_suppresses_duplicate_cross_session_only() {
        let (_dir, pool) = d7_pool().await;
        let previous_session = Ulid::new().to_string();

        // A previous attempt passed the review; the completion event is
        // durable even though the process died right after (oracle comment
        // 3: check for completion BEFORE resuming).
        append_review_snapshot(
            &pool,
            &review_payload(
                ReviewCycleStatus::RevisionQueued,
                &previous_session,
                vec![revision_entry(1, "off-by-one")],
            ),
        )
        .await;
        append_review_snapshot(
            &pool,
            &review_payload(ReviewCycleStatus::Completed, &previous_session, Vec::new()),
        )
        .await;

        // The RESTARTED attempt (different session) must NOT run a second
        // review for the same target — the recorded pass stands.
        let resolved = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "restarted-session",
        )
        .await
        .expect("load");
        assert!(
            matches!(resolved, ReviewResume::Resolved { .. }),
            "a settled review suppresses the duplicate: {resolved:?}"
        );

        // Within the SAME live session a terminal snapshot belongs to a
        // sibling subtask with an identical description, which still gets
        // its own fresh review.
        let sibling = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            &previous_session,
        )
        .await
        .expect("load");
        assert!(
            matches!(sibling, ReviewResume::Fresh),
            "same-session terminals never suppress a live sibling: {sibling:?}"
        );
    }

    #[tokio::test]
    async fn escalated_round_trips_as_resolved() {
        let (_dir, pool) = d7_pool().await;
        let session = Ulid::new().to_string();
        append_review_snapshot(
            &pool,
            &review_payload(
                ReviewCycleStatus::Escalated,
                &session,
                vec![
                    revision_entry(1, "still wrong"),
                    revision_entry(2, "worse"),
                    revision_entry(3, "hopeless"),
                ],
            ),
        )
        .await;
        let resolved = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "next-session",
        )
        .await
        .expect("load");
        match resolved {
            ReviewResume::Resolved { status, feedback_ledger } => {
                assert_eq!(status, ReviewCycleStatus::Escalated);
                assert_eq!(feedback_ledger.len(), 3, "every spent cycle is in the ledger");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tampered_and_inconsistent_rows_are_rejected_not_trusted() {
        // Oracle comment 1: stale/corrupt injection must not drive a resume.
        let (_dir, pool) = d7_pool().await;

        // Layer 1 — a row whose payload is mutated AFTER commit no longer
        // hashes to its stored content_hash and is ignored.
        append_review_snapshot(
            &pool,
            &review_payload(ReviewCycleStatus::Completed, "crashed-session", Vec::new()),
        )
        .await;
        let mut tampered = serde_json::to_value(review_payload(
            ReviewCycleStatus::Completed,
            "crashed-session",
            Vec::new(),
        ))
        .expect("payload serializes");
        tampered["status"] = serde_json::json!("completed-forged");
        sqlx::query("UPDATE whiteboard_events SET payload = ? WHERE kind = 'review-state'")
            .bind(tampered.to_string())
            .execute(&pool)
            .await
            .expect("tamper applied");

        // Layer 2 — a well-hashed row whose counters contradict its ledger
        // (a buggy writer) is rejected by structural validation.
        let mut inconsistent = review_payload(
            ReviewCycleStatus::Started,
            "crashed-session",
            vec![revision_entry(1, "reason")],
        );
        inconsistent.retry_count = 5; // ledger.len() == 1
        append_review_snapshot(&pool, &inconsistent).await;

        let resumed = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "restarted-session",
        )
        .await
        .expect("load");
        assert!(
            matches!(resumed, ReviewResume::Fresh),
            "corrupt rows are rejected wholesale, never trusted for a resume: {resumed:?}"
        );
    }

    #[tokio::test]
    async fn escalated_with_unspent_cycles_is_rejected() {
        // An Escalated claim that did not spend its cap contradicts every
        // writer here — reject rather than settle on it.
        let (_dir, pool) = d7_pool().await;
        append_review_snapshot(
            &pool,
            &review_payload(
                ReviewCycleStatus::Escalated,
                "crashed-session",
                vec![revision_entry(1, "only one of three cycles spent")],
            ),
        )
        .await;
        let resumed = load_review_resume(
            &pool,
            "plan-d7",
            &review_target_identity("coder", REVIEW_TARGET),
            "restarted-session",
        )
        .await
        .expect("load");
        assert!(matches!(resumed, ReviewResume::Fresh), "{resumed:?}");
    }

    #[tokio::test]
    async fn review_target_identity_is_deterministic_and_discriminating() {
        assert_eq!(
            review_target_identity("coder", REVIEW_TARGET),
            review_target_identity("coder", REVIEW_TARGET),
            "identity is stable across restarts"
        );
        assert_ne!(
            review_target_identity("coder", REVIEW_TARGET),
            review_target_identity("reviewer", REVIEW_TARGET),
            "role participates in the identity"
        );
        assert_ne!(
            review_target_identity("coder", REVIEW_TARGET),
            review_target_identity("coder", "another target"),
            "description participates in the identity"
        );
    }
}
