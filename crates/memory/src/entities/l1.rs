//! L1 typed extraction (PersonaMem) + the LLM dedup judge (NORM S6).
//!
//! L1 turns a conversation slice into typed, long-horizon memories
//! (`persona` / `episodic` / `instruction` / `work_fact`) with a light scene
//! label, then — at `MemorySystem::store` time — lets an LLM judge decide
//! store / update / merge / skip against already-stored chunks (ADR-46
//! symbolic offload; see docs/TODO.md).
//!
//! The type vocabulary ([`L1MemoryType`], [`L1ExtractedMemory`],
//! [`L1Candidate`], [`L1DedupVerdict`]) lives beside the two extractors that
//! produce and consume it so the JSON contracts and their parsers cannot
//! drift apart.

use concerto_core::error::MemoryError;
use concerto_core::types::{Message, Role};
use std::sync::Arc;

/// The L1 memory taxonomy: what kind of long-horizon memory an extracted
/// fact represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L1MemoryType {
    /// Stable user identity, preferences, and personal facts.
    Persona,
    /// A specific past event or decision.
    Episodic,
    /// An explicit user instruction or recurring constraint.
    Instruction,
    /// A fact about the user's work: projects, tools, processes, environment.
    WorkFact,
}

impl L1MemoryType {
    /// Stable wire spelling of this kind — the same token the extraction
    /// prompt accepts (`persona` / `episodic` / `instruction` / `work_fact`),
    /// so persisted `l1_kind` metadata round-trips through the parser.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Persona => "persona",
            Self::Episodic => "episodic",
            Self::Instruction => "instruction",
            Self::WorkFact => "work_fact",
        }
    }
}

/// A single L1-extracted, typed memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1ExtractedMemory {
    /// Fresh ULID assigned at extraction time.
    pub id: String,
    pub kind: L1MemoryType,
    /// Lightweight scene label for the conversational context (ADRs,
    /// "scene segmentation light").
    pub scene: Option<String>,
    pub content: String,
    pub owner: Option<String>,
    pub deadline: Option<String>,
    pub status: Option<String>,
}

/// L1 typed extractor: conversation messages -> typed long-horizon memories.
///
/// Pure extractor — it never writes to the stores. Writing (including the
/// dedup pass) is `MemorySystem`'s job.
pub struct L1Extractor {
    summarizer: Arc<dyn crate::summarizer::LLMSummarizer>,
}

impl L1Extractor {
    pub fn new(summarizer: Arc<dyn crate::summarizer::LLMSummarizer>) -> Self {
        Self { summarizer }
    }

    /// Extract typed memories from a conversation slice.
    ///
    /// Fail-soft: an empty slice is a no-op and any summarizer error is
    /// propagated to the caller.
    ///
    /// # Errors
    ///
    /// Returns the summarizer's error when the underlying LLM call fails;
    /// an empty message slice is a successful no-op.
    pub async fn extract(
        &self,
        messages: &[Message],
    ) -> Result<Vec<L1ExtractedMemory>, MemoryError> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let response = self
            .summarizer
            .summarize(messages, crate::summarizer::TYPED_L1_EXTRACTION_PROMPT)
            .await?;
        Ok(parse_l1_extraction_response(&response))
    }
}

/// Parse the L1 extraction response into typed memories.
///
/// Accepts either a bare JSON array or a `{"memories": [...]}` object.
/// Entries with an unknown `type` or empty `content` are skipped fail-soft
/// so a single malformed item never drops the whole batch.
fn parse_l1_extraction_response(response: &str) -> Vec<L1ExtractedMemory> {
    let value: serde_json::Value = match serde_json::from_str(response) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let items: &[serde_json::Value] = match &value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(obj) => match obj.get("memories") {
            Some(serde_json::Value::Array(items)) => items,
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    let mut memories = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(kind) = (match obj.get("type").and_then(|v| v.as_str()) {
            Some("persona") => Some(L1MemoryType::Persona),
            Some("episodic") => Some(L1MemoryType::Episodic),
            Some("instruction") => Some(L1MemoryType::Instruction),
            Some("work_fact") => Some(L1MemoryType::WorkFact),
            _ => None,
        }) else {
            continue; // unknown type: fail-soft skip
        };
        let Some(content) = obj.get("content").and_then(|v| v.as_str()).filter(|c| !c.is_empty())
        else {
            continue; // missing/empty content: fail-soft skip
        };
        memories.push(L1ExtractedMemory {
            id: ulid::Ulid::new().to_string(),
            kind,
            scene: obj.get("scene").and_then(|v| v.as_str()).map(str::to_string),
            content: content.to_string(),
            owner: obj.get("owner").and_then(|v| v.as_str()).map(str::to_string),
            deadline: obj.get("deadline").and_then(|v| v.as_str()).map(str::to_string),
            status: obj.get("status").and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    memories
}

/// One candidate chunk the dedup judge can act on.
#[derive(Debug, Clone)]
pub struct L1Candidate {
    pub chunk_id: String,
    pub content: String,
}

/// The dedup judge's decision for a new memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L1DedupVerdict {
    /// Keep the new memory as its own chunk (no duplicate detected).
    Store,
    /// The new memory is already represented — do not store it.
    Skip,
    /// The new memory replaces `target_id`.
    Update { target_id: String },
    /// The new memory extends `target_id`; merge the two.
    Merge { target_id: String },
}

/// LLM judge deciding store / update / merge / skip for `MemorySystem::store`.
///
/// Purely advisory: the caller is responsible for failing open (storing
/// unchanged) when the judge errors or produces an unrecognized verdict.
pub struct L1DedupJudge {
    summarizer: Arc<dyn crate::summarizer::LLMSummarizer>,
}

impl L1DedupJudge {
    pub fn new(summarizer: Arc<dyn crate::summarizer::LLMSummarizer>) -> Self {
        Self { summarizer }
    }

    /// Ask the judge what to do with `new_content` given the recalled
    /// `candidates`.
    ///
    /// # Errors
    ///
    /// Returns the summarizer's error when the underlying LLM call fails;
    /// the caller then fails open and stores the memory unchanged.
    pub async fn judge(
        &self,
        new_content: &str,
        candidates: &[L1Candidate],
    ) -> Result<L1DedupVerdict, MemoryError> {
        let mut candidate_lines = String::new();
        for (index, candidate) in candidates.iter().enumerate() {
            candidate_lines
                .push_str(&format!("[{index}] id={} {}\n", candidate.chunk_id, candidate.content));
        }
        let prompt_body = format!(
            "NEW MEMORY:\n{new_content}\n\nEXISTING MEMORIES (id, content):\n{candidate_lines}"
        );
        let message = Message {
            role: Role::User,
            content: prompt_body,
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let response =
            self.summarizer.summarize(&[message], crate::summarizer::L1_DEDUP_PROMPT).await?;
        Ok(parse_l1_dedup_verdict(&response))
    }
}

/// Parse the dedup judge's JSON verdict.
///
/// Fail-open: anything that is not a well-formed verdict with a valid
/// `target_id` for update/merge resolves to [`L1DedupVerdict::Store`] so a
/// judge regression can never silently drop memory.
fn parse_l1_dedup_verdict(response: &str) -> L1DedupVerdict {
    let value: serde_json::Value = match serde_json::from_str(response) {
        Ok(value) => value,
        Err(_) => return L1DedupVerdict::Store,
    };
    let Some(action) = value.get("action").and_then(|v| v.as_str()) else {
        return L1DedupVerdict::Store;
    };
    let target_id = value
        .get("target_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|id| !id.is_empty());
    match action {
        "skip" => L1DedupVerdict::Skip,
        "update" => match target_id {
            Some(target_id) => L1DedupVerdict::Update { target_id },
            None => L1DedupVerdict::Store,
        },
        "merge" => match target_id {
            Some(target_id) => L1DedupVerdict::Merge { target_id },
            None => L1DedupVerdict::Store,
        },
        // "store" and any unrecognized action: keep the new entry intact.
        _ => L1DedupVerdict::Store,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeSummarizer;

    // -----------------------------------------------------------------------
    // L1 typed extraction + dedup judge
    // -----------------------------------------------------------------------

    #[test]
    fn l1_parse_extraction_accepts_typed_array() {
        let response = r#"[
            {"type": "persona", "scene": "setup", "content": "the user prefers rust"},
            {"type": "episodic", "content": "the design review was rescheduled"},
            {"type": "instruction", "content": "always pin the toolchain in CI", "owner": "user"},
            {"type": "work_fact", "content": "concerto pins rust 1.96.0", "deadline": "2026-10-01"}
        ]"#;
        let memories = parse_l1_extraction_response(response);
        assert_eq!(memories.len(), 4);
        assert_eq!(memories[0].kind, L1MemoryType::Persona);
        assert_eq!(memories[0].scene.as_deref(), Some("setup"));
        assert_eq!(memories[0].content, "the user prefers rust");
        assert!(!memories[0].id.is_empty(), "id should be a fresh ulid");
        assert_eq!(memories[1].kind, L1MemoryType::Episodic);
        assert_eq!(memories[2].kind, L1MemoryType::Instruction);
        assert_eq!(memories[2].owner.as_deref(), Some("user"));
        assert_eq!(memories[3].kind, L1MemoryType::WorkFact);
        assert_eq!(memories[3].deadline.as_deref(), Some("2026-10-01"));
        assert_eq!(memories[2].status, None);
    }

    #[test]
    fn l1_parse_extraction_accepts_wrapped_object() {
        let response = r#"{"memories": [{"type": "work_fact", "content": "the api is axum"}]}"#;
        let memories = parse_l1_extraction_response(response);
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].kind, L1MemoryType::WorkFact);
        assert_eq!(memories[0].content, "the api is axum");
    }

    #[test]
    fn l1_parse_extraction_skips_unknown_type_and_empty_content() {
        let response = r#"[
            {"type": "horoscope", "content": "rise and shine"},
            {"type": "persona", "content": ""},
            {"type": "persona", "content": "valid one"}
        ]"#;
        let memories = parse_l1_extraction_response(response);
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "valid one");
    }

    #[test]
    fn l1_parse_extraction_malformed_returns_empty() {
        assert!(parse_l1_extraction_response("not json").is_empty());
        assert!(parse_l1_extraction_response("{\"memories\": {}}").is_empty());
        assert!(parse_l1_extraction_response("42").is_empty());
    }

    #[tokio::test]
    async fn l1_extractor_empty_messages_is_a_noop() {
        let extractor = L1Extractor::new(Arc::new(FakeSummarizer::new("should never be used")));
        let memories = extractor.extract(&[]).await.unwrap();
        assert!(memories.is_empty());
    }

    #[test]
    fn l1_dedup_parse_all_actions() {
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "store", "target_id": null}"#),
            L1DedupVerdict::Store
        );
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "skip", "target_id": null}"#),
            L1DedupVerdict::Skip
        );
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "update", "target_id": "c1"}"#),
            L1DedupVerdict::Update { target_id: "c1".into() }
        );
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "merge", "target_id": "c2"}"#),
            L1DedupVerdict::Merge { target_id: "c2".into() }
        );
    }

    #[test]
    fn l1_dedup_malformed_or_missing_target_defaults_to_store() {
        // Unrecognized action, missing action, malformed JSON, and
        // update/merge without a target all fail open to Store.
        assert_eq!(parse_l1_dedup_verdict("not json"), L1DedupVerdict::Store);
        assert_eq!(parse_l1_dedup_verdict(r#"{"action": "explode"}"#), L1DedupVerdict::Store);
        assert_eq!(parse_l1_dedup_verdict(r#"{"target_id": "c1"}"#), L1DedupVerdict::Store);
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "update", "target_id": null}"#),
            L1DedupVerdict::Store
        );
        assert_eq!(
            parse_l1_dedup_verdict(r#"{"action": "merge", "target_id": ""}"#),
            L1DedupVerdict::Store
        );
    }

    #[tokio::test]
    async fn l1_dedup_judge_returns_verdict_from_prompt() {
        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "merge", "target_id": "c1"}"#,
        )));
        let candidates = vec![L1Candidate { chunk_id: "c1".into(), content: "old fact".into() }];
        let verdict = judge.judge("new fact", &candidates).await.unwrap();
        assert_eq!(verdict, L1DedupVerdict::Merge { target_id: "c1".into() });
    }

    #[tokio::test]
    async fn l1_dedup_judge_propagates_summarizer_errors() {
        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new_err("boom")));
        let candidates = vec![L1Candidate { chunk_id: "c1".into(), content: "old".into() }];
        assert!(judge.judge("new", &candidates).await.is_err());
    }
}
