//! Write gate — ADR-60 D4 vertical slice (S2): the single
//! policy-enforcement and durability chokepoint for agent tool writes.
//!
//! ## Ordering invariant (WAL-before-execute)
//!
//! Every [`WriteGate::submit`] request is evaluated once, durably recorded on
//! the whiteboard, and only then executed:
//!
//! 1. **Replay check** — a `call_id` that already has a terminal decision
//!    (`write-applied` or `write-rejected`) never re-evaluates or re-executes
//!    (set-once semantics, ADR-60 D4 "dedup by `event_id`"). A
//!    `write-applied` replay returns the stored outcome with
//!    `replayed: true`.
//! 2. **Policy** — `Allow` proceeds; `Deny` / `RequireApproval` / any other
//!    verdict appends `write-rejected` (event_id = `call_id`) and returns
//!    [`GateError::Denied`].
//! 3. **Pre-image** — for `filesystem` write/delete/move/copy requests the
//!    target's current bytes are hashed (blake3) before any terminal decision,
//!    so both `write-applied` and `write-rejected` events carry the pre-image
//!    when computable, letting later attribution prove which bytes the agent
//!    changed (ADR-60 D5).
//! 4. **WAL append** — `write-applied` is committed **before** the tool runs;
//!    a crash after this point replays as `replayed: true`, never re-executed.
//! 5. **Execute** — results return; execution errors log a `failure` event
//!    (causation = `call_id`) and propagate, unless the attempt was cancelled
//!    (cancellation leaves no `failure` row — the `write-applied` WAL entry is
//!    not a stray event, it is the durability contract).
//!
//! ## One policy engine
//!
//! The gate shares the *same* [`PolicyEngine`] instance the [`ToolExecutor`]
//! executes under. It is not a second, parallel policy decision — there is
//! exactly one enforcement point, so the audit trail and the gate's verdict
//! cannot diverge.
//!
//! ## Concurrency
//!
//! - **Replay-race claim**: an in-process per-`call_id` claim set
//!   (`Mutex<HashMap<call_id, Arc<Mutex<()>>>>`) serializes concurrent
//!   retries of the same id. The first touch inserts the claim lock and
//!   awaits it — a `tokio` mutex, so a waiter can never lose a wakeup, and
//!   cancellation is race-free via `select!`. The winner holds the lock for
//!   the whole gated op through an RAII guard whose `Drop` removes the map
//!   entry identity-checked (`Arc::ptr_eq`), so a re-inserted claim for the
//!   same id is never clobbered. Losers wake to re-check the durable log
//!   and replay instead of re-executing.
//! - **Per-agent limiter**: every agent has its own FIFO [`Semaphore`]
//!   (default cap 1, configurable via [`WriteGate::new`]), acquired after
//!   the claim; agents do not block one another.
//! - **Cancellation** is honored at every await point (claim lock, limiter,
//!   policy evaluation), surfacing as [`GateError::Cancelled`] without
//!   appending a `failure` row.
//!
//! ## Shared-file conflicts (ADR-60 D5)
//!
//! - **Optimistic `base_version` checks**: a request declaring the hash of a
//!   versioned target's state is refused with [`GateError::Conflict`] — no
//!   whiteboard row — when the target's current pre-image hash differs.
//! - **Hunk-aware staging**: when the declared base diverged but this write
//!   touches *different lines* than the sibling's intervening change, the
//!   write's hunks are spliced onto the current content
//!   ([`crate::hunk::stage_three_way`]) instead of refusing, so disjoint
//!   concurrent edits both survive. Same-hunk collisions surface loudly for
//!   manual resolution; anything undiffable (CRLF/binary/unobserved base)
//!   falls back to the file-level check — never to silent loss.
//! - **Explicit lock tokens** reserve hot shared files for exclusive write:
//!   [`WriteGate::acquire_lock`] / [`WriteGate::release_lock`] manage an
//!   in-memory TTL table, checked inside [`WriteGate::submit`] before the
//!   conflict logic — a foreign-held lock is a loud [`GateError::Conflict`]
//!   carrying holder info, never a silent race or drop.
//!
//! ## Deferred to S4 (ADR-60)
//!
//! - Weighted round-robin fairness across agents (D4).
//! - Interactive approval surfacing — the gate returns [`GateError::Denied`]
//!   for approval-required verdicts today.
//! - Supervisor wiring — the runtime does not yet call [`WriteGate::submit`]
//!   for every agent write; the gate is exercised by its unit tests.

use crate::hunk::{stage_three_way, HunkStaging};
use concerto_core::error::{PolicyError, ToolError};
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::{new_id, Ulid};
use concerto_core::traits::policy::PolicyEngine;
use concerto_core::types::{
    CapabilitySet, PolicyAction, PolicyVerdict, SessionContext, ToolDefinition,
};
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::{
    append_whiteboard_event, latest_gate_seq, load_whiteboard_events_up_to, NewWhiteboardEvent,
    WhiteboardKind,
};
use concerto_sessions::SessionError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Semaphore;

/// Per-agent pre-image source for write attribution (ADR-60 D5).
///
/// The gate calls [`PreImageReader::read`] for the target path of a
/// `filesystem` write/delete/move/copy *before* the `write-applied` event is
/// appended, and persists the blake3 hash of the returned bytes on the event.
#[async_trait::async_trait]
pub trait PreImageReader: Send + Sync {
    /// Return the raw bytes of `relative_path` under the project root, or
    /// `None` when the file does not exist.
    async fn read(&self, relative_path: &Path) -> std::io::Result<Option<Vec<u8>>>;
}

/// Default [`PreImageReader`] that reads files under a project root via
/// `tokio::fs`.
pub struct FilePreImageReader {
    project_root: PathBuf,
}

impl FilePreImageReader {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self { project_root: project_root.into() }
    }
}

#[async_trait::async_trait]
impl PreImageReader for FilePreImageReader {
    async fn read(&self, relative_path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        let absolute = self.project_root.join(relative_path);
        match tokio::fs::read(&absolute).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// One write request submitted to the gate by an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateRequest {
    /// Caller-generated idempotency key (uuid v4 per ADR-60 D3); re-submitting
    /// an existing `call_id` replays the stored decision without re-executing.
    pub call_id: String,
    /// Agent producing the write (whiteboard attribution).
    pub agent_id: String,
    /// Tool name to execute, e.g. `filesystem`.
    pub tool: String,
    /// Tool input (policy-evaluated and passed through to the tool).
    pub input: serde_json::Value,
    /// Owning session, when known.
    pub session_id: Option<String>,
    /// Subscription/topic filter.
    pub scope: String,
    /// Structured-state key for future #152 reads (ADR-60 D7); not a FK.
    pub plan_id: Option<String>,
    /// Trigger `event_id` / HLC that caused this write (optional).
    pub causation: Option<String>,
    /// ADR-60 D5 optimistic concurrency: per-relative-target blake3 hex hashes
    /// the caller believes it is editing (its "base versions"), keyed by the
    /// same relative paths [`versioned_targets`] produces. When a versioned
    /// (mutated) target carries a declared claim here, the gate refuses with
    /// [`GateError::Conflict`] — and appends nothing to the whiteboard — if
    /// the target's current pre-image hash differs, so a sibling agent's
    /// intervening write to that target is never silently overwritten. An
    /// absent target carries no claim (a fresh create has no prior version to
    /// conflict with → documented last-writer-wins), and a target without a
    /// declared claim is not conflict-checked. Declared claims on read-only
    /// targets (the `copy` source, see [`versioned_targets`]) are ignored.
    /// Empty on the wire for clients that predate the field (deserializes to
    /// an empty map via `#[serde(default)]`).
    #[serde(default)]
    pub base_versions: BTreeMap<String, String>,
}

/// Outcome of a gated write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// The `call_id` (== whiteboard `event_id` for the applied event).
    pub event_id: String,
    /// Global total order assigned by the whiteboard log.
    pub gate_seq: u64,
    /// `true` when the request was a replay of an already-applied write
    /// (dedup by `event_id`); the tool was NOT re-executed.
    pub replayed: bool,
    /// Tool output payload for fresh executions; a `{ "replayed": true }`
    /// marker for replays.
    pub result: serde_json::Value,
}

/// Errors from the write gate.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum GateError {
    /// Policy produced a non-`Allow` verdict (deny or approval required). A
    /// `write-rejected` event for the `call_id` was committed first.
    #[error("write {event_id} denied by policy: {reason}")]
    Denied {
        /// The rejected `call_id`.
        event_id: String,
        /// Human-readable verdict/reason for the rejection.
        reason: String,
    },
    /// ADR-60 D5 optimistic concurrency violation: the request declared a
    /// `base_versions` claim for a versioned (mutated) target and the
    /// target's current pre-image hash does not match (a sibling agent
    /// changed the file first, or the target no longer exists). Nothing is
    /// appended to the whiteboard — the write is surfaced loudly to the
    /// caller/supervisor for a manual-resolution path, never silently
    /// dropped.
    #[error("optimistic conflict on {event_id}: {reason}")]
    Conflict {
        /// The rejected `call_id`.
        event_id: String,
        /// Human-readable mismatch detail.
        reason: String,
    },
    /// ADR-60 D5 explicit lock tokens: an acquire/release attempt on a path
    /// whose exclusive-write reservation belongs to a different agent or
    /// token. Only the lock-management API returns this; a *write* submitted
    /// against a foreign-held lock surfaces as [`GateError::Conflict`] with
    /// holder info instead.
    #[error("path {path} is locked for exclusive write by agent '{holder}'")]
    Locked {
        /// The contended relative target path.
        path: String,
        /// The agent currently holding (or owning) the reservation.
        holder: String,
    },
    /// Policy evaluation itself failed.
    #[error("policy evaluation failed: {0}")]
    Policy(String),
    /// Whiteboard write/read failed.
    #[error("whiteboard log error: {0}")]
    Whiteboard(String),
    /// Tool execution failed (after the WAL append committed); a `failure`
    /// event was logged with causation = `call_id`.
    #[error("tool execution failed: {0}")]
    Execution(String),
    /// Pre-image read for ADR-60 D5 attribution failed.
    #[error("pre-image capture failed: {0}")]
    PreImage(String),
    /// The request was cancelled before completion. If the `write-applied`
    /// WAL event had already committed, it remains (replay-safe); no
    /// `failure` event is logged for a cancellation.
    #[error("gate cancelled")]
    Cancelled,
    /// The request carried an invalid field.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl From<PolicyError> for GateError {
    fn from(error: PolicyError) -> Self {
        GateError::Policy(error.to_string())
    }
}

impl From<ToolError> for GateError {
    fn from(error: ToolError) -> Self {
        GateError::Execution(error.to_string())
    }
}

impl From<SessionError> for GateError {
    fn from(error: SessionError) -> Self {
        GateError::Whiteboard(error.to_string())
    }
}

/// Per-`call_id` in-flight claim lock. `tokio`'s mutex so waiting is
/// cancellation-safe via `select!` and a waiting locker can never lose a
/// wakeup (unlike a `Notify` handshake, which has a register-vs-notify race).
type ClaimLock = tokio::sync::Mutex<()>;

/// The in-process replay-race claim set: `call_id` → claim lock.
type ClaimSet = HashMap<String, Arc<ClaimLock>>;

/// RAII release for a claimed `call_id`: keeps the claim lock held for the
/// whole gated op, then — on drop, success and error alike — removes the map
/// entry and releases the lock, letting the next queued submitter of the
/// same `call_id` re-check the durable log and replay. The entry is removed
/// identity-checked so a re-inserted claim for the same id (which can only
/// appear after this guard's entry was already gone) is never clobbered.
struct InFlightClaim {
    call_id: String,
    claim: Arc<ClaimLock>,
    /// The owned claim lock; released when this struct drops (after the map
    /// entry removal in [`Drop::drop`]).
    _lock: tokio::sync::OwnedMutexGuard<()>,
    in_flight: Arc<Mutex<ClaimSet>>,
}

impl Drop for InFlightClaim {
    fn drop(&mut self) {
        if let Ok(mut map) = self.in_flight.lock() {
            if map.get(&self.call_id).is_some_and(|current| Arc::ptr_eq(current, &self.claim)) {
                map.remove(&self.call_id);
            }
        }
    }
}

/// A terminal whiteboard decision for a `call_id`.
enum StoredDecision {
    /// `write-applied` — replay returns the recorded outcome.
    Applied(u64),
    /// `write-rejected` — the request was denied earlier; set-once.
    Rejected,
}

/// Handle for an exclusive-write reservation on one path (ADR-60 D5 explicit
/// lock tokens, v1: hot shared files). Returned by
/// [`WriteGate::acquire_lock`]; pass it to [`WriteGate::release_lock`] to
/// free the path early. Reservations lapse lazily at `expires_at_ms`
/// (purged on the next acquire/submit/release touching the table) — there is
/// no background sweeper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockToken {
    /// Unguessable id proving ownership; a release with a stale or forged id
    /// is refused.
    pub token_id: String,
    /// The reserved relative target path (the [`versioned_targets`] form).
    pub path: String,
    /// The reserving agent; re-acquisition by the same agent refreshes.
    pub agent_id: String,
    /// Unix epoch millis after which the reservation lapses.
    pub expires_at_ms: i64,
}

/// The live reservation behind a [`LockToken`] (private mirror).
struct ActiveLock {
    token_id: String,
    agent_id: String,
    expires_at_ms: i64,
}

/// Exclusive-write reservations keyed by relative target path (ADR-60 D5).
/// In-memory by design for v1 — locks are advisory coordination between the
/// 3–6 agents of one supervised run sharing this gate process, not crash
/// state; the whiteboard remains the durable authority.
type LockTable = HashMap<String, ActiveLock>;

/// Upper bounds of the pre-image text cache (see [`PreImageTextCache`]).
const PRE_IMAGE_CACHE_MAX_FILES: usize = 64;
const PRE_IMAGE_CACHE_MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const PRE_IMAGE_CACHE_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

/// Bounded in-memory store of observed pre-image bytes, keyed by their blake3
/// hex — the recovery mechanism hunk-aware staging needs.
///
/// Every gated submit already reads each attributed path's bytes for the
/// pre-image hash (ADR-60 D5 attribution); caching them here lets the gate
/// later reconstruct "the base text an agent declared" from its hash alone,
/// without widening the wire format (`base_versions` carries hashes only).
/// A claimed base the gate never observed (e.g. a file that predates this
/// gate process and was never written through it) is simply absent, and
/// staging falls back to the file-level conflict check.
///
/// Content-addressed and FIFO-evicted under three caps (file count, total
/// bytes, single-entry bytes); oversized files are never cached, so writes to
/// them degrade gracefully to file-level checks.
#[derive(Default)]
struct PreImageTextCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    total_bytes: usize,
}

impl PreImageTextCache {
    fn insert(&mut self, hash: String, bytes: Arc<Vec<u8>>) {
        if bytes.len() > PRE_IMAGE_CACHE_MAX_ENTRY_BYTES || self.entries.contains_key(&hash) {
            return;
        }
        self.total_bytes += bytes.len();
        self.order.push_back(hash.clone());
        self.entries.insert(hash, bytes);
        while self.total_bytes > PRE_IMAGE_CACHE_MAX_TOTAL_BYTES
            || self.entries.len() > PRE_IMAGE_CACHE_MAX_FILES
        {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn get(&self, hash: &str) -> Option<Arc<Vec<u8>>> {
        self.entries.get(hash).cloned()
    }
}

/// Policy-gated, whiteboard-logged tool execution (ADR-60 D4).
pub struct WriteGate {
    policy: Arc<dyn PolicyEngine>,
    executor: Arc<ToolExecutor>,
    log_pool: sqlx::SqlitePool,
    pre_image: Arc<dyn PreImageReader>,
    /// Session project dir handed to the executor (ADR-60 D4 session context).
    project_root: PathBuf,
    /// Concurrent write cap per agent (ADR-60 D4 limiter); default 1.
    max_in_flight_per_agent: usize,
    /// Per-agent FIFO semaphores, created on first use and left in place
    /// (bounded by roster size).
    limits: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// In-process replay-race claim set: `call_id` → claim lock.
    in_flight: Arc<Mutex<ClaimSet>>,
    /// Exclusive-write reservations for hot shared files (ADR-60 D5 lock
    /// tokens); lazily purged of expired entries.
    locks: Arc<Mutex<LockTable>>,
    /// Observed pre-image bytes keyed by blake3 hex — lets hunk staging
    /// recover a claimed base text without a wire format change.
    text_cache: Mutex<PreImageTextCache>,
}

/// ADR-60 D5: the relative paths a request mutates, in a canonical order, or
/// empty when the request does not target a file under the project root
/// (non-`filesystem` tool, non-write operation, or missing path field).
///
/// This is the SINGLE source of truth for "what a filesystem op mutates":
/// pre-image capture, the base_version conflict check, and the supervisor's
/// always-on stamp all key off it, so they can never drift.
///
/// READ-ONLY vs MUTATED distinction: every entry here is mutated and fully
/// versioned — attributed (pre-image captured for the WAL row) AND
/// conflict-checked AND claim-stamped. The one exception is the `copy`
/// SOURCE (the `path` field): a copy *reads* the source to produce the
/// destination, so a concurrent write to the source cannot be lost by the
/// copy (the copy reads whatever is there). The copy source is therefore NOT
/// a versioned target: it is attributed (its pre-image is recorded for the
/// audit trail) but never conflict-checked and never claim-stamped. A
/// `move`'s source IS mutated (the file is removed), so it is a versioned
/// target on equal footing with the destination.
fn versioned_targets(req: &GateRequest) -> Vec<&str> {
    if req.tool != "filesystem" {
        return Vec::new();
    }
    let Some(operation) = req.input.get("operation").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let path = req.input.get("path").and_then(serde_json::Value::as_str);
    let destination = req.input.get("destination").and_then(serde_json::Value::as_str);
    match operation {
        "write" | "delete" => path.into_iter().collect(),
        // A move removes the source and writes the destination: both are
        // mutated and both must be versioned (see module docs for why).
        "move" => {
            let mut targets = Vec::with_capacity(2);
            if let Some(source) = path {
                targets.push(source);
            }
            if let Some(destination) = destination {
                targets.push(destination);
            }
            targets
        }
        // A copy writes only the destination; the source is read-only (see
        // the doc comment above) and is handled separately by
        // [`attributed_paths`].
        "copy" => destination.into_iter().collect(),
        _ => Vec::new(),
    }
}

/// INTERNAL/legacy: the PRIMARY versioned target of a request — the path
/// whose pre-image the whiteboard `pre_image_hash` column records.
///
/// Retained only to keep the single-target-era `pre_image_hash` column
/// semantics stable. `versioned_targets` orders write/delete as `[path]`,
/// move as `[source, destination]`, and copy as `[destination]`, so the last
/// entry is exactly the primary target of the single-target era (write/delete
/// → `path`, move/copy → `destination`). New code must use
/// [`versioned_targets`] — the multi-target source of truth — never this
/// legacy projection. `None` for non-versioned requests.
fn versioned_target(req: &GateRequest) -> Option<&str> {
    versioned_targets(req).last().copied()
}

/// Every path whose pre-image must be captured and persisted for attribution:
/// each [`versioned_targets`] entry plus the read-only `copy` source (the
/// copy reads the source to produce the destination; its pre-image is
/// recorded on the WAL row for the audit trail even though it is not a
/// versioned/conflict-checked target). Paths that do not exist simply carry
/// no hash in the capture map.
fn attributed_paths(req: &GateRequest) -> Vec<&str> {
    let mut paths = versioned_targets(req);
    if req.tool == "filesystem"
        && req.input.get("operation").and_then(serde_json::Value::as_str) == Some("copy")
    {
        if let Some(source) = req.input.get("path").and_then(serde_json::Value::as_str) {
            if !paths.contains(&source) {
                paths.push(source);
            }
        }
    }
    paths
}

/// Result of one hunk-staging attempt inside [`WriteGate::run_gated`].
enum HunkAttempt {
    /// The write's hunks did not overlap the sibling's: execute this merged
    /// content (the sibling's state plus this write's hunks) instead of the
    /// request's original, claiming `onto_hash` as the truthful base.
    Staged(StagedWrite),
    /// Same-hunk collision — a loud conflict with resolution detail.
    Collision(String),
    /// Staging was impossible or unsafe — fall back to the file-level
    /// `base_version` check. Carries the reason (debug-logged).
    NotAttempted(String),
}

/// A successfully staged write: what to execute and how to attribute it.
struct StagedWrite {
    /// The sibling's current content with this write's hunks spliced in.
    merged: String,
    /// The hash of the state this write is now built on (the pre-image).
    onto_hash: String,
    /// Observability note riding the WAL payload (`hunk_staging`).
    note: serde_json::Value,
}

impl WriteGate {
    /// Create a gate over the *shared* policy/executor pair, the whiteboard
    /// log, a pre-image reader rooted at the project, and a per-agent
    /// in-flight write cap (min 1).
    pub fn new(
        policy: Arc<dyn PolicyEngine>,
        executor: Arc<ToolExecutor>,
        log_pool: sqlx::SqlitePool,
        pre_image: Arc<dyn PreImageReader>,
        project_root: PathBuf,
        max_in_flight_per_agent: usize,
    ) -> Self {
        Self {
            policy,
            executor,
            log_pool,
            pre_image,
            project_root,
            max_in_flight_per_agent: max_in_flight_per_agent.max(1),
            limits: Mutex::new(HashMap::new()),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
            text_cache: Mutex::new(PreImageTextCache::default()),
        }
    }

    /// The tool registry the gate executes under — the single source of
    /// truth the supervisor publishes to agents via `list-tools` (ADR-60 S5).
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.executor.tool_definitions()
    }

    /// Reserve `path` for exclusive write by `agent_id` for `ttl` (ADR-60 D5
    /// explicit lock tokens — v1 coordination for hot shared files).
    ///
    /// Server-side by design for v1: the table lives on this gate process,
    /// and every supervised/in-process write funnels through
    /// [`WriteGate::submit`], which enforces reservations ahead of its
    /// conflict logic — no IPC surface needed yet. Re-acquiring a path this
    /// agent already holds refreshes the reservation (latest token wins);
    /// acquiring a path another agent holds fails loudly with holder info
    /// ([`GateError::Locked`]). Expired reservations lapse lazily.
    pub fn acquire_lock(
        &self,
        path: impl Into<String>,
        agent_id: &str,
        ttl: Duration,
    ) -> Result<LockToken, GateError> {
        let path = path.into();
        let mut locks = self.locks.lock().map_err(|error| {
            GateError::Policy(format!("write-gate lock table poisoned: {error}"))
        })?;
        purge_expired_locks(&mut locks, now_millis());
        if let Some(active) = locks.get(path.as_str()) {
            if active.agent_id != agent_id {
                return Err(GateError::Locked {
                    path: path.clone(),
                    holder: active.agent_id.clone(),
                });
            }
        }
        let expires_at_ms =
            now_millis().saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX));
        let token = LockToken {
            token_id: new_id().to_string(),
            path: path.clone(),
            agent_id: agent_id.to_owned(),
            expires_at_ms,
        };
        locks.insert(
            path,
            ActiveLock {
                token_id: token.token_id.clone(),
                agent_id: token.agent_id.clone(),
                expires_at_ms,
            },
        );
        Ok(token)
    }

    /// Release a reservation granted by [`WriteGate::acquire_lock`].
    ///
    /// The genuine token always succeeds; an expired reservation has already
    /// lapsed (also success — the desired state is reached); a stale or
    /// forged token on a live reservation held by someone else is refused
    /// loudly with holder info.
    pub fn release_lock(&self, token: &LockToken) -> Result<(), GateError> {
        let mut locks = self.locks.lock().map_err(|error| {
            GateError::Policy(format!("write-gate lock table poisoned: {error}"))
        })?;
        purge_expired_locks(&mut locks, now_millis());
        match locks.get(token.path.as_str()) {
            Some(active) if active.token_id == token.token_id => {
                locks.remove(token.path.as_str());
                Ok(())
            }
            Some(active) => {
                Err(GateError::Locked { path: token.path.clone(), holder: active.agent_id.clone() })
            }
            None => Ok(()),
        }
    }

    /// Attempt hunk-aware staging for one mismatched versioned target (see
    /// [`crate::hunk`] and the module docs).
    ///
    /// Only whole-content `filesystem` `write` operations are stageable —
    /// delete/move/copy mutate the file as a unit, so they always take the
    /// file-level check. Staging further requires the claimed base text AND
    /// the current text to be recoverable from the observed-pre-image cache
    /// and decodable as UTF-8 without carriage returns; anything else degrades
    /// to [`HunkAttempt::NotAttempted`] and the caller falls back to the
    /// file-level conflict — never to silent last-writer-wins.
    fn try_stage_hunks(
        &self,
        req: &GateRequest,
        target: &str,
        expected: &str,
        actual: Option<String>,
    ) -> HunkAttempt {
        if req.tool != "filesystem"
            || req.input.get("operation").and_then(serde_json::Value::as_str) != Some("write")
            || req.input.get("path").and_then(serde_json::Value::as_str) != Some(target)
        {
            return HunkAttempt::NotAttempted("not a whole-content filesystem write".to_owned());
        }
        let Some(proposed) = req.input.get("content").and_then(serde_json::Value::as_str) else {
            return HunkAttempt::NotAttempted("no string `content` field".to_owned());
        };
        let Some(actual_hash) = actual else {
            return HunkAttempt::NotAttempted("target no longer exists".to_owned());
        };
        let (base_bytes, current_bytes) = match self.text_cache.lock() {
            Ok(cache) => (cache.get(expected), cache.get(actual_hash.as_str())),
            Err(error) => {
                return HunkAttempt::NotAttempted(format!("pre-image text cache poisoned: {error}"))
            }
        };
        let (Some(base_bytes), Some(current_bytes)) = (base_bytes, current_bytes) else {
            return HunkAttempt::NotAttempted(
                "claimed base or current text not observed by this gate".to_owned(),
            );
        };
        let (Some(base), Some(current)) =
            (std::str::from_utf8(&base_bytes).ok(), std::str::from_utf8(&current_bytes).ok())
        else {
            return HunkAttempt::NotAttempted(
                "binary (non-UTF-8) content cannot be hunk-diffed".to_owned(),
            );
        };
        match stage_three_way(base, current, proposed) {
            HunkStaging::Merged(merged) => {
                let note = serde_json::json!({
                    "target": target,
                    "declared_base_hash": expected,
                    "staged_onto_hash": actual_hash,
                });
                HunkAttempt::Staged(StagedWrite { merged, onto_hash: actual_hash, note })
            }
            HunkStaging::Collision(detail) => HunkAttempt::Collision(detail),
            HunkStaging::NotApplicable(reason) => HunkAttempt::NotAttempted(reason),
        }
    }

    /// Submit a write request for policy evaluation and gated execution.
    ///
    /// Ordering (WAL-before-execute, see module docs): replay check → claim →
    /// per-agent limiter → policy → pre-image → `write-applied` append →
    /// execute. Concurrent retries of the same `call_id` serialize behind one
    /// in-flight claim, and per-agent concurrency is bounded by the limiter.
    pub async fn submit(
        &self,
        req: GateRequest,
        cancel: CancellationToken,
    ) -> Result<GateOutcome, GateError> {
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        self.submit_dedup(req, cancel).await
    }

    /// Dedup + serialization around [`Self::run_gated`].
    ///
    /// Order: durable replay fast-path → in-flight claim → per-agent limiter
    /// → authoritative replay check under the claim → execute.
    async fn submit_dedup(
        &self,
        req: GateRequest,
        cancel: CancellationToken,
    ) -> Result<GateOutcome, GateError> {
        // Durable replay dedup first (fast path): a terminal decision for
        // `call_id` short-circuits without touching the claim set.
        match self.stored_decision(&req.call_id).await? {
            Some(StoredDecision::Applied(gate_seq)) => {
                return Ok(GateOutcome {
                    event_id: req.call_id,
                    gate_seq,
                    replayed: true,
                    result: serde_json::json!({ "replayed": true }),
                });
            }
            Some(StoredDecision::Rejected) => {
                return Err(GateError::Denied {
                    event_id: req.call_id.clone(),
                    reason: "previously rejected (write decisions are set-once per call_id)"
                        .to_string(),
                });
            }
            None => {}
        }

        // In-process replay-race claim (ADR-60 D4 "dedup by event_id"):
        // claim on first touch, then await the claim lock — concurrent
        // retries of the same `call_id` serialize here, and exactly one
        // submit proceeds to execute. The RAII guard pins the map entry for
        // every path below (including cancellation while queued on the
        // limiter) and removes it on drop.
        let claim = {
            let mut map = self.in_flight.lock().map_err(|error| {
                GateError::Policy(format!("write-gate claim lock poisoned: {error}"))
            })?;
            map.entry(req.call_id.clone()).or_insert_with(|| Arc::new(ClaimLock::new(()))).clone()
        };
        let guard = tokio::select! {
            guard = claim.clone().lock_owned() => guard,
            _ = cancel.cancelled() => return Err(GateError::Cancelled),
        };
        let _claim_guard = InFlightClaim {
            call_id: req.call_id.clone(),
            claim,
            _lock: guard,
            in_flight: self.in_flight.clone(),
        };
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }

        // Per-agent in-flight limiter (ADR-60 D4), acquired after the claim
        // so the claim set — not the semaphore — serializes same-`call_id`
        // retries. tokio's semaphore grants permits in FIFO waiter order;
        // each agent has its own semaphore created on first use (left in
        // place, bounded by roster size), so agents never block one another.
        // The permit is held for the whole gated op and released on drop.
        let semaphore = {
            let mut limits = self.limits.lock().map_err(|error| {
                GateError::Policy(format!("write-gate limiter poisoned: {error}"))
            })?;
            limits
                .entry(req.agent_id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(self.max_in_flight_per_agent)))
                .clone()
        };
        let _permit = tokio::select! {
            permit = semaphore.acquire_owned() => permit.map_err(|error| {
                GateError::Policy(format!("write-gate limiter closed: {error}"))
            })?,
            _ = cancel.cancelled() => return Err(GateError::Cancelled),
        };

        // Re-check the durable log while still holding the claim: the winner
        // may have committed a terminal decision while this submit waited on
        // the claim lock or the limiter — replay it instead of re-executing
        // (set-once semantics).
        match self.stored_decision(&req.call_id).await? {
            Some(StoredDecision::Applied(gate_seq)) => {
                return Ok(GateOutcome {
                    event_id: req.call_id,
                    gate_seq,
                    replayed: true,
                    result: serde_json::json!({ "replayed": true }),
                });
            }
            Some(StoredDecision::Rejected) => {
                return Err(GateError::Denied {
                    event_id: req.call_id.clone(),
                    reason: "previously rejected (write decisions are set-once per call_id)"
                        .to_string(),
                });
            }
            None => {}
        }

        self.run_gated(req, cancel).await
    }

    /// The gated write itself: policy → pre-image → WAL append → execute.
    async fn run_gated(
        &self,
        mut req: GateRequest,
        cancel: CancellationToken,
    ) -> Result<GateOutcome, GateError> {
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }

        let session_id = self.session_ulid(&req.session_id)?;
        let action = PolicyAction {
            tool_name: &req.tool,
            input: &req.input,
            session_id,
            correlation_id: new_id(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };

        // Only `Allow` executes. Every other verdict is a durable
        // `write-rejected` decision (event_id = `call_id`) and surfaces as a
        // `Denied` error. A cancelled evaluation surfaces as `Cancelled` with
        // no row appended (the policy engine honors the token itself).
        let verdict = match self.policy.evaluate(&action, cancel.clone()).await {
            Ok(verdict) => verdict,
            Err(PolicyError::Cancelled) => return Err(GateError::Cancelled),
            Err(error) => return Err(GateError::from(error)),
        };

        // ADR-60 D5: capture the current bytes of every attributed path
        // (each versioned/mutated target plus the read-only `copy` source)
        // before ANY terminal decision is committed, so both the
        // `write-applied` and `write-rejected` WAL rows carry the pre-images
        // when computable and the conflict check below compares against the
        // same capture.
        let pre_images = self.pre_image_hashes(&req).await?;
        // The whiteboard `pre_image_hash` column keeps the PRIMARY target's
        // hash (the single-target-era semantics, so the column is stable).
        let primary_pre_image =
            versioned_target(&req).and_then(|target| pre_images.get(target).cloned());

        if !matches!(verdict, PolicyVerdict::Allow) {
            self.append_rejected(&req, &verdict, primary_pre_image, &pre_images).await?;
            return Err(GateError::Denied {
                event_id: req.call_id.clone(),
                reason: format!("{verdict:?}"),
            });
        }

        // ADR-60 D5 explicit lock tokens: a versioned target reserved for
        // exclusive write by ANOTHER agent refuses loudly here — before any
        // WAL append — with holder info in the reason. The reserving owner
        // proceeds through its own lock, and an expired reservation has
        // already lapsed (lazy purge).
        let versioned =
            versioned_targets(&req).into_iter().map(str::to_owned).collect::<Vec<String>>();
        {
            let mut locks = self.locks.lock().map_err(|error| {
                GateError::Policy(format!("write-gate lock table poisoned: {error}"))
            })?;
            purge_expired_locks(&mut locks, now_millis());
            for target in &versioned {
                if let Some(active) = locks.get(target.as_str()) {
                    if active.agent_id != req.agent_id {
                        return Err(GateError::Conflict {
                            event_id: req.call_id.clone(),
                            reason: format!(
                                "target {target} is locked for exclusive write by agent '{}' \
                                 (explicit lock token, ADR-60 D5)",
                                active.agent_id,
                            ),
                        });
                    }
                }
            }
        }

        // ADR-60 D5: optimistic base_version conflict detection over every
        // mutated (versioned) target, refined by hunk-aware staging. When the
        // caller declared the hash of a target's state it believes it is
        // editing, that target's pre-image must match right now. A mismatch —
        // including a target that does not exist — means a sibling agent
        // already changed the file. If both edits touch different lines of
        // the shared base, the write's hunks are STAGED onto the current
        // content (disjoint concurrent edits both survive); otherwise the
        // collision surfaces loudly with NO whiteboard row (nothing was
        // applied, so nothing is logged) for the manual-resolution path.
        // Targets without a declared claim are not checked (fresh creates
        // carry no claim → documented last-writer-wins), and declared claims
        // on read-only targets (the `copy` source, absent from
        // [`versioned_targets`]) are ignored.
        let mut staging_notes = Vec::new();
        for target in &versioned {
            let Some(expected) = req.base_versions.get(target).cloned() else {
                continue;
            };
            let actual = pre_images.get(target);
            if actual.map(String::as_str) == Some(expected.as_str()) {
                continue;
            }
            match self.try_stage_hunks(&req, target, &expected, actual.cloned()) {
                HunkAttempt::Staged(staged) => {
                    tracing::info!(
                        target_path = %target,
                        call_id = %req.call_id,
                        "hunk-aware staging merged disjoint sibling edits"
                    );
                    staging_notes.push(staged.note);
                    // The truthful claim going forward is the state this
                    // write is now built on (the sibling's current bytes).
                    req.base_versions.insert(target.clone(), staged.onto_hash);
                    req.input["content"] = serde_json::Value::String(staged.merged);
                    continue;
                }
                HunkAttempt::Collision(reason) => {
                    return Err(GateError::Conflict {
                        event_id: req.call_id.clone(),
                        reason: format!("hunk-aware staging refused on {target}: {reason}"),
                    });
                }
                HunkAttempt::NotAttempted(reason) => {
                    tracing::debug!(
                        target_path = %target,
                        %reason,
                        "hunk staging unavailable; falling back to the file-level base_version check"
                    );
                }
            }
            // File-level fallback (the pre-staging contract, unchanged):
            // undiffable or unstageable mismatches refuse wholesale.
            let current = actual.map(String::as_str).unwrap_or("<absent>");
            let absent = if actual.is_none() { " (target does not exist)" } else { "" };
            return Err(GateError::Conflict {
                event_id: req.call_id.clone(),
                reason: format!(
                    "base_version mismatch on {target}: expected {expected}, current {current}{absent}",
                ),
            });
        }

        // WAL-before-execute: the applied event is durable before the tool
        // runs; a crash after this point replays as `replayed: true`.
        let mut payload = serde_json::json!({
            "tool": &req.tool,
            "input": &req.input,
            "policy_verdict": "allow",
            "pre_images": &pre_images,
        });
        if !staging_notes.is_empty() {
            // Observability for the manual-resolution trail: records exactly
            // what was staged onto whose state (ADR-60 D5 attribution).
            payload["hunk_staging"] = serde_json::Value::Array(staging_notes);
        }
        let stored = append_whiteboard_event(
            &self.log_pool,
            &NewWhiteboardEvent {
                event_id: req.call_id.clone(),
                agent_id: req.agent_id.clone(),
                kind: WhiteboardKind::WriteApplied,
                scope: req.scope.clone(),
                session_id: req.session_id.clone(),
                plan_id: req.plan_id.clone(),
                causation: req.causation.clone(),
                payload,
                pre_image_hash: primary_pre_image,
                created_at: now_millis(),
            },
        )
        .await?;

        // Execute AFTER the WAL append commits. Cancellation propagates as-is
        // (no `failure` event — the attempt was abandoned, not failed); other
        // errors log a `failure` event causally linked to the applied write.
        let session_ctx = self.session(session_id);
        let input = req.input.clone(); // keep `req` intact for failure logging below
        let output = self.executor.execute(&req.tool, input, &session_ctx, cancel).await;
        let output = match output {
            Ok(output) => output,
            Err(ToolError::Cancelled) => return Err(GateError::Cancelled),
            Err(error) => {
                self.append_failure(&req, &error).await?;
                return Err(GateError::from(error));
            }
        };

        // The full `ToolOutput` (summary + data) rides the wire so the agent
        // side can rebuild the shape the loop consumes (ADR-60 S5).
        let result = serde_json::to_value(&output)
            .map_err(|error| GateError::Execution(format!("output serialization: {error}")))?;

        Ok(GateOutcome {
            event_id: stored.event_id,
            gate_seq: stored.gate_seq,
            replayed: false,
            result,
        })
    }

    /// Append the `write-rejected` decision for a non-`Allow` verdict. The
    /// row is keyed by the rejected `call_id` (event_id = `call_id`) and
    /// carries the ADR-60 D5 pre-image hashes when computable.
    async fn append_rejected(
        &self,
        req: &GateRequest,
        verdict: &PolicyVerdict,
        pre_image_hash: Option<String>,
        pre_images: &BTreeMap<String, String>,
    ) -> Result<(), GateError> {
        append_whiteboard_event(
            &self.log_pool,
            &NewWhiteboardEvent {
                event_id: req.call_id.clone(),
                agent_id: req.agent_id.clone(),
                kind: WhiteboardKind::WriteRejected,
                scope: req.scope.clone(),
                session_id: req.session_id.clone(),
                plan_id: req.plan_id.clone(),
                causation: req.causation.clone(),
                payload: serde_json::json!({
                    "tool": &req.tool,
                    "input": &req.input,
                    "reason": format!("{verdict:?}"),
                    "pre_images": pre_images,
                }),
                pre_image_hash,
                created_at: now_millis(),
            },
        )
        .await?;
        Ok(())
    }

    /// Append a `failure` event for an execution error. Uses a fresh
    /// `event_id` (the `call_id` is taken by the `write-applied` WAL row) and
    /// links back to it via `causation`.
    async fn append_failure(&self, req: &GateRequest, error: &ToolError) -> Result<(), GateError> {
        append_whiteboard_event(
            &self.log_pool,
            &NewWhiteboardEvent {
                event_id: new_id().to_string(),
                agent_id: req.agent_id.clone(),
                kind: WhiteboardKind::Failure,
                scope: req.scope.clone(),
                session_id: req.session_id.clone(),
                plan_id: req.plan_id.clone(),
                causation: Some(req.call_id.clone()),
                payload: serde_json::json!({
                    "tool": &req.tool,
                    "error": error.to_string(),
                }),
                pre_image_hash: None,
                created_at: now_millis(),
            },
        )
        .await?;
        Ok(())
    }

    /// Compute the ADR-60 D5 pre-image hashes for a request, keyed by each
    /// attributed path (every [`versioned_targets`] entry plus the read-only
    /// `copy` source). Paths with no current file are absent from the map (a
    /// fresh create has no pre-image); a read failure for any path is a
    /// [`GateError::PreImage`], matching the single-target-era contract.
    async fn pre_image_hashes(
        &self,
        req: &GateRequest,
    ) -> Result<BTreeMap<String, String>, GateError> {
        let mut hashes = BTreeMap::new();
        for relative_path in attributed_paths(req) {
            let bytes = self.pre_image.read(Path::new(relative_path)).await.map_err(|error| {
                GateError::PreImage(format!("read pre-image for {relative_path:?}: {error}"))
            })?;
            if let Some(bytes) = bytes {
                let hash = blake3::hash(&bytes).to_hex().to_string();
                // Cache the observed bytes (content-addressed by hash) so
                // hunk-aware staging can later reconstruct this exact content
                // from its hash alone. A poisoned cache only degrades staging
                // to file-level checks, never correctness.
                match self.text_cache.lock() {
                    Ok(mut cache) => cache.insert(hash.clone(), Arc::new(bytes)),
                    Err(error) => tracing::debug!(
                        target_path = %relative_path,
                        %error,
                        "pre-image text cache poisoned; hunk staging may fall back to file-level checks"
                    ),
                }
                hashes.insert(relative_path.to_owned(), hash);
            }
        }
        Ok(hashes)
    }

    /// Parse a caller session id, or mint one when absent (policy/eval need a
    /// `Ulid`; the whiteboard keeps the original `Option<String>`).
    fn session_ulid(&self, session_id: &Option<String>) -> Result<Ulid, GateError> {
        match session_id {
            Some(id) => Ulid::from_string(id)
                .map_err(|_| GateError::InvalidRequest(format!("invalid session_id: {id}"))),
            None => Ok(new_id()),
        }
    }

    /// Build the `SessionContext` handed to `ToolExecutor::execute`, rooted at
    /// the gate's project dir (the supervisor's session context, ADR-60 D4).
    fn session(&self, session_id: Ulid) -> SessionContext {
        SessionContext::new(session_id, self.project_root.clone())
    }

    /// Terminal whiteboard decision for `call_id`, if any.
    async fn stored_decision(&self, call_id: &str) -> Result<Option<StoredDecision>, GateError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT gate_seq, kind FROM whiteboard_events WHERE event_id = ?")
                .bind(call_id)
                .fetch_optional(&self.log_pool)
                .await
                .map_err(SessionError::from)?;
        match row {
            Some((seq, kind)) => {
                let seq = u64::try_from(seq)
                    .map_err(|_| GateError::Whiteboard("negative gate_seq".to_string()))?;
                let decision = if kind == WhiteboardKind::WriteApplied.as_str() {
                    StoredDecision::Applied(seq)
                } else {
                    StoredDecision::Rejected
                };
                Ok(Some(decision))
            }
            None => Ok(None),
        }
    }

    /// ADR-60 D5 (i): materialize the gate-boundary checkpoint at
    /// `gate_seq` — the projected file state of every applied filesystem
    /// write with `gate_seq <= gate_seq` (the consistent cut "everything ≤
    /// seq S"), restricted to `session_id` when given.
    ///
    /// Durability rides the log, not a second store: every folded event was
    /// committed WAL-first by this gate before its tool executed (the module's
    /// ordering invariant), so the cut is reconstructible after any crash. The
    /// checkpoint is a projection — the raw log is never truncated or
    /// rewritten. Restoring forward (and per-agent revert, D5 (ii)) replays
    /// the tail over the snapshot via
    /// [`crate::checkpoint::GateBoundaryCheckpoint::replay_tail_excluding`].
    ///
    /// Reachable wherever the gate is attached —
    /// [`crate::supervisor::SupervisorServices::gate`] on the supervised path
    /// (opt-in `[orchestration] supervisor_enabled`) and the in-process loop's
    /// gate alike — for revert/restore flows built on top.
    pub async fn create_checkpoint_at(
        &self,
        gate_seq: u64,
        session_id: Option<&str>,
    ) -> Result<crate::checkpoint::GateBoundaryCheckpoint, GateError> {
        let events = load_whiteboard_events_up_to(&self.log_pool, gate_seq, session_id).await?;
        Ok(crate::checkpoint::GateBoundaryCheckpoint::at_cut(
            &events,
            gate_seq,
            session_id.map(str::to_owned),
        ))
    }

    /// Convenience form of [`WriteGate::create_checkpoint_at`] pinned to the
    /// current head of the log: "checkpoint now".
    pub async fn create_checkpoint(
        &self,
        session_id: Option<&str>,
    ) -> Result<crate::checkpoint::GateBoundaryCheckpoint, GateError> {
        let head = latest_gate_seq(&self.log_pool).await.map_err(GateError::from)?;
        self.create_checkpoint_at(head, session_id).await
    }
}

/// ADR-60 D5 always-on injection: stamp an incoming gated write with the
/// current pre-image hash of every one of its versioned (mutated) targets,
/// UNLESS the agent already declared a claim for that target (a caller-
/// declared claim must win — never clobber it, per target).
///
/// The supervisor is the single owner of the write gate, so it is also the
/// single authority able to attest "the target looks like this right now":
/// every versioned filesystem write/delete/move/copy that reaches the gate
/// carries per-target base versions computed from *this* function — the gate's
/// own reader + blake3 hex convention — so the stamp and the gate's conflict
/// check share one source of truth and can never drift. A same-target sibling
/// write landing between this stamp and the gate's own pre-image capture (a
/// sub-millisecond window, but exactly the race D5 exists for) then surfaces
/// as a loud [`GateError::Conflict`] instead of silent last-writer-wins.
///
/// Deliberate edges:
/// - A fresh create (target has no pre-image yet) carries no claim for that
///   target — there is no version to conflict with (documented LWW).
/// - A pre-image read failure for a target leaves NO claim for it and is
///   logged at `debug` (the silent degradation to LWW must be observable).
/// - Read-only targets (the `copy` source) are not versioned targets and are
///   never stamped here — the gate still records their pre-image for
///   attribution on the WAL row.
///
/// Reusable by every path that submits a gated write: the supervisor's
/// `handle_execute_tool` today, the in-process agent loop (a later stage)
/// tomorrow.
pub(crate) async fn stamp_base_versions(gate: &WriteGate, request: &mut GateRequest) {
    let targets: Vec<String> = versioned_targets(request).into_iter().map(str::to_owned).collect();
    if targets.is_empty() {
        return;
    }
    for target in targets {
        if request.base_versions.contains_key(&target) {
            // A caller-declared claim wins per target — never clobber it,
            // even when it is stale (the gate surfaces the conflict then).
            continue;
        }
        let bytes = match gate.pre_image.read(Path::new(&target)).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue, // fresh create: no prior version to claim
            Err(error) => {
                tracing::debug!(
                    target: "concerto_orchestrator::supervisor",
                    target_path = %target,
                    %error,
                    "supervisor: base_version stamp skipped for target (pre-image read failed)"
                );
                continue;
            }
        };
        request.base_versions.insert(target, blake3::hash(&bytes).to_hex().to_string());
    }
}

/// Drop lock reservations past their TTL (ADR-60 D5 lazy expiry: no
/// background sweeper — the next acquire/release/submit touching the table
/// observes the lapses).
fn purge_expired_locks(locks: &mut LockTable, now_ms: i64) {
    locks.retain(|_, active| active.expires_at_ms > now_ms);
}

/// Unix epoch milliseconds (UTC) — the whiteboard `created_at` contract
/// (ADR-60 D3). `time`'s millis-precision helpers are not enabled in this
/// workspace, so compute from `std::time`.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use concerto_core::error::PolicyError;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{Condition, PolicyAction, PolicyRule, ToolOutput, ToolRegistry};
    use concerto_sessions::whiteboard::{
        load_whiteboard_events, WhiteboardEvent, WhiteboardLoadOpts,
    };
    use concerto_tools::filesystem::FilesystemTool;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    /// No-op audit log for tests (mirrors `agent_loop`'s `TestAudit`).
    struct TestAudit;

    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    /// Policy stub that reports cancellation at evaluation time — exercises
    /// the gate's `PolicyError::Cancelled` → `GateError::Cancelled` mapping.
    /// The real `SimplePolicyEngine` returns the same error when its token is
    /// cancelled mid-evaluation (`policy.rs`: `if cancel.is_cancelled()`), so
    /// this stub deterministically reaches the "at policy evaluation" path.
    struct CancellingPolicy {
        audit: Arc<TestAudit>,
    }

    #[async_trait]
    impl PolicyEngine for CancellingPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Err(PolicyError::Cancelled)
        }
        fn audit_log(&self) -> &dyn AuditLog {
            self.audit.as_ref()
        }
    }

    /// Trivial tool that counts invocations — proves a replay never
    /// re-executes. The brief sleep widens the race window so a broken
    /// replay-race claim actually double-executes under concurrent retries
    /// (the counter, not the sleep, is the assertion).
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "gate_test"
        }
        fn description(&self) -> &str {
            "counts invocations"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(ToolOutput {
                summary: "ok".into(),
                data: json!({ "probe": self.calls.load(Ordering::SeqCst) }),
            })
        }
    }

    /// No-op stub for a named tool — lets the gate's WAL+execute path succeed
    /// for, e.g., `filesystem` without touching a real filesystem.
    struct StubTool {
        name: &'static str,
    }

    impl StubTool {
        fn new(name: &'static str) -> Self {
            Self { name }
        }
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "ok".into(), data: json!({ "ok": true }) })
        }
    }

    /// Tool whose execution always fails — exercises the `failure` event.
    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::ExecutionFailed { message: "simulated write failure".to_string() })
        }
    }

    /// Tool that reports cancellation — the executor's mid-path cancellation
    /// signal.
    struct CancellingTool;

    #[async_trait]
    impl Tool for CancellingTool {
        fn name(&self) -> &str {
            "cancel"
        }
        fn description(&self) -> &str {
            "returns ToolError::Cancelled"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Cancelled)
        }
    }

    /// Tool that reports how many of its executions overlap — drives the
    /// per-agent limiter test. Each execution sends on `entered`, then blocks
    /// on `release` (a 0-permit semaphore the test lifts one permit at a time).
    /// `Clone` so a shared instance can live both in the tool registry and the
    /// test — every field is `Arc`/cloneable, so clones observe the same state.
    #[derive(Clone)]
    struct GatedProbeTool {
        entered: mpsc::UnboundedSender<()>,
        release: Arc<Semaphore>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for GatedProbeTool {
        fn name(&self) -> &str {
            "gated_probe"
        }
        fn description(&self) -> &str {
            "reports and gates concurrent executions"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            let before = self.active.fetch_add(1, Ordering::SeqCst);
            self.max_active.fetch_max(before + 1, Ordering::SeqCst);
            let _ = self.entered.send(());
            let permit = self.release.clone().acquire_owned().await.map_err(|error| {
                ToolError::ExecutionFailed { message: format!("probe release closed: {error}") }
            })?;
            // Do NOT drop the permit: an `OwnedSemaphorePermit` returns its
            // permit to the semaphore on drop, so a completing execution would
            // hand its "go" signal to the next queued tool and the test's
            // one-permit-at-a-time drain would become self-sustaining. Each
            // `add_permits(1)` from the test must lift exactly one execution.
            std::mem::forget(permit);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput { summary: "ok".into(), data: json!({ "ok": true }) })
        }
    }

    /// File-backed pool with the same PRAGMAs as production connectivity
    /// (WAL, busy_timeout, synchronous=NORMAL) and all sessions migrations
    /// applied (this crate's own migrations live in `../sessions/`).
    async fn test_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("gate_test.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn engine(rules: Vec<PolicyRule>) -> Arc<SimplePolicyEngine> {
        Arc::new(SimplePolicyEngine::new(rules, Arc::new(TestAudit)))
    }

    fn allow_engine() -> Arc<SimplePolicyEngine> {
        engine(vec![PolicyRule::AutoApprove(Condition::Always)])
    }

    fn deny_engine() -> Arc<SimplePolicyEngine> {
        engine(vec![PolicyRule::AutoDeny(Condition::Always)])
    }

    fn approval_engine() -> Arc<SimplePolicyEngine> {
        engine(vec![PolicyRule::RequireApproval(Condition::Always)])
    }

    fn registry(probe: Option<Arc<GatedProbeTool>>) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(StubTool::new("filesystem")));
        registry.register(Box::new(FailingTool));
        registry.register(Box::new(CancellingTool));
        if let Some(probe) = probe {
            registry.register(Box::new((*probe).clone()));
        }
        registry
    }

    /// Build a gate with the default per-agent cap of 1. `root` is both the
    /// session project dir and the pre-image reader root.
    fn gate(
        policy: Arc<SimplePolicyEngine>,
        calls: Arc<AtomicUsize>,
        pool: sqlx::SqlitePool,
        root: PathBuf,
    ) -> Arc<WriteGate> {
        gate_with(policy, calls, pool, root, None, 1)
    }

    /// Build a gate with a custom limiter cap and an optional probe tool.
    fn gate_with(
        policy: Arc<SimplePolicyEngine>,
        calls: Arc<AtomicUsize>,
        pool: sqlx::SqlitePool,
        root: PathBuf,
        probe: Option<Arc<GatedProbeTool>>,
        max_parallel: usize,
    ) -> Arc<WriteGate> {
        let mut registry = registry(probe);
        registry.register(Box::new(CountingTool { calls }));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
        Arc::new(WriteGate::new(
            policy,
            executor,
            pool,
            Arc::new(FilePreImageReader::new(root.clone())),
            root,
            max_parallel,
        ))
    }

    fn request(call_id: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: "agent-a".to_owned(),
            tool: "gate_test".to_owned(),
            input: json!({}),
            session_id: None,
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
        }
    }

    fn filesystem_request(call_id: &str, operation: &str, path: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: "agent-a".to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({ "operation": operation, "path": path }),
            session_id: None,
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
        }
    }

    /// A `filesystem` move request (the tool's `path` is the source).
    fn fs_move(call_id: &str, source: &str, destination: &str) -> GateRequest {
        let mut request = filesystem_request(call_id, "move", source);
        request.input["destination"] = json!(destination);
        request
    }

    /// A `filesystem` copy request (the tool's `path` is the source).
    fn fs_copy(call_id: &str, source: &str, destination: &str) -> GateRequest {
        let mut request = filesystem_request(call_id, "copy", source);
        request.input["destination"] = json!(destination);
        request
    }

    async fn whiteboard_row_count(pool: &sqlx::SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM whiteboard_events")
            .fetch_one(pool)
            .await
            .expect("row count")
    }

    /// Load a stored whiteboard event by `event_id`.
    async fn applied_row(pool: &sqlx::SqlitePool, event_id: &str) -> WhiteboardEvent {
        let events = load_whiteboard_events(pool, &WhiteboardLoadOpts::default())
            .await
            .expect("load events");
        events
            .into_iter()
            .find(|event| event.event_id == event_id)
            .unwrap_or_else(|| panic!("whiteboard event {event_id} missing"))
    }

    #[tokio::test]
    async fn allowed_path_appends_before_execute_and_replay_never_reexecutes() {
        let (_dir, pool) = test_pool(1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = gate(allow_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"));

        // Fresh write: policy Allow → append BEFORE execute → tool runs once.
        let first = gate.submit(request("call-1"), CancellationToken::new()).await.expect("allow");
        assert!(!first.replayed, "fresh write is not a replay");
        assert_eq!(first.event_id, "call-1");
        assert_eq!(first.gate_seq, 1);
        assert_eq!(first.result["data"]["probe"], json!(1), "tool executed once");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(whiteboard_row_count(&pool).await, 1, "one WriteApplied row");
        assert_eq!(
            applied_row(&pool, "call-1").await.kind,
            WhiteboardKind::WriteApplied,
            "the single row is a write-applied event"
        );

        // Replay of the same call_id: dedup by event_id, no re-execution.
        let replay =
            gate.submit(request("call-1"), CancellationToken::new()).await.expect("replay");
        assert!(replay.replayed, "duplicate call_id is a replay");
        assert_eq!(replay.gate_seq, 1, "replay returns the stored gate_seq");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "tool NOT re-executed on replay");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "no new row on replay");
    }

    #[tokio::test]
    async fn denied_path_appends_write_rejected_and_never_executes() {
        let (_dir, pool) = test_pool(1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = gate(deny_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"));

        let result = gate.submit(request("call-2"), CancellationToken::new()).await;
        match result {
            Err(GateError::Denied { event_id, reason }) => {
                assert_eq!(event_id, "call-2");
                assert!(!reason.is_empty(), "reason carries the verdict");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool never reached on deny");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "the reject is durable");
        let rejected = applied_row(&pool, "call-2").await;
        assert_eq!(rejected.kind, WhiteboardKind::WriteRejected);
        assert_eq!(rejected.event_id, "call-2", "reject row keyed by the call_id");
        assert_eq!(rejected.payload["tool"], json!("gate_test"));
        assert_eq!(rejected.payload["input"], json!({}), "reject payload carries the input");
        assert!(rejected.payload.get("reason").is_some(), "reject reason recorded");
    }

    #[tokio::test]
    async fn rejected_call_id_is_set_once_and_never_reexecutes() {
        let (_dir, pool) = test_pool(1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let denied = gate(deny_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"));
        let result = denied.submit(request("call-3"), CancellationToken::new()).await;
        assert!(matches!(result, Err(GateError::Denied { .. })));
        assert_eq!(whiteboard_row_count(&pool).await, 1);

        // Even under a now-permissive policy, the same call_id is set-once:
        // its terminal decision was already recorded.
        let permissive = gate(allow_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"));
        let retry = permissive.submit(request("call-3"), CancellationToken::new()).await;
        assert!(
            matches!(retry, Err(GateError::Denied { event_id, .. }) if event_id == "call-3"),
            "a previously rejected call_id stays rejected"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "never executed, even under allow policy");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "no second row for the rejected id");
    }

    #[tokio::test]
    async fn require_approval_appends_write_rejected() {
        let (_dir, pool) = test_pool(1).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = gate(approval_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"));

        let result = gate.submit(request("call-4"), CancellationToken::new()).await;
        assert!(matches!(result, Err(GateError::Denied { .. })), "approval-required is denied");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool never reached");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "reject recorded");
        assert_eq!(applied_row(&pool, "call-4").await.kind, WhiteboardKind::WriteRejected);
    }

    #[tokio::test]
    async fn execution_error_appends_failure_event_and_propagates() {
        let (_dir, pool) = test_pool(1).await;
        let gate = gate(
            allow_engine(),
            Arc::new(AtomicUsize::new(0)),
            pool.clone(),
            PathBuf::from("/tmp"),
        );

        let mut req = request("fail-call");
        req.tool = "fail".into();
        let result = gate.submit(req, CancellationToken::new()).await;
        assert!(
            matches!(result, Err(GateError::Execution(_))),
            "execution error propagates as GateError::Execution"
        );
        assert_eq!(whiteboard_row_count(&pool).await, 2, "WriteApplied + Failure");

        let applied = applied_row(&pool, "fail-call").await;
        assert_eq!(applied.kind, WhiteboardKind::WriteApplied);
        let events = load_whiteboard_events(&pool, &WhiteboardLoadOpts::default())
            .await
            .expect("load events");
        let failure = events
            .iter()
            .find(|event| event.kind == WhiteboardKind::Failure)
            .unwrap_or_else(|| panic!("failure event missing"));
        assert_eq!(failure.causation.as_deref(), Some("fail-call"), "failure links to the write");
        assert!(failure.gate_seq > applied.gate_seq, "failure sequenced after the applied event");
        assert_eq!(failure.payload["tool"], json!("fail"));
        assert!(failure.payload.get("error").is_some(), "error detail recorded");
        assert_ne!(
            failure.event_id, "fail-call",
            "failure uses a fresh event_id (the call_id row is the applied WAL event)"
        );
    }

    #[tokio::test]
    async fn cancellation_leaves_no_stray_events() {
        let (_dir, pool) = test_pool(1).await;
        let gate = gate(
            allow_engine(),
            Arc::new(AtomicUsize::new(0)),
            pool.clone(),
            PathBuf::from("/tmp"),
        );

        // Cancelled before any work: nothing evaluated, nothing logged.
        let pre_cancelled = CancellationToken::new();
        pre_cancelled.cancel();
        let early = gate.submit(request("cancel-early"), pre_cancelled).await;
        assert!(matches!(early, Err(GateError::Cancelled)));
        assert_eq!(whiteboard_row_count(&pool).await, 0, "no events for a pre-cancelled request");

        // Cancelled mid-path (the executor reports ToolError::Cancelled after
        // the WAL append): the applied event stays (replay-safe WAL), and no
        // failure event is logged — cancellation is not a failed write.
        let mut req = request("cancel-mid");
        req.tool = "cancel".into();
        let mid = gate.submit(req, CancellationToken::new()).await;
        assert!(matches!(mid, Err(GateError::Cancelled)), "mid-path cancellation surfaces");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "only the WAL applied event");
        assert_eq!(
            applied_row(&pool, "cancel-mid").await.kind,
            WhiteboardKind::WriteApplied,
            "the retained event is the applied WAL row"
        );
    }

    #[tokio::test]
    async fn policy_evaluation_cancellation_maps_to_cancelled_and_appends_nothing() {
        let (_dir, pool) = test_pool(1).await;
        // A policy engine that reports `PolicyError::Cancelled` at evaluation
        // time — the same error `SimplePolicyEngine::evaluate` returns when
        // its token is cancelled (verified at `policy.rs` `is_cancelled()`).
        let policy: Arc<dyn PolicyEngine> =
            Arc::new(CancellingPolicy { audit: Arc::new(TestAudit) });
        let mut registry = registry(None);
        registry.register(Box::new(CountingTool { calls: Arc::new(AtomicUsize::new(0)) }));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
        let gate = Arc::new(WriteGate::new(
            policy,
            executor,
            pool.clone(),
            Arc::new(FilePreImageReader::new(PathBuf::from("/tmp"))),
            PathBuf::from("/tmp"),
            1,
        ));

        let result = gate.submit(request("cancel-policy"), CancellationToken::new()).await;
        assert!(
            matches!(result, Err(GateError::Cancelled)),
            "policy-evaluation cancellation surfaces as GateError::Cancelled, got {result:?}"
        );
        assert_eq!(
            whiteboard_row_count(&pool).await,
            0,
            "no WAL row for a cancelled policy evaluation"
        );
    }

    #[tokio::test]
    async fn pre_image_hash_captured_for_filesystem_writes() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        std::fs::write(root.join("notes.txt"), "v1").expect("seed file");
        let c1 = gate
            .submit(filesystem_request("pic-1", "write", "notes.txt"), CancellationToken::new())
            .await;
        assert!(c1.is_ok(), "filesystem write allowed: {c1:?}");
        assert_eq!(
            applied_row(&pool, "pic-1").await.pre_image_hash,
            Some(blake3::hash(b"v1").to_hex().to_string()),
            "pre-image captures the pre-write bytes"
        );

        // A modify (new call_id) captures the new pre-image.
        std::fs::write(root.join("notes.txt"), "v2").expect("re-seed file");
        let c2 = gate
            .submit(filesystem_request("pic-2", "write", "notes.txt"), CancellationToken::new())
            .await;
        assert!(c2.is_ok(), "second filesystem write allowed: {c2:?}");
        assert_eq!(
            applied_row(&pool, "pic-2").await.pre_image_hash,
            Some(blake3::hash(b"v2").to_hex().to_string())
        );

        // A delete of a missing file has no pre-image.
        let c3 = gate
            .submit(filesystem_request("pic-3", "delete", "ghost.txt"), CancellationToken::new())
            .await;
        assert!(c3.is_ok(), "delete allowed: {c3:?}");
        assert_eq!(
            applied_row(&pool, "pic-3").await.pre_image_hash,
            None,
            "no file -> no pre-image"
        );

        // Non-filesystem tools never capture a pre-image.
        let c4 = gate.submit(request("pic-4"), CancellationToken::new()).await;
        assert!(c4.is_ok(), "non-filesystem tool allowed: {c4:?}");
        assert_eq!(applied_row(&pool, "pic-4").await.pre_image_hash, None);
    }

    /// A `filesystem` request with a content payload — used with the real
    /// `FilesystemTool` so the gate's execute path actually materializes files.
    fn real_fs_write(call_id: &str, path: &str, content: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: "agent-a".to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({ "operation": "write", "path": path, "content": content }),
            session_id: None,
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn denied_filesystem_write_records_pre_image_hash_when_computable() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(deny_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // Existing file: the rejected WAL row carries the pre-write bytes.
        std::fs::write(root.join("target.txt"), "sensitive").expect("seed file");
        let denied = gate
            .submit(
                filesystem_request("deny-pre-1", "write", "target.txt"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(denied, Err(GateError::Denied { .. })), "denied write");
        let rejected = applied_row(&pool, "deny-pre-1").await;
        assert_eq!(rejected.kind, WhiteboardKind::WriteRejected);
        assert_eq!(
            rejected.pre_image_hash,
            Some(blake3::hash(b"sensitive").to_hex().to_string()),
            "rejected event records the pre-write bytes"
        );

        // Absent file: no pre-image on the rejected row.
        let denied = gate
            .submit(
                filesystem_request("deny-pre-2", "write", "ghost.txt"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(denied, Err(GateError::Denied { .. })), "denied write");
        assert_eq!(
            applied_row(&pool, "deny-pre-2").await.pre_image_hash,
            None,
            "no file -> no pre-image on the rejected row"
        );
    }

    #[tokio::test]
    async fn real_filesystem_write_pre_image_create_modify_and_non_file() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let (_pool_dir, pool) = test_pool(8).await;
        let policy = allow_engine();

        // Register the real filesystem tool (overwriting the test stub) so the
        // gate both captures pre-images and actually materializes disk writes.
        let mut registry = registry(None);
        registry.register(Box::new(CountingTool { calls: Arc::new(AtomicUsize::new(0)) }));
        let utf8_root = camino::Utf8PathBuf::from_path_buf(root.clone()).expect("utf8 root");
        registry.register(Box::new(FilesystemTool::new(utf8_root)));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
        let gate = Arc::new(WriteGate::new(
            policy,
            executor,
            pool.clone(),
            Arc::new(FilePreImageReader::new(root.clone())),
            root.clone(),
            1,
        ));

        // Create: no prior file -> pre_image None, and the write materializes it.
        let created = gate
            .submit(real_fs_write("real-create", "new.txt", "hello"), CancellationToken::new())
            .await;
        assert!(created.is_ok(), "create allowed: {created:?}");
        let applied = applied_row(&pool, "real-create").await;
        assert_eq!(applied.kind, WhiteboardKind::WriteApplied);
        assert_eq!(applied.pre_image_hash, None, "no prior file -> no pre-image");
        assert!(root.join("new.txt").is_file(), "the write materialized the file on disk");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).expect("read"), "hello");

        // Modify: pre_image = hash of the OLD on-disk bytes, then the write
        // replaces the content.
        std::fs::write(root.join("mod.txt"), "old").expect("seed file");
        let modified = gate
            .submit(real_fs_write("real-modify", "mod.txt", "new"), CancellationToken::new())
            .await;
        assert!(modified.is_ok(), "modify allowed: {modified:?}");
        assert_eq!(
            applied_row(&pool, "real-modify").await.pre_image_hash,
            Some(blake3::hash(b"old").to_hex().to_string()),
            "pre-image captures the pre-write bytes"
        );
        assert_eq!(std::fs::read_to_string(root.join("mod.txt")).expect("read"), "new");

        // Non-file tools never capture a pre-image.
        let other = gate.submit(request("real-other"), CancellationToken::new()).await;
        assert!(other.is_ok(), "non-file tool allowed: {other:?}");
        assert_eq!(applied_row(&pool, "real-other").await.pre_image_hash, None);
    }

    #[tokio::test]
    async fn whiteboard_rows_carry_scope_session_plan_and_causation() {
        let (_dir, pool) = test_pool(1).await;
        let gate = gate(
            allow_engine(),
            Arc::new(AtomicUsize::new(0)),
            pool.clone(),
            PathBuf::from("/tmp"),
        );

        let session = new_id().to_string();
        let mut req = request("attr-call");
        req.scope = "ssh".to_owned();
        req.session_id = Some(session.clone());
        req.plan_id = Some("plan-42".to_owned());
        req.causation = Some("cause-7".to_owned());
        gate.submit(req, CancellationToken::new()).await.expect("allowed");

        let row = applied_row(&pool, "attr-call").await;
        assert_eq!(row.scope, "ssh");
        assert_eq!(row.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(row.plan_id.as_deref(), Some("plan-42"));
        assert_eq!(row.causation.as_deref(), Some("cause-7"));

        // The S1 reader's session/scope filters return the stored row with
        // the caller's fields exactly as submitted.
        let filtered = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts {
                session_id: Some(session.clone()),
                scope: Some("ssh".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("S1 filtered load");
        assert_eq!(filtered.len(), 1, "session+scope filtered read returns the event");
        let filtered_row = &filtered[0];
        assert_eq!(filtered_row.event_id, "attr-call");
        assert_eq!(filtered_row.kind, WhiteboardKind::WriteApplied);
        assert_eq!(filtered_row.scope, "ssh");
        assert_eq!(filtered_row.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(filtered_row.plan_id.as_deref(), Some("plan-42"));
        assert_eq!(filtered_row.causation.as_deref(), Some("cause-7"));
    }

    #[tokio::test]
    async fn concurrent_same_call_id_serials_exactly_one_execution() {
        let (_dir, pool) = test_pool(8).await;
        let calls = Arc::new(AtomicUsize::new(0));
        // Cap 2 so two submits can pass the limiter at once — the in-flight
        // claim, not the semaphore, is what serializes the tool execution.
        let gate =
            gate_with(allow_engine(), calls.clone(), pool.clone(), PathBuf::from("/tmp"), None, 2);

        const N: usize = 5;
        let mut handles = Vec::new();
        for _ in 0..N {
            let gate = gate.clone();
            handles.push(tokio::spawn(async move {
                gate.submit(request("race-call"), CancellationToken::new()).await
            }));
        }

        let mut fresh = 0;
        let mut replayed = 0;
        for handle in handles {
            let outcome = handle.await.expect("join").expect("submit ok");
            assert_eq!(outcome.event_id, "race-call", "every outcome reports the same event_id");
            if outcome.replayed {
                replayed += 1;
                assert_eq!(outcome.gate_seq, 1, "replay reports the stored gate_seq");
            } else {
                fresh += 1;
                assert_eq!(outcome.gate_seq, 1);
                assert_eq!(outcome.result["data"]["probe"], json!(1), "the sole executor ran once");
            }
        }
        assert_eq!(fresh, 1, "exactly one submit executed the tool");
        assert_eq!(replayed, N - 1, "all other concurrent retries replayed");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "tool executed exactly once");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "one durable row for the call_id");
    }

    #[tokio::test]
    async fn per_agent_limiter_bounds_concurrency_but_agents_are_independent() {
        let (_dir, pool) = test_pool(8).await;
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let probe = Arc::new(GatedProbeTool {
            entered: entered_tx,
            release: Arc::new(Semaphore::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::new(AtomicUsize::new(0)),
        });
        let gate = gate_with(
            allow_engine(),
            Arc::new(AtomicUsize::new(0)),
            pool.clone(),
            PathBuf::from("/tmp"),
            Some(probe.clone()),
            1,
        );

        // Agent A: three concurrent write submissions on the gated probe tool.
        let mut a_handles = Vec::new();
        for i in 0..3 {
            let gate = gate.clone();
            a_handles.push(tokio::spawn(async move {
                let mut req = request(&format!("limiter-a-{i}"));
                req.agent_id = "agent-a".to_owned();
                req.tool = "gated_probe".to_owned();
                gate.submit(req, CancellationToken::new()).await
            }));
        }

        // Exactly one agent-a write is in flight; the others wait on their
        // agent's semaphore.
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .expect("A1 enters within timeout")
            .expect("entered channel open");
        assert_eq!(probe.active.load(Ordering::SeqCst), 1, "one agent-a write in flight");

        // Agent B completes independently while agent-a still holds its single
        // permit — agents do not block one another.
        let mut b_req = request("limiter-b-1");
        b_req.agent_id = "agent-b".to_owned();
        tokio::time::timeout(Duration::from_secs(10), gate.submit(b_req, CancellationToken::new()))
            .await
            .expect("agent-b write finishes within timeout")
            .expect("agent-b write allowed");
        assert_eq!(probe.active.load(Ordering::SeqCst), 1, "agent-a queue unaffected by agent-b");

        // Drain agent-a: releasing one permit lets the next queued write in;
        // concurrency never exceeds the cap.
        let mut entered_count = 1;
        while entered_count < 3 {
            probe.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
                .await
                .expect("next agent-a op enters within timeout")
                .expect("entered channel open");
            entered_count += 1;
            assert_eq!(probe.active.load(Ordering::SeqCst), 1, "never more than one in flight");
        }
        probe.release.add_permits(1); // let the final agent-a op finish

        let mut a_outcomes = Vec::with_capacity(3);
        for handle in a_handles {
            let outcome = handle.await.expect("join").expect("agent-a write allowed");
            assert!(!outcome.replayed, "agent-a writes are fresh executions");
            a_outcomes.push(outcome.gate_seq);
        }
        a_outcomes.sort_unstable();
        a_outcomes.dedup();
        assert_eq!(a_outcomes.len(), 3, "agent-a writes get distinct gate_seqs");
        assert_eq!(probe.max_active.load(Ordering::SeqCst), 1, "per-agent cap held throughout");
        assert_eq!(whiteboard_row_count(&pool).await, 4, "three agent-a + one agent-b writes");
    }

    #[tokio::test]
    async fn matching_base_version_applies_the_write_and_records_the_pre_image() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let base_hash = blake3::hash(b"base").to_hex().to_string();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        let mut request = real_fs_write("conf-1", "f.txt", "new");
        request.base_versions.insert("f.txt".to_owned(), base_hash.clone());
        let outcome =
            gate.submit(request, CancellationToken::new()).await.expect("match -> applied");
        assert!(!outcome.replayed, "fresh execution on a matching base");
        let row = applied_row(&pool, "conf-1").await;
        assert_eq!(
            row.pre_image_hash.as_deref(),
            Some(base_hash.as_str()),
            "the applied row records the pre-write bytes the caller claimed"
        );
    }

    #[tokio::test]
    async fn stale_base_version_conflicts_appends_nothing_and_does_not_touch_disk() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let stale_hash = blake3::hash(b"someone-elese-wrote-it").to_hex().to_string();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        let mut request = real_fs_write("conf-2", "f.txt", "hijack");
        request.base_versions.insert("f.txt".to_owned(), stale_hash);
        let error = gate
            .submit(request, CancellationToken::new())
            .await
            .expect_err("stale base_version must refuse");
        match error {
            GateError::Conflict { event_id, reason } => {
                assert_eq!(event_id, "conf-2");
                assert!(
                    reason.contains("base_version mismatch"),
                    "reason explains the race: {reason}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(
            whiteboard_row_count(&pool).await,
            0,
            "a conflict is never silently dropped — but it is also never logged as applied"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("file still there"),
            "base",
            "the conflicting write never reached disk"
        );
    }

    #[tokio::test]
    async fn base_version_with_an_absent_target_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        let mut request = real_fs_write("conf-3", "ghost.txt", "x");
        request
            .base_versions
            .insert("ghost.txt".to_owned(), blake3::hash(b"whatever").to_hex().to_string());
        let error = gate
            .submit(request, CancellationToken::new())
            .await
            .expect_err("a claimed base on a missing target must refuse");
        assert!(matches!(error, GateError::Conflict { .. }), "{error:?}");
        assert_eq!(whiteboard_row_count(&pool).await, 0, "nothing appended for an absent target");
    }

    #[tokio::test]
    async fn base_version_is_ignored_for_non_versioned_operations() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // `list` has no versioned target: the claim map is metadata, not a
        // claim, so it is ignored entirely.
        let mut request = filesystem_request("conf-4", "list", ".");
        request
            .base_versions
            .insert("stale.txt".to_owned(), blake3::hash(b"stale").to_hex().to_string());
        let outcome =
            gate.submit(request, CancellationToken::new()).await.expect("list is not versioned");
        assert!(!outcome.replayed);
        assert_eq!(
            applied_row(&pool, "conf-4").await.pre_image_hash,
            None,
            "no pre-image is captured for non-versioned operations"
        );
    }

    /// A gate whose executor is the REAL `FilesystemTool` rooted at `root` —
    /// the shape the supervisor builds in production, so injected writes both
    /// capture pre-images and materialize on disk.
    fn real_fs_gate(pool: sqlx::SqlitePool, root: PathBuf) -> Arc<WriteGate> {
        let policy = allow_engine();
        let mut registry = registry(None);
        let utf8_root =
            camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir root is utf-8");
        registry.register(Box::new(FilesystemTool::new(utf8_root)));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
        Arc::new(WriteGate::new(
            policy,
            executor,
            pool,
            Arc::new(FilePreImageReader::new(root.clone())),
            root,
            1,
        ))
    }

    #[tokio::test]
    async fn stamp_base_versions_fills_each_versioned_target_and_leaves_no_claim_for_absent() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // The supervisor's injection primitive (a reusable gate-level helper):
        // a versioned write stamps the target's current pre-image hash — the
        // same blake3 hex the gate's conflict check compares against.
        let mut write_req = filesystem_request("stamp-1", "write", "f.txt");
        stamp_base_versions(&gate, &mut write_req).await;
        assert_eq!(
            write_req.base_versions.get("f.txt").map(String::as_str),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "injection reads the same bytes the gate's conflict check compares against"
        );

        // A target with no current file carries no claim (fresh creates have
        // no prior version to conflict with -> documented LWW).
        let mut create_req = filesystem_request("stamp-2", "write", "ghost.txt");
        stamp_base_versions(&gate, &mut create_req).await;
        assert!(
            write_req.base_versions.contains_key("f.txt")
                && !create_req.base_versions.contains_key("ghost.txt"),
            "no file -> no claim to inject"
        );

        // A move stamps BOTH the source and the destination — both are
        // mutated by the operation.
        std::fs::write(root.join("src.txt"), "move me").expect("seed source");
        let mut move_req = filesystem_request("stamp-3", "move", "src.txt");
        move_req.input["destination"] = json!("dest.txt");
        stamp_base_versions(&gate, &mut move_req).await;
        assert_eq!(
            move_req.base_versions.get("src.txt").map(String::as_str),
            Some(blake3::hash(b"move me").to_hex().to_string().as_str()),
            "a move's source is mutated and is stamped"
        );
        assert!(
            !move_req.base_versions.contains_key("dest.txt"),
            "a fresh destination carries no claim to conflict with"
        );

        // A copy stamps only the destination: the source is read-only (its
        // pre-image is still attributed on the WAL row).
        let mut copy_req = filesystem_request("stamp-4", "copy", "src.txt");
        copy_req.input["destination"] = json!("copy.txt");
        stamp_base_versions(&gate, &mut copy_req).await;
        assert!(
            !copy_req.base_versions.contains_key("src.txt"),
            "the read-only copy source is never stamped as a claim"
        );
        assert!(
            !copy_req.base_versions.contains_key("copy.txt"),
            "a fresh copy destination carries no claim to conflict with"
        );

        // Non-versioned operations and non-filesystem tools carry no claims.
        let mut listed = filesystem_request("stamp-5", "list", ".");
        stamp_base_versions(&gate, &mut listed).await;
        assert!(listed.base_versions.is_empty(), "non-versioned operation -> no claims");
        let mut other = request("stamp-6");
        stamp_base_versions(&gate, &mut other).await;
        assert!(other.base_versions.is_empty(), "non-filesystem tool -> no claims");
    }

    #[tokio::test]
    async fn stamp_base_versions_declare_wins_per_target() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("src.txt"), "move me").expect("seed source");
        std::fs::write(root.join("dest.txt"), "old dest").expect("seed destination");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // A caller-declared claim always wins PER TARGET — never clobbered,
        // even when stale (the gate surfaces the conflict, not the stamp).
        let mut request = filesystem_request("stamp-dw-1", "move", "src.txt");
        request.input["destination"] = json!("dest.txt");
        request.base_versions.insert("src.txt".to_owned(), "declared-stale".to_owned());
        stamp_base_versions(&gate, &mut request).await;
        assert_eq!(
            request.base_versions.get("src.txt").map(String::as_str),
            Some("declared-stale"),
            "a declared claim is never overwritten by the injection"
        );
        // The undeclared destination is still stamped by the injection.
        assert_eq!(
            request.base_versions.get("dest.txt").map(String::as_str),
            Some(blake3::hash(b"old dest").to_hex().to_string().as_str()),
            "declare-wins is per target: the uninjected destination still gets stamped"
        );
    }

    #[tokio::test]
    async fn auto_injected_matching_base_version_applies_and_materializes() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // What the supervisor does at request arrival: stamp the request with
        // the target's current pre-image hash (same reader + blake3 hex the
        // gate's conflict check compares against).
        let mut request = real_fs_write("auto-1", "f.txt", "new");
        stamp_base_versions(&gate, &mut request).await;
        assert_eq!(
            request.base_versions.get("f.txt").map(String::as_str),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "the request carries the arrival-time claim"
        );
        let outcome =
            gate.submit(request, CancellationToken::new()).await.expect("injected match applies");
        assert!(!outcome.replayed, "fresh execution on a matching injected base");
        assert_eq!(
            applied_row(&pool, "auto-1").await.pre_image_hash.as_deref(),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "the applied row records the injected pre-write hash"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "new",
            "the injected-base write materializes"
        );
    }

    #[tokio::test]
    async fn stale_auto_injected_base_version_conflicts_when_sibling_write_lands_first() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("f.txt"), "base").expect("seed file");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // Stamp at request arrival...
        let mut request = real_fs_write("auto-2", "f.txt", "mine");
        stamp_base_versions(&gate, &mut request).await;
        assert_eq!(
            request.base_versions.get("f.txt").map(String::as_str),
            Some(blake3::hash(b"base").to_hex().to_string().as_str()),
            "the request carries the arrival-time claim"
        );

        // ...then a sibling write lands on the same target before the gate
        // processes the request (the sub-millisecond race D5 exists for).
        std::fs::write(root.join("f.txt"), "sibling-interloper").expect("sibling write");

        let error = gate
            .submit(request, CancellationToken::new())
            .await
            .expect_err("stale injected base must refuse");
        assert!(matches!(error, GateError::Conflict { .. }), "expected Conflict, got {error:?}");
        assert_eq!(
            whiteboard_row_count(&pool).await,
            0,
            "the conflict appends nothing to the whiteboard"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "sibling-interloper",
            "the sibling's write is untouched — no silent overwrite"
        );
    }

    #[tokio::test]
    async fn move_with_matching_claims_applies_and_records_both_pre_images() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("src.txt"), "src-v1").expect("seed source");
        std::fs::write(root.join("dest.txt"), "dest-v0").expect("seed destination");
        let src_hash = blake3::hash(b"src-v1").to_hex().to_string();
        let dest_hash = blake3::hash(b"dest-v0").to_hex().to_string();
        let (_pool_dir, pool) = test_pool(8).await;
        // Stub executor: the WAL contract under test here is the multi-target
        // pre-image attribution, not the tool's disk semantics (the real
        // FilesystemTool refuses an existing destination at the VFS layer, and
        // real move materialization is covered end-to-end by the supervisor
        // move tests).
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // A move mutates BOTH the source and the destination; declared claims
        // for both, matching the current pre-images, let the move apply.
        let mut request = fs_move("move-1", "src.txt", "dest.txt");
        request.base_versions.insert("src.txt".to_owned(), src_hash.clone());
        request.base_versions.insert("dest.txt".to_owned(), dest_hash.clone());
        let outcome =
            gate.submit(request, CancellationToken::new()).await.expect("matching move applies");
        assert!(!outcome.replayed, "fresh execution on matching claims");

        // Attribution: the column stays the PRIMARY (destination) pre-image,
        // and the WAL payload carries BOTH the source and destination
        // pre-images — source-side attribution the audit gap demanded.
        let row = applied_row(&pool, "move-1").await;
        assert_eq!(row.pre_image_hash.as_deref(), Some(dest_hash.as_str()));
        assert_eq!(row.payload["pre_images"]["src.txt"], json!(src_hash));
        assert_eq!(row.payload["pre_images"]["dest.txt"], json!(dest_hash));
    }

    #[tokio::test]
    async fn stale_move_source_claim_conflicts_appends_nothing_and_touches_no_disk() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("src.txt"), "src-v1").expect("seed source");
        std::fs::write(root.join("dest.txt"), "dest-v0").expect("seed destination");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // The audit gap: a stale SOURCE claim simulated a concurrent write to
        // the move's source. The source is mutated by the move, so the gate
        // must refuse loudly instead of moving the file away over the write.
        let mut request = fs_move("move-2", "src.txt", "dest.txt");
        request.base_versions.insert(
            "src.txt".to_owned(),
            blake3::hash(b"someone-else-wrote-it").to_hex().to_string(),
        );
        let error = gate
            .submit(request, CancellationToken::new())
            .await
            .expect_err("stale source claim must refuse");
        match error {
            GateError::Conflict { event_id, reason } => {
                assert_eq!(event_id, "move-2");
                assert!(
                    reason.contains("src.txt"),
                    "the message names the conflicting target path: {reason}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(
            whiteboard_row_count(&pool).await,
            0,
            "a source contradiction appends nothing to the whiteboard"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src.txt")).expect("read"),
            "src-v1",
            "the source is not moved away over a fresh sibling write"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("dest.txt")).expect("read"),
            "dest-v0",
            "the destination is untouched too"
        );
    }

    #[tokio::test]
    async fn move_absent_source_conflicts_when_declared_but_applies_when_undeclared() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("dest.txt"), "dest-v0").expect("seed destination");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // A declared claim on a source that no longer exists (a sibling moved
        // it first) refuses with a Conflict and no WAL row.
        let mut declared = fs_move("move-3", "ghost-src.txt", "dest.txt");
        declared
            .base_versions
            .insert("ghost-src.txt".to_owned(), blake3::hash(b"gone").to_hex().to_string());
        let error = gate
            .submit(declared, CancellationToken::new())
            .await
            .expect_err("an absent source with a declared claim must refuse");
        assert!(matches!(error, GateError::Conflict { .. }), "{error:?}");
        assert_eq!(whiteboard_row_count(&pool).await, 0, "zero WAL rows for the conflict");

        // An absent source WITHOUT a declared claim carries no claim (fresh-
        // create LWW semantics): the move flows through the gate normally.
        let undecided = fs_move("move-4", "ghost-src.txt", "other.txt");
        let outcome = gate.submit(undecided, CancellationToken::new()).await;
        assert!(outcome.is_ok(), "an undeclared absent source is not a conflict: {outcome:?}");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "the move still appends its WAL row");
        let row = applied_row(&pool, "move-4").await;
        assert_eq!(row.pre_image_hash, None, "no pre-image for absent targets");
    }

    #[tokio::test]
    async fn copy_destination_claim_enforced_but_copy_source_claim_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("orig.txt"), "copy me").expect("seed source");
        let orig_hash = blake3::hash(b"copy me").to_hex().to_string();
        std::fs::write(root.join("out.txt"), "old out").expect("seed destination");
        let (_pool_dir, pool) = test_pool(8).await;
        // Stub executor: the contract under test is the conflict/attribution
        // policy per target, not the VFS's copy disk semantics.
        let gate = gate(allow_engine(), Arc::new(AtomicUsize::new(0)), pool.clone(), root.clone());

        // A stale claim on the DESTINATION refuses: the destination is mutated
        // by the copy (a concurrent write to it would be overwritten).
        let mut bad_dest = fs_copy("copy-1", "orig.txt", "out.txt");
        bad_dest.base_versions.insert(
            "out.txt".to_owned(),
            blake3::hash(b"not-what-is-on-disk").to_hex().to_string(),
        );
        let error = gate
            .submit(bad_dest, CancellationToken::new())
            .await
            .expect_err("a stale destination claim must refuse");
        assert!(matches!(error, GateError::Conflict { .. }), "{error:?}");
        assert_eq!(whiteboard_row_count(&pool).await, 0, "zero WAL rows for the conflict");

        // A stale claim on the SOURCE is ignored for conflict: the copy reads
        // whatever is there (the source is read-only by design, see
        // [`versioned_targets`]), so the copy applies.
        let mut bad_src = fs_copy("copy-2", "orig.txt", "out.txt");
        bad_src.base_versions.insert(
            "orig.txt".to_owned(),
            blake3::hash(b"an-outdated-source-view").to_hex().to_string(),
        );
        let outcome = gate.submit(bad_src, CancellationToken::new()).await;
        assert!(outcome.is_ok(), "a copy source claim is ignored: {outcome:?}");

        // Source-side attribution: the WAL payload carries the copy source's
        // pre-image even though it is not conflict-checked.
        let row = applied_row(&pool, "copy-2").await;
        assert_eq!(
            row.payload["pre_images"]["orig.txt"],
            json!(orig_hash),
            "the read-only copy source is attributed on the WAL row"
        );
        assert_eq!(
            row.payload["pre_images"]["out.txt"],
            json!(blake3::hash(b"old out").to_hex().to_string()),
            "the destination pre-image is attributed too"
        );
    }

    #[tokio::test]
    async fn create_checkpoint_materializes_consistent_cut_for_per_agent_revert() {
        let (_dir, pool) = test_pool(1).await;
        let gate = gate(
            allow_engine(),
            Arc::new(AtomicUsize::new(0)),
            pool.clone(),
            PathBuf::from("/tmp"),
        );

        // Two agents write the same path through the gate (stub executor; the
        // contract under test is the WAL projection, not disk semantics).
        let mut req_a = filesystem_request("cp-a", "write", "shared.txt");
        req_a.input["content"] = json!("from-a");
        gate.submit(req_a, CancellationToken::new()).await.expect("agent-a applies");

        let mut req_b = filesystem_request("cp-b", "write", "shared.txt");
        req_b.agent_id = "agent-b".to_owned();
        req_b.input["content"] = json!("from-b");
        gate.submit(req_b, CancellationToken::new()).await.expect("agent-b applies");

        // Cut at seq 1: only agent-a's write is inside the consistent cut.
        let cut = gate.create_checkpoint_at(1, None).await.expect("checkpoint at 1");
        assert_eq!(cut.gate_seq, 1);
        assert_eq!(cut.files.get("shared.txt").map(String::as_str), Some("from-a"));

        // Head checkpoint ("checkpoint now"): both writes folded, and the
        // log's total order decides the surviving content.
        let head = gate.create_checkpoint(None).await.expect("checkpoint at head");
        assert_eq!(head.gate_seq, 2, "the head is the latest assigned gate_seq");
        assert_eq!(head.files.get("shared.txt").map(String::as_str), Some("from-b"));

        // Per-agent revert (D5 ii): restore to the cut, replay the tail
        // without agent-b — shared.txt stays exactly where the cut left it.
        let events =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        let reverted = cut.replay_tail_excluding(&events, Some("agent-b"));
        assert_eq!(reverted.get("shared.txt").map(String::as_str), Some("from-a"));
    }

    // ---- ADR-60 D5 second half: hunk-aware staging + explicit lock tokens ----

    /// Fifteen distinct lines — neighbors never repeat, so the histogram diff
    /// of a single-line edit is exactly one clean hunk.
    fn numbered_file() -> String {
        let joined = (1..=15).map(|n| format!("line-{n:02}")).collect::<Vec<_>>().join("\n");
        joined + "\n"
    }

    fn replace_line(content: &str, index: usize, with: &str) -> String {
        let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
        lines[index] = with.to_owned();
        lines.join("\n") + "\n"
    }

    #[tokio::test]
    async fn hunk_aware_staging_applies_disjoint_sibling_edits_without_conflict() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let seed = numbered_file();
        std::fs::write(root.join("shared.txt"), &seed).expect("seed file");
        let base_hash = blake3::hash(seed.as_bytes()).to_hex().to_string();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // Agent-a rewrites line 2 off the shared base and lands first.
        let a_content = replace_line(&seed, 1, "A-edit");
        let mut req_a = real_fs_write("stage-a", "shared.txt", &a_content);
        req_a.base_versions.insert("shared.txt".to_owned(), base_hash.clone());
        gate.submit(req_a, CancellationToken::new()).await.expect("agent-a applies");

        // Agent-b still holds the ORIGINAL base view (its claim predates a's
        // landing) and rewrites line 14 — a disjoint hunk. Instead of the
        // old wholesale refusal, the write stages onto a's state.
        let b_content = replace_line(&seed, 13, "B-edit");
        let mut req_b = real_fs_write("stage-b", "shared.txt", &b_content);
        req_b.agent_id = "agent-b".to_owned();
        req_b.base_versions.insert("shared.txt".to_owned(), base_hash); // stale on purpose

        let outcome =
            gate.submit(req_b, CancellationToken::new()).await.expect("disjoint hunks stage");
        assert!(!outcome.replayed, "a staged write is a fresh execution");

        // The executed content keeps BOTH edits — not b's verbatim text,
        // which would have silently reverted a's line.
        let expected_merged = replace_line(&a_content, 13, "B-edit");
        assert_eq!(
            std::fs::read_to_string(root.join("shared.txt")).expect("read"),
            expected_merged,
            "sibling's line-2 edit AND late writer's line-14 edit both survive"
        );

        // Attribution stays honest: the pre-image is what was actually on
        // disk (a's state), and the WAL row records the merged input plus a
        // staging note for the manual-resolution trail.
        let row = applied_row(&pool, "stage-b").await;
        assert_eq!(
            row.pre_image_hash.as_deref(),
            Some(blake3::hash(a_content.as_bytes()).to_hex().to_string().as_str()),
            "the applied row attributes the true pre-write state"
        );
        assert_eq!(row.payload["input"]["content"], json!(expected_merged));
        assert_eq!(row.payload["hunk_staging"][0]["target"], json!("shared.txt"));
        assert_eq!(
            row.payload["hunk_staging"][0]["staged_onto_hash"],
            json!(blake3::hash(a_content.as_bytes()).to_hex().to_string()),
            "the staging note names the exact state the write was built on"
        );
    }

    #[tokio::test]
    async fn same_hunk_collision_conflicts_loudly_for_manual_resolution() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        let seed = numbered_file();
        std::fs::write(root.join("shared.txt"), &seed).expect("seed file");
        let base_hash = blake3::hash(seed.as_bytes()).to_hex().to_string();
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // Agent-a rewrites line 2 off the shared base and lands first.
        let a_content = replace_line(&seed, 1, "A-edit");
        let mut req_a = real_fs_write("collide-a", "shared.txt", &a_content);
        req_a.base_versions.insert("shared.txt".to_owned(), base_hash.clone());
        gate.submit(req_a, CancellationToken::new()).await.expect("agent-a applies");

        // Agent-b rewrites THE SAME LINE off the same stale base: the hunks
        // overlap, so the write refuses loudly instead of picking a winner.
        let b_content = replace_line(&seed, 1, "B-edit");
        let mut req_b = real_fs_write("collide-b", "shared.txt", &b_content);
        req_b.agent_id = "agent-b".to_owned();
        req_b.base_versions.insert("shared.txt".to_owned(), base_hash);

        let error = gate
            .submit(req_b, CancellationToken::new())
            .await
            .expect_err("same-hunk collision must refuse loudly");
        match error {
            GateError::Conflict { event_id, reason } => {
                assert_eq!(event_id, "collide-b");
                assert!(
                    reason.contains("same-hunk collision"),
                    "the reason names the collision class: {reason}"
                );
                assert!(
                    reason.contains("manual resolution"),
                    "the reason points at the manual-resolution path: {reason}"
                );
                assert!(reason.contains("shared.txt"), "the reason names the target: {reason}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        assert_eq!(
            whiteboard_row_count(&pool).await,
            1,
            "only agent-a's applied row — the collision appends nothing"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("shared.txt")).expect("read"),
            a_content,
            "the colliding write never reached disk"
        );
    }

    #[tokio::test]
    async fn explicit_lock_blocks_foreign_writes_with_holder_info_until_release() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("hot.txt"), "hot-v1").expect("seed file");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // Agent-a reserves the hot shared file for exclusive write.
        let token =
            gate.acquire_lock("hot.txt", "agent-a", Duration::from_secs(60)).expect("lock granted");
        assert_eq!(token.path, "hot.txt");
        assert_eq!(token.agent_id, "agent-a");

        // A foreign write against the locked file is refused LOUDLY with
        // holder info — before any WAL append, before touching disk.
        let mut foreign = real_fs_write("lock-1", "hot.txt", "sneaky-bypass");
        foreign.agent_id = "agent-b".to_owned();
        let error = gate.submit(foreign, CancellationToken::new()).await.expect_err("locked");
        match error {
            GateError::Conflict { event_id, reason } => {
                assert_eq!(event_id, "lock-1");
                assert!(
                    reason.contains("agent-a") && reason.contains("locked for exclusive write"),
                    "the conflict names the lock holder: {reason}"
                );
            }
            other => panic!("expected Conflict with lock info, got {other:?}"),
        }
        assert_eq!(whiteboard_row_count(&pool).await, 0, "the refused write appends nothing");
        assert_eq!(
            std::fs::read_to_string(root.join("hot.txt")).expect("read"),
            "hot-v1",
            "the locked file is untouched"
        );

        // The reserving owner writes through its own lock.
        let mut owned = real_fs_write("lock-2", "hot.txt", "owner-write");
        owned.agent_id = "agent-a".to_owned();
        gate.submit(owned, CancellationToken::new()).await.expect("holder writes through own lock");

        // A stale/forged token cannot release someone's reservation...
        let mut forged = token.clone();
        forged.token_id = "forged-id".to_owned();
        assert!(
            matches!(gate.release_lock(&forged), Err(GateError::Locked { ref holder, .. }) if holder == "agent-a"),
            "release with a wrong token id is refused with holder info"
        );
        // ...and the genuine token frees the path.
        gate.release_lock(&token).expect("genuine release succeeds");

        let mut after = real_fs_write("lock-3", "hot.txt", "free-now");
        after.agent_id = "agent-b".to_owned();
        let outcome = gate.submit(after, CancellationToken::new()).await;
        assert!(outcome.is_ok(), "the write flows once the reservation is released: {outcome:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("hot.txt")).expect("read"),
            "free-now",
            "the released write materializes"
        );
    }

    #[tokio::test]
    async fn expired_lock_lapses_and_stops_blocking_writes() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("hot.txt"), "hot-v1").expect("seed file");
        let (_pool_dir, pool) = test_pool(8).await;
        let gate = real_fs_gate(pool.clone(), root.clone());

        // A very short TTL: after it lapses (purged lazily on the next
        // table touch), a foreign agent may write again — no sweeper needed.
        let token =
            gate.acquire_lock("hot.txt", "agent-a", Duration::from_millis(50)).expect("granted");
        tokio::time::sleep(Duration::from_millis(120)).await;

        let mut foreign = real_fs_write("expiry-1", "hot.txt", "post-lapse");
        foreign.agent_id = "agent-b".to_owned();
        let outcome = gate.submit(foreign, CancellationToken::new()).await;
        assert!(outcome.is_ok(), "an expired reservation no longer blocks writes: {outcome:?}");

        // Releasing an already-lapsed reservation is idempotent success: the
        // desired state is already reached.
        gate.release_lock(&token).expect("release after lapse still succeeds");
        assert_eq!(whiteboard_row_count(&pool).await, 1, "only the post-lapse write applied");
    }
}
