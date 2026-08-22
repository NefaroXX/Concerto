use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use wasmtime::{Caller, Linker};

use concerto_core::error::ProviderError;
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
    if len < 0 {
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
fn handle_violation(caller: &mut Caller<'_, PluginStoreData>, capability: &str) -> anyhow::Error {
    caller.data_mut().violation_count += 1;
    if caller.data().violation_count >= PluginHost::MAX_VIOLATIONS {
        caller.data_mut().disabled = true;
    }
    into_anyhow(PluginError::UnauthorizedHostCall {
        plugin_id: caller.data().plugin_id.clone(),
        capability: capability.to_string(),
    })
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

fn shell_command(command: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut process = std::process::Command::new(shell);
        process.arg("/D").arg("/S").arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;

        let mut process = std::process::Command::new("sh");
        process.arg("-c").arg(command);
        // Isolate the plugin command in its own process group so timeout
        // cleanup can terminate descendants as well as the shell process.
        process.process_group(0);
        process
    }
}

fn terminate_process_tree(child_id: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child_id.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;

        // Negative pid targets the whole process group (the child is its
        // own group leader via `process_group(0)`), killing descendants
        // as well as the shell itself. A direct syscall — no subprocess
        // spawn, which could itself stall under load.
        let _ = kill(Pid::from_raw(-(child_id as i32)), Signal::SIGKILL);
    }
}

fn execute_shell_with_timeout(
    command: &str,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::{Read, Seek};
    use std::process::Stdio;
    use std::sync::mpsc;

    let mut stdout_file = tempfile::tempfile()?;
    let mut stderr_file = tempfile::tempfile()?;
    let mut child = shell_command(command)
        .stdout(Stdio::from(stdout_file.try_clone()?))
        .stderr(Stdio::from(stderr_file.try_clone()?))
        .spawn()?;
    let child_id = child.id();

    // Block on a kernel-timer deadline (`recv_timeout`) instead of
    // polling `try_wait` with short sleeps. A polling loop is fragile on
    // a loaded machine: every scheduling stall stretches each iteration,
    // so the deadline can be missed by seconds (observed in CI: 3.5s
    // elapsed for a 100ms timeout). `recv_timeout` wakes exactly at the
    // deadline and is reaped by the waiter thread.
    let (done_tx, done_rx) = mpsc::channel::<std::io::Result<std::process::ExitStatus>>();
    let waiter = std::thread::spawn(move || {
        let status = child.wait();
        let _ = done_tx.send(status);
    });

    let status = match done_rx.recv_timeout(timeout) {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => return Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            terminate_process_tree(child_id);
            // The waiter thread reaps the child once it is dead.
            let _ = waiter.join();
            return Ok(None);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(std::io::Error::other("shell waiter thread exited without a status"));
        }
    };
    let _ = waiter.join();

    stdout_file.rewind()?;
    stderr_file.rewind()?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    stdout_file.read_to_end(&mut stdout)?;
    stderr_file.read_to_end(&mut stderr)?;
    Ok(Some(std::process::Output { status, stdout, stderr }))
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

async fn host_read_file(
    mut caller: Caller<'_, PluginStoreData>,
    path_ptr: i32,
    path_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    check_enabled(&caller).map_err(into_anyhow)?;
    let path_str = read_string(&mut caller, path_ptr, path_len).map_err(into_anyhow)?;
    match check_path_allowed(
        &caller.data().granted_caps,
        &caller.data().plugin_id,
        &path_str,
        false, // read
    ) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "FilesystemRead"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }
    // std::fs is blocking; offload to a blocking thread so a slow filesystem
    // cannot stall the wasmtime async executor (ADR-38 Decision 5).
    // The closure must not touch `Caller`; only the collected `path_str` is
    // moved in and the content comes back out.
    let content =
        match tokio::task::spawn_blocking(move || std::fs::read_to_string(&path_str)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                caller.data_mut().last_error = Some(format!("read_file failed: {e}"));
                return Ok(RESULT_ERROR);
            }
            Err(e) => {
                caller.data_mut().last_error = Some(format!("read_file offload failed: {e}"));
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
    match check_path_allowed(
        &caller.data().granted_caps,
        &caller.data().plugin_id,
        &path_str,
        true, // write
    ) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "FilesystemWrite"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }
    let content = read_bytes(&mut caller, content_ptr, content_len).map_err(into_anyhow)?;
    // std::fs::write is blocking (ADR-38 Decision 5). Move the path and bytes
    // into the offloaded closure; the `Caller` must not be touched inside.
    match tokio::task::spawn_blocking(move || std::fs::write(&path_str, content)).await {
        Ok(Ok(_)) => Ok(0),
        Ok(Err(e)) => {
            caller.data_mut().last_error = Some(format!("write_file failed: {e}"));
            Ok(-1)
        }
        Err(e) => {
            caller.data_mut().last_error = Some(format!("write_file offload failed: {e}"));
            Ok(-1)
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
    use std::io::Read;
    const HTTP_BODY_CAP: usize = 10 * 1024 * 1024;

    check_enabled(&caller).map_err(into_anyhow)?;
    let url = read_string(&mut caller, url_ptr, url_len).map_err(into_anyhow)?;
    match check_url_allowed(&caller.data().granted_caps, &caller.data().plugin_id, &url) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "NetworkOutbound"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }
    let granted_caps = caller.data().granted_caps.clone();
    let plugin_id = caller.data().plugin_id.clone();
    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        if check_url_allowed(&granted_caps, &plugin_id, attempt.url().as_str()).is_ok() {
            attempt.follow()
        } else {
            attempt.error("redirect target is outside the plugin's granted domains")
        }
    });

    // `reqwest::blocking` is a blocking HTTP client; offload the whole fetch
    // (client build + send + capped body read) to a blocking thread so it can
    // never stall the wasmtime async executor (ADR-38 Decision 5). Everything
    // the closure needs (`url`, redirect policy) is collected before offload;
    // the `Caller` is not touched inside the closure.
    let body_result = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::builder()
            .redirect(redirect_policy)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client.get(&url).send().map_err(|e| e.to_string())?;
        let mut body = Vec::with_capacity(HTTP_BODY_CAP.min(4096));
        let mut buf = [0u8; 8192];
        loop {
            let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let remaining = HTTP_BODY_CAP.saturating_sub(body.len());
            if remaining == 0 {
                break;
            }
            let take = n.min(remaining);
            body.extend_from_slice(&buf[..take]);
        }
        Ok::<Vec<u8>, String>(body)
    })
    .await;

    let body = match body_result {
        Ok(Ok(body)) => body,
        Ok(Err(e)) => {
            caller.data_mut().last_error = Some(format!("http_get failed: {e}"));
            return Err(into_anyhow(PluginError::Http(e)));
        }
        Err(e) => {
            caller.data_mut().last_error = Some(format!("http_get offload failed: {e}"));
            return Err(into_anyhow(PluginError::Http(e.to_string())));
        }
    };
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
    let result = write_to_scratch(&mut caller, &body).map_err(into_anyhow)?;
    Ok(result)
}

async fn host_shell_exec(
    mut caller: Caller<'_, PluginStoreData>,
    cmd_ptr: i32,
    cmd_len: i32,
    scratch_ptr: i32,
    scratch_len: i32,
) -> anyhow::Result<i64> {
    const SHELL_TIMEOUT_SECS: u64 = 30;
    const SHELL_OUTPUT_CAP: usize = 10 * 1024 * 1024;

    check_enabled(&caller).map_err(into_anyhow)?;
    let cmd = read_string(&mut caller, cmd_ptr, cmd_len).map_err(into_anyhow)?;
    match check_shell_allowed(&caller.data().granted_caps, &caller.data().plugin_id, &cmd) {
        Ok(()) => {}
        Err(PluginError::CapabilityDenied(_)) => {
            return Err(handle_violation(&mut caller, "ShellExecute"));
        }
        Err(e) => return Err(into_anyhow(e)),
    }

    // Shell execution blocks on `recv_timeout`/`wait` (bounded to
    // `SHELL_TIMEOUT_SECS`); offload it to a blocking thread so it cannot
    // stall the wasmtime async executor (ADR-38 Decision 5). Only `cmd` and
    // the duration are moved into the closure; the `Caller` is untouched.
    let output = match tokio::task::spawn_blocking(move || {
        execute_shell_with_timeout(&cmd, std::time::Duration::from_secs(SHELL_TIMEOUT_SECS))
    })
    .await
    {
        Ok(Ok(Some(output))) => output,
        Ok(Ok(None)) => {
            caller.data_mut().last_error = Some("shell execution timed out".into());
            return Ok(RESULT_ERROR);
        }
        Ok(Err(error)) => {
            caller.data_mut().last_error = Some(format!("shell execution failed: {error}"));
            return Ok(RESULT_ERROR);
        }
        Err(e) => {
            caller.data_mut().last_error = Some(format!("shell execution offload failed: {e}"));
            return Ok(RESULT_ERROR);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        caller.data_mut().last_error = Some(format!(
            "shell exited with code {}: {}",
            output.status.code().unwrap_or(-1),
            stderr,
        ));
    }

    let stdout = &output.stdout[..output.stdout.len().min(SHELL_OUTPUT_CAP)];
    caller.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };
    write_to_scratch(&mut caller, stdout).map_err(into_anyhow)
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
    let mut stream = match provider.stream_completion(request, cancel.clone()).await {
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
    while let Some(chunk) = stream.next().await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_timeout_returns_without_waiting_for_command_completion() {
        // Deterministic check: the command writes a marker file only after
        // finishing. The timeout must kill it well before that, so the
        // marker must never appear. No wall-clock assertion — those flake
        // under load (a 100ms timeout once took 3.5s of wall time on CI
        // while the mechanism itself worked). `sleep 15` leaves huge
        // headroom: even a multi-second scheduling stall cannot create
        // the marker.
        let marker = std::env::temp_dir()
            .join(format!("concerto_shell_timeout_marker_{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        #[cfg(windows)]
        let command = format!("ping -n 16 127.0.0.1 >NUL && echo done > \"{}\"", marker.display());
        #[cfg(not(windows))]
        let command = format!("sleep 15 && touch \"{}\"", marker.display());

        let output = execute_shell_with_timeout(&command, Duration::from_millis(100)).unwrap();

        assert!(output.is_none(), "expected timeout, got: {output:?}");
        assert!(!marker.exists(), "command must not complete before the timeout fires");
        let _ = std::fs::remove_file(&marker);
    }
}
