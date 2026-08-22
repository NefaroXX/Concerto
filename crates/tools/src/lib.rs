#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-tools` — policy-gated tool implementations.
//!
//! Phase 2: filesystem, shell, and git tools plus the `ToolExecutor`
//! choke-point that enforces policy before every tool call.

pub mod common;
pub mod containment;
pub mod diff;
pub mod error;
pub mod filesystem;
pub mod git;
pub mod git_init;
pub mod process;
pub mod shell;
pub mod shell_backend;
pub mod virtual_fs;

#[cfg(test)]
pub mod testing;
pub mod undo;
