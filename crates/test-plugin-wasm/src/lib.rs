#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]

//! Example WASM plugin for testing the Concerto plugin system.
//!
//! This plugin exports two tools:
//! - `greet` — returns a greeting string
//! - `echo` — returns the input unchanged
//! - `count` — returns the byte length of the input
//!
//! Compile with:
//! ```sh
//! cargo build --target wasm32-wasip2 -p test-plugin-wasm --release
//! ```

use std::format;
use std::string::String;
use std::string::ToString;

/// Static manifest JSON embedded in the plugin binary.
fn manifest() -> &'static [u8] {
    br#"{
  "id": "test-plugin",
  "name": "Test Plugin",
  "version": "0.1.0",
  "description": "Example plugin for testing the WASM plugin system",
  "abi_version": 1,
  "capabilities_required": [],
  "provides": [
    {
      "Tool": {
        "name": "greet",
        "description": "Return a greeting for the given name",
        "input_schema": {
          "type": "object",
          "properties": {
            "name": { "type": "string", "description": "Name to greet" }
          },
          "required": ["name"]
        }
      }
    },
    {
      "Tool": {
        "name": "echo",
        "description": "Echo the input back unchanged",
        "input_schema": {
          "type": "object",
          "properties": {
            "message": { "type": "string", "description": "Message to echo" }
          },
          "required": ["message"]
        }
      }
    },
    {
      "Tool": {
        "name": "count",
        "description": "Return the byte length of the input message",
        "input_schema": {
          "type": "object",
          "properties": {
            "message": { "type": "string", "description": "Message to measure" }
          },
          "required": ["message"]
        }
      }
    }
  ]
}"#
}

/// Dispatch a tool call and return a JSON result.
fn call_tool(name: &str, args: &str) -> String {
    match name {
        "greet" => handle_greet(args),
        "echo" => handle_echo(args),
        "count" => handle_count(args),
        _ => format!(r#"{{"error":"unknown tool: {name}"}}"#),
    }
}

fn handle_greet(args: &str) -> String {
    // Simple JSON parse — extract the "name" field.
    if let Some(name) = extract_string_field(args, "name") {
        format!(r#"{{"greeting":"Hello, {name}! Welcome from WASM plugin."}}"#)
    } else {
        r#"{"error":"missing 'name' field"}"#.into()
    }
}

fn handle_echo(args: &str) -> String {
    // Return the entire JSON input wrapped in a result.
    format!(r#"{{"echoed":{args}}}"#)
}

fn handle_count(args: &str) -> String {
    let len = args.len();
    format!(r#"{{"byte_length":{len}}}"#)
}

/// Crude JSON string-field extractor (no serde dependency needed for this
/// simple case).  Returns the value of a string field by name.
fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let search = format!(r#""{field}":"#);
    let start = json.find(&search)?;
    let value_start = start + search.len();
    // Skip whitespace.
    let remaining = &json[value_start..];
    let trimmed = remaining.trim_start();
    if let Some(inner) = trimmed.strip_prefix('"') {
        // Extract quoted string.
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        // Extract unquoted value (number, bool, etc.) — take until comma/brace.
        let end = trimmed.find([',', '}', ']']).unwrap_or(trimmed.len());
        Some(trimmed[..end].trim().to_string())
    }
}

// Generate the WASM exports.
concerto_plugin_sdk::plugin_entry!(manifest, call_tool);
