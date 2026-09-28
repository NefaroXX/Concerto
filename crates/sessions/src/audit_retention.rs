//! Audit-log retention: archive old rows out of the hot database, then
//! delete them (DEFERRED rows 44 + 19).
//!
//! [`SqliteSessionStore::prune_audit`] moves rows strictly older than
//! `retention_days` into a sibling database (`audit-archive.db`) and only
//! then removes them from the hot `audit_log`:
//!
//! * **Archive first, delete second, one transaction.** SQLite's transaction
//!   spans the attached archive and the hot database, so a crash rolls both
//!   back — and if a crash ever did land between the two, the next prune
//!   converges (`INSERT OR IGNORE` on the primary key plus a "nothing about
//!   to be deleted is missing from the archive" check).
//! * **Same key as the hot database.** The archive is attached with the
//!   store's at-rest key, so enabling encryption never leaves old audit rows
//!   sitting in a plaintext file.
//! * **Fail closed.** A failed archive write (missing directory, full disk,
//!   wrong key) aborts before the delete: audit rows are never removed
//!   without a copy.
//! * **Cancellable.** `cancel` is checked before every statement via
//!   [`check_cancel`].
//!
//! `connect()` runs a best-effort prune at startup when `[audit]
//! retention_days` is set; direct callers get the full error.

use std::path::Path;

use concerto_core::CancellationToken;
use sqlx::{AssertSqlSafe, Connection, SqliteConnection};

use crate::at_rest::{exec_sql, sql_quote};
use crate::{check_cancel, SessionError, SqliteSessionStore};

/// Archive file name inside the archive directory (`[audit] archive_dir`,
/// defaulting to the app data directory).
const ARCHIVE_FILE_NAME: &str = "audit-archive.db";

/// Archive schema: `audit_log`'s columns in their physical order (the insert
/// below uses `SELECT *`, so the orders must match) plus `archived_at`.
/// No foreign keys — archived rows deliberately outlive sessions (ADR-40).
const ARCHIVE_DDL: &str = "
CREATE TABLE IF NOT EXISTS archive.audit_log_archive (
    id                      TEXT    PRIMARY KEY,
    session_id              TEXT,
    correlation_id          TEXT    NOT NULL,
    tool_name               TEXT    NOT NULL,
    verdict                 TEXT    NOT NULL,
    input_hash              TEXT    NOT NULL,
    rule_matched            TEXT,
    user_response           TEXT,
    created_at              INTEGER NOT NULL,
    profile_id              TEXT,
    resolved_executable     TEXT,
    argv                    TEXT,
    working_directory       TEXT,
    network_requested       INTEGER,
    filesystem_scope        TEXT,
    destructive_classification TEXT,
    exit_code               INTEGER,
    duration_ms             INTEGER,
    toolchain_version       TEXT,
    plan_id                 TEXT,
    source_revision         TEXT,
    error_kind              TEXT,
    server_id               TEXT,
    plugin_id               TEXT,
    path_operation          TEXT,
    attempted_path          TEXT,
    resolved_path           TEXT,
    attempted_destination   TEXT,
    resolved_destination    TEXT,
    result_facts            TEXT,
    archived_at             INTEGER NOT NULL
)";

/// Copy every row older than the cutoff into the archive. `SELECT *` keeps
/// the statement schema-tracking (no hand-maintained column list in the
/// source side): a future `audit_log` column makes this fail loudly — before
/// anything is deleted — instead of silently dropping the new column.
const ARCHIVE_INSERT: &str = "
INSERT OR IGNORE INTO archive.audit_log_archive (
    id, session_id, correlation_id, tool_name, verdict, input_hash,
    rule_matched, user_response, created_at, profile_id, resolved_executable,
    argv, working_directory, network_requested, filesystem_scope,
    destructive_classification, exit_code, duration_ms, toolchain_version,
    plan_id, source_revision, error_kind, server_id, plugin_id,
    path_operation, attempted_path, resolved_path, attempted_destination,
    resolved_destination, result_facts, archived_at
)
SELECT *, ? FROM audit_log WHERE created_at < ?";

/// Outcome of one `prune_audit` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRetentionReport {
    /// Rows newly written to the archive. Rows already present from an
    /// interrupted earlier run are not counted here (they are in `deleted`).
    pub archived: u64,
    /// Rows deleted from the hot `audit_log`.
    pub deleted: u64,
    /// Cutoff of this run: rows with `created_at < cutoff_unix` were eligible.
    pub cutoff_unix: i64,
}

impl SqliteSessionStore {
    /// Archive and delete `audit_log` rows strictly older than
    /// `retention_days`, writing the archive into `archive_dir`.
    ///
    /// Errors:
    /// * [`SessionError::Validation`] when `retention_days` is `0` (a config
    ///   value of `0` is folded to "disabled" before it ever reaches here;
    ///   this guard keeps a direct caller from wiping the log).
    /// * [`SessionError::Storage`] / [`SessionError::Database`] when the
    ///   archive could not be written — in which case **nothing** was
    ///   deleted.
    /// * [`SessionError::Database("operation cancelled")]` from
    ///   [`check_cancel`] when `cancel` fires between statements.
    pub async fn prune_audit(
        &self,
        retention_days: u64,
        archive_dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<AuditRetentionReport, SessionError> {
        if retention_days == 0 {
            return Err(SessionError::Validation(
                "audit retention must be at least 1 day; omit retention_days to \
                 keep every row"
                    .to_string(),
            ));
        }
        check_cancel(cancel)?;

        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        // A window wider than i64 seconds (~292 billion days) means "keep
        // everything"; never let the arithmetic wrap into a recent cutoff.
        let cutoff = i64::try_from(retention_days)
            .ok()
            .and_then(|days| days.checked_mul(86_400))
            .map_or(i64::MIN, |seconds| now.saturating_sub(seconds));
        if cutoff == i64::MIN {
            tracing::debug!(
                "audit retention window exceeds the i64 second range; keeping all rows"
            );
            return Ok(AuditRetentionReport { archived: 0, deleted: 0, cutoff_unix: cutoff });
        }

        std::fs::create_dir_all(archive_dir).map_err(|e| {
            SessionError::Storage(format!(
                "failed to create audit archive directory {}: {e}",
                archive_dir.display()
            ))
        })?;
        let archive_path = archive_dir.join(ARCHIVE_FILE_NAME);

        // A dedicated connection carrying the pool's connect options — for an
        // encrypted store that includes the `key` pragma *and* statement
        // logging disabled, so neither the hot key nor the archive key can
        // reach a log line. Closing it drops any ATTACH with it, so a failed
        // run can never leak an attachment back into the pool.
        let options = self.pool.connect_options();
        let mut conn = SqliteConnection::connect_with(&*options).await.map_err(|e| {
            SessionError::Database(format!("failed to open audit prune connection: {e}"))
        })?;

        let outcome = archive_and_delete(
            &mut conn,
            &archive_path,
            self.at_rest_key.as_ref(),
            cutoff,
            now,
            cancel,
        )
        .await;
        let closed = conn.close().await.map_err(|e| SessionError::Database(e.to_string()));

        let (archived, deleted) = match (outcome, closed) {
            (Ok(counts), Ok(())) => counts,
            (Ok(_), Err(error)) => return Err(error),
            (Err(error), closed) => {
                if let Err(close_error) = closed {
                    tracing::warn!(%close_error, "audit prune connection failed to close");
                }
                return Err(error);
            }
        };
        Ok(AuditRetentionReport { archived, deleted, cutoff_unix: cutoff })
    }
}

/// ATTACH the archive, then archive + delete in one transaction.
async fn archive_and_delete(
    conn: &mut SqliteConnection,
    archive_path: &Path,
    key: Option<&crate::AtRestKey>,
    cutoff: i64,
    archived_at: i64,
    cancel: &CancellationToken,
) -> Result<(u64, u64), SessionError> {
    check_cancel(cancel)?;

    // The archive gets the hot database's key: same protection, same key
    // rotation story, no plaintext side channel for pruned rows.
    let path = sql_quote(&archive_path.display().to_string());
    let attach = match key {
        Some(key) => format!("ATTACH DATABASE {path} AS archive KEY {}", sql_quote(&key.to_hex())),
        None => format!("ATTACH DATABASE {path} AS archive"),
    };
    sqlx::query(AssertSqlSafe(attach.as_str())).execute(&mut *conn).await.map_err(|e| {
        SessionError::Database(format!(
            "failed to open audit archive {}: {e}",
            archive_path.display()
        ))
    })?;

    let outcome = copy_then_delete(conn, cutoff, archived_at, cancel).await;
    if outcome.is_err() {
        // A failed statement leaves the transaction open; roll back before
        // detaching (detaching inside a transaction is an error).
        if let Err(error) = exec_sql(conn, "ROLLBACK;").await {
            tracing::debug!(%error, "audit prune rollback (no open transaction?)");
        }
    }
    if let Err(error) = exec_sql(conn, "DETACH DATABASE archive;").await {
        // This connection is dedicated and closed right after, so a failed
        // detach cannot leak into pooled work.
        tracing::warn!(%error, "failed to detach the audit archive");
    }
    outcome
}

/// The transaction proper: create the archive table, copy, *verify*, delete,
/// commit. Verified before the delete so `INSERT OR IGNORE` can never quietly
/// skip a row that is about to disappear from the hot log.
async fn copy_then_delete(
    conn: &mut SqliteConnection,
    cutoff: i64,
    archived_at: i64,
    cancel: &CancellationToken,
) -> Result<(u64, u64), SessionError> {
    exec_sql(conn, "BEGIN IMMEDIATE;").await?;
    check_cancel(cancel)?;

    exec_sql(conn, ARCHIVE_DDL).await?;
    let archived = sqlx::query(ARCHIVE_INSERT)
        .bind(archived_at)
        .bind(cutoff)
        .execute(&mut *conn)
        .await
        .map_err(|e| SessionError::Database(format!("failed to archive audit rows: {e}")))?
        .rows_affected();
    check_cancel(cancel)?;

    // Every eligible row must now exist in the archive.
    let missing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log a WHERE a.created_at < ? AND NOT EXISTS (\
         SELECT 1 FROM archive.audit_log_archive x WHERE x.id = a.id)",
    )
    .bind(cutoff)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| SessionError::Database(format!("failed to verify the audit archive: {e}")))?;
    if missing > 0 {
        return Err(SessionError::Storage(format!(
            "audit archive is missing {missing} row(s) that were about to be deleted; \
             aborting the prune without deleting anything"
        )));
    }

    let deleted = sqlx::query("DELETE FROM audit_log WHERE created_at < ?")
        .bind(cutoff)
        .execute(&mut *conn)
        .await
        .map_err(|e| SessionError::Database(format!("failed to delete archived audit rows: {e}")))?
        .rows_affected();

    exec_sql(conn, "COMMIT;").await?;
    Ok((archived, deleted))
}
