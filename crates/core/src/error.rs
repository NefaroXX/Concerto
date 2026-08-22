//! Layered error taxonomy. One `thiserror` type per domain (per the
//! roadmap's "Error Taxonomy" cross-cutting requirement). Cross-domain
//! errors use `From` conversions, added as real call sites need them —
//! don't pre-build conversions speculatively in Phase 0.
//!
//! No `unwrap()` / `expect()` anywhere in this crate or any library crate.
//! `expect()` is permitted only in a `main()` binary, for startup
//! configuration that must be present, with an explicit panic message.

use std::error::Error;
use std::time::Duration;
use thiserror::Error;

/// Render an error together with its full `source()` chain.
///
/// `reqwest::Error`'s `Display` impl only prints the top-level message and
/// never walks `source()`, which is where the real cause lives (DNS failure,
/// TLS failure, connection refused, or a connect timeout). Use this at
/// provider boundaries so a failed network call reports something debuggable
/// instead of the generic `"error sending request for url (...)"`.
pub fn describe_error_chain(e: &(dyn Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(" | caused by: ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

use crate::ids::Ulid;
use camino::Utf8PathBuf;

/// Root error type for core infrastructure failures.
///
/// This error encompasses failures in the event bus, ID generation, and other
/// foundational systems that don't fit into domain-specific error types.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// Event bus send operation failed.
    ///
    /// This occurs when the broadcast channel is closed or all receivers have
    /// been dropped. Typically indicates a shutdown condition.
    #[error("event bus send failed: {0}")]
    EventBus(String),

    /// Unique ID generation failed.
    ///
    /// This is extremely rare and usually indicates a system-level failure
    /// in the ULID generator or random number source.
    #[error("id generation failed: {0}")]
    IdGeneration(String),
}

/// Configuration loading and validation errors.
///
/// These errors occur during application startup when loading configuration
/// from files, environment variables, or keychain. They typically indicate
/// misconfiguration that must be fixed before the application can run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// Failed to load configuration from file or environment.
    ///
    /// This can occur due to file I/O errors, missing configuration files,
    /// or invalid environment variable values.
    #[error("failed to load config: {0}")]
    Load(String),

    /// Configuration schema version mismatch.
    ///
    /// The configuration file uses a different schema version than expected.
    /// This typically requires a migration or manual update to the config file.
    #[error("config schema mismatch: found v{found}, expected v{expected}")]
    SchemaMismatch { found: u32, expected: u32 },

    /// Required credential not found in keychain or environment.
    ///
    /// Provider API keys, authentication tokens, or other secrets are missing.
    /// Check keychain configuration or environment variables.
    #[error("credential not found: {0}")]
    CredentialMissing(String),

    /// Keychain operation failed.
    ///
    /// Failed to read, write, or delete credentials from the OS keychain.
    /// This can occur due to permission issues or keychain corruption.
    #[error("keychain operation failed: {0}")]
    Keychain(String),

    /// Configuration value is invalid or out of range.
    ///
    /// A configuration parameter has an invalid value (e.g., negative timeout,
    /// invalid URL, out-of-range numeric value).
    #[error("invalid config value: {0}")]
    InvalidValue(String),
}

/// LLM provider communication and execution errors.
///
/// These errors occur when interacting with LLM providers (OpenAI, Anthropic, etc.).
/// They cover authentication failures, rate limiting, network issues, and provider-specific
/// error conditions. Many of these errors are transient and can be retried.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// No model provider is configured for this run.
    ///
    /// The application attempted to make an LLM call but no provider was configured
    /// in the config file or environment. Check provider configuration.
    #[error("no model provider is configured for this run")]
    NotConfigured,

    /// Provider has no configured credential.
    ///
    /// The specified provider is configured but its API key or authentication
    /// credential is missing from the keychain or environment.
    #[error("provider '{provider}' has no configured credential")]
    CredentialMissing {
        /// The provider name (e.g., "openai", "anthropic").
        provider: String,
    },

    /// Unsupported provider type.
    ///
    /// The configuration specifies a provider type that Concerto doesn't support.
    /// Check the provider name spelling and ensure it's a supported provider.
    #[error("unsupported provider type '{provider}'")]
    UnsupportedProvider {
        /// The unsupported provider name.
        provider: String,
    },

    /// Rate limited by the provider.
    ///
    /// The provider rejected the request due to rate limiting. The `retry_after`
    /// duration indicates how long to wait before retrying. This is a transient
    /// error that should be retried after the specified delay.
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimit {
        /// How long to wait before retrying.
        retry_after: Duration,
    },

    /// Provider returned a non-2xx HTTP status code.
    ///
    /// The provider's API returned an error response. The status code, optional
    /// retry-after header, and error message are captured for retry policy
    /// classification and debugging.
    #[error("provider returned HTTP {status}: {message}")]
    HttpStatus {
        /// HTTP status code (e.g., 401, 429, 500).
        status: u16,
        /// Optional retry-after duration from the response header.
        retry_after: Option<Duration>,
        /// Error message from the provider's response body.
        message: String,
    },

    /// Context window overflow.
    ///
    /// The input tokens exceed the provider's context window capacity. This can
    /// occur when the conversation history is too long or the system prompt is
    /// too large. Consider truncating history or using a model with larger context.
    #[error("context overflow: {tokens_in} tokens in, capacity is {capacity}")]
    ContextOverflow {
        /// Number of input tokens attempted.
        tokens_in: u64,
        /// Provider's context window capacity in tokens.
        capacity: u64,
    },

    /// Provider authentication failed.
    ///
    /// The API key or credential is invalid, expired, or revoked. Check the
    /// credential in the keychain or environment and ensure it's valid.
    #[error("provider authentication failed")]
    AuthFailure,

    /// Provider call was cancelled.
    ///
    /// The LLM call was cancelled via CancellationToken before completion.
    /// This is not an error condition but rather an intentional cancellation.
    #[error("provider call cancelled")]
    Cancelled,

    /// Network-level failure during provider communication.
    ///
    /// DNS resolution failed, TLS handshake failed, connection was reset, or
    /// the socket was dropped. These are typically transient errors that can
    /// be retried with exponential backoff.
    #[error("network error: {0}")]
    Network(String),

    /// A provider request stopped making progress at a specific phase.
    #[error("provider {phase} timed out after {timeout:?}")]
    Timeout { phase: &'static str, timeout: Duration },

    /// JSON serialization or deserialization error.
    ///
    /// Failed to serialize a request to JSON or deserialize a response from JSON.
    /// This can occur due to malformed provider responses or internal serialization bugs.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Invalid or unexpected response from the provider.
    ///
    /// The provider returned a response that doesn't match the expected format.
    /// This can occur due to provider API changes, malformed responses, or
    /// streaming protocol errors.
    #[error("invalid response from provider: {0}")]
    InvalidResponse(String),

    /// Generic provider error not covered by other variants.
    ///
    /// Catch-all for provider-specific errors that don't fit into the other
    /// categories. The error message contains provider-specific details.
    #[error("provider error: {0}")]
    Other(String),

    /// All retry attempts were exhausted without success.
    ///
    /// The provider call was retried multiple times (optionally capped by an
    /// elapsed-time fuse) but all attempts failed. This is a terminal error
    /// that must be surfaced to the user, not treated as a successful completion.
    #[error("provider retries exhausted after {attempts} attempts ({elapsed:?}): {last_error}")]
    RetryExhausted {
        /// Number of retry attempts made.
        attempts: u32,
        /// Total elapsed time across all attempts.
        elapsed: Duration,
        /// Error message from the final failed attempt.
        last_error: String,
    },
}

impl ProviderError {
    /// Classify a provider error as transient (retryable) or permanent.
    ///
    /// Transient errors (rate limits, network failures, timeouts, 5xx, and
    /// generic provider errors) may resolve on retry and should be treated
    /// as recoverable by the coordinator's subtask retry mechanism.
    /// Non-transient errors (auth, config, cancellation, context overflow)
    /// will not resolve on retry and should fail immediately.
    pub fn is_transient(&self) -> bool {
        match self {
            ProviderError::RateLimit { .. } => true,
            ProviderError::HttpStatus { status, .. } => *status >= 500 || *status == 429,
            ProviderError::Network(_) => true,
            ProviderError::Timeout { .. } => true,
            ProviderError::InvalidResponse(_) => true,
            // Generic catch-all — assume transient to avoid false fatal
            ProviderError::Other(_) => true,
            // Serialization errors are typically code bugs, not transient
            ProviderError::Serialization(_) => false,
            // RetryExhausted is already a terminal retry signal
            ProviderError::RetryExhausted { .. } => false,
            // Everything below here is configuration, auth, or cancellation
            ProviderError::NotConfigured
            | ProviderError::CredentialMissing { .. }
            | ProviderError::UnsupportedProvider { .. }
            | ProviderError::AuthFailure
            | ProviderError::Cancelled
            | ProviderError::ContextOverflow { .. } => false,
        }
    }
}

/// Tool execution and policy enforcement errors.
///
/// These errors occur when executing tools (filesystem, shell, git, LSP) or
/// when policy rules deny an operation. They cover permission denials, execution
/// failures, timeouts, and virtual filesystem conflicts.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// Policy denied the tool execution.
    ///
    /// The configured policy rules explicitly denied this tool operation.
    /// The rule name indicates which policy rule triggered the denial.
    #[error("policy denied: {rule}")]
    PolicyDenied {
        /// Name of the policy rule that denied the operation.
        rule: String,
    },

    /// Tool execution failed.
    ///
    /// The tool encountered an error during execution. The message contains
    /// tool-specific error details (e.g., command failed, file not found).
    #[error("tool execution failed: {message}")]
    ExecutionFailed {
        /// Tool-specific error message.
        message: String,
    },

    /// Operation requires a git repository but git is unavailable or not a repo.
    ///
    /// The tool attempted a git operation (undo, diff, etc.) but the project
    /// directory is not a git repository or git is not installed.
    #[error("not a git repository (or git is unavailable): {message}")]
    NotARepository {
        /// Additional context about the failure.
        message: String,
    },

    /// Tool execution timed out.
    ///
    /// The tool exceeded its configured timeout duration and was terminated.
    /// This can occur with long-running shell commands or network operations.
    #[error("operation timed out after {timeout_secs}s")]
    Timeout {
        /// Timeout duration in seconds.
        timeout_secs: u64,
    },

    /// Tool execution was cancelled.
    ///
    /// The tool was cancelled via CancellationToken before completion.
    /// This is not an error condition but rather an intentional cancellation.
    #[error("cancelled")]
    Cancelled,

    /// Virtual filesystem conflict detected.
    ///
    /// The tool attempted to modify a file that has conflicting changes in the
    /// virtual filesystem overlay. The path and conflict reason are provided
    /// for debugging and resolution.
    #[error("virtual fs conflict on {path}: {reason}")]
    VirtualFsConflict {
        /// Path to the conflicting file.
        path: camino::Utf8PathBuf,
        /// Description of the conflict.
        reason: String,
    },

    /// Tool does not support rollback operations.
    ///
    /// An attempt was made to rollback a tool execution, but the tool does not
    /// implement rollback functionality. Only tools with explicit rollback
    /// support can be undone.
    #[error("rollback not supported by this tool")]
    RollbackNotSupported,

    /// Language Server Protocol (LSP) error.
    ///
    /// An error occurred during LSP communication or operation. This can include
    /// server startup failures, request timeouts, or protocol errors.
    #[error("LSP error: {message}")]
    LspError {
        /// LSP-specific error message.
        message: String,
    },

    /// I/O error during tool execution.
    ///
    /// A filesystem, network, or process I/O error occurred. This is typically
    /// a low-level system error wrapped from `std::io::Error`.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Policy evaluation and enforcement errors.
///
/// These errors occur during policy rule evaluation, approval workflows, and
/// audit log operations. They cover rule violations, timeouts, and configuration
/// errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PolicyError {
    /// Policy rule violation.
    ///
    /// The operation violated a configured policy rule. The message describes
    /// which rule was violated and why.
    #[error("policy rule violation: {0}")]
    RuleViolation(String),

    /// Approval request timed out.
    ///
    /// The user did not respond to an approval request within the configured
    /// timeout period. The operation was denied by default.
    #[error("approval timed out")]
    ApprovalTimeout,

    /// Policy evaluation was cancelled.
    ///
    /// The policy evaluation was cancelled via CancellationToken before completion.
    /// This can occur when the user cancels the operation or the session ends.
    #[error("policy evaluation cancelled")]
    Cancelled,

    /// Audit log write failed.
    ///
    /// Failed to write an audit entry to the persistent audit log. This can
    /// occur due to database errors, disk full, or permission issues.
    #[error("audit log write failed: {0}")]
    AuditLogWriteFailed(String),

    /// Invalid policy rule configuration.
    ///
    /// A policy rule in the configuration is malformed or contains invalid
    /// values. The message describes the specific validation error.
    #[error("invalid policy rule: {0}")]
    InvalidRule(String),
}

/// Memory system errors (storage, retrieval, indexing, embeddings).
///
/// These errors occur during memory operations including persistence, retrieval,
/// indexing, embedding generation, and vector store operations. They cover
/// database errors, serialization failures, and embedding model mismatches.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// Operation was cancelled before completion.
    #[error("cancelled")]
    Cancelled,

    /// Database persistence error.
    ///
    /// Failed to write to or read from the memory database (SQLite). This can
    /// occur due to database corruption, disk full, or permission issues.
    #[error("persistence error: {0}")]
    Persistence(String),

    /// Serialization or deserialization error.
    ///
    /// Failed to serialize memory data to JSON or deserialize it back. This can
    /// occur due to data corruption or schema changes.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Memory entry not found.
    ///
    /// The requested memory entry (by ID or key) does not exist in the store.
    /// This is not necessarily an error condition but indicates a cache miss.
    #[error("entry not found: {0}")]
    NotFound(String),

    /// File indexing failed.
    ///
    /// Failed to index a file for memory retrieval. The path and reason are
    /// provided for debugging (e.g., parse error, unsupported format).
    #[error("indexing failed for {path}: {reason}")]
    IndexingFailed {
        /// Path to the file that failed to index.
        path: Utf8PathBuf,
        /// Reason for the indexing failure.
        reason: String,
    },

    /// Memory retrieval failed.
    ///
    /// Failed to retrieve memory entries matching the query. This can occur
    /// due to database errors or query parsing failures.
    #[error("retrieval failed: {0}")]
    RetrievalFailed(String),

    /// Embedding generation failed.
    ///
    /// Failed to generate vector embeddings for text. This can occur due to
    /// embedding model errors, API failures, or invalid input text.
    #[error("embedding failed: {reason}")]
    EmbeddingFailed {
        /// Reason for the embedding failure.
        reason: String,
    },

    /// Vector store operation failed.
    ///
    /// An error occurred during vector store operations (insert, search, delete).
    /// This can occur due to database errors or vector dimension mismatches.
    #[error("vector store error: {reason}")]
    VectorStoreError {
        /// Reason for the vector store error.
        reason: String,
    },

    /// Embedding model version mismatch.
    ///
    /// The stored embeddings were generated with a different model version than
    /// the current one. The embeddings are stale and should be regenerated.
    #[error("stale embedding: stored model version {stored}, current {current}")]
    StaleEmbedding {
        /// Model version used to generate the stored embeddings.
        stored: String,
        /// Current model version.
        current: String,
    },

    /// Text summarization failed.
    ///
    /// Failed to summarize text (e.g., for context compaction). This can occur
    /// due to provider errors, invalid input, or summarization model failures.
    #[error("summarization failed: {reason}")]
    SummarizationFailed {
        /// Reason for the summarization failure.
        reason: String,
    },

    /// Database schema version mismatch.
    ///
    /// The memory database schema version doesn't match the expected version.
    /// This typically requires a database migration or recreation.
    #[error("schema version mismatch: stored={stored}, current={current}")]
    SchemaVersionMismatch {
        /// Schema version found in the database.
        stored: String,
        /// Expected schema version.
        current: String,
    },

    /// Database schema migration failed.
    ///
    /// Failed to migrate the memory database schema to a newer version. This
    /// can occur due to migration script errors or incompatible data.
    #[error("schema migration failed: {0}")]
    MigrationFailed(String),

    /// Embeddings are stale and pending re-index.
    ///
    /// The embeddings in the vector store are outdated and need to be regenerated.
    /// This is a warning condition that indicates a re-index operation is needed.
    #[error("embeddings are stale and pending re-index")]
    StaleEmbeddings,

    /// Cross-project vector leakage detected.
    ///
    /// A critical security error indicating that vectors from one project were
    /// accessible in another project's context. This is a bug and should be
    /// reported immediately.
    #[error("cross-project vector leakage detected — this is a bug")]
    CrossProjectLeakage,

    /// Data directory is locked by another instance.
    ///
    /// Another Concerto instance is already using the data directory. Only one
    /// instance can access the data directory at a time to prevent corruption.
    #[error("another instance holds the data directory lock")]
    DataDirLocked,
}

/// Session persistence and replay errors.
///
/// These errors occur during session storage, retrieval, and replay operations.
/// They cover database errors, data corruption, and missing task references.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionError {
    /// Session persistence failed.
    ///
    /// Failed to write session data to the database. This can occur due to
    /// database errors, disk full, or permission issues.
    #[error("session persistence failed: {0}")]
    PersistenceFailed(String),

    /// Session replay data is corrupted.
    ///
    /// The session replay log contains invalid or corrupted data that cannot
    /// be parsed. This can occur due to incomplete writes or data corruption.
    #[error("session replay corrupted: {0}")]
    ReplayCorruption(String),

    /// Task not found in session.
    ///
    /// The requested task ID does not exist in the session. This can occur
    /// if the task was deleted or the session was reset.
    #[error("task not found: {0}")]
    TaskNotFound(String),
}

/// Undo operation errors.
///
/// These errors occur during git stash-based undo operations. They cover
/// missing stashes, failed pops, and already-committed changes.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UndoError {
    /// Git stash pop failed.
    ///
    /// Failed to apply the stashed changes back to the working directory.
    /// This can occur due to merge conflicts or git errors.
    #[error("stash pop failed: {reason}")]
    StashPopFailed {
        /// Reason for the stash pop failure.
        reason: String,
    },

    /// No stash found for the session.
    ///
    /// The undo operation requires a git stash for the session, but no stash
    /// exists. This can occur if the stash was already applied or deleted.
    #[error("no stash found for session {session_id}")]
    StashNotFound {
        /// Session ID that should have a stash.
        session_id: Ulid,
    },

    /// Cannot undo: changes already committed to disk.
    ///
    /// The VirtualFs overlay was already committed to disk, so the changes
    /// cannot be undone via git stash. Manual intervention is required.
    #[error("cannot undo: VirtualFs was already committed to disk")]
    AlreadyCommitted,
}

/// Agent orchestration and execution errors.
///
/// These errors occur during agent loop execution, task graph management,
/// multi-agent coordination, and review/validation cycles. They cover
/// iteration limits, cycle detection, budget exhaustion, and task graph errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OrchestratorError {
    /// Agent loop execution error.
    ///
    /// A generic error occurred during agent loop execution. The message
    /// contains details about the specific failure.
    #[error("agent loop error: {0}")]
    AgentLoopError(String),

    /// Maximum iterations reached without completing the task.
    ///
    /// The agent loop exceeded its configured iteration limit without
    /// successfully completing the task. This can indicate an infinite loop
    /// or an overly complex task.
    #[error("max iterations ({max}) reached without completing the task")]
    MaxIterationsReached {
        /// Maximum number of iterations allowed.
        max: u32,
    },

    /// Tool call cycle detected.
    ///
    /// The agent is calling the same tool with identical input repeatedly,
    /// indicating a potential infinite loop or stuck state.
    #[error("cycle detected: tool '{tool_name}' called {count} times with identical input")]
    CycleDetected {
        /// Name of the tool being called repeatedly.
        tool_name: String,
        /// Number of times the tool was called with identical input.
        count: u32,
    },

    /// Task was cancelled.
    ///
    /// The task was cancelled via CancellationToken before completion.
    /// This is not an error condition but rather an intentional cancellation.
    #[error("task was cancelled")]
    Cancelled,

    /// Unrecoverable error occurred.
    ///
    /// A critical error occurred that cannot be recovered from. The agent
    /// must terminate and report the error to the user.
    #[error("unrecoverable error: {message}")]
    Unrecoverable {
        /// Description of the unrecoverable error.
        message: String,
    },

    /// Task graph construction or execution error.
    ///
    /// An error occurred during task graph operations (construction, validation,
    /// execution). The message contains details about the specific failure.
    #[error("task graph error: {0}")]
    TaskGraphError(String),

    /// Provider error during orchestration.
    ///
    /// A provider error occurred during agent execution. This is a transparent
    /// wrapper around `ProviderError` for error propagation.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Tool error during orchestration.
    ///
    /// A tool error occurred during agent execution. This is a transparent
    /// wrapper around `ToolError` for error propagation.
    #[error(transparent)]
    Tool(#[from] ToolError),

    /// Memory error during orchestration.
    ///
    /// A memory error occurred during agent execution. This is a transparent
    /// wrapper around `MemoryError` for error propagation.
    #[error(transparent)]
    Memory(#[from] MemoryError),

    /// Multi-agent planning failed.
    ///
    /// The coordinator failed to create a valid task plan for multi-agent
    /// execution. This can occur due to complex task decomposition or
    /// resource constraints.
    #[error("multi-agent planning failed: {reason}")]
    MultiAgentPlanFailed {
        /// Reason for the planning failure.
        reason: String,
    },

    /// Invalid task graph structure.
    ///
    /// The task graph contains invalid structure (e.g., cycles, missing
    /// dependencies, invalid edges). The reason describes the specific issue.
    #[error("invalid task graph: {reason}")]
    InvalidTaskGraph {
        /// Description of the graph validation error.
        reason: String,
    },

    /// No remaining budget for task delegation.
    ///
    /// The spend tracker has reached its limit and no further budget is
    /// available for delegating tasks to agents.
    #[error("no remaining budget for delegation")]
    NoBudgetForDelegation,

    /// Maximum review cycles exceeded.
    ///
    /// The task exceeded the configured maximum number of review cycles
    /// without passing review. This can indicate a persistent quality issue.
    #[error("max review cycles ({cycles}) exceeded for task {task_id}")]
    MaxReviewCyclesExceeded {
        /// Task ID that exceeded review cycles.
        task_id: crate::types::TaskId,
        /// Number of review cycles attempted.
        cycles: u32,
    },

    /// Maximum validation cycles exceeded.
    ///
    /// The task exceeded the configured maximum number of validation cycles
    /// without passing validation. This can indicate a persistent issue.
    #[error("max validation cycles ({cycles}) exceeded for task {task_id}")]
    MaxValidationCyclesExceeded {
        /// Task ID that exceeded validation cycles.
        task_id: crate::types::TaskId,
        /// Number of validation cycles attempted.
        cycles: u32,
    },

    /// No affordable model available for the agent role.
    ///
    /// The routing engine could not find a model within the budget constraints
    /// for the specified agent role. Consider increasing the budget or using
    /// a cheaper model.
    #[error("no affordable model for role {role:?}")]
    NoAffordableModel {
        /// Agent role that requires a model.
        role: crate::types::AgentId,
    },

    /// No model with required capability for the agent role.
    ///
    /// The routing engine could not find a model that supports the required
    /// capability (e.g., tool calling, vision) for the specified agent role.
    #[error("no model with required capability '{capability}' for role {role:?}")]
    NoCapableModel {
        /// Agent role that requires a model.
        role: crate::types::AgentId,
        /// Required capability that no model supports.
        capability: String,
    },

    /// Pinned model not found in provider configuration.
    ///
    /// The configuration specifies a pinned model for an agent role, but that
    /// model is not available in the provider's model list. Check the model
    /// name spelling and provider configuration.
    #[error(
        "configured model '{model}' was not found for role {role:?} and provider config {provider_config_id:?}"
    )]
    PinnedModelNotFound {
        /// Agent role that requires the model.
        role: crate::types::AgentId,
        /// Provider configuration ID (if specified).
        provider_config_id: Option<String>,
        /// Model name that was not found.
        model: String,
    },

    /// Pinned model lacks required capability.
    ///
    /// The configuration specifies a pinned model for an agent role, but that
    /// model does not support the required capability (e.g., tool calling).
    #[error("pinned model '{model}' lacks required capability '{capability}' for role {role:?}")]
    PinnedModelMissingCapability {
        /// Agent role that requires the capability.
        role: crate::types::AgentId,
        /// Model name that lacks the capability.
        model: String,
        /// Required capability that the model doesn't support.
        capability: String,
    },

    /// Pinned model exceeds remaining budget.
    ///
    /// The configuration specifies a pinned model for an agent role, but the
    /// estimated cost exceeds the remaining budget. Consider using a cheaper
    /// model or increasing the budget.
    #[error("pinned model '{model}' exceeds remaining budget (estimated {estimated:.4}, remaining {remaining:.4})")]
    PinnedModelBudgetExceeded {
        /// Model name that exceeds the budget.
        model: String,
        /// Estimated cost for the model.
        estimated: f64,
        /// Remaining budget available.
        remaining: f64,
    },

    /// Action-required task completed without executing any tools.
    ///
    /// A task marked as action-required (expecting tool execution) completed
    /// without calling any tools. This indicates the agent did not attempt
    /// the required action.
    #[error("action-required task completed with zero tool calls")]
    ExecutionRequiredButNoTools,

    /// Subtask retries exhausted without success.
    ///
    /// A subtask remained blocked after multiple retry attempts. The subtask
    /// could not make progress and is now considered failed.
    #[error(
        "subtask {task_id} ({role:?}) remained blocked after {attempts} attempts: {last_error}"
    )]
    SubTaskRetriesExhausted {
        /// Task ID of the failed subtask.
        task_id: crate::types::TaskId,
        /// Agent role assigned to the subtask.
        role: crate::types::AgentId,
        /// Number of retry attempts made.
        attempts: u32,
        /// Error message from the final failed attempt.
        last_error: String,
    },

    /// Invalid policy configuration.
    ///
    /// The policy configuration contains invalid rules or values that prevent
    /// the policy engine from initializing. Check the policy rules in the
    /// configuration file.
    #[error("invalid policy configuration: {reason}")]
    InvalidPolicyConfiguration {
        /// Description of the configuration error.
        reason: String,
    },
}

/// Evaluation and benchmark errors.
///
/// These errors occur during evaluation harness setup, test execution, and
/// benchmark runs. They cover setup failures, test runner errors, missing
/// benchmark suites, and regression detection.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EvalError {
    /// Evaluation harness setup failed.
    ///
    /// Failed to initialize the evaluation harness (e.g., missing dependencies,
    /// invalid configuration). The message contains details about the failure.
    #[error("eval harness setup failed: {0}")]
    HarnessSetupFailed(String),

    /// Test runner execution failed.
    ///
    /// The test runner encountered an error during execution (e.g., test
    /// framework crash, invalid test format). The message contains details
    /// about the failure.
    #[error("test runner failed: {0}")]
    TestRunnerFailed(String),

    /// Benchmark suite not found.
    ///
    /// The requested benchmark suite does not exist or is not configured.
    /// Check the benchmark suite name and configuration.
    #[error("benchmark suite not found: {0}")]
    BenchmarkSuiteNotFound(String),

    /// Performance regression detected.
    ///
    /// The current benchmark results show a significant degradation compared
    /// to the baseline. The metric, degradation percentage, and baseline/current
    /// values are provided for analysis.
    #[error("regression detected: {metric} degraded by {delta_pct:.1}% (baseline={baseline:.4}, current={current:.4})")]
    RegressionDetected {
        /// Name of the degraded metric.
        metric: String,
        /// Percentage degradation from baseline.
        delta_pct: f64,
        /// Baseline metric value.
        baseline: f64,
        /// Current metric value.
        current: f64,
    },

    /// Evaluation task timed out.
    ///
    /// The evaluation task exceeded its configured timeout duration and was
    /// terminated. This can occur with long-running tests or benchmarks.
    #[error("eval task timed out after {timeout_ms}ms")]
    TaskTimeout {
        /// Timeout duration in milliseconds.
        timeout_ms: u64,
    },

    /// Mock provider not configured for evaluation.
    ///
    /// Evaluation tasks require either a MockProvider (for deterministic testing)
    /// or real API keys (for integration testing). Neither is configured.
    #[error("mock provider not configured — eval tasks require MockProvider or real API keys")]
    MockProviderMissing,
}

/// Observability exporter initialization and operation errors.
///
/// These errors occur during observability exporter setup (OpenTelemetry,
/// Prometheus, Langfuse) and shutdown. They cover initialization failures
/// and missing trace spans.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObservabilityError {
    /// OpenTelemetry exporter initialization failed.
    ///
    /// Failed to initialize the OpenTelemetry exporter (e.g., invalid endpoint,
    /// missing credentials). The message contains details about the failure.
    #[error("otel exporter initialization failed: {0}")]
    OtelInitFailed(String),

    /// Prometheus exporter initialization failed.
    ///
    /// Failed to initialize the Prometheus metrics exporter (e.g., port already
    /// in use, invalid configuration). The message contains details about the
    /// failure.
    #[error("prometheus exporter initialization failed: {0}")]
    PrometheusInitFailed(String),

    /// Langfuse exporter initialization failed.
    ///
    /// Failed to initialize the Langfuse exporter (e.g., invalid host, missing
    /// API keys). The message contains details about the failure.
    #[error("langfuse exporter initialization failed: {0}")]
    LangfuseInitFailed(String),

    /// Exporter shutdown failed.
    ///
    /// Failed to gracefully shutdown an observability exporter. This can occur
    /// due to network errors or exporter-specific issues. Data may not have
    /// been fully flushed.
    #[error("exporter shutdown failed: {0}")]
    ShutdownFailed(String),

    /// No active trace span for cost attribution.
    ///
    /// Attempted to attribute provider costs to a trace span, but no active
    /// span exists. This indicates a missing instrumentation point.
    #[error("no active trace span for cost attribution")]
    NoActiveSpan,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    #[error("sandbox profile {profile} is not implemented: {reason}")]
    NotImplemented { profile: String, reason: String },

    #[error("sandbox denied operation: {operation} under profile {profile}")]
    Denied { operation: String, profile: String },

    #[error("spend cap exceeded: type={cap_type}, current={current_usd:.4}, cap={cap_usd:.4}")]
    SpendCapExceeded { cap_type: String, current_usd: f64, cap_usd: f64 },

    #[error("rate limit exceeded: provider={provider}, rpm={rpm}")]
    RateLimitExceeded { provider: String, rpm: u64 },
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn core_error_display_event_bus() {
        let err = CoreError::EventBus("channel closed".into());
        assert!(err.to_string().contains("channel closed"));
    }

    #[test]
    fn tool_error_display_execution_failed() {
        let err = ToolError::ExecutionFailed { message: "command not found".into() };
        assert!(err.to_string().contains("command not found"));
    }

    #[test]
    fn tool_error_display_policy_denied() {
        let err = ToolError::PolicyDenied { rule: "not allowed".into() };
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn tool_error_display_timeout() {
        let err = ToolError::Timeout { timeout_secs: 30 };
        assert!(err.to_string().contains("30"));
    }

    #[test]
    fn tool_error_display_not_a_repository() {
        let err = ToolError::NotARepository { message: "no .git".into() };
        assert!(err.to_string().contains("no .git"));
    }

    #[test]
    fn provider_error_display_auth_failure() {
        let err = ProviderError::AuthFailure;
        assert!(err.to_string().contains("auth"));
    }

    #[test]
    fn provider_error_display_rate_limited() {
        let err = ProviderError::RateLimit { retry_after: std::time::Duration::from_secs(60) };
        assert!(err.to_string().contains("60"));
    }

    #[test]
    fn provider_error_display_context_overflow() {
        let err = ProviderError::ContextOverflow { tokens_in: 5000, capacity: 4096 };
        assert!(err.to_string().contains("5000"));
        assert!(err.to_string().contains("4096"));
    }

    #[test]
    fn policy_error_display() {
        let err = PolicyError::RuleViolation("no matching rule found".into());
        assert!(err.to_string().contains("no matching rule"));
        let err = PolicyError::Cancelled;
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn memory_error_display() {
        let err = MemoryError::NotFound("chunk".into());
        assert!(err.to_string().contains("chunk"));
        let err = MemoryError::Persistence("db locked".into());
        assert!(err.to_string().contains("db locked"));
    }

    #[test]
    fn config_error_display() {
        let err = ConfigError::Load("missing file".into());
        assert!(err.to_string().contains("missing file"));
    }

    #[test]
    fn sandbox_error_display() {
        let err = SandboxError::NotImplemented {
            profile: "container".into(),
            reason: "not ready".into(),
        };
        assert!(err.to_string().contains("container"));
        let err = SandboxError::SpendCapExceeded {
            cap_type: "daily".into(),
            current_usd: 5.0,
            cap_usd: 10.0,
        };
        assert!(err.to_string().contains("5.0"));
        let err = SandboxError::RateLimitExceeded { provider: "openai".into(), rpm: 100 };
        assert!(err.to_string().contains("openai"));
    }

    #[test]
    fn orchestrator_error_display() {
        let err = OrchestratorError::CycleDetected { tool_name: "read_file".into(), count: 3 };
        assert!(err.to_string().contains("read_file"));
        assert!(err.to_string().contains("3"));
        let err = OrchestratorError::NoBudgetForDelegation;
        assert!(err.to_string().contains("budget"));
    }

    #[test]
    fn observability_error_display() {
        let err = ObservabilityError::PrometheusInitFailed("port in use".into());
        assert!(err.to_string().contains("port in use"));
        let err = ObservabilityError::OtelInitFailed("timeout".into());
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn conversion_from_tool_error_to_orchestrator_error() {
        let tool_err = ToolError::ExecutionFailed { message: "oops".into() };
        let orch_err: OrchestratorError = tool_err.into();
        assert!(orch_err.to_string().contains("oops"));
    }

    #[test]
    fn describe_error_chain_returns_message_when_no_source() {
        let err = CoreError::EventBus("simple".into());
        let result = describe_error_chain(&err);
        assert_eq!(result, "event bus send failed: simple");
    }

    #[test]
    fn orchestrator_error_display_retries_exhausted() {
        let err = OrchestratorError::SubTaskRetriesExhausted {
            task_id: crate::types::TaskId::new(),
            role: crate::types::AgentId::new("coder"),
            attempts: 3,
            last_error: "rate limited".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("coder"), "should include role name");
        assert!(msg.contains("3"), "should include attempt count");
        assert!(msg.contains("rate limited"), "should include last error");
    }

    #[test]
    fn orchestrator_error_display_invalid_task_graph() {
        let err = OrchestratorError::InvalidTaskGraph { reason: "missing node".into() };
        let msg = err.to_string();
        assert!(msg.contains("missing node"), "should include reason");
    }

    #[test]
    fn provider_error_display_cancelled() {
        let err = ProviderError::Cancelled;
        assert!(
            err.to_string().contains("cancelled"),
            "cancelled provider error should display correctly"
        );
    }

    #[test]
    fn provider_error_display_retry_exhausted() {
        let err = ProviderError::RetryExhausted {
            attempts: 5,
            elapsed: std::time::Duration::from_secs(30),
            last_error: "timeout".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("5"), "should include attempt count");
        assert!(msg.contains("30"), "should include elapsed seconds");
        assert!(msg.contains("timeout"), "should include last error");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `describe_error_chain` never panics regardless of the error's message
        /// content or type. Tests against 4 different core error types.
        #[test]
        fn describe_error_chain_never_panics(msg in ".{0,100}") {
            let errors: [Box<dyn std::error::Error + 'static>; 4] = [
                Box::new(CoreError::EventBus(msg.clone())),
                Box::new(ConfigError::Load(msg.clone())),
                Box::new(MemoryError::Persistence(msg.clone())),
                Box::new(ToolError::ExecutionFailed { message: msg.clone() }),
            ];
            for err in &errors {
                let _ = describe_error_chain(err.as_ref());
                // No panic = success
            }
        }

        /// `describe_error_chain` includes all source errors in its output,
        /// separated by ` | caused by: ` chains.
        #[test]
        fn describe_error_chain_includes_all_sources(msg in "[a-z]{1,50}") {
            // Build a 3-level chain: io::Error <- ToolError <- OrchestratorError
            let io_err = std::io::Error::other(msg.clone());
            let tool_err = ToolError::Io(io_err);
            let orch_err = OrchestratorError::Tool(tool_err);

            let result = describe_error_chain(&orch_err);

            // The deepest source message must appear in the output
            prop_assert!(
                result.contains(&msg),
                "output should contain the deepest source message: {:?}",
                msg,
            );
        }
    }
}
