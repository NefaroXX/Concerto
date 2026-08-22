use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current JSON envelope version for [`CommandResult`].
pub const COMMAND_RESULT_SCHEMA_VERSION: u16 = 1;

/// Outcome category for a command invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandStatus {
    Succeeded,
    SucceededWithWarnings,
    RecoverableFailure,
    AwaitingApproval,
    Blocked,
    Cancelled,
    TerminalFailure,
}

impl CommandStatus {
    /// Whether an automation loop can safely continue after this result.
    #[must_use]
    pub const fn permits_continuation(self) -> bool {
        !matches!(self, Self::TerminalFailure)
    }

    /// Whether the requested command completed successfully.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Succeeded | Self::SucceededWithWarnings)
    }
}

/// Severity of a structured command diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// A stable, machine-readable explanation attached to a command result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            help: None,
        }
    }

    #[must_use]
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            help: None,
        }
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// A file, report, or other durable output produced by a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: String,
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a registered command originates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandSource {
    Builtin,
    External,
    Ai,
    Workflow,
    Plugin,
    Runtime,
}

/// Trace data that explains which subsystem produced a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandProvenance {
    pub source: CommandSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
}

impl CommandProvenance {
    #[must_use]
    pub const fn runtime() -> Self {
        Self { source: CommandSource::Runtime, provider: None, model: None, agent_role: None }
    }
}

/// Versioned result envelope returned by every Concerto shell command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub schema_version: u16,
    pub command: String,
    pub status: CommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub summary: String,
    pub data: Value,
    pub diagnostics: Vec<Diagnostic>,
    pub artifacts: Vec<Artifact>,
    pub suggestions: Vec<String>,
    pub duration_ms: u64,
    pub provenance: CommandProvenance,
}

impl CommandResult {
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        status: CommandStatus,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: COMMAND_RESULT_SCHEMA_VERSION,
            command: command.into(),
            status,
            exit_code: None,
            summary: summary.into(),
            data: Value::Null,
            diagnostics: Vec::new(),
            artifacts: Vec::new(),
            suggestions: Vec::new(),
            duration_ms: 0,
            provenance: CommandProvenance::runtime(),
        }
    }

    #[must_use]
    pub fn recoverable(
        command: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        let mut result = Self::new(command, CommandStatus::RecoverableFailure, message.clone());
        result.diagnostics.push(Diagnostic::error(code, message));
        result
    }

    #[must_use]
    pub fn terminal(
        command: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        let mut result = Self::new(command, CommandStatus::TerminalFailure, message.clone());
        result.diagnostics.push(Diagnostic::error(code, message));
        result
    }

    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    #[must_use]
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestions.push(suggestion.into());
        self
    }

    /// Serialize this result for an agent or `--json` frontend.
    ///
    /// # Errors
    ///
    /// Returns an error only if serialization of the result envelope fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub(crate) fn set_duration_ms(&mut self, duration_ms: u64) {
        self.duration_ms = duration_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandResult, CommandStatus};

    #[test]
    fn only_terminal_failure_stops_continuation() {
        let continuing = [
            CommandStatus::Succeeded,
            CommandStatus::SucceededWithWarnings,
            CommandStatus::RecoverableFailure,
            CommandStatus::AwaitingApproval,
            CommandStatus::Blocked,
            CommandStatus::Cancelled,
        ];
        assert!(continuing.into_iter().all(CommandStatus::permits_continuation));
        assert!(!CommandStatus::TerminalFailure.permits_continuation());
    }

    #[test]
    fn result_envelope_round_trips_through_json() {
        let result =
            CommandResult::recoverable("example", "shell.example.recoverable", "example failure")
                .with_suggestion("Try a different argument.");
        let json = serde_json::to_string(&result).expect("serialize command result");
        let decoded: CommandResult =
            serde_json::from_str(&json).expect("deserialize command result");
        assert_eq!(decoded, result);
    }
}
