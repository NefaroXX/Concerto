use async_trait::async_trait;
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::{
    CommandRegistry, CommandResult, CommandSource, HistoryError, RegistryError, ShellContext,
    ShellHistory,
};

/// Effects a command may perform. These are explicit policy inputs, not tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandEffect {
    ProjectRead,
    ProjectWrite,
    ProcessSpawn,
    NetworkAccess,
    GitMutation,
    AgentInvocation,
    MemoryRead,
    MemoryWrite,
}

/// Discoverable metadata for a registered command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub source: CommandSource,
    pub effects: Vec<CommandEffect>,
    pub records_history: bool,
}

/// Parsed command and arguments, with the original line retained for auditing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInvocation {
    pub command: String,
    pub arguments: Vec<String>,
    pub raw: String,
}

/// Read-only services shared with built-ins and future extension commands.
#[derive(Clone)]
pub struct CommandServices {
    registry: CommandRegistry,
    history: ShellHistory,
}

impl CommandServices {
    pub(crate) const fn new(registry: CommandRegistry, history: ShellHistory) -> Self {
        Self { registry, history }
    }

    /// List registered command specifications.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lock is unavailable.
    pub fn command_specs(&self) -> Result<Vec<CommandSpec>, RegistryError> {
        self.registry.specs()
    }

    /// Fetch the latest non-meta command result.
    ///
    /// # Errors
    ///
    /// Returns an error if the history lock is unavailable.
    pub fn last_result(&self) -> Result<Option<CommandResult>, HistoryError> {
        self.history.last()
    }
}

/// Executable command contract. Ordinary operational failures are returned as
/// `CommandResult` values so they cannot accidentally terminate the runtime.
#[async_trait]
pub trait ShellCommand: Send + Sync {
    fn spec(&self) -> &CommandSpec;

    async fn execute(
        &self,
        invocation: &CommandInvocation,
        context: &ShellContext,
        services: &CommandServices,
        cancel: CancellationToken,
    ) -> CommandResult;
}
