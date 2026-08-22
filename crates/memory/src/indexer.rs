//! Project indexing pipeline — line/sliding-window chunks + fastembed.
//!
//! Based on §3.1 Spike B findings: swiftide does NOT support
//! `CancellationToken`, so we build a custom pipeline using
//! simple language-aware line chunking and `fastembed` (local embeddings)
//! directly. This gives us fine-grained cancellation control at every
//! stage.

use std::path::Path;
use std::sync::Arc;

use camino::Utf8PathBuf;

use concerto_core::error::MemoryError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::memory::{ChunkType, EmbeddingRecord, ProjectId};
use concerto_core::CancellationToken;

use crate::embedder::EmbeddingGenerator;
use crate::embedder_health::EmbedderHealth;
use crate::ignore_rules::IndexIgnoreMatcher;
use crate::sync::ChunkSyncService;

/// Configuration for a project indexing run.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    pub project_dir: Utf8PathBuf,
    pub exclude_patterns: Vec<String>,
    pub file_size_limit_mb: u64,
    pub ignore_file: Option<Utf8PathBuf>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            project_dir: Utf8PathBuf::from("."),
            exclude_patterns: vec![
                ".git/".into(),
                ".hg/".into(),
                ".svn/".into(),
                "target/".into(),
                "node_modules/".into(),
                "dist/".into(),
                "build/".into(),
                ".next/".into(),
                "coverage/".into(),
                "__pycache__/".into(),
                ".cache/".into(),
                ".mypy_cache/".into(),
                ".pytest_cache/".into(),
                ".ruff_cache/".into(),
                ".venv/".into(),
                "venv/".into(),
                "vendor/".into(),
            ],
            file_size_limit_mb: 1,
            ignore_file: None,
        }
    }
}

/// Language detection result.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Other,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "rs" => Language::Rust,
            "ts" | "tsx" => Language::TypeScript,
            "py" => Language::Python,
            "go" => Language::Go,
            _ => Language::Other,
        }
    }

    /// Returns the recognized language label, if available.
    pub fn grammar_name(&self) -> Option<&'static str> {
        match self {
            Language::Rust => Some("rust"),
            Language::TypeScript => Some("typescript"),
            Language::Python => Some("python"),
            Language::Go => Some("go"),
            Language::Other => None,
        }
    }
}

/// The project indexer — walks a directory, chunks recognized text by lines,
/// generates embeddings, and stores the results.
pub struct ProjectIndexer {
    embedder: Arc<dyn EmbeddingGenerator>,
    bus: EventBus,
    project_id: ProjectId,
}

impl ProjectIndexer {
    pub fn new(
        embedder: Arc<dyn EmbeddingGenerator>,
        bus: EventBus,
        project_id: ProjectId,
    ) -> Self {
        Self { embedder, bus, project_id }
    }

    /// Index a project directory.
    ///
    /// Returns the number of chunks created. Respects `CancellationToken`
    /// so long indexing runs can be interrupted.
    pub async fn index(
        &self,
        config: &IndexConfig,
        cancel: CancellationToken,
    ) -> Result<Vec<EmbeddingRecord>, MemoryError> {
        let ignore = IndexIgnoreMatcher::new(config)?;
        self.index_with_matcher(config, &ignore, cancel).await
    }

    pub(crate) async fn index_with_matcher(
        &self,
        config: &IndexConfig,
        ignore: &IndexIgnoreMatcher,
        cancel: CancellationToken,
    ) -> Result<Vec<EmbeddingRecord>, MemoryError> {
        let start_time = std::time::Instant::now();
        let mut records = Vec::new();
        let project_id = self.project_id.clone();
        let health = EmbedderHealth::for_project(&project_id);

        // Global event: intentionally unscoped (background project indexer,
        // no session context).
        let _ = self.bus.publish_raw(EventKind::IndexingStarted {
            project_id: project_id.to_string(),
            file_count: 0,
        });

        // Recursive directory walk using WalkDir for full project indexing.
        // Use filter_entry to skip excluded patterns at the directory level.
        let walker = walkdir::WalkDir::new(&config.project_dir)
            .into_iter()
            .filter_entry(|entry| !ignore.is_hard_ignored(entry.path(), entry.file_type().is_dir()))
            .filter_map(|e| e.ok())
            .filter(|entry| entry.file_type().is_file() && !ignore.is_ignored(entry.path(), false));
        let entry_list: Vec<_> = walker.collect();
        let total_files = entry_list.len();

        for (idx, entry) in entry_list.into_iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }

            let path = entry.path();
            let file_path = match ignore.relative_path(path) {
                Ok(fp) => fp,
                Err(_) => return Ok(Vec::new()),
            };

            let file_size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if file_size > config.file_size_limit_mb * 1024 * 1024 {
                continue;
            }

            if let Ok(content) = std::fs::read_to_string(path) {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let language = Language::from_extension(ext);
                let chunk_type = if language == Language::Other {
                    ChunkType::SlidingWindow
                } else {
                    ChunkType::Function
                };
                let chunks: Vec<String> = if language == Language::Other {
                    sliding_window_chunks(&content, 512, 64)
                } else {
                    // Try tree-sitter AST chunking first; fall back to line-by-line.
                    crate::treesitter::treesitter_chunks(&content, language)
                        .unwrap_or_else(|| content.lines().map(|s| s.to_string()).collect())
                };

                for (ordinal, located) in locate_chunks(&content, chunks).into_iter().enumerate() {
                    let chunk = located.content;
                    let chunk_hash = blake3::hash(chunk.as_bytes()).to_string();
                    let id = stable_chunk_id(
                        &project_id,
                        &file_path,
                        ordinal,
                        located.start_line,
                        &chunk_hash,
                    );

                    // Embed best‑effort. On failure (model not downloaded /
                    // offline / provider error) we record the chunk WITHOUT a
                    // usable vector (the empty-vector sentinel `[]`, which the
                    // similarity search already excludes) so full‑text search
                    // still works instead of aborting the indexing run — and
                    // never store a zero‑vector similarity row (ADR‑39).
                    //
                    // Health: once the embedder is broken for this project we
                    // pause embedding attempts until the current backoff window
                    // expires (sustained outages re-enter, capped at 120s) — FTS
                    // chunk recording continues; a later success clears the
                    // broken state.
                    let now = std::time::Instant::now();
                    let embedding: Vec<f32> = if health.is_broken(now) {
                        Vec::new()
                    } else {
                        match self.embedder.embed(&chunk).await {
                            Ok(v) => {
                                health.record_success();
                                v
                            }
                            Err(e) => {
                                if let Some(delay) = health.record_failure(now) {
                                    // Exactly one event per transition into the
                                    // broken window, not per chunk.
                                    let _ = self.bus.publish_raw(EventKind::EmbedderDegraded {
                                        project_id: project_id.to_string(),
                                        reason: format!(
                                            "embedding failed: {e}; pausing for {delay:?}"
                                        ),
                                    });
                                }
                                tracing::warn!(
                                    "embedding failed for {}: {e}; storing chunk without \
                                     vector (FTS-only)",
                                    path.display()
                                );
                                Vec::new()
                            }
                        }
                    };

                    let record = EmbeddingRecord {
                        id,
                        project_id: project_id.clone(),
                        chunk_hash,
                        content: chunk,
                        file_path: file_path.clone(),
                        start_line: located.start_line,
                        end_line: located.end_line,
                        chunk_type,
                        vector: embedding,
                        model_id: self.embedder.model_id().to_string(),
                        model_version: self.embedder.model_version().to_string(),
                        stale: false,
                        created_at: time::OffsetDateTime::now_utc(),
                    };
                    records.push(record);
                }

                // Global event: intentionally unscoped (background project
                // indexer, no session context).
                let _ = self.bus.publish_raw(EventKind::IndexingProgress {
                    project_id: project_id.to_string(),
                    files_processed: idx + 1,
                    files_total: total_files,
                });
            }
        }

        let duration = start_time.elapsed().as_millis();
        // Global event: intentionally unscoped (background project indexer,
        // no session context).
        let _ = self.bus.publish_raw(EventKind::IndexingCompleted {
            project_id: project_id.to_string(),
            chunk_count: records.len(),
            duration_ms: duration as u64,
        });

        Ok(records)
    }

    /// Index a single file and persist its chunks through the chunk sync
    /// service (vector store + FTS). Used for re-indexing changed files.
    ///
    /// Skips directories, unreadable/non-UTF8 files, and files exceeding the
    /// configured size limit. Embedding failures fall back to FTS-only (a
    /// zero vector) rather than failing the file.
    pub async fn index_file(
        &self,
        path: &Path,
        sync: &ChunkSyncService,
        cancel: CancellationToken,
    ) -> Result<u64, MemoryError> {
        let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
                .unwrap_or_else(|path| Utf8PathBuf::from(path.to_string_lossy().as_ref())),
            ..IndexConfig::default()
        };
        self.index_file_with_config(path, &config, sync, cancel).await
    }

    /// Index a single file using the same root and exclusion policy as a full
    /// project scan. Deleted, newly ignored, oversized, or unreadable files
    /// have their previous chunks removed.
    pub async fn index_file_with_config(
        &self,
        path: &Path,
        config: &IndexConfig,
        sync: &ChunkSyncService,
        cancel: CancellationToken,
    ) -> Result<u64, MemoryError> {
        let ignore = IndexIgnoreMatcher::new(config)?;
        self.index_file_with_matcher(path, config, &ignore, sync, cancel).await
    }

    pub(crate) async fn index_file_with_matcher(
        &self,
        path: &Path,
        config: &IndexConfig,
        ignore: &IndexIgnoreMatcher,
        sync: &ChunkSyncService,
        cancel: CancellationToken,
    ) -> Result<u64, MemoryError> {
        if cancel.is_cancelled() {
            return Ok(0);
        }

        let file_path = match ignore.relative_path(path) {
            Ok(fp) => fp,
            Err(_) => return Ok(0),
        };
        if ignore.is_ignored(path, false) {
            sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
            return Ok(0);
        }

        let metadata = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
                return Ok(0);
            }
            Err(error) => {
                return Err(MemoryError::IndexingFailed {
                    path: file_path,
                    reason: format!("failed to read file metadata: {error}"),
                });
            }
        };
        if metadata.file_type().is_symlink() {
            sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
            return Ok(0);
        }
        if metadata.is_dir() {
            return Ok(0);
        }
        let limit = config.file_size_limit_mb * 1024 * 1024;
        if metadata.len() > limit {
            tracing::debug!("skip index_file (too large) {}", path.display());
            sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
            return Ok(0);
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                tracing::debug!("skip index_file (non-utf8) {}", path.display());
                sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
                return Ok(0);
            }
        };

        if content.trim().is_empty() {
            tracing::debug!("skip index_file (whitespace-only) {}", path.display());
            sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;
            return Ok(0);
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language = Language::from_extension(ext);
        let chunk_type = if language == Language::Other {
            ChunkType::SlidingWindow
        } else {
            ChunkType::Function
        };
        let chunks: Vec<String> = if language == Language::Other {
            sliding_window_chunks(&content, 512, 64)
        } else {
            // Try tree-sitter AST chunking first; fall back to line-by-line.
            crate::treesitter::treesitter_chunks(&content, language)
                .unwrap_or_else(|| content.lines().map(|s| s.to_string()).collect())
        };

        // Remove any previously-indexed chunks for this file so a changed
        // file does not leave orphaned stale chunks behind (idempotent
        // re-index).
        sync.delete_by_file_path(&self.project_id, &file_path, cancel.clone()).await?;

        let mut count = 0u64;
        let health = EmbedderHealth::for_project(&self.project_id);
        for (ordinal, located) in locate_chunks(&content, chunks).into_iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let chunk = located.content;
            let chunk_hash = blake3::hash(chunk.as_bytes()).to_string();
            let id = stable_chunk_id(
                &self.project_id,
                &file_path,
                ordinal,
                located.start_line,
                &chunk_hash,
            );
            // FTS-only sentinel handling — see `index_with_matcher` for the
            // full comment. Never write a zero-vector similarity row (ADR-39).
            let now = std::time::Instant::now();
            let embedding: Vec<f32> = if health.is_broken(now) {
                Vec::new()
            } else {
                match self.embedder.embed(&chunk).await {
                    Ok(v) => {
                        health.record_success();
                        v
                    }
                    Err(e) => {
                        if let Some(delay) = health.record_failure(now) {
                            let _ = self.bus.publish_raw(EventKind::EmbedderDegraded {
                                project_id: self.project_id.to_string(),
                                reason: format!("embedding failed: {e}; pausing for {delay:?}"),
                            });
                        }
                        tracing::warn!(
                            "embedding failed for {}: {e}; storing chunk without vector \
                             (FTS-only)",
                            path.display()
                        );
                        Vec::new()
                    }
                }
            };
            let record = EmbeddingRecord {
                id,
                project_id: self.project_id.clone(),
                chunk_hash,
                content: chunk,
                file_path: file_path.clone(),
                start_line: located.start_line,
                end_line: located.end_line,
                chunk_type,
                vector: embedding,
                model_id: self.embedder.model_id().to_string(),
                model_version: self.embedder.model_version().to_string(),
                stale: false,
                created_at: time::OffsetDateTime::now_utc(),
            };
            sync.store(&record, cancel.clone()).await?;
            count += 1;
        }
        Ok(count)
    }
}

#[derive(Debug)]
struct LocatedChunk {
    content: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

fn locate_chunks(content: &str, chunks: Vec<String>) -> Vec<LocatedChunk> {
    let mut search_from = 0usize;
    chunks
        .into_iter()
        .map(|chunk| {
            let found = content[search_from..]
                .find(&chunk)
                .map(|offset| search_from + offset)
                .or_else(|| content.find(&chunk));
            let (start_line, end_line) = found.map_or((None, None), |start| {
                let start_line = content[..start].bytes().filter(|byte| *byte == b'\n').count() + 1;
                let line_count = chunk.lines().count().max(1);
                let end_line = start_line.saturating_add(line_count).saturating_sub(1);
                search_from = content[start..]
                    .char_indices()
                    .nth(1)
                    .map_or(content.len(), |(offset, _)| start + offset);
                (u32::try_from(start_line).ok(), u32::try_from(end_line).ok())
            });
            LocatedChunk { content: chunk, start_line, end_line }
        })
        .collect()
}

fn stable_chunk_id(
    project_id: &ProjectId,
    file_path: &Utf8PathBuf,
    ordinal: usize,
    start_line: Option<u32>,
    chunk_hash: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    let ordinal = ordinal.to_string();
    let start_line = start_line.unwrap_or_default().to_string();
    for value in [
        project_id.0.as_str(),
        file_path.as_str(),
        ordinal.as_str(),
        start_line.as_str(),
        chunk_hash,
    ] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Sliding-window chunker for non-code files.
///
/// Splits text into chunks of `window_size` tokens with `overlap` token overlap.
pub fn sliding_window_chunks(text: &str, window_size: usize, overlap: usize) -> Vec<String> {
    // Rough token count: ~4 chars per token
    let chars_per_token = 4;
    let window_chars = window_size.saturating_mul(chars_per_token).max(1);
    let overlap_chars = overlap.saturating_mul(chars_per_token).min(window_chars.saturating_sub(1));
    let chars: Vec<char> = text.chars().collect();

    if chars.len() <= window_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + window_chars).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        if end >= chars.len() {
            break;
        }
        start += window_chars - overlap_chars;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts::FullTextStore;
    use crate::sync::ChunkSyncService;
    use crate::testing::{InMemoryFullTextStore, InMemoryVectorStore};
    use crate::vector_store::VectorStore;
    use concerto_core::event::EventBus;
    use concerto_core::memory::ProjectId;
    use tempfile::tempdir;

    #[test]
    fn sliding_window_small_text() {
        let text = "Hello, world!";
        let chunks = sliding_window_chunks(text, 512, 64);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn sliding_window_splits_large_text() {
        let text = "A".repeat(10000);
        let chunks = sliding_window_chunks(&text, 512, 64);
        // 10000 / (512*4) ≈ 5 chunks
        assert!(chunks.len() >= 4);
        assert!(chunks.len() <= 7);
    }

    #[test]
    fn sliding_window_preserves_unicode_boundaries() {
        let text = "🎨 FlexiAuto │ templates ├─ reusable UI\n".repeat(300);
        let chunks = sliding_window_chunks(&text, 64, 8);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
        assert!(chunks.iter().any(|chunk| chunk.contains('│')));
    }

    #[test]
    fn sliding_window_clamps_oversized_overlap() {
        let text = "abcdef".repeat(20);
        let chunks = sliding_window_chunks(&text, 2, 4);
        assert!(!chunks.is_empty());
        assert_eq!(chunks.last().and_then(|chunk| chunk.chars().last()), Some('f'));
    }

    #[test]
    fn language_detection() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("tsx"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("md"), Language::Other);
    }

    #[tokio::test]
    async fn index_empty_project() {
        let temp_dir = tempfile::tempdir().unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let bus = EventBus::default();
        let indexer =
            ProjectIndexer::new(embedder, bus, concerto_core::memory::ProjectId("test".into()));
        let config = IndexConfig {
            project_dir: camino::Utf8PathBuf::try_from(temp_dir.path().to_owned()).unwrap(),
            ..IndexConfig::default()
        };
        let cancel = CancellationToken::new();

        let records = indexer.index(&config, cancel).await.unwrap();
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn full_index_honours_ignore_files_and_sensitive_defaults() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("generated")).unwrap();
        std::fs::create_dir_all(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(dir.path().join(".concertoignore"), "private/\n").unwrap();
        std::fs::write(dir.path().join(".env"), "TOKEN=do-not-index\n").unwrap();
        std::fs::write(dir.path().join("generated/code.rs"), "fn generated() {}\n").unwrap();
        std::fs::write(dir.path().join("private/notes.md"), "private notes\n").unwrap();
        std::fs::write(dir.path().join("visible.md"), "visible project context\n").unwrap();

        let project_id = ProjectId("proj".into());
        let indexer = ProjectIndexer::new(
            Arc::new(crate::testing::MockEmbeddingGenerator::new(8)),
            EventBus::default(),
            project_id.clone(),
        );
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
            ..IndexConfig::default()
        };

        let records = indexer.index(&config, CancellationToken::new()).await.unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().all(|record| record.project_id == project_id));
        assert!(records.iter().all(|record| record.file_path == "visible.md"));
        assert!(records.iter().all(|record| !record.content.contains("do-not-index")));
    }

    #[tokio::test]
    async fn identical_content_in_different_files_gets_distinct_ids() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("one.txt"), "same content").unwrap();
        std::fs::write(dir.path().join("two.txt"), "same content").unwrap();
        let indexer = ProjectIndexer::new(
            Arc::new(crate::testing::MockEmbeddingGenerator::new(8)),
            EventBus::default(),
            ProjectId("proj".into()),
        );
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
            ..IndexConfig::default()
        };

        let records = indexer.index(&config, CancellationToken::new()).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_ne!(records[0].id, records[1].id);
        assert_ne!(records[0].file_path, records[1].file_path);
        assert_eq!(records[0].chunk_hash, records[1].chunk_hash);
    }

    #[tokio::test]
    async fn index_file_stores_chunks_in_vector_and_fts() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {\n    println!(\"hello world\");\n}\n").unwrap();

        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let bus = EventBus::default();
        let project_id = ProjectId("proj".into());
        let indexer = ProjectIndexer::new(embedder, bus, project_id.clone());

        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let count =
            indexer.index_file(file.as_path(), &sync, CancellationToken::new()).await.unwrap();
        assert!(count > 0, "expected at least one chunk to be indexed");

        // Vector store should contain the embedded chunks...
        let v_results =
            vs.search(&project_id, &vec![0.1; 384], 5, CancellationToken::new()).await.unwrap();
        assert!(!v_results.is_empty());

        // ...and FTS should be searchable by content.
        let f_results =
            fts.search("println", &project_id, 5, CancellationToken::new()).await.unwrap();
        assert!(!f_results.is_empty());
    }

    #[tokio::test]
    async fn index_file_skips_directories() {
        let dir = tempdir().unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let bus = EventBus::default();
        let indexer = ProjectIndexer::new(embedder, bus, ProjectId("proj".into()));
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let count = indexer.index_file(dir.path(), &sync, CancellationToken::new()).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn deleting_a_file_purges_its_indexed_chunks() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("obsolete.txt");
        std::fs::write(&file, "obsolete searchable content").unwrap();
        let project_id = ProjectId("proj".into());
        let indexer = ProjectIndexer::new(
            Arc::new(crate::testing::MockEmbeddingGenerator::new(8)),
            EventBus::default(),
            project_id.clone(),
        );
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vector_store, fts_store.clone());
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
            ..IndexConfig::default()
        };

        indexer
            .index_file_with_config(&file, &config, &sync, CancellationToken::new())
            .await
            .unwrap();
        assert!(!fts_store
            .search("obsolete", &project_id, 5, CancellationToken::new())
            .await
            .unwrap()
            .is_empty());

        std::fs::remove_file(&file).unwrap();
        indexer
            .index_file_with_config(&file, &config, &sync, CancellationToken::new())
            .await
            .unwrap();
        assert!(fts_store
            .search("obsolete", &project_id, 5, CancellationToken::new())
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sliding_window_single_char() {
        let chunks = sliding_window_chunks("a", 512, 64);
        assert_eq!(chunks, vec!["a"]);
    }

    #[test]
    fn sliding_window_exact_fit() {
        let text = "x".repeat(512);
        let chunks = sliding_window_chunks(&text, 512, 64);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn language_detection_unknown_extensions() {
        assert_eq!(Language::from_extension("txt"), Language::Other);
        assert_eq!(Language::from_extension("csv"), Language::Other);
        assert_eq!(Language::from_extension("html"), Language::Other);
    }

    #[test]
    fn language_detection_case_sensitive() {
        assert_eq!(Language::from_extension("RS"), Language::Other);
        assert_eq!(Language::from_extension("TS"), Language::Other);
    }

    #[test]
    fn index_config_default_file_size_limit() {
        let config = IndexConfig::default();
        assert!(config.file_size_limit_mb > 0);
    }

    #[test]
    fn indexing_a_directory_outside_project_returns_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let indexer = ProjectIndexer::new(
            embedder,
            EventBus::default(),
            concerto_core::memory::ProjectId("test".into()),
        );
        let outside_path = temp_dir.path().join("../outside.txt");
        let sync_service = crate::sync::ChunkSyncService::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
        );
        let result = indexer.index_file(&outside_path, &sync_service, CancellationToken::new());
        // Nonexistent file should return 0 indexed chunks
        let rt = tokio::runtime::Runtime::new().unwrap();
        let count = rt.block_on(result).unwrap();
        assert_eq!(count, 0);
    }

    /// Indexing an empty file produces zero chunks.
    #[test]
    fn index_empty_file_produces_zero_chunks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("empty.rs");
        std::fs::write(&file_path, "").unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let indexer = ProjectIndexer::new(embedder, EventBus::default(), ProjectId("test".into()));
        let sync_service = ChunkSyncService::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
        );
        let result = indexer.index_file(&file_path, &sync_service, CancellationToken::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let count = rt.block_on(result).unwrap();
        assert_eq!(count, 0, "empty file should produce zero indexed chunks");
    }

    /// Indexing a file with only whitespace produces zero chunks.
    #[test]
    fn index_whitespace_file_produces_zero_chunks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("whitespace.rs");
        std::fs::write(&file_path, "   \n\n  \t\n").unwrap();
        let embedder = Arc::new(crate::testing::MockEmbeddingGenerator::new(384));
        let indexer = ProjectIndexer::new(embedder, EventBus::default(), ProjectId("test".into()));
        let sync_service = ChunkSyncService::new(
            Arc::new(InMemoryVectorStore::new()),
            Arc::new(InMemoryFullTextStore::new()),
        );
        let result = indexer.index_file(&file_path, &sync_service, CancellationToken::new());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let count = rt.block_on(result).unwrap();
        assert_eq!(count, 0, "whitespace-only file should produce zero indexed chunks");
    }

    /// Embedder that always fails — used to exercise the degraded path.
    struct FailingEmbedder;

    #[async_trait::async_trait]
    impl crate::embedder::EmbeddingGenerator for FailingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoryError> {
            Err(MemoryError::EmbeddingFailed { reason: "offline".into() })
        }
        fn model_id(&self) -> &str {
            "failing"
        }
        fn model_version(&self) -> &str {
            "1"
        }
        fn dims(&self) -> usize {
            8
        }
    }

    #[tokio::test]
    async fn embed_failure_writes_no_zero_vector_rows_and_breaks_backoff() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn c() {}\nfn d() {}\n").unwrap();

        let bus = EventBus::default();
        let indexer = ProjectIndexer::new(
            Arc::new(FailingEmbedder),
            bus.clone(),
            ProjectId("failing-proj".into()),
        );
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
            ..IndexConfig::default()
        };

        let records = indexer.index(&config, CancellationToken::new()).await.unwrap();
        assert!(!records.is_empty());
        // ADR-39: never a zero-vector placeholder — the record carries an EMPTY
        // (absent) vector so it never participates in vector similarity.
        for record in &records {
            assert!(
                record.vector.is_empty(),
                "embed failure must store an empty (absent) vector, got {} elements",
                record.vector.len()
            );
        }
        // Health is broken for the project after the failing run.
        let health =
            crate::embedder_health::EmbedderHealth::for_project(&ProjectId("failing-proj".into()));
        assert!(health.is_broken(std::time::Instant::now()));
    }

    #[tokio::test]
    async fn embed_failure_emits_exactly_one_degraded_event() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn c() {}\n").unwrap();

        let bus = EventBus::default();
        let mut rx = bus.subscribe_durable();
        let indexer = ProjectIndexer::new(
            Arc::new(FailingEmbedder),
            bus,
            ProjectId("failing-proj-event".into()),
        );
        let config = IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap(),
            ..IndexConfig::default()
        };
        let records = indexer.index(&config, CancellationToken::new()).await.unwrap();
        assert!(!records.is_empty());

        let mut degraded = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event.kind, EventKind::EmbedderDegraded { .. }) {
                degraded += 1;
            }
        }
        assert_eq!(degraded, 1, "exactly one broken-transition event, got {degraded}");
    }
}
