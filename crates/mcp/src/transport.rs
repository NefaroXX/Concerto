//! STDIO framing and JSON-RPC message classification.
//!
//! Framing is **newline-delimited JSON**: exactly one JSON-RPC message per
//! line, with no `Content-Length` headers (the normative framing in every
//! published MCP spec revision). `serde_json`'s compact form never emits a
//! raw newline inside a message — string values are escaped — so a `\n`
//! always separates messages. A single message is bounded to 4 MiB so a
//! misbehaving server cannot exhaust client memory with one unbounded line.

use crate::error::McpError;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// Maximum size of a single newline-delimited JSON message, in bytes.
pub(crate) const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Chunk size used when reading a message. Small enough that the carry
/// between calls (bytes after a newline that arrived in the same read) stays
/// bounded.
const READ_CHUNK_BYTES: usize = 4096;

/// Serialize `message` and write it as one line (message + `\n`).
pub(crate) async fn write_message<W>(writer: &mut W, message: &Value) -> Result<(), McpError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(message)
        .map_err(|e| McpError::Protocol { detail: format!("failed to serialize message: {e}") })?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(McpError::LineTooLong { len: line.len() });
    }
    if line.contains(&b'\n') {
        return Err(McpError::Protocol { detail: "message would break newline framing".into() });
    }
    line.push(b'\n');
    writer.write_all(&line).await.map_err(McpError::from)?;
    Ok(())
}

/// Read exactly one newline-delimited message from `reader`.
///
/// `buf` is a caller-owned carry buffer that persists across calls: bytes
/// read past a newline (which land in the same OS read) stay in `buf` for the
/// next call instead of being dropped, which was the bug in the earlier
/// `Take`+inner-`BufReader` design. Start with an empty `Vec` and keep reusing
/// it for the lifetime of the connection.
///
/// Returns `Ok(None)` on EOF. A blank line is not a valid JSON-RPC message
/// and terminates the connection with [`McpError::Protocol`] (fail fast on
/// garbage rather than busy-looping). A trailing `\r` is tolerated
/// defensively, though the spec defines `\n` as the only terminator.
pub(crate) async fn read_message<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<Value>, McpError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    loop {
        // A complete line is already in the carry buffer.
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            // Split off the terminator first so the tail (bytes after the
            // newline) survives as the carry for the next call, then pop the
            // `\n` off the line and move the line out of `buf`.
            let tail = buf.split_off(pos + 1);
            buf.pop(); // '\n'
            let line = std::mem::replace(buf, tail);
            return parse_line(&line);
        }

        // Unterminated line exceeds the cap. Checked before each read so a
        // server that never emits a newline cannot grow the buffer unbounded.
        if buf.len() > MAX_MESSAGE_BYTES {
            return Err(McpError::LineTooLong { len: buf.len() });
        }

        let mut chunk = [0u8; READ_CHUNK_BYTES];
        let n = reader.read(&mut chunk).await.map_err(McpError::from)?;
        if n == 0 {
            // EOF. A clean EOF (empty carry) means the server closed its
            // output; a trailing unterminated line is parsed defensively.
            if buf.is_empty() {
                return Ok(None);
            }
            return parse_line(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parse one line (newline already removed) into a JSON-RPC message.
fn parse_line(line: &[u8]) -> Result<Option<Value>, McpError> {
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(McpError::LineTooLong { len: line.len() });
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return Err(McpError::Protocol {
            detail: "empty line is not a valid JSON-RPC message".into(),
        });
    }
    let message: Value = serde_json::from_slice(line)
        .map_err(|e| McpError::Protocol { detail: format!("malformed JSON-RPC message: {e}") })?;
    if !message.is_object() {
        return Err(McpError::Protocol { detail: "JSON-RPC message must be a JSON object".into() });
    }
    Ok(Some(message))
}

/// A client→server or server→client notification: has `method`, no `id`.
pub(crate) fn is_notification(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_none()
}

/// A server→client request: has both `method` and `id` (e.g. `ping`).
pub(crate) fn is_server_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some()
}

/// A response to one of our requests: has `id` and either `result` or
/// `error`.
pub(crate) fn is_response(message: &Value) -> bool {
    message.get("id").is_some()
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// The numeric id of a response, if present and numeric.
pub(crate) fn response_id(message: &Value) -> Option<u64> {
    message.get("id").and_then(Value::as_u64)
}

/// Extract the outcome of a response message.
///
/// Maps a server `error` object to [`McpError::JsonRpc`], except for the
/// protocol-version negotiation error (`-32602` with a non-empty
/// `data.supported` array), which is surfaced as
/// [`McpError::VersionMismatch`].
pub(crate) fn extract_result(message: &Value) -> Result<Value, McpError> {
    if let Some(error) = message.get("error") {
        return Err(jsonrpc_error_to_mcp(error).unwrap_or_else(|| McpError::Protocol {
            detail: format!("malformed json-rpc error object: {error}"),
        }));
    }
    match message.get("result") {
        Some(result) => Ok(result.clone()),
        None => {
            Err(McpError::Protocol { detail: "response has neither 'result' nor 'error'".into() })
        }
    }
}

/// Build the `{"jsonrpc":"2.0","id":..,"result":{}}` reply to a server
/// `ping` request.
pub(crate) fn ping_reply(message: &Value) -> Option<Value> {
    let id = message.get("id")?;
    Some(json_rpc_message(id.clone(), Some(json!({})), None))
}

fn jsonrpc_error_to_mcp(error: &Value) -> Option<McpError> {
    let code = error.get("code").and_then(Value::as_i64)?;
    let message = error.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
    let data = error.get("data").cloned();
    // Version negotiation (2025-11-25 spec): the server rejects `initialize`
    // with code -32602 and `data.supported` listing the versions it speaks.
    if code == -32602 {
        let supported: Vec<String> = data
            .as_ref()
            .and_then(|d| d.get("supported"))
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if !supported.is_empty() {
            return Some(McpError::VersionMismatch { supported });
        }
    }
    Some(McpError::JsonRpc { code, message, data })
}

fn json_rpc_message(id: Value, result: Option<Value>, error: Option<Value>) -> Value {
    let mut message = json!({ "jsonrpc": "2.0", "id": id });
    if let Some(result) = result {
        message["result"] = result;
    }
    if let Some(error) = error {
        message["error"] = error;
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_from(data: &[u8]) -> Result<Option<Value>, McpError> {
        let mut reader: &[u8] = data;
        let mut buf = Vec::new();
        read_message(&mut reader, &mut buf).await
    }

    #[tokio::test]
    async fn round_trip_single_message() {
        let mut wire = Vec::new();
        let original = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": {} });
        write_message(&mut wire, &original).await.expect("write should succeed");
        assert!(wire.ends_with(b"\n"));
        let parsed = read_from(&wire).await.expect("read should succeed").expect("not EOF");
        assert_eq!(parsed, original);
    }

    #[tokio::test]
    async fn two_messages_are_split_on_newlines() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({ "jsonrpc": "2.0", "id": 1, "result": { "a": 1 } }))
            .await
            .expect("write should succeed");
        write_message(&mut wire, &json!({ "jsonrpc": "2.0", "id": 2, "result": { "b": 2 } }))
            .await
            .expect("write should succeed");
        let mut reader: &[u8] = &wire;
        let mut buf = Vec::new();
        let first = read_message(&mut reader, &mut buf).await.expect("read should succeed");
        let second = read_message(&mut reader, &mut buf).await.expect("read should succeed");
        assert_eq!(first.expect("first message").get("id"), Some(&json!(1)));
        assert_eq!(second.expect("second message").get("id"), Some(&json!(2)));
        let eof = read_message(&mut reader, &mut buf).await.expect("read should succeed");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn crlf_line_terminator_is_tolerated() {
        let parsed = read_from(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n")
            .await
            .expect("read should succeed")
            .expect("not EOF");
        assert_eq!(parsed.get("id"), Some(&json!(1)));
    }

    #[tokio::test]
    async fn blank_line_is_a_protocol_error() {
        let err = read_from(b"\n").await.expect_err("blank line must fail");
        assert!(matches!(err, McpError::Protocol { .. }));
    }

    #[tokio::test]
    async fn overlong_line_is_rejected() {
        let mut line = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        line.push(b'\n');
        let err = read_from(&line).await.expect_err("must fail");
        assert!(matches!(err, McpError::LineTooLong { len } if len > MAX_MESSAGE_BYTES));
    }

    #[tokio::test]
    async fn malformed_json_is_a_protocol_error() {
        let err = read_from(b"this is not json\n").await.expect_err("must fail");
        assert!(matches!(err, McpError::Protocol { .. }));
    }

    #[tokio::test]
    async fn non_object_message_is_a_protocol_error() {
        let err = read_from(b"[1, 2, 3]\n").await.expect_err("must fail");
        assert!(matches!(err, McpError::Protocol { .. }));
    }

    #[tokio::test]
    async fn eof_without_newline_parses_partial_final_line() {
        let parsed = read_from(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}")
            .await
            .expect("read should succeed")
            .expect("not EOF");
        assert_eq!(parsed.get("id"), Some(&json!(9)));
    }

    #[test]
    fn message_classification() {
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": {} });
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let response = json!({ "jsonrpc": "2.0", "id": 1, "result": {} });
        let error_response =
            json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32601, "message": "nope" } });

        assert!(is_server_request(&request));
        assert!(!is_notification(&request));
        assert!(!is_response(&request));

        assert!(is_notification(&notification));
        assert!(!is_server_request(&notification));
        assert!(!is_response(&notification));

        assert!(is_response(&response));
        assert!(is_response(&error_response));
        assert_eq!(response_id(&response), Some(1));
        assert_eq!(response_id(&error_response), Some(2));
        assert_eq!(response_id(&notification), None);
    }

    #[test]
    fn extract_result_handles_success_and_error() {
        let success = json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } });
        assert_eq!(extract_result(&success).expect("success"), json!({ "ok": true }));

        let error =
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "bad" } });
        let err = extract_result(&error).expect_err("must fail");
        assert!(matches!(err, McpError::JsonRpc { code: -32602, .. }));
    }

    #[test]
    fn version_negotiation_error_maps_to_version_mismatch() {
        let error = json!({
            "code": -32602,
            "message": "unsupported protocol version",
            "data": { "supported": ["2024-11-05", "2025-03-26"] }
        });
        let err = extract_result(&json!({ "id": 1, "error": error })).expect_err("must fail");
        match err {
            McpError::VersionMismatch { supported } => {
                assert_eq!(supported, vec!["2024-11-05".to_string(), "2025-03-26".to_string()]);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn version_negotiation_without_supported_stays_jsonrpc() {
        let error = json!({ "code": -32602, "message": "bad params" });
        let err = extract_result(&json!({ "id": 1, "error": error })).expect_err("must fail");
        assert!(matches!(err, McpError::JsonRpc { code: -32602, .. }));
    }

    #[test]
    fn ping_reply_shape() {
        let request = json!({ "jsonrpc": "2.0", "id": 42, "method": "ping", "params": {} });
        let reply = ping_reply(&request).expect("reply should exist");
        assert_eq!(reply.get("id"), Some(&json!(42)));
        assert_eq!(reply.get("result"), Some(&json!({})));
        assert!(reply.get("error").is_none());
    }
}
