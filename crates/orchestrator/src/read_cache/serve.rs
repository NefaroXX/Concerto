//! Serve-from-cache decision predicates (ADR-65 §4) — the *value-in /
//! value-out* half of the read cache.
//!
//! This module owns the serve decision: the plain-read argument predicate
//! ([`plain_single_path_read`]) and [`maybe_serve_read`], which enforces all
//! four **never-stale** rules before a cached read may be served — the rules
//! are listed in the parent module's docs. Nothing here *writes* cache state:
//! the parent's `cache_read_output` attaches content bytes to an existing
//! observation row. Call sites must check advisory `Allow` before invoking
//! this disk-reading probe and record `ServedFromCache` after a hit (see
//! `crate::agent_loop`), so a served read is never a policy-engine bypass.
//!
//! A matching timestamp and size are only cheap rejection checks. Serving
//! also requires byte equality with the actual contained path the filesystem
//! tool would read; lexical resource keys are not filesystem identities.

use std::path::Path;

use concerto_core::CancellationToken;
use concerto_sessions::{CachedRead, ResourceFacts};
use tokio::io::AsyncReadExt;

use crate::tool_facts::{canonical_project_path, mtime_ms, project_root_hash, ToolFactContext};

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
/// Callers must obtain advisory policy Allow before this reads disk bytes.
///
/// Resource keys retain their lexical spelling for lookup, but the requested
/// path and the cache key must resolve to the same contained file under the
/// filesystem tool's rules. A bounded read then verifies current disk bytes.
/// Rows from a different project root (including legacy unscoped rows) never
/// serve. This observes bytes at read time; it does not lock out later edits.
pub async fn maybe_serve_read(
    facts: &ToolFactContext,
    project_root: &Path,
    tool: &str,
    args: &serde_json::Value,
    cancel: &CancellationToken,
) -> Option<ReadServe> {
    let pool = facts.pool()?;
    let raw_path = plain_single_path_read(tool, args)?;
    let path = canonical_project_path(project_root, raw_path)?;
    let root_hash = project_root_hash(project_root);

    let store = ResourceFacts::new(pool.clone());
    // Rule 2 + cached content (rule 4's first half): a row that is observed,
    // clean, and actually content-cached. Any of these failing → execute.
    let cached = store.cached_read(&root_hash, &path, cancel).await.ok()??;
    // Defense in depth: the scoped query already filters, but a row that ever
    // carried another root's identity must never be served (legacy "" rows are
    // preserved for attribution only — ADR-65 F5c).
    if cached.row.project_root_hash != root_hash {
        return None;
    }
    if cached.row.dirty {
        return None;
    }
    let content_hash = cached.row.content_hash.as_deref()?;

    // Rule 4's second half: the cached bytes must hash to the observed hash.
    // Refuse corrupted or oversized rows before doing any filesystem work.
    if cached.content.len() > concerto_sessions::resource_facts::CACHE_LIMIT_BYTES
        || blake3::hash(cached.content.as_bytes()).to_hex().to_string() != content_hash
    {
        return None;
    }

    current_file_matches(project_root, raw_path, &path, &cached, cancel).await?;
    if cancel.is_cancelled() {
        return None;
    }
    cached.row.last_event_id.map(|event_id| ReadServe { path, content: cached.content, event_id })
}

/// Resolve like the tool, then compare a bounded read through one file handle.
/// Metadata cannot establish byte freshness: editors may preserve timestamps.
async fn current_file_matches(
    project_root: &Path,
    raw_path: &str,
    cache_path: &str,
    cached: &CachedRead,
    cancel: &CancellationToken,
) -> Option<()> {
    let root = camino::Utf8PathBuf::from_path_buf(project_root.to_path_buf()).ok()?;
    let raw_path = camino::Utf8PathBuf::from(raw_path);
    let cache_path = camino::Utf8PathBuf::from(cache_path);
    let resolved = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        result = tokio::task::spawn_blocking(move || {
            use concerto_tools::common::{path_comparison_key, resolve_path};
            let requested = resolve_path(&root, &raw_path).ok()?;
            let observed = resolve_path(&root, &cache_path).ok()?;
            if path_comparison_key(&requested) != path_comparison_key(&observed)
                || !requested.metadata().ok()?.is_file()
            {
                return None;
            }
            Some(requested)
        }) => result.ok()??,
    };
    let read = async {
        let file = tokio::fs::File::open(resolved).await.ok()?;
        let meta = file.metadata().await.ok()?;
        let expected_len = cached.content.len();
        if !meta.is_file()
            || meta.len() != expected_len as u64
            || cached.row.size_bytes != Some(meta.len())
            || cached.row.mtime_ms != mtime_ms(&meta)
        {
            return None;
        }
        // One extra byte detects growth after stat without an unbounded read.
        let mut bytes = Vec::with_capacity(expected_len + 1);
        file.take(expected_len as u64 + 1).read_to_end(&mut bytes).await.ok()?;
        (bytes == cached.content.as_bytes()).then_some(())
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = read => result,
    }
}

#[cfg(test)]
mod tests {
    use concerto_sessions::{ObservedPath, ResourceFacts, ToolExecutedPayload};

    use super::*;
    // The shared fixtures (`test_pool`, `seed_clean_read`, `read_args`, …)
    // stay in the parent's test module — marked `pub(super)` — so the serve
    // tests here and the cache-write tests there use one definition instead
    // of duplicated scaffolding.
    use super::super::tests::{cancel, mtime_ms, read_args, seed_clean_read, test_pool};

    #[tokio::test]
    async fn never_serves_same_size_external_edit_with_restored_mtime() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("observed.txt");
        seed_clean_read(&pool, root.path(), "observed.txt", b"old bytes").await;
        let before = std::fs::metadata(&path).unwrap();
        let modified = before.modified().unwrap();
        std::fs::write(&path, b"new bytes").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(mtime_ms(&before), mtime_ms(&after));
        let ctx = ToolFactContext::new(Some(pool), "reader");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "observed.txt"),
                &cancel(),
            )
            .await
            .is_none(),
            "matching stat metadata does not prove cached bytes still match disk"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_serves_a_different_file_after_rewriting_a_literal_backslash() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        seed_clean_read(&pool, root.path(), "dir/file.txt", b"cached").await;
        std::fs::write(root.path().join(r"dir\file.txt"), b"actual").unwrap();
        let ctx = ToolFactContext::new(Some(pool), "reader");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", r"dir\file.txt"),
                &cancel(),
            )
            .await
            .is_none(),
            "Unix backslashes name a different file, not a directory separator"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_serves_a_different_file_after_collapsing_a_symlink_parent() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().unwrap();
        seed_clean_read(&pool, root.path(), "dir/file.txt", b"cached").await;
        std::fs::create_dir_all(root.path().join("other/child")).unwrap();
        std::fs::write(root.path().join("other/file.txt"), b"actual").unwrap();
        std::os::unix::fs::symlink(root.path().join("other/child"), root.path().join("dir/link"))
            .unwrap();
        let raw = "dir/link/../file.txt";
        assert_eq!(std::fs::read(root.path().join(raw)).unwrap(), b"actual");
        let ctx = ToolFactContext::new(Some(pool), "reader");
        assert!(maybe_serve_read(
            &ctx,
            root.path(),
            "filesystem",
            &read_args("read", raw),
            &cancel()
        )
        .await
        .is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_serves_after_a_path_becomes_a_symlink_escape() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        seed_clean_read(&pool, root.path(), "file.txt", b"same").await;
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let target = outside.path().join("file.txt");
        std::fs::write(&target, b"same").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let ctx = ToolFactContext::new(Some(pool), "reader");
        assert!(
            maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "file.txt"),
                &cancel()
            )
            .await
            .is_none(),
            "cache hits must obey the tool's containment rule even when bytes and metadata match"
        );
    }

    #[tokio::test]
    async fn serves_exact_current_bytes_at_cache_boundaries_and_honors_cancellation() {
        let (_dir, pool) = test_pool().await;
        for content in [
            String::new(),
            "雪🦀".to_owned(),
            "x".repeat(concerto_sessions::resource_facts::CACHE_LIMIT_BYTES),
        ] {
            let root = tempfile::tempdir().unwrap();
            seed_clean_read(&pool, root.path(), "file.txt", content.as_bytes()).await;
            let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
            let served = maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "file.txt"),
                &cancel(),
            )
            .await
            .unwrap();
            assert_eq!(served.content, content);
            let stopped = cancel();
            stopped.cancel();
            assert!(maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", "file.txt"),
                &stopped
            )
            .await
            .is_none());
        }
    }

    #[tokio::test]
    async fn never_serves_dirty_or_missing_rows() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("b.md"), b"dirty content").expect("fixture");
        seed_clean_read(&pool, root.path(), "b.md", b"dirty content").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        let store = ResourceFacts::new(pool.clone());
        store.mark_dirty(&project_root_hash(root.path()), "b.md", &cancel()).await.expect("dirty");
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
    async fn never_serves_content_cached_but_never_hashed_rows() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        // Content stays within the cache bound so the byte cache is attachable;
        // the row still carries NO content hash — that alone must block serve.
        std::fs::write(root.path().join("big.bin"), b"observed but never hashed").expect("fixture");
        // Seed observed WITHOUT a content hash and cache the bytes anyway — the
        // hash rule still blocks serve.
        let store = ResourceFacts::new(pool.clone());
        let root_hash = project_root_hash(root.path());
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
            project_root_hash: root_hash.clone(),
            served_from: None,
            canonical_tool: None,
            canonical_operation: None,
            failure: None,
            paths: vec![ObservedPath {
                path: "big.bin".to_owned(),
                size_bytes: Some(meta.len()),
                mtime_ms: mtime_ms(&meta),
                content_hash: None,
            }],
        };
        store
            .apply_observed("ev-1", "seeder", crate::tool_facts::unix_ms(), &payload, &cancel())
            .await
            .expect("observe");
        let content = "observed but never hashed".to_owned();
        assert!(
            store
                .store_read_content(&root_hash, "big.bin", &content, &cancel())
                .await
                .expect("cached"),
            "the byte cache attaches (content within the cache bound)"
        );
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
}
