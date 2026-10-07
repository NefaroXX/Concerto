//! Go line-prefix entity scanning (NORM S6).
//!
//! Detects top-level `func`s (receiver methods included), `type Name struct`
//! and `type Name interface` declarations from trimmed lines — `//` and `/*`
//! comment lines are skipped.

use concerto_core::memory::ProjectId;

use super::extract_doc_comment;
use crate::entities::types::{CodeEntity, EntityKind};

/// Extract Go entities using regex-based parsing.
pub(super) fn extract_go_entities(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::EntityExtractor;
    use std::io::Write;

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
}
