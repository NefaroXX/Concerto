//! Project memory operations shared by interactive and command frontends.
use std::path::Path;
use std::sync::{Arc, Mutex};

use concerto_config::AppConfig;
use concerto_core::event::EventBus;
use concerto_core::lock::DataDirLock;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryFilter, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::{CancellationToken, MemoryStore};
use concerto_memory::indexer::{IndexConfig, ProjectIndexer};
use concerto_memory::sync::ChunkSyncService;

use crate::runtime_runner::{init_memory_system, ActiveMemoryServices};

#[derive(Default)]
struct Lifecycle {
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    lock: Arc<Mutex<Option<Arc<DataDirLock>>>>,
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
            cancel.cancel();
        }
    }
}

/// Holds standalone memory resources until the explorer closes. A running
/// frontend can instead borrow its existing project store without stopping it.
pub struct MemoryExplorer {
    store: Arc<dyn MemoryStore>,
    indexer: Arc<ProjectIndexer>,
    sync: Arc<ChunkSyncService>,
    project_id: ProjectId,
    index_config: IndexConfig,
    _lifecycle: Option<Lifecycle>,
}

fn index_config(project: &Path, config: &AppConfig) -> Result<IndexConfig, String> {
    let mut index = IndexConfig {
        project_dir: camino::Utf8PathBuf::from_path_buf(project.to_path_buf())
            .map_err(|_| "project path must be UTF-8".to_string())?,
        ..IndexConfig::default()
    };
    index.exclude_patterns.extend(config.memory.exclude_patterns.clone());
    index.ignore_file = config.memory.ignore_file.clone();
    Ok(index)
}

impl MemoryExplorer {
    /// Reuse a frontend's active store, with an explicit project check.
    pub fn from_active(
        active: &ActiveMemoryServices,
        project: &Path,
        config: &AppConfig,
    ) -> Result<Self, String> {
        let project_id = ProjectId(concerto_core::helpers::project_id_hash(project));
        if active.project_id != project_id {
            return Err("memory belongs to another project".into());
        }
        Ok(Self {
            store: active.store.clone(),
            indexer: active.reindex.clone(),
            sync: active.reindex_sync.clone(),
            project_id,
            index_config: index_config(project, config)?,
            _lifecycle: None,
        })
    }

    /// Initialise through the same bootstrap as Desktop and agent runs. The
    /// guard also cleans up when cancellation interrupts a partial bootstrap.
    pub async fn open(
        project: &Path,
        config: &AppConfig,
        bus: EventBus,
        cancel: CancellationToken,
    ) -> Result<Self, String> {
        if !config.memory.enabled {
            return Err("Memory is disabled. Enable memory.enabled in settings.".into());
        }
        let lifecycle = Lifecycle::default();
        let indexer = Arc::new(Mutex::new(None));
        let sync = Arc::new(Mutex::new(None));
        let store = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("memory operation cancelled".into()),
            result = init_memory_system(bus, config, project, &indexer, &sync, &lifecycle.cancel, &lifecycle.lock) => result.map_err(|e| e.to_string())?,
        };
        let indexer = indexer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or("memory indexer was not initialized")?;
        let sync = sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or("memory synchronization was not initialized")?;
        Ok(Self {
            store,
            indexer,
            sync,
            project_id: ProjectId(concerto_core::helpers::project_id_hash(project)),
            index_config: index_config(project, config)?,
            _lifecycle: Some(lifecycle),
        })
    }

    /// Browse or search using the canonical retrieval pipeline and metadata.
    pub async fn search(
        &self,
        text: &str,
        kind: Option<ChunkType>,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, String> {
        if !(1..=1000).contains(&limit) {
            return Err("memory limit must be between 1 and 1000".into());
        }
        let result = if text.trim().is_empty() {
            self.store.browse(&self.project_id, limit, cancel.clone()).await
        } else {
            self.store
                .retrieve(
                    &MemoryQuery {
                        text: text.trim().to_string(),
                        project_id: self.project_id.clone(),
                        namespace: MemoryNamespace::Project(self.project_id.clone()),
                        top_k: limit,
                        filters: kind
                            .map(|kind| vec![MemoryFilter::ChunkType(kind)])
                            .unwrap_or_default(),
                    },
                    cancel.clone(),
                )
                .await
        };
        if cancel.is_cancelled() {
            return Err("memory operation cancelled".into());
        }
        Ok(result
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|chunk| {
                chunk.project_id == self.project_id
                    && kind.is_none_or(|kind| kind == chunk.chunk_type)
            })
            .collect())
    }

    /// Full content and metadata, including entries outside the current list.
    pub async fn inspect(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<MemoryChunk>, String> {
        self.store
            .inspect_project_chunk(&self.project_id, id, cancel)
            .await
            .map_err(|e| e.to_string())
    }

    /// The caller must obtain explicit confirmation of the selected ID first.
    pub async fn delete(&self, id: &str, cancel: CancellationToken) -> Result<(), String> {
        self.store
            .invalidate_project_chunk(&self.project_id, id, cancel)
            .await
            .map_err(|e| e.to_string())
    }

    /// Rebuild the selected project's index with its configured exclusions.
    pub async fn reindex(&self, cancel: CancellationToken) -> Result<usize, String> {
        let records = self
            .indexer
            .index(&self.index_config, cancel.clone())
            .await
            .map_err(|e| e.to_string())?;
        if cancel.is_cancelled() {
            return Err("memory re-index cancelled".into());
        }
        self.sync
            .replace_project(&self.project_id, &records, cancel)
            .await
            .map_err(|e| e.to_string())?;
        Ok(records.len())
    }
}
