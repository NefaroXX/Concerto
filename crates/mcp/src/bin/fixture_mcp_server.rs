//! Minimal MCP stdio server used by the `concerto-mcp` integration tests.
//!
//! Speaks newline-delimited JSON-RPC 2.0 with protocol version `2025-11-25`
//! over stdin/stdout. Deliberately std-only (besides `serde_json`), so it
//! doubles as a readable reference for the wire format. Each request is
//! handled on its own thread so a blocking tool (e.g. `slow`) does not stall
//! other requests; responses are serialized through a shared stdout lock.
//!
//! Knobs (read from the environment):
//! - `FIXTURE_CRASH_ON_START=1` — exit(1) immediately at startup.
//! - `FIXTURE_PID_FILE=<path>` — write the process pid to `<path>` on start.
//! - `FIXTURE_VERSION=<v>` — report `<v>` as `protocolVersion` on initialize.
//! - `FIXTURE_REJECT_INITIALIZE=1` — reject initialize with the `-32602`
//!   negotiation error carrying `data.supported`.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2025-11-25";

fn main() {
    if std::env::var("FIXTURE_CRASH_ON_START").as_deref() == Ok("1") {
        eprintln!("fixture-mcp-server: FIXTURE_CRASH_ON_START=1, exiting 1");
        std::process::exit(1);
    }
    if let Ok(path) = std::env::var("FIXTURE_PID_FILE") {
        if let Err(e) = std::fs::write(&path, std::process::id().to_string()) {
            eprintln!("fixture-mcp-server: cannot write pid file {path}: {e}");
        }
    }
    let fixture_version = std::env::var("FIXTURE_VERSION").ok();
    let reject_initialize = std::env::var("FIXTURE_REJECT_INITIALIZE").as_deref() == Ok("1");
    let stdout = Arc::new(Mutex::new(io::stdout()));

    let mut stdin = io::stdin().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF: exit cleanly
            Ok(_) => {}
            Err(e) => {
                eprintln!("fixture-mcp-server: read error: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(e) => {
                eprintln!("fixture-mcp-server: ignoring unparseable line: {e}");
                continue;
            }
        };
        let stdout = stdout.clone();
        let fixture_version = fixture_version.clone();
        std::thread::spawn(move || {
            let response = handle(message, fixture_version.as_deref(), reject_initialize);
            if let Some(response) = response {
                if let Ok(serialized) = serde_json::to_string(&response) {
                    let mut out = serialized;
                    out.push('\n');
                    // Ignore poisoned locks / broken pipes: the client may
                    // have gone away.
                    if let Ok(mut guard) = stdout.lock() {
                        let _ = guard.write_all(out.as_bytes());
                        let _ = guard.flush();
                    }
                }
            }
        });
    }
}

fn handle(message: Value, fixture_version: Option<&str>, reject_initialize: bool) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            if reject_initialize {
                return Some(error_response(
                    &id,
                    -32602,
                    "unsupported protocol version",
                    Some(json!({ "supported": ["2024-11-05", "2025-03-26"] })),
                ));
            }
            Some(ok_response(
                &id,
                json!({
                    "protocolVersion": fixture_version.unwrap_or(PROTOCOL_VERSION),
                    "capabilities": {},
                    "serverInfo": { "name": "fixture-mcp-server", "version": env!("CARGO_PKG_VERSION") }
                }),
            ))
        }
        "notifications/initialized" => None,
        "ping" => Some(ok_response(&id, json!({}))),
        "tools/list" => Some(ok_response(
            &id,
            json!({ "tools": [
                {
                    "name": "echo",
                    "description": "Echo the text argument back",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                },
                {
                    "name": "fail",
                    "description": "Always fails",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "slow",
                    "description": "Sleeps for 10 seconds then responds",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "crash",
                    "description": "Exits the server with code 1",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ] }),
        )),
        "tools/call" => {
            let name = message
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match name {
                "echo" => {
                    let text = message
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("text"))
                        .and_then(Value::as_str);
                    match text {
                        Some(text) => Some(ok_response(
                            &id,
                            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                        )),
                        None => Some(error_response(
                            &id,
                            -32602,
                            "echo requires string argument 'text'",
                            None,
                        )),
                    }
                }
                "fail" => Some(ok_response(
                    &id,
                    json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true }),
                )),
                "slow" => {
                    std::thread::sleep(Duration::from_secs(10));
                    Some(ok_response(
                        &id,
                        json!({ "content": [{ "type": "text", "text": "slow done" }], "isError": false }),
                    ))
                }
                "crash" => std::process::exit(1),
                _ => Some(error_response(&id, -32602, format!("unknown tool: {name}"), None)),
            }
        }
        _ => Some(error_response(&id, -32601, format!("method not found: {method}"), None)),
    }
}

fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result })
}

fn error_response(id: &Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut response = json!({ "jsonrpc": "2.0", "id": id.clone(), "error": { "code": code, "message": message.into() } });
    if let Some(data) = data {
        response["error"]["data"] = data;
    }
    response
}
