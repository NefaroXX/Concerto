//! Python line-prefix entity scanning (NORM S6).
//!
//! Detects `def` functions and `class` declarations from trimmed lines —
//! `#` comment lines are skipped and doc comments are not collected.

use concerto_core::memory::ProjectId;

use crate::entities::types::{CodeEntity, EntityKind};

/// Extract Python entities using regex-based parsing.
pub(super) fn extract_python_entities(
    content: &str,
    path: &str,
    project_id: &ProjectId,
) -> Vec<CodeEntity> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::EntityExtractor;
    use std::io::Write;

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

    /// The `.py` extension routes the file to the Python scanner.
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
}
