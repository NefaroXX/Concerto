//! Tree-sitter based AST chunking for indexed source files.
//!
//! Walks top-level definitions (functions, structs, classes, impls, etc.)
//! and returns each as a separate text chunk. Falls back to line-by-line
//! if the grammar is unavailable or parsing fails.

use crate::indexer::Language;
use tree_sitter::Parser;

/// Node kind patterns that represent top-level definitions in each language.
/// These are matched by prefix so that e.g. both `function_item` and
/// `function_signature` (if it exists) are captured, but the match is tight
/// enough to avoid false positives.
const TOP_LEVEL_RUST: &[&str] = &[
    "function_item",
    "struct_item",
    "impl_item",
    "trait_item",
    "mod_item",
    "type_item",
    "enum_item",
    "union_item",
    "const_item",
    "static_item",
    "macro_definition",
    "macro_invocation",
];

const TOP_LEVEL_TS: &[&str] = &[
    "function_declaration",
    "class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
    "module_declaration",
    "lexical_declaration",
];

const TOP_LEVEL_PYTHON: &[&str] =
    &["function_definition", "class_definition", "decorated_definition"];

const TOP_LEVEL_GO: &[&str] = &[
    "function_declaration",
    "method_declaration",
    "type_declaration",
    "var_declaration",
    "const_declaration",
];

/// Returns the set of top-level definition kinds for the given language.
fn top_level_kinds(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => TOP_LEVEL_RUST,
        Language::TypeScript => TOP_LEVEL_TS,
        Language::Python => TOP_LEVEL_PYTHON,
        Language::Go => TOP_LEVEL_GO,
        Language::Other => &[],
    }
}

/// Create a tree-sitter parser configured for the given language.
fn parser_for_language(language: Language) -> Option<Parser> {
    let mut parser = Parser::new();
    let result = match language {
        Language::Rust => parser.set_language(&tree_sitter_rust::LANGUAGE.into()),
        Language::TypeScript => {
            // Try TypeScript first (for .ts), fall back to TSX (for .tsx).
            // tree-sitter-typescript exposes both grammars; we try TS first.
            parser.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        }
        Language::Python => parser.set_language(&tree_sitter_python::LANGUAGE.into()),
        Language::Go => parser.set_language(&tree_sitter_go::LANGUAGE.into()),
        Language::Other => return None,
    };
    match result {
        Ok(()) => Some(parser),
        Err(e) => {
            tracing::warn!("tree-sitter language not available for {:?}: {e}", language);
            None
        }
    }
}

/// Chunk source code using tree-sitter AST walking.
///
/// Parses the source, walks the root node's named children, and returns
/// one chunk per top-level definition. Non-definition children (comments,
/// blank lines between items) are merged into adjacent definition chunks.
///
/// Returns `None` if parsing fails or the language has no grammar — the
/// caller should fall back to line/sliding-window chunking.
pub fn treesitter_chunks(content: &str, language: Language) -> Option<Vec<String>> {
    let kinds = top_level_kinds(language);
    if kinds.is_empty() {
        return None;
    }

    let mut parser = parser_for_language(language)?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();

    // Collect byte ranges of top-level definition nodes.
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            ranges.push(child.byte_range());
        }
    }

    // If no definitions found, fall back so we don't silently skip a file.
    if ranges.is_empty() {
        return None;
    }

    // Merge adjacent text into chunks. Each definition becomes its own
    // chunk, but we include any leading non-definition text (comments,
    // blank lines, whitespace) in the preceding chunk.
    let bytes = content.as_bytes();
    let mut chunks: Vec<String> = Vec::with_capacity(ranges.len());

    // First chunk starts from byte 0.
    let mut chunk_start = 0usize;
    for range in &ranges {
        if range.start > chunk_start {
            // Non-definition text (comments, blank lines, whitespace)
            // between this def and the previous one (or the start of file).
            let gap = &bytes[chunk_start..range.start];
            if chunks.is_empty() {
                // Leading text before first def — include with this def.
                let text = &bytes[chunk_start..range.end];
                if let Ok(s) = std::str::from_utf8(text) {
                    chunks.push(s.to_string());
                }
                chunk_start = range.end;
                continue;
            }
            // Attach inter-def whitespace to the *previous* chunk so that
            // chunk boundaries align with definition boundaries.
            if let Ok(gap_str) = std::str::from_utf8(gap) {
                if let Some(last) = chunks.last_mut() {
                    last.push_str(gap_str);
                }
            }
        }
        // Normal case: extract the definition range.
        let text = &bytes[range.start..range.end];
        if let Ok(s) = std::str::from_utf8(text) {
            chunks.push(s.to_string());
        }
        chunk_start = range.end;
    }

    // If there's trailing text after the last definition, merge it into the
    // last chunk.
    if chunk_start < bytes.len() {
        if let Some(last) = chunks.last_mut() {
            if let Ok(tail) = std::str::from_utf8(&bytes[chunk_start..]) {
                last.push_str(tail);
            }
        }
    }

    Some(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_chunking() {
        let source = r#"
use std::collections::HashMap;

/// A doc comment.
fn foo() {
    let x = 1;
}

struct Bar {
    x: i32,
}

impl Bar {
    fn new() -> Self {
        Bar { x: 0 }
    }
}
"#;
        let chunks = treesitter_chunks(source, Language::Rust).unwrap();
        // Should have at least foo(), struct Bar, impl Bar
        assert!(chunks.len() >= 3, "expected at least 3 chunks, got {}", chunks.len());
        assert!(chunks.iter().any(|c| c.contains("fn foo")));
        assert!(chunks.iter().any(|c| c.contains("struct Bar")));
        assert!(chunks.iter().any(|c| c.contains("impl Bar")));
    }

    #[test]
    fn python_class_and_function() {
        let source = r#"
import os

class MyClass:
    def method(self):
        pass

def top_level():
    pass
"#;
        let chunks = treesitter_chunks(source, Language::Python).unwrap();
        assert!(chunks.len() >= 2, "expected >=2 chunks, got {}", chunks.len());
        assert!(chunks.iter().any(|c| c.contains("class MyClass")));
        assert!(chunks.iter().any(|c| c.contains("def top_level")));
    }

    #[test]
    fn go_function_chunking() {
        let source = r#"
package main

import "fmt"

func hello() string {
    return "hello"
}

type Config struct {
    Name string
}

const DEFAULT = "world"
"#;
        let chunks = treesitter_chunks(source, Language::Go).unwrap();
        assert!(chunks.len() >= 3, "expected >=3 chunks, got {}", chunks.len());
        assert!(chunks.iter().any(|c| c.contains("func hello")));
        assert!(chunks.iter().any(|c| c.contains("type Config")));
        assert!(chunks.iter().any(|c| c.contains("const DEFAULT")));
    }

    #[test]
    fn typescript_chunking() {
        let source = r#"
import { Component } from "react";

interface Props {
    name: string;
}

function Greeting(props: Props) {
    return <div>{props.name}</div>;
}

class App extends Component {
    render() {
        return <Greeting name="world" />;
    }
}
"#;
        let chunks = treesitter_chunks(source, Language::TypeScript).unwrap();
        assert!(chunks.len() >= 3, "expected >=3 chunks, got {}", chunks.len());
        assert!(chunks.iter().any(|c| c.contains("interface Props")));
        assert!(chunks.iter().any(|c| c.contains("function Greeting")));
        assert!(chunks.iter().any(|c| c.contains("class App")));
    }

    #[test]
    fn empty_file_returns_none() {
        let result = treesitter_chunks("", Language::Rust);
        assert!(result.is_none());
    }

    #[test]
    fn file_with_no_definitions_returns_none() {
        let result = treesitter_chunks("// just a comment\n", Language::Rust);
        assert!(result.is_none());
    }

    #[test]
    fn unknown_language_returns_none() {
        let result = treesitter_chunks("some text", Language::Other);
        assert!(result.is_none());
    }

    // ── proptest property tests ──────────────────────────────────────

    use proptest::prelude::*;

    /// Strategy: generate valid Rust source snippets with various
    /// top-level definitions, leading whitespace/comments, and blank lines.
    fn valid_rust_code() -> impl Strategy<Value = String> {
        let snippets = vec![
            "fn foo() {}".to_string(),
            "fn bar(x: i32) -> i32 { x }".to_string(),
            "struct Foo;".to_string(),
            "struct Bar { x: i32, y: String }".to_string(),
            "enum Color { Red, Green, Blue }".to_string(),
            "trait Qux { fn quux(&self); }".to_string(),
            "const MAX: usize = 100;".to_string(),
            "type MyResult<T> = Result<T, String>;".to_string(),
        ];
        prop::collection::vec(prop::sample::select(snippets), 1..4).prop_map(|defs| defs.join("\n"))
    }

    /// Strategy: generate edge-case code that may not parse (empty file,
    /// only whitespace, only comments, unknown-language text).
    fn edge_case_code() -> impl Strategy<Value = (String, Language)> {
        prop_oneof![
            (Just(String::new()), Just(Language::Rust)),
            (Just("   \n\n  ".to_string()), Just(Language::Rust)),
            (Just("// just a comment\n".to_string()), Just(Language::Rust)),
            (Just("/* block */\n".to_string()), Just(Language::Rust)),
            (Just("print('hello')".to_string()), Just(Language::Python)),
            (Just("package main\n\nfunc main() {}".to_string()), Just(Language::Go)),
            (Just("let x: number = 1;".to_string()), Just(Language::TypeScript)),
            (Just("random text".to_string()), Just(Language::Other)),
        ]
    }

    proptest! {
        /// Invariant: concatenating all chunks with no separator yields the
        /// original input (lossless roundtrip).
        #[test]
        fn lossless_roundtrip(code in valid_rust_code()) {
            let Some(chunks) = treesitter_chunks(&code, Language::Rust) else {
                // If the parser returned None (e.g. no top-level defs
                // found), skip — the fallback path handles this.
                return Ok(());
            };
            let reconstructed: String = chunks.concat();
            prop_assert_eq!(&reconstructed, &code,
                "concatenated chunks should equal original input");
        }

        /// Invariant: every chunk is non-empty.
        #[test]
        fn no_empty_chunks(code in valid_rust_code()) {
            let Some(chunks) = treesitter_chunks(&code, Language::Rust) else {
                return Ok(());
            };
            for (i, chunk) in chunks.iter().enumerate() {
                prop_assert!(!chunk.is_empty(),
                    "chunk {i} is empty");
            }
        }

        /// Invariant: chunks appear in source order and each is a substring
        /// of the original.
        #[test]
        fn ordering_and_substrings(code in valid_rust_code()) {
            let Some(chunks) = treesitter_chunks(&code, Language::Rust) else {
                return Ok(());
            };
            let mut pos = 0usize;
            for (i, chunk) in chunks.iter().enumerate() {
                let Some(offset) = code[pos..].find(chunk) else {
                    prop_assert!(false,
                        "chunk {i} not found as substring at position {pos}");
                    return Ok(());
                };
                pos += offset + chunk.len();
            }
        }

        /// Invariant: for Rust code, each chunk starts at or before the
        /// byte position of a known top-level definition keyword.
        #[test]
        fn def_keyword_in_every_chunk(code in valid_rust_code()) {
            let Some(chunks) = treesitter_chunks(&code, Language::Rust) else {
                return Ok(());
            };
            let keywords = ["fn ", "struct ", "enum ", "trait ",
                "const ", "type "];
            for chunk in &chunks {
                let has_kw = keywords.iter().any(|kw| chunk.contains(kw));
                prop_assert!(has_kw,
                    "chunk is not a definition: {chunk:?}");
            }
        }

        /// Invariant: edge cases either return None or produce valid chunks.
        #[test]
        fn edge_cases_do_not_crash(code_lang in edge_case_code()) {
            let (code, lang) = code_lang;
            // Must never panic.
            let _result = treesitter_chunks(&code, lang);
        }
    }
}
