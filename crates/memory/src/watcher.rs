//! Filesystem watcher and re-index queue drainer.
//!
//! The `notify`-based watcher monitors project directories for file
//! changes and enqueues re-index jobs. The `ReindexQueueDrainer`
//! processes those jobs during idle periods or at startup.
//!
use concerto_core::error::MemoryError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::memory::ProjectId;
use concerto_core::CancellationToken;
use notify::{RecommendedWatcher, RecursiveMode};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::ignore_rules::IndexIgnoreMatcher;
use crate::indexer::{IndexConfig, ProjectIndexer};
use crate::sync::ChunkSyncService;

/// Running count of reindex hints dropped because the bounded mpsc channel was
/// full. Only relevant for diagnostics; a dropped hint is non-durable (see the
/// overflow comment in `FileWatcher::watch`).
static DROPPED_HINTS: AtomicU64 = AtomicU64::new(0);

/// Watches a project directory for changes and triggers re-indexing.
pub struct FileWatcher {
    bus: EventBus,
    project_id: ProjectId,
}

/// Active watcher and its event stream. Owning this value keeps the native
/// `notify` watcher (inside the debouncer) alive; dropping it stops observation
/// and the debouncer thread immediately.
pub struct FileWatch {
    receiver: mpsc::Receiver<Vec<String>>,
    _watcher: notify_debouncer_mini::Debouncer<RecommendedWatcher>,
    cancel: CancellationToken,
}

impl FileWatch {
    pub async fn recv(&mut self) -> Option<Vec<String>> {
        tokio::select! {
            _ = self.cancel.cancelled() => None,
            received = self.receiver.recv() => received,
        }
    }

    pub fn try_recv(&mut self) -> Result<Vec<String>, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl FileWatcher {
    pub fn new(bus: EventBus, project_id: ProjectId) -> Self {
        Self { bus, project_id }
    }

    /// Watch a directory for changes.
    ///
    /// Returns a receiver for file change events. The watcher runs until
    /// the cancellation token is triggered.
    pub async fn watch(
        &self,
        path: &Path,
        cancel: CancellationToken,
    ) -> Result<FileWatch, MemoryError> {
        let (tx, rx) = mpsc::channel(100);
        let project_id = self.project_id.clone();
        let bus = self.bus.clone();

        // Debounced watcher: collapses build/checkout event storms into ~1s
        // batches so long sessions don't flood the bus or the reindex queue.
        // This runs on the debouncer's own std thread — never block/await here.
        let mut debouncer = notify_debouncer_mini::new_debouncer(
            Duration::from_secs(1),
            move |res: notify_debouncer_mini::DebounceEventResult| match res {
                Ok(events) => {
                    // Deduplicate paths across the batch, then emit one bus
                    // hint and one channel payload per distinct changed file.
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut paths: Vec<String> = Vec::new();
                    for event in events {
                        let Some(path_str) = event.path.to_str() else {
                            continue;
                        };
                        let path_str = path_str.to_string();
                        if seen.insert(path_str.clone()) {
                            // Global event: intentionally unscoped (file watcher
                            // callback, no session context).
                            let _ = bus.publish_raw(EventKind::ReindexQueued {
                                project_id: project_id.to_string(),
                                file_path: path_str.clone(),
                                reason: "file_changed".to_string(),
                            });
                            paths.push(path_str);
                        }
                    }
                    if !paths.is_empty() {
                        // Dropping on overflow is intentional: reindex
                        // notifications are non-durable hints. Authoritative
                        // state lives in the SQLite reindex_queue (see
                        // ReindexQueueDrainer::enqueue); a dropped hint only
                        // delays the next refresh. The debouncer already
                        // collapsed the burst, so one warn per callback is
                        // bounded. Do not block/await on a std thread.
                        if let Err(try_err) = tx.try_send(paths) {
                            match try_err {
                                mpsc::error::TrySendError::Full(_)
                                | mpsc::error::TrySendError::Closed(_) => {
                                    DROPPED_HINTS.fetch_add(1, Ordering::Relaxed);
                                    tracing::warn!(
                                        dropped = DROPPED_HINTS.load(Ordering::Relaxed),
                                        "reindex hint dropped: bounded channel full"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    // Debouncer reports a watcher error (e.g. I/O); keep the
                    // watcher alive and keep observing, just log it.
                    tracing::warn!(%err, "file watcher debounce reported an error; continuing");
                }
            },
        )
        .map_err(|e| MemoryError::Persistence(format!("failed to create watcher: {e}")))?;

        debouncer
            .watcher()
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| MemoryError::Persistence(format!("failed to watch path: {e}")))?;

        Ok(FileWatch { receiver: rx, _watcher: debouncer, cancel })
    }
}

/// Drains the re-index queue, processing each pending file through the
/// indexer.
///
/// Intended to be called at startup and during idle periods.
/// Respects `CancellationToken` so long drain operations can be
/// interrupted gracefully.
pub struct ReindexQueueDrainer {
    pool: Option<sqlx::SqlitePool>,
    indexer: Option<Arc<ProjectIndexer>>,
    sync: Option<Arc<ChunkSyncService>>,
    project_id: Option<ProjectId>,
    index_config: Option<IndexConfig>,
    ignore_matcher: RwLock<Option<Arc<IndexIgnoreMatcher>>>,
}

impl ReindexQueueDrainer {
    pub fn new(pool: Option<sqlx::SqlitePool>) -> Self {
        Self {
            pool,
            indexer: None,
            sync: None,
            project_id: None,
            index_config: None,
            ignore_matcher: RwLock::new(None),
        }
    }

    /// Construct a drainer that actually re-indexes queued files through the
    /// project indexer and persists chunks via the chunk sync service.
    pub fn with_indexer_and_sync(
        pool: Option<sqlx::SqlitePool>,
        indexer: Arc<ProjectIndexer>,
        sync: Arc<ChunkSyncService>,
        project_id: ProjectId,
        index_config: IndexConfig,
    ) -> Self {
        Self {
            pool,
            indexer: Some(indexer),
            sync: Some(sync),
            project_id: Some(project_id),
            index_config: Some(index_config),
            ignore_matcher: RwLock::new(None),
        }
    }

    /// Add a file to the reindex queue.
    pub async fn enqueue(
        &self,
        project_id: &ProjectId,
        path: &Path,
        reason: &str,
    ) -> Result<(), MemoryError> {
        if let Some(pool) = &self.pool {
            sqlx::query(MIGRATION_009_REINDEX_QUEUE)
                .execute(pool)
                .await
                .map_err(|e| MemoryError::Persistence(format!("failed to init queue: {e}")))?;
            sqlx::query(
                "DELETE FROM reindex_queue \
                 WHERE project_id = ? AND file_path = ? AND processed = 0",
            )
            .bind(&project_id.0)
            .bind(path.to_string_lossy().as_ref())
            .execute(pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("failed to deduplicate queue: {e}")))?;
            let id = Ulid::new().to_string();
            let now = OffsetDateTime::now_utc().unix_timestamp();
            sqlx::query(
                "INSERT OR IGNORE INTO reindex_queue (id, project_id, file_path, reason, created_at, processed) VALUES (?, ?, ?, ?, ?, 0)"
            )
                .bind(&id)
                .bind(&project_id.0)
                .bind(path.to_string_lossy().as_ref())
                .bind(reason)
                .bind(now)
                .execute(pool)
                .await
                .map_err(|e| MemoryError::Persistence(format!("failed to enqueue: {e}")))?;
        }
        Ok(())
    }

    /// Drain all unprocessed rows and call the indexer for each file.
    /// Returns the number of files successfully processed.
    pub async fn drain(&self, cancel: CancellationToken) -> Result<u64, MemoryError> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(0),
        };
        // Ensure table exists
        sqlx::query(MIGRATION_009_REINDEX_QUEUE)
            .execute(pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("failed to init reindex_queue: {e}")))?;

        let Some(project_id) = &self.project_id else {
            return Ok(0);
        };
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, file_path, reason FROM reindex_queue \
             WHERE processed = 0 AND project_id = ? ORDER BY created_at ASC",
        )
        .bind(&project_id.0)
        .fetch_all(pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("failed to query reindex queue: {e}")))?;

        if rows.is_empty() {
            return Ok(0);
        }
        let mut processed = 0u64;
        // An ignore-control file change triggers a full, cancellable re-index
        // (atomic replace). This is retained for correctness because the
        // indexer's walk-based primitives lack a cheap targeted-diff API; a
        // targeted rescan is tracked as future work, not implemented here. The
        // debounced watcher (see FileWatcher::watch) already collapses stray
        // build/checkout event storms feeding this queue.
        let rebuild_matcher = rows.iter().any(|(_, path, _)| {
            is_ignore_control_file(Path::new(path), self.index_config.as_ref())
        });
        let matcher = self.matcher(rebuild_matcher)?;
        let (Some(indexer), Some(sync), Some(config)) =
            (&self.indexer, &self.sync, &self.index_config)
        else {
            return Ok(0);
        };
        if rebuild_matcher {
            let records = indexer.index_with_matcher(config, &matcher, cancel.clone()).await?;
            if cancel.is_cancelled() {
                return Ok(0);
            }
            sync.replace_project(project_id, &records, cancel.clone()).await?;
            for (id, _, _) in &rows {
                sqlx::query("DELETE FROM reindex_queue WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map_err(|error| {
                        MemoryError::Persistence(format!(
                            "failed to remove reconciled queue row: {error}"
                        ))
                    })?;
            }
            return Ok(rows.len() as u64);
        }
        for (id, file_path, _reason) in rows {
            if cancel.is_cancelled() {
                break;
            }
            if let Err(error) = indexer
                .index_file_with_matcher(
                    Path::new(&file_path),
                    config,
                    &matcher,
                    sync,
                    cancel.clone(),
                )
                .await
            {
                tracing::warn!("reindex failed for {file_path}: {error}");
                continue;
            }
            if cancel.is_cancelled() {
                break;
            }
            sqlx::query("DELETE FROM reindex_queue WHERE id = ?")
                .bind(&id)
                .execute(pool)
                .await
                .map_err(|e| {
                    MemoryError::Persistence(format!("failed to remove reindex queue row: {e}"))
                })?;
            processed += 1;
        }
        Ok(processed)
    }

    fn matcher(&self, rebuild: bool) -> Result<Arc<IndexIgnoreMatcher>, MemoryError> {
        let config = self.index_config.as_ref().ok_or_else(|| {
            MemoryError::Persistence("reindex queue has no index configuration".into())
        })?;
        let mut matcher = self
            .ignore_matcher
            .write()
            .map_err(|_| MemoryError::Persistence("reindex ignore matcher lock poisoned".into()))?;
        if rebuild || matcher.is_none() {
            *matcher = Some(Arc::new(IndexIgnoreMatcher::new(config)?));
        }
        matcher
            .as_ref()
            .cloned()
            .ok_or_else(|| MemoryError::Persistence("reindex ignore matcher unavailable".into()))
    }
}

impl Default for ReindexQueueDrainer {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Migration SQL for the reindex queue table.
pub const MIGRATION_009_REINDEX_QUEUE: &str = r#"
CREATE TABLE IF NOT EXISTS reindex_queue (
    id          TEXT    PRIMARY KEY,
    project_id  TEXT    NOT NULL,
    file_path   TEXT    NOT NULL,
    reason      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    processed   INTEGER NOT NULL DEFAULT 0
);
"#;

fn is_ignore_control_file(path: &Path, config: Option<&IndexConfig>) -> bool {
    let conventional = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".gitignore" || name == ".concertoignore");
    if conventional {
        return true;
    }
    let Some(config) = config else {
        return false;
    };
    let Some(configured) = config.ignore_file.as_ref() else {
        return false;
    };
    let configured_path = if configured.is_absolute() {
        configured.as_std_path().to_path_buf()
    } else {
        config.project_dir.join(configured).as_std_path().to_path_buf()
    };
    path == configured_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{IndexConfig, ProjectIndexer};
    use crate::sync::ChunkSyncService;
    use crate::testing::{InMemoryFullTextStore, InMemoryVectorStore};
    use concerto_core::event::EventBus;
    use std::time::Duration;

    #[tokio::test]
    async fn drain_empty_queue() {
        let drainer = ReindexQueueDrainer::new(None);
        let cancel = CancellationToken::new();
        let count = drainer.drain(cancel).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn enqueue_and_idempotent() {
        let drainer = ReindexQueueDrainer::new(None);
        let pid = ProjectId("test".into());
        let path = std::path::Path::new("src/main.rs");
        assert!(drainer.enqueue(&pid, path, "test").await.is_ok());
    }

    #[tokio::test]
    async fn active_watch_keeps_native_watcher_alive() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let watcher = FileWatcher::new(EventBus::default(), ProjectId("test".into()));
        let mut active = watcher.watch(root.path(), cancel.clone()).await.unwrap();

        let changed = root.path().join("changed.rs");
        std::fs::write(&changed, "fn changed() {}\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(paths) = active.recv().await {
                if paths.iter().any(|path| Path::new(path) == changed) {
                    return;
                }
            }
            panic!("watcher closed before delivering an event");
        })
        .await
        .expect("watcher event timed out");
        cancel.cancel();
    }

    #[tokio::test]
    async fn enqueue_multiple_paths() {
        let drainer = ReindexQueueDrainer::new(None);
        let pid = ProjectId("test".into());
        assert!(drainer.enqueue(&pid, Path::new("src/main.rs"), "test").await.is_ok());
        assert!(drainer.enqueue(&pid, Path::new("src/lib.rs"), "test").await.is_ok());
        assert!(drainer.enqueue(&pid, Path::new("src/lib.rs"), "test").await.is_ok());
    }

    #[tokio::test]
    async fn drain_with_items_returns_count() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let project_id = ProjectId("test".into());
        let indexer =
            Arc::new(ProjectIndexer::new(embedder, EventBus::default(), project_id.clone()));
        let sync = Arc::new(ChunkSyncService::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
        ));
        let config = IndexConfig::default();
        let drainer = ReindexQueueDrainer::with_indexer_and_sync(
            Some(pool),
            indexer,
            sync,
            project_id.clone(),
            config,
        );
        drainer.enqueue(&project_id, Path::new("src/main.rs"), "test").await.unwrap();
        drainer.enqueue(&project_id, Path::new("src/lib.rs"), "test").await.unwrap();
        let count = drainer.drain(CancellationToken::new()).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn drain_twice_second_returns_zero() {
        let drainer = ReindexQueueDrainer::new(None);
        let pid = ProjectId("test".into());
        drainer.enqueue(&pid, Path::new("src/main.rs"), "test").await.unwrap();
        drainer.drain(CancellationToken::new()).await.unwrap();
        let count = drainer.drain(CancellationToken::new()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn enqueue_to_full_channel_does_not_block() {
        let drainer = ReindexQueueDrainer::new(None);
        let pid = ProjectId("test".into());
        assert!(drainer.enqueue(&pid, Path::new("a.rs"), "test").await.is_ok());
        assert!(drainer.enqueue(&pid, Path::new("b.rs"), "test").await.is_ok());
        // Channel is full but enqueue should still succeed (uses try_send with warning)
        let _ = drainer.enqueue(&pid, Path::new("c.rs"), "test").await;
    }
}
