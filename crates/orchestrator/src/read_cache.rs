//! ADR-65 §4 — safe read deduplication (serve-from-cache).
//!
//! A plain single-path filesystem read that has already been observed (clean
//! row in `resource_facts`, within the **current project root's scope**) can be
//! answered from the cache instead of invoking the executor — BUT only when it
//! can be proven the cache still matches the current disk. The **never-stale**
//! rule is absolute: on any doubt the tool executes normally.
//!
//! Every serve must satisfy ALL of these predicates (see [`maybe_serve_read`]):
//! 1. the guarded arguments are exactly a plain single-path read (no
//!    range/glob/recursive/options — see [`plain_single_path_read`]);
//! 2. the path canonicalizes to a key inside the project root (ADR-65 F5d) and
//!    a clean row exists for it under the root's `project_root_hash` (F5c);
//! 3. a fresh `stat` of the resolved path NOW matches the row's `size_bytes`
//!    and `mtime_ms` (never trusting the watcher — re-statted per serve);
//! 4. cached content is present AND `blake3(content) == content_hash`.
//!
//! The final gate lives at the call site, not here: `maybe_serve_read` proves
//! the cache matches the disk, and the loop then re-runs the policy engine
//! through the **advisory** path (ADR-65 F1a) and serves only on an explicit
//! `Allow`, recording a `ServedFromCache` audit row (F1b). A non-`Allow`
//! verdict falls through to normal execution, so a served read is never a
//! policy-engine bypass.

use std::path::Path;

use concerto_core::CancellationToken;

use crate::tool_facts::ToolFactContext;

// The serve-decision cluster (the plain-read argument predicate plus the four
// never-stale serve predicates), extracted to `read_cache/serve.rs` with the
// failure tests that pin its edges; re-exported so every existing
// `crate::read_cache::{...}` path keeps resolving unchanged.
mod serve;
pub use serve::{maybe_serve_read, plain_single_path_read};

/// After a successful plain filesystem read, cache its exact output content so
/// an identical later read can be served. The observation row already exists
/// (created by `record_tool_fact` → `apply_observed`); this only attaches the
/// content bytes under the project root's scope (ADR-65 F5c). Fail-soft and
/// bounded — see `ToolFactContext::cache_read_content`.
pub async fn cache_read_output(
    facts: &ToolFactContext,
    project_root: &Path,
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
    facts.cache_read_content(project_root, path, content, cancel).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use concerto_core::CancellationToken;
    use concerto_sessions::{ObservedPath, ResourceFacts, ToolExecutedPayload};

    use super::*;
    use crate::tool_facts::project_root_hash;

    pub(super) fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    pub(super) async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
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

    pub(super) fn mtime_ms(meta: &std::fs::Metadata) -> Option<u64> {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    }

    /// Write `bytes` to `root/<path>` and seed a clean, content-cached
    /// observation matching it, scoped under that root (ADR-65 F5c) — the
    /// state under which a serve should succeed.
    pub(super) async fn seed_clean_read(
        pool: &sqlx::SqlitePool,
        root: &Path,
        path: &str,
        bytes: &[u8],
    ) {
        let file = root.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("fixture dir");
        }
        std::fs::write(&file, bytes).expect("fixture write");
        let meta = std::fs::metadata(&file).expect("fixture metadata");
        let store = ResourceFacts::new(pool.clone());
        let root_hash = project_root_hash(root);
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
        assert!(store
            .store_read_content(&root_hash, path, &content, &cancel())
            .await
            .expect("cached"));
    }

    pub(super) fn read_args(operation: &str, path: &str) -> serde_json::Value {
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
    async fn never_serves_when_cached_bytes_do_not_hash_to_content_hash() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("d.md"), b"stable content").expect("fixture");
        seed_clean_read(&pool, root.path(), "d.md", b"stable content").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");
        let store = ResourceFacts::new(pool.clone());

        // Disk is untouched — only the cached bytes are tampered with, so the
        // hash-vs-row guard (and no other check) must catch it.
        let root_hash = project_root_hash(root.path());
        assert!(
            store
                .store_read_content(&root_hash, "d.md", "tampered bytes", &cancel())
                .await
                .expect("tamper"),
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
            root.path(),
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
            root.path(),
            "filesystem",
            &read_args("read", "e.md"),
            &serde_json::json!({ "content": "cache me", "path": "e.md" }),
            &cancel(),
        )
        .await;
        let store = ResourceFacts::new(pool.clone());
        let root_hash = project_root_hash(root.path());
        let cached =
            store.cached_read(&root_hash, "e.md", &cancel()).await.expect("cached").expect("row");
        assert_eq!(cached.content, "cache me");

        // Non-plain reads (extra keys) are never cached by this helper.
        cache_read_output(
            &ctx,
            root.path(),
            "filesystem",
            &serde_json::json!({ "operation": "read", "path": "e.md", "range": [0, 1] }),
            &serde_json::json!({ "content": "cache me", "path": "e.md" }),
            &cancel(),
        )
        .await;
        assert_eq!(
            store
                .cached_read(&root_hash, "e.md", &cancel())
                .await
                .expect("cached")
                .expect("row")
                .content,
            "cache me",
            "range reads do not overwrite the cached plain-read bytes"
        );
    }

    #[tokio::test]
    async fn never_serves_a_row_observed_under_another_project_root() {
        let (_dir, pool) = test_pool().await;
        let root_a = tempfile::tempdir().expect("root a");
        let root_b = tempfile::tempdir().expect("root b");
        // The same relative path exists in BOTH roots with identical bytes; a
        // clean, content-cached row exists for it ONLY under root A.
        std::fs::create_dir_all(root_b.path().join("sub")).expect("dir b");
        std::fs::write(root_b.path().join("sub/a.md"), b"shared").expect("fixture b");
        seed_clean_read(&pool, root_a.path(), "sub/a.md", b"shared").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");

        // Reading the path under root B must NOT serve the row that was
        // observed under root A — that row's stat refers to a different file.
        assert!(
            maybe_serve_read(
                &ctx,
                root_b.path(),
                "filesystem",
                &read_args("read", "sub/a.md"),
                &cancel()
            )
            .await
            .is_none(),
            "a row scoped under another project root must never serve (ADR-65 F5c)"
        );

        // And the same read under root A (its own scope) serves as normal.
        assert!(
            maybe_serve_read(
                &ctx,
                root_a.path(),
                "filesystem",
                &read_args("read", "sub/a.md"),
                &cancel()
            )
            .await
            .is_some(),
            "the row serves within its own project root's scope"
        );
    }

    /// F5d: lexical spellings of the SAME canonical file serve — dot segments
    /// collapse and the absolute form strips the root prefix. The canonical
    /// key (not the raw spelling) decides, so `./docs/note.md`,
    /// `docs/./note.md`, `docs/x/../note.md`, and the absolute path all map to
    /// the observed key `docs/note.md`.
    #[tokio::test]
    async fn serves_canonical_spellings_within_the_root() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("docs")).expect("dir");
        std::fs::write(root.path().join("docs/note.md"), b"canonical").expect("fixture");
        seed_clean_read(&pool, root.path(), "docs/note.md", b"canonical").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");

        let spellings = ["./docs/note.md", "docs/./note.md", "docs/x/../note.md", "docs//note.md"];
        for spelling in spellings {
            let serve = maybe_serve_read(
                &ctx,
                root.path(),
                "filesystem",
                &read_args("read", spelling),
                &cancel(),
            )
            .await
            .expect("spelling resolves to the observed canonical key");
            assert_eq!(serve.path, "docs/note.md", "served under the canonical key: {spelling}");
            assert_eq!(serve.content, "canonical");
        }

        let absolute = root.path().join("docs/note.md").to_string_lossy().into_owned();
        let serve = maybe_serve_read(
            &ctx,
            root.path(),
            "filesystem",
            &read_args("read", &absolute),
            &cancel(),
        )
        .await
        .expect("the absolute within-root spelling serves");
        assert_eq!(serve.path, "docs/note.md");
        assert_eq!(serve.content, "canonical");
    }

    /// F5d: any spelling that escapes the project root — `..` that pops past
    /// the root boundary or an absolute path under a different root — yields no
    /// canonical key and is never served (falls through to normal execution).
    #[tokio::test]
    async fn never_serves_traversal_or_out_of_root_spellings() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(root.path().join("note.md"), b"inside").expect("fixture");
        std::fs::write(outside.path().join("secret.md"), b"outside").expect("fixture outside");
        seed_clean_read(&pool, root.path(), "note.md", b"inside").await;
        let ctx = ToolFactContext::new(Some(pool.clone()), "reader");

        let outside_abs = outside.path().join("secret.md").to_string_lossy().into_owned();
        for spelling in ["../outside.md", "../secret.md", "a/../../secret.md", &outside_abs] {
            assert!(
                maybe_serve_read(
                    &ctx,
                    root.path(),
                    "filesystem",
                    &read_args("read", spelling),
                    &cancel()
                )
                .await
                .is_none(),
                "a path escaping the project root is never served (F5d): {spelling}"
            );
        }
    }
}
