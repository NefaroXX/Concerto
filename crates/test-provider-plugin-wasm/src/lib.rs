#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]

//! Example WASM provider plugin for testing the Concerto plugin system.
//!
//! This plugin implements a minimal LLM provider that:
//! - Returns a hardcoded completion for "complete" operations
//! - Returns a single model info for "list_models" operations
//!
//! Compile with:
//! ```sh
//! cargo build --target wasm32-wasip2 -p test-provider-plugin-wasm --release
//! ```

use std::format;
use std::string::String;

/// Static manifest JSON embedded in the plugin binary.
fn manifest() -> &'static [u8] {
    br#"{
  "id": "test-provider-plugin",
  "name": "Test Provider Plugin",
  "version": "0.1.0",
  "description": "Example provider plugin for testing WASM provider support",
  "abi_version": 1,
  "capabilities_required": [],
  "provides": [
    {
      "Provider": {
        "name": "test-provider",
        "model": "test-model"
      }
    }
  ]
}"#
}

/// Dispatch a provider operation and return a JSON result.
fn call_provider(op: &str, args: &str) -> String {
    match op {
        "complete" => handle_complete(args),
        "list_models" => handle_list_models(),
        _ => format!(r#"{{"error":"unknown operation: {op}"}}"#),
    }
}

fn handle_complete(args: &str) -> String {
    let model = extract_string_field(args, "model").unwrap_or_else(|| "unknown".into());
    format!(
        r#"{{"content":"Hello from WASM provider plugin! (model: {model})","finish_reason":"stop"}}"#
    )
}

fn handle_list_models() -> String {
    r#"[{"id":"test-model","name":"Test Model","owned_by":"concerto"}]"#.into()
}

/// Crude JSON string-field extractor (no serde dependency needed for this
/// simple case). Returns the value of a string field by name.
fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let search = format!(r#""{field}":"#);
    let start = json.find(&search)?;
    let value_start = start + search.len();
    let remaining = &json[value_start..];
    let trimmed = remaining.trim_start();
    if let Some(inner) = trimmed.strip_prefix('"') {
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        let end = trimmed.find([',', '}', ']']).unwrap_or(trimmed.len());
        Some(trimmed[..end].trim().to_string())
    }
}

// Generate the WASM exports for a provider plugin.
concerto_plugin_sdk::plugin_entry_provider!(manifest, call_provider);
