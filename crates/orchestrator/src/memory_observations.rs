//! H11 observation seam: the `resource_facts`-backed
//! [`PathObservationSource`](concerto_core::memory::PathObservationSource)
//! that lets evidence-bearing recall verify or stale an item from runtime
//! observations.
//!
//! The memory crate cannot see the sessions pool, so this adapter lives
//! here (a service seam, not coordinator logic): it wraps a `ResourceFacts`
//! handle bound to one project root and translates rows into the honesty
//! bits recall derives from. Attached post-construction through
//! [`MemoryStore::attach_path_observations`](concerto_core::traits::memory::MemoryStore::attach_path_observations)
//! — no constructor signature changes anywhere. `run_shared_agent`
//! attaches it on every run that has a sessions DB pool open, so CLI and
//! desktop RUNS get the observation axis alike; the desktop's separate
//! pre-init path (`loaders.rs`) never attaches — it only indexes and
//! re-indexes, never recalls — and a recall from a handle that skipped the
//! attach reads the axis as unknown (never as fine).
//!
//! Key-space contract (ADR-65 F5c/F5d): lookups are scoped by the blake3
//! project-root hash ([`crate::tool_facts::project_root_hash`], the same
//! function the observation producers use) and keyed by the canonical
//! project-relative path ([`crate::tool_facts::canonical_project_path`],
//! the same normalization the whiteboard inventory uses). A recalled path
//! that does not normalize into this project's key space yields no entry
//! (unknown), never a cross-project or cross-root row.
//!
//! Fail-open throughout: an unparseable store, a lookup error, a missing
//! row, or a cancelled token yields absent entries — recall reads those
//! paths as unknown, and a fully cancelled recall additionally marks its
//! composites interrupted (see the memory crate's enrichment).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use concerto_core::memory::{PathObservation, PathObservationSource};
use concerto_core::CancellationToken;
use concerto_sessions::resource_facts::ResourceFacts;

/// `resource_facts` rows for one project root, served as recall standing.
pub struct ResourceFactObservations {
    facts: ResourceFacts,
    project_root: PathBuf,
    project_root_hash: String,
}

impl ResourceFactObservations {
    /// Bind the seam to the sessions pool's `resource_facts` table scoped
    /// to `project_dir`. All sessions migrations (including 029–031) must
    /// already have been applied to the pool — the run's session DB
    /// qualifies; a foreign pool simply yields no rows (fail-open).
    pub fn new(pool: sqlx::SqlitePool, project_dir: &Path) -> Self {
        let project_root_hash = crate::tool_facts::project_root_hash(project_dir);
        Self {
            facts: ResourceFacts::new(pool),
            project_root: project_dir.to_path_buf(),
            project_root_hash,
        }
    }
}

#[async_trait]
impl PathObservationSource for ResourceFactObservations {
    async fn observe_paths(
        &self,
        file_paths: &[String],
        cancel: CancellationToken,
    ) -> HashMap<String, PathObservation> {
        let mut observations = HashMap::new();
        for raw in file_paths {
            if cancel.is_cancelled() {
                break;
            }
            // Into this project's key space, or not at all.
            let Some(canonical) =
                crate::tool_facts::canonical_project_path(&self.project_root, raw)
            else {
                continue;
            };
            let row = match self.facts.lookup(&self.project_root_hash, &canonical, &cancel).await {
                Ok(row) => row,
                Err(_) => continue, // fail-open: unknown, never an error
            };
            if let Some(row) = row {
                observations.insert(
                    raw.clone(),
                    PathObservation {
                        dirty: row.dirty,
                        generation: Some(row.generation),
                        last_event_id: row.last_event_id,
                    },
                );
            }
        }
        observations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_sessions::resource_facts::{ObservedPath, ToolExecutedPayload};

    /// File-backed sessions pool with all sessions migrations applied —
    /// the same shape the run's session DB has.
    async fn sessions_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("sessions.db");
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("sessions pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn observed(path: &str) -> ToolExecutedPayload {
        ToolExecutedPayload {
            agent_id: Some("agent-a".to_owned()),
            task_id: None,
            run_id: None,
            tool: "read".to_owned(),
            args: serde_json::json!({ "path": path }),
            success: true,
            exit_code: Some(0),
            generation: "gen-1".to_owned(),
            project_root_hash: crate::tool_facts::project_root_hash(Path::new("/proj")),
            paths: vec![ObservedPath {
                path: path.to_owned(),
                size_bytes: Some(10),
                mtime_ms: Some(1_000),
                content_hash: Some("h".to_owned()),
            }],
            served_from: None,
            canonical_tool: None,
            canonical_operation: None,
            failure: None,
        }
    }

    #[tokio::test]
    async fn seam_serves_clean_rows_and_skips_missing_paths() {
        let (_dir, pool) = sessions_pool().await;
        let facts = ResourceFacts::new(pool.clone());
        let payload = observed("src/a.rs");
        facts
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &payload,
                &CancellationToken::new(),
            )
            .await
            .expect("observe");

        let seam = ResourceFactObservations::new(pool, Path::new("/proj"));
        let found = seam
            .observe_paths(
                &["src/a.rs".to_string(), "ghost.rs".to_string()],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(found.len(), 1, "only the observed path answers");
        let observation = &found["src/a.rs"];
        assert!(!observation.dirty);
        assert_eq!(observation.generation.as_deref(), Some("gen-1"));
        assert_eq!(observation.last_event_id.as_deref(), Some("ev-1"));
    }

    #[tokio::test]
    async fn seam_scopes_to_its_project_root() {
        let (_dir, pool) = sessions_pool().await;
        let facts = ResourceFacts::new(pool.clone());
        let payload = observed("src/a.rs");
        facts
            .apply_observed(
                "ev-1",
                "agent-a",
                1_700_000_000_000,
                &payload,
                &CancellationToken::new(),
            )
            .await
            .expect("observe");

        // A different project root never sees another root's rows, even
        // for the same relative path.
        let other = ResourceFactObservations::new(pool, Path::new("/elsewhere"));
        let found = other.observe_paths(&["src/a.rs".to_string()], CancellationToken::new()).await;
        assert!(found.is_empty(), "cross-root rows never served");
    }

    #[tokio::test]
    async fn seam_cancelled_token_yields_no_observations() {
        let (_dir, pool) = sessions_pool().await;
        let seam = ResourceFactObservations::new(pool, Path::new("/proj"));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let found = seam.observe_paths(&["src/a.rs".to_string()], cancelled).await;
        assert!(found.is_empty(), "cancelled seam serves nothing (unknown downstream)");
    }
}
