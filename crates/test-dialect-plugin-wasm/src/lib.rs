#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]

//! Example WASM dialect plugin for testing the Concerto plugin system
//! (ADR-53).
//!
//! A dialect plugin owns the request-side wire format of the provider it
//! backs. It is a pure string → string transformer:
//! - `render` — the host sends the canonical OpenAI-shaped request body
//!   (`{"request": <body>, "model": "...", "echo": "..."}`) and the plugin
//!   re-renders it into its own wire dialect. This plugin keeps the canonical
//!   request and marks the body with `"custom_dialect": true`.
//! - `cache` — applies the dialect's cache-control semantics to a wire body;
//!   this dialect has none, so it returns the body unchanged (a no-op).
//!
//! Transport and response parsing stay in the Rust host; the plugin never
//! makes host calls.
//!
//! Compile with:
//! ```sh
//! cargo build --target wasm32-wasip2 -p test-dialect-plugin-wasm --release
//! ```

use std::format;
use std::string::String;

/// Static manifest JSON embedded in the plugin binary.
fn manifest() -> &'static [u8] {
    br#"{
  "id": "test-dialect-plugin",
  "name": "Test Dialect Plugin",
  "version": "0.1.0",
  "description": "Example dialect plugin for testing WASM wire serialization",
  "abi_version": 1,
  "capabilities_required": [],
  "provides": [
    {
      "Provider": {
        "name": "dialect-provider",
        "model": "dialect-model"
      }
    },
    {
      "Dialect": {
        "name": "custom-wire"
      }
    }
  ]
}"#
}

/// Dispatch a dialect operation and return a JSON result (ADR-53 §1).
///
/// The two defined ops are `"render"` and `"cache"`; any other op must return
/// the `{"error":"unsupported operation"}` shape the host understands.
fn call_dialect(op: &str, args: &str) -> String {
    match op {
        "render" => render_body(args),
        "cache" => cache_body(args),
        _ => r#"{"error":"unsupported operation"}"#.into(),
    }
}

/// Re-render the canonical request body in this dialect's wire format.
///
/// Minimal dialect: keep the canonical request as-is and mark the body with
/// `"custom_dialect": true` so tests can verify the rendered wire reached the
/// completion call. A real dialect would reshape the body for vLLM / Cohere /
/// a custom endpoint and place `reasoning_content` per the `echo` policy.
fn render_body(args: &str) -> String {
    let envelope: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid render input: {e}"}}"#),
    };
    let mut body = envelope.get("request").cloned().unwrap_or_else(|| serde_json::json!({}));
    body["custom_dialect"] = serde_json::json!(true);
    serde_json::to_string(&body)
        .unwrap_or_else(|e| format!(r#"{{"error":"serialize wire body: {e}"}}"#))
}

/// Apply this dialect's cache-control semantics to a wire body.
///
/// This dialect has none, so the input body is returned unchanged (no-op).
fn cache_body(args: &str) -> String {
    let input: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid cache input: {e}"}}"#),
    };
    input.get("body").and_then(|b| b.as_str()).unwrap_or_default().to_string()
}

// Generate the WASM exports for a dialect plugin (ADR-53).
concerto_plugin_sdk::plugin_entry_dialect!(manifest, call_dialect);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_appends_custom_dialect_and_preserves_body() {
        let envelope = r#"{
            "request": {"model":"m","messages":[{"role":"user","content":"hi"}]},
            "model": "m",
            "echo": "if-present"
        }"#;
        let out = render_body(envelope);
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["custom_dialect"], serde_json::json!(true));
        assert_eq!(value["model"], "m");
        assert_eq!(value["messages"][0]["content"], "hi");
    }

    #[test]
    fn render_reports_invalid_input() {
        let out = render_body("not json");
        assert!(out.contains("invalid render input"), "unexpected: {out}");
    }

    #[test]
    fn cache_is_noop() {
        let body = r#"{"custom_dialect":true,"model":"m"}"#;
        let input = serde_json::json!({ "body": body }).to_string();
        let out = cache_body(&input);
        assert_eq!(out, body);
    }

    #[test]
    fn unknown_op_returns_unsupported_operation() {
        let out = call_dialect("bogus", "{}");
        assert!(out.contains(r#""error":"unsupported operation""#), "unexpected: {out}");
    }
}
