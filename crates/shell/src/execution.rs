use std::sync::Arc;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::executor::ToolExecutor;
use concerto_core::types::SessionContext;
use concerto_core::{CancellationToken, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::path::{resolve_path_in_root, PathError};
use crate::{
    profile_is_available, CommandEffect, CommandInvocation, CommandResult, CommandServices,
    CommandSource, CommandSpec, CommandStatus, Diagnostic, ShellCommand, ShellContext,
    ShellProfileCatalog,
};

const MAX_TIMEOUT_SECS: u64 = 300;

/// Concrete request sent through Concerto's existing policy-gated shell tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalExecutionRequest {
    pub program: String,
    pub arguments: Vec<String>,
    pub cwd: Utf8PathBuf,
    pub timeout_secs: Option<u64>,
}

/// Adapter from typed shell commands to the central [`ToolExecutor`].
///
/// The adapter cannot execute a process directly. Policy evaluation, approval,
/// auditing, tool-level deny rules, project sandboxing, cancellation, and the
/// actual process spawn all occur behind `ToolExecutor::execute`.
#[derive(Clone)]
pub struct PolicyExecutionAdapter {
    executor: Arc<ToolExecutor>,
    session: SessionContext,
}

impl PolicyExecutionAdapter {
    #[must_use]
    pub fn new(executor: Arc<ToolExecutor>, session: SessionContext) -> Self {
        Self { executor, session }
    }

    #[must_use]
    pub fn project_dir(&self) -> &std::path::Path {
        &self.session.project_dir
    }

    pub(crate) async fn execute(
        &self,
        result_command: &str,
        request: ExternalExecutionRequest,
        cancel: CancellationToken,
    ) -> CommandResult {
        let input = json!({
            "command": request.program,
            "args": request.arguments,
            "cwd": request.cwd,
            "timeout_secs": request.timeout_secs,
        });

        match self.executor.execute("shell", input, &self.session, cancel).await {
            Ok(output) => tool_output_result(result_command, output),
            Err(error) => tool_error_result(result_command, error),
        }
    }
}

pub(crate) fn external_commands(
    adapter: PolicyExecutionAdapter,
    profiles: ShellProfileCatalog,
) -> Vec<Arc<dyn ShellCommand>> {
    let adapter = Arc::new(adapter);
    let profiles = Arc::new(profiles);
    vec![
        Arc::new(RunCommand::new(adapter.clone())),
        Arc::new(ShellRunCommand::new(adapter, profiles.clone())),
        Arc::new(ShellProfilesCommand::new(profiles)),
    ]
}

struct RunCommand {
    spec: CommandSpec,
    adapter: Arc<PolicyExecutionAdapter>,
}

impl RunCommand {
    fn new(adapter: Arc<PolicyExecutionAdapter>) -> Self {
        Self {
            spec: CommandSpec {
                name: "run".to_owned(),
                description: "Run an executable through Concerto policy".to_owned(),
                usage: "run [--cwd PATH] [--timeout SECONDS] [--] PROGRAM [ARG ...]".to_owned(),
                source: CommandSource::External,
                effects: vec![CommandEffect::ProcessSpawn],
                records_history: true,
            },
            adapter,
        }
    }
}

#[async_trait]
impl ShellCommand for RunCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        context: &ShellContext,
        _services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult {
        let options = match parse_execution_options(&invocation.arguments, false) {
            Ok(options) => options,
            Err(message) => return usage_failure(&self.spec, &message),
        };
        let Some((program, arguments)) = options.operands.split_first() else {
            return usage_failure(&self.spec, "run requires a program");
        };
        let cwd = match resolve_cwd(context, options.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => return path_error_result(&self.spec.name, error),
        };
        let request = ExternalExecutionRequest {
            program: program.clone(),
            arguments: arguments.to_vec(),
            cwd,
            timeout_secs: options.timeout_secs,
        };
        self.adapter.execute(&self.spec.name, request, cancel).await
    }
}

struct ShellRunCommand {
    spec: CommandSpec,
    adapter: Arc<PolicyExecutionAdapter>,
    profiles: Arc<ShellProfileCatalog>,
}

impl ShellRunCommand {
    fn new(adapter: Arc<PolicyExecutionAdapter>, profiles: Arc<ShellProfileCatalog>) -> Self {
        Self {
            spec: CommandSpec {
                name: "shell-run".to_owned(),
                description: "Run a script with a selected shell profile through policy".to_owned(),
                usage: "shell-run [--profile ID] [--cwd PATH] [--timeout SECONDS] [--] SCRIPT"
                    .to_owned(),
                source: CommandSource::External,
                effects: vec![CommandEffect::ProcessSpawn],
                records_history: true,
            },
            adapter,
            profiles,
        }
    }
}

#[async_trait]
impl ShellCommand for ShellRunCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        context: &ShellContext,
        _services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult {
        let options = match parse_execution_options(&invocation.arguments, true) {
            Ok(options) => options,
            Err(message) => return usage_failure(&self.spec, &message),
        };
        if options.operands.is_empty() {
            return usage_failure(&self.spec, "shell-run requires a script");
        }
        let script = options.operands.join(" ");
        let profile = match options.profile.as_deref() {
            Some(id) => self.profiles.get(id),
            None => self.profiles.selected(),
        };
        let Some(profile) = profile else {
            return CommandResult::recoverable(
                &self.spec.name,
                "shell.profile.unavailable",
                "the selected shell profile is unavailable",
            )
            .with_suggestion("Run `shell-profiles` and choose an available profile.");
        };
        if !profile_is_available(profile) {
            return CommandResult::recoverable(
                &self.spec.name,
                "shell.profile.program-not-found",
                format!(
                    "shell profile `{}` cannot resolve program `{}`",
                    profile.id,
                    profile.resolve_executable().display()
                ),
            )
            .with_suggestion("Update the profile program or select another shell profile.");
        }

        let cwd = match resolve_cwd(context, options.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => return path_error_result(&self.spec.name, error),
        };
        let request = ExternalExecutionRequest {
            program: profile.resolve_executable().to_string_lossy().into_owned(),
            arguments: profile.command_args(&script),
            cwd,
            timeout_secs: options.timeout_secs,
        };
        self.adapter.execute(&self.spec.name, request, cancel).await
    }
}

struct ShellProfilesCommand {
    spec: CommandSpec,
    profiles: Arc<ShellProfileCatalog>,
}

impl ShellProfilesCommand {
    fn new(profiles: Arc<ShellProfileCatalog>) -> Self {
        Self {
            spec: CommandSpec {
                name: "shell-profiles".to_owned(),
                description: "List configured and discovered shell profiles".to_owned(),
                usage: "shell-profiles".to_owned(),
                source: CommandSource::Builtin,
                effects: Vec::new(),
                records_history: false,
            },
            profiles,
        }
    }
}

#[async_trait]
impl ShellCommand for ShellProfilesCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        _context: &ShellContext,
        _services: &CommandServices,
        _cancel: CancellationToken,
    ) -> CommandResult {
        if !invocation.arguments.is_empty() {
            return usage_failure(&self.spec, "shell-profiles does not accept arguments");
        }

        let selected_id = self.profiles.selected().map(|profile| profile.id.clone());
        let profiles = self
            .profiles
            .profiles()
            .into_iter()
            .map(|profile| {
                json!({
                    "id": profile.id,
                    "name": profile.name,
                    "program": profile.resolve_executable(),
                    "backend": profile.backend,
                    "available": profile_is_available(profile),
                    "selected": selected_id.as_deref() == Some(profile.id.as_str()),
                })
            })
            .collect::<Vec<_>>();
        let available = profiles
            .iter()
            .filter(|profile| {
                profile.get("available").and_then(|value| value.as_bool()) == Some(true)
            })
            .count();

        CommandResult::new(
            &self.spec.name,
            CommandStatus::Succeeded,
            format!("Found {available} available shell profile(s)"),
        )
        .with_data(json!({ "profiles": profiles, "selected_profile": selected_id }))
    }
}

#[derive(Debug)]
struct ExecutionOptions {
    cwd: Option<String>,
    timeout_secs: Option<u64>,
    profile: Option<String>,
    operands: Vec<String>,
}

fn parse_execution_options(
    arguments: &[String],
    allow_profile: bool,
) -> Result<ExecutionOptions, String> {
    let mut cwd = None;
    let mut timeout_secs = None;
    let mut profile = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            index += 1;
            break;
        }
        if !argument.starts_with('-') {
            break;
        }

        match argument.as_str() {
            "--cwd" => cwd = Some(option_value(arguments, &mut index, "--cwd")?),
            "--timeout" => {
                let value = option_value(arguments, &mut index, "--timeout")?;
                timeout_secs = Some(parse_timeout(&value)?);
            }
            "--profile" if allow_profile => {
                profile = Some(option_value(arguments, &mut index, "--profile")?);
            }
            "--profile" => return Err("--profile is valid only for shell-run".to_owned()),
            _ => return Err(format!("unknown option `{argument}`")),
        }
        index += 1;
    }

    Ok(ExecutionOptions { cwd, timeout_secs, profile, operands: arguments[index..].to_vec() })
}

fn option_value(arguments: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    *index += 1;
    arguments.get(*index).cloned().ok_or_else(|| format!("{option} requires a value"))
}

fn parse_timeout(value: &str) -> Result<u64, String> {
    let timeout = value.parse::<u64>().map_err(|_| format!("invalid timeout `{value}`"))?;
    if !(1..=MAX_TIMEOUT_SECS).contains(&timeout) {
        return Err(format!("timeout must be between 1 and {MAX_TIMEOUT_SECS} seconds"));
    }
    Ok(timeout)
}

/// Resolve the effective working directory for process execution.
///
/// A `--cwd` request is confined to the project root by
/// [`resolve_path_in_root`]: `..` traversal, absolute paths outside the root,
/// and symlink escapes are rejected before the request ever reaches the
/// policy-gated shell tool. The default (no `--cwd`) is the runtime's own
/// `context.cwd`, which is anchored at the project root by construction.
fn resolve_cwd(context: &ShellContext, requested: Option<&str>) -> Result<Utf8PathBuf, PathError> {
    let Some(requested) = requested else {
        return Ok(context.cwd.clone());
    };
    resolve_path_in_root(&context.project_root, &context.cwd, requested)
}

/// Map a root-confined cwd resolution failure onto a `CommandResult`:
/// escapes block, I/O failures are recoverable.
fn path_error_result(result_command: &str, error: PathError) -> CommandResult {
    match error {
        PathError::OutsideRoot { requested, root }
        | PathError::SymlinkEscape { requested, root } => {
            let mut result = CommandResult::new(
                result_command,
                CommandStatus::Blocked,
                format!(
                    "working directory `{requested}` is outside the permitted project `{root}`"
                ),
            );
            result
                .diagnostics
                .push(Diagnostic::error("shell.project.boundary", format!("{requested}: {root}")));
            result
        }
        PathError::RootInaccessible { root, source } => CommandResult::recoverable(
            result_command,
            "shell.project.unavailable",
            format!("cannot access project root `{root}`: {source}"),
        ),
        other => {
            CommandResult::recoverable(result_command, "shell.path.unavailable", other.to_string())
        }
    }
}

fn usage_failure(spec: &CommandSpec, message: &str) -> CommandResult {
    CommandResult::recoverable(&spec.name, "shell.arguments.invalid", message)
        .with_suggestion(format!("Usage: {}", spec.usage))
}

fn tool_output_result(
    result_command: &str,
    output: concerto_core::types::ToolOutput,
) -> CommandResult {
    let Some(exit_code) = output
        .data
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
    else {
        return CommandResult::recoverable(
            result_command,
            "shell.execution.invalid-output",
            "the shell tool returned no valid exit code",
        )
        .with_data(output.data);
    };

    let status =
        if exit_code == 0 { CommandStatus::Succeeded } else { CommandStatus::RecoverableFailure };
    let mut result =
        CommandResult::new(result_command, status, output.summary).with_data(output.data);
    result.exit_code = Some(exit_code);
    if exit_code != 0 {
        result.diagnostics.push(
            Diagnostic::error(
                "shell.process.non-zero-exit",
                format!("process exited with status {exit_code}"),
            )
            .with_help("Inspect stdout and stderr, adjust the command, and retry."),
        );
    }
    result
}

fn tool_error_result(result_command: &str, error: ToolError) -> CommandResult {
    match error {
        ToolError::PolicyDenied { rule } if rule == "requires_approval_no_sink" => {
            let mut result = CommandResult::new(
                result_command,
                CommandStatus::AwaitingApproval,
                "command requires approval but no approval channel is attached",
            );
            result.diagnostics.push(Diagnostic::error("shell.policy.approval-required", rule));
            result
        }
        ToolError::PolicyDenied { rule } => {
            let mut result = CommandResult::new(
                result_command,
                CommandStatus::Blocked,
                format!("command blocked by policy: {rule}"),
            );
            result.diagnostics.push(Diagnostic::error("shell.policy.denied", rule));
            result
        }
        ToolError::Cancelled => CommandResult::new(
            result_command,
            CommandStatus::Cancelled,
            "command cancelled by caller",
        ),
        ToolError::Timeout { timeout_secs } => CommandResult::recoverable(
            result_command,
            "shell.process.timeout",
            format!("command timed out after {timeout_secs} seconds"),
        )
        .with_suggestion("Increase --timeout or investigate why the process did not finish."),
        ToolError::VirtualFsConflict { path, reason } => {
            let mut result = CommandResult::new(
                result_command,
                CommandStatus::Blocked,
                format!("working directory `{path}` is outside the permitted project: {reason}"),
            );
            result.diagnostics.push(Diagnostic::error("shell.project.boundary", reason));
            result
        }
        ToolError::ExecutionFailed { message } => {
            CommandResult::recoverable(result_command, "shell.execution.failed", message)
        }
        ToolError::Io(error) => {
            CommandResult::recoverable(result_command, "shell.execution.io", error.to_string())
        }
        ToolError::LspError { message } => CommandResult::recoverable(
            result_command,
            "shell.execution.unexpected-tool-error",
            message,
        ),
        ToolError::RollbackNotSupported => CommandResult::recoverable(
            result_command,
            "shell.execution.rollback-unsupported",
            "the execution tool does not support rollback",
        ),
        ToolError::NotARepository { message } => {
            CommandResult::recoverable(result_command, "shell.repository.not-found", message)
        }
        _ => CommandResult::recoverable(
            result_command,
            "shell.execution.unexpected-tool-error",
            "unexpected tool error category",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use concerto_config::{ShellBackendType, ShellProfileConfig};
    use concerto_core::error::PolicyError;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::traits::policy::{AuditEntry, AuditLog, PolicyEngine};
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{
        CapabilitySet, PolicyAction, PolicyVerdict, SessionContext, ToolOutput, ToolRegistry,
    };
    use concerto_core::{CancellationToken, ToolError};
    use serde_json::json;

    use super::{
        option_value, parse_execution_options, parse_timeout, path_error_result, resolve_cwd,
        tool_output_result, ExternalExecutionRequest, PathError, PolicyExecutionAdapter,
        MAX_TIMEOUT_SECS,
    };
    use crate::{CommandStatus, ShellContext, ShellProfileCatalog, ShellRuntime};

    struct IgnoreAudit;

    #[async_trait]
    impl AuditLog for IgnoreAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    struct FixedPolicy(PolicyVerdict);

    #[async_trait]
    impl PolicyEngine for FixedPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(self.0.clone())
        }

        fn audit_log(&self) -> &dyn AuditLog {
            &IgnoreAudit
        }
    }

    struct FakeShell {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for FakeShell {
        fn name(&self) -> &str {
            "shell"
        }

        fn description(&self) -> &str {
            "fake shell"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }

        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }

        async fn execute(
            &self,
            input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let exit_code = if input.get("command").and_then(|value| value.as_str()) == Some("fail")
            {
                2
            } else {
                0
            };
            Ok(ToolOutput {
                summary: "fake output".to_owned(),
                data: json!({ "exit_code": exit_code, "stdout": "ok", "stderr": "" }),
            })
        }
    }

    fn adapter(
        verdict: PolicyVerdict,
    ) -> (PolicyExecutionAdapter, Arc<AtomicUsize>, tempfile::TempDir) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FakeShell { calls: calls.clone() }));
        let executor =
            Arc::new(ToolExecutor::new(Arc::new(registry), Arc::new(FixedPolicy(verdict))));
        let directory = tempfile::tempdir().expect("temporary project");
        let session =
            SessionContext::new(concerto_core::ids::Ulid::new(), directory.path().to_path_buf());
        (PolicyExecutionAdapter::new(executor, session), calls, directory)
    }

    fn request(directory: &tempfile::TempDir, program: &str) -> ExternalExecutionRequest {
        ExternalExecutionRequest {
            program: program.to_owned(),
            arguments: Vec::new(),
            cwd: Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
                .expect("UTF-8 temporary path"),
            timeout_secs: None,
        }
    }

    fn profiles() -> ShellProfileCatalog {
        let profile = ShellProfileConfig {
            id: "test".to_owned(),
            name: "Test shell".to_owned(),
            backend: ShellBackendType::System,
            executable: "unused-test-shell".to_owned(),
            ..Default::default()
        };
        ShellProfileCatalog::from_profiles([profile], Some("test".to_owned()))
            .expect("valid test profile catalog")
    }

    #[test]
    fn parses_options_before_program() {
        let options = parse_execution_options(
            &[
                "--cwd".to_owned(),
                "src".to_owned(),
                "--timeout".to_owned(),
                "15".to_owned(),
                "cargo".to_owned(),
                "check".to_owned(),
                "--workspace".to_owned(),
            ],
            false,
        )
        .expect("valid execution options");
        assert_eq!(options.cwd.as_deref(), Some("src"));
        assert_eq!(options.timeout_secs, Some(15));
        assert_eq!(options.operands, ["cargo", "check", "--workspace"]);
    }

    #[tokio::test]
    async fn policy_denial_blocks_without_calling_tool() {
        let (adapter, calls, directory) = adapter(PolicyVerdict::Deny);
        let result =
            adapter.execute("run", request(&directory, "echo"), CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Blocked);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn missing_approval_channel_waits_without_stopping_runtime() {
        let verdict =
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) };
        let (adapter, calls, directory) = adapter(verdict);
        let result =
            adapter.execute("run", request(&directory, "echo"), CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::AwaitingApproval);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn non_zero_exit_is_recoverable() {
        let (adapter, calls, directory) = adapter(PolicyVerdict::Allow);
        let result =
            adapter.execute("run", request(&directory, "fail"), CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert_eq!(result.exit_code, Some(2));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn runtime_run_command_uses_policy_executor() {
        let (adapter, calls, directory) = adapter(PolicyVerdict::Allow);
        let project_root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let runtime = ShellRuntime::with_external_execution(
            ShellContext::new(project_root),
            adapter,
            profiles(),
        )
        .expect("external runtime");

        let result = runtime.execute_line("run echo hello", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // -------------------------------------------------------------------
    // Helper function tests
    // -------------------------------------------------------------------

    #[test]
    fn parse_timeout_accepts_valid_values() {
        assert_eq!(parse_timeout("1"), Ok(1));
        assert_eq!(parse_timeout("300"), Ok(300));
        assert_eq!(parse_timeout(&MAX_TIMEOUT_SECS.to_string()), Ok(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn parse_timeout_rejects_zero() {
        let error = parse_timeout("0").expect_err("timeout 0 must fail");
        assert!(error.contains("timeout must be between 1 and"));
    }

    #[test]
    fn parse_timeout_rejects_excessive_value() {
        let too_high = MAX_TIMEOUT_SECS + 1;
        let error = parse_timeout(&too_high.to_string()).expect_err("excessive timeout must fail");
        assert!(error.contains("timeout must be between 1 and"));
    }

    #[test]
    fn parse_timeout_rejects_non_numeric() {
        let error = parse_timeout("not-a-number").expect_err("invalid timeout must fail");
        assert!(error.contains("invalid timeout"));
    }

    #[test]
    fn option_value_returns_next_argument() {
        let args = vec!["--flag".to_owned(), "value".to_owned(), "other".to_owned()];
        let mut index = 0;
        let value = option_value(&args, &mut index, "--flag").expect("value exists");
        assert_eq!(value, "value");
        assert_eq!(index, 1);
    }

    #[test]
    fn option_value_missing_returns_error() {
        let args = vec!["--flag".to_owned()];
        let mut index = 0;
        let error = option_value(&args, &mut index, "--flag").expect_err("missing value");
        assert_eq!(error, "--flag requires a value");
    }

    #[test]
    fn resolve_cwd_uses_context_when_not_requested() {
        let context = ShellContext::new(Utf8PathBuf::from("/project"));
        let result = resolve_cwd(&context, None).expect("default cwd resolves without I/O");
        assert_eq!(result, Utf8PathBuf::from("/project"));
    }

    #[test]
    fn resolve_cwd_rejects_absolute_path_outside_root() {
        let directory = tempfile::tempdir().expect("temporary project");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let context = ShellContext::new(root);
        let result = resolve_cwd(&context, Some("/tmp"));
        assert!(matches!(result, Err(PathError::OutsideRoot { .. })));
    }

    #[test]
    fn resolve_cwd_rejects_dotdot_escape() {
        let directory = tempfile::tempdir().expect("temporary project");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let context = ShellContext::new(root);
        let result = resolve_cwd(&context, Some(".."));
        assert!(matches!(result, Err(PathError::OutsideRoot { .. })));
    }

    #[test]
    fn resolve_cwd_accepts_relative_subdir_inside_root() {
        let directory = tempfile::tempdir().expect("temporary project");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        std::fs::create_dir(root.join("sub")).expect("create subdir");
        let context = ShellContext::new(root.clone());
        let result = resolve_cwd(&context, Some("sub")).expect("in-root relative cwd");
        assert_eq!(result, root.join("sub"));
    }

    #[test]
    fn resolve_cwd_accepts_absolute_path_inside_root() {
        let directory = tempfile::tempdir().expect("temporary project");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let context = ShellContext::new(root.clone());
        let result = resolve_cwd(&context, Some(root.as_str())).expect("in-root absolute cwd");
        assert_eq!(result, root);
    }

    #[test]
    fn path_error_result_maps_escape_to_blocked() {
        let result = path_error_result(
            "run",
            PathError::OutsideRoot {
                requested: Utf8PathBuf::from("../escape"),
                root: Utf8PathBuf::from("/project"),
            },
        );
        assert_eq!(result.status, CommandStatus::Blocked);
        assert!(result.status.permits_continuation());
        assert!(result.diagnostics.iter().any(|d| d.code == "shell.project.boundary"));
    }

    #[test]
    fn path_error_result_maps_unresolvable_to_recoverable() {
        let result =
            path_error_result("run", PathError::Unresolvable { requested: "x".to_owned() });
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn run_escaped_cwd_blocks_without_calling_tool() {
        let (adapter, calls, directory) = adapter(PolicyVerdict::Allow);
        let project_root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let runtime = ShellRuntime::with_external_execution(
            ShellContext::new(project_root),
            adapter,
            profiles(),
        )
        .expect("external runtime");

        // `--cwd ..` resolves outside the project root; containment must
        // block the command before the policy-gated tool is ever called.
        let result =
            runtime.execute_line("run --cwd .. echo hello", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Blocked);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(result.status.permits_continuation());
    }

    #[test]
    fn tool_output_result_maps_zero_exit_to_success() {
        let output = ToolOutput {
            summary: "done".to_owned(),
            data: json!({ "exit_code": 0, "stdout": "ok", "stderr": "" }),
        };
        let result = tool_output_result("test-cmd", output);
        assert_eq!(result.status, CommandStatus::Succeeded);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn tool_output_result_maps_non_zero_exit_to_recoverable() {
        let output = ToolOutput {
            summary: "failed".to_owned(),
            data: json!({ "exit_code": 1, "stdout": "", "stderr": "error" }),
        };
        let result = tool_output_result("test-cmd", output);
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert_eq!(result.exit_code, Some(1));
        assert!(!result.diagnostics.is_empty());
        assert_eq!(result.diagnostics[0].code, "shell.process.non-zero-exit");
    }

    #[test]
    fn tool_output_result_missing_exit_code_is_recoverable() {
        let output =
            ToolOutput { summary: "no exit code".to_owned(), data: json!({ "stdout": "ok" }) };
        let result = tool_output_result("test-cmd", output);
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert!(result.diagnostics.iter().any(|d| d.code == "shell.execution.invalid-output"));
    }
}
