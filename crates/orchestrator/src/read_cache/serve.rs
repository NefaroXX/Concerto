//! Serve-from-cache decision predicates (ADR-65 §4) — the *value-in /
//! value-out* half of the read cache.
//!
//! This module owns the serve decision: the plain-read argument predicate
//! ([`plain_single_path_read`]) and [`maybe_serve_read`], which enforces all
//! four **never-stale** rules before a cached read may be served — the rules
//! are listed in the parent module's docs. Nothing here *writes* cache state:
//! the parent's `cache_read_output` attaches content bytes to an existing
//! observation row, and the advisory `Allow` gate plus the `ServedFromCache`
//! audit row stay at the call sites (ADR-65 F1a/F1b, see
//! `crate::agent_loop`), so a served read is never a policy-engine bypass.
//!
//! Stability contract: the four predicates and their check order are
//! observable behavior — serving must remain impossible on any doubt. The
//! function bodies move verbatim from `read_cache.rs`. The failure tests
//! below pin the missing-row, stat-divergence (stale), and partial-observation
//! (never-hashed) edges.

use std::path::Path;

use concerto_core::CancellationToken;
use concerto_sessions::ResourceFacts;

use crate::tool_facts::{
    canonical_project_path, mtime_ms, project_root_hash, resolve_path, ToolFactContext,
};

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
/// The serve re-stats the same resolved path the observation hashed by reducing
/// the tool-reported path to its **canonical project-relative key** first
/// (ADR-65 F5d) and then reusing [`crate::tool_facts::resolve_path`],
/// guaranteeing `rule (3)` compares the row against the exact path that
/// produced it. The lookup is scoped under the project root's
/// `project_root_hash` (F5c): a row observed under a *different* root — or a
/// legacy pre-scoping row (`project_root_hash == ""`) — never serves.
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

    let resolved = resolve_path(project_root, &path);
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

    cached.row.last_event_id.map(|event_id| ReadServe { path, content: cached.content, event_id })
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
