#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-plugins` — WASM plugin runtime (Phase 7).
//!
//! Provides the plugin host, loader, capability manager, and ABI types
//! for loading and running WASM plugins in Concerto.

pub mod active_plugin;
pub mod capability;
pub mod dialect_host;
pub mod discovery;
pub mod error;
pub mod guest_abi;
pub mod host;
pub mod host_fns;
pub mod loader;
pub mod manager;
pub mod memory_adapter_host;
pub mod provider_host;
pub mod tool_bridge;
