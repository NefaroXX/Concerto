//! The five foundational trait contracts (per the roadmap's Phase 0 "Core
//! traits" section). Every implementation in later phases builds on these
//! — get the shape right here before writing anything that depends on it.

pub mod agent;
pub mod approval;
pub mod context_overflow;
pub mod memory;
pub mod policy;
pub mod provider;
pub mod tool;
pub mod vector_store;

pub use agent::ExpertAgent;
pub use approval::{ApprovalDecision, ApprovalSink};
pub use context_overflow::{ContextOverflowStrategy, NoOpOverflowStrategy, TruncateOldest};
pub use memory::MemoryStore;
pub use policy::{AuditLog, PolicyEngine};
pub use provider::{CompletionStream, LlmProvider};
pub use tool::Tool;
pub use vector_store::VectorStore;
