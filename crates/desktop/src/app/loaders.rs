//! The async loader Task-factories for [`App`] — NORM slice S46.
//!
//! This module owns the app's batch of `iced::Task` factory helpers: the
//! Memory re-index / graph / entries / delete loaders, the Git-summary
//! refresh, the sidebar session-list load (with its synchronous
//! project-tree rebuild), the session resume, the Spend Log reload, and
//! the read-only Studio runtime-snapshot load. Every body moved verbatim
//! from `app.rs` at the same 4-space `impl` indent, so each copy is
//! line-for-line with its origin; the only structural edit is each method
//! becoming `pub(super)`, which keeps the existing callers — the root
//! `update` arms, the sibling submodule groups, and the `App::new` boot
//! batch — resolving through `self.*` / `app.*` unchanged (the same
//! visibility pattern as the sibling group methods). No `Message` / `App`
//! shape change and no behavior change: the `Task` factories are moved
//! as-is, so async timing and ordering are identical, and the tests in
//! `app.rs`'s `mod tests` stay put and drive `update()` unchanged.
//!
//! Left in `app.rs` deliberately: the synchronous status-bar spend/cap
//! helpers (`sync_session_cap_from_config`, `reset_spend_state`,
//! `reconcile_cap_state`) and the VFS diff reload (`load_diff_from_vfs`) —
//! they mutate state in place and build no `Task`, so they are not
//! loaders.

use super::*;

impl App {
    /// Trigger a real project re-index, persisting chunks via the live sync
    /// service. Used by the Memory view's Refresh / Re-index controls. Until
    /// If memory has not been initialised yet, this initializes it first so
    /// the Memory page works immediately after a restart.
    pub(super) fn trigger_reindex(&mut self) -> iced::Task<Message> {
        if !self.config.as_ref().is_some_and(|config| config.memory.enabled) {
            self.memory.set_enabled(false);
            self.toasts.push(
                ToastLevel::Info,
                "Enable memory in Settings before re-indexing.".to_string(),
            );
            return iced::Task::none();
        }
        let memory = self.memory_services.clone();
        let project_dir = self.project_dir.clone();
        let bus = self.bus.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let mut index_config = IndexConfig {
            project_dir: camino::Utf8PathBuf::from_path_buf(project_dir.clone())
                .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())),
            ..IndexConfig::default()
        };
        index_config.exclude_patterns.extend(app_config.memory.exclude_patterns.clone());
        index_config.ignore_file = app_config.memory.ignore_file.clone();
        self.memory.status = MemoryStatus::Indexing { processed: 0, total: 0 };
        iced::Task::perform(
            async move {
                let project_id = concerto_core::types::ProjectId(
                    concerto_core::helpers::project_id_hash(&project_dir),
                );
                // Check if memory is already initialized for this project
                let active = {
                    let lock = memory.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().and_then(|m| {
                        if m.project_id == project_id {
                            Some((
                                m.store.clone(),
                                m.reindex.clone(),
                                m.reindex_sync.clone(),
                                m.cancel.child_token(),
                            ))
                        } else {
                            None
                        }
                    })
                };
                if let Some((_store, indexer, sync, cancel)) = active {
                    // Already initialized — trigger reindex
                    match indexer.index(&index_config, cancel.clone()).await {
                        Ok(records) if !cancel.is_cancelled() => {
                            match sync.replace_project(&project_id, &records, cancel.clone()).await
                            {
                                Ok(()) => ReindexResult::Done(records.len()),
                                Err(error) => ReindexResult::Failed(error.to_string()),
                            }
                        }
                        Ok(_) => ReindexResult::Failed("memory re-index cancelled".into()),
                        Err(e) => ReindexResult::Failed(e.to_string()),
                    }
                } else {
                    // Cancel previous project's lifecycle if present
                    if let Some(prev) = memory.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        prev.cancel.cancel();
                    }
                    // Initialize memory system first
                    let reindex_temp: Arc<Mutex<Option<Arc<ProjectIndexer>>>> =
                        Arc::new(Mutex::new(None));
                    let reindex_sync_temp: Arc<Mutex<Option<Arc<ChunkSyncService>>>> =
                        Arc::new(Mutex::new(None));
                    let cancel_temp: Arc<Mutex<Option<CancellationToken>>> =
                        Arc::new(Mutex::new(None));
                    let lock_temp: Arc<Mutex<Option<Arc<concerto_core::lock::DataDirLock>>>> =
                        Arc::new(Mutex::new(None));
                    match init_memory_system(
                        bus,
                        &app_config,
                        &project_dir,
                        &reindex_temp,
                        &reindex_sync_temp,
                        &cancel_temp,
                        &lock_temp,
                    )
                    .await
                    {
                        Ok(store) => {
                            let indexer =
                                reindex_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let sync =
                                reindex_sync_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let cancel = cancel_temp
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .take()
                                .unwrap_or_default();
                            let data_dir_lock =
                                lock_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                            match (indexer, sync) {
                                (Some(indexer), Some(sync)) => {
                                    let active = ActiveMemoryServices {
                                        project_id: project_id.clone(),
                                        store: store.clone(),
                                        reindex: indexer.clone(),
                                        reindex_sync: sync.clone(),
                                        cancel: cancel.clone(),
                                        data_dir_lock,
                                        decision_store: None,
                                        task_tree: None,
                                    };
                                    *memory.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(active);
                                    // Run the initial index
                                    let child_cancel = cancel.child_token();
                                    match indexer.index(&index_config, child_cancel.clone()).await {
                                        Ok(records) if !child_cancel.is_cancelled() => {
                                            match sync
                                                .replace_project(
                                                    &project_id,
                                                    &records,
                                                    child_cancel.clone(),
                                                )
                                                .await
                                            {
                                                Ok(()) => ReindexResult::Done(records.len()),
                                                Err(error) => {
                                                    ReindexResult::Failed(error.to_string())
                                                }
                                            }
                                        }
                                        Ok(_) => ReindexResult::Started,
                                        Err(e) => ReindexResult::Failed(e.to_string()),
                                    }
                                }
                                _ => ReindexResult::Skipped,
                            }
                        }
                        Err(error) => ReindexResult::Failed(error.to_string()),
                    }
                }
            },
            Message::ReindexResult,
        )
    }

    /// Load the project's memory graph from `<app data>/memory/memory.db`
    /// (read-only, ADR-69 slice 3). `MemoryError`s and a missing db both land
    /// as `MemoryGraphLoaded(Err)` / an empty graph, never a panic.
    pub(super) fn load_memory_graph(&self) -> iced::Task<Message> {
        let project_dir = self.project_dir.clone();
        iced::Task::perform(
            async move {
                let db_path = match concerto_sessions::app_data_dir() {
                    Ok(dir) => dir.join("memory").join("memory.db"),
                    Err(e) => return Err(format!("could not resolve app data dir: {e}")),
                };
                let project_id = concerto_core::memory::ProjectId(project_id_hash(&project_dir));
                concerto_memory::mermaid::load_memory_graph(
                    &db_path,
                    &project_id,
                    200,
                    CancellationToken::new(),
                )
                .await
                .map_err(|e| e.to_string())
            },
            Message::MemoryGraphLoaded,
        )
    }

    pub(super) fn load_memory_entries(&self) -> iced::Task<Message> {
        use concerto_core::memory::{
            ChunkType, MemoryFilter, MemoryNamespace, MemoryQuery, ProjectId,
        };

        if !self.config.as_ref().is_some_and(|config| config.memory.enabled) {
            return iced::Task::none();
        }
        let memory = self.memory_services.clone();
        let project_dir = self.project_dir.clone();
        let bus = self.bus.clone();
        let config = self.config.clone().unwrap_or_default();
        let project_id_for_query = ProjectId(project_id_hash(&self.project_dir));
        let query_text = self.memory.search_query().trim().to_string();
        let type_filter = self.memory.type_filter();

        iced::Task::perform(
            async move {
                let project_id = concerto_core::types::ProjectId(
                    concerto_core::helpers::project_id_hash(&project_dir),
                );
                // Look up the store, scoped to project
                let store = {
                    let lock = memory.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().and_then(|m| {
                        if m.project_id == project_id {
                            Some(m.store.clone())
                        } else {
                            None
                        }
                    })
                };
                let store: Arc<dyn MemoryStore> = if let Some(store) = store {
                    store
                } else {
                    // Cancel previous project's lifecycle if present
                    if let Some(prev) = memory.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        prev.cancel.cancel();
                    }
                    let reindex_temp: Arc<Mutex<Option<Arc<ProjectIndexer>>>> =
                        Arc::new(Mutex::new(None));
                    let reindex_sync_temp: Arc<Mutex<Option<Arc<ChunkSyncService>>>> =
                        Arc::new(Mutex::new(None));
                    let cancel_temp: Arc<Mutex<Option<CancellationToken>>> =
                        Arc::new(Mutex::new(None));
                    let lock_temp: Arc<Mutex<Option<Arc<concerto_core::lock::DataDirLock>>>> =
                        Arc::new(Mutex::new(None));
                    let store = init_memory_system(
                        bus,
                        &config,
                        &project_dir,
                        &reindex_temp,
                        &reindex_sync_temp,
                        &cancel_temp,
                        &lock_temp,
                    )
                    .await
                    .map_err(|error| error.to_string())?;

                    let reindex =
                        reindex_temp.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or_else(
                            || "init_memory_system did not populate project indexer".to_string(),
                        )?;
                    let reindex_sync = reindex_sync_temp
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .ok_or_else(|| {
                            "init_memory_system did not populate chunk sync service".to_string()
                        })?;
                    let cancel = cancel_temp
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .unwrap_or_default();
                    let data_dir_lock = lock_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                    let active = ActiveMemoryServices {
                        project_id,
                        store: store.clone(),
                        reindex,
                        reindex_sync,
                        cancel,
                        data_dir_lock,
                        decision_store: None,
                        task_tree: None,
                    };
                    *memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
                    store
                };

                let cancel = concerto_core::CancellationToken::new();
                let chunks = if query_text.is_empty() {
                    store.browse(&project_id_for_query, 200, cancel).await
                } else {
                    let chunk_type = match type_filter {
                        views::memory::MemoryEntryType::SlidingWindow => {
                            Some(ChunkType::SlidingWindow)
                        }
                        views::memory::MemoryEntryType::SessionSummary => {
                            Some(ChunkType::SessionSummary)
                        }
                        views::memory::MemoryEntryType::Fact => Some(ChunkType::Fact),
                        _ => None,
                    };
                    let filters = chunk_type
                        .map(|kind| vec![MemoryFilter::ChunkType(kind)])
                        .unwrap_or_default();
                    store
                        .retrieve(
                            &MemoryQuery {
                                text: query_text,
                                project_id: project_id_for_query.clone(),
                                namespace: MemoryNamespace::Project(project_id_for_query.clone()),
                                top_k: 200,
                                filters,
                            },
                            cancel.clone(),
                        )
                        .await
                }
                .map_err(|error| error.to_string())?;

                Ok(chunks
                    .into_iter()
                    .map(|chunk| {
                        let entry_type = match chunk.chunk_type {
                            ChunkType::SessionSummary => {
                                views::memory::MemoryEntryType::SessionSummary
                            }
                            ChunkType::Fact => views::memory::MemoryEntryType::Fact,
                            _ => views::memory::MemoryEntryType::SlidingWindow,
                        };
                        let source = chunk
                            .file_path
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "indexed memory".to_string());
                        let mut preview: String = chunk.content.chars().take(240).collect();
                        if chunk.content.chars().count() > 240 {
                            preview.push('…');
                        }
                        views::memory::MemoryRow {
                            id: chunk.id,
                            content_preview: preview,
                            source,
                            age: String::new(),
                            score: chunk.score as f32,
                            entry_type,
                            stale: chunk.stale,
                        }
                    })
                    .collect())
            },
            Message::MemoryEntriesLoaded,
        )
    }

    pub(super) fn delete_memory_entry(&self, id: String) -> iced::Task<Message> {
        let memory = self.memory_services.clone();
        let result_id = id.clone();
        iced::Task::perform(
            async move {
                let store = memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|m| m.store.clone())
                    .ok_or_else(|| "Memory is not initialized yet.".to_string())?;
                let cancel = concerto_core::CancellationToken::new();
                store.invalidate_chunk(&id, cancel).await.map_err(|error| error.to_string())
            },
            move |result| Message::MemoryEntryDeleted { id: result_id.clone(), result },
        )
    }

    /// Refresh the active project's Git status without blocking the UI thread.
    pub(super) fn load_git_summary(&self) -> iced::Task<Message> {
        let project_dir = self.project_dir.clone();
        iced::Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    concerto_tools::git::repository_summary(&project_dir).ok()
                })
                .await
                .unwrap_or(None)
            },
            Message::GitSummaryLoaded,
        )
    }

    /// Load the recent sessions for one project folder so the sidebar tree can
    /// list them. Best-effort: any failure yields an empty list rather than
    /// breaking the UI.
    pub(super) fn load_sessions_for_project(&self, project_dir: PathBuf) -> iced::Task<Message> {
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let provider_labels: std::collections::HashMap<String, String> = self
            .settings
            .providers
            .iter()
            .map(|provider| {
                let label = if provider.name == provider.provider {
                    provider.name.clone()
                } else {
                    format!("{} ({})", provider.name, provider.provider)
                };
                (provider.id.clone(), label)
            })
            .collect();
        iced::Task::perform(
            async move {
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return (project_dir, Vec::new()),
                        },
                    }
                };
                let utf8 = match camino::Utf8PathBuf::from_path_buf(project_dir.clone()) {
                    Ok(p) => p,
                    Err(_) => return (project_dir, Vec::new()),
                };
                match handler
                    .manager()
                    .list_project_sessions(&utf8, 50, CancellationToken::new())
                    .await
                {
                    Ok(sessions) => (
                        project_dir,
                        sessions
                            .into_iter()
                            .map(|s| views::chat::SessionRow {
                                session_id: s.id.to_string(),
                                created_at: s.created_at.to_string(),
                                message_count: s.message_count,
                                cost: s.total_cost_usd,
                                tokens_in: s.total_tokens_in,
                                tokens_out: s.total_tokens_out,
                                duration: String::new(),
                                provider: provider_labels
                                    .get(&s.provider)
                                    .cloned()
                                    .unwrap_or_else(|| s.provider.clone()),
                            })
                            .collect(),
                    ),
                    Err(_) => (project_dir, Vec::new()),
                }
            },
            |(path, sessions)| Message::ProjectSessionsLoaded { path, sessions },
        )
    }

    /// Rebuild the sidebar project→session tree from the project registry.
    /// Most-recent projects come first; the active project's node is expanded
    /// by default (its sessions are loaded by the caller). Session lists are
    /// reset to "not loaded" so they are lazily refetched on the next expand.
    pub(super) fn rebuild_project_tree(&mut self) {
        let registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
        let active = concerto_core::helpers::canonical_project_path(&self.project_dir);
        let mut tree: Vec<ProjectTreeNode> = registry
            .recent()
            .map(|path| {
                let expanded = concerto_core::helpers::canonical_project_path(path) == active;
                ProjectTreeNode {
                    path: path.to_path_buf(),
                    name: project_name(path),
                    expanded,
                    sessions: None,
                }
            })
            .collect();
        // The active project always has a node, even when it is not (yet) in
        // the registry (e.g. a manually-typed folder).
        if !tree
            .iter()
            .any(|node| concerto_core::helpers::canonical_project_path(&node.path) == active)
        {
            tree.push(ProjectTreeNode {
                path: self.project_dir.clone(),
                name: project_name(&self.project_dir),
                expanded: true,
                sessions: None,
            });
        }
        self.project_tree = tree;
    }

    /// Resume a previously picked session: make it active for the project and
    /// load its history (and durable typed transcript) so the chat shows the
    /// resumed conversation.
    pub(super) fn select_session(&self, session_id: String) -> iced::Task<Message> {
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let project_dir = self.project_dir.clone();
        iced::Task::perform(
            async move {
                let sid = match concerto_core::ids::Ulid::from_string(&session_id) {
                    Ok(s) => s,
                    Err(_) => return (session_id, Vec::new(), Vec::new(), Vec::new()),
                };
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return (session_id, Vec::new(), Vec::new(), Vec::new()),
                        },
                    }
                };
                // Make this session the active one so the next run continues it.
                let _ = handler.set_active_session(&project_dir, sid).await;
                // Seed the chat with its prior conversation. The durable typed
                // transcript (ADR-36) is canonical when present; `history` and
                // the local transcript.json remain the legacy fallback.
                let history = handler.load_history(sid).await.unwrap_or_default();
                let events = handler.load_events(sid).await.unwrap_or_default();
                let transcript = handler.load_transcript(sid).await.unwrap_or_default();
                (session_id, history, events, transcript)
            },
            |(session_id, history, events, transcript)| Message::SessionSelected {
                session_id,
                history,
                events,
                transcript,
            },
        )
    }

    /// Load the active session's persisted spend records for the Spend Log
    /// modal. Best-effort, mirroring [`Self::load_sessions_for_project`]: any
    /// failure (or missing session) yields an empty list rather than breaking
    /// the UI.
    pub(super) fn load_spend_log(&self) -> iced::Task<Message> {
        let Some(session_id) = self.active_session_id else {
            return iced::Task::none();
        };
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        iced::Task::perform(
            async move {
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return Vec::new(),
                        },
                    }
                };
                handler.list_spend_records(session_id).await.unwrap_or_default()
            },
            |records| Message::Chat(views::chat::Message::SpendLogsLoaded(records)),
        )
    }

    /// Load the read-only Coordinator 2.0 runtime snapshot for the chat
    /// Runtime modal's observability panels from the active session's
    /// persisted checkpoint.
    ///
    /// Fail-soft: a missing session, a store failure, or a checkpoint parse
    /// failure yields an empty snapshot (with a muted note) rather than an
    /// error. The read is outside the run lifecycle, so it carries a fresh
    /// cancellation token (the reader still honors it at the seam) — the
    /// run's own token must not permanently disable the panel after a cancel.
    pub(super) fn load_studio_runtime(&self) -> iced::Task<Message> {
        let session_id = self.active_session_id;
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        iced::Task::perform(
            async move {
                let Some(session_id) = session_id else {
                    // No active session yet: empty panels, no error.
                    return (None, Box::new(StudioRuntimeSnapshot::default()));
                };
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(handler) => handler,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(handler) => Arc::new(handler),
                            Err(error) => {
                                return (
                                    Some(session_id),
                                    Box::new(StudioRuntimeSnapshot::unavailable(format!(
                                        "runtime state unavailable: {error}"
                                    ))),
                                );
                            }
                        },
                    }
                };
                let reader = CheckpointStudioRuntimeReader::new(handler.manager().store());
                let snapshot = match reader.load(session_id, CancellationToken::new()).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => StudioRuntimeSnapshot::unavailable(format!(
                        "runtime state unavailable: {error}"
                    )),
                };
                (Some(session_id), Box::new(snapshot))
            },
            |(session_id, snapshot)| Message::StudioRuntimeLoaded(session_id, snapshot),
        )
    }
}
