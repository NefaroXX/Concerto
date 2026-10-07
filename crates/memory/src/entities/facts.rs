//! LLM-based architectural fact assembly (NORM S6).
//!
//! [`FactExtractor`] reads documentation/comment files in batches and asks
//! the summarizer for structured architectural facts, rate-limited to one
//! LLM call per 100 files per session. The raw JSON response is parsed into
//! [`FactEntry`] rows by [`parse_fact_extraction_response`], which skips
//! malformed items fail-soft instead of dropping the batch.

use concerto_core::error::MemoryError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::memory::ProjectId;
use concerto_core::types::{Message, Role};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::entities::types::{FactCategory, FactEntry};

/// LLM-based fact extractor (rate-limited).
pub struct FactExtractor {
    bus: EventBus,
    summarizer: Arc<dyn crate::summarizer::LLMSummarizer>,
    files_processed_since_last_call: AtomicUsize,
}

impl FactExtractor {
    pub fn new(bus: EventBus, summarizer: Arc<dyn crate::summarizer::LLMSummarizer>) -> Self {
        Self { bus, summarizer, files_processed_since_last_call: AtomicUsize::new(0) }
    }

    /// Extract facts from documentation and comments.
    ///
    /// Rate-limited: no more than 1 LLM call per 100 files per session.
    ///
    /// # Errors
    ///
    /// Returns the summarizer's error when the underlying LLM call fails;
    /// batches below the rate-limit threshold return `Ok` with no facts.
    pub async fn extract_facts(
        &self,
        files: &[String],
        project_id: &ProjectId,
    ) -> Result<Vec<FactEntry>, MemoryError> {
        // Accumulate file count and check rate limit
        let prev = self.files_processed_since_last_call.fetch_add(files.len(), Ordering::SeqCst);
        if prev + files.len() < 100 {
            // Not enough files accumulated yet — skip LLM call
            // Global event: intentionally unscoped (background fact
            // extractor, no session context).
            let _ = self.bus.publish_raw(EventKind::FactExtracted {
                project_id: project_id.0.clone(),
                fact_count: 0,
            });
            return Ok(Vec::new());
        }

        // Rate limit reached: reset counter and proceed
        self.files_processed_since_last_call.store(0, Ordering::SeqCst);

        // Read file contents
        let mut messages = Vec::with_capacity(files.len());
        for path in files {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            messages.push(Message {
                role: Role::User,
                content,
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            });
        }

        // Call LLM summarizer
        let response =
            self.summarizer.summarize(&messages, crate::summarizer::FACT_EXTRACTION_PROMPT).await?;

        // Parse JSON array response into FactEntry values
        let facts = parse_fact_extraction_response(&response, project_id);

        let count = facts.len();
        // Global event: intentionally unscoped (background fact extractor,
        // no session context).
        let _ = self.bus.publish_raw(EventKind::FactExtracted {
            project_id: project_id.0.clone(),
            fact_count: count,
        });
        Ok(facts)
    }
}

/// Parse the LLM response JSON array into `FactEntry` values.
fn parse_fact_extraction_response(response: &str, project_id: &ProjectId) -> Vec<FactEntry> {
    let json_val: serde_json::Value = match serde_json::from_str(response) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let arr = match json_val {
        serde_json::Value::Array(ref arr) => arr.clone(),
        _ => return Vec::new(),
    };

    let mut facts = Vec::new();
    for item in arr {
        let obj = match item {
            serde_json::Value::Object(ref map) => map,
            _ => continue,
        };
        let content = match obj.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => continue,
        };
        let category_str = obj.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let category = match category_str {
            "architecture" => FactCategory::Architecture,
            "constraint" => FactCategory::Constraint,
            "pattern" => FactCategory::Pattern,
            "decision" => FactCategory::Decision,
            _ => continue,
        };
        let source_file = obj.get("source_file").and_then(|v| v.as_str()).map(|s| s.to_string());

        facts.push(FactEntry {
            id: format!("{}", ulid::Ulid::new()),
            project_id: project_id.clone(),
            content,
            category,
            source_file,
            confidence: None,
            expires_at: None,
        });
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid() -> ProjectId {
        ProjectId("test".into())
    }

    /// A malformed or non-array response parses to no facts (fail-soft).
    #[test]
    fn parse_fact_extraction_malformed_returns_empty() {
        assert!(parse_fact_extraction_response("not json", &pid()).is_empty());
        assert!(parse_fact_extraction_response("42", &pid()).is_empty());
        assert!(parse_fact_extraction_response("{\"facts\": []}", &pid()).is_empty());
    }

    /// Array entries that are not objects, or that lack usable content or an
    /// unknown category, are skipped without dropping the valid entries.
    #[test]
    fn parse_fact_extraction_skips_malformed_items() {
        let response = r#"[
            42,
            "loose string",
            {"category": "architecture"},
            {"content": "", "category": "pattern"},
            {"content": "unknown category", "category": "astrology"},
            {"content": "kept fact", "category": "decision"}
        ]"#;
        let facts = parse_fact_extraction_response(response, &pid());
        assert_eq!(facts.len(), 1, "only the well-formed item survives");
        assert_eq!(facts[0].content, "kept fact");
        assert_eq!(facts[0].category, FactCategory::Decision);
        assert!(!facts[0].id.is_empty(), "id should be a fresh ulid");
    }

    /// A well-formed item keeps its content, category, and source file.
    #[test]
    fn parse_fact_extraction_accepts_well_formed_items() {
        let response = r#"[{"content": "the api is axum", "category": "architecture", "source_file": "a.md"}]"#;
        let facts = parse_fact_extraction_response(response, &pid());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "the api is axum");
        assert_eq!(facts[0].category, FactCategory::Architecture);
        assert_eq!(facts[0].source_file.as_deref(), Some("a.md"));
        assert_eq!(facts[0].confidence, None);
        assert_eq!(facts[0].expires_at, None);
    }
}
