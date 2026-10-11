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

use concerto_core::tool_failure::{ToolDisposition, ToolFailure};
use concerto_core::types::ToolOutput;
use concerto_core::ToolError;

/// Maximum corrective repair turns per failed `shell` tool call id, per run.
///
/// Small by design and intentionally not configurable: the budget only bounds
/// the coaching loop for one broken command. When it is exhausted the failure
/// surfaces unchanged (recoverable result, evidence preserved in the tool
/// result already in the transcript), and the run-level iteration cap and
/// continuation-round bound remain the global backstops.
pub const MAX_SHELL_REPAIR_ATTEMPTS: u32 = 5;

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

/// H02 typed repair verdict: every [`ToolDisposition`] maps to a distinct
/// verdict, so a cancelled call is never coached like a transient fault and
/// an approval-park is never retried like a timeout.
///
/// The two retry-family dispositions (`BoundedRetryWithBackoff`, `Unknown`)
/// deliberately share the bounded coaching turn — the turn is one piece of
/// fix-the-command coaching under [`MAX_SHELL_REPAIR_ATTEMPTS`], not a retry
/// guarantee, and the envelope's disposition still rides the tool result the
/// model reads — while the four stop-family dispositions each surface
/// unchanged for a different recorded reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepairVerdict {
    /// The failure earns one corrective user turn with this failure.
    Repair(ShellFailure),
    /// Surface unchanged: deterministic refusals, policy denials, and
    /// containment blocks — the identical action cannot succeed.
    DoNotRepair,
    /// Surface unchanged: a deliberate stop; only a new user instruction
    /// may re-issue the intent, never a repair turn.
    Cancelled,
    /// Surface unchanged: parked awaiting the operator; resolution is the
    /// answer, not a retry.
    AwaitingDecision,
    /// Surface unchanged: reconcile the workspace first — a same-command
    /// retry turn would coach the wrong recovery.
    NeedsRebase,
}

/// Classify a failed `shell` tool execution into a typed repair verdict
/// (H02).
///
/// The typed [`ToolFailure`] envelope — code, category, disposition carried
/// on the tool result — is the derivation source; prose is never parsed on
/// this path, with exactly one fail-closed exception below. A missing
/// envelope, or an `opaque` one (genuinely opaque third-party failure text
/// with no native code), takes the bounded legacy fallback
/// ([`repairable_error_failure`]), which keeps the historical substring
/// heuristics in that demoted role only.
///
/// Fail-closed boundary veto (not classification): the gate-rejection
/// envelope (`gate rejected the write …`) and the executor's approval-timeout
/// literal (`approval timed out`) are still stringly `ExecutionFailed` until
/// the executor's `ApprovalTimeout` mapping gains construction-site typing.
/// They veto a repair on BOTH paths — coaching a model around a gate or a
/// missing approval would be a policy bypass (ADR-62), and no disposition
/// may override that. This veto recognizes our own boundary-envelope shapes
/// (exact prefix / literal); the generic denial-substring scan is fallback
/// only.
pub(crate) fn repair_verdict(
    tool_name: &str,
    error: &ToolError,
    failure: Option<&ToolFailure>,
) -> RepairVerdict {
    if tool_name != SHELL_TOOL_NAME {
        return RepairVerdict::DoNotRepair;
    }
    if let ToolError::ExecutionFailed { message } = error {
        if is_gate_envelope_or_approval_timeout(message) {
            return RepairVerdict::DoNotRepair;
        }
    }
    match failure {
        Some(envelope) if !envelope.opaque => verdict_from_failure(envelope),
        // Absent or opaque envelope: the bounded legacy fallback classifies
        // the prose. Opaque text never masquerades as a native verdict — the
        // fallback can only earn the generic coaching turn, never a typed
        // claim.
        _ => match repairable_error_failure(tool_name, error) {
            Some(failure) => RepairVerdict::Repair(failure),
            None => RepairVerdict::DoNotRepair,
        },
    }
}

/// Classify a non-opaque typed envelope into its repair verdict. Pure on the
/// envelope's code / category / disposition — no `Display` text is read.
/// The wildcard is fail-closed by design: the disposition enum is
/// `#[non_exhaustive]`, so a future disposition surfaces unchanged instead
/// of silently earning a repair turn.
pub(crate) fn verdict_from_failure(failure: &ToolFailure) -> RepairVerdict {
    match failure.disposition {
        ToolDisposition::DoNotReIssue => RepairVerdict::DoNotRepair,
        ToolDisposition::DoNotReIssueSameIntent => RepairVerdict::Cancelled,
        ToolDisposition::AwaitingDecision => RepairVerdict::AwaitingDecision,
        ToolDisposition::BoundedRetryWithBackoff | ToolDisposition::Unknown => {
            RepairVerdict::Repair(repairable_shell_failure(failure))
        }
        ToolDisposition::RetryableAfterRebase => RepairVerdict::NeedsRebase,
        // Future dispositions (`#[non_exhaustive]`): surface unchanged.
        _ => RepairVerdict::DoNotRepair,
    }
}

/// Build the coachable [`ShellFailure`] for a retry-family envelope from
/// structured fields only: the timeout seconds come from the envelope's
/// `detail` object, the diagnostic from the (verbatim-adopted) failure code,
/// and the not-found hint from stable codes — never from message text.
///
/// H02 note on the retired F9 exemption: a genuine EACCES spawn fault is a
/// structural `Io(PermissionDenied)`, which the envelope maps to
/// `DoNotReIssue` — so it never reaches this constructor, and no
/// "permission denied (os error …)" substring exemption is needed on the
/// typed path. The exemption survives only in the opaque fallback.
fn repairable_shell_failure(failure: &ToolFailure) -> ShellFailure {
    if failure.code == "exec.timeout" {
        let timeout_secs =
            failure.detail.get("timeout_secs").and_then(serde_json::Value::as_u64).unwrap_or(0);
        return ShellFailure::Timeout { timeout_secs };
    }
    ShellFailure::ProcessError {
        message: failure.message.clone(),
        not_found: typed_not_found(failure),
        diagnostic: diag_for_code(&failure.code),
    }
}

/// Whether the structured envelope states a not-found cause: the stable
/// `shell.profile.program-not-found` code (verbatim-adopted, never re-minted)
/// on the top-level code or any diagnostic, or a structural `NotFound`
/// `io_kind` in the detail object. Message text is never consulted.
fn typed_not_found(failure: &ToolFailure) -> bool {
    const NOT_FOUND_CODES: &[&str] = &["shell.profile.program-not-found"];
    NOT_FOUND_CODES.contains(&failure.code.as_str())
        || failure
            .diagnostics
            .iter()
            .any(|diagnostic| NOT_FOUND_CODES.contains(&diagnostic.code.as_str()))
        || (failure.code == "exec.io_error"
            && failure.detail.get("io_kind").and_then(serde_json::Value::as_str)
                == Some("NotFound"))
}

/// Map a (verbatim-adopted) failure code onto the stable repair diagnostic.
/// Shell codes are referenced — the slice-2 conversion adopted them verbatim
/// and this table never re-mints them — so a shell-derived envelope keeps
/// its identity through the repair turn.
fn diag_for_code(code: &str) -> &'static str {
    match code {
        "shell.process.timeout" => DIAG_TIMEOUT,
        "shell.process.non-zero-exit" => DIAG_NON_ZERO_EXIT,
        "shell.execution.io" | "exec.io_error" => DIAG_EXECUTION_IO,
        _ => DIAG_EXECUTION_FAILED,
    }
}

/// Fail-closed boundary veto: our own gate-rejection envelope and the
/// executor's approval-timeout literal. Exact-shape recognition (trimmed
/// prefix / case-insensitive literal) — not the generic denial-substring
/// grammar, which is opaque-fallback only.
fn is_gate_envelope_or_approval_timeout(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.eq_ignore_ascii_case("approval timed out")
        || trimmed.starts_with("gate rejected the write")
}

/// Classify a failed `shell` tool execution into a repairable execution
/// failure, if any — the BOUNDED OPAQUE FALLBACK (H02 demoted role).
///
/// This is the prose path: it parses `Display` text (denial markers, the
/// gate envelope, not-found heuristics, the EACCES exemption). It runs ONLY
/// when no typed envelope exists or the envelope is `opaque` — genuinely
/// opaque third-party failure text with no native code — and is never
/// consulted when a typed verdict already classified the failure (see
/// [`repair_verdict`], whose typed path takes precedence). Kept (never
/// deleted) so opaque text still earns its bounded coaching turn; the typed
/// path is the retirement road for every shape that gains a native code.
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
///
/// Envelope checks (F8, security review 2026-09-09) run on the TRIMMED
/// message so a denial wrapped with leading/trailing whitespace cannot
/// pretend to be an ordinary execution fault. Genuine spawn-permission
/// faults (EACCES — `permission denied (os error 13)`) are exempted BEFORE
/// the deny-marker scan: their text contains "denied", but they are real
/// process faults (F9) and keep their bounded repair turn — without the
/// exemption the deny scan swallowed them and dropped the repair turn.
fn is_policy_denial_message(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.eq_ignore_ascii_case("approval timed out") {
        return true;
    }
    // The write-gate rejection envelope produced by gate_proxy.rs /
    // ipc.rs: nothing repairable ever rides it, whatever the inner verdict.
    // Checked on the trimmed text (F8) so a padded envelope is classified
    // the same as the exact one.
    if trimmed.starts_with("gate rejected the write") {
        return true;
    }
    // Genuine EACCES spawn faults are execution failures, not policy
    // denials, despite the "denied" substring (F9 exemption). Compared on
    // the lowercased text.
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.contains("permission denied (os error") {
        return false;
    }
    const DENIAL_MARKERS: &[&str] =
        &["gatedenied", "requireapproval", "policydenied", "blocked", "denied", "cancelled"];
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
    use concerto_core::tool_failure::{ToolFailureCategory, ToolFailureContext};
    use serde_json::json;

    fn shell_output(exit_code: i64, stdout: &str, stderr: &str) -> ToolOutput {
        ToolOutput {
            summary: format!("Command `x` exited {exit_code}."),
            data: json!({ "exit_code": exit_code, "stdout": stdout, "stderr": stderr }),
        }
    }

    /// The native envelope for a tool error, exactly as the loop builds it.
    fn envelope_for(error: &ToolError) -> ToolFailure {
        ToolFailure::from_error(error, ToolFailureContext::builtin())
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
    fn untrimmed_gate_envelope_is_never_repairable() {
        // F8 (security review 2026-09-09): the envelope check runs on the
        // TRIMMED message — a denial padded (either side or both) with
        // whitespace used to slip past the exact-prefix check and be turned
        // into a repairable process error, coaching a model around the
        // gate.
        for message in [
            " gate rejected the write (GateDenied): Blocked",
            "gate rejected the write (GateDenied): Blocked ",
            "  gate rejected the write (GateDenied): RequireApproval { timeout: 30s }  ",
            "\n gate rejected the write (GateError): policy evaluation failed",
        ] {
            let error = ToolError::ExecutionFailed { message: message.into() };
            assert!(
                repairable_error_failure(SHELL_TOOL_NAME, &error).is_none(),
                "padded gate envelope is never repairable: {message:?}"
            );
        }
    }

    #[test]
    fn eacces_spawn_faults_keep_the_repair_turn() {
        // F9 exemption (security review 2026-09-09): a genuine EACCES spawn
        // fault is an execution failure, not a policy denial — the "denied"
        // substring used to swallow it and drop the repair turn.
        for message in [
            "failed to spawn process: Permission denied (os error 13)",
            "Permission denied (os error 13)",
            "  Permission denied (os error 13)  ",
        ] {
            let error = ToolError::ExecutionFailed { message: message.into() };
            assert!(
                matches!(
                    repairable_error_failure(SHELL_TOOL_NAME, &error),
                    Some(ShellFailure::ProcessError { .. })
                ),
                "EACCES spawn fault stays repairable: {message}"
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

        assert!(message.starts_with("[shell-repair attempt 1/5]"), "marker missing: {message}");
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
        assert!(message.starts_with("[shell-repair attempt 2/5]"), "marker: {message}");
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

    // -----------------------------------------------------------------------
    // H02: typed verdicts — every disposition maps distinctly, the typed path
    // takes precedence over prose, and the opaque fallback is preserved.
    // -----------------------------------------------------------------------

    /// Every disposition maps to its own verdict: denials and deterministic
    /// faults surface unchanged, cancellations and approval-parks surface
    /// unchanged for their own reasons, rebase conflicts surface unchanged
    /// awaiting reconciliation, and only the retry family earns a turn.
    #[test]
    fn typed_dispositions_map_to_distinct_verdicts() {
        // DoNotReIssue (policy denial): surface unchanged.
        assert_eq!(
            repair_verdict(
                SHELL_TOOL_NAME,
                &ToolError::PolicyDenied { rule: "deny".into() },
                Some(&envelope_for(&ToolError::PolicyDenied { rule: "deny".into() })),
            ),
            RepairVerdict::DoNotRepair
        );
        // DoNotReIssueSameIntent (cancelled): a deliberate stop, never a turn.
        assert_eq!(
            repair_verdict(
                SHELL_TOOL_NAME,
                &ToolError::Cancelled,
                Some(&envelope_for(&ToolError::Cancelled)),
            ),
            RepairVerdict::Cancelled
        );
        // AwaitingDecision (approval park): resolution, not a retry.
        let approval = ToolError::PausedAwaitingApproval {
            tool_name: "shell".into(),
            detail: "rm -rf /".into(),
            input_hash: "hash".into(),
            correlation_id: concerto_core::ids::Ulid::new(),
            timeout_secs: 30,
        };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &approval, Some(&envelope_for(&approval))),
            RepairVerdict::AwaitingDecision
        );
        // BoundedRetryWithBackoff (timeout): earns the timeout-shaped turn.
        let timeout = ToolError::Timeout { timeout_secs: 30 };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &timeout, Some(&envelope_for(&timeout))),
            RepairVerdict::Repair(ShellFailure::Timeout { timeout_secs: 30 })
        );
        // RetryableAfterRebase (genuine overlay conflict): reconcile first.
        let conflict = ToolError::VirtualFsConflict {
            path: "a.txt".into(),
            reason: "the file changed since it was read".into(),
        };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &conflict, Some(&envelope_for(&conflict))),
            RepairVerdict::NeedsRebase
        );
        // Unknown (unmarked execution fault): earns the bounded generic turn.
        let blip =
            ToolError::ExecutionFailed { message: "the model produced malformed JSON".into() };
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &blip, Some(&envelope_for(&blip))),
                RepairVerdict::Repair(ShellFailure::ProcessError {
                    diagnostic: DIAG_EXECUTION_FAILED,
                    ..
                })
            ),
            "an unclassified native fault keeps its bounded coaching turn"
        );
        // Containment conflicts are deterministic: surface unchanged.
        let contained = ToolError::VirtualFsConflict {
            path: "../etc/passwd".into(),
            reason: "path traversal detected — resolved path escapes workspace root".into(),
        };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &contained, Some(&envelope_for(&contained))),
            RepairVerdict::DoNotRepair
        );
    }

    /// The typed path takes precedence over the prose heuristics for every
    /// disposition: when the envelope disagrees with the message text, the
    /// envelope wins.
    #[test]
    fn typed_path_takes_precedence_over_prose() {
        // "file not found" prose would earn a not-found coaching turn on the
        // legacy path — the typed DoNotReIssue verdict refuses it.
        let not_found =
            ToolError::ExecutionFailed { message: "file not found: src/main.rs".into() };
        assert!(
            repairable_error_failure(SHELL_TOOL_NAME, &not_found).is_some(),
            "legacy path still repairs the prose (fallback preserved)"
        );
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &not_found, Some(&envelope_for(&not_found))),
            RepairVerdict::DoNotRepair,
            "typed DoNotReIssue wins over the not-found heuristic"
        );
        // A benign-looking fault with a typed cancellation never repairs.
        let blip = ToolError::ExecutionFailed { message: "a transient blip".into() };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &blip, Some(&envelope_for(&ToolError::Cancelled))),
            RepairVerdict::Cancelled,
            "typed cancellation wins over benign prose"
        );
        // A structurally-typed timeout repairs even when its (absent) text
        // would say nothing — and a typed refusal wins over the variant.
        let timeout = ToolError::Timeout { timeout_secs: 5 };
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &timeout, Some(&envelope_for(&timeout))),
                RepairVerdict::Repair(ShellFailure::Timeout { timeout_secs: 5 })
            ),
            "typed timeout keeps its turn"
        );
        assert_eq!(
            repair_verdict(
                SHELL_TOOL_NAME,
                &timeout,
                Some(&envelope_for(&ToolError::PolicyDenied { rule: "deny".into() })),
            ),
            RepairVerdict::DoNotRepair,
            "typed refusal wins over the timeout variant"
        );
        // A structural Io(NotFound) repairs nothing on its own — but a
        // contradictory typed retry verdict wins over the structural kind.
        let io_missing = ToolError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &io_missing, Some(&envelope_for(&io_missing))),
            RepairVerdict::DoNotRepair
        );
        let mut retrying = envelope_for(&io_missing);
        retrying.disposition = ToolDisposition::BoundedRetryWithBackoff;
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &io_missing, Some(&retrying)),
                RepairVerdict::Repair(_)
            ),
            "typed retry wins over the structural Io kind"
        );
    }

    /// The fail-closed boundary veto holds on the typed path too: gate
    /// envelopes and approval-timeout literals never earn a repair turn, no
    /// matter what disposition the envelope claims. Safety outranks
    /// precedence here — coaching around a gate would be a policy bypass.
    #[test]
    fn boundary_veto_holds_on_the_typed_path() {
        for message in [
            "approval timed out",
            "gate rejected the write (GateDenied): RequireApproval { timeout: 30s }",
            "  gate rejected the write (GateDenied): Blocked  ",
        ] {
            let error = ToolError::ExecutionFailed { message: message.into() };
            let envelope = envelope_for(&error);
            assert_eq!(
                envelope.disposition,
                ToolDisposition::Unknown,
                "the stringly shape carries no typed signal: {message}"
            );
            assert_eq!(
                repair_verdict(SHELL_TOOL_NAME, &error, Some(&envelope)),
                RepairVerdict::DoNotRepair,
                "the veto refuses the repair despite Unknown: {message}"
            );
        }
    }

    /// The demoted opaque fallback still classifies genuinely opaque text:
    /// opaque envelopes (and absent ones) keep the historical heuristics —
    /// denials refused, EACCES exempted, not-found coached — marked opaque.
    #[test]
    fn opaque_fallback_still_classifies_genuinely_opaque_text() {
        fn opaque(message: &str) -> (ToolError, ToolFailure) {
            let error = ToolError::ExecutionFailed { message: message.into() };
            let failure = ToolFailure::from_error(
                &error,
                ToolFailureContext::with_origin(concerto_core::tool_failure::ToolOrigin::Plugin(
                    "third-party".into(),
                )),
            );
            assert!(failure.opaque, "bridge origins are opaque");
            (error, failure)
        }
        // Opaque denial text is still refused.
        let (error, failure) = opaque("denied: outside project root");
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &error, Some(&failure)),
            RepairVerdict::DoNotRepair
        );
        assert!(repairable_error_failure(SHELL_TOOL_NAME, &error).is_none());
        // The EACCES exemption lives on in the fallback only.
        let (error, failure) = opaque("failed to spawn process: Permission denied (os error 13)");
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &error, Some(&failure)),
                RepairVerdict::Repair(_)
            ),
            "opaque EACCES keeps its turn via the fallback exemption"
        );
        // Opaque not-found text keeps its coaching turn.
        let (error, failure) = opaque("shell executable not found on PATH: fruitloop");
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &error, Some(&failure)),
                RepairVerdict::Repair(ShellFailure::ProcessError { not_found: true, .. })
            ),
            "opaque not-found keeps its coaching turn"
        );
        // Absent envelopes take the same fallback.
        assert!(
            matches!(
                repair_verdict(SHELL_TOOL_NAME, &error, None),
                RepairVerdict::Repair(ShellFailure::ProcessError { not_found: true, .. })
            ),
            "absent envelopes fall back identically"
        );
    }

    /// EACCES needs no substring exemption on the typed path: the structural
    /// `PermissionDenied` kind maps to `DoNotReIssue`, so the typed verdict
    /// refuses without ever reading the "denied" text.
    #[test]
    fn eacces_needs_no_exemption_on_the_typed_path() {
        let io_denied = ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        ));
        let envelope = envelope_for(&io_denied);
        assert_eq!(envelope.disposition, ToolDisposition::DoNotReIssue);
        assert_eq!(envelope.category, ToolFailureCategory::Execution);
        assert!(!envelope.opaque);
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &io_denied, Some(&envelope)),
            RepairVerdict::DoNotRepair,
            "structural Io covers the EACCES case the old heuristic exempted"
        );
        // The stringly ExecutionFailed shape is deterministic too — and the
        // typed verdict refuses it without the exemption either.
        let text_denied = ToolError::ExecutionFailed {
            message: "failed to spawn process: Permission denied (os error 13)".into(),
        };
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &text_denied, Some(&envelope_for(&text_denied))),
            RepairVerdict::DoNotRepair
        );
    }

    /// A missing executable is deterministic and earns NO repair turn. This
    /// locks the shared marker's lowercase invariant: the fold lowercases the
    /// message (`"...on PATH"` → `"...on path"`) but not the marker, so a
    /// marker left in mixed case would silently never match and the failure
    /// would be classified `Unknown` — earning a pointless repair attempt for
    /// a provably impossible action.
    #[test]
    fn missing_shell_executable_is_deterministic_on_the_typed_path() {
        let error = ToolError::ExecutionFailed {
            message: "shell executable not found on PATH: fruitloop".into(),
        };
        let envelope = envelope_for(&error);
        assert_eq!(
            envelope.disposition,
            ToolDisposition::DoNotReIssue,
            "the shared marker must match the lowercased message"
        );
        assert_eq!(
            repair_verdict(SHELL_TOOL_NAME, &error, Some(&envelope)),
            RepairVerdict::DoNotRepair,
            "a missing executable cannot succeed on an identical re-issue"
        );
        // The marker table is lowercase by contract, so no marker may carry
        // an uppercase byte (the exact defect that made the PATH marker dead).
        for marker in concerto_core::error::deterministic_failure_markers_for_test() {
            assert_eq!(
                marker.to_lowercase(),
                *marker,
                "deterministic failure markers must be lowercase: {marker:?}"
            );
        }
    }

    /// Shell-derived envelopes keep their verbatim codes through the repair
    /// turn: the timeout code earns the timeout shape, the non-zero-exit
    /// code keeps its diagnostic, and the program-not-found code derives
    /// the not-found hint — all without reading message text.
    #[test]
    fn shell_codes_keep_identity_through_repair() {
        // shell.process.timeout → the timeout diagnostic on the generic
        // shape: a shell-derived envelope carries no structured seconds in
        // its detail, so it must not claim `Timeout { timeout_secs }` — the
        // message already states the timeout.
        let mut timeout = envelope_for(&ToolError::Timeout { timeout_secs: 9 });
        timeout.code = "shell.process.timeout".to_string();
        assert!(
            matches!(
                verdict_from_failure(&timeout),
                RepairVerdict::Repair(ShellFailure::ProcessError { diagnostic: DIAG_TIMEOUT, .. })
            ),
            "the verbatim shell timeout code keeps its diagnostic"
        );
        // shell.process.non-zero-exit → its own diagnostic.
        let mut nonzero = envelope_for(&ToolError::ExecutionFailed { message: "exit 2".into() });
        nonzero.code = "shell.process.non-zero-exit".to_string();
        nonzero.disposition = ToolDisposition::Unknown;
        assert!(
            matches!(
                verdict_from_failure(&nonzero),
                RepairVerdict::Repair(ShellFailure::ProcessError {
                    diagnostic: DIAG_NON_ZERO_EXIT,
                    not_found: false,
                    ..
                })
            ),
            "the verbatim shell code keeps its diagnostic"
        );
        // shell.profile.program-not-found → the not-found hint, structurally.
        let mut missing = envelope_for(&ToolError::ExecutionFailed {
            message: "fruitloop: command not found".into(),
        });
        missing.code = "shell.profile.program-not-found".to_string();
        missing.disposition = ToolDisposition::Unknown;
        assert!(
            matches!(
                verdict_from_failure(&missing),
                RepairVerdict::Repair(ShellFailure::ProcessError { not_found: true, .. })
            ),
            "the stable shell code derives not-found without prose"
        );
        // exec.timeout detail carries the seconds structurally.
        let mut detail_timeout = envelope_for(&ToolError::Timeout { timeout_secs: 45 });
        assert_eq!(detail_timeout.code, "exec.timeout");
        assert!(
            matches!(
                verdict_from_failure(&detail_timeout),
                RepairVerdict::Repair(ShellFailure::Timeout { timeout_secs: 45 })
            ),
            "timeout seconds ride the envelope detail"
        );
        detail_timeout.detail = serde_json::Value::Null;
        assert_eq!(
            verdict_from_failure(&detail_timeout),
            RepairVerdict::Repair(ShellFailure::Timeout { timeout_secs: 0 }),
            "absent detail degrades to zero, never to prose parsing"
        );
        // exec.io_error with a structural NotFound io_kind derives not-found.
        let mut io_envelope = envelope_for(&ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )));
        io_envelope.code = "exec.io_error".to_string();
        io_envelope.disposition = ToolDisposition::Unknown;
        io_envelope.detail = json!({ "io_kind": "NotFound" });
        assert!(
            matches!(
                verdict_from_failure(&io_envelope),
                RepairVerdict::Repair(ShellFailure::ProcessError { not_found: true, .. })
            ),
            "structural io_kind derives not-found"
        );
    }

    /// Non-shell tools never earn a repair turn on either path.
    #[test]
    fn other_tools_never_repair_on_the_typed_path() {
        let timeout = ToolError::Timeout { timeout_secs: 5 };
        assert_eq!(
            repair_verdict("filesystem", &timeout, Some(&envelope_for(&timeout))),
            RepairVerdict::DoNotRepair
        );
    }
}
