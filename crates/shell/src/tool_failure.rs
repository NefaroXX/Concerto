//! ADR-82 slice 2 — shell-envelope conversion into the core failure vocabulary.
//!
//! Lives in `concerto-shell` (not core) because only a crate that depends on
//! both core and shell may name both (dependency-layering rule). The stable
//! `shell.*` diagnostic codes are adopted verbatim by prefix convention —
//! never re-minted into core codes — and every [`CommandStatus`] maps, so
//! the conversion is lossless: the envelope keeps the original code(s),
//! severity, and message, and only *adds* the typed category/disposition.

use concerto_core::tool_failure::{
    is_valid_code, ToolDiagnostic, ToolDisposition, ToolFailure, ToolFailureCategory,
    ToolFailureContext, ToolSeverity, ToolStatus, TOOL_FAILURE_SCHEMA_VERSION,
};

use crate::{CommandResult, CommandStatus, DiagnosticSeverity};

impl CommandResult {
    /// Convert this result into the core [`ToolFailure`] envelope.
    ///
    /// Lossless by construction: the top-level `code` is the first
    /// diagnostic code (or a status-derived `shell.*` default when the
    /// result carries no diagnostics), every diagnostic is preserved
    /// verbatim (sanitized + capped), and the status maps 1:1 onto
    /// [`ToolStatus`]. The `(category, disposition)` pair is refined from
    /// known stable `shell.*` codes and falls back to the status default
    /// (`Unknown` disposition — "not deterministic" never implies
    /// "retryable").
    ///
    /// The shell envelope is Concerto's own code, so the origin is always
    /// builtin and `opaque` reflects only the category, never the bridge.
    /// Callers that need correlation/evidence ids attach them to the
    /// returned envelope afterwards (all fields are public).
    #[must_use]
    pub fn to_tool_failure(&self) -> ToolFailure {
        let code = self
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.code.clone())
            .unwrap_or_else(|| status_default_code(self.status));
        let (category, disposition) = classify_shell_code(&code);
        let diagnostics = self
            .diagnostics
            .iter()
            .map(|diagnostic| ToolDiagnostic {
                code: diagnostic.code.clone(),
                severity: match diagnostic.severity {
                    DiagnosticSeverity::Info => ToolSeverity::Info,
                    DiagnosticSeverity::Warning => ToolSeverity::Warning,
                    DiagnosticSeverity::Error => ToolSeverity::Error,
                },
                message: diagnostic.message.clone(),
                help: diagnostic.help.clone(),
            })
            .collect::<Vec<_>>();
        let mut failure = ToolFailure::from_error(
            &concerto_core::ToolError::ExecutionFailed { message: self.summary.clone() },
            ToolFailureContext::builtin().with_detail(self.data.clone()),
        );
        // `from_error` supplies the message sanitization, the 512-byte
        // cap, and the 4 KiB hash+len detail bound; this conversion only
        // overwrites the shell-derived identity below.
        debug_assert!(is_valid_code(&code), "shell codes follow <area>.<name>");
        failure.code = code.clone();
        failure.category = category;
        failure.status = map_status(self.status);
        failure.disposition = disposition;
        if !diagnostics.is_empty() {
            failure.diagnostics = diagnostics;
        }
        failure.opaque = matches!(category, ToolFailureCategory::Opaque);
        failure.schema_version = TOOL_FAILURE_SCHEMA_VERSION;
        failure
    }
}

/// 1:1 status mirror (same shape, core names no shell type and vice versa).
/// `#[allow(unreachable_patterns)]`: the wildcard is unreachable while the
/// shell status set is closed, and becomes the forward-compatible arm for a
/// future `CommandStatus` variant.
#[allow(unreachable_patterns)]
fn map_status(status: CommandStatus) -> ToolStatus {
    match status {
        CommandStatus::Succeeded => ToolStatus::Succeeded,
        CommandStatus::SucceededWithWarnings => ToolStatus::SucceededWithWarnings,
        CommandStatus::RecoverableFailure => ToolStatus::RecoverableFailure,
        CommandStatus::AwaitingApproval => ToolStatus::AwaitingApproval,
        CommandStatus::Blocked => ToolStatus::Blocked,
        CommandStatus::Cancelled => ToolStatus::Cancelled,
        CommandStatus::TerminalFailure => ToolStatus::TerminalFailure,
        _ => ToolStatus::RecoverableFailure,
    }
}

/// Status-derived `shell.*` code when a result carries no diagnostics.
/// Same forward-compat wildcard as [`map_status`].
#[allow(unreachable_patterns)]
fn status_default_code(status: CommandStatus) -> String {
    match status {
        CommandStatus::Succeeded | CommandStatus::SucceededWithWarnings => "shell.ok".to_string(),
        CommandStatus::AwaitingApproval => "shell.policy.approval-required".to_string(),
        CommandStatus::Blocked => "shell.policy.denied".to_string(),
        CommandStatus::Cancelled => "shell.cancelled".to_string(),
        CommandStatus::RecoverableFailure | CommandStatus::TerminalFailure => {
            "shell.execution.failed".to_string()
        }
        _ => "shell.execution.failed".to_string(),
    }
}

/// Refine `(category, disposition)` from a stable `shell.*` diagnostic code.
/// Unknown codes degrade to `(Opaque, Unknown)` — never a fabricated retry
/// claim. The table mirrors the shell producer sites (`execution.rs`
/// `tool_error_result` / `tool_output_result` / `path_error_result` /
/// `usage_failure`).
fn classify_shell_code(code: &str) -> (ToolFailureCategory, ToolDisposition) {
    match code {
        "shell.policy.denied" => (ToolFailureCategory::Policy, ToolDisposition::DoNotReIssue),
        "shell.policy.approval-required" => {
            (ToolFailureCategory::Policy, ToolDisposition::AwaitingDecision)
        }
        "shell.project.boundary" => (ToolFailureCategory::Workspace, ToolDisposition::DoNotReIssue),
        "shell.process.timeout" => {
            (ToolFailureCategory::Execution, ToolDisposition::BoundedRetryWithBackoff)
        }
        "shell.process.non-zero-exit" | "shell.execution.failed" => {
            (ToolFailureCategory::Execution, ToolDisposition::Unknown)
        }
        "shell.execution.io" => {
            (ToolFailureCategory::Execution, ToolDisposition::BoundedRetryWithBackoff)
        }
        "shell.execution.invalid-output" => {
            (ToolFailureCategory::Execution, ToolDisposition::Unknown)
        }
        // The shell encoding merged LSP faults and the catch-all here: the
        // envelope alone cannot tell them apart, so it is honestly opaque.
        "shell.execution.unexpected-tool-error" => {
            (ToolFailureCategory::Opaque, ToolDisposition::Unknown)
        }
        "shell.execution.rollback-unsupported" => {
            (ToolFailureCategory::Environment, ToolDisposition::DoNotReIssue)
        }
        "shell.repository.not-found" => {
            (ToolFailureCategory::Environment, ToolDisposition::DoNotReIssue)
        }
        "shell.arguments.invalid" => {
            (ToolFailureCategory::Execution, ToolDisposition::DoNotReIssue)
        }
        "shell.path.unavailable" | "shell.project.unavailable" => {
            (ToolFailureCategory::Environment, ToolDisposition::BoundedRetryWithBackoff)
        }
        "shell.profile.unavailable" | "shell.profile.program-not-found" => {
            (ToolFailureCategory::Environment, ToolDisposition::DoNotReIssue)
        }
        "shell.ok" => (ToolFailureCategory::Execution, ToolDisposition::Unknown),
        "shell.cancelled" => {
            (ToolFailureCategory::Control, ToolDisposition::DoNotReIssueSameIntent)
        }
        // Unknown codes: the envelope alone cannot classify them, so they
        // degrade to `(Opaque, Unknown)` — never a fabricated retry claim.
        // The 1:1 status mirror above still carries the coarse signal.
        _ => (ToolFailureCategory::Opaque, ToolDisposition::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandProvenance, CommandSource, Diagnostic};
    use concerto_core::tool_failure::ToolOrigin;

    fn result_with(
        status: CommandStatus,
        codes: &[(&str, DiagnosticSeverity, &str, Option<&str>)],
    ) -> CommandResult {
        let mut result = CommandResult::new("run", status, "summary text");
        for (code, severity, message, help) in codes {
            let mut diagnostic = Diagnostic {
                code: (*code).to_string(),
                severity: *severity,
                message: (*message).to_string(),
                help: None,
            };
            if let Some(help) = help {
                diagnostic.help = Some((*help).to_string());
            }
            result.diagnostics.push(diagnostic);
        }
        result
    }

    /// Every `CommandStatus` maps losslessly: status 1:1, first diagnostic
    /// code adopted verbatim, all diagnostics preserved.
    #[test]
    fn every_status_maps_losslessly() {
        let cases = [
            (CommandStatus::Succeeded, "shell.ok"),
            (CommandStatus::SucceededWithWarnings, "shell.ok"),
            (CommandStatus::RecoverableFailure, "shell.execution.failed"),
            (CommandStatus::AwaitingApproval, "shell.policy.approval-required"),
            (CommandStatus::Blocked, "shell.policy.denied"),
            (CommandStatus::Cancelled, "shell.cancelled"),
            (CommandStatus::TerminalFailure, "shell.execution.failed"),
        ];
        for (status, default_code) in cases {
            let failure = result_with(status, &[]).to_tool_failure();
            assert_eq!(failure.status, map_status(status), "status 1:1 for {status:?}");
            assert_eq!(failure.code, default_code, "default code for {status:?}");
            assert!(is_valid_code(&failure.code), "code grammar for {status:?}");
            assert_eq!(failure.schema_version, TOOL_FAILURE_SCHEMA_VERSION);
            assert!(!failure.opaque || failure.category == ToolFailureCategory::Opaque);
        }

        // AwaitingApproval keeps its disposition without a diagnostic.
        let failure = result_with(CommandStatus::AwaitingApproval, &[]).to_tool_failure();
        assert_eq!(failure.disposition, ToolDisposition::AwaitingDecision);
        let failure = result_with(CommandStatus::Cancelled, &[]).to_tool_failure();
        assert_eq!(failure.disposition, ToolDisposition::DoNotReIssueSameIntent);
    }

    /// Every stable shell diagnostic code maps to its documented
    /// category/disposition with the code adopted verbatim.
    #[test]
    fn every_stable_shell_code_maps_losslessly() {
        let cases: Vec<(&str, ToolFailureCategory, ToolDisposition)> = vec![
            ("shell.policy.denied", ToolFailureCategory::Policy, ToolDisposition::DoNotReIssue),
            (
                "shell.policy.approval-required",
                ToolFailureCategory::Policy,
                ToolDisposition::AwaitingDecision,
            ),
            (
                "shell.project.boundary",
                ToolFailureCategory::Workspace,
                ToolDisposition::DoNotReIssue,
            ),
            (
                "shell.process.timeout",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                "shell.process.non-zero-exit",
                ToolFailureCategory::Execution,
                ToolDisposition::Unknown,
            ),
            ("shell.execution.failed", ToolFailureCategory::Execution, ToolDisposition::Unknown),
            (
                "shell.execution.io",
                ToolFailureCategory::Execution,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                "shell.execution.invalid-output",
                ToolFailureCategory::Execution,
                ToolDisposition::Unknown,
            ),
            (
                "shell.execution.unexpected-tool-error",
                ToolFailureCategory::Opaque,
                ToolDisposition::Unknown,
            ),
            (
                "shell.execution.rollback-unsupported",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
            (
                "shell.repository.not-found",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
            (
                "shell.arguments.invalid",
                ToolFailureCategory::Execution,
                ToolDisposition::DoNotReIssue,
            ),
            (
                "shell.path.unavailable",
                ToolFailureCategory::Environment,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                "shell.project.unavailable",
                ToolFailureCategory::Environment,
                ToolDisposition::BoundedRetryWithBackoff,
            ),
            (
                "shell.profile.unavailable",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
            (
                "shell.profile.program-not-found",
                ToolFailureCategory::Environment,
                ToolDisposition::DoNotReIssue,
            ),
        ];
        for (code, category, disposition) in cases {
            let failure = result_with(
                CommandStatus::RecoverableFailure,
                &[(code, DiagnosticSeverity::Error, "boom", Some("try this"))],
            )
            .to_tool_failure();
            assert_eq!(failure.code, code, "code adopted verbatim for {code}");
            assert_eq!(failure.category, category, "category for {code}");
            assert_eq!(failure.disposition, disposition, "disposition for {code}");
            assert_eq!(failure.diagnostics.len(), 1);
            assert_eq!(failure.diagnostics[0].code, code, "diagnostic kept for {code}");
            assert_eq!(failure.diagnostics[0].help.as_deref(), Some("try this"));
            assert!(is_valid_code(&failure.code), "grammar for {code}");
        }
    }

    /// Unknown codes degrade to `(Opaque, Unknown)` — never a fabricated
    /// retry claim — while the code itself is still preserved verbatim.
    #[test]
    fn unknown_shell_codes_degrade_without_masquerading() {
        let failure = result_with(
            CommandStatus::RecoverableFailure,
            &[("shell.future.unknown-thing", DiagnosticSeverity::Error, "?", None)],
        )
        .to_tool_failure();
        assert_eq!(failure.code, "shell.future.unknown-thing", "code preserved");
        assert_eq!(failure.category, ToolFailureCategory::Opaque);
        assert_eq!(failure.disposition, ToolDisposition::Unknown);
        assert!(failure.opaque);
    }

    /// Multiple diagnostics: the first becomes the top-level code, all are
    /// preserved in order.
    #[test]
    fn first_code_wins_and_all_diagnostics_survive() {
        let failure = result_with(
            CommandStatus::Blocked,
            &[
                ("shell.policy.denied", DiagnosticSeverity::Error, "denied", None),
                ("shell.project.boundary", DiagnosticSeverity::Warning, "edge", None),
            ],
        )
        .to_tool_failure();
        assert_eq!(failure.code, "shell.policy.denied");
        assert_eq!(failure.diagnostics.len(), 2);
        assert_eq!(failure.diagnostics[1].code, "shell.project.boundary");
        assert_eq!(failure.diagnostics[1].severity, ToolSeverity::Warning);
    }

    /// The shell conversion round-trips through the core envelope JSON and
    /// preserves provenance-adjacent identity (command name rides the
    /// diagnostic trail, not the failure code).
    #[test]
    fn shell_envelope_survives_core_round_trip() {
        let mut result = result_with(
            CommandStatus::RecoverableFailure,
            &[("shell.process.non-zero-exit", DiagnosticSeverity::Error, "exit 2", None)],
        );
        result.provenance = crate::CommandProvenance {
            source: CommandSource::External,
            ..CommandProvenance::runtime()
        };
        let failure = result.to_tool_failure();
        let json = serde_json::to_string(&failure).expect("failure serializes");
        let back = ToolFailure::from_json(&json).expect("core envelope loads");
        assert_eq!(back, failure);
        assert_eq!(back.origin, ToolOrigin::Builtin);
    }
}
