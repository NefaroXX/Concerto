//! Plugin-backed wire dialect (ADR-53).
//!
//! Wraps an [`ActivePlugin`] that exports `call_dialect` and exposes the two
//! dialect operations as string-based methods. A dialect plugin is a pure
//! string → string transformer: it receives the canonical request JSON (the
//! exact body [`crate::provider_host::PluginBackedProvider::build_request_json`]
//! produces today) and produces the plugin's own wire body. Transport and
//! response parsing stay in the Rust host.
//!
//! There is **no cross-crate trait dependency**: orchestration of a
//! plugin-provided dialect happens entirely inside this crate, and
//! [`crate::provider_host`] consumes the produced wire **string**.

use std::sync::Arc;

use concerto_core::CancellationToken;
use tokio::sync::Mutex;

use crate::active_plugin::ActivePlugin;
use crate::error::PluginError;

/// A plugin that implements provider wire serialization (ADR-53).
///
/// The hosting plugin must export `call_dialect` — the canonical 6-param ABI
/// with the ops `"render"` and `"cache"`.
pub struct DialectHost {
    plugin: Arc<Mutex<ActivePlugin>>,
}

impl DialectHost {
    /// Wrap an active plugin that declares `PluginProvides::Dialect`.
    pub fn new(plugin: Arc<Mutex<ActivePlugin>>) -> Self {
        Self { plugin }
    }

    /// Render the canonical request body into the dialect's wire format.
    ///
    /// `request_json` is exactly the canonical OpenAI-shaped body
    /// [`crate::provider_host::PluginBackedProvider::build_request_json`]
    /// produces; `model` and `echo` (the reasoning-echo policy, ADR-46) let the
    /// dialect place `reasoning_content`/thinking content correctly.
    pub async fn render_chat_body(
        &self,
        request_json: &serde_json::Value,
        model: &str,
        echo: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PluginError> {
        let input = serde_json::json!({
            "request": request_json,
            "model": model,
            "echo": echo,
        });
        let result = self.call_dialect("render", &input, cancel).await?;
        Self::wire_string_from_result(&result)
    }

    /// Apply the dialect's cache-control semantics (prompt-cache breakpoints,
    /// pooled-prefix markers) to a wire body. Dialects without caching return
    /// `body` unchanged.
    pub async fn apply_cache_breakpoints(
        &self,
        body: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PluginError> {
        let input = serde_json::json!({ "body": body });
        let result = self.call_dialect("cache", &input, cancel).await?;
        Self::wire_string_from_result(&result)
    }

    /// Run one dialect op, mapping the cancellation state.
    async fn call_dialect(
        &self,
        op: &str,
        input: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, PluginError> {
        if cancel.is_cancelled() {
            return Err(PluginError::PluginDialectFailed(format!("dialect op '{op}' cancelled")));
        }
        // Reject re-entrancy the same way the provider host does: if the plugin
        // mutex is held by an ancestor call we fail closed instead of hanging.
        let mut plugin = self.plugin.try_lock().map_err(|_| {
            PluginError::PluginDialectFailed("plugin mutex held; refusing re-entrancy".into())
        })?;
        if cancel.is_cancelled() {
            return Err(PluginError::PluginDialectFailed(format!("dialect op '{op}' cancelled")));
        }
        plugin.set_cancel(Some(cancel.clone()));
        plugin.call_dialect(op, input).await
    }

    /// The dialect ABI returns a JSON **string** (the wire body). A JSON object
    /// is an error shape (e.g. `{"error":"unsupported operation"}`).
    fn wire_string_from_result(result: &serde_json::Value) -> Result<String, PluginError> {
        if let Some(s) = result.as_str() {
            return Ok(s.to_string());
        }
        if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
            return Err(PluginError::PluginDialectFailed(err.to_string()));
        }
        Err(PluginError::PluginDialectFailed("dialect plugin returned a non-string result".into()))
    }
}

impl std::fmt::Debug for DialectHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialectHost").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_string_from_result_accepts_string() {
        // The dialect ABI returns a JSON string (the wire body); a `json!` str
        // literal produces the matching `Value::String` directly.
        let result = json!(r#"{"model":"m"}"#);
        let body = DialectHost::wire_string_from_result(&result).expect("string result");
        assert_eq!(body, r#"{"model":"m"}"#);
    }

    #[test]
    fn wire_string_from_result_surfaces_error_object() {
        let result = json!({"error": "unsupported operation"});
        let err = DialectHost::wire_string_from_result(&result).unwrap_err();
        assert!(
            matches!(&err, PluginError::PluginDialectFailed(msg) if msg == "unsupported operation"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn wire_string_from_result_rejects_non_string() {
        let result = json!({"model": "m"});
        let err = DialectHost::wire_string_from_result(&result).unwrap_err();
        assert!(
            matches!(&err, PluginError::PluginDialectFailed(msg) if msg.contains("non-string")),
            "unexpected error: {err}"
        );
    }
}
