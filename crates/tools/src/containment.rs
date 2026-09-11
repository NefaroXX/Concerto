//! Shell argv/cwd containment (ADR-55 §2, Phase 1b).
//!
//! [`common::resolve_path`] confines only the `filesystem` tool's paths; the
//! shell tool historically executed commands with no path confinement, so an
//! agent could `cd`, redirect, or pass an argument that lands outside the
//! session project root. As intent-gated authorization lands (Phase 1c), an
//! authorized-but-wrong shell command must not escape the scoped project —
//! this module is that compensating control.
//!
//! This is a **documented v1 heuristic**, deliberately conservative and easy
//! to extend, NOT a shell parser:
//!
//! - **Working directory.** A `cd <dir>` / `pushd <dir>` target at a command
//!   boundary that resolves outside the project root rejects the whole command
//!   without executing it. Bare `cd`, `cd -` (returns to `$OLDPWD`), and
//!   `popd` (a directory-stack restore a parser-less scan cannot track) are
//!   allowed.
//! - **Path arguments.** Path-like tokens (absolute, `./`/`../`, `~`-prefixed,
//!   separator-bearing, or extension-bearing) that resolve outside the root are
//!   rejected for mutation-capable verbs. Read-only verbs ([`READ_ONLY_VERBS`])
//!   are exempt — `cat /etc/os-release` is a legitimate diagnostic and keeps
//!   working. `sed`, `grep`, and `awk` count as read-only only when none of
//!   their recognized write flags is present.
//! - **Redirect writes.** `> file`, `>> file`, `2> file`, `&> file`, and glued
//!   forms (`2>/tmp/x`) whose target resolves outside the root are rejected
//!   regardless of the verb — a read-only verb does not get to write outside
//!   the project.
//! - **git `-C <dir>`.** Mutation subcommands with a `-C` target outside the
//!   root are rejected; read subcommands (`status`/`log`/`diff`/`show`/
//!   `branch`/`fetch`) are exempt so `git -C <outside> status` stays usable as
//!   a diagnostic. `fetch` is network egress — normal policy gates it;
//!   containment only checks paths.
//! - **Empty project root.** With an empty/absent project root there is no
//!   trust anchor to confine against, so containment is disabled (a session
//!   with no root has nothing to scope).
//!
//! Resolution mirrors [`common::resolve_path`]: relative tokens resolve
//! against the effective (canonical) working directory, `~` expands to the
//! home directory, and candidates are canonicalized to defeat `..`/symlink
//! tricks. When a candidate does not yet exist its deepest existing ancestor
//! is canonicalized instead, so creating new in-root files stays reachable
//! while a `..`/symlink climb is still caught. If the workspace root itself
//! cannot be canonicalized, containment fails closed (rejects) because the
//! trust anchor is unverifiable.
//!
//! Known v1 limitations (accepted and documented): wrapping verbs (`sh -c`,
//! `bash -c`) with a `cd` inside the quoted command string are not broken out;
//! the shell directory stack (`popd` restore targets) is not tracked; quoted
//! content that *looks* like a redirect/path (e.g. `echo "a > /outside"`) is
//! treated conservatively (may block); short-flag bundles (`sed -in`) count as
//! in-place writes via prefix matching, so they are not exempt.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use concerto_core::ToolError;
use std::collections::HashSet;

/// Read-only verbs exempt from the outside-root path-argument block.
///
/// Observe-tier reads: `cat /etc/os-release` is a legitimate diagnostic and
/// must keep working. Extend this table freely for other read-only tools.
const READ_ONLY_VERBS: &[&str] = &[
    "cat", "grep", "ls", "head", "tail", "less", "file", "stat", "which", "echo", "uname",
    "printf", "type", "dirname", "basename", "wc", "sort", "uniq", "cut", "sed", "awk",
];

/// In-place-write verbs among [`READ_ONLY_VERBS`].
///
/// Plain `sed`/`grep`/`awk` count as read-only only when none of the listed
/// write flags is present as a separate token. `grep -i` is case-insensitive
/// (read) and stays read-only; `grep -w` is treated conservatively as a write
/// marker per the ADR-55 v1 contract.
const INPLACE_WRITE_FLAGS: &[(&str, &[&str])] =
    &[("sed", &["-i", "--in-place"]), ("grep", &["-w"]), ("awk", &["-i", "--in-place", "-w"])];

/// Write-redirect operators whose target is contained regardless of the verb.
const WRITE_REDIRECT_OPERATORS: &[&str] = &[">", ">>", "2>", "2>>", "&>", "&>>", ">|"];

/// git subcommands that mutate the working tree or repository. A `-C <dir>`
/// pointing outside the root is rejected for these.
const GIT_MUTATE_SUBCOMMANDS: &[&str] =
    &["add", "commit", "push", "reset", "clean", "checkout", "stash", "merge", "rebase", "restore"];

/// git subcommands exempt from the `-C <dir>` check. `fetch` is network egress
/// and left to normal policy — containment only checks paths.
const GIT_READ_SUBCOMMANDS: &[&str] = &["status", "log", "diff", "show", "branch", "fetch"];

/// Windows reserved device names (`nul`, `con`, `prn`, `aux`, `com1`..`com9`,
/// `lpt1`..`lpt9`). Windows normally refuses these as file names, but the
/// `\\?\` extended-length path prefix bypasses that check, so a literal
/// 0-byte file named `nul` can be created — the observed bug. Matched
/// case-insensitively against the exact token/basename only; `nul.txt` and
/// `com10` are ordinary files. Shared by containment (device targets are
/// exempt like `/dev/null`), the filesystem tool (device targets are
/// rejected), and the shell tool's Windows cleanup (stale device files are
/// removed after a `bash -c` run).
pub(crate) const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "nul", "con", "prn", "aux", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Tokens that start a new shell command position for `cd`/`pushd` detection.
fn is_command_boundary(prev: &str) -> bool {
    matches!(
        prev,
        "&&" | "||" | "|" | ";" | "&" | ";;" | "(" | ")" | "{" | "}" | "!" | "then" | "do" | "else"
    ) || prev.ends_with(';')
        || prev.ends_with('&')
        || prev.ends_with('|')
}

/// Whether `token` looks like a path for containment purposes: an absolute
/// path, a `./`/`../`-relative path, a `~`-prefixed path, any token containing
/// a path separator, or an extension-bearing bare name (`notes.txt`). Hidden
/// files (`.env`) and the `.`/`..` special names are not extension-bearing.
fn is_path_like(token: &str) -> bool {
    if token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('~')
        || token.contains('/')
        || token.contains('\\')
    {
        return true;
    }
    match token.rfind('.') {
        Some(dot) => {
            let name = &token[..dot];
            let extension = &token[dot + 1..];
            !name.is_empty() && !extension.is_empty()
        }
        None => false,
    }
}

/// Whether `verb` counts as read-only for this invocation: a plain
/// [`READ_ONLY_VERBS`] member, or an in-place verb whose recognized write
/// flags are all absent from the remaining tokens.
fn is_read_only(verb: &str, trailing: &[String]) -> bool {
    if !READ_ONLY_VERBS.contains(&verb) {
        return false;
    }
    let Some(write_flags) = INPLACE_WRITE_FLAGS.iter().find(|(v, _)| *v == verb) else {
        return true;
    };
    let write_flags = write_flags.1;
    // Read-only only while no recognized write flag is present: `sed -i` /
    // `--in-place` writes in place, so a write-flagged invocation must not be
    // exempt from the outside-root path-argument block, while plain `sed`
    // stays a read.
    !trailing.iter().any(|arg| {
        write_flags.iter().any(|flag| arg == *flag || (flag.len() > 1 && arg.starts_with(flag)))
    })
}

/// Flatten command + args into whitespace-separated tokens with surrounding
/// shell quotes stripped, mirroring the denylist's flattened-string scan.
/// Newline/CR list separators (ADR-55 shell lists: `a\nb` sequences the shell
/// as two commands) cannot survive whitespace tokenization, so they are
/// translated into explicit `;` separator tokens that [`list_segments`] can
/// split on — a quoted multi-line body conservatively gains the separator too.
fn flat_tokens(command: &str, args: &[String]) -> Vec<String> {
    let mut flat = command.replace(['\n', '\r'], " ; ");
    for arg in args {
        flat.push(' ');
        flat.push_str(&arg.replace(['\n', '\r'], " ; "));
    }
    flat.split_whitespace()
        .filter(|token| !token.is_empty())
        .map(|token| token.trim_matches(|c| c == '\'' || c == '"').to_string())
        .collect()
}

/// Expand `~`/`~/...` and anchor `target` against `cwd`, returning the
/// lexical candidate (not yet canonicalized).
fn build_candidate(cwd: &Utf8Path, target: &str) -> Result<Utf8PathBuf, ToolError> {
    if target == "~" || target.starts_with("~/") {
        let home = home_dir().ok_or_else(|| ToolError::ExecutionFailed {
            message: "containment: cannot expand '~' (no home directory)".into(),
        })?;
        let home = Utf8PathBuf::from_path_buf(home).map_err(|_| ToolError::ExecutionFailed {
            message: "containment: home directory is not valid UTF-8".into(),
        })?;
        if target == "~" {
            return Ok(home);
        }
        return Ok(home.join(&target[2..]));
    }
    if target.starts_with('/') {
        // Git-Bash/MSYS reports Windows drives as `/c/...` (or `//c/...`).
        // On Windows, map those to the native `C:/...` form so the candidate
        // can be compared against the Windows-style project root; on other
        // platforms the path is left verbatim and stays confined as before.
        if cfg!(windows) {
            if let Some(windows_form) = msys_drive_to_windows(target) {
                return Ok(Utf8PathBuf::from(windows_form));
            }
        }
        return Ok(Utf8PathBuf::from(target));
    }
    Ok(cwd.join(target))
}

/// Map an MSYS/Git-Bash drive-letter absolute path to its Windows absolute
/// form: one or two leading slashes followed by a single ASCII letter and then
/// `/` or end-of-input becomes `<letter>:/...` (`/c/foo` -> `C:/foo`,
/// `//c/foo` -> `C:/foo`, `/c` -> `C:`). Returns `None` for any other input so
/// non-drive absolute paths (`/etc`, `/tmp`) keep their Unix form and stay
/// confined exactly as before. Pure and platform-independent so it is
/// unit-testable everywhere; [`build_candidate`] applies it only on Windows.
pub(crate) fn msys_drive_to_windows(target: &str) -> Option<String> {
    let rest = target.strip_prefix("//").or_else(|| target.strip_prefix('/'))?;
    let mut chars = rest.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    // The drive letter must be followed by `/` or be the whole path.
    match chars.next() {
        Some('/') | None => {}
        Some(_) => return None,
    }
    let mut out = String::with_capacity(rest.len() + 1);
    out.push(letter.to_ascii_uppercase());
    out.push(':');
    out.push_str(&rest[letter.len_utf8()..]);
    Some(out)
}

/// Interpolation metacharacters that make a path target unresolvable at scan
/// time: the shell expands `$`/backtick substitutions against its own
/// environment, so the running command's real target is NOT the literal the
/// scanner anchors (`tee $HOME/x` scans as an in-root relative literal while
/// the shell writes to the real home — the `$HOME`-spelling asymmetry with
/// `~/x`, which is expanded and contained). Mirrors the `$`/backtick members
/// of [`SHELL_INTERPOLATION_CHARS`] (core authorization, tier classifier);
/// `%`/quote chars stay with the tier classifier (quotes are already stripped
/// per-token by `flat_tokens`).
const INTERPOLATION_TARGET_CHARS: &[char] = &['$', '`'];

/// Canonical form of the containment trust anchor.
fn canonical_root(root: &Utf8Path) -> Result<Utf8PathBuf, ToolError> {
    root.canonicalize_utf8().map_err(|error| ToolError::ExecutionFailed {
        message: format!(
            "containment: cannot canonicalize project root '{}': {error}; refusing to run an unconfined shell command",
            root
        ),
    })
}

/// Escape rejection: a path resolved outside the project root.
fn outside_root(target: &str) -> ToolError {
    ToolError::VirtualFsConflict {
        path: Utf8PathBuf::from(target),
        reason: "shell containment (ADR-55): path resolves outside the project root; \
                 command rejected without execution"
            .into(),
    }
}

/// Link-escape rejection: the resolved path passes through a symlink that
/// points outside the project root.
fn link_escape(target: &str) -> ToolError {
    ToolError::VirtualFsConflict {
        path: Utf8PathBuf::from(target),
        reason: "shell containment (ADR-55): resolved path passes through a \
                 symlink outside the project root; command rejected without \
                 execution"
            .into(),
    }
}

/// Whether `target` is a device token exempt from containment: the exact
/// literal `/dev/null`, or a Windows reserved device name ([`WINDOWS_DEVICE_NAMES`])
/// matched case-insensitively. The whole token must be the device — `nul.txt`,
/// `com10`, `/dev/nullx`, and `dir/nul` are ordinary paths and stay confined.
fn is_device_target(target: &str) -> bool {
    if target == "/dev/null" {
        return true;
    }
    WINDOWS_DEVICE_NAMES.iter().any(|device| target.eq_ignore_ascii_case(device))
}

/// Strip trailing shell punctuation/separators that a flattened-token scan
/// may have captured into a redirect target or path argument: `;`, `&` (a
/// bare background `&` or the tail of `&&`), `|`, whitespace, and `\r` (CRLF).
/// Only the trailing run is removed — internal characters are never touched —
/// so `2>/dev/null;` and `nul;` resolve to `/dev/null` and `nul`. When
/// trimming empties the token the original is returned, so an all-punctuation
/// token stays confined like any other path.
fn strip_trailing_command_punct(token: &str) -> &str {
    let trimmed = token.trim_end_matches([';', '&', '|', ' ', '\t', '\n', '\r']);
    if trimmed.is_empty() {
        token
    } else {
        trimmed
    }
}

/// Resolve `target` against `cwd` and canonicalize, rejecting any path that
/// lands outside `root`. Mirrors [`common::resolve_path`]: `~` expands to the
/// home directory, absolute targets are used as-is, and non-existent paths
/// resolve through their deepest existing ancestor (rejecting link-like
/// components) so new in-root files stay reachable without letting `..`/
/// symlink tricks climb above the root.
fn resolve_within(root: &Utf8Path, cwd: &Utf8Path, target: &str) -> Result<Utf8PathBuf, ToolError> {
    // An interpolation-carrying target is unresolvable, not in-root: the
    // shell expands the metacharacters after containment returns, so the
    // lexical candidate is a lie. Reject without execution (2026-09-11: the
    // `$HOME`-spelling asymmetry — `~/x` expands and is contained, `$HOME/x`
    // scanned as a harmless relative literal). `/dev/null`-style devices
    // below cannot contain these characters, so order is immaterial.
    if target.chars().any(|c| INTERPOLATION_TARGET_CHARS.contains(&c)) {
        return Err(ToolError::VirtualFsConflict {
            path: Utf8PathBuf::from(target),
            reason: "shell containment (ADR-55): target contains shell-interpolation \
                     metacharacters ('$' or '`'); the path the shell resolves cannot \
                     be scanned and the command is rejected without execution"
                .into(),
        });
    }
    // `/dev/null` and the Windows reserved device names (`nul`, `con`, `prn`,
    // `aux`, `com1`..`com9`, `lpt1`..`lpt9`) are devices, not confined files:
    // canonicalization and root-prefix checks cannot meaningfully confine
    // them, and writing to one is harmless/ephemeral. Only the exact token is
    // exempt — `nul.txt`, `com10`, `/dev/nullx`, and `dir/nul` are ordinary
    // paths and stay confined like any absolute path. `~`-relative forms
    // (`~/dev/null`, `~/nul`) are not the device and still expand below.
    if is_device_target(target) {
        return Ok(Utf8PathBuf::from(target));
    }
    let candidate = build_candidate(cwd, target)?;
    let root_canonical = canonical_root(root)?;

    if let Ok(exists) = candidate.canonicalize_utf8() {
        if exists.starts_with(&root_canonical) {
            return Ok(exists);
        }
        return Err(outside_root(target));
    }

    // The candidate does not exist yet: canonicalize the deepest existing
    // ancestor (verifying it stays within the root), then re-append the
    // remaining segments, collapsing `..` and rejecting link-like components
    // so a dangling symlink cannot smuggle the new path outside the root.
    let mut ancestor = candidate.to_path_buf();
    let existing_ancestor = loop {
        let Some(parent) = ancestor.parent() else {
            return Err(ToolError::ExecutionFailed {
                message: format!("containment: cannot resolve '{target}' (no existing ancestor)"),
            });
        };
        match parent.canonicalize_utf8() {
            Ok(buf) => {
                // A deepest existing ancestor that resolves outside the root
                // was reached through a link or a `..` climb. A link climb is
                // a deliberate escape and named as such; a plain `..` climb is
                // reported as outside-root.
                if !buf.starts_with(&root_canonical)
                    && escapes_via_link_below(&root_canonical, parent)
                {
                    return Err(link_escape(target));
                }
                break buf;
            }
            Err(_) => ancestor = parent.to_path_buf(),
        }
    };
    if !existing_ancestor.starts_with(&root_canonical) {
        return Err(outside_root(target));
    }

    let suffix = candidate.strip_prefix(&existing_ancestor).unwrap_or(&candidate);
    let mut resolved = existing_ancestor;
    for component in suffix.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if let Some(parent) = resolved.parent() {
                    resolved = parent.to_path_buf();
                }
            }
            other => {
                resolved.push(other.as_str());
                if is_symlink(&resolved) {
                    return Err(link_escape(target));
                }
            }
        }
    }
    if !resolved.starts_with(&root_canonical) {
        return Err(outside_root(target));
    }
    Ok(resolved)
}

/// Whether `path` exists as a symlink (link-like). Used only while
/// reconstructing a not-yet-existing candidate, where canonicalization cannot
/// vouch for the final location.
fn is_symlink(path: &Utf8Path) -> bool {
    std::fs::symlink_metadata(path.as_std_path())
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// Whether walking `path` (a lexical path anchored at `root_canonical`)
/// crosses a symlink whose canonical target resolves outside the root. Used to
/// classify a deepest-existing-ancestor escape as a link climb rather than a
/// plain `..` climb, so a not-yet-existing candidate like `<root>/link/new.txt`
/// with `link -> /outside-dir` is rejected as link-like even though
/// canonicalizing `root/link` alone already escapes.
fn escapes_via_link_below(root_canonical: &Utf8Path, path: &Utf8Path) -> bool {
    let Ok(relative) = path.strip_prefix(root_canonical) else {
        return false;
    };
    let mut prefix = root_canonical.to_path_buf();
    for component in relative.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                // A parent climb is a plain `..`, not a link.
                prefix.pop();
            }
            other => {
                prefix.push(other.as_str());
                if is_symlink(&prefix) {
                    if let Ok(target) = prefix.canonicalize_utf8() {
                        if !target.starts_with(root_canonical) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Best-effort resolution of the user's home directory.
fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
}

/// Scan for `cd` / `pushd` targets that change the working directory to a
/// location outside the project root. Targets already resolved in-root are
/// added to `exempt` so the general path scan does not re-report them.
fn scan_directory_changes(
    root: &Utf8Path,
    cwd: &Utf8Path,
    tokens: &[String],
    exempt: &mut HashSet<usize>,
) -> Result<(), ToolError> {
    for (i, token) in tokens.iter().enumerate() {
        if token != "cd" && token != "pushd" {
            continue;
        }
        // Only a `cd`/`pushd` at a command boundary changes the directory; a
        // `cd` mid-arguments (e.g. `echo cd /tmp`) is a plain argument.
        if i > 0 && !is_command_boundary(&tokens[i - 1]) {
            continue;
        }
        // Skip option tokens (`cd -P path`); the next non-option token is the
        // target. Bare `cd`/`cd -`/`pushd` (no target) are directory-stack /
        // home operations and stay allowed.
        let mut j = i + 1;
        while j < tokens.len() && tokens[j].starts_with('-') && tokens[j] != "--" {
            j += 1;
        }
        if j >= tokens.len() {
            continue;
        }
        if tokens[j] == "--" {
            // `cd -- path` marks the end of options; a bare `cd --` is allowed.
            if j + 1 >= tokens.len() {
                continue;
            }
            j += 1;
        }
        resolve_within(root, cwd, &tokens[j])?;
        exempt.insert(j);
    }
    Ok(())
}

/// Scan for write-redirect targets (`> file`, `>> file`, `2> file`, `&> file`,
/// and glued forms) that resolve outside the project root. The block applies
/// regardless of the verb: a read-only verb must not write outside the root.
/// Input-redirect (`< file`, glued `<file`) targets are checked the same way
/// (Finding C): reading from outside the root is the same scope escape —
/// `cat </etc/passwd` must not bypass containment just because `cat` is a
/// read-only verb.
fn scan_redirects(
    root: &Utf8Path,
    cwd: &Utf8Path,
    tokens: &[String],
    exempt: &mut HashSet<usize>,
) -> Result<(), ToolError> {
    for (i, token) in tokens.iter().enumerate() {
        if WRITE_REDIRECT_OPERATORS.contains(&token.as_str()) {
            if let Some(target) = tokens.get(i + 1) {
                resolve_within(root, cwd, strip_trailing_command_punct(target))?;
                exempt.insert(i + 1);
            }
            continue;
        }
        // Input-redirect operator (`< file`): the target is read, but from
        // outside the root it is still an escape.
        if token == "<" {
            if let Some(target) = tokens.get(i + 1) {
                resolve_within(root, cwd, strip_trailing_command_punct(target))?;
                exempt.insert(i + 1);
            }
            continue;
        }
        // Operator glued to its target: `2>/tmp/x`, `>file`, `</etc/passwd`.
        if let Some(idx) = token.rfind('>') {
            let after = token[idx + 1..].trim_matches(|c| c == '\'' || c == '"');
            let after = strip_trailing_command_punct(after);
            if !after.is_empty() && is_path_like(after) {
                resolve_within(root, cwd, after)?;
            }
            continue;
        }
        // Input-redirect glued to its target: `</etc/passwd`.
        if let Some(idx) = token.find('<') {
            let after = token[idx + 1..].trim_matches(|c| c == '\'' || c == '"');
            let after = strip_trailing_command_punct(after);
            if !after.is_empty() && is_path_like(after) {
                resolve_within(root, cwd, after)?;
            }
        }
    }
    Ok(())
}

/// Scan `git -C <dir>` invocations: mutation subcommands with a `-C` target
/// outside the root are rejected; read subcommands are exempt. Every `-C`
/// value is added to `exempt` — the git rule owns those tokens, so the general
/// path scan must not re-block a read `-C` outside the root.
fn scan_git_change_dir(
    root: &Utf8Path,
    cwd: &Utf8Path,
    tokens: &[String],
    exempt: &mut HashSet<usize>,
) -> Result<(), ToolError> {
    if tokens.first().map(String::as_str) != Some("git") {
        return Ok(());
    }

    // Determine the subcommand: the first token after `git` that is neither a
    // flag nor the value of a `-C`/`--git-dir`/`--work-tree` option.
    let mut subcommand: Option<&str> = None;
    let mut skip_next = false;
    for token in tokens.iter().skip(1) {
        if subcommand.is_some() {
            break;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "-C" || token == "--git-dir" || token == "--work-tree" {
            skip_next = true;
            continue;
        }
        if token.starts_with("-C") && token.len() > 2 {
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        subcommand = Some(token.as_str());
    }
    let mutates = subcommand.is_some_and(|sub| GIT_MUTATE_SUBCOMMANDS.contains(&sub));
    let reads = subcommand.is_some_and(|sub| GIT_READ_SUBCOMMANDS.contains(&sub));

    let mut i = 1;
    while i < tokens.len() {
        let token = &tokens[i];
        // Resolution order: read the `-C` value up front, then advance `i`.
        let value: Option<(usize, &str)> = if token == "-C" {
            let value = tokens.get(i + 1).map(|s| (i + 1, s.as_str()));
            i += 2;
            value
        } else if token.starts_with("-C") && token.len() > 2 {
            let value = Some((i, &token[2..]));
            i += 1;
            value
        } else {
            i += 1;
            None
        };
        if let Some((value_index, value)) = value {
            if mutates {
                resolve_within(root, cwd, value)?;
                exempt.insert(value_index);
            } else if reads {
                // A read `-C` outside the root is exempt from the general path
                // scan so `git -C <outside> status` stays usable.
                exempt.insert(value_index);
            }
            // Unknown/absent subcommand: fall through to the general path scan
            // (git is a mutation-capable verb there), pre-exempting nothing.
        }
    }
    Ok(())
}

/// A list segment of the flattened token list: the content sections (each a
/// flattened-token index plus its pre-boundary character content) the shell
/// will sequence as one command, plus the segment's leading verb when one is
/// known. Every shell list separator ends the current segment — standalone
/// (`|`, `&&`, `||`, `;`) or glued inside a token (`/;`, `ls&&rm`); a
/// non-empty post-glue section (`ls|tee` → `tee`) becomes the next segment's
/// leading verb, so the first token after the boundary is governed by the
/// running executable the shell actually resolves there.
struct ListSegment {
    sections: Vec<(usize, String)>,
    lead: Option<String>,
}

/// Width (in characters) of the shell operator at `bytes[pos..]`, or `0` when
/// that position starts no separator. Operator-aware exactly like the tier
/// classifier's [`SHELL_SEGMENT_SEPARATORS`] split: glued `&&`/`||` are ONE
/// boundary (`a||cat /etc/os-release` must not yield a bogus `|`-prefixed
/// lead that misclassifies a legitimate fallback), while a lone `|` is the
/// pipe and single `&`/`;`/`\n`/`\r` are plain list separators.
fn separator_width(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    match bytes[pos] {
        b'&' if bytes.get(pos + 1) == Some(&b'&') => 2,
        b'|' if bytes.get(pos + 1) == Some(&b'|') => 2,
        b'&' | b'|' | b';' | b'\n' | b'\r' => 1,
        _ => 0,
    }
}

/// Split one flattened token into [`ListSegment`] content sections: alternating
/// non-separator content (carrying the token's index) and separator runs
/// (`None`), with `&&`/`||` kept as single boundaries.
fn token_sections(index: usize, token: &str) -> Vec<Option<(usize, String)>> {
    let mut sections = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = token[cursor..].find(['&', '|', ';', '\n', '\r']) {
        let pos = cursor + rel;
        let head = &token[cursor..pos];
        if !head.is_empty() {
            sections.push(Some((index, head.to_string())));
        }
        let width = separator_width(token, pos);
        sections.push(None);
        cursor = pos + width;
    }
    let tail = &token[cursor..];
    if !tail.is_empty() {
        sections.push(Some((index, tail.to_string())));
    }
    sections
}

/// Split the flattened token list into [`ListSegment`]s, modeling command
/// lists the way the shell will sequence them: on every separator —
/// `&`, `|`, `;`, `\n`, `\r` — standalone, glued inside a token, or trailing a
/// token, with glued `&&`/`||` recognized as single boundaries. Segments that
/// contain no content sections are skipped. This mirrors the tier classifier's
/// segment split (`authorization.rs command_segments`) so a read-only leading
/// verb exempts only its own list segment: `ls /; rm /etc/shadow` and
/// `ls / && rm -f ~/…` cannot launder the mutating segment behind `ls`'s
/// read-only tier.
fn list_segments(tokens: &[String]) -> Vec<ListSegment> {
    let mut segments = Vec::new();
    let mut current: Option<ListSegment> = None;
    for (index, token) in tokens.iter().enumerate() {
        for section in token_sections(index, token) {
            match section {
                None => {
                    if let Some(segment) = current.take() {
                        segments.push(segment);
                    }
                }
                Some(section) => {
                    let segment = current
                        .get_or_insert_with(|| ListSegment { sections: Vec::new(), lead: None });
                    segment.sections.push(section.clone());
                    if segment.lead.is_none() {
                        segment.lead = Some(section.1);
                    }
                }
            }
        }
    }
    if let Some(segment) = current {
        segments.push(segment);
    }
    segments
}

/// General path-argument containment: for mutation-capable leads, any
/// path-like token that resolves outside the project root rejects the
/// command. The exemption is PER LIST SEGMENT: a read-only leading verb
/// exempts only its own segment's tokens — a later `tee`/mutating segment in
/// a pipe (`cat f | tee /tmp/x`) is a self-contained mutation and must have
/// its targets contained like any other write.
///
/// A glued token may carry content sections of SEVERAL segments (`cat a||rm
/// x` → sections `a` and the tail); a token is read-only-exempt only when
/// EVERY content section it carries belongs to a read-only segment — any
/// mutating section on the same token disables the exemption for the whole
/// token (conservative: a false positive only costs a rejection).
fn scan_path_arguments(
    root: &Utf8Path,
    cwd: &Utf8Path,
    tokens: &[String],
    exempt: &HashSet<usize>,
) -> Result<(), ToolError> {
    // Per original token: how many content sections cover it, and whether all
    // covering segments are read-only. A token with zero content sections
    // (pure separator) stays out of the general scan.
    let mut section_count = vec![0u32; tokens.len()];
    let mut all_read_only = vec![true; tokens.len()];
    for segment in list_segments(tokens) {
        let lead = segment.lead.clone().unwrap_or_default();
        // A segment's trailing tokens for write-flag detection: the flattened
        // tokens carrying this segment's content (the lead's own token
        // included only when it is not the lead itself).
        let trailing: Vec<String> =
            segment.sections.iter().skip(1).map(|(index, _)| tokens[*index].clone()).collect();
        let read_only = is_read_only(&lead, &trailing);
        for (index, _) in &segment.sections {
            section_count[*index] += 1;
            if !read_only {
                all_read_only[*index] = false;
            }
        }
    }
    for (i, token) in tokens.iter().enumerate() {
        if exempt.contains(&i) || i == 0 || section_count[i] == 0 {
            continue;
        }
        if all_read_only[i] {
            continue;
        }
        if is_path_like(token) {
            resolve_within(root, cwd, strip_trailing_command_punct(token))?;
        }
    }
    Ok(())
}

/// Validate that a shell command's working-directory changes, redirect writes,
/// and path-like arguments stay within the session project root (ADR-55 §2,
/// Phase 1b).
///
/// `root` is the containment trust anchor (the session project directory);
/// `effective_cwd` is the working directory the process will actually be
/// spawned in. Any violation rejects the command — the caller must not spawn
/// anything after this returns an error. With an empty/absent `root`,
/// containment is disabled (there is no trust anchor to confine against).
pub(crate) fn contain_shell_command(
    root: &Utf8Path,
    effective_cwd: &Utf8Path,
    command: &str,
    args: &[String],
) -> Result<(), ToolError> {
    if root.as_str().trim().is_empty() {
        return Ok(());
    }

    let cwd = resolve_within(root, root, effective_cwd.as_str())?;
    let tokens = flat_tokens(command, args);
    if tokens.is_empty() {
        return Ok(());
    }

    // Token indices whose targets are owned by a dedicated rule (the program
    // token itself, `cd`/`pushd` targets, redirect targets, git `-C` values).
    let mut exempt = HashSet::new();
    exempt.insert(0);

    scan_directory_changes(root, &cwd, &tokens, &mut exempt)?;
    scan_redirects(root, &cwd, &tokens, &mut exempt)?;
    scan_git_change_dir(root, &cwd, &tokens, &mut exempt)?;
    scan_path_arguments(root, &cwd, &tokens, &exempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellTool;
    use crate::testing::AllowAllPolicy;
    use concerto_core::ids::Ulid;
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::SessionContext;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// Fresh temp root; returns (root, TempDir) keeping the dir alive.
    fn temp_root() -> (Utf8PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 temp path");
        (root, dir)
    }

    fn session(root: &Utf8Path) -> SessionContext {
        SessionContext::new(Ulid::new(), root.as_std_path().to_path_buf())
    }

    // -----------------------------------------------------------------------
    // Working directory: cd / pushd containment.
    // -----------------------------------------------------------------------
    // Working directory: cd / pushd containment.
    // -----------------------------------------------------------------------

    #[test]
    fn cd_absolute_escape_rejected() {
        let (root, _dir) = temp_root();
        let err = contain_shell_command(&root, &root, "cd", &["/tmp".into()]).unwrap_err();
        assert!(err.to_string().contains("/tmp"), "must name the offending path: {err}");
    }

    #[test]
    fn cd_parent_from_root_rejected() {
        let (root, _dir) = temp_root();
        let err = contain_shell_command(&root, &root, "cd", &["..".into()]).unwrap_err();
        assert!(err.to_string().contains(".."), "must name the offending path: {err}");
    }

    #[test]
    fn cd_double_parent_from_subdir_rejected() {
        let (root, dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        // cwd = root/subdir; `../..` climbs above root.
        let err = contain_shell_command(&root, &sub, "cd", &["../..".into()]).unwrap_err();
        assert!(err.to_string().contains("../.."), "must name the offending path: {err}");
        let _ = dir;
    }

    #[test]
    fn cd_within_root_allowed() {
        let (root, _dir) = temp_root();
        let sub = root.join("subdir");
        std::fs::create_dir(&sub).expect("create subdir");
        contain_shell_command(&root, &root, "cd", &["subdir".into()])
            .expect("in-root cd is allowed");
        // `cd ..` from a subdir lands back in the root — allowed.
        contain_shell_command(&root, &sub, "cd", &["..".into()])
            .expect("cd .. from subdir stays in root");
    }

    #[test]
    fn pushd_escape_rejected_bare_cd_and_cd_dash_allowed() {
        let (root, _dir) = temp_root();
        assert!(contain_shell_command(&root, &root, "pushd", &["/tmp".into()]).is_err());
        contain_shell_command(&root, &root, "cd", &[]).expect("bare cd allowed");
        contain_shell_command(&root, &root, "cd", &["-".into()]).expect("cd - allowed");
        contain_shell_command(&root, &root, "popd", &[]).expect("popd allowed");
    }

    #[test]
    fn cd_as_argument_is_not_a_directory_change() {
        let (root, _dir) = temp_root();
        // `echo cd /tmp` prints a message; the `cd` here is not a builtin.
        contain_shell_command(&root, &root, "echo", &["cd".into(), "/tmp".into()])
            .expect("argument cd must not trigger containment");
    }

    #[test]
    fn cd_after_separator_is_a_directory_change() {
        let (root, _dir) = temp_root();
        let err = contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), "&&".into(), "cd".into(), "/tmp".into()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("/tmp"), "compound cd must be caught: {err}");
    }

    // -----------------------------------------------------------------------
    // Read-only verb exemption vs mutation-capable path arguments.
    // -----------------------------------------------------------------------

    #[test]
    fn interpolation_metachar_targets_are_unresolvable() {
        let (root, dir) = temp_root();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        // The `$HOME`-spelling asymmetry (2026-09-11): `$HOME/x` scanned as an
        // in-root relative literal while the shell expands it to the real
        // home (the same as `~/x`, which containment blocks). Targets carrying
        // `$` / a backtick are UNRESOLVABLE at scan time → reject.
        for command in ["tee $HOME/x", "cat f > $HOME/x", ">$HOME/x", "cat f >> $(pwd)/x"] {
            let err = contain_shell_command(&root, &root, command, &[]).unwrap_err();
            assert!(
                err.to_string().contains("shell-interpolation"),
                "'{command}' must reject the interpolated target, got: {err}"
            );
        }
        // The backtick-substituted form rejects like `$HOME`.
        let err = contain_shell_command(&root, &root, "tee", &["`pwd`/x".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("shell-interpolation"),
            "backtick target must reject, got: {err}"
        );
        // Plain in-root relative targets keep passing — no over-block.
        contain_shell_command(&root, &root, "tee out.txt", &[]).expect("in-root tee allowed");
        contain_shell_command(&root, &root, "cat f > out.txt", &[]).expect("in-root > allowed");
        contain_shell_command(&root, &root, "cat f > sub/out.txt", &[])
            .expect("in-root subdir > allowed");
    }

    #[test]
    fn read_only_verb_outside_absolute_allowed() {
        let (root, _dir) = temp_root();
        // `cat /etc/os-release` is a legitimate diagnostic.
        contain_shell_command(&root, &root, "cat", &["/etc/os-release".into()])
            .expect("read-only outside read must stay allowed");
        contain_shell_command(&root, &root, "ls", &["/tmp".into()]).expect("ls /tmp allowed");
        contain_shell_command(
            &root,
            &root,
            "head",
            &["-n".into(), "1".into(), "/etc/hosts".into()],
        )
        .expect("head of an outside file allowed");
    }

    #[test]
    fn mutation_verb_outside_path_blocked() {
        let (root, _dir) = temp_root();
        assert!(contain_shell_command(&root, &root, "rm", &["/tmp/foo".into()]).is_err());
        assert!(contain_shell_command(&root, &root, "cp", &["/tmp/a".into(), "b".into()]).is_err());
        let err =
            contain_shell_command(&root, &root, "touch", &["../../escape.txt".into()]).unwrap_err();
        assert!(err.to_string().contains("../../escape.txt"), "must name the path: {err}");
    }

    #[test]
    fn mutation_verb_extension_bearing_outside_blocked() {
        let (root, _dir) = temp_root();
        assert!(contain_shell_command(&root, &root, "touch", &["/somewhere.txt".into()]).is_err());
        // Extension-bearing in-root token stays allowed.
        contain_shell_command(&root, &root, "touch", &["notes.txt".into()]).expect("in-root ok");
    }

    #[test]
    fn inplace_write_flag_lifts_read_only_exemption() {
        let (root, _dir) = temp_root();
        // Plain `sed` on an outside file is a read — allowed.
        contain_shell_command(&root, &root, "sed", &["s/x/y/".into(), "/etc/hosts".into()])
            .expect("plain sed read allowed");
        // `sed -i` writes in place — the outside path is blocked.
        assert!(contain_shell_command(
            &root,
            &root,
            "sed",
            &["-i".into(), "s/x/y/".into(), "/etc/hosts".into()]
        )
        .is_err());
        assert!(contain_shell_command(
            &root,
            &root,
            "sed",
            &["--in-place".into(), "s/x/y/".into(), "/etc/hosts".into()]
        )
        .is_err());
        // grep -i stays read-only; grep -w is treated as write per the v1 contract.
        contain_shell_command(
            &root,
            &root,
            "grep",
            &["-i".into(), "x".into(), "/etc/hosts".into()],
        )
        .expect("grep -i is a read");
        assert!(contain_shell_command(
            &root,
            &root,
            "grep",
            &["-w".into(), "x".into(), "/etc/hosts".into()]
        )
        .is_err());
    }

    // -----------------------------------------------------------------------
    // Redirect writes.
    // -----------------------------------------------------------------------

    #[test]
    fn redirect_write_outside_blocked() {
        let (root, _dir) = temp_root();
        assert!(
            contain_shell_command(&root, &root, "cat", &[">".into(), "/outside/x".into()]).is_err()
        );
        assert!(contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), ">".into(), "/outside/x".into()]
        )
        .is_err());
        assert!(contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), "2>".into(), "/outside/err".into()]
        )
        .is_err());
        // Glued form.
        assert!(contain_shell_command(&root, &root, "cat", &[">/outside/glued".into()]).is_err());
        // In-root redirect stays allowed.
        contain_shell_command(&root, &root, "cat", &[">".into(), "out.txt".into()])
            .expect("in-root redirect allowed");
    }

    // -----------------------------------------------------------------------
    // Pipe modeling and input redirects (F5, Finding C).
    // -----------------------------------------------------------------------

    #[test]
    fn tee_pipe_target_outside_root_rejected() {
        let (root, _dir) = temp_root();
        // Read-only leading verb does not exempt a later tee segment: `cat`'s
        // exemption must not launder the tee-segment's write target.
        for command in [
            "cat f | tee /tmp/out",
            "cat f | tee ~/out",
            "cat f|tee /tmp/out",
            "ls | tee /tmp/out",
            "cat secret | tee ../escape",
        ] {
            let err = contain_shell_command(&root, &root, command, &[]).unwrap_err();
            assert!(
                err.to_string().contains("outside the project root"),
                "'{command}' must reject the tee target outside root, got: {err}"
            );
        }
        // A tee-subcommand prefix (e.g. `tee -a`) is still a write.
        assert!(
            contain_shell_command(&root, &root, "cat f | tee -a /tmp/out", &[]).is_err(),
            "tee -a with outside-root target must reject"
        );
    }

    #[test]
    fn read_only_pipe_stays_allowed_and_in_root_tee_allowed() {
        let (root, dir) = temp_root();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        // Read-only chains keep working.
        contain_shell_command(&root, &root, "cat f | grep x", &[]).expect("read pipe allowed");
        contain_shell_command(&root, &root, "ls sub | head", &[]).expect("ls|head allowed");
        contain_shell_command(&root, &root, "cat f", &[]).expect("plain cat allowed");
        contain_shell_command(&root, &root, "cat f|grep x", &[]).expect("glued pipe allowed");
        // In-root tee is allowed (existing path rules apply, not a new block).
        contain_shell_command(&root, &root, "cat f | tee out.txt", &[])
            .expect("in-root tee target allowed");
        contain_shell_command(&root, &root, "cat f | tee sub/out.txt", &[])
            .expect("in-root subdir tee target allowed");
    }

    #[test]
    fn fallback_splitter_lets_legit_fallback_through() {
        let (root, dir) = temp_root();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        // `||` (glued or spaced) is a boundary, not a pipe: the fallback
        // segment keeps its own read-only lead and legitimate fallback reads
        // stay allowed.
        contain_shell_command(&root, &root, "cat a || cat /etc/os-release", &[])
            .expect("spaced || fallback read allowed");
        contain_shell_command(&root, &root, "cat f||cat /etc/os-release", &[])
            .expect("glued || fallback read allowed");
        // The bogus `|`-lead must not survive splitting; unit-check segments.
        let segs = list_segments(&["cat".into(), "a||cat".into(), "/etc/os-release".into()]);
        let leads: Vec<Option<&str>> = segs.iter().map(|s| s.lead.as_deref()).collect();
        assert_eq!(leads, vec![Some("cat"), Some("cat")], "|| must not produce a `|` lead");
        // A lone `|` in a chain of reads stays read-only (unchanged).
        contain_shell_command(&root, &root, "cat f|grep x", &[]).expect("pipe read allowed");
    }

    #[test]
    fn read_only_lead_cannot_exempt_later_list_segments() {
        let (root, _dir) = temp_root();
        // The read-only exemption is scoped per list segment: a mutating
        // segment after `&`/`;`/`&&`/`||`/newline separators keeps scanning
        // even when the FIRST segment's verb is read-only.
        for command in [
            "ls /; rm /etc/shadow",
            "ls / && rm -f ~/.ssh/authorized_keys",
            "ls /; rm -rf ../outside",
            "ls /\nrm /etc/shadow",
            "cat f & rm /etc/shadow",
            "cat f||rm /etc/shadow",
            "ls /&&rm -f ../outside",
        ] {
            let err = contain_shell_command(&root, &root, command, &[]).unwrap_err();
            assert!(
                err.to_string().contains("outside the project root"),
                "'{command}' must reject the mutating later segment, got: {err}"
            );
        }
    }

    #[test]
    fn mutating_segment_keeps_path_containment() {
        let (root, dir) = temp_root();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        // A mutating second segment keeps its own path args contained: the
        // read-only exemption of `ls` does not cover `mkdir`'s targets.
        assert!(
            contain_shell_command(&root, &root, "ls | mkdir /tmp/pwned", &[]).is_err(),
            "escaping mkdir segment after pipe must reject"
        );
        // In-root mutating segment stays allowed.
        contain_shell_command(&root, &root, "ls sub | mkdir sub/newdir", &[])
            .expect("in-root mkdir segment allowed");
    }

    #[test]
    fn input_redirect_outside_root_rejected() {
        let (root, dir) = temp_root();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        // Finding C: `< /etc/passwd` reading outside the root is an escape.
        for command in ["cat < /etc/passwd", "cat </etc/passwd", "wc -l < /etc/passwd"] {
            let err = contain_shell_command(&root, &root, command, &[]).unwrap_err();
            assert!(
                err.to_string().contains("outside the project root"),
                "'{command}' must reject the input-redirect target, got: {err}"
            );
        }
        // In-root input redirects keep working.
        contain_shell_command(&root, &root, "cat < f", &[]).expect("in-root < allowed");
        contain_shell_command(&root, &root, "cat <f", &[]).expect("glued in-root < allowed");
    }

    // -----------------------------------------------------------------------
    // MSYS drive-letter paths (Windows Git-Bash) and the null device.
    // -----------------------------------------------------------------------

    #[test]
    fn msys_drive_to_windows_converts_drive_letter_forms() {
        assert_eq!(msys_drive_to_windows("/c/foo"), Some("C:/foo".into()));
        assert_eq!(msys_drive_to_windows("//c/foo"), Some("C:/foo".into()));
        assert_eq!(msys_drive_to_windows("/C/Users/x"), Some("C:/Users/x".into()));
        assert_eq!(msys_drive_to_windows("/c"), Some("C:".into()));
    }

    #[test]
    fn msys_drive_to_windows_non_drive_inputs_return_none() {
        assert_eq!(msys_drive_to_windows("/etc"), None);
        assert_eq!(msys_drive_to_windows("/home/user"), None);
        assert_eq!(msys_drive_to_windows("relative"), None);
        assert_eq!(msys_drive_to_windows("C:/abs"), None);
        assert_eq!(msys_drive_to_windows("~/x"), None);
        assert_eq!(msys_drive_to_windows("c"), None);
        assert_eq!(msys_drive_to_windows(""), None);
    }

    #[test]
    fn dev_null_resolves_without_escape_check() {
        let (root, _dir) = temp_root();
        let resolved = resolve_within(&root, &root, "/dev/null")
            .expect("literal /dev/null must pass containment");
        assert_eq!(resolved.as_str(), "/dev/null");
        // Redirecting to the null device is harmless regardless of the verb.
        contain_shell_command(&root, &root, "cat", &[">".into(), "/dev/null".into()])
            .expect("redirect to /dev/null must be allowed");
        contain_shell_command(&root, &root, "rm", &["/dev/null".into()])
            .expect("/dev/null as an argument must be allowed");
    }

    #[test]
    fn other_dev_paths_and_non_drive_absolutes_still_rejected() {
        let (root, _dir) = temp_root();
        // Other /dev/... paths are confined like any absolute path.
        assert!(contain_shell_command(&root, &root, "rm", &["/dev/sda".into()]).is_err());
        assert!(
            contain_shell_command(&root, &root, "cat", &[">".into(), "/dev/full".into()]).is_err()
        );
        // Non-drive absolute paths stay outside-root on every platform.
        let err = contain_shell_command(&root, &root, "cd", &["/etc".into()]).unwrap_err();
        assert!(err.to_string().contains("/etc"), "must name the path: {err}");
        assert!(contain_shell_command(&root, &root, "rm", &["/etc/passwd".into()]).is_err());
    }

    // -----------------------------------------------------------------------
    // Windows reserved device names (nul/con/prn/aux/com1..9/lpt1..9) and
    // trailing command separators on redirect targets.
    // -----------------------------------------------------------------------

    #[test]
    fn device_names_resolve_without_escape_check() {
        let (root, _dir) = temp_root();
        for name in ["nul", "NUL", "Con", "cOn", "PRN", "aux", "COM1", "com9", "lpt1", "LPT9"] {
            let resolved = resolve_within(&root, &root, name)
                .unwrap_or_else(|e| panic!("literal device '{name}' must pass containment: {e}"));
            assert_eq!(resolved.as_str(), name, "device token must be returned verbatim");
        }
    }

    #[test]
    fn device_names_allowed_as_redirect_target_and_argument() {
        let (root, _dir) = temp_root();
        for name in ["nul", "NUL", "Con", "PRN", "com1", "lpt9"] {
            // Redirect target: scan_redirects always resolves the target, so
            // the exemption must fire for every case.
            contain_shell_command(&root, &root, "cat", &[">".into(), name.into()])
                .unwrap_or_else(|e| panic!("redirect to '{name}' must be allowed: {e}"));
            // As an argument: `rm` is mutation-capable, so a non-exempt target
            // outside the root would be blocked; the device token is exempt.
            contain_shell_command(&root, &root, "rm", &[name.into()])
                .unwrap_or_else(|e| panic!("'{name}' as an argument must be allowed: {e}"));
        }
    }

    #[test]
    fn redirect_with_trailing_command_separator_is_trimmed() {
        let (root, _dir) = temp_root();
        // The live containment false positive: `2>/dev/null;` — the redirect
        // scanner captured the trailing `;` as part of the target, so the
        // exact-literal `/dev/null` exemption did not fire.
        contain_shell_command(&root, &root, "echo", &["hi".into(), "2>/dev/null;".into()])
            .expect("glued 2>/dev/null; must be allowed");
        contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), ">".into(), "/dev/null;".into()],
        )
        .expect("separated > /dev/null; must be allowed");
        // Device names carry the same trailing separator.
        contain_shell_command(&root, &root, "cat", &[">".into(), "nul;".into()])
            .expect("device target with trailing ; must be allowed");
        contain_shell_command(&root, &root, "cat", &[">".into(), "NUL;".into()])
            .expect("case-variant device target with trailing ; must be allowed");
        // Trimming must never silence a real escape: an outside-root redirect
        // with a trailing separator is still rejected.
        assert!(contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), ">/tmp/escape.txt;".into()]
        )
        .is_err());
    }

    #[test]
    fn device_exemption_does_not_widen_to_lookalikes() {
        let (root, _dir) = temp_root();
        // Extension-bearing and near-miss names are ordinary files: an
        // outside-root absolute target stays rejected exactly as before.
        for target in ["/tmp/nul.txt", "/tmp/com10", "/dev/nullx", "/dev/nul"] {
            assert!(
                contain_shell_command(&root, &root, "rm", &[target.into()]).is_err(),
                "outside-root absolute '{target}' must stay rejected"
            );
        }
        assert!(contain_shell_command(&root, &root, "cat", &[">".into(), "/tmp/nul.txt".into()])
            .is_err());
        // Inside-root lookalike names keep working as plain files — the
        // exemption only ever applies to the exact bare device name.
        contain_shell_command(&root, &root, "cat", &[">".into(), "nul.txt".into()])
            .expect("in-root nul.txt redirect stays a normal file");
        contain_shell_command(&root, &root, "cat", &[">".into(), "com10".into()])
            .expect("in-root com10 redirect stays a normal file");
    }

    #[test]
    fn genuinely_escaping_absolute_paths_still_rejected() {
        let (root, _dir) = temp_root();
        assert!(contain_shell_command(&root, &root, "cd", &["/tmp".into()]).is_err());
        assert!(contain_shell_command(&root, &root, "rm", &["/etc/passwd".into()]).is_err());
        assert!(contain_shell_command(
            &root,
            &root,
            "echo",
            &["hi".into(), ">".into(), "/outside/x".into()]
        )
        .is_err());
        assert!(contain_shell_command(&root, &root, "touch", &["../escape.txt".into()]).is_err());
    }

    #[test]
    fn trailing_command_punct_is_stripped_only_at_the_end() {
        let (root, _dir) = temp_root();
        // `&&` / bare `&` / `|` / `;` / whitespace / `\r` tails are stripped,
        // so device and `/dev/null` targets resolve as their exact literals.
        for target in ["/dev/null&&", "/dev/null&", "/dev/null |", "/dev/null;  ", "/dev/null\r"] {
            let resolved = resolve_within(&root, &root, strip_trailing_command_punct(target))
                .expect("trailing command punctuation must be stripped");
            assert_eq!(resolved.as_str(), "/dev/null");
        }
        // Internal punctuation is never touched.
        assert_eq!(strip_trailing_command_punct("a;b"), "a;b");
        assert_eq!(strip_trailing_command_punct("2>/dev/null; echo hi"), "2>/dev/null; echo hi");
        // An all-punctuation token returns itself, so it stays confined.
        assert_eq!(strip_trailing_command_punct(";;;"), ";;;");
        // Trimming never turns a real escape into an allowed path.
        assert!(contain_shell_command(&root, &root, "rm", &["/tmp/escape.txt;".into()]).is_err());
    }

    // -----------------------------------------------------------------------
    // git -C containment.
    // -----------------------------------------------------------------------

    #[test]
    fn git_change_dir_read_verb_outside_allowed() {
        let (root, _dir) = temp_root();
        contain_shell_command(&root, &root, "git", &["-C".into(), "/tmp".into(), "status".into()])
            .expect("git -C /tmp status must stay allowed (read verb)");
        contain_shell_command(&root, &root, "git", &["-C".into(), "/tmp".into(), "log".into()])
            .expect("git -C /tmp log allowed");
    }

    #[test]
    fn git_change_dir_mutate_verb_outside_blocked() {
        let (root, _dir) = temp_root();
        for sub in [
            "add", "commit", "push", "reset", "clean", "checkout", "stash", "merge", "rebase",
            "restore",
        ] {
            let err = contain_shell_command(
                &root,
                &root,
                "git",
                &["-C".into(), "/tmp".into(), sub.into()],
            )
            .expect_err("mutate git -C outside the root must be rejected");
            assert!(
                err.to_string().contains("/tmp"),
                "{sub}: rejection must name the offending -C path: {err}"
            );
        }
        // Glued `-C/tmp` form.
        assert!(contain_shell_command(&root, &root, "git", &["-C/tmp".into(), "commit".into()])
            .is_err());
        // In-root `-C` for a mutate verb stays allowed.
        contain_shell_command(&root, &root, "git", &["-C".into(), ".".into(), "commit".into()])
            .expect("git -C . commit allowed");
    }

    #[test]
    fn git_general_path_argument_blocked() {
        let (root, _dir) = temp_root();
        // `git` is not read-only; an absolute outside path argument is blocked
        // by the general rule even with a read subcommand.
        assert!(contain_shell_command(&root, &root, "git", &["add".into(), "/outside/x".into()])
            .is_err());
    }

    // -----------------------------------------------------------------------
    // Symlink escape.
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn symlink_escape_blocked() {
        let (root, dir) = temp_root();
        let outside = dir.path().parent().expect("temp parent");
        std::os::unix::fs::symlink(outside, root.join("escape")).expect("symlink");
        // `cd escape` resolves through the link to an outside dir.
        let err = contain_shell_command(&root, &root, "cd", &["escape".into()]).unwrap_err();
        assert!(err.to_string().contains("escape"), "must name the link: {err}");
        // A mutation verb touching a path through the link is blocked.
        assert!(contain_shell_command(&root, &root, "rm", &["escape/file".into()]).is_err());
        // Read-only reads through the link stay allowed (Observe-tier).
        contain_shell_command(&root, &root, "ls", &["escape".into()])
            .expect("ls through link allowed");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_new_file_outside_blocked() {
        let (root, dir) = temp_root();
        let outside = dir.path().parent().expect("temp parent");
        std::os::unix::fs::symlink(outside, root.join("escape")).expect("symlink");
        // The target does not exist yet; a redirect through the link must not
        // land outside the root.
        let err =
            contain_shell_command(&root, &root, "cat", &[">".into(), "escape/new.txt".into()])
                .unwrap_err();
        assert!(err.to_string().contains("symlink"), "link-like component must be rejected: {err}");
    }

    // -----------------------------------------------------------------------
    // Tool-level execution integration.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_read_only_verb_outside_allowed() {
        let (root, _dir) = temp_root();
        let tool = ShellTool::allow_all_direct();
        let args: Vec<String> = vec!["/etc/os-release".into()];
        let input = json!({ "command": "cat", "args": args });
        let result =
            tool.execute(input, &AllowAllPolicy, &session(&root), CancellationToken::new()).await;
        assert!(result.is_ok(), "cat /etc/os-release must be allowed: {:?}", result.err());
    }

    #[tokio::test]
    async fn execute_redirect_write_outside_blocked() {
        let (root, _dir) = temp_root();
        let tool = ShellTool::allow_all_direct();
        let args: Vec<String> = vec![">".into(), "/outside/x".into()];
        let input = json!({ "command": "cat", "args": args });
        let result =
            tool.execute(input, &AllowAllPolicy, &session(&root), CancellationToken::new()).await;
        match result {
            Err(ToolError::VirtualFsConflict { .. }) => {}
            other => panic!("expected VirtualFsConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_git_change_dir_read_allowed_mutate_blocked() {
        let (root, _dir) = temp_root();
        let tool = ShellTool::allow_all_direct();

        let read_input = json!({ "command": "git", "args": ["-C", "/tmp", "status"] });
        let result = tool
            .execute(read_input, &AllowAllPolicy, &session(&root), CancellationToken::new())
            .await;
        // Containment allows the read; git itself may fail at runtime, but a
        // non-zero exit still surfaces as an Ok ToolOutput — so any Err here
        // would mean containment blocked it.
        assert!(result.is_ok(), "git -C /tmp status must pass containment: {:?}", result.err());

        let mutate_input = json!({ "command": "git", "args": ["-C", "/tmp", "commit", "-m", "x"] });
        let result = tool
            .execute(mutate_input, &AllowAllPolicy, &session(&root), CancellationToken::new())
            .await;
        match result {
            Err(ToolError::VirtualFsConflict { .. }) => {}
            other => panic!("expected VirtualFsConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_in_root_operations_all_pass() {
        let (root, dir) = temp_root();
        std::fs::write(root.join("notes.txt"), "hello").expect("write fixture");
        let tool = ShellTool::allow_all_direct();

        let echo = tool
            .execute(
                json!({ "command": "echo", "args": ["hello", "world"] }),
                &AllowAllPolicy,
                &session(&root),
                CancellationToken::new(),
            )
            .await;
        assert!(echo.is_ok(), "echo must pass: {:?}", echo.err());

        let cat = tool
            .execute(
                json!({ "command": "cat", "args": ["notes.txt"] }),
                &AllowAllPolicy,
                &session(&root),
                CancellationToken::new(),
            )
            .await;
        assert!(cat.is_ok(), "cat notes.txt must pass: {:?}", cat.err());

        // cwd confined to root; an in-root subdir cwd works.
        let sub = root.join("sub");
        std::fs::create_dir(&sub).expect("create sub");
        let pwd = tool
            .execute(
                json!({ "command": "pwd", "cwd": "sub" }),
                &AllowAllPolicy,
                &session(&root),
                CancellationToken::new(),
            )
            .await;
        assert!(pwd.is_ok(), "pwd in sub must pass: {:?}", pwd.err());
        let _ = dir;
    }

    #[tokio::test]
    async fn execute_cd_escape_blocked() {
        let (root, _dir) = temp_root();
        let tool = ShellTool::allow_all_direct();
        let input = json!({ "command": "cd", "args": ["/tmp"] });
        let result =
            tool.execute(input, &AllowAllPolicy, &session(&root), CancellationToken::new()).await;
        match result {
            Err(ToolError::VirtualFsConflict { .. }) => {}
            other => panic!("expected VirtualFsConflict, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Empty project root disables containment.
    // -----------------------------------------------------------------------

    #[test]
    fn empty_root_disables_containment() {
        let empty = Utf8Path::new("");
        contain_shell_command(empty, empty, "cat", &["/etc/passwd".into()])
            .expect("empty root: containment off");
        contain_shell_command(empty, empty, "cd", &["/tmp".into()])
            .expect("empty root: containment off");
    }
}
