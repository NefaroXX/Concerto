//! Bounded corrective turns for failed shell executions (custom-ai-shell
//! plan, Phase C subset).
//!
//! When the single-agent loop's `shell` tool call fails at EXECUTION time —
//! a non-zero exit, a timeout, or a process-start error — the loop may push
//! ONE corrective user turn so the model can retry a fixed command without
//! consuming a continuation round. Each failed tool call id has a small
//! per-run repair budget ([`MAX_SHELL_REPAIR_ATTEMPTS`]); when exhausted the
//! failure surfaces unchanged as the ordinary error tool result, with the
//! evidence already preserved in the transcript.
//!
//! Hard boundaries, by design:
//! - Policy outcomes are NEVER repaired. `PolicyDenied` denials, approval
//!   outcomes, and containment blocks (`VirtualFsConflict`) surface
//!   unchanged — coaching a model around them would be a policy bypass.
//! - Cancellation is respected: a cancelled run spends no repair turn.
//! - This budget is independent of the tool-guard arg-shape budget
//!   (`tool_guard::MAX_TOOL_GUARD_REJECTS`): different counter, different
//!   trigger. Arg-shape repair happens BEFORE execution; this module only
//!   sees completed executions.
//!
//! Everything here is derived from the failed `ToolOutput` / `ToolError`
//! already in hand — no new shell-history plumbing, and no process is ever
//! spawned to build a corrective turn.

use concerto_core::types::ToolOutput;
use concerto_core::ToolError;

/// Maximum corrective repair turns per failed `shell` tool call id, per run.
///
/// Small by design and intentionally not configurable: the budget only bounds
/// the coaching loop for one broken command. When it is exhausted the failure
/// surfaces unchanged (recoverable result, evidence preserved in the tool
/// result already in the transcript), and the run-level iteration cap and
/// continuation-round bound remain the global backstops.
pub const MAX_SHELL_REPAIR_ATTEMPTS: u32 = 2;

/// Character caps for the corrective message. Chosen so a repair turn stays
/// small even when a command dumps megabytes of output.
pub const MAX_COMMAND_CHARS: usize = 500;
pub const MAX_STDERR_CHARS: usize = 1500;
pub const MAX_STDOUT_CHARS: usize = 500;

/// Name of the shell tool whose execution failures this module repairs.
pub const SHELL_TOOL_NAME: &str = "shell";

/// Stable diagnostic codes mirrored from the shell runtime's `CommandResult`
/// diagnostics (crates/shell/src/execution.rs `tool_output_result` /
/// `tool_error_result`). The agent-loop path derives the same code from the
/// failed result's shape, so coaching text stays stable across both paths.
pub const DIAG_NON_ZERO_EXIT: &str = "shell.process.non-zero-exit";
pub const DIAG_TIMEOUT: &str = "shell.process.timeout";
pub const DIAG_EXECUTION_FAILED: &str = "shell.execution.failed";
pub const DIAG_EXECUTION_IO: &str = "shell.execution.io";

/// The execution failure a repair turn coaches for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellFailure {
    /// The command ran and exited non-zero; output was captured.
    NonZeroExit { exit_code: i32, stdout: String, stderr: String },
    /// The command exceeded its timeout; no output was captured.
    Timeout { timeout_secs: u64 },
    /// The tool could not run the command to completion (spawn failure,
    /// containment canonicalization, oversized input, ...). Carries the
    /// stable diagnostic code matching the shell runtime's mapping
    /// (`ExecutionFailed` → `shell.execution.failed`, `Io` →
    /// `shell.execution.io`).
    ProcessError { message: String, not_found: bool, diagnostic: &'static str },
}

impl ShellFailure {
    /// The stable diagnostic code for this failure, mirroring the shell
    /// runtime's `CommandResult` diagnostics.
    pub(crate) fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::NonZeroExit { .. } => DIAG_NON_ZERO_EXIT,
            Self::Timeout { .. } => DIAG_TIMEOUT,
            Self::ProcessError { diagnostic, .. } => diagnostic,
        }
    }
}

/// Classify a completed `shell` tool result into a repairable execution
/// failure, if any.
///
/// A successful exit, any other tool, or a result without a usable exit code
/// never classifies. Note this reads the SAME `ToolOutput` the loop already
/// pushed as a tool message — no additional plumbing.
pub(crate) fn repairable_failure(tool_name: &str, output: &ToolOutput) -> Option<ShellFailure> {
    if tool_name != SHELL_TOOL_NAME {
        return None;
    }
    let exit_code = output.data.get("exit_code").and_then(serde_json::Value::as_i64)?;
    let exit_code = i32::try_from(exit_code).ok()?;
    if exit_code == 0 {
        return None;
    }
    Some(ShellFailure::NonZeroExit {
        exit_code,
        stdout: string_field(&output.data, "stdout"),
        stderr: string_field(&output.data, "stderr"),
    })
}

/// Classify a failed `shell` tool execution into a repairable execution
/// failure, if any.
///
/// Policy outcomes surface unchanged and are never repaired: repairing any
/// of them would coach a model around a gate. `PolicyDenied` (executor
/// verdicts and the tool's own denylist), approval outcomes
/// ([`PolicyError::ApprovalTimeout`] arrives both as the literal
/// "approval timed out" message and as the write-gate rejection envelope
/// `gate rejected the write (…): …` — e.g.
/// `"gate rejected the write (GateDenied): RequireApproval { timeout: 30s }"`
/// — where the gate denial shows up as an `ExecutionFailed`),
/// cancellation, and containment blocks (`VirtualFsConflict`) all surface
/// unchanged. Only genuine process faults (non-zero exit handled by
/// [`repairable_failure`], `Timeout`, spawn/I/O errors) are repairable.
pub(crate) fn repairable_error_failure(tool_name: &str, error: &ToolError) -> Option<ShellFailure> {
    if tool_name != SHELL_TOOL_NAME {
        return None;
    }
    match error {
        ToolError::Timeout { timeout_secs } => {
            Some(ShellFailure::Timeout { timeout_secs: *timeout_secs })
        }
        ToolError::ExecutionFailed { message } if is_policy_denial_message(message) => {
            // A wrapped gate/policy denial: surface unchanged, never repaired
            // (the module's hard boundary holds by capability, not by the
            // denial text matching one literal).
            None
        }
        ToolError::ExecutionFailed { message } => Some(ShellFailure::ProcessError {
            message: message.clone(),
            not_found: message_looks_not_found(message),
            diagnostic: DIAG_EXECUTION_FAILED,
        }),
        ToolError::Io(error) => Some(ShellFailure::ProcessError {
            message: error.to_string(),
            not_found: error.kind() == std::io::ErrorKind::NotFound,
            diagnostic: DIAG_EXECUTION_IO,
        }),
        // PolicyDenied / Cancelled / VirtualFsConflict / NotARepository and
        // every other category: surface unchanged, never repaired.
        _ => None,
    }
}

/// Semantic classification of the embedded-denial shapes an
/// `ExecutionFailed` carries (the write-gate rejection envelope and the
/// executor's wrapped verdict strings). Anything matching is a policy or
/// approval outcome — never an execution failure a repair turn may coach.
fn is_policy_denial_message(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.eq_ignore_ascii_case("approval timed out") {
        return true;
    }
    // The write-gate rejection envelope produced by gate_proxy.rs /
    // ipc.rs: nothing repairable ever rides it, whatever the inner verdict.
    if message.starts_with("gate rejected the write") {
        return true;
    }
    const DENIAL_MARKERS: &[&str] =
        &["gatedenied", "requireapproval", "policydenied", "blocked", "denied", "cancelled"];
    let lowered = trimmed.to_ascii_lowercase();
    DENIAL_MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// Heuristic for the not-found cause category: the message reports a missing
/// executable ("shell executable not found on PATH: …") or a failed spawn
/// ("failed to spawn process: No such file or directory").
fn message_looks_not_found(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("not found")
        || lowered.contains("no such file")
        || lowered.contains("failed to spawn process")
}

/// Render the corrective user-turn text for one repair attempt.
///
/// `attempt` is 1-based. The text is prefixed with an auditable
/// `[shell-repair attempt N/M]` marker, states the stable diagnostic code and
/// the likely cause category, and asks for exactly ONE corrected `shell`
/// call.
pub(crate) fn corrective_message_text(
    attempt: u32,
    command: &str,
    failure: &ShellFailure,
) -> String {
    let mut message = format!(
        "[shell-repair attempt {attempt}/{}]\n\
         The `shell` tool call failed (diagnostic: {}).\n\
         Failed command: {}\n",
        MAX_SHELL_REPAIR_ATTEMPTS,
        failure.diagnostic_code(),
        head_chars(command, MAX_COMMAND_CHARS),
    );
    match failure {
        ShellFailure::NonZeroExit { exit_code, stdout, stderr } => {
            message.push_str(&format!("Exit code: {exit_code}\n"));
            if !stderr.trim().is_empty() {
                message.push_str(&format!(
                    "Stderr (last {} chars):\n{}\n",
                    MAX_STDERR_CHARS,
                    tail_chars(stderr, MAX_STDERR_CHARS)
                ));
            } else if !stdout.trim().is_empty() {
                message.push_str(&format!(
                    "Stderr was empty. Stdout (last {} chars):\n{}\n",
                    MAX_STDOUT_CHARS,
                    tail_chars(stdout, MAX_STDOUT_CHARS)
                ));
            } else {
                message.push_str("Stdout and stderr were empty.\n");
            }
            message.push_str(
                "Likely cause: the command ran but exited non-zero — inspect the captured \
                 output above and fix the command.\n",
            );
        }
        ShellFailure::Timeout { timeout_secs } => {
            message
                .push_str(&format!("Exit code: not recorded (timed out after {timeout_secs}s)\n"));
            message.push_str(
                "Likely cause: the command exceeded its timeout — split the work into \
                 smaller steps or raise the timeout explicitly.\n",
            );
        }
        ShellFailure::ProcessError { message: error_message, not_found, .. } => {
            message.push_str("Exit code: not recorded (the process did not complete)\n");
            message.push_str(&format!("Error: {error_message}\n"));
            if *not_found {
                message.push_str(
                    "Likely cause: the process could not start — check the executable \
                     name for this shell.\n",
                );
            } else {
                message.push_str(
                    "Likely cause: the tool could not run the command — inspect the \
                     error above and fix the command.\n",
                );
            }
        }
    }
    message
        .push_str("Reply with exactly ONE corrected `shell` tool call (no other tools, no prose).");
    message
}

/// The failed command as written: the shell input's `command` plus its `args`
/// array, rendered from the validated tool arguments already in hand.
pub(crate) fn command_from_arguments(arguments: &serde_json::Value) -> String {
    let mut command =
        arguments.get("command").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    if let Some(args) = arguments.get("args").and_then(serde_json::Value::as_array) {
        for arg in args {
            if let Some(arg) = arg.as_str() {
                command.push(' ');
                command.push_str(arg);
            }
        }
    }
    command
}

/// The stable diagnostic code for a failure — exposed for logging.
pub(crate) fn diagnostic_code(failure: &ShellFailure) -> &'static str {
    failure.diagnostic_code()
}

fn string_field(data: &serde_json::Value, key: &str) -> String {
    data.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_string()
}

/// First `max_chars` characters of `text`, char-boundary safe.
pub(crate) fn head_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices().nth(max_chars).map_or(text, |(index, _)| &text[..index])
}

/// Last `max_chars` characters of `text`, char-boundary safe.
pub(crate) fn tail_chars(text: &str, max_chars: usize) -> &str {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let start = text.char_indices().nth(total - max_chars).map_or(text.len(), |(index, _)| index);
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell_output(exit_code: i64, stdout: &str, stderr: &str) -> ToolOutput {
        ToolOutput {
            summary: format!("Command `x` exited {exit_code}."),
            data: json!({ "exit_code": exit_code, "stdout": stdout, "stderr": stderr }),
        }
    }

    #[test]
    fn non_zero_exit_classifies_with_output() {
        let failure = repairable_failure(SHELL_TOOL_NAME, &shell_output(2, "out", "err"))
            .expect("non-zero exit must be repairable");
        assert_eq!(
            failure,
            ShellFailure::NonZeroExit {
                exit_code: 2,
                stdout: "out".to_string(),
                stderr: "err".to_string()
            }
        );
        assert_eq!(diagnostic_code(&failure), DIAG_NON_ZERO_EXIT);
    }

    #[test]
    fn zero_exit_and_missing_exit_code_do_not_classify() {
        assert!(repairable_failure(SHELL_TOOL_NAME, &shell_output(0, "", "")).is_none());
        let no_code = ToolOutput { summary: "s".into(), data: json!({ "stdout": "x" }) };
        assert!(repairable_failure(SHELL_TOOL_NAME, &no_code).is_none());
    }

    #[test]
    fn other_tools_never_classify() {
        assert!(repairable_failure("filesystem", &shell_output(1, "", "")).is_none());
        assert!(repairable_error_failure("filesystem", &ToolError::Timeout { timeout_secs: 5 })
            .is_none());
    }

    #[test]
    fn policy_and_cancellation_errors_never_classify() {
        assert!(repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::PolicyDenied { rule: "deny".into() }
        )
        .is_none());
        assert!(repairable_error_failure(SHELL_TOOL_NAME, &ToolError::Cancelled).is_none());
        assert!(repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::VirtualFsConflict { path: "/x".into(), reason: "outside".into() }
        )
        .is_none());
    }

    #[test]
    fn approval_timeout_is_not_repairable() {
        // The executor maps `PolicyError::ApprovalTimeout` to this literal
        // message; repairing it would coach a retry past a missing approval.
        assert!(repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::ExecutionFailed { message: "approval timed out".into() }
        )
        .is_none());
    }

    #[test]
    fn wrapped_gate_denials_are_never_repairable() {
        // The exact string observed live (2026-09 smoke): the unattended shell
        // write hit the 30s approval timeout and the gate surfaced the denial
        // wrapped as `ExecutionFailed`. The literal-string check used to
        // classify it as repairable, coaching a model around the gate.
        let live = "tool execution failed: gate rejected the write (GateDenied): \
                    RequireApproval { timeout: 30s }";
        assert!(repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::ExecutionFailed { message: live.into() }
        )
        .is_none());

        // Every gate-rejection shape this path can produce is non-repairable:
        // any code (GateDenied/GateError/Conflict/...) and any embedded reason
        // (any RequireApproval shape, Blocked, Denied).
        for message in [
            "gate rejected the write (GateDenied): RequireApproval { timeout: 30s }",
            "gate rejected the write (GateDenied): un_granted",
            "gate rejected the write (GateError): policy evaluation failed",
            "gate rejected the write (Conflict): base_version mismatch",
            "supervisor gate said: gate rejected the write (GateDenied): Blocked: containment",
        ] {
            let wrapped = ToolError::ExecutionFailed { message: message.into() };
            assert!(
                repairable_error_failure(SHELL_TOOL_NAME, &wrapped).is_none(),
                "a wrapped gate rejection is never a repairable execution failure: {message}"
            );
        }
    }

    #[test]
    fn embedded_denial_variants_are_never_repairable() {
        // Variant/semantic classification: the denial vocabulary — whatever
        // shape or timeout the verdict carries — is never a repairable
        // execution failure.
        let denials = [
            "RequireApproval { timeout: 30s }",
            "RequireApprovalWithTimeout { timeout: 5s }",
            "RequireApproval(Condition)",
            "GateDenied",
            "PolicyDenied: shell_danger",
            "Blocked: containment rejected the target",
            "denied: outside project root",
            "The operation was cancelled",
        ];
        for message in denials {
            let error = ToolError::ExecutionFailed { message: message.into() };
            assert!(
                repairable_error_failure(SHELL_TOOL_NAME, &error).is_none(),
                "denial must never be repaired: {message}"
            );
        }
    }

    #[test]
    fn genuine_process_failures_still_repair() {
        // Non-denial execution failures keep the bounded (2-attempt) repair
        // behavior: spawn leniacy, oversized input, io faults, timeouts.
        for message in [
            "failed to spawn process: No such file or directory (os error 2)",
            "shell executable not found on PATH: fruitloop",
            "command exceeds maximum length",
        ] {
            assert!(
                matches!(
                    repairable_error_failure(
                        SHELL_TOOL_NAME,
                        &ToolError::ExecutionFailed { message: message.into() }
                    ),
                    Some(ShellFailure::ProcessError { .. })
                ),
                "genuine execution failure stays repairable: {message}"
            );
        }
    }

    #[test]
    fn timeout_and_process_errors_classify() {
        let timeout =
            repairable_error_failure(SHELL_TOOL_NAME, &ToolError::Timeout { timeout_secs: 30 })
                .expect("timeout must be repairable");
        assert_eq!(timeout, ShellFailure::Timeout { timeout_secs: 30 });
        assert_eq!(diagnostic_code(&timeout), DIAG_TIMEOUT);

        let not_found = repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::ExecutionFailed {
                message: "shell executable not found on PATH: fruitloop".into(),
            },
        )
        .expect("spawn failure must be repairable");
        assert!(
            matches!(
                not_found,
                ShellFailure::ProcessError {
                    not_found: true,
                    diagnostic: DIAG_EXECUTION_FAILED,
                    ..
                }
            ),
            "spawn failure misclassified: {not_found:?}"
        );

        let generic = repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::ExecutionFailed { message: "command exceeds maximum length".into() },
        )
        .expect("execution failure must be repairable");
        assert!(
            matches!(
                generic,
                ShellFailure::ProcessError {
                    not_found: false,
                    diagnostic: DIAG_EXECUTION_FAILED,
                    ..
                }
            ),
            "execution failure misclassified: {generic:?}"
        );

        let io_not_found = repairable_error_failure(
            SHELL_TOOL_NAME,
            &ToolError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
        )
        .expect("io failure must be repairable");
        assert!(
            matches!(
                io_not_found,
                ShellFailure::ProcessError { not_found: true, diagnostic: DIAG_EXECUTION_IO, .. }
            ),
            "io failure misclassified: {io_not_found:?}"
        );
    }

    #[test]
    fn corrective_message_honors_caps_and_includes_diagnostics() {
        let long_command = "x".repeat(900);
        let failure = ShellFailure::NonZeroExit {
            exit_code: 2,
            stdout: "y".repeat(900),
            stderr: "e".repeat(2500),
        };
        let message = corrective_message_text(1, &long_command, &failure);

        assert!(message.starts_with("[shell-repair attempt 1/2]"), "marker missing: {message}");
        assert!(message.contains(DIAG_NON_ZERO_EXIT), "diagnostic missing: {message}");
        assert!(message.contains("Exit code: 2"), "exit code missing: {message}");
        // Command head-capped.
        assert!(message.contains(&"x".repeat(MAX_COMMAND_CHARS)));
        assert!(message.len() < 900 + 2500 + 1000, "caps not honored: {}", message.len());
        // Stderr tail-capped (head + stderr tail + boilerplate).
        assert!(message.contains(&"e".repeat(MAX_STDERR_CHARS)), "stderr tail missing: {message}");
        assert!(message.contains("inspect the captured output above and fix the command"));
        assert!(message.contains("exactly ONE corrected `shell` tool call"));
    }

    #[test]
    fn corrective_message_uses_stdout_tail_when_stderr_empty() {
        let failure = ShellFailure::NonZeroExit {
            exit_code: 1,
            stdout: "o".repeat(800),
            stderr: String::new(),
        };
        let message = corrective_message_text(2, "cargo test", &failure);
        assert!(message.starts_with("[shell-repair attempt 2/2]"), "marker: {message}");
        assert!(
            message.contains("Stderr was empty. Stdout (last 500 chars):"),
            "stdout fallback missing: {message}"
        );
        assert!(message.contains(&"o".repeat(MAX_STDOUT_CHARS)));
        assert!(!message.contains(&"o".repeat(501)), "stdout tail not capped: {message}");
    }

    #[test]
    fn corrective_message_timeout_and_not_found_categories() {
        let timeout_message =
            corrective_message_text(1, "build", &ShellFailure::Timeout { timeout_secs: 30 });
        assert!(timeout_message.contains(DIAG_TIMEOUT));
        assert!(timeout_message.contains("split the work into smaller steps or raise the timeout"));
        assert!(timeout_message.contains("Failed command: build"));

        let not_found_message = corrective_message_text(
            1,
            "fruitloop --help",
            &ShellFailure::ProcessError {
                message: "failed to spawn process: No such file or directory (os error 2)".into(),
                not_found: true,
                diagnostic: DIAG_EXECUTION_FAILED,
            },
        );
        assert!(not_found_message.contains(DIAG_EXECUTION_FAILED));
        assert!(not_found_message.contains("check the executable name for this shell"));
    }

    #[test]
    fn command_from_arguments_renders_command_and_args() {
        let arguments = json!({ "command": "git", "args": ["log", "--oneline"] });
        assert_eq!(command_from_arguments(&arguments), "git log --oneline");
        assert_eq!(command_from_arguments(&json!({ "command": "ls" })), "ls");
        assert_eq!(command_from_arguments(&json!({})), "");
    }

    #[test]
    fn truncation_helpers_are_char_boundary_safe() {
        assert_eq!(head_chars("hello", 3), "hel");
        assert_eq!(head_chars("hi", 10), "hi");
        assert_eq!(tail_chars("hello", 3), "llo");
        assert_eq!(tail_chars("hi", 10), "hi");
        // Multi-byte characters are never split mid-codepoint.
        let emoji_text = "aé漢😀字".repeat(10);
        let head = head_chars(&emoji_text, 7);
        let tail = tail_chars(&emoji_text, 7);
        assert!(emoji_text.contains(head) && emoji_text.contains(tail));
        assert_eq!(head.chars().count(), 7);
        assert_eq!(tail.chars().count(), 7);
    }
}
