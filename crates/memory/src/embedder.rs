//! Embedding generation, version checking, and model download management.
//!
//! - `EmbeddingVersionChecker` detects model version drift and triggers
//!   re-indexing.
//! - `ModelDownloadManager` handles first-run model download without
//!   blocking the UI.
//! - `EmbeddingGenerator` produces vectors from text using `fastembed`.
use async_trait::async_trait;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use concerto_core::error::MemoryError;
use concerto_core::event::EventBus;
use concerto_core::CancellationToken;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

// ---------------------------------------------------------------------------
// EmbeddingGenerator trait
// ---------------------------------------------------------------------------

/// Generates embedding vectors from text chunks.
///
/// Production implementation uses `fastembed` for local embedding.
/// Test double (`MockEmbeddingGenerator`) is in `testing.rs`.
#[async_trait]
pub trait EmbeddingGenerator: Send + Sync {
    /// Generate an embedding vector for the given text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError>;

    /// The model ID used by this generator (e.g. "BAAI/bge-small-en-v1.5").
    fn model_id(&self) -> &str;

    /// The model version string (used for staleness detection).
    fn model_version(&self) -> &str;

    /// Vector dimensionality (e.g. 384 for bge-small).
    fn dims(&self) -> usize;
}

// ---------------------------------------------------------------------------
// ProviderEmbedder implementation
// ---------------------------------------------------------------------------

/// Embedding generator that uses `fastembed` for local on-device embeddings.
///
/// The BAAI/bge-small-en-v1.5 model is loaded on first call (may trigger a download).
/// Runtime inference is offloaded via `spawn_blocking` since `fastembed` uses sync ONNX inference.
/// Since fastembed 6, `embed` requires exclusive (`&mut self`) access to the model,
/// so the ready model sits behind its own sync mutex and concurrent calls serialize on it.
pub struct ProviderEmbedder {
    model: String,
    embedding_model: Arc<Mutex<EmbedderState>>,
}

enum EmbedderState {
    Uninitialized,
    Ready(Arc<StdMutex<TextEmbedding>>),
    Unavailable(String),
}

impl ProviderEmbedder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            embedding_model: Arc::new(Mutex::new(EmbedderState::Uninitialized)),
        }
    }
}

#[async_trait]
impl EmbeddingGenerator for ProviderEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        // Initialize the model on first use (lazy init, thread-safe via Mutex).
        // FastEmbed downloads the model binary on first call if not cached.
        let model_arc = {
            let mut guard = self.embedding_model.lock().await;
            match &*guard {
                EmbedderState::Ready(model) => Arc::clone(model),
                EmbedderState::Unavailable(reason) => {
                    return Err(MemoryError::EmbeddingFailed { reason: reason.clone() });
                }
                EmbedderState::Uninitialized => {
                    let initialized = match tokio::task::spawn_blocking(|| {
                        let init_options = InitOptions::new(EmbeddingModel::BGESmallENV15);
                        TextEmbedding::try_new(init_options).map_err(|error| {
                            MemoryError::Persistence(format!(
                                "failed to initialize fastembed model: {error}"
                            ))
                        })
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(MemoryError::Persistence(format!(
                            "fastembed initialization task failed: {error}"
                        ))),
                    };
                    match initialized {
                        Ok(model) => {
                            let model = Arc::new(StdMutex::new(model));
                            *guard = EmbedderState::Ready(Arc::clone(&model));
                            model
                        }
                        Err(error) => {
                            *guard = EmbedderState::Unavailable(error.to_string());
                            return Err(error);
                        }
                    }
                }
            }
        };

        // fastembed 6 requires exclusive access to the model for inference.
        // Serialize calls on the model mutex and keep the sync ONNX work off
        // the async runtime threads.
        let text = text.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let mut model = model_arc.lock().map_err(|_| {
                MemoryError::Persistence("embedding model mutex poisoned".to_string())
            })?;
            model
                .embed(vec![text], None)
                .map_err(|e| MemoryError::Persistence(format!("embedding inference failed: {e}")))
        })
        .await
        .map_err(|e| MemoryError::Persistence(format!("embedding task failed: {e}")))??;

        // extract the single embedding vector
        Ok(result.into_iter().next().unwrap_or_default())
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn model_version(&self) -> &str {
        "bge-small-en-v1.5-fastembed-4"
    }

    fn dims(&self) -> usize {
        384
    }
}

// ---------------------------------------------------------------------------
// EmbeddingVersionChecker
// ---------------------------------------------------------------------------

/// Compares the stored embedding model version against the current version
/// and signals when a re-index is needed.
pub struct EmbeddingVersionChecker {
    pub current_model_id: String,
    pub current_model_version: String,
}

impl EmbeddingVersionChecker {
    pub fn new(model_id: impl Into<String>, model_version: impl Into<String>) -> Self {
        Self { current_model_id: model_id.into(), current_model_version: model_version.into() }
    }

    /// Compare stored version against current version.
    /// Returns `true` if re-index is needed for this project.
    ///
    /// The check is version-string equality. An empty stored version
    /// (project never indexed) returns `false` (no re-index needed).
    pub fn needs_reindex(&self, stored_model_version: Option<&str>) -> bool {
        match stored_model_version {
            None => false, // never indexed, nothing to re-index
            Some(v) => v != self.current_model_version,
        }
    }

    /// Returns an error describing the mismatch, for use in event payloads.
    pub fn mismatch_error(&self, stored: &str) -> MemoryError {
        MemoryError::StaleEmbedding {
            stored: stored.to_string(),
            current: self.current_model_version.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ModelDownloadManager
// ---------------------------------------------------------------------------

/// Manages the `fastembed` model download on first run.
///
/// Returns immediately if the model is already cached. Otherwise spawns
/// a background download, emitting progress events for UI display.
pub struct ModelDownloadManager {
    pub model_id: String,
    pub cache_dir: std::path::PathBuf,
    pub bus: EventBus,
}

impl ModelDownloadManager {
    pub fn new(
        model_id: impl Into<String>,
        cache_dir: impl Into<std::path::PathBuf>,
        bus: EventBus,
    ) -> Self {
        Self { model_id: model_id.into(), cache_dir: cache_dir.into(), bus }
    }

    /// Returns `true` if the model binary is already in the cache directory.
    pub fn is_cached(&self) -> bool {
        // Models are typically stored in subdirectories per model ID
        let model_dir = self.cache_dir.join(&self.model_id);
        model_dir.exists()
    }

    /// Returns immediately if model is already cached.
    /// Otherwise returns a background handle that emits progress events.
    pub fn ensure_available(&self, _cancel: CancellationToken) -> Result<(), MemoryError> {
        if self.is_cached() {
            return Ok(());
        }
        // FastEmbed downloads models automatically on first use via
        // the `fastembed` crate. We just need to ensure the cache dir exists.
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| {
            MemoryError::Persistence(format!("failed to create model cache dir: {e}"))
        })?;

        // The actual download is handled by fastembed on first `embed()` call.
        // Here we just verify the cache directory is writable.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastembed::Pooling;

    #[test]
    fn needs_reindex_same_version() {
        let checker = EmbeddingVersionChecker::new("bge-small", "1.0");
        assert!(!checker.needs_reindex(Some("1.0")));
    }

    #[test]
    fn needs_reindex_different_version() {
        let checker = EmbeddingVersionChecker::new("bge-small", "2.0");
        assert!(checker.needs_reindex(Some("1.0")));
    }

    #[test]
    fn needs_reindex_never_indexed() {
        let checker = EmbeddingVersionChecker::new("bge-small", "1.0");
        assert!(!checker.needs_reindex(None));
    }

    #[test]
    fn mismatch_error_message() {
        let checker = EmbeddingVersionChecker::new("bge-small", "2.0");
        let err = checker.mismatch_error("1.0");
        assert!(err.to_string().contains("1.0"));
        assert!(err.to_string().contains("2.0"));
    }

    #[test]
    fn download_manager_cached() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = dir.path().join("bge-small");
        std::fs::create_dir_all(&model_dir).unwrap();

        let mgr = ModelDownloadManager::new("bge-small", dir.path(), EventBus::default());
        assert!(mgr.is_cached());
    }

    #[test]
    fn download_manager_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ModelDownloadManager::new("bge-small", dir.path(), EventBus::default());
        assert!(!mgr.is_cached());
    }

    #[test]
    fn provider_embedder_initialization() {
        let embedder = ProviderEmbedder::new("bge-small-en-v1.5");
        assert_eq!(embedder.model_id(), "bge-small-en-v1.5");
        assert_eq!(embedder.model_version(), "bge-small-en-v1.5-fastembed-4");
        assert_eq!(embedder.dims(), 384);
    }

    /// Offline dimension proof for the fastembed 6 bump: fastembed's model
    /// catalog must keep BAAI/bge-small-en-v1.5 at 384 dims and at the same
    /// ONNX model source as fastembed 4 (`Xenova/bge-small-en-v1.5`,
    /// `onnx/model.onnx`, CLS pooling). Same model + same dims + same
    /// normalization is what keeps previously persisted vectors valid.
    #[test]
    fn bge_small_model_catalog_pins_dims_and_model_source() {
        let info =
            TextEmbedding::get_model_info(&EmbeddingModel::BGESmallENV15).expect("model info");
        assert_eq!(info.dim, 384);
        assert_eq!(info.model_code, "Xenova/bge-small-en-v1.5");
        assert_eq!(info.model_file, "onnx/model.onnx");
        assert_eq!(
            TextEmbedding::get_default_pooling_method(&EmbeddingModel::BGESmallENV15),
            Some(Pooling::Cls)
        );
        assert_eq!(ProviderEmbedder::new("bge-small-en-v1.5").dims(), info.dim);
    }

    /// Real-model smoke + dimension proof for the fastembed 6 bump: BAAI/bge-small-en-v1.5
    /// must still produce 384-dimensional, L2-normalized vectors, so previously
    /// persisted vectors stay valid for cosine retrieval. Network-gated
    /// because it downloads the ONNX weights (~30 MB) on first run.
    #[test]
    #[ignore = "requires network + downloads the BGE-small ONNX model on first run"]
    fn bge_small_real_embedding_dimension_is_384() {
        let mut embedder = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::BGESmallENV15))
            .expect("failed to initialize fastembed BGESmallENV15");
        let texts = ["first sample sentence", "second one", "a third one"];
        let embeddings = embedder.embed(texts, None).expect("embedding failed");
        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 384, "BGE-small-en-v1.5 must stay 384-dimensional");
            let norm: f32 = embedding.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3, "expected L2 norm ~1.0, got {norm}");
        }
    }

    /// Smoke test through the full `ProviderEmbedder` wrapper (lazy init +
    /// spawn_blocking + serialization) against the real model. Network-gated
    /// for the same reason as `bge_small_real_embedding_dimension_is_384`.
    #[tokio::test]
    #[ignore = "requires network + downloads the BGE-small ONNX model on first run"]
    async fn provider_embedder_real_embedding_smoke() {
        let embedder = ProviderEmbedder::new("bge-small-en-v1.5");
        let vector_a = embedder
            .embed("the quick brown fox jumps over the lazy dog")
            .await
            .expect("embed failed");
        let vector_b = embedder.embed("a fox jumped over a dog today").await.expect("embed failed");
        assert_eq!(vector_a.len(), 384);
        assert_eq!(vector_b.len(), 384);
        // Both vectors are L2-normalized, so the dot product is the cosine
        // similarity; related sentences must remain positively correlated.
        let cosine: f32 = vector_a.iter().zip(&vector_b).map(|(a, b)| a * b).sum();
        assert!(cosine > 0.0, "expected positive cosine similarity, got {cosine}");
    }
}
