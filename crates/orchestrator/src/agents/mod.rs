//! Specialist agent implementations for Phase 5 multi-agent orchestration.
//!
//! Each agent implements the [`ExpertAgent`] trait and is registered in
//! [`AgentRegistry`](crate::registry::AgentRegistry).
//!
//! ADR-35 phase 4 + audit A-01: all five built-ins (architect, researcher,
//! coder, reviewer, validator) are config-driven seeds backed by
//! [`GenericSpecialistAgent`] (defined in the config crate). The validator
//! seed carries an attached [`EvalEngine`](concerto_eval::EvalEngine) and
//! runs in eval mode (no LLM call).

pub(crate) mod execution_state;
mod generic;
pub(crate) mod task_contract;
pub(crate) mod tool_blocks;

pub use generic::GenericSpecialistAgent;
