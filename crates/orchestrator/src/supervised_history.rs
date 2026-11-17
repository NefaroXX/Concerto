//! Parent-owned history and metrics files for supervised agent processes.
//!
//! ADR-60 keeps the child free of session-database access: the supervisor
//! passes the child's prior typed history through a file in a parent-created
//! per-run directory, and the child appends its own completed turns back to
//! the same file. The file is therefore both the restore seed and the
//! accumulated history:
//!
//! 1. the parent loads `(session_id, agent_id)` history from the session
//!    store, writes it once with [`write_seed`], and stamps the path on the
//!    child process;
//! 2. the child reads the accumulated file at startup and appends only the
//!    turns it completes ([`append_turns`], atomic rewrite);
//! 3. a restarted child re-reads the accumulated file, so it inherits every
//!    turn the prior process flushed (the supervisor reuses the same command
//!    and environment across restarts).
//!
//! Metrics use a separate JSON Lines sidecar ([`append_metrics`] /
//! [`read_metrics`]) so the parent can attribute spend to the child agent
//! without adding provider metrics to the IPC payload (no protocol bump).
//!
//! All writes are bounded: history reads stop at 32 MiB and the metrics
//! sidecar is capped at 4 MiB. File writes are atomic (same-directory
//! temporary + rename) and mode `0600` on Unix.

use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use concerto_core::types::{Message, ProviderMetrics};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Environment variable carrying the accumulated history file path.
pub const HISTORY_ENV: &str = "CONCERTO_AGENT_HISTORY_PATH";
/// Environment variable carrying the JSON Lines metrics file path.
pub const METRICS_ENV: &str = "CONCERTO_AGENT_METRICS_PATH";

/// Hard read cap for the accumulated history file (32 MiB).
pub const MAX_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Hard cap for the child metrics sidecar (4 MiB).
pub const MAX_METRICS_BYTES: u64 = 4 * 1024 * 1024;

/// Distinguishes concurrent temporary files in one directory.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The two per-agent paths inside a parent-created run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedHistoryPaths {
    /// Accumulated typed conversation history (JSON array).
    pub history: PathBuf,
    /// Provider metrics JSON Lines sidecar.
    pub metrics: PathBuf,
}

/// Derive the per-agent file paths inside `root`.
///
/// `agent_id` is sanitized to ASCII alphanumerics, `_` and `-`; every other
/// character (including path separators and `.`) becomes `_`, so a hostile
/// id can never escape `root` or produce a nested path.
pub fn paths_for(root: &Path, agent_id: &str) -> SupervisedHistoryPaths {
    let safe = sanitize_agent_id(agent_id);
    SupervisedHistoryPaths {
        history: root.join(format!("{safe}.history.json")),
        metrics: root.join(format!("{safe}.metrics.jsonl")),
    }
}

/// Replace every character outside `[A-Za-z0-9_-]` with `_`; an empty id
/// becomes the single character `_`.
fn sanitize_agent_id(agent_id: &str) -> String {
    let mut sanitized = String::with_capacity(agent_id.len().max(1));
    for character in agent_id.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    sanitized
}

/// Remove assistant tool calls at the tail that have no matching tool result.
///
/// This is the single implementation used by the in-process specialist runner
/// and the supervised history file: OpenAI-compatible and Anthropic APIs
/// reject an assistant `tool_calls` message with no following tool result, so
/// an interrupted call must never become provider history.
pub fn trim_unanswered_tool_call_tail(messages: &mut Vec<Message>) {
    while messages.last().is_some_and(|message| message.tool_calls.is_some()) {
        messages.pop();
    }
}

/// Write the initial restore seed as a UTF-8 JSON array.
///
/// The write is atomic (sibling temporary + rename) and mode `0600` on Unix.
pub async fn write_seed(path: &Path, messages: &[Message]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(messages)
        .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))?;
    reject_oversized_history(bytes.len())?;
    write_atomic(path, &bytes).await
}

/// Read the accumulated history.
///
/// A missing file is an empty history. Files larger than
/// [`MAX_HISTORY_BYTES`] are rejected with [`ErrorKind::InvalidData`] rather
/// than read into memory.
pub async fn read_messages(path: &Path) -> std::io::Result<Vec<Message>> {
    read_messages_bounded(path, MAX_HISTORY_BYTES).await
}

/// Append completed turns to the accumulated history and rewrite atomically.
///
/// The accumulated file is read first and the new turns are appended. An
/// unanswered tool call is deliberately *not* trimmed here: the child flushes
/// the assistant turn carrying a tool call before the tool result exists, so a
/// periodic flush that trimmed it would lose the call and leave a later result
/// orphaned. The file may therefore hold a transient unanswered tail while the
/// child is running. Restore and harvest trim it before the history is used:
/// the child rewrites a trimmed file at startup, and the parent trims its
/// harvested delta.
pub async fn append_turns(path: &Path, new_turns: &[Message]) -> std::io::Result<()> {
    if new_turns.is_empty() {
        return Ok(());
    }
    let mut accumulated = read_messages(path).await?;
    accumulated.extend_from_slice(new_turns);
    let bytes = serde_json::to_vec(&accumulated)
        .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))?;
    reject_oversized_history(bytes.len())?;
    write_atomic(path, &bytes).await
}

/// Refuse a rewrite that would exceed the read cap. The existing file is left
/// untouched, so a too-large append never turns readable history into a file
/// the next child cannot restore.
fn reject_oversized_history(size: usize) -> std::io::Result<()> {
    if size as u64 > MAX_HISTORY_BYTES {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("history would exceed the {MAX_HISTORY_BYTES}-byte cap"),
        ));
    }
    Ok(())
}

/// Append provider metrics as JSON Lines.
///
/// One metric per line. Once the file would exceed [`MAX_METRICS_BYTES`] the
/// remaining lines are dropped with a warning; the sidecar is telemetry and
/// must never grow without bound.
pub async fn append_metrics(path: &Path, metrics: &[ProviderMetrics]) -> std::io::Result<()> {
    if metrics.is_empty() {
        return Ok(());
    }
    let mut payload = String::new();
    for metric in metrics {
        let line = serde_json::to_string(metric)
            .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))?;
        payload.push_str(&line);
        payload.push('\n');
    }
    let existing = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    if existing.saturating_add(payload.len() as u64) > MAX_METRICS_BYTES {
        tracing::warn!(
            path = %path.display(),
            existing,
            incoming = payload.len(),
            cap = MAX_METRICS_BYTES,
            "supervised metrics sidecar cap reached; dropping new metric lines"
        );
        return Ok(());
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    set_mode_0600(&mut options);
    let mut file = options.open(path).await?;
    file.write_all(payload.as_bytes()).await?;
    file.flush().await
}

/// Read the metrics sidecar.
///
/// A missing file is empty. Malformed lines are logged and skipped; they
/// never abort the read or panic. A file larger than [`MAX_METRICS_BYTES`] is
/// rejected with [`ErrorKind::InvalidData`].
pub async fn read_metrics(path: &Path) -> std::io::Result<Vec<ProviderMetrics>> {
    read_metrics_bounded(path, MAX_METRICS_BYTES).await
}

async fn read_messages_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<Message>> {
    let Some(bytes) = read_limited(path, limit).await? else {
        return Ok(Vec::new());
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))
}

async fn read_metrics_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<ProviderMetrics>> {
    let Some(bytes) = read_limited(path, limit).await? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8(bytes)
        .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))?;
    let mut metrics = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ProviderMetrics>(line) {
            Ok(metric) => metrics.push(metric),
            Err(error) => tracing::warn!(
                path = %path.display(),
                line = index + 1,
                %error,
                "ignoring malformed supervised metrics line"
            ),
        }
    }
    Ok(metrics)
}

/// Read at most `limit` bytes. `Ok(None)` means the file is absent.
async fn read_limited(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Read one byte past the cap so an oversized file is detectable without
    // trusting a pre-read metadata length (which could change concurrently).
    let mut reader = file.take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("file exceeds the {limit}-byte cap"),
        ));
    }
    Ok(Some(bytes))
}

/// Write `bytes` to a sibling temporary file, then rename it over `path`.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("history");
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_mode_0600(&mut options);
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        // Unix renames over an existing file atomically; Windows does not.
        // Removing the destination first leaves the small non-atomic window
        // the rest of the workspace accepts for rewrite-over-existing.
        #[cfg(windows)]
        if path.exists() {
            tokio::fs::remove_file(path).await?;
        }
        tokio::fs::rename(&temporary, path).await
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

#[cfg(unix)]
fn set_mode_0600(options: &mut tokio::fs::OpenOptions) {
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_mode_0600(_options: &mut tokio::fs::OpenOptions) {}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::provider_replay::GeminiReplay;
    use concerto_core::types::{Role, ToolCall, ToolResult};
    use serde_json::json;

    fn message(role: Role, content: &str) -> Message {
        Message {
            gemini_replay: None,
            role,
            content: content.to_owned(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: json!({ "path": "hello.txt" }),
            thought_signature: Some("opaque-signature".to_owned()),
        }
    }

    fn tool_result(id: &str, name: &str) -> ToolResult {
        ToolResult { id: id.to_owned(), name: name.to_owned(), content: json!({ "ok": true }) }
    }

    fn metric(provider: &str, tokens_in: u64) -> ProviderMetrics {
        ProviderMetrics {
            provider: provider.to_owned(),
            model: "model".to_owned(),
            tokens_in,
            tokens_out: tokens_in / 2,
            cost_usd: 0.01,
            latency_ms: 5,
        }
    }

    #[tokio::test]
    async fn seed_round_trip_preserves_reasoning_gemini_and_tool_fields() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("seed.json");
        let mut assistant = message(Role::Assistant, "");
        assistant.reasoning_content = Some("deliberate".to_owned());
        assistant.tool_calls = Some(vec![tool_call("call-1", "filesystem")]);
        assistant.gemini_replay = Some(
            GeminiReplay::capture(
                vec![json!({ "functionCall": { "name": "filesystem" } })],
                "",
                Some("deliberate"),
                &[tool_call("call-1", "filesystem")],
            )
            .expect("capture replay"),
        );
        let mut tool = message(Role::Tool, "");
        tool.tool_results = Some(vec![tool_result("call-1", "filesystem")]);
        let seed = vec![message(Role::User, "do it"), assistant, tool];

        write_seed(&path, &seed).await.expect("write seed");
        let loaded = read_messages(&path).await.expect("read seed");

        assert_eq!(
            serde_json::to_value(&loaded).expect("loaded json"),
            serde_json::to_value(&seed).expect("seed json"),
            "every typed field must survive the file round-trip"
        );
    }

    #[tokio::test]
    async fn append_turns_adds_delta_without_duplicating_seed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        let seed = vec![message(Role::User, "first"), message(Role::Assistant, "answer")];
        write_seed(&path, &seed).await.expect("write seed");

        let delta = vec![message(Role::Assistant, "follow-up")];
        append_turns(&path, &delta).await.expect("append delta");

        let loaded = read_messages(&path).await.expect("read history");
        assert_eq!(loaded.len(), 3, "seed plus exactly one delta: {loaded:?}");
        assert_eq!(loaded[0].content, "first");
        assert_eq!(loaded[1].content, "answer");
        assert_eq!(loaded[2].content, "follow-up");
    }

    #[tokio::test]
    async fn write_seed_atomically_replaces_existing_content() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        write_seed(&path, &[message(Role::User, "old")]).await.expect("first seed");
        write_seed(&path, &[message(Role::Assistant, "new")]).await.expect("second seed");

        let loaded = read_messages(&path).await.expect("read history");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "new");

        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .expect("read temp dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary files may survive: {leftovers:?}");
    }

    #[tokio::test]
    async fn oversized_append_is_rejected_without_replacing_existing_history() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        let original = vec![message(Role::User, "keep me")];
        write_seed(&path, &original).await.expect("write seed");
        let oversized = message(Role::Assistant, &"x".repeat(MAX_HISTORY_BYTES as usize));

        let error =
            append_turns(&path, &[oversized]).await.expect_err("an append past the cap must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        let loaded = read_messages(&path).await.expect("existing history survives");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "keep me");
    }

    #[tokio::test]
    async fn append_turns_preserves_an_unanswered_call_until_its_result_arrives() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        let mut resolved_call = message(Role::Assistant, "");
        resolved_call.tool_calls = Some(vec![tool_call("call-1", "filesystem")]);
        let mut resolved_result = message(Role::Tool, "");
        resolved_result.tool_results = Some(vec![tool_result("call-1", "filesystem")]);
        let seed = vec![message(Role::User, "go"), resolved_call, resolved_result];
        write_seed(&path, &seed).await.expect("write seed");

        // Tick 1: the assistant turn is persisted before its tool executes.
        let mut pending_call = message(Role::Assistant, "");
        pending_call.tool_calls = Some(vec![tool_call("call-2", "filesystem")]);
        append_turns(&path, &[pending_call]).await.expect("append pending call");
        let after_call = read_messages(&path).await.expect("read pending history");
        assert_eq!(
            after_call.last().and_then(|message| message.tool_calls.as_ref()).map(Vec::len),
            Some(1),
            "a periodic flush must not drop a call whose result is still in flight"
        );

        // Tick 2: the matching result arrives and both stay paired.
        let mut pending_result = message(Role::Tool, "");
        pending_result.tool_results = Some(vec![tool_result("call-2", "filesystem")]);
        append_turns(&path, &[pending_result]).await.expect("append result");
        let loaded = read_messages(&path).await.expect("read history");
        assert_eq!(loaded.len(), seed.len() + 2);
        assert_eq!(
            loaded[3].tool_calls.as_ref().map(|calls| calls[0].id.as_str()),
            Some("call-2"),
            "the call precedes its result"
        );
        assert_eq!(
            loaded[4].tool_results.as_ref().map(|results| results[0].id.as_str()),
            Some("call-2"),
            "the result follows its call"
        );
    }

    #[tokio::test]
    async fn append_and_read_metrics_round_trip() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("metrics.jsonl");
        assert!(read_metrics(&path).await.expect("missing file is empty").is_empty());

        append_metrics(&path, &[metric("openai", 10), metric("anthropic", 20)])
            .await
            .expect("append metrics");
        let loaded = read_metrics(&path).await.expect("read metrics");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].provider, "openai");
        assert_eq!(loaded[0].tokens_in, 10);
        assert_eq!(loaded[1].provider, "anthropic");
        assert_eq!(loaded[1].tokens_in, 20);
    }

    #[tokio::test]
    async fn malformed_metric_lines_are_skipped_not_panicked() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("metrics.jsonl");
        let valid = serde_json::to_string(&metric("openai", 7)).expect("serialize metric");
        std::fs::write(&path, format!("{valid}\nnot-json\n\n{valid}\n")).expect("seed metrics");

        let loaded = read_metrics(&path).await.expect("read tolerates malformed lines");
        assert_eq!(loaded.len(), 2, "two valid lines survive, malformed one is skipped");
    }

    #[tokio::test]
    async fn missing_history_file_reads_as_empty() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("missing.json");
        assert!(read_messages(&path).await.expect("missing is empty").is_empty());
    }

    #[tokio::test]
    async fn oversized_history_is_rejected_without_allocating_the_cap() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        std::fs::write(&path, b"[]").expect("write fixture");

        let error = read_messages_bounded(&path, 1)
            .await
            .expect_err("a one-byte cap must reject the two-byte fixture");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn paths_sanitize_agent_ids_and_prevent_traversal() {
        let root = Path::new("/tmp/run");
        let normal = paths_for(root, "review_agent-1");
        assert_eq!(normal.history, Path::new("/tmp/run/review_agent-1.history.json"));
        assert_eq!(normal.metrics, Path::new("/tmp/run/review_agent-1.metrics.jsonl"));

        let hostile = paths_for(root, "../../etc/passwd");
        assert_eq!(hostile.history.parent(), Some(root), "must stay inside the run dir");
        assert_eq!(hostile.metrics.parent(), Some(root));
        assert!(!hostile.history.to_string_lossy().contains(".."));
        let hostile_name = hostile.history.file_name().expect("file name");
        assert!(!hostile_name.to_string_lossy().contains('/'));
        assert!(!hostile_name.to_string_lossy().contains('\\'));

        let empty = paths_for(root, "");
        assert_eq!(empty.history, Path::new("/tmp/run/_.history.json"));
    }
}
