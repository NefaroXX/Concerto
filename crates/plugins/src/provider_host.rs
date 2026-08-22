//! Plugin-backed LLM provider.
//!
//! Wraps an [`ActivePlugin`] that exports `call_provider` and implements
//! [`concerto_core::traits::provider::LlmProvider`] by delegating all
//! operations to the WASM plugin via JSON messages.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use tokio::sync::{Mutex, MutexGuard};

use crate::active_plugin::ActivePlugin;
use crate::dialect_host::DialectHost;

/// A plugin that provides LLM completions by delegating to a WASM plugin.
pub struct PluginBackedProvider {
    plugin: Arc<Mutex<ActivePlugin>>,
    /// Provider name reported by `provider_name()`.
    provider_name: &'static str,
    /// Model name advertised by this plugin.
    model: String,
    /// Default context window (may be overridden by plugin).
    context_window: u64,
    /// Optional wire dialect (ADR-53): when `Some`, the request body is
    /// rendered by the dialect instead of the hardcoded OpenAI shape. `None`
    /// keeps today's exact code path (bit-for-bit current behavior).
    dialect: Option<Arc<DialectHost>>,
    /// Optional completion keepalive (ADR-53 §4): emit a non-terminal
    /// liveness chunk on this interval while awaiting the plugin call.
    heartbeat_interval: Option<Duration>,
    /// Reasoning-echo policy forwarded to the dialect (`"always"` |
    /// `"if-present"`, ADR-46). Defaults to `"if-present"`.
    reasoning_echo: &'static str,
}

impl PluginBackedProvider {
    /// Create a new plugin-backed provider.
    ///
    /// `plugin_id` is used to construct the provider name (`plugin:<plugin_id>`).
    /// `model` is the model identifier this plugin claims to serve.
    ///
    /// The provider uses the hardcoded OpenAI-shaped request body and no
    /// heartbeat — today's exact code path.
    pub fn new(plugin: Arc<Mutex<ActivePlugin>>, plugin_id: &str, model: String) -> Self {
        Self {
            plugin,
            provider_name: leak_provider_name(plugin_id),
            model,
            context_window: 8192,
            dialect: None,
            heartbeat_interval: None,
            reasoning_echo: "if-present",
        }
    }

    /// Create a plugin-backed provider that renders its request body through a
    /// wire dialect (ADR-53).
    ///
    /// `heartbeat_interval` enables the completion keepalive while the host
    /// awaits a slow plugin completion.
    pub fn with_dialect(
        plugin: Arc<Mutex<ActivePlugin>>,
        plugin_id: &str,
        model: String,
        dialect: Arc<DialectHost>,
        heartbeat_interval: Option<Duration>,
    ) -> Self {
        Self {
            plugin,
            provider_name: leak_provider_name(plugin_id),
            model,
            context_window: 8192,
            dialect: Some(dialect),
            heartbeat_interval,
            reasoning_echo: "if-present",
        }
    }

    /// Create a plugin-backed provider that emits a completion keepalive while
    /// a slow plugin completion is awaited (ADR-53 §4) but renders the request
    /// body with the hardcoded OpenAI shape.
    pub fn with_heartbeat(
        plugin: Arc<Mutex<ActivePlugin>>,
        plugin_id: &str,
        model: String,
        heartbeat_interval: Option<Duration>,
    ) -> Self {
        Self {
            plugin,
            provider_name: leak_provider_name(plugin_id),
            model,
            context_window: 8192,
            dialect: None,
            heartbeat_interval,
            reasoning_echo: "if-present",
        }
    }

    /// Build a completion request JSON from the standard request type.
    fn build_request_json(req: &CompletionRequest) -> serde_json::Value {
        serde_json::json!({
            "model": req.model,
            "messages": req.messages.iter().map(|m| serde_json::json!({
                "role": m.role,
                "content": m.content,
            })).collect::<Vec<_>>(),
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        })
    }

    /// Wait a single plugin completion through the shared plugin guard.
    async fn complete(
        &self,
        req_value: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<CompletionChunk, ProviderError> {
        let mut plugin = self.lock_plugin()?;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        // Thread the caller's cancellation token into the plugin store so
        // in-flight async host calls (e.g. `concerto.completion` invoked from
        // within `call_provider`) observe agent cancellation (ADR-38).
        plugin.set_cancel(Some(cancel.clone()));

        let result = plugin
            .call_provider("complete", req_value)
            .await
            .map_err(|e| ProviderError::Other(format!("plugin completion failed: {e}")))?;

        Ok(Self::chunk_from_result(&result))
    }

    /// Map a plugin `call_provider` result to a [`CompletionChunk`].
    fn chunk_from_result(result: &serde_json::Value) -> CompletionChunk {
        let content = result.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let is_final = result
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(|r| r == "stop" || r == "end_turn")
            .unwrap_or(true);

        CompletionChunk { delta: content, reasoning: None, tool_call: None, is_final, usage: None }
    }

    /// Emit the completion, interspersed with keepalive chunks on `interval`
    /// while the plugin call is awaited (ADR-53 §4).
    ///
    /// The plugin call runs on its own spawned task (which owns the plugin
    /// guard and threads the caller's token into the store); a second task
    /// emits a non-terminal chunk on the interval until the call finishes or
    /// the caller's token fires. The returned stream ends when the channel
    /// closes — i.e. after the plugin-call task has finished and any heartbeat
    /// task has stopped sending.
    fn heartbeat_stream(
        &self,
        req_value: serde_json::Value,
        interval: Duration,
        cancel: CancellationToken,
    ) -> CompletionStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<CompletionChunk, ProviderError>>(8);
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();

        // 1) The plugin call (guarded, cancellable); sends the final chunk.
        let call_plugin = self.plugin.clone();
        let call_cancel = cancel.clone();
        let call_tx = tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut plugin = call_plugin.try_lock().map_err(|_| ProviderError::Cancelled)?;
                if call_cancel.is_cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                plugin.set_cancel(Some(call_cancel.clone()));
                let result = plugin
                    .call_provider("complete", &req_value)
                    .await
                    .map_err(|e| ProviderError::Other(format!("plugin completion failed: {e}")))?;
                Ok(PluginBackedProvider::chunk_from_result(&result))
            }
            .await;
            let _ = call_tx.send(result).await;
            let _ = done_tx.send(());
        });

        // 2) Heartbeat task: keepalive chunk on `interval` until done/cancelled.
        let hb_cancel = cancel.clone();
        let hb_tx = tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // `interval` fires immediately on first tick; consume it so a
            // keepalive is not emitted at t=0 (no liveness question yet).
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !hb_cancel.is_cancelled()
                            && hb_tx.send(Ok(CompletionChunk::keepalive())).await.is_err()
                        {
                            break;
                        }
                    }
                    _ = &mut done_rx => break,
                }
            }
        });

        // The original sender must be dropped so the stream ends when both
        // spawned tasks finish. Both spawned tasks hold their own clones.
        drop(tx);

        // Receiver → stream, producing `Result<CompletionChunk, ProviderError>`.
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }

    /// Acquire the plugin guard, rejecting re-entrancy instead of deadlocking.
    ///
    /// M3 guard: if a plugin's own tool (or host function such as
    /// `concerto.completion`) reaches a plugin-backed provider that wraps the
    /// *same* plugin, the outer call already holds the plugin's mutex. A normal
    /// `lock().await` would then wait forever on the plugin's own mutex. We use
    /// a cheap, fail-closed `try_lock`: if the mutex is already held (by an
    /// ancestor call on the same singleton plugin) we return `Cancelled`
    /// immediately rather than hanging. A genuinely concurrent call to the same
    /// singleton plugin provider is also serialised by this mutex and would be
    /// rejected here too — that configuration (one plugin registered as both a
    /// tool and a provider) is pathological, and failing closed beats a hang.
    fn lock_plugin(&self) -> Result<MutexGuard<'_, ActivePlugin>, ProviderError> {
        self.plugin.try_lock().map_err(|_| ProviderError::Cancelled)
    }
}

/// Leak the plugin id string to satisfy `&'static str`.
///
/// This is acceptable because plugin metadata lives for the process lifetime.
fn leak_provider_name(plugin_id: &str) -> &'static str {
    Box::leak(format!("plugin:{plugin_id}").into_boxed_str())
}

#[async_trait]
impl LlmProvider for PluginBackedProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        // Request body handed to the plugin: the canonical OpenAI shape by
        // default, or the dialect's wire body when one is configured (ADR-53).
        let req_value = match &self.dialect {
            Some(dialect) => {
                let canonical = Self::build_request_json(&request);
                let wire = dialect
                    .render_chat_body(&canonical, &request.model, self.reasoning_echo, &cancel)
                    .await
                    .map_err(|e| ProviderError::Other(format!("dialect render failed: {e}")))?;
                let wire = dialect
                    .apply_cache_breakpoints(&wire, &cancel)
                    .await
                    .map_err(|e| ProviderError::Other(format!("dialect cache failed: {e}")))?;
                serde_json::from_str(&wire).map_err(|e| {
                    ProviderError::Other(format!("dialect output is not valid JSON: {e}"))
                })?
            }
            None => Self::build_request_json(&request),
        };

        // No heartbeat: single-chunk stream, exactly as before.
        let Some(heartbeat) = self.heartbeat_interval else {
            let chunk = self.complete(&req_value, &cancel).await?;
            let stream: CompletionStream =
                Box::pin(futures::stream::once(async move { Ok(chunk) }));
            return Ok(stream);
        };

        // Heartbeat: intersperse keepalive chunks while the plugin call is in
        // flight.
        Ok(self.heartbeat_stream(req_value, heartbeat, cancel))
    }

    fn context_capacity(&self, _model: &str) -> TokenBudget {
        TokenBudget::new(self.context_window, self.context_window / 4)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        // Plugin providers are "free" from the host's perspective
        0.0
    }

    fn provider_name(&self) -> &'static str {
        self.provider_name
    }

    async fn list_models(
        &self,
        cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let req_json = serde_json::json!({});
        let mut plugin = self.lock_plugin()?;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        // Thread the caller's cancellation token into the plugin store so
        // in-flight async host calls observe agent cancellation (ADR-38).
        plugin.set_cancel(Some(cancel.clone()));

        let result = plugin
            .call_provider("list_models", &req_json)
            .await
            .map_err(|e| ProviderError::Other(format!("plugin list_models failed: {e}")))?;

        let models: Vec<ModelInfo> = serde_json::from_value(result).unwrap_or_default();
        Ok(models)
    }
}

impl std::fmt::Debug for PluginBackedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBackedProvider")
            .field("provider_name", &self.provider_name)
            .field("model", &self.model)
            .field("has_dialect", &self.dialect.is_some())
            .field("heartbeat_interval", &self.heartbeat_interval)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{CompletionRequest, Message, Role};

    #[test]
    fn build_request_json_structure() {
        let req = CompletionRequest {
            model: "test-model".into(),
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            temperature: Some(0.7),
            max_tokens: Some(100),
            ..Default::default()
        };
        let json = PluginBackedProvider::build_request_json(&req);
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["messages"][0]["role"], "User");
        assert_eq!(json["messages"][0]["content"], "hello");
        // serde_json uses f64 for numbers; avoid floating-point precision comparisons
        assert!((json["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(json["max_tokens"], 100);
    }

    #[test]
    fn build_request_json_omits_optional_fields() {
        let req = CompletionRequest {
            model: "test".into(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        };
        let json = PluginBackedProvider::build_request_json(&req);
        assert_eq!(json["model"], "test");
        assert!(json["temperature"].is_null());
        assert!(json["max_tokens"].is_null());
    }

    #[test]
    fn chunk_from_result_maps_stop_final() {
        let result = serde_json::json!({
            "content": "hello",
            "finish_reason": "stop",
        });
        let chunk = PluginBackedProvider::chunk_from_result(&result);
        assert_eq!(chunk.delta, "hello");
        assert!(chunk.is_final);
        assert!(chunk.reasoning.is_none());
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn chunk_from_result_end_turn_final() {
        let result = serde_json::json!({
            "content": "bye",
            "finish_reason": "end_turn",
        });
        let chunk = PluginBackedProvider::chunk_from_result(&result);
        assert!(chunk.is_final);
    }

    #[test]
    fn chunk_from_result_length_not_final() {
        let result = serde_json::json!({
            "content": "truncated",
            "finish_reason": "length",
        });
        let chunk = PluginBackedProvider::chunk_from_result(&result);
        assert!(!chunk.is_final, "length finish must not mark the chunk final");
    }

    #[test]
    fn keepalive_chunk_is_non_terminal_empty_delta() {
        let chunk = CompletionChunk::keepalive();
        assert!(chunk.delta.is_empty());
        assert!(!chunk.is_final);
        assert!(chunk.usage.is_none());
    }
}
