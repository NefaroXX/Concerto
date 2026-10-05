//! Shared audited native-console setup for human clients. No provider is needed.
use crate::{PolicyExecutionAdapter, ShellContext, ShellRuntime};
use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_config::AppConfig;
use concerto_core::traits::{PolicyEngine, Tool};
use concerto_core::types::{
    CapabilitySet, PathPolicyFacts, SessionContext, ToolOutput, ToolRegistry,
};
use concerto_core::{
    ApprovalSink, CancellationToken, PolicyPresets, SimplePolicyEngine, ToolError, ToolExecutor,
};
use concerto_sessions::{SessionStore, SqliteSessionStore};
use concerto_tools::{filesystem::FilesystemTool, native_process::NativeProcessTool};
use serde_json::Value;
use std::sync::Arc;

pub async fn open(
    project: std::path::PathBuf,
    config: AppConfig,
    approval: Arc<dyn ApprovalSink>,
) -> Result<Arc<ShellRuntime>, String> {
    config.shell_security.validate()?;
    let project = project.canonicalize().map_err(|e| e.to_string())?;
    let root =
        Utf8PathBuf::from_path_buf(project.clone()).map_err(|_| "project path must be UTF-8")?;
    let store = SqliteSessionStore::connect_shared().await.map_err(|e| e.to_string())?;
    let session = store
        .create_session(&root, "concerto", "native-shell", CancellationToken::new())
        .await
        .map_err(|e| e.to_string())?;
    let rules = config
        .policy
        .as_ref()
        .map(|p| p.to_rules())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(PolicyPresets::default_rules);
    let policy = SimplePolicyEngine::new(rules, store.audit_sink())
        .with_shell_security(config.shell_security.clone(), project.clone())
        .with_protected_config(concerto_config::default_config_path());
    policy.validate().map_err(|e| e.to_string())?;
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(NativeProcessTool::new(config.shell_security.clone())));
    registry.register(Box::new(ClientFilesystem { root: root.clone() }));
    let executor = Arc::new(
        ToolExecutor::new(Arc::new(registry), Arc::new(policy)).with_approval_sink(approval),
    );
    let adapter = PolicyExecutionAdapter::new(executor, SessionContext::new(session.id, project));
    ShellRuntime::native(ShellContext::new(root), adapter, config.shell_security.history_enabled)
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// Interactive operations use the existing filesystem tool after policy approval.
/// A fresh overlay per call avoids retaining file contents in the console.
struct ClientFilesystem {
    root: Utf8PathBuf,
}
#[async_trait]
impl Tool for ClientFilesystem {
    fn name(&self) -> &str {
        "filesystem"
    }
    fn description(&self) -> &str {
        "Native project file operations"
    }
    fn input_schema(&self) -> Value {
        FilesystemTool::new(self.root.clone()).input_schema()
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default().with_requirement("filesystem")
    }
    fn path_facts(&self, input: &Value, session: &SessionContext) -> Option<PathPolicyFacts> {
        FilesystemTool::new(self.root.clone()).path_facts(input, session)
    }
    async fn execute(
        &self,
        input: Value,
        policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        FilesystemTool::new(self.root.clone()).execute(input, policy, session, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::error::PolicyError;
    use concerto_core::shell_security::{ShellPermission, ShellSecurity};
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::types::{Condition, PolicyRule};

    struct Audit;
    #[async_trait]
    impl AuditLog for Audit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    fn runtime(root: &std::path::Path, security: ShellSecurity) -> ShellRuntime {
        let root_utf8 = Utf8PathBuf::from_path_buf(root.to_owned()).expect("UTF-8 project");
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(ClientFilesystem { root: root_utf8.clone() }));
        let policy = SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(Audit),
        )
        .with_shell_security(security, root.to_owned());
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), Arc::new(policy)));
        let session = SessionContext::new(concerto_core::ids::Ulid::new(), root.to_owned());
        ShellRuntime::native(
            ShellContext::new(root_utf8),
            PolicyExecutionAdapter::new(executor, session),
            false,
        )
        .expect("native runtime")
    }

    // verifies: native filesystem results and committed content work without a process exit code or interpreter.
    #[tokio::test]
    async fn native_file_commands_share_policy_and_return_success() {
        let directory = tempfile::tempdir().expect("project");
        let security = ShellSecurity { writes: ShellPermission::Policy, ..Default::default() };
        let runtime = runtime(directory.path(), security);
        for line in [
            "write source.txt 'hello world'",
            "cp source.txt copy.txt",
            "mv copy.txt moved.txt",
            "cat moved.txt",
            "ls .",
            "rm moved.txt",
        ] {
            let result = runtime.execute_line(line, CancellationToken::new()).await;
            assert!(result.status.is_success(), "{line}: {result:?}");
        }
        assert_eq!(
            std::fs::read_to_string(directory.path().join("source.txt")).expect("read"),
            "hello world"
        );
        assert!(!directory.path().join("moved.txt").exists());
    }

    // verifies: deny/ask ceilings block allow-all file writes without consent; protected files stay unreadable.
    #[tokio::test]
    async fn file_commands_cannot_bypass_security_ceiling() {
        let directory = tempfile::tempdir().expect("project");
        std::fs::write(directory.path().join(".env"), "secret").expect("fixture");
        for writes in [ShellPermission::Deny, ShellPermission::Ask] {
            let runtime = runtime(directory.path(), ShellSecurity { writes, ..Default::default() });
            for line in ["write changed.txt content", "cat .env", "cp .env copy.txt"] {
                let result = runtime.execute_line(line, CancellationToken::new()).await;
                let expected = if writes == ShellPermission::Ask && line.starts_with("write ") {
                    crate::CommandStatus::AwaitingApproval
                } else {
                    crate::CommandStatus::Blocked
                };
                assert_eq!(result.status, expected, "{line}: {result:?}");
            }
        }
        assert!(!directory.path().join("changed.txt").exists());
        assert!(!directory.path().join("copy.txt").exists());
    }

    // verifies: cancellation before dispatch cannot commit files.
    #[tokio::test]
    async fn cancelled_file_command_has_no_effect() {
        let directory = tempfile::tempdir().expect("project");
        let runtime = runtime(
            directory.path(),
            ShellSecurity { writes: ShellPermission::Policy, ..Default::default() },
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = runtime.execute_line("write changed.txt content", cancel).await;
        assert_eq!(result.status, crate::CommandStatus::Cancelled);
        assert!(!directory.path().join("changed.txt").exists());
    }

    // verifies: supervised read fast paths honor user protection even when ordinary write rules are advisory.
    #[tokio::test]
    async fn read_fast_path_still_enforces_user_security() {
        let directory = tempfile::tempdir().expect("project");
        std::fs::write(directory.path().join(".env"), "secret").expect("secret");
        std::fs::write(directory.path().join("public.txt"), "public").expect("public");
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(
            Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8"),
        )));
        let policy =
            SimplePolicyEngine::new(vec![PolicyRule::AutoDeny(Condition::Always)], Arc::new(Audit))
                .with_shell_security(ShellSecurity::default(), directory.path().to_owned());
        let executor = ToolExecutor::new(Arc::new(registry), Arc::new(policy));
        let session =
            SessionContext::new(concerto_core::ids::Ulid::new(), directory.path().to_owned());
        let secret = executor
            .execute_read_only(
                "filesystem",
                serde_json::json!({"operation":"read","path":".env"}),
                &session,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(secret, Err(ToolError::PolicyDenied { .. })));
        let public = executor
            .execute_read_only(
                "filesystem",
                serde_json::json!({"operation":"read","path":"public.txt"}),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("ordinary reads retain fast-path behavior");
        assert_eq!(public.data["content"], "public");
    }
}
