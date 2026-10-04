use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use wasmtime::{Caller, Linker};

use concerto_core::error::ProviderError;
use concerto_core::traits::policy::AuditLog;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::CompletionRequest;

use crate::capability::{check_path_allowed, check_shell_allowed, check_url_allowed};
use crate::error::PluginError;
use crate::guest_abi::{pack_ptr_len, RESULT_ERROR};
use crate::host::{PluginHost, PluginStoreData, ScratchBuffer};

/// `last_error` marker set when a `concerto.completion` host call is cancelled.
///
/// This is a *distinguishable* cancellation shape (vs. the generic
/// `"completion failed: …"` text) so the tool bridge can map a cancelled
/// in-flight host call to `ToolError::Cancelled` (M4) rather than a generic
/// execution failure. It is also how `host_shell_exec`-style cancellations
/// would be exposed if a provider ignored the token (M1).
pub const COMPLETION_CANCELLED: &str = "completion cancelled";

fn read_bytes(
    caller: &mut Caller<'_, PluginStoreData>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, PluginError> {
    let mem =
        caller.get_export("memory").and_then(|e| e.into_memory()).ok_or(PluginError::NoMemory)?;
    let data = mem.data(&*caller);
    // Reject negative pointers explicitly before the usize cast.
    if ptr < 0 {
        return Err(PluginError::MemoryViolation { ptr, len });
    }
    if len < 0 || len > caller.data().max_scratch_size {
        return Err(PluginError::MemoryViolation { ptr, len });
    }
    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or(PluginError::MemoryViolation { ptr, len })?;
    if end > data.len() {
        return Err(PluginError::MemoryViolation { ptr, len });
    }
    Ok(data[start..end].to_vec())
}

fn read_string(
    caller: &mut Caller<'_, PluginStoreData>,
    ptr: i32,
    len: i32,
) -> Result<String, PluginError> {
    let bytes = read_bytes(caller, ptr, len)?;
    String::from_utf8(bytes).map_err(|_| PluginError::InvalidUtf8)
}

fn write_to_scratch(
    caller: &mut Caller<'_, PluginStoreData>,
    data: &[u8],
) -> Result<i64, PluginError> {
    let scratch_ptr = caller.data().scratch.ptr;
    let scratch_len = caller.data().scratch.len;

    // Reject negative scratch buffer parameters.
    if scratch_ptr < 0 || scratch_len < 0 {
        caller.data_mut().last_error =
            Some(format!("negative scratch: ptr={scratch_ptr} len={scratch_len}"));
        return Ok(RESULT_ERROR);
    }
    let scratch_len = scratch_len as usize;

    if data.len() > scratch_len {
        caller.data_mut().last_error = Some(format!("scratch_overflow:{}", data.len()));
        return Ok(RESULT_ERROR);
    }
    let mem =
        caller.get_export("memory").and_then(|e| e.into_memory()).ok_or(PluginError::NoMemory)?;
    let start = scratch_ptr as usize;
    mem.write(caller, start, data).map_err(PluginError::MemoryWrite)?;
    Ok(pack_ptr_len(scratch_ptr, data.len() as i32))
}

fn into_anyhow(e: PluginError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Check that the plugin hasn't exceeded MAX_VIOLATIONS.
fn check_enabled(caller: &Caller<'_, PluginStoreData>) -> Result<(), PluginError> {
    if caller.data().disabled {
        return Err(PluginError::NotActive { id: caller.data().plugin_id.clone() });
    }
    Ok(())
}

/// Increment the violation count and disable the plugin if it hits the
/// MAX_VIOLATIONS threshold. Returns the UnauthorizedHostCall error.
///
/// Audits each violation (`CapabilityDenied`) and, when the threshold trips,
/// audits the resulting disable (`PluginDisabled`). Emission is fail-soft: the
/// audit sink is called from a detached task so a slow/broken sink can never
/// affect the host call's outcome.
fn handle_violation(caller: &mut Caller<'_, PluginStoreData>, capability: &str) -> anyhow::Error {
    caller.data_mut().violation_count += 1;
    let disabled_now = caller.data().violation_count >= PluginHost::MAX_VIOLATIONS;
    if disabled_now {
        caller.data_mut().disabled = true;
    }
    let plugin_id = caller.data().plugin_id.clone();
    if let Some(audit) = caller.data().audit_log.clone() {
        // A host call runs inside the agent runtime, so a current-thread
        // handle is expected; guard anyway so a host call outside a runtime
        // can never panic (fail-soft audit).
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let violation = concerto_core::traits::policy::InfraAuditEntry::plugin(
                plugin_id.clone(),
                concerto_core::traits::policy::InfraVerdict::CapabilityDenied,
                "unauthorized_host_call",
                format!("unauthorized host call to {capability}"),
            );
            let disable = disabled_now.then(|| {
                concerto_core::traits::policy::InfraAuditEntry::plugin(
                    plugin_id.clone(),
                    concerto_core::traits::policy::InfraVerdict::PluginDisabled,
                    "violation_threshold",
                    format!("disabled after unauthorized host call to {capability}"),
                )
            });
            rt.spawn(async move {
                if let Err(error) =
                    audit.record_infra(violation, concerto_core::CancellationToken::new()).await
                {
                    tracing::warn!(%error, "plugin violation audit write failed; continuing");
                }
                if let Some(disable) = disable {
                    if let Err(error) =
                        audit.record_infra(disable, concerto_core::CancellationToken::new()).await
                    {
                        tracing::warn!(%error, "plugin disable audit write failed; continuing");
                    }
                }
            });
        }
    }
    into_anyhow(PluginError::UnauthorizedHostCall {
        plugin_id: caller.data().plugin_id.clone(),
        capability: capability.to_string(),
    })
}

/// Emit a fail-soft `egress_denied` infra audit row for a rejected
/// plugin-initiated HTTP egress attempt (threat model §6 gap #7).
///
/// The row carries the full rejection detail, which names the rule that
/// refused the target (see `capability::RULE_EGRESS_ALLOWLIST` /
/// `capability::RULE_NETWORK_CAPABILITY`) plus the offending URL, so an
/// operator can reconstruct what the plugin tried to reach and why it was
/// refused. Emission is detached and best-effort: a slow or broken sink can
/// never affect the host call's outcome.
///
/// Initial and redirect refusals both carry sanitized URL facts. No userinfo,
/// query, or fragment is recorded.
fn emit_egress_audit(
    rt: Option<tokio::runtime::Handle>,
    audit: Option<Arc<dyn AuditLog>>,
    plugin_id: String,
    url: String,
    detail: String,
) {
    let (Some(rt), Some(audit)) = (rt, audit) else {
        tracing::debug!(
            target: "plugin",
            plugin_id,
            "no runtime or audit sink; plugin egress deny row skipped"
        );
        return;
    };
    let entry = concerto_core::traits::policy::InfraAuditEntry::plugin(
        plugin_id,
        concerto_core::traits::policy::InfraVerdict::CapabilityDenied,
        "egress_denied",
        format!(
            "network egress to {} denied: {detail}",
            concerto_core::types::PathPolicyFacts::for_url("get", &url)
                .attempted_path
                .unwrap_or_default()
        ),
    );
    rt.spawn(async move {
        if let Err(error) = audit.record_infra(entry, concerto_core::CancellationToken::new()).await
        {
            tracing::warn!(%error, "plugin egress audit write failed; continuing");
        }
    });
}

/// Check whether a plugin may emit events: requires at least one granted
/// capability (any discriminant). A plugin with zero grants is completely
/// unapproved and should not be able to push events into the bus.
fn check_event_allowed(
    caps: &crate::capability::GrantedCapabilities,
    _plugin_id: &str,
) -> Result<(), PluginError> {
    let has_any =
        !caps.session_grants.is_empty() || caps.persistent_grants.values().any(|m| !m.is_empty());
    if !has_any {
        return Err(PluginError::CapabilityDenied("EventEmit".into()));
    }
    Ok(())
}

// ── Individual host function implementations ────────────────────────

/// All host functions are async (ADR-38). They are registered via
/// `Linker::func_wrap_async` and may await host services directly, observing
/// the caller's cancellation token where one was threaded into the store.
async fn host_log(
    mut caller: Caller<'_, PluginStoreData>,
    level_ptr: i32,
    level_len: i32,
    msg_ptr: i32,
    msg_len: i32,
) -> anyhow::Result<()> {
    let level = read_string(&mut caller, level_ptr, level_len).map_err(into_anyhow)?;
    let msg = read_string(&mut caller, msg_ptr, msg_len).map_err(into_anyhow)?;
    tracing::info!(
        target: "plugin",
        plugin_id = %caller.data().plugin_id,
        level,
        "{msg}"
    );
    Ok(())
}

async fn host_last_error(
    mut caller: Caller<'_, PluginStoreData>,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    let err = caller.data_mut().last_error.take().unwrap_or_default();
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
    let result = write_to_scratch(&mut caller, err.as_bytes()).map_err(into_anyhow)?;
    Ok(result)
}

async fn host_resize_scratch(
    mut caller: Caller<'_, PluginStoreData>,
    new_size: i32,
) -> anyhow::Result<i32> {
    let max_scratch = caller.data().max_scratch_size;
    if new_size <= 0 || new_size > max_scratch || new_size as usize > 256 * 1024 * 1024 {
        return Ok(-1);
    }
    caller.data_mut().scratch_resize_count += 1;
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow::anyhow!("no memory"))?;
    let old_pages = mem.size(&caller);
    let needed_pages = (new_size as usize).div_ceil(0x10000) as u64;
    if needed_pages > old_pages {
        mem.grow(&mut caller, needed_pages - old_pages)
            .map_err(|_| anyhow::anyhow!("memory grow failed"))?;
    }
    Ok(0)
}

async fn execute_host_tool(
    caller: &Caller<'_, PluginStoreData>,
    name: &str,
    input: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let context = caller
        .data()
        .execution
        .read()
        .map_err(|_| anyhow::anyhow!("plugin execution context unavailable"))?
        .clone()
        .ok_or_else(|| anyhow::anyhow!("plugin host effects require an active agent run"))?;
    let executor =
        context.executor.upgrade().ok_or_else(|| anyhow::anyhow!("plugin agent run has ended"))?;
    let cancel = caller.data().cancel.clone().unwrap_or_default();
    let result = executor.execute(name, input, &context.session, cancel).await?;
    Ok(result.data)
}

async fn host_read_file(
    mut caller: Caller<'_, PluginStoreData>,
    path_ptr: i32,
    path_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let path_str = read_string(&mut caller, path_ptr, path_len).map_err(into_anyhow)?;
    let resolved = match check_path_allowed(
        &caller.data().granted_caps,
        &caller.data().plugin_id,
        &path_str,
        false, // read
    ) {
        Ok(resolved) => resolved,
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "FilesystemRead"));
        }
        Err(e) => return Err(into_anyhow(e)),
    };
    // Forward exactly the path that passed confinement, never the raw caller
    // string: validating a canonicalized path and then executing the original
    // reopens a validate-then-use race (W7-2).
    let resolved = resolved.to_str().ok_or_else(|| {
        into_anyhow(PluginError::CapabilityDenied("resolved path is not valid UTF-8".into()))
    })?;
    let output = execute_host_tool(&caller, "filesystem", serde_json::json!({"operation":"read", "path":resolved, "max_bytes":caller.data().max_scratch_size})).await;
    let content = match output {
        Ok(output) => match output.get("content").and_then(serde_json::Value::as_str) {
            Some(content) => content.to_owned(),
            // A tool result without `content` is not a legitimately empty file:
            // fail closed so the guest never mistakes a missing result for a
            // successful empty read (W7-3).
            None => {
                caller.data_mut().last_error = Some("filesystem read returned no content".into());
                return Ok(RESULT_ERROR);
            }
        },
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(RESULT_ERROR);
        }
    };
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
    let result = write_to_scratch(&mut caller, content.as_bytes()).map_err(into_anyhow)?;
    Ok(result)
}

async fn host_write_file(
    mut caller: Caller<'_, PluginStoreData>,
    path_ptr: i32,
    path_len: i32,
    content_ptr: i32,
    content_len: i32,
) -> anyhow::Result<i32> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let path_str = read_string(&mut caller, path_ptr, path_len).map_err(into_anyhow)?;
    let resolved = match check_path_allowed(
        &caller.data().granted_caps,
        &caller.data().plugin_id,
        &path_str,
        true, // write
    ) {
        Ok(resolved) => resolved,
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "FilesystemWrite"));
        }
        Err(e) => return Err(into_anyhow(e)),
    };
    // Forward the confined path (canonical parent + file name for a new file),
    // never the raw caller string (W7-2).
    let resolved = resolved.to_str().ok_or_else(|| {
        into_anyhow(PluginError::CapabilityDenied("resolved path is not valid UTF-8".into()))
    })?;
    let content = read_bytes(&mut caller, content_ptr, content_len).map_err(into_anyhow)?;
    let content = String::from_utf8(content).map_err(|_| into_anyhow(PluginError::InvalidUtf8))?;
    match execute_host_tool(
        &caller,
        "filesystem",
        serde_json::json!({"operation":"write", "path":resolved, "content":content}),
    )
    .await
    {
        Ok(_) => Ok(0),
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            Ok(-1)
        }
    }
}

/// One HTTP hop; the executor mediates each concrete URL, including redirects.
struct HttpHostOperation {
    max_bytes: usize,
}
#[async_trait::async_trait]
impl concerto_core::traits::tool::Tool for HttpHostOperation {
    fn name(&self) -> &str {
        "http"
    }
    fn description(&self) -> &str {
        "Plugin HTTP GET through shared policy"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn capability_requirements(&self) -> concerto_core::types::CapabilitySet {
        Default::default()
    }
    fn path_facts(
        &self,
        input: &serde_json::Value,
        _: &concerto_core::types::SessionContext,
    ) -> Option<concerto_core::types::PathPolicyFacts> {
        input
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(|url| concerto_core::types::PathPolicyFacts::for_url("get", url))
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _: &dyn concerto_core::traits::PolicyEngine,
        _: &concerto_core::types::SessionContext,
        cancel: concerto_core::CancellationToken,
    ) -> Result<concerto_core::types::ToolOutput, concerto_core::error::ToolError> {
        use concerto_core::error::ToolError;
        let fetch = async {
            let url = input
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing URL"))?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?;
            let mut response = client.get(url).send().await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|h| h.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("redirect missing Location"))?;
                let next = response.url().join(location)?;
                return Ok::<_, anyhow::Error>(concerto_core::types::ToolOutput {
                    summary: "HTTP redirect".into(),
                    data: serde_json::json!({"redirect":next.as_str()}),
                });
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if chunk.len() > self.max_bytes.saturating_sub(body.len()) {
                    return Err(anyhow::anyhow!("HTTP body exceeds plugin output limit"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(concerto_core::types::ToolOutput {
                summary: format!("HTTP {} bytes", body.len()),
                data: serde_json::json!({"body":body}),
            })
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            result = fetch => result.map_err(|_| ToolError::ExecutionFailed { message: "Plugin HTTP request failed".into() }),
        }
    }
}
async fn host_http_get(
    mut caller: Caller<'_, PluginStoreData>,
    url_ptr: i32,
    url_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let mut url = read_string(&mut caller, url_ptr, url_len).map_err(into_anyhow)?;
    if let Err(error) =
        check_url_allowed(&caller.data().granted_caps, &caller.data().plugin_id, &url)
    {
        if let PluginError::CapabilityDenied(detail) = error {
            emit_egress_audit(
                tokio::runtime::Handle::try_current().ok(),
                caller.data().audit_log.clone(),
                caller.data().plugin_id.clone(),
                url,
                detail,
            );
            return Err(handle_violation(&mut caller, "NetworkOutbound"));
        }
        return Err(into_anyhow(error));
    }
    let context = caller
        .data()
        .execution
        .read()
        .map_err(|_| anyhow::anyhow!("plugin execution context unavailable"))?
        .clone()
        .ok_or_else(|| anyhow::anyhow!("HTTP requires an active agent run"))?;
    let executor = context.executor.upgrade().ok_or_else(|| anyhow::anyhow!("agent run ended"))?;
    let cancel = caller.data().cancel.clone().unwrap_or_default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for _ in 0..=10 {
        match check_url_allowed(&caller.data().granted_caps, &caller.data().plugin_id, &url) {
            Ok(()) => {}
            Err(PluginError::CapabilityDenied(detail)) => {
                emit_egress_audit(
                    tokio::runtime::Handle::try_current().ok(),
                    caller.data().audit_log.clone(),
                    caller.data().plugin_id.clone(),
                    url,
                    detail,
                );
                return Err(handle_violation(&mut caller, "NetworkOutbound"));
            }
            Err(error) => return Err(into_anyhow(error)),
        }
        let operation =
            HttpHostOperation { max_bytes: caller.data().max_scratch_size.max(0) as usize };
        let result = tokio::time::timeout_at(
            deadline,
            executor.execute_host_operation(
                &operation,
                serde_json::json!({"url":url}),
                &context.session,
                cancel.clone(),
            ),
        )
        .await;
        let output = match result {
            Ok(Ok(output)) => output,
            _ => {
                caller.data_mut().last_error =
                    Some("HTTP denied, cancelled, timed out, or exceeded its output limit".into());
                return Ok(RESULT_ERROR);
            }
        };
        if let Some(next) = output.data.get("redirect").and_then(serde_json::Value::as_str) {
            url = next.to_owned();
            continue;
        }
        let body: Vec<u8> =
            serde_json::from_value(output.data.get("body").cloned().unwrap_or_default())
                .map_err(|_| anyhow::anyhow!("invalid HTTP body"))?;
        caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
        return write_to_scratch(&mut caller, &body).map_err(into_anyhow);
    }
    caller.data_mut().last_error = Some("too many HTTP redirects".into());
    Ok(RESULT_ERROR)
}

async fn host_shell_exec(
    mut caller: Caller<'_, PluginStoreData>,
    cmd_ptr: i32,
    cmd_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let cmd = read_string(&mut caller, cmd_ptr, cmd_len).map_err(into_anyhow)?;
    match check_shell_allowed(&caller.data().granted_caps, &caller.data().plugin_id, &cmd) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "ShellExecute"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }

    let output = match execute_host_tool(
        &caller,
        "shell",
        serde_json::json!({"command":cmd, "timeout_secs":30}),
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            caller.data_mut().last_error = Some(error.to_string());
            return Ok(RESULT_ERROR);
        }
    };
    let stdout = output.get("stdout").and_then(serde_json::Value::as_str).unwrap_or_default();
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
    write_to_scratch(&mut caller, stdout.as_bytes()).map_err(into_anyhow)
}

async fn host_emit_event(
    mut caller: Caller<'_, PluginStoreData>,
    event_ptr: i32,
    event_len: i32,
) -> anyhow::Result<()> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let json = read_string(&mut caller, event_ptr, event_len).map_err(into_anyhow)?;
    match check_event_allowed(&caller.data().granted_caps, &caller.data().plugin_id) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "EventEmit"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }
    if let Some(tx) = &caller.data().event_bus {
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&json) {
            let _ = tx.send(Arc::new(event));
        }
    }
    Ok(())
}

/// Guest-facing request JSON (no serde derives on core `CompletionRequest`).
#[derive(Debug, Clone, serde::Deserialize)]
struct PluginCompletionRequest {
    model: String,
    messages: Vec<concerto_core::types::Message>,
    temperature: Option<f32>,
    max_tokens: Option<u64>,
}

/// Response JSON written back to the scratch buffer.
#[derive(Debug, Clone, serde::Serialize)]
struct PluginCompletionResponse {
    content: String,
}

async fn host_completion(
    mut caller: Caller<'_, PluginStoreData>,
    req_ptr: i32,
    req_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };

    // 1. Read and parse the request from guest memory.
    let json = match read_string(&mut caller, req_ptr, req_len) {
        Ok(s) => s,
        Err(e) => {
            caller.data_mut().last_error = Some(format!("failed to read request: {e}"));
            return Ok(RESULT_ERROR);
        }
    };

    let guest_req: PluginCompletionRequest = match serde_json::from_str(&json) {
        Ok(r) => r,
        Err(e) => {
            caller.data_mut().last_error = Some(format!("invalid completion request JSON: {e}"));
            return Ok(RESULT_ERROR);
        }
    };

    // 2. Map to the core CompletionRequest.
    let request = CompletionRequest {
        model: guest_req.model,
        messages: guest_req.messages,
        tools: None,
        tool_choice: None,
        temperature: guest_req.temperature,
        max_tokens: guest_req.max_tokens,
        stream: true,
    };

    // 3. Get the provider.
    let provider: Arc<dyn LlmProvider> = match caller.data().provider.clone() {
        Some(p) => p,
        None => {
            caller.data_mut().last_error = Some("no LLM provider configured for plugins".into());
            return Ok(RESULT_ERROR);
        }
    };

    // 4. Drive the async provider call directly on the plugin host's async
    //    runtime (ADR-38) — no per-call `Runtime::new()`/`block_on`. Observe
    //    the caller's cancellation token when one was threaded into the store
    //    (via `ActivePlugin::set_cancel`); otherwise fall back to a fresh
    //    token so cancellation is still possible locally.
    // `CancellationToken::default()` == `CancellationToken::new()` (tokio_util),
    // so a missing store token still yields a live, never-cancelled token.
    let cancel = caller.data().cancel.clone().unwrap_or_default();
    check_enabled(&caller).map_err(into_anyhow)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let start = tokio::select! {
        _ = cancel.cancelled() => Err(ProviderError::Cancelled),
        result = tokio::time::timeout_at(deadline, provider.stream_completion(request, cancel.clone())) => result.unwrap_or_else(|_| Err(ProviderError::Cancelled)),
    };
    let mut stream = match start {
        Ok(s) => s,
        Err(ProviderError::Cancelled) => {
            caller.data_mut().last_error = Some(COMPLETION_CANCELLED.into());
            return Ok(RESULT_ERROR);
        }
        Err(e) => {
            caller.data_mut().last_error = Some(format!("completion failed: {e}"));
            return Ok(RESULT_ERROR);
        }
    };
    let mut content = String::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => { caller.data_mut().last_error = Some(COMPLETION_CANCELLED.into()); return Ok(RESULT_ERROR); },
            next = tokio::time::timeout_at(deadline, stream.next()) => match next {
                Ok(next) => next,
                Err(_) => { caller.data_mut().last_error = Some("completion timed out".into()); return Ok(RESULT_ERROR); }
            }
        };
        let Some(chunk) = next else { break };
        // M1: observe cancellation on every iteration so a provider that
        // ignores the token cannot let a cancelled host call run on.
        if cancel.is_cancelled() {
            caller.data_mut().last_error = Some(COMPLETION_CANCELLED.into());
            return Ok(RESULT_ERROR);
        }
        let chunk = match chunk {
            Ok(c) => c,
            Err(ProviderError::Cancelled) => {
                caller.data_mut().last_error = Some(COMPLETION_CANCELLED.into());
                return Ok(RESULT_ERROR);
            }
            Err(e) => {
                caller.data_mut().last_error = Some(format!("completion failed: {e}"));
                return Ok(RESULT_ERROR);
            }
        };
        if chunk.delta.len()
            > (caller.data().max_scratch_size.max(0) as usize).saturating_sub(content.len())
        {
            caller.data_mut().last_error = Some("completion exceeds plugin output limit".into());
            return Ok(RESULT_ERROR);
        }
        content.push_str(&chunk.delta);
    }

    // 5. Serialize response and write to scratch.
    let response = PluginCompletionResponse { content };
    let response_json = match serde_json::to_vec(&response) {
        Ok(r) => r,
        Err(e) => {
            caller.data_mut().last_error = Some(format!("response serialization failed: {e}"));
            return Ok(RESULT_ERROR);
        }
    };

    write_to_scratch(&mut caller, &response_json).map_err(|e| {
        caller.data_mut().last_error = Some(format!("scratch write failed: {e}"));
        anyhow::anyhow!("{e}")
    })
}

/// Register all 9 host functions on the provided linker.
///
/// All functions are registered as async (ADR-38) — the engine is built with
/// `async_support(true)`, so a sync `func_wrap` cannot be invoked from the
/// async stores created by the plugin host.
pub fn register_host_functions(linker: &mut Linker<PluginStoreData>) -> Result<(), PluginError> {
    // Async (ADR-38): `func_wrap_async` passes the wasm params as a single
    // `WasmTyList` tuple, so destructure them before forwarding to the host
    // function. Each closure returns a pinned boxed future that awaits the
    // host service directly on the plugin host's async runtime.
    linker.func_wrap_async(
        "concerto",
        "log",
        |caller, (level_ptr, level_len, msg_ptr, msg_len): (i32, i32, i32, i32)| {
            Box::new(host_log(caller, level_ptr, level_len, msg_ptr, msg_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "last_error",
        |caller, (scratch_ptr, scratch_len): (i32, i32)| {
            Box::new(host_last_error(caller, scratch_ptr, scratch_len))
        },
    )?;
    linker.func_wrap_async("concerto", "resize_scratch", |caller, (new_size,): (i32,)| {
        Box::new(host_resize_scratch(caller, new_size))
    })?;
    linker.func_wrap_async(
        "concerto",
        "read_file",
        |caller, (path_ptr, path_len, scratch_ptr, scratch_len): (i32, i32, i32, i32)| {
            Box::new(host_read_file(caller, path_ptr, path_len, scratch_ptr, scratch_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "write_file",
        |caller, (path_ptr, path_len, content_ptr, content_len): (i32, i32, i32, i32)| {
            Box::new(host_write_file(caller, path_ptr, path_len, content_ptr, content_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "http_get",
        |caller, (url_ptr, url_len, scratch_ptr, scratch_len): (i32, i32, i32, i32)| {
            Box::new(host_http_get(caller, url_ptr, url_len, scratch_ptr, scratch_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "shell_exec",
        |caller, (cmd_ptr, cmd_len, scratch_ptr, scratch_len): (i32, i32, i32, i32)| {
            Box::new(host_shell_exec(caller, cmd_ptr, cmd_len, scratch_ptr, scratch_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "emit_event",
        |caller, (event_ptr, event_len): (i32, i32)| {
            Box::new(host_emit_event(caller, event_ptr, event_len))
        },
    )?;
    linker.func_wrap_async(
        "concerto",
        "completion",
        |caller, (req_ptr, req_len, scratch_ptr, scratch_len): (i32, i32, i32, i32)| {
            Box::new(host_completion(caller, req_ptr, req_len, scratch_ptr, scratch_len))
        },
    )?;
    Ok(())
}

pub fn register_minimal_host_functions(
    linker: &mut Linker<PluginStoreData>,
) -> Result<(), PluginError> {
    linker.func_wrap_async(
        "concerto",
        "log",
        |caller, (level_ptr, level_len, msg_ptr, msg_len): (i32, i32, i32, i32)| {
            Box::new(host_log(caller, level_ptr, level_len, msg_ptr, msg_len))
        },
    )?;

    linker.func_wrap_async(
        "concerto",
        "last_error",
        |caller, (scratch_ptr, scratch_len): (i32, i32)| {
            Box::new(host_last_error(caller, scratch_ptr, scratch_len))
        },
    )?;

    // Minimal-stub signatures must match the real host function signatures
    // so that the linker does not mismatch types when the plugin calls them.
    linker
        .func_wrap_async(
            "concerto",
            "read_file",
            |_: Caller<'_, PluginStoreData>,
             (_path_ptr, _path_len, _scratch_ptr, _scratch_len): (i32, i32, i32, i32)| {
                Box::new(async move { Ok::<i64, anyhow::Error>(0) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "write_file",
            |_: Caller<'_, PluginStoreData>,
             (_path_ptr, _path_len, _content_ptr, _content_len): (i32, i32, i32, i32)| {
                Box::new(async move { Ok::<i32, anyhow::Error>(0) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "http_get",
            |_: Caller<'_, PluginStoreData>,
             (_url_ptr, _url_len, _scratch_ptr, _scratch_len): (i32, i32, i32, i32)| {
                Box::new(async move { Ok::<i64, anyhow::Error>(0) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "shell_exec",
            |_: Caller<'_, PluginStoreData>,
             (_cmd_ptr, _cmd_len, _scratch_ptr, _scratch_len): (i32, i32, i32, i32)| {
                Box::new(async move { Ok::<i64, anyhow::Error>(0) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "emit_event",
            |_: Caller<'_, PluginStoreData>, (_event_ptr, _event_len): (i32, i32)| {
                Box::new(async move { Ok::<(), anyhow::Error>(()) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "completion",
            |_: Caller<'_, PluginStoreData>,
             (_req_ptr, _req_len, _scratch_ptr, _scratch_len): (i32, i32, i32, i32)| {
                Box::new(async move { Ok::<i64, anyhow::Error>(0) })
            },
        )
        .ok();
    linker
        .func_wrap_async(
            "concerto",
            "resize_scratch",
            |_: Caller<'_, PluginStoreData>, (_new_size,): (i32,)| {
                Box::new(async move { Ok::<i32, anyhow::Error>(0) })
            },
        )
        .ok();

    Ok(())
}
