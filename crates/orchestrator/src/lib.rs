#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-orchestrator` — single-agent and dependency-aware multi-agent
//! execution.
//!
//! [`agent_loop::AgentLoop`] drives plan/act/observe cycles with recovery and
//! cancellation. [`coordinator::CoordinatorAgent`] plans and dispatches a DAG
//! of specialist tasks, relationships, review, validation, and partial-result
//! recovery. [`runtime_runner`] builds the shared provider/tool/memory/session
//! runtime used by frontends.

pub mod agent_loop;
pub mod agent_runner;
pub mod agents;
pub mod capsule;
pub mod checkpoint;
pub mod conflict;
pub mod consolidation;
mod context_compaction;
pub mod context_engine;
pub mod coordinator;
pub mod cost;
pub mod cycle;
pub mod cycle_manager;
pub mod delta;
pub mod exec_backend;
pub mod fingerprint;
pub mod gate;
pub mod gate_proxy;
pub mod graph;
pub mod hash;
pub mod hunk;
pub mod in_process_gate;
pub mod intent_classifier;
pub mod intent_grants;
pub mod ipc;
mod memory_prompt;
pub mod memory_serial;
pub mod plan_approval;
pub mod relationship;
pub use relationship::{
    AgentHandoff, AgentRelationship, CollaborationRule, HandoffDeliverable, RelationshipManager,
};
pub mod planner;
pub mod prompts;
pub mod registry;
pub mod resolver;
pub mod resolver_integration;
pub mod skills_context;
pub mod state;
pub mod subscriptions;
pub mod supervisor;
pub mod timeline;
mod tool_facts;
mod tool_guard;
mod working_memory;
pub mod workspace_snapshot;

#[path = "runtime_runner_persistent.rs"]
pub mod runtime_runner;
#[path = "runtime_runner.rs"]
mod runtime_runner_impl;
pub mod services;
pub mod session_manager;
pub mod testing;
