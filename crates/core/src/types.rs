//! Placeholder domain types referenced by the Phase 0 trait contracts.
//!
//! These exist only so the traits in `traits/` compile. Their real shape
//! belongs to whichever phase owns the concept (e.g. `CompletionRequest`
//! is Phase 1's, `CapabilitySet` is Phase 2's). Treat every struct in this
//! file as provisional — expect fields to be added, not the types to be
//! renamed, so downstream phases shouldn't need a rewrite, just extension.

use crate::error::ProviderError;
use crate::ids::Ulid;
use camino::Utf8PathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

// ---- providers (Phase 1) --------------------------------------------------

/// Controls whether and which tool an LLM must call on a given turn.
///
/// Maps to each provider's native `tool_choice` parameter:
/// - OpenAI: `"auto" | "none" | "required" | {"type":"function","function":{"name":"..."}}`
/// - Anthropic: `{"type":"auto"} | {"type":"none"} | {"type":"any"} | {"type":"tool","name":"..."}`
/// - Google: `functionCallingConfig.mode = "AUTO" | "NONE" | "ANY"` (+ `allowedFunctionNames`)
/// - Ollama: not supported (falls back to auto)
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ToolChoice {
    /// Model decides whether to call a tool (default behavior).
    Auto,
    /// No tool calls allowed this turn.
    None,
    /// Model must call at least one tool from the available set.
    Required,
    /// Model must call the specified tool by name.
    Forced(String),
}

#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    /// Controls tool-calling behavior. `None` is treated as `Auto`.
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u64>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    /// The function/tool name that produced this result.
    ///
    /// Required by providers like Google Gemini that use `functionResponse.name`
    /// to match results to their function declarations. OpenAI uses `tool_call_id`
    /// (matching `ToolCall.id`) instead, so this field is provider-dependent.
    pub name: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_results: Option<Vec<ToolResult>>,
    /// Model-authored reasoning/thinking text (e.g. DeepSeek `reasoning_content`).
    ///
    /// ADR-46: captured at the stream boundary and echoed back to the provider
    /// on the OpenAI-compatible path. `#[serde(default)]` keeps old persisted
    /// JSON (without this field) deserializable.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Provider-reported input token usage for this message (ADR-48 §4).
    ///
    /// `None` means usage is unknown and the estimator heuristic applies
    /// (`measured beats estimate` policy). `#[serde(default)]` keeps old
    /// persisted JSON (without this field) deserializable. The sessions layer
    /// stores `0` for `None` because the column is `NOT NULL DEFAULT 0`
    /// (see `crates/sessions/src/lib.rs`).
    ///
    /// Matches [`concerto_core::types::ProviderMetrics::tokens_in`] (u64) so
    /// provider-reported counts flow through without truncation.
    #[serde(default)]
    pub tokens_in: Option<u64>,
    /// Provider-reported output token usage for this message (ADR-48 §4).
    ///
    /// Mirrors [`Self::tokens_in`]: `None` = unknown (use the estimator),
    /// `#[serde(default)]` keeps legacy rows readable.
    #[serde(default)]
    pub tokens_out: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    pub delta: String,
    pub reasoning: Option<String>,
    pub tool_call: Option<ToolCall>,
    /// Whether this chunk is the terminal chunk of the stream.
    pub is_final: bool,
    /// Provider-reported usage, present only on the final chunk.
    ///
    /// ADR-48 §4: the connector attaches `Some(usage)` to the final chunk when
    /// the provider reports it (OpenAI `usage` object); intermediate chunks
    /// always carry `None`. The orchestrator uses this to record real token
    /// counts on the persisted assistant message.
    pub usage: Option<CompletionUsage>,
}

impl CompletionChunk {
    /// The ADR-53 completion keepalive — a non-terminal liveness marker.
    ///
    /// ADR-53 §4 has a plugin-backed provider emit this chunk on the manifest's
    /// `heartbeat_interval_secs` while the host awaits a slow plugin
    /// completion. It carries an empty `delta`, no reasoning/tool-call/usage,
    /// and `is_final: false`, so downstream collectors (orchestrator,
    /// `host_fns`, metered streams) treat it as a no-op non-terminal chunk —
    /// the same shape the SSE parsers already emit for transport keepalives.
    ///
    /// The ADR names this `CompletionChunk::KeepAlive` (a "new additive
    /// variant"); `CompletionChunk` is a plain struct today, so the additive
    /// representation is this constructor, not an enum variant.
    pub fn keepalive() -> Self {
        Self {
            delta: String::new(),
            reasoning: None,
            tool_call: None,
            is_final: false,
            usage: None,
        }
    }
}

/// Provider-reported token usage for one completion (ADR-48 §4).
///
/// Mirrors the OpenAI chat-completions `usage` object. All fields are
/// optional because providers differ in what they report (e.g. reasoning-only
/// endpoints may omit `completion_tokens`, and `total_tokens` is derived).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompletionUsage {
    /// Input (prompt) tokens attributed to this completion.
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Output (completion) tokens attributed to this completion.
    #[serde(default)]
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenBudget {
    pub capacity: u64,
    pub reserved_for_response: u64,
    pub available: u64,
}

impl TokenBudget {
    pub fn new(capacity: u64, reserved_for_response: u64) -> Self {
        let available = capacity.saturating_sub(reserved_for_response);
        Self { capacity, reserved_for_response, available }
    }

    pub fn reserve(&mut self, n: u64) -> Result<u64, ProviderError> {
        if n > self.available {
            return Err(ProviderError::ContextOverflow { tokens_in: n, capacity: self.capacity });
        }
        self.available -= n;
        Ok(self.available)
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderMetrics {
    pub provider: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub latency_ms: u64,
}

// ---- tools / policy (Phase 2) --------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CapabilitySet {
    requirements: Vec<String>,
}

impl CapabilitySet {
    pub fn filesystem(path_globs: Vec<String>, write: bool) -> Self {
        Self { requirements: vec![format!("filesystem(globs={path_globs:?}, write={write})")] }
    }

    pub fn shell(allowed_patterns: Vec<String>, working_dir: String) -> Self {
        Self {
            requirements: vec![format!("shell(patterns={allowed_patterns:?}, cwd={working_dir})")],
        }
    }

    pub fn is_subset(&self, other: &CapabilitySet) -> bool {
        self.requirements.iter().all(|r| other.requirements.contains(r))
    }

    /// Returns `true` if `self` contains all requirements from `other`.
    pub fn is_superset(&self, other: &CapabilitySet) -> bool {
        other.is_subset(self)
    }

    /// Append a capability requirement string, returning self for chaining.
    ///
    /// Requirements are plain strings like `"filesystem(globs=[\"**\"], write=true)"`.
    /// Use the typed constructors (`filesystem`, `shell`) for common cases; use this
    /// method for testing or dynamic capability sets.
    pub fn with_requirement(mut self, requirement: &str) -> Self {
        self.requirements.push(requirement.to_string());
        self
    }

    /// Returns `true` when the set holds no requirement strings.
    pub fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub summary: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RollbackSnapshot {
    pub tool_name: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_id: Ulid,
    pub project_id: ProjectId,
    pub project_dir: std::path::PathBuf,
    pub user_prefs: HashMap<String, String>,
}

impl SessionContext {
    pub fn new(session_id: Ulid, project_dir: std::path::PathBuf) -> Self {
        Self {
            session_id,
            project_id: ProjectId::resolve(&project_dir),
            project_dir,
            user_prefs: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
/// A tool invocation presented to the policy engine for evaluation.
///
/// `tool_name` and `input` are borrowed with lifetime `'a` for zero-copy
/// evaluation on the hot path. This is good for performance but limits
/// ergonomics (actions are not freely storable or movable across async
/// boundaries); that trade-off is accepted for the eval path.
pub struct PolicyAction<'a> {
    pub tool_name: &'a str,
    pub input: &'a serde_json::Value,
    pub session_id: Ulid,
    pub correlation_id: Ulid,
    pub capability_requirements: CapabilitySet,
    /// Active sandbox profile for this execution context.
    /// `None` = `SandboxProfile::None` (no sandboxing).
    pub sandbox_profile: Option<SandboxProfile>,
    /// Estimated cost of this operation in USD (for spend cap checking).
    pub estimated_cost_usd: Option<f64>,
    /// ADR-28 §6: structured, pre-resolved facts about a command execution
    /// (resolved executable, argv, working directory, etc.). `None` for tools
    /// that do not involve command execution (e.g. filesystem, http) or when
    /// the producing tool has not populated them. Kept optional so existing
    /// call sites and non-shell tools are unaffected.
    pub command_facts: Option<CommandPolicyFacts>,
}

// ---- ADR-28 §6: structured command-policy facts ----------------------------

/// Filesystem access scope classification for a command (ADR-28 §6).
///
/// Heuristic, populated by the producing tool; used by the policy engine and
/// audit log. `Unknown` is the zero-value so a missing classification never
/// silently asserts a safe scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FilesystemScope {
    #[default]
    Unknown,
    None,
    ReadOnly,
    ProjectOnly,
    ProjectAndTemp,
    Anywhere,
}

impl FilesystemScope {
    /// Classify a working directory relative to a project root.
    pub fn classify_for(cwd: Option<&Path>, project_dir: &Path) -> Self {
        match cwd {
            None => Self::Unknown,
            Some(p) if p == project_dir => Self::ProjectOnly,
            Some(p) if p.starts_with(project_dir) => Self::ProjectOnly,
            Some(_) => Self::Anywhere,
        }
    }
}

/// Destructive-operation classification for a command (ADR-28 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DestructiveClass {
    #[default]
    Unknown,
    NonDestructive,
    Modifying,
    Destructive,
}

impl DestructiveClass {
    /// Best-effort classification from a command string.
    pub fn classify_command(cmd: &str) -> Self {
        let lower = cmd.to_ascii_lowercase();
        if ["rm ", "dd ", "mkfs", "format", "shred", "truncate ", ":(){"]
            .iter()
            .any(|p| lower.contains(p))
        {
            return Self::Destructive;
        }
        if ["mv ", "cp ", "touch ", "chmod", "chown", "ln ", "write", "create", "delete", "remove"]
            .iter()
            .any(|p| lower.contains(p))
        {
            return Self::Modifying;
        }
        Self::NonDestructive
    }
}

/// Structured, pre-resolved facts about a command execution, presented to the
/// policy engine and audit log (ADR-28 §6/§7).
///
/// Populated by command-executing tools (e.g. the shell tool) *before* policy
/// evaluation so a managed or custom environment cannot become a policy bypass
/// by hiding behind a raw command string. The executable and argv describe the
/// process Concerto will actually spawn; for shell-wrapped execution the command
/// itself is retained as the launcher argument.
///
/// `None` for tools that do not involve command execution (e.g. filesystem,
/// http) or when not populated.
#[derive(Debug, Clone, Default)]
pub struct CommandPolicyFacts {
    /// Identifier of the shell/profile that produced this command, if any.
    pub shell_profile_id: Option<String>,
    /// The best-effort resolved executable Concerto will actually spawn.
    pub resolved_executable: Option<PathBuf>,
    /// The full argv (`program` + args) after resolution.
    pub argv: Vec<String>,
    /// Working directory the command runs in.
    pub working_directory: Option<PathBuf>,
    /// Whether the command is expected to make network egress.
    pub network_requested: bool,
    /// Filesystem access scope classification.
    pub filesystem_scope: FilesystemScope,
    /// Destructive-operation classification.
    pub destructive_classification: DestructiveClass,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PolicyVerdict {
    Allow,
    Deny,
    RequireApproval { timeout: std::time::Duration },
    RequireApprovalWithTimeout { timeout: std::time::Duration },
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PolicyRule {
    AutoApprove(Condition),
    AutoDeny(Condition),
    RequireApproval(Condition),
    RequireApprovalWithTimeout {
        condition: Condition,
        timeout_secs: u64,
    },
    /// ADR-28: when a managed-toolchain shell profile is active, commands that
    /// would mutate or update the managed toolchain must be approved before
    /// running.
    RequireManagedToolApproval(Condition),
    /// ADR-28: any operation that changes the toolchain (install/update/remove
    /// of tooling) requires explicit approval regardless of sandbox profile.
    RequireToolchainApproval(Condition),
    /// ADR-28: deny network egress for the matched tool/operation even when no
    /// sandbox profile is active (e.g. a `shell` or `http` call that reaches
    /// the network). Non-network operations matched by the condition are
    /// unaffected — the rule only applies to actual egress.
    DenyNetworkEgress(Condition),
}

/// Category of code for policy-based trust domains.
///
/// Used by `Condition::CodeType` to match operations against the
/// semantic category of the code being modified. The classifier is
/// heuristic (path- and keyword-based) — not LLM-dependent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CodeCategory {
    /// Code under `tests/`, `*_test.rs`, `*.spec.rs`, or similar.
    Test,
    /// Paths under `auth/`, `session/`, or files referencing
    /// `password`, `token`, `jwt`, `credential`, `apikey`.
    Auth,
    /// Database migration files under `migrations/`.
    Migration,
    /// Configuration files: `*.toml`, `*.yaml`, `*.yml`, `*.json`,
    /// `*.env`, `Dockerfile`, `Makefile`, etc.
    Config,
    /// Everything that does not match the above categories.
    Other,
}

impl CodeCategory {
    /// Classify a file path into a heuristic `CodeCategory`.
    ///
    /// This is a deterministic, zero-dependency classifier — no LLM or
    /// parsing involved. Used by `Condition::CodeType` for policy matching.
    pub fn classify(path: &str) -> Self {
        let path_lower = path.to_lowercase();

        // Test files
        if path_lower.contains("test")
            || path_lower.ends_with("_test.rs")
            || path_lower.ends_with(".spec.rs")
            || path_lower.ends_with(".test.ts")
            || path_lower.ends_with(".test.js")
            || path_lower.contains("/tests/")
            || path_lower.contains("__tests__")
        {
            return Self::Test;
        }

        // Auth / session / credential patterns
        if path_lower.contains("/auth/")
            || path_lower.contains("/session/")
            || path_lower.contains("/sessions/")
            || path_lower.contains("password")
            || path_lower.contains("token")
            || path_lower.contains("jwt")
            || path_lower.contains("credential")
            || path_lower.contains("apikey")
            || path_lower.contains("api_key")
            || path_lower.contains("secret")
            || path_lower.contains("oauth")
            || path_lower.contains("login")
        {
            return Self::Auth;
        }

        // Migration files
        if path_lower.contains("/migrations/") || path_lower.ends_with(".sql") {
            return Self::Migration;
        }

        // Config files by extension
        if path_lower.ends_with(".toml")
            || path_lower.ends_with(".yaml")
            || path_lower.ends_with(".yml")
            || path_lower.ends_with(".json")
            || path_lower.ends_with(".env")
            || path_lower.ends_with("dockerfile")
            || path_lower.ends_with("makefile")
            || path_lower.ends_with(".ini")
            || path_lower.ends_with(".cfg")
            || path_lower.ends_with(".conf")
            || path_lower.contains("/config/")
            || path_lower.contains(".github/")
            || path_lower.contains(".gitlab/")
        {
            return Self::Config;
        }

        Self::Other
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Condition {
    ToolName(String),
    /// Match any tool whose name starts with the given prefix.
    ///
    /// ADR-43 §6: enables server-level MCP rules such as "every
    /// `mcp:github:*` tool requires approval" without enumerating each
    /// remote tool. First-match-wins ordering still applies, so a narrower
    /// rule placed earlier in the rule list overrides a broader prefix rule.
    ToolNamePrefix(String),
    PathGlob(String),
    CommandPattern(String),
    /// Match the exact value of a tool input's `operation` field.
    Operation(String),
    /// Legacy Git-specific operation condition. New configuration should use
    /// `All([ToolName("git"), Operation(...)])` so the tool scope is explicit.
    GitOperation(String),
    Capability(CapabilitySet),
    /// Match operations against the heuristic `CodeCategory` of the
    /// file being modified (test, auth, migration, config, other).
    CodeType(CodeCategory),
    /// Match operations whose file contents (from the input's `content`
    /// or `path`'s resolved content) match the given regex pattern.
    /// Used for "no secrets in diffs" and similar policy rules.
    SecretPattern(String),
    Always,
    Not(Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    /// ADR-28 §6: match the shell/profile id that produced the command.
    ShellProfile(String),
    /// ADR-28 §6: match the resolved executable path (after alias/function/
    /// script/symlink resolution) against a regex.
    ResolvedExecutable(String),
    /// ADR-28 §6: match the full argv (`program args...`, joined by spaces)
    /// against a regex.
    ArgvPattern(String),
    /// ADR-28 §6: match the working directory against a glob.
    WorkingDir(String),
    /// ADR-55 §2: the intent gate. As the **top-level condition of an
    /// approval-producing rule** (`RequireApproval`,
    /// `RequireApprovalWithTimeout`, `RequireManagedToolApproval`,
    /// `RequireToolchainApproval`) it applies the attached
    /// [`crate::authorization::IntentAuthorization`] verdict mechanically:
    /// [`crate::authorization::IntentVerdict::Allow`] upgrades
    /// `RequireApproval` → `Allow`, [`crate::authorization::IntentVerdict::RequireApproval`]
    /// keeps the action under the rule's approval path, and
    /// [`crate::authorization::IntentVerdict::Deny`] is a final pre-sink
    /// denial; the verdict's rule name becomes the audit row's `rule_matched`.
    /// Only the *bare* condition triggers the gate. When no authorization
    /// provider is attached (the default) the rule does not match and
    /// evaluation falls through to later rules — identical to pre-ADR-55
    /// behavior.
    ///
    /// Used inside combinators or non-approval rules it behaves as a plain
    /// boolean condition that matches only when the attached authorization
    /// allows the action; it never upgrades `RequireApproval` → `Allow`, so a
    /// compound `{IntentAuthorized, ...}` condition never grants on its own.
    IntentAuthorized,
}

/// Lifecycle state of one MCP server child process (ADR-43, decision 7).
///
/// A server is `Disabled` until its manager starts it, `Connecting` while the
/// child process is spawned and the `initialize` handshake runs, `Connected`
/// once `initialize` and `tools/list` succeeded, `Failed` if the process
/// crashed or registration errored, and `Stopped` after a graceful stop. The
/// state is surfaced to the desktop/CLI via the watch channel on
/// [`crate::event::EventKind::McpServerStateChanged`] events and the
/// `McpManager` accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerState {
    /// Not started (default; `mcp.enabled = false` or the server is disabled
    /// in config). Disabled servers are never spawned.
    Disabled,
    /// Child process spawned; `initialize` handshake in progress.
    Connecting,
    /// `initialize` + `tools/list` succeeded; tools are registered.
    Connected,
    /// The server process crashed/exited or registration failed. The error
    /// detail is carried on the matching `McpServerStateChanged` event.
    Failed,
    /// Gracefully stopped (or stopped after a crash).
    Stopped,
}

// ---- TaskId newtype -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Ulid);

impl TaskId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---- TaskExecutionMode ------------------------------------------------------

/// Execution mode for an agent task, determining whether tools are required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub enum TaskExecutionMode {
    /// Answer-only mode — the model can respond with text only, no tools required.
    #[default]
    AnswerOnly,
    /// Action-required mode — the model MUST use tools to complete the task.
    ActionRequired {
        /// Minimum number of tool calls expected before considering the task complete.
        min_tool_calls: u32,
        /// Whether verification (e.g., running tests) is required after tool execution.
        require_verification: bool,
    },
}

// ---- System prompts (ADR-55 Phase 1e) --------------------------------------

/// Build-mode system prompt: write code/files to disk via tools. Used for
/// [`RequestedOutcome::Execute`] runs.
///
/// Formerly the `AgentMode::Build` prompt text; preserved verbatim when the
/// mode picker was removed (ADR-55 Phase 1e) so intent-gated Execute runs keep
/// the same behavior.
pub const SYSTEM_PROMPT_BUILD: &str =
    "You are a careful, capable software engineering assistant working \
    directly in the user's codebase. You can read and write files and run \
    shell commands via the tools available to you; use them instead of \
    describing what you would do. Prefer small, verifiable steps over \
    large speculative changes. Ask before taking actions that are \
    destructive, expensive, or hard to undo — routine reads and edits do \
    not need confirmation. When you are unsure about the user's intent, \
    make a reasonable assumption, state it briefly, and proceed rather \
    than stalling on a clarifying question.\n\
    \n\
    TOOL USE: every tool call is a JSON function call with one complete \
    arguments object. Fill every required field exactly as named; never \
    call a tool with empty or missing arguments.\n\
    Examples:\n\
    filesystem {\"operation\": \"read\", \"path\": \"src/main.rs\"}\n\
    filesystem {\"operation\": \"list\", \"path\": \"src\"}\n\
    filesystem {\"operation\": \"write\", \"path\": \"src/main.rs\", \"content\": \"fn main() {}\"}\n\
    shell {\"command\": \"cargo test\"}\n\
    filesystem operations: read, write, delete, exists, list, move, copy \
    (write needs content; move/copy need destination). shell takes command \
    (required) and optional cwd.";

/// Chat-mode system prompt: conversational answer only, no tool use. Used for
/// every non-Execute, non-Plan outcome (Answer, Diagnose, Review, Verify, and
/// any future outcome).
pub const SYSTEM_PROMPT_CHAT: &str =
    "You are Concerto, a helpful and concise conversational assistant. \
    Answer the user's questions clearly. Do not use tools and do not write \
    or modify files; respond with text only.";

/// Plan-mode system prompt: produce a plan/design as text, no writes. Used for
/// [`RequestedOutcome::Plan`] runs.
pub const SYSTEM_PROMPT_PLAN: &str =
    "You are a senior software architect. Given the user's request, produce \
    a clear, concrete plan or design as text. Do not write files or run \
    commands. Outline the approach, the components involved, and the \
    step-by-step steps you would take to implement it.";

/// Select the run's system prompt from the intent-gate outcome (ADR-55
/// Phase 1e): the intent gate is now the ONLY routing path, so the prompt is
/// derived from the classified [`RequestedOutcome`] instead of a
/// user-selectable mode picker.
///
/// Mapping: Execute → build prompt, Plan → plan prompt, everything else →
/// chat prompt. The match is exhaustive without a wildcard: `RequestedOutcome`
/// is defined in this crate, so adding a future outcome forces this function to
/// name it explicitly (fail-fast) instead of silently falling back.
pub fn system_prompt_for(outcome: crate::intent::RequestedOutcome) -> &'static str {
    match outcome {
        crate::intent::RequestedOutcome::Execute => SYSTEM_PROMPT_BUILD,
        crate::intent::RequestedOutcome::Plan => SYSTEM_PROMPT_PLAN,
        crate::intent::RequestedOutcome::Answer
        | crate::intent::RequestedOutcome::Diagnose
        | crate::intent::RequestedOutcome::Review
        | crate::intent::RequestedOutcome::Verify => SYSTEM_PROMPT_CHAT,
    }
}

// ---- agents (Phase 3 / 5) -------------------------------------------------

/// Open agent identifier — a lowercase ASCII string.
///
/// Replaces the closed `AgentRole` enum. Six well-known constants mirror the
/// original roles; any other string is a valid custom agent ID.
///
/// Serializes transparently as a string. On deserialization, the six known
/// IDs are normalized to lowercase (accepting both `"Architect"` from old
/// checkpoint files and `"architect"` from new configs). Custom IDs are
/// lowercased for consistent comparison.
///
/// `Debug` prints the bare id (e.g. `coder`) so `{role:?}` in error
/// formatting stays readable.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AgentId(String);

impl std::fmt::Debug for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AgentId {
    /// Create a new AgentId, normalizing to lowercase.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into().to_ascii_lowercase())
    }

    /// View the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for AgentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AgentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        // Normalise to lowercase.  Known IDs accept both PascalCase (old
        // checkpoint format) and lowercase (canonical form).  Custom IDs are
        // lowercased for consistent comparison.
        Ok(Self(s.to_ascii_lowercase()))
    }
}

/// Stage tag — the pipeline phase an agent participates in.
///
/// The Coordinator has built-in algorithms for five known stages. Since the
/// ADR-58 blueprint registry, any **other** tag string is rejected at config
/// load time by `concerto-config` (rulebook (g)) with guidance to retag the
/// agent as `run_once` (the sanctioned Freeform tag) or register a matching
/// stage in the orchestration blueprint — the coordinator itself still treats
/// `None`/`run_once` as Freeform (run once, no lifecycle).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AgentStage(String);

impl<'de> serde::Deserialize<'de> for AgentStage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        // Normalize to lowercase so "Review" in a config file equals the
        // canonical "review" stage tag.
        Ok(Self(s.to_ascii_lowercase()))
    }
}

impl AgentStage {
    pub const DESIGN: &'static str = "design";
    pub const RESEARCH: &'static str = "research";
    pub const IMPLEMENT: &'static str = "implement";
    pub const REVIEW: &'static str = "review";
    pub const VALIDATE: &'static str = "validate";

    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_design(&self) -> bool {
        self.0 == Self::DESIGN
    }
    pub fn is_research(&self) -> bool {
        self.0 == Self::RESEARCH
    }
    pub fn is_implement(&self) -> bool {
        self.0 == Self::IMPLEMENT
    }
    pub fn is_review(&self) -> bool {
        self.0 == Self::REVIEW
    }
    pub fn is_validate(&self) -> bool {
        self.0 == Self::VALIDATE
    }
    /// Returns true for any of the five known pipeline stages.
    pub fn is_known(&self) -> bool {
        matches!(self.0.as_str(), "design" | "research" | "implement" | "review" | "validate")
    }
}

impl std::fmt::Display for AgentStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct AgentTask {
    pub id: TaskId,
    pub session_id: Ulid,
    pub description: String,
    pub created_at: OffsetDateTime,
    /// Execution mode determining whether tool calls are required.
    pub execution_mode: TaskExecutionMode,
}

impl AgentTask {
    /// Create a new answer-only task (default for conversational prompts).
    pub fn new(session_id: Ulid, description: impl Into<String>) -> Self {
        Self {
            id: TaskId::new(),
            session_id,
            description: description.into(),
            created_at: OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::AnswerOnly,
        }
    }

    /// Create a new action-required task (for template/materialization tasks).
    pub fn new_action_required(session_id: Ulid, description: impl Into<String>) -> Self {
        Self {
            id: TaskId::new(),
            session_id,
            description: description.into(),
            created_at: OffsetDateTime::now_utc(),
            execution_mode: TaskExecutionMode::ActionRequired {
                min_tool_calls: 1,
                require_verification: true,
            },
        }
    }

    /// Rough heuristic: description under ~40 words, no "and" conjunction → likely scoped.
    pub fn is_scoped(&self) -> bool {
        self.description.split_whitespace().count() < 40
            && !self.description.to_lowercase().contains(" and ")
    }
}

#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub task_id: TaskId,
    pub session_id: Ulid,
    /// Short model-authored summary/notes. For action-required tasks this is
    /// the optional free-text "Notes" only; the authoritative final answer is
    /// composed by [`AgentOutput::summary`] from structured execution data.
    pub final_message: String,
    pub files_modified: Vec<Utf8PathBuf>,
    pub tool_call_count: u32,
    pub eval_result: Option<EvalResult>,
    /// Structured per-tool execution records (only populated for the live
    /// single-agent path). Empty for multi-agent/legacy callers.
    pub tool_events: Vec<ToolExecutionSummary>,
    /// Structured verification results (e.g. `py_compile`) after file writes.
    pub verification: Vec<VerificationSummary>,
    /// Project root the task executed in, for displaying absolute file paths.
    pub project_root: Option<Utf8PathBuf>,
    /// Whether the requested task completed or only recoverable partial
    /// progress was preserved.
    pub completion_status: AgentCompletionStatus,
    /// Provider usage generated by this run. Each entry is persisted against
    /// the session so Dashboard totals reflect both single- and multi-agent
    /// activity.
    pub provider_metrics: Vec<ProviderMetrics>,
    /// Serialised orchestration checkpoint for partial-result resume.
    /// Present only when `completion_status == Partial` and the run was
    /// dispatched through `CoordinatorAgent`.  On "Continue" this is passed
    /// back to `CoordinatorAgent::run_with_checkpoint` to skip re-architecting.
    pub checkpoint_json: Option<String>,
}

/// Completion state carried with output so the UI cannot render preserved
/// partial progress as a successful completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentCompletionStatus {
    Completed,
    Partial,
}

/// One structured record of a single tool execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecutionSummary {
    pub tool_name: String,
    pub operation: Option<String>,
    pub path: Option<Utf8PathBuf>,
    pub success: bool,
    /// Human-readable one-line summary (e.g. "Wrote 42 bytes" or the error).
    pub summary: String,
}

/// One structured verification result for a written file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationSummary {
    pub path: Utf8PathBuf,
    /// Short command label, e.g. "py_compile", "cargo check", "npm test".
    pub command: String,
    pub passed: bool,
    /// Captured command output/error for inspectability.
    pub output: String,
}

/// Explicit outcome of a single agent run attempt (`AgentLoop::run_once`).
///
/// This replaces the previous "iteration cap = silent success" behavior.
/// Hitting the iteration cap is its own signal (`IterationCapHit`) so the
/// higher-level runner can auto-continue the same session or escalate, never
/// reporting a false `Done`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentRunExit {
    /// The task completed and, when required, verification passed.
    Done(AgentOutput),
    /// The model indicated it needs user input before it can proceed.
    NeedsUser { reason: String, partial: AgentOutput },
    /// A real blocker stopped progress (e.g. minimum tool calls unmet, or no
    /// file-changing action succeeded).
    Blocked { reason: String, partial: AgentOutput },
    /// The iteration cap was hit without a natural completion. Carries the
    /// partial progress so the runner can continue the same session.
    IterationCapHit { reason: String, partial: AgentOutput },
}

impl AgentOutput {
    /// Compose the authoritative final answer from structured execution data.
    ///
    /// For action-required tasks this is built from real `files_modified`,
    /// `verification`, and (optionally) `final_message` notes — never from
    /// unverified provider prose. For conversational (answer-only) tasks it
    /// returns `final_message` verbatim.
    pub fn summary(&self) -> String {
        let has_execution = !self.files_modified.is_empty()
            || !self.verification.is_empty()
            || !self.tool_events.is_empty();

        if !has_execution {
            return self.final_message.clone();
        }

        let mut s = match self.completion_status {
            AgentCompletionStatus::Completed => String::from("Completed.\n\n"),
            AgentCompletionStatus::Partial => String::from("Partial progress preserved.\n\n"),
        };
        s.push_str("Files changed:\n");
        for p in &self.files_modified {
            s.push_str(&format!("- {}\n", p));
        }

        if !self.verification.is_empty() {
            s.push_str("\nVerification:\n");
            for v in &self.verification {
                let status = if v.passed { "passed" } else { "failed" };
                s.push_str(&format!("- {}: {} {}\n", v.path, v.command, status));
            }
        }

        let notes = self.final_message.trim();
        if !notes.is_empty() {
            s.push_str(&format!("\nNotes:\n{}", notes));
        }

        if let Some(root) = &self.project_root {
            let display_root = root.as_str().strip_prefix(r"\\?\").unwrap_or(root.as_str());
            s.push_str(&format!("\nProject root:\n{display_root}"));
        }

        s
    }
}

/// Coverage measurement from a coverage tool run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageInfo {
    /// Tool used to collect coverage.
    pub tool: CoverageTool,
    /// Line coverage percentage (0.0 – 100.0).
    pub line_percent: f64,
    /// Function coverage percentage (0.0 – 100.0), if available.
    pub function_percent: Option<f64>,
    /// Branch coverage percentage (0.0 – 100.0), if available.
    pub branch_percent: Option<f64>,
    /// Raw output tail from the coverage tool for debugging.
    pub raw_tail: String,
}

/// Supported coverage collection tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum CoverageTool {
    LlvmCov,
    Tarpaulin,
    /// Generic fallback for unrecognised tools.
    Other(String),
}

impl fmt::Display for CoverageTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoverageTool::LlvmCov => write!(f, "llvm-cov"),
            CoverageTool::Tarpaulin => write!(f, "tarpaulin"),
            CoverageTool::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Result of running a project's test suite after agent completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub runner: TestRunner,
    pub exit_code: i32,
    pub passed: bool,
    pub duration_ms: u64,
    pub output_tail: String,
    /// Optional coverage measurement collected alongside the test run.
    #[serde(default)]
    pub coverage: Option<CoverageInfo>,
}

/// Detected test runner for a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TestRunner {
    Cargo,
    Npm,
    Pytest,
    Make,
    Unknown(String),
}

impl fmt::Display for TestRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestRunner::Cargo => write!(f, "cargo"),
            TestRunner::Npm => write!(f, "npm"),
            TestRunner::Pytest => write!(f, "pytest"),
            TestRunner::Make => write!(f, "make"),
            TestRunner::Unknown(s) => write!(f, "{s}"),
        }
    }
}

/// Registry of available tools, keyed by tool name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn crate::traits::tool::Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn crate::traits::tool::Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn crate::traits::tool::Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// Remove and return the tool registered under `name`, if any.
    ///
    /// Used by the MCP manager (ADR-43 §4) to unregister a server's tools on
    /// stop without silently clobbering a later registration of the same
    /// name. Unlike [`Self::register`] (which overwrites), this returns the
    /// displaced tool so callers can fail loudly instead.
    pub fn unregister(&mut self, name: &str) -> Option<Box<dyn crate::traits::tool::Tool>> {
        self.tools.remove(name)
    }

    pub fn capability_filter(&self, caps: &CapabilitySet) -> Vec<&dyn crate::traits::tool::Tool> {
        self.tools
            .values()
            .map(|t| t.as_ref())
            .filter(|t| t.capability_requirements().is_subset(caps))
            .collect()
    }

    /// True when at least one registered tool *requires* a capability that
    /// `caps` satisfies.
    ///
    /// Tools with empty capability requirements (LSP tools, MCP bridge) are
    /// offered to every agent but never count here: a capability-free agent
    /// must still get the strict forced-contract treatment rather than an
    /// auto tool choice it did not earn.
    pub fn has_capability_gated_tools(&self, caps: &CapabilitySet) -> bool {
        self.tools.values().any(|t| {
            let required = t.capability_requirements();
            !required.is_empty() && required.is_subset(caps)
        })
    }

    /// Return all registered tools as [`ToolDefinition`]s for passing to an LLM.
    pub fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.input_schema(),
            })
            .collect()
    }
}

// ---- ProjectId --------------------------------------------------------------

/// Project identity — blake3 hash of the canonicalised project directory.
///
/// `project_id = blake3(canonicalize(project_dir))` is computed once at
/// startup by `ProjectIdHelper` and reused across every module that
/// needs project-scoped storage. The newtype prevents accidental mixing
/// of project IDs with raw strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub String);

impl ProjectId {
    /// Git repos: blake3 of the default remote URL (stable across clones/moves).
    /// Non-git directories: blake3 of the canonicalized absolute path.
    pub fn resolve(project_dir: &Path) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use gix::discover;
            if let Ok(repo) = discover(project_dir) {
                if let Some(Ok(remote)) = repo.find_default_remote(gix::remote::Direction::Fetch) {
                    if let Some(url_ref) = remote.url(gix::remote::Direction::Fetch) {
                        let bstring = url_ref.to_bstring();
                        return Self(blake3::hash(bstring.as_ref()).to_hex().to_string());
                    }
                }
            }
        }
        // Fallback: canonicalize path and hash it
        let canonical = project_dir.canonicalize().unwrap_or_else(|_| project_dir.to_path_buf());
        Self(blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex().to_string())
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---- Phase 5: structured agent output types ---------------------------------

/// Structured architecture plan produced by the design stage.
///
/// All `Vec` fields default to empty when absent so a partial-but-valid JSON
/// response from the LLM does not needlessly cause a retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignDoc {
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub proposed_files: Vec<Utf8PathBuf>,
    pub interface_sketch: String,
    #[serde(default)]
    pub risks: Vec<String>,
}

/// Structured output mode for a config-driven agent (ADR-35 follow-up).
///
/// `Freeform` (the default) keeps the historical free-text result semantics;
/// `DesignDoc` routes the agent through the typed `submit_design_doc`
/// submission contract (audit H-01) so the accepted design document is
/// validated field-by-field and surfaced as canonical JSON. `ResearchReport`
/// and `ReviewReport` generalize the same typed submission machinery to the
/// researcher and reviewer stages (ADR-35 phase 4 — those agents are now
/// config-driven seeds backed by the generic specialist).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// Unstructured text output (historical behavior).
    #[default]
    Freeform,
    /// Structured DesignDoc output via the `submit_design_doc` tool.
    DesignDoc,
    /// Structured research output via the `submit_research_report` tool.
    ResearchReport,
    /// Structured review output via the `submit_review_report` tool.
    ReviewReport,
}

/// Canonical typed input for the `submit_design_doc` submission contract.
///
/// The provider-facing JSON schema is generated from this exact type via
/// `schemars::schema_for!` — there is no hand-maintained duplicate. The
/// `files`/`interface` serde aliases accept the legacy field names the
/// previous hand-built schema exposed, so models that learned the old
/// contract still submit successfully.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SubmitDesignDocInput {
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default, alias = "files")]
    #[schemars(with = "Vec<String>")]
    pub proposed_files: Vec<Utf8PathBuf>,
    #[serde(alias = "interface")]
    pub interface_sketch: String,
    #[serde(default)]
    pub risks: Vec<String>,
}

impl From<SubmitDesignDocInput> for DesignDoc {
    /// Adopt an accepted submission as the persisted design document. The
    /// coordinator's existing snapshot path keeps working because the agent
    /// surfaces this exact document as its canonical JSON summary.
    fn from(input: SubmitDesignDocInput) -> Self {
        Self {
            goals: input.goals,
            constraints: input.constraints,
            proposed_files: input.proposed_files,
            interface_sketch: input.interface_sketch,
            risks: input.risks,
        }
    }
}

/// Structured research output by the research stage.
///
/// All `Vec` fields default to empty when absent so a partial-but-valid JSON
/// response from the LLM does not needlessly cause a retry. `JsonSchema` is
/// derived so the provider-facing `submit_research_report` schema is
/// generated from this exact type (single source of truth, audit H-01).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResearchReport {
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub relevant_files: Vec<Utf8PathBuf>,
    #[serde(default)]
    pub code_snippets: Vec<CodeSnippet>,
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub unknowns: Vec<String>,
}

/// A code snippet with file path and line range.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeSnippet {
    #[schemars(with = "String")]
    pub file: Utf8PathBuf,
    /// Declared as `[u32; 2]` for schema purposes: schemars renders the Rust
    /// tuple `(u32, u32)` as a draft-2020-12 `prefixItems` tuple, which Google
    /// Gemini rejects (`INVALID_ARGUMENT` on unknown name), while a sized
    /// array emits `items` + `minItems`/`maxItems` — accepted by every
    /// provider. Serde serializes tuples and sized arrays identically, so the
    /// wire contract (`[start, end]`) is unchanged.
    #[schemars(with = "[u32; 2]")]
    pub lines: (u32, u32),
    pub content: String,
}

/// Structured review output by the review stage.
///
/// All `Vec` fields default to empty when absent so a partial-but-valid JSON
/// response from the LLM does not needlessly cause a retry. `JsonSchema` is
/// derived so the provider-facing `submit_review_report` schema is generated
/// from this exact type (single source of truth, audit H-01).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewReport {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub issues: Vec<Issue>,
    #[serde(default)]
    pub suggestions: Vec<String>,
}

/// Review verdict — pass, fail, or needs revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[non_exhaustive]
pub enum ReviewVerdict {
    Pass,
    Fail,
    NeedsRevision,
}

/// A single issue found during review.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Issue {
    pub severity: Severity,
    #[schemars(with = "Option<String>")]
    pub file: Option<Utf8PathBuf>,
    pub line: Option<u32>,
    pub description: String,
}

/// Severity level for review issues.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[non_exhaustive]
pub enum Severity {
    Critical,
    Major,
    Minor,
    Info,
}

// ---- Phase 5: SubTask -------------------------------------------------------

/// Status of a subtask in the task graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SubTaskStatus {
    Pending,
    Blocked,
    Running,
    AwaitingReview,
    NeedsRevision,
    Completed,
    Failed,
}

impl SubTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Blocked => "blocked",
            Self::Running => "running",
            Self::AwaitingReview => "awaiting_review",
            Self::NeedsRevision => "needs_revision",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_name(s: &str) -> Self {
        match s {
            "blocked" => Self::Blocked,
            "running" => Self::Running,
            "awaiting_review" => Self::AwaitingReview,
            "needs_revision" => Self::NeedsRevision,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

/// A single work unit in the multi-agent task graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub session_id: Ulid,
    pub role: AgentId,
    pub description: String,
    pub status: SubTaskStatus,
    pub dependencies: Vec<TaskId>,
    pub deliverable: Option<String>,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
}

impl SubTask {
    pub fn new(session_id: Ulid, role: AgentId, description: impl Into<String>) -> Self {
        Self {
            id: TaskId::new(),
            parent_id: None,
            session_id,
            role,
            description: description.into(),
            status: SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }
}

// ---- Phase 5: AgentRunResult ------------------------------------------------

/// Outcome of a single agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum AgentOutcome {
    Success,
    NeedsRevision { reason: String },
    Failed { error: String },
    Blocked { on: Vec<TaskId> },
}

/// Result of running a single specialist agent on a subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub task_id: TaskId,
    pub role: AgentId,
    pub outcome: AgentOutcome,
    pub summary: String,
    pub files_modified: Vec<Utf8PathBuf>,
    pub tool_call_count: u32,
    pub cost_usd: f64,
    pub latency_ms: u64,
    /// Provider name used for this run (e.g., "openai", "anthropic").
    pub provider: String,
    /// Exact model id used for this specialist run.
    #[serde(default)]
    pub model: String,
    /// Estimated input tokens consumed.
    pub tokens_in: u64,
    /// Estimated output tokens produced.
    pub tokens_out: u64,
}

// ---- Phase 5: AgentContext --------------------------------------------------

/// Context passed to an expert agent when it runs.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub session: SessionContext,
    pub parent_task: Option<AgentTask>,
    pub working_memory: crate::memory::WorkingMemorySnapshot,
    pub retrieved_chunks: Vec<crate::memory::MemoryChunk>,
    pub previous_results: Vec<AgentRunResult>,
    pub budget_remaining_usd: Option<f64>,
    /// Files that the plan expects to exist after this agent completes.
    /// The agent runner checks these after execution and rejects premature
    /// completion if required artifacts are missing.
    pub expected_artifacts: Vec<Utf8PathBuf>,
    /// ADR-64 §6: task-specific workspace capsule preloaded into agent
    /// prompts so agents never re-read files merely to confirm existence.
    /// `None` when the timeline projection is unavailable or the feature is
    /// disabled; `Some` at dispatch time when the coordinator builds it.
    pub workspace_capsule: Option<WorkspaceCapsule>,
}

impl AgentContext {
    pub fn new(session: SessionContext) -> Self {
        let session_id = session.session_id;
        Self {
            session,
            parent_task: None,
            working_memory: crate::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id,
                decisions: Vec::new(),
                task_tree: Vec::new(),
                created_at: OffsetDateTime::now_utc(),
            },
            retrieved_chunks: Vec::new(),
            previous_results: Vec::new(),
            budget_remaining_usd: None,
            expected_artifacts: Vec::new(),
            workspace_capsule: None,
        }
    }
}

// ---- Phase 5: WorkspaceCapsule (ADR-64 §6) ----------------------------------

/// A file known to the workspace — either written via the write gate (timeline
/// `WroteFile` / `WroteFilesFromPath`) or modified by a completed dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleFileEntry {
    /// Absolute or workspace-relative path.
    pub path: String,
    /// blake3 content hex from the timeline or pre-image cache.
    pub content_hash: String,
    /// Whiteboard gate sequence at which this file was last observed.
    pub last_modified_gate_seq: u64,
}

/// A pending (not-yet-completed) task in the execution graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsulePendingTask {
    /// The task's stable identifier.
    pub task_id: String,
    /// Human-readable description of the work.
    pub description: String,
    /// Task IDs this task depends on.
    pub dependencies: Vec<String>,
}

/// A task-specific workspace capsule: the bounded, typed context packet that
/// preloads file metadata from the timeline so agents never re-read files
/// merely to confirm existence.
///
/// Built by the orchestrator's [`capsule::build_capsule`] and serialized
/// into agent prompts by [`capsule::format_capsule`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCapsule {
    /// Files known to exist (from timeline `WroteFile` + `WroteFilesFromPath`
    /// events).
    pub known_files: Vec<CapsuleFileEntry>,
    /// Files modified earlier in this run by completed dependency tasks.
    pub modified_files: Vec<CapsuleFileEntry>,
    /// Pending work not yet done (from the task graph).
    pub pending_work: Vec<CapsulePendingTask>,
    /// Expected outputs for this specific task.
    pub expected_outputs: Vec<String>,
}

impl WorkspaceCapsule {
    /// True when the capsule carries no information beyond the expected outputs.
    pub fn is_empty(&self) -> bool {
        self.known_files.is_empty()
            && self.modified_files.is_empty()
            && self.pending_work.is_empty()
            && self.expected_outputs.is_empty()
    }
}

// ---- Phase 5: RoutingProfile ------------------------------------------------

/// Compatibility and cost profile for a configured provider/model pair.
///
/// Model-level metadata used for per-agent routing decisions. Extended in Phase
/// 1 with context window, tool-calling support, and local/remote distinction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingProfile {
    /// Stable ID of the provider configuration that owns this model.
    #[serde(default)]
    pub provider_config_id: String,
    pub provider: String,
    pub model: String,
    pub cost_per_1k_tokens: f64,
    pub avg_latency_ms: u64,
    /// Maximum context window size in tokens.
    #[serde(default)]
    pub context_window: u32,
    /// Whether the model supports tool/function calling.
    #[serde(default)]
    pub supports_tool_calling: bool,
    /// Optional custom API base URL (e.g. for self-hosted endpoints).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

impl RoutingProfile {
    /// Returns `true` if this profile points to a local model
    /// (Ollama or custom base URL).
    pub fn is_local(&self) -> bool {
        self.provider == "ollama" || self.base_url.is_some()
    }
}

// ---- Phase 8: Cost attribution -----------------------------------------------

/// Cost information for a single LLM provider call, attached to traces
/// and observable via the OTEL/Langfuse exporters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostInfo {
    pub total_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub provider: String,
    pub model: String,
}

// ---- Phase 8: Eval types -----------------------------------------------------

/// A single benchmark task — a project snapshot with expected outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalTask {
    pub id: Ulid,
    pub description: String,
    pub project_snapshot_path: Utf8PathBuf,
    pub expected_outcomes: Vec<EvalOutcome>,
}

/// Expected outcomes for an eval task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalOutcome {
    pub files_changed: Vec<Utf8PathBuf>,
    pub tests_pass: bool,
    pub min_patterns: Vec<String>,
    pub max_patterns: Vec<String>,
}

/// Result of running a single benchmark task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub task_id: Ulid,
    pub task_description: String,
    pub completed: bool,
    pub interventions: u32,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub time_to_complete_ms: u64,
    pub tests_passing_after: bool,
    pub files_correctly_modified: f32,
    pub agent_outcome: String,
}

/// Metrics tracked for regression detection during benchmarks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BenchmarkMetric {
    PassRate,
    AvgLatencyMs,
    AvgCostUsd,
    AvgTokens,
    AvgInterventions,
}

impl BenchmarkMetric {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PassRate => "pass_rate",
            Self::AvgLatencyMs => "avg_latency_ms",
            Self::AvgCostUsd => "avg_cost_usd",
            Self::AvgTokens => "avg_tokens",
            Self::AvgInterventions => "avg_interventions",
        }
    }
}

/// Aggregate report from running a benchmark suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub task_count: usize,
    pub pass_rate: f64,
    pub avg_latency_ms: u64,
    pub avg_cost_usd: f64,
    pub avg_tokens: u64,
    pub avg_interventions: f32,
    pub individual_results: Vec<BenchmarkResult>,
}

// ---- ModelInfo ---------------------------------------------------------------

/// Information about an LLM model returned by a provider's model listing API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Unique model identifier (e.g. "gpt-4o", "claude-3-opus-20240229").
    pub id: String,
    /// Human-readable display name, if available.
    pub name: Option<String>,
    /// Entity that owns/publishes the model, if available.
    pub owned_by: Option<String>,
}

// ---- Phase 8: SandboxProfile --------------------------------------------------

/// Sandbox isolation level for tool execution.
///
/// **This is currently a stub.** No variant provides OS-level isolation
/// (no containers, seccomp, namespaces, or Landlock). All non-`None`
/// profiles are rejected by the policy engine until real sandboxing is
/// implemented.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SandboxProfile {
    /// No sandboxing — tools run as the invoking user.
    None,
    /// Filesystem writes are denied (reads allowed). **Not implemented.**
    ReadOnlyFs,
    /// Network operations are denied (shell/http tools blocked). **Not implemented.**
    NetworkIsolated,
    /// Full containerization. **Not implemented.**
    Containerized,
}

impl SandboxProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReadOnlyFs => "read_only_fs",
            Self::NetworkIsolated => "network_isolated",
            Self::Containerized => "containerized",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_new_sets_available() {
        let budget = TokenBudget::new(1000, 200);
        assert_eq!(budget.capacity, 1000);
        assert_eq!(budget.reserved_for_response, 200);
        assert_eq!(budget.available, 800);
    }

    #[test]
    fn completion_chunk_keepalive_is_non_terminal_liveness_marker() {
        // ADR-53 §4: the keepalive must be a no-op, non-terminal chunk so
        // downstream collectors treat it as a liveness marker, not a delta.
        let chunk = CompletionChunk::keepalive();
        assert!(chunk.delta.is_empty(), "keepalive carries no delta");
        assert!(!chunk.is_final, "keepalive must not terminate the stream");
        assert!(chunk.reasoning.is_none());
        assert!(chunk.tool_call.is_none());
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn token_budget_reserve_reduces_available() {
        let mut budget = TokenBudget::new(1000, 200);
        budget.reserve(300).unwrap();
        assert_eq!(budget.available, 500);
    }

    #[test]
    fn token_budget_reserve_overflow_returns_error() {
        let mut budget = TokenBudget::new(100, 50);
        let result = budget.reserve(100);
        assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
    }

    #[test]
    fn agent_stage_serde_normalizes_lowercase_and_round_trips() {
        // Mixed-case input is normalized to the canonical lowercase form.
        let stage: AgentStage = serde_json::from_str(r#""Review""#).unwrap();
        assert!(stage.is_review());
        assert_eq!(stage.as_str(), AgentStage::REVIEW);
        assert_eq!(stage.to_string(), "review");

        // Unknown stages stay freeform strings (ADR-35 open stage tags).
        let freeform: AgentStage = serde_json::from_str(r#""documentation""#).unwrap();
        assert!(!freeform.is_known());
        assert_eq!(freeform.as_str(), "documentation");

        // Serialization round-trip preserves the canonical form.
        let encoded = serde_json::to_string(&stage).unwrap();
        assert_eq!(encoded, r#""review""#);
        let decoded: AgentStage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, stage);
    }

    #[test]
    fn capability_set_filesystem() {
        let caps = CapabilitySet::filesystem(vec!["**".into()], true);
        assert!(caps.requirements[0].contains("filesystem"));
        assert!(caps.requirements[0].contains("write=true"));
    }

    #[test]
    fn capability_set_shell() {
        let caps = CapabilitySet::shell(vec!["echo.*".into()], "/tmp".into());
        assert!(caps.requirements[0].contains("shell"));
        assert!(caps.requirements[0].contains("cwd=/tmp"));
    }

    #[test]
    fn capability_set_subset_and_superset() {
        let a = CapabilitySet::default().with_requirement("x").with_requirement("y");
        let b = CapabilitySet::default().with_requirement("x");
        assert!(b.is_subset(&a));
        assert!(a.is_superset(&b));
        assert!(!a.is_subset(&b));
    }

    #[test]
    fn filesystem_scope_classify() {
        let project = Path::new("/home/user/project");
        assert_eq!(FilesystemScope::classify_for(None, project), FilesystemScope::Unknown);
        assert_eq!(
            FilesystemScope::classify_for(Some(project), project),
            FilesystemScope::ProjectOnly
        );
        assert_eq!(
            FilesystemScope::classify_for(Some(Path::new("/outside")), project),
            FilesystemScope::Anywhere
        );
    }

    #[test]
    fn destructive_classify_commands() {
        assert_eq!(DestructiveClass::classify_command("rm -rf /"), DestructiveClass::Destructive);
        assert_eq!(
            DestructiveClass::classify_command("dd if=/dev/zero of=/dev/sda"),
            DestructiveClass::Destructive
        );
        assert_eq!(
            DestructiveClass::classify_command("mv file1 file2"),
            DestructiveClass::Modifying
        );
        assert_eq!(
            DestructiveClass::classify_command("echo hello"),
            DestructiveClass::NonDestructive
        );
    }

    #[test]
    fn code_category_classify_paths() {
        assert_eq!(CodeCategory::classify("src/tests/test_auth.rs"), CodeCategory::Test);
        assert_eq!(CodeCategory::classify("src/auth/login.rs"), CodeCategory::Auth);
        assert_eq!(CodeCategory::classify("db/migrations/001_init.sql"), CodeCategory::Migration);
        assert_eq!(CodeCategory::classify("config/app.toml"), CodeCategory::Config);
        assert_eq!(CodeCategory::classify("src/main.rs"), CodeCategory::Other);
    }

    #[test]
    fn system_prompt_for_execute_is_build() {
        assert_eq!(
            system_prompt_for(crate::intent::RequestedOutcome::Execute),
            SYSTEM_PROMPT_BUILD
        );
    }

    /// The build prompt must teach weak models the exact tool-call wire
    /// format: JSON function calls with the real advertised field names
    /// (`filesystem.operation/path/content`, `shell.command/cwd`) and a
    /// concrete example per operation family. A wrong field name here would
    /// teach models to hallucinate exactly the keys the tool-call guard has
    /// to repair (see `crates/providers/src/adapters/schema_loose.rs`).
    #[test]
    fn system_prompt_build_documents_tool_call_format() {
        let prompt = SYSTEM_PROMPT_BUILD;

        // Format statement + the never-empty rule.
        assert!(prompt.contains("JSON function call"), "{prompt}");
        assert!(
            prompt.contains("never call a tool with empty or missing arguments"),
            "prompt must forbid empty/missing arguments: {prompt}"
        );

        // Concrete examples with the real field names.
        assert!(prompt.contains(r#"{"operation": "read", "path": "src/main.rs"}"#), "{prompt}");
        assert!(prompt.contains(r#"{"operation": "list", "path": "src"}"#), "{prompt}");
        assert!(
            prompt.contains(r#"{"operation": "write", "path": "src/main.rs", "content":"#),
            "a write example with content must be shown: {prompt}"
        );
        assert!(prompt.contains(r#"{"command": "cargo test"}"#), "{prompt}");

        // Real schema vocabulary: the operation enum, and `cwd` — never a
        // made-up `workdir` key.
        assert!(prompt.contains("read, write, delete, exists, list, move, copy"), "{prompt}");
        assert!(prompt.contains("cwd"), "{prompt}");
        assert!(!prompt.contains("workdir"), "shell has no workdir field: {prompt}");
    }

    #[test]
    fn system_prompt_for_plan_is_plan() {
        assert_eq!(system_prompt_for(crate::intent::RequestedOutcome::Plan), SYSTEM_PROMPT_PLAN);
    }

    #[test]
    fn system_prompt_for_conversational_outcomes_is_chat() {
        for outcome in [
            crate::intent::RequestedOutcome::Answer,
            crate::intent::RequestedOutcome::Diagnose,
            crate::intent::RequestedOutcome::Review,
            crate::intent::RequestedOutcome::Verify,
        ] {
            assert_eq!(system_prompt_for(outcome), SYSTEM_PROMPT_CHAT, "{outcome:?}");
        }
    }

    #[test]
    fn agent_task_new_creates_answer_only() {
        let session_id = Ulid::new();
        let task = AgentTask::new(session_id, "do something");
        assert_eq!(task.session_id, session_id);
        assert_eq!(task.execution_mode, TaskExecutionMode::AnswerOnly);
    }

    #[test]
    fn agent_task_new_action_required() {
        let session_id = Ulid::new();
        let task = AgentTask::new_action_required(session_id, "write code");
        assert!(matches!(task.execution_mode, TaskExecutionMode::ActionRequired { .. }));
        assert!(task.is_scoped());
    }

    #[test]
    fn agent_task_not_scoped_when_long() {
        let session_id = Ulid::new();
        let long_desc = "do this and that and the other thing and more stuff and extra words";
        let task = AgentTask::new(session_id, long_desc);
        assert!(!task.is_scoped());
    }

    #[test]
    fn task_id_display() {
        let tid = TaskId::new();
        let s = tid.to_string();
        assert!(!s.is_empty());
    }

    #[test]
    fn subtask_status_roundtrip() {
        for (name, status) in [
            ("pending", SubTaskStatus::Pending),
            ("blocked", SubTaskStatus::Blocked),
            ("running", SubTaskStatus::Running),
            ("awaiting_review", SubTaskStatus::AwaitingReview),
            ("needs_revision", SubTaskStatus::NeedsRevision),
            ("completed", SubTaskStatus::Completed),
            ("failed", SubTaskStatus::Failed),
        ] {
            assert_eq!(status.as_str(), name);
            assert_eq!(SubTaskStatus::from_name(name), status);
        }
        assert_eq!(SubTaskStatus::from_name("unknown"), SubTaskStatus::Pending);
    }

    #[test]
    fn routing_profile_is_local() {
        let local = RoutingProfile {
            provider_config_id: String::new(),
            provider: "ollama".into(),
            model: "llama3".into(),
            cost_per_1k_tokens: 0.0,
            avg_latency_ms: 0,
            context_window: 0,
            supports_tool_calling: false,
            base_url: None,
            description: None,
        };
        assert!(local.is_local());
        let remote = RoutingProfile {
            provider_config_id: String::new(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            cost_per_1k_tokens: 0.0,
            avg_latency_ms: 0,
            context_window: 0,
            supports_tool_calling: false,
            base_url: None,
            description: None,
        };
        assert!(!remote.is_local());
    }

    #[test]
    fn sandbox_profile_as_str() {
        assert_eq!(SandboxProfile::None.as_str(), "none");
        assert_eq!(SandboxProfile::ReadOnlyFs.as_str(), "read_only_fs");
        assert_eq!(SandboxProfile::NetworkIsolated.as_str(), "network_isolated");
        assert_eq!(SandboxProfile::Containerized.as_str(), "containerized");
    }

    #[test]
    fn coverage_tool_display() {
        assert_eq!(CoverageTool::LlvmCov.to_string(), "llvm-cov");
        assert_eq!(CoverageTool::Tarpaulin.to_string(), "tarpaulin");
        assert_eq!(CoverageTool::Other("custom".into()).to_string(), "custom");
    }

    #[test]
    fn test_runner_display() {
        assert_eq!(TestRunner::Cargo.to_string(), "cargo");
        assert_eq!(TestRunner::Npm.to_string(), "npm");
        assert_eq!(TestRunner::Pytest.to_string(), "pytest");
        assert_eq!(TestRunner::Make.to_string(), "make");
        assert_eq!(TestRunner::Unknown("custom".into()).to_string(), "custom");
    }

    #[test]
    fn benchmark_metric_as_str() {
        assert_eq!(BenchmarkMetric::PassRate.as_str(), "pass_rate");
        assert_eq!(BenchmarkMetric::AvgLatencyMs.as_str(), "avg_latency_ms");
        assert_eq!(BenchmarkMetric::AvgCostUsd.as_str(), "avg_cost_usd");
        assert_eq!(BenchmarkMetric::AvgTokens.as_str(), "avg_tokens");
        assert_eq!(BenchmarkMetric::AvgInterventions.as_str(), "avg_interventions");
    }

    #[test]
    fn agent_output_summary_no_execution() {
        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: "hello".into(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: vec![],
            verification: vec![],
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: vec![],
            checkpoint_json: None,
        };
        assert_eq!(output.summary(), "hello");
    }

    #[test]
    fn agent_output_summary_with_files() {
        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: "done".into(),
            files_modified: vec![Utf8PathBuf::from("src/main.rs")],
            tool_call_count: 1,
            eval_result: None,
            tool_events: vec![],
            verification: vec![],
            project_root: Some(Utf8PathBuf::from("/project")),
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: vec![],
            checkpoint_json: None,
        };
        let s = output.summary();
        assert!(s.contains("Completed"));
        assert!(s.contains("src/main.rs"));
        assert!(s.contains("/project"));
    }

    #[test]
    fn role_serialization() {
        let system = serde_json::to_value(Role::System).unwrap();
        assert_eq!(system, serde_json::json!("System"));
        let user = serde_json::to_value(Role::User).unwrap();
        assert_eq!(user, serde_json::json!("User"));
    }

    #[test]
    fn tool_call_roundtrip() {
        let tc = ToolCall {
            id: "call_1".into(),
            name: "test_tool".into(),
            arguments: serde_json::json!({"input": "hello"}),
        };
        let json = serde_json::to_value(&tc).unwrap();
        let back: ToolCall = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, "call_1");
        assert_eq!(back.name, "test_tool");
    }

    #[test]
    fn tool_definition_defaults() {
        let td = ToolDefinition {
            name: "my_tool".into(),
            description: "My tool".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        assert_eq!(td.name, "my_tool");
        assert_eq!(td.description, "My tool");
    }

    #[test]
    fn session_context_new() {
        let sid = Ulid::new();
        let dir = std::env::current_dir().unwrap();
        let ctx = SessionContext::new(sid, dir.clone());
        assert_eq!(ctx.session_id, sid);
        assert_eq!(ctx.project_dir, dir);
    }

    #[test]
    fn agent_run_exit_variants() {
        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: String::new(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: vec![],
            verification: vec![],
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: vec![],
            checkpoint_json: None,
        };
        match AgentRunExit::Done(output) {
            AgentRunExit::Done(_) => {}
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn message_default_fields() {
        let msg = Message {
            role: Role::User,
            content: "hello".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_results.is_none());
        assert!(msg.reasoning_content.is_none());
    }

    #[test]
    fn message_serde_round_trip_with_and_without_reasoning() {
        // Without reasoning (legacy path): field serializes as null.
        let plain = Message {
            role: Role::Assistant,
            content: "ok".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let json = serde_json::to_string(&plain).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_content, None);
        assert_eq!(back.content, "ok");

        // With reasoning: field round-trips.
        let thinking = Message {
            role: Role::Assistant,
            content: "answer".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: Some("reason".into()),
            tokens_in: None,
            tokens_out: None,
        };
        let json = serde_json::to_string(&thinking).unwrap();
        assert!(json.contains("\"reasoning_content\":\"reason\""));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_content.as_deref(), Some("reason"));
    }

    #[test]
    fn legacy_json_without_reasoning_field_still_deserializes() {
        // Old stored JSON has no reasoning_content key; `#[serde(default)]`
        // yields None instead of a hard error.
        let legacy = r#"{"role":"Assistant","content":"hi","tool_calls":null,"tool_results":null}"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(msg.content, "hi");
        assert!(msg.reasoning_content.is_none());
    }

    #[test]
    fn message_token_counters_serde_round_trip() {
        // Measured usage persists and round-trips (ADR-48 §4).
        let with_usage = Message {
            role: Role::Assistant,
            content: "measured".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: Some(1_234),
            tokens_out: Some(56),
        };
        let json = serde_json::to_string(&with_usage).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens_in, Some(1_234));
        assert_eq!(back.tokens_out, Some(56));

        // `None` must not mean `0`: unknown usage stays distinctly unknown.
        let unknown = Message {
            role: Role::User,
            content: "unknown".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let json = serde_json::to_string(&unknown).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens_in, None);
        assert_eq!(back.tokens_out, None);

        // Legacy JSON without the fields deserializes as `None` (serde default),
        // so estimators can still apply the byte heuristic.
        let legacy = r#"{"role":"User","content":"legacy","tool_calls":null,"tool_results":null,"reasoning_content":null}"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(msg.tokens_in, None);
        assert_eq!(msg.tokens_out, None);
    }

    #[test]
    fn completion_usage_serde_round_trip() {
        let usage = CompletionUsage { prompt_tokens: Some(10), completion_tokens: Some(5) };
        let json = serde_json::to_string(&usage).unwrap();
        let back: CompletionUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);

        // Partial reports (one side absent) stay partial.
        let partial = serde_json::from_str::<CompletionUsage>(
            r#"{"prompt_tokens":7,"completion_tokens":null}"#,
        )
        .unwrap();
        assert_eq!(partial.prompt_tokens, Some(7));
        assert_eq!(partial.completion_tokens, None);
    }
}
