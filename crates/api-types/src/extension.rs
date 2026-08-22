//! Shared extension types for skills and MCP clients (ADR-43).
//!
//! These are plain serde data types used across desktop, CLI, and
//! orchestrator. The TOML-shaped configuration structs (`SkillsConfig`,
//! `McpConfig`, `McpServerConfig`) live in `concerto-config`; this module
//! holds the domain-level types exchanged between extension backends and the
//! UI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A skill pack manifest (`skill.toml` / `SKILL.md` front matter).
///
/// Instructions are supplied either in a file (`instructions_path`, relative
/// to the skill pack root) or inline (`instructions`); at most one should be
/// set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillManifest {
    /// Stable skill id used for enable/disable and deduplication.
    pub id: String,
    /// Human-readable skill name.
    pub name: String,
    /// Semantic version of the skill pack.
    pub version: String,
    /// One-paragraph description shown in the Extensions UI.
    pub description: String,
    /// Path to the instructions file, relative to the pack root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_path: Option<PathBuf>,
    /// Inline instruction text (alternative to `instructions_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Tools this skill suggests. Prompt-level hints only in v1; no policy
    /// stripping of non-listed tools.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Resource files bundled with the skill pack.
    #[serde(default)]
    pub resources: Vec<PathBuf>,
}

/// A loaded skill: its manifest plus the resolved instruction text and
/// absolute resource paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillDescriptor {
    /// Stable skill id (mirrors `manifest.id`).
    pub id: String,
    /// The parsed skill manifest.
    pub manifest: SkillManifest,
    /// Resolved instruction text (loaded from `instructions_path` or inline).
    pub instructions: String,
    /// Absolute paths to the skill's resource files.
    #[serde(default)]
    pub resource_paths: Vec<PathBuf>,
}

/// MCP transport protocol. Stdio is the only transport in v1; SSE is
/// feature-gated later.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum McpTransport {
    /// Spawn the server as a child process speaking JSON-RPC over stdin/stdout.
    Stdio,
}

/// A remote tool exposed by an MCP server, mirroring the `tools/list` result
/// shape (`name`, `description`, `inputSchema`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolDescriptor {
    /// Tool name as reported by the server (unqualified).
    pub name: String,
    /// Human-readable description, when the server provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// Extension kind tags used by the unified Extensions UI surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionKind {
    /// WASM plugin (existing `PluginManager` backend).
    Plugin,
    /// Local filesystem skill pack (`SkillManager`).
    Skill,
    /// MCP server (`McpManager`).
    Mcp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_manifest_round_trip_with_instructions_path() {
        let manifest = SkillManifest {
            id: "rust-testing".into(),
            name: "Rust Testing".into(),
            version: "1.0.0".into(),
            description: "Cargo verification guidance".into(),
            instructions_path: Some(PathBuf::from("instructions.md")),
            instructions: None,
            tools: vec!["cargo test".into(), "cargo clippy".into()],
            resources: vec![PathBuf::from("fixtures/sample.rs")],
        };
        let json = serde_json::to_string(&manifest).expect("serialization should succeed");
        let deserialized: SkillManifest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, manifest);
    }

    #[test]
    fn skill_manifest_round_trip_with_inline_instructions() {
        let manifest = SkillManifest {
            id: "commit-style".into(),
            name: "Commit Style".into(),
            version: "0.2.0".into(),
            description: "Conventional commits guidance".into(),
            instructions_path: None,
            instructions: Some("Prefer conventional commit messages.".into()),
            tools: vec![],
            resources: vec![],
        };
        let json = serde_json::to_string(&manifest).expect("serialization should succeed");
        let deserialized: SkillManifest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, manifest);
    }

    #[test]
    fn skill_descriptor_round_trip() {
        let descriptor = SkillDescriptor {
            id: "rust-testing".into(),
            manifest: SkillManifest {
                id: "rust-testing".into(),
                name: "Rust Testing".into(),
                version: "1.0.0".into(),
                description: "Cargo verification guidance".into(),
                instructions_path: Some(PathBuf::from("instructions.md")),
                instructions: None,
                tools: vec!["cargo test".into()],
                resources: vec![PathBuf::from("fixtures/sample.rs")],
            },
            instructions: "Prefer cargo nextest.".into(),
            resource_paths: vec![PathBuf::from("/abs/fixtures/sample.rs")],
        };
        let json = serde_json::to_string(&descriptor).expect("serialization should succeed");
        let deserialized: SkillDescriptor =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, descriptor);
    }

    #[test]
    fn mcp_transport_serde_is_lowercase() {
        let json =
            serde_json::to_string(&McpTransport::Stdio).expect("serialization should succeed");
        assert_eq!(json, "\"stdio\"");
        let parsed: McpTransport =
            serde_json::from_str("\"stdio\"").expect("deserialization should succeed");
        assert_eq!(parsed, McpTransport::Stdio);
    }

    #[test]
    fn mcp_tool_descriptor_round_trip_and_defaults() {
        let tool = McpToolDescriptor {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        let json = serde_json::to_string(&tool).expect("serialization should succeed");
        let deserialized: McpToolDescriptor =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized, tool);

        // description is optional; input_schema defaults to null.
        let minimal: McpToolDescriptor =
            serde_json::from_str(r#"{"name":"list"}"#).expect("deserialization should succeed");
        assert_eq!(minimal.description, None);
        assert_eq!(minimal.input_schema, serde_json::Value::Null);
    }

    #[test]
    fn extension_kind_serde_is_lowercase() {
        for (kind, name) in [
            (ExtensionKind::Plugin, "plugin"),
            (ExtensionKind::Skill, "skill"),
            (ExtensionKind::Mcp, "mcp"),
        ] {
            let json = serde_json::to_string(&kind).expect("serialization should succeed");
            assert_eq!(json, format!("\"{name}\""));
            let parsed: ExtensionKind = serde_json::from_str(&format!("\"{name}\""))
                .expect("deserialization should succeed");
            assert_eq!(parsed, kind);
        }
    }
}
