//! Timeline projection for zero-waste orchestration (ADR-64 Phase 2).
//!
//! A **pure derived view** over Concerto's durable sources — the whiteboard
//! log, gate-boundary checkpoints, and plan artifacts — that produces a typed,
//! chronologically-ordered projection for reuse decisions and agent context
//! enrichment. This module never writes to a new storage table; it reads
//! existing durable sources and folds them idempotently.
//!
//! ## Design
//!
//! The projection is recomputable from the WAL-first durable log (ADR-60 D4
//! invariant), so it can never drift from the source of truth and has no
//! migration surface. Agents receive a compact, task-specific slice via
//! [`WorkingMemorySnapshot`] enrichment — never the raw transcript.
//!
//! ## Consumed sources
//!
//! - **Whiteboard log** (`concerto_sessions::whiteboard`): the append-only
//!   source of truth for all run events.
//! - **Gate-boundary checkpoints** (`crate::checkpoint::GateBoundaryCheckpoint`):
//!   projected file state at consistent cuts.
//! - **Plan artifacts** folded from `PlanApproved` whiteboard events.
//!
//! Phase 2 scope: projection + enrichment. Phase 3 adds semantic keys;
//! Phase 4 adds the pre-dispatch resolver.

use std::collections::HashMap;

use concerto_core::memory::{Decision, DecisionCategory, DecisionId, WorkingMemorySnapshot};
use concerto_core::types::{AgentRunResult, TaskId};
use concerto_sessions::whiteboard::{self, WhiteboardEvent, WhiteboardKind};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::debug;

use crate::checkpoint::{CheckpointStoreError, GateBoundaryCheckpoint};
use crate::planner::PlanArtifact;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Why a timeline projection could not be built.
#[derive(Debug, Error)]
pub enum TimelineError {
    /// Whiteboard log read failure.
    #[error("whiteboard log error: {0}")]
    Whiteboard(#[from] concerto_sessions::SessionError),
    /// Gate-boundary checkpoint load failure.
    #[error("checkpoint store error: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A typed, chronologically-ordered timeline entry folded from durable
/// sources. Minimal but sufficient for: (a) a pre-dispatch reuse resolver
/// (does a deliverable/key already exist & is it current?), (b) agent context
/// enrichment (what is known/complete/remaining).
#[derive(Debug, Clone)]
pub enum TimelineEvent {
    /// A general whiteboard event not covered by the more specific variants.
    Whiteboard {
        event_id: String,
        gate_seq: u64,
        kind: WhiteboardKind,
        scope: String,
        content_hash: String,
        created_at: i64,
    },
    /// A file was written via the write gate (ADR-60 D4).
    WroteFile { gate_seq: u64, path: String, content_hash: String, created_at: i64 },
    /// A subtask completed successfully.
    SubtaskCompleted {
        event_id: String,
        gate_seq: u64,
        task_id: TaskId,
        summary: String,
        files_modified: Vec<camino::Utf8PathBuf>,
        content_hash: String,
        created_at: i64,
        /// Semantic key hex for identity matching (ADR-64 Phase 4).
        /// Empty string when the event was produced before semantic keys were
        /// available; the resolver treats it as "unknown identity".
        semantic_key_hex: String,
        /// Recorded input fingerprints at the time of completion, for freshness
        /// comparison by the resolver (ADR-64 Phase 4).
        recorded_inputs: Vec<crate::fingerprint::ArtifactFingerprint>,
    },
    /// An approved plan was recorded on the whiteboard.
    PlanApproved { gate_seq: u64, plan_id: String, content_hash: String, created_at: i64 },
}

/// A complete timeline projection: chronologically-ordered events, gate-boundary
/// checkpoints, plan artifacts, and completed results. This is the read-only
/// projection that Phase 4's resolver and Phase 5's capsules consume.
#[derive(Debug, Clone)]
pub struct TimelineProjection {
    /// Chronologically-ordered timeline events (sorted by `gate_seq`).
    pub events: Vec<TimelineEvent>,
    /// Gate-boundary checkpoints at consistent cuts.
    pub checkpoints: Vec<GateBoundaryCheckpoint>,
    /// Plan artifacts folded from `PlanApproved` whiteboard events.
    pub plan_artifacts: Vec<PlanArtifact>,
    /// Completed agent results, keyed by task id.
    pub completed_results: HashMap<TaskId, AgentRunResult>,
}

// ---------------------------------------------------------------------------
// TimelineEvent helpers
// ---------------------------------------------------------------------------

impl TimelineEvent {
    /// The gate sequence ordering this event in the log.
    pub fn gate_seq(&self) -> u64 {
        match self {
            Self::Whiteboard { gate_seq, .. }
            | Self::WroteFile { gate_seq, .. }
            | Self::SubtaskCompleted { gate_seq, .. } => *gate_seq,
            Self::PlanApproved { gate_seq, .. } => *gate_seq,
        }
    }

    /// The content hash for staleness comparison.
    pub fn content_hash(&self) -> &str {
        match self {
            Self::Whiteboard { content_hash, .. }
            | Self::WroteFile { content_hash, .. }
            | Self::SubtaskCompleted { content_hash, .. }
            | Self::PlanApproved { content_hash, .. } => content_hash,
        }
    }

    /// The creation timestamp (unix epoch ms).
    pub fn created_at(&self) -> i64 {
        match self {
            Self::Whiteboard { created_at, .. }
            | Self::WroteFile { created_at, .. }
            | Self::SubtaskCompleted { created_at, .. }
            | Self::PlanApproved { created_at, .. } => *created_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Projection builder
// ---------------------------------------------------------------------------

/// Build a timeline projection by folding durable sources up to
/// `gate_seq_cut` (`u64::MAX` = full log). Pure derived view: reads only,
/// never writes.
///
/// - `pool`: SQLite connection pool (sessions crate).
/// - `session_id`: optional session filter (applied to whiteboard load).
/// - `plan_id`: optional plan filter. When set, only whiteboard events
///   whose `plan_id` matches are folded; gate-boundary checkpoints and plan
///   artifacts are still loaded unconditionally (they are run-scoped, not
///   plan-scoped).
/// - `gate_seq_cut`: inclusive upper bound on `gate_seq`. `u64::MAX` folds
///   the entire log.
pub async fn build_timeline(
    pool: &SqlitePool,
    session_id: Option<&str>,
    plan_id: Option<&str>,
    gate_seq_cut: u64,
) -> Result<TimelineProjection, TimelineError> {
    // 1. Load whiteboard events up to the cut.
    let raw_events =
        whiteboard::load_whiteboard_events_up_to(pool, gate_seq_cut, session_id).await?;

    // 2. Fold whiteboard events into typed TimelineEvents.
    let mut events: Vec<TimelineEvent> = raw_events
        .iter()
        .filter(|e| plan_id.is_none() || e.plan_id.as_deref() == plan_id)
        .map(fold_whiteboard_event)
        .collect();

    // 3. Load gate-boundary checkpoint at the cut (if one exists).
    let session_id_owned = session_id.map(|s| s.to_owned());
    let checkpoint = GateBoundaryCheckpoint::at_cut(&raw_events, gate_seq_cut, session_id_owned);
    let checkpoints = if checkpoint.files.is_empty() { Vec::new() } else { vec![checkpoint] };

    // 4. Collect plan artifacts from PlanApproved events.
    // PlanArtifact only derives Serialize (not Deserialize), so we construct
    // minimal artifacts from the whiteboard event metadata. The full artifact
    // lives on disk and can be loaded in Phase 4.
    let plan_artifacts: Vec<PlanArtifact> = raw_events
        .iter()
        .filter(|e| e.kind == WhiteboardKind::PlanApproved)
        .filter(|e| plan_id.is_none() || e.plan_id.as_deref() == plan_id)
        .filter_map(extract_plan_artifact)
        .collect();

    // 5. Collect completed results from SubtaskCompleted events.
    let completed_results: HashMap<TaskId, AgentRunResult> = raw_events
        .iter()
        .filter(|e| e.kind == WhiteboardKind::SubtaskCompleted)
        .filter(|e| plan_id.is_none() || e.plan_id.as_deref() == plan_id)
        .filter_map(extract_completed_result)
        .collect();

    // Sort events by gate_seq for deterministic ordering.
    events.sort_by_key(|e| e.gate_seq());

    debug!(
        target: "concerto_orchestrator::timeline",
        events = events.len(),
        checkpoints = checkpoints.len(),
        plan_artifacts = plan_artifacts.len(),
        completed_results = completed_results.len(),
        "timeline projection built"
    );

    Ok(TimelineProjection { events, checkpoints, plan_artifacts, completed_results })
}

// ---------------------------------------------------------------------------
// Enrichment
// ---------------------------------------------------------------------------

/// Merge a timeline projection into a [`WorkingMemorySnapshot`] in place.
///
/// Appends a lightweight timeline summary as `Decision`s so the snapshot is
/// visibly enriched. This is idempotent: re-running on an already-enriched
/// snapshot does not duplicate entries (guarded by the marker).
///
/// Phase 5 (capsules) will replace this raw injection with a bounded
/// `TimelineContext`; for now it is the simplest way to make timeline
/// knowledge visible in agent prompts.
pub fn enrich_working_memory(
    snapshot: &mut WorkingMemorySnapshot,
    projection: &TimelineProjection,
) {
    // Guard: skip if already enriched (idempotency).
    if snapshot
        .decisions
        .iter()
        .any(|d| d.what == "__timeline_enrichment__" || d.what.starts_with("timeline: "))
    {
        return;
    }

    let session_id = snapshot.session_id;

    // Record a summary of completed work.
    if !projection.completed_results.is_empty() {
        let completed_summary = projection
            .completed_results
            .values()
            .map(|r| format!("{}: {}", r.role, truncate_summary(&r.summary, 120)))
            .collect::<Vec<_>>()
            .join("; ");
        snapshot.decisions.push(Decision {
            id: DecisionId(concerto_core::ids::Ulid::new()),
            session_id,
            task_id: None,
            what: "timeline: completed subtasks".into(),
            why: completed_summary,
            outcome: Some(format!("{} completed", projection.completed_results.len())),
            category: DecisionCategory::Implementation,
            confidence: 1.0,
            superseded_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        });
    }

    // Record approved plans.
    if !projection.plan_artifacts.is_empty() {
        let plan_summary = projection
            .plan_artifacts
            .iter()
            .map(|p| format!("plan {} ({} tasks)", p.plan_id, p.tasks.len()))
            .collect::<Vec<_>>()
            .join(", ");
        snapshot.decisions.push(Decision {
            id: DecisionId(concerto_core::ids::Ulid::new()),
            session_id,
            task_id: None,
            what: "timeline: approved plans".into(),
            why: plan_summary,
            outcome: Some(format!("{} plans approved", projection.plan_artifacts.len())),
            category: DecisionCategory::Architecture,
            confidence: 1.0,
            superseded_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        });
    }

    // Record file writes.
    let wrote_files: Vec<&str> = projection
        .events
        .iter()
        .filter_map(|e| match e {
            TimelineEvent::WroteFile { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    if !wrote_files.is_empty() {
        snapshot.decisions.push(Decision {
            id: DecisionId(concerto_core::ids::Ulid::new()),
            session_id,
            task_id: None,
            what: "timeline: files written".into(),
            why: wrote_files.join(", "),
            outcome: Some(format!("{} files written", wrote_files.len())),
            category: DecisionCategory::Implementation,
            confidence: 1.0,
            superseded_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        });
    }

    // Record a summary of whiteboard findings/decisions (non-write events).
    let other_events: Vec<String> = projection
        .events
        .iter()
        .filter_map(|e| match e {
            TimelineEvent::Whiteboard { kind, scope, .. } => {
                Some(format!("{} ({})", kind.as_str(), scope))
            }
            _ => None,
        })
        .take(10) // Cap at 10 to avoid bloat.
        .collect();
    if !other_events.is_empty() {
        snapshot.decisions.push(Decision {
            id: DecisionId(concerto_core::ids::Ulid::new()),
            session_id,
            task_id: None,
            what: "timeline: whiteboard summary".into(),
            why: other_events.join("; "),
            outcome: Some(format!("{} events observed", projection.events.len())),
            category: DecisionCategory::Other,
            confidence: 1.0,
            superseded_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        });
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Fold one whiteboard event into the appropriate `TimelineEvent` variant.
fn fold_whiteboard_event(event: &WhiteboardEvent) -> TimelineEvent {
    match event.kind {
        WhiteboardKind::WriteApplied => {
            let path = event
                .payload
                .get("input")
                .and_then(|input| input.get("path"))
                .and_then(|p| p.as_str())
                .unwrap_or("<unknown>")
                .to_owned();
            TimelineEvent::WroteFile {
                gate_seq: event.gate_seq,
                path,
                content_hash: event.content_hash.clone(),
                created_at: event.created_at,
            }
        }
        WhiteboardKind::SubtaskCompleted => {
            // Extract task_id from payload if present; fallback to a synthetic id.
            let task_id = event
                .payload
                .get("task_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Ulid>().ok())
                .map(TaskId)
                .unwrap_or_default();
            let summary =
                event.payload.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let files_modified = event
                .payload
                .get("files_modified")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            // Extract semantic key hex (ADR-64 Phase 4) if present.
            let semantic_key_hex = event
                .payload
                .get("semantic_key_hex")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            // Extract recorded inputs fingerprints (ADR-64 Phase 4) if present.
            let recorded_inputs: Vec<crate::fingerprint::ArtifactFingerprint> = event
                .payload
                .get("recorded_inputs")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            TimelineEvent::SubtaskCompleted {
                event_id: event.event_id.clone(),
                gate_seq: event.gate_seq,
                task_id,
                summary,
                files_modified,
                content_hash: event.content_hash.clone(),
                created_at: event.created_at,
                semantic_key_hex,
                recorded_inputs,
            }
        }
        WhiteboardKind::PlanApproved => {
            let plan_id = event
                .plan_id
                .clone()
                .or_else(|| event.payload.get("plan_id").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default();
            TimelineEvent::PlanApproved {
                gate_seq: event.gate_seq,
                plan_id,
                content_hash: event.content_hash.clone(),
                created_at: event.created_at,
            }
        }
        _ => TimelineEvent::Whiteboard {
            event_id: event.event_id.clone(),
            gate_seq: event.gate_seq,
            kind: event.kind,
            scope: event.scope.clone(),
            content_hash: event.content_hash.clone(),
            created_at: event.created_at,
        },
    }
}

/// Extract a [`PlanArtifact`] from a `PlanApproved` whiteboard event.
///
/// `PlanArtifact` only derives `Serialize` (not `Deserialize`), so we
/// construct a minimal artifact from the event's `plan_id` and payload
/// metadata. The full plan lives on disk (`plan-<id>.json`) and will be
/// loaded in Phase 4 when the resolver needs it.
fn extract_plan_artifact(event: &WhiteboardEvent) -> Option<PlanArtifact> {
    let plan_id = event
        .plan_id
        .clone()
        .or_else(|| event.payload.get("plan_id").and_then(|v| v.as_str()).map(String::from))?;

    let task_description =
        event.payload.get("task_description").and_then(|v| v.as_str()).unwrap_or("").to_owned();

    Some(PlanArtifact {
        plan_id,
        task_description,
        tasks: Vec::new(), // Full task list lives on disk; loaded in Phase 4.
    })
}

/// Extract a minimal [`AgentRunResult`] from a `SubtaskCompleted` whiteboard
/// event payload. Returns `None` if the payload is malformed.
fn extract_completed_result(event: &WhiteboardEvent) -> Option<(TaskId, AgentRunResult)> {
    let task_id = event
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Ulid>().ok())
        .map(TaskId)
        .or_else(|| {
            // Fallback: try to extract from the event's causation field.
            event.causation.as_ref().and_then(|c| c.parse::<Ulid>().ok().map(TaskId))
        })?;

    let summary = event.payload.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_owned();

    let files_modified: Vec<camino::Utf8PathBuf> = event
        .payload
        .get("files_modified")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let role = event.payload.get("role").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();

    Some((
        task_id,
        AgentRunResult {
            task_id,
            role: concerto_core::types::AgentId::new(&role),
            outcome: concerto_core::types::AgentOutcome::Success,
            summary,
            files_modified,
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
        },
    ))
}

/// Truncate a summary string to `max_chars`, adding `"…"` if truncated.
fn truncate_summary(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_owned()
    } else {
        // floor_char_boundary avoids splitting a multi-byte UTF-8 character.
        format!("{}…", &s[..s.floor_char_boundary(max_chars)])
    }
}

// Re-export the core ULID type used by extractors.
use concerto_core::ids::Ulid;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_sessions::whiteboard::{NewWhiteboardEvent, WhiteboardKind};
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use tempfile::TempDir;

    /// File-backed pool with WAL and all migrations applied (same pattern as
    /// whiteboard.rs tests).
    async fn test_pool() -> (TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("timeline_test.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("test pool connects");
        // NB: same portable pattern as gate.rs's test_pool — the sessions
        // migrations are referenced via a relative path, never a symlink.
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    /// Helper to create a whiteboard event for tests.
    fn new_event(event_id: &str, kind: WhiteboardKind) -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: event_id.to_owned(),
            agent_id: "test-agent".to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({}),
            pre_image_hash: None,
            created_at: 1_700_000_000_000 + event_id.len() as i64,
        }
    }

    // ------------------------------------------------------------------
    // Test 1: empty projection
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn build_timeline_returns_empty_on_empty_log() {
        let (_dir, pool) = test_pool().await;
        let projection = build_timeline(&pool, None, None, u64::MAX).await.expect("build timeline");
        assert!(projection.events.is_empty(), "no events on empty log");
        assert!(projection.checkpoints.is_empty(), "no checkpoints on empty log");
        assert!(projection.plan_artifacts.is_empty(), "no plans on empty log");
        assert!(projection.completed_results.is_empty(), "no results on empty log");
    }

    // ------------------------------------------------------------------
    // Test 2: events folded into correct variants
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn build_timeline_folds_events_into_correct_variants() {
        let (_dir, pool) = test_pool().await;

        // Append a Finding.
        let mut finding = new_event("f1", WhiteboardKind::Finding);
        finding.payload = json!({ "note": "important finding" });
        whiteboard::append_whiteboard_event(&pool, &finding).await.expect("append finding");

        // Append a WriteApplied with a path.
        let mut write = new_event("w1", WhiteboardKind::WriteApplied);
        write.payload = json!({
            "input": { "operation": "write", "path": "src/lib.rs", "content": "fn main() {}" }
        });
        whiteboard::append_whiteboard_event(&pool, &write).await.expect("append write");

        // Append a SubtaskCompleted with task_id.
        let task_id = TaskId::new();
        let mut completed = new_event("c1", WhiteboardKind::SubtaskCompleted);
        completed.payload = json!({
            "task_id": task_id.to_string(),
            "summary": "implemented feature X",
            "files_modified": ["src/lib.rs"],
            "role": "coder"
        });
        whiteboard::append_whiteboard_event(&pool, &completed).await.expect("append completed");

        // Append a PlanApproved.
        let mut plan = new_event("p1", WhiteboardKind::PlanApproved);
        plan.plan_id = Some("plan-1".to_owned());
        plan.payload = json!({
            "task_description": "build feature X"
        });
        whiteboard::append_whiteboard_event(&pool, &plan).await.expect("append plan");

        let projection = build_timeline(&pool, None, None, u64::MAX).await.expect("build timeline");

        assert_eq!(projection.events.len(), 4, "four events folded");

        // Verify variant mapping.
        match &projection.events[0] {
            TimelineEvent::Whiteboard { kind, .. } => {
                assert_eq!(*kind, WhiteboardKind::Finding);
            }
            other => panic!("expected Whiteboard variant, got: {other:?}"),
        }
        match &projection.events[1] {
            TimelineEvent::WroteFile { path, .. } => {
                assert_eq!(path, "src/lib.rs");
            }
            other => panic!("expected WroteFile variant, got: {other:?}"),
        }
        match &projection.events[2] {
            TimelineEvent::SubtaskCompleted { summary, .. } => {
                assert_eq!(summary, "implemented feature X");
            }
            other => panic!("expected SubtaskCompleted variant, got: {other:?}"),
        }
        match &projection.events[3] {
            TimelineEvent::PlanApproved { plan_id, .. } => {
                assert_eq!(plan_id, "plan-1");
            }
            other => panic!("expected PlanApproved variant, got: {other:?}"),
        }

        // Plan artifact extracted (minimal: plan_id + task_description, no tasks).
        assert_eq!(projection.plan_artifacts.len(), 1, "one plan artifact");
        assert_eq!(projection.plan_artifacts[0].plan_id, "plan-1");
        assert_eq!(projection.plan_artifacts[0].task_description, "build feature X");
        assert!(projection.plan_artifacts[0].tasks.is_empty(), "tasks loaded from disk in Phase 4");

        // Completed result extracted.
        assert_eq!(projection.completed_results.len(), 1, "one completed result");
        assert!(projection.completed_results.contains_key(&task_id));
    }

    // ------------------------------------------------------------------
    // Test 3: gate_seq_cut filtering
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn build_timeline_respects_gate_seq_cut() {
        let (_dir, pool) = test_pool().await;

        // Append three events (they get gate_seq 1, 2, 3).
        for i in 1..=3 {
            let mut ev = new_event(&format!("e{i}"), WhiteboardKind::Finding);
            ev.payload = json!({ "seq": i });
            whiteboard::append_whiteboard_event(&pool, &ev).await.expect("append");
        }

        // Cut at gate_seq 2: only events 1 and 2 included.
        let projection = build_timeline(&pool, None, None, 2).await.expect("build at cut 2");
        assert_eq!(projection.events.len(), 2, "only events <= gate_seq 2");
        assert_eq!(projection.events[0].gate_seq(), 1);
        assert_eq!(projection.events[1].gate_seq(), 2);

        // Full log.
        let full = build_timeline(&pool, None, None, u64::MAX).await.expect("build full");
        assert_eq!(full.events.len(), 3, "all three events at full cut");
    }

    // ------------------------------------------------------------------
    // Test 4: plan_id filtering
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn build_timeline_filters_by_plan_id() {
        let (_dir, pool) = test_pool().await;

        // Plan-1 event.
        let mut p1 = new_event("p1-ev", WhiteboardKind::Finding);
        p1.plan_id = Some("plan-1".to_owned());
        whiteboard::append_whiteboard_event(&pool, &p1).await.expect("append p1");

        // Plan-2 event.
        let mut p2 = new_event("p2-ev", WhiteboardKind::Finding);
        p2.plan_id = Some("plan-2".to_owned());
        whiteboard::append_whiteboard_event(&pool, &p2).await.expect("append p2");

        // Unscoped event (plan_id = None).
        whiteboard::append_whiteboard_event(&pool, &new_event("ns", WhiteboardKind::Decision))
            .await
            .expect("append unscoped");

        // Filter to plan-1: only the plan-1 event.
        let projection =
            build_timeline(&pool, None, Some("plan-1"), u64::MAX).await.expect("plan-1 filter");
        assert_eq!(projection.events.len(), 1, "only plan-1 events");
        assert_eq!(projection.events[0].gate_seq(), 1);

        // No filter: all three events.
        let all = build_timeline(&pool, None, None, u64::MAX).await.expect("no filter");
        assert_eq!(all.events.len(), 3, "all events without plan filter");
    }

    // ------------------------------------------------------------------
    // Test 5: enrich_working_memory is idempotent
    // ------------------------------------------------------------------

    #[test]
    fn enrich_working_memory_is_idempotent() {
        let mut snapshot = WorkingMemorySnapshot {
            id: Ulid::new(),
            session_id: Ulid::new(),
            decisions: Vec::new(),
            task_tree: Vec::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };
        let original_decisions_len = snapshot.decisions.len();

        // Build a minimal projection with some data.
        let projection = TimelineProjection {
            events: vec![TimelineEvent::WroteFile {
                gate_seq: 1,
                path: "src/lib.rs".to_owned(),
                content_hash: "abc123".to_owned(),
                created_at: 1_700_000_000_000,
            }],
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };

        // First enrichment.
        enrich_working_memory(&mut snapshot, &projection);
        let after_first = snapshot.decisions.len();
        assert!(after_first > original_decisions_len, "enrichment added decisions");

        // Second enrichment: no additional decisions added.
        enrich_working_memory(&mut snapshot, &projection);
        assert_eq!(snapshot.decisions.len(), after_first, "idempotent: no duplicate decisions");
    }

    // ------------------------------------------------------------------
    // Test 6: enrich_working_memory modifies snapshot with all event types
    // ------------------------------------------------------------------

    #[test]
    fn enrich_working_memory_populates_all_decision_types() {
        let mut snapshot = WorkingMemorySnapshot {
            id: Ulid::new(),
            session_id: Ulid::new(),
            decisions: Vec::new(),
            task_tree: Vec::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };

        let task_id = TaskId::new();
        let mut completed_results = HashMap::new();
        completed_results.insert(
            task_id,
            AgentRunResult {
                task_id,
                role: concerto_core::types::AgentId::new("coder"),
                outcome: concerto_core::types::AgentOutcome::Success,
                summary: "built the thing".to_owned(),
                files_modified: vec![camino::Utf8PathBuf::from("src/lib.rs")],
                tool_call_count: 5,
                cost_usd: 0.01,
                latency_ms: 1000,
                provider: "openai".into(),
                model: "gpt-4".into(),
                tokens_in: 100,
                tokens_out: 50,
            },
        );

        let projection = TimelineProjection {
            events: vec![
                TimelineEvent::WroteFile {
                    gate_seq: 1,
                    path: "src/lib.rs".to_owned(),
                    content_hash: "h1".to_owned(),
                    created_at: 1_700_000_000_000,
                },
                TimelineEvent::Whiteboard {
                    event_id: "e1".to_owned(),
                    gate_seq: 2,
                    kind: WhiteboardKind::Finding,
                    scope: "research".to_owned(),
                    content_hash: "h2".to_owned(),
                    created_at: 1_700_000_000_001,
                },
            ],
            checkpoints: Vec::new(),
            plan_artifacts: vec![PlanArtifact {
                plan_id: "plan-1".to_owned(),
                task_description: "build feature X".to_owned(),
                tasks: Vec::new(),
            }],
            completed_results,
        };

        enrich_working_memory(&mut snapshot, &projection);

        // Should have decisions for: completed, plans, files written, whiteboard summary.
        assert!(snapshot.decisions.len() >= 4, "at least 4 decision types");
        let whats: Vec<&str> = snapshot.decisions.iter().map(|d| d.what.as_str()).collect();
        assert!(whats.iter().any(|w| w.contains("completed")), "has completed decision");
        assert!(whats.iter().any(|w| w.contains("plans")), "has plans decision");
        assert!(whats.iter().any(|w| w.contains("files written")), "has files decision");
        assert!(whats.iter().any(|w| w.contains("whiteboard")), "has whiteboard decision");
    }

    // ------------------------------------------------------------------
    // Test 7: truncate_summary
    // ------------------------------------------------------------------

    #[test]
    fn truncate_summary_short_string_unchanged() {
        assert_eq!(truncate_summary("hello", 10), "hello");
    }

    #[test]
    fn truncate_summary_long_string_truncated() {
        let long = "a".repeat(200);
        let result = truncate_summary(&long, 50);
        assert_eq!(result.len(), 53); // 50 bytes + "…" (3 bytes in UTF-8)
        assert!(result.ends_with('…'));
    }
}
