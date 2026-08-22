use concerto_core::error::ToolError;
use concerto_core::CancellationToken;
use lsp_types::{InitializeParams, WorkspaceFolder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

type PendingMap = HashMap<u64, oneshot::Sender<Result<serde_json::Value, ToolError>>>;
type DiagMap = HashMap<String, Vec<serde_json::Value>>;

#[derive(Debug)]
pub struct LspClient {
    project_dir: PathBuf,
    server_cmd: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: Arc<AtomicU64>,
    diagnostics: Arc<Mutex<DiagMap>>,
}

impl LspClient {
    pub fn new<P: AsRef<Path>>(project_dir: P, server_cmd: impl Into<String>) -> Self {
        Self {
            project_dir: project_dir.as_ref().to_path_buf(),
            server_cmd: server_cmd.into(),
            child: None,
            stdin: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn write_message(&mut self, msg: &str) -> Result<(), ToolError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ToolError::LspError { message: "stdin not available".into() })?;
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        stdin.flush().await.map_err(|e| ToolError::LspError { message: e.to_string() })
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&request)
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.write_message(&framed).await?;
        match timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(ToolError::LspError { message: "response channel closed".into() }),
            Err(_) => Err(ToolError::Timeout { timeout_secs: 30 }),
        }
    }

    pub async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), ToolError> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&notification)
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.write_message(&framed).await
    }

    pub async fn start(&mut self, cancel: CancellationToken) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if self.child.is_some() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.server_cmd);
        cmd.current_dir(&self.project_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| ToolError::LspError {
            message: format!("Failed to spawn LSP server: {}", e),
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::LspError { message: "No stdout from LSP server".into() })?;
        self.stdin = child.stdin.take();
        self.child = Some(child);
        let pending = self.pending.clone();
        let diagnostics = self.diagnostics.clone();
        let cancel_for_loop = cancel.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut headers = String::new();
            loop {
                if cancel_for_loop.is_cancelled() {
                    return;
                }
                headers.clear();
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) => return,
                        Ok(_) => {
                            if line == "\r\n" || line == "\n" {
                                break;
                            }
                            headers.push_str(&line);
                        }
                        Err(_) => return,
                    }
                }
                let mut content_length: usize = 0;
                for header in headers.lines() {
                    if let Some(val) = header.strip_prefix("Content-Length:") {
                        if let Ok(len) = val.trim().parse() {
                            content_length = len;
                        }
                    }
                }
                if content_length == 0 {
                    continue;
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                if cancel_for_loop.is_cancelled() {
                    return;
                }
                let msg: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // Handle response or notification
                if let Some(id) = msg.get("id").and_then(|i| i.as_u64()) {
                    let sender_opt = {
                        let mut pending = pending.lock().await;
                        pending.remove(&id)
                    };
                    if let Some(sender) = sender_opt {
                        let result = msg.get("result").cloned().ok_or_else(|| {
                            ToolError::LspError { message: "Missing result".into() }
                        });
                        let _ = sender.send(result);
                    }
                } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                    if method == "textDocument/publishDiagnostics" {
                        if let Some(params) = msg.get("params") {
                            if let (Some(uri), Some(diags)) =
                                (params.get("uri"), params.get("diagnostics"))
                            {
                                if let Some(uri_str) = uri.as_str() {
                                    let path = uri_str.trim_start_matches("file://");
                                    let mut diags_map = diagnostics.lock().await;
                                    diags_map.insert(
                                        path.to_string(),
                                        diags.as_array().cloned().unwrap_or_default(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });
        // Handshake
        let uri = lsp_types::Uri::from_str(&format!("file://{}", self.project_dir.display()))
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            capabilities: Default::default(),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri,
                name: self.project_dir.to_string_lossy().to_string(),
            }]),
            ..Default::default()
        };
        let _init_res = self
            .send_request(
                "initialize",
                serde_json::to_value(init_params)
                    .map_err(|e| ToolError::LspError { message: e.to_string() })?,
            )
            .await?;
        // ignore init_res
        self.send_notification("initialized", serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn stop(&mut self, cancel: CancellationToken) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // shutdown request
        let _ = self.send_request("shutdown", serde_json::json!({})).await;
        let _ = self.send_notification("exit", serde_json::json!({})).await;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Ok(())
    }

    pub async fn get_diagnostics(&self, file_path: &str) -> Vec<serde_json::Value> {
        let map = self.diagnostics.lock().await;
        map.get(file_path).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the constructor initialises all fields to their default/empty state.
    #[test]
    fn test_client_constructor_sets_defaults() {
        let client = LspClient::new("/tmp/proj", "rust-analyzer");

        assert_eq!(client.project_dir.to_str(), Some("/tmp/proj"));
        assert_eq!(client.server_cmd, "rust-analyzer");
        assert!(client.child.is_none());
        assert!(client.stdin.is_none());
        assert_eq!(client.next_id.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Internal maps start empty.
        let pending_empty = client.pending.try_lock().unwrap().is_empty();
        assert!(pending_empty);
    }

    /// Calling `send_request` before the server has started must fail because stdin
    /// has not been set up yet.
    #[tokio::test]
    async fn test_client_send_request_fails_without_stdin() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.send_request("textDocument/hover", serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stdin not available"));
    }

    /// Calling `send_notification` before the server has started must fail.
    #[tokio::test]
    async fn test_client_send_notification_fails_without_stdin() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.send_notification("initialized", serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stdin not available"));
    }

    /// `stop` on a client that was never started should return `Ok(())` without
    /// attempting to kill a non-existent child process.
    #[tokio::test]
    async fn test_client_stop_returns_ok_when_not_started() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = client.stop(cancel).await;
        assert!(result.is_ok());
    }

    /// `get_diagnostics` for a file path that has never received diagnostics
    /// should return an empty `Vec`.
    #[tokio::test]
    async fn test_client_get_diagnostics_returns_empty_for_unknown_file() {
        let client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.get_diagnostics("/nonexistent/file.rs").await;
        assert!(result.is_empty());
    }

    /// Two separate `LspClient` instances must have isolated diagnostics maps.
    #[tokio::test]
    async fn test_client_isolation_between_instances() {
        let client_a = LspClient::new("/tmp/a", "server-a");
        let client_b = LspClient::new("/tmp/b", "server-b");

        // Insert a diagnostic into client_a's map directly (for testing only).
        {
            let mut map = client_a.diagnostics.lock().await;
            map.insert("/tmp/a/file.rs".into(), vec![serde_json::json!({"severity": 1})]);
        }

        // client_b must not see client_a's diagnostics.
        let b_diags = client_b.get_diagnostics("/tmp/a/file.rs").await;
        assert!(b_diags.is_empty());

        // client_a must still see its own.
        let a_diags = client_a.get_diagnostics("/tmp/a/file.rs").await;
        assert_eq!(a_diags.len(), 1);
    }
}
