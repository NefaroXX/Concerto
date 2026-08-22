use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// The result of computing a diff on a single file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffResult {
    pub path: Utf8PathBuf,
    pub hunks: Vec<Hunk>,
}

/// A single diff hunk (a section of changed lines).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hunk {
    pub old_start: u64,
    pub old_len: u64,
    pub new_start: u64,
    pub new_len: u64,
    pub lines: Vec<DiffLine>,
}

/// A single line in a diff hunk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum DiffLine {
    Addition { content: String, line_num: u64 },
    Deletion { content: String, line_num: u64 },
    Context { content: String, line_num: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test DiffResult serialization round-trip with empty hunks.
    #[test]
    fn diff_result_empty_hunks() {
        let diff = DiffResult { path: Utf8PathBuf::from("src/main.rs"), hunks: vec![] };
        let json = serde_json::to_string(&diff).expect("serialization should succeed");
        let deserialized: DiffResult =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.path, "src/main.rs");
        assert!(deserialized.hunks.is_empty());
    }

    /// Test DiffResult serialization with multiple hunks.
    #[test]
    fn diff_result_multiple_hunks() {
        let diff = DiffResult {
            path: Utf8PathBuf::from("lib.rs"),
            hunks: vec![
                Hunk {
                    old_start: 1,
                    old_len: 3,
                    new_start: 1,
                    new_len: 4,
                    lines: vec![
                        DiffLine::Context { content: "use std::io;".to_string(), line_num: 1 },
                        DiffLine::Deletion { content: "fn old() {}".to_string(), line_num: 2 },
                        DiffLine::Addition { content: "fn new() {}".to_string(), line_num: 2 },
                        DiffLine::Addition { content: "fn another() {}".to_string(), line_num: 3 },
                    ],
                },
                Hunk {
                    old_start: 10,
                    old_len: 2,
                    new_start: 11,
                    new_len: 2,
                    lines: vec![
                        DiffLine::Context { content: "// comment".to_string(), line_num: 10 },
                        DiffLine::Addition { content: "let x = 42;".to_string(), line_num: 11 },
                    ],
                },
            ],
        };
        let json = serde_json::to_string(&diff).expect("serialization should succeed");
        let deserialized: DiffResult =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.hunks.len(), 2);
        assert_eq!(deserialized.hunks[0].lines.len(), 4);
        assert_eq!(deserialized.hunks[1].lines.len(), 2);
    }

    /// Test Hunk serialization with various line ranges.
    #[test]
    fn hunk_line_ranges() {
        let hunk = Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 1,
            new_len: 5,
            lines: vec![DiffLine::Addition { content: "new line".to_string(), line_num: 1 }],
        };
        let json = serde_json::to_string(&hunk).expect("serialization should succeed");
        let deserialized: Hunk =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.old_start, 0);
        assert_eq!(deserialized.old_len, 0);
        assert_eq!(deserialized.new_start, 1);
        assert_eq!(deserialized.new_len, 5);
    }

    /// Test DiffLine::Addition serialization and PartialEq.
    #[test]
    fn diff_line_addition() {
        let line = DiffLine::Addition { content: "added line".to_string(), line_num: 42 };
        let json = serde_json::to_string(&line).expect("serialization should succeed");
        assert!(json.contains("Addition"));
        let deserialized: DiffLine =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, line);
    }

    /// Test DiffLine::Deletion serialization and PartialEq.
    #[test]
    fn diff_line_deletion() {
        let line = DiffLine::Deletion { content: "removed line".to_string(), line_num: 10 };
        let json = serde_json::to_string(&line).expect("serialization should succeed");
        assert!(json.contains("Deletion"));
        let deserialized: DiffLine =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, line);
    }

    /// Test DiffLine::Context serialization and PartialEq.
    #[test]
    fn diff_line_context() {
        let line = DiffLine::Context { content: "unchanged line".to_string(), line_num: 5 };
        let json = serde_json::to_string(&line).expect("serialization should succeed");
        assert!(json.contains("Context"));
        let deserialized: DiffLine =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, line);
    }

    /// Test DiffLine enum variant discrimination in JSON.
    #[test]
    fn diff_line_variant_discrimination() {
        let addition = DiffLine::Addition { content: "add".to_string(), line_num: 1 };
        let deletion = DiffLine::Deletion { content: "del".to_string(), line_num: 2 };
        let context = DiffLine::Context { content: "ctx".to_string(), line_num: 3 };

        let add_json = serde_json::to_string(&addition).unwrap();
        let del_json = serde_json::to_string(&deletion).unwrap();
        let ctx_json = serde_json::to_string(&context).unwrap();

        assert!(add_json.contains("Addition"));
        assert!(del_json.contains("Deletion"));
        assert!(ctx_json.contains("Context"));

        // Each should deserialize to the correct variant
        let add_back: DiffLine = serde_json::from_str(&add_json).unwrap();
        let del_back: DiffLine = serde_json::from_str(&del_json).unwrap();
        let ctx_back: DiffLine = serde_json::from_str(&ctx_json).unwrap();

        assert!(matches!(add_back, DiffLine::Addition { .. }));
        assert!(matches!(del_back, DiffLine::Deletion { .. }));
        assert!(matches!(ctx_back, DiffLine::Context { .. }));
    }

    /// Test DiffResult PartialEq with identical content.
    #[test]
    fn diff_result_partial_eq_identical() {
        let diff1 = DiffResult {
            path: Utf8PathBuf::from("test.rs"),
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                lines: vec![DiffLine::Addition { content: "line".to_string(), line_num: 1 }],
            }],
        };
        let diff2 = diff1.clone();
        assert_eq!(diff1, diff2);
    }

    /// Test DiffResult PartialEq with different content.
    #[test]
    fn diff_result_partial_eq_different() {
        let diff1 = DiffResult { path: Utf8PathBuf::from("test1.rs"), hunks: vec![] };
        let diff2 = DiffResult { path: Utf8PathBuf::from("test2.rs"), hunks: vec![] };
        assert_ne!(diff1, diff2);
    }

    /// Test DiffLine with special characters in content.
    #[test]
    fn diff_line_special_characters() {
        let line = DiffLine::Addition {
            content: "line with \"quotes\" and \n newlines".to_string(),
            line_num: 1,
        };
        let json = serde_json::to_string(&line).expect("serialization should succeed");
        let deserialized: DiffLine =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, line);
    }
}
