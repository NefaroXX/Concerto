use std::sync::Arc;
use std::time::Instant;

use camino::Utf8PathBuf;
use concerto_config::ShellSettings;
use concerto_core::CancellationToken;
use thiserror::Error;

use crate::builtins::{apply_provenance, standard_commands};
use crate::execution::external_commands;
use crate::{
    parse_command_line, CommandEffect, CommandRegistry, CommandResult, CommandServices,
    CommandStatus, PolicyExecutionAdapter, RegistryError, ShellCommand, ShellContext, ShellHistory,
    ShellProfileCatalog, ShellProfileError,
};

const DEFAULT_HISTORY_CAPACITY: usize = 100;

/// Error constructing a runtime or extending it without a policy adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Profile(#[from] ShellProfileError),
    #[error("command `{command}` declares effect `{effect:?}` but no effect-policy adapter is installed")]
    EffectPolicyRequired { command: String, effect: CommandEffect },
    #[error(
        "shell context project `{context_root}` does not match policy executor project `{executor_root}`"
    )]
    ProjectMismatch { context_root: Utf8PathBuf, executor_root: std::path::PathBuf },
}

/// Long-lived typed command runtime. Command-level failures are returned as data.
pub struct ShellRuntime {
    context: ShellContext,
    registry: CommandRegistry,
    history: ShellHistory,
}

impl ShellRuntime {
    /// Create the standard read-only runtime.
    ///
    /// # Errors
    ///
    /// Returns an error only if built-in registration violates runtime
    /// invariants.
    pub fn standard(context: ShellContext) -> Result<Self, RuntimeBuildError> {
        let runtime = Self {
            context,
            registry: CommandRegistry::new(),
            history: ShellHistory::new(DEFAULT_HISTORY_CAPACITY),
        };
        for command in standard_commands() {
            runtime.register_read_only(command)?;
        }
        Ok(runtime)
    }

    /// Create a runtime with policy-gated process execution commands.
    ///
    /// `run` and `shell-run` can spawn processes only through the supplied
    /// [`PolicyExecutionAdapter`]. The runtime refuses to start if its project
    /// context differs from the executor's sandbox root.
    ///
    /// # Errors
    ///
    /// Returns an error for a project-root mismatch or invalid command
    /// registration.
    pub fn with_external_execution(
        context: ShellContext,
        adapter: PolicyExecutionAdapter,
        profiles: ShellProfileCatalog,
    ) -> Result<Self, RuntimeBuildError> {
        if !same_project_root(&context.project_root, adapter.project_dir()) {
            return Err(RuntimeBuildError::ProjectMismatch {
                context_root: context.project_root,
                executor_root: adapter.project_dir().to_path_buf(),
            });
        }

        let runtime = Self::standard(context)?;
        for command in external_commands(adapter, profiles) {
            runtime.registry.register(command)?;
        }
        Ok(runtime)
    }

    /// Create a policy-gated runtime from Concerto's canonical shell settings.
    ///
    /// The canonical selected profile is the default used by `shell-run`; an
    /// explicit `--profile` may still select another configured profile.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid profile settings, project-root mismatch,
    /// or invalid command registration.
    pub fn with_configured_external_execution(
        context: ShellContext,
        adapter: PolicyExecutionAdapter,
        settings: &ShellSettings,
    ) -> Result<Self, RuntimeBuildError> {
        let profiles = ShellProfileCatalog::from_settings(settings)?;
        Self::with_external_execution(context, adapter, profiles)
    }

    /// Add a command whose only possible effect is project reading.
    ///
    /// Effectful extension commands will be enabled by the Phase B policy
    /// adapter rather than silently trusted here.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported effects or invalid registration.
    pub fn register_read_only(
        &self,
        command: Arc<dyn ShellCommand>,
    ) -> Result<(), RuntimeBuildError> {
        if let Some(effect) = command
            .spec()
            .effects
            .iter()
            .find(|effect| !matches!(effect, CommandEffect::ProjectRead))
        {
            return Err(RuntimeBuildError::EffectPolicyRequired {
                command: command.spec().name.clone(),
                effect: *effect,
            });
        }
        self.registry.register(command)?;
        Ok(())
    }

    #[must_use]
    pub const fn context(&self) -> &ShellContext {
        &self.context
    }

    /// Return registered command metadata in stable order.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lock is unavailable.
    pub fn command_specs(&self) -> Result<Vec<crate::CommandSpec>, RegistryError> {
        self.registry.specs()
    }

    /// Execute a parsed command line. Parse and operational failures are result
    /// values and leave the runtime available for the next invocation.
    pub async fn execute_line(&self, line: &str, cancel: CancellationToken) -> CommandResult {
        let invocation = match parse_command_line(line) {
            Ok(invocation) => invocation,
            Err(error) => {
                return CommandResult::recoverable(
                    "shell",
                    "shell.parse.invalid",
                    error.to_string(),
                )
                .with_suggestion("Run `help` to list supported commands.");
            }
        };
        self.execute(invocation, cancel).await
    }

    /// Execute an already-parsed command invocation.
    pub async fn execute(
        &self,
        invocation: crate::CommandInvocation,
        cancel: CancellationToken,
    ) -> CommandResult {
        if cancel.is_cancelled() {
            return CommandResult::new(
                &invocation.command,
                CommandStatus::Cancelled,
                "command cancelled by caller",
            );
        }

        let command = match self.registry.get(&invocation.command) {
            Ok(Some(command)) => command,
            Ok(None) => {
                return CommandResult::recoverable(
                    &invocation.command,
                    "shell.command.unknown",
                    format!("unknown command `{}`", invocation.command),
                )
                .with_suggestion("Run `help` to list available commands.");
            }
            Err(error) => {
                return CommandResult::terminal(
                    &invocation.command,
                    "shell.registry.unavailable",
                    error.to_string(),
                );
            }
        };

        let started = Instant::now();
        let services = CommandServices::new(self.registry.clone(), self.history.clone());
        let mut result = command.execute(&invocation, &self.context, &services, cancel).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        result.set_duration_ms(duration_ms);
        apply_provenance(&mut result, command.spec().source, &self.context);

        if command.spec().records_history {
            if let Err(error) = self.history.record(result.clone()) {
                return CommandResult::terminal(
                    &invocation.command,
                    "shell.history.unavailable",
                    error.to_string(),
                );
            }
        }
        result
    }
}

fn same_project_root(context_root: &camino::Utf8Path, executor_root: &std::path::Path) -> bool {
    match (std::fs::canonicalize(context_root), std::fs::canonicalize(executor_root)) {
        (Ok(context), Ok(executor)) => context == executor,
        _ => context_root.as_std_path() == executor_root,
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use concerto_core::CancellationToken;
    use tempfile::tempdir;

    use super::ShellRuntime;
    use crate::{CommandStatus, ShellContext};

    fn runtime() -> (tempfile::TempDir, ShellRuntime) {
        let directory = tempdir().expect("temporary project");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("UTF-8 temporary path");
        let runtime = ShellRuntime::standard(ShellContext::new(root)).expect("standard runtime");
        (directory, runtime)
    }

    #[tokio::test]
    async fn ordinary_failures_do_not_stop_following_commands() {
        let (_directory, runtime) = runtime();

        let unknown = runtime.execute_line("does-not-exist", CancellationToken::new()).await;
        assert_eq!(unknown.status, CommandStatus::RecoverableFailure);
        assert!(unknown.status.permits_continuation());

        let next = runtime.execute_line("project-info", CancellationToken::new()).await;
        assert_eq!(next.status, CommandStatus::Succeeded);
    }

    #[tokio::test]
    async fn malformed_input_is_recoverable() {
        let (_directory, runtime) = runtime();
        let result = runtime.execute_line("ls-tree 'unfinished", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::RecoverableFailure);
        assert_eq!(result.diagnostics[0].code, "shell.parse.invalid");
    }

    #[tokio::test]
    async fn cancellation_leaves_runtime_usable() {
        let (_directory, runtime) = runtime();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = runtime.execute_line("ls-tree", cancel).await;
        assert_eq!(cancelled.status, CommandStatus::Cancelled);

        let next = runtime.execute_line("help", CancellationToken::new()).await;
        assert_eq!(next.status, CommandStatus::Succeeded);
    }

    #[tokio::test]
    async fn last_returns_latest_recorded_result_as_json() {
        let (_directory, runtime) = runtime();
        let project = runtime.execute_line("project-info", CancellationToken::new()).await;
        assert_eq!(project.status, CommandStatus::Succeeded);

        let last = runtime.execute_line("last --json", CancellationToken::new()).await;
        assert_eq!(last.status, CommandStatus::Succeeded);
        assert!(last.summary.contains("\"command\": \"project-info\""));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tree_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (_directory, runtime) = runtime();
        let outside = tempdir().expect("outside directory");
        let link = runtime.context().project_root.join("outside-link");
        symlink(outside.path(), &link).expect("create symlink");

        let result = runtime.execute_line("ls-tree outside-link", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Blocked);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn tree_blocks_dotdot_escape() {
        let (_directory, runtime) = runtime();
        let result = runtime.execute_line("ls-tree ../outside", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Blocked);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn tree_blocks_absolute_escape() {
        let (_directory, runtime) = runtime();
        let result = runtime.execute_line("ls-tree /etc", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Blocked);
        assert!(result.status.permits_continuation());
    }

    #[tokio::test]
    async fn tree_allows_in_root_path() {
        let (_directory, runtime) = runtime();
        std::fs::create_dir_all(runtime.context().project_root.join("src"))
            .expect("create src directory");

        let result = runtime.execute_line("ls-tree src", CancellationToken::new()).await;
        assert_eq!(result.status, CommandStatus::Succeeded);

        let default = runtime.execute_line("ls-tree", CancellationToken::new()).await;
        assert_eq!(default.status, CommandStatus::Succeeded);
    }
}
