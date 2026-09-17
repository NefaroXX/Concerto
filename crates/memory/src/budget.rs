//! RAG-only context budget bound.
//!
//! Replaces ad-hoc percentage calculations with a tested, configurable
//! struct that enforces the RAG allocation limit. Per ADR-16 code reality
//! (ADR-48), the only load-bearing use of this allocator is the score-ordered
//! RAG bound in [`ContextBudgetAllocator::truncate_to_rag_limit`]. The former
//! full assembly (`build_context`) had no verified production caller and was
//! removed; conversation and working-memory bounds belong to `ContextEngine`
//! + `ContextGuardProvider` in the orchestrator.

use concerto_core::error::MemoryError;
use concerto_core::memory::MemoryChunk;
use concerto_core::types::TokenBudget;

const CLIPPED_SUFFIX: &str = "\n[Content clipped to fit the context budget.]";

/// Bounds the amount of retrieved memory (RAG) injected into the prompt.
///
/// RAG-only intent (ADR-16/ADR-48): this allocator no longer splits capacity
/// across working memory and conversation history — that assembly lives in
/// the orchestrator's `ContextEngine` + `ContextGuardProvider`. The single
/// responsibility here is the aggregate RAG token bound.
pub struct ContextBudgetAllocator {
    /// Fraction of the request budget reserved for RAG chunks (default 0.25).
    pub rag_pct: f64,
}

impl Default for ContextBudgetAllocator {
    fn default() -> Self {
        Self { rag_pct: 0.25 }
    }
}

impl ContextBudgetAllocator {
    pub fn new(rag_pct: f64) -> Result<Self, MemoryError> {
        // `Range::contains` compares with `<`, so NaN (and ±inf) fall outside
        // the range and are rejected alongside negatives and full-window values.
        if !(0.0..1.0).contains(&rag_pct) {
            return Err(MemoryError::Persistence(
                "RAG percentage must be finite, non-negative, and leave room for the rest of the context"
                    .into(),
            ));
        }
        Ok(Self { rag_pct })
    }

    /// Token limit for RAG chunks given the available request tokens.
    ///
    /// Takes the *available* prompt tokens (capacity minus response
    /// reservation), never the raw total context capacity.
    pub fn rag_limit(&self, available: u64) -> u64 {
        (available as f64 * self.rag_pct) as u64
    }

    /// Select and, when necessary, clip chunks to the aggregate RAG budget.
    /// Highest-scored chunks are retained first; one oversized chunk can no
    /// longer consume the complete provider context.
    ///
    /// `budget` is the provider-reported [`TokenBudget`]; the bound is
    /// computed against [`TokenBudget::available`] (`capacity` minus
    /// `reserved_for_response`), never the raw total `capacity`.
    pub fn truncate_to_rag_limit(
        &self,
        mut chunks: Vec<MemoryChunk>,
        budget: &TokenBudget,
    ) -> Vec<MemoryChunk> {
        let limit = self.rag_limit(budget.available);
        if chunks.is_empty() || limit == 0 {
            return Vec::new();
        }

        chunks.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut selected = Vec::new();
        let mut used = 0u64;

        for mut chunk in chunks {
            let remaining = limit.saturating_sub(used);
            if remaining == 0 {
                break;
            }

            let estimated = estimate_text_tokens(&chunk.content);
            if estimated > remaining {
                chunk.content = clip_text_to_tokens(&chunk.content, remaining);
            }

            let retained = estimate_text_tokens(&chunk.content);
            if retained == 0 || retained > remaining {
                break;
            }
            used = used.saturating_add(retained);
            selected.push(chunk);
        }

        selected
    }
}

fn estimate_text_tokens(value: &str) -> u64 {
    value.len().div_ceil(4) as u64
}

fn clip_text_to_tokens(value: &str, token_limit: u64) -> String {
    let max_bytes = token_limit.saturating_mul(4) as usize;
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let suffix_bytes = CLIPPED_SUFFIX.len().min(max_bytes);
    let target = max_bytes.saturating_sub(suffix_bytes);
    let mut end = target.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    let mut clipped = value[..end].to_string();
    let suffix_start = CLIPPED_SUFFIX.len().saturating_sub(suffix_bytes);
    clipped.push_str(&CLIPPED_SUFFIX[suffix_start..]);
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::{ChunkType, MemoryNamespace, ProjectId};

    fn make_chunk(content: &str, score: f64) -> MemoryChunk {
        MemoryChunk {
            id: "test".into(),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            content: content.into(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::SlidingWindow,
            score,
            model_id: "test".into(),
            model_version: "1.0".into(),
        }
    }

    /// Budget for a given (capacity, reserved) pair, exercising the
    /// `available = capacity - reserved` semantics.
    fn budget(capacity: u64, reserved_for_response: u64) -> TokenBudget {
        TokenBudget::new(capacity, reserved_for_response)
    }

    #[test]
    fn default_allocation() {
        let alloc = ContextBudgetAllocator::default();
        assert_eq!(alloc.rag_limit(10_000), 2_500);
    }

    #[test]
    fn new_rejects_invalid_allocations() {
        assert!(ContextBudgetAllocator::new(1.0).is_err());
        assert!(ContextBudgetAllocator::new(-0.1).is_err());
        assert!(ContextBudgetAllocator::new(f64::NAN).is_err());
    }

    #[test]
    fn new_rejects_full_window_allocation() {
        // RAG must leave room for the rest of the context: exactly 1.0 and
        // anything above it is rejected.
        assert!(ContextBudgetAllocator::new(1.0).is_err());
        assert!(ContextBudgetAllocator::new(2.0).is_err());
    }

    #[test]
    fn new_accepts_valid_zero_percentage() {
        let alloc = ContextBudgetAllocator::new(0.0).unwrap();
        assert_eq!(alloc.rag_limit(10_000), 0);
    }

    #[test]
    fn new_accepts_valid_boundary_below_one() {
        let alloc = ContextBudgetAllocator::new(0.99).unwrap();
        assert_eq!(alloc.rag_limit(10_000), 9_900);
    }

    #[test]
    fn truncate_keeps_highest_scores() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(80), 0.9),
            make_chunk(&"b".repeat(80), 0.1),
            make_chunk(&"c".repeat(80), 0.5),
        ];
        let selected = alloc.truncate_to_rag_limit(chunks, &budget(200, 0));
        assert!(!selected.is_empty());
        assert_eq!(selected[0].score, 0.9);
        assert!(
            selected.iter().map(|chunk| estimate_text_tokens(&chunk.content)).sum::<u64>() <= 50
        );
    }

    #[test]
    fn truncate_empty_chunks_returns_empty() {
        let alloc = ContextBudgetAllocator::default();
        let selected = alloc.truncate_to_rag_limit(vec![], &budget(1_000, 0));
        assert!(selected.is_empty());
    }

    #[test]
    fn truncate_all_fit_within_large_budget() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(40), 0.9),
            make_chunk(&"b".repeat(40), 0.8),
            make_chunk(&"c".repeat(40), 0.7),
        ];
        let selected = alloc.truncate_to_rag_limit(chunks, &budget(10_000, 0));
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn truncate_stops_when_budget_exhausted() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(800), 0.9), // ~200 tokens each
            make_chunk(&"b".repeat(800), 0.8),
            make_chunk(&"c".repeat(800), 0.7),
        ];
        // rag_limit(available=100) = 25 tokens; each chunk is ~200 tokens, so
        // only the highest-scored chunk survives.
        let selected = alloc.truncate_to_rag_limit(chunks, &budget(100, 0)); // rag_limit=25
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].score, 0.9);
    }

    #[test]
    fn truncate_respects_score_order_when_tied() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"T".repeat(800), 0.5),
            make_chunk(&"U".repeat(800), 0.5),
            make_chunk(&"V".repeat(800), 0.5),
        ];
        // rag_limit(100) = 25 tokens, each chunk ~200 tokens → only 1 fits after clip
        let selected = alloc.truncate_to_rag_limit(chunks, &budget(100, 0));
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn oversized_single_chunk_is_clipped_to_rag_limit() {
        let alloc = ContextBudgetAllocator::default();
        let selected = alloc
            .truncate_to_rag_limit(vec![make_chunk(&"x".repeat(20_000), 1.0)], &budget(1_000, 0));
        assert_eq!(selected.len(), 1);
        assert!(estimate_text_tokens(&selected[0].content) <= alloc.rag_limit(1_000));
        assert!(selected[0].content.contains("clipped"));
    }

    #[test]
    fn zero_capacity_returns_empty() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![make_chunk("test", 1.0)];
        assert!(alloc.truncate_to_rag_limit(chunks, &budget(0, 0)).is_empty());
    }

    #[test]
    fn rag_limit_rounds_down() {
        let alloc = ContextBudgetAllocator::new(0.25).unwrap();
        // 7 * 0.25 = 1.75 → truncates to 1
        assert_eq!(alloc.rag_limit(7), 1);
    }

    #[test]
    fn rag_budget_binds_to_available_not_capacity() {
        // The provider reserves part of the window for the response. The RAG
        // bound must be derived from `available` (capacity - reserved), never
        // raw total capacity — the ambiguity the `TokenBudget` entry point
        // resolves.
        let alloc = ContextBudgetAllocator::default(); // rag_pct = 0.25
        let full = budget(10_000, 0);
        let with_reservation = budget(10_000, 4_000);
        assert_eq!(alloc.rag_limit(full.available), 2_500);
        // available = 6_000 → rag_limit = 1_500, not 2_500.
        assert_eq!(alloc.rag_limit(with_reservation.available), 1_500);
        assert_eq!(
            alloc.truncate_to_rag_limit(vec![make_chunk("x", 1.0)], &with_reservation).len(),
            1
        );
    }

    #[test]
    fn clip_text_to_tokens_exact_fit() {
        let text = "Hello, world!";
        // 13 bytes / 4 = 3.25 → ceil = 4 tokens
        let clipped = clip_text_to_tokens(text, 4);
        assert_eq!(clipped, text);
    }

    #[test]
    fn clip_text_to_tokens_zero_limit() {
        let clipped = clip_text_to_tokens("some text", 0);
        assert!(clipped.is_empty());
    }

    #[test]
    fn clip_text_to_tokens_small_limit_only_suffix() {
        let text = "This is a very long text that should be clipped";
        // limit of 1 token = 4 bytes, suffix alone may be longer than that
        let clipped = clip_text_to_tokens(text, 1);
        assert!(clipped.len() < text.len() || clipped == text);
    }
}
