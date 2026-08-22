use concerto_core::error::PolicyError;
use concerto_core::ids::Ulid;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::CancellationToken;
use sqlx::SqlitePool;

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

        sqlx::query(
            "INSERT INTO audit_log (\
                id, session_id, correlation_id, tool_name, verdict, input_hash, \
                rule_matched, user_response, created_at, \
                profile_id, resolved_executable, argv, working_directory, \
                network_requested, filesystem_scope, destructive_classification, \
                exit_code, duration_ms, toolchain_version, plan_id, source_revision) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("audit log write failed: {e}");
            PolicyError::AuditLogWriteFailed(e.to_string())
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::PolicyEngine;
    use concerto_core::types::{CapabilitySet, Condition, PolicyAction, PolicyRule, PolicyVerdict};
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
        };
        audit.record(entry, CancellationToken::new()).await.unwrap();
        // Read back the raw argv column and verify it is valid JSON.
        let raw: (String,) =
            sqlx::query_as("SELECT argv FROM audit_log").fetch_one(&pool).await.unwrap();
        let parsed: Vec<String> = serde_json::from_str(&raw.0).expect("argv must be valid JSON");
        assert_eq!(parsed, argv);
    }

    #[tokio::test]
    /// Schema-derived intent columns (ADR-55 Phase 1d §4) must round-trip:
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
}
