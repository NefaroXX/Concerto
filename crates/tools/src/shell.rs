//! Shell tool implementation — async, cancellable, sandboxed process execution.

use crate::common::canonicalize_within;
use crate::container::{containerize, containerize_with, ContainerConfig};
use crate::containment::contain_shell_command;
use crate::process::{CpuBudget, ProcessHandle, ProcessOutput};
use crate::shell_backend::ShellProfileFactory;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use concerto_config::shell::ShellProfileConfig;
use concerto_core::sandbox::ContainerRuntimeProbe;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{
    CapabilitySet, CommandPolicyFacts, CommandRouting, DestructiveClass, FilesystemScope,
    SessionContext, ToolOutput,
};
use concerto_core::{CancellationToken, ToolError};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for shell commands when not specified by the caller.
///
/// Generous enough for a cold `cargo build`/`cargo test` on a fresh workspace:
/// the smoke-proven failure mode was a model never passing `timeout_secs` and
/// a build being killed at the old 30s default. Short commands are unaffected
/// (the timeout is an upper bound, not a wait); the hard [`MAX_TIMEOUT_SECS`]
/// ceiling still bounds a hung command.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Hard upper bound for user-specified timeouts (5 minutes).
const MAX_TIMEOUT_SECS: u64 = 300;

/// Resolve the effective timeout: the caller's value when present, otherwise
/// the default, always clamped to the hard ceiling.
fn resolve_timeout_secs(requested: Option<u64>) -> u64 {
    requested.unwrap_or(DEFAULT_TIMEOUT_SECS).min(MAX_TIMEOUT_SECS)
}

/// Maximum allowed command string length (characters).
const MAX_COMMAND_LENGTH: usize = 4096;

/// Maximum allowed number of arguments.
const MAX_ARGS_COUNT: usize = 100;

/// Environment variable an operator can set to give every shell command a
/// CPU-time budget (threat-model §6, gap #6) without a code change — the
/// row-43 `CONCERTO_API_RATE_LIMIT` precedent for a fail-closed-by-default
/// runtime escape hatch.
///
/// Only consulted when [`ShellConfig::cpu_budget_secs`] is `None`; an
/// explicit config value (including `0`, "off") always wins. An unparsable
/// or `0` value means off, never "kill immediately".
const CPU_BUDGET_ENV: &str = "CONCERTO_SHELL_CPU_BUDGET_SECS";

/// Resolve the effective CPU budget: the explicit config value when present,
/// else the environment fallback, else off.
///
/// Pure on purpose — tests exercise the whole matrix without mutating the
/// process environment (which would race with tests running beside it).
fn resolve_cpu_budget(configured: Option<u64>, env: Option<&str>) -> Option<CpuBudget> {
    match configured {
        // Explicit config wins, including `Some(0)` = "off".
        Some(seconds) => CpuBudget::from_secs(seconds),
        None => env.and_then(|raw| raw.trim().parse::<u64>().ok()).and_then(CpuBudget::from_secs),
    }
}

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
    #[schemars(
        description = "Optional execution timeout in seconds. Defaults to 120. Pass a higher \
                       value (up to 300) for long builds/tests; pass a small value for \
                       interactive or short commands."
    )]
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
    /// CPU-time ceiling in seconds per command (threat-model §6, gap #6:
    /// "No CPU rate limiting on shell commands"). `None` (default) consults
    /// [`CPU_BUDGET_ENV`] and otherwise runs with no budget — the
    /// pre-existing behaviour; `Some(0)` is an explicit "off" that also
    /// ignores the environment.
    ///
    /// When a budget is set it is enforced in layers: a process-group
    /// watchdog on hosts that can account for CPU time, plus a `ulimit`
    /// `RLIMIT_CPU` backstop in front of POSIX-wrapped plans. The wall-clock
    /// `timeout_secs` remains an independent limit.
    pub cpu_budget_secs: Option<u64>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
            cpu_budget_secs: None,
        }
    }
}

/// Builds the hardcoded denylist regex patterns.
fn build_hardcoded_denylist() -> Vec<Regex> {
    HARDCODED_DENY_PATTERNS.iter().filter_map(|pattern| Regex::new(pattern).ok()).collect()
}

// ---------------------------------------------------------------------------
// Shell dialects, argument quoting, and launch planning (threat gap #3:
// "Windows Shell Quoting Weakness", security-threat-model.md §6)
// ---------------------------------------------------------------------------

/// Which quoting rule set (and launcher switch) a shell invocation uses.
///
/// Deliberately small: on Windows the only shell Concerto launches directly is
/// `cmd.exe`; every other shell an operator can select through
/// [`ShellConfig::shell`] (Git-Bash, MSYS2 bash, WSL sh, ...) speaks POSIX
/// `-c` quoting, and quoting it with cmd rules would be wrong in both
/// directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellDialect {
    /// sh/bash/zsh/dash: single-quote quoting, `-c` launcher switch.
    Posix,
    /// cmd.exe: cmd/CRT quoting, launched through [`cmd_verbatim_launch`].
    Cmd,
}

/// The platform whose rules apply.
///
/// Passed explicitly into the pure planning/quoting functions rather than read
/// from `cfg!` at each call site, so the Windows-only branches are exercised
/// by unit tests on the Linux-only CI runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Unix,
    Windows,
}

/// The host this binary was compiled for.
fn host() -> Host {
    if cfg!(unix) {
        Host::Unix
    } else {
        Host::Windows
    }
}

/// Shell syntax characters. A *command string* containing one of these is
/// shell-dependent and must not be spawned argv-direct.
///
/// Not all of these are metacharacters to cmd.exe — `'`, `$`, backtick, `*`,
/// `?`, `~`, `#`, `!` are literal there — but they are shell syntax to the
/// POSIX shells an operator can select via [`ShellConfig::shell`], so a
/// command string containing one is treated as shell-dependent rather than
/// guessed at. Arguments are *quoted* or *rejected*, never scanned with this
/// table: on the argv-direct path they never reach a shell at all.
const SHELL_SYNTAX: &[char] = &[
    '|', '&', ';', '<', '>', '(', ')', '^', '"', '\'', '$', '`', '*', '?', '~', '#', '%', '!',
    '\n', '\r',
];

/// cmd.exe builtins: there is no on-disk program to spawn, so the command only
/// means anything inside cmd.exe.
///
/// Deliberately generous — `find`, `findstr`, `more`, and `sort` also exist as
/// external programs, and misclassifying them merely keeps them on the
/// hardened shell path, which is where they ran before argv-direct preference
/// existed. A missed builtin would be a functional regression (spawn of a
/// non-existent program), so the table errs towards "shell required".
const CMD_BUILTINS: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir",
    "doskey", "echo", "endlocal", "erase", "exit", "find", "findstr", "for", "ftype", "goto", "if",
    "md", "mkdir", "mklink", "more", "move", "not", "path", "pause", "popd", "print", "prompt",
    "pushd", "rd", "rem", "ren", "rename", "rmdir", "set", "setlocal", "shift", "sort", "start",
    "time", "title", "type", "ver", "verify", "vol",
];

/// Characters that must never appear unquoted on a cmd.exe command line;
/// their presence in an argument forces the argument to be wrapped in `"`.
///
/// Matches the pre-hardening trigger set exactly (plus newlines, as defence in
/// depth for the allowlist string — [`validate_cmd_args`] rejects newline
/// arguments before anything spawns). Two characters are deliberately *not*
/// triggers: `!` (literal because every cmd launch passes `/V:OFF`) and `=`
/// (a cmd token separator that cannot begin a command, and quoting it would
/// visibly change `echo a=b` output).
const WINDOWS_QUOTE_TRIGGERS: &[char] =
    &[' ', '\t', '\n', '\r', '"', '\\', '%', '<', '>', '|', '&', '^', '(', ')', ';', ','];

/// cmd.exe switches that make the `/C` operand be taken verbatim:
/// `/D` disables AutoRun (a user registry entry cannot rewrite the command
/// line), `/V:OFF` forces delayed expansion off (so `!` is a literal), and
/// `/S` selects the "strip the leading quote and the last quote" rule so the
/// operand is delivered byte-for-byte.
const CMD_VERBATIM_SWITCHES: [&str; 4] = ["/D", "/V:OFF", "/S", "/C"];

/// Wrap `full_command` as the `/C` operand. With `/S`, cmd.exe strips exactly
/// the first and last quote of the remainder, so the inner text — whatever it
/// contains, including quotes and backslashes — is what cmd.exe executes.
fn cmd_verbatim_operand(full_command: &str) -> String {
    format!("\"{full_command}\"")
}

/// Launcher switches plus operand for running `full_command` through cmd.exe.
/// `prefix` carries profile-specific switches that must precede `/C`.
fn cmd_verbatim_launch(prefix: &[String], full_command: &str) -> (Vec<String>, String) {
    let mut switches = prefix.to_vec();
    switches.extend(CMD_VERBATIM_SWITCHES.iter().map(|s| (*s).to_string()));
    (switches, cmd_verbatim_operand(full_command))
}

/// Whether `arg` must be wrapped before it can go on a cmd.exe command line.
///
/// An empty argument is forced through quoting too: emitted bare it would
/// vanish from the command line entirely, silently dropping an argv element.
fn windows_arg_needs_quoting(arg: &str) -> bool {
    arg.is_empty() || arg.chars().any(|c| WINDOWS_QUOTE_TRIGGERS.contains(&c))
}

/// Quote a single argument so cmd.exe hands the child one, byte-for-byte
/// identical, argv element.
///
/// cmd.exe and the C run-time parse the same command line with different
/// rules, and both must agree, so:
///
/// 1. An argument with no shell-significant character stays bare (preserving
///    e.g. `echo hello`, which would otherwise print literal quotes).
/// 2. Otherwise it is wrapped in `"`, so every one of its characters sits
///    inside quotes at every point of cmd.exe's quote toggling.
/// 3. A `"` inside the argument becomes `""`: cmd.exe sees two toggles (net
///    zero, quoting unaffected) while the run-time sees one literal quote.
/// 4. A backslash run touching a quote is doubled first — the run-time halves
///    any run immediately before a `"`, so doubling keeps the child's copy
///    identical (including a trailing run, doubled before the closing quote).
///
/// `%` is intentionally *not* escaped: cmd.exe expands `%...%` even inside
/// quotes and provides no escape for it outside batch files. See
/// [`validate_cmd_args`], which rejects arguments that would expand.
fn shell_quote_windows(arg: &str) -> String {
    if !windows_arg_needs_quoting(arg) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 {
                    out.push('\\');
                }
                backslashes = 0;
                out.push_str("\"\"");
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // A run that reaches the closing quote must be doubled before it.
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

/// POSIX single-quote a single argument: `'arg'`, with an embedded `'`
/// written as `'\''`. The 4-character escape is load-bearing — the 5-character
/// `''\''` spelling leaks an extra `'` into the argument and breaks allowlist
/// patterns anchored on the canonical escape.
fn shell_quote_posix(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Shell-quote one argument for `dialect`. Not `cfg!`-gated: the Windows rules
/// are pure and are unit-tested on every CI host.
fn shell_quote(arg: &str, dialect: ShellDialect) -> String {
    match dialect {
        ShellDialect::Posix => shell_quote_posix(arg),
        ShellDialect::Cmd => shell_quote_windows(arg),
    }
}

/// Builds the full command string from command and args with proper quoting.
fn build_full_command(command: &str, args: &[String], dialect: ShellDialect) -> String {
    if args.is_empty() {
        return command.to_string();
    }
    let mut out = command.to_string();
    for a in args {
        out.push(' ');
        out.push_str(&shell_quote(a, dialect));
    }
    out
}

/// Lower-cased executable stem of a shell path, without a `.exe` suffix, so
/// `cmd`, `cmd.exe`, and `C:\Windows\System32\cmd.exe` all compare equal.
fn shell_stem(shell: &str) -> String {
    let base = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").map(str::to_string).unwrap_or_else(|| lower)
}

/// Dialect of an explicit shell path, independent of the host (pure, so
/// cmd.exe detection is covered on Linux CI).
fn dialect_for_shell(shell: &str) -> ShellDialect {
    if shell_stem(shell) == "cmd" {
        ShellDialect::Cmd
    } else {
        ShellDialect::Posix
    }
}

/// Dialect that applies on `host`. Unix hosts are pinned to
/// [`ShellDialect::Posix`] so a POSIX host can never take the cmd.exe path.
fn effective_dialect_for(host: Host, shell: &str) -> ShellDialect {
    match host {
        Host::Unix => ShellDialect::Posix,
        Host::Windows => dialect_for_shell(shell),
    }
}

/// True when any `%...%` pair inside `arg` names something the environment
/// resolves.
///
/// Every pair of `%` positions is checked, not just adjacent ones, so
/// `%%PATH%%` is caught by its inner pair (`PATH`); `%%` on its own has an
/// empty name and passes. `%Y-%m-%d` passes because no pair resolves.
fn percent_pair_expands(arg: &str) -> bool {
    let positions: Vec<usize> = arg.match_indices('%').map(|(i, _)| i).collect();
    // Bounded scan: an argument with hundreds of `%` would otherwise cost
    // O(n^2) passes. Fail closed — that is not a legitimate literal.
    if positions.len() > 64 {
        return true;
    }
    for (n, &open) in positions.iter().enumerate() {
        for &close in positions.iter().skip(n + 1) {
            // `%VAR:~0,1%` substring syntax: only the name before `:` counts.
            let name = &arg[open + 1..close];
            let base = name.split(':').next().unwrap_or(name);
            // `env::var_os` panics on a key containing '=' or NUL.
            if base.is_empty() || base.contains('=') || base.contains('\0') {
                continue;
            }
            if std::env::var_os(base).is_some() {
                return true;
            }
        }
    }
    false
}

/// Fail-closed validation of arguments headed for a cmd.exe launch.
///
/// cmd.exe expands `%...%` *before* the command runs and has no escape for it
/// outside batch files, so no amount of quoting can protect an argument whose
/// `%`-pair names a real variable — reject instead. Newlines are rejected too:
/// they separate commands in cmd.exe's grammar, and an unquoted argument is
/// emitted bare by [`shell_quote_windows`].
fn validate_cmd_args(args: &[String]) -> Result<(), ToolError> {
    for arg in args {
        if arg.chars().any(|c| c == '\n' || c == '\r') {
            return Err(ToolError::ExecutionFailed {
                message: "argument contains a newline, which cmd.exe cannot quote safely".into(),
            });
        }
        if percent_pair_expands(arg) {
            return Err(ToolError::ExecutionFailed {
                message: "argument contains a %...% pair that resolves in the environment; \
                          cmd.exe would expand it before the program runs"
                    .into(),
            });
        }
    }
    Ok(())
}

/// Whether a cmd.exe builtin name (with or without `.exe`).
fn cmd_builtin_reason(command: &str) -> Option<&'static str> {
    let stem = shell_stem(command);
    CMD_BUILTINS.contains(&stem.as_str()).then_some("the command is a cmd.exe builtin")
}

/// Whether `command` names a `.bat`/`.cmd` script. `CreateProcess` cannot run
/// those directly; they are cmd.exe's own format.
fn is_batch_script(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.ends_with(".bat") || lower.ends_with(".cmd")
}

/// Whether `command` starts with POSIX-style `NAME=` environment assignment,
/// which cmd.exe does not implement and only a shell does.
fn looks_like_env_assignment(command: &str) -> bool {
    let Some(eq) = command.find('=') else {
        return false;
    };
    let mut chars = command[..eq].chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// Why a Windows command cannot be spawned argv-direct, or `None` when it can.
///
/// Only the `command` string is examined. Arguments never require shell
/// semantics: on the argv-direct path they are handed to the OS untouched, and
/// on the shell path they are quoted by [`shell_quote_windows`] or rejected by
/// [`validate_cmd_args`].
fn windows_shell_requirement(shell_override: Option<&str>, command: &str) -> Option<&'static str> {
    if shell_override.is_some() {
        return Some("an explicit shell override is configured");
    }
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Some("the command string is empty");
    }
    // The tool contract is `command` + `args`; a `command` with whitespace is
    // a shell command line (or a path with spaces, which the shell path has
    // always received). Spawning it argv-direct would treat the whole string
    // as one program name.
    if trimmed.chars().any(char::is_whitespace) {
        return Some("the command string is a shell command line, not a single program");
    }
    if trimmed.chars().any(|c| SHELL_SYNTAX.contains(&c)) {
        return Some("the command string contains shell syntax");
    }
    if let Some(reason) = cmd_builtin_reason(trimmed) {
        return Some(reason);
    }
    if is_batch_script(trimmed) {
        return Some("the command is a batch script, which only cmd.exe can run");
    }
    if looks_like_env_assignment(trimmed) {
        return Some("the command assigns a variable before running a program");
    }
    None
}

/// How a shell invocation will be launched.
///
/// Built once per execution and consumed by both the spawner and
/// `command_facts`, so the argv that is audited can never drift from the argv
/// that actually runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellPlan {
    /// Spawn `program` directly with its own argv — no shell in between.
    Direct { program: String, args: Vec<String> },
    /// Spawn `program` (a shell) with `switches` followed by `operand`.
    /// `verbatim` means the operand must reach the command line without CRT
    /// escaping (cmd.exe only — see [`cmd_verbatim_launch`]).
    Wrapped { program: String, switches: Vec<String>, operand: String, verbatim: bool },
}

/// Launch plan for a legacy (non-profile) shell invocation.
///
/// On Unix nothing changes: the shell always wraps the command, exactly as
/// before. On Windows the command is spawned argv-direct whenever no shell
/// semantics are needed — the shell is then the injection surface we avoid —
/// and only shell-dependent commands fall back to a hardened cmd.exe
/// invocation (or to the override shell, quoted for its own dialect).
fn legacy_shell_plan(
    host: Host,
    bypass_shell: bool,
    shell: &str,
    shell_override: Option<&str>,
    command: &str,
    args: &[String],
    full_command: &str,
) -> ShellPlan {
    if bypass_shell {
        return ShellPlan::Direct { program: command.to_string(), args: args.to_vec() };
    }
    if host == Host::Windows && windows_shell_requirement(shell_override, command).is_none() {
        return ShellPlan::Direct { program: command.to_string(), args: args.to_vec() };
    }
    if effective_dialect_for(host, shell) == ShellDialect::Cmd {
        let (switches, operand) = cmd_verbatim_launch(&[], full_command);
        ShellPlan::Wrapped { program: shell.to_string(), switches, operand, verbatim: true }
    } else {
        ShellPlan::Wrapped {
            program: shell.to_string(),
            switches: vec!["-c".to_string()],
            operand: full_command.to_string(),
            verbatim: false,
        }
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
    /// ADR-72: opt-in container routing. When set, the planned invocation is
    /// wrapped in a `docker`/`podman run` argv (fail-closed if no runtime is
    /// available). `None` (default) is byte-identical to pre-ADR-72 behavior.
    container: Option<ContainerConfig>,
    /// ADR-72 §3: injectable container-runtime detection seam. `None` (the
    /// default) uses the process-wide system probe; tests inject a fixed
    /// found/absent result so no container runtime is ever required.
    container_probe: Option<Arc<dyn ContainerRuntimeProbe>>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    /// Creates a new `ShellTool` with the default hardcoded denylist.
    pub fn new() -> Self {
        Self {
            config: ShellConfig::default(),
            profile: None,
            container: None,
            container_probe: None,
        }
    }

    /// Creates a `ShellTool` with a custom configuration.
    pub fn with_config(config: ShellConfig) -> Self {
        Self { config, profile: None, container: None, container_probe: None }
    }

    /// Creates a `ShellTool` driven by a configured shell profile (ADR-28).
    ///
    /// `allow_all` lifts the tool's own deny-by-default allowlist (the policy
    /// engine remains the real gate) — used for agent execution the user has
    /// already approved, matching the legacy `ShellTool::allow_all` behaviour.
    pub fn with_profile(profile: ShellProfileConfig, allow_all: bool) -> Self {
        Self {
            config: ShellConfig { allow_all, ..Default::default() },
            profile: Some(profile),
            container: None,
            container_probe: None,
        }
    }

    /// Creates a `ShellTool` that permits all commands. The policy engine and
    /// approval sink remain the actual gate; this only lifts the tool's own
    /// empty-allowlist deny-by-default so local agent runs can execute shell
    /// commands that the user has approved. The hardcoded denylist (e.g.
    /// `rm -rf /`, `dd`, `mkfs`) still always applies.
    pub fn allow_all() -> Self {
        Self {
            config: ShellConfig { allow_all: true, ..Default::default() },
            profile: None,
            container: None,
            container_probe: None,
        }
    }

    /// Creates an allow-all tool that spawns the requested executable
    /// directly. This is intended for typed adapters that have already chosen
    /// an interpreter and pass its arguments separately. Central policy,
    /// approval, sandboxing, and the hardcoded denylist still apply.
    pub fn allow_all_direct() -> Self {
        Self {
            config: ShellConfig { bypass_shell: true, allow_all: true, ..Default::default() },
            profile: None,
            container: None,
            container_probe: None,
        }
    }

    /// Route this tool's invocations through an OS-level container runtime
    /// (ADR-72). The invocation is refused fail-closed when no runtime is
    /// available or the platform is unsupported.
    pub fn with_container(mut self, container: ContainerConfig) -> Self {
        self.container = Some(container);
        self
    }

    /// Inject the container-runtime detection probe (ADR-72 §3). Defaults to
    /// the process-wide system probe when unset. Tests use this to make
    /// found/absent/malformed detection deterministic without a container
    /// runtime; operators may use it to supply a differently cached probe.
    pub fn with_container_runtime_probe(mut self, probe: Arc<dyn ContainerRuntimeProbe>) -> Self {
        self.container_probe = Some(probe);
        self
    }

    /// The shell program this tool spawns: the profile-resolved executable
    /// when a profile is configured, else the explicit override, else the OS
    /// default. Single source for [`ShellTool::dialect`] and
    /// [`ShellTool::shell_plan_for`] so quoting and planning cannot disagree.
    fn resolved_shell_program(&self) -> String {
        if let Some(profile) = &self.profile {
            let backend = ShellProfileFactory::backend_for(profile);
            backend.resolved_program(profile).to_string_lossy().into_owned()
        } else {
            self.config.shell.clone().unwrap_or_else(detect_os_default_shell)
        }
    }

    /// Quoting dialect for `full_command` on this host.
    fn dialect(&self) -> ShellDialect {
        self.dialect_for(host())
    }

    /// Quoting dialect for `host` (the parameterised form exists so the
    /// Windows-only profile path is unit-testable on the Linux-only CI).
    fn dialect_for(&self, host: Host) -> ShellDialect {
        effective_dialect_for(host, &self.resolved_shell_program())
    }

    /// Shell-quoted `command` + `args` in this tool's dialect: the string the
    /// allowlist matches, the policy facts classify, and the shell receives.
    fn full_command(&self, command: &str, args: &[String]) -> String {
        build_full_command(command, args, self.dialect())
    }

    /// Launch plan for `host`, shared by [`execute`](Self::execute) and
    /// `command_facts` so the audited argv can never drift from the argv that
    /// actually runs. `full_command` must be [`ShellTool::full_command`] for
    /// the production host (the test seam passes its own).
    fn shell_plan_for(
        &self,
        host: Host,
        command: &str,
        args: &[String],
        full_command: &str,
    ) -> ShellPlan {
        let plan = if let Some(profile) = &self.profile {
            let backend = ShellProfileFactory::backend_for(profile);
            let program = backend.resolved_program(profile).to_string_lossy().into_owned();
            if self.dialect_for(host) == ShellDialect::Cmd {
                // A cmd.exe profile (the primary Windows path, e.g.
                // `os-comspec`): we own the switch set and the operand must
                // reach cmd.exe verbatim, so `command_args`'s plain `/C` is
                // not used here.
                let (switches, operand) = cmd_verbatim_launch(&profile.args, full_command);
                ShellPlan::Wrapped { program, switches, operand, verbatim: true }
            } else {
                // POSIX-ish profile: `command_args` yields profile args + a
                // launcher switch + the command; the command is the operand.
                let mut switches = backend.command_args(profile, full_command);
                let operand = switches.pop().unwrap_or_default();
                ShellPlan::Wrapped { program, switches, operand, verbatim: false }
            }
        } else {
            let shell = self.config.shell.clone().unwrap_or_else(detect_os_default_shell);
            legacy_shell_plan(
                host,
                self.config.bypass_shell,
                &shell,
                self.config.shell.as_deref(),
                command,
                args,
                full_command,
            )
        };
        // Built here rather than at spawn time: `command_facts` derives the
        // audited argv from this same plan, so the `ulimit` backstop that is
        // enforced is exactly the one that is audited (threat gap #6).
        self.with_cpu_limit(plan)
    }

    /// Production-host shorthand for [`ShellTool::shell_plan_for`].
    fn shell_plan(&self, shell_input: &ShellInput, full_command: &str) -> ShellPlan {
        self.shell_plan_for(host(), &shell_input.command, &shell_input.args, full_command)
    }

    /// ADR-72: apply container routing when configured. A no-op (byte-identical
    /// to pre-ADR-72) when no container config is attached. Refuses fail-closed
    /// when the profile cannot be enforced.
    ///
    /// Detection is the injected probe when present, else the process-wide
    /// system probe. A missing runtime yields an explicit refusal here, so a
    /// container-configured tool can never silently execute its inner command
    /// unconfined.
    fn containerized_plan(
        &self,
        plan: ShellPlan,
        cwd: &Utf8Path,
        project_root: &Utf8Path,
    ) -> Result<ShellPlan, ToolError> {
        let Some(config) = &self.container else {
            return Ok(plan);
        };
        match &self.container_probe {
            Some(probe) => containerize_with(&plan, config, probe.probe(), cwd, project_root),
            None => containerize(&plan, config, cwd, project_root),
        }
    }

    /// Effective CPU budget for one execution: explicit config, else the
    /// environment fallback, else none (see [`resolve_cpu_budget`]).
    fn cpu_budget(&self) -> Option<CpuBudget> {
        resolve_cpu_budget(
            self.config.cpu_budget_secs,
            std::env::var(CPU_BUDGET_ENV).ok().as_deref(),
        )
    }

    /// Prefix `plan` with the `ulimit` CPU backstop when a budget applies to
    /// it. A no-op for every plan the backstop cannot serve (argv-direct
    /// plans, cmd.exe's verbatim operand) and whenever no budget is
    /// configured, which is the default.
    fn with_cpu_limit(&self, plan: ShellPlan) -> ShellPlan {
        let Some(budget) = self.cpu_budget() else {
            return plan;
        };
        if !plan_takes_cpu_backstop(&plan, Some(budget)) {
            return plan;
        }
        let ShellPlan::Wrapped { program, switches, operand, verbatim } = plan else {
            return plan;
        };
        ShellPlan::Wrapped {
            program,
            switches,
            operand: format!("{}{operand}", cpu_limit_prelude(budget)),
            verbatim,
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
        let full_command = self.full_command(command, args);
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
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 0,
                        "description": "Optional execution timeout in seconds. Defaults to 120. Pass a higher value (up to 300) for long builds/tests." }
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
        let full_command = self.full_command(&shell_input.command, &shell_input.args);

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

        // Describe the executable and argv that will actually be spawned, from
        // the same plan `execute` runs: a profile/legacy shell is the launcher,
        // direct mode (bypass, or Windows argv-direct preference) launches the
        // requested program itself. Under ADR-72 the plan is first routed
        // through the container so the audited argv is the container argv that
        // runs (a failure to containerize yields no facts; `execute` refuses).
        //
        // The `container_routing` marker is set ONLY on the branch that actually
        // wrapped the invocation: if containerization fails, `.ok()?` returns no
        // facts at all, so the policy gate can never see a `Containerized`
        // routing claim for an unrouted plan.
        let plan = self.shell_plan(&shell_input, &full_command);
        let (plan, container_routing) = match &self.container {
            Some(_) => {
                let cwd_utf8 = Utf8PathBuf::from_path_buf(
                    working_directory.clone().unwrap_or_else(|| project_dir.clone()),
                )
                .ok()?;
                let root_utf8 = Utf8PathBuf::from_path_buf(project_dir.clone()).ok()?;
                let routed = self.containerized_plan(plan, &cwd_utf8, &root_utf8).ok()?;
                (routed, CommandRouting::Containerized)
            }
            None => (plan, CommandRouting::Direct),
        };
        let (resolved_executable, argv) = match &plan {
            ShellPlan::Direct { program, args } => (
                resolve_program_in_path(program),
                std::iter::once(program.clone()).chain(args.iter().cloned()).collect(),
            ),
            ShellPlan::Wrapped { program, switches, operand, .. } => (
                // A profile backend resolves its own executable (ADR-28), so
                // keep its program as-is; a legacy shell goes through PATH.
                if self.profile.is_some() {
                    Some(PathBuf::from(program))
                } else {
                    resolve_program_in_path(program)
                },
                std::iter::once(program.clone())
                    .chain(switches.iter().cloned())
                    .chain(std::iter::once(operand.clone()))
                    .collect(),
            ),
        };

        Some(CommandPolicyFacts {
            shell_profile_id: self.profile.as_ref().map(|p| p.id.clone()),
            resolved_executable,
            argv,
            working_directory,
            network_requested,
            filesystem_scope,
            destructive_classification,
            container_routing,
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
        let full_command = self.full_command(&shell_input.command, &shell_input.args);

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
        let timeout_secs = resolve_timeout_secs(shell_input.timeout_secs);
        let timeout = Duration::from_secs(timeout_secs);

        // Resolve the effective CPU budget once for this execution; the plan
        // carries the matching `ulimit` backstop (see `with_cpu_limit`).
        let cpu_budget = self.cpu_budget();

        // Profile-driven execution (ADR-28): if a shell profile is configured,
        // run through its backend so the agent honours the selected executable,
        // args, env, and working-directory behaviour. A missing/broken profile
        // produces a recoverable diagnostic rather than aborting the session.
        if let Some(profile) = &self.profile {
            let backend = ShellProfileFactory::backend_for(profile);
            backend.check_available(profile)?;
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
            // Containment (ADR-55 §3, Phase 1b): the profile backend spawns the
            // process in `effective_cwd`; keep the command's directory changes,
            // redirects, and path arguments inside the project root.
            contain_shell_command(
                &project_dir,
                &effective_cwd,
                &shell_input.command,
                &shell_input.args,
            )?;
            let plan = self.shell_plan(&shell_input, &full_command);
            validate_plan_args(&plan, &shell_input.args)?;
            let plan = self.containerized_plan(plan, &effective_cwd, &project_dir)?;
            let result =
                spawn_plan(&plan, &effective_cwd, Some(&env), timeout, cpu_budget, cancel).await;
            return into_tool_output(result, &shell_input.command, timeout_secs);
        }

        // Containment (ADR-55 §3, Phase 1b): the command's directory changes,
        // redirect writes, and path-like arguments must stay within the
        // project root. `cwd` is the sandboxed working directory from which
        // the process will be spawned.
        contain_shell_command(&project_dir, &cwd, &shell_input.command, &shell_input.args)?;

        // Decide how to launch: argv-direct (bypass, or Windows when no shell
        // semantics are needed — the shell is then the injection surface we
        // avoid) or a shell-wrapped plan quoted for its own dialect. Unix is
        // always shell-wrapped, exactly as before.
        let plan = self.shell_plan(&shell_input, &full_command);
        validate_plan_args(&plan, &shell_input.args)?;
        let plan = self.containerized_plan(plan, &cwd, &project_dir)?;
        let result = spawn_plan(&plan, &cwd, None, timeout, cpu_budget, cancel).await;

        into_tool_output(result, &shell_input.command, timeout_secs)
    }
}

/// Reject arguments that the launcher cannot deliver literally.
///
/// Only cmd.exe-launched plans (`verbatim`) need this: cmd.exe expands
/// `%...%` before the program runs and offers no escape for it outside batch
/// files, and it treats newlines as command separators. Arguments on the
/// argv-direct path never reach a shell, and POSIX plans are quoted by
/// [`shell_quote_posix`], so neither is rejected here.
fn validate_plan_args(plan: &ShellPlan, args: &[String]) -> Result<(), ToolError> {
    if let ShellPlan::Wrapped { verbatim: true, .. } = plan {
        return validate_cmd_args(args);
    }
    Ok(())
}

/// True when `plan` is one the `ulimit` CPU backstop can serve: a
/// POSIX-wrapped shell invocation with a budget configured.
///
/// `Direct` plans never go through a shell, and a `verbatim` operand is
/// cmd.exe's, which has no `ulimit` builtin — prefixing either would be
/// ignored or would corrupt the argv, so the process-group watchdog (where
/// available) is the only enforcement for them.
fn plan_takes_cpu_backstop(plan: &ShellPlan, budget: Option<CpuBudget>) -> bool {
    budget.is_some() && matches!(plan, ShellPlan::Wrapped { verbatim: false, .. })
}

/// The shell prelude that installs the `RLIMIT_CPU` ceiling.
///
/// `ulimit -S -t N` sets only the *soft* CPU limit, leaving the hard limit
/// where it was. At `N` seconds of CPU the kernel sends `SIGXCPU`, whose
/// default action terminates the process — a spelling [`super::process`] can
/// recognise (`128 + SIGXCPU`). Setting the hard limit to `N` instead makes
/// the kernel deliver `SIGKILL` directly, which is indistinguishable from an
/// OOM or external kill, so it would be reported as a plain exit code 137.
fn cpu_limit_prelude(budget: CpuBudget) -> String {
    format!("ulimit -S -t {}; ", budget.seconds())
}

/// Spawn `plan` in `cwd`, with cancel/timeout support, an optional CPU budget,
/// and optional profile environment. This is the single place a shell tool
/// starts a process, so the audited plan (see `command_facts`) and the
/// executed argv are the same object.
async fn spawn_plan(
    plan: &ShellPlan,
    cwd: &Utf8Path,
    env: Option<&HashMap<String, String>>,
    timeout: Duration,
    cpu_budget: Option<CpuBudget>,
    cancel: CancellationToken,
) -> Result<ProcessOutput, ToolError> {
    // The plan already carries the `ulimit` prelude when one applies, so
    // `kernel_backstop` and the audited argv derive from the same object.
    let kernel_backstop = plan_takes_cpu_backstop(plan, cpu_budget);
    match plan {
        ShellPlan::Direct { program, args } => {
            let refs = str_refs(args);
            ProcessHandle::run_limited(
                program,
                &refs,
                None,
                cwd,
                env,
                timeout,
                cpu_budget,
                kernel_backstop,
                cancel,
            )
            .await
        }
        ShellPlan::Wrapped { program, switches, operand, verbatim } => {
            let switch_refs = str_refs(switches);
            let result = if *verbatim {
                ProcessHandle::run_limited(
                    program,
                    &switch_refs,
                    Some(operand),
                    cwd,
                    env,
                    timeout,
                    cpu_budget,
                    kernel_backstop,
                    cancel,
                )
                .await
            } else {
                let mut refs = switch_refs;
                refs.push(operand);
                ProcessHandle::run_limited(
                    program,
                    &refs,
                    None,
                    cwd,
                    env,
                    timeout,
                    cpu_budget,
                    kernel_backstop,
                    cancel,
                )
                .await
            };
            // A shell-wrapped command (`bash -c`, `cmd /C`) may have
            // materialized a literal `nul`/`con`/... file via a `> nul`
            // redirect; sweep it up. Direct spawns have no shell to do that.
            cleanup_reserved_device_files(cwd);
            result
        }
    }
}

/// Borrow a `&[String]` as `&[&str]` for a spawn call.
fn str_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
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

/// Heuristic detection of network-reaching commands (ADR-28 §6), mirroring
/// the policy engine's `cmd_is_network_op` (and the intent classifier's
/// `SHELL_NETWORK_VERBS`) so the structured `network_requested` fact agrees
/// with the legacy string scan. Word table kept in sync with
/// `crates/core/src/authorization.rs` — `ncat`, `socat`, and `sftp` added so
/// e.g. `ncat evil.com 9000` sets the egress fact (F3, security review
/// 2026-09-09).
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
    lower.split(|c: char| !c.is_alphanumeric()).any(|w| {
        matches!(
            w,
            "curl"
                | "wget"
                | "ssh"
                | "scp"
                | "rsync"
                | "ftp"
                | "sftp"
                | "telnet"
                | "nc"
                | "ncat"
                | "socat"
        )
    })
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

    /// A real project root plus a real subdirectory for ADR-72 container
    /// routing: the containment check canonicalizes both sides, so fake paths
    /// fail closed. The returned `TempDir` must be held for the test lifetime.
    fn container_paths() -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(dir.path()).expect("canonical tempdir"),
        )
        .expect("utf8 root");
        let cwd = root.join("sub");
        std::fs::create_dir_all(&cwd).expect("create subdir");
        (dir, root, cwd)
    }

    /// ADR-72 §5: the one refusal Windows produces for a containerized plan,
    /// checked before runtime detection or any routing decision.
    const WINDOWS_UNSUPPORTED_RULE: &str = "sandbox_containerized_unsupported_platform";

    /// The named refusal rule a containerization attempt must produce on this
    /// platform for an outcome that only holds off Windows: Windows refuses the
    /// profile itself (`WINDOWS_UNSUPPORTED_RULE`) before the caller's condition
    /// is ever evaluated, so both expectations stay asserted instead of one
    /// being compiled out.
    fn expected_rule(off_windows: &'static str) -> &'static str {
        if cfg!(windows) {
            WINDOWS_UNSUPPORTED_RULE
        } else {
            off_windows
        }
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
            cpu_budget_secs: None,
        })
    }

    /// ADR-72: with no container config the plan is untouched (byte-identical
    /// to pre-ADR-72).
    #[test]
    fn containerized_plan_is_identity_without_container_config() {
        let tool = test_tool();
        let input = ShellInput {
            command: "echo".into(),
            args: vec!["hi".into()],
            cwd: None,
            timeout_secs: None,
        };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let root = Utf8PathBuf::from("/proj");
        let routed = tool.containerized_plan(plan.clone(), &root, &root).expect("identity");
        assert_eq!(routed, plan);
    }

    /// ADR-72: an attached container config routes the planned invocation
    /// through the runtime, preserving the row-42 launch shape as the inner
    /// command (asserted at the seam; no runtime is invoked).
    #[test]
    fn containerized_plan_routes_wrapped_shell_through_runtime() {
        let tool = ShellTool::new().with_container(ContainerConfig {
            runtime: Some(concerto_core::sandbox::ContainerRuntime::Docker),
            ..ContainerConfig::new("alpine:3")
        });
        let input = ShellInput {
            command: "echo".into(),
            args: vec!["hi".into()],
            cwd: None,
            timeout_secs: None,
        };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let (_dir, root, _cwd) = container_paths();
        let routed = tool.containerized_plan(plan, &root, &root);
        // ADR-72 §5: the expected outcome is platform-defined. Windows refuses
        // the containerized profile outright (no POSIX shell in the default
        // images; Job Object isolation needs the unsafe FFI this workspace
        // denies), everything else routes. Both are asserted — neither is
        // skipped.
        #[cfg(windows)]
        {
            let err = routed.expect_err("windows must refuse the containerized profile");
            assert!(
                matches!(
                    err,
                    ToolError::PolicyDenied { ref rule } if rule == WINDOWS_UNSUPPORTED_RULE
                ),
                "unexpected refusal: {err:?}"
            );
        }
        #[cfg(not(windows))]
        {
            let routed = routed.expect("routed");
            let ShellPlan::Direct { program, args } = routed else {
                panic!("container plan must be argv-direct");
            };
            assert_eq!(program, "docker");
            assert_eq!(args[0], "run");
            assert!(args.contains(&"alpine:3".to_string()));
            assert!(args.contains(&format!("{root}:{root}")));
        }
    }

    /// ADR-72: a working directory outside the mount is refused fail-closed
    /// with the named unenforceable rule — or, on Windows, with the rule that
    /// refuses the containerized profile itself before containment is ever
    /// evaluated (ADR-72 §5). Either way the refusal is `PolicyDenied`.
    #[test]
    fn containerized_plan_refuses_out_of_root_cwd() {
        let tool = ShellTool::new().with_container(ContainerConfig {
            runtime: Some(concerto_core::sandbox::ContainerRuntime::Docker),
            ..ContainerConfig::new("alpine:3")
        });
        let input =
            ShellInput { command: "echo".into(), args: vec![], cwd: None, timeout_secs: None };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let (_dir, root, _cwd) = container_paths();
        let outside_tmp = tempfile::tempdir().expect("outside tempdir");
        let cwd = Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(outside_tmp.path()).expect("canonical outside"),
        )
        .expect("utf8 outside");
        let err = tool.containerized_plan(plan, &cwd, &root).expect_err("must refuse");
        assert!(
            matches!(
                err,
                ToolError::PolicyDenied { ref rule }
                    if rule == expected_rule("sandbox_containerized_unenforceable")
            ),
            "unexpected refusal: {err:?}"
        );
    }

    /// ADR-72 test double: a container-runtime probe with a fixed verdict, so
    /// detection is deterministic without a container runtime installed.
    struct FixedRuntimeProbe(concerto_core::sandbox::RuntimeAvailability);

    impl ContainerRuntimeProbe for FixedRuntimeProbe {
        fn probe(&self) -> concerto_core::sandbox::RuntimeAvailability {
            self.0.clone()
        }
    }

    /// ADR-72 §2: with a container config attached but detection reporting no
    /// runtime, the invocation is refused fail-closed — the unconfined inner
    /// plan is never returned. Deterministic (injected probe), no runtime
    /// required. Windows refuses the profile before detection runs, so the
    /// named rule is platform-defined; both are asserted.
    #[test]
    fn containerized_plan_refuses_when_probe_reports_absent() {
        let tool = ShellTool::new()
            .with_container(ContainerConfig::new("alpine:3"))
            .with_container_runtime_probe(Arc::new(FixedRuntimeProbe(
                concerto_core::sandbox::RuntimeAvailability::Unavailable {
                    reason: "test: no runtime".into(),
                },
            )));
        let input =
            ShellInput { command: "echo".into(), args: vec![], cwd: None, timeout_secs: None };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let root = Utf8PathBuf::from("/proj");
        let err = tool
            .containerized_plan(plan, &root, &root)
            .expect_err("absent runtime must refuse, never pass through");
        assert!(
            matches!(
                err,
                ToolError::PolicyDenied { ref rule }
                    if rule == expected_rule("sandbox_containerized_runtime_unavailable")
            ),
            "unexpected refusal: {err:?}"
        );
    }

    /// ADR-72 §3: an injected probe reporting an available runtime routes the
    /// plan through it, without consulting the host `PATH`.
    #[test]
    fn containerized_plan_routes_with_injected_available_probe() {
        let tool = ShellTool::new()
            .with_container(ContainerConfig::new("alpine:3"))
            .with_container_runtime_probe(Arc::new(FixedRuntimeProbe(
                concerto_core::sandbox::RuntimeAvailability::Available(
                    concerto_core::sandbox::ContainerRuntime::Podman,
                ),
            )));
        let input =
            ShellInput { command: "echo".into(), args: vec![], cwd: None, timeout_secs: None };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let (_dir, root, _cwd) = container_paths();
        let routed = tool.containerized_plan(plan, &root, &root);
        // ADR-72 §5: Windows refuses the containerized profile before the
        // injected verdict is even consulted; elsewhere the probe's verdict
        // routes the plan. Both outcomes are asserted.
        #[cfg(windows)]
        {
            let err = routed.expect_err("windows must refuse the containerized profile");
            assert!(
                matches!(
                    err,
                    ToolError::PolicyDenied { ref rule } if rule == WINDOWS_UNSUPPORTED_RULE
                ),
                "unexpected refusal: {err:?}"
            );
        }
        #[cfg(not(windows))]
        {
            let routed = routed.expect("routed");
            let ShellPlan::Direct { program, args } = routed else { panic!("argv-direct") };
            assert_eq!(program, "podman");
            assert_eq!(args[0], "run");
        }
    }

    /// ADR-72 §4 / row 45: the CPU `ulimit` prelude built by `with_cpu_limit`
    /// rides inside the container command unchanged, is not duplicated as a
    /// runtime ceiling, and the resulting argv-direct container plan takes no
    /// kernel backstop, so the in-container `ulimit` is the single CPU guard.
    #[test]
    fn containerized_plan_carries_row45_cpu_budget_inside_container() {
        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: Some("/bin/sh".to_string()),
            bypass_shell: false,
            allow_all: false,
            cpu_budget_secs: Some(5),
        })
        .with_container(ContainerConfig::new("alpine:3"))
        .with_container_runtime_probe(Arc::new(FixedRuntimeProbe(
            concerto_core::sandbox::RuntimeAvailability::Available(
                concerto_core::sandbox::ContainerRuntime::Docker,
            ),
        )));

        let input = ShellInput {
            command: "echo".into(),
            args: vec!["hi".into()],
            cwd: None,
            timeout_secs: None,
        };
        let full = tool.full_command(&input.command, &input.args);
        let plan = tool.shell_plan(&input, &full);
        let ShellPlan::Wrapped { operand, verbatim, .. } = &plan else {
            panic!("expected a POSIX wrapped plan, got {plan:?}");
        };
        assert!(!verbatim);
        assert!(operand.starts_with("ulimit -S -t 5; "));

        let (_dir, root, _cwd) = container_paths();
        let routed = tool.containerized_plan(plan, &root, &root);
        // ADR-72 §5: Windows refuses the containerized profile outright, so the
        // container argv this test audits can only exist off Windows. The
        // refusal itself is asserted on Windows rather than skipped.
        #[cfg(windows)]
        {
            let err = routed.expect_err("windows must refuse the containerized profile");
            assert!(
                matches!(
                    err,
                    ToolError::PolicyDenied { ref rule } if rule == WINDOWS_UNSUPPORTED_RULE
                ),
                "unexpected refusal: {err:?}"
            );
        }
        #[cfg(not(windows))]
        {
            let routed = routed.expect("routed");
            let ShellPlan::Direct { program, args } = &routed else { panic!("argv-direct") };
            assert_eq!(program, "docker");
            let image = args.iter().position(|a| a == "alpine:3").expect("image");
            let expected: Vec<String> =
                vec!["/bin/sh".into(), "-c".into(), format!("ulimit -S -t 5; {full}")];
            assert_eq!(&args[image + 1..], expected.as_slice());
            // The runtime itself is never asked to impose a CPU ceiling.
            assert!(!args.iter().any(|a| a == "--ulimit" || a == "--cpus"));
            // The outer container argv takes no kernel backstop; the prelude inside
            // is the enforcement, exactly as ADR-72 §4 requires.
            assert!(!plan_takes_cpu_backstop(&routed, tool.cpu_budget()));
        }
    }

    /// ADR-72: the audited command facts describe the container argv that
    /// actually runs, and assert the routing marker the policy gate requires.
    #[test]
    fn container_command_facts_describe_the_runtime_argv() {
        let tool = ShellTool::new().with_container(ContainerConfig {
            runtime: Some(concerto_core::sandbox::ContainerRuntime::Docker),
            ..ContainerConfig::new("alpine:3")
        });
        let session = test_session();
        let input = json!({"command": "echo", "args": ["hi"]});
        let facts = tool.command_facts(&input, &session);
        // A refusal must produce no facts at all: the policy gate can never be
        // satisfied by a `Containerized` claim for a plan that never routed.
        // Windows refuses the containerized profile (ADR-72 §5), so `None` is
        // the expected outcome there; every other platform must produce the
        // routed argv below.
        #[cfg(windows)]
        {
            assert!(
                facts.is_none(),
                "a refused containerization must yield no facts, got {facts:?}"
            );
        }
        #[cfg(not(windows))]
        {
            let facts = facts.expect("container facts");
            assert_eq!(facts.argv.first().map(String::as_str), Some("docker"));
            assert_eq!(facts.argv.get(1).map(String::as_str), Some("run"));
            assert!(facts.argv.contains(&"alpine:3".to_string()));
            assert!(facts.working_directory.is_some());
            // The marker is set only on the branch that actually routed, so the gate
            // cannot be satisfied by a bare working directory.
            assert_eq!(facts.container_routing, CommandRouting::Containerized);
        }
    }

    /// ADR-72 §2: a tool with no container config produces `Direct` routing, so
    /// an unrelated shell invocation can never satisfy the Containerized gate.
    #[test]
    fn non_container_command_facts_assert_direct_routing() {
        let tool = ShellTool::new();
        let session = test_session();
        let input = json!({"command": "echo", "args": ["hi"]});
        let facts = tool.command_facts(&input, &session).expect("facts");
        assert_eq!(facts.container_routing, CommandRouting::Direct);
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
    async fn shell_tool_default_timeout_is_120_seconds() {
        // The default must clear a cold build; the old 30s default killed
        // `cargo build` in smoke.
        assert_eq!(DEFAULT_TIMEOUT_SECS, 120);
        assert_eq!(MAX_TIMEOUT_SECS, 300);

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

    #[test]
    fn shell_timeout_resolution_applies_default_and_ceiling() {
        // Absent → default.
        assert_eq!(resolve_timeout_secs(None), DEFAULT_TIMEOUT_SECS);
        // Explicit value under the cap is honoured.
        assert_eq!(resolve_timeout_secs(Some(5)), 5);
        assert_eq!(resolve_timeout_secs(Some(120)), 120);
        // Over the cap is clamped to the hard ceiling (no unbounded hang).
        assert_eq!(resolve_timeout_secs(Some(MAX_TIMEOUT_SECS)), MAX_TIMEOUT_SECS);
        assert_eq!(resolve_timeout_secs(Some(u64::MAX)), MAX_TIMEOUT_SECS);
        assert_eq!(resolve_timeout_secs(Some(9_999_999)), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn shell_timeout_schema_documents_build_guidance() {
        let schema = ShellTool::new().input_schema();
        let description = schema["properties"]["timeout_secs"]["description"]
            .as_str()
            .expect("timeout_secs must carry a description");
        assert!(
            description.contains("120"),
            "the description must name the default so models can pass it: {description}"
        );
        assert!(
            description.to_lowercase().contains("build"),
            "the description must steer models to pass a timeout for builds: {description}"
        );
        assert!(
            description.contains("300"),
            "the description must name the hard ceiling: {description}"
        );
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

        // Compare leaf names, not spellings: on Windows the child is MSYS
        // `pwd`, which prints its own `/tmp`-mounted spelling that no
        // normalization can map back to the Win32 path. The leaf is preserved
        // across every spelling (POSIX, verbatim, 8.3 short), and tempfile
        // names are unique, so the leaf identifies the directory on all
        // platforms. (`cmd /c cd` cannot serve here: containment rejects
        // `/c` as an absolute path.)
        let reported = stdout.trim();
        let reported_leaf = reported.rsplit(['/', '\\']).next().unwrap_or(reported);
        let expected_leaf = dir
            .path()
            .file_name()
            .expect("tempdir has a leaf name")
            .to_str()
            .expect("UTF-8 tempdir");
        assert_eq!(
            reported_leaf, expected_leaf,
            "pwd reported {reported:?}, which must be the project directory"
        );
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

    // F3 (security review 2026-09-09): the facts word list is aligned with
    // the policy engine's `SHELL_NETWORK_VERBS` — the previously missing
    // `ncat`/`socat`/`sftp` transport clients now set the egress fact.
    #[test]
    fn command_looks_networked_matches_aligned_transport_clients() {
        assert!(command_looks_networked("ncat", &["evil.example".into(), "9000".into()]));
        assert!(command_looks_networked("socat", &["TCP-LISTEN:9000".into()]));
        assert!(command_looks_networked("sftp", &["host".into()]));
        // The table was already aligned on `nc`; word-scan, no substring.
        assert!(command_looks_networked("nc", &["host".into(), "9000".into()]));
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
        // POSIX single-quote wrapping. The exact escape sequence for an
        // embedded single quote is `'\''` (close-quote, backslash-escaped
        // quote, reopen-quote), NOT `''\''` — that bug leaks an extra `'`
        // into the arg and breaks allowlist anchoring. Pure rules: the
        // dialect is passed explicitly, so this runs on every CI host.
        let quote = |arg: &str| shell_quote(arg, ShellDialect::Posix);
        assert_eq!(quote("hello"), "'hello'");
        assert_eq!(quote("hello world"), "'hello world'");
        assert_eq!(quote("a'b"), "'a'\\''b'");
        // A semicolon stays inside the quotes; the wrapping does not split
        // the arg into two shell tokens.
        let q = quote("hello; rm -rf ~");
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
            cpu_budget_secs: None,
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
        // The injected `mkdir` must never create the marker dir. Wrap mode
        // quoting keeps it from executing; containment (2026-09-11 list-
        // segmentation alignment) additionally treats a flattened `;` glued
        // in an arg as a segment boundary like the pipe case, so an
        // escaping `; mkdir <marker>` segment can be rejected outright —
        // an equally valid outcome for the same invariant — but the marker
        // must stay absent either way.
        let _ = result;
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
            cpu_budget_secs: None,
        });
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let payload = format!("| mkdir {}", marker.to_str().unwrap());
        let input = json!({"command": "echo", "args": [payload], "timeout_secs": 10u64});
        let result = tool.execute(input, &policy, &session, cancel).await;
        // The injected `mkdir` must never create the marker dir. Wrap mode
        // quoting keeps it from executing; containment (F5 pipe modeling)
        // additionally rejects the escaping pipe segment outright, which is
        // an equally valid outcome for the same invariant — but the marker
        // must stay absent either way.
        let _ = result;
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
            cpu_budget_secs: None,
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

    // -----------------------------------------------------------------------
    // Threat model §6 #3 — Windows shell quoting hardening (DEFERRED row 42).
    // Everything here is pure: the Windows-only rules are exercised on the
    // Linux-only CI via `Host` and the reference parsers below.
    // -----------------------------------------------------------------------

    /// Reference implementation of the Windows C run-time argument parser
    /// (`parse_lp_cmd_line` in `library/std/src/sys/args/windows.rs`, Rust
    /// 1.96), ported so our cmd.exe quoting can be round-tripped in tests.
    ///
    /// The rules it encodes: space/tab outside quotes split arguments; a
    /// quote toggles quoting; `""` inside quotes is one literal quote; a run
    /// of `n` backslashes immediately before a quote is halved (an odd run
    /// escapes the quote instead); a backslash run not followed by a quote is
    /// literal; the final argument is kept even when empty if the line ended
    /// inside quotes.
    fn crt_parse_args(line: &str) -> Vec<String> {
        let mut chars = line.chars().peekable();

        // argv[0]: quotes toggle unconditionally; whitespace ends it.
        let mut argv0 = String::new();
        let mut in_quotes = false;
        for c in chars.by_ref() {
            match c {
                '"' => in_quotes = !in_quotes,
                ' ' | '\t' if !in_quotes => break,
                other => argv0.push(other),
            }
        }
        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }
        let mut out = vec![argv0];

        let mut cur = String::new();
        in_quotes = false;
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' if !in_quotes => {
                    out.push(std::mem::take(&mut cur));
                    while matches!(chars.peek(), Some(' ' | '\t')) {
                        chars.next();
                    }
                }
                '\\' => {
                    let mut run = 1usize;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        run += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        for _ in 0..run / 2 {
                            cur.push('\\');
                        }
                        if run % 2 == 1 {
                            chars.next();
                            cur.push('"');
                        }
                    } else {
                        for _ in 0..run {
                            cur.push('\\');
                        }
                    }
                }
                '"' if in_quotes => match chars.peek().copied() {
                    Some('"') => {
                        cur.push('"');
                        chars.next();
                    }
                    Some(_) => in_quotes = false,
                    // End of line: keep `cur` even if empty (in_quotes set).
                    None => break,
                },
                '"' => in_quotes = true,
                other => cur.push(other),
            }
        }
        if !cur.is_empty() || in_quotes {
            out.push(cur);
        }
        out
    }

    /// Metacharacter matrix for the quoting tests: everything cmd.exe or a
    /// CRT run-time can reinterpret, plus the argv shapes that must survive.
    fn quoting_matrix() -> Vec<String> {
        [
            "plain",
            "hello world",
            "",
            "a\"b",
            "\"\"",
            "C:\\src\\",
            "a\\b",
            "\\\\",
            "trailing\\",
            "a b|c",
            "&whoami",
            "%PATH%",
            "a&b",
            "a|b",
            "a>b",
            "a<b",
            "a^b",
            "(x)",
            "a;b",
            "a,b",
            "100%",
            "a%b",
            "\ttab",
            "line1\nline2",
            "a=b",
            "!bang!",
            "git status",
            "héllo wörld",
            "nul",
            "..\\..\\escape",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    #[test]
    fn shell_quote_windows_round_trips_through_crt_arg_parsing() {
        // The child program parses its own command line with the C run-time
        // rules, so every argument we emit for cmd.exe must decode back to
        // the original bytes after CRT parsing — quotes, backslash runs,
        // empty arguments, and metacharacters alike.
        for arg in quoting_matrix() {
            let quoted = shell_quote_windows(&arg);
            let line = format!("prog {quoted}");
            let parsed = crt_parse_args(&line);
            assert_eq!(
                parsed,
                vec!["prog".to_string(), arg.clone()],
                "CRT round-trip failed: quoted `{quoted}`"
            );
        }
    }

    #[test]
    fn shell_quote_windows_exposes_no_cmd_syntax_outside_quotes() {
        // cmd.exe parses the operand with its own quote toggling (no
        // backslash rules) to find command boundaries. No separator or
        // redirect character may sit outside quotes, or `& whoami`-style
        // payloads would run; the quotes themselves must balance.
        const OUTSIDE: &[char] =
            &[' ', '\t', '\n', '\r', '&', '|', '<', '>', '^', '(', ')', '%', ';', ','];
        for input in quoting_matrix() {
            let quoted = shell_quote_windows(&input);
            let mut in_quotes = false;
            for (idx, c) in quoted.chars().enumerate() {
                if c == '"' {
                    in_quotes = !in_quotes;
                    continue;
                }
                if !in_quotes {
                    assert!(
                        !OUTSIDE.contains(&c),
                        "`{c:?}` at {idx} of `{quoted}` (input `{input}`) is outside quotes"
                    );
                }
            }
            assert!(!in_quotes, "unbalanced quotes for `{input}`: `{quoted}`");
        }
    }

    #[test]
    fn shell_quote_windows_quoting_matrix_is_exact() {
        let cases: &[(&str, &str)] = &[
            // No trigger character: emitted bare so `echo hello` output is
            // unchanged (and allowlist patterns anchored on the plain form
            // keep matching).
            ("plain", "plain"),
            // Trigger characters force wrapping.
            ("hello world", "\"hello world\""),
            ("", "\"\""),
            ("a\"b", "\"a\"\"b\""),
            // Two content quotes: open + two `""` pairs + close = six.
            ("\"\"", "\"\"\"\"\"\""),
            // A backslash run before the closing quote is doubled so the
            // run-time halves it back.
            ("C:\\src\\", "\"C:\\src\\\\\""),
            ("trailing\\", "\"trailing\\\\\""),
            // A backslash not touching a quote stays as-is.
            ("a\\b", "\"a\\b\""),
            // Shell syntax and %-variables are quoted, never escaped: cmd.exe
            // has no escape for `%` outside batch files, so `validate_cmd_args`
            // rejects expanding pairs instead.
            ("&whoami", "\"&whoami\""),
            ("%PATH%", "\"%PATH%\""),
            ("\ttab", "\"\ttab\""),
        ];
        for (input, expected) in cases {
            assert_eq!(&shell_quote_windows(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn cmd_verbatim_operand_matches_slash_s_quote_strip() {
        // With `/S`, cmd.exe strips exactly the first and last quote of the
        // remainder after `/C` and executes what is left — so wrapping the
        // operand in one quote pair must deliver `full_command` byte-for-byte,
        // including when it itself starts/ends with quotes or backslashes.
        for full in [
            "echo hi",
            "dir \"C:\\a b\"",
            "cd C:\\",
            "\"C:\\Program Files\\x.exe\" a",
            "echo \"a",
            "echo a\"",
            "",
        ] {
            let operand = cmd_verbatim_operand(full);
            let mut chars = operand.chars();
            assert_eq!(chars.next(), Some('"'), "operand must open with a quote: {operand}");
            let mut inner: String = chars.collect();
            assert_eq!(inner.pop(), Some('"'), "operand must close with a quote: {operand}");
            assert_eq!(inner, full, "/S strip must yield the original command");
        }
    }

    #[test]
    fn cmd_verbatim_launch_orders_profile_args_then_switches_then_operand() {
        let (switches, operand) =
            cmd_verbatim_launch(&["/K".to_string(), "chcp 65001".to_string()], "dir");
        assert_eq!(switches, ["/K", "chcp 65001", "/D", "/V:OFF", "/S", "/C"].map(String::from));
        assert_eq!(operand, "\"dir\"");
        // Without profile args the standard four switches stand alone.
        let (switches, _) = cmd_verbatim_launch(&[], "echo hi");
        assert_eq!(switches, ["/D", "/V:OFF", "/S", "/C"].map(String::from));
    }

    #[test]
    fn dialect_for_shell_detects_cmd_and_posix() {
        assert_eq!(dialect_for_shell("cmd"), ShellDialect::Cmd);
        assert_eq!(dialect_for_shell("CMD.EXE"), ShellDialect::Cmd);
        assert_eq!(dialect_for_shell("C:\\Windows\\System32\\cmd.exe"), ShellDialect::Cmd);
        assert_eq!(dialect_for_shell("/bin/bash"), ShellDialect::Posix);
        assert_eq!(dialect_for_shell("C:\\Program Files\\Git\\bin\\bash.exe"), ShellDialect::Posix);
        // PowerShell legacy overrides stay on the POSIX-side plan for now
        // (row 42 scope is cmd.exe); the launch switch differs only where a
        // pwsh override is explicitly configured.
        assert_eq!(dialect_for_shell("pwsh"), ShellDialect::Posix);
        // A Unix host can never take the cmd.exe path, whatever the string.
        assert_eq!(effective_dialect_for(Host::Unix, "cmd.exe"), ShellDialect::Posix);
        assert_eq!(effective_dialect_for(Host::Windows, "cmd.exe"), ShellDialect::Cmd);
        assert_eq!(effective_dialect_for(Host::Windows, "bash.exe"), ShellDialect::Posix);
    }

    #[test]
    fn windows_shell_requirement_classifies_commands() {
        // Plain external programs need no shell: they spawn argv-direct.
        assert_eq!(windows_shell_requirement(None, "cargo"), None);
        assert_eq!(windows_shell_requirement(None, "git"), None);
        // cmd.exe builtins only mean anything inside cmd.exe.
        assert!(windows_shell_requirement(None, "echo").is_some());
        assert!(windows_shell_requirement(None, "DIR.EXE").is_some());
        assert!(windows_shell_requirement(None, "set").is_some());
        // Batch scripts are cmd.exe's own format; CreateProcess can't run them.
        assert!(windows_shell_requirement(None, "build.bat").is_some());
        assert!(windows_shell_requirement(None, "build.CMD").is_some());
        // Shell syntax, command lines (whitespace), POSIX env assignments,
        // empty/blank commands, and any configured shell override.
        assert!(windows_shell_requirement(None, "ls | wc").is_some());
        assert!(windows_shell_requirement(None, "foo&bar").is_some());
        assert!(windows_shell_requirement(None, "my tool").is_some());
        assert!(windows_shell_requirement(None, "FOO=1").is_some());
        assert!(windows_shell_requirement(None, "").is_some());
        assert!(windows_shell_requirement(None, "   ").is_some());
        assert!(windows_shell_requirement(Some("cmd.exe"), "cargo").is_some());
    }

    #[test]
    fn validate_cmd_args_rejects_expanding_pairs_and_newlines() {
        // `%` pairs that resolve in the environment expand inside quotes too
        // and have no escape outside batch files: reject before spawning.
        if std::env::var_os("PATH").is_some() {
            let args = vec!["--path".to_string(), "%PATH%".to_string()];
            assert!(validate_cmd_args(&args).is_err(), "%PATH% must be rejected");
            // `%%PATH%%` is caught by its inner pair, not just adjacent ones.
            let args = vec!["%%PATH%%".to_string()];
            assert!(validate_cmd_args(&args).is_err(), "%%PATH%% must be rejected");
        }
        // Newlines separate commands in cmd.exe's grammar.
        let args = vec!["ok".to_string(), "a\nb".to_string()];
        assert!(validate_cmd_args(&args).is_err(), "newline args must be rejected");
        let args = vec!["ok".to_string(), "a\rb".to_string()];
        assert!(validate_cmd_args(&args).is_err(), "CR args must be rejected");
        // Fail closed on absurdly many `%` (bounded scan).
        let args = vec!["%x".repeat(65)];
        assert!(validate_cmd_args(&args).is_err(), "runaway % must be rejected");
        // Format-looking and non-resolving `%` usage stays usable.
        let args = vec!["--date=%d/%m/%Y".to_string(), "%".to_string(), "100%".to_string()];
        assert!(validate_cmd_args(&args).is_ok(), "literal percent args must pass");
        let args = vec!["C:\\src".to_string(), "a&b".to_string()];
        assert!(validate_cmd_args(&args).is_ok(), "plain args must pass");
    }

    #[test]
    fn legacy_plan_prefers_argv_direct_on_windows_only() {
        let args: Vec<String> = ["build", "--release"].iter().map(|s| (*s).to_string()).collect();
        let full = build_full_command("cargo", &args, ShellDialect::Cmd);

        // No shell semantics → spawn argv-direct; the shell is then the
        // injection surface we avoid.
        let plan = legacy_shell_plan(Host::Windows, false, "cmd.exe", None, "cargo", &args, &full);
        assert_eq!(plan, ShellPlan::Direct { program: "cargo".into(), args: args.clone() });

        // Arguments are never scanned for shell syntax: on the direct path
        // they never reach a shell, so even `|`-bearing args stay direct.
        let nasty: Vec<String> = ["a|b"].iter().map(|s| (*s).to_string()).collect();
        let plan =
            legacy_shell_plan(Host::Windows, false, "cmd.exe", None, "cargo", &nasty, "cargo");
        assert_eq!(plan, ShellPlan::Direct { program: "cargo".into(), args: nasty.clone() });

        // cmd.exe builtins fall back to the hardened cmd.exe launch.
        let plan = legacy_shell_plan(Host::Windows, false, "cmd.exe", None, "echo", &[], "echo hi");
        assert_eq!(
            plan,
            ShellPlan::Wrapped {
                program: "cmd.exe".into(),
                switches: ["/D", "/V:OFF", "/S", "/C"].map(String::from).to_vec(),
                operand: "\"echo hi\"".into(),
                verbatim: true,
            }
        );

        // Shell syntax in the command forces the shell path.
        let plan = legacy_shell_plan(
            Host::Windows,
            false,
            "C:\\Windows\\System32\\cmd.exe",
            None,
            "a & b",
            &[],
            "a & b",
        );
        assert!(matches!(plan, ShellPlan::Wrapped { verbatim: true, .. }));

        // An explicit shell override always wins (the operator asked for it),
        // quoted for that shell's own dialect.
        let plan = legacy_shell_plan(
            Host::Windows,
            false,
            "C:\\msys64\\usr\\bin\\bash.exe",
            Some("C:\\msys64\\usr\\bin\\bash.exe"),
            "cargo",
            &args,
            &full,
        );
        assert_eq!(
            plan,
            ShellPlan::Wrapped {
                program: "C:\\msys64\\usr\\bin\\bash.exe".into(),
                switches: vec!["-c".into()],
                operand: full.clone(),
                verbatim: false,
            }
        );

        // bypass_shell keeps its meaning on every host: direct, always.
        let plan = legacy_shell_plan(Host::Windows, true, "cmd.exe", None, "echo", &[], "echo");
        assert_eq!(plan, ShellPlan::Direct { program: "echo".into(), args: vec![] });
    }

    #[test]
    fn legacy_plan_keeps_unix_shell_wrapping_unchanged() {
        // Row 42 is Windows-only: a Unix host is always shell-wrapped with
        // `-c`, argv-direct preference never engages, and quoting is POSIX.
        let args: Vec<String> = ["a|b"].iter().map(|s| (*s).to_string()).collect();
        let full = build_full_command("cargo", &args, ShellDialect::Posix);
        let plan = legacy_shell_plan(Host::Unix, false, "/bin/bash", None, "cargo", &args, &full);
        assert_eq!(
            plan,
            ShellPlan::Wrapped {
                program: "/bin/bash".into(),
                switches: vec!["-c".into()],
                operand: full,
                verbatim: false,
            }
        );
        // Even a command string that looks like `cmd.exe` stays POSIX-wrapped.
        let plan = legacy_shell_plan(Host::Unix, false, "/bin/sh", None, "echo", &[], "echo hi");
        assert_eq!(
            plan,
            ShellPlan::Wrapped {
                program: "/bin/sh".into(),
                switches: vec!["-c".into()],
                operand: "echo hi".into(),
                verbatim: false,
            }
        );
        // bypass_shell still spawns directly.
        let plan = legacy_shell_plan(Host::Unix, true, "/bin/sh", None, "pwd", &[], "pwd");
        assert_eq!(plan, ShellPlan::Direct { program: "pwd".into(), args: vec![] });
    }

    #[test]
    fn shell_plan_profile_cmd_is_verbatim_on_windows() {
        // The primary Windows production path: `os-comspec`-style profile
        // selected, cmd.exe detected, operand delivered byte-for-byte.
        let profile = concerto_config::shell::ShellProfileConfig {
            id: "os-comspec".into(),
            executable: "cmd.exe".into(),
            args: vec!["/K".into(), "chcp 65001".into()],
            ..Default::default()
        };
        let tool = ShellTool::with_profile(profile, true);
        let args = vec!["C:\\a b".to_string()];
        let full = build_full_command("dir", &args, ShellDialect::Cmd);
        let plan = tool.shell_plan_for(Host::Windows, "dir", &args, &full);
        let (program, switches, operand, verbatim) = match plan {
            ShellPlan::Wrapped { program, switches, operand, verbatim } => {
                (program, switches, operand, verbatim)
            }
            other => panic!("expected a wrapped cmd.exe plan, got {other:?}"),
        };
        assert!(verbatim, "the cmd.exe operand must be launched verbatim");
        assert_eq!(shell_stem(&program), "cmd");
        assert_eq!(switches, ["/K", "chcp 65001", "/D", "/V:OFF", "/S", "/C"].map(String::from));
        assert_eq!(operand, format!("\"{full}\""));
        // The quoted form is what /S strips back to.
        let stripped = operand.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
        assert_eq!(stripped, Some(full.as_str()));
    }

    #[test]
    fn shell_plan_profile_posix_pops_command_as_operand() {
        // A POSIX profile (Git Bash, managed bash, any Unix host) keeps the
        // backend's own launch args, with the command as the final operand.
        let profile = concerto_config::shell::ShellProfileConfig {
            id: "system-default".into(),
            executable: "bash".into(),
            ..Default::default()
        };
        let tool = ShellTool::with_profile(profile, true);
        let args = vec!["hello".to_string()];
        let full = build_full_command("echo", &args, ShellDialect::Posix);
        let plan = tool.shell_plan_for(Host::Unix, "echo", &args, &full);
        let (program, switches, operand, verbatim) = match plan {
            ShellPlan::Wrapped { program, switches, operand, verbatim } => {
                (program, switches, operand, verbatim)
            }
            other => panic!("expected a wrapped POSIX plan, got {other:?}"),
        };
        assert!(!verbatim);
        assert_eq!(shell_stem(&program), "bash");
        assert_eq!(switches, vec!["-c".to_string()]);
        assert_eq!(operand, full);
        assert_eq!(operand, "echo 'hello'");
    }

    #[test]
    fn command_facts_direct_plan_is_program_plus_args() {
        // The facts of a direct spawn are the program and its argv, with no
        // shell in between (bypass mode on any host; Windows direct
        // preference produces the same `ShellPlan::Direct` shape).
        let tool = ShellTool::allow_all_direct();
        let input = json!({ "command": "cargo", "args": ["build", "--release"] });
        let facts = Tool::command_facts(&tool, &input, &test_session()).expect("facts produced");
        let argv: Vec<&str> = facts.argv.iter().map(String::as_str).collect();
        assert_eq!(argv, vec!["cargo", "build", "--release"]);
        assert!(facts.shell_profile_id.is_none());
    }

    // ---------------------------------------------------------------------
    // CPU-time budget (threat gap #6). Pure tests cover the whole budget
    // matrix without mutating the process environment; the end-to-end test
    // proves the breach actually kills.
    // ---------------------------------------------------------------------

    #[test]
    fn resolve_cpu_budget_prefers_config_then_env_then_off() {
        // Explicit config wins, including an explicit zero ("off").
        assert_eq!(resolve_cpu_budget(Some(7), Some("900")), CpuBudget::from_secs(7));
        assert_eq!(resolve_cpu_budget(Some(0), Some("900")), None);
        // No config: the environment fallback is parsed, trimmed, and zero is off.
        assert_eq!(resolve_cpu_budget(None, Some(" 30 ")), CpuBudget::from_secs(30));
        assert_eq!(resolve_cpu_budget(None, Some("0")), None);
        // Unparsable or absent env means off, never "kill immediately".
        assert_eq!(resolve_cpu_budget(None, Some("banana")), None);
        assert_eq!(resolve_cpu_budget(None, None), None);
    }

    #[test]
    fn only_posix_wrapped_plans_take_the_ulimit_backstop() {
        let budget = CpuBudget::from_secs(5);
        let direct = ShellPlan::Direct { program: "cargo".into(), args: vec![] };
        let posix = ShellPlan::Wrapped {
            program: "sh".into(),
            switches: vec!["-c".into()],
            operand: "echo hi".into(),
            verbatim: false,
        };
        let verbatim = ShellPlan::Wrapped {
            program: "cmd.exe".into(),
            switches: vec!["/C".into()],
            operand: "\"echo hi\"".into(),
            verbatim: true,
        };
        assert!(!plan_takes_cpu_backstop(&direct, budget));
        assert!(plan_takes_cpu_backstop(&posix, budget));
        assert!(!plan_takes_cpu_backstop(&verbatim, budget));
        // No budget, no backstop, whatever the plan shape.
        assert!(!plan_takes_cpu_backstop(&posix, None));
    }

    #[test]
    fn cpu_limit_prelude_sets_rlimit_cpu_in_seconds() {
        let budget = CpuBudget::from_secs(42).expect("non-zero budget");
        assert_eq!(cpu_limit_prelude(budget), "ulimit -S -t 42; ");
    }

    #[test]
    fn with_cpu_limit_prefixes_eligible_plans_only() {
        let args = vec!["hi".to_string()];
        let full = build_full_command("echo", &args, ShellDialect::Posix);

        let wrapped = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
            cpu_budget_secs: Some(3),
        });
        match wrapped.shell_plan_for(Host::Unix, "echo", &args, &full) {
            ShellPlan::Wrapped { operand, verbatim, .. } => {
                assert!(!verbatim);
                assert_eq!(operand, format!("ulimit -S -t 3; {full}"));
            }
            other => panic!("expected a POSIX wrapped plan, got {other:?}"),
        }

        // A direct plan never goes through a shell, so it is never prefixed.
        let direct = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: true,
            allow_all: false,
            cpu_budget_secs: Some(3),
        });
        assert!(matches!(
            direct.shell_plan_for(Host::Unix, "echo", &args, &full),
            ShellPlan::Direct { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cpu_budget_breach_surfaces_an_explicit_error() {
        // `allow_all` lifts the tool's own allowlist; the denylist still
        // applies. A spinning shell burns far past a one-second budget long
        // before the generous wall-clock timeout.
        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: true,
            cpu_budget_secs: Some(1),
        });
        let session = test_session();
        let policy = test_policy();
        let cancel = CancellationToken::new();
        let input =
            json!({"command": "sh", "args": ["-c", "while :; do :; done"], "timeout_secs": 30u64});
        let result = tool.execute(input, &policy, &session, cancel).await;
        let error = result.expect_err("a spinning shell command must breach its cpu budget");
        let ToolError::ExecutionFailed { message } = error else {
            panic!("expected ExecutionFailed, got {error:?}");
        };
        // The explicit breach message names the budget; the executor records
        // the tool error as an `ExecutionError(...)` audit row.
        assert!(message.contains("cpu budget exceeded"), "message was: {message}");
        assert!(message.contains("1s"), "the budget must be named: {message}");
    }

    #[test]
    fn explicit_zero_budget_leaves_the_plan_unprefixed() {
        // `Some(0)` is an explicit "off": the env fallback is ignored and the
        // launch plan is byte-for-byte the pre-budget one. The process layer
        // separately proves an unconfigured budget never arms a watchdog.
        let args = vec!["hi".to_string()];
        let full = build_full_command("echo", &args, ShellDialect::Posix);
        let tool = ShellTool::with_config(ShellConfig {
            allowlist: vec![Regex::new(r"^echo( .*)?$").unwrap()],
            denylist: build_hardcoded_denylist(),
            shell: None,
            bypass_shell: false,
            allow_all: false,
            cpu_budget_secs: Some(0),
        });
        match tool.shell_plan_for(Host::Unix, "echo", &args, &full) {
            ShellPlan::Wrapped { operand, .. } => assert_eq!(operand, full),
            other => panic!("expected a POSIX wrapped plan, got {other:?}"),
        }
    }
}
