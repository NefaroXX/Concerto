//! H12 — observed maintenance outcomes.
//!
//! A durable ledger of maintenance *events* performed against this memory
//! database: revalidation, supersession, contradiction, expiry and orphan
//! pruning. Each row records what was acted on, the evidence cited, the
//! outcome (`completed` | `degraded` | `failed`) with a reason, who performed
//! it, when, and the project it ran for.
//!
//! Why a table and not log lines (ADR-65 §3/§4): the runtime authors observed
//! facts, and a decision must be able to cite an existing observation. "Was
//! this memory ever revalidated, superseded, contradicted, expired or pruned
//! — and did that maintenance succeed?" is a query, not a grep. Crucially, a
//! maintenance run that could not complete (store locked, embedder down,
//! cancelled mid-pass) is recorded as [`MaintenanceOutcome::Degraded`] or
//! [`MaintenanceOutcome::Failed`] with a reason — an *absent* record for an
//! attempted run is the defect H12 exists to close, so recording is
//! deliberately **not** gated on the maintenance pass's own cancellation
//! token (see [`MaintenanceLedger::record`]).
//!
//! Recording is best-effort, and the code claims no more than that: every
//! attempt records, but a ledger-write failure (the table unavailable, the
//! pool closed) degrades to a `tracing::warn!` and leaves the pass's own
//! result unchanged. A failed write cannot be recorded in the ledger it
//! failed to write — a bounded circularity, not a silent success.
//!
//! Identity: targets are memory-native ids (chunk ids, decision ids); a sweep
//! targets the project id. This does not reuse the `resource_facts` keying
//! (`project_root_hash` + `canonical_project_path`), which identifies
//! workspace *file paths* — a maintenance event acts on a stored memory
//! object, not a path, and this database is already project-scoped.
//!
//! Coordinator authority (plan constraint 4): these rows are *observations
//! about memory state*. They are never consulted to re-rank or override a
//! Coordinator selection; nothing in this module feeds dispatch.

use concerto_core::error::MemoryError;
use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;
use time::OffsetDateTime;

/// Upper bound on a stored reason string (bounded durable prose).
pub const MAX_REASON_CHARS: usize = 240;

/// What kind of maintenance event a row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaintenanceKind {
    /// A stored attribute was re-checked against its current source/model.
    Revalidation,
    /// A memory was superseded (by another chunk or a decision).
    Supersession,
    /// A contradiction was applied against a memory.
    Contradiction,
    /// A row was expired by a TTL/retention sweep.
    Expiry,
    /// A cold, linked derived row was pruned.
    OrphanPrune,
}

impl MaintenanceKind {
    /// The stable string form stored in the `kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Revalidation => "revalidation",
            Self::Supersession => "supersession",
            Self::Contradiction => "contradiction",
            Self::Expiry => "expiry",
            Self::OrphanPrune => "orphan_prune",
        }
    }

    /// Parse a stored `kind`. Unknown values return `None` so an older binary
    /// skips (fail-open) a newer row instead of erroring.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "revalidation" => Some(Self::Revalidation),
            "supersession" => Some(Self::Supersession),
            "contradiction" => Some(Self::Contradiction),
            "expiry" => Some(Self::Expiry),
            "orphan_prune" => Some(Self::OrphanPrune),
            _ => None,
        }
    }
}

/// How a maintenance event ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaintenanceOutcome {
    /// The pass ran to completion over its whole scope.
    Completed,
    /// The pass ran but could not fully complete — partial progress, a
    /// fail-open skip, or a cancelled pass. Never absent.
    Degraded,
    /// The pass could not run or aborted on an error.
    Failed,
}

impl MaintenanceOutcome {
    /// The stable string form stored in the `outcome` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }

    /// Parse a stored `outcome`; unknown values return `None` (fail-open).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "degraded" => Some(Self::Degraded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// What a maintenance event acted on. Keeps the `target` string
/// interpretable: a chunk id, a decision id, or a project sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaintenanceTargetKind {
    Chunk,
    Decision,
    /// A whole-project sweep (the target is the project id).
    Project,
}

impl MaintenanceTargetKind {
    /// The stable string form stored in the `target_kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::Decision => "decision",
            Self::Project => "project",
        }
    }

    /// Parse a stored `target_kind`; unknown values return `None` (fail-open).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chunk" => Some(Self::Chunk),
            "decision" => Some(Self::Decision),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// One durable maintenance observation.
///
/// Construct through [`MaintenanceRecord::completed`],
/// [`MaintenanceRecord::degraded`] or [`MaintenanceRecord::failed`]. The
/// degraded/failed constructors take a reason; a blank one is not rejected —
/// it is replaced by an explicit placeholder (see `with_outcome`), so a
/// missing reason is *made visible* in the stored row rather than silently
/// omitted. The constructor does not require a reason; it refuses to store an
/// empty one that would read as "no problem".
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceRecord {
    /// Unique row id (ULID).
    pub id: String,
    /// The maintenance event kind.
    pub kind: MaintenanceKind,
    /// What was acted on (chunk id, decision id, or project id for a sweep).
    pub target: String,
    /// How to interpret [`Self::target`].
    pub target_kind: MaintenanceTargetKind,
    /// Evidence ids cited (link sources/targets, decision ids, removed ids).
    pub evidence: Vec<String>,
    /// How the event ended.
    pub outcome: MaintenanceOutcome,
    /// Why, in bounded prose. A blank value for a degraded/failed outcome is
    /// replaced by an explicit placeholder, never stored empty.
    pub reason: String,
    /// Who performed it (explicit attribution, never inferred).
    pub actor: String,
    /// The project the event ran for.
    pub project_id: String,
    /// When the outcome was observed (UTC).
    pub observed_at: OffsetDateTime,
}

impl MaintenanceRecord {
    /// A fully completed event. `reason` is empty by construction — a
    /// completed pass needs no apology.
    #[must_use]
    pub fn completed(
        kind: MaintenanceKind,
        target: impl Into<String>,
        target_kind: MaintenanceTargetKind,
        actor: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Ulid::new().to_string(),
            kind,
            target: target.into(),
            target_kind,
            evidence: Vec::new(),
            outcome: MaintenanceOutcome::Completed,
            reason: String::new(),
            actor: actor.into(),
            project_id: project_id.into(),
            observed_at: OffsetDateTime::now_utc(),
        }
    }

    /// A degraded event (partial progress, a fail-open skip, or a cancelled
    /// pass). The reason is mandatory: a degraded run that cannot say why is
    /// not an honest record.
    #[must_use]
    pub fn degraded(
        kind: MaintenanceKind,
        target: impl Into<String>,
        target_kind: MaintenanceTargetKind,
        reason: impl Into<String>,
        actor: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self::with_outcome(
            kind,
            target.into(),
            target_kind,
            MaintenanceOutcome::Degraded,
            reason.into(),
            actor.into(),
            project_id.into(),
        )
    }

    /// A failed event. The reason is mandatory.
    #[must_use]
    pub fn failed(
        kind: MaintenanceKind,
        target: impl Into<String>,
        target_kind: MaintenanceTargetKind,
        reason: impl Into<String>,
        actor: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self::with_outcome(
            kind,
            target.into(),
            target_kind,
            MaintenanceOutcome::Failed,
            reason.into(),
            actor.into(),
            project_id.into(),
        )
    }

    fn with_outcome(
        kind: MaintenanceKind,
        target: String,
        target_kind: MaintenanceTargetKind,
        outcome: MaintenanceOutcome,
        reason: String,
        actor: String,
        project_id: String,
    ) -> Self {
        // A non-completed outcome with no reason is a programming defect, not
        // an absence: keep the record but make the gap explicit rather than
        // storing an empty reason that reads as "no problem".
        let reason = if reason.trim().is_empty() {
            "unspecified (no reason supplied by the maintenance action)".to_string()
        } else {
            reason
        };
        Self {
            id: Ulid::new().to_string(),
            kind,
            target,
            target_kind,
            evidence: Vec::new(),
            outcome,
            reason,
            actor,
            project_id,
            observed_at: OffsetDateTime::now_utc(),
        }
    }

    /// Attach the evidence ids this event cited.
    #[must_use]
    pub fn with_evidence(mut self, evidence: Vec<String>) -> Self {
        self.evidence = evidence;
        self
    }
}

/// Durable ledger of maintenance outcomes over one memory database pool.
///
/// Cheap to clone (an `Arc` handle is held by the callers that share it), and
/// fail-soft at the call sites: a ledger write failure logs and never changes
/// the maintenance action's own result.
#[derive(Clone)]
pub struct MaintenanceLedger {
    pool: SqlitePool,
}

/// The ledger plus the explicit attribution a maintenance-emitting store
/// carries: who the store acts as and which project it serves. A store that
/// holds no project id of its own (the decision and link stores) is given one
/// here rather than inventing it.
#[derive(Clone)]
pub struct MaintenanceContext {
    /// The shared ledger every emitter writes to.
    pub ledger: Arc<MaintenanceLedger>,
    /// Who performed the maintenance (explicit, never inferred).
    pub actor: String,
    /// The project the maintenance ran for.
    pub project_id: String,
}

impl MaintenanceContext {
    /// Build a context from a ledger, an actor label, and a project id.
    #[must_use]
    pub fn new(
        ledger: Arc<MaintenanceLedger>,
        actor: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self { ledger, actor: actor.into(), project_id: project_id.into() }
    }

    /// Record one outcome, fail-soft: a ledger write failure logs and never
    /// changes the emitter's own result.
    pub async fn record(&self, record: &MaintenanceRecord) {
        if let Err(error) = self.ledger.record(record).await {
            tracing::warn!(%error, "failed to record maintenance outcome");
        }
    }
}

/// DDL mirroring migration 003, so a pool created directly (without the
/// migration runner) still records; the migration stays the canonical path.
const CREATE_MAINTENANCE_OUTCOMES: &str = r#"
CREATE TABLE IF NOT EXISTS maintenance_outcomes (
    id          TEXT    PRIMARY KEY,
    kind        TEXT    NOT NULL,
    target      TEXT    NOT NULL,
    target_kind TEXT    NOT NULL,
    evidence    TEXT    NOT NULL DEFAULT '[]',
    outcome     TEXT    NOT NULL,
    reason      TEXT    NOT NULL DEFAULT '',
    actor       TEXT    NOT NULL,
    project_id  TEXT    NOT NULL,
    observed_at INTEGER NOT NULL
)
"#;

impl MaintenanceLedger {
    /// DDL-free constructor: assumes migration 003 has run (the real memory
    /// database does). Used by callers that hold an already-migrated pool.
    #[must_use]
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Ensure the ledger table exists (mirrors migration 003), then wrap the
    /// pool. For pools that may not have run the migration (tests, foreign
    /// pools) — the migration is still the canonical path.
    pub async fn connect(pool: SqlitePool) -> Result<Self, MemoryError> {
        sqlx::query(CREATE_MAINTENANCE_OUTCOMES)
            .execute(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("maintenance ledger DDL failed: {e}")))?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_maintenance_outcomes_target \
             ON maintenance_outcomes(target)",
        )
        .execute(&pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("maintenance ledger DDL failed: {e}")))?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_maintenance_outcomes_project \
             ON maintenance_outcomes(project_id, observed_at)",
        )
        .execute(&pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("maintenance ledger DDL failed: {e}")))?;
        Ok(Self { pool })
    }

    /// The backing pool (so sibling maintenance writers share one ledger).
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Persist one outcome. Append-only.
    ///
    /// Deliberately takes **no** cancellation token: a maintenance pass that
    /// was itself cancelled must still record its degraded outcome, so gating
    /// this write on the pass's token would make the cancelled-pass case
    /// unrecordable — the exact absence bug this ledger exists to prevent.
    /// The write is one bounded atomic INSERT.
    pub async fn record(&self, record: &MaintenanceRecord) -> Result<(), MemoryError> {
        let mut reason = record.reason.clone();
        crate::recall::truncate_to_chars(&mut reason, MAX_REASON_CHARS);
        let evidence = serde_json::to_string(&record.evidence).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO maintenance_outcomes \
             (id, kind, target, target_kind, evidence, outcome, reason, actor, project_id, observed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(record.kind.as_str())
        .bind(&record.target)
        .bind(record.target_kind.as_str())
        .bind(&evidence)
        .bind(record.outcome.as_str())
        .bind(&reason)
        .bind(&record.actor)
        .bind(&record.project_id)
        .bind(record.observed_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("record maintenance outcome failed: {e}")))?;
        Ok(())
    }

    /// All outcomes recorded against one target, newest first.
    pub async fn for_target(
        &self,
        target: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<MaintenanceRecord>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let rows = sqlx::query(
            "SELECT id, kind, target, target_kind, evidence, outcome, reason, actor, \
             project_id, observed_at \
             FROM maintenance_outcomes WHERE target = ? \
             ORDER BY observed_at DESC, id DESC",
        )
        .bind(target)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        Ok(rows.iter().filter_map(row_to_record).collect())
    }

    /// The most recent outcomes across all targets, newest first, bounded by
    /// `limit` (a caller-supplied bound; `0` reads nothing).
    pub async fn list(
        &self,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<MaintenanceRecord>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT id, kind, target, target_kind, evidence, outcome, reason, actor, \
             project_id, observed_at \
             FROM maintenance_outcomes ORDER BY observed_at DESC, id DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        Ok(rows.iter().filter_map(row_to_record).collect())
    }

    /// Every outcome recorded for a project, newest first.
    pub async fn for_project(
        &self,
        project_id: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<MaintenanceRecord>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let rows = sqlx::query(
            "SELECT id, kind, target, target_kind, evidence, outcome, reason, actor, \
             project_id, observed_at \
             FROM maintenance_outcomes WHERE project_id = ? \
             ORDER BY observed_at DESC, id DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        Ok(rows.iter().filter_map(row_to_record).collect())
    }
}

/// Decode one stored row, or `None` to SKIP it.
///
/// An unknown `kind`/`outcome`/`target_kind` (a row written by a newer binary
/// — the enums are `#[non_exhaustive]`, so this is expected) is skipped with a
/// per-row warning, never an error: one future row must not brick a whole
/// read. This is the fail-open contract the migration and the enum `parse`
/// docs promise. Every recognized row decodes; a malformed `evidence` blob
/// falls back to an empty list (never an error).
fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Option<MaintenanceRecord> {
    let id: String = row.get("id");
    let kind_str: String = row.get("kind");
    let outcome_str: String = row.get("outcome");
    let target_kind_str: String = row.get("target_kind");
    let Some(kind) = MaintenanceKind::parse(&kind_str) else {
        tracing::warn!(
            row_id = %id,
            kind = %kind_str,
            "skipping maintenance outcome with unknown kind (fail-open)"
        );
        return None;
    };
    let Some(outcome) = MaintenanceOutcome::parse(&outcome_str) else {
        tracing::warn!(
            row_id = %id,
            outcome = %outcome_str,
            "skipping maintenance outcome with unknown outcome (fail-open)"
        );
        return None;
    };
    let Some(target_kind) = MaintenanceTargetKind::parse(&target_kind_str) else {
        tracing::warn!(
            row_id = %id,
            target_kind = %target_kind_str,
            "skipping maintenance outcome with unknown target kind (fail-open)"
        );
        return None;
    };
    let evidence_json: String = row.get("evidence");
    let evidence: Vec<String> = serde_json::from_str(&evidence_json).unwrap_or_default();
    let observed_at_unix: i64 = row.get("observed_at");
    Some(MaintenanceRecord {
        id,
        kind,
        target: row.get("target"),
        target_kind,
        evidence,
        outcome,
        reason: row.get("reason"),
        actor: row.get("actor"),
        project_id: row.get("project_id"),
        observed_at: OffsetDateTime::from_unix_timestamp(observed_at_unix)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> (SqlitePool, MaintenanceLedger) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let ledger = MaintenanceLedger::connect(pool.clone()).await.unwrap();
        (pool, ledger)
    }

    fn completed(target: &str) -> MaintenanceRecord {
        MaintenanceRecord::completed(
            MaintenanceKind::OrphanPrune,
            target,
            MaintenanceTargetKind::Project,
            "test-actor",
            "proj-1",
        )
    }

    #[test]
    fn kind_outcome_round_trip() {
        for kind in [
            MaintenanceKind::Revalidation,
            MaintenanceKind::Supersession,
            MaintenanceKind::Contradiction,
            MaintenanceKind::Expiry,
            MaintenanceKind::OrphanPrune,
        ] {
            assert_eq!(MaintenanceKind::parse(kind.as_str()), Some(kind));
        }
        for outcome in [
            MaintenanceOutcome::Completed,
            MaintenanceOutcome::Degraded,
            MaintenanceOutcome::Failed,
        ] {
            assert_eq!(MaintenanceOutcome::parse(outcome.as_str()), Some(outcome));
        }
        for target_kind in [
            MaintenanceTargetKind::Chunk,
            MaintenanceTargetKind::Decision,
            MaintenanceTargetKind::Project,
        ] {
            assert_eq!(MaintenanceTargetKind::parse(target_kind.as_str()), Some(target_kind));
        }
        assert_eq!(MaintenanceKind::parse("future"), None);
        assert_eq!(MaintenanceOutcome::parse("maybe"), None);
    }

    #[tokio::test]
    async fn record_and_read_round_trip() {
        let (_pool, ledger) = ledger().await;
        let record =
            completed("chunk-1").with_evidence(vec!["src-1".to_string(), "src-2".to_string()]);
        ledger.record(&record).await.expect("record");

        let read = ledger.for_target("chunk-1", CancellationToken::new()).await.unwrap();
        assert_eq!(read.len(), 1);
        // `observed_at` is stored at unix-second precision (like
        // `decisions.created_at`); every other field round-trips exactly.
        assert_eq!(read[0].id, record.id);
        assert_eq!(read[0].kind, record.kind);
        assert_eq!(read[0].target, record.target);
        assert_eq!(read[0].target_kind, record.target_kind);
        assert_eq!(read[0].outcome, record.outcome);
        assert_eq!(read[0].reason, record.reason);
        assert_eq!(read[0].actor, record.actor);
        assert_eq!(read[0].project_id, record.project_id);
        assert_eq!(
            read[0].observed_at.unix_timestamp(),
            record.observed_at.unix_timestamp(),
            "observed-at survives at second precision"
        );
        assert_eq!(read[0].evidence, vec!["src-1".to_string(), "src-2".to_string()]);

        let all = ledger.list(10, CancellationToken::new()).await.unwrap();
        assert_eq!(all.len(), 1);
        let by_project = ledger.for_project("proj-1", CancellationToken::new()).await.unwrap();
        assert_eq!(by_project.len(), 1);
        assert!(ledger.for_target("other", CancellationToken::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn degraded_and_failed_require_and_keep_a_reason() {
        let (_pool, ledger) = ledger().await;
        let degraded = MaintenanceRecord::degraded(
            MaintenanceKind::OrphanPrune,
            "proj-1",
            MaintenanceTargetKind::Project,
            "link store unavailable (fail-open)",
            "ttl-manager",
            "proj-1",
        );
        let failed = MaintenanceRecord::failed(
            MaintenanceKind::Expiry,
            "proj-1",
            MaintenanceTargetKind::Project,
            "purge aborted: database is locked",
            "ttl-manager",
            "proj-1",
        );
        ledger.record(&degraded).await.unwrap();
        ledger.record(&failed).await.unwrap();

        let all = ledger.list(10, CancellationToken::new()).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|r| r.outcome == MaintenanceOutcome::Degraded
            && r.reason.contains("link store unavailable")));
        assert!(all
            .iter()
            .any(|r| r.outcome == MaintenanceOutcome::Failed
                && r.reason.contains("database is locked")));

        // A non-completed outcome with a blank reason still records (absence
        // is never the answer) but names the gap explicitly.
        let blank = MaintenanceRecord::failed(
            MaintenanceKind::Expiry,
            "proj-1",
            MaintenanceTargetKind::Project,
            "   ",
            "ttl-manager",
            "proj-1",
        );
        ledger.record(&blank).await.unwrap();
        let read = ledger.for_target("proj-1", CancellationToken::new()).await.unwrap();
        assert_eq!(read.len(), 3, "degraded + failed + blank-reason failed all recorded");
        assert!(
            read.iter().any(
                |r| r.outcome == MaintenanceOutcome::Failed && r.reason.contains("unspecified")
            ),
            "a blank reason is made explicit, never stored empty: {read:?}"
        );
    }

    #[tokio::test]
    async fn reason_is_bounded_on_write() {
        let (_pool, ledger) = ledger().await;
        let long = "x".repeat(MAX_REASON_CHARS * 4);
        let record = MaintenanceRecord::degraded(
            MaintenanceKind::Revalidation,
            "proj-1",
            MaintenanceTargetKind::Project,
            long,
            "actor",
            "proj-1",
        );
        ledger.record(&record).await.unwrap();
        let read = ledger.for_target("proj-1", CancellationToken::new()).await.unwrap();
        assert_eq!(read.len(), 1);
        assert!(read[0].reason.chars().count() <= MAX_REASON_CHARS);
    }

    #[tokio::test]
    async fn cancelled_reads_error_but_writes_are_not_gated_on_a_pass_token() {
        let (_pool, ledger) = ledger().await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            ledger.for_target("x", cancelled.clone()).await,
            Err(MemoryError::Cancelled)
        ));
        assert!(matches!(ledger.list(10, cancelled).await, Err(MemoryError::Cancelled)));

        // The write has no token parameter by design: a cancelled pass still
        // records its degraded outcome.
        ledger.record(&completed("chunk-after-cancel")).await.unwrap();
        assert_eq!(
            ledger.for_target("chunk-after-cancel", CancellationToken::new()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_stored_kind_is_skipped_not_fatal() {
        let (pool, ledger) = ledger().await;
        // A known row and a future-kind row (a newer binary wrote it).
        ledger.record(&completed("known-target")).await.unwrap();
        sqlx::query(
            "INSERT INTO maintenance_outcomes \
             (id, kind, target, target_kind, evidence, outcome, reason, actor, project_id, observed_at) \
             VALUES ('future-1', 'future_kind', 'future-target', 'chunk', '[]', 'completed', '', 'a', 'p', 2)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // The read SUCCEEDS, the unknown row is skipped (with a warning), and
        // every known row is still returned — one future row never bricks a
        // whole read (the migration/parse fail-open contract).
        let all = ledger.list(10, CancellationToken::new()).await.expect("read must not fail");
        assert_eq!(all.len(), 1, "only the known row is returned: {all:?}");
        assert_eq!(all[0].target, "known-target");

        // Same skip on the point read, and the known target still answers.
        assert!(
            ledger.for_target("future-target", CancellationToken::new()).await.unwrap().is_empty(),
            "the future-kind row is skipped, not reinterpreted"
        );
        assert_eq!(
            ledger.for_target("known-target", CancellationToken::new()).await.unwrap().len(),
            1
        );
        // The known row's project ("proj-1" from `completed`) still answers;
        // the future row's project "p" has only a skipped row.
        assert_eq!(ledger.for_project("proj-1", CancellationToken::new()).await.unwrap().len(), 1);
        assert!(
            ledger.for_project("p", CancellationToken::new()).await.unwrap().is_empty(),
            "the future-kind row is skipped on the project read too"
        );
    }

    #[tokio::test]
    async fn unknown_stored_outcome_and_target_kind_are_also_skipped() {
        let (pool, ledger) = ledger().await;
        sqlx::query(
            "INSERT INTO maintenance_outcomes \
             (id, kind, target, target_kind, evidence, outcome, reason, actor, project_id, observed_at) \
             VALUES ('future-2', 'expiry', 't2', 'chunk', '[]', 'maybe', '', 'a', 'p', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO maintenance_outcomes \
             (id, kind, target, target_kind, evidence, outcome, reason, actor, project_id, observed_at) \
             VALUES ('future-3', 'expiry', 't3', 'future_target_kind', '[]', 'completed', '', 'a', 'p', 2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let all = ledger.list(10, CancellationToken::new()).await.expect("read must not fail");
        assert!(all.is_empty(), "unknown outcome/target_kind rows are skipped: {all:?}");
    }
}
