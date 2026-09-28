use concerto_core::error::PolicyError;
use concerto_core::ids::Ulid;
use concerto_core::traits::policy::{AuditEntry, AuditLog, InfraAuditEntry, RULE_INFRA_FAILURE};
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, SqlitePool};

use crate::{check_cancel, SessionError, SqliteSessionStore};

/// SQLite-backed append-only audit log.
pub struct SqliteAuditLog {
    pool: SqlitePool,
}

impl SqliteAuditLog {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl AuditLog for SqliteAuditLog {
    async fn record(
        &self,
        entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        let created_at_unix = entry.timestamp.unix_timestamp();
        // argv is stored as JSON text (SQLite has no array type).
        let argv_json = entry
            .argv
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()));
        // Path-shaped structured facts are persisted into dedicated nullable
        // TEXT columns. All five are `None` when the tool named no path.
        let (
            path_operation,
            attempted_path,
            resolved_path,
            attempted_destination,
            resolved_destination,
        ) = match entry.path_facts {
            Some(facts) => (
                Some(facts.operation),
                facts.attempted_path,
                facts.resolved_path,
                facts.attempted_destination,
                facts.resolved_destination,
            ),
            None => (None, None, None, None, None),
        };
        // Read-only result summary (migration 035): a bounded, content-free
        // canonical string (`exists=true`, `entries=3`, `bytes=42`) rendered
        // from the typed carrier, or NULL when the operation was mutating,
        // failed, or names no read-only result. Never file content.
        let result_facts = entry.result_facts.map(|facts| facts.to_string());

        sqlx::query(
            "INSERT INTO audit_log (\
                id, session_id, correlation_id, tool_name, verdict, input_hash, \
                rule_matched, user_response, created_at, \
                profile_id, resolved_executable, argv, working_directory, \
                network_requested, filesystem_scope, destructive_classification, \
                exit_code, duration_ms, toolchain_version, plan_id, source_revision, \
                path_operation, attempted_path, resolved_path, \
                attempted_destination, resolved_destination, result_facts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Ulid::new().to_string())
        .bind(entry.session_id.to_string())
        .bind(entry.correlation_id.to_string())
        .bind(&entry.tool_name)
        .bind(&entry.verdict)
        .bind(&entry.input_hash)
        .bind(&entry.rule_matched)
        .bind(&entry.user_response)
        .bind(created_at_unix)
        .bind(entry.profile_id)
        .bind(entry.resolved_executable)
        .bind(argv_json)
        .bind(entry.working_directory)
        .bind(entry.network_requested.map(|b| b as i64))
        .bind(entry.filesystem_scope)
        .bind(entry.destructive_classification)
        .bind(entry.exit_code)
        .bind(entry.duration_ms)
        .bind(entry.toolchain_version)
        .bind(entry.plan_id)
        .bind(entry.source_revision)
        .bind(path_operation)
        .bind(attempted_path)
        .bind(resolved_path)
        .bind(attempted_destination)
        .bind(resolved_destination)
        .bind(result_facts)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("audit log write failed: {e}");
            PolicyError::AuditLogWriteFailed(e.to_string())
        })?;

        Ok(())
    }

    /// Persist an infrastructure-failure row (MCP/plugin).
    ///
    /// Infra rows carry no agent session (they happen at startup, in the MCP
    /// state watcher, or against a plugin that outlives a session), so
    /// `session_id` is written as `NULL`. The synthetic verdict,
    /// [`RULE_INFRA_FAILURE`] sentinel, and the `error_kind` / `server_id` /
    /// `plugin_id` attribution columns are populated. `input_hash` is stored
    /// as an empty string because no tool input is hashed for an infra event.
    async fn record_infra(
        &self,
        entry: InfraAuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        let created_at_unix = entry.timestamp.unix_timestamp();
        sqlx::query(
            "INSERT INTO audit_log (\
                id, session_id, correlation_id, tool_name, verdict, input_hash, \
                rule_matched, user_response, created_at, error_kind, server_id, plugin_id) \
             VALUES (?, NULL, ?, ?, ?, '', ?, ?, ?, ?, ?, ?)",
        )
        .bind(Ulid::new().to_string())
        .bind(entry.correlation_id.to_string())
        .bind(&entry.tool_name)
        .bind(entry.verdict.to_string())
        .bind(RULE_INFRA_FAILURE)
        .bind(&entry.detail)
        .bind(created_at_unix)
        .bind(&entry.error_kind)
        .bind(&entry.server_id)
        .bind(&entry.plugin_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("audit log infra write failed: {e}");
            PolicyError::AuditLogWriteFailed(e.to_string())
        })?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Read path — the audit trail as a sequence
//
// The writer above records the path/operation facts added by migration 034
// and the read-only result facts added by migration 035; this is the
// matching read so those columns are usable without raw sqlite3.
// ---------------------------------------------------------------------------

/// One `audit_log` row, projected for reading back (forensics / display).
///
/// Rows written before migration 034 carry `NULL` in all five path columns
/// and arrive here as `None`; tools that name no path look the same. Both
/// must be rendered honestly — use [`AuditLogRow::has_path_facts`] to mark
/// them instead of silently blanking them. Migration 035's `result_facts`
/// behaves the same way for rows written before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditLogRow {
    /// Audit row id (ULID).
    pub id: String,
    /// Session the decision belongs to (infra rows are `NULL` and are never
    /// returned by [`SqliteSessionStore::load_audit_log`]).
    pub session_id: Option<String>,
    /// Unix seconds. Rows come back ordered by `(created_at, rowid)`.
    pub created_at: i64,
    /// Tool that was evaluated (e.g. `filesystem`, `shell`, `mcp:<id>`).
    pub tool_name: String,
    /// Recorded verdict: `Allow` / `Deny` / `RequireApproval …` for policy
    /// decisions, the [`InfraVerdict`] labels for infrastructure failures.
    ///
    /// [`InfraVerdict`]: concerto_core::traits::policy::InfraVerdict
    pub verdict: String,
    /// Policy rule that produced the verdict (`infra_failure` for infra rows).
    pub rule_matched: Option<String>,
    /// Failure category for infrastructure rows (migration 032).
    pub error_kind: Option<String>,
    /// Tool execution duration in milliseconds, when recorded.
    pub duration_ms: Option<i64>,
    /// Path-shaped operation attempted (`read`, `write`, `move`, …).
    pub path_operation: Option<String>,
    /// Path/URL exactly as supplied by the caller (secrets already stripped).
    pub attempted_path: Option<String>,
    /// Confined absolute path after workspace containment (`NULL` when the
    /// resolution was rejected).
    pub resolved_path: Option<String>,
    /// Destination as supplied (move/copy only).
    pub attempted_destination: Option<String>,
    /// Confined absolute destination (move/copy only).
    pub resolved_destination: Option<String>,
    /// What a successful **read-only** operation returned (migration 035):
    /// `exists=<bool>` / `entries=<count>` / `bytes=<size>`, as stored.
    ///
    /// `NULL` for mutating operations, failed executions, decision-time rows,
    /// and rows written before migration 035. SAFETY: the column holds only a
    /// boolean, a count, or a byte size — never file content, entry names, or
    /// secrets (see `concerto_core::types::ReadResultFacts`).
    pub result_facts: Option<String>,
}

impl AuditLogRow {
    /// Whether any migration-034 path fact is present on this row.
    ///
    /// `false` for rows written before migration 034 *and* for post-034 rows
    /// whose tool named no path — the two are indistinguishable in the data,
    /// so display both as "no path facts recorded".
    pub fn has_path_facts(&self) -> bool {
        self.path_operation.is_some()
            || self.attempted_path.is_some()
            || self.resolved_path.is_some()
            || self.attempted_destination.is_some()
            || self.resolved_destination.is_some()
    }
}

/// Filters for [`SqliteSessionStore::load_audit_log`]. All fields are
/// optional; [`Default`] (nothing filtered) returns the whole trail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditLogFilter {
    /// Exact `tool_name` match (e.g. `filesystem`).
    pub tool: Option<String>,
    /// Exact `path_operation` match (e.g. `read`, `write`, `move`).
    pub operation: Option<String>,
    /// Only entries whose verdict is not `Allow` — `Deny`,
    /// `RequireApproval …`, infrastructure failures.
    pub failed: bool,
    /// Maximum number of rows *after* ordering; `None` returns everything.
    pub limit: Option<u64>,
}

impl SqliteSessionStore {
    /// Read one session's audit trail in deterministic write order.
    ///
    /// Ordered by `(created_at, rowid)`: `rowid` breaks ties inside the same
    /// second, so the result reads as the true sequence of decisions. Only
    /// session-scoped rows match — infrastructure rows carry a `NULL`
    /// `session_id` and are never returned.
    ///
    /// Errors: [`SessionError`] from the query itself, or
    /// `SessionError::Database("operation cancelled")` when `cancel` fires
    /// before the statement runs.
    pub async fn load_audit_log(
        &self,
        session_id: Ulid,
        filter: &AuditLogFilter,
        cancel: CancellationToken,
    ) -> Result<Vec<AuditLogRow>, SessionError> {
        check_cancel(&cancel)?;

        let mut sql = String::from(
            "SELECT id, session_id, created_at, tool_name, verdict, rule_matched, \
             error_kind, duration_ms, path_operation, attempted_path, resolved_path, \
             attempted_destination, resolved_destination, result_facts \
             FROM audit_log WHERE session_id = ?",
        );
        if filter.tool.is_some() {
            sql.push_str(" AND tool_name = ?");
        }
        if filter.operation.is_some() {
            sql.push_str(" AND path_operation = ?");
        }
        if filter.failed {
            sql.push_str(" AND verdict <> 'Allow'");
        }
        sql.push_str(" ORDER BY created_at, rowid");
        if filter.limit.is_some() {
            sql.push_str(" LIMIT ?");
        }

        // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is assembled solely from
        // static fragments; every filter value is bound via `?`.
        let mut query =
            sqlx::query_as::<_, AuditLogRow>(AssertSqlSafe(sql)).bind(session_id.to_string());
        if let Some(tool) = &filter.tool {
            query = query.bind(tool);
        }
        if let Some(operation) = &filter.operation {
            query = query.bind(operation);
        }
        if let Some(limit) = filter.limit {
            query = query.bind(i64::try_from(limit).unwrap_or(i64::MAX));
        }
        Ok(query.fetch_all(&self.pool).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::PolicyEngine;
    use concerto_core::types::{
        CapabilitySet, Condition, PathPolicyFacts, PolicyAction, PolicyRule, PolicyVerdict,
        ReadResultFacts,
    };
    use concerto_core::CancellationToken;
    use std::sync::Arc;

    #[tokio::test]
    async fn sqlite_audit_log_record_writes_row() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(include_str!("../migrations/001_initial_schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/002_audit_log.sql")).execute(&pool).await.unwrap();
        sqlx::query(include_str!("../migrations/016_audit_command_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/024_audit_intent_columns.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/034_audit_path_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/035_audit_result_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();

        // Insert a session row for FK.
        let session_id = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

        let audit = SqliteAuditLog::new(pool.clone());
        let entry = AuditEntry {
            tool_name: "test_tool".into(),
            verdict: "Allow".into(),
            input_hash: "abc".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: Some("auto_approve".into()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
        };
        let result = audit.record(entry, CancellationToken::new()).await;
        assert!(result.is_ok(), "record should succeed: {:?}", result.err());

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 1, "expected one audit log entry");
    }

    #[tokio::test]
    async fn sqlite_audit_log_does_not_panic_inside_runtime() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query(include_str!("../migrations/001_initial_schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/002_audit_log.sql")).execute(&pool).await.unwrap();
        sqlx::query(include_str!("../migrations/016_audit_command_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/024_audit_intent_columns.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/034_audit_path_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/035_audit_result_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();

        // Insert a dummy session row for FK.
        let session_id = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

        let audit = Arc::new(SqliteAuditLog::new(pool.clone()));
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("test_tool".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);

        let input = serde_json::json!({});
        let action = PolicyAction {
            tool_name: "test_tool",
            input: &input,
            session_id,
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
            orchestrator_authority: false,
            path_facts: None,
        };

        let result = engine.evaluate(&action, CancellationToken::new()).await;
        assert!(result.is_ok(), "evaluate should not panic: {:?}", result.err());
        assert_eq!(result.unwrap(), PolicyVerdict::Allow);

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 1, "expected one audit log entry");
    }

    // -----------------------------------------------------------------------
    // New tests added below (8 tests)
    // -----------------------------------------------------------------------

    /// Helper to set up an in-memory pool with the audit log schema and a
    /// dummy session row for FK constraints. Returns (pool, session_id).
    async fn setup_audit_pool() -> (sqlx::SqlitePool, Ulid) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(include_str!("../migrations/001_initial_schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/002_audit_log.sql")).execute(&pool).await.unwrap();
        sqlx::query(include_str!("../migrations/016_audit_command_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/024_audit_intent_columns.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/034_audit_path_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/032_audit_infra_columns.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/035_audit_result_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let sid = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(sid.to_string())
        .execute(&pool)
        .await
        .unwrap();
        (pool, sid)
    }

    #[tokio::test]
    /// SQLite audit log with all `AuditEntry` fields populated.
    async fn sqlite_audit_log_all_fields() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let entry = AuditEntry {
            tool_name: "full_tool".into(),
            verdict: "Deny".into(),
            input_hash: "hash123".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: Some("user said no".into()),
            rule_matched: Some("manual_review".into()),
            profile_id: Some("profile_1".into()),
            resolved_executable: Some("/usr/bin/test".into()),
            argv: Some(vec!["test".into(), "--flag".into()]),
            working_directory: Some("/home/user".into()),
            network_requested: Some(true),
            filesystem_scope: Some("/tmp".into()),
            destructive_classification: Some("modify".into()),
            exit_code: Some(1),
            duration_ms: Some(1500),
            toolchain_version: Some("1.0.0".into()),
            plan_id: Some("01J4V6Q8X000000000000000099".into()),
            source_revision: Some("abc1234".into()),
            path_facts: None,
            result_facts: None,
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 1, "expected one audit log entry with all fields");
    }

    #[tokio::test]
    /// SQLite audit log with nullable fields left as `None`.
    async fn sqlite_audit_log_nullable_fields() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let entry = AuditEntry {
            tool_name: "minimal".into(),
            verdict: "Allow".into(),
            input_hash: "min".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: None,
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 1, "expected one audit log entry with nullable fields");
    }

    #[tokio::test]
    /// Concurrent writes to the SQLite audit log must not cause failures.
    async fn sqlite_audit_log_concurrent_writes() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = Arc::new(SqliteAuditLog::new(pool.clone()));
        let mut handles = Vec::new();
        for i in 0..5 {
            let a = Arc::clone(&audit);
            let sid = session_id;
            handles.push(tokio::spawn(async move {
                let entry = AuditEntry {
                    tool_name: format!("tool_{i}"),
                    verdict: "Allow".into(),
                    input_hash: format!("hash_{i}"),
                    session_id: sid,
                    correlation_id: Ulid::new(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    user_response: None,
                    rule_matched: None,
                    profile_id: None,
                    resolved_executable: None,
                    argv: None,
                    working_directory: None,
                    network_requested: None,
                    filesystem_scope: None,
                    destructive_classification: None,
                    exit_code: None,
                    duration_ms: None,
                    toolchain_version: None,
                    plan_id: None,
                    source_revision: None,
                    path_facts: None,
                    result_facts: None,
                };
                a.record(entry, CancellationToken::new()).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 5, "expected 5 concurrent audit log entries");
    }

    #[tokio::test]
    /// The `argv` field must be serialized as JSON in the database.
    async fn sqlite_audit_log_argv_json_serialization() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let argv = vec!["ls".into(), "-la".into(), "/tmp".into()];
        let entry = AuditEntry {
            tool_name: "argv_test".into(),
            verdict: "Allow".into(),
            input_hash: "argv_hash".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: None,
            profile_id: None,
            resolved_executable: None,
            argv: Some(argv.clone()),
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();
        // Read back the raw argv column and verify it is valid JSON.
        let raw: (String,) =
            sqlx::query_as("SELECT argv FROM audit_log").fetch_one(&pool).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&raw.0).expect("argv must be valid JSON");
        assert_eq!(parsed, argv);
    }

    #[tokio::test]
    /// Schema-derived intent columns (ADR-55 §6) must round-trip:
    /// `plan_id` / `source_revision` land in dedicated columns while the
    /// `user_response` JSON envelope is preserved for replay.
    async fn sqlite_audit_log_intent_columns_round_trip() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let plan_id = "01J4V6Q8X0000000000000000a1";
        let source_revision = "f00dcafe";
        let entry = AuditEntry {
            tool_name: "intent:plan".into(),
            verdict: "apply".into(),
            input_hash: "0123456789abcdef0123456789abcdef".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({
                    "plan_id": plan_id,
                    "source_revision": source_revision,
                })
                .to_string(),
            ),
            rule_matched: Some("apply".into()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: Some(plan_id.into()),
            source_revision: Some(source_revision.into()),
            path_facts: None,
            result_facts: None,
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();

        // The schema-derived columns are populated independently of the
        // JSON envelope.
        let (stored_plan_id, stored_source_revision): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT plan_id, source_revision FROM audit_log")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_plan_id.as_deref(), Some(plan_id));
        assert_eq!(stored_source_revision.as_deref(), Some(source_revision));

        // The envelope remains intact for replay/backward compatibility.
        let raw: (String,) =
            sqlx::query_as("SELECT user_response FROM audit_log").fetch_one(&pool).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw.0).expect("envelope is JSON");
        assert_eq!(parsed["plan_id"], plan_id);
        assert_eq!(parsed["source_revision"], source_revision);
    }

    #[tokio::test]
    /// `InMemoryAuditLog` must preserve insertion order.
    async fn in_memory_audit_log_entry_ordering() {
        let log = crate::testing::InMemoryAuditLog::new();
        let e1 = AuditEntry {
            tool_name: "first".into(),
            verdict: "Allow".into(),
            input_hash: "a".into(),
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: None,
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
        };
        let e2 = AuditEntry { tool_name: "second".into(), ..e1.clone() };
        log.record(e1.clone(), CancellationToken::new()).await.unwrap();
        log.record(e2.clone(), CancellationToken::new()).await.unwrap();
        let entries = log.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tool_name, "first");
        assert_eq!(entries[1].tool_name, "second");
    }

    #[tokio::test]
    /// `InMemoryAuditLog::entry_count` must reflect the actual number of
    /// entries.
    async fn in_memory_audit_log_entry_count_accuracy() {
        let log = crate::testing::InMemoryAuditLog::new();
        assert_eq!(log.entry_count(), 0);
        for i in 0..7 {
            let entry = AuditEntry {
                tool_name: format!("t{i}"),
                verdict: "Allow".into(),
                input_hash: format!("h{i}"),
                session_id: Ulid::new(),
                correlation_id: Ulid::new(),
                timestamp: time::OffsetDateTime::now_utc(),
                user_response: None,
                rule_matched: None,
                profile_id: None,
                resolved_executable: None,
                argv: None,
                working_directory: None,
                network_requested: None,
                filesystem_scope: None,
                destructive_classification: None,
                exit_code: None,
                duration_ms: None,
                toolchain_version: None,
                plan_id: None,
                source_revision: None,
                path_facts: None,
                result_facts: None,
            };
            log.record(entry, CancellationToken::new()).await.unwrap();
        }
        assert_eq!(log.entry_count(), 7);
    }

    #[tokio::test]
    /// `InMemoryAuditLog` must be safe to access from concurrent tasks.
    async fn in_memory_audit_log_thread_safety() {
        use std::sync::Arc;
        let log = Arc::new(crate::testing::InMemoryAuditLog::new());
        let mut handles = Vec::new();
        for i in 0..10 {
            let l = Arc::clone(&log);
            handles.push(tokio::spawn(async move {
                let entry = AuditEntry {
                    tool_name: format!("ct_{i}"),
                    verdict: "Allow".into(),
                    input_hash: format!("ch_{i}"),
                    session_id: Ulid::new(),
                    correlation_id: Ulid::new(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    user_response: None,
                    rule_matched: None,
                    profile_id: None,
                    resolved_executable: None,
                    argv: None,
                    working_directory: None,
                    network_requested: None,
                    filesystem_scope: None,
                    destructive_classification: None,
                    exit_code: None,
                    duration_ms: None,
                    toolchain_version: None,
                    plan_id: None,
                    source_revision: None,
                    path_facts: None,
                    result_facts: None,
                };
                l.record(entry, CancellationToken::new()).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(log.entry_count(), 10);
    }

    #[tokio::test]
    /// Since `AuditEntry` does not derive `PartialEq`, we manually compare
    /// every field for equality.
    async fn audit_entry_partial_eq() {
        let entry = AuditEntry {
            tool_name: "tool".into(),
            verdict: "Allow".into(),
            input_hash: "hash".into(),
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: Some("ok".into()),
            rule_matched: Some("rule".into()),
            profile_id: Some("prof".into()),
            resolved_executable: Some("/bin/sh".into()),
            argv: Some(vec!["sh".into(), "-c".into(), "echo".into()]),
            working_directory: Some("/tmp".into()),
            network_requested: Some(false),
            filesystem_scope: Some("local".into()),
            destructive_classification: Some("read".into()),
            exit_code: Some(0),
            duration_ms: Some(42),
            toolchain_version: Some("1.2.3".into()),
            plan_id: Some("plan_1".into()),
            source_revision: Some("deadbeef".into()),
            path_facts: None,
            result_facts: None,
        };
        let clone = entry.clone();
        // Field-by-field comparison.
        assert_eq!(entry.tool_name, clone.tool_name);
        assert_eq!(entry.verdict, clone.verdict);
        assert_eq!(entry.input_hash, clone.input_hash);
        assert_eq!(entry.session_id, clone.session_id);
        assert_eq!(entry.correlation_id, clone.correlation_id);
        assert_eq!(entry.user_response, clone.user_response);
        assert_eq!(entry.rule_matched, clone.rule_matched);
        assert_eq!(entry.profile_id, clone.profile_id);
        assert_eq!(entry.resolved_executable, clone.resolved_executable);
        assert_eq!(entry.argv, clone.argv);
        assert_eq!(entry.working_directory, clone.working_directory);
        assert_eq!(entry.network_requested, clone.network_requested);
        assert_eq!(entry.filesystem_scope, clone.filesystem_scope);
        assert_eq!(entry.destructive_classification, clone.destructive_classification);
        assert_eq!(entry.exit_code, clone.exit_code);
        assert_eq!(entry.duration_ms, clone.duration_ms);
        assert_eq!(entry.toolchain_version, clone.toolchain_version);
        assert_eq!(entry.plan_id, clone.plan_id);
        assert_eq!(entry.source_revision, clone.source_revision);
    }

    /// Infra-failure rows (MCP/plugin) must land in `audit_log` with a NULL
    /// session id, the synthetic verdict, the `infra_failure` rule sentinel,
    /// and the `error_kind` / `server_id` / `plugin_id` attribution columns.
    ///
    /// Three representative failures are written: a duplicate MCP tool, a
    /// plugin `init` failure (fake `InitFailed(-2)`), and a hash-mismatch grant
    /// prune. One row's `correlation_id` is shared with a `session_events` row
    /// to prove the two tables can be correlated.
    #[tokio::test]
    async fn sqlite_audit_infra_rows() {
        use concerto_core::traits::policy::{InfraAuditEntry, InfraVerdict, RULE_INFRA_FAILURE};

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations apply");

        // A session exists so the correlated `session_events` row can be
        // written; the infra audit rows themselves stay session-less.
        let session_id = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
             VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

        let audit = SqliteAuditLog::new(pool.clone());

        // 1. MCP duplicate-tool collision.
        let duplicate = InfraAuditEntry::mcp(
            "srv-a",
            InfraVerdict::McpServerFailed,
            "duplicate_tool",
            "tool name 'mcp:srv-a:search' already registered",
        );
        let duplicate_correlation = duplicate.correlation_id;
        audit.record_infra(duplicate, CancellationToken::new()).await.unwrap();

        // 2. Plugin init failure with a fake `InitFailed(-2)` code.
        let init_failed = InfraAuditEntry::plugin(
            "plug-init",
            InfraVerdict::PluginLoadFailed,
            "init_failed",
            "plugin init failed with code -2",
        );
        audit.record_infra(init_failed, CancellationToken::new()).await.unwrap();

        // 3. Hash-mismatch grant prune.
        let pruned = InfraAuditEntry::plugin(
            "plug-pruned",
            InfraVerdict::CapabilityDenied,
            "grant_pruned_hash",
            "1 persisted grant pruned: wasm hash mismatch",
        );
        audit.record_infra(pruned, CancellationToken::new()).await.unwrap();

        // Three rows, all session-less, all tagged with the infra sentinel.
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM audit_log \
             WHERE session_id IS NULL AND rule_matched = ?",
        )
        .bind(RULE_INFRA_FAILURE)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count.0, 3, "expected three session-less infra rows");

        // Duplicate-tool row: MCP verdict + server_id, no plugin_id.
        let (verdict, error_kind, server_id, plugin_id, detail): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT verdict, error_kind, server_id, plugin_id, user_response \
             FROM audit_log WHERE tool_name = 'mcp:srv-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(verdict, "McpServerFailed");
        assert_eq!(error_kind.as_deref(), Some("duplicate_tool"));
        assert_eq!(server_id.as_deref(), Some("srv-a"));
        assert_eq!(plugin_id, None);
        assert_eq!(detail.as_deref(), Some("tool name 'mcp:srv-a:search' already registered"));

        // Plugin init-failure row: Wasm verdict attribution in plugin_id.
        let (verdict, error_kind, server_id, plugin_id): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT verdict, error_kind, server_id, plugin_id \
             FROM audit_log WHERE tool_name = 'plugin:plug-init'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(verdict, "PluginLoadFailed");
        assert_eq!(error_kind.as_deref(), Some("init_failed"));
        assert_eq!(server_id, None);
        assert_eq!(plugin_id.as_deref(), Some("plug-init"));

        // Hash-mismatch prune row: CapabilityDenied verdict + prune error kind.
        let (verdict, error_kind, plugin_id): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT verdict, error_kind, plugin_id \
                 FROM audit_log WHERE tool_name = 'plugin:plug-pruned'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(verdict, "CapabilityDenied");
        assert_eq!(error_kind.as_deref(), Some("grant_pruned_hash"));
        assert_eq!(plugin_id.as_deref(), Some("plug-pruned"));

        // Correlated session_events row: same correlation_id joins back to the
        // duplicate-tool audit row.
        sqlx::query(
            "INSERT INTO session_events \
                (id, session_id, sequence_num, correlation_id, event_kind, payload, created_at) \
             VALUES (?, ?, 1, ?, 'PluginStateChanged', '{}', 0)",
        )
        .bind(Ulid::new().to_string())
        .bind(session_id.to_string())
        .bind(duplicate_correlation.to_string())
        .execute(&pool)
        .await
        .unwrap();

        let joined: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM audit_log a \
             JOIN session_events e ON a.correlation_id = e.correlation_id \
             WHERE a.tool_name = 'mcp:srv-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(joined.0, 1, "audit infra row must correlate with its session event");
    }

    /// Helper: a minimal `AuditEntry` with all legacy fields `None` and the
    /// given path facts.
    fn entry_with_path_facts(
        session_id: Ulid,
        tool_name: &str,
        verdict: &str,
        path_facts: Option<PathPolicyFacts>,
    ) -> AuditEntry {
        AuditEntry {
            tool_name: tool_name.into(),
            verdict: verdict.into(),
            input_hash: "hash".into(),
            session_id,
            correlation_id: Ulid::new(),
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: Some("auto_approve".into()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts,
            result_facts: None,
        }
    }

    /// The five nullable path-facts columns, fetched as one row.
    type PathFactsRow =
        (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);

    /// Every path-facts column round-trips through SQLite unchanged.
    #[tokio::test]
    async fn sqlite_audit_log_round_trips_path_facts() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let facts = PathPolicyFacts {
            operation: "move".into(),
            attempted_path: Some("a.txt".into()),
            resolved_path: Some("/proj/a.txt".into()),
            attempted_destination: Some("b.txt".into()),
            resolved_destination: Some("/proj/b.txt".into()),
        };
        audit
            .record(
                entry_with_path_facts(session_id, "filesystem", "Allow", Some(facts.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let row: PathFactsRow = sqlx::query_as(
            "SELECT path_operation, attempted_path, resolved_path, \
                 attempted_destination, resolved_destination FROM audit_log",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("move"));
        assert_eq!(row.1.as_deref(), Some("a.txt"));
        assert_eq!(row.2.as_deref(), Some("/proj/a.txt"));
        assert_eq!(row.3.as_deref(), Some("b.txt"));
        assert_eq!(row.4.as_deref(), Some("/proj/b.txt"));
    }

    /// Migration 035 end to end: each read-only result kind stores its
    /// canonical bounded string, a mutating operation stores NULL, and the
    /// `read` case proves content never reaches the row (only `bytes=…`).
    #[tokio::test]
    async fn sqlite_audit_log_records_read_result_facts() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());

        let cases: [(PathPolicyFacts, Option<ReadResultFacts>, Option<&str>); 6] = [
            (
                PathPolicyFacts { operation: "exists".into(), ..PathPolicyFacts::default() },
                Some(ReadResultFacts::Exists(true)),
                Some("exists=true"),
            ),
            (
                PathPolicyFacts { operation: "exists".into(), ..PathPolicyFacts::default() },
                Some(ReadResultFacts::Exists(false)),
                Some("exists=false"),
            ),
            (
                PathPolicyFacts { operation: "list".into(), ..PathPolicyFacts::default() },
                Some(ReadResultFacts::Entries(0)),
                Some("entries=0"),
            ),
            (
                PathPolicyFacts { operation: "list".into(), ..PathPolicyFacts::default() },
                Some(ReadResultFacts::Entries(3)),
                Some("entries=3"),
            ),
            (
                PathPolicyFacts { operation: "read".into(), ..PathPolicyFacts::default() },
                Some(ReadResultFacts::Bytes(13)),
                Some("bytes=13"),
            ),
            // Mutating operation: no result is ever recorded.
            (
                PathPolicyFacts { operation: "write".into(), ..PathPolicyFacts::default() },
                None,
                None,
            ),
        ];

        // The canonical rendering expected per row, captured before `cases`
        // is consumed by the loop.
        let expected_values: Vec<Option<String>> =
            cases.iter().map(|(_, _, expected)| expected.map(str::to_owned)).collect();

        for (path_facts, result_facts, expected) in cases {
            assert_eq!(
                result_facts.map(|facts| facts.to_string()).as_deref(),
                expected,
                "canonical rendering"
            );
            let entry = AuditEntry {
                result_facts,
                ..entry_with_path_facts(session_id, "filesystem", "Allow", Some(path_facts))
            };
            audit.record(entry, CancellationToken::new()).await.expect("record");
        }

        let stored: Vec<Option<String>> = sqlx::query_as::<_, (Option<String>,)>(
            "SELECT result_facts FROM audit_log ORDER BY rowid",
        )
        .fetch_all(&pool)
        .await
        .expect("result_facts column")
        .into_iter()
        .map(|(value,)| value)
        .collect();
        assert_eq!(
            stored, expected_values,
            "every recorded value is a canonical, content-free scalar"
        );
        // Explicit safety net: no free-form content can be in the column.
        for value in stored.iter().flatten() {
            assert!(value.len() <= 24, "bounded rendering: {value}");
            assert!(value.split('=').count() == 2, "canonical shape: {value}");
        }
    }

    /// The forensic regression: a sequence of filesystem operations is fully
    /// reconstructable from the audit rows alone — which file, which operation,
    /// which verdict, in what order.
    #[tokio::test]
    async fn audit_rows_alone_reconstruct_a_filesystem_operation_sequence() {
        let (pool, session_id) = setup_audit_pool().await;
        // Same rule shape as the live default: reads auto-approve, writes deny.
        let rules = vec![
            PolicyRule::AutoApprove(Condition::All(vec![
                Condition::ToolName("filesystem".into()),
                Condition::Operation("read".into()),
            ])),
            PolicyRule::AutoDeny(Condition::All(vec![
                Condition::ToolName("filesystem".into()),
                Condition::Operation("write".into()),
            ])),
        ];
        let audit = Arc::new(SqliteAuditLog::new(pool.clone()));
        let engine = SimplePolicyEngine::new(rules, audit);

        let operations = [("read", "src/main.rs"), ("write", "src/lib.rs"), ("read", "README.md")];
        for (operation, path) in operations {
            let input = serde_json::json!({"operation": operation, "path": path});
            let facts = PathPolicyFacts {
                operation: operation.into(),
                attempted_path: Some(path.into()),
                resolved_path: Some(format!("/proj/{path}")),
                ..PathPolicyFacts::default()
            };
            let action = PolicyAction {
                tool_name: "filesystem",
                input: &input,
                session_id,
                correlation_id: Ulid::new(),
                capability_requirements: CapabilitySet::default(),
                sandbox_profile: None,
                estimated_cost_usd: None,
                command_facts: None,
                path_facts: Some(facts),
                orchestrator_authority: false,
            };
            // The verdict is not asserted here; the audit rows below are.
            let _ = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        }

        // Reconstruct from the rows alone, in write order.
        let rows: Vec<(Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT path_operation, attempted_path, verdict FROM audit_log ORDER BY rowid",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let reconstructed: Vec<(Option<String>, Option<String>)> =
            rows.iter().map(|(op, path, _)| (op.clone(), path.clone())).collect();
        assert_eq!(
            reconstructed,
            vec![
                (Some("read".into()), Some("src/main.rs".into())),
                (Some("write".into()), Some("src/lib.rs".into())),
                (Some("read".into()), Some("README.md".into())),
            ],
            "the audit rows must name each operation and file, in order"
        );
        assert_eq!(rows[0].2, "Allow", "read auto-approved");
        assert_eq!(rows[1].2, "Deny", "write denied");
        assert_eq!(rows[2].2, "Allow", "read auto-approved");
    }

    /// A secret embedded in a URL query/fragment/userinfo must never reach an
    /// audit row: only scheme+host+path are recorded.
    #[tokio::test]
    async fn audit_row_never_contains_url_secrets() {
        let (pool, session_id) = setup_audit_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());
        let facts = PathPolicyFacts::for_url(
            "request",
            "https://user:pass@api.example.com/v1/completions?api_key=SUPERSECRET#access_token",
        );
        audit
            .record(
                entry_with_path_facts(session_id, "http", "Allow", Some(facts)),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let (path_operation, attempted_path): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT path_operation, attempted_path FROM audit_log")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(path_operation.as_deref(), Some("request"));
        assert_eq!(attempted_path.as_deref(), Some("https://api.example.com/v1/completions"));
        let recorded = attempted_path.unwrap_or_default();
        assert!(!recorded.contains("SUPERSECRET"), "query secret must not be recorded");
        assert!(!recorded.contains("access_token"), "fragment must not be recorded");
        assert!(!recorded.contains("pass"), "userinfo must not be recorded");
    }

    /// Migration 034 applies over a database that already holds pre-034 rows;
    /// legacy rows keep their values with the new columns NULL, and new writes
    /// still succeed.
    #[tokio::test]
    async fn migration_034_preserves_legacy_rows() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        for migration in [
            include_str!("../migrations/001_initial_schema.sql"),
            include_str!("../migrations/002_audit_log.sql"),
            include_str!("../migrations/016_audit_command_facts.sql"),
            include_str!("../migrations/024_audit_intent_columns.sql"),
            include_str!("../migrations/032_audit_infra_columns.sql"),
        ] {
            sqlx::query(migration).execute(&pool).await.unwrap();
        }
        let session_id = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
             VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        // A legacy row written by the pre-034 schema.
        sqlx::query(
            "INSERT INTO audit_log (id, session_id, correlation_id, tool_name, verdict, \
             input_hash, created_at) VALUES (?, ?, ?, 'filesystem', 'Allow', 'legacyhash', 0)",
        )
        .bind(Ulid::new().to_string())
        .bind(session_id.to_string())
        .bind(Ulid::new().to_string())
        .execute(&pool)
        .await
        .unwrap();

        // Applying 034 must not disturb the legacy row.
        sqlx::query(include_str!("../migrations/034_audit_path_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let legacy: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT input_hash, path_operation, attempted_path FROM audit_log")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy.0, "legacyhash", "the legacy input_hash is retained");
        assert_eq!(legacy.1, None);
        assert_eq!(legacy.2, None);

        // Migration 035 completes the writer's column set; it too must apply
        // cleanly over those rows before a post-034 write can land.
        sqlx::query(include_str!("../migrations/035_audit_result_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();

        // And a post-034 write still lands.
        let audit = SqliteAuditLog::new(pool.clone());
        audit
            .record(
                entry_with_path_facts(
                    session_id,
                    "filesystem",
                    "Allow",
                    Some(PathPolicyFacts {
                        operation: "read".into(),
                        attempted_path: Some("x.txt".into()),
                        ..PathPolicyFacts::default()
                    }),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM audit_log").fetch_one(&pool).await.unwrap();
        assert_eq!(count.0, 2);
    }

    /// Migration 035 applies over a database that already holds pre-035 rows:
    /// legacy rows keep their values with `result_facts` NULL, and post-035
    /// writes land the canonical result string.
    #[tokio::test]
    async fn migration_035_preserves_legacy_rows() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        for migration in [
            include_str!("../migrations/001_initial_schema.sql"),
            include_str!("../migrations/002_audit_log.sql"),
            include_str!("../migrations/016_audit_command_facts.sql"),
            include_str!("../migrations/024_audit_intent_columns.sql"),
            include_str!("../migrations/032_audit_infra_columns.sql"),
            include_str!("../migrations/034_audit_path_facts.sql"),
        ] {
            sqlx::query(migration).execute(&pool).await.unwrap();
        }
        let session_id = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
             VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(session_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        // A legacy row written by the pre-035 schema.
        sqlx::query(
            "INSERT INTO audit_log (id, session_id, correlation_id, tool_name, verdict, \
             input_hash, created_at, path_operation, attempted_path) \
             VALUES (?, ?, ?, 'filesystem', 'Allow', 'legacyhash', 0, 'read', 'x.txt')",
        )
        .bind(Ulid::new().to_string())
        .bind(session_id.to_string())
        .bind(Ulid::new().to_string())
        .execute(&pool)
        .await
        .unwrap();

        // Applying 035 must not disturb the legacy row.
        sqlx::query(include_str!("../migrations/035_audit_result_facts.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let legacy: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT input_hash, path_operation, result_facts FROM audit_log")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy.0, "legacyhash", "the legacy input_hash is retained");
        assert_eq!(legacy.1.as_deref(), Some("read"), "pre-035 path facts stay");
        assert_eq!(legacy.2, None, "pre-035 rows simply have no result facts");

        // And a post-035 write lands.
        let audit = SqliteAuditLog::new(pool.clone());
        let entry = AuditEntry {
            result_facts: Some(ReadResultFacts::Entries(0)),
            ..entry_with_path_facts(
                session_id,
                "filesystem",
                "Allow",
                Some(PathPolicyFacts {
                    operation: "list".into(),
                    attempted_path: Some(".".into()),
                    ..PathPolicyFacts::default()
                }),
            )
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();
        let stored: Vec<Option<String>> = sqlx::query_as::<_, (Option<String>,)>(
            "SELECT result_facts FROM audit_log ORDER BY rowid",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(value,)| value)
        .collect();
        assert_eq!(stored, vec![None, Some("entries=0".to_owned())]);
    }

    // -----------------------------------------------------------------------
    // Read path (SqliteSessionStore::load_audit_log)
    // -----------------------------------------------------------------------

    /// In-memory pool with the *full* migration set plus one session row, so
    /// the read path is exercised against the real schema (columns 002/016/
    /// 024/032/034 included).
    async fn setup_audit_read_pool() -> (sqlx::SqlitePool, Ulid) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let sid = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
             VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(sid.to_string())
        .execute(&pool)
        .await
        .unwrap();
        (pool, sid)
    }

    /// A store over `pool` for the read path (test-only construction: the
    /// fields are crate-private and this module is inside the crate).
    fn read_store(pool: &sqlx::SqlitePool) -> SqliteSessionStore {
        SqliteSessionStore { pool: pool.clone(), _data_dir_lock: None, at_rest_key: None }
    }

    /// Run `load_audit_log` once against `pool` and unwrap the rows.
    async fn load_trail(
        pool: &sqlx::SqlitePool,
        session_id: Ulid,
        filter: AuditLogFilter,
    ) -> Vec<AuditLogRow> {
        read_store(pool)
            .load_audit_log(session_id, &filter, CancellationToken::new())
            .await
            .unwrap()
    }

    /// `AuditEntry` stamped with a fixed unix timestamp for ordering tests.
    fn entry_at(
        session_id: Ulid,
        tool_name: &str,
        verdict: &str,
        timestamp: i64,
        path_facts: Option<PathPolicyFacts>,
    ) -> AuditEntry {
        AuditEntry {
            timestamp: time::OffsetDateTime::from_unix_timestamp(timestamp)
                .expect("test timestamp within range"),
            ..entry_with_path_facts(session_id, tool_name, verdict, path_facts)
        }
    }

    /// The read path returns every path column, ordered by
    /// `(created_at, rowid)` — the oldest decision first, ties broken by
    /// insertion order, so the result reads as the true sequence.
    #[tokio::test]
    async fn load_audit_log_returns_path_columns_in_write_order() {
        let (pool, session_id) = setup_audit_read_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());

        // Two rows share `created_at` (rowid decides); one is older.
        let entries = [
            (
                1_000i64,
                PathPolicyFacts {
                    operation: "read".into(),
                    attempted_path: Some("a.txt".into()),
                    resolved_path: Some("/proj/a.txt".into()),
                    ..PathPolicyFacts::default()
                },
            ),
            (
                1_000,
                PathPolicyFacts {
                    operation: "write".into(),
                    attempted_path: Some("b.txt".into()),
                    resolved_path: Some("/proj/b.txt".into()),
                    ..PathPolicyFacts::default()
                },
            ),
            (
                500,
                PathPolicyFacts {
                    operation: "move".into(),
                    attempted_path: Some("c.txt".into()),
                    resolved_path: Some("/proj/c.txt".into()),
                    attempted_destination: Some("d.txt".into()),
                    resolved_destination: Some("/proj/d.txt".into()),
                },
            ),
        ];
        for (timestamp, facts) in entries {
            audit
                .record(
                    entry_at(session_id, "filesystem", "Allow", timestamp, Some(facts)),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let store = read_store(&pool);
        let rows = store
            .load_audit_log(session_id, &AuditLogFilter::default(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(rows.len(), 3, "all three entries are returned");
        // Oldest first; the two same-second rows keep insertion order.
        assert_eq!(rows[0].created_at, 500);
        assert_eq!(rows[1].created_at, 1_000);
        assert_eq!(rows[2].created_at, 1_000);

        let sequence: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row.path_operation.as_deref().unwrap_or_default(),
                    row.attempted_path.as_deref().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            sequence,
            vec![("move", "c.txt"), ("read", "a.txt"), ("write", "b.txt")],
            "the trail must read as the true sequence of operations"
        );

        // Every requested column arrives.
        let last = &rows[0];
        assert_eq!(last.tool_name, "filesystem");
        assert_eq!(last.verdict, "Allow");
        assert_eq!(last.rule_matched.as_deref(), Some("auto_approve"));
        assert_eq!(last.attempted_destination.as_deref(), Some("d.txt"));
        assert_eq!(last.resolved_destination.as_deref(), Some("/proj/d.txt"));
        assert_eq!(last.resolved_path.as_deref(), Some("/proj/c.txt"));
        assert!(last.has_path_facts());
        let expected_session = session_id.to_string();
        assert!(rows
            .iter()
            .all(|row| row.session_id.as_deref() == Some(expected_session.as_str())));
    }

    /// A row written with the pre-034 shape (NULL path columns) reads back
    /// as `None` in every path field and is flagged by `has_path_facts()`
    /// so callers can mark it instead of blanking it; a post-034 row with
    /// facts is not flagged.
    #[tokio::test]
    async fn load_audit_log_marks_rows_without_path_facts() {
        let (pool, session_id) = setup_audit_read_pool().await;

        // Legacy row: the exact column set a pre-034 writer left behind.
        sqlx::query(
            "INSERT INTO audit_log (id, session_id, correlation_id, tool_name, verdict, \
             input_hash, rule_matched, created_at, duration_ms) \
             VALUES (?, ?, ?, 'filesystem', 'Allow', 'legacyhash', 'auto_approve', 42, 7)",
        )
        .bind(Ulid::new().to_string())
        .bind(session_id.to_string())
        .bind(Ulid::new().to_string())
        .execute(&pool)
        .await
        .unwrap();
        // Post-034 row that carries path facts.
        let audit = SqliteAuditLog::new(pool.clone());
        audit
            .record(
                entry_at(
                    session_id,
                    "filesystem",
                    "Deny",
                    43,
                    Some(PathPolicyFacts {
                        operation: "write".into(),
                        attempted_path: Some("x.txt".into()),
                        ..PathPolicyFacts::default()
                    }),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let store = read_store(&pool);
        let rows = store
            .load_audit_log(session_id, &AuditLogFilter::default(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        let legacy = &rows[0];
        assert_eq!(legacy.created_at, 42);
        assert!(!legacy.has_path_facts(), "legacy rows must be flagged");
        assert_eq!(legacy.path_operation, None);
        assert_eq!(legacy.attempted_path, None);
        assert_eq!(legacy.resolved_path, None);
        assert_eq!(legacy.attempted_destination, None);
        assert_eq!(legacy.resolved_destination, None);
        // The non-path columns of the legacy row survive intact.
        assert_eq!(legacy.verdict, "Allow");
        assert_eq!(legacy.rule_matched.as_deref(), Some("auto_approve"));
        assert_eq!(legacy.duration_ms, Some(7));

        let fresh = &rows[1];
        assert!(fresh.has_path_facts());
        assert_eq!(fresh.path_operation.as_deref(), Some("write"));
        assert_eq!(fresh.attempted_path.as_deref(), Some("x.txt"));
    }

    /// The read path carries `result_facts` back out: a read-only row returns
    /// its canonical string, a mutating row and a pre-035 row both read `None`
    /// (indistinguishable at this seam, which is honest — neither learned a
    /// result).
    #[tokio::test]
    async fn load_audit_log_returns_result_facts() {
        let (pool, session_id) = setup_audit_read_pool().await;

        // Pre-035 legacy row: written without the column at all.
        sqlx::query(
            "INSERT INTO audit_log (id, session_id, correlation_id, tool_name, verdict, \
             input_hash, rule_matched, created_at, path_operation) \
             VALUES (?, ?, ?, 'filesystem', 'Allow', 'legacyhash', 'auto_approve', 44, 'read')",
        )
        .bind(Ulid::new().to_string())
        .bind(session_id.to_string())
        .bind(Ulid::new().to_string())
        .execute(&pool)
        .await
        .unwrap();

        let audit = SqliteAuditLog::new(pool.clone());
        // A read-only row with a recorded result.
        let read_row = AuditEntry {
            result_facts: Some(ReadResultFacts::Bytes(13)),
            ..entry_at(
                session_id,
                "filesystem",
                "Allow",
                45,
                Some(PathPolicyFacts {
                    operation: "read".into(),
                    attempted_path: Some("a.txt".into()),
                    ..PathPolicyFacts::default()
                }),
            )
        };
        audit.record(read_row, CancellationToken::new()).await.unwrap();
        // A mutating row: no result is recorded.
        let write_row = entry_at(
            session_id,
            "filesystem",
            "Allow",
            46,
            Some(PathPolicyFacts {
                operation: "write".into(),
                attempted_path: Some("b.txt".into()),
                ..PathPolicyFacts::default()
            }),
        );
        audit.record(write_row, CancellationToken::new()).await.unwrap();

        let rows = load_trail(&pool, session_id, AuditLogFilter::default()).await;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].result_facts, None, "pre-035 row");
        assert_eq!(rows[1].result_facts.as_deref(), Some("bytes=13"), "read-only row");
        assert_eq!(rows[2].result_facts, None, "mutating row");
        assert_eq!(rows[1].path_operation.as_deref(), Some("read"));
    }

    /// `--tool`, `--operation`, `--failed` and `--limit` each narrow the
    /// trail, and the query never leaks another session's rows.
    #[tokio::test]
    async fn load_audit_log_filters_by_tool_operation_verdict_and_limit() {
        let (pool, session_id) = setup_audit_read_pool().await;
        let audit = SqliteAuditLog::new(pool.clone());

        let cases = [
            ("filesystem", "Allow", 100i64, "read"),
            ("filesystem", "Deny", 101, "write"),
            ("http", "Deny", 102, "request"),
        ];
        for (tool, verdict, timestamp, operation) in cases {
            audit
                .record(
                    entry_at(
                        session_id,
                        tool,
                        verdict,
                        timestamp,
                        Some(PathPolicyFacts {
                            operation: operation.into(),
                            attempted_path: Some(format!("{operation}.txt")),
                            ..PathPolicyFacts::default()
                        }),
                    ),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        // A fourth row in a *different* session must never leak into this
        // session's trail.
        let other_session = Ulid::new();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, project_dir, provider, model) \
             VALUES (?, 0, '/tmp', 'test', 'test')",
        )
        .bind(other_session.to_string())
        .execute(&pool)
        .await
        .unwrap();
        audit
            .record(
                entry_at(
                    other_session,
                    "filesystem",
                    "Allow",
                    99,
                    Some(PathPolicyFacts {
                        operation: "read".into(),
                        attempted_path: Some("other.txt".into()),
                        ..PathPolicyFacts::default()
                    }),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Unfiltered: this session only, oldest first.
        let all = load_trail(&pool, session_id, AuditLogFilter::default()).await;
        assert_eq!(all.len(), 3, "other sessions must not leak in");
        assert_eq!(all.iter().map(|r| r.created_at).collect::<Vec<_>>(), vec![100, 101, 102]);

        // --tool
        let by_tool = load_trail(
            &pool,
            session_id,
            AuditLogFilter { tool: Some("filesystem".into()), ..AuditLogFilter::default() },
        )
        .await;
        assert_eq!(by_tool.len(), 2);
        assert!(by_tool.iter().all(|row| row.tool_name == "filesystem"));

        // --operation
        let by_operation = load_trail(
            &pool,
            session_id,
            AuditLogFilter { operation: Some("write".into()), ..AuditLogFilter::default() },
        )
        .await;
        assert_eq!(by_operation.len(), 1);
        assert_eq!(by_operation[0].attempted_path.as_deref(), Some("write.txt"));

        // --failed: everything that did not receive an Allow verdict.
        let failed = load_trail(
            &pool,
            session_id,
            AuditLogFilter { failed: true, ..AuditLogFilter::default() },
        )
        .await;
        assert_eq!(failed.len(), 2);
        assert!(failed.iter().all(|row| row.verdict == "Deny"));
        assert_eq!(failed[0].created_at, 101);

        // --limit
        let limited = load_trail(
            &pool,
            session_id,
            AuditLogFilter { limit: Some(1), ..AuditLogFilter::default() },
        )
        .await;
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].created_at, 100, "limit keeps the oldest rows");

        // Filters compose.
        let combined = load_trail(
            &pool,
            session_id,
            AuditLogFilter {
                tool: Some("filesystem".into()),
                failed: true,
                ..AuditLogFilter::default()
            },
        )
        .await;
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].path_operation.as_deref(), Some("write"));

        // A filter that matches nothing returns an empty trail, not an error.
        let nothing = load_trail(
            &pool,
            session_id,
            AuditLogFilter { tool: Some("no-such-tool".into()), ..AuditLogFilter::default() },
        )
        .await;
        assert!(nothing.is_empty());
    }
}
