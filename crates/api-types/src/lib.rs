#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-api-types` — shared API types for external consumers.
//!
//! Phase 2: DiffResult types for unified diff output.

pub mod api;
pub mod diff;
pub mod extension;
pub mod plugin;
