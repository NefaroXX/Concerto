//! ADR-65 §4 — safe read deduplication (serve-from-cache).
//!
//! A plain single-path filesystem read that has already been observed (clean
//! row in `resource_facts`) can be answered from the cache instead of invoking
//! the executor — BUT only when it can be proven the cache still matches the
//! current disk. The **never-stale** rule is absolute: on any doubt the tool
//! executes normally.
//!
//! Every serve must satisfy ALL of these predicates (see [`maybe_serve_read`]):
//! 1. the guarded arguments are exactly a plain single-path read (no
//!    range/glob/recursive/options — see [`plain_single_path_read`]);
//! 2. a clean row exists for the path (`dirty == 0`);
//! 3. a fresh `stat` of the resolved path NOW matches the row's `size_bytes`
//!    and `mtime_ms` (never trusting the watcher — re-statted per serve);
//! 4. cached content is present AND `blake3(content) == content_hash`.
//!
//! When served, the executor is NOT invoked and the policy engine never
//! re-evaluates (it lives inside `ToolExecutor::execute`). This is the accepted
//! residual risk of the task spec: serving a plain read of an already-observed
//! clean file skips an approval prompt that would only ever grep the same
//! already-approved path.

use std::path::Path;

use concerto_core::CancellationToken;
use concerto_sessions::ResourceFacts;

use crate::tool_facts::{mtime_ms, resolve_path, ToolFactContext};

/// The result of a successful cache serve — the cached content plus the
/// `event_id` the content is attributable to (the clean row's `last_event_id`),
/// so the served `ToolExecuted` fact can carry `served_from`.
pub struct ReadServe {
    /// The raw (tool-reported) path that was served.
    pub path: String,
    pub content: String,
    pub event_id: String,
}

/// True when the guarded arguments are exactly a plain single-path read: tool
/// `filesystem`, operation `read`, key set ⊆ `{operation, path}` (any extras —
/// flags/options/range/glob/recursive/destination — force fresh execution),
/// and a non-empty string `path`.
pub fn plain_single_path_read<'a>(tool: &str, args: &'a serde_json::Value) -> Option<&'a str> {
    if tool != "filesystem" {
        return None;
    }
    if args.get("operation").and_then(|v| v.as_str()) != Some("read") {
        return None;
    }
    let obj = args.as_object()?;
    if obj.keys().any(|key| key != "operation" && key != "path") {
        return None;
    }
    let path = args.get("path").and_then(|v| v.as_str())?;
    if path.is_empty() {
        return None;
    }
    Some(path)
}

/// Decide whether to serve this read from cache. Returns `Some` only when all
/// four predicates hold; returns `None` (execute normally) on any doubt or
/// error — serving never fails the loop, it just doesn't happen.
///
/// The serve re-stats the same resolved path the observation hashed by reusing
/// [`crate::tool_facts::resolve_path`], guaranteeing `rule (3)` compares the
/// row against the exact path that produced it.
pub async fn maybe_serve_read(
    facts: &ToolFactContext,
    project_root: &Path,
    tool: &str,
    args: &serde_json::Value,
    cancel: &CancellationToken,
) -> Option<ReadServe> {
    let pool = facts.pool()?;
    let path = plain_single_path_read(tool, args)?;

    let store = ResourceFacts::new(pool.clone());
    // Rule 2 + cached content (rule 4's first half): a row that is observed,
    // clean, and actually content-cached. Any of these failing → execute.
    let cached = store.cached_read(path, cancel).await.ok()??;
    if cached.row.dirty {
        return None;
    }
    let content_hash = cached.row.content_hash.as_deref()?;

    let resolved = resolve_path(project_root, path);
    // Rule 3: re-stat NOW and compare size + mtime with the observation.
    let Ok(meta) = std::fs::metadata(&resolved) else {
        return None;
    };
    if cached.row.size_bytes != Some(meta.len()) {
        return None;
    }
    if cached.row.mtime_ms != mtime_ms(&meta) {
        return None;
    }

    // Rule 4's second half: the cached bytes must hash to the observed hash.
    // This also catches a corrupted cache-vs-row mismatch with no extra disk
    // read (the content already sits in the cached string).
    if blake3::hash(cached.content.as_bytes()).to_hex().to_string() != content_hash {
        return None;
    }

    cached.row.last_event_id.map(|event_id| ReadServe {
        path: path.to_owned(),
        content: cached.content,
        event_id,
    })
}

/// After a successful plain filesystem read, cache its exact output content so
/// an identical later read can be served. The observation row already exists
/// (created by `record_tool_fact` → `apply_observed`); this only attaches the
/// content bytes. Fail-soft and bounded — see `ToolFactContext::cache_read_content`.
pub async fn cache_read_output(
    facts: &ToolFactContext,
    tool: &str,
    args: &serde_json::Value,
    output_data: &serde_json::Value,
    cancel: &CancellationToken,
) {
    // Cache only what we could serve: a plain single-path read.
    let Some(path) = plain_single_path_read(tool, args) else {
        return;
    };
    let Some(content) = output_data.get("content").and_then(|v| v.as_str()) else {
        return;
    };
    facts.cache_read_content(path, content, cancel).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use concerto_core::CancellationToken;
    use concerto_sessions::{ObservedPath, ResourceFacts, ToolExecutedPayload};

    use super::*;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("read_cache_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    }

    /// Write `bytes` to `root/<path>` and seed a clean, content-cached
    /// observation matching it — the state under which a serve should succeed.
    async fn seed_clean_read(pool: &sqlx::SqlitePool, root: &Path, path: &str, bytes: &[u8]) {
        let file = root.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("fixture dir");
        }
        std::fs::write(&file, bytes).expect("fixture write");
        let meta = std::fs::metadata(&file).expect("fixture metadata");
        let store = ResourceFacts::new(pool.clone());
        let payload = ToolExecutedPayload {
            agent_id: Some("seeder".to_owned()),
            task_id: None,
            run_id: None,
            tool: "filesystem".to_owned(),
            args: serde_json::json!({}),
            success: true,
            exit_code: None,
            generation: "g1".to_owned(),
            served_from: None,
            paths: vec![ObservedPath {
                path: path.to_owned(),
                size_bytes: Some(bytes.len() as u64),
                mtime_ms: mtime_ms(&meta),
                content_hash: Some(blake3::hash(bytes).to_hex().to_string()),
            }],
        };
        store
            .apply_observed("ev-1", "seeder", crate::tool_facts::unix_ms(), &payload, &cancel())
            .await
            .expect("seed observed");
        let content = String::from_utf8_lossy(bytes).into_owned();
        assert!(store.store_read_content(path, &content, &cancel()).await.expect("cached"));
    }

    fn read_args(operation: &str, path: &str) -> serde_json::Value {
        serde_json::json!({ "operation": operation, "path": path })
    }

    #[test]
    fn plain_single_path_read_accepts_only_plain_filesystem_reads() {
        assert_eq!(plain_single_path_read("filesystem", &read_args("read", "a.md")), Some("a.md"));
        assert_eq!(
            plain_single_path_read("filesystem", &serde_json::json!({ "operation": "read" })),
            None,
            "missing path is not a plain read"
        );
        assert_eq!(
            plain_single_path_read("filesystem", &read_args("write", "a.md")),
            None,
            "writes are never served"
        );
        assert_eq!(
            plain_single_path_read("read_file", &read_args("read", "a.md")),
            None,
            "only the filesystem tool is considered"
        );
        assert_eq!(
            plain_single_path_read(
                "filesystem",
                &serde_json::json!({ "operation": "read", "path": "a.md", "range": [0, 10] })
            ),
            None,
            "extra keys (range/options/glob/recursive) force fresh execution"
        );
        assert_eq!(
            plain_single_path_read("filesystem", &read_args("read", "")),
            None,
            "an empty path is not servable"
        );
    }

    #[tokio::test]
    async fn serves_when_observed_clean_and_unchanged_on_disk() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("a.md"), b"hello world").expect("fixture");
        seed_clean_read(&pool, root.path(), "a.md", b"hello world").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        let args = read_args("read", "a.md");

        let serve = maybe_serve_read(&ctx, root.path(), "filesystem", &args, &cancel()).await;
        let serve = serve.expect("a clean, unchanged, content-cached read serves");
        assert_eq!(serve.path, "a.md");
        assert_eq!(serve.content, "hello world");
        assert_eq!(serve.event_id, "ev-1", "served_from attributes the original observation");
        assert_eq!(serve.path, "a.md");
    }

    #[tokio::test]
    async fn never_serves_dirty_or_missing_rows() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("b.md"), b"dirty content").expect("fixture");
        seed_clean_read(&pool, root.path(), "b.md", b"dirty content").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        let store = ResourceFacts::new(pool.clone());
        store.mark_dirty("b.md", &cancel()).await.expect("dirty");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "b.md"),
                &cancel()
            )
            .await
            .is_none(),
            "a dirty row is never served"
        );
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "ghost.md"),
                &cancel()
            )
            .await
            .is_none(),
            "a missing row is never served"
        );
    }

    #[tokio::test]
    async fn never_serves_when_disk_diverges_from_the_observation() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("c.md"), b"old content").expect("fixture");
        seed_clean_read(&pool, root.path(), "c.md", b"old content").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");

        // Same mtime tick or not, a size change is a deterministic divergence:
        // the never-stale rule must block the serve.
        std::fs::write(root.path().join("c.md"), b"changed!").expect("rewrite");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "c.md"),
                &cancel()
            )
            .await
            .is_none(),
            "a stat divergence (size) blocks the serve"
        );
    }

    #[tokio::test]
    async fn never_serves_when_cached_bytes_do_not_hash_to_content_hash() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("d.md"), b"stable content").expect("fixture");
        seed_clean_read(&pool, root.path(), "d.md", b"stable content").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        let store = ResourceFacts::new(pool.clone());

        // Disk is untouched — only the cached bytes are tampered with, so the
        // hash-vs-row guard (and no other check) must catch it.
        assert!(
            store.store_read_content("d.md", "tampered bytes", &cancel()).await.expect("tamper"),
            "tamper with the cached bytes"
        );
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "d.md"),
                &cancel()
            )
            .await
            .is_none(),
            "cached bytes that do not hash to content_hash are never served"
        );
    }

    #[tokio::test]
    async fn never_serves_content_cached_but_never_hashed_rows() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        let big = vec![b'x'; 80 * 1024];
        std::fs::write(root.path().join("big.bin"), &big).expect("fixture");
        // Seed observed WITHOUT a content hash (bigger than MAX_HASH_BYTES) and
        // attempt to cache the bytes anyway — the hash rule still blocks serve.
        let store = ResourceFacts::new(pool.clone());
        let meta = std::fs::metadata(root.path().join("big.bin")).expect("meta");
        let payload = ToolExecutedPayload {
            agent_id: Some("seeder".to_owned()),
            task_id: None,
            run_id: None,
            tool: "filesystem".to_owned(),
            args: serde_json::json!({}),
            success: true,
            exit_code: None,
            generation: "g1".to_owned(),
            served_from: None,
            paths: vec![ObservedPath {
                path: "big.bin".to_owned(),
                size_bytes: Some(big.len() as u64),
                mtime_ms: mtime_ms(&meta),
                content_hash: None,
            }],
        };
        store
            .apply_observed("ev-1", "seeder", crate::tool_facts::unix_ms(), &payload, &cancel())
            .await
            .expect("observe");
        let content = String::from_utf8_lossy(&big).into_owned();
        assert!(store.store_read_content("big.bin", &content, &cancel()).await.expect("cached"));
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "big.bin"),
                &cancel()
            )
            .await
            .is_none(),
            "a row without a content hash is never served even when bytes are cached"
        );
    }

    #[tokio::test]
    async fn disabled_writer_never_serves_or_caches() {
        let ctx = ToolFactContext::new(None, "ghost");
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("d.md"), b"x").expect("fixture");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "d.md"),
                &cancel()
            )
            .await
            .is_none(),
            "no pool → never served"
        );
        cache_read_output(
            &ctx,
            "filesystem",
            &read_args("read", "d.md"),
            &serde_json::json!({ "content": "x", "path": "d.md" }),
            &cancel(),
        )
        .await;
        // No panic; nothing persisted (no pool to persist to).
    }

    #[tokio::test]
    async fn cache_read_output_caches_exact_read_bytes_for_plain_reads() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("e.md"), b"cache me").expect("fixture");
        seed_clean_read(&pool, root.path(), "e.md", b"cache me").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");

        // Simulate an executed read whose output carries the exact content.
        cache_read_output(
            &ctx,
            "filesystem",
            &read_args("read", "e.md"),
            &serde_json::json!({ "content": "cache me", "path": "e.md" }),
            &cancel(),
        )
        .await;
        let store = ResourceFacts::new(pool.clone());
        let cached = store.cached_read("e.md", &cancel()).await.expect("cached").expect("row");
        assert_eq!(cached.content, "cache me");

        // Non-plain reads (extra keys) are never cached by this helper.
        cache_read_output(
            &ctx,
            "filesystem",
            &serde_json::json!({ "operation": "read", "path": "e.md", "range": [0, 1] }),
            &serde_json::json!({ "content": "cache me", "path": "e.md" }),
            &cancel(),
        )
        .await;
        assert_eq!(
            store.cached_read("e.md", &cancel()).await.expect("cached").expect("row").content,
            "cache me",
            "range reads do not overwrite the cached plain-read bytes"
        );
    }
}
