//! TypeScript/TSX line-prefix entity scanning (NORM S6).
//!
//! Detects functions (declaration, arrow assignment), interfaces, classes
//! and enums from trimmed lines — `//` and `/*` comment lines are skipped.

use concerto_core::memory::ProjectId;

use super::extract_doc_comment;
use crate::entities::types::{CodeEntity, EntityKind};

/// Extract TypeScript entities using regex-based parsing.
pub(super) fn extract_typescript_entities(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::EntityExtractor;
    use std::io::Write;

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
}
