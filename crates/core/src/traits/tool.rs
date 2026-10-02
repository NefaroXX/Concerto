use crate::error::ToolError;
use crate::traits::PolicyEngine;
use crate::types::{
    CapabilitySet, CommandPolicyFacts, PathPolicyFacts, RollbackSnapshot, SessionContext,
    ToolOutput,
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

    /// Resolve a concrete request before policy and approval. Implementations
    /// must not perform effects here. The prepared input is both evaluated and
    /// executed, so PATH lookup need not change while approval is pending.
    fn prepare_input(
        &self,
        _input: &mut serde_json::Value,
        _session: &SessionContext,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    /// Backend-selected isolation requirement; never taken from agent input.
    fn sandbox_profile(&self) -> Option<crate::types::SandboxProfile> {
        None
    }

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

    /// Structured, pre-resolved facts about a path-shaped tool operation.
    ///
    /// Path-shaped tools (filesystem, git, LSP, …) override this to record the
    /// operation and the target path — attempted as supplied and resolved after
    /// workspace containment — so the audit log can attribute the action to a
    /// concrete target rather than a hash only. The session is supplied so
    /// relative paths can be resolved before approval. The default (`None`)
    /// preserves existing behaviour for every other tool, so adding this method
    /// is non-breaking.
    fn path_facts(
        &self,
        _input: &serde_json::Value,
        _session: &SessionContext,
    ) -> Option<PathPolicyFacts> {
        None
    }

    /// Canonical `(tool_name, input)` the policy engine evaluates for this
    /// call.
    ///
    /// The default is the tool's own registered name and the caller's input,
    /// so every existing tool keeps its current rule coverage. Thin alias
    /// tools (e.g. a `write` tool delegating to `filesystem`) override this to
    /// present the CANONICAL tool's name and operation-bearing input, so the
    /// alias inherits the canonical tool's `Condition::ToolName` /
    /// `Condition::Operation` rule coverage exactly instead of falling through
    /// to the catch-all. Execution and audit still use the registered name and
    /// the caller's original input.
    fn policy_view(&self, input: &serde_json::Value) -> (String, serde_json::Value) {
        (self.name().to_string(), input.clone())
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
