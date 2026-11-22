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
//! temporary + rename), synchronize the temporary file before the rename, and
//! mode `0600` on Unix.
//!
//! ## Trust boundary
//!
//! The child is the attribution boundary (ADR-60): history lines it writes
//! are trusted *semantically* as that child's own completed turns, and the
//! parent does not try to re-validate their meaning. The parent only enforces
//! structure — repairing an interrupted tool batch in memory for each provider
//! request and applying the size caps below — and splits the file at the
//! parent-stamped seed length. A child that wrote a forged or malformed but
//! structurally valid turn would be trusted exactly like one it actually
//! produced.

use std::collections::HashMap;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use concerto_core::types::{Message, ProviderMetrics};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ulid::Ulid;

/// Environment variable carrying the accumulated history file path.
pub const HISTORY_ENV: &str = "CONCERTO_AGENT_HISTORY_PATH";
/// Environment variable carrying the JSON Lines metrics file path.
pub const METRICS_ENV: &str = "CONCERTO_AGENT_METRICS_PATH";
/// Environment variable carrying the parent's exact repaired seed message count.
///
/// The parent repairs the seed before writing it, so the file holds exactly
/// this many messages when the child starts. The parent uses the same value to
/// slice child-owned turns during harvest; the child reads the whole file and
/// repairs it in memory without rewriting it.
pub const SEED_LEN_ENV: &str = "CONCERTO_AGENT_HISTORY_SEED_LEN";

/// Hard read cap for the accumulated history file (32 MiB).
pub const MAX_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Hard cap for the child metrics sidecar (4 MiB).
pub const MAX_METRICS_BYTES: u64 = 4 * 1024 * 1024;

/// Distinguishes concurrent temporary files in one directory.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Maximum readable prefix characters retained in a per-agent file stem.
const MAX_SAFE_PREFIX: usize = 48;

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
/// The file stem is a readable, path-safe prefix of the agent id plus a 64-bit
/// hash of the *complete* id. Sanitizing alone would collapse distinct valid
/// ids such as `review.v1` and `review_v1` onto one file; the hash keeps those
/// identities separate while the prefix keeps filenames debuggable. Every
/// other character (including path separators and `.`) becomes `_`, so a
/// hostile id can never escape `root` or produce a nested path. The prefix is
/// bounded so the filename stays comfortably under platform limits.
pub fn paths_for(root: &Path, agent_id: &str) -> SupervisedHistoryPaths {
    let stem = file_stem(agent_id);
    SupervisedHistoryPaths {
        history: root.join(format!("{stem}.history.json")),
        metrics: root.join(format!("{stem}.metrics.jsonl")),
    }
}

/// Readable prefix plus a stable 64-bit hash of the complete agent id.
fn file_stem(agent_id: &str) -> String {
    format!("{}-{:016x}", sanitize_agent_id(agent_id), stable_agent_id_hash(agent_id))
}

/// Replace every character outside `[A-Za-z0-9_-]` with `_` and retain at most
/// [`MAX_SAFE_PREFIX`] characters; an empty prefix becomes `_`.
fn sanitize_agent_id(agent_id: &str) -> String {
    let mut sanitized = String::with_capacity(agent_id.len().clamp(1, MAX_SAFE_PREFIX));
    for character in agent_id.chars().take(MAX_SAFE_PREFIX) {
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

/// Deterministic FNV-1a hash used only to disambiguate ephemeral filenames.
///
/// This is not a security boundary: the sanitized prefix already prevents path
/// traversal, and the hash only needs to be stable across processes and
/// restarts so a child re-reads the file its parent wrote.
fn stable_agent_id_hash(agent_id: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in agent_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Derive paths for a run's agents, excluding every agent whose physical
/// history file would be shared with another id.
///
/// Returns `(paths, colliding_agent_ids)`. Collisions are reported in input
/// order so the caller's disabled set and logging are deterministic. A normal
/// run has no collisions; this guards the invariant that two configured
/// agents must never read or write one history/metrics file.
pub fn unique_paths_for_agents<'a>(
    root: &Path,
    agent_ids: impl IntoIterator<Item = &'a str>,
) -> (HashMap<String, SupervisedHistoryPaths>, Vec<String>) {
    let derived: Vec<_> =
        agent_ids.into_iter().map(|id| (id.to_owned(), paths_for(root, id))).collect();
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();
    for (_, paths) in &derived {
        *counts.entry(paths.history.clone()).or_insert(0) += 1;
    }
    let mut unique = HashMap::new();
    let mut colliding = Vec::new();
    for (agent_id, paths) in derived {
        if counts.get(&paths.history).copied().unwrap_or(0) > 1 {
            colliding.push(agent_id);
        } else {
            unique.insert(agent_id, paths);
        }
    }
    (unique, colliding)
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
/// child is running. Durable evidence is never rewritten to hide that: every
/// provider-facing restore repairs it in memory with
/// [`crate::provider_transcript::restore_tool_responses`], and harvest persists
/// the raw child turns so load-time repair sees them again.
///
/// The child-written turns are trusted as that child's own turns (ADR-60
/// attribution boundary); this function only applies the structural size cap.
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

/// Restore the accumulated history for a supervised child process.
///
/// The file is read but never rewritten: the parent's `initial_seed_len`
/// harvest slice depends on the file's exact prefix, so a child that shrank or
/// repaired it would shift that slice and drop or misattribute turns. An
/// interrupted tool batch is repaired in memory with
/// [`crate::provider_transcript::restore_tool_responses`] instead, keeping the
/// durable evidence raw while making the child's first provider request valid.
/// The repair is idempotent, so a seed the parent already repaired is
/// unchanged.
pub async fn restore_for_child(path: &Path) -> std::io::Result<Vec<Message>> {
    let messages = read_messages(path).await?;
    Ok(crate::provider_transcript::restore_tool_responses(messages))
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
/// One metric per line. When the incoming batch would exceed
/// [`MAX_METRICS_BYTES`], a contiguous prefix of whole lines that fits is
/// appended and the rest are dropped with a warning; a line is never split.
/// The sidecar is telemetry and must never grow without bound. The file is
/// forced to mode `0600` after opening because an `OpenOptions` mode only
/// applies when the file is created, not when an existing file is reopened.
pub async fn append_metrics(path: &Path, metrics: &[ProviderMetrics]) -> std::io::Result<()> {
    if metrics.is_empty() {
        return Ok(());
    }
    let existing = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    let mut budget = MAX_METRICS_BYTES.saturating_sub(existing);
    let mut payload = String::new();
    let mut kept = 0usize;
    for metric in metrics {
        let mut line = serde_json::to_string(metric)
            .map_err(|error| IoError::new(ErrorKind::InvalidData, error.to_string()))?;
        line.push('\n');
        let line_len = line.len() as u64;
        if line_len > budget {
            break;
        }
        budget -= line_len;
        payload.push_str(&line);
        kept += 1;
    }
    let dropped = metrics.len() - kept;
    if kept == 0 {
        tracing::warn!(
            path = %path.display(),
            existing,
            incoming = metrics.len(),
            dropped,
            cap = MAX_METRICS_BYTES,
            "supervised metrics sidecar cap reached; dropping all new metric lines"
        );
        return Ok(());
    }
    if dropped > 0 {
        tracing::warn!(
            path = %path.display(),
            existing,
            kept,
            dropped,
            cap = MAX_METRICS_BYTES,
            "supervised metrics sidecar cap reached; dropping metric lines that do not fit"
        );
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    set_mode_0600(&mut options);
    let mut file = options.open(path).await?;
    set_file_mode_0600(path).await?;
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

/// Write `bytes` to a sibling temporary file, sync it, then rename it over
/// `path`.
///
/// A `sync_all` failure fails the write: a history file that is visible after
/// the rename but whose contents may be lost on a crash is worse than no
/// rewrite at all (the previous file is left intact). The temporary name
/// carries a fresh ULID in addition to the PID and sequence so concurrent
/// writers cannot collide even across restarts.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("history");
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary =
        parent.join(format!(".{name}.{}.{}.{sequence}.tmp", std::process::id(), Ulid::new()));

    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_mode_0600(&mut options);
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        // Durability before the rename: a synced temporary either becomes the
        // new history or leaves the old one in place.
        file.sync_all().await?;
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

/// Force an existing file to mode `0600`; `OpenOptions::mode` only applies at
/// creation, so an appended-to sidecar needs an explicit chmod.
#[cfg(unix)]
async fn set_file_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o600);
    tokio::fs::set_permissions(path, permissions).await
}

#[cfg(not(unix))]
async fn set_file_mode_0600(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

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
            agent_id: None,
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
    async fn restore_for_child_repairs_an_interrupted_batch_without_rewriting() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        let mut assistant = message(Role::Assistant, "calling two tools");
        assistant.tool_calls =
            Some(vec![tool_call("call-a", "filesystem"), tool_call("call-b", "filesystem")]);
        let mut answered = message(Role::Tool, "");
        answered.tool_results = Some(vec![tool_result("call-a", "filesystem")]);
        let seed = vec![message(Role::User, "go"), assistant, answered];
        write_seed(&path, &seed).await.expect("write seed");
        let before = std::fs::read(&path).expect("read seed bytes");

        let restored = restore_for_child(&path).await.expect("restore child history");
        assert_eq!(restored.len(), 4, "only the unanswered call gets a synthetic result");
        assert_eq!(restored[1].tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            restored[2].tool_results.as_ref().map(|results| results[0].id.as_str()),
            Some("call-a"),
            "a recorded result is preserved unchanged"
        );
        let synthetic = restored[3]
            .tool_results
            .as_ref()
            .and_then(|results| results.first())
            .expect("synthetic result for the unanswered call");
        assert_eq!(synthetic.id, "call-b");
        assert_eq!(synthetic.content["status"], "outcome_unknown");
        assert_eq!(
            std::fs::read(&path).expect("reread seed bytes"),
            before,
            "restore never rewrites durable evidence"
        );

        assert_eq!(
            serde_json::to_value(crate::provider_transcript::restore_tool_responses(
                restored.clone()
            ))
            .expect("serialize re-repaired prompt"),
            serde_json::to_value(&restored).expect("serialize restored prompt"),
            "repairing an already-repaired prompt must not duplicate results"
        );
    }

    #[tokio::test]
    async fn restore_for_child_keeps_trailing_observations_after_synthetic_results() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("history.json");
        let mut assistant = message(Role::Assistant, "");
        assistant.tool_calls = Some(vec![tool_call("call-a", "filesystem")]);
        let seed = vec![
            message(Role::User, "go"),
            assistant,
            message(Role::System, "runtime verification"),
        ];
        write_seed(&path, &seed).await.expect("write seed");

        let restored = restore_for_child(&path).await.expect("restore child history");
        assert_eq!(restored.len(), 4);
        assert_eq!(restored[2].role, Role::Tool);
        assert_eq!(
            restored[2].tool_results.as_ref().map(|results| results[0].id.as_str()),
            Some("call-a")
        );
        assert_eq!(
            restored[3].role,
            Role::System,
            "a non-tool observation stays after the results"
        );
        assert_eq!(restored[3].content, "runtime verification");
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
    async fn metrics_cap_keeps_only_the_whole_lines_that_fit() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("metrics.jsonl");
        let first = metric("openai", 10);
        let second = metric("anthropic", 20);
        let mut first_line = serde_json::to_string(&first).expect("serialize first");
        first_line.push('\n');
        let second_line = serde_json::to_string(&second).expect("serialize second");
        // Leave room for exactly the first line; the second must be dropped
        // whole rather than split or taking the first batch entry with it.
        let existing = MAX_METRICS_BYTES - first_line.len() as u64;
        std::fs::write(&path, vec![b' '; existing as usize]).expect("fill to the cap");

        append_metrics(&path, &[first, second]).await.expect("append partial batch");

        let bytes = std::fs::read(&path).expect("read metrics sidecar");
        assert_eq!(bytes.len() as u64, MAX_METRICS_BYTES);
        assert!(bytes.ends_with(first_line.as_bytes()), "the line that fits is appended whole");
        assert!(!bytes.windows(second_line.len()).any(|window| window == second_line.as_bytes()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metrics_permissions_are_forced_to_0600_on_append() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("metrics.jsonl");
        std::fs::write(&path, b"").expect("pre-create metrics file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen fixture mode");

        append_metrics(&path, &[metric("openai", 1)]).await.expect("append metrics");

        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an existing metrics file must be chmodded to 0600");
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
    async fn legacy_metric_lines_without_agent_id_deserialize_as_root() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("metrics.jsonl");
        // Pre-agent_id sidecar shape: the field must default to None so an
        // upgraded parent can still harvest a child started by an older build.
        let legacy = r#"{"provider":"legacy","model":"m","tokens_in":5,"tokens_out":2,"cost_usd":0.5,"latency_ms":10}"#;
        std::fs::write(&path, format!("{legacy}\n")).expect("write legacy metrics");

        let loaded = read_metrics(&path).await.expect("legacy line must parse");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].agent_id, None);
        assert_eq!(loaded[0].tokens_in, 5);
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
        let expected_stem = file_stem("review_agent-1");
        assert_eq!(normal.history, root.join(format!("{expected_stem}.history.json")));
        assert_eq!(normal.metrics, root.join(format!("{expected_stem}.metrics.jsonl")));
        assert_eq!(paths_for(root, "review_agent-1"), normal, "paths are deterministic");

        let hostile = paths_for(root, "../../etc/passwd");
        assert_eq!(hostile.history.parent(), Some(root), "must stay inside the run dir");
        assert_eq!(hostile.metrics.parent(), Some(root));
        assert!(!hostile.history.to_string_lossy().contains(".."));
        let hostile_name = hostile.history.file_name().expect("file name");
        assert!(!hostile_name.to_string_lossy().contains('/'));
        assert!(!hostile_name.to_string_lossy().contains('\\'));

        let empty = paths_for(root, "");
        assert_eq!(empty.history.parent(), Some(root));
        assert!(empty.history.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.starts_with("_-") && name.ends_with(".history.json")
        }));
    }

    #[test]
    fn distinct_valid_ids_never_share_a_history_or_metrics_path() {
        let root = Path::new("/tmp/run");
        // Both ids pass `config::agents::validate_agent_id`; sanitizing alone
        // would collapse them onto `review_v1`.
        let dotted = paths_for(root, "review.v1");
        let underscored = paths_for(root, "review_v1");
        assert_ne!(dotted.history, underscored.history);
        assert_ne!(dotted.metrics, underscored.metrics);
        assert!(dotted
            .history
            .file_name()
            .is_some_and(|name| { name.to_string_lossy().starts_with("review_v1-") }));
        assert!(underscored
            .history
            .file_name()
            .is_some_and(|name| { name.to_string_lossy().starts_with("review_v1-") }));

        let (unique, colliding) = unique_paths_for_agents(root, ["review.v1", "review_v1"]);
        assert!(colliding.is_empty(), "distinct ids must not collide: {colliding:?}");
        assert_eq!(unique.len(), 2);
        assert_eq!(unique.get("review.v1"), Some(&dotted));
        assert_eq!(unique.get("review_v1"), Some(&underscored));
    }

    #[test]
    fn unique_paths_disable_duplicate_physical_paths_deterministically() {
        let root = Path::new("/tmp/run");
        let first = unique_paths_for_agents(root, ["reviewer", "reviewer"]);
        let second = unique_paths_for_agents(root, ["reviewer", "reviewer"]);
        assert_eq!(first, second, "the collision check is deterministic");
        assert!(first.0.is_empty(), "neither duplicate may keep a shared file");
        assert_eq!(first.1, vec!["reviewer".to_owned(), "reviewer".to_owned()]);
    }

    #[test]
    fn long_agent_ids_keep_filenames_bounded() {
        let root = Path::new("/tmp/run");
        let long_id = "a".repeat(4_096);
        let paths = paths_for(root, &long_id);
        let name = paths.history.file_name().expect("file name").to_string_lossy();
        // 48-character prefix + `-` + 16 hex hash + `.history.json`.
        assert!(name.len() <= 48 + 1 + 16 + ".history.json".len(), "bounded name: {name}");
        assert!(name.starts_with(&"a".repeat(48)));
    }
}
