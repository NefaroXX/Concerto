//! Provenance checks for Coordinator completion evidence.
//!
//! Tool facts attest execution, not canonical harness verification. Keep the
//! two meanings separate until specialist results carry `VerificationSummary`.

use concerto_sessions::whiteboard::{WhiteboardEvent, WhiteboardKind};
use concerto_sessions::ToolExecutedPayload;

pub(crate) fn matches_run(recorded: Option<&str>, current: Option<&str>) -> bool {
    matches!((recorded, current), (Some(recorded), Some(current))
        if !current.is_empty() && recorded == current)
}

pub(crate) fn successful_fact(
    event: &WhiteboardEvent,
    run_id: Option<&str>,
) -> Option<ToolExecutedPayload> {
    if event.kind != WhiteboardKind::ToolExecuted {
        return None;
    }
    let fact: ToolExecutedPayload = serde_json::from_value(event.payload.clone()).ok()?;
    if !matches_run(fact.run_id.as_deref(), run_id)
        || !fact.success
        || fact.failure.is_some()
        || fact.exit_code.is_some_and(|code| code != 0)
    {
        return None;
    }
    // `outcome` is newer than the derived-store payload type. Consult the
    // original event rather than silently discarding a contradictory value.
    if let Some(outcome) = event.payload.get("outcome") {
        if outcome.as_str() != Some("ok") {
            return None;
        }
    }
    Some(fact)
}

/// Conservative recognition of a check invocation. A runner's help, listing,
/// formatting writes, arbitrary npm script, or compound shell expression is
/// not evidence that a build/test/lint check actually ran.
pub(crate) fn is_build_or_test_command(args: &serde_json::Value) -> bool {
    let Some(command) = args.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let mut parts = vec![command];
    if let Some(values) = args.get("args") {
        let Some(values) = values.as_array() else { return false };
        for value in values {
            let Some(value) = value.as_str() else { return false };
            parts.push(value);
        }
    }
    let command = parts.join(" ");
    if command.chars().any(|ch| matches!(ch, ';' | '|' | '&' | '<' | '>' | '`' | '$' | '\n' | '\r')) {
        return false;
    }
    let tokens: Vec<_> = command.split_whitespace().collect();
    if tokens.iter().any(|token| {
        matches!(*token, "-h" | "--help" | "-V" | "--version" | "--list" | "--list-tests"
            | "--no-run" | "--collect-only" | "--dry-run")
    }) {
        return false;
    }
    let first = tokens.first().copied().unwrap_or("");
    let second = tokens.get(1).copied().unwrap_or("");
    let third = tokens.get(2).copied().unwrap_or("");
    match first {
        "cargo" => match second {
            "build" | "check" | "test" | "clippy" | "audit" => true,
            "fmt" => tokens.contains(&"--check"),
            "nextest" => third == "run",
            "deny" => third == "check",
            _ => false,
        },
        "make" => matches!(second, "test" | "check" | "lint" | "build" | "verify"),
        "npm" | "yarn" | "pnpm" => second == "test"
            || (second == "run" && matches!(third, "test" | "build" | "lint" | "check" | "typecheck" | "verify")),
        "pytest" => true,
        "python" | "python3" => second == "-m" && third == "pytest",
        "go" => matches!(second, "build" | "test" | "vet"),
        "dotnet" => matches!(second, "build" | "test"),
        "mvn" => matches!(second, "test" | "verify" | "package"),
        "gradle" | "./gradlew" => matches!(second, "test" | "build"),
        "tsc" => !tokens.contains(&"--watch") && !tokens.contains(&"-w"),
        "eslint" => !tokens.contains(&"--fix"),
        "rustfmt" => tokens.contains(&"--check"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(payload: serde_json::Value) -> WhiteboardEvent {
        WhiteboardEvent {
            event_id: "fact-1".into(), gate_seq: 1, agent_id: "coder".into(), agent_seq: 1,
            kind: WhiteboardKind::ToolExecuted, scope: String::new(), session_id: Some("s".into()),
            plan_id: None, causation: None, payload, content_hash: String::new(),
            pre_image_hash: None, created_at: 0,
        }
    }

    #[test]
    fn current_run_is_required_even_for_legacy_success() {
        let mut payload = serde_json::json!({"success": true, "run_id": "current"});
        assert!(successful_fact(&event(payload.clone()), Some("current")).is_some());
        assert!(successful_fact(&event(payload.clone()), Some("other")).is_none());
        assert!(successful_fact(&event(payload.clone()), None).is_none());
        payload["run_id"] = serde_json::Value::Null;
        assert!(successful_fact(&event(payload), Some("current")).is_none());
        assert!(!matches_run(Some(""), Some("")));
    }

    #[test]
    fn contradictory_outcomes_and_exit_codes_never_attest_success() {
        for outcome in ["failed", "denied", "interrupted", "skipped", "unavailable", "unknown"] {
            assert!(successful_fact(&event(serde_json::json!({
                "success": true, "outcome": outcome, "run_id": "r"
            })), Some("r")).is_none());
        }
        assert!(successful_fact(&event(serde_json::json!({
            "success": true, "outcome": "ok", "exit_code": 1, "run_id": "r"
        })), Some("r")).is_none());
        assert!(successful_fact(&event(serde_json::json!({
            "success": false, "outcome": "ok", "exit_code": 0, "run_id": "r"
        })), Some("r")).is_none());
        assert!(successful_fact(&event(serde_json::json!({
            "success": true, "outcome": "ok", "exit_code": 0, "run_id": "r"
        })), Some("r")).is_some());
        let failure = concerto_core::tool_failure::ToolFailure::from_error(
            &concerto_core::ToolError::Timeout { timeout_secs: 1 },
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert!(successful_fact(&event(serde_json::json!({
            "success": true, "outcome": "ok", "run_id": "r", "failure": failure
        })), Some("r")).is_none());
    }

    #[test]
    fn check_classifier_rejects_non_checks_and_compound_commands() {
        for command in ["cargo test --help", "cargo test --no-run", "cargo nextest list",
            "cargo deny --version", "cargo fmt", "rustfmt src/lib.rs", "npm run dev",
            "make", "pytest --collect-only", "cargo test; false", "cargo test || true",
            "cargo test && echo done", "cargo test\nfalse", "eslint --fix .", "tsc --watch"] {
            assert!(!is_build_or_test_command(&serde_json::json!({"command": command})), "{command}");
        }
        for command in ["cargo test", "cargo nextest run", "cargo fmt -- --check",
            "cargo deny check", "make test", "npm run lint", "pytest", "rustfmt --check src/lib.rs"] {
            assert!(is_build_or_test_command(&serde_json::json!({"command": command})), "{command}");
        }
        assert!(!is_build_or_test_command(&serde_json::json!({"command": "cargo", "args": ["test", false]})));
    }
}
