#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]
//! Typed, context-aware command runtime for Concerto.
//!
//! This crate intentionally does not implement a PTY or replace the existing
//! policy-gated agent shell tool. It provides the structured command layer that
//! both interactive and automated frontends can use.

mod builtins;
mod command;
mod context;
mod execution;
mod history;
mod model;
mod parser;
mod path;
mod profile;
mod registry;
mod runtime;

pub use command::{CommandEffect, CommandInvocation, CommandServices, CommandSpec, ShellCommand};
pub use context::ShellContext;
pub use execution::{ExternalExecutionRequest, PolicyExecutionAdapter};
pub use history::{HistoryError, ShellHistory};
pub use model::{
    Artifact, CommandProvenance, CommandResult, CommandSource, CommandStatus, Diagnostic,
    DiagnosticSeverity,
};
pub use parser::{parse_command_line, ParseError};
pub use profile::{profile_is_available, ShellProfileCatalog, ShellProfileError};
pub use registry::{CommandRegistry, RegistryError};
pub use runtime::{RuntimeBuildError, ShellRuntime};
