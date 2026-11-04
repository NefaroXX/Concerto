//! Session-scoped read provenance. Never infer a restored version from disk.

use std::path::Path;

use concerto_core::CancellationToken;

use super::{path_keys, read_observation, written_version, MAX_OBSERVATIONS, REREAD};
use crate::gate::{versioned_targets, GateError, GateRequest, WriteGate};

impl WriteGate {
    pub(in crate::gate) async fn record_read_version(
        &self,
        req: &GateRequest,
        data: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<(), GateError> {
        if cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        let Some((path, version)) = read_observation(req, data) else { return Ok(()) };
        if req.session_id.is_none() {
            self.read_versions
                .lock()
                .map_err(|_| provenance_error("read ledger poisoned"))?
                .record_read(&self.project_root, req, data);
            return Ok(());
        }
        self.persist_read_versions(req, &[(path.to_owned(), version)], cancel).await
    }

    /// A failed lookup is a missing-provenance claim, never permission to
    /// recapture a possibly newer disk version.
    pub(in crate::gate) async fn observed_version(
        &self,
        req: &GateRequest,
        path: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        if cancel.is_cancelled() {
            return Some(REREAD.into());
        }
        let Some(session) = req.session_id.as_deref() else {
            return match self.read_versions.lock() {
                Ok(versions) => versions.observed(&self.project_root, req, path),
                Err(_) => Some(REREAD.into()),
            };
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Some(REREAD.into()),
            result = self.load_read_version(req, session, path) => result,
        };
        match result {
            Ok(version) => version,
            Err(error) => {
                tracing::warn!(%error, "read provenance unavailable; requiring a fresh read");
                Some(REREAD.into())
            }
        }
    }

    async fn load_read_version(
        &self,
        req: &GateRequest,
        session: &str,
        path: &str,
    ) -> Result<Option<String>, GateError> {
        let root = root_key(&self.project_root)?;
        for key in stored_paths(&self.project_root, path)? {
            let version = sqlx::query_scalar::<_, String>(
                "SELECT version FROM gate_read_versions WHERE project_root = ? \
                 AND session_id = ? AND agent_id = ? AND path = ?",
            )
            .bind(&root)
            .bind(session)
            .bind(&req.agent_id)
            .bind(key)
            .fetch_optional(&self.log_pool)
            .await
            .map_err(provenance_error)?;
            if version.is_some() {
                return Ok(version);
            }
        }
        let unavailable = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM gate_read_scopes WHERE project_root = ? \
             AND session_id = ? AND saturated = 1) OR EXISTS(SELECT 1 FROM \
             gate_read_legacy_sessions WHERE session_id = ?)",
        )
        .bind(&root)
        .bind(session)
        .bind(session)
        .fetch_one(&self.log_pool)
        .await
        .map_err(provenance_error)?;
        Ok(unavailable.then(|| REREAD.into()))
    }

    pub(in crate::gate) async fn record_write_versions(
        &self,
        req: &GateRequest,
        staged: bool,
        cancel: &CancellationToken,
    ) -> Result<(), GateError> {
        if req.session_id.is_none() {
            self.read_versions
                .lock()
                .map_err(|_| provenance_error("read ledger poisoned after write"))?
                .record_write(&self.project_root, req, staged);
            return Ok(());
        }
        let mut updates = Vec::new();
        for path in versioned_targets(req) {
            if self.observed_version(req, &path, cancel).await.is_some() {
                let version = written_version(req, &path, staged);
                updates.push((path, version));
            }
        }
        // A failed update leaves the prior claim intact. It may require a
        // reread after a completed write, but cannot bless unobserved bytes.
        self.persist_read_versions(req, &updates, cancel).await
    }

    async fn persist_read_versions(
        &self,
        req: &GateRequest,
        updates: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<(), GateError> {
        if updates.is_empty() {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(GateError::Cancelled),
            result = self.persist_read_versions_inner(req, updates) => result,
        }
    }

    async fn persist_read_versions_inner(
        &self,
        req: &GateRequest,
        updates: &[(String, String)],
    ) -> Result<(), GateError> {
        let session = req.session_id.as_deref().ok_or_else(|| provenance_error("no session"))?;
        let root = root_key(&self.project_root)?;
        let mut tx = self.log_pool.begin().await.map_err(provenance_error)?;
        // Acquire the SQLite write lock before inspecting capacity. Concurrent
        // observations cannot each reserve the last slot based on stale counts.
        sqlx::query(
            "INSERT INTO gate_read_scopes (project_root, session_id) VALUES (?, ?) \
             ON CONFLICT(project_root, session_id) DO UPDATE SET saturated = saturated",
        )
        .bind(&root)
        .bind(session)
        .execute(&mut *tx)
        .await
        .map_err(provenance_error)?;
        for (path, version) in updates {
            for key in stored_paths(&self.project_root, path)? {
                persist_one(&mut tx, &root, session, &req.agent_id, &key, version).await?;
            }
        }
        tx.commit().await.map_err(provenance_error)
    }
}

async fn persist_one(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    root: &str,
    session: &str,
    agent: &str,
    path: &str,
    version: &str,
) -> Result<(), GateError> {
    let saved = sqlx::query(
        "INSERT INTO gate_read_versions (project_root, session_id, agent_id, path, version) \
         SELECT ?, ?, ?, ?, ? WHERE EXISTS(SELECT 1 FROM gate_read_versions \
         WHERE project_root = ? AND session_id = ? AND agent_id = ? AND path = ?) \
         OR (SELECT COUNT(*) FROM gate_read_versions WHERE project_root = ? AND session_id = ?) < ? \
         ON CONFLICT(project_root, session_id, agent_id, path) DO UPDATE SET version = excluded.version",
    )
    .bind(root).bind(session).bind(agent).bind(path).bind(version)
    .bind(root).bind(session).bind(agent).bind(path)
    .bind(root).bind(session).bind(MAX_OBSERVATIONS as i64)
    .execute(&mut **tx)
    .await
    .map_err(provenance_error)?;
    if saved.rows_affected() == 0 {
        sqlx::query(
            "UPDATE gate_read_scopes SET saturated = 1 WHERE project_root = ? AND session_id = ?",
        )
        .bind(root)
        .bind(session)
        .execute(&mut **tx)
        .await
        .map_err(provenance_error)?;
    }
    Ok(())
}

fn stored_paths(root: &Path, path: &str) -> Result<Vec<String>, GateError> {
    path_keys(root, path)
        .into_iter()
        .map(|key| {
            key.into_os_string().into_string().map_err(|_| provenance_error("non-UTF-8 path"))
        })
        .collect()
}

fn root_key(root: &Path) -> Result<String, GateError> {
    let resolved = root.canonicalize().map_err(provenance_error)?;
    let utf8 = camino::Utf8Path::from_path(&resolved)
        .ok_or_else(|| provenance_error("non-UTF-8 project root"))?;
    Ok(concerto_tools::common::path_comparison_key(utf8))
}

fn provenance_error(error: impl std::fmt::Display) -> GateError {
    GateError::Execution(format!("read-version persistence failed: {error}; reread before editing"))
}
