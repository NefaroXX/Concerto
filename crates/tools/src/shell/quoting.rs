//! Argument quoting, cmd.exe launch switches, and cmd.exe argv validation for
//! the shell tool (threat model §6 gap #3, "Windows Shell Quoting Weakness").
//!
//! This module owns the *pure* half of shell launch planning: which quoting
//! dialect applies, how a single argument is quoted for that dialect, how the
//! full command string is assembled, and the fail-closed validation of
//! arguments that reach a cmd.exe verbatim operand. Nothing here spawns a
//! process, mutates the [`VirtualFs`](crate::virtual_fs::VirtualFs) overlay, or
//! consults policy — the policy engine and the executor in the parent module
//! consume these results.
//!
//! Security notes (why this is security-sensitive and must stay byte-stable):
//!
//! - cmd.exe expands `%...%` *before* the program runs and offers no escape
//!   outside batch files, so quoting alone cannot protect such an argument —
//!   [`validate_cmd_args`] rejects it instead (fail closed).
//! - Newlines/CR separate commands in cmd.exe's grammar and are rejected for
//!   the same reason.
//! - The POSIX escape for an embedded `'` is the 4-character `\''`; the
//!   5-character spelling leaks an extra `'` and breaks allowlist patterns
//!   anchored on the canonical escape.
//! - The Windows quoting rules are pure (the host is passed explicitly), so
//!   the Windows-only branches are exercised by unit tests on the Linux-only
//!   CI runner.

use concerto_core::ToolError;

/// Which quoting rule set (and launcher switch) a shell invocation uses.
///
/// Deliberately small: on Windows the only shell Concerto launches directly is
/// `cmd.exe`; every other shell an operator can select through
/// [`super::ShellConfig::shell`] (Git-Bash, MSYS2 bash, WSL sh, ...) speaks POSIX
/// `-c` quoting, and quoting it with cmd rules would be wrong in both
/// directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellDialect {
    /// sh/bash/zsh/dash: single-quote quoting, `-c` launcher switch.
    Posix,
    /// cmd.exe: cmd/CRT quoting, launched through [`cmd_verbatim_launch`].
    Cmd,
}

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
pub(super) fn cmd_verbatim_launch(prefix: &[String], full_command: &str) -> (Vec<String>, String) {
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
pub(super) fn shell_quote_windows(arg: &str) -> String {
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
pub(super) fn build_full_command(command: &str, args: &[String], dialect: ShellDialect) -> String {
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
pub(super) fn validate_cmd_args(args: &[String]) -> Result<(), ToolError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Threat model §6 #3 — Windows shell quoting hardening (DEFERRED row 42).
    // Everything here is pure: the Windows-only rules are exercised on the
    // Linux-only CI without a Windows host.
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
}
