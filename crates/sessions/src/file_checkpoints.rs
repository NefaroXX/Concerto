//! Durable file-checkpoint sidecar (ADR-82 §4, slice 5 + chunked completion):
//! migrations 037 (record envelope + single-BLOB pre-images) and 039
//! (chunked pre-image store).
//!
//! [`FileCheckpointStore`] owns the sidecar tables — `file_checkpoints`
//! (the [`CheckpointRecord`](concerto_core::checkpoint::CheckpointRecord)
//! envelope per checkpoint), `checkpoint_pre_image_chunks` (the restorable
//! bytes, keyed by `(checkpoint_id, canonical_path, chunk_index)`), and the
//! legacy `checkpoint_pre_images` single-BLOB table (read-only back-compat
//! for pre-039 checkpoints; new captures write chunks only). It implements
//! the core [`CheckpointStore`](concerto_core::checkpoint::CheckpointStore)
//! trait the executor restores through and the core
//! [`MutationCaptureSink`](concerto_core::checkpoint::MutationCaptureSink)
//! trait the executor's pre-execution hook captures through, so neither the
//! gate seam nor the direct path names SQL.
//!
//! Design rules enforced here:
//!
//! * **Own byte budgets.** Every stored chunk is bounded by
//!   [`CHECKPOINT_BLOB_LIMIT_BYTES`](concerto_core::checkpoint::CHECKPOINT_BLOB_LIMIT_BYTES),
//!   and every checkpoint's banked total by
//!   [`CHECKPOINT_TOTAL_LIMIT_BYTES`](concerto_core::checkpoint::CHECKPOINT_TOTAL_LIMIT_BYTES) —
//!   never by the evidence-path `CACHE_LIMIT_BYTES`. Over-budget bytes are
//!   refused at the store boundary (creation fails loudly — a record claiming
//!   bytes the store will not hold is a caller bug); the capture seams only
//!   ever backfill within budget.
//! * **Reassembly is verified, never trusted.** Chunks for one path must form
//!   a dense `0..=max` index run whose concatenated bytes reproduce the
//!   recorded whole-file size and hash — a gap, size mismatch, or hash
//!   mismatch reads as hash-only/missing (restore reports `Partial`, never a
//!   success claim over corrupt bytes).
//! * **Derived data** (migration-030 precedent): the hashes on the
//!   never-pruned whiteboard log are the durable reference; these rows are
//!   repudiable bytes. [`ResourceFacts::rebuild_from_log`](crate::resource_facts::ResourceFacts::rebuild_from_log)
//!   deliberately neither repopulates nor deletes them.
//! * **Retention invariant:** prune-eligibility is gated on checkpoint status.
//!   [`FileCheckpointStore::prune_checkpoints`] deletes sidecar rows (chunks
//!   and legacy single-BLOB rows) whose checkpoint is no longer `Active` OR
//!   older than `retention_days` (delete-only, no archive — hashes stay on
//!   the never-pruned log) and transitions pruned-bytes checkpoints to
//!   `Expired`, withdrawing the restorable claim from that instant. The
//!   restore journal and attempt envelopes are collected under the identical
//!   predicate inside the same transaction — they exist only to resume or
//!   report a restore of that checkpoint, so they live no longer than the
//!   restorable bytes they describe; record envelopes always survive for
//!   audit. A checkpoint advertised as restorable is never prune-eligible.
//!   Cancellable at statement boundaries, fail-soft at the `connect()` call
//!   site (a failed prune leaves rows in place).
//! * **Encryption is conditional** (ADR-73 interaction): these tables live
//!   inside `sessions.db`, so they are encrypted WHEN the operator enables
//!   `encrypt_at_rest` — the same guarantee and operator control as every
//!   other table — and plaintext otherwise.

use std::collections::BTreeMap;

use concerto_core::checkpoint::{
    now_millis, plan_capture, CaptureKind, CheckpointRecord, CheckpointResource, CheckpointScope,
    CheckpointStatus, CheckpointStoreError, PreImageBytes, RestoreAttemptInfo,
    RestoreAttemptOutcome, RestoreMethod, RestoreProgressEntry, RestoreResourceState,
    CHECKPOINT_BLOB_LIMIT_BYTES, CHECKPOINT_TOTAL_LIMIT_BYTES,
};
use concerto_core::ids::Ulid;
use concerto_core::types::SessionContext;
use concerto_core::CancellationToken;
use sqlx::{Connection, SqliteConnection, SqlitePool};

use crate::at_rest::exec_sql;
use crate::resource_facts::CACHE_LIMIT_BYTES;
use crate::{check_cancel, SessionError};

// Compile-time proof the two budgets are not aliased: the checkpoint store
// must never key its bound off the evidence-path constant, even though both
// are 64 KiB today. If either side changes value this assertion still holds
// (it pins the *independence*, not the equality); the comment above explains
// why they must evolve separately.
const _BUDGETS_ARE_DISTINCT_CONSTANTS: () = {
    // Referencing both constants here forces a compile error if either is
    // ever removed or renamed, so the budgets cannot silently merge.
    let _ = CHECKPOINT_BLOB_LIMIT_BYTES;
    let _ = CACHE_LIMIT_BYTES;
};

/// Outcome of [`FileCheckpointStore::create_checkpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCreateReport {
    /// Sidecar rows newly banked with bytes.
    pub stored: usize,
    /// Included resources already hash-only (absent-at-checkpoint paths need
    /// no row; oversized/binary paths were excluded by the plan).
    pub hash_only: usize,
}

/// Outcome of [`FileCheckpointStore::prune_checkpoints`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointPruneReport {
    /// Checkpoints transitioned `Active` → `Expired` (restorable claim
    /// withdrawn by this run).
    pub expired: u64,
    /// Sidecar byte rows deleted (legacy + chunks; the restore journal and
    /// attempt envelopes deleted alongside under the same predicate are not
    /// counted — see [`FileCheckpointStore::prune_checkpoints`]).
    pub sidecar_rows_deleted: u64,
    /// Cutoff of this run: active checkpoints with `created_at < cutoff_unix`
    /// were eligible.
    pub cutoff_unix: i64,
}

/// One journal row as read: path, state word, verified hash, attempt,
// agent, update time, detail.
type JournalRow = (String, String, Option<String>, String, String, i64, String);

/// The checkpoint sidecar over one session database pool.
#[derive(Debug, Clone)]
pub struct FileCheckpointStore {
    pool: SqlitePool,
}

impl FileCheckpointStore {
    /// Open the sidecar over an already-migrated pool (migration 037).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Persist a checkpoint record plus its snapshotted blobs.
    ///
    /// `blobs` maps canonical paths to their checkpoint-time bytes (the
    /// creation snapshot, arbitrary bytes — binary included). Every
    /// `included` resource carrying a `byte_ref` must have an entry here
    /// within [`CHECKPOINT_TOTAL_LIMIT_BYTES`], and the entries' combined
    /// total must fit that same budget (the creation seam enforces it
    /// first-fit; this is the backstop); disagreement fails loudly
    /// ([`SessionError::Storage`]) — a record claiming bytes the store will
    /// not hold is a caller bug, and failing here keeps the retention
    /// invariant ("never advertise restorable without bytes") from ever
    /// going stale. Hash-only resources (no `byte_ref`) bank no row:
    /// absence restores exactly without bytes.
    ///
    /// Each blob is stored as [`CHECKPOINT_BLOB_LIMIT_BYTES`]-capped chunk
    /// rows (migration 039); reassembly verifies size and hash on read.
    pub async fn create_checkpoint(
        &self,
        record: &CheckpointRecord,
        blobs: &BTreeMap<String, Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<CheckpointCreateReport, SessionError> {
        check_cancel(cancel)?;
        let checkpoint_id = record.checkpoint_id.to_string();
        let record_json =
            record.to_json().map_err(|e| SessionError::Serialization(e.to_string()))?;

        // Strict agreement first: the store never persists a record whose
        // byte claims it will not honor.
        let mut banked_total: usize = 0;
        for resource in &record.included {
            if resource.byte_ref.is_some() {
                match blobs.get(&resource.canonical_path) {
                    Some(bytes) if bytes.len() <= CHECKPOINT_TOTAL_LIMIT_BYTES => {
                        banked_total = banked_total.saturating_add(bytes.len());
                    }
                    Some(bytes) => {
                        return Err(SessionError::Storage(format!(
                            "checkpoint {} path {} banks {} bytes, over CHECKPOINT_TOTAL_LIMIT_BYTES ({CHECKPOINT_TOTAL_LIMIT_BYTES})",
                            checkpoint_id,
                            resource.canonical_path,
                            bytes.len()
                        )));
                    }
                    None => {
                        return Err(SessionError::Storage(format!(
                            "checkpoint {checkpoint_id} claims bytes for {} but no blob was snapshotted",
                            resource.canonical_path
                        )));
                    }
                }
            }
        }
        if banked_total > CHECKPOINT_TOTAL_LIMIT_BYTES {
            return Err(SessionError::Storage(format!(
                "checkpoint {checkpoint_id} banks {banked_total} bytes total, over CHECKPOINT_TOTAL_LIMIT_BYTES ({CHECKPOINT_TOTAL_LIMIT_BYTES})"
            )));
        }

        sqlx::query(
            "INSERT INTO file_checkpoints \
             (checkpoint_id, session_id, status, record_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&checkpoint_id)
        .bind(record.session_id.to_string())
        .bind(record.status.state_str())
        .bind(&record_json)
        .bind(record.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(SessionError::from)?;
        check_cancel(cancel)?;

        let mut report = CheckpointCreateReport { stored: 0, hash_only: 0 };
        for resource in &record.included {
            let Some(byte_ref) = resource.byte_ref.as_ref() else {
                report.hash_only += 1;
                continue;
            };
            // Agreement was verified above; the lookup cannot miss.
            let Some(bytes) = blobs.get(&resource.canonical_path) else {
                return Err(SessionError::Storage(format!(
                    "checkpoint {checkpoint_id} lost blob for {} after agreement",
                    resource.canonical_path
                )));
            };
            let hash = blake3::hash(bytes).to_hex().to_string();
            insert_byte_chunks(
                &self.pool,
                &byte_ref.checkpoint_id,
                &byte_ref.canonical_path,
                &hash,
                bytes,
                CaptureKind::Snapshot,
                record.created_at_ms,
                cancel,
            )
            .await?;
            report.stored += 1;
        }
        Ok(report)
    }

    /// Backfill pre-image bytes for active checkpoints covering `captures`.
    ///
    /// Both capture seams (the gate beside the `WriteApplied` append, the
    /// executor's pre-execution hook on the direct path) funnel here:
    /// first bytes win at path granularity (a path that already holds chunk
    /// rows — or a legacy snapshot row — keeps them, so the
    /// checkpoint-creation snapshot stays authoritative), over-budget
    /// captures are skipped (the total budget is a store boundary, never
    /// exceeded), and only paths the checkpoint's own record banks
    /// (`byte_ref` present) gain rows — hash-only resources stay hash-only.
    /// Returns the paths newly banked.
    ///
    /// Fail-soft contract: callers on the write hot path ignore the `Err`
    /// (capture failure downgrades the resource, never fails the write); the
    /// error exists so tests and direct callers can assert.
    pub async fn backfill_pre_images(
        &self,
        captures: &[(String, String, Vec<u8>)],
        session_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<usize, SessionError> {
        check_cancel(cancel)?;
        if captures.is_empty() {
            return Ok(0);
        }
        // Cheap short-circuit: no active checkpoint means no matching work.
        let active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM file_checkpoints WHERE status = 'active'")
                .fetch_one(&self.pool)
                .await
                .map_err(SessionError::from)?;
        if active == 0 {
            return Ok(0);
        }
        check_cancel(cancel)?;

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT checkpoint_id, record_json FROM file_checkpoints WHERE status = 'active'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(SessionError::from)?;
        let mut banked = 0usize;
        for (path, hash, bytes) in captures {
            if bytes.len() > CHECKPOINT_TOTAL_LIMIT_BYTES {
                tracing::debug!(
                    path = %path,
                    bytes = bytes.len(),
                    "checkpoint backfill skips over-budget pre-image (total-budget boundary)"
                );
                continue;
            }
            for (checkpoint_id, record_json) in &rows {
                check_cancel(cancel)?;
                let Ok(record) = CheckpointRecord::from_json(record_json) else {
                    tracing::warn!(
                        checkpoint_id = %checkpoint_id,
                        "checkpoint backfill skips an undecodable record envelope"
                    );
                    continue;
                };
                if !record_covers_path(&record, path, session_id) {
                    continue;
                }
                if !record
                    .included
                    .iter()
                    .any(|r| r.canonical_path == *path && r.byte_ref.is_some())
                {
                    continue;
                }
                if path_has_banked_bytes(&self.pool, checkpoint_id, path).await? {
                    continue;
                }
                insert_byte_chunks(
                    &self.pool,
                    checkpoint_id,
                    path,
                    hash,
                    bytes,
                    CaptureKind::PreImage,
                    now_millis(),
                    cancel,
                )
                .await?;
                banked += 1;
            }
        }
        Ok(banked)
    }

    /// Latest write-gate pre-image hash known for `canonical_path`: the
    /// newest `write-applied` row (bounded scan) whose `pre_images` map names
    /// the path. Provenance for [`CheckpointResource::pre_hash`] only — never
    /// a restore source. `None` when the log knows no pre-image (fresh path,
    /// unscanned history, or unreadable payload — all fail-soft).
    pub async fn latest_write_pre_hash(
        &self,
        canonical_path: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, SessionError> {
        check_cancel(cancel)?;
        // Bounded scan: the newest write-applied rows first. A path whose
        // last write is older than this window reads `None` (provenance
        // degrades, capture does not fail).
        const PRE_HASH_SCAN_ROWS: i64 = 200;
        let payloads: Vec<(String,)> = sqlx::query_as(
            "SELECT payload FROM whiteboard_events WHERE kind = 'write-applied' \
             ORDER BY gate_seq DESC LIMIT ?",
        )
        .bind(PRE_HASH_SCAN_ROWS)
        .fetch_all(&self.pool)
        .await
        .map_err(SessionError::from)?;
        for (payload_json,) in payloads {
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
                continue;
            };
            if let Some(hash) = payload
                .get("pre_images")
                .and_then(|map| map.get(canonical_path))
                .and_then(serde_json::Value::as_str)
            {
                return Ok(Some(hash.to_owned()));
            }
        }
        Ok(None)
    }

    /// Load one checkpoint's record envelope (`None` when unknown).
    ///
    /// The `status` column is the source of truth for lifecycle (it is what
    /// retention mutates); the embedded JSON status is reconciled onto it so
    /// a reader never sees a stale claim after a prune transitioned the row.
    pub async fn load_checkpoint_record(
        &self,
        checkpoint_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<CheckpointRecord>, SessionError> {
        check_cancel(cancel)?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT status, record_json FROM file_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(checkpoint_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(SessionError::from)?;
        let Some((status_col, record_json)) = row else {
            return Ok(None);
        };
        let mut record = CheckpointRecord::from_json(&record_json)
            .map_err(|e| SessionError::Serialization(e.to_string()))?;
        reconcile_status(&mut record, &status_col);
        Ok(Some(record))
    }

    /// Transition `Active`/`Partial` → `Restored{at_ms, by}`. Returns `false`
    /// (changing nothing) when already `Restored` — the idempotent re-restore
    /// path. Refuses from terminal states so a prune-expired checkpoint can
    /// never be re-armed restorable behind retention's back.
    pub async fn transition_to_restored(
        &self,
        checkpoint_id: &str,
        at_ms: i64,
        by: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, SessionError> {
        check_cancel(cancel)?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT status, record_json FROM file_checkpoints WHERE checkpoint_id = ?",
        )
        .bind(checkpoint_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(SessionError::from)?;
        let Some((status_col, _)) = row else {
            return Err(SessionError::Storage(format!(
                "checkpoint {checkpoint_id} has no record row"
            )));
        };
        if status_col == "restored" {
            return Ok(false);
        }
        if status_col != "active" && status_col != "partial" {
            return Err(SessionError::Storage(format!(
                "checkpoint {checkpoint_id} is '{status_col}', not in a restorable state"
            )));
        }
        let patched = patch_status_json(
            &self.current_record_json(checkpoint_id, cancel).await?,
            &CheckpointStatus::Restored { at_ms, by: by.to_owned() },
        )?;
        sqlx::query("UPDATE file_checkpoints SET status = 'restored', record_json = ? WHERE checkpoint_id = ?")
            .bind(patched)
            .bind(checkpoint_id)
            .execute(&self.pool)
            .await
            .map_err(SessionError::from)?;
        Ok(true)
    }

    /// Transition to `Partial{missing}` after a restore that found some paths
    /// byte-less. Allowed from `Active`/`Partial` (a retry narrows it) and
    /// from `Restored` (a later partial re-restore honestly re-opens the
    /// claim — the bytes, not the status, decide).
    pub async fn transition_to_partial(
        &self,
        checkpoint_id: &str,
        missing: Vec<String>,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        let current = self.current_record_json(checkpoint_id, cancel).await?;
        let patched = patch_status_json(&current, &CheckpointStatus::Partial { missing })?;
        let affected = sqlx::query(
            "UPDATE file_checkpoints SET status = 'partial', record_json = ? WHERE checkpoint_id = ?",
        )
        .bind(patched)
        .bind(checkpoint_id)
        .execute(&self.pool)
        .await
        .map_err(SessionError::from)?
        .rows_affected();
        if affected == 0 {
            return Err(SessionError::Storage(format!(
                "checkpoint {checkpoint_id} has no record row"
            )));
        }
        Ok(())
    }

    /// Extend the retention run to the sidecar: delete byte rows whose
    /// checkpoint is no longer `Active` OR older than `retention_days`, and
    /// transition aged-`Active` checkpoints to `Expired` (restorable claim
    /// withdrawn from that instant).
    ///
    /// Delete-only, no archive: the hashes stay on the never-pruned
    /// whiteboard log, and every record envelope survives for audit. The
    /// restore journal and attempt envelopes for the SAME predicate-matched
    /// checkpoints go in the same transaction (they describe a restore of a
    /// checkpoint that is no longer restorable-within-window; a later attempt
    /// without them re-verifies from scratch rather than assuming progress).
    /// Shares `prune_audit`'s cutoff arithmetic (including the overflow
    /// keeps-all behavior) so the two halves of one retention run can never
    /// disagree on the window.
    ///
    /// [`CheckpointPruneReport::sidecar_rows_deleted`] counts byte rows only
    /// (legacy + chunks), as before — the bookkeeping deletes are reported
    /// through `tracing` rather than a new field, keeping the report shape
    /// stable.
    pub async fn prune_checkpoints(
        &self,
        retention_days: u64,
        cancel: &CancellationToken,
    ) -> Result<CheckpointPruneReport, SessionError> {
        if retention_days == 0 {
            return Err(SessionError::Validation(
                "checkpoint retention must be at least 1 day; omit retention_days to keep every row"
                    .to_string(),
            ));
        }
        check_cancel(cancel)?;

        let now = now_millis();
        let cutoff = i64::try_from(retention_days)
            .ok()
            .and_then(|days| days.checked_mul(86_400))
            .map_or(i64::MIN, |seconds| now.saturating_sub(seconds));
        if cutoff == i64::MIN {
            tracing::debug!(
                "checkpoint retention window exceeds the i64 second range; keeping all rows"
            );
            return Ok(CheckpointPruneReport {
                expired: 0,
                sidecar_rows_deleted: 0,
                cutoff_unix: cutoff,
            });
        }

        // Aged-but-still-Active checkpoints whose claim this run withdraws.
        let to_expire: Vec<(String,)> = sqlx::query_as(
            "SELECT checkpoint_id FROM file_checkpoints WHERE status = 'active' AND created_at < ?",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(SessionError::from)?;
        check_cancel(cancel)?;

        // One transaction on a DEDICATED connection (like `prune_audit`): pool
        // statements may land on different connections, which would split the
        // transaction and wedge on locks. A crash inside converges on the next
        // run (the delete is `IN`-gated on the same status/age predicate, and
        // the transition is idempotent over the same `to_expire` set).
        let options = self.pool.connect_options();
        let mut conn = SqliteConnection::connect_with(&options).await.map_err(|e| {
            SessionError::Database(format!("failed to open checkpoint prune connection: {e}"))
        })?;
        let outcome =
            delete_and_expire(&mut conn, &to_expire, cutoff, retention_days, cancel).await;
        match outcome {
            Ok((expired, deleted)) => {
                exec_sql(&mut conn, "COMMIT;").await?;
                close_conn(conn).await?;
                Ok(CheckpointPruneReport {
                    expired,
                    sidecar_rows_deleted: deleted,
                    cutoff_unix: cutoff,
                })
            }
            Err(error) => {
                if let Err(rollback) = exec_sql(&mut conn, "ROLLBACK;").await {
                    tracing::debug!(%rollback, "checkpoint prune rollback (no open transaction?)");
                }
                close_conn(conn).await?;
                Err(error)
            }
        }
    }

    /// Current record JSON for `checkpoint_id` (fails when the row is absent).
    async fn current_record_json(
        &self,
        checkpoint_id: &str,
        cancel: &CancellationToken,
    ) -> Result<String, SessionError> {
        check_cancel(cancel)?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT record_json FROM file_checkpoints WHERE checkpoint_id = ?")
                .bind(checkpoint_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(SessionError::from)?;
        row.map(|(json,)| json).ok_or_else(|| {
            SessionError::Storage(format!("checkpoint {checkpoint_id} has no record row"))
        })
    }

    /// Open one restore attempt's durable envelope (`INSERT OR IGNORE`: the
    /// `attempt_id` PRIMARY KEY makes re-submitting the same attempt a replay,
    /// never a fork — the gate's `call_id` dedup contract at attempt
    /// granularity).
    pub async fn begin_restore_attempt(
        &self,
        attempt: &RestoreAttemptInfo,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        sqlx::query(
            "INSERT OR IGNORE INTO checkpoint_restore_attempts \
             (attempt_id, checkpoint_id, by_agent, started_at, outcome) \
             VALUES (?, ?, ?, ?, 'running')",
        )
        .bind(&attempt.attempt_id)
        .bind(&attempt.checkpoint_id)
        .bind(&attempt.by_agent)
        .bind(attempt.started_at_ms)
        .execute(&self.pool)
        .await
        .map_err(SessionError::from)?;
        Ok(())
    }

    /// Stamp one attempt's terminal outcome. Updating a missing attempt is a
    /// no-op (the begin row is the authority; a finish without a begin only
    /// means the begin was lost — the per-resource rows still tell the truth).
    pub async fn finish_restore_attempt(
        &self,
        attempt_id: &str,
        outcome: RestoreAttemptOutcome,
        finished_at_ms: i64,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        sqlx::query(
            "UPDATE checkpoint_restore_attempts SET outcome = ?, finished_at = ? \
             WHERE attempt_id = ?",
        )
        .bind(outcome.as_str())
        .bind(finished_at_ms)
        .bind(attempt_id)
        .execute(&self.pool)
        .await
        .map_err(SessionError::from)?;
        Ok(())
    }

    /// Upsert one resource's restore progress, monotonically: `running` never
    /// overwrites `done` (a concurrent duplicate submission cannot regress a
    /// landed write), while `done` always wins (a resume's verification
    /// refreshes the row). Every other transition applies unconditionally —
    /// `failed`/`missing`/`pending` describe the latest attempt's knowledge,
    /// and a missing row means "unknown progress", never "assume done".
    pub async fn record_restore_progress(
        &self,
        checkpoint_id: &str,
        entry: &RestoreProgressEntry,
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(cancel)?;
        sqlx::query(
            "INSERT INTO checkpoint_restore_journal \
             (checkpoint_id, canonical_path, state, content_hash, attempt_id, by_agent, updated_at, detail) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(checkpoint_id, canonical_path) DO UPDATE SET \
               state = excluded.state, content_hash = excluded.content_hash, \
               attempt_id = excluded.attempt_id, by_agent = excluded.by_agent, \
               updated_at = excluded.updated_at, detail = excluded.detail \
             WHERE checkpoint_restore_journal.state != 'done' OR excluded.state = 'done'",
        )
        .bind(checkpoint_id)
        .bind(&entry.canonical_path)
        .bind(entry.state.as_str())
        .bind(entry.content_hash.as_deref())
        .bind(&entry.attempt_id)
        .bind(&entry.by_agent)
        .bind(entry.updated_at_ms)
        .bind(&entry.detail)
        .execute(&self.pool)
        .await
        .map_err(SessionError::from)?;
        Ok(())
    }

    /// Load the durable per-resource restore progress for `checkpoint_id`,
    /// in canonical-path order. A row carrying an unknown state word reads as
    /// `failed` (re-verify, never assume done) with a warning — the journal
    /// must degrade to "unknown progress", never to a fabricated `done`.
    pub async fn load_restore_progress(
        &self,
        checkpoint_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<RestoreProgressEntry>, SessionError> {
        check_cancel(cancel)?;
        let rows: Vec<JournalRow> = sqlx::query_as(
            "SELECT canonical_path, state, content_hash, attempt_id, by_agent, updated_at, detail \
                 FROM checkpoint_restore_journal WHERE checkpoint_id = ? ORDER BY canonical_path",
        )
        .bind(checkpoint_id)
        .fetch_all(&self.pool)
        .await
        .map_err(SessionError::from)?;
        let mut entries = Vec::with_capacity(rows.len());
        for (
            canonical_path,
            state_raw,
            content_hash,
            attempt_id,
            by_agent,
            updated_at_ms,
            detail,
        ) in rows
        {
            let Some(state) = RestoreResourceState::parse(&state_raw) else {
                tracing::warn!(
                    checkpoint_id = %checkpoint_id,
                    canonical_path = %canonical_path,
                    state = %state_raw,
                    "restore journal holds an unknown state word; reading as failed (re-verify, never assume done)"
                );
                entries.push(RestoreProgressEntry {
                    canonical_path,
                    state: RestoreResourceState::Failed,
                    content_hash,
                    attempt_id,
                    by_agent,
                    updated_at_ms,
                    detail,
                });
                continue;
            };
            entries.push(RestoreProgressEntry {
                canonical_path,
                state,
                content_hash,
                attempt_id,
                by_agent,
                updated_at_ms,
                detail,
            });
        }
        Ok(entries)
    }

    /// Load one attempt envelope (`None` when the id was never begun).
    pub async fn load_restore_attempt(
        &self,
        attempt_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<(RestoreAttemptInfo, Option<(String, Option<i64>)>)>, SessionError> {
        check_cancel(cancel)?;
        let row: Option<(String, String, String, i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT attempt_id, checkpoint_id, by_agent, started_at, outcome, finished_at \
             FROM checkpoint_restore_attempts WHERE attempt_id = ?",
        )
        .bind(attempt_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(SessionError::from)?;
        Ok(row.map(|(attempt_id, checkpoint_id, by_agent, started_at_ms, outcome, finished_at)| {
            (
                RestoreAttemptInfo { attempt_id, checkpoint_id, by_agent, started_at_ms },
                Some((outcome, finished_at)),
            )
        }))
    }
}

/// The prune transaction proper on a dedicated connection: delete byte rows
/// first, then withdraw the aged-Active claims. Verified ordering (delete
/// before transition) keeps the invariant "a checkpoint advertised as
/// restorable is never prune-eligible" at every instant: the only rows that
/// can lose bytes while still `Active` are the ones this same transaction
/// transitions.
async fn delete_and_expire(
    conn: &mut SqliteConnection,
    to_expire: &[(String,)],
    cutoff: i64,
    retention_days: u64,
    cancel: &CancellationToken,
) -> Result<(u64, u64), SessionError> {
    exec_sql(conn, "BEGIN IMMEDIATE;").await?;
    check_cancel(cancel)?;
    let legacy_deleted = sqlx::query(
        "DELETE FROM checkpoint_pre_images WHERE checkpoint_id IN (\
           SELECT checkpoint_id FROM file_checkpoints \
           WHERE status != 'active' OR created_at < ?)",
    )
    .bind(cutoff)
    .execute(&mut *conn)
    .await
    .map_err(SessionError::from)?
    .rows_affected();
    let chunks_deleted = sqlx::query(
        "DELETE FROM checkpoint_pre_image_chunks WHERE checkpoint_id IN (\
           SELECT checkpoint_id FROM file_checkpoints \
           WHERE status != 'active' OR created_at < ?)",
    )
    .bind(cutoff)
    .execute(&mut *conn)
    .await
    .map_err(SessionError::from)?
    .rows_affected();
    // Restore bookkeeping is collected with the IDENTICAL predicate as the
    // byte rows: the journal and attempt envelopes exist only to resume or
    // report a restore OF that checkpoint, so they live exactly as long as
    // the checkpoint stays restorable within the retention window (at most).
    // Deleting them is fail-safe, not lossy: no journal row reads as
    // "unknown progress, re-verify" on any later attempt, never "assume
    // done". Record envelopes and the never-pruned whiteboard log still
    // survive for audit — the durable hashes and the attempts' story on the
    // log are not this transaction's to touch.
    let journal_deleted = sqlx::query(
        "DELETE FROM checkpoint_restore_journal WHERE checkpoint_id IN (\
           SELECT checkpoint_id FROM file_checkpoints \
           WHERE status != 'active' OR created_at < ?)",
    )
    .bind(cutoff)
    .execute(&mut *conn)
    .await
    .map_err(SessionError::from)?
    .rows_affected();
    let attempts_deleted = sqlx::query(
        "DELETE FROM checkpoint_restore_attempts WHERE checkpoint_id IN (\
           SELECT checkpoint_id FROM file_checkpoints \
           WHERE status != 'active' OR created_at < ?)",
    )
    .bind(cutoff)
    .execute(&mut *conn)
    .await
    .map_err(SessionError::from)?
    .rows_affected();
    tracing::debug!(
        journal_deleted,
        attempts_deleted,
        "checkpoint retention collected restore bookkeeping"
    );
    let deleted = legacy_deleted + chunks_deleted;
    let mut expired = 0u64;
    for (checkpoint_id,) in to_expire {
        check_cancel(cancel)?;
        let current: (String,) =
            sqlx::query_as("SELECT record_json FROM file_checkpoints WHERE checkpoint_id = ?")
                .bind(checkpoint_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(SessionError::from)?;
        let patched = patch_status_json(
            &current.0,
            &CheckpointStatus::Expired {
                reason: format!(
                    "retention prune: sidecar bytes older than {retention_days} day(s) collected; restorable claim withdrawn"
                ),
            },
        )?;
        sqlx::query(
            "UPDATE file_checkpoints SET status = 'expired', record_json = ? WHERE checkpoint_id = ?",
        )
        .bind(patched)
        .bind(checkpoint_id)
        .execute(&mut *conn)
        .await
        .map_err(SessionError::from)?;
        expired += 1;
    }
    Ok((expired, deleted))
}

/// Store one path's bytes as [`CHECKPOINT_BLOB_LIMIT_BYTES`]-capped chunk
/// rows (migration 039). Every chunk repeats the whole-file `hash`,
/// whole-file `size_bytes`, `kind`, and `captured_at` so a chunk subset
/// alone still names its file for the reassembly verifier. `INSERT OR
/// IGNORE` per chunk keeps a racing first writer's bytes (creation snapshot
/// vs seam backfill); callers that need path-granularity first-wins check
/// [`path_has_banked_bytes`] first.
#[allow(clippy::too_many_arguments)]
async fn insert_byte_chunks(
    pool: &SqlitePool,
    checkpoint_id: &str,
    canonical_path: &str,
    hash: &str,
    bytes: &[u8],
    kind: CaptureKind,
    captured_at_ms: i64,
    cancel: &CancellationToken,
) -> Result<(), SessionError> {
    let size_bytes = bytes.len() as i64;
    let kind_str = kind.as_str();
    for (index, chunk) in bytes.chunks(CHECKPOINT_BLOB_LIMIT_BYTES).enumerate() {
        crate::check_cancel(cancel)?;
        sqlx::query(
            "INSERT OR IGNORE INTO checkpoint_pre_image_chunks \
             (checkpoint_id, canonical_path, chunk_index, content_hash, content, chunk_size, size_bytes, capture_kind, captured_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(checkpoint_id)
        .bind(canonical_path)
        .bind(index as i64)
        .bind(hash)
        .bind(chunk)
        .bind(chunk.len() as i64)
        .bind(size_bytes)
        .bind(kind_str)
        .bind(captured_at_ms)
        .execute(pool)
        .await
        .map_err(SessionError::from)?;
    }
    // An empty file banks zero chunks: record the emptiness explicitly with
    // one zero-length chunk so "banked empty" stays distinguishable from
    // "no row" (Absent) on read.
    if bytes.is_empty() {
        crate::check_cancel(cancel)?;
        sqlx::query(
            "INSERT OR IGNORE INTO checkpoint_pre_image_chunks \
             (checkpoint_id, canonical_path, chunk_index, content_hash, content, chunk_size, size_bytes, capture_kind, captured_at) \
             VALUES (?, ?, 0, ?, ?, 0, 0, ?, ?)",
        )
        .bind(checkpoint_id)
        .bind(canonical_path)
        .bind(hash)
        .bind(bytes)
        .bind(kind_str)
        .bind(captured_at_ms)
        .execute(pool)
        .await
        .map_err(SessionError::from)?;
    }
    Ok(())
}

/// Whether `checkpoint_id`+`path` already holds banked bytes — chunk rows
/// (migration 039) or a legacy single-BLOB row (migration 037). Backfill
/// consults this so the first bytes win at path granularity.
async fn path_has_banked_bytes(
    pool: &SqlitePool,
    checkpoint_id: &str,
    canonical_path: &str,
) -> Result<bool, SessionError> {
    let chunks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM checkpoint_pre_image_chunks \
         WHERE checkpoint_id = ? AND canonical_path = ?",
    )
    .bind(checkpoint_id)
    .bind(canonical_path)
    .fetch_one(pool)
    .await
    .map_err(SessionError::from)?;
    if chunks > 0 {
        return Ok(true);
    }
    let legacy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM checkpoint_pre_images \
         WHERE checkpoint_id = ? AND canonical_path = ?",
    )
    .bind(checkpoint_id)
    .bind(canonical_path)
    .fetch_one(pool)
    .await
    .map_err(SessionError::from)?;
    Ok(legacy > 0)
}

/// One chunk row as read for reassembly: index, bytes, whole-file hash,
/// whole-file size, provenance, capture time.
type ChunkRow = (i64, Vec<u8>, String, i64, String, i64);

/// Reassemble ordered chunk rows into sidecar bytes (pure: unit-testable
/// without a database).
///
/// Returns `Stored` only for a dense `0..=max` index run whose concatenated
/// bytes reproduce the recorded whole-file size with one unanimous hash and
/// one unanimous provenance; anything else (gap, size mismatch, hash or
/// provenance disagreement, empty input) reads as `HashOnly`/`Absent` —
/// restore then reports the path `missing` (Partial), never a success claim
/// over corrupt bytes. The hash re-verification at restore time is the
/// second line of defense; this structural check is the first.
fn reassemble_chunks(rows: Vec<ChunkRow>) -> PreImageBytes {
    if rows.is_empty() {
        return PreImageBytes::Absent;
    }
    let (hash, size_bytes, kind_raw, captured_at_ms) = {
        let (_, _, hash, size_bytes, kind_raw, captured_at_ms) = &rows[0];
        (hash.clone(), *size_bytes, kind_raw.clone(), *captured_at_ms)
    };
    for (expected, row) in rows.iter().enumerate() {
        let (index, _, row_hash, row_size, row_kind, _) = row;
        if *index != expected as i64
            || *row_hash != hash
            || *row_size != size_bytes
            || *row_kind != kind_raw
        {
            tracing::warn!(
                expected_index = expected,
                "checkpoint chunk set is structurally corrupt (gap or disagreement); reading hash-only"
            );
            return PreImageBytes::HashOnly { hash: Some(hash) };
        }
    }
    let mut bytes = Vec::new();
    for (_, chunk, _, _, _, _) in &rows {
        bytes.extend_from_slice(chunk);
    }
    if bytes.len() as i64 != size_bytes {
        tracing::warn!(
            expected = size_bytes,
            actual = bytes.len(),
            "checkpoint chunk set size mismatch; reading hash-only"
        );
        return PreImageBytes::HashOnly { hash: Some(hash) };
    }
    // Unknown provenance spells read as the WEAKER claim (pre-image, with
    // its caveat note) — never over-claim exactness.
    let capture_kind = CaptureKind::parse(&kind_raw).unwrap_or(CaptureKind::PreImage);
    PreImageBytes::Stored { bytes, hash, captured_at_ms, capture_kind }
}

/// Close a dedicated prune connection, surfacing close failures (a leaked
/// ATTACH cannot happen here — this connection never attaches — but a failed
/// close must not silently swallow the run's outcome).
async fn close_conn(conn: SqliteConnection) -> Result<(), SessionError> {
    conn.close().await.map_err(|e| SessionError::Database(e.to_string()))
}
/// Rewrite a record envelope's `status` field in place (the column stays the
/// source of truth; the JSON rides along for audit readers).
fn patch_status_json(record_json: &str, status: &CheckpointStatus) -> Result<String, SessionError> {
    let mut value: serde_json::Value = serde_json::from_str(record_json)?;
    value["status"] =
        serde_json::to_value(status).map_err(|e| SessionError::Serialization(e.to_string()))?;
    serde_json::to_string(&value).map_err(SessionError::from)
}

/// Reconcile an embedded JSON status onto the lifecycle column: the column
/// wins (retention mutates the column and patches the JSON in one
/// transaction, but a reader must never trust a stale embedded claim). States
/// carrying detail (`Restored`, `Expired`, `Partial`) keep the envelope's
/// detail when it agrees with the column; a column/envelope disagreement
/// falls back to the column's word with an honest generic detail rather than
/// a fabricated one.
fn reconcile_status(record: &mut CheckpointRecord, status_col: &str) {
    let column_word = record.status.state_str();
    if column_word == status_col {
        return;
    }
    record.status = match status_col {
        "active" => CheckpointStatus::Active,
        "superseded" => CheckpointStatus::Superseded,
        "restored" => match &record.status {
            already @ CheckpointStatus::Restored { .. } => already.clone(),
            // The column says restored but the envelope predates the detail:
            // keep the envelope's state rather than fabricating at/by. The
            // column still gates retention (it reads the column, not this).
            _ => return,
        },
        "expired" => CheckpointStatus::Expired {
            reason: "retention prune withdrew the restorable claim".to_string(),
        },
        "partial" => CheckpointStatus::Partial { missing: Vec::new() },
        _ => return,
    };
}

/// Whether `record`'s scope covers `path` for capture backfill.
///
/// * `FilesystemWrites` — exact canonical-path match only (never a prefix
///   guess: `notes.md` must not capture `notes.md.bak`).
/// * `Run` — the seam's session equals the record's session.
/// * `Task` — never matched at the seams: they carry no task context, and
///   guessing task attribution would mis-bank bytes. Task-scoped checkpoints
///   restore from their creation snapshot only (documented ceiling).
pub fn record_covers_path(record: &CheckpointRecord, path: &str, session_id: Option<&str>) -> bool {
    match &record.scope {
        CheckpointScope::FilesystemWrites { paths } => paths.iter().any(|p| p == path),
        CheckpointScope::Run => session_id == Some(record.session_id.to_string().as_str()),
        CheckpointScope::Task { .. } => false,
    }
}

#[async_trait::async_trait]
impl concerto_core::checkpoint::CheckpointStore for FileCheckpointStore {
    async fn load_record(
        &self,
        checkpoint_id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<CheckpointRecord>, CheckpointStoreError> {
        self.load_checkpoint_record(checkpoint_id, &cancel).await.map_err(store_error)
    }

    async fn load_bytes(
        &self,
        checkpoint_id: &str,
        canonical_path: &str,
        cancel: CancellationToken,
    ) -> Result<PreImageBytes, CheckpointStoreError> {
        check_cancel(&cancel).map_err(|_| CheckpointStoreError::Cancelled)?;
        // Chunked rows first (migration 039); the legacy single-BLOB table
        // stays as the fallback for pre-039 checkpoints.
        let chunks: Vec<(i64, Vec<u8>, String, i64, String, i64)> = sqlx::query_as(
            "SELECT chunk_index, content, content_hash, size_bytes, capture_kind, captured_at \
             FROM checkpoint_pre_image_chunks \
             WHERE checkpoint_id = ? AND canonical_path = ? ORDER BY chunk_index",
        )
        .bind(checkpoint_id)
        .bind(canonical_path)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        if !chunks.is_empty() {
            return Ok(reassemble_chunks(chunks));
        }
        let row: Option<(Option<Vec<u8>>, String, String, i64)> = sqlx::query_as(
            "SELECT content, content_hash, capture_kind, captured_at \
             FROM checkpoint_pre_images WHERE checkpoint_id = ? AND canonical_path = ?",
        )
        .bind(checkpoint_id)
        .bind(canonical_path)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        match row {
            None => Ok(PreImageBytes::Absent),
            Some((None, hash, _, _)) => Ok(PreImageBytes::HashOnly { hash: Some(hash) }),
            Some((Some(bytes), hash, kind_raw, captured_at_ms)) => {
                // Unknown provenance spells read as the WEAKER claim
                // (pre-image, with its caveat note) — never over-claim exactness.
                let capture_kind = CaptureKind::parse(&kind_raw).unwrap_or(CaptureKind::PreImage);
                Ok(PreImageBytes::Stored { bytes, hash, captured_at_ms, capture_kind })
            }
        }
    }

    async fn mark_restored(
        &self,
        checkpoint_id: &str,
        at_ms: i64,
        by: &str,
        cancel: CancellationToken,
    ) -> Result<bool, CheckpointStoreError> {
        self.transition_to_restored(checkpoint_id, at_ms, by, &cancel).await.map_err(store_error)
    }

    async fn mark_partial(
        &self,
        checkpoint_id: &str,
        missing: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<(), CheckpointStoreError> {
        self.transition_to_partial(checkpoint_id, missing, &cancel).await.map_err(store_error)
    }

    async fn load_restore_progress(
        &self,
        checkpoint_id: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<RestoreProgressEntry>, CheckpointStoreError> {
        self.load_restore_progress(checkpoint_id, &cancel).await.map_err(store_error)
    }

    async fn record_restore_progress(
        &self,
        checkpoint_id: &str,
        entry: &RestoreProgressEntry,
        cancel: CancellationToken,
    ) -> Result<(), CheckpointStoreError> {
        self.record_restore_progress(checkpoint_id, entry, &cancel).await.map_err(store_error)
    }

    async fn begin_restore_attempt(
        &self,
        attempt: &RestoreAttemptInfo,
        cancel: CancellationToken,
    ) -> Result<(), CheckpointStoreError> {
        self.begin_restore_attempt(attempt, &cancel).await.map_err(store_error)
    }

    async fn finish_restore_attempt(
        &self,
        attempt_id: &str,
        outcome: RestoreAttemptOutcome,
        finished_at_ms: i64,
        cancel: CancellationToken,
    ) -> Result<(), CheckpointStoreError> {
        self.finish_restore_attempt(attempt_id, outcome, finished_at_ms, &cancel)
            .await
            .map_err(store_error)
    }
}

#[async_trait::async_trait]
impl concerto_core::checkpoint::MutationCaptureSink for FileCheckpointStore {
    async fn capture_before_mutation(
        &self,
        canonical_paths: Vec<String>,
        session: &SessionContext,
        cancel: CancellationToken,
    ) {
        if cancel.is_cancelled() || canonical_paths.is_empty() {
            return;
        }
        // Read-before-materialize per path, fail-soft per path: a missing
        // file is a fresh create (no pre-image); any other read failure only
        // degrades that path (hash-only), never the write. Over-budget files
        // are skipped before reading (the backfill would refuse them — no
        // point materializing megabytes that can never bank).
        let mut captures = Vec::new();
        for path in &canonical_paths {
            if cancel.is_cancelled() {
                return;
            }
            let absolute = session.project_dir.join(path);
            match tokio::fs::metadata(&absolute).await {
                Ok(meta) if meta.len() > CHECKPOINT_TOTAL_LIMIT_BYTES as u64 => {
                    tracing::debug!(
                        path = %path,
                        bytes = meta.len(),
                        "checkpoint capture skips an over-budget pre-image before reading (fail-soft)"
                    );
                    continue;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(
                        path = %path,
                        %error,
                        "checkpoint capture skipped an unreadable pre-image (fail-soft)"
                    );
                    continue;
                }
            }
            match tokio::fs::read(&absolute).await {
                Ok(bytes) => {
                    captures.push((path.clone(), blake3::hash(&bytes).to_hex().to_string(), bytes));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(
                        path = %path,
                        %error,
                        "checkpoint capture skipped an unreadable pre-image (fail-soft)"
                    );
                }
            }
        }
        if captures.is_empty() {
            return;
        }
        let session_id = session.session_id.to_string();
        if let Err(error) =
            self.backfill_pre_images(&captures, Some(session_id.as_str()), &cancel).await
        {
            tracing::debug!(%error, "checkpoint capture backfill failed (fail-soft)");
        }
    }
}

/// Map a store-local failure onto the core store error (cancel stays cancel).
fn store_error(error: SessionError) -> CheckpointStoreError {
    match error {
        SessionError::Database(message) if message == "operation cancelled" => {
            CheckpointStoreError::Cancelled
        }
        SessionError::Database(message) => CheckpointStoreError::Storage(message),
        SessionError::Storage(message) => CheckpointStoreError::Storage(message),
        SessionError::Serialization(message) => CheckpointStoreError::Serialization(message),
        SessionError::Validation(message) => CheckpointStoreError::Storage(message),
        SessionError::NotFound(message) => CheckpointStoreError::Storage(message),
        SessionError::Lock(message) => CheckpointStoreError::Storage(message),
    }
}

/// Plan one path's capture the way the creation seam does: the orchestrator's
/// create path and tests share this exact rule application (total budget +
/// kind provenance from [`plan_capture`]).
pub fn plan_checkpoint_resource(
    checkpoint_id: &Ulid,
    canonical_path: &str,
    pre_hash: Option<String>,
    post_bytes: Option<&[u8]>,
) -> (Option<CheckpointResource>, Option<concerto_core::checkpoint::CheckpointExclusion>) {
    let planned = plan_capture(checkpoint_id, canonical_path, pre_hash, post_bytes);
    (planned.included, planned.excluded)
}

/// Default restore method for a fresh plan: `Manual` when nothing banked
/// bytes (there is nothing to auto-restore), `PreImageStore` otherwise. The
/// caller upgrades to `GitStash` explicitly for stash-backed checkpoints.
#[must_use]
pub fn default_method_for_plan(included: &[CheckpointResource]) -> RestoreMethod {
    if included.iter().any(|r| r.byte_ref.is_some()) {
        RestoreMethod::PreImageStore
    } else {
        RestoreMethod::Manual
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::checkpoint::{
        CheckpointExclusionClass, CheckpointResourceKind, CHECKPOINT_BLOB_LIMIT_BYTES,
        PARTIAL_SCOPE_LIMITATION,
    };

    async fn test_store() -> (tempfile::TempDir, FileCheckpointStore) {
        // File-backed pool (same topology as production): prune opens a
        // dedicated connection, which would see an empty database on a
        // `mode=memory` pool (each connection gets its own DB).
        let (dir, pool) = test_pool().await;
        (dir, FileCheckpointStore::new(pool))
    }

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("ckpt_test.db"))
            .create_if_missing(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    fn record_with(
        status: CheckpointStatus,
        included: Vec<CheckpointResource>,
        method: RestoreMethod,
    ) -> CheckpointRecord {
        CheckpointRecord::new(
            Ulid::new(),
            Ulid::new(),
            CheckpointScope::FilesystemWrites {
                paths: included.iter().map(|r| r.canonical_path.clone()).collect(),
            },
            included,
            Vec::new(),
            BTreeMap::new(),
            status,
            method,
            Vec::new(),
            "agent-a".to_string(),
            now_millis(),
            None,
        )
        .expect("test record builds")
    }

    fn banked(path: &str, cid: &Ulid) -> CheckpointResource {
        CheckpointResource {
            kind: CheckpointResourceKind::FileOverwrite,
            canonical_path: path.to_owned(),
            pre_hash: None,
            post_hash: Some("h".to_string()),
            byte_ref: Some(concerto_core::checkpoint::CheckpointByteRef {
                checkpoint_id: cid.to_string(),
                canonical_path: path.to_owned(),
            }),
            size_bytes: Some(3),
        }
    }

    #[tokio::test]
    async fn sidecar_round_trip_preserves_record_and_bytes() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("a.txt", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        let blobs = BTreeMap::from([("a.txt".to_owned(), b"abc".to_vec())]);
        let report = store.create_checkpoint(&record, &blobs, &token()).await.expect("create");
        assert_eq!(report, CheckpointCreateReport { stored: 1, hash_only: 0 });

        let back = store.load_checkpoint_record(&cid.to_string(), &token()).await.expect("load");
        assert_eq!(back, Some(record));

        match load_bytes_via_trait(&store, &cid.to_string(), "a.txt").await {
            PreImageBytes::Stored { bytes, hash, capture_kind, .. } => {
                assert_eq!(bytes, b"abc");
                assert_eq!(hash, blake3::hash(b"abc").to_hex().to_string());
                assert_eq!(capture_kind, CaptureKind::Snapshot);
            }
            other => panic!("expected Stored bytes, got {other:?}"),
        }
        // Unknown path reads Absent, never a guess.
        match load_bytes_via_trait(&store, &cid.to_string(), "missing.txt").await {
            PreImageBytes::Absent => {}
            other => panic!("expected Absent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn store_refuses_over_total_blobs_at_the_boundary() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // A record claiming bytes over the per-checkpoint total fails
        // loudly: the store never persists a restorable claim it will not
        // honor. (Multi-chunk sizes within the total are fine — chunking is
        // storage detail.)
        let record = record_with(
            CheckpointStatus::Active,
            vec![banked("big.bin", &cid)],
            RestoreMethod::PreImageStore,
        );
        let big = vec![b'x'; CHECKPOINT_TOTAL_LIMIT_BYTES + 1];
        let blobs = BTreeMap::from([("big.bin".to_owned(), big)]);
        match store.create_checkpoint(&record, &blobs, &token()).await {
            Err(SessionError::Storage(message)) => {
                assert!(message.contains("CHECKPOINT_TOTAL_LIMIT_BYTES"));
            }
            other => panic!("over-total blob must fail creation, got {other:?}"),
        }
        // And a missing blob for a byte-claiming resource fails too.
        match store.create_checkpoint(&record, &BTreeMap::new(), &token()).await {
            Err(SessionError::Storage(_)) => {}
            other => panic!("missing blob must fail creation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn store_refuses_a_running_total_over_budget() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // Each blob fits alone, but together they overflow the
        // per-checkpoint total: the store backstop refuses (the creation
        // seam normally downgrades first-fit before reaching here).
        let half = CHECKPOINT_TOTAL_LIMIT_BYTES / 2 + 1;
        let record = record_with(
            CheckpointStatus::Active,
            vec![banked("a.bin", &cid), banked("b.bin", &cid)],
            RestoreMethod::PreImageStore,
        );
        let blobs = BTreeMap::from([
            ("a.bin".to_owned(), vec![b'a'; half]),
            ("b.bin".to_owned(), vec![b'b'; half]),
        ]);
        match store.create_checkpoint(&record, &blobs, &token()).await {
            Err(SessionError::Storage(message)) => {
                assert!(message.contains("total"), "names the running total: {message}");
            }
            other => panic!("over-total checkpoint must fail creation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chunked_round_trip_reassembles_multi_chunk_binary() {
        use concerto_core::checkpoint::CHECKPOINT_BLOB_LIMIT_BYTES;
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // Non-UTF-8 bytes spanning several chunks: the reassembly must be
        // byte-exact, with the snapshot provenance intact.
        let bytes: Vec<u8> =
            (0..CHECKPOINT_BLOB_LIMIT_BYTES * 2 + 12345).map(|i| (i % 251) as u8).collect();
        assert!(bytes.len() > CHECKPOINT_BLOB_LIMIT_BYTES);
        assert!(std::str::from_utf8(&bytes).is_err(), "test bytes must be binary");
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("bin.dat", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        let report = store
            .create_checkpoint(
                &record,
                &BTreeMap::from([("bin.dat".to_owned(), bytes.clone())]),
                &token(),
            )
            .await
            .expect("create");
        assert_eq!(report, CheckpointCreateReport { stored: 1, hash_only: 0 });

        let chunk_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ?",
        )
        .bind(cid.to_string())
        .fetch_one(&store.pool)
        .await
        .expect("count chunks");
        assert_eq!(
            chunk_rows,
            bytes.len().div_ceil(CHECKPOINT_BLOB_LIMIT_BYTES) as i64,
            "one row per chunk"
        );

        match load_bytes_via_trait(&store, &cid.to_string(), "bin.dat").await {
            PreImageBytes::Stored { bytes: back, hash, capture_kind, .. } => {
                assert_eq!(back, bytes, "byte-exact reassembly");
                assert_eq!(hash, blake3::hash(&bytes).to_hex().to_string());
                assert_eq!(capture_kind, CaptureKind::Snapshot);
            }
            other => panic!("expected Stored bytes, got {other:?}"),
        }
    }

    /// The plan's "retained bytes" metric at the accounting level: the bytes the
    /// sidecar retains across every banked path equal the bytes written, for
    /// representative sizes (small text, multi-chunk binary within budget, a
    /// large within-budget file), and a hash-only resource banks nothing and is
    /// accounted as such. Per-file byte-exactness is asserted by the round-trip
    /// tests above; this is the aggregate accounting invariant, including the
    /// hash-only boundary.
    #[tokio::test]
    async fn retained_bytes_account_for_every_banked_path() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();

        let text = b"hello, checkpoint\n".to_vec();
        let binary: Vec<u8> =
            (0..CHECKPOINT_BLOB_LIMIT_BYTES + 4096).map(|i| (i % 251) as u8).collect();
        let large = vec![b'q'; CHECKPOINT_BLOB_LIMIT_BYTES * 3 + 7];
        let written_total = text.len() + binary.len() + large.len();

        let mut resources =
            vec![banked("small.txt", &cid), banked("bin.dat", &cid), banked("large.txt", &cid)];
        // A hash-only resource (no `byte_ref`) banks no bytes.
        resources.push(CheckpointResource {
            kind: CheckpointResourceKind::FileOverwrite,
            canonical_path: "hash-only.txt".to_owned(),
            pre_hash: Some("deadbeef".to_owned()),
            post_hash: Some("deadbeef".to_owned()),
            byte_ref: None,
            size_bytes: None,
        });

        let mut record =
            record_with(CheckpointStatus::Active, resources, RestoreMethod::PreImageStore);
        record.checkpoint_id = cid;
        let blobs = BTreeMap::from([
            ("small.txt".to_owned(), text.clone()),
            ("bin.dat".to_owned(), binary.clone()),
            ("large.txt".to_owned(), large.clone()),
        ]);
        let report = store.create_checkpoint(&record, &blobs, &token()).await.expect("create");
        assert_eq!(report, CheckpointCreateReport { stored: 3, hash_only: 1 });

        let mut retained_total = 0usize;
        for (path, original) in [("small.txt", &text), ("bin.dat", &binary), ("large.txt", &large)]
        {
            match load_bytes_via_trait(&store, &cid.to_string(), path).await {
                PreImageBytes::Stored { bytes, .. } => {
                    assert_eq!(&bytes, original, "{path} retains exactly the bytes written");
                    retained_total += bytes.len();
                }
                other => panic!("{path} must retain bytes, got {other:?}"),
            }
        }
        assert_eq!(retained_total, written_total, "retained bytes equal bytes written");

        // The hash-only path retains zero restorable bytes.
        match load_bytes_via_trait(&store, &cid.to_string(), "hash-only.txt").await {
            PreImageBytes::HashOnly { .. } | PreImageBytes::Absent => {}
            other => panic!("hash-only path must bank no bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_file_banks_an_explicit_empty_row() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("empty.txt", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        store
            .create_checkpoint(
                &record,
                &BTreeMap::from([("empty.txt".to_owned(), Vec::new())]),
                &token(),
            )
            .await
            .expect("create");
        // Banked-empty reads Stored(empty) — distinguishable from Absent, so
        // restore recreates the empty file instead of reporting it missing.
        match load_bytes_via_trait(&store, &cid.to_string(), "empty.txt").await {
            PreImageBytes::Stored { bytes, .. } => assert!(bytes.is_empty()),
            other => panic!("expected Stored empty bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_single_blob_rows_still_restore() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // A pre-039 checkpoint wrote the single-BLOB table only: the legacy
        // read path still serves it (back-compat for older checkpoints).
        sqlx::query(
            "INSERT INTO checkpoint_pre_images \
             (checkpoint_id, canonical_path, content_hash, content, size_bytes, capture_kind, captured_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(cid.to_string())
        .bind("old.txt")
        .bind(blake3::hash(b"legacy").to_hex().to_string())
        .bind(b"legacy".as_slice())
        .bind(6_i64)
        .bind(CaptureKind::Snapshot.as_str())
        .bind(now_millis())
        .execute(&store.pool)
        .await
        .expect("legacy row");
        match load_bytes_via_trait(&store, &cid.to_string(), "old.txt").await {
            PreImageBytes::Stored { bytes, .. } => assert_eq!(bytes, b"legacy"),
            other => panic!("expected legacy Stored bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gapped_chunk_set_reads_hash_only_never_bytes() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        let bytes: Vec<u8> =
            (0..CHECKPOINT_BLOB_LIMIT_BYTES * 3).map(|i| (i % 251) as u8).collect();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("gap.dat", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        store
            .create_checkpoint(
                &record,
                &BTreeMap::from([("gap.dat".to_owned(), bytes.clone())]),
                &token(),
            )
            .await
            .expect("create");
        // Corrupt the set the way a crash or tamper would: drop the middle
        // chunk. Reassembly must refuse bytes (restore reports Partial),
        // keeping the recorded hash for the honesty trail.
        sqlx::query(
            "DELETE FROM checkpoint_pre_image_chunks \
             WHERE checkpoint_id = ? AND canonical_path = ? AND chunk_index = 1",
        )
        .bind(cid.to_string())
        .bind("gap.dat")
        .execute(&store.pool)
        .await
        .expect("drop a chunk");
        match load_bytes_via_trait(&store, &cid.to_string(), "gap.dat").await {
            PreImageBytes::HashOnly { hash } => {
                assert_eq!(hash, Some(blake3::hash(&bytes).to_hex().to_string()));
            }
            other => panic!("gapped chunks must read hash-only, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tampered_chunk_bytes_stay_verifiable_against_the_recorded_hash() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        let bytes: Vec<u8> =
            (0..CHECKPOINT_BLOB_LIMIT_BYTES + 100).map(|i| (i % 251) as u8).collect();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("tamper.dat", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        store
            .create_checkpoint(
                &record,
                &BTreeMap::from([("tamper.dat".to_owned(), bytes.clone())]),
                &token(),
            )
            .await
            .expect("create");
        // Tamper that preserves structure (dense indices, same sizes): the
        // chunk read still returns Stored, but against the RECORDED hash —
        // so the restore-time re-verification catches it (never written).
        sqlx::query(
            "UPDATE checkpoint_pre_image_chunks SET content = ? \
             WHERE checkpoint_id = ? AND canonical_path = ? AND chunk_index = 0",
        )
        .bind(vec![0xAA_u8; CHECKPOINT_BLOB_LIMIT_BYTES])
        .bind(cid.to_string())
        .bind("tamper.dat")
        .execute(&store.pool)
        .await
        .expect("tamper a chunk");
        match load_bytes_via_trait(&store, &cid.to_string(), "tamper.dat").await {
            PreImageBytes::Stored { bytes: back, hash, .. } => {
                assert_ne!(back, bytes, "tamper took effect");
                assert_eq!(hash, blake3::hash(&bytes).to_hex().to_string());
                assert_ne!(
                    blake3::hash(&back).to_hex().to_string(),
                    hash,
                    "restore-time verification must fail on these bytes"
                );
            }
            other => panic!("structurally dense tamper still reads Stored, got {other:?}"),
        }
    }

    #[test]
    fn reassemble_chunks_rejects_structural_corruption() {
        use concerto_core::checkpoint::CHECKPOINT_BLOB_LIMIT_BYTES;
        let hash = blake3::hash(b"ab").to_hex().to_string();
        let good = vec![
            (0_i64, b"a".to_vec(), hash.clone(), 2_i64, "snapshot".to_string(), 7_i64),
            (1_i64, b"b".to_vec(), hash.clone(), 2_i64, "snapshot".to_string(), 7_i64),
        ];
        match reassemble_chunks(good) {
            PreImageBytes::Stored { bytes, .. } => assert_eq!(bytes, b"ab"),
            other => panic!("dense run reassembles, got {other:?}"),
        }
        // Gap.
        let gapped = vec![
            (0_i64, b"a".to_vec(), hash.clone(), 2_i64, "snapshot".to_string(), 7_i64),
            (2_i64, b"b".to_vec(), hash.clone(), 2_i64, "snapshot".to_string(), 7_i64),
        ];
        assert!(matches!(reassemble_chunks(gapped), PreImageBytes::HashOnly { .. }));
        // Size disagreement.
        let sized = vec![
            (0_i64, b"a".to_vec(), hash.clone(), 99_i64, "snapshot".to_string(), 7_i64),
            (1_i64, b"b".to_vec(), hash.clone(), 99_i64, "snapshot".to_string(), 7_i64),
        ];
        assert!(matches!(reassemble_chunks(sized), PreImageBytes::HashOnly { .. }));
        // Hash disagreement across chunks.
        let split = vec![
            (0_i64, b"a".to_vec(), hash.clone(), 2_i64, "snapshot".to_string(), 7_i64),
            (1_i64, b"b".to_vec(), "other".to_string(), 2_i64, "snapshot".to_string(), 7_i64),
        ];
        assert!(matches!(reassemble_chunks(split), PreImageBytes::HashOnly { .. }));
        // Empty input reads Absent.
        assert!(matches!(reassemble_chunks(Vec::new()), PreImageBytes::Absent));
        let _ = CHECKPOINT_BLOB_LIMIT_BYTES;
    }

    #[tokio::test]
    async fn prune_collects_multi_chunk_rows_and_expires() {
        let (_dir, store) = test_store().await;
        let old_ms = now_millis() - 30 * 86_400 * 1000;
        let cid = Ulid::new();
        let bytes: Vec<u8> =
            (0..CHECKPOINT_BLOB_LIMIT_BYTES * 2 + 10).map(|i| (i % 251) as u8).collect();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("aged.bin", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        record.created_at_ms = old_ms;
        store
            .create_checkpoint(&record, &BTreeMap::from([("aged.bin".to_owned(), bytes)]), &token())
            .await
            .expect("aged create");
        let report = store.prune_checkpoints(7, &token()).await.expect("prune");
        assert_eq!(report.expired, 1);
        let chunks_left: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM checkpoint_pre_image_chunks WHERE checkpoint_id = ?",
        )
        .bind(cid.to_string())
        .fetch_one(&store.pool)
        .await
        .expect("count chunks");
        assert_eq!(chunks_left, 0, "every chunk row collected");
        match load_bytes_via_trait(&store, &cid.to_string(), "aged.bin").await {
            PreImageBytes::Absent => {}
            other => panic!("pruned bytes must read Absent, got {other:?}"),
        }
        let back = store
            .load_checkpoint_record(&cid.to_string(), &token())
            .await
            .expect("load")
            .expect("envelope survives for audit");
        assert!(
            matches!(back.status, CheckpointStatus::Expired { .. }),
            "pruned-bytes checkpoint must read Expired, got {:?}",
            back.status
        );
    }

    #[tokio::test]
    async fn retention_prunes_by_status_and_age_with_expired_transition() {
        let (_dir, store) = test_store().await;
        let old_ms = now_millis() - 30 * 86_400 * 1000;

        // Fresh Active: bytes survive.
        let fresh_cid = Ulid::new();
        let mut fresh = record_with(
            CheckpointStatus::Active,
            vec![banked("fresh.txt", &fresh_cid)],
            RestoreMethod::PreImageStore,
        );
        fresh.checkpoint_id = fresh_cid;
        fresh.created_at_ms = now_millis();
        store
            .create_checkpoint(
                &fresh,
                &BTreeMap::from([("fresh.txt".to_owned(), b"f".to_vec())]),
                &token(),
            )
            .await
            .expect("fresh create");

        // Aged Active: bytes pruned AND status transitions to Expired.
        let aged_cid = Ulid::new();
        let mut aged = record_with(
            CheckpointStatus::Active,
            vec![banked("aged.txt", &aged_cid)],
            RestoreMethod::PreImageStore,
        );
        aged.checkpoint_id = aged_cid;
        aged.created_at_ms = old_ms;
        store
            .create_checkpoint(
                &aged,
                &BTreeMap::from([("aged.txt".to_owned(), b"a".to_vec())]),
                &token(),
            )
            .await
            .expect("aged create");

        // Non-Active (Restored) fresh: bytes pruned (terminal claims need no
        // bytes), status untouched.
        let done_cid = Ulid::new();
        let mut done = record_with(
            CheckpointStatus::Restored { at_ms: now_millis(), by: "agent-a".to_string() },
            vec![banked("done.txt", &done_cid)],
            RestoreMethod::PreImageStore,
        );
        done.checkpoint_id = done_cid;
        done.created_at_ms = now_millis();
        store
            .create_checkpoint(
                &done,
                &BTreeMap::from([("done.txt".to_owned(), b"d".to_vec())]),
                &token(),
            )
            .await
            .expect("done create");

        let whiteboard_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM whiteboard_events")
            .fetch_one(&store.pool)
            .await
            .expect("count whiteboard");

        let report = store.prune_checkpoints(7, &token()).await.expect("prune");
        assert_eq!(report.expired, 1, "only the aged Active transitions");
        assert_eq!(report.sidecar_rows_deleted, 2, "aged + terminal rows collected");

        // Fresh Active keeps restorable bytes.
        match load_bytes_via_trait(&store, &fresh_cid.to_string(), "fresh.txt").await {
            PreImageBytes::Stored { .. } => {}
            other => panic!("fresh Active bytes must survive, got {other:?}"),
        }
        // Aged Active lost bytes and now advertises Expired — never restorable.
        match load_bytes_via_trait(&store, &aged_cid.to_string(), "aged.txt").await {
            PreImageBytes::Absent => {}
            other => panic!("aged bytes must be collected, got {other:?}"),
        }
        let aged_back = store
            .load_checkpoint_record(&aged_cid.to_string(), &token())
            .await
            .expect("load aged")
            .expect("record envelope survives for audit");
        assert!(
            matches!(aged_back.status, CheckpointStatus::Expired { .. }),
            "pruned-bytes checkpoint must read Expired, got {:?}",
            aged_back.status
        );
        // Terminal checkpoint keeps its status (only its bytes were collected).
        let done_back = store
            .load_checkpoint_record(&done_cid.to_string(), &token())
            .await
            .expect("load done")
            .expect("record envelope survives");
        assert!(
            matches!(done_back.status, CheckpointStatus::Restored { .. }),
            "terminal status untouched, got {:?}",
            done_back.status
        );
        // Every record envelope survives: retention is delete-only on bytes.
        let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_checkpoints")
            .fetch_one(&store.pool)
            .await
            .expect("count records");
        assert_eq!(records, 3);
        // The never-pruned log is untouched by construction.
        let whiteboard_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM whiteboard_events")
            .fetch_one(&store.pool)
            .await
            .expect("count whiteboard");
        assert_eq!(whiteboard_before, whiteboard_after);
    }

    #[tokio::test]
    async fn retention_rejects_zero_days() {
        let (_dir, store) = test_store().await;
        match store.prune_checkpoints(0, &token()).await {
            Err(SessionError::Validation(_)) => {}
            other => panic!("zero retention must be rejected, got {other:?}"),
        }
    }

    /// Restore bookkeeping shares the byte rows' retention fate: an aged
    /// checkpoint's journal + attempt rows are collected by the SAME
    /// predicate in the same transaction, while a fresh Active checkpoint's
    /// progress and attempt survive intact (a later resume still finds them).
    /// The report counts byte rows only — bookkeeping deletes are traced, not
    /// a new field.
    #[tokio::test]
    async fn prune_collects_restore_journal_and_attempts_with_the_byte_predicate() {
        let (_dir, store) = test_store().await;
        let old_ms = now_millis() - 30 * 86_400 * 1000;

        // Aged Active: bytes + journal + attempt collected together.
        let aged_cid = Ulid::new();
        let mut aged = record_with(
            CheckpointStatus::Active,
            vec![banked("aged.txt", &aged_cid)],
            RestoreMethod::PreImageStore,
        );
        aged.checkpoint_id = aged_cid;
        aged.created_at_ms = old_ms;
        store
            .create_checkpoint(
                &aged,
                &BTreeMap::from([("aged.txt".to_owned(), b"a".to_vec())]),
                &token(),
            )
            .await
            .expect("aged create");
        store
            .begin_restore_attempt(&restore_attempt("att-aged", &aged_cid.to_string()), &token())
            .await
            .expect("begin aged");
        store
            .record_restore_progress(
                &aged_cid.to_string(),
                &progress_entry("aged.txt", RestoreResourceState::Done, Some("h"), "att-aged"),
                &token(),
            )
            .await
            .expect("journal aged");

        // Fresh Active: restorable bytes keep their progress and attempt.
        let fresh_cid = Ulid::new();
        let mut fresh = record_with(
            CheckpointStatus::Active,
            vec![banked("fresh.txt", &fresh_cid)],
            RestoreMethod::PreImageStore,
        );
        fresh.checkpoint_id = fresh_cid;
        fresh.created_at_ms = now_millis();
        store
            .create_checkpoint(
                &fresh,
                &BTreeMap::from([("fresh.txt".to_owned(), b"f".to_vec())]),
                &token(),
            )
            .await
            .expect("fresh create");
        store
            .begin_restore_attempt(&restore_attempt("att-fresh", &fresh_cid.to_string()), &token())
            .await
            .expect("begin fresh");
        store
            .record_restore_progress(
                &fresh_cid.to_string(),
                &progress_entry("fresh.txt", RestoreResourceState::Running, None, "att-fresh"),
                &token(),
            )
            .await
            .expect("journal fresh");

        let report = store.prune_checkpoints(7, &token()).await.expect("prune");
        assert_eq!(report.expired, 1, "only the aged Active transitions");
        assert_eq!(
            report.sidecar_rows_deleted, 1,
            "byte rows only; the bookkeeping deletes are traced, not counted"
        );

        assert!(
            store
                .load_restore_progress(&aged_cid.to_string(), &token())
                .await
                .expect("load")
                .is_empty(),
            "aged journal rows collected with their bytes"
        );
        assert!(
            store.load_restore_attempt("att-aged", &token()).await.expect("load").is_none(),
            "aged attempt envelope collected with its bytes"
        );
        assert!(
            store
                .load_restore_progress(&fresh_cid.to_string(), &token())
                .await
                .expect("load")
                .len()
                == 1,
            "fresh Active progress survives for a later resume"
        );
        assert!(
            store.load_restore_attempt("att-fresh", &token()).await.expect("load").is_some(),
            "fresh Active attempt envelope survives"
        );
    }

    #[tokio::test]
    async fn rebuild_from_log_leaves_the_sidecar_alone() {
        use crate::resource_facts::ResourceFacts;
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        let mut record = record_with(
            CheckpointStatus::Active,
            vec![banked("a.txt", &cid)],
            RestoreMethod::PreImageStore,
        );
        record.checkpoint_id = cid;
        store
            .create_checkpoint(
                &record,
                &BTreeMap::from([("a.txt".to_owned(), b"abc".to_vec())]),
                &token(),
            )
            .await
            .expect("create");

        ResourceFacts::new(store.pool.clone()).rebuild_from_log(&token()).await.expect("rebuild");

        // Both the envelope and the bytes survive a derived-view rebuild.
        let back = store
            .load_checkpoint_record(&cid.to_string(), &token())
            .await
            .expect("load")
            .expect("envelope survives rebuild");
        assert_eq!(back.checkpoint_id, cid);
        match load_bytes_via_trait(&store, &cid.to_string(), "a.txt").await {
            PreImageBytes::Stored { bytes, .. } => assert_eq!(bytes, b"abc"),
            other => panic!("bytes must survive rebuild, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn backfill_banks_first_mutation_bytes_for_covered_paths() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // Record banks "a.txt" (snapshot present) and "huge.bin"
        // (byte_ref'd but over the total budget — backfill must refuse it);
        // "b.txt" is scoped but hash-only (no byte_ref) — backfill must not
        // invent bytes for it.
        let record = CheckpointRecord::new(
            cid,
            Ulid::new(),
            CheckpointScope::FilesystemWrites {
                paths: vec!["a.txt".to_string(), "b.txt".to_string(), "huge.bin".to_string()],
            },
            vec![
                banked("a.txt", &cid),
                CheckpointResource {
                    kind: CheckpointResourceKind::FileDelete,
                    canonical_path: "b.txt".to_owned(),
                    pre_hash: None,
                    post_hash: None,
                    byte_ref: None,
                    size_bytes: None,
                },
                banked("huge.bin", &cid),
            ],
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            now_millis(),
            None,
        )
        .expect("test record builds");
        // No creation snapshot at all (every path simulates a missed
        // capture): insert the record row directly so the test controls
        // exactly which paths hold rows.
        let mut record_json = record;
        record_json.checkpoint_id = cid;
        sqlx::query(
            "INSERT INTO file_checkpoints \
             (checkpoint_id, session_id, status, record_json, created_at) \
             VALUES (?, ?, 'active', ?, ?)",
        )
        .bind(cid.to_string())
        .bind(Ulid::new().to_string())
        .bind(record_json.to_json().expect("record json"))
        .bind(now_millis())
        .execute(&store.pool)
        .await
        .expect("record row");

        let session = Ulid::new().to_string();
        let banked_count = store
            .backfill_pre_images(
                &[
                    ("a.txt".to_owned(), blake3::hash(b"v0").to_hex().to_string(), b"v0".to_vec()),
                    ("b.txt".to_owned(), blake3::hash(b"vb").to_hex().to_string(), b"vb".to_vec()),
                    (
                        "huge.bin".to_owned(),
                        "h".to_string(),
                        vec![0u8; CHECKPOINT_TOTAL_LIMIT_BYTES + 1],
                    ),
                ],
                Some(session.as_str()),
                &token(),
            )
            .await
            .expect("backfill");
        // "a.txt" is covered and byte-banked; "b.txt" stays hash-only (no
        // byte_ref); "huge.bin" is covered but over the total budget.
        assert_eq!(banked_count, 1);
        match load_bytes_via_trait(&store, &cid.to_string(), "a.txt").await {
            PreImageBytes::Stored { bytes, capture_kind, .. } => {
                assert_eq!(bytes, b"v0");
                assert_eq!(capture_kind, CaptureKind::PreImage);
            }
            other => panic!("expected backfilled bytes, got {other:?}"),
        }
        match load_bytes_via_trait(&store, &cid.to_string(), "b.txt").await {
            PreImageBytes::Absent => {}
            other => panic!("hash-only paths gain no rows, got {other:?}"),
        }
        match load_bytes_via_trait(&store, &cid.to_string(), "huge.bin").await {
            PreImageBytes::Absent => {}
            other => panic!("over-budget paths gain no rows, got {other:?}"),
        }
        // Second backfill is a no-op (first bytes win at path granularity).
        let again = store
            .backfill_pre_images(
                &[("a.txt".to_owned(), blake3::hash(b"v9").to_hex().to_string(), b"v9".to_vec())],
                Some(session.as_str()),
                &token(),
            )
            .await
            .expect("backfill again");
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn backfill_keeps_legacy_snapshot_rows_authoritative() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new();
        // A pre-039 checkpoint holds a legacy single-BLOB row: backfill
        // must not bank chunk rows beside it (first bytes win across both
        // tables, so the creation snapshot stays authoritative).
        sqlx::query(
            "INSERT INTO checkpoint_pre_images \
             (checkpoint_id, canonical_path, content_hash, content, size_bytes, capture_kind, captured_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(cid.to_string())
        .bind("old.txt")
        .bind(blake3::hash(b"snap").to_hex().to_string())
        .bind(b"snap".as_slice())
        .bind(4_i64)
        .bind(CaptureKind::Snapshot.as_str())
        .bind(now_millis())
        .execute(&store.pool)
        .await
        .expect("legacy row");
        let mut record = CheckpointRecord::new(
            cid,
            Ulid::new(),
            CheckpointScope::FilesystemWrites { paths: vec!["old.txt".to_string()] },
            vec![banked("old.txt", &cid)],
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            now_millis(),
            None,
        )
        .expect("test record builds");
        record.checkpoint_id = cid;
        sqlx::query(
            "INSERT INTO file_checkpoints \
             (checkpoint_id, session_id, status, record_json, created_at) \
             VALUES (?, ?, 'active', ?, ?)",
        )
        .bind(cid.to_string())
        .bind(Ulid::new().to_string())
        .bind(record.to_json().expect("record json"))
        .bind(now_millis())
        .execute(&store.pool)
        .await
        .expect("record row");

        let session = Ulid::new().to_string();
        let banked_count = store
            .backfill_pre_images(
                &[(
                    "old.txt".to_owned(),
                    blake3::hash(b"new").to_hex().to_string(),
                    b"new".to_vec(),
                )],
                Some(session.as_str()),
                &token(),
            )
            .await
            .expect("backfill");
        assert_eq!(banked_count, 0, "legacy snapshot wins");
        match load_bytes_via_trait(&store, &cid.to_string(), "old.txt").await {
            PreImageBytes::Stored { bytes, capture_kind, .. } => {
                assert_eq!(bytes, b"snap");
                assert_eq!(capture_kind, CaptureKind::Snapshot);
            }
            other => panic!("legacy bytes must survive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn over_total_plan_marks_manual_with_partial_scope_limitation() {
        // The creation-time rule behind over-total→Manual: no bankable bytes
        // means the Manual method plus the partial-scope limitation.
        let cid = Ulid::new();
        let big = vec![b'x'; CHECKPOINT_TOTAL_LIMIT_BYTES + 1];
        let planned = plan_capture(&cid, "big.bin", None, Some(&big));
        assert!(planned.included.expect("anchored").byte_ref.is_none());
        let excluded = planned.excluded.expect("honesty entry");
        assert_eq!(excluded.class, CheckpointExclusionClass::OversizedFile);
        assert_eq!(default_method_for_plan(&[]), RestoreMethod::Manual);
        assert!(PARTIAL_SCOPE_LIMITATION.contains("CHECKPOINT_TOTAL_LIMIT_BYTES"));
    }

    #[test]
    fn record_covers_path_matches_exactly() {
        let session = Ulid::new();
        let other = Ulid::new();
        let record = CheckpointRecord::new(
            Ulid::new(),
            session,
            CheckpointScope::FilesystemWrites { paths: vec!["notes.md".to_string()] },
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            0,
            None,
        )
        .expect("record builds");
        assert!(record_covers_path(&record, "notes.md", None));
        assert!(!record_covers_path(&record, "notes.md.bak", None), "no prefix guesses");
        assert!(!record_covers_path(&record, "other.md", None));

        let run_record = CheckpointRecord::new(
            Ulid::new(),
            session,
            CheckpointScope::Run,
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            0,
            None,
        )
        .expect("record builds");
        assert!(record_covers_path(
            &run_record,
            "anything.txt",
            Some(session.to_string().as_str())
        ));
        assert!(!record_covers_path(&run_record, "anything.txt", Some(other.to_string().as_str())));
        assert!(!record_covers_path(&run_record, "anything.txt", None));

        let task_record = CheckpointRecord::new(
            Ulid::new(),
            session,
            CheckpointScope::Task { task_id: concerto_core::types::TaskId::new() },
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            0,
            None,
        )
        .expect("record builds");
        assert!(!record_covers_path(
            &task_record,
            "anything.txt",
            Some(session.to_string().as_str())
        ));
    }

    // Test-only bridge to the trait method (the trait is the executor's seam;
    // tests exercise it without importing the executor crate).
    async fn load_bytes_via_trait(
        store: &FileCheckpointStore,
        checkpoint_id: &str,
        canonical_path: &str,
    ) -> PreImageBytes {
        use concerto_core::checkpoint::CheckpointStore as _;
        store.load_bytes(checkpoint_id, canonical_path, token()).await.expect("load bytes")
    }

    fn restore_attempt(attempt_id: &str, checkpoint_id: &str) -> RestoreAttemptInfo {
        RestoreAttemptInfo {
            attempt_id: attempt_id.to_owned(),
            checkpoint_id: checkpoint_id.to_owned(),
            by_agent: "agent-a".to_string(),
            started_at_ms: now_millis(),
        }
    }

    fn progress_entry(
        path: &str,
        state: RestoreResourceState,
        hash: Option<&str>,
        attempt_id: &str,
    ) -> RestoreProgressEntry {
        RestoreProgressEntry {
            canonical_path: path.to_owned(),
            state,
            content_hash: hash.map(str::to_owned),
            attempt_id: attempt_id.to_owned(),
            by_agent: "agent-a".to_string(),
            updated_at_ms: now_millis(),
            detail: String::new(),
        }
    }

    async fn reopen_store(dir: &tempfile::TempDir) -> FileCheckpointStore {
        // Crash-restart topology: a brand-new pool over the same database
        // file (no in-memory state survives — only what was committed).
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("ckpt_test.db"))
            .create_if_missing(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("reopen pool");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations re-apply cleanly");
        FileCheckpointStore::new(pool)
    }

    /// The journal upsert is monotonic (`running` never regresses `done`;
    /// `done` always wins) and the attempt envelope replays instead of
    /// forking on a duplicate begin.
    #[tokio::test]
    async fn restore_journal_upserts_are_monotonic_and_attempts_replay() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new().to_string();
        let hash = blake3::hash(b"v1").to_hex().to_string();

        store
            .begin_restore_attempt(&restore_attempt("att-1", &cid), &token())
            .await
            .expect("begin");
        // Re-submitting the same attempt id replays (single row, no fork).
        store
            .begin_restore_attempt(&restore_attempt("att-1", &cid), &token())
            .await
            .expect("re-begin replays");
        let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM checkpoint_restore_attempts WHERE attempt_id = 'att-1'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("count attempts");
        assert_eq!(rows, 1, "duplicate begin is a no-op replay");

        store
            .record_restore_progress(
                &cid,
                &progress_entry("a.txt", RestoreResourceState::Running, None, "att-1"),
                &token(),
            )
            .await
            .expect("running");
        store
            .record_restore_progress(
                &cid,
                &progress_entry("a.txt", RestoreResourceState::Done, Some(&hash), "att-1"),
                &token(),
            )
            .await
            .expect("done");
        // A concurrent duplicate's `running` must not regress the landed `done`.
        store
            .record_restore_progress(
                &cid,
                &progress_entry("a.txt", RestoreResourceState::Running, None, "att-2"),
                &token(),
            )
            .await
            .expect("stale running");
        let back = store.load_restore_progress(&cid, &token()).await.expect("load");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].state, RestoreResourceState::Done);
        assert_eq!(back[0].content_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(back[0].attempt_id, "att-1", "the stale running row never overwrote done");

        // A resume's fresh `done` refreshes the row (done always wins).
        store
            .record_restore_progress(
                &cid,
                &progress_entry("a.txt", RestoreResourceState::Done, Some(&hash), "att-3"),
                &token(),
            )
            .await
            .expect("resume done");
        let back = store.load_restore_progress(&cid, &token()).await.expect("load");
        assert_eq!(back[0].attempt_id, "att-3");

        store
            .finish_restore_attempt(
                "att-1",
                RestoreAttemptOutcome::Restored,
                now_millis(),
                &token(),
            )
            .await
            .expect("finish");
        let (info, outcome) = store
            .load_restore_attempt("att-1", &token())
            .await
            .expect("load")
            .expect("attempt row");
        assert_eq!(info.checkpoint_id, cid);
        assert_eq!(outcome.expect("finished").0, RestoreAttemptOutcome::Restored.as_str());
        // Finishing an unknown attempt is a harmless no-op, never an error.
        store
            .finish_restore_attempt(
                "att-ghost",
                RestoreAttemptOutcome::Failed,
                now_millis(),
                &token(),
            )
            .await
            .expect("ghost finish is silent");
        assert!(store.load_restore_attempt("att-ghost", &token()).await.expect("load").is_none());

        // Full state matrix: the SQL guard
        // `WHERE journal.state != 'done' OR excluded.state = 'done'` pinned
        // for EVERY prior×incoming pair — `done` sticky, every other prior
        // state replaceable by anything (including `done`). The core test
        // fake mirrors this same matrix, so the durable path and the in-
        // memory path can never drift apart silently.
        let states = [
            RestoreResourceState::Pending,
            RestoreResourceState::Running,
            RestoreResourceState::Done,
            RestoreResourceState::Failed,
            RestoreResourceState::Missing,
        ];
        for existing in states {
            for incoming in states {
                let path = format!("{}--{}.txt", existing.as_str(), incoming.as_str());
                store
                    .record_restore_progress(
                        &cid,
                        &progress_entry(&path, existing, Some(&hash), "att-m"),
                        &token(),
                    )
                    .await
                    .expect("seed prior state");
                store
                    .record_restore_progress(
                        &cid,
                        &progress_entry(&path, incoming, Some(&hash), "att-m"),
                        &token(),
                    )
                    .await
                    .expect("apply incoming state");
                let rows = store.load_restore_progress(&cid, &token()).await.expect("load");
                let got = rows.iter().find(|row| row.canonical_path == path).map(|row| row.state);
                let expected = if existing == RestoreResourceState::Done
                    && incoming != RestoreResourceState::Done
                {
                    existing
                } else {
                    incoming
                };
                assert_eq!(
                    got,
                    Some(expected),
                    "existing {existing:?} + incoming {incoming:?} must match the SQL guard"
                );
            }
        }
    }

    /// Crash durability: progress + attempts committed before the kill are
    /// all present after reopening the store from the same database file —
    /// the resume path then has precise progress, never a guess.
    #[tokio::test]
    async fn restore_journal_survives_crash_restart() {
        let (dir, store) = test_store().await;
        let cid = Ulid::new().to_string();
        let hash = blake3::hash(b"v1").to_hex().to_string();
        store
            .begin_restore_attempt(&restore_attempt("att-crash", &cid), &token())
            .await
            .expect("begin");
        store
            .record_restore_progress(
                &cid,
                &progress_entry("a.txt", RestoreResourceState::Done, Some(&hash), "att-crash"),
                &token(),
            )
            .await
            .expect("done");
        store
            .record_restore_progress(
                &cid,
                &progress_entry("b.txt", RestoreResourceState::Failed, None, "att-crash"),
                &token(),
            )
            .await
            .expect("failed");
        // The kill: drop every connection without finishing the attempt.
        drop(store);

        let reopened = reopen_store(&dir).await;
        let back = reopened.load_restore_progress(&cid, &token()).await.expect("load after reopen");
        assert_eq!(back.len(), 2, "every committed row survives the kill");
        assert_eq!(back[0].canonical_path, "a.txt");
        assert_eq!(back[0].state, RestoreResourceState::Done);
        assert_eq!(back[0].content_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(back[1].canonical_path, "b.txt");
        assert_eq!(back[1].state, RestoreResourceState::Failed);
        let (info, outcome) = reopened
            .load_restore_attempt("att-crash", &token())
            .await
            .expect("load")
            .expect("attempt envelope survives");
        assert_eq!(info.by_agent, "agent-a");
        assert_eq!(outcome.expect("unfinished").0, RestoreAttemptOutcome::Running.as_str());
        // Progress for an unknown checkpoint reads empty (unknown, never done).
        assert!(reopened
            .load_restore_progress("ckpt-ghost", &token())
            .await
            .expect("load")
            .is_empty());
    }

    /// A journal row carrying an unknown state word reads as `failed`
    /// (re-verify), never as `done` — forward-compat without trust.
    #[tokio::test]
    async fn unknown_journal_state_reads_failed_never_done() {
        let (_dir, store) = test_store().await;
        let cid = Ulid::new().to_string();
        sqlx::query(
            "INSERT INTO checkpoint_restore_journal \
             (checkpoint_id, canonical_path, state, attempt_id, by_agent, updated_at) \
             VALUES (?, ?, 'done-ish', 'att-x', 'agent-a', ?)",
        )
        .bind(&cid)
        .bind("weird.txt")
        .bind(now_millis())
        .execute(&store.pool)
        .await
        .expect("raw row");
        let back = store.load_restore_progress(&cid, &token()).await.expect("load");
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].state,
            RestoreResourceState::Failed,
            "unknown words degrade to re-verify, never assume done"
        );
    }
}
