//! `EntityExtractor` — per-extension dispatch over the line-prefix scanners
//! (NORM S6).
//!
//! This is the public entry point: it reads a file, picks the scanner for its
//! extension and returns the entities that scanner found (unreadable files
//! are an empty list, never an error). The scanners themselves sit beside
//! this module in `rust`, `typescript`, `python` and `go`, each holding one
//! language's line-prefix rules and its failure-case tests.

mod go;
mod python;
mod rust;
mod typescript;

use self::go::extract_go_entities;
use self::python::extract_python_entities;
use self::rust::extract_rust_entities;
use self::typescript::extract_typescript_entities;

use concerto_core::error::MemoryError;
use concerto_core::memory::ProjectId;

use crate::entities::types::CodeEntity;

/// Extracts code entities from indexed files using regex-based scanning.
pub struct EntityExtractor;

impl EntityExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Extract entities from a source file.
    ///
    /// Returns entities found in the file.
    ///
    /// # Errors
    ///
    /// No error is currently reachable: an unreadable file is reported as an
    /// empty entity list rather than a failure.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_from_file_returns_empty_for_missing_file() {
        let extractor = EntityExtractor::new();
        let pid = ProjectId("test".into());
        let entities = extractor.extract_from_file("nonexistent_file.rs", &pid).unwrap();
        assert!(entities.is_empty());
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
