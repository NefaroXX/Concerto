//! Durable plan bindings (ADR-55 Phase 2b live-fix).
//!
//! The in-run `/Apply|Replan` dialog is armed by the process-scoped
//! [`concerto_orchestrator::plan_approval::PlanApprovalRegistry`], which is
//! keyed by `(session_id, objective_hash)` and non-durable: an app restart
//! between a planning run and the user's "i approve the plan" clears the
//! registry, so the approval silently re-plans instead of offering the
//! dialog. This module mirrors the registry in the session database — same
//! key, same newest-wins UPSERT semantics, and rows are deleted when a plan
//! is applied so a later bare approval cannot re-arm a dialog for an
//! already-executed plan.

use time::OffsetDateTime;

use concerto_core::ids::Ulid;

use crate::SessionError;

/// One durable plan binding, mirroring the in-memory registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBindingRecord {
    /// Session the plan belongs to.
    pub session_id: Ulid,
    /// The objective input hash the plan is keyed on.
    pub objective_hash: String,
    /// Stable plan identifier; also names the durable `plan-<id>.json`
    /// artifact (ADR-52) and the audit `intent:plan` rows.
    pub plan_id: String,
    /// The rendered plan text shown in the Apply/Replan dialog.
    pub plan_text: String,
    /// blake3 fingerprint of `plan_text` captured at creation (ADR-55 §1
    /// pending: diff-vs-artifact). `None` for rows written before migration
    /// 025 — those are unverifiable at dialog arming.
    pub artifact_hash: Option<String>,
    /// The git revision the plan was created at, when known.
    pub source_revision: Option<String>,
    /// When the plan was recorded (UTC); drives newest-wins ordering after a
    /// restart, so a rehydrated binding ages like the original one.
    pub created_at: OffsetDateTime,
}

/// Save (upsert) a plan binding, newest-wins per `(session_id, objective_hash)`.
///
/// Mirrors the in-memory registry: a whitespace-only plan is never stored.
/// Fail-soft contract at call sites: a persistence failure never fails the
/// run — the in-memory registry still arms the dialog in-process.
pub async fn save_plan_binding(
    pool: &sqlx::SqlitePool,
    record: &PlanBindingRecord,
) -> Result<(), SessionError> {
    if record.plan_text.trim().is_empty() {
        return Ok(());
    }
    let created_at_ms = record.created_at.unix_timestamp() * 1000;
    sqlx::query(
        "INSERT INTO plan_bindings
             (session_id, objective_hash, plan_id, plan_text, source_revision, artifact_hash, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (session_id, objective_hash) DO UPDATE SET
             plan_id = excluded.plan_id,
             plan_text = excluded.plan_text,
             source_revision = excluded.source_revision,
             artifact_hash = excluded.artifact_hash,
             created_at_ms = excluded.created_at_ms",
    )
    .bind(record.session_id.to_string())
    .bind(&record.objective_hash)
    .bind(&record.plan_id)
    .bind(&record.plan_text)
    .bind(&record.source_revision)
    .bind(&record.artifact_hash)
    .bind(created_at_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the newest durable binding for `session_id` across every objective.
///
/// Ordering is `created_at_ms DESC, id DESC` (mirrors the registry's
/// `created_at` + `plan_id` tie-break; the row id is the insertion order).
/// Returns `Ok(None)` for an unknown session or when the session has no
/// bindings.
pub async fn load_newest_plan_binding(
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
) -> Result<Option<PlanBindingRecord>, SessionError> {
    let row = sqlx::query_as::<_, PlanBindingRow>(
        "SELECT session_id, objective_hash, plan_id, plan_text, source_revision, artifact_hash, created_at_ms
         FROM plan_bindings
         WHERE session_id = ?
         ORDER BY created_at_ms DESC, id DESC
         LIMIT 1",
    )
    .bind(session_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Delete the durable binding for `(session_id, objective_hash)`, if present.
///
/// Called when an Apply decision executes the stored plan: the plan is
/// consumed, so a later bare approval ("yes") must not re-arm the dialog for
/// an already-executed plan. Deleting a missing row is a no-op that returns
/// `Ok(false)`.
pub async fn delete_plan_binding(
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
    objective_hash: &str,
) -> Result<bool, SessionError> {
    let result =
        sqlx::query("DELETE FROM plan_bindings WHERE session_id = ? AND objective_hash = ?")
            .bind(session_id.to_string())
            .bind(objective_hash)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Raw row shape for `sqlx::query_as` (TEXT columns stay strings; created_at
/// is stored as an integer of UTC milliseconds).
#[derive(Debug, sqlx::FromRow)]
struct PlanBindingRow {
    session_id: String,
    objective_hash: String,
    plan_id: String,
    plan_text: String,
    source_revision: Option<String>,
    artifact_hash: Option<String>,
    created_at_ms: i64,
}

impl TryFrom<PlanBindingRow> for PlanBindingRecord {
    type Error = SessionError;

    fn try_from(row: PlanBindingRow) -> Result<Self, SessionError> {
        let session_id = Ulid::from_string(&row.session_id).map_err(|error| {
            SessionError::Storage(format!("invalid session id in plan_bindings row: {error}"))
        })?;
        let created_at =
            OffsetDateTime::from_unix_timestamp(row.created_at_ms / 1000).map_err(|error| {
                SessionError::Storage(format!(
                    "invalid created_at_ms in plan_bindings row: {error}"
                ))
            })?;
        Ok(PlanBindingRecord {
            session_id,
            objective_hash: row.objective_hash,
            plan_id: row.plan_id,
            plan_text: row.plan_text,
            artifact_hash: row.artifact_hash,
            source_revision: row.source_revision,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory pool with migration 023 (plan_bindings has no foreign keys)
    /// plus migration 025 (the nullable artifact_hash column). Pinned to one
    /// connection: a pooled `sqlite::memory:` gives each connection its own
    /// database, which would silently lose the table.
    async fn test_pool() -> sqlx::SqlitePool {
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(include_str!("../migrations/023_plan_bindings.sql"))
            .execute(&pool)
            .await
            .expect("migration 023 applied");
        sqlx::query(include_str!("../migrations/025_plan_bindings_artifact_hash.sql"))
            .execute(&pool)
            .await
            .expect("migration 025 applied");
        pool
    }

    fn record(session: Ulid, objective: &str, plan_id: &str) -> PlanBindingRecord {
        PlanBindingRecord {
            session_id: session,
            objective_hash: objective.to_owned(),
            plan_id: plan_id.to_owned(),
            plan_text: format!("step 1: build {plan_id}"),
            // Opaque to the sessions crate — the hash is computed and
            // verified by concerto-orchestrator (plan_approval).
            artifact_hash: Some(format!("artifact-hash:{plan_id}")),
            source_revision: Some("abc1234".to_owned()),
            created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .unwrap_or_else(|_| OffsetDateTime::now_utc()),
        }
    }

    #[tokio::test]
    async fn save_load_and_delete_round_trip() {
        let pool = test_pool().await;
        let session = Ulid::new();
        let binding = record(session, "obj-hash-1", "plan-1");

        save_plan_binding(&pool, &binding).await.expect("save succeeds");
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        assert_eq!(loaded.as_ref(), Some(&binding), "round trip preserves fields");

        // Delete by (session, objective) removes it.
        assert!(delete_plan_binding(&pool, session, "obj-hash-1").await.expect("delete works"));
        assert!(
            load_newest_plan_binding(&pool, session).await.unwrap_or(None).is_none(),
            "binding removed after delete"
        );
        // Deleting a missing row is a no-op (false, not an error).
        assert!(!delete_plan_binding(&pool, session, "obj-hash-1").await.expect("no-op delete"));
    }

    #[tokio::test]
    async fn upsert_is_newest_wins_per_objective() {
        let pool = test_pool().await;
        let session = Ulid::new();

        // Two bindings for the same objective: the later write replaces it.
        let mut first = record(session, "obj-hash-1", "plan-1");
        save_plan_binding(&pool, &first).await.expect("first save");
        first.plan_id = "plan-1-rev2".to_owned();
        first.plan_text = "step 1: build plan-1-rev2".to_owned();
        save_plan_binding(&pool, &first).await.expect("upsert save");
        first.created_at =
            OffsetDateTime::from_unix_timestamp(1_700_000_001).expect("valid timestamp");

        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        let loaded = loaded.expect("binding exists");
        assert_eq!(loaded.plan_id, "plan-1-rev2", "newest write wins per objective");
        // The objective key count stays at one — a strict-hash replay still
        // matches only the newest binding.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_bindings")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn newest_across_objectives_orders_by_created_at_then_insertion() {
        let pool = test_pool().await;
        let session = Ulid::new();

        let older = record(session, "obj-older", "plan-old");
        save_plan_binding(&pool, &older).await.expect("older save");
        let newer = record(session, "obj-newer", "plan-new");
        save_plan_binding(&pool, &newer).await.expect("newer save");

        // `record` stamps both with the same created_at; insertion order
        // (id DESC) must break the tie deterministically.
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        assert_eq!(loaded.map(|b| b.plan_id), Some("plan-new".to_owned()));

        // Re-stamp the older row to be genuinely newer: age must win.
        let mut aged = older.clone();
        aged.created_at =
            OffsetDateTime::from_unix_timestamp(1_700_000_999).expect("valid timestamp");
        save_plan_binding(&pool, &aged).await.expect("aged save");
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        assert_eq!(loaded.map(|b| b.plan_id), Some("plan-old".to_owned()));
    }

    #[tokio::test]
    async fn whitespace_plan_text_is_never_stored() {
        let pool = test_pool().await;
        let session = Ulid::new();
        let mut binding = record(session, "obj-hash-1", "plan-1");
        binding.plan_text = "   ".to_owned();
        save_plan_binding(&pool, &binding).await.expect("save returns Ok for empty plan");
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        assert!(loaded.is_none(), "whitespace-only plan is not a binding");
    }

    #[tokio::test]
    async fn null_artifact_hash_round_trips_for_legacy_rows() {
        let pool = test_pool().await;
        let session = Ulid::new();
        let mut binding = record(session, "obj-hash-1", "plan-1");
        binding.artifact_hash = None;
        save_plan_binding(&pool, &binding).await.expect("save succeeds");
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        assert_eq!(
            loaded.as_ref(),
            Some(&binding),
            "a NULL artifact hash survives a save/load round trip"
        );
    }

    /// Backward-compat: a row written before migration 025 (raw INSERT without
    /// the artifact_hash column) loads with `artifact_hash: None` — the value
    /// the migration's `ADD COLUMN` backfills for existing rows.
    #[tokio::test]
    async fn legacy_row_without_artifact_hash_loads_as_none() {
        let pool = test_pool().await;
        let session = Ulid::new();
        sqlx::query(
            "INSERT INTO plan_bindings
                 (session_id, objective_hash, plan_id, plan_text, source_revision, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(session.to_string())
        .bind("obj-hash-1")
        .bind("plan-1")
        .bind("step 1: build verdict")
        .bind(Option::<String>::None)
        .bind(1_700_000_000_000i64)
        .execute(&pool)
        .await
        .expect("raw insert without artifact_hash");
        let loaded = load_newest_plan_binding(&pool, session).await.expect("load succeeds");
        let loaded = loaded.expect("binding exists");
        assert!(loaded.artifact_hash.is_none(), "legacy rows load with a NULL artifact hash");
    }
}
