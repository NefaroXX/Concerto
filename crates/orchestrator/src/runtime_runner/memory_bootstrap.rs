//! Memory-system bootstrap for `runtime_runner_impl`.
//!
//! This module owns the one funnel that stands up a project's memory
//! subsystem: `init_memory_system` / `init_memory_system_with_handles`
//! (ADR-11 data-dir lock, vector + FTS + decision/task stores, global tier,
//! background indexing/watching), plus the two fail-open attachments built
//! alongside it — the ADR-46 L1 summarizer and the ADR-69 slice-1 link
//! store. The cluster moves verbatim from `runtime_runner.rs` (NORM S21b):
//! no behavior, signature, or call-site change.
//!
//! Stability contract: `init_memory_system` keeps a `pub` re-export from the
//! parent — `runtime_runner_persistent` republishes it as
//! `runtime_runner::init_memory_system`, which the desktop front end calls.
//! The rest of the cluster is re-exported `pub(crate)` and consumed by the
//! parent's `select_or_init_memory_services` and the memory init-path tests
//! in `runtime_runner::runtime_runner_tests`, all through the parent's
//! re-exports. This file is loaded via the parent's explicit
//! `#[path = "runtime_runner/memory_bootstrap.rs"]`, mirroring
//! `runtime_runner/recorders.rs` (the parent itself is loaded via `#[path]`
//! as `runtime_runner_impl`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use concerto_config::AppConfig;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::lock::DataDirLock;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::ProjectId;
use concerto_core::{CancellationToken, OrchestratorError};

use concerto_memory::decision_store::DecisionStore;
use concerto_memory::embedder::{EmbeddingGenerator, ProviderEmbedder};
use concerto_memory::entities::{L1DedupJudge, L1Extractor};
use concerto_memory::fts::{FullTextStore, SqliteFullTextStore};
use concerto_memory::indexer::{IndexConfig, ProjectIndexer};
use concerto_memory::links::LinkStore;
use concerto_memory::rag::{CascadeTier, LinkCascadeConfig};
use concerto_memory::scoring::DECAY_FLOOR_DAYS;
use concerto_memory::storage::MemoryDb;
use concerto_memory::summarizer::LLMSummarizer;
use concerto_memory::sync::ChunkSyncService;
use concerto_memory::task_tree::TaskTreeStore;
use concerto_memory::vector_store::{SqliteVectorStore, VectorStore};
use concerto_memory::watcher::{FileWatcher, ReindexQueueDrainer};

use crate::services::ProviderSummarizer;

use super::{resolve_provider_and_model, MemorySystemHandles};

/// How long memory initialisation waits for the `.concerto.lock` (ADR-11)
/// before failing the memory init.
pub(crate) const MEMORY_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Initialise or reuse the memory system scoped to a project root.
pub async fn init_memory_system(
    bus: EventBus,
    config: &AppConfig,
    project_dir: &std::path::Path,
    reindex: &Arc<Mutex<Option<Arc<ProjectIndexer>>>>,
    reindex_sync: &Arc<Mutex<Option<Arc<ChunkSyncService>>>>,
    memory_cancel: &Arc<Mutex<Option<CancellationToken>>>,
    data_dir_lock: &Arc<Mutex<Option<Arc<DataDirLock>>>>,
) -> Result<Arc<dyn MemoryStore>, OrchestratorError> {
    init_memory_system_with_handles(
        bus,
        config,
        project_dir,
        reindex,
        reindex_sync,
        memory_cancel,
        data_dir_lock,
    )
    .await
    .map(|handles| handles.store)
}

/// As [`init_memory_system`], additionally returning the shared
/// decision/task-store handles the coordinator's Phase 6 M3a/M3b wiring needs.
pub(crate) async fn init_memory_system_with_handles(
    bus: EventBus,
    config: &AppConfig,
    project_dir: &std::path::Path,
    reindex: &Arc<Mutex<Option<Arc<ProjectIndexer>>>>,
    reindex_sync: &Arc<Mutex<Option<Arc<ChunkSyncService>>>>,
    memory_cancel: &Arc<Mutex<Option<CancellationToken>>>,
    data_dir_lock: &Arc<Mutex<Option<Arc<DataDirLock>>>>,
) -> Result<MemorySystemHandles, OrchestratorError> {
    // Re‑use the same implementation as CLI/Desktop apps – copy/paste the
    // `init_memory_system` logic from those modules (project ID hashing, DB
    // path, vector & FTS stores, optional embedder, background indexing).
    let (project_id, lifecycle, db, pool, data_dir) =
        open_memory_db(project_dir, memory_cancel, data_dir_lock).await?;
    let (link_store, vector_store, fts_store) = build_memory_stores(&pool, config).await?;
    run_memory_retention(&vector_store, &fts_store, &pool, &link_store, &project_id, config).await;
    let decision_store = Arc::new(DecisionStore::load(db.clone()).await.map_err(|error| {
        OrchestratorError::AgentLoopError(format!("DecisionStore load error: {error}"))
    })?);
    let task_tree = Arc::new(TaskTreeStore::load(db.clone()).await.map_err(|error| {
        OrchestratorError::AgentLoopError(format!("TaskTreeStore load error: {error}"))
    })?);

    // Local fastembed embedder (BAAI/bge-small-en-v1.5). The model binary
    // downloads on first `embed` call; indexing is best‑effort and falls back
    // to FTS‑only when embedding is unavailable (e.g. offline).
    let embedder: Arc<dyn EmbeddingGenerator> =
        Arc::new(ProviderEmbedder::new("bge-small-en-v1.5"));

    // ADR-12 Option A startup gate: rows produced by an earlier embedder
    // version are marked stale (kept searchable but rank-demoted) so the
    // background re-index can lazily refresh only the rows that differ.
    // Rows already at the live version are untouched. Runs BEFORE the
    // background index task so every stale row is flagged up front.
    let live_version = embedder.model_version().to_string();
    match vector_store.row_index_facts(&project_id, lifecycle.child_token()).await {
        Ok(facts) => {
            let stale_rows: Vec<_> =
                facts.iter().filter(|fact| fact.model_version != live_version).collect();
            if !stale_rows.is_empty() {
                let _ = bus.publish_raw(EventKind::EmbeddingModelMismatch {
                    stored_version: stale_rows[0].model_version.clone(),
                    current_version: live_version.clone(),
                });
                let _ = bus.publish_raw(EventKind::StaleVectorsDetected {
                    project_id: project_id.0.clone(),
                    stale_count: stale_rows.len(),
                });
                if let Err(error) = vector_store
                    .mark_stale(&project_id, &live_version, lifecycle.child_token())
                    .await
                {
                    tracing::warn!(
                        %error,
                        "failed to mark stale embeddings at startup (re-index will refresh them)"
                    );
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to read model-version facts for staleness gate");
        }
    }

    // Chunk sync service is the single write path for the vector + FTS stores.
    let sync = Arc::new(ChunkSyncService::new(vector_store.clone(), fts_store.clone()));
    let indexer = Arc::new(ProjectIndexer::new(embedder.clone(), bus.clone(), project_id.clone()));

    // Expose the live indexer + sync so the UI can trigger re-indexes.
    *reindex.lock().unwrap_or_else(|e| e.into_inner()) = Some(indexer.clone());
    *reindex_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(sync.clone());

    // ADR-54 re-enable: global (user-scoped) memory lives in its own SQLite
    // database beside the project DB. Opening it is fail-soft — an unreadable
    // or non-UTF-8 path logs and leaves the global tier disabled; the run
    // never aborts. `GlobalMemoryStore::connect` self-heals a corrupted file
    // (quarantine + fresh open) before surfacing an error.
    let global_store = match camino::Utf8PathBuf::from_path_buf(data_dir.join("global_memory.db")) {
        Ok(path) => match concerto_memory::global::GlobalMemoryStore::connect(&path).await {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "global memory store unavailable; global tier disabled (ADR-54)"
                );
                None
            }
        },
        Err(path) => {
            tracing::warn!(
                path = %path.display(),
                "global memory DB path is not valid UTF-8; global tier disabled"
            );
            None
        }
    };
    let system = concerto_memory::system::MemorySystem::new(
        vector_store,
        fts_store,
        decision_store.clone(),
        task_tree.clone(),
        Some(embedder.clone()),
        project_id.clone(),
        global_store,
    );
    // ADR-46 L1 symbolic pass: the dedup judge AND the typed extractor
    // (TODO.md "L1 typed extraction") share ONE summarizer — one resolved
    // provider/model — and the same `[memory] dedup_judge` gate, so no new
    // config key is introduced. Attach both inside this initializer so EVERY
    // runtime path that builds a project memory system gets them — desktop
    // pre-init, the persistent runner, and `run_shared_agent` alike (the
    // store is cached per project and reused).
    // Fail-open: dedup disabled by config or an unresolvable model provider
    // leaves both off and stores behave exactly as before (plain writes).
    let system = match build_llm_summarizer(config, &lifecycle) {
        Some(summarizer) => system
            .with_dedup_judge(L1DedupJudge::new(Arc::clone(&summarizer)))
            .with_l1_extractor(L1Extractor::new(summarizer)),
        None => system,
    };
    // ADR-69 slice 1 link store: attach it inside this initializer so EVERY
    // runtime path that builds a project memory system gets it — desktop
    // pre-init, the persistent runner, and `run_shared_agent` alike (the
    // store is cached per project and reused). It was built above over the
    // SAME pool the vector store uses (see `build_link_store`), so
    // `memory_links` rows live in the same database as the chunks they
    // reference. Fail-open: an unopenable store leaves links off and memory
    // behaves exactly as before (plain writes).
    let system = match &link_store {
        Some(store) => system.with_link_store(store.clone()),
        None => system,
    };
    // ADR-69 slice 2 link cascade: when the link store attached, consume
    // link evidence at retrieval time. `cascade_decay_days` unset → the
    // 90-day ADR A5 floor; Some(0) disables decay entirely. The cascade is
    // fail-open (scoring errors/timeouts keep the plain RRF order), so it
    // can never degrade a query.
    let system = match &link_store {
        Some(_) => system.with_link_cascade(LinkCascadeConfig {
            decay_days: config.memory.cascade_decay_days.or(Some(DECAY_FLOOR_DAYS as u16)),
            start_tier: CascadeTier::Mild,
            score_timeout: std::time::Duration::from_millis(250),
        }),
        None => system,
    };
    let system = Arc::new(system);

    // Background project indexing — actually persists chunks (FTS + vectors).
    let mut index_config = IndexConfig {
        project_dir: camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())),
        ..IndexConfig::default()
    };
    index_config.exclude_patterns.extend(config.memory.exclude_patterns.clone());
    index_config.ignore_file = config.memory.ignore_file.clone();
    let indexer_bg = indexer.clone();
    let sync_bg = sync.clone();
    let pid_bg = project_id.clone();
    let index_cancel = lifecycle.child_token();

    // File watcher + reindex queue: enqueue changed files and drain by
    // re‑indexing them (rather than only marking them processed).
    let watcher = FileWatcher::new(bus.clone(), project_id.clone());
    let watch = match watcher.watch(project_dir, lifecycle.child_token()).await {
        Ok(watch) => Some(watch),
        Err(error) => {
            tracing::warn!(%error, "failed to start file watcher for project indexing");
            None
        }
    };
    let drainer = Arc::new(ReindexQueueDrainer::with_indexer_and_sync(
        Some(pool.clone()),
        indexer.clone(),
        sync.clone(),
        project_id.clone(),
        index_config.clone(),
    ));
    let drainer_cancel = lifecycle.child_token();
    tokio::spawn(async move {
        tracing::info!("starting background project indexing for {pid_bg}");
        match indexer_bg.index(&index_config, index_cancel.clone()).await {
            Ok(records) if !index_cancel.is_cancelled() => {
                match sync_bg.replace_project(&pid_bg, &records, index_cancel.clone()).await {
                    Ok(()) => {
                        tracing::info!(count = records.len(), "project indexing completed");
                    }
                    Err(error) => tracing::error!(%error, "failed to replace project index"),
                }
            }
            Ok(_) => tracing::debug!("project indexing cancelled before reconciliation"),
            Err(error) => tracing::error!(%error, "project indexing failed"),
        }

        if let Err(error) = drainer.drain(drainer_cancel.clone()).await {
            tracing::warn!(%error, "failed to drain queued memory re-index jobs");
        }
        let Some(mut watch) = watch else {
            return;
        };
        while let Some(mut paths) = watch.recv().await {
            while let Ok(mut queued_paths) = watch.try_recv() {
                paths.append(&mut queued_paths);
            }
            paths.sort();
            paths.dedup();
            for path in paths {
                if let Err(error) = drainer.enqueue(&pid_bg, Path::new(&path), "file_changed").await
                {
                    tracing::warn!(%error, %path, "failed to queue memory re-index");
                }
            }
            if let Err(error) = drainer.drain(drainer_cancel.clone()).await {
                tracing::warn!(%error, "failed to drain queued memory re-index jobs");
            }
        }
    });

    Ok(MemorySystemHandles { store: system as Arc<dyn MemoryStore>, decision_store, task_tree })
}

/// H1: open (and lock) the project memory database. Derives the project id,
/// mints a fresh lifecycle token (cancelling any previous one), acquires the
/// ADR-11 data-dir lock, ensures the memory directory exists, and connects the
/// pool.
///
/// Returns `(project_id, lifecycle, db, pool, memory_data_dir)`. The lock is
/// parked in `data_dir_lock` so it is held for the memory subsystem's
/// lifetime, not just during init.
async fn open_memory_db(
    project_dir: &std::path::Path,
    memory_cancel: &Arc<Mutex<Option<CancellationToken>>>,
    data_dir_lock: &Arc<Mutex<Option<Arc<DataDirLock>>>>,
) -> Result<
    (ProjectId, CancellationToken, Arc<MemoryDb>, sqlx::SqlitePool, std::path::PathBuf),
    OrchestratorError,
> {
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(project_dir));
    let lifecycle = CancellationToken::new();
    let previous =
        memory_cancel.lock().unwrap_or_else(|error| error.into_inner()).replace(lifecycle.clone());
    if let Some(previous) = previous {
        previous.cancel();
    }

    // ADR-11: one `.concerto.lock` at the data root governs the memory
    // database. Acquired here and parked into `data_dir_lock` so it is held
    // for the memory subsystem's lifetime, not just during init.
    let root_data_dir = concerto_sessions::app_data_dir()
        .map_err(|e| OrchestratorError::AgentLoopError(format!("data directory error: {e}")))?;
    let lock = concerto_core::lock::acquire_data_dir_lock(
        &root_data_dir,
        Some(MEMORY_LOCK_TIMEOUT),
        Some(&lifecycle),
    )
    .map_err(|e| OrchestratorError::AgentLoopError(format!("data directory error: lock {e}")))?;
    *data_dir_lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(lock);

    let data_dir = root_data_dir.join("memory");
    let db_path = data_dir.join("memory.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OrchestratorError::AgentLoopError(format!("IO error: {}", e)))?;
    }
    let db = Arc::new(
        MemoryDb::connect(
            &camino::Utf8PathBuf::from_path_buf(db_path)
                .map_err(|e| OrchestratorError::AgentLoopError(format!("Path error: {:?}", e)))?,
        )
        .await
        .map_err(|e| OrchestratorError::AgentLoopError(format!("MemoryDb connect error: {}", e)))?,
    );
    let pool = db.pool().clone();
    Ok((project_id, lifecycle, db, pool, data_dir))
}

/// H2: build the project memory stores over `pool` — the ADR-69 slice-1 link
/// store (fail-open), the vector store, and the FTS store. Vector/FTS
/// construction errors abort memory init; an unopenable link store degrades to
/// `None`.
async fn build_memory_stores(
    pool: &sqlx::SqlitePool,
    config: &AppConfig,
) -> Result<(Option<Arc<LinkStore>>, Arc<dyn VectorStore>, Arc<dyn FullTextStore>), OrchestratorError>
{
    // ADR-69 slice 1 link store: built here (over the same pool the vector
    // store uses) so the init retention prunes below can also run the
    // slice-2 orphan-evidence prune against it. Fail-open: an unopenable
    // store leaves links off and memory behaves exactly as before.
    let link_store = build_link_store(pool, config.memory.max_out_degree).await;
    let vector_store: Arc<dyn VectorStore> = Arc::new(
        SqliteVectorStore::new(pool.clone())
            .await
            .map_err(|e| OrchestratorError::AgentLoopError(format!("VectorStore error: {}", e)))?,
    );
    let fts_store: Arc<dyn FullTextStore> =
        Arc::new(SqliteFullTextStore::new(pool.clone()).await.map_err(|e| {
            OrchestratorError::AgentLoopError(format!("FullTextStore error: {}", e))
        })?);
    Ok((link_store, vector_store, fts_store))
}

/// H3: run the memory retention passes at init — TTL expiry purge, derived
/// summary prune, and (when the link store attached) the ADR-69 slice-2
/// orphaned derived-evidence prune. All three are fail-soft: a failed pass
/// logs and the run continues. Never returns a value and never fails init.
async fn run_memory_retention(
    vector_store: &Arc<dyn VectorStore>,
    fts_store: &Arc<dyn FullTextStore>,
    pool: &sqlx::SqlitePool,
    link_store: &Option<Arc<LinkStore>>,
    project_id: &ProjectId,
    config: &AppConfig,
) {
    let ttl = concerto_memory::ttl::TtlManager::with_default_ttl_days(
        vector_store.clone(),
        fts_store.clone(),
        pool.clone(),
        config.memory.ttl_days,
    );
    if let Err(error) = ttl.purge_expired(project_id, CancellationToken::new()).await {
        tracing::warn!(%error, "failed to purge expired project memory");
    }
    // ADR-65 §8: prune derived summary chunks (Fact/SessionSummary) past the
    // configured retention — NEVER source chunks. Fail-soft: a failed prune
    // only logs; the pass itself logs what it removed.
    if let Err(error) = ttl
        .prune_derived_summaries(
            project_id,
            config.memory.summary_keep_per_session,
            config.memory.summary_retention_days,
            CancellationToken::new(),
        )
        .await
    {
        tracing::warn!(%error, "failed to prune derived summaries past retention");
    }
    // ADR-69 slice 2: prune DERIVED rows whose incoming link evidence has
    // decayed cold — NEVER source chunks and NEVER unlinked rows. Same policy
    // knob as the retrieval cascade (`cascade_decay_days`; `None` → the
    // 90-day ADR A5 floor, `Some(0)` disables decay). Fail-open: a failed
    // prune only logs.
    if let Some(store) = link_store {
        if let Err(error) = ttl
            .prune_orphaned_derived(
                project_id,
                store,
                config.memory.cascade_decay_days.or(Some(DECAY_FLOOR_DAYS as u16)),
                CancellationToken::new(),
            )
            .await
        {
            tracing::warn!(%error, "failed to prune orphaned derived memory");
        }
    } else {
        tracing::debug!("link store unavailable — orphan-evidence prune skipped (fail-open)");
    }
}

/// Build the shared summarizer behind the ADR-46 L1 symbolic pass, or `None`.
///
/// One summarizer serves BOTH consumers of the pass: the L1 dedup judge and
/// the L1 typed extractor (TODO.md "L1 typed extraction"), so both always
/// talk to the same resolved model. It is wired from the *default* resolved
/// model rather than the model a specific run selects: the pass is advisory
/// (ADR-46 symbolic offload), so a cheap, stable model is preferable to
/// chasing per-run parity.
///
/// Fail-open by construction:
/// - `[memory] dedup_judge = false` disables the whole pass;
/// - a provider that cannot be resolved at init time (e.g. plugin-backed
///   only configs, or a missing model configuration) logs at debug and
///   yields `None`, leaving `MemorySystem` stores byte-identical to the
///   pre-pass behavior.
pub(crate) fn build_llm_summarizer(
    config: &AppConfig,
    lifecycle: &CancellationToken,
) -> Option<Arc<dyn LLMSummarizer>> {
    if !config.memory.dedup_judge {
        tracing::debug!("l1 symbolic pass disabled by [memory] dedup_judge=false");
        return None;
    }
    // No plugin providers are loaded inside memory init; resolution is
    // best-effort and must never abort the memory subsystem.
    let no_plugin_providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    let (provider, model, _provider_config_id) =
        match resolve_provider_and_model(config, None, None, &no_plugin_providers) {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::debug!(
                    %error,
                    "l1 symbolic pass not attached: no resolvable model provider at memory init"
                );
                return None;
            }
        };
    Some(Arc::new(ProviderSummarizer::new(provider, model, lifecycle.child_token())))
}

/// Build the ADR-69 slice-1 symbolic link store over the project memory pool,
/// or `None` (fail-open).
///
/// The link store shares the SAME SQLite pool the vector store was built on,
/// so `memory_links` rows live in the same database — and same write space —
/// as the chunks they reference. Slice 1 is write-only and advisory: a pool
/// or schema problem logs a warning and yields `None`, leaving memory stores
/// byte-identical to the pre-link behavior (no links are ever written).
///
/// `max_out_degree` applies the configured ADR-69 A2 per-chunk cap; `None`
/// keeps the link store's built-in default.
pub(crate) async fn build_link_store(
    pool: &sqlx::SqlitePool,
    max_out_degree: Option<usize>,
) -> Option<Arc<LinkStore>> {
    match LinkStore::new(pool.clone()).await {
        Ok(store) => match max_out_degree {
            Some(cap) => Some(Arc::new(store.with_max_out_degree(cap))),
            None => Some(Arc::new(store)),
        },
        Err(error) => {
            tracing::warn!(%error, "memory link store not attached; ADR-69 links disabled");
            None
        }
    }
}
