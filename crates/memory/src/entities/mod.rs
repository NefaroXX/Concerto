//! Entity extraction and fact extraction for long-term memory.
//!
//! - `EntityExtractor`: post-index regex-based scanning to extract code
//!   entities (functions, structs, traits, classes, interfaces, etc.).
//! - `FactExtractor`: LLM-based extraction of architectural facts from
//!   documentation and comments.
//!
//! Entity extraction uses line-prefix matching (no tree-sitter dependency).
//! See `extract_rust_entities`, `extract_typescript_entities`, etc.
//! Fact extraction uses LLMSummarizer to extract architectural facts from
//! documentation and comments, with rate limiting (1 call per 100 files).
//!
//! Split by responsibility (NORM S6) — this file is the thin re-export
//! surface, every previously public item keeps its `entities::` path:
//! - `extractor` — `EntityExtractor` dispatch + one module per language
//!   scanner (`rust`, `typescript`, `python`, `go`);
//! - `facts` — rate-limited LLM fact assembly ([`FactExtractor`]);
//! - `l1` — L1 typed extraction and the dedup judge ([`L1Extractor`],
//!   [`L1DedupJudge`]);
//! - `types` — shared value types and the entities migration SQL.

mod extractor;
mod facts;
mod l1;
mod types;

pub use extractor::EntityExtractor;
pub use facts::FactExtractor;
pub use l1::{
    L1Candidate, L1DedupJudge, L1DedupVerdict, L1ExtractedMemory, L1Extractor, L1MemoryType,
};
pub use types::{
    CodeEntity, EntityKind, EntityRelation, FactCategory, FactEntry, RelationKind,
    MIGRATION_010_ENTITIES,
};
