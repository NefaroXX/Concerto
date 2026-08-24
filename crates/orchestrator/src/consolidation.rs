//! ADR-60 D6 memory-spine consolidation — minimal thin-slice projection.
//!
//! The whiteboard log (`whiteboard_events`) is the append-only source of
//! truth and is never summarized away (ADR-60 D3/D6). This module builds the
//! *projection*: an out-of-band supervisor task that folds foldable whiteboard
//! events ([`WhiteboardKind::Decision`], [`WhiteboardKind::PlanApproved`],
//! [`WhiteboardKind::ReviewState`]) into the existing hybrid memory store
//! ([`SqliteVectorStore`]) so agents can retrieve consolidated context
//! without scanning the raw log.
//!
//! ## Contract (ADR-60 D6 + Phase 4 oracle review `ses_fd445849cffeUMv2EsH8hmBpkZ`)
//!
//! - **Out-of-band** (oracle #1): the trigger counts appends observed by the
//!   supervisor's write-path handlers; every
//!   [`CONSOLIDATION_TRIGGER_APPENDS`] appends detaches ONE consolidation
//!   pass onto the tokio runtime via [`tokio::spawn`]. The gated-write /
//!   publish reply path never awaits indexing work.
//! - **Bi-temporal** metadata: every projection carries `world_time_*_ms`
//!   (the folded events' own timestamps — when the facts were true) and
//!   `ingestion_time_ms` (when this pass projected them).
//! - **Invalidate-not-delete with provenance** (oracle #2): a newer
//!   projection of the same group tombstones the previous chunk (the row is
//!   retained) and cites the superseded chunk ids AND their source
//!   `event_ids` in its own provenance, keeping the audit trail unbroken.
//! - **Idempotent**: chunk ids derive deterministically from the folded
//!   content identity (project + group + watermark `gate_seq`), so a crash
//!   between storing a chunk and recording its
//!   [`WhiteboardKind::Consolidation`] bookmark converges on re-run instead
//!   of duplicating. The log stays the source of truth: deleting a projection
//!   loses nothing.
//! - **One disclosure level / bounded shortlist** (D6): retrieval surfaces
//!   cap at [`DISCLOSURE_MAX_CHUNKS`] chunks (the supervisor's
//!   `retrieve-memory` handler clamps to this).
//!
//! ## Deliberately deferred (documented, not built)
//!
//! Real embedding-model vectors (the slice uses deterministic feature-hash
//! vectors so projections are retrievable without a downloaded model — swap
//! the embedder later without changing the contract, per the Phase 4 oracle
//! answer), bi-temporal *query* support, relevance/recency filtering, and
//! multi-level disclosure all remain behind ADR-60 Deferred item 4.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::ids::Ulid;
use concerto_core::memory::{ChunkType, EmbeddingRecord, ProjectId};
use concerto_core::CancellationToken;
use concerto_memory::vector_store::{SqliteVectorStore, VectorStore};
use concerto_sessions::whiteboard::{
    append_whiteboard_event, load_whiteboard_events, NewWhiteboardEvent, WhiteboardEvent,
    WhiteboardKind, WhiteboardLoadOpts,
};
use concerto_sessions::SessionError;
use sqlx::SqlitePool;
use thiserror::Error;
use time::OffsetDateTime;

/// Appends observed between consolidation passes (ADR-60 D6: "triggered by
/// event-count thresholds").
pub const CONSOLIDATION_TRIGGER_APPENDS: u64 = 16;

/// Upper bound of the retrieval shortlist (ADR-60 D6: "retrieval shortlists
/// 5–10 chunks"). One disclosure level: everything above this is dropped at
/// the surface, never silently accumulated.
pub const DISCLOSURE_MAX_CHUNKS: usize = 10;

/// Maximum whiteboard rows scanned per pass; anything beyond waits for the
/// next trigger (bounded work per detached task).
const MAX_EVENTS_PER_PASS: usize = 512;

/// Scope stamped on `consolidation` bookmark events so watermark recovery can
/// find them cheaply without a kind-filtered reader.
const SCOPE_CONSOLIDATION: &str = "consolidation";

/// Group key for foldable events that carry no `plan_id`.
const UNPLANNED_GROUP: &str = "run";

/// Per-summary-line char cap (truncated on a char boundary, deterministically)
/// so one huge decision cannot dominate a chunk.
const SUMMARY_LINE_MAX_CHARS: usize = 200;

/// Feature-hash embedding dimension. Deterministic placeholder until a real
/// embedder is wired into the supervised spine (see module docs).
const EMBEDDING_DIM: usize = 256;

/// Errors from a consolidation pass.
#[derive(Debug, Error)]
pub enum ConsolidationError {
    /// Vector-store failure.
    #[error(transparent)]
    Memory(#[from] MemoryError),
    /// Whiteboard log failure.
    #[error("whiteboard log error: {0}")]
    Log(#[from] SessionError),
    /// Payload serialization failure.
    #[error("consolidation serialization error: {0}")]
    Serialization(String),
}

/// The out-of-banded D6 projection task bound to one project's whiteboard log
/// and vector store.
///
/// Shared behind an [`Arc`]: all mutable state is atomic counters, so the
/// supervisor's detached handler tasks can call [`Consolidator::note_append`]
/// concurrently without locks on the write path.
pub struct Consolidator {
    pool: SqlitePool,
    store: Arc<SqliteVectorStore>,
    project_id: ProjectId,
    /// Appends observed since the last trigger evaluation. A failed pass does
    /// not reset progress accounting — nothing is lost because the watermark
    /// did not advance; the next trigger simply retries over the same span.
    pending_appends: AtomicU64,
    /// Single-flight guard: at most one detached pass at a time; triggers that
    /// arrive mid-pass coalesce into the next threshold.
    pass_in_flight: AtomicBool,
}

impl Consolidator {
    /// Bind a consolidator to the whiteboard log pool, the projection target,
    /// and the project namespace.
    pub fn new(pool: SqlitePool, store: Arc<SqliteVectorStore>, project_id: ProjectId) -> Self {
        Self {
            pool,
            store,
            project_id,
            pending_appends: AtomicU64::new(0),
            pass_in_flight: AtomicBool::new(false),
        }
    }

    /// Observe one appended whiteboard event (called by the supervisor's
    /// write-path handlers right where subscribers are woken).
    ///
    /// Every [`CONSOLIDATION_TRIGGER_APPENDS`] appends detaches ONE
    /// consolidation pass onto the runtime — fire-and-forget: the calling
    /// handler proceeds immediately, satisfying oracle comment #1 that
    /// consolidation never blocks the gate. At most one pass runs at a time;
    /// skipped triggers coalesce (the next threshold fires again).
    pub fn note_append(self: &Arc<Self>, cancel: CancellationToken) {
        let appends = self.pending_appends.fetch_add(1, Ordering::SeqCst) + 1;
        if !appends.is_multiple_of(CONSOLIDATION_TRIGGER_APPENDS) {
            return;
        }
        // Reset the accounting for the next window. A heuristic trigger: an
        // append racing this reset lands in the fresh window, which merely
        // delays the next pass — never a correctness boundary (the watermark,
        // not the counter, decides what a pass folds).
        self.pending_appends.store(0, Ordering::SeqCst);
        if self
            .pass_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("consolidation pass already in flight; trigger coalesced");
            return;
        }
        let consolidator = Arc::clone(self);
        tokio::spawn(async move {
            // The single-flight guard releases on every path: success, empty
            // pass, or failure (the next trigger retries a failed span).
            let result = consolidator.consolidate_once(cancel).await;
            consolidator.pass_in_flight.store(false, Ordering::SeqCst);
            match result {
                Ok(projected) if projected > 0 => tracing::info!(
                    projected,
                    "consolidation pass projected whiteboard groups (ADR-60 D6)"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    %error,
                    "consolidation pass failed; the whiteboard log is unaffected and \
                     the next trigger retries"
                ),
            }
        });
    }

    /// Appends currently counted toward the next trigger (test observability).
    #[cfg(test)]
    fn pending_appends(&self) -> u64 {
        self.pending_appends.load(Ordering::SeqCst)
    }

    /// Run one consolidation pass: fold every foldable event past the last
    /// projection watermark into at most one chunk per group, invalidate the
    /// superseded projections, and record ONE bookmark event covering the
    /// whole pass. Returns the number of groups projected (`0` = nothing
    /// new — a deliberate no-op).
    ///
    /// Ordering makes a crash anywhere convergent: chunks are stored BEFORE
    /// the bookmark, so a crash mid-pass leaves no bookmark and the next pass
    /// re-folds the same span. Chunk ids derive from the folded CONTENT, so
    /// the retry rebuilds identical rows for unchanged groups and tombstones
    /// any partially-written predecessor via the normal invalidate path.
    pub async fn consolidate_once(
        &self,
        cancel: CancellationToken,
    ) -> Result<usize, ConsolidationError> {
        let watermark = self.last_watermark().await?;
        let events = self.load_foldable(watermark).await?;
        if events.is_empty() {
            return Ok(0);
        }
        let max_gate_seq = events.iter().map(|event| event.gate_seq).max().unwrap_or(watermark);
        let ingestion_time_ms = unix_ms();

        // Group by plan identity; within a group, state-bearing kinds keep
        // only their newest snapshot and cite older ones as superseded.
        let mut groups: BTreeMap<String, Vec<WhiteboardEvent>> = BTreeMap::new();
        for event in events {
            let key = event.plan_id.clone().unwrap_or_else(|| UNPLANNED_GROUP.to_owned());
            groups.entry(key).or_default().push(event);
        }

        let mut source_event_ids: Vec<String> = Vec::new();
        let mut superseded_event_ids: Vec<String> = Vec::new();
        let mut superseded_chunk_ids: Vec<String> = Vec::new();
        let mut world_min = i64::MAX;
        let mut world_max = 0i64;
        let mut projected = 0usize;

        for (group_key, members) in &groups {
            if cancel.is_cancelled() {
                return Ok(projected);
            }
            let group_supersessions = within_fold_supersessions(members);

            // Summary projects CURRENT facts only: superseded snapshots stay
            // in the log and in the provenance lists, never in the digest.
            let summary = build_group_summary(group_key, members, &group_supersessions);
            // Content-derived identity: unchanged folds rebuild the same row
            // (idempotency); grown folds mint a fresh id so the predecessor
            // flows through the invalidate path below.
            let chunk_id = projection_chunk_id(&self.project_id.0, group_key, &summary);
            let file_path = camino::Utf8PathBuf::from(format!("whiteboard/{group_key}"));

            // Invalidate-not-delete: collect the group's live predecessor
            // chunks and inherit their provenance so the chain of custody
            // stays citable from the newest projection alone (oracle #2).
            let mut chunk_superseded = group_supersessions.clone();
            let mut superseded_chunk_ids_of_group: Vec<String> = Vec::new();
            for prior in self
                .store
                .projections_by_path(&self.project_id, file_path.as_str(), cancel.clone())
                .await?
            {
                // An identical retry of this very fold rebuilt the same id:
                // never cite or tombstone the row being (re)stored here.
                if prior.tombstoned || prior.chunk_id == chunk_id {
                    continue;
                }
                if let Some(metadata) = &prior.metadata {
                    if let Some(prior_sources) =
                        metadata.get("source_event_ids").and_then(|value| value.as_array())
                    {
                        for id in prior_sources.iter().filter_map(|value| value.as_str()) {
                            let known = chunk_superseded.iter().any(|candidate| candidate == id)
                                || superseded_event_ids.iter().any(|candidate| candidate == id)
                                || members.iter().any(|event| event.event_id == id);
                            if !known {
                                chunk_superseded.push(id.to_owned());
                            }
                        }
                    }
                }
                superseded_chunk_ids_of_group.push(prior.chunk_id.clone());
            }

            world_min = world_min.min(members.iter().map(|e| e.created_at).min().unwrap_or(0));
            world_max = world_max.max(members.iter().map(|e| e.created_at).max().unwrap_or(0));
            source_event_ids.extend(members.iter().map(|event| event.event_id.clone()));
            superseded_event_ids.extend(chunk_superseded.iter().cloned());

            let metadata = serde_json::json!({
                "kind": "adr60-d6-consolidation",
                "group_key": group_key,
                "chunk_id": chunk_id,
                "source_event_ids": members.iter().map(|e| e.event_id.clone())
                    .collect::<Vec<_>>(),
                "superseded_event_ids": chunk_superseded,
                "world_time_min_ms": members.iter().map(|e| e.created_at).min().unwrap_or(0),
                "world_time_max_ms": members.iter().map(|e| e.created_at).max().unwrap_or(0),
                "ingestion_time_ms": ingestion_time_ms,
            });
            let vector = feature_hash_embedding(&summary);
            self.store
                .store_projection(
                    &EmbeddingRecord {
                        id: chunk_id,
                        project_id: self.project_id.clone(),
                        chunk_hash: blake3::hash(summary.as_bytes()).to_hex().to_string(),
                        content: summary,
                        file_path,
                        start_line: None,
                        end_line: None,
                        chunk_type: ChunkType::Fact,
                        vector,
                        model_id: "feature-hash".to_owned(),
                        model_version: "1".to_owned(),
                        stale: false,
                        created_at: OffsetDateTime::now_utc(),
                    },
                    &metadata,
                    cancel.clone(),
                )
                .await?;
            superseded_chunk_ids.extend(superseded_chunk_ids_of_group);
            projected += 1;
        }

        for stale_id in &superseded_chunk_ids {
            self.store.tombstone(stale_id, &self.project_id, cancel.clone()).await?;
        }

        // One bookmark per PASS: durable provenance + the next pass's
        // watermark (its `max_gate_seq`). The projection is a derived view;
        // the event is its record.
        superseded_event_ids.sort();
        superseded_event_ids.dedup();
        append_whiteboard_event(
            &self.pool,
            &NewWhiteboardEvent {
                event_id: Ulid::new().to_string(),
                agent_id: "supervisor".to_owned(),
                kind: WhiteboardKind::Consolidation,
                scope: SCOPE_CONSOLIDATION.to_owned(),
                session_id: None,
                plan_id: None,
                causation: None,
                payload: serde_json::json!({
                    "kind": "adr60-d6-consolidation",
                    "groups": groups.keys().cloned().collect::<Vec<_>>(),
                    "source_event_ids": source_event_ids,
                    "superseded_event_ids": superseded_event_ids,
                    "superseded_chunk_ids": superseded_chunk_ids,
                    "world_time_min_ms": if world_min == i64::MAX { 0 } else { world_min },
                    "world_time_max_ms": world_max,
                    "ingestion_time_ms": ingestion_time_ms,
                    "max_gate_seq": max_gate_seq,
                }),
                pre_image_hash: None,
                created_at: ingestion_time_ms,
            },
        )
        .await?;
        Ok(projected)
    }

    /// The highest `gate_seq` already covered by a recorded projection.
    ///
    /// Recovery reads the bookmark events themselves (scope-filtered), so a
    /// restart resumes exactly where the last committed pass stopped — state
    /// lives in the log, not the process.
    async fn last_watermark(&self) -> Result<u64, ConsolidationError> {
        let opts = WhiteboardLoadOpts {
            after_gate_seq: 0,
            session_id: None,
            scope: Some(SCOPE_CONSOLIDATION.to_owned()),
            limit: MAX_EVENTS_PER_PASS,
        };
        let bookmarks = load_whiteboard_events(&self.pool, &opts).await?;
        Ok(bookmarks
            .iter()
            .filter_map(|event| event.payload.get("max_gate_seq").and_then(|value| value.as_u64()))
            .max()
            .unwrap_or(0))
    }

    /// Foldable events strictly past `watermark`, oldest first.
    async fn load_foldable(
        &self,
        watermark: u64,
    ) -> Result<Vec<WhiteboardEvent>, ConsolidationError> {
        let opts = WhiteboardLoadOpts {
            after_gate_seq: watermark,
            session_id: None,
            scope: None,
            limit: MAX_EVENTS_PER_PASS,
        };
        Ok(load_whiteboard_events(&self.pool, &opts)
            .await?
            .into_iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    WhiteboardKind::Decision
                        | WhiteboardKind::PlanApproved
                        | WhiteboardKind::ReviewState
                )
            })
            .collect())
    }
}

/// State-bearing events superseded WITHIN one fold: every `plan-approved`
/// except the newest under the same plan id, and every `review-state` except
/// the newest snapshot per review-target group. Their ids stay cited in the
/// projection provenance; the log rows themselves are never touched.
fn within_fold_supersessions(events: &[WhiteboardEvent]) -> Vec<String> {
    let mut superseded: Vec<(u64, String)> = Vec::new();

    if let Some(newest) = events
        .iter()
        .filter(|event| event.kind == WhiteboardKind::PlanApproved)
        .map(|event| event.gate_seq)
        .max()
    {
        for event in events
            .iter()
            .filter(|event| event.kind == WhiteboardKind::PlanApproved && event.gate_seq != newest)
        {
            superseded.push((event.gate_seq, event.event_id.clone()));
        }
    }

    let mut newest_per_target: HashMap<String, u64> = HashMap::new();
    for event in events.iter().filter(|event| event.kind == WhiteboardKind::ReviewState) {
        let entry = newest_per_target.entry(review_state_target(event)).or_insert(event.gate_seq);
        if event.gate_seq > *entry {
            *entry = event.gate_seq;
        }
    }
    for event in events.iter().filter(|event| event.kind == WhiteboardKind::ReviewState) {
        if let Some(&newest) = newest_per_target.get(&review_state_target(event)) {
            if event.gate_seq != newest {
                superseded.push((event.gate_seq, event.event_id.clone()));
            }
        }
    }

    superseded.sort_unstable();
    superseded.into_iter().map(|(_, id)| id).collect()
}

/// The stable review-target group key of a `review-state` event: the payload's
/// `review_target_hash` when present, else its raw target text, else the row's
/// own event id (an unidentifiable snapshot supersedes nothing but itself).
fn review_state_target(event: &WhiteboardEvent) -> String {
    event
        .payload
        .get("review_target_hash")
        .and_then(|value| value.as_str())
        .or_else(|| event.payload.get("review_target").and_then(|value| value.as_str()))
        .map_or_else(|| event.event_id.clone(), ToOwned::to_owned)
}

/// Deterministic one-level summary of a group's foldable events (oldest
/// first), skipping `skip` event ids (the within-fold supersessions: the
/// digest projects current facts; history stays in the log + provenance). No
/// wall-clock content: two passes over the same event window must produce
/// byte-identical chunk text, which is what makes the projection idempotent.
fn build_group_summary(group_key: &str, events: &[WhiteboardEvent], skip: &[String]) -> String {
    let mut lines = Vec::with_capacity(events.len() + 1);
    lines.push(format!(
        "Consolidated whiteboard facts for {} (through gate_seq {}):",
        group_key,
        events.iter().map(|event| event.gate_seq).max().unwrap_or(0)
    ));
    for event in events {
        if skip.iter().any(|id| id == &event.event_id) {
            continue;
        }
        let line = match event.kind {
            WhiteboardKind::Decision => format!(
                "- decision [{} @seq {}]: {}",
                event.agent_id,
                event.gate_seq,
                truncate_line(&first_string(
                    &event.payload,
                    &["decision", "what", "summary", "note", "description", "reason"],
                ))
            ),
            WhiteboardKind::PlanApproved => format!(
                "- plan approved {}: artifact {}",
                event.payload.get("plan_id").and_then(|value| value.as_str()).unwrap_or("?"),
                event.payload.get("artifact_hash").and_then(|value| value.as_str()).unwrap_or("?"),
            ),
            WhiteboardKind::ReviewState => format!(
                "- review [{}]: target {} status {} revisions {}",
                event.payload.get("implement_role").and_then(|value| value.as_str()).unwrap_or("?"),
                event.payload.get("review_target").and_then(|value| value.as_str()).unwrap_or("?"),
                event.payload.get("status").and_then(|value| value.as_str()).unwrap_or("?"),
                event.payload.get("retry_count").and_then(|value| value.as_u64()).unwrap_or(0),
            ),
            _ => continue,
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// Probe `payload` for the first present string field among `keys`, falling
/// back to the compact JSON itself so a foreign payload shape still lands in
/// the summary (defensive extraction, mirroring `fold_ledger`'s posture).
fn first_string(payload: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(|value| value.as_str()) {
            return value.to_owned();
        }
    }
    serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned())
}

/// Truncate one summary line to [`SUMMARY_LINE_MAX_CHARS`] on a char boundary.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= SUMMARY_LINE_MAX_CHARS {
        return line.to_owned();
    }
    let truncated: String = line.chars().take(SUMMARY_LINE_MAX_CHARS).collect();
    format!("{truncated}…")
}

/// Deterministic projection chunk id: identical inputs (project, group, and
/// the folded summary) rebuild the identical id, so a crash-and-retry upserts
/// the same row instead of duplicating it (idempotency contract).
fn projection_chunk_id(project_id: &str, group_key: &str, summary: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"adr60-d6\0");
    hasher.update(project_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(group_key.as_bytes());
    hasher.update(&[0]);
    hasher.update(summary.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Deterministic feature-hash ("hashing trick") bag-of-tokens vector,
/// L2-normalized. Not semantically equivalent to a trained embedder — it makes
/// lexical overlap retrievable so the projection is usable before the real
/// embedder lands in the supervised spine (module docs: upgrade path).
fn feature_hash_embedding(text: &str) -> Vec<f32> {
    let mut buckets = vec![0.0f32; EMBEDDING_DIM];
    for token in text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
        let digest = blake3::hash(token.to_lowercase().as_bytes());
        for word in digest.as_bytes().chunks_exact(8) {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(word);
            let index = (u64::from_le_bytes(bytes) % EMBEDDING_DIM as u64) as usize;
            buckets[index] += 1.0;
        }
    }
    let norm = buckets.iter().map(|b| (*b as f64).powi(2)).sum::<f64>().sqrt();
    if norm == 0.0 {
        return buckets;
    }
    buckets.iter().map(|b| *b / norm as f32).collect()
}

/// Unix epoch milliseconds (UTC).
fn unix_ms() -> i64 {
    let now = OffsetDateTime::now_utc();
    now.unix_timestamp() * 1000 + i64::from(now.millisecond())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use std::time::Duration;
    use tempfile::TempDir;

    /// File-backed pool over the sessions schema (whiteboard migrations) with
    /// the production PRAGMAs; shared by the log and the projection store.
    async fn test_pool() -> (TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("consolidation_test.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    /// A consolidator plus its projection store, both over `pool` (tests share
    /// one DB; production opens the memory DB for the store instead).
    async fn consolidator(pool: SqlitePool) -> (Arc<SqliteVectorStore>, Consolidator) {
        let project_id = ProjectId("proj-d6".to_owned());
        let store =
            Arc::new(SqliteVectorStore::new(pool.clone()).await.expect("vector store opens"));
        (store.clone(), Consolidator::new(pool, store, project_id))
    }

    fn event(
        plan: Option<&str>,
        kind: WhiteboardKind,
        payload: serde_json::Value,
    ) -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "agent-a".to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: plan.map(ToOwned::to_owned),
            causation: None,
            payload,
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    async fn append(pool: &SqlitePool, event: NewWhiteboardEvent) -> WhiteboardEvent {
        append_whiteboard_event(pool, &event).await.expect("append")
    }

    fn projection_metadata<'a>(
        rows: &'a [concerto_memory::vector_store::ProjectionRow],
        chunk_id: &str,
    ) -> &'a serde_json::Value {
        rows.iter()
            .find(|row| row.chunk_id == chunk_id)
            .and_then(|row| row.metadata.as_ref())
            .expect("projection metadata present")
    }

    async fn bookmarks(pool: &SqlitePool) -> Vec<WhiteboardEvent> {
        load_whiteboard_events(
            pool,
            &WhiteboardLoadOpts {
                scope: Some(SCOPE_CONSOLIDATION.to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("bookmark load")
    }

    #[tokio::test]
    async fn pass_projects_groups_with_provenance_and_supersession() {
        let (_dir, pool) = test_pool().await;
        let (store, consolidator) = consolidator(pool.clone()).await;

        // Two decisions; TWO plan approvals for the same plan (the older one
        // must be cited as superseded); TWO review snapshots for the same
        // target group (older superseded); an unrelated plan (its own group);
        // and an unplanned decision (the "run" group).
        let d1 = append(
            &pool,
            event(Some("plan-1"), WhiteboardKind::Decision, json!({"decision": "use sqlite wal"})),
        )
        .await;
        let d2 = append(
            &pool,
            event(
                Some("plan-1"),
                WhiteboardKind::Decision,
                json!({"decision": "gate every write"}),
            ),
        )
        .await;
        let p_old = append(
            &pool,
            event(
                Some("plan-1"),
                WhiteboardKind::PlanApproved,
                json!({"plan_id": "plan-1", "artifact_hash": "aaa"}),
            ),
        )
        .await;
        let r_old = append(
            &pool,
            event(
                Some("plan-1"),
                WhiteboardKind::ReviewState,
                json!({
                    "plan_id": "plan-1", "implement_role": "Coder",
                    "review_target": "module x", "review_target_hash": "target-x",
                    "status": "revision-queued", "retry_count": 1
                }),
            ),
        )
        .await;
        let p_new = append(
            &pool,
            event(
                Some("plan-1"),
                WhiteboardKind::PlanApproved,
                json!({"plan_id": "plan-1", "artifact_hash": "bbb"}),
            ),
        )
        .await;
        let r_new = append(
            &pool,
            event(
                Some("plan-1"),
                WhiteboardKind::ReviewState,
                json!({
                    "plan_id": "plan-1", "implement_role": "Coder",
                    "review_target": "module x", "review_target_hash": "target-x",
                    "status": "completed", "retry_count": 1
                }),
            ),
        )
        .await;
        append(
            &pool,
            event(
                Some("plan-2"),
                WhiteboardKind::PlanApproved,
                json!({"plan_id": "plan-2", "artifact_hash": "ccc"}),
            ),
        )
        .await;
        let last = append(
            &pool,
            event(None, WhiteboardKind::Decision, json!({"decision": "rename crate"})),
        )
        .await;

        let projected =
            consolidator.consolidate_once(CancellationToken::new()).await.expect("pass");
        assert_eq!(projected, 3, "one chunk per group: plan-1, plan-2, run");

        let plan_rows = store
            .projections_by_path(
                &consolidator.project_id,
                "whiteboard/plan-1",
                CancellationToken::new(),
            )
            .await
            .expect("read plan projection");
        assert_eq!(plan_rows.len(), 1, "exactly one live projection for the group");
        let metadata = projection_metadata(&plan_rows, &plan_rows[0].chunk_id);

        assert_eq!(
            metadata["source_event_ids"],
            json!([
                d1.event_id,
                d2.event_id,
                p_old.event_id,
                r_old.event_id,
                p_new.event_id,
                r_new.event_id
            ]),
            "provenance cites every folded event id in gate_seq order"
        );
        assert_eq!(
            metadata["superseded_event_ids"],
            json!([p_old.event_id, r_old.event_id]),
            "older approval + review snapshot cited as superseded (invalidate-not-delete)"
        );
        assert_eq!(metadata["group_key"], json!("plan-1"));
        assert_eq!(
            metadata["world_time_min_ms"],
            json!(1_700_000_000_000_i64),
            "world time comes from the folded events, not the projection clock"
        );
        assert!(
            metadata["ingestion_time_ms"].as_i64().unwrap_or(0) > 0,
            "ingestion time is recorded"
        );

        let content = &plan_rows[0].content;
        assert!(content.contains("use sqlite wal"), "decisions summarized: {content}");
        assert!(content.contains("plan approved plan-1"), "approval fact present");
        assert!(content.contains("status completed"), "newest review status present");
        assert!(!content.contains("revision-queued"), "superseded snapshot not projected");

        // The unplanned group got its own chunk.
        let run_rows = store
            .projections_by_path(
                &consolidator.project_id,
                "whiteboard/run",
                CancellationToken::new(),
            )
            .await
            .expect("read run projection");
        assert_eq!(run_rows.len(), 1);

        // ONE bookmark per pass, carrying the full provenance + watermark.
        let marks = bookmarks(&pool).await;
        assert_eq!(marks.len(), 1, "one bookmark per pass");
        assert_eq!(marks[0].kind, WhiteboardKind::Consolidation);
        assert_eq!(
            marks[0].payload["source_event_ids"].as_array().map(Vec::len),
            Some(8),
            "every folded event across all groups is cited"
        );
        assert_eq!(marks[0].payload["max_gate_seq"], json!(last.gate_seq), "watermark");
    }

    #[tokio::test]
    async fn rerun_without_new_events_is_a_no_op() {
        let (_dir, pool) = test_pool().await;
        let (store, consolidator) = consolidator(pool.clone()).await;
        append(&pool, event(None, WhiteboardKind::Decision, json!({"decision": "first"}))).await;
        assert_eq!(
            consolidator.consolidate_once(CancellationToken::new()).await.expect("first"),
            1
        );
        assert_eq!(
            consolidator.consolidate_once(CancellationToken::new()).await.expect("second"),
            0,
            "nothing past the watermark — deliberate no-op"
        );

        let rows = store
            .projections_by_path(
                &consolidator.project_id,
                "whiteboard/run",
                CancellationToken::new(),
            )
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1, "no duplicate chunk");
        assert_eq!(bookmarks(&pool).await.len(), 1, "no duplicate bookmark");
    }

    #[tokio::test]
    async fn later_pass_tombstones_prior_projection_and_cites_its_provenance() {
        let (_dir, pool) = test_pool().await;
        let (store, consolidator) = consolidator(pool.clone()).await;
        let d1 =
            append(&pool, event(None, WhiteboardKind::Decision, json!({"decision": "v1"}))).await;
        consolidator.consolidate_once(CancellationToken::new()).await.expect("pass 1");

        let d2 =
            append(&pool, event(None, WhiteboardKind::Decision, json!({"decision": "v2"}))).await;
        assert_eq!(
            consolidator.consolidate_once(CancellationToken::new()).await.expect("pass 2"),
            1
        );

        let rows = store
            .projections_by_path(
                &consolidator.project_id,
                "whiteboard/run",
                CancellationToken::new(),
            )
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2, "old chunk retained (invalidate-not-delete)");
        assert!(
            rows.iter().any(|row| row.tombstoned),
            "prior projection invalidated, never deleted"
        );
        let live = rows.iter().find(|row| !row.tombstoned).expect("live projection");
        assert!(live.content.contains("v2"));
        let metadata = projection_metadata(&rows, &live.chunk_id);
        assert_eq!(metadata["source_event_ids"], json!([d2.event_id]));
        assert_eq!(
            metadata["superseded_event_ids"],
            json!([d1.event_id]),
            "the new projection inherits the superseded chunk's source events"
        );

        // The pass bookmark cites the tombstoned predecessor by id.
        let marks = bookmarks(&pool).await;
        assert_eq!(marks.len(), 2, "one bookmark per pass");
        let latest = &marks[marks.len() - 1];
        assert_eq!(
            latest.payload["superseded_chunk_ids"].as_array().map(Vec::len),
            Some(1),
            "the tombstoned predecessor chunk is cited by id"
        );
    }

    #[tokio::test]
    async fn trigger_counts_appends_and_detaches_a_pass_on_the_threshold() {
        let (_dir, pool) = test_pool().await;
        let (_store, consolidator) = consolidator(pool.clone()).await;
        let consolidator = Arc::new(consolidator);

        for _ in 0..(CONSOLIDATION_TRIGGER_APPENDS - 1) {
            consolidator.note_append(CancellationToken::new());
        }
        assert_eq!(consolidator.pending_appends(), CONSOLIDATION_TRIGGER_APPENDS - 1);

        // The threshold append detaches a pass and RESETS the counter (the
        // spawned pass runs against the real log — empty here, so it no-ops).
        consolidator.note_append(CancellationToken::new());
        assert_eq!(consolidator.pending_appends(), 0);

        // Let the detached pass finish and release the single-flight guard so
        // the next trigger can fire again.
        tokio::time::sleep(Duration::from_millis(50)).await;
        consolidator.note_append(CancellationToken::new());
        assert_eq!(consolidator.pending_appends(), 1, "counting resumes after a fired trigger");
    }
}
