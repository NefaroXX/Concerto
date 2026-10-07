//! Rust line-prefix entity scanning: `fn` / `struct` / `enum` / `trait` /
//! `impl` (NORM S6).
//!
//! Line-prefix matching, no tree-sitter dependency. Each `try_extract_*`
//! helper inspects one already-trimmed line and returns the entity it
//! recognizes — or `None`, which the delegation in [`extract_rust_entities`]
//! treats as "this line declares nothing for this kind".

use concerto_core::memory::ProjectId;

use super::extract_doc_comment;
use crate::entities::types::{CodeEntity, EntityKind};

/// Extract Rust-specific entities using simple regex-based parsing.
pub(super) fn extract_rust_entities(
    content: &str,
    path: &str,
    project_id: &ProjectId,
) -> Vec<CodeEntity> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::EntityExtractor;
    use std::io::Write;

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

    /// Malformed declaration lines (a `fn` without a parameter list and
    /// `;`-terminated type/trait declarations) declare nothing and are
    /// skipped rather than mis-parsed.
    #[test]
    fn skips_malformed_declaration_lines() {
        let pid = ProjectId("test".into());
        let content = "fn no_parens\nstruct Marker;\nenum Flag;\ntrait Marker;\n";
        let entities = extract_rust_entities(content, "malformed.rs", &pid);
        assert!(entities.is_empty(), "malformed lines must yield no entities: {entities:?}");
    }

    /// An `impl` line with no type after the keyword (bare `impl`, or
    /// `impl X for` with nothing to implement for) has no name to record and
    /// is skipped.
    #[test]
    fn skips_empty_impl_lines() {
        let pid = ProjectId("test".into());
        let content = "impl\nimpl Marker for\n";
        let entities = extract_rust_entities(content, "empty_impl.rs", &pid);
        assert!(entities.is_empty(), "empty impls must yield no entities: {entities:?}");
    }

    /// Mixed input: comments, malformed lines and valid declarations in one
    /// file — only the well-formed items are extracted, at their own lines.
    #[test]
    fn extracts_only_valid_items_from_mixed_input() {
        let pid = ProjectId("test".into());
        let content = "// header comment\nfn no_parens\nstruct Marker;\n\nfn valid() {}\n";
        let entities = extract_rust_entities(content, "mixed.rs", &pid);

        assert_eq!(entities.len(), 1, "only the valid item survives: {entities:?}");
        assert_eq!(entities[0].name, "valid");
        assert_eq!(entities[0].kind, EntityKind::Function);
        assert_eq!(entities[0].line_start, 5, "line numbers count from the file top");
        assert_eq!(entities[0].doc_comment, None, "the preceding line is not a doc comment");
    }
}
