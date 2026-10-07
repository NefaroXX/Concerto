//! MCP draft validation (Settings → Extensions → MCP).
//!
//! The pure, state-free half of the MCP add/edit drafts: the server-id rules,
//! the command requirement, the timeout hard cap, and the JSON argument-array
//! parser shared by both drafts. [`super::state::State`] keeps thin delegates
//! so the `update` arms and their inline error strings stay where they are;
//! nothing here reads or writes state, so every rule is unit-tested in
//! isolation. The same messages are asserted byte-for-byte by `super::state`'s
//! round-trip tests — keep the copy identical.

use concerto_config::McpServerConfig;

/// Validate the timeout field of the MCP edit draft. Blank is valid (the
/// crate default is 60s); otherwise the value must be a whole number in
/// `1..=300` — the hard cap the MCP bridge enforces.
pub(super) fn validate_mcp_timeout(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None; // blank is valid (means "use the 60s default")
    }
    match trimmed.parse::<u64>() {
        Ok(0) => Some("Must be a positive number".into()),
        Ok(v) if v > 300 => Some("Hard cap is 300 seconds".into()),
        Ok(_) => None,
        Err(_) => Some("Must be a whole number".into()),
    }
}

/// Validate the id of an MCP add draft against the configured servers.
/// Mirrors `McpConfig::validate` (ADR-43 §4): non-empty, no `:` — tools
/// are namespaced `mcp:<server_id>:<tool_name>` — and unique.
pub(super) fn validate_mcp_add_id(mcp_servers: &[McpServerConfig], id: &str) -> Option<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Some("Id is required".into());
    }
    if trimmed.contains(':') {
        return Some("Id must not contain ':'".into());
    }
    if mcp_servers.iter().any(|server| server.id == trimmed) {
        return Some("An MCP server with this id already exists".into());
    }
    None
}

/// Validate the command field of an MCP add/edit draft: a server needs an
/// executable to spawn, so blank (after trimming) is rejected with the one
/// inline message both drafts show.
pub(super) fn validate_mcp_command(command: &str) -> Option<String> {
    if command.trim().is_empty() {
        Some("Command is required".into())
    } else {
        None
    }
}

/// Parse the `args` field of an MCP draft: blank means "no arguments",
/// otherwise the input must be a JSON array of strings. Returns the parsed
/// list, or the inline error shown next to the field — the save paths block
/// on the error half (`.err()`), the apply paths take the list.
pub(super) fn parse_mcp_args(input: &str) -> Result<Vec<String>, String> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(input).map_err(|_| {
        "Arguments must be a JSON array of strings, for example [\"-y\", \"path with spaces\"]"
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured server, for the uniqueness rule of [`validate_mcp_add_id`].
    fn configured(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            command: "npx".to_string(),
            args: Vec::new(),
            env: None,
            enabled: true,
            timeout_secs: None,
        }
    }

    // ── validate_mcp_timeout ─────────────────────────────────────────────

    #[test]
    fn timeout_accepts_blank_trimmed_and_in_range_values() {
        assert_eq!(validate_mcp_timeout(""), None, "blank means the 60s default");
        assert_eq!(validate_mcp_timeout("   "), None, "whitespace-only is blank too");
        assert_eq!(validate_mcp_timeout("1"), None, "the 1s floor is inclusive");
        assert_eq!(validate_mcp_timeout("300"), None, "the 300s cap is inclusive");
        assert_eq!(validate_mcp_timeout("  60  "), None, "input is trimmed before parsing");
    }

    #[test]
    fn timeout_rejects_zero_over_cap_and_non_integer_input() {
        assert_eq!(validate_mcp_timeout("0").as_deref(), Some("Must be a positive number"));
        assert_eq!(validate_mcp_timeout("301").as_deref(), Some("Hard cap is 300 seconds"));
        assert_eq!(validate_mcp_timeout("-1").as_deref(), Some("Must be a whole number"));
        assert_eq!(validate_mcp_timeout("60.5").as_deref(), Some("Must be a whole number"));
        assert_eq!(validate_mcp_timeout("soon").as_deref(), Some("Must be a whole number"));
    }

    // ── validate_mcp_add_id ──────────────────────────────────────────────

    #[test]
    fn add_id_accepts_a_fresh_id_and_trims_before_comparing() {
        let servers = [configured("files")];
        assert_eq!(validate_mcp_add_id(&servers, "github"), None);
        assert_eq!(validate_mcp_add_id(&servers, "  github  "), None, "input is trimmed first");
        assert_eq!(validate_mcp_add_id(&[], "anything"), None, "no servers means no clash");
    }

    #[test]
    fn add_id_rejects_blank_colon_and_duplicate_ids() {
        let servers = [configured("files"), configured("tools")];
        assert_eq!(validate_mcp_add_id(&servers, "").as_deref(), Some("Id is required"));
        assert_eq!(validate_mcp_add_id(&servers, "   ").as_deref(), Some("Id is required"));
        assert_eq!(
            validate_mcp_add_id(&servers, "a:b").as_deref(),
            Some("Id must not contain ':'")
        );
        assert_eq!(
            validate_mcp_add_id(&servers, " files ").as_deref(),
            Some("An MCP server with this id already exists"),
            "uniqueness compares the trimmed id"
        );
    }

    // ── validate_mcp_command ─────────────────────────────────────────────

    #[test]
    fn command_rejects_blank_and_accepts_any_executable() {
        assert_eq!(validate_mcp_command("").as_deref(), Some("Command is required"));
        assert_eq!(validate_mcp_command("   ").as_deref(), Some("Command is required"));
        assert_eq!(validate_mcp_command("npx"), None);
        assert_eq!(validate_mcp_command("  cargo run  "), None, "outer spaces are not blank");
    }

    // ── parse_mcp_args ───────────────────────────────────────────────────

    #[test]
    fn parse_args_treats_blank_as_no_arguments() {
        assert_eq!(parse_mcp_args(""), Ok(Vec::new()));
        assert_eq!(parse_mcp_args("   "), Ok(Vec::new()));
        assert_eq!(
            parse_mcp_args(r#"["-y", "path with spaces"]"#),
            Ok(vec!["-y".to_string(), "path with spaces".to_string()]),
            "spaces inside quoted arguments are preserved"
        );
    }

    #[test]
    fn parse_args_rejects_everything_that_is_not_a_string_array() {
        let expected: Result<Vec<String>, String> = Err(
            "Arguments must be a JSON array of strings, for example [\"-y\", \"path with spaces\"]"
                .into(),
        );
        assert_eq!(parse_mcp_args("not json"), expected);
        assert_eq!(parse_mcp_args("[1]"), expected, "arrays of non-strings fail closed");
        assert_eq!(parse_mcp_args(r#"{"args": []}"#), expected, "an object is not an array");
    }
}
