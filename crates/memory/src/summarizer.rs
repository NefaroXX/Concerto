//! LLM-based summarization trait and prompt constants.
//!
//! The `LLMSummarizer` trait is used by `SummarizeOldest` (short-term
//! memory overflow strategy) and by the entity `FactExtractor` for
//! LLM-based fact extraction.
//!
//! `SUMMARIZATION_PROMPT` is a named constant so it appears in ADRs,
//! can be tested in snapshot tests, and cannot silently drift between
//! releases.

use async_trait::async_trait;

use concerto_core::error::MemoryError;
use concerto_core::types::Message;

/// Pinned summarization prompt.
///
/// Must match the ROADMAP spec exactly. Changing this prompt changes
/// the behaviour of the short-term memory overflow strategy and the
/// fact extractor. Update ADR-16 when modifying.
pub const SUMMARIZATION_PROMPT: &str =
    "Summarize these messages as bullet points capturing all facts, decisions, \
     and code changes. Be concise. Preserve file names and line numbers.";

/// Prompt for fact extraction used by `FactExtractor`.
pub const FACT_EXTRACTION_PROMPT: &str =
    "Extract architectural facts from the following code files. Return a JSON array of objects with fields: content (string), category (one of: \"architecture\", \"constraint\", \"pattern\", \"decision\"), source_file (string). Be concise and precise.";

/// LLM-based summarizer.
///
/// Implementations wrap an LLM provider client.
/// `ProviderSummarizer` is the production implementation;
/// `FakeSummarizer` is the test double.
#[async_trait]
pub trait LLMSummarizer: Send + Sync {
    /// Summarize a slice of conversation messages into a single string.
    async fn summarize(&self, messages: &[Message], prompt: &str) -> Result<String, MemoryError>;
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

/// A summarizer that returns a fixed string — for unit tests.
#[cfg(test)]
pub struct FakeSummarizer {
    pub returns: Result<String, MemoryError>,
}

#[cfg(test)]
impl FakeSummarizer {
    pub fn new(returns: impl Into<String>) -> Self {
        Self { returns: Ok(returns.into()) }
    }

    pub fn new_err(msg: impl Into<String>) -> Self {
        Self { returns: Err(MemoryError::Persistence(msg.into())) }
    }
}

#[cfg(test)]
#[async_trait]
impl LLMSummarizer for FakeSummarizer {
    async fn summarize(&self, _messages: &[Message], _prompt: &str) -> Result<String, MemoryError> {
        self.returns.clone()
    }
}
