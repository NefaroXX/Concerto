//! Production memory-init path coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24A): the two `XDG_DATA_HOME`-redirect serial
//! locks (`MEMORY_INIT_ENV_LOCK` / `MEMORY_INIT_TEST_SERIAL`), the
//! `XdgDataHomeGuard` RAII redirect, and the two ADR-69 production `init_path_*`
//! tests move verbatim out of `runtime_runner::runtime_runner_tests`, so test
//! names, statics, and assertions are unchanged. The locks and the guard move
//! WITH the tests (never duplicated), so the two init tests still serialise
//! process-wide against each other and against the real user data dir.
//! `use super::super::*;` keeps the parent (`runtime_runner_impl`) items in
//! scope; the `concerto_memory` imports are the subset of the old test-mod
//! imports this cluster touches.

use super::super::*;
use concerto_memory::storage::MemoryDb;
use concerto_memory::vector_store::{SqliteVectorStore, VectorStore};

// ===========================================================================
// ADR-69 slice 1: production link-store activation (migration 002 + wiring).
// ===========================================================================

/// Serializes tests that redirect `XDG_DATA_HOME` so memory init uses a
/// temp data root instead of the real user data dir (mirrors `ENV_LOCK`
/// in `concerto-cli`).
static MEMORY_INIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes the `XDG_DATA_HOME`-redirecting init tests for their WHOLE
/// duration. `MEMORY_INIT_ENV_LOCK` only guards the synchronous
/// set/restore (it must not be held across an `.await`); without this
/// process-wide async lock two init tests can interleave their redirects
/// and read each other's data root.
static MEMORY_INIT_TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII redirect of `XDG_DATA_HOME` to a fresh temp directory; restores
/// the previous value on drop (panic-safe). The serialization lock is only
/// held around the synchronous set/restore, never across an `.await`
/// (clippy::await_holding_lock).
struct XdgDataHomeGuard {
    previous: Option<String>,
}

impl XdgDataHomeGuard {
    fn redirect(temp: &std::path::Path) -> Self {
        let xdg_data = temp.join("xdg-data");
        std::fs::create_dir_all(&xdg_data).expect("create xdg-data");
        let previous = {
            let _lock = MEMORY_INIT_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            let previous = std::env::var("XDG_DATA_HOME").ok();
            std::env::set_var("XDG_DATA_HOME", &xdg_data);
            previous
        };
        Self { previous }
    }
}

impl Drop for XdgDataHomeGuard {
    fn drop(&mut self) {
        let _lock = MEMORY_INIT_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        match &self.previous {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

/// ADR-69 slice 1 production activation. The real production init path —
/// `init_memory_system_with_handles`, the single funnel every frontend
/// (desktop pre-init, persistent runner, `run_shared_agent`) routes its
/// memory construction through — must attach the link store over the same
/// SQLite pool as the vector store and run migration 002 via the normal
/// `MemoryDb::connect` chain. We assert on the concrete production
/// database under a redirected `XDG_DATA_HOME`:
///   1. the production-built system carries the link store (reported
///      through the `Arc<dyn MemoryStore>` seam by
///      `MemoryStore::link_store_attached`, without downcasting), and
///   2. migration 002 created the `memory_links` table (fresh install), and
///   3. the production link-store helper round-trips a link over the
///      shared project database.
#[tokio::test]
async fn init_path_attaches_link_store_over_project_pool() {
    use concerto_core::memory::{MemoryLink, MemoryLinkKind};

    let _serial = MEMORY_INIT_TEST_SERIAL.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _xdg = XdgDataHomeGuard::redirect(temp.path());

    // Empty project dir: the background index is a no-op and never needs
    // the embedder model download.
    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let reindex: Arc<Mutex<Option<Arc<ProjectIndexer>>>> = Arc::new(Mutex::new(None));
    let reindex_sync: Arc<Mutex<Option<Arc<ChunkSyncService>>>> = Arc::new(Mutex::new(None));
    let memory_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let data_dir_lock: Arc<Mutex<Option<Arc<DataDirLock>>>> = Arc::new(Mutex::new(None));

    let mut config = AppConfig::default();
    // Keep init hermetic: the provider-backed L1 dedup judge is orthogonal
    // to the link store and must not be resolved in a test.
    config.memory.dedup_judge = false;

    let handles = init_memory_system_with_handles(
        EventBus::default(),
        &config,
        &project_dir,
        &reindex,
        &reindex_sync,
        &memory_cancel,
        &data_dir_lock,
    )
    .await
    .expect("production memory init must succeed under a temp data root");

    // 1. The production-built system carries the link store:
    //    `MemoryStore::link_store_attached` reports it through the
    //    `Arc<dyn MemoryStore>` handle, so `true` proves
    //    `.with_link_store(...)` ran on the init path.
    assert!(
        handles.store.link_store_attached(),
        "production-built memory system must carry the link store"
    );
    drop(handles.store);

    // 2. Migration 002 ran through the production connect chain (fresh
    //    install: the `memory_links` table exists in the project DB).
    let db_path = temp.path().join("xdg-data/concerto/memory/memory.db");
    assert!(db_path.is_file(), "production memory db must exist at {db_path:?}");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(30));
    let pool =
        sqlx::SqlitePool::connect_with(options).await.expect("open the production memory db");
    let table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'memory_links'",
    )
    .fetch_one(&pool)
    .await
    .expect("query sqlite_master for memory_links");
    assert_eq!(table, 1, "production init must run migration 002 (memory_links table)");

    // 3. The production link-store helper is functional over the shared
    //    project database: a link persisted through the same builder the
    //    init path calls round-trips from the database the vector store
    //    indexes into.
    let link_store =
        build_link_store(&pool, None).await.expect("production link-store helper must attach");
    let link = MemoryLink::new("src-init", "tgt-init", MemoryLinkKind::References);
    link_store
        .put(&link, CancellationToken::new())
        .await
        .expect("persist a link through the production link-store helper");
    let read_back = link_store
        .links_from("src-init", CancellationToken::new())
        .await
        .expect("read the link back");
    assert_eq!(read_back, vec![link], "production memory db must round-trip a persisted link");
}

/// ADR-54 re-enable + ADR-69 slice 2 wiring on the production init path:
/// global memory is opened beside the project DB, and the init retention
/// pass prunes cold-but-linked derived rows while never touching source
/// chunks.
#[tokio::test]
async fn init_path_opens_global_store_and_prunes_orphaned_derived() {
    use concerto_core::memory::{ChunkType, EmbeddingRecord, MemoryLinkKind, ProjectId};

    let _serial = MEMORY_INIT_TEST_SERIAL.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _xdg = XdgDataHomeGuard::redirect(temp.path());
    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Pre-seed the production project DB (the path init will open) with a
    // cold-linked derived Fact, a fresh source Function, and an old link.
    let memory_dir = temp.path().join("xdg-data/concerto/memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    let db_path = camino::Utf8PathBuf::from_path_buf(memory_dir.join("memory.db")).unwrap();
    let db = MemoryDb::connect(&db_path).await.expect("seed db");
    let pool = db.pool().clone();
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(&project_dir));
    let store = SqliteVectorStore::new(pool.clone()).await.expect("vector store");

    let record = |id: &str, chunk_type: ChunkType| EmbeddingRecord {
        id: id.to_string(),
        project_id: project_id.clone(),
        chunk_hash: format!("hash-{id}"),
        content: id.to_string(),
        file_path: format!("seed/{id}").into(),
        start_line: None,
        end_line: None,
        chunk_type,
        vector: vec![0.5, -0.25],
        model_id: "test".into(),
        model_version: "1".into(),
        stale: false,
        created_at: time::OffsetDateTime::now_utc(),
    };
    store
        .store_projection(
            &record("cold-linked", ChunkType::Fact),
            &serde_json::json!({ "session_id": "sess" }),
            CancellationToken::new(),
        )
        .await
        .expect("seed derived row");
    store
        .store(&[record("fn-src", ChunkType::Function)], CancellationToken::new())
        .await
        .expect("seed source chunk");

    // Cold evidence: a 120-day-old link on the 90-day ADR A5 floor.
    let cold = (time::OffsetDateTime::now_utc() - time::Duration::days(120)).to_string();
    sqlx::query(
        "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at) \
         VALUES (?, ?, ?, 1.0, ?)",
    )
    .bind("src-cold")
    .bind("cold-linked")
    .bind(MemoryLinkKind::Supports.as_str())
    .bind(&cold)
    .execute(&pool)
    .await
    .expect("seed cold link");
    drop(store);
    drop(pool);

    let reindex: Arc<Mutex<Option<Arc<ProjectIndexer>>>> = Arc::new(Mutex::new(None));
    let reindex_sync: Arc<Mutex<Option<Arc<ChunkSyncService>>>> = Arc::new(Mutex::new(None));
    let memory_cancel: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let data_dir_lock: Arc<Mutex<Option<Arc<DataDirLock>>>> = Arc::new(Mutex::new(None));

    let mut config = AppConfig::default();
    config.memory.dedup_judge = false;

    let handles = init_memory_system_with_handles(
        EventBus::default(),
        &config,
        &project_dir,
        &reindex,
        &reindex_sync,
        &memory_cancel,
        &data_dir_lock,
    )
    .await
    .expect("production memory init must succeed under a temp data root");
    drop(handles.store);

    // Global tier: the database is created with its table.
    let global_path = temp.path().join("xdg-data/concerto/memory/global_memory.db");
    assert!(global_path.is_file(), "init must create global_memory.db (ADR-54 re-enable)");
    let global_options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&global_path)
        .busy_timeout(std::time::Duration::from_secs(30));
    let global_pool = sqlx::SqlitePool::connect_with(global_options)
        .await
        .expect("open the production global db");
    let table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'global_memory'",
    )
    .fetch_one(&global_pool)
    .await
    .expect("query sqlite_master for global_memory");
    assert_eq!(table, 1, "global memory table must exist after init");

    // Orphan prune: cold-but-linked derived row gone, source chunk kept.
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .busy_timeout(std::time::Duration::from_secs(30));
    let pool =
        sqlx::SqlitePool::connect_with(options).await.expect("reopen the production memory db");
    let cold_left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vector_store WHERE id = 'cold-linked'")
            .fetch_one(&pool)
            .await
            .expect("count cold-linked");
    assert_eq!(cold_left, 0, "cold-but-linked derived row must be pruned at init");
    let src_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vector_store WHERE id = 'fn-src'")
        .fetch_one(&pool)
        .await
        .expect("count fn-src");
    assert_eq!(src_left, 1, "source chunk with old evidence must never be pruned");
}
