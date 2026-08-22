#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]

//! Example WASM memory adapter plugin for testing the Concerto plugin system.
//!
//! This plugin implements a minimal VectorStore in WASM that:
//! - Acknowledges all store/tombstone/delete operations
//! - Returns empty search/list results
//!
//! Compile with:
//! ```sh
//! cargo build --target wasm32-wasip2 -p test-adapter-plugin-wasm --release
//! ```

use std::format;
use std::string::String;

/// Static manifest JSON embedded in the plugin binary.
fn manifest() -> &'static [u8] {
    br#"{
  "id": "test-adapter-plugin",
  "name": "Test Adapter Plugin",
  "version": "0.1.0",
  "description": "Example memory adapter plugin for testing WASM adapter support",
  "abi_version": 1,
  "capabilities_required": [],
  "provides": [
    {
      "MemoryAdapter": {
        "name": "test-adapter",
        "kind": "wasm"
      }
    }
  ]
}"#
}

/// Dispatch a memory adapter operation and return a JSON result.
fn call_adapter(op: &str, _args: &str) -> String {
    match op {
        "store" => r#"{}"#.into(),
        "search" => r#"{"results":[]}"#.into(),
        "list" => r#"{"results":[]}"#.into(),
        "tombstone" => r#"{}"#.into(),
        "delete_tombstoned" => r#"{}"#.into(),
        "mark_stale" => r#"{}"#.into(),
        "delete_by_project" => r#"{}"#.into(),
        "delete_by_file_path" => r#"{"ids":[]}"#.into(),
        _ => format!(r#"{{"error":"unknown operation: {op}"}}"#),
    }
}

// Generate the WASM exports for a memory adapter plugin.
concerto_plugin_sdk::plugin_entry_adapter!(manifest, call_adapter);
