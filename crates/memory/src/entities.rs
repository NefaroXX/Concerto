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

use concerto_core::error::MemoryError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::memory::ProjectId;
use concerto_core::types::{Message, Role};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A single code entity extracted from source code.
#[derive(Debug, Clone)]
pub struct CodeEntity {
    pub id: String,
    pub project_id: ProjectId,
    pub name: String,
    pub kind: EntityKind,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
}

/// Kind of code entity.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum EntityKind {
    Function,
    Struct,
    Trait,
    Impl,
    Enum,
    Interface,
    Class,
    Method,
    Module,
}

/// A relation between two code entities.
#[derive(Debug, Clone)]
pub struct EntityRelation {
    pub id: String,
    pub project_id: ProjectId,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation: RelationKind,
}

/// Kind of relation between entities.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum RelationKind {
    Imports,
    Extends,
    Implements,
    Calls,
    Contains,
}

/// An extracted architectural fact.
#[derive(Debug, Clone)]
pub struct FactEntry {
    pub id: String,
    pub project_id: ProjectId,
    pub content: String,
    pub category: FactCategory,
    pub source_file: Option<String>,
    pub confidence: Option<f32>,
    pub expires_at: Option<i64>,
}

/// Category of extracted fact.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum FactCategory {
    Architecture,
    Constraint,
    Pattern,
    Decision,
}

/// Extracts code entities from indexed files using regex-based scanning.
pub struct EntityExtractor;

impl EntityExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Extract entities from a source file.
    ///
    /// Returns entities found in the file.
    pub fn extract_from_file(
        &self,
        path: &str,
        project_id: &ProjectId,
    ) -> Result<Vec<CodeEntity>, MemoryError> {
        use std::fs;

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };

        let mut entities = Vec::new();
        let ext =
            std::path::Path::new(path).extension().map(|e| e.to_str().unwrap_or("")).unwrap_or("");

        if ext == "rs" {
            entities.extend(extract_rust_entities(&content, path, project_id));
        } else if ext == "ts" || ext == "tsx" {
            entities.extend(extract_typescript_entities(&content, path, project_id));
        } else if ext == "py" {
            entities.extend(extract_python_entities(&content, path, project_id));
        } else if ext == "go" {
            entities.extend(extract_go_entities(&content, path, project_id));
        }

        Ok(entities)
    }
}

/// Extract Rust-specific entities using simple regex-based parsing.
fn extract_rust_entities(content: &str, path: &str, project_id: &ProjectId) -> Vec<CodeEntity> {
    let mut entities = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        if let Some(entity) = try_extract_fn(trimmed, line_num, &lines, idx, path, project_id) {
            entities.push(entity);
        }
        if let Some(entity) = try_extract_struct(trimmed, line_num, &lines, idx, path, project_id) {
            entities.push(entity);
        }
        if let Some(entity) = try_extract_enum(trimmed, line_num, &lines, idx, path, project_id) {
            entities.push(entity);
        }
        if let Some(entity) = try_extract_trait(trimmed, line_num, &lines, idx, path, project_id) {
            entities.push(entity);
        }
        if let Some(entity) = try_extract_impl(trimmed, line_num, &lines, idx, path, project_id) {
            entities.push(entity);
        }
    }

    entities
}

/// Try to extract a function entity from a trimmed line.
fn try_extract_fn(
    trimmed: &str,
    line_num: usize,
    lines: &[&str],
    idx: usize,
    path: &str,
    project_id: &ProjectId,
) -> Option<CodeEntity> {
    if !trimmed.starts_with("fn ") || !trimmed.contains("(") {
        return None;
    }
    let name_end = trimmed.find("(")?;
    let name = trimmed[3..name_end].trim().to_string();
    if name.is_empty() || name.starts_with("<") {
        return None;
    }
    Some(CodeEntity {
        id: format!("{}", ulid::Ulid::new()),
        project_id: project_id.clone(),
        name,
        kind: EntityKind::Function,
        file_path: path.to_string(),
        line_start: line_num,
        line_end: line_num,
        signature: Some(trimmed.to_string()),
        doc_comment: extract_doc_comment(lines, idx),
    })
}

/// Try to extract a struct entity from a trimmed line.
fn try_extract_struct(
    trimmed: &str,
    line_num: usize,
    lines: &[&str],
    idx: usize,
    path: &str,
    project_id: &ProjectId,
) -> Option<CodeEntity> {
    if !(trimmed.starts_with("pub struct ")
        || (trimmed.starts_with("struct ") && !trimmed.contains(";")))
    {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let name = parts.iter().position(|&p| p == "struct").and_then(|i| parts.get(i + 1))?;
    let name = name.split("<").next().unwrap_or("").trim_end_matches("{").trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(CodeEntity {
        id: format!("{}", ulid::Ulid::new()),
        project_id: project_id.clone(),
        name,
        kind: EntityKind::Struct,
        file_path: path.to_string(),
        line_start: line_num,
        line_end: line_num,
        signature: Some(trimmed.to_string()),
        doc_comment: extract_doc_comment(lines, idx),
    })
}

/// Try to extract an enum entity from a trimmed line.
fn try_extract_enum(
    trimmed: &str,
    line_num: usize,
    lines: &[&str],
    idx: usize,
    path: &str,
    project_id: &ProjectId,
) -> Option<CodeEntity> {
    if !(trimmed.starts_with("pub enum ")
        || (trimmed.starts_with("enum ") && !trimmed.contains(";")))
    {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let name = parts.iter().position(|&p| p == "enum").and_then(|i| parts.get(i + 1))?;
    let name = name.trim_end_matches("{").trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(CodeEntity {
        id: format!("{}", ulid::Ulid::new()),
        project_id: project_id.clone(),
        name,
        kind: EntityKind::Enum,
        file_path: path.to_string(),
        line_start: line_num,
        line_end: line_num,
        signature: Some(trimmed.to_string()),
        doc_comment: extract_doc_comment(lines, idx),
    })
}

/// Try to extract a trait entity from a trimmed line.
fn try_extract_trait(
    trimmed: &str,
    line_num: usize,
    lines: &[&str],
    idx: usize,
    path: &str,
    project_id: &ProjectId,
) -> Option<CodeEntity> {
    if !(trimmed.starts_with("pub trait ")
        || (trimmed.starts_with("trait ") && !trimmed.contains(";")))
    {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let name = parts.iter().position(|&p| p == "trait").and_then(|i| parts.get(i + 1))?;
    let name =
        name.split("<").next().unwrap_or(name).split("{").next().unwrap_or(name).trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(CodeEntity {
        id: format!("{}", ulid::Ulid::new()),
        project_id: project_id.clone(),
        name,
        kind: EntityKind::Trait,
        file_path: path.to_string(),
        line_start: line_num,
        line_end: line_num,
        signature: Some(trimmed.to_string()),
        doc_comment: extract_doc_comment(lines, idx),
    })
}

/// Try to extract an impl entity from a trimmed line.
fn try_extract_impl(
    trimmed: &str,
    line_num: usize,
    lines: &[&str],
    idx: usize,
    path: &str,
    project_id: &ProjectId,
) -> Option<CodeEntity> {
    if !trimmed.starts_with("impl") || trimmed.contains(";") {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let impl_pos = parts.iter().position(|&p| p == "impl")?;
    let name = if let Some(for_pos) = parts.iter().position(|&p| p == "for") {
        parts.get(for_pos + 1)
    } else {
        parts.get(impl_pos + 1)
    }?;
    let name =
        name.split("<").next().unwrap_or(name).trim().trim_end_matches("{").trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(CodeEntity {
        id: format!("{}", ulid::Ulid::new()),
        project_id: project_id.clone(),
        name,
        kind: EntityKind::Impl,
        file_path: path.to_string(),
        line_start: line_num,
        line_end: line_num,
        signature: Some(trimmed.to_string()),
        doc_comment: extract_doc_comment(lines, idx),
    })
}

/// Extract TypeScript entities using regex-based parsing.
fn extract_typescript_entities(
    content: &str,
    path: &str,
    project_id: &ProjectId,
) -> Vec<CodeEntity> {
    let mut entities = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }

        // Detect functions: function name(, async function name(, export function name(
        if trimmed.contains("function ") && trimmed.contains("(") {
            if let Some(start) = trimmed.find("function ").map(|i| i + "function ".len()) {
                let name = trimmed[start..].trim().to_string();
                let name = name.split("(").next().unwrap_or("").trim().to_string();
                if !name.is_empty() {
                    entities.push(CodeEntity {
                        id: format!("{}", ulid::Ulid::new()),
                        project_id: project_id.clone(),
                        name,
                        kind: EntityKind::Function,
                        file_path: path.to_string(),
                        line_start: line_num,
                        line_end: line_num,
                        signature: Some(trimmed.to_string()),
                        doc_comment: extract_doc_comment(&lines, idx),
                    });
                }
            }
        }

        // Detect arrow function assignments: const name = (
        if (trimmed.starts_with("const ") || trimmed.starts_with("let ")) && trimmed.contains("= (")
        {
            let name = trimmed
                .split("=")
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches("const")
                .trim_start_matches("let")
                .trim()
                .to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Function,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }

        // Detect interfaces
        if trimmed.starts_with("interface ") || trimmed.starts_with("export interface ") {
            let after = trimmed
                .strip_prefix("export interface ")
                .or_else(|| trimmed.strip_prefix("interface "))
                .unwrap_or(trimmed);
            let name = after.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Interface,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }

        // Detect classes
        if trimmed.starts_with("class ") || trimmed.starts_with("export class ") {
            let after = trimmed
                .strip_prefix("export class ")
                .or_else(|| trimmed.strip_prefix("class "))
                .unwrap_or(trimmed);
            let name = after
                .split("extends")
                .next()
                .unwrap_or("")
                .split("implements")
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Class,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }

        // Detect enums
        if trimmed.starts_with("enum ") || trimmed.starts_with("export enum ") {
            let after = trimmed
                .strip_prefix("export enum ")
                .or_else(|| trimmed.strip_prefix("enum "))
                .unwrap_or(trimmed);
            let name = after.split("{").next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Enum,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }
    }

    entities
}

/// Extract Python entities using regex-based parsing.
fn extract_python_entities(content: &str, path: &str, project_id: &ProjectId) -> Vec<CodeEntity> {
    let mut entities = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with("#") {
            continue;
        }

        // Detect functions: def name(
        if trimmed.starts_with("def ") && trimmed.contains("(") {
            if let Some(name_end) = trimmed.find("(") {
                let name = trimmed[4..name_end].trim().to_string();
                if !name.is_empty() {
                    entities.push(CodeEntity {
                        id: format!("{}", ulid::Ulid::new()),
                        project_id: project_id.clone(),
                        name,
                        kind: EntityKind::Function,
                        file_path: path.to_string(),
                        line_start: line_num,
                        line_end: line_num,
                        signature: Some(trimmed.to_string()),
                        doc_comment: None,
                    });
                }
            }
        }

        // Detect classes
        if trimmed.starts_with("class ") && trimmed.contains(":") {
            let after = &trimmed["class ".len()..];
            let name = after
                .split(":")
                .next()
                .unwrap_or("")
                .split("(")
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Class,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: None,
                });
            }
        }
    }

    entities
}

/// Extract Go entities using regex-based parsing.
fn extract_go_entities(content: &str, path: &str, project_id: &ProjectId) -> Vec<CodeEntity> {
    let mut entities = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }

        // Detect functions: func Name(
        if trimmed.starts_with("func ") && trimmed.contains("(") {
            let after = &trimmed["func ".len()..];
            // Skip methods (receiver): func (r *Type) Name(
            let name = if after.trim().starts_with("(") {
                // Method - extract method name after receiver
                if let Some(closing) = after.find(")") {
                    after[closing + 1..].trim().to_string()
                } else {
                    continue;
                }
            } else {
                after.to_string()
            };
            let name = name.split("(").next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Function,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }

        // Detect structs: type Name struct
        if trimmed.starts_with("type ") && trimmed.contains(" struct") {
            let after = &trimmed["type ".len()..];
            let name = after.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Struct,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }

        // Detect interfaces: type Name interface
        if trimmed.starts_with("type ") && trimmed.contains(" interface") {
            let after = &trimmed["type ".len()..];
            let name = after.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                entities.push(CodeEntity {
                    id: format!("{}", ulid::Ulid::new()),
                    project_id: project_id.clone(),
                    name,
                    kind: EntityKind::Interface,
                    file_path: path.to_string(),
                    line_start: line_num,
                    line_end: line_num,
                    signature: Some(trimmed.to_string()),
                    doc_comment: extract_doc_comment(&lines, idx),
                });
            }
        }
    }

    entities
}

/// Extract doc comment from previous lines.
fn extract_doc_comment(lines: &[&str], current_idx: usize) -> Option<String> {
    let mut comments = Vec::new();
    for idx in (0..current_idx).rev() {
        let line = lines[idx].trim();
        if line.starts_with("///") {
            comments.push(line.trim_start_matches("///").trim().to_string());
        } else if !line.is_empty() {
            break;
        }
    }
    if comments.is_empty() {
        None
    } else {
        // Reverse to get correct order
        comments.reverse();
        Some(comments.join(" "))
    }
}

impl Default for EntityExtractor {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Migration SQL for the entities tables.
pub const MIGRATION_010_ENTITIES: &str = r#"
CREATE TABLE IF NOT EXISTS code_entities (
    id           TEXT    PRIMARY KEY,
    project_id   TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    kind         TEXT    NOT NULL
                 CHECK  (kind IN ('function','struct','trait','impl','enum',
                                  'interface','class','method','module')),
    file_path    TEXT    NOT NULL,
    line_start   INTEGER NOT NULL,
    line_end     INTEGER NOT NULL,
    signature    TEXT,
    doc_comment  TEXT,
    last_seen    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_relations (
    id               TEXT    PRIMARY KEY,
    project_id       TEXT    NOT NULL,
    source_entity_id TEXT    NOT NULL,
    target_entity_id TEXT    NOT NULL,
    relation         TEXT    NOT NULL
                     CHECK  (relation IN ('imports','extends','implements',
                                          'calls','contains')),
    last_seen        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_entries (
    id          TEXT    PRIMARY KEY,
    project_id  TEXT    NOT NULL,
    content     TEXT    NOT NULL,
    category    TEXT    NOT NULL
                CHECK  (category IN ('architecture','constraint',
                                     'pattern','decision')),
    source_file TEXT,
    confidence  REAL    CHECK (confidence IS NULL OR
                               (confidence >= 0.0 AND confidence <= 1.0)),
    expires_at  INTEGER,
    created_at  INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extract_from_file_returns_empty_for_missing_file() {
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file("nonexistent_file.rs", &pid).unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn extract_rust_entities_finds_fn_struct_trait_impl_enum() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.rs");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(
            file,
            r#"
/// This is a test function
fn test_func() {{}}

pub struct TestStruct {{}}

pub enum TestEnum {{
    A,
    B,
}}

pub trait TestTrait {{}}

impl TestTrait for TestStruct {{}}
"#
        )
        .unwrap();

        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();

        assert!(
            entities.iter().any(|e| e.name == "test_func" && e.kind == EntityKind::Function),
            "should find function 'test_func'"
        );
        assert!(
            entities.iter().any(|e| e.name == "TestStruct" && e.kind == EntityKind::Struct),
            "should find struct 'TestStruct'"
        );
        assert!(
            entities.iter().any(|e| e.name == "TestEnum" && e.kind == EntityKind::Enum),
            "should find enum 'TestEnum'"
        );
        assert!(
            entities.iter().any(|e| e.name == "TestTrait" && e.kind == EntityKind::Trait),
            "should find trait 'TestTrait'"
        );
        assert!(
            entities.iter().any(|e| e.name == "TestStruct" && e.kind == EntityKind::Impl),
            "should find impl for 'TestStruct'"
        );

        // Verify doc comment on the function
        let func_entity = entities.iter().find(|e| e.name == "test_func").unwrap();
        assert_eq!(func_entity.doc_comment.as_deref(), Some("This is a test function"));
    }

    #[test]
    fn extract_typescript_entities_finds_function_class_interface_enum() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.ts");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(
            file,
            r#"
function foo() {{}}

export class Bar extends Base {{}}

export interface Baz {{}}

export enum Qux {{
    X,
    Y
}}

const arrow = () => {{}};
"#
        )
        .unwrap();

        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();

        assert!(
            entities.iter().any(|e| e.name == "foo" && e.kind == EntityKind::Function),
            "should find function 'foo'"
        );
        assert!(
            entities.iter().any(|e| e.name == "Bar" && e.kind == EntityKind::Class),
            "should find class 'Bar'"
        );
        assert!(
            entities.iter().any(|e| e.name == "Baz" && e.kind == EntityKind::Interface),
            "should find interface 'Baz'"
        );
        assert!(
            entities.iter().any(|e| e.name == "Qux" && e.kind == EntityKind::Enum),
            "should find enum 'Qux'"
        );
        // arrow function should also be extracted
        assert!(
            entities.iter().any(|e| e.name == "arrow" && e.kind == EntityKind::Function),
            "should find arrow function 'arrow'"
        );
    }

    #[test]
    fn extract_python_entities_finds_def_and_class() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.py");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(
            file,
            r#"
def my_function():
    pass

class MyClass(Base):
    pass
"#
        )
        .unwrap();

        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();

        assert!(
            entities.iter().any(|e| e.name == "my_function" && e.kind == EntityKind::Function),
            "should find function 'my_function'"
        );
        assert!(
            entities.iter().any(|e| e.name == "MyClass" && e.kind == EntityKind::Class),
            "should find class 'MyClass'"
        );
    }

    #[test]
    fn extract_go_entities_finds_func_struct_interface() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.go");
        let mut file = std::fs::File::create(&file_path).unwrap();
        write!(
            file,
            r#"
package test

func TopLevel() {{}}

type MyStruct struct {{}}

type MyInterface interface {{}}
"#
        )
        .unwrap();

        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();

        assert!(
            entities.iter().any(|e| e.name == "TopLevel" && e.kind == EntityKind::Function),
            "should find function 'TopLevel'"
        );
        assert!(
            entities.iter().any(|e| e.name == "MyStruct" && e.kind == EntityKind::Struct),
            "should find struct 'MyStruct'"
        );
        assert!(
            entities.iter().any(|e| e.name == "MyInterface" && e.kind == EntityKind::Interface),
            "should find interface 'MyInterface'"
        );
    }

    #[test]
    fn entity_kind_debug_output() {
        assert_eq!(format!("{:?}", EntityKind::Function), "Function");
        assert_eq!(format!("{:?}", EntityKind::Struct), "Struct");
        assert_eq!(format!("{:?}", EntityKind::Enum), "Enum");
        assert_eq!(format!("{:?}", EntityKind::Trait), "Trait");
        assert_eq!(format!("{:?}", EntityKind::Impl), "Impl");
        assert_eq!(format!("{:?}", EntityKind::Class), "Class");
        assert_eq!(format!("{:?}", EntityKind::Interface), "Interface");
        assert_eq!(format!("{:?}", EntityKind::Method), "Method");
        assert_eq!(format!("{:?}", EntityKind::Module), "Module");
    }

    #[test]
    fn extract_from_file_unsupported_extension_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("data.bin");
        std::fs::write(&file_path, "binary data").unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn extract_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("empty.rs");
        std::fs::write(&file_path, "").unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn extract_rust_with_only_comments_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("comments.rs");
        std::fs::write(&file_path, "// just a comment\n// another one\n").unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.is_empty());
    }

    #[test]
    fn python_entity_detection() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.py");
        std::fs::write(&file_path, "def my_function():\n    pass\n\nclass MyClass:\n    pass\n")
            .unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.iter().any(|e| e.name == "my_function"), "should find python function");
    }

    /// Extracting entities from an empty file should return empty results.
    #[test]
    fn extract_from_empty_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("empty.rs");
        std::fs::write(&file_path, "").unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.is_empty());
    }

    /// Extracting entities from a file with only comments should return empty.
    #[test]
    fn extract_from_comment_only_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("comments.rs");
        std::fs::write(&file_path, "// This is a comment\n// Another comment\n").unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file(file_path.to_str().unwrap(), &pid).unwrap();
        assert!(entities.is_empty());
    }

    /// Extracting entities from a non-UTF-8 file should not panic.
    #[test]
    fn extract_from_binary_file_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("binary.bin");
        let bytes: Vec<u8> = (0..255).collect();
        std::fs::write(&file_path, &bytes).unwrap();
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        // Should handle gracefully (likely empty or skip the file).
        let result = extractor.extract_from_file(file_path.to_str().unwrap(), &pid);
        assert!(result.is_ok(), "binary file should not cause panic");
    }
}
