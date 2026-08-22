# Expanded Research: AI-Native Shell Architecture
## Concrete Schemas, Code Patterns & Provider Integrations

**Project:** Concerto  
**Date:** 2026-08-01  
**Scope:** Implementation-ready reference for `concerto-shell` crate

---

## Table of Contents

1. The `ToolManifest` System — Complete Specification
2. Schema Design Patterns — Strict Mode Deep-Dive
3. Built-in Tool Reference Implementations
4. Provider-Native Schema Conversion
5. Validation Layer Architecture
6. Output Summarization Engine
7. Hierarchical Tool Namespace Loading
8. Idempotency & State Delta Protocol
9. Observable Shell State Machine
10. Dry-Run & Confirmation Framework
11. Cross-Platform Tool Crate Specifications
12. Single Binary Distribution Strategy
13. Multi-Agent Validation Hooks
14. Hallucination Benchmark Specification

---

## 1. The `ToolManifest` System — Complete Specification

### 1.1 Core Types

```rust
// concerto-shell/src/manifest.rs

use schemars::{schema::RootSchema, JsonSchema};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub type ToolName = &'static str;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ToolVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch }
    }

    pub fn schema_hash(&self, schema: &RootSchema) -> String {
        use blake3::Hasher;
        let mut hasher = Hasher::new();
        hasher.update(&serde_json::to_vec(schema).unwrap_or_default());
        hasher.finalize().to_hex().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectClass {
    ReadOnly,
    Idempotent,
    Destructive,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: ToolName,
    pub version: ToolVersion,
    pub description: &'static str,
    pub input_schema: RootSchema,
    pub output_schema: RootSchema,
    pub effect_class: EffectClass,
    pub max_output_bytes: usize,
    pub timeout_ms: u64,
    pub namespace: &'static str,
    pub tags: &'static [&'static str],
    pub supports_dry_run: bool,
    pub supports_idempotency: bool,
    #[serde(skip)]
    pub schema_hash: String,
}

pub struct ToolRegistry {
    tools: HashMap<ToolName, &'static ToolManifest>,
    by_namespace: HashMap<&'static str, Vec<ToolName>>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&ToolManifest> {
        self.tools.get(name).copied()
    }

    pub fn tools_in_namespace(&self, ns: &str) -> Vec<&ToolManifest> {
        self.by_namespace
            .get(ns)
            .map(|names| names.iter().filter_map(|n| self.get(n)).collect())
            .unwrap_or_default()
    }

    pub fn all_tools(&self) -> Vec<&ToolManifest> {
        self.tools.values().copied().collect()
    }

    pub fn available_names(&self) -> Vec<String> {
        self.tools.keys().map(|s| s.to_string()).collect()
    }
}
```

### 1.2 Tool Registration Example

```rust
// concerto-shell/src/tools/fs/read.rs

use concerto_shell::manifest::{ToolManifest, ToolVersion, EffectClass};
use schemars::schema_for;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref FS_READ_MANIFEST: ToolManifest = ToolManifest {
        name: "fs.read",
        version: ToolVersion::new(1, 0, 0),
        description: r#"Reads file content as UTF-8 text.

EXPLICIT CONSTRAINTS:
- Only reads regular files, not directories or symlinks.
- Returns UTF-8 text only. For binary files, use fs.read_bytes.
- The path MUST be absolute. Relative paths are rejected.
- Use offset and limit to read specific line ranges.
- Maximum output is 50KB; larger files are summarized automatically."#,
        input_schema: schema_for!(FsReadArgs),
        output_schema: schema_for!(FsReadOutput),
        effect_class: EffectClass::ReadOnly,
        max_output_bytes: 50_000,
        timeout_ms: 5_000,
        namespace: "fs",
        tags: &["file", "read", "content", "text"],
        supports_dry_run: false,
        supports_idempotency: false,
        schema_hash: String::new(),
    };
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[schemars(description = "Arguments for fs.read")]
pub struct FsReadArgs {
    #[schemars(description = "Absolute file path. Must start with / or drive letter.")]
    pub path: String,

    #[schemars(description = "Start line (1-indexed). Omit to read from beginning.")]
    pub offset: Option<usize>,

    #[schemars(description = "Maximum lines to return. Omit to read entire file (subject to max_output_bytes).")]
    pub limit: Option<usize>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[schemars(description = "Output from fs.read")]
pub struct FsReadOutput {
    pub content: String,
    pub total_lines: usize,
    pub returned_lines: usize,
    pub size_bytes: usize,
    pub content_hash: String,
    pub truncated: bool,
    pub summary: Option<String>,
}
```

---

## 2. Schema Design Patterns — Strict Mode Deep-Dive

### 2.1 The Strict Mode Contract

Every tool schema MUST follow these rules. Violations are caught by a `SchemaLinter` at build time.

```rust
// concerto-shell/src/schema_linter.rs

pub struct SchemaLinter;

impl SchemaLinter {
    pub fn lint(manifest: &ToolManifest) -> Result<(), Vec<SchemaViolation>> {
        let mut violations = Vec::new();

        // Rule 1: additionalProperties must be false at root
        if !Self::has_additional_properties_false(&manifest.input_schema) {
            violations.push(SchemaViolation::MissingAdditionalPropertiesFalse);
        }

        // Rule 2: All properties must have descriptions
        for (prop_name, prop_schema) in Self::properties(&manifest.input_schema) {
            if !Self::has_description(prop_schema) {
                violations.push(SchemaViolation::MissingDescription {
                    field: prop_name.clone(),
                });
            }
        }

        // Rule 3: No optional fields without null union type
        for (prop_name, required) in Self::required_fields(&manifest.input_schema) {
            if !required && !Self::has_null_union(&manifest.input_schema, &prop_name) {
                violations.push(SchemaViolation::OptionalWithoutNull {
                    field: prop_name,
                });
            }
        }

        // Rule 4: maxLength/maxItems/maximum constraints on unbounded fields
        for (prop_name, schema) in Self::unbounded_fields(&manifest.input_schema) {
            if !Self::has_bound_constraint(schema) {
                violations.push(SchemaViolation::UnboundedField {
                    field: prop_name,
                    hint: "Add maxLength, maxItems, or maximum constraint".to_string(),
                });
            }
        }

        if violations.is_empty() { Ok(()) } else { Err(violations) }
    }
}
```

### 2.2 Example: Proper vs Improper Schema

**IMPROPER — Will fail linting:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "content": { "type": "string" },
    "force": { "type": "boolean" }
  },
  "required": ["path", "content"]
}
```
Problems: `additionalProperties` not false, no descriptions, optional without null, no maxLength.

**PROPER — Passes linting:**
```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "path": {
      "type": "string",
      "description": "Absolute path to write to. Must be absolute. Max 4096 chars.",
      "maxLength": 4096
    },
    "content": {
      "type": "string",
      "description": "UTF-8 content to write. Max 1MB.",
      "maxLength": 1048576
    },
    "expect_hash": {
      "type": ["string", "null"],
      "description": "BLAKE3 hash of expected current content. Null to skip check.",
      "maxLength": 64
    },
    "dry_run": {
      "type": ["boolean", "null"],
      "description": "If true, returns preview without writing. Null defaults to false."
    }
  },
  "required": ["path", "content", "expect_hash", "dry_run"]
}
```

### 2.3 The "Affordance" Pattern in Descriptions

```rust
const FS_SEARCH_DESC: &str = r#"Search file content using regex or literal string.

EXPLICIT CONSTRAINTS:
- Searches file CONTENT, not filenames. Use fs.glob for filename patterns.
- pattern supports regex syntax when regex: true. When regex: false, performs literal substring match.
- path must be absolute. Can be a file or directory.
- Returns max 1000 matches (configurable via max_results).
- Does NOT support: case-insensitive flag (always case-sensitive), whole-word matching, or fuzzy search.
- For case-insensitive search, use regex: true with (?i) flag in pattern."#;
```

---

## 3. Built-in Tool Reference Implementations

### 3.1 `fs.read` — The Foundation

```rust
// tool-fs/src/read.rs

use std::fs;
use std::path::Path;
use blake3::Hasher;
use concerto_shell::{
    manifest::{ToolManifest, EffectClass},
    output::{ToolOutput, Summary},
    ToolContext, ToolResult,
};

pub struct FsReadTool;

impl FsReadTool {
    pub async fn execute(args: FsReadArgs, ctx: &ToolContext) -> ToolResult<ToolOutput> {
        let path = Path::new(&args.path);

        if !path.is_absolute() {
            return Err(ToolError::InvalidArgument {
                field: "path".to_string(),
                expected: "absolute path".to_string(),
                hint: format!("Path '{}' is relative. Use absolute paths only.", args.path),
            });
        }

        ctx.policy.check_read(path)?;

        let metadata = fs::metadata(path).map_err(|e| ToolError::Io {
            path: args.path.clone(),
            message: e.to_string(),
        })?;

        if !metadata.is_file() {
            return Err(ToolError::InvalidArgument {
                field: "path".to_string(),
                expected: "regular file".to_string(),
                hint: format!("'{}' is not a regular file.", args.path),
            });
        }

        let content = fs::read_to_string(path).map_err(|e| ToolError::Io {
            path: args.path.clone(),
            message: format!("Failed to read as UTF-8: {}. Use fs.read_bytes for binary files.", e),
        })?;

        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let (start, end) = match (args.offset, args.limit) {
            (Some(offset), Some(limit)) => {
                let start = offset.saturating_sub(1);
                let end = (start + limit).min(total_lines);
                (start, end)
            }
            (Some(offset), None) => (offset.saturating_sub(1), total_lines),
            (None, Some(limit)) => (0, limit.min(total_lines)),
            (None, None) => (0, total_lines),
        };

        let selected_lines = &lines[start..end];
        let returned_content = selected_lines.join("\n");
        let returned_lines = selected_lines.len();

        let manifest = ctx.registry.get("fs.read").unwrap();
        let max_bytes = manifest.max_output_bytes;
        let content_bytes = returned_content.len();
        let truncated = content_bytes > max_bytes;

        let (final_content, summary) = if truncated {
            let truncated_content = &returned_content[..max_bytes];
            let last_newline = truncated_content.rfind('\n').unwrap_or(0);
            let clean_truncated = &truncated_content[..last_newline];
            let remaining_lines = total_lines.saturating_sub(clean_truncated.lines().count());
            let summary_text = format!(
                "File has {} total lines. Showing lines {}-{}. {} lines remaining. Use offset={} to continue.",
                total_lines, start + 1, start + clean_truncated.lines().count(),
                remaining_lines, start + clean_truncated.lines().count() + 1
            );
            (clean_truncated.to_string(), Some(summary_text))
        } else {
            (returned_content, None)
        };

        let output = FsReadOutput {
            content: final_content,
            total_lines,
            returned_lines,
            size_bytes: metadata.len() as usize,
            content_hash: hash,
            truncated,
            summary,
        };

        let tool_output = if content_bytes < 1024 {
            ToolOutput::Full(serde_json::to_value(output)?)
        } else {
            let ast_summary = ctx.summarizer.summarize_code(&content, &args.path).await;
            ToolOutput::Summarized(Summary {
                description: format!("{}-line file at {}", total_lines, args.path),
                key_points: ast_summary.signatures,
                metadata: serde_json::json!({
                    "language": ast_summary.language,
                    "imports": ast_summary.imports,
                    "line_count": total_lines,
                    "content_hash": hash,
                }),
                full_ref: ctx.memory.store_large_output(&args.path, &content).await?,
            })
        };

        Ok(tool_output)
    }
}
```

### 3.2 `fs.write` — Idempotency & Dry-Run

```rust
// tool-fs/src/write.rs

use std::fs;
use std::path::Path;

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsWriteArgs {
    #[schemars(description = "Absolute path to write. Must be absolute. Max 4096 chars.")]
    pub path: String,

    #[schemars(description = "UTF-8 content to write. Max 1MB.")]
    pub content: String,

    #[schemars(description = "BLAKE3 hash of expected current content. Null to skip.")]
    pub expect_hash: Option<String>,

    #[schemars(description = "If true, returns preview without writing. Null defaults to false.")]
    pub dry_run: Option<bool>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsWriteOutput {
    pub bytes_written: usize,
    pub new_hash: String,
    pub dry_run: bool,
    pub previous_hash: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsWriteConflict {
    pub current_hash: String,
    pub diff: Vec<DiffHunk>,
    pub message: String,
}

pub enum FsWriteResult {
    Success(FsWriteOutput),
    Conflict(FsWriteConflict),
}

impl FsWriteTool {
    pub async fn execute(args: FsWriteArgs, ctx: &ToolContext) -> ToolResult<FsWriteResult> {
        let path = Path::new(&args.path);

        if !path.is_absolute() {
            return Err(ToolError::InvalidArgument {
                field: "path".to_string(),
                expected: "absolute path".to_string(),
                hint: format!("Path '{}' is relative.", args.path),
            });
        }

        ctx.policy.check_write(path)?;

        let is_dry_run = args.dry_run.unwrap_or(false);
        let content_hash = blake3::hash(args.content.as_bytes()).to_hex().to_string();

        let (previous_hash, exists) = if path.exists() {
            let existing = fs::read_to_string(path)?;
            let hash = blake3::hash(existing.as_bytes()).to_hex().to_string();
            (Some(hash), true)
        } else {
            (None, false)
        };

        // Idempotency check
        if let Some(expected) = &args.expect_hash {
            if let Some(current) = &previous_hash {
                if current != expected {
                    let existing = fs::read_to_string(path)?;
                    let diff = compute_diff(&existing, &args.content);

                    return Ok(FsWriteResult::Conflict(FsWriteConflict {
                        current_hash: current.clone(),
                        diff,
                        message: format!(
                            "Idempotency check failed. Expected hash {}, but current hash is {}. File has changed since last read. Review the diff and retry with current_hash.",
                            expected, current
                        ),
                    }));
                }
            } else {
                return Err(ToolError::InvalidArgument {
                    field: "expect_hash".to_string(),
                    expected: "file to exist".to_string(),
                    hint: "File does not exist, but expect_hash was provided. Use expect_hash: null for new files.".to_string(),
                });
            }
        }

        if is_dry_run {
            let preview = if exists {
                format!(
                    "Would overwrite {} bytes at {} (current hash: {}, new hash: {}). {} lines changed.",
                    args.content.len(), args.path,
                    previous_hash.as_ref().unwrap_or(&"(new file)".to_string()),
                    content_hash, args.content.lines().count()
                )
            } else {
                format!(
                    "Would create new file {} ({} bytes, {} lines, hash: {}).",
                    args.path, args.content.len(), args.content.lines().count(), content_hash
                )
            };

            return Ok(FsWriteResult::Success(FsWriteOutput {
                bytes_written: args.content.len(),
                new_hash: content_hash,
                dry_run: true,
                previous_hash,
            }));
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(path, &args.content)?;
        ctx.audit.log_tool_call("fs.write", &args, &content_hash).await?;

        Ok(FsWriteResult::Success(FsWriteOutput {
            bytes_written: args.content.len(),
            new_hash: content_hash,
            dry_run: false,
            previous_hash,
        }))
    }
}

fn compute_diff(old: &str, new: &str) -> Vec<DiffHunk> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();

    for change in diff.iter_all_changes() {
        hunks.push(DiffHunk {
            tag: match change.tag() {
                ChangeTag::Delete => "removed",
                ChangeTag::Insert => "added",
                ChangeTag::Equal => "unchanged",
            }.to_string(),
            line: change.old_index().or(change.new_index()).map(|i| i + 1),
            content: change.value().to_string(),
        });
    }
    hunks
}
```

### 3.3 `fs.search` — Replacing `grep`

```rust
// tool-fs/src/search.rs

use regex::Regex;
use walkdir::WalkDir;
use memmap2::Mmap;

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsSearchArgs {
    #[schemars(description = "Search pattern. Regex syntax when regex=true, literal otherwise.")]
    pub pattern: String,

    #[schemars(description = "Absolute path to search. Can be file or directory.")]
    pub path: String,

    #[schemars(description = "If true, pattern is treated as regex. If false, literal substring match.")]
    pub regex: Option<bool>,

    #[schemars(description = "Max results to return. Default 100, max 1000.", maximum = 1000)]
    pub max_results: Option<usize>,

    #[schemars(description = "File glob filter. Examples: '*.rs', '*.toml'. Null for all files.")]
    pub filter: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsSearchMatch {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub match_text: String,
    pub context_before: String,
    pub context_after: String,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct FsSearchOutput {
    pub matches: Vec<FsSearchMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub truncated: bool,
}

impl FsSearchTool {
    pub async fn execute(args: FsSearchArgs, ctx: &ToolContext) -> ToolResult<ToolOutput> {
        let max_results = args.max_results.unwrap_or(100).min(1000);
        let use_regex = args.regex.unwrap_or(false);
        let path = Path::new(&args.path);

        let pattern = if use_regex {
            Regex::new(&args.pattern).map_err(|e| ToolError::InvalidArgument {
                field: "pattern".to_string(),
                expected: "valid regex".to_string(),
                hint: format!("Regex error: {}", e),
            })?
        } else {
            Regex::new(&regex::escape(&args.pattern)).unwrap()
        };

        let mut matches = Vec::new();
        let mut files_searched = 0;
        let mut truncated = false;

        let targets: Vec<_> = if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            WalkDir::new(path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| {
                    if let Some(ref glob) = args.filter {
                        e.file_name().to_string_lossy().ends_with(&glob.replace("*.", "."))
                    } else { true }
                })
                .map(|e| e.path().to_path_buf())
                .collect()
        };

        for file_path in targets {
            if matches.len() >= max_results {
                truncated = true;
                break;
            }

            files_searched += 1;
            let file = fs::File::open(&file_path)?;
            let mmap = unsafe { Mmap::map(&file)? };
            let content = std::str::from_utf8(&mmap).unwrap_or("");

            for (line_num, line) in content.lines().enumerate() {
                if matches.len() >= max_results {
                    truncated = true;
                    break;
                }

                for mat in pattern.find_iter(line) {
                    let context_before = if line_num > 0 {
                        content.lines().nth(line_num.saturating_sub(1)).unwrap_or("").to_string()
                    } else { String::new() };

                    let context_after = content.lines().nth(line_num + 1).unwrap_or("").to_string();

                    matches.push(FsSearchMatch {
                        path: file_path.to_string_lossy().to_string(),
                        line: line_num + 1,
                        column: mat.start() + 1,
                        match_text: mat.as_str().to_string(),
                        context_before,
                        context_after,
                    });
                }
            }
        }

        let output = FsSearchOutput {
            matches,
            total_matches: matches.len(),
            files_searched,
            truncated,
        };

        Ok(ToolOutput::Full(serde_json::to_value(output)?))
    }
}
```

---

## 4. Provider-Native Schema Conversion

### 4.1 OpenAI Function Schema Converter

```rust
// concerto-shell/src/providers/openai.rs

use schemars::schema::RootSchema;
use serde_json::json;

pub struct OpenAiSchemaConverter;

impl OpenAiSchemaConverter {
    pub fn convert(manifest: &ToolManifest) -> serde_json::Value {
        let strict_schema = Self::make_strict(&manifest.input_schema);

        json!({
            "type": "function",
            "function": {
                "name": manifest.name,
                "description": manifest.description,
                "parameters": strict_schema,
                "strict": true
            }
        })
    }

    fn make_strict(schema: &RootSchema) -> serde_json::Value {
        let mut value = serde_json::to_value(schema).unwrap();
        Self::enforce_strict(&mut value);
        value
    }

    fn enforce_strict(value: &mut serde_json::Value) {
        if let Some(obj) = value.as_object_mut() {
            if obj.get("type").and_then(|t| t.as_str()) == Some("object") {
                obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(false));

                if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
                    let required: Vec<String> = props.keys().cloned().collect();
                    obj.insert("required".to_string(), json!(required));
                }
            }

            for (_, v) in obj.iter_mut() {
                Self::enforce_strict(v);
            }
        }
    }
}
```

### 4.2 Anthropic Tool Use Schema Converter

```rust
// concerto-shell/src/providers/anthropic.rs

pub struct AnthropicSchemaConverter;

impl AnthropicSchemaConverter {
    pub fn convert(manifest: &ToolManifest) -> serde_json::Value {
        let input_schema = Self::simplify_for_anthropic(&manifest.input_schema);

        json!({
            "name": manifest.name,
            "description": manifest.description,
            "input_schema": input_schema
        })
    }

    fn simplify_for_anthropic(schema: &RootSchema) -> serde_json::Value {
        let mut value = serde_json::to_value(schema).unwrap();

        if let Some(obj) = value.as_object_mut() {
            Self::truncate_descriptions(obj, 1024);
        }

        value
    }

    fn truncate_descriptions(obj: &mut serde_json::Map<String, serde_json::Value>, max_len: usize) {
        if let Some(desc) = obj.get_mut("description") {
            if let Some(s) = desc.as_str() {
                if s.len() > max_len {
                    *desc = json!(&s[..max_len]);
                }
            }
        }

        for (_, v) in obj.iter_mut() {
            if let Some(nested) = v.as_object_mut() {
                Self::truncate_descriptions(nested, max_len);
            }
        }
    }
}
```

### 4.3 Gemini Function Declaration Converter

```rust
// concerto-shell/src/providers/gemini.rs

pub struct GeminiSchemaConverter;

impl GeminiSchemaConverter {
    pub fn convert(manifest: &ToolManifest) -> serde_json::Value {
        json!({
            "name": manifest.name.replace('.', "_"),
            "description": manifest.description,
            "parameters": Self::convert_schema(&manifest.input_schema)
        })
    }

    fn convert_schema(schema: &RootSchema) -> serde_json::Value {
        let mut value = serde_json::to_value(schema).unwrap();
        Self::normalize_for_gemini(&mut value);
        value
    }

    fn normalize_for_gemini(value: &mut serde_json::Value) {
        if let Some(obj) = value.as_object_mut() {
            if let Some(types) = obj.get("type").and_then(|t| t.as_array()) {
                if types.len() > 1 {
                    let mut any_of = Vec::new();
                    for t in types {
                        if t != "null" {
                            any_of.push(json!({"type": t}));
                        }
                    }
                    obj.remove("type");
                    if !any_of.is_empty() {
                        obj.insert("anyOf".to_string(), json!(any_of));
                    }
                }
            }

            for (_, v) in obj.iter_mut() {
                Self::normalize_for_gemini(v);
            }
        }
    }
}
```

---

## 5. Validation Layer Architecture

### 5.1 Pre-Dispatch Validation Flow

```rust
// concerto-shell/src/executor.rs

use std::time::Instant;

pub struct ValidatingToolExecutor {
    registry: ToolRegistry,
    validator: SchemaValidator,
    policy: PolicyEngine,
    audit: AuditLog,
}

impl ValidatingToolExecutor {
    pub async fn execute(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        ctx: &mut ExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        // STEP 1: Tool existence check
        let manifest = self.registry.get(tool_name).ok_or_else(|| {
            ToolError::UnknownTool {
                name: tool_name.to_string(),
                available: self.registry.available_names(),
            }
        })?;

        // STEP 2: Schema validation (before any side effects)
        self.validator.validate(&manifest.input_schema, &args)
            .map_err(|e| ToolError::SchemaViolation {
                detail: format!("{}", e),
            })?;

        // STEP 3: Semantic validation (business rules)
        self.semantic_validate(manifest, &args)?;

        // STEP 4: Policy engine check
        self.policy.evaluate(tool_name, &args, ctx)?;

        // STEP 5: Effect class check
        if manifest.effect_class == EffectClass::Destructive && ctx.restricted_mode {
            if !args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Err(ToolError::PolicyDenied {
                    rule: "Destructive tools require dry_run: true in restricted mode".to_string(),
                });
            }
        }

        // STEP 6: Execute with timeout
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(manifest.timeout_ms),
            self.dispatch(tool_name, args.clone(), ctx)
        ).await.map_err(|_| ToolError::Timeout {
            tool: tool_name.to_string(),
            limit_ms: manifest.timeout_ms,
        })?;

        // STEP 7: Output validation
        let output = result?;
        let output_json = serde_json::to_value(&output)?;
        let output_bytes = output_json.to_string().len();

        if output_bytes > manifest.max_output_bytes {
            return Err(ToolError::OutputLimitExceeded {
                limit: manifest.max_output_bytes,
                actual: output_bytes,
            });
        }

        // STEP 8: Audit logging
        let duration_ms = start.elapsed().as_millis() as u64;
        self.audit.log(AuditEvent {
            tool_name: tool_name.to_string(),
            args: args.clone(),
            output_size: output_bytes,
            duration_ms,
            success: true,
            session_id: ctx.session_id,
        }).await?;

        Ok(output)
    }

    fn semantic_validate(
        &self,
        manifest: &ToolManifest,
        args: &serde_json::Value,
    ) -> Result<(), ToolError> {
        match manifest.name {
            "fs.read" | "fs.write" | "fs.search" | "fs.list" => {
                let path = args.get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidArgument {
                        field: "path".to_string(),
                        expected: "string".to_string(),
                        hint: "path is required and must be a string".to_string(),
                    })?;

                if !path.starts_with('/') && !path.starts_with("C:\") {
                    return Err(ToolError::InvalidArgument {
                        field: "path".to_string(),
                        expected: "absolute path".to_string(),
                        hint: format!(
                            "Path '{}' is not absolute. Use fs.list to discover absolute paths.",
                            path
                        ),
                    });
                }
            }
            "process.spawn" => {
                let cmd = args.get("cmd")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ToolError::InvalidArgument {
                        field: "cmd".to_string(),
                        expected: "array of strings".to_string(),
                        hint: "cmd must be an array like ["cargo", "build"], not a single string".to_string(),
                    })?;

                if cmd.is_empty() {
                    return Err(ToolError::InvalidArgument {
                        field: "cmd".to_string(),
                        expected: "non-empty array".to_string(),
                        hint: "cmd array must contain at least one element (the program name)".to_string(),
                    });
                }
            }
            _ => {}
        }

        Ok(())
    }
}
```

### 5.2 The `SchemaValidator` Implementation

```rust
// concerto-shell/src/validator.rs

use jsonschema::{JSONSchema, ValidationError};
use serde_json::Value;

pub struct SchemaValidator {
    cache: dashmap::DashMap<String, JSONSchema>,
}

impl SchemaValidator {
    pub fn new() -> Self {
        Self { cache: dashmap::DashMap::new() }
    }

    pub fn validate(
        &self,
        schema: &schemars::schema::RootSchema,
        value: &Value,
    ) -> Result<(), ValidationError<'static>> {
        let schema_json = serde_json::to_value(schema).unwrap();
        let schema_key = blake3::hash(&schema_json.to_string().as_bytes()).to_hex().to_string();

        let compiled = self.cache.entry(schema_key).or_insert_with(|| {
            JSONSchema::compile(&schema_json)
                .expect("Schema compilation failed — this is a build-time bug")
        }).clone();

        compiled.validate(value).map_err(|e| {
            let error = e.next();
            error.unwrap_or_else(|| ValidationError {
                instance: std::borrow::Cow::from(""),
                kind: jsonschema::ValidationErrorKind::Schema,
                instance_path: jsonschema::paths::Path::new(),
                schema_path: jsonschema::paths::Path::new(),
            })
        })
    }
}
```

---

## 6. Output Summarization Engine

### 6.1 Summarizer Architecture

```rust
// concerto-shell/src/summarizer.rs

use tree_sitter::{Language, Parser, Query, QueryCursor};

pub struct Summarizer {
    parsers: HashMap<String, Language>,
    queries: HashMap<String, Query>,
}

pub struct CodeSummary {
    pub language: String,
    pub signatures: Vec<String>,
    pub imports: Vec<String>,
    pub doc_comments: Vec<String>,
    pub struct_names: Vec<String>,
    pub enum_names: Vec<String>,
    pub trait_names: Vec<String>,
}

impl Summarizer {
    pub async fn summarize_code(&self, content: &str, path: &str) -> CodeSummary {
        let ext = Path::new(path).extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let language = match ext {
            "rs" => "rust",
            "py" => "python",
            "js" | "ts" | "tsx" => "typescript",
            "go" => "go",
            "java" => "java",
            _ => "text",
        };

        if language == "text" {
            return self.fallback_summary(content, path);
        }

        if let Some(lang) = self.parsers.get(language) {
            let mut parser = Parser::new();
            parser.set_language(*lang).unwrap();

            let tree = parser.parse(content, None).unwrap();
            let root = tree.root_node();

            let mut signatures = Vec::new();
            let mut imports = Vec::new();
            let mut doc_comments = Vec::new();

            let query = self.queries.get(language).unwrap();
            let mut cursor = QueryCursor::new();

            for match_ in cursor.matches(query, root, content.as_bytes()) {
                for capture in match_.captures {
                    let text = &content[capture.node.byte_range()];
                    match capture.index {
                        0 => signatures.push(text.to_string()),
                        1 => imports.push(text.to_string()),
                        2 => doc_comments.push(text.to_string()),
                        _ => {}
                    }
                }
            }

            CodeSummary {
                language: language.to_string(),
                signatures,
                imports,
                doc_comments,
                struct_names: Vec::new(),
                enum_names: Vec::new(),
                trait_names: Vec::new(),
            }
        } else {
            self.fallback_summary(content, path)
        }
    }

    fn fallback_summary(&self, content: &str, path: &str) -> CodeSummary {
        let lines: Vec<&str> = content.lines().collect();
        let first_lines: Vec<String> = lines.iter().take(5).map(|s| s.to_string()).collect();

        CodeSummary {
            language: "text".to_string(),
            signatures: first_lines,
            imports: Vec::new(),
            doc_comments: Vec::new(),
            struct_names: Vec::new(),
            enum_names: Vec::new(),
            trait_names: Vec::new(),
        }
    }
}
```

### 6.2 Output Tier Selection

```rust
// concerto-shell/src/output.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Full(Value),
    Summarized(Summary),
    Truncated { summary: Summary, full_ref: FileRef },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub description: String,
    pub key_points: Vec<String>,
    pub metadata: Value,
    pub full_ref: FileRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    pub store_id: String,
    pub content_hash: String,
    pub size_bytes: usize,
}

impl ToolOutput {
    pub async fn from_content(
        content: Value,
        manifest: &ToolManifest,
        ctx: &ToolContext,
    ) -> Result<Self, ToolError> {
        let json_str = serde_json::to_string(&content)?;
        let size = json_str.len();

        if size < 1024 {
            Ok(ToolOutput::Full(content))
        } else if size < manifest.max_output_bytes {
            let summary = ctx.summarizer.summarize_value(&content).await?;
            Ok(ToolOutput::Summarized(summary))
        } else {
            let summary = ctx.summarizer.summarize_value(&content).await?;
            let full_ref = ctx.memory.store_large_output(&manifest.name, &json_str).await?;
            Ok(ToolOutput::Truncated { summary, full_ref })
        }
    }

    pub fn to_llm_format(&self) -> Value {
        match self {
            ToolOutput::Full(v) => v.clone(),
            ToolOutput::Summarized(s) => serde_json::to_value(s).unwrap(),
            ToolOutput::Truncated { summary, full_ref } => {
                json!({
                    "summary": summary,
                    "full_content_available": true,
                    "retrieve_with": format!("memory.retrieve("{}")", full_ref.store_id),
                })
            }
        }
    }
}
```

---

## 7. Hierarchical Tool Namespace Loading

```rust
// concerto-shell/src/namespaces.rs

pub struct ToolWorkspace {
    active_namespaces: HashSet<String>,
    phase: PlanPhase,
    registry: Arc<ToolRegistry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanPhase {
    Exploration,
    Analysis,
    Coding,
    Validation,
    Deployment,
    Recovery,
}

impl ToolWorkspace {
    pub fn active_tools(&self) -> Vec<&ToolManifest> {
        let mut tools = Vec::new();
        tools.extend(self.registry.tools_in_namespace("shell"));

        match self.phase {
            PlanPhase::Exploration => {
                tools.extend(self.registry.tools_in_namespace("fs"));
                tools.extend(self.registry.tools_in_namespace("search"));
                tools.extend(self.registry.tools_in_namespace("git"));
            }
            PlanPhase::Analysis => {
                tools.extend(self.registry.tools_in_namespace("fs"));
                tools.extend(self.registry.tools_in_namespace("search"));
                tools.extend(self.registry.tools_in_namespace("lsp"));
            }
            PlanPhase::Coding => {
                tools.extend(self.registry.tools_in_namespace("fs"));
                tools.extend(self.registry.tools_in_namespace("process"));
                tools.extend(self.registry.tools_in_namespace("git"));
            }
            PlanPhase::Validation => {
                tools.extend(self.registry.tools_in_namespace("process"));
                tools.extend(self.registry.tools_in_namespace("net"));
                tools.extend(self.registry.tools_in_namespace("fs"));
            }
            PlanPhase::Deployment => {
                tools.extend(self.registry.tools_in_namespace("process"));
                tools.extend(self.registry.tools_in_namespace("archive"));
                tools.extend(self.registry.tools_in_namespace("net"));
            }
            PlanPhase::Recovery => {
                tools.extend(self.registry.tools_in_namespace("fs"));
                tools.extend(self.registry.tools_in_namespace("process"));
                tools.extend(self.registry.tools_in_namespace("git"));
                tools.extend(self.registry.tools_in_namespace("search"));
            }
        }

        tools
    }

    pub fn transition_to(&mut self, new_phase: PlanPhase) -> Vec<String> {
        let old_tools: HashSet<String> = self.active_tools()
            .iter().map(|t| t.name.to_string()).collect();

        self.phase = new_phase;

        let new_tools: HashSet<String> = self.active_tools()
            .iter().map(|t| t.name.to_string()).collect();

        new_tools.difference(&old_tools).cloned().collect()
    }
}
```

---

## 8. Idempotency & State Delta Protocol

```rust
// concerto-shell/src/idempotency.rs

pub trait IdempotentTool {
    fn compute_state_hash(&self, args: &serde_json::Value) -> Result<String, ToolError>;
    fn compute_state_diff(
        &self,
        expected_hash: &str,
        actual_hash: &str,
        args: &serde_json::Value,
    ) -> Result<Vec<DiffHunk>, ToolError>;
}

pub struct IdempotencyKey {
    pub tool: String,
    pub resource: String,
    pub hash: String,
}

impl IdempotencyKey {
    pub fn parse(key: &str) -> Result<Self, String> {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() != 3 {
            return Err("Invalid idempotency key format. Expected: tool:resource:hash".to_string());
        }
        Ok(Self {
            tool: parts[0].to_string(),
            resource: parts[1].to_string(),
            hash: parts[2].to_string(),
        })
    }
}
```

---

## 9. Observable Shell State Machine

```rust
// concerto-shell/src/state.rs

use std::sync::{Arc, RwLock};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellState {
    pub session_id: String,
    pub started_at: DateTime<Utc>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub last_exit_code: i32,
    pub last_tool_call: Option<ToolCallRecord>,
    pub open_files: Vec<OpenFileRecord>,
    pub running_processes: Vec<ProcessRecord>,
    pub active_namespaces: Vec<String>,
    pub current_phase: String,
    pub tool_call_count: usize,
    pub total_tokens_used: usize,
    pub errors_this_session: Vec<ErrorRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: Value,
    pub result_summary: String,
    pub duration_ms: u64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenFileRecord {
    pub path: String,
    pub opened_at: DateTime<Utc>,
    pub last_read_hash: String,
    pub last_write_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub status: ProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProcessStatus {
    Running,
    Exited { code: i32, at: DateTime<Utc> },
    Killed { signal: String, at: DateTime<Utc> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub tool_name: String,
    pub error_type: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
    pub recovered: bool,
}
```

---

## 10. Dry-Run & Confirmation Framework

```rust
// concerto-shell/src/dry_run.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunResult {
    pub would_succeed: bool,
    pub description: String,
    pub side_effects: Vec<SideEffect>,
    pub policy_check: PolicyCheckResult,
    pub idempotency_check: Option<IdempotencyCheckResult>,
    pub resource_estimate: ResourceEstimate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideEffect {
    pub type_: String,
    pub target: String,
    pub description: String,
    pub severity: SideEffectSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SideEffectSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheckResult {
    pub allowed: bool,
    pub matched_rule: Option<String>,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyCheckResult {
    pub expected_hash: String,
    pub current_hash: String,
    pub would_conflict: bool,
    pub diff_preview: Option<Vec<DiffHunk>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEstimate {
    pub bytes_written: usize,
    pub bytes_read: usize,
    pub processes_spawned: usize,
    pub estimated_duration_ms: u64,
}
```

---

## 11. Cross-Platform Tool Crate Specifications

### 11.1 `tool-git` — Pure Rust via `git2`

```rust
// tool-git/src/lib.rs

use git2::{Repository, StatusOptions};

pub struct GitStatusTool;

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct GitStatusArgs {
    #[schemars(description = "Absolute path to git repository root. Must contain .git directory.")]
    pub path: String,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct GitStatusOutput {
    pub branch: String,
    pub ahead: i32,
    pub behind: i32,
    pub is_clean: bool,
    pub changes: Vec<GitChange>,
    pub untracked: Vec<String>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct GitChange {
    pub path: String,
    pub status: String,
    pub staged: bool,
}

impl GitStatusTool {
    pub async fn execute(args: GitStatusArgs) -> ToolResult<GitStatusOutput> {
        let repo = Repository::open(&args.path).map_err(|e| ToolError::Io {
            path: args.path.clone(),
            message: format!("Not a git repository: {}", e),
        })?;

        let head = repo.head()?;
        let branch = head.shorthand().unwrap_or("HEAD").to_string();

        let mut status_opts = StatusOptions::new();
        status_opts.include_untracked(true);
        let statuses = repo.statuses(Some(&mut status_opts))?;

        let mut changes = Vec::new();
        let mut untracked = Vec::new();
        let mut is_clean = true;

        for entry in statuses.iter() {
            let status = entry.status();
            let path = entry.path().unwrap_or("").to_string();

            if status.is_index_new() || status.is_wt_new() {
                untracked.push(path.clone());
            }

            let change_status = if status.is_index_modified() || status.is_wt_modified() {
                "modified"
            } else if status.is_index_deleted() || status.is_wt_deleted() {
                "deleted"
            } else if status.is_conflicted() {
                "conflicted"
            } else { "other" };

            changes.push(GitChange {
                path,
                status: change_status.to_string(),
                staged: status.is_index_modified() || status.is_index_new(),
            });
            is_clean = false;
        }

        let (ahead, behind) = if let Ok(branch_ref) = repo.find_branch(&branch, git2::BranchType::Local) {
            if let Some(upstream) = branch_ref.upstream().ok().and_then(|u| u.get().target()) {
                let local = head.target().unwrap();
                repo.graph_ahead_behind(local, upstream).unwrap_or((0, 0))
            } else { (0, 0) }
        } else { (0, 0) };

        Ok(GitStatusOutput {
            branch,
            ahead: ahead as i32,
            behind: behind as i32,
            is_clean,
            changes,
            untracked,
        })
    }
}
```

### 11.2 `tool-process` — Cross-Platform

```rust
// tool-process/src/lib.rs

use tokio::process::Command;
use sysinfo::{System, SystemExt, ProcessExt};

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct ProcessSpawnArgs {
    #[schemars(description = "Command and arguments as array. First element is program name.")]
    pub cmd: Vec<String>,

    #[schemars(description = "Working directory. Must be absolute path. Null uses current directory.")]
    pub cwd: Option<String>,

    #[schemars(description = "Environment variables to set. Null for inherited env.")]
    pub env: Option<HashMap<String, String>>,

    #[schemars(description = "Timeout in milliseconds. Default 30000. Max 300000.")]
    pub timeout: Option<u64>,
}

#[derive(JsonSchema, Serialize, Deserialize, Debug, Clone)]
pub struct ProcessSpawnOutput {
    pub pid: u32,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

impl ProcessSpawnTool {
    pub async fn execute(args: ProcessSpawnArgs) -> ToolResult<ProcessSpawnOutput> {
        let mut cmd = Command::new(&args.cmd[0]);
        cmd.args(&args.cmd[1..]);

        if let Some(cwd) = args.cwd { cmd.current_dir(cwd); }
        if let Some(env) = args.env { cmd.envs(env); }

        let timeout = args.timeout.unwrap_or(30000);
        let start = Instant::now();

        let output = tokio::time::timeout(
            Duration::from_millis(timeout),
            cmd.output()
        ).await;

        let timed_out = output.is_err();
        let output = output.unwrap_or_else(|_| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: b"Process timed out".to_vec(),
        })?;

        let duration_ms = start.elapsed().as_millis() as u64;
        let max_output = 10000;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let stdout = if stdout.len() > max_output {
            format!("{}... [truncated, {} bytes total]", &stdout[..max_output], stdout.len())
        } else { stdout.to_string() };

        let stderr = if stderr.len() > max_output {
            format!("{}... [truncated, {} bytes total]", &stderr[..max_output], stderr.len())
        } else { stderr.to_string() };

        Ok(ProcessSpawnOutput {
            pid: 0,
            stdout,
            stderr,
            exit_code: output.status.code().unwrap_or(-1),
            duration_ms,
            timed_out,
        })
    }
}
```

---

## 12. Single Binary Distribution Strategy

### 12.1 Build Configuration

```toml
# Cargo.toml (top-level)
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"

[workspace]
members = [
    "crates/shell",
    "crates/tool-fs",
    "crates/tool-process",
    "crates/tool-git",
    "crates/tool-search",
    "crates/tool-archive",
    "crates/tool-net",
]
```

### 12.2 Embedded Assets

```rust
// concerto-shell/src/assets.rs

pub struct EmbeddedAssets;

impl EmbeddedAssets {
    pub fn default_policy_rules() -> &'static str {
        include_str!("../../assets/default-policy.toml")
    }

    pub fn restricted_profile() -> &'static str {
        include_str!("../../assets/profiles/restricted.toml")
    }

    pub fn tool_schemas() -> HashMap<String, &'static str> {
        let mut schemas = HashMap::new();
        schemas.insert("fs.read".to_string(), include_str!("../../assets/schemas/fs.read.json"));
        schemas.insert("fs.write".to_string(), include_str!("../../assets/schemas/fs.write.json"));
        schemas.insert("fs.search".to_string(), include_str!("../../assets/schemas/fs.search.json"));
        schemas.insert("git.status".to_string(), include_str!("../../assets/schemas/git.status.json"));
        schemas.insert("process.spawn".to_string(), include_str!("../../assets/schemas/process.spawn.json"));
        schemas
    }
}
```

### 12.3 Cross-Compilation Script

```bash
#!/bin/bash
# scripts/build-release.sh
set -e

TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
    "x86_64-apple-darwin"
    "aarch64-apple-darwin"
    "x86_64-pc-windows-msvc"
)

VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)

for target in "${TARGETS[@]}"; do
    echo "Building for $target..."
    rustup target add "$target" 2>/dev/null || true
    cargo build --release --target "$target" --bin concerto

    mkdir -p "dist/$target"
    if [[ "$target" == *"windows"* ]]; then
        cp "target/$target/release/concerto.exe" "dist/$target/"
        cd "dist/$target" && zip "concerto-$VERSION-$target.zip" concerto.exe
    else
        cp "target/$target/release/concerto" "dist/$target/"
        cd "dist/$target" && tar czf "concerto-$VERSION-$target.tar.gz" concerto
    fi
    cd ../..
done

echo "Build complete. Artifacts in dist/"
```

---

## 13. Multi-Agent Validation Hooks

```rust
// orchestrator/src/validation.rs

pub struct ValidatorAgent {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
}

impl ValidatorAgent {
    pub async fn validate(
        &self,
        tool_call: &ToolCall,
        ctx: &ExecutionContext,
    ) -> ValidationResult {
        let manifest = match self.registry.get(&tool_call.name) {
            Some(m) => m,
            None => return ValidationResult::Reject {
                reason: format!("Unknown tool: {}", tool_call.name),
                hint: format!("Available tools: {:?}", self.registry.available_names()),
            },
        };

        let prompt = format!(r#"You are a strict tool call validator. Review this tool call for correctness.

TOOL MANIFEST:
Name: {}
Description: {}
Input Schema: {}

PROPOSED CALL:
{}

Respond with EXACTLY one of:
- APPROVED
- REJECT: [reason] | HINT: [corrective action]"#,
            manifest.name,
            manifest.description,
            serde_json::to_string_pretty(&manifest.input_schema).unwrap(),
            serde_json::to_string_pretty(&tool_call.args).unwrap(),
        );

        let response = self.provider.complete_text(&prompt).await?;

        if response.trim() == "APPROVED" {
            ValidationResult::Approve
        } else if response.starts_with("REJECT:") {
            let parts: Vec<&str> = response.split("| HINT:").collect();
            ValidationResult::Reject {
                reason: parts[0].replace("REJECT:", "").trim().to_string(),
                hint: parts.get(1).unwrap_or(&"").trim().to_string(),
            }
        } else {
            ValidationResult::Approve
        }
    }
}

pub enum ValidationResult {
    Approve,
    Reject { reason: String, hint: String },
}
```

---

## 14. Hallucination Benchmark Specification

```rust
// eval-runner/src/hallucination_benchmark.rs

pub struct HallucinationBenchmark {
    registry: ToolRegistry,
    executor: ValidatingToolExecutor,
}

pub const HALLUCINATION_TESTS: &[HallucinationTest] = &[
    HallucinationTest {
        name: "unknown_tool_rejection",
        tool_call: ToolCall {
            name: "fs.copy".to_string(),
            args: json!({"src": "/a", "dst": "/b"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::InventedTool,
    },
    HallucinationTest {
        name: "invented_parameter_rejection",
        tool_call: ToolCall {
            name: "fs.read".to_string(),
            args: json!({"path": "/etc/passwd", "encoding": "base64"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::InventedParameter,
    },
    HallucinationTest {
        name: "wrong_type_rejection",
        tool_call: ToolCall {
            name: "fs.read".to_string(),
            args: json!({"path": 123, "offset": "5"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::TypeMismatch,
    },
    HallucinationTest {
        name: "relative_path_rejection",
        tool_call: ToolCall {
            name: "fs.read".to_string(),
            args: json!({"path": "./README.md"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::SemanticViolation,
    },
    HallucinationTest {
        name: "missing_required_rejection",
        tool_call: ToolCall {
            name: "fs.write".to_string(),
            args: json!({"path": "/tmp/test.txt"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::MissingRequired,
    },
    HallucinationTest {
        name: "command_string_rejection",
        tool_call: ToolCall {
            name: "process.spawn".to_string(),
            args: json!({"cmd": "rm -rf /"}),
        },
        expected: Expectation::RejectWithHint,
        category: HallucinationCategory::TypeMismatch,
    },
];

impl HallucinationBenchmark {
    pub async fn run(&self) -> BenchmarkReport {
        let mut passed = 0;
        let mut failed = 0;
        let mut results = Vec::new();

        for test in HALLUCINATION_TESTS {
            let result = self.executor.execute(
                &test.tool_call.name,
                test.tool_call.args.clone(),
                &mut ExecutionContext::test(),
            ).await;

            let test_passed = match (&test.expected, &result) {
                (Expectation::RejectWithHint, Err(ToolError::UnknownTool { .. })) => true,
                (Expectation::RejectWithHint, Err(ToolError::InvalidArgument { .. })) => true,
                (Expectation::RejectWithHint, Err(ToolError::SchemaViolation { .. })) => true,
                (Expectation::RejectWithHint, Err(ToolError::PolicyDenied { .. })) => true,
                (Expectation::Success, Ok(_)) => true,
                _ => false,
            };

            if test_passed { passed += 1; } else { failed += 1; }

            results.push(TestResult {
                name: test.name.to_string(),
                passed: test_passed,
                expected: format!("{:?}", test.expected),
                actual: format!("{:?}", result),
                category: test.category,
            });
        }

        BenchmarkReport {
            total: HALLUCINATION_TESTS.len(),
            passed,
            failed,
            pass_rate: passed as f64 / HALLUCINATION_TESTS.len() as f64,
            results,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pass_rate: f64,
    pub results: Vec<TestResult>,
}
```

---

## Appendix A: Complete Tool Registry at Launch

| Tool | Namespace | EffectClass | Replaces |
|------|-----------|-------------|----------|
| `fs.read` | fs | ReadOnly | `cat` |
| `fs.write` | fs | Destructive | `echo > file` |
| `fs.search` | fs | ReadOnly | `grep -r` |
| `fs.list` | fs | ReadOnly | `ls -la` |
| `fs.diff` | fs | ReadOnly | `diff` |
| `fs.glob` | fs | ReadOnly | `find` |
| `fs.delete` | fs | Destructive | `rm` |
| `fs.mkdir` | fs | Idempotent | `mkdir -p` |
| `process.spawn` | process | External | `sh -c` |
| `process.list` | process | ReadOnly | `ps` |
| `process.kill` | process | Destructive | `kill` |
| `process.env` | process | ReadOnly | `env` |
| `git.status` | git | ReadOnly | `git status` |
| `git.log` | git | ReadOnly | `git log` |
| `git.diff` | git | ReadOnly | `git diff` |
| `git.branch` | git | ReadOnly | `git branch` |
| `git.commit` | git | Destructive | `git commit` |
| `search.content` | search | ReadOnly | `grep` (content) |
| `search.symbols` | search | ReadOnly | `ctags` |
| `archive.extract` | archive | Destructive | `unzip`, `tar -x` |
| `archive.create` | archive | Destructive | `zip`, `tar -c` |
| `net.http` | net | External | `curl` |
| `net.ping` | net | External | `ping` |
| `shell.state` | shell | ReadOnly | `pwd`, `env`, `ps` |
| `shell.history` | shell | ReadOnly | `history` |

---

## Appendix B: Error Message Templates

All validation errors follow this format to maximize LLM self-correction:

```
ERROR: {error_type}
Tool: {tool_name}
Field: {field_name}
Expected: {expected_value}
Received: {received_value}
Hint: {corrective_action}
Available: {list_of_valid_options}
```

Example:
```
ERROR: InvalidArgument
Tool: fs.read
Field: path
Expected: absolute path starting with '/' or 'C:\'
Received: "./src/main.rs"
Hint: Use fs.list to discover absolute paths, or convert to absolute using the current working directory.
Current working directory: /home/user/projects/concerto
Suggested: "/home/user/projects/concerto/src/main.rs"
```

---

*Expanded research document with concrete implementations for Concerto v0.2.0. All code is reference-quality and designed to be adapted into the existing crate structure.*
