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

/// L1 typed-extraction prompt (PersonaMem).
///
/// The L1 extractor turns a conversation slice into typed, long-horizon
/// memories (`persona` / `episodic` / `instruction` / `work_fact`) with a
/// light scene label. The JSON contract is parsed by
/// [`crate::entities::parse_l1_extraction_response`] — keep the two in sync.
pub const TYPED_L1_EXTRACTION_PROMPT: &str =
    "Extract durable, long-horizon memories from this conversation. Return a JSON object \
     with a \"memories\" array (empty if nothing durable). Each memory is an object with:\n\
     - \"type\": one of \"persona\" (stable user identity, preferences, personal facts), \
     \"episodic\" (a specific past event or decision), \"instruction\" (an explicit user \
     instruction or constraint), \"work_fact\" (facts about the user's projects, tools, \
     processes, environment);\n\
     - \"scene\": a short label for the conversational context this memory came from \
     (e.g. \"code review\", \"setup\", \"planning\"), or null;\n\
     - \"content\": the memory as a concise, self-contained sentence (preserve names, \
     paths, versions);\n\
     - optional \"owner\", \"deadline\", \"status\".\n\
     Skip greetings, transient chatter, and anything already stated elsewhere. Be precise.";

/// L1 dedup-judge prompt.
///
/// The judge decides how a NEW memory relates to the EXISTING memories
/// recalled for it. Output is parsed by [`crate::entities::parse_l1_dedup_verdict`]
/// — keep the two in sync. The judge is advisory and fail-open: anything
/// malformed defaults to `store`.
pub const L1_DEDUP_PROMPT: &str =
    "Decide what to do with a NEW memory being stored, given EXISTING memories already in \
     the store. Return a single JSON object exactly:\n\
     {\"action\": \"store\" | \"skip\" | \"update\" | \"merge\", \"target_id\": \"<existing chunk id>\" | null}\n\
     - \"store\": keep the new memory as its own chunk (no existing memory duplicates it);\n\
     - \"skip\": the new memory is already fully represented — do not store it;\n\
     - \"update\": the new memory replaces an existing one — supersede target_id and store \
     the new content;\n\
     - \"merge\": the new memory extends an existing one — supersede target_id and store the \
     combined content.\n\
     Only use update/merge with a target_id that genuinely matches one of the candidates. \
     Never invent a target_id. If unsure, prefer \"store\".";

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
