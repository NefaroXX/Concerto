//! Durable whiteboard event log (ADR-60 D3) — S1 vertical-slice substrate.
//!
//! The whiteboard is the append-only source of truth for concurrent-agent
//! runs: findings, decisions, write applications/rejections, failures, plan/
//! design artifacts and consolidations, stored in the `whiteboard_events`
//! table (migration `026_whiteboard_events`). Ordering is a central sequencer
//! (research brief §4, option A): `gate_seq` is a global total order — the
//! consistent-cut coordinate for checkpoints ("everything ≤ seq S") — and
//! `agent_seq` is a per-agent monotonic sequence for per-agent recovery.
//!
//! Both sequences are assigned at insert time by the append path under a
//! `BEGIN IMMEDIATE` transaction: SQLite's write lock is taken at `BEGIN`, so
//! concurrent appends serialize and each `COALESCE(MAX(..),0)+1` is computed
//! against a stable snapshot — no duplicate `gate_seq`, no retries needed.
//! This is the log the supervisor's write gate appends to in S2; the gate,
//! IPC, and supervisor are out of scope here.
//!
//! Idempotency: appending an already-present `event_id` is a no-op that
//! returns the existing row (`INSERT OR IGNORE`), implementing the research
//! brief's at-least-once + dedup-by-`event_id` contract so replays and retries
//! are safe.
//!
//! These are free functions over `&SqlitePool`, mirroring `plan_bindings`.
//! They do not take a `CancellationToken`: the crate's standalone CRUD helpers
//! (`plan_bindings`) don't thread one either, so none is invented here.

use serde::{Deserialize, Serialize};
use sqlx::query_as;
use sqlx::AssertSqlSafe;

use crate::SessionError;

/// Typed kind of a whiteboard event, stored kebab-case (`serde(rename_all)`).
///
/// Deliberately not a free-form string: the kind set is closed because the
/// log is the durable substrate for projections and deterministic replay,
/// which need stable kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhiteboardKind {
    Finding,
    Decision,
    WriteApplied,
    WriteRejected,
    Failure,
    PlanApproved,
    TaskGraph,
    SubtaskStarted,
    SubtaskCompleted,
    SubtaskFailed,
    ReviewState,
    Consolidation,
    MemoryFact,
    // ADR-65 evidence spine (additive). These are runtime-appended observed
    // facts; older binaries reading the log see them as unknown kinds and must
    // treat them as opaque — already the case for the JSON payload design.
    ToolExecuted,
    WorkspaceSnapshot,
    /// ADR-65 §5: the DesignDoc produced for a planning stage, recorded as an
    /// evidence-backed CLAIM. Payload is the serialized `DesignDoc`; the
    /// deterministic verifier later resolves that claim against grounded
    /// observations to either bind it (Verified) or quarantine it. Note this
    /// is an *assertion about the intended workspace contract*, not a record
    /// of observed reality — so it is spelled `design-doc`, not folded into
    /// the runtime-observed kinds above.
    DesignDoc,
}

impl WhiteboardKind {
    /// The storage/kebab-case string form of this kind.
    ///
    /// Kept in sync with `serde(rename_all = "kebab-case")` above and used for
    /// `content_hash` canonicalization (which needs the plain string, not a
    /// quoted JSON encoding) and for row decoding.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Decision => "decision",
            Self::WriteApplied => "write-applied",
            Self::WriteRejected => "write-rejected",
            Self::Failure => "failure",
            Self::PlanApproved => "plan-approved",
            Self::TaskGraph => "task-graph",
            Self::SubtaskStarted => "subtask-started",
            Self::SubtaskCompleted => "subtask-completed",
            Self::SubtaskFailed => "subtask-failed",
            Self::ReviewState => "review-state",
            Self::Consolidation => "consolidation",
            Self::MemoryFact => "memory-fact",
            Self::ToolExecuted => "tool-executed",
            Self::WorkspaceSnapshot => "workspace-snapshot",
            Self::DesignDoc => "design-doc",
        }
    }

    fn from_db_string(s: &str) -> Option<Self> {
        serde_json::from_str::<Self>(&format!("\"{s}\"")).ok()
    }
}

/// The topic set a whiteboard subscriber subscribes to (ADR-60 D3).
///
/// A scope is a list of [`WhiteboardKind`] topics; an event whose `kind` is in
/// the list matches the scope. Scopes are declared at handshake (`protocol
/// 0.2.0`, `IpcParams::Handshake.subscriptions`) and persisted as JSON in
/// `whiteboard_subscriptions.scopes`; each topic serializes kebab-case via the
/// kind enum, so the wire form is stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhiteboardScope {
    /// The [`WhiteboardKind`] topics in this scope.
    pub topics: Vec<WhiteboardKind>,
}

/// One row of `whiteboard_subscriptions` — a subscriber's registered scopes
/// and its acknowledged consistent-cut cursor (ADR-60 D3).
///
/// `cursor_gate_seq` means "everything ≤ this `gate_seq` is already acked and
/// applied" and advances only on the subscriber's `AckWhiteboard`
/// ([`ack_whiteboard_subscription`]); it never advances at enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhiteboardSubscription {
    /// Subscriber identity — the registered agent_id.
    pub subscriber_id: String,
    /// The subscribed topic scopes, persisted as compact JSON in `scopes`.
    pub scopes: Vec<WhiteboardScope>,
    /// The acknowledged consistent-cut coordinate (`gate_seq`).
    pub cursor_gate_seq: u64,
}

/// A stored whiteboard event, mirroring one row of `whiteboard_events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhiteboardEvent {
    /// Caller-generated idempotency key (uuid v4 per ADR-60 D3). Re-appending
    /// an existing id returns the existing row (dedup by `event_id`).
    pub event_id: String,
    /// Global total order, assigned at insert time. Excluded from
    /// `content_hash` — a log-assigned sequencing artifact, not content.
    pub gate_seq: u64,
    /// The agent that produced the event.
    pub agent_id: String,
    /// Per-agent monotonic sequence, assigned at insert time. Excluded from
    /// `content_hash` for the same reason as `gate_seq`.
    pub agent_seq: u64,
    /// Typed event kind (kebab-case on the wire / in the column).
    pub kind: WhiteboardKind,
    /// Subscription/topic filter; empty string for unscoped events.
    pub scope: String,
    /// Optional owning session (nullable, no FK — events outlive pruning).
    pub session_id: Option<String>,
    /// Future #152 structured-state key (ADR-60 D7); not a FK.
    pub plan_id: Option<String>,
    /// Trigger `event_id` or HLC string that caused this event (optional).
    pub causation: Option<String>,
    /// JSON payload.
    pub payload: serde_json::Value,
    /// Deterministic blake3 fingerprint of the canonical fields (see
    /// [`compute_content_hash`]).
    pub content_hash: String,
    /// Hash of the target before apply, for write events — filled by the
    /// write gate in S2; `None` for non-write events.
    pub pre_image_hash: Option<String>,
    /// Unix epoch milliseconds (UTC).
    pub created_at: i64,
}

/// Caller-supplied fields for appending a whiteboard event.
///
/// `event_id` is generated by the caller (uuid v4 per ADR-60 D3); the lib
/// does not mint ids, so a retry/replay reuses the same id and dedups.
/// `created_at` is caller-provided so a replay reproduces the original wall
/// time (world-time vs ingestion-time distinction). `gate_seq` and `agent_seq`
/// are **not** inputs — the log assigns them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewWhiteboardEvent {
    /// Caller-generated idempotency key (uuid v4 per ADR-60 D3).
    pub event_id: String,
    /// Agent that produced the event.
    pub agent_id: String,
    /// Typed event kind.
    pub kind: WhiteboardKind,
    /// Subscription/topic filter; defaults to `""`.
    #[serde(default)]
    pub scope: String,
    /// Optional owning session (no FK).
    pub session_id: Option<String>,
    /// Future #152 structured-state key (no FK).
    pub plan_id: Option<String>,
    /// Trigger `event_id` or HLC string that caused this event (optional).
    pub causation: Option<String>,
    /// JSON payload (serialized compactly for `content_hash`).
    pub payload: serde_json::Value,
    /// Pre-image hash for write events (filled by the gate in S2); `None` for
    /// non-write events.
    pub pre_image_hash: Option<String>,
    /// Unix epoch milliseconds (UTC).
    pub created_at: i64,
}

/// Cursor options for [`load_whiteboard_events`] — the per-subscriber
/// catch-up primitive.
#[derive(Debug, Clone)]
pub struct WhiteboardLoadOpts {
    /// Return only events with `gate_seq` strictly greater than this
    /// (exclusive cursor). `0` starts at the beginning of the log.
    pub after_gate_seq: u64,
    /// Restrict to one session (optional).
    pub session_id: Option<String>,
    /// Restrict to one scope/topic (optional).
    pub scope: Option<String>,
    /// Maximum rows to return. Defaults to 200.
    pub limit: usize,
}

impl Default for WhiteboardLoadOpts {
    fn default() -> Self {
        Self { after_gate_seq: 0, session_id: None, scope: None, limit: 200 }
    }
}

/// Deterministic blake3 fingerprint of a [`NewWhiteboardEvent`]'s canonical
/// content fields.
///
/// Canonical fields, in order, each length-prefixed (8-byte big-endian length
/// followed by the raw bytes) so field boundaries are unambiguous regardless
/// of the bytes' content:
///
/// 1. `event_id`
/// 2. `agent_id`
/// 3. `kind` (kebab-case string, [`WhiteboardKind::as_str`])
/// 4. `scope`
/// 5. `session_id` (empty when `None`)
/// 6. `plan_id` (empty when `None`)
/// 7. `causation` (empty when `None`)
/// 8. `payload` (compact JSON — `serde_json::Value`'s object map sorts keys,
///    so equal values produce equal JSON)
/// 9. `pre_image_hash` (empty when `None`)
/// 10. `created_at` (8-byte big-endian integer)
///
/// `gate_seq` and `agent_seq` are **not** included: they are log-assigned
/// sequencing artifacts, not event content, so replay verification can
/// recompute the expected hash from the caller-attested fields alone.
pub fn compute_content_hash(event: &NewWhiteboardEvent) -> Result<String, SessionError> {
    let payload_json = serde_json::to_string(&event.payload)?;
    // Capacity floor only: 10 canonical fields each contribute at least their
    // 8-byte length prefix; the buffer grows as needed.
    let mut buf: Vec<u8> = Vec::with_capacity(10 * 8);
    push_field(&mut buf, event.event_id.as_bytes());
    push_field(&mut buf, event.agent_id.as_bytes());
    push_field(&mut buf, event.kind.as_str().as_bytes());
    push_field(&mut buf, event.scope.as_bytes());
    push_field(&mut buf, event.session_id.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, event.plan_id.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, event.causation.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, payload_json.as_bytes());
    push_field(&mut buf, event.pre_image_hash.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, &event.created_at.to_be_bytes());
    Ok(blake3::hash(&buf).to_hex().to_string())
}

/// Append a length-prefixed field to the canonical hash buffer: an 8-byte
/// big-endian length followed by the raw bytes.
fn push_field(buf: &mut Vec<u8>, value: &[u8]) {
    buf.extend_from_slice(&(value.len() as u64).to_be_bytes());
    buf.extend_from_slice(value);
}

/// Append a whiteboard event and return the stored row.
///
/// `gate_seq` (global) and `agent_seq` (per-agent) are assigned inside a
/// `BEGIN IMMEDIATE` transaction: SQLite takes the write lock at `BEGIN`, so
/// concurrent appends serialize and each `COALESCE(MAX(..),0)+1` is computed
/// against a stable snapshot — every appended event gets a unique, strictly
/// increasing `gate_seq` and a per-agent monotonic `agent_seq`.
///
/// Idempotent by `event_id`: re-appending an existing id (retry or replay) is
/// a no-op that returns the existing row (`INSERT OR IGNORE`), per the
/// at-least-once + dedup contract. The returned row is the DB authority —
/// duplicate inserts never mutate sequencing state.
pub async fn append_whiteboard_event(
    pool: &sqlx::SqlitePool,
    event: &NewWhiteboardEvent,
) -> Result<WhiteboardEvent, SessionError> {
    let payload_json = serde_json::to_string(&event.payload)?;
    let content_hash = compute_content_hash(event)?;

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    sqlx::query(
        "INSERT OR IGNORE INTO whiteboard_events
             (event_id, gate_seq, agent_id, agent_seq, kind, scope, session_id,
              plan_id, causation, payload, content_hash, pre_image_hash, created_at)
         SELECT ?, (SELECT COALESCE(MAX(gate_seq), 0) + 1 FROM whiteboard_events), ?,
                (SELECT COALESCE(MAX(agent_seq), 0) + 1 FROM whiteboard_events WHERE agent_id = ?),
                ?, ?, ?, ?, ?, ?, ?, ?, ?",
    )
    .bind(&event.event_id)
    .bind(&event.agent_id)
    .bind(&event.agent_id)
    .bind(event.kind.as_str())
    .bind(&event.scope)
    .bind(&event.session_id)
    .bind(&event.plan_id)
    .bind(&event.causation)
    .bind(&payload_json)
    .bind(&content_hash)
    .bind(&event.pre_image_hash)
    .bind(event.created_at)
    .execute(&mut *tx)
    .await?;

    // Whether the row was freshly inserted or already existed (INSERT OR
    // IGNORE no-op), the stored row is the authority returned to the caller.
    let stored = load_whiteboard_event(&mut *tx, &event.event_id).await?;

    tx.commit().await?;
    Ok(stored)
}

/// Cursor-based reader for catch-up / subscription slices.
///
/// Returns events with `gate_seq > opts.after_gate_seq` (exclusive), filtered
/// by the optional `session_id`/`scope`, ordered by `gate_seq` ascending, up
/// to `opts.limit` rows (default 200). This is the per-subscriber catch-up
/// primitive: a subscriber tracks its last-seen `gate_seq` and keeps paging
/// forward.
pub async fn load_whiteboard_events(
    pool: &sqlx::SqlitePool,
    opts: &WhiteboardLoadOpts,
) -> Result<Vec<WhiteboardEvent>, SessionError> {
    let mut sql = format!("SELECT {EVENT_COLUMNS} FROM whiteboard_events WHERE gate_seq > ?");
    if opts.session_id.is_some() {
        sql.push_str(" AND session_id = ?");
    }
    if opts.scope.is_some() {
        sql.push_str(" AND scope = ?");
    }
    sql.push_str(" ORDER BY gate_seq ASC LIMIT ?");

    // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is assembled solely from static
    // fragments and the const `EVENT_COLUMNS`; every filter value is bound via `?`.
    let mut query =
        query_as::<_, WhiteboardEventRow>(AssertSqlSafe(sql)).bind(opts.after_gate_seq as i64);
    if let Some(session_id) = &opts.session_id {
        query = query.bind(session_id);
    }
    if let Some(scope) = &opts.scope {
        query = query.bind(scope);
    }
    let rows = query.bind(opts.limit as i64).fetch_all(pool).await?;
    rows.into_iter().map(WhiteboardEvent::try_from).collect()
}

/// Load every event keyed to a plan, ordered by `gate_seq` ascending.
///
/// Future #152 structured-state reads (ADR-60 D7): the DesignDoc, task graph,
/// and decision/action ledger are whiteboard events keyed by `plan_id`, so
/// Execute can load the structured object plus ledger instead of re-deriving
/// it from prose.
pub async fn load_whiteboard_events_by_plan(
    pool: &sqlx::SqlitePool,
    plan_id: &str,
) -> Result<Vec<WhiteboardEvent>, SessionError> {
    let sql = format!(
        "SELECT {EVENT_COLUMNS} FROM whiteboard_events WHERE plan_id = ? ORDER BY gate_seq ASC"
    );
    // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is assembled solely from static
    // fragments and the const `EVENT_COLUMNS`; every filter value is bound via `?`.
    let rows =
        query_as::<_, WhiteboardEventRow>(AssertSqlSafe(sql)).bind(plan_id).fetch_all(pool).await?;
    rows.into_iter().map(WhiteboardEvent::try_from).collect()
}

/// ADR-60 D5 consistent-cut read: every event with `gate_seq <= max_gate_seq`
/// ("everything ≤ seq S"), optionally restricted to one session, ordered by
/// `gate_seq` ascending.
///
/// Unlike [`load_whiteboard_events`] there is no exclusive cursor and no
/// limit: a gate-boundary checkpoint materializes the whole prefix of the cut,
/// which is exactly the replay input for restore. The raw log is only read —
/// never truncated or rewritten (the checkpoint is a projection).
pub async fn load_whiteboard_events_up_to(
    pool: &sqlx::SqlitePool,
    max_gate_seq: u64,
    session_id: Option<&str>,
) -> Result<Vec<WhiteboardEvent>, SessionError> {
    let mut sql = format!("SELECT {EVENT_COLUMNS} FROM whiteboard_events WHERE gate_seq <= ?");
    if session_id.is_some() {
        sql.push_str(" AND session_id = ?");
    }
    sql.push_str(" ORDER BY gate_seq ASC");

    // Log-assigned seqs always fit i64; saturate absurd bounds (`u64::MAX`
    // meaning "through end of log") instead of wrapping negative on cast.
    let bound = i64::try_from(max_gate_seq).unwrap_or(i64::MAX);
    // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is assembled solely from static
    // fragments and the const `EVENT_COLUMNS`; every filter value is bound via `?`.
    let mut query = query_as::<_, WhiteboardEventRow>(AssertSqlSafe(sql)).bind(bound);
    if let Some(session_id) = session_id {
        query = query.bind(session_id);
    }
    let rows = query.fetch_all(pool).await?;
    rows.into_iter().map(WhiteboardEvent::try_from).collect()
}

/// Current head of the log: the largest assigned `gate_seq`, or `0` when the
/// log is empty. This is the natural boundary for "checkpoint at the current
/// gate" ([`load_whiteboard_events_up_to`] takes it as its inclusive bound).
pub async fn latest_gate_seq(pool: &sqlx::SqlitePool) -> Result<u64, SessionError> {
    let head: Option<i64> =
        sqlx::query_scalar("SELECT MAX(gate_seq) FROM whiteboard_events").fetch_one(pool).await?;
    match head {
        Some(seq) => {
            u64::try_from(seq).map_err(|_| SessionError::Storage("negative gate_seq".to_string()))
        }
        None => Ok(0),
    }
}

const EVENT_COLUMNS: &str =
    "event_id, gate_seq, agent_id, agent_seq, kind, scope, session_id, plan_id, \
     causation, payload, content_hash, pre_image_hash, created_at";

/// Load a single stored event by id — the authority a duplicate/replayed
/// append returns.
async fn load_whiteboard_event(
    conn: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    event_id: &str,
) -> Result<WhiteboardEvent, SessionError> {
    let sql = format!("SELECT {EVENT_COLUMNS} FROM whiteboard_events WHERE event_id = ?");
    // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is assembled solely from static
    // fragments and the const `EVENT_COLUMNS`; every filter value is bound via `?`.
    let row = query_as::<_, WhiteboardEventRow>(AssertSqlSafe(sql))
        .bind(event_id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| SessionError::NotFound(format!("whiteboard event {event_id}")))?;
    WhiteboardEvent::try_from(row)
}

/// Raw row shape for row decoding (TEXT columns stay strings; integer columns
/// come back as `i64`).
#[derive(Debug, sqlx::FromRow)]
struct WhiteboardEventRow {
    event_id: String,
    gate_seq: i64,
    agent_id: String,
    agent_seq: i64,
    kind: String,
    scope: String,
    session_id: Option<String>,
    plan_id: Option<String>,
    causation: Option<String>,
    payload: String,
    content_hash: String,
    pre_image_hash: Option<String>,
    created_at: i64,
}

impl TryFrom<WhiteboardEventRow> for WhiteboardEvent {
    type Error = SessionError;

    fn try_from(row: WhiteboardEventRow) -> Result<Self, SessionError> {
        let kind = WhiteboardKind::from_db_string(&row.kind).ok_or_else(|| {
            SessionError::Storage(format!("unknown whiteboard event kind: {}", row.kind))
        })?;
        let payload = serde_json::from_str(&row.payload).map_err(|e| {
            SessionError::Serialization(format!(
                "invalid payload JSON in whiteboard event {}: {e}",
                row.event_id
            ))
        })?;
        let gate_seq = u64::try_from(row.gate_seq).map_err(|_| {
            SessionError::Storage("negative gate_seq in whiteboard event".to_string())
        })?;
        let agent_seq = u64::try_from(row.agent_seq).map_err(|_| {
            SessionError::Storage("negative agent_seq in whiteboard event".to_string())
        })?;
        Ok(Self {
            event_id: row.event_id,
            gate_seq,
            agent_id: row.agent_id,
            agent_seq,
            kind,
            scope: row.scope,
            session_id: row.session_id,
            plan_id: row.plan_id,
            causation: row.causation,
            payload,
            content_hash: row.content_hash,
            pre_image_hash: row.pre_image_hash,
            created_at: row.created_at,
        })
    }
}

/// Upsert a subscriber's subscription (`INSERT OR REPLACE`, ADR-60 D3): store
/// `scopes` as compact JSON and set the cursor to `cursor_gate_seq` — the
/// initial cursor (typically `0` for a first registration; the caller chooses
/// any value for a deliberate re-registration).
///
/// Replace semantics, not `ON CONFLICT DO UPDATE` preserve: registration is a
/// full re-declaration of the subscriber's scopes by design, so the caller
/// passes the authoritative row. The caller rehydrates a persisted cursor
/// before re-registering when it wants to resume rather than restart.
pub async fn upsert_whiteboard_subscription(
    pool: &sqlx::SqlitePool,
    subscription: &WhiteboardSubscription,
) -> Result<(), SessionError> {
    let scopes = serde_json::to_string(&subscription.scopes)?;
    sqlx::query(
        "INSERT OR REPLACE INTO whiteboard_subscriptions (subscriber_id, scopes, cursor_gate_seq) \
         VALUES (?, ?, ?)",
    )
    .bind(&subscription.subscriber_id)
    .bind(&scopes)
    .bind(subscription.cursor_gate_seq as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load a subscriber's subscription by id; `Ok(None)` when unknown (ADR-60
/// D3). Decodes the `scopes` column from its compact-JSON form back into
/// [`WhiteboardScope`]s, so a save/load cycle round-trips.
pub async fn load_whiteboard_subscription(
    pool: &sqlx::SqlitePool,
    subscriber_id: &str,
) -> Result<Option<WhiteboardSubscription>, SessionError> {
    let sql = "SELECT subscriber_id, scopes, cursor_gate_seq \
               FROM whiteboard_subscriptions WHERE subscriber_id = ?";
    let row = query_as::<_, WhiteboardSubscriptionRow>(sql)
        .bind(subscriber_id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let scopes = serde_json::from_str(&row.scopes).map_err(|e| {
        SessionError::Serialization(format!(
            "invalid scopes JSON in whiteboard_subscriptions row {}: {e}",
            row.subscriber_id
        ))
    })?;
    let cursor_gate_seq = u64::try_from(row.cursor_gate_seq).map_err(|_| {
        SessionError::Storage("negative cursor_gate_seq in whiteboard_subscriptions".to_string())
    })?;
    Ok(Some(WhiteboardSubscription { subscriber_id: row.subscriber_id, scopes, cursor_gate_seq }))
}

/// Advance a subscriber's persisted cursor to `end_gate_seq` — monotonic via
/// `MAX(cursor_gate_seq, ?)`, so an ack can never lower it (ADR-60 D3: the
/// cursor is a consistent-cut coordinate; lower values would re-deliver acked
/// events). A missing row is a no-op: there is nothing to advance, and acking
/// must not conjure a registration the supervisor never accepted.
pub async fn ack_whiteboard_subscription(
    pool: &sqlx::SqlitePool,
    subscriber_id: &str,
    end_gate_seq: u64,
) -> Result<(), SessionError> {
    sqlx::query(
        "UPDATE whiteboard_subscriptions \
         SET cursor_gate_seq = MAX(cursor_gate_seq, ?) \
         WHERE subscriber_id = ?",
    )
    .bind(end_gate_seq as i64)
    .bind(subscriber_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Raw row shape for subscription decoding (TEXT columns stay strings; the
/// cursor integer comes back as `i64`).
#[derive(Debug, sqlx::FromRow)]
struct WhiteboardSubscriptionRow {
    subscriber_id: String,
    scopes: String,
    cursor_gate_seq: i64,
}

/// A whiteboard checkpoint: gate-boundary snapshot for D5 reversibility and
/// deterministic replay (ADR-60 D5). The snapshot is an opaque JSON blob
/// (file-system or whiteboard projection) taken at `gate_seq`; restore is
/// snapshot + replay of tail events with `gate_seq > gate_seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhiteboardCheckpoint {
    /// Checkpoint id (ULID string).
    pub id: String,
    /// Gate sequence the snapshot is taken at (consistent cut).
    pub gate_seq: u64,
    /// Opaque snapshot payload (JSON).
    pub snapshot: String,
    /// Creation time (unix epoch ms, UTC).
    pub created_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct WhiteboardCheckpointRow {
    id: String,
    gate_seq: i64,
    snapshot: String,
    created_at: i64,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Create a whiteboard checkpoint at `gate_seq` with `snapshot` payload.
pub async fn create_whiteboard_checkpoint(
    pool: &sqlx::SqlitePool,
    gate_seq: u64,
    snapshot: &str,
) -> Result<WhiteboardCheckpoint, SessionError> {
    let id = ulid::Ulid::new().to_string();
    let created_at = now_millis();
    sqlx::query(
        "INSERT INTO whiteboard_checkpoints (id, gate_seq, snapshot, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(gate_seq as i64)
    .bind(snapshot)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(WhiteboardCheckpoint { id, gate_seq, snapshot: snapshot.to_owned(), created_at })
}

/// Load a checkpoint by id.
pub async fn load_whiteboard_checkpoint(
    pool: &sqlx::SqlitePool,
    id: &str,
) -> Result<Option<WhiteboardCheckpoint>, SessionError> {
    let row = sqlx::query_as::<_, WhiteboardCheckpointRow>(
        "SELECT id, gate_seq, snapshot, created_at FROM whiteboard_checkpoints WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| WhiteboardCheckpoint {
        id: r.id,
        gate_seq: r.gate_seq as u64,
        snapshot: r.snapshot,
        created_at: r.created_at,
    }))
}

/// Load the latest checkpoint at or before `gate_seq`.
pub async fn load_whiteboard_checkpoint_by_gate_seq(
    pool: &sqlx::SqlitePool,
    gate_seq: u64,
) -> Result<Option<WhiteboardCheckpoint>, SessionError> {
    let row = sqlx::query_as::<_, WhiteboardCheckpointRow>(
        "SELECT id, gate_seq, snapshot, created_at FROM whiteboard_checkpoints WHERE gate_seq <= ? ORDER BY gate_seq DESC LIMIT 1",
    )
    .bind(gate_seq as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| WhiteboardCheckpoint {
        id: r.id,
        gate_seq: r.gate_seq as u64,
        snapshot: r.snapshot,
        created_at: r.created_at,
    }))
}

/// List all checkpoints ordered by gate_seq ascending.
pub async fn list_whiteboard_checkpoints(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<WhiteboardCheckpoint>, SessionError> {
    let rows = sqlx::query_as::<_, WhiteboardCheckpointRow>(
        "SELECT id, gate_seq, snapshot, created_at FROM whiteboard_checkpoints ORDER BY gate_seq ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| WhiteboardCheckpoint {
            id: r.id,
            gate_seq: r.gate_seq as u64,
            snapshot: r.snapshot,
            created_at: r.created_at,
        })
        .collect())
}

/// Replay tail events after `checkpoint_gate_seq`, excluding `exclude_event_ids`.
/// Used for per-agent revert: restore snapshot at checkpoint, then replay
/// excluding the reverted agent's event_ids. Returns events in gate_seq order.
pub async fn replay_whiteboard_tail_excluding(
    pool: &sqlx::SqlitePool,
    checkpoint_gate_seq: u64,
    exclude_event_ids: &[String],
) -> Result<Vec<WhiteboardEvent>, SessionError> {
    let mut events = load_whiteboard_events(
        pool,
        &WhiteboardLoadOpts {
            after_gate_seq: checkpoint_gate_seq,
            session_id: None,
            scope: None,
            limit: 10000,
        },
    )
    .await?;
    if !exclude_event_ids.is_empty() {
        events.retain(|e| !exclude_event_ids.contains(&e.event_id));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use tempfile::TempDir;

    /// File-backed pool with the same PRAGMAs as production connectivity
    /// (WAL, busy_timeout, synchronous=NORMAL) and all migrations applied.
    /// File-backed — not `sqlite::memory:` — so multiple connections share one
    /// database, which the concurrency test requires (a pooled in-memory name
    /// gives each connection its own private database).
    async fn test_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("whiteboard_test.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn new_event(event_id: &str, agent_id: &str, kind: WhiteboardKind) -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: event_id.to_owned(),
            agent_id: agent_id.to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({ "note": event_id }),
            pre_image_hash: None,
            created_at: 1_700_000_000_000 + event_id.len() as i64,
        }
    }

    #[tokio::test]
    async fn migration_applies_and_log_starts_empty() {
        let (_dir, pool) = test_pool(1).await;

        // The migration created the table and every index.
        let objects: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table', 'index') AND name LIKE '%whiteboard_%' \
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("schema query");
        assert!(
            objects.iter().any(|n| n == "whiteboard_events"),
            "whiteboard_events table exists; got: {objects:?}"
        );
        for index in [
            "idx_whiteboard_agent_seq",
            "idx_whiteboard_session_gate",
            "idx_whiteboard_scope_gate",
            "idx_whiteboard_plan_gate",
        ] {
            assert!(objects.iter().any(|n| n == index), "index {index} exists");
        }

        // A fresh log reads empty.
        let events =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn appends_assign_unique_global_and_per_agent_seqs() {
        let (_dir, pool) = test_pool(1).await;
        let a =
            append_whiteboard_event(&pool, &new_event("e1", "agent-a", WhiteboardKind::Finding))
                .await
                .expect("append a1");
        let b =
            append_whiteboard_event(&pool, &new_event("e2", "agent-b", WhiteboardKind::Decision))
                .await
                .expect("append b1");
        let c = append_whiteboard_event(
            &pool,
            &new_event("e3", "agent-a", WhiteboardKind::WriteApplied),
        )
        .await
        .expect("append a2");
        let d = append_whiteboard_event(
            &pool,
            &new_event("e4", "agent-b", WhiteboardKind::SubtaskCompleted),
        )
        .await
        .expect("append b2");
        let e =
            append_whiteboard_event(&pool, &new_event("e5", "agent-a", WhiteboardKind::Finding))
                .await
                .expect("append a3");

        assert_eq!((a.gate_seq, a.agent_seq), (1, 1), "agent-a first event");
        assert_eq!((b.gate_seq, b.agent_seq), (2, 1), "agent-b first event");
        assert_eq!((c.gate_seq, c.agent_seq), (3, 2), "agent-a second event");
        assert_eq!((d.gate_seq, d.agent_seq), (4, 2), "agent-b second event");
        assert_eq!((e.gate_seq, e.agent_seq), (5, 3), "agent-a third event");

        // The total order is exactly the append order.
        let loaded =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        assert_eq!(
            loaded.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5],
            "gate_seq grows strictly with append order"
        );
    }

    /// The concurrency-safety proof: N concurrent appends from spawned tasks
    /// on one pool must yield N unique `gate_seq`s (1..=N), per-agent
    /// `agent_seq`s unique, zero failures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_assign_unique_gate_seqs() {
        const N: usize = 60;
        let (_dir, pool) = test_pool(8).await;

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let agent = if i % 2 == 0 { "agent-a" } else { "agent-b" };
                let kind =
                    if i % 3 == 0 { WhiteboardKind::Finding } else { WhiteboardKind::Decision };
                let event = new_event(&format!("e{i:03}"), agent, kind);
                append_whiteboard_event(&pool, &event).await
            }));
        }

        let mut events = Vec::with_capacity(N);
        for handle in handles {
            // Task panics (none expected) surface as Err here; append errors
            // (e.g. SQLITE_BUSY) would fail the assertion below.
            events.push(handle.await.expect("concurrent task joined").expect("append succeeded"));
        }
        assert_eq!(events.len(), N);

        let mut gate_seqs: Vec<u64> = events.iter().map(|ev| ev.gate_seq).collect();
        gate_seqs.sort_unstable();
        gate_seqs.dedup();
        assert_eq!(gate_seqs.len(), N, "all N gate_seqs unique");
        assert_eq!(gate_seqs[0], 1);
        assert_eq!(gate_seqs[N - 1], N as u64);

        for agent in ["agent-a", "agent-b"] {
            let mut agent_seqs: Vec<u64> =
                events.iter().filter(|ev| ev.agent_id == agent).map(|ev| ev.agent_seq).collect();
            agent_seqs.sort_unstable();
            agent_seqs.dedup();
            assert_eq!(agent_seqs.len(), N / 2, "per-agent {agent} agent_seqs unique");
        }

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM whiteboard_events")
            .fetch_one(&pool)
            .await
            .expect("row count");
        assert_eq!(count, N as i64);
    }

    #[tokio::test]
    async fn consistent_cut_reader_returns_inclusive_prefix_and_head() {
        let (_dir, pool) = test_pool(1).await;
        assert_eq!(latest_gate_seq(&pool).await.expect("empty head"), 0, "empty log head is 0");

        for (i, (agent, session)) in
            [("agent-a", Some("sess-1")), ("agent-b", None), ("agent-a", None)].iter().enumerate()
        {
            let mut event = new_event(&format!("cut-{i}"), agent, WhiteboardKind::WriteApplied);
            event.session_id = session.map(str::to_owned);
            append_whiteboard_event(&pool, &event).await.expect("append");
        }

        assert_eq!(latest_gate_seq(&pool).await.expect("head"), 3);

        // Inclusive upper bound: seq 2 folds rows 1 and 2, never 3.
        let cut = load_whiteboard_events_up_to(&pool, 2, None).await.expect("cut at 2");
        assert_eq!(cut.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(), vec![1, 2]);

        let full = load_whiteboard_events_up_to(&pool, u64::MAX, None).await.expect("full cut");
        assert_eq!(full.len(), 3);

        // Session filter composes with the cut.
        let sess1 = load_whiteboard_events_up_to(&pool, u64::MAX, Some("sess-1"))
            .await
            .expect("session cut");
        assert_eq!(sess1.iter().map(|ev| ev.event_id.as_str()).collect::<Vec<_>>(), vec!["cut-0"]);
    }

    #[tokio::test]
    async fn cursor_pagination_respects_limit_and_ordering() {
        let (_dir, pool) = test_pool(1).await;
        for i in 1..=5 {
            append_whiteboard_event(
                &pool,
                &new_event(&format!("e{i}"), "agent-a", WhiteboardKind::Finding),
            )
            .await
            .expect("append");
        }

        let page1 =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts { limit: 2, ..Default::default() })
                .await
                .expect("page 1");
        assert_eq!(page1.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(), vec![1, 2]);

        let cursor = page1.last().expect("page 1 non-empty").gate_seq;
        let page2 = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: cursor, limit: 2, ..Default::default() },
        )
        .await
        .expect("page 2");
        assert_eq!(page2.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(), vec![3, 4]);

        let cursor = page2.last().expect("page 2 non-empty").gate_seq;
        let page3 = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: cursor, ..Default::default() },
        )
        .await
        .expect("page 3");
        assert_eq!(page3.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(), vec![5]);

        assert_eq!(WhiteboardLoadOpts::default().limit, 200, "default limit");
    }

    #[tokio::test]
    async fn duplicate_event_id_returns_existing_row() {
        let (_dir, pool) = test_pool(1).await;
        let first = append_whiteboard_event(
            &pool,
            &new_event("dup-1", "agent-a", WhiteboardKind::Decision),
        )
        .await
        .expect("first append");
        let replay = append_whiteboard_event(
            &pool,
            &new_event("dup-1", "agent-a", WhiteboardKind::Decision),
        )
        .await
        .expect("replay append");

        assert_eq!(first, replay, "a replay returns the existing row unchanged (no new seq)");
        assert_eq!(replay.gate_seq, 1);
        assert_eq!(replay.agent_seq, 1);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM whiteboard_events")
            .fetch_one(&pool)
            .await
            .expect("row count");
        assert_eq!(count, 1, "the duplicate was not inserted");

        // A genuinely new event afterward still gets the next free sequences.
        let next = append_whiteboard_event(
            &pool,
            &new_event("fresh-1", "agent-a", WhiteboardKind::Finding),
        )
        .await
        .expect("fresh append");
        assert_eq!((next.gate_seq, next.agent_seq), (2, 2));
    }

    #[tokio::test]
    async fn serde_round_trip_preserves_events() {
        let (_dir, pool) = test_pool(1).await;
        let mut event = new_event("serde-1", "agent-a", WhiteboardKind::WriteApplied);
        event.scope = "files".to_owned();
        event.session_id = Some("sess-1".to_owned());
        event.plan_id = Some("plan-1".to_owned());
        event.causation = Some("trigger-evt".to_owned());
        event.pre_image_hash = Some("preimage".to_owned());
        event.payload = json!({ "path": "a.md", "prev": null });

        let stored = append_whiteboard_event(&pool, &event).await.expect("append");
        let decoded: WhiteboardEvent =
            serde_json::from_str(&serde_json::to_string(&stored).expect("serialize"))
                .expect("deserialize");
        assert_eq!(decoded, stored, "serde round trip preserves the event");

        // Kinds serialize kebab-case on the wire.
        assert_eq!(serde_json::to_value(stored.kind).expect("kind"), json!("write-applied"));

        // Nested payload survives the JSON round trip.
        assert_eq!(decoded.payload["path"], json!("a.md"));
        assert_eq!(decoded.payload["missing"], json!(null));
    }

    #[test]
    fn kind_serde_uses_kebab_case_for_every_variant() {
        let cases = [
            (WhiteboardKind::Finding, "finding"),
            (WhiteboardKind::Decision, "decision"),
            (WhiteboardKind::WriteApplied, "write-applied"),
            (WhiteboardKind::WriteRejected, "write-rejected"),
            (WhiteboardKind::Failure, "failure"),
            (WhiteboardKind::PlanApproved, "plan-approved"),
            (WhiteboardKind::TaskGraph, "task-graph"),
            (WhiteboardKind::SubtaskStarted, "subtask-started"),
            (WhiteboardKind::SubtaskCompleted, "subtask-completed"),
            (WhiteboardKind::SubtaskFailed, "subtask-failed"),
            (WhiteboardKind::ReviewState, "review-state"),
            (WhiteboardKind::Consolidation, "consolidation"),
            (WhiteboardKind::MemoryFact, "memory-fact"),
            (WhiteboardKind::ToolExecuted, "tool-executed"),
            (WhiteboardKind::WorkspaceSnapshot, "workspace-snapshot"),
            (WhiteboardKind::DesignDoc, "design-doc"),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected, "as_str kebab-case");
            assert_eq!(
                serde_json::to_value(kind).expect("kind serializes"),
                json!(expected),
                "serde kebab-case"
            );
        }
    }

    /// ADR-65 evidence-spine kinds are ordinary log kinds: they append, load
    /// back, dedup by event_id, and their payload survives the JSON round trip
    /// exactly like the established kinds.
    #[tokio::test]
    async fn adr65_evidence_kinds_round_trip_through_the_log() {
        let (_dir, pool) = test_pool(1).await;

        let mut executed = new_event("ev-1", "agent-a", WhiteboardKind::ToolExecuted);
        executed.payload = json!({
            "tool": "apply_diff",
            "args": { "path": "a.md" },
            "success": true,
            "generation": "gen-3",
            "paths": [{ "path": "a.md", "content_hash": "h1" }]
        });
        let mut snapshot = new_event("ev-2", "agent-a", WhiteboardKind::WorkspaceSnapshot);
        snapshot.payload = json!({
            "generation": "gen-3",
            "files": [{ "path": "a.md", "size_bytes": 42, "content_hash": "h1" }]
        });

        let stored_exec = append_whiteboard_event(&pool, &executed).await.expect("append executed");
        let stored_snap = append_whiteboard_event(&pool, &snapshot).await.expect("append snapshot");

        // Kinds serialize kebab-case on the wire.
        assert_eq!(serde_json::to_value(stored_exec.kind).expect("kind"), json!("tool-executed"));
        assert_eq!(
            serde_json::to_value(stored_snap.kind).expect("kind"),
            json!("workspace-snapshot")
        );

        // They are ordinary log rows: dedup by event_id, ordered by gate_seq.
        let replay = append_whiteboard_event(&pool, &executed).await.expect("replay executed");
        assert_eq!(replay.event_id, "ev-1", "dedup keeps the original row");
        assert_eq!(replay.gate_seq, 1, "the duplicate did not advance the log");

        let loaded =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        assert_eq!(
            loaded.iter().map(|ev| ev.kind).collect::<Vec<_>>(),
            vec![WhiteboardKind::ToolExecuted, WhiteboardKind::WorkspaceSnapshot],
            "both evidence kinds load back in append order"
        );

        // Nested evidence payloads survive the JSON round trip.
        let decoded: WhiteboardEvent =
            serde_json::from_str(&serde_json::to_string(&stored_snap).expect("serialize"))
                .expect("deserialize");
        assert_eq!(decoded.payload["files"][0]["path"], json!("a.md"));
        assert_eq!(decoded.payload["files"][0]["size_bytes"], json!(42));
        assert_eq!(decoded.payload["generation"], json!("gen-3"));
    }

    #[tokio::test]
    async fn load_by_plan_returns_only_that_plans_events() {
        let (_dir, pool) = test_pool(1).await;
        let mut p1_graph = new_event("p1-1", "agent-a", WhiteboardKind::TaskGraph);
        p1_graph.plan_id = Some("plan-1".to_owned());
        append_whiteboard_event(&pool, &p1_graph).await.expect("plan-1 task graph");

        let mut p2_approved = new_event("p2-1", "agent-b", WhiteboardKind::PlanApproved);
        p2_approved.plan_id = Some("plan-2".to_owned());
        append_whiteboard_event(&pool, &p2_approved).await.expect("plan-2 approval");

        let mut p1_started = new_event("p1-2", "agent-a", WhiteboardKind::SubtaskStarted);
        p1_started.plan_id = Some("plan-1".to_owned());
        append_whiteboard_event(&pool, &p1_started).await.expect("plan-1 subtask");

        // Unscoped (plan_id = NULL) events are never returned by a plan query.
        append_whiteboard_event(&pool, &new_event("np-1", "agent-a", WhiteboardKind::Finding))
            .await
            .expect("unscoped event");

        let plan1 = load_whiteboard_events_by_plan(&pool, "plan-1").await.expect("plan-1 load");
        assert_eq!(plan1.len(), 2, "only plan-1 events");
        assert_eq!(plan1.iter().map(|ev| ev.gate_seq).collect::<Vec<_>>(), vec![1, 3]);

        let plan2 = load_whiteboard_events_by_plan(&pool, "plan-2").await.expect("plan-2 load");
        assert_eq!(plan2.len(), 1);
        assert_eq!(plan2[0].event_id, "p2-1");

        let unknown =
            load_whiteboard_events_by_plan(&pool, "plan-zzz").await.expect("unknown load");
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn cursor_reader_filters_by_session_and_scope() {
        let (_dir, pool) = test_pool(1).await;
        let mut s1 = new_event("s1", "agent-a", WhiteboardKind::Finding);
        s1.session_id = Some("sess-1".to_owned());
        s1.scope = "fs".to_owned();
        append_whiteboard_event(&pool, &s1).await.expect("s1");

        let mut s2 = new_event("s2", "agent-a", WhiteboardKind::Decision);
        s2.session_id = Some("sess-2".to_owned());
        s2.scope = "fs".to_owned();
        append_whiteboard_event(&pool, &s2).await.expect("s2");

        let mut s3 = new_event("s3", "agent-b", WhiteboardKind::Finding);
        s3.session_id = Some("sess-1".to_owned());
        s3.scope = "memory".to_owned();
        append_whiteboard_event(&pool, &s3).await.expect("s3");

        let by_session = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { session_id: Some("sess-1".to_owned()), ..Default::default() },
        )
        .await
        .expect("session filter");
        assert_eq!(
            by_session.iter().map(|ev| ev.event_id.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s3"]
        );

        let by_scope = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { scope: Some("fs".to_owned()), ..Default::default() },
        )
        .await
        .expect("scope filter");
        assert_eq!(
            by_scope.iter().map(|ev| ev.event_id.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );

        let combined = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts {
                session_id: Some("sess-1".to_owned()),
                scope: Some("fs".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("combined filter");
        assert_eq!(combined.iter().map(|ev| ev.event_id.as_str()).collect::<Vec<_>>(), vec!["s1"]);
    }

    #[tokio::test]
    async fn content_hash_is_deterministic_and_excludes_sequencing() {
        let ev_a = new_event("same-content", "agent-a", WhiteboardKind::Decision);
        let ev_b = new_event("same-content", "agent-a", WhiteboardKind::Decision);
        let hash_a = compute_content_hash(&ev_a).expect("hash a");
        let hash_b = compute_content_hash(&ev_b).expect("hash b");
        assert_eq!(hash_a, hash_b, "identical caller attestation => identical hash");

        // Changing content changes the hash.
        let mut changed = ev_a.clone();
        changed.payload = json!({ "note": "different" });
        let changed_hash = compute_content_hash(&changed).expect("hash changed");
        assert_ne!(changed_hash, hash_a, "content is part of the hash");

        // gate_seq/agent_seq are assigned by the log and are NOT part of the
        // hash: computing the hash only needs the caller-attested fields, and
        // the stored row carries exactly that value.
        let (_dir, pool) = test_pool(1).await;
        let stored = append_whiteboard_event(&pool, &ev_a).await.expect("append");
        assert_eq!(stored.content_hash, hash_a, "stored hash equals canonical hash");
        assert_eq!(stored.gate_seq, 1);
        assert_eq!(stored.agent_seq, 1);
    }

    // --- ADR-60 D3: subscription cursor persistence (migration 027) ---

    fn subscription(subscriber_id: &str, cursor: u64) -> WhiteboardSubscription {
        WhiteboardSubscription {
            subscriber_id: subscriber_id.to_owned(),
            scopes: vec![WhiteboardScope {
                topics: vec![WhiteboardKind::WriteApplied, WhiteboardKind::Decision],
            }],
            cursor_gate_seq: cursor,
        }
    }

    #[tokio::test]
    async fn subscription_migration_applies_and_row_starts_at_cursor_zero() {
        let (_dir, pool) = test_pool(1).await;

        // Migration 027 created the table.
        let objects: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'whiteboard_subscriptions'",
        )
        .fetch_all(&pool)
        .await
        .expect("schema query");
        assert_eq!(objects, vec!["whiteboard_subscriptions".to_owned()]);

        // A fresh registration declares `cursor_gate_seq = 0` — the
        // consistent-cut origin — matching the column's DEFAULT 0.
        upsert_whiteboard_subscription(&pool, &subscription("agent-b", 0)).await.expect("upsert");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded.subscriber_id, "agent-b");
        assert_eq!(loaded.cursor_gate_seq, 0, "a fresh subscriber starts at cursor 0");
    }

    #[tokio::test]
    async fn ack_advances_monotonically_and_never_lowers() {
        let (_dir, pool) = test_pool(1).await;
        upsert_whiteboard_subscription(&pool, &subscription("agent-b", 7)).await.expect("upsert");

        // Large then small: the small ack must NOT lower the cursor.
        ack_whiteboard_subscription(&pool, "agent-b", 20).await.expect("ack to 20");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded.cursor_gate_seq, 20);

        ack_whiteboard_subscription(&pool, "agent-b", 5).await.expect("stale ack");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded.cursor_gate_seq, 20, "MAX() guard keeps the cursor monotonic");

        // Advancing again works.
        ack_whiteboard_subscription(&pool, "agent-b", 42).await.expect("ack to 42");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded.cursor_gate_seq, 42);
    }

    #[tokio::test]
    async fn ack_for_unknown_subscriber_is_a_no_op() {
        let (_dir, pool) = test_pool(1).await;
        // No registration yet: acking must not conjure a row (the supervisor
        // only persists acks for handshake-registered subscribers).
        ack_whiteboard_subscription(&pool, "ghost", 9).await.expect("no-op ack");
        assert!(
            load_whiteboard_subscription(&pool, "ghost").await.expect("load").is_none(),
            "ack must not create a subscription"
        );
    }

    #[tokio::test]
    async fn load_unknown_subscriber_is_none() {
        let (_dir, pool) = test_pool(1).await;
        assert!(load_whiteboard_subscription(&pool, "never-registered")
            .await
            .expect("load")
            .is_none());
    }

    #[tokio::test]
    async fn scopes_round_trip_through_save_and_load() {
        let (_dir, pool) = test_pool(1).await;
        let mut sub = subscription("agent-b", 3);
        sub.scopes = vec![
            WhiteboardScope { topics: vec![WhiteboardKind::WriteApplied] },
            WhiteboardScope {
                topics: vec![WhiteboardKind::SubtaskStarted, WhiteboardKind::SubtaskCompleted],
            },
        ];

        upsert_whiteboard_subscription(&pool, &sub).await.expect("upsert");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded, sub, "scopes + cursor survive a save/load cycle");
        assert_eq!(
            serde_json::to_value(&loaded.scopes).expect("scopes serialize"),
            serde_json::json!([
                { "topics": ["write-applied"] },
                { "topics": ["subtask-started", "subtask-completed"] }
            ]),
            "scopes serialize kebab-case for the DB column"
        );

        // A re-registration (INSERT OR REPLACE) replaces the row wholesale.
        let re_registered = subscription("agent-b", 0);
        upsert_whiteboard_subscription(&pool, &re_registered).await.expect("re-register");
        let loaded =
            load_whiteboard_subscription(&pool, "agent-b").await.expect("load").expect("row");
        assert_eq!(loaded, re_registered, "re-registration replaces scopes and cursor");
    }

    // --- ADR-60 D5: checkpoint durability (migration 028) ---

    #[tokio::test]
    async fn whiteboard_checkpoint_round_trips() {
        let (_dir, pool) = test_pool(1).await;
        let cp1 = create_whiteboard_checkpoint(&pool, 5, r#"{"files":{"a.txt":"hello"}}"#)
            .await
            .expect("create cp1");
        assert_eq!(cp1.gate_seq, 5);
        let loaded = load_whiteboard_checkpoint(&pool, &cp1.id).await.expect("load").expect("row");
        assert_eq!(loaded, cp1);
        let cp2 = create_whiteboard_checkpoint(&pool, 10, r#"{"files":{"a.txt":"hello world"}}"#)
            .await
            .expect("create cp2");
        let by_gate = load_whiteboard_checkpoint_by_gate_seq(&pool, 7)
            .await
            .expect("load by gate")
            .expect("row");
        assert_eq!(by_gate.id, cp1.id, "latest checkpoint <= gate_seq");
        let all = list_whiteboard_checkpoints(&pool).await.expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].gate_seq, 5);
        assert_eq!(all[1].gate_seq, 10);
        let _ = cp2;
    }

    #[tokio::test]
    async fn replay_excluding_filters_per_agent_events() {
        let (_dir, pool) = test_pool(1).await;
        let e1 = append_whiteboard_event(
            &pool,
            &new_event("e1", "agent-a", WhiteboardKind::WriteApplied),
        )
        .await
        .expect("append e1");
        let e2 = append_whiteboard_event(
            &pool,
            &new_event("e2", "agent-a", WhiteboardKind::WriteApplied),
        )
        .await
        .expect("append e2");
        let e3 = append_whiteboard_event(
            &pool,
            &new_event("e3", "agent-a", WhiteboardKind::WriteApplied),
        )
        .await
        .expect("append e3");
        create_whiteboard_checkpoint(&pool, e1.gate_seq, r#"{"snap":1}"#)
            .await
            .expect("checkpoint");
        // Replay after e1, excluding e2 (per-agent revert)
        let replay = replay_whiteboard_tail_excluding(
            &pool,
            e1.gate_seq,
            std::slice::from_ref(&e2.event_id),
        )
        .await
        .expect("replay");
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].event_id, e3.event_id);
        // Full replay without exclusion
        let full = replay_whiteboard_tail_excluding(&pool, e1.gate_seq, &[]).await.expect("full");
        assert_eq!(full.len(), 2);
        let _ = e1;
    }
}
