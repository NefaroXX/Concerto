use crate::error::ToolError;
use crate::traits::PolicyEngine;
use crate::types::{
    CapabilitySet, CommandPolicyFacts, RollbackSnapshot, SessionContext, ToolOutput,
};
use crate::CancellationToken;
use async_trait::async_trait;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    /// JSON Schema describing this tool's input shape.
    fn input_schema(&self) -> serde_json::Value;

    fn capability_requirements(&self) -> CapabilitySet;

    /// ADR-28 §6: optional structured command-execution facts for this tool.
    ///
    /// Command-executing tools (e.g. the shell tool) override this to return
    /// the resolved executable, argv, effective working directory, and related
    /// classifications, which the executor merges into the `PolicyAction`
    /// presented to the policy engine and audit log. The session is supplied
    /// so project-relative paths can be classified before approval. The default (`None`)
    /// preserves existing behaviour for every other tool, so adding this
    /// method is non-breaking.
    fn command_facts(
        &self,
        _input: &serde_json::Value,
        _session: &SessionContext,
    ) -> Option<CommandPolicyFacts> {
        None
    }

    fn rollback_support(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError>;

    async fn rollback(
        &self,
        _snapshot: RollbackSnapshot,
        _cancel: CancellationToken,
    ) -> Result<(), ToolError> {
        Err(ToolError::RollbackNotSupported)
    }
}
