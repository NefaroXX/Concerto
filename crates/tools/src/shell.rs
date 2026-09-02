//! Shell tool implementation — async, cancellable, sandboxed process execution.

use crate::common::canonicalize_within;
use crate::containment::contain_shell_command;
use crate::process::{ProcessHandle, ProcessOutput};
use crate::shell_backend::ShellProfileFactory;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use concerto_config::shell::ShellProfileConfig;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{
    CapabilitySet, CommandPolicyFacts, DestructiveClass, FilesystemScope, SessionContext,
    ToolOutput,
};
use concerto_core::{CancellationToken, ToolError};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Default timeout for shell commands when not specified by the caller.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Hard upper bound for user-specified timeouts (5 minutes).
const MAX_TIMEOUT_SECS: u64 = 300;

/// Maximum allowed command string length (characters).
const MAX_COMMAND_LENGTH: usize = 4096;

/// Maximum allowed number of arguments.
const MAX_ARGS_COUNT: usize = 100;

/// Hardcoded deny patterns that are always rejected regardless of config.
const HARDCODED_DENY_PATTERNS: &[&str] = &[
    // `rm` with both -r* and -f* flags (any order, any spelling incl.
    // --recursive/--force) targeting `/`, `~`, `*`, or `.` (the bare dot
    // meaning `cwd` — NOT `./path` which is project-relative and safe).
    // Audited bypass: `rm -fr /`, `rm -r -f /`, `rm --recursive --force /`
    // all slipped past the original `rm\s+-rf\s+/` substring scan. The
    // `regex` crate doesn't support lookahead, so we enumerate the order
    // variants explicitly. Combined short-flags `-rf`/`-fr` are matched by
    // the first two patterns; separated `-r ... -f` style flags by the
    // next two; long-flag forms by the last two.
    // The target set is `(?:/|~|\*|\.(?:\s|$))` — `/`, `~`, `*`, or `.` at
    // end-of-input (with optional trailing whitespace), explicitly NOT
    // `./anything` which means a project-relative path.
    r"\brm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+(?:/|~|\*|\.(?:\s|$))",
    r"\brm\s+-[a-zA-Z]*f[a-zA-Z]*r[a-zA-Z]*\s+(?:/|~|\*|\.(?:\s|$))",
    r"\brm\s+(?:-\w+\s+)*-\w*r\w*\s+(?:-\w+\s+)*-\w*f\w*\s+(?:/|~|\*|\.(?:\s|$))",
    r"\brm\s+(?:-\w+\s+)*-\w*f\w*\s+(?:-\w+\s+)*-\w*r\w*\s+(?:/|~|\*|\.(?:\s|$))",
    r"\brm\s+(?:-\w+\s+)*--recursive(?:\s+--force)?\s+(?:/|~|\*|\.(?:\s|$))",
    r"\brm\s+(?:-\w+\s+)*--force(?:\s+--recursive)?\s+(?:/|~|\*|\.(?:\s|$))",
    // dd writing from image to anything (write side rejected separately).
    r"\bdd\s+if=",
    // Filesystem-format commands.
    r"\bmkfs(?:\.\w+)?\b",
    // Classic bash fork bomb: `:(){ :|:& };:`.
    r":\s*\(\s*\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:",
];

/// Input schema for the shell tool.
///
/// The JSON schema advertised to models is derived from this struct (see
/// [`ShellTool::input_schema`]), so the deserialization target and the
/// advertised contract cannot drift. `command` is the only required field;
/// `args` carries `#[serde(default)]` and the remaining fields are `Option`,
/// so the derived schema marks them optional.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShellInput {
    #[schemars(description = "The shell command to execute.")]
    pub command: String,
    /// Defaults to an empty argument list so callers (and LLM tool calls that
    /// omit `args`, which the schema only requires `command`) can run
    /// argument-less commands without a deserialization error.
    #[serde(default)]
    #[schemars(description = "Arguments passed to the command.")]
    pub args: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Optional working directory for the command.")]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional execution timeout in seconds.")]
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Guard heuristic inference (adaptive tool-guard Solution 3)
// ---------------------------------------------------------------------------

/// Alias keys weak models emit for the canonical `command` field.
const COMMAND_ALIASES: [&str; 2] = ["cmd", "action"];

/// Conservative heuristic inference for a missing required `command` argument
/// (adaptive tool-guard Solution 3: last-mile adaptability for weak
/// tool-calling models).
///
/// Called by the orchestrator's tool-call guard only when `command` is absent
/// or `null` after parse+coerce. `raw` is the model's ORIGINAL argument object
/// (pre-coercion, so hallucinated alias keys are still present) and `missing`
/// lists the unresolved required field names. Returns `(field, value)`
/// insertions for the guard to apply; the guard re-coerces and re-validates
/// the completed arguments, so a wrong guess can never reach the executor.
///
/// Alias recovery only: `cmd`/`action` → `command` (non-empty string values).
/// No command is ever synthesized from prose, the tool name, or `args` — a
/// guessed command would be executed, so any ambiguity must fall through to
/// the guard's corrective reject instead. Policy (allowlist/denylist) still
/// gates whatever command ends up running.
pub fn infer_missing_arguments(
    raw: &serde_json::Map<String, serde_json::Value>,
    missing: &[String],
) -> Vec<(String, serde_json::Value)> {
    if !missing.iter().any(|field| field == "command") {
        return Vec::new();
    }
    COMMAND_ALIASES
        .iter()
        .find_map(|alias| {
            raw.get(*alias)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|command| {
                    ("command".to_string(), serde_json::Value::String(command.to_string()))
                })
        })
        .into_iter()
        .collect()
}

/// Configuration for allowlist/denylist filtering of shell commands.
///
/// # Deny-by-default
///
/// When `allowlist` is empty no command is permitted.  Operators must
/// explicitly populate the allowlist with the commands a model is
/// trusted to run.  The denylist always applies as an additional
/// block on top of allowed commands.
///
/// # Shell wrapping
///
/// By default (`bypass_shell: false`, `shell: None`), commands are wrapped in
/// the OS default shell (`$SHELL` on Unix, `%COMSPEC%` on Windows) so that
/// shell features (pipes, redirects, variable expansion) work.  Set
/// `bypass_shell: true` to execute the command binary directly (the previous
/// behaviour).  Set `shell: Some("/path/to/shell")` to override the shell
/// binary (e.g. for MSYS2 on Windows).
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Regex patterns that commands MUST match to be permitted.
    /// When empty, all commands are denied (deny-by-default).
    pub allowlist: Vec<Regex>,
    /// Regex patterns that commands must NOT match (always applied).
    pub denylist: Vec<Regex>,
    /// Path to the shell binary.  `None` (default) = auto-detect OS default
    /// shell.  Set to `Some("/path/to/shell")` to force a specific shell
    /// (e.g. `"C:\\msys64\\usr\\bin\\bash.exe"` for MSYS2).
    pub shell: Option<String>,
    /// When `true`, execute the command binary directly without shell
    /// wrapping.  Default is `false` (use shell).
    pub bypass_shell: bool,
    /// When `true`, skip the allowlist check (the denylist still always
    /// applies). Opt-in escape hatch for local runs the user has approved;
    /// used by [`ShellTool::allow_all`]. Default is `false`.
    pub allow_all: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
        }
    }
}

/// Builds the hardcoded denylist regex patterns.
fn build_hardcoded_denylist() -> Vec<Regex> {
    HARDCODED_DENY_PATTERNS.iter().filter_map(|pattern| Regex::new(pattern).ok()).collect()
}

/// Shell-quote a single argument for the configured shell.
///
/// On POSIX (sh/bash/zsh/dash), wrap in single quotes and escape any
/// embedded single quote as `'\''`. On Windows cmd, wrap in double
/// quotes and escape embedded double quotes/backslashes/`%` per the
/// CRT rules. The goal is that the resulting token is treated as a
/// literal by the shell — never parsed as a metacharacter.
fn shell_quote(arg: &str) -> String {
    if cfg!(unix) {
        // POSIX single-quote escape: 'arg' -> '\''\''arg'\'' '\''
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('\'');
        for c in arg.chars() {
            if c == '\'' {
                // POSIX: close the single-quoted string, add an escaped
                // single quote, then reopen the single-quoted string. This
                // produces the literal `'` in the arg as a 4-char sequence
                // `'\''` rather than the 5-char `''\''`, which would leak
                // an extra `'` into the argument and could break anchoring
                // on patterns designed for the canonical escape.
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        out.push('\'');
        out
    } else {
        // Windows cmd quoting. Wrap in double quotes, double any embedded
        // double quotes, and escape backslashes that precede a quote or
        // end of string. The `%` is also escaped as `%%` to prevent
        // %VAR% expansion in cmd.
        let needs_quoting = arg.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t' | '"' | '\\' | '%' | '<' | '>' | '|' | '&' | '^' | '(' | ')' | ';' | ','
            )
        });
        if !needs_quoting {
            return arg.to_string();
        }
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        // Count trailing backslashes so we can double them before the closing quote.
        let trailing_backslashes = arg.chars().rev().take_while(|&c| c == '\\').count();
        for (i, c) in arg.char_indices() {
            match c {
                '"' => {
                    out.push_str("\\\"");
                }
                '\\' => {
                    // Double backslashes that precede a closing quote (at end-of-string
                    // or right before the closing quote we'll append). The simplest
                    // correct rule for our usage: double every backslash that's at a
                    // position where the remaining string is all-backslashes OR where
                    // the next char is a quote.
                    let remaining_after =
                        arg[i..].chars().skip(1).take_while(|&c| c == '\\').count();
                    let is_trailing_run = remaining_after
                        == arg[i..].chars().count().saturating_sub(1)
                        && (arg.len() - i - remaining_after - 1) == 0;
                    if is_trailing_run {
                        out.push('\\');
                        out.push('\\'); // double it
                    } else {
                        out.push('\\');
                    }
                }
                '%' => {
                    out.push_str("%%");
                }
                _ => out.push(c),
            }
        }
        // Double the trailing-backslash run before the closing quote.
        for _ in 0..trailing_backslashes {
            out.push('\\');
        }
        out.push('"');
        out
    }
}

/// Builds the full command string from command and args with proper quoting.
fn build_full_command(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        let mut quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
        let mut out = command.to_string();
        for q in &quoted {
            out.push(' ');
            out.push_str(q);
        }
        let _ = &mut quoted; // silence unused-mut if any
        out
    }
}

/// Map a Git-Bash/MSYS-style `/c/...` cwd (no drive prefix) to its Windows
/// absolute form so is_absolute()/join()/canonicalize_within() see a proper
/// drive path instead of folding it under the project root (`\\?\C:\c\...`).
#[cfg(not(windows))]
fn normalize_msys_cwd(cwd: &str) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Borrowed(cwd)
}
#[cfg(windows)]
fn normalize_msys_cwd(cwd: &str) -> std::borrow::Cow<'_, str> {
    crate::containment::msys_drive_to_windows(cwd)
        .map(std::borrow::Cow::Owned)
        .unwrap_or_else(|| std::borrow::Cow::Borrowed(cwd))
}

/// Detects the OS default shell path.
///
/// On Unix: respects `$SHELL` env var, falling back to `/bin/sh`.
/// On Windows: respects `%COMSPEC%` env var, falling back to `cmd.exe`.
pub fn detect_os_default_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

/// Shell tool — executes arbitrary shell commands with sandboxing,
/// cancellation, timeout, and regex-based filtering.
pub struct ShellTool {
    config: ShellConfig,
    /// Optional shell profile (ADR-28). When set, the agent shell runs through
    /// the selected profile's executable/args/env instead of the hardcoded OS
    /// default. `None` preserves the legacy `ShellConfig` behaviour.
    profile: Option<ShellProfileConfig>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    /// Creates a new `ShellTool` with the default hardcoded denylist.
    pub fn new() -> Self {
        Self { config: ShellConfig::default(), profile: None }
    }

    /// Creates a `ShellTool` with a custom configuration.
    pub fn with_config(config: ShellConfig) -> Self {
        Self { config, profile: None }
    }

    /// Creates a `ShellTool` driven by a configured shell profile (ADR-28).
    ///
    /// `allow_all` lifts the tool's own deny-by-default allowlist (the policy
    /// engine remains the real gate) — used for agent execution the user has
    /// already approved, matching the legacy `ShellTool::allow_all` behaviour.
    pub fn with_profile(profile: ShellProfileConfig, allow_all: bool) -> Self {
        Self { config: ShellConfig { allow_all, ..Default::default() }, profile: Some(profile) }
    }

    /// Creates a `ShellTool` that permits all commands. The policy engine and
    /// approval sink remain the actual gate; this only lifts the tool's own
    /// empty-allowlist deny-by-default so local agent runs can execute shell
    /// commands that the user has approved. The hardcoded denylist (e.g.
    /// `rm -rf /`, `dd`, `mkfs`) still always applies.
    pub fn allow_all() -> Self {
        Self { config: ShellConfig { allow_all: true, ..Default::default() }, profile: None }
    }

    /// Creates an allow-all tool that spawns the requested executable
    /// directly. This is intended for typed adapters that have already chosen
    /// an interpreter and pass its arguments separately. Central policy,
    /// approval, sandboxing, and the hardcoded denylist still apply.
    pub fn allow_all_direct() -> Self {
        Self {
            config: ShellConfig { bypass_shell: true, allow_all: true, ..Default::default() },
            profile: None,
        }
    }

    /// Validates the full command string against allowlist and denylist.
    fn validate_input(&self, command: &str, args: &[String]) -> Result<(), ToolError> {
        if command.len() > MAX_COMMAND_LENGTH {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "command exceeds maximum length of {MAX_COMMAND_LENGTH} characters (got {})",
                    command.len()
                ),
            });
        }
        if args.len() > MAX_ARGS_COUNT {
            return Err(ToolError::ExecutionFailed {
                message: format!("too many arguments: max {MAX_ARGS_COUNT}, got {}", args.len()),
            });
        }
        Ok(())
    }

    fn validate_command(&self, command: &str, args: &[String]) -> Result<(), ToolError> {
        // Build the raw (unquoted) string for denylist matching so that
        // patterns like `rm -rf /` match even when args contain shell
        // metacharacters that would be escaped by shell-quoting.
        let raw_command = if args.is_empty() {
            command.to_string()
        } else {
            let mut raw = command.to_string();
            for a in args {
                raw.push(' ');
                raw.push_str(a);
            }
            raw
        };

        // Check denylist against the raw unquoted string.
        for pattern in &self.config.denylist {
            if pattern.is_match(&raw_command) {
                return Err(ToolError::PolicyDenied {
                    rule: format!("command matched deny pattern: {}", pattern.as_str()),
                });
            }
        }

        // Opt-in allow-all bypass: the denylist above still applies, but the
        // allowlist check is skipped. Used for local runs the user approved.
        if self.config.allow_all {
            return Ok(());
        }

        // Deny-by-default: empty allowlist = no commands permitted.
        if self.config.allowlist.is_empty() {
            return Err(ToolError::PolicyDenied {
                rule: "shell is deny-by-default: no allowlist patterns are configured".into(),
            });
        }

        // Check allowlist against the *quoted* command string (what the shell
        // actually sees). This is important: anchored patterns like `^echo( .*)?$`
        // continue to match when args contain quotes.
        let full_command = build_full_command(command, args);
        let allowed = self.config.allowlist.iter().any(|pattern| pattern.is_match(&full_command));
        if !allowed {
            return Err(ToolError::PolicyDenied {
                rule: "command did not match any allowlist pattern".into(),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl concerto_core::traits::tool::Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command with arguments, optional working directory, and timeout."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Derive the schema from the Rust type so the advertised contract can
        // never drift from the deserialization target. Requiredness follows
        // the struct: `command` (no default) is required; `args` (`#[serde(
        // default)]`), `cwd`, and `timeout_secs` (`Option`) are optional.
        let root = schemars::schema_for!(ShellInput);
        let mut value = serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize ShellInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": ["string", "null"] },
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 0 }
                },
                "required": ["command"]
            })
        });
        if let Some(obj) = value.as_object_mut() {
            // Some tool-calling APIs accept only a restricted JSON Schema subset
            // and reject dialect/definition keywords.
            obj.remove("$schema");
            obj.remove("$defs");
            obj.remove("definitions");
        }
        value
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // NOTE: The shell is deny-by-default.  The allowlist in
        // `ShellConfig` controls which commands a model may run.
        // This capability requirement is still broad because the
        // real gate is the allowlist check inside `execute()`.
        // Coarse flag vocabulary matching the agent capability flags.
        CapabilitySet::default().with_requirement("shell")
    }

    /// ADR-28 §6: produce structured command-execution facts for the policy
    /// engine and audit log. The executor merges these into the single gated
    /// `PolicyAction` so a managed/custom shell environment cannot become a
    /// policy bypass by hiding behind a raw command string.
    fn command_facts(
        &self,
        input: &serde_json::Value,
        session: &SessionContext,
    ) -> Option<CommandPolicyFacts> {
        let shell_input: ShellInput = serde_json::from_value(input.clone()).ok()?;
        let full_command = build_full_command(&shell_input.command, &shell_input.args);

        // Describe the executable and argv that will actually be spawned. A
        // profile/legacy shell is the launcher; direct mode launches the
        // requested program itself.
        let (resolved_executable, argv) = if let Some(profile) = &self.profile {
            let backend = ShellProfileFactory::backend_for(profile);
            let program = backend.resolved_program(profile);
            let arguments = backend.command_args(profile, &full_command);
            (
                Some(program.clone()),
                std::iter::once(program.to_string_lossy().into_owned()).chain(arguments).collect(),
            )
        } else if self.config.bypass_shell {
            (
                resolve_program_in_path(&shell_input.command),
                std::iter::once(shell_input.command.clone())
                    .chain(shell_input.args.iter().cloned())
                    .collect(),
            )
        } else {
            let shell = self.config.shell.clone().unwrap_or_else(detect_os_default_shell);
            let shell_arg = if cfg!(unix) { "-c" } else { "/C" };
            (
                resolve_program_in_path(&shell),
                vec![shell, shell_arg.to_owned(), full_command.clone()],
            )
        };

        let network_requested = command_looks_networked(&shell_input.command, &shell_input.args);
        let destructive_classification = DestructiveClass::classify_command(&full_command);

        let project_dir = &session.project_dir;
        let working_directory = match shell_input.cwd.as_ref() {
            Some(cwd) => {
                let cwd = PathBuf::from(normalize_msys_cwd(cwd).as_ref());
                Some(if cwd.is_absolute() { cwd } else { project_dir.join(cwd) })
            }
            None => self
                .profile
                .as_ref()
                .and_then(|profile| profile.resolve_working_dir(project_dir, home_dir().as_deref()))
                .or(Some(project_dir.clone())),
        };
        let filesystem_scope =
            FilesystemScope::classify_for(working_directory.as_deref(), project_dir);

        Some(CommandPolicyFacts {
            shell_profile_id: self.profile.as_ref().map(|p| p.id.clone()),
            resolved_executable,
            argv,
            working_directory,
            network_requested,
            filesystem_scope,
            destructive_classification,
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let shell_input: ShellInput = serde_json::from_value(input).map_err(|e| {
            ToolError::ExecutionFailed { message: format!("invalid shell input: {e}") }
        })?;

        // Validate input size bounds before any processing
        self.validate_input(&shell_input.command, &shell_input.args)?;

        // Validate command against denylist/allowlist.
        // Denylist is checked against the raw (unquoted) string so patterns
        // like `rm -rf /` match even in shell-wrap mode. Allowlist is checked
        // against the shell-quoted string so anchored patterns work correctly.
        self.validate_command(&shell_input.command, &shell_input.args)?;

        // Build the shell-quoted command string for actual execution.
        let full_command = build_full_command(&shell_input.command, &shell_input.args);

        // Determine working directory with sandboxing
        let project_dir =
            Utf8PathBuf::from_path_buf(session.project_dir.clone()).map_err(|_| {
                ToolError::ExecutionFailed {
                    message: "project directory is not valid UTF-8".into(),
                }
            })?;

        let cwd = if let Some(ref user_cwd) = shell_input.cwd {
            let normalized = normalize_msys_cwd(user_cwd);
            let user_path = Utf8Path::new(normalized.as_ref());
            canonicalize_within(&project_dir, user_path)?
        } else {
            project_dir.clone()
        };

        // Resolve timeout with a hard upper cap.
        let timeout_secs =
            shell_input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).min(MAX_TIMEOUT_SECS);
        let timeout = Duration::from_secs(timeout_secs);

        // Profile-driven execution (ADR-28): if a shell profile is configured,
        // run through its backend so the agent honours the selected executable,
        // args, env, and working-directory behaviour. A missing/broken profile
        // produces a recoverable diagnostic rather than aborting the session.
        if let Some(profile) = &self.profile {
            let backend = ShellProfileFactory::backend_for(profile);
            backend.check_available(profile)?;
            // The managed backend resolves to the installed Concerto runtime;
            // other backends resolve via the profile's own executable.
            let program = backend.resolved_program(profile);
            let args: Vec<String> = backend.command_args(profile, &full_command);
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let base: HashMap<String, String> = std::env::vars().collect();
            let env = backend.effective_env(profile, &base);
            let effective_cwd = if shell_input.cwd.is_some() {
                cwd.clone()
            } else {
                match profile.resolve_working_dir(project_dir.as_std_path(), home_dir().as_deref())
                {
                    Some(p) => Utf8PathBuf::from_path_buf(p).unwrap_or(cwd.clone()),
                    None => cwd.clone(),
                }
            };
            // Containment (ADR-55 §2, Phase 1b): the profile backend spawns the
            // process in `effective_cwd`; keep the command's directory changes,
            // redirects, and path arguments inside the project root.
            contain_shell_command(
                &project_dir,
                &effective_cwd,
                &shell_input.command,
                &shell_input.args,
            )?;
            let result = ProcessHandle::run_with_env(
                &program.to_string_lossy(),
                &arg_refs,
                &effective_cwd,
                Some(&env),
                timeout,
                cancel,
            )
            .await;
            // A managed `bash -c` wrapper may have materialized a literal
            // `nul`/`con`/... file via a `> nul` redirect; sweep it up.
            cleanup_reserved_device_files(&effective_cwd);
            return into_tool_output(result, &shell_input.command, timeout_secs);
        }

        // Containment (ADR-55 §2, Phase 1b): the command's directory changes,
        // redirect writes, and path-like arguments must stay within the
        // project root. `cwd` is the sandboxed working directory from which
        // the process will be spawned.
        contain_shell_command(&project_dir, &cwd, &shell_input.command, &shell_input.args)?;

        // Determine execution mode: direct or shell-wrapped.
        //
        // When `bypass_shell` is true the command binary is spawned
        // directly (pre-shell-wrapping behaviour).  Otherwise we wrap
        // in the configured (or auto-detected) OS shell so that shell
        // features (pipes, redirects, variable expansion) work.
        let result = if self.config.bypass_shell {
            // Direct execution — spawn the command binary directly.
            let args: Vec<&str> = shell_input.args.iter().map(|s| s.as_str()).collect();
            ProcessHandle::run(&shell_input.command, &args, &cwd, timeout, cancel).await
        } else {
            // Shell execution — wrap in `{shell} -c "full command"`.
            let shell = self.config.shell.clone().unwrap_or_else(detect_os_default_shell);

            // On Unix the flag is `-c`; on Windows cmd it is `/C`.
            // (PowerShell uses `-Command` which is equivalent.)
            let shell_arg = if cfg!(unix) { "-c" } else { "/C" };

            let shell_args: Vec<&str> = vec![shell_arg, &full_command];
            let result = ProcessHandle::run(&shell, &shell_args, &cwd, timeout, cancel).await;
            // Git-Bash `bash -c` can materialize a literal `nul` file in the
            // working directory via a `> nul` redirect (the `\\?\` extended
            // path bypasses Windows' reserved-name check); sweep it up.
            cleanup_reserved_device_files(&cwd);
            result
        };

        into_tool_output(result, &shell_input.command, timeout_secs)
    }
}

/// Map a raw process result into the tool's [`ToolOutput`], preserving the
/// distinct `Cancelled` / `Timeout` error variants.
fn into_tool_output(
    result: Result<ProcessOutput, ToolError>,
    command: &str,
    _timeout_secs: u64,
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(output) => {
            let summary = if output.exit_code == 0 {
                format!("Command `{command}` succeeded.")
            } else {
                format!("Command `{command}` failed with exit code {}.", output.exit_code)
            };

            let data = serde_json::json!({
                "exit_code": output.exit_code,
                "stdout": output.stdout,
                "stderr": output.stderr,
            });

            Ok(ToolOutput { summary, data })
        }
        Err(ToolError::Cancelled) => Err(ToolError::Cancelled),
        Err(ToolError::Timeout { timeout_secs }) => Err(ToolError::Timeout { timeout_secs }),
        Err(other) => Err(other),
    }
}

/// Best-effort resolution of the user's home directory.
fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}

/// After a `bash -c`-wrapped shell command, best-effort-remove any stale
/// reserved-device-name file (`nul`, `con`, `prn`, `aux`, `com1`..`com9`,
/// `lpt1`..`lpt9`) left in `cwd` by a `> nul`-style redirect. Windows normally
/// refuses these names, but Git-Bash/MSYS can materialize a literal 0-byte
/// file through the `\\?\` extended-path bypass; a stale `nul` in the project
/// root then poisons every later exploration. Errors are ignored by design —
/// the command already ran; cleanup must never fail the tool call. Only those
/// exact basenames in `cwd` are ever touched; no general cleanup.
#[cfg(windows)]
fn cleanup_reserved_device_files(cwd: &Utf8Path) {
    for device in crate::containment::WINDOWS_DEVICE_NAMES {
        let _ = std::fs::remove_file(cwd.join(device));
    }
}

#[cfg(not(windows))]
fn cleanup_reserved_device_files(_cwd: &Utf8Path) {}

/// Best-effort resolution of a program name via the `PATH` (ADR-28 §6).
///
/// Absolute/relative paths (containing a separator) are returned as-is; bare
/// names are resolved against `PATH` and only accepted if they are a regular
/// file. This is a heuristic — full alias/function/script resolution is a
/// later, deeper-resolution concern — but it gives the policy engine an
/// auditable `resolved_executable` rather than a raw string.
fn resolve_program_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return Some(PathBuf::from(program));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(program);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Heuristic detection of network-reaching commands (ADR-28 §6), mirroring the
/// policy engine's `cmd_is_network_op` so the structured `network_requested`
/// fact agrees with the legacy string scan.
fn command_looks_networked(command: &str, args: &[String]) -> bool {
    let joined =
        if args.is_empty() { command.to_string() } else { format!("{command} {}", args.join(" ")) };
    let lower = joined.to_ascii_lowercase();
    if lower.starts_with("curl ")
        || lower.starts_with("wget ")
        || lower.starts_with("ssh ")
        || ["git clone", "git fetch", "git pull", "git push"]
            .iter()
            .any(|prefix| lower.starts_with(*prefix))
    {
        return true;
    }
    if lower.contains("http://") || lower.contains("https://") || lower.contains("github.com") {
        return true;
    }
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| matches!(w, "curl" | "wget" | "ssh" | "scp" | "rsync" | "ftp" | "telnet" | "nc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AllowAllPolicy;
    use camino::Utf8PathBuf;
    use concerto_core::traits::tool::Tool;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn test_policy() -> AllowAllPolicy {
        AllowAllPolicy
    }

    fn test_session() -> SessionContext {
        SessionContext::new(concerto_core::ids::Ulid::new(), std::env::current_dir().unwrap())
    }

    fn test_session_with_dir(dir: PathBuf) -> SessionContext {
        SessionContext::new(concerto_core::ids::Ulid::new(), dir)
    }

    fn test_tool() -> ShellTool {
        // The allowlist is matched against the full command string
        // (command + " " + shell-quoted args joined), so anchors must account for args.
        let allowlist = vec![
            Regex::new(r"^echo( .*)?$").unwrap(),
            Regex::new(r"^sleep( .*)?$").unwrap(),
            Regex::new(r"^pwd$").unwrap(),
        ];
        ShellTool::with_config(ShellConfig {
            allowlist,
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: true, // tests bypass the shell for direct process control
            allow_all: false,
        })
    }

    #[tokio::test]
    async fn shell_tool_deny_by_default() {
        let tool = ShellTool::new();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = json!({
            "command": "echo",
            "args": ["hello"]
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected deny-by-default to block command");
        match result.unwrap_err() {
            ToolError::PolicyDenied { .. } => {}
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[test]
    fn allow_all_direct_keeps_denylist_and_bypasses_shell_wrapping() {
        let tool = ShellTool::allow_all_direct();
        assert!(tool.config.allow_all);
        assert!(tool.config.bypass_shell);
        assert!(!tool.config.denylist.is_empty());
    }

    #[tokio::test]
    async fn shell_tool_mock_process_success() {
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // Use `echo` as a mock process that we can verify
        let input = json!({
            "command": "echo",
            "args": ["hello", "world"],
            "timeout_secs": 5
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "expected success, got: {:?}", result.err());

        let output = result.unwrap();
        assert!(output.summary.contains("succeeded"));
        assert!(output.data["stdout"].as_str().unwrap().contains("hello world"));
        assert_eq!(output.data["exit_code"], 0);
    }

    #[tokio::test]
    async fn shell_tool_accepts_missing_args() {
        // Regression test: the input schema only requires `command`, so LLM
        // tool calls for argument-less commands (e.g. `pwd`, `ls`) omit `args`.
        // Deserialization must not fail with "missing field `args`".
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = json!({ "command": "pwd" });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "expected success without args, got: {:?}", result.err());
        assert!(result.unwrap().summary.contains("succeeded"));
    }

    #[tokio::test]
    async fn shell_tool_cancellation() {
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Spawn a long-running command and cancel it
        let input = json!({
            "command": "sleep",
            "args": ["10"],
            "timeout_secs": 30
        });

        let handle =
            tokio::spawn(async move { tool.execute(input, &policy, &session, cancel_clone).await });

        // Cancel immediately
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(result.is_err(), "expected error after cancellation");
        match result.unwrap_err() {
            ToolError::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_denylist_blocks_dangerous_command() {
        let tool = ShellTool::new();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = json!({
            "command": "rm",
            "args": ["-rf", "/"],
            "timeout_secs": 5
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected denylist to block command");
        match result.unwrap_err() {
            ToolError::PolicyDenied { .. } => {}
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_denylist_blocks_dd_command() {
        let tool = ShellTool::new();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = json!({
            "command": "dd",
            "args": ["if=/dev/zero", "of=/dev/sda"],
            "timeout_secs": 5
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected denylist to block dd command");
        match result.unwrap_err() {
            ToolError::PolicyDenied { .. } => {}
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_denylist_blocks_mkfs_command() {
        let tool = ShellTool::new();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let input = json!({
            "command": "mkfs",
            "args": ["-t", "ext4", "/dev/sda1"],
            "timeout_secs": 5
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected denylist to block mkfs command");
        match result.unwrap_err() {
            ToolError::PolicyDenied { .. } => {}
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_timeout_returns_timeout_error() {
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // Use a very short timeout with a command that sleeps
        let input = json!({
            "command": "sleep",
            "args": ["5"],
            "timeout_secs": 1
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected timeout error");
        match result.unwrap_err() {
            ToolError::Timeout { timeout_secs } => {
                assert_eq!(timeout_secs, 1);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_default_timeout_is_30_seconds() {
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // Command without timeout_secs should use default
        let input = json!({
            "command": "echo",
            "args": ["test"]
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "expected success with default timeout");
    }

    #[tokio::test]
    async fn shell_tool_sandbox_cwd_to_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session_with_dir(dir.path().to_path_buf());
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let tool = test_tool();

        // cwd not specified — should default to project_dir
        let input = json!({
            "command": "pwd",
            "args": []
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = output.data["stdout"].as_str().unwrap();
        assert!(stdout.contains(dir.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn shell_tool_sandbox_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session_with_dir(dir.path().to_path_buf());
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let tool = test_tool();

        // Attempt path traversal via cwd — use `../` which resolves to an
        // existing parent directory outside the temp dir.
        let input = json!({
            "command": "pwd",
            "args": [],
            "cwd": "../"
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected path traversal to be rejected");
        match result.unwrap_err() {
            ToolError::VirtualFsConflict { .. } => {}
            e @ ToolError::Io(_) => {
                // Io(NotFound) is also acceptable if the parent doesn't exist.
                assert!(
                    e.to_string().contains("No such file or directory")
                        || e.to_string().contains("entity not found"),
                    "expected traversal or not-found, got: {e}"
                );
            }
            other => panic!("expected VirtualFsConflict or Io, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_tool_allows_valid_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).unwrap();

        let session = test_session_with_dir(dir.path().to_path_buf());
        let policy = test_policy();
        let cancel = CancellationToken::new();

        let tool = test_tool();

        let input = json!({
            "command": "pwd",
            "args": [],
            "cwd": "subdir"
        });

        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "expected valid cwd to be allowed");
    }

    // -----------------------------------------------------------------------
    // normalize_msys_cwd: Git-Bash/MSYS `/c/...` cwd mapping. The broken
    // `\\?\C:\c\...` cwd came from feeding a drive-less `/c/...` path to
    // is_absolute()/join()/canonicalize_within(), which folded it under the
    // project root on Windows.
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn normalize_msys_cwd_maps_msys_drive_forms_on_windows() {
        // Git-Bash/MSYS cwd reports with no drive prefix map to their Windows
        // absolute form so is_absolute()/join()/canonicalize_within() see a
        // proper drive path instead of `\\?\C:\c\...`.
        assert_eq!(normalize_msys_cwd("/c/Users/x"), "C:/Users/x");
        assert_eq!(normalize_msys_cwd("//c/Users/x"), "C:/Users/x");
        assert_eq!(normalize_msys_cwd("/C/Users/x"), "C:/Users/x");
        // Already-Windows forms and relative paths are left untouched.
        assert_eq!(normalize_msys_cwd("C:/abs"), "C:/abs");
        assert_eq!(normalize_msys_cwd("C:\\abs"), "C:\\abs");
        assert_eq!(normalize_msys_cwd("relative/path"), "relative/path");
    }

    #[cfg(not(windows))]
    #[test]
    fn normalize_msys_cwd_is_identity_off_windows() {
        // On non-Windows platforms the cwd string is never rewritten.
        assert_eq!(normalize_msys_cwd("/c/Users/x"), "/c/Users/x");
        assert_eq!(normalize_msys_cwd("C:/abs"), "C:/abs");
        assert_eq!(normalize_msys_cwd("relative/path"), "relative/path");
    }

    #[tokio::test]
    async fn shell_tool_input_schema_is_valid_json() {
        let tool = ShellTool::new();
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["properties"]["args"].is_object());
        assert!(schema["properties"]["cwd"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
    }

    #[tokio::test]
    async fn shell_tool_capability_requirements_include_shell() {
        let tool = ShellTool::new();
        let caps = tool.capability_requirements();
        // CapabilitySet::shell creates a requirement string containing "shell"
        let empty = CapabilitySet::default();
        assert!(!caps.is_subset(&empty));
    }

    // -----------------------------------------------------------------------
    // Schema-shape tests: the advertised contract matches the struct.
    // -----------------------------------------------------------------------

    #[test]
    fn shell_tool_input_schema_shape() {
        let schema = ShellTool::new().input_schema();
        assert_eq!(schema["type"], "object");

        let props = schema["properties"].as_object().expect("properties must be an object");
        for field in ["command", "args", "cwd", "timeout_secs"] {
            assert!(props.contains_key(field), "schema missing property `{field}`");
        }

        // No provider-incompatible dialect/definition keywords.
        assert!(schema.get("$schema").is_none(), "schema must not emit $schema");
        assert!(schema.get("$defs").is_none(), "schema must not emit $defs");
        assert!(schema.get("definitions").is_none(), "schema must not emit definitions");

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"command"), "command must be required");
        assert!(!required.contains(&"args"), "args must be optional");
        assert!(!required.contains(&"cwd"), "cwd must be optional");
        assert!(!required.contains(&"timeout_secs"), "timeout_secs must be optional");
    }

    // -----------------------------------------------------------------------
    // Schema/runtime contract tests: the schema's required fields deserialize
    // into ShellInput, tying the advertised contract to the parse target.
    // -----------------------------------------------------------------------

    #[test]
    fn shell_tool_schema_runtime_contract_minimal() {
        // Build the smallest valid input from the schema's own `required` set
        // and verify it deserializes with the correct defaults.
        let schema = ShellTool::new().input_schema();
        let required = schema["required"].as_array().expect("required must be an array");
        let mut obj = serde_json::Map::new();
        for field in required {
            let name = field.as_str().expect("required entry is a string");
            match name {
                "command" => {
                    obj.insert("command".to_string(), serde_json::json!("pwd"));
                }
                other => panic!("unexpected required field in schema: {other}"),
            }
        }
        let input = serde_json::Value::Object(obj);
        let parsed: ShellInput = serde_json::from_value(input)
            .expect("input built from schema-required fields must deserialize");
        assert_eq!(parsed.command, "pwd");
        assert!(parsed.args.is_empty(), "args should default to empty");
        assert!(parsed.cwd.is_none(), "cwd should default to None");
        assert!(parsed.timeout_secs.is_none(), "timeout_secs should default to None");
    }

    #[test]
    fn shell_tool_schema_runtime_contract_full() {
        // A representative fully-populated input must also deserialize, proving
        // the schema's optional fields map onto the struct correctly.
        let input = serde_json::json!({
            "command": "echo",
            "args": ["hello", "world"],
            "cwd": "/tmp",
            "timeout_secs": 5
        });
        let parsed: ShellInput =
            serde_json::from_value(input).expect("full input must deserialize");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.args, vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp"));
        assert_eq!(parsed.timeout_secs, Some(5));
    }

    // -----------------------------------------------------------------------
    // ADR-28 §6: structured command-fact helpers (pure, no process spawn).
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_program_in_path_absolute_returns_as_is() {
        assert_eq!(resolve_program_in_path("/bin/sh"), Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn resolve_program_in_path_relative_returns_as_is() {
        assert_eq!(resolve_program_in_path("./local-tool"), Some(PathBuf::from("./local-tool")));
    }

    #[test]
    fn command_looks_networked_detects_transport_verbs() {
        assert!(command_looks_networked("curl", &["https://example.com".into()]));
        assert!(command_looks_networked("wget", &["file".into()]));
        assert!(command_looks_networked("git", &["clone".to_string(), "https://x".into()]));
    }

    #[test]
    fn command_looks_networked_rejects_benign_commands() {
        assert!(!command_looks_networked("echo", &["hello".into()]));
        assert!(!command_looks_networked("ls", &["-la".into()]));
    }

    #[test]
    fn command_facts_override_carries_profile_and_argv() {
        // A profile-driven shell tool must surface its profile id and the
        // resolved argv so the policy engine can reason about what runs.
        let profile = concerto_config::shell::ShellProfileConfig {
            id: "managed-bash".into(),
            executable: "bash".into(),
            ..Default::default()
        };
        let tool = ShellTool::with_profile(profile, true);
        let input = json!({ "command": "echo", "args": ["hello"] });
        let session = test_session();
        let facts = Tool::command_facts(&tool, &input, &session).expect("facts produced");
        assert_eq!(facts.shell_profile_id.as_deref(), Some("managed-bash"));
        assert_eq!(facts.argv.get(1).map(String::as_str), Some("-c"));
        // With shell-quoting, args are now properly quoted: 'hello' becomes 'hello'
        assert_eq!(facts.argv.last().map(String::as_str), Some("echo 'hello'"));
        assert_eq!(facts.filesystem_scope, FilesystemScope::ProjectOnly);
    }

    #[test]
    fn command_facts_classify_the_full_command() {
        let tool = ShellTool::allow_all_direct();
        let input = json!({ "command": "rm", "args": ["-rf", "target"] });
        let session = test_session();

        let facts = Tool::command_facts(&tool, &input, &session).expect("facts produced");

        assert_eq!(facts.destructive_classification, DestructiveClass::Destructive);
    }

    // -----------------------------------------------------------------------
    // §2.1 audit regression tests: shell injection through args and
    // bypassed deny patterns. These exercise paths the original tests
    // never touched (all original tests ran with `bypass_shell: true`).
    // -----------------------------------------------------------------------

    #[test]
    fn shell_quote_posix_wraps_in_single_quotes() {
        // On Unix we expect POSIX single-quote wrapping. The exact escape
        // sequence for an embedded single quote is `'\''` (close-quote,
        // backslash-escaped quote, reopen-quote), NOT `''\''` — that bug
        // leaks an extra `'` into the arg and breaks allowlist anchoring.
        if !cfg!(unix) {
            return;
        }
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        // A semicolon stays inside the quotes; the wrapping does not split
        // the arg into two shell tokens.
        let q = shell_quote("hello; rm -rf ~");
        assert_eq!(q, "'hello; rm -rf ~'");
    }

    #[tokio::test]
    async fn shell_wrap_mode_does_not_inject_through_semicolon_in_args() {
        // Regression test for §2.1: an arg containing `; rm -rf ~` must NOT
        // cause `rm` to run when `bypass_shell: false`. We craft a payload
        // that would create a marker directory if injection succeeded, then
        // assert the marker is absent after the call.
        if !cfg!(unix) {
            // cmd quoting differs from POSIX; skip on Windows.
            return;
        }
        let marker_base = std::env::temp_dir().join(format!(
            "concerto-injection-test-semi-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is before UNIX_EPOCH")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&marker_base);

        // Audited configuration: bypass_shell default (false), a
        // loosely-anchored allowlist pattern (`^echo( .*)?$`) that an
        // operator might trust blindly because echo is harmless.
        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
        });
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();

        // Arg is crafted so that WITHOUT quoting, the shell would run
        // `echo ; mkdir <marker>` and create the marker. With quoting
        // the whole `; mkdir <marker>` is a literal arg to echo, which
        // echo happily prints — marker never created.
        let payload = format!("; mkdir {}", marker_base.to_str().unwrap());
        let input = json!({
            "command": "echo",
            "args": [payload],
            "timeout_secs": 10u64,
        });
        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "echo should succeed even with a weird arg: {:?}", result.err());
        assert!(
            !marker_base.exists(),
            "shell injection regression: marker dir was created, args were not quoted properly"
        );
        let _ = std::fs::remove_dir_all(&marker_base);
    }

    #[tokio::test]
    async fn shell_wrap_mode_does_not_inject_through_pipe_in_args() {
        if !cfg!(unix) {
            return;
        }
        let marker = std::env::temp_dir().join(format!(
            "concerto-injection-test-pipe-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is before UNIX_EPOCH")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&marker);

        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
        });
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let payload = format!("| mkdir {}", marker.to_str().unwrap());
        let input = json!({"command": "echo", "args": [payload], "timeout_secs": 10u64});
        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_ok(), "echo should succeed: {:?}", result.err());
        assert!(
            !marker.exists(),
            "shell injection regression: marker dir was created via pipe injection"
        );
        let _ = std::fs::remove_dir_all(&marker);
    }

    #[tokio::test]
    async fn shell_wrap_mode_rejects_denylisted_command_via_loose_allowlist() {
        // The audit's specific scenario: a loosely-anchored allowlist
        // (`^git .*$` matches `git status; rm -rf ...`) combined with a
        // shell-injected second command must be caught — either by the
        // denylist (rm -rf targeting /) or by the quoted arg preventing
        // execution. We expect a PolicyDenied (denylist catches it on the
        // flattened string before the shell ever runs).
        if !cfg!(unix) {
            return;
        }
        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^git .*$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
        });
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        // The arg contains a `; rm -rf /` payload. Even though the quoted
        // form is a literal arg to `git status` and `rm` would not run, the
        // denylist regex still scans the flattened string and catches the
        // `rm ... /` pattern, so PolicyDenied wins first.
        let payload = "x; rm -rf /";
        let input = json!({
            "command": "git",
            "args": ["status", payload],
            "timeout_secs": 5u64,
        });
        let result = tool.execute(input, &policy, &session, cancel).await;
        assert!(result.is_err(), "expected denylist to catch injected rm -rf /");
        match result.unwrap_err() {
            ToolError::PolicyDenied { .. } => {}
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[test]
    fn hardcoded_denylist_catches_bypassed_rm_variants() {
        // Audit §2.1: `rm -fr /`, `rm -r -f /`, `rm --recursive --force /`
        // must all be rejected, not just the original `rm -rf /`.
        let dl = build_hardcoded_denylist();
        let cases = [
            "rm -rf /",
            "rm -fr /",
            "rm -r -f /",
            "rm -f -r /",
            "rm --recursive --force /",
            "rm -rf ~",
            "rm -rf *",
            "rm -rf .",
        ];
        for cmd in cases {
            let blocked = dl.iter().any(|p| p.is_match(cmd));
            assert!(blocked, "denylist failed to catch: `{cmd}`");
        }
    }

    #[test]
    fn hardcoded_denylist_does_not_block_benign_lookalikes() {
        // Sanity: a safely-scoped `rm -rf ./build` should NOT trip the
        // tightened patterns; only root/home/glob targets do.
        let dl = build_hardcoded_denylist();
        let safe = ["rm -rf ./target", "rm -rf build", "rm -r ./out", "git rm -rf path"];
        for cmd in safe {
            let blocked = dl.iter().any(|p| p.is_match(cmd));
            assert!(!blocked, "denylist over-blocked benign command: `{cmd}`");
        }
    }

    /// Verify that a timeout value is correctly propagated to shell execution.
    #[tokio::test]
    async fn shell_tool_respects_timeout_parameter() {
        let tool = test_tool();
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let input = json!({
            "command": "sleep",
            "args": ["5"],
            "timeout_secs": 1u64,
        });
        let result = tool.execute(input, &policy, &session, cancel).await;
        match result {
            Err(ToolError::Timeout { .. }) => {} // Expected
            Err(other) => panic!("expected Timeout error, got: {other:?}"),
            Ok(_) => panic!("expected timeout, got Ok"),
        }
    }

    // -- guard heuristic inference (Solution 3) --------------------------------

    /// Builds the `missing` argument for [`infer_missing_arguments`].
    fn missing(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_string()).collect()
    }

    #[test]
    fn infer_command_from_cmd_alias() {
        // Canonical Solution-3 example: shell with `cmd` infers `command`.
        let raw = json!({ "cmd": "cargo test" }).as_object().unwrap().clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["command"]));
        assert_eq!(inferred, vec![("command".to_string(), json!("cargo test"))]);
    }

    #[test]
    fn infer_command_from_action_alias() {
        let raw = json!({ "action": " pwd " }).as_object().unwrap().clone();
        let inferred = infer_missing_arguments(&raw, &missing(&["command"]));
        assert_eq!(inferred[0].1, json!("pwd"), "alias value is trimmed");
    }

    #[test]
    fn no_command_invention_from_args_or_empty_values() {
        // `args` alone never becomes a command; empty, whitespace-only, and
        // non-string aliases are ignored; with nothing to recover, the guard
        // must reject instead of guessing.
        let raw = json!({ "args": ["ls", "-la"] }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["command"])).is_empty());

        let raw = json!({ "cmd": "" }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["command"])).is_empty());

        let raw = json!({ "cmd": "  ", "action": 7 }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["command"])).is_empty());

        let raw = serde_json::Map::new();
        assert!(infer_missing_arguments(&raw, &missing(&["command"])).is_empty());
    }

    #[test]
    fn no_inference_when_command_is_not_missing() {
        let raw = json!({ "cmd": "ls" }).as_object().unwrap().clone();
        assert!(infer_missing_arguments(&raw, &missing(&["args"])).is_empty());
    }
}
