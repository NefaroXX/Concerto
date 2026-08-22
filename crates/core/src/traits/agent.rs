//! Agent contract — used for both the Phase 3 single-agent loop and the
//! Phase 5 specialist agents (audit A-01: all built-ins are config-driven
//! seeds backed by the generic specialist). One trait for both keeps the
//! Phase 5 multi-agent mode an additive change rather than a parallel
//! system.
//!
//! Phase 5 update: `run` now takes `&SubTask` + `AgentContext` instead of
//! individual arguments, and returns `AgentRunResult` instead of `AgentOutput`.

use crate::error::OrchestratorError;
use crate::types::{AgentContext, AgentId, AgentRunResult, AgentStage, CapabilitySet, SubTask};
use crate::CancellationToken;
use async_trait::async_trait;

#[async_trait]
pub trait ExpertAgent: Send + Sync {
    fn id(&self) -> AgentId;

    /// The pipeline stage this agent belongs to, if any.
    /// `None` means Freeform (no lifecycle participation).
    fn stage(&self) -> Option<AgentStage> {
        None
    }

    fn capabilities(&self) -> CapabilitySet;

    async fn run(
        &self,
        task: &SubTask,
        context: AgentContext,
        model: &str,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError>;
}
