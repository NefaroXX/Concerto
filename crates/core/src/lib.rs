#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]
//! `concerto-core` — the foundational contracts every other crate builds on.
//!
//! Provides the layered error taxonomy, event bus, ULID-based correlation
//! IDs, core trait definitions (provider, tool, agent, policy, memory,
//! approval), and foundational types.

pub mod authorization;
pub mod error;
pub mod event;
pub mod executor;
pub mod failures;
pub mod helpers;
pub mod ids;
pub mod intent;
pub mod memory;
pub mod policy;
pub mod policy_presets;
pub mod sanitizer;
#[cfg(test)]
pub mod testing;
pub mod text;
pub mod traits;
pub mod transcript;
pub mod types;

/// Every async operation that does I/O, calls a provider, executes a tool,
/// or touches memory must accept this. Re-exported here so downstream
/// crates depend on `concerto_core::CancellationToken`, not on
/// `tokio_util` directly — keeps the cancellation primitive swappable
/// behind one seam if it ever needs to be.
pub use tokio_util::sync::CancellationToken;

/// Re-export common error types for convenience.
pub use error::{EvalError, MemoryError, OrchestratorError, ToolError, UndoError};
pub use executor::ToolExecutor;
pub use policy::SimplePolicyEngine;
pub use policy::{RpmLimiter, SpendTracker};
pub use policy_presets::{inject_intent_gate_rule, PolicyPresets};

pub use types::{
    AgentId, AgentOutput, AgentStage, AgentTask, McpServerState, ModelInfo, ProjectId,
    SubTaskStatus, TaskId,
};

// Phase 0 intent-routing vocabulary (ADR-55): types plus the confidence
// threshold the router uses for path selection.
pub use intent::{
    PlanDecision, RequestedOutcome, RouterOutput, RouterRoute, RunStage, TaskScope,
    LOW_CONFIDENCE_THRESHOLD,
};

// ADR-55 batch 1c: intent tiers, the pure classifier, the verdict-source
// authorization seam the policy engine consults under
// `Condition::IntentAuthorized`, and the audit rule-name vocabulary.
pub use authorization::{
    classify_tier, IntentAuthorization, IntentTier, IntentVerdict, RULE_CONSEQUENTIAL,
    RULE_INTENT_AUTHORIZED, RULE_INTENT_READONLY_DENY, RULE_OBSERVE, RULE_SHELL_REQUIRES_APPROVAL,
    RULE_UN_GRANTED,
};

// Re-export Phase 4 memory types from the dedicated module.
pub use memory::{
    ChunkType, Decision, DecisionCategory, DecisionId, EmbeddingRecord, FtsResult, MemoryChunk,
    MemoryEntry, MemoryFilter, MemoryId, MemoryNamespace, MemoryQuery, TaskNode, TaskNodeId,
    TaskStatus, VectorResult, WorkingMemorySnapshot,
};

// Re-export traits needed by other crates
pub use traits::approval::{ApprovalDecision, ApprovalSink};
pub use traits::context_overflow::{ContextOverflowStrategy, NoOpOverflowStrategy, TruncateOldest};
pub use traits::memory::MemoryStore;
pub use traits::provider::LlmProvider;
pub use traits::vector_store::VectorStore;

/// Durable typed session transcript model (ADR-36).
pub use transcript::{
    transcript_entry_from_event, transcript_entry_from_event_with_labels, GateLabels,
    TranscriptEntry, TranscriptToolStatus,
};

/// Agent execution interface.
///
/// This trait defines the contract for executing agent tasks, allowing the eval crate
/// to depend on core instead of orchestrator, breaking the circular dependency.
pub mod agent {
    use crate::error::OrchestratorError;
    use crate::types::{AgentOutput, AgentTask};
    use crate::CancellationToken;

    /// Trait for an agent that can execute tasks.
    pub trait AgentRunner {
        fn run_task(
            &mut self,
            task: AgentTask,
            cancel: CancellationToken,
        ) -> impl std::future::Future<Output = Result<AgentOutput, OrchestratorError>> + Send;
    }
}

pub use agent::AgentRunner;
