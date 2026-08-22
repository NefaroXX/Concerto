//! ADR-55: intent tiers and the authorization seam for the policy gate.
//!
//! This is batch 1c of the three-generation gate (ADR-55 §2). It reworks the
//! batch-1a seam into a *verdict source* the policy engine maps mechanically:
//!
//! - [`IntentTier`] — the three capability tiers (Observe / MutateLocal /
//!   Consequential) and the pure, deterministic [`classify_tier`] classifier.
//! - [`IntentVerdict`] — the full policy outcome for one action (`Allow` /
//!   `RequireApproval` / `Deny`, each carrying the `rule_matched` name that
//!   flows into the audit row).
//! - [`IntentAuthorization`] — the trait-object source of run/grant state the
//!   engine consults. Its default [`IntentAuthorization::verdict`] derives the
//!   policy outcome from the pure classification plus two state hooks; engines
//!   that attach no provider (the default) behave exactly as before ADR-55.
//!
//! The policy engine's gate (`Condition::IntentAuthorized`) lives in
//! `crate::policy`; it consults the authorization trait object defined here
//! but stays deterministic itself.

use crate::types::PolicyAction;
use std::borrow::Cow;

/// The three capability tiers an action falls into (ADR-55 §2).
///
/// `Observe` needs no authorization, `MutateLocal` is authorizable *in scope*
/// only, and `Consequential` sits outside what any blanket grant can cover —
/// it always prompts. The tier is a *classification*: the classifier never
/// grants. Grants come exclusively from the user via the authorization state
/// ([`IntentAuthorization`]) that later batches wire up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntentTier {
    /// Read-only: reads, search, inspection, diagnostics, planning. No
    /// authorization needed by the gate.
    Observe,
    /// Local, undoable mutations (file edits, local git ops). Authorizable
    /// within scope by a confirmed user decision.
    MutateLocal,
    /// Actions blanket authorization must never cover: network egress,
    /// destructive/reverting operations, secrets access, install/publish,
    /// force-flags, and shell scope escapes. Always prompts.
    Consequential,
}

/// Outcome of consulting an [`IntentAuthorization`] for one action.
///
/// The gate provider expresses the *full* policy outcome; the engine maps each
/// variant mechanically and the carried `rule` becomes the audit row's
/// `rule_matched`:
///
/// - [`IntentVerdict::Allow`] — the action runs without approval (`rule =
///   "observe"` for read-only actions, `rule = "intent_authorized"` for
///   in-scope grantable mutations).
/// - [`IntentVerdict::RequireApproval`] — the action goes through the matched
///   rule's normal approval path (`rule = "consequential"`, `"un_granted"`,
///   `"shell_requires_approval"`).
/// - [`IntentVerdict::Deny`] — a final pre-sink denial that is never surfaced
///   to the approval sink, so even session auto-approve cannot bypass it
///   (`rule = "intent_readonly_deny"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntentVerdict {
    /// Allow the action outright; `rule` names the matched rule.
    Allow { rule: &'static str },
    /// Ask for a human decision through the normal approval path; `rule`
    /// names the matched rule.
    RequireApproval { rule: &'static str },
    /// Final denial, audited and never surfaced to the approval sink; `rule`
    /// names the deny rule.
    Deny { rule: &'static str },
}

/// Audit `rule_matched` value for Observe-tier actions auto-allowed through
/// the gate (ADR-55 §2).
pub const RULE_OBSERVE: &str = "observe";

/// Audit `rule_matched` value when the intent gate upgrades
/// `RequireApproval` → `Allow` for an in-scope grantable mutation (ADR-55 §2).
pub const RULE_INTENT_AUTHORIZED: &str = "intent_authorized";

/// Audit `rule_matched` value when the intent gate keeps a Consequential-tier
/// action under `RequireApproval` — blanket grants never cover it.
pub const RULE_CONSEQUENTIAL: &str = "consequential";

/// Audit `rule_matched` value when a shell mutation requires approval:
/// shell MutateLocal is never grantable (ADR-55 §2 shell scope hole).
pub const RULE_SHELL_REQUIRES_APPROVAL: &str = "shell_requires_approval";

/// Audit `rule_matched` value for the hard read-only-intent denial: any
/// mutation (filesystem, shell, or git) in a run that was started without
/// mutation intent is denied outright, never surfaced to the approval sink —
/// even session auto-approve cannot approve it (B-1).
pub const RULE_INTENT_READONLY_DENY: &str = "intent_readonly_deny";

/// Audit `rule_matched` value for a grantable-class mutation outside any
/// active grant scope.
pub const RULE_UN_GRANTED: &str = "un_granted";

/// Session-scoped, non-durable source of intent-authorization state (ADR-55
/// §1/§4). Owned by the run loop, never persisted, re-confirmed on resume.
///
/// This is a *state source, not a decision maker*: the `SimplePolicyEngine`
/// stays deterministic. The default [`Self::verdict`] derives the full policy
/// outcome from the pure [`classify_tier`] classification plus the two state
/// hooks a run-loop implementation overrides ([`Self::is_read_only_intent`],
/// [`Self::grant_covers`]). An attached provider opts into the gate; an engine
/// with **no** provider attached skips the gate entirely and behaves exactly
/// as before ADR-55.
pub trait IntentAuthorization: Send + Sync {
    /// True when the current run is a read-only-intent run.
    ///
    /// A read-only run has no grant by definition: any mutation — filesystem,
    /// shell, or git — is denied outright ([`RULE_INTENT_READONLY_DENY`])
    /// rather than prompted, so even session auto-approve cannot bypass the
    /// read-only guarantee. Defaults to `false` (a normal, mutation-capable
    /// run).
    fn is_read_only_intent(&self) -> bool {
        false
    }

    /// True when an active grant covers `action`'s scope.
    ///
    /// Grants are session-scoped, non-durable, and bound to the run's
    /// (objective, revision) scope (ADR-55 §4). Defaults to `false` (no
    /// grant).
    fn grant_covers(&self, _action: &PolicyAction<'_>) -> bool {
        false
    }

    /// Return the full authorization outcome for `action`.
    ///
    /// The default derives the outcome from [`classify_tier`] plus the state
    /// hooks, so a run-loop provider only needs to override those; a provider
    /// MAY override this method entirely to express custom policy outcomes.
    ///
    /// Per action class (ADR-55 §2):
    /// - Observe → [`IntentVerdict::Allow`] (`rule = "observe"`).
    /// - Consequential → [`IntentVerdict::RequireApproval`]
    ///   (`rule = "consequential"`) — blanket grants never cover these.
    /// - MutateLocal → in a read-only-intent run ANY mutation (filesystem,
    ///   shell, or git) → [`IntentVerdict::Deny`]
    ///   (`rule = "intent_readonly_deny"`), never surfaced to the approval
    ///   sink (B-1); otherwise an in-scope grant on a grantable class
    ///   (filesystem write/edit tools and git local-mutate tools) →
    ///   [`IntentVerdict::Allow`] (`rule = "intent_authorized"`); a grantable
    ///   class without a grant → [`IntentVerdict::RequireApproval`]
    ///   (`rule = "un_granted"`); and shell mutations (never grantable,
    ///   ADR-55 §2 shell scope hole) → [`IntentVerdict::RequireApproval`]
    ///   (`rule = "shell_requires_approval"`).
    fn verdict(&self, action: &PolicyAction<'_>) -> IntentVerdict {
        let tier = classify_tier(action);
        match tier {
            IntentTier::Observe => IntentVerdict::Allow { rule: RULE_OBSERVE },
            IntentTier::Consequential => {
                IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL }
            }
            IntentTier::MutateLocal => {
                if self.is_read_only_intent() {
                    // Hard read-only enforcement (B-1): ANY mutation in a
                    // read-only-intent run is denied even when session
                    // auto-approve is active, and is never shown to the
                    // approval sink — shell MutateLocal (touch/mv/cp/mkdir)
                    // and grantable filesystem/git mutations alike. The user
                    // can stop and rephrase the request — changing the intent
                    // is a top-level flow (a new run), not a mid-run prompt.
                    IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY }
                } else if action.tool_name == "shell" {
                    // Shell MutateLocal is never grantable (ADR-55 §2 shell
                    // scope hole): the command stays under approval.
                    IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
                } else if is_grantable_class(action) {
                    if self.grant_covers(action) {
                        IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED }
                    } else {
                        IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED }
                    }
                } else {
                    IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED }
                }
            }
        }
    }
}

/// Classify an action into an [`IntentTier`] (ADR-55 §2).
///
/// Deterministic and pure: no state, no I/O, no model. Consequential is
/// evaluated first because it is the closed allowlist that blanket
/// authorization must never cover — a matching signature wins over a
/// read-looking verb (e.g. `git push --force` is never Observe). Anything not
/// matched by the read-only allowlist falls to `MutateLocal` (conservative:
/// an unknown action is authorizable, never auto-observe).
///
/// The allowlists below are **v1-extensible**: kept deliberately small and
/// reviewed (ADR-55 §Consequences), they are the ceiling, not the floor.
pub fn classify_tier(action: &PolicyAction<'_>) -> IntentTier {
    if is_consequential(action) {
        IntentTier::Consequential
    } else if is_observe(action) {
        IntentTier::Observe
    } else {
        IntentTier::MutateLocal
    }
}

/// Filesystem operations that constitute destructive commits (v1 set).
const FS_DESTRUCTIVE_OPS: &[&str] = &["delete", "remove", "truncate"];

/// Git operations that are network egress or destructive/reverting (v1 set).
const GIT_CONSEQUENTIAL_OPS: &[&str] = &["push", "fetch", "pull", "clone", "clean", "reset", "gc"];

/// Git operations that are purely read-only inspection (v1 set).
const GIT_READ_OPS: &[&str] =
    &["status", "log", "diff", "show", "branch_list", "stash_list", "blame", "rev-parse"];

/// Shell verbs that are read-only on their own (v1 set).
///
/// Mirrors the containment module's `READ_ONLY_VERBS` table
/// (`crates/tools/src/containment.rs`) — the source of truth for which shell
/// verbs are safe diagnostics. Core mirrors the table because the dependency
/// direction (tools depends on core) forbids importing it, so keep the two in
/// sync. A leading path (`/usr/bin/ls`) is normalized to its basename before
/// matching. At the *policy* tier a write-redirect additionally demotes even
/// one of these verbs to a mutation (`echo x > f` writes `f`, B-1); that
/// deliberately diverges from the containment module's `is_read_only`, which
/// only governs the outside-root path-argument exemption — redirect targets
/// are independently contained there by `scan_redirects`, so execution stays
/// safe either way.
const SHELL_READ_VERBS: &[&str] = &[
    "cat", "grep", "ls", "head", "tail", "less", "file", "stat", "which", "echo", "uname",
    "printf", "type", "dirname", "basename", "wc", "sort", "uniq", "cut", "sed", "awk",
];

/// In-place-write verbs among [`SHELL_READ_VERBS`] (mirrors the containment
/// module's `INPLACE_WRITE_FLAGS`). Plain `sed`/`grep`/`awk` classify as
/// read-only only when none of their recognized write flags is present.
const INPLACE_WRITE_FLAGS: &[(&str, &[&str])] =
    &[("sed", &["-i", "--in-place"]), ("grep", &["-w"]), ("awk", &["-i", "--in-place", "-w"])];

/// Shell verbs that destroy or overwrite data (v1 set).
const SHELL_DESTRUCTIVE_VERBS: &[&str] =
    &["rm", "rmdir", "shred", "dd", "mkfs", "truncate", "unlink"];

/// Shell verbs that are themselves network-egress clients (v1 set).
const SHELL_NETWORK_VERBS: &[&str] =
    &["curl", "wget", "ssh", "scp", "rsync", "ftp", "sftp", "telnet", "nc", "ncat", "socat"];

/// Package managers whose `publish`/`install`/`add` subcommands mutate the
/// environment/network (v1 set).
const SHELL_PACKAGE_MANAGERS: &[&str] =
    &["npm", "pnpm", "yarn", "bun", "cargo", "pip", "pip3", "gem", "go", "uv"];

/// Git network egress subcommands reachable through the shell (v1 set).
const GIT_SHELL_NETWORK_WORDS: &[&str] = &["push", "fetch", "pull", "clone"];

/// Git subcommands reachable through the shell that are read-only inspection
/// (v1 set).
const GIT_SHELL_READ_WORDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "rev-parse",
    "ls-files",
    "describe",
    "stash",
    "list",
];

/// Long-form force flags that mark an explicit mutation signature (v1 set).
/// The bare `-f` short flag is deliberately excluded: it is ambiguous across
/// tools (`ls -f`, `find -f`) and would misclassify read-only uses.
const FORCE_FLAG_TOKENS: &[&str] = &["--force", "--forced"];

/// Write-redirect operators whose target the containment module confines
/// (mirrors `containment.rs`'s `WRITE_REDIRECT_OPERATORS`).
const WRITE_REDIRECT_OPERATORS: &[&str] = &[">", ">>", "2>", "2>>", "&>", "&>>", ">|"];

/// Secrets-adjacent file path markers: env files, credential/key stores
/// (ADR-55 §2 "secrets access"). v1 set.
const SECRETS_PATH_MARKERS: &[&str] = &[
    ".env",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".kdbx",
    ".jks",
    ".netrc",
    ".git-credentials",
    "credentials",
    "keyring",
    "gnupg",
    "secret",
    "secrets",
    "id_rsa",
    "id_ed25519",
    "id_dsa",
    "id_ecdsa",
    "passwords",
    "passwd",
];

fn is_consequential(action: &PolicyAction<'_>) -> bool {
    match action.tool_name {
        "filesystem" => fs_is_consequential(action),
        "git" => git_is_consequential(action),
        "shell" => shell_is_consequential(action),
        // HTTP/fetch/curl tools are network egress by construction.
        "http" | "fetch" | "curl" => true,
        _ => false,
    }
}

fn is_observe(action: &PolicyAction<'_>) -> bool {
    match action.tool_name {
        "filesystem" => fs_is_observe(action),
        "git" => git_is_observe(action),
        "shell" => shell_is_observe(action),
        // Read-only inspector tool names.
        "search" | "read" | "inspect" | "diagnose" | "diagnostics" => true,
        _ => false,
    }
}

fn fs_is_consequential(action: &PolicyAction<'_>) -> bool {
    if matches!(
        input_operation(action),
        Some(op) if FS_DESTRUCTIVE_OPS.contains(&op)
    ) {
        return true;
    }
    path_has_secrets_marker(input_paths(action))
}

fn fs_is_observe(action: &PolicyAction<'_>) -> bool {
    matches!(
        input_operation(action),
        Some("read" | "list" | "exists" | "search" | "inspect" | "stat")
    )
}

fn git_is_consequential(action: &PolicyAction<'_>) -> bool {
    matches!(
        input_operation(action),
        Some(op) if GIT_CONSEQUENTIAL_OPS.contains(&op)
    )
}

fn git_is_observe(action: &PolicyAction<'_>) -> bool {
    matches!(
        input_operation(action),
        Some(op) if GIT_READ_OPS.contains(&op)
    )
}

fn shell_is_consequential(action: &PolicyAction<'_>) -> bool {
    // Structured facts that flag egress are conclusive: the producing tool has
    // already decided this command reaches the network.
    if action.command_facts.as_ref().is_some_and(|facts| facts.network_requested) {
        return true;
    }
    let Some(text) = shell_command_text(action) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let Some(first) = tokens.first().copied() else {
        return false;
    };
    let verb = verb_basename(first);

    // Destructive fs verbs.
    if SHELL_DESTRUCTIVE_VERBS.contains(&verb) {
        return true;
    }
    // Network-egress clients.
    if SHELL_NETWORK_VERBS.contains(&verb) {
        return true;
    }
    // Package publish/install/add.
    if SHELL_PACKAGE_MANAGERS.contains(&verb)
        && tokens.iter().any(|t| matches!(*t, "publish" | "install" | "add"))
    {
        return true;
    }
    // Git network/destructive verbs — token-based so `-C`/flag style layouts
    // still match.
    if tokens.contains(&"git") {
        if tokens.iter().any(|token| GIT_SHELL_NETWORK_WORDS.contains(token)) {
            return true;
        }
        if tokens.contains(&"clean") || (tokens.contains(&"reset") && tokens.contains(&"--hard")) {
            return true;
        }
    }
    // Long-form force flags mark an explicit mutation signature.
    if tokens.iter().any(|token| FORCE_FLAG_TOKENS.contains(token)) {
        return true;
    }
    // Secrets-adjacent file access (e.g. `cat .env`).
    if SECRETS_PATH_MARKERS.iter().any(|marker| lower.contains(*marker)) {
        return true;
    }
    // Write-redirect or cd/pushd targets that escape the session project root
    // are scope escapes: never grantable. In-root relative redirects are
    // demoted to MutateLocal by `is_read_only_verb_invocation` (any redirect
    // is a write, B-1), so only escape checks live here.
    if has_escaping_redirect(&tokens) || has_escaping_cd(&tokens) {
        return true;
    }
    false
}

fn shell_is_observe(action: &PolicyAction<'_>) -> bool {
    let Some(text) = shell_command_text(action) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    // Git-wide read verbs via the shell are read-only (ADR-55 §2).
    if tokens.contains(&"git") && tokens.iter().any(|token| GIT_SHELL_READ_WORDS.contains(token)) {
        return true;
    }
    let Some(verb) = tokens.first().copied().map(verb_basename) else {
        return false;
    };
    is_read_only_verb_invocation(verb, &tokens[1..])
}

/// True when `verb` is a read-only shell verb in this invocation: a plain
/// [`SHELL_READ_VERBS`] member, or an in-place verb whose recognized write
/// flags are all absent. A write-redirect anywhere in the trailing tokens
/// makes even a read-only verb a mutation — `echo x > f` writes `f` (B-1).
/// Escaped redirects and `cd` climbs were already classified Consequential, so
/// a read-only verb with only in-root arguments and no redirect is genuinely
/// read-only.
fn is_read_only_verb_invocation(verb: &str, trailing: &[&str]) -> bool {
    if !SHELL_READ_VERBS.contains(&verb) {
        return false;
    }
    if has_write_redirect(trailing) {
        return false;
    }
    let Some((_, write_flags)) = INPLACE_WRITE_FLAGS.iter().find(|(v, _)| *v == verb) else {
        return true;
    };
    !trailing.iter().any(|arg| {
        write_flags.iter().any(|flag| *arg == *flag || (flag.len() > 1 && arg.starts_with(flag)))
    })
}

/// True when `tokens` contains a write-redirect: an exact
/// [`WRITE_REDIRECT_OPERATORS`] member (`>`, `>>`, `2>`, ...) or a glued form
/// (`2>/tmp/out`, `>out`). Mirrors the operator scan in
/// [`has_escaping_redirect`] minus the escape check: any redirect target means
/// the invocation writes.
fn has_write_redirect(tokens: &[&str]) -> bool {
    tokens.iter().any(|token| {
        WRITE_REDIRECT_OPERATORS.contains(token)
            || WRITE_REDIRECT_OPERATORS
                .iter()
                .any(|op| token.strip_prefix(*op).is_some_and(|rest| !rest.is_empty()))
    })
}

/// True when the command contains a write-redirect (or glued operator form)
/// whose target escapes the workspace.
fn has_escaping_redirect(tokens: &[&str]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        if WRITE_REDIRECT_OPERATORS.contains(token) {
            // Whitespace-separated operator: the next token is the target.
            return tokens.get(i + 1).is_some_and(|target| is_escaping_path(target));
        }
        // Glued forms ("2>/tmp/x", ">/outside"): peel the operator and classify
        // the remainder as the target.
        WRITE_REDIRECT_OPERATORS.iter().any(|op| {
            token.strip_prefix(*op).is_some_and(|rest| !rest.is_empty() && is_escaping_path(rest))
        })
    })
}

/// True when the command contains a `cd`/`pushd` whose target escapes the
/// workspace — the containment module rejects exactly this.
fn has_escaping_cd(tokens: &[&str]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        matches!(*token, "cd" | "pushd")
            && tokens.get(i + 1).is_some_and(|target| is_escaping_path(target))
    })
}

/// True when `target` resolves outside the session project root: absolute,
/// home-relative, or a parent climb. Mirrors the containment module's boundary
/// without needing the root value — any such target is outside by construction
/// for a scoped run.
fn is_escaping_path(target: &str) -> bool {
    target.starts_with('/')
        || target.starts_with('~')
        || target == ".."
        || target.starts_with("../")
}

/// Whether `action` belongs to a grantable mutation class: filesystem
/// write/edit tools and git local-mutate tools. Shell MutateLocal is NEVER
/// grantable (ADR-55 §2 shell scope hole); other tools are not grantable
/// either. Only meaningful within the MutateLocal tier arm, where the tier is
/// already established.
fn is_grantable_class(action: &PolicyAction<'_>) -> bool {
    matches!(action.tool_name, "filesystem" | "git")
}

/// Read the tool input's `operation` field, if any.
fn input_operation<'a>(action: &'a PolicyAction<'a>) -> Option<&'a str> {
    action.input.get("operation").and_then(serde_json::Value::as_str)
}

/// Candidate file paths mentioned in a filesystem-style action input.
fn input_paths<'a>(action: &'a PolicyAction<'a>) -> impl Iterator<Item = &'a str> {
    ["path", "file_path", "destination"]
        .into_iter()
        .filter_map(|key| action.input.get(key).and_then(serde_json::Value::as_str))
}

/// True when any candidate path carries a secrets-adjacent marker
/// (case-insensitive).
fn path_has_secrets_marker<'a>(mut paths: impl Iterator<Item = &'a str>) -> bool {
    paths.any(|path| {
        let lower = path.to_ascii_lowercase();
        SECRETS_PATH_MARKERS.iter().any(|marker| lower.contains(*marker))
    })
}

/// Reconstruct the command text the shell tool runs: the clean `command` +
/// `args` from the raw input when present, else the structured `argv` facts.
///
/// Classification reasons on *what actually runs*, so both representations are
/// acceptable; the raw input is preferred because a shell-wrapped argv
/// (`/bin/bash -c <cmd>`) buries the real command in the last element.
fn shell_command_text<'a>(action: &'a PolicyAction<'a>) -> Option<Cow<'a, str>> {
    if let Some(text) = input_command_text(action.input) {
        return Some(text);
    }
    action.command_facts.as_ref().map(|facts| Cow::Owned(facts.argv.join(" ")))
}

/// Build `"{command} {args...}"` from a shell input's `command`/`args` fields.
fn input_command_text(input: &serde_json::Value) -> Option<Cow<'_, str>> {
    let command = input.get("command")?.as_str()?;
    let args = input.get("args").and_then(serde_json::Value::as_array);
    if let Some(args) = args {
        let args: Vec<&str> = args.iter().filter_map(serde_json::Value::as_str).collect();
        if !args.is_empty() {
            return Some(Cow::Owned(format!("{command} {}", args.join(" "))));
        }
    }
    Some(Cow::Borrowed(command))
}

/// Normalize a command token to its basename so `/usr/bin/ls` matches `ls`.
fn verb_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Ulid;
    use crate::types::{
        CapabilitySet, CommandPolicyFacts, DestructiveClass, FilesystemScope, SandboxProfile,
    };
    use std::path::PathBuf;

    fn action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
        PolicyAction {
            tool_name,
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        }
    }

    fn tier(tool_name: &str, input: serde_json::Value) -> IntentTier {
        let action = action(tool_name, &input);
        classify_tier(&action)
    }

    /// Test provider with configurable run/grant state that exercises the
    /// trait's default verdict computation.
    #[derive(Clone, Copy)]
    struct StatefulAuth {
        read_only_intent: bool,
        grant_active: bool,
        granted_tool: &'static str,
    }

    impl StatefulAuth {
        fn idle() -> Self {
            Self { read_only_intent: false, grant_active: false, granted_tool: "" }
        }

        fn granted(tool: &'static str) -> Self {
            Self { read_only_intent: false, grant_active: true, granted_tool: tool }
        }

        fn read_only() -> Self {
            Self { read_only_intent: true, grant_active: false, granted_tool: "" }
        }
    }

    impl IntentAuthorization for StatefulAuth {
        fn is_read_only_intent(&self) -> bool {
            self.read_only_intent
        }

        fn grant_covers(&self, action: &PolicyAction<'_>) -> bool {
            self.grant_active && action.tool_name == self.granted_tool
        }
    }

    #[test]
    fn default_provider_expresses_gate_outcomes_from_classification() {
        // A provider that overrides no state hooks opts into the gate and
        // derives outcomes purely from the classification: a read is
        // Allow("observe"), a shell mutation is never grantable, an
        // ungranted fs write requires approval.
        let auth = StatefulAuth::idle();
        let read_input = serde_json::json!({"operation": "read", "path": "src/main.rs"});
        let read = action("filesystem", &read_input);
        assert_eq!(auth.verdict(&read), IntentVerdict::Allow { rule: RULE_OBSERVE });
        let shell_input = serde_json::json!({"command": "touch", "args": ["x"]});
        let shell = action("shell", &shell_input);
        assert_eq!(
            auth.verdict(&shell),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
        let write_input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let write = action("filesystem", &write_input);
        assert_eq!(auth.verdict(&write), IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED });
    }

    #[test]
    fn in_scope_grantable_mutation_allows_intent_authorized() {
        let auth = StatefulAuth::granted("filesystem");
        let write_input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let write = action("filesystem", &write_input);
        assert_eq!(auth.verdict(&write), IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED });
        // A git local mutation is equally grantable in scope.
        let auth = StatefulAuth::granted("git");
        let commit_input = serde_json::json!({"operation": "commit"});
        let commit = action("git", &commit_input);
        assert_eq!(auth.verdict(&commit), IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED });
        // A grant for the wrong tool does not cover the action.
        let auth = StatefulAuth::granted("git");
        let write = action("filesystem", &write_input);
        assert_eq!(auth.verdict(&write), IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED });
    }

    #[test]
    fn shell_mutation_is_never_grantable() {
        // Even an active shell grant cannot upgrade a shell MutateLocal
        // (ADR-55 §2 shell scope hole).
        let auth = StatefulAuth::granted("shell");
        let input = serde_json::json!({"command": "touch", "args": ["src/main.rs"]});
        assert_eq!(
            auth.verdict(&action("shell", &input)),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    #[test]
    fn consequential_is_never_grantable_even_with_grant() {
        let auth = StatefulAuth::granted("shell");
        let input = serde_json::json!({"command": "curl", "args": ["https://example.com"]});
        assert_eq!(
            auth.verdict(&action("shell", &input)),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL }
        );
    }

    #[test]
    fn read_only_intent_denies_all_mutation_classes() {
        let write_input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let read_input = serde_json::json!({"operation": "read", "path": "src/main.rs"});
        let shell_input = serde_json::json!({"command": "touch", "args": ["x"]});
        let git_input = serde_json::json!({"operation": "commit"});
        let auth = StatefulAuth::read_only();
        let write = action("filesystem", &write_input);
        assert_eq!(auth.verdict(&write), IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY });
        // Reads stay allowed in a read-only run.
        let read = action("filesystem", &read_input);
        assert_eq!(auth.verdict(&read), IntentVerdict::Allow { rule: RULE_OBSERVE });
        // Shell MutateLocal (touch/mv/cp/mkdir) is hard-denied too — session
        // auto-approve must never approve it (B-1a).
        let shell = action("shell", &shell_input);
        assert_eq!(auth.verdict(&shell), IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY });
        // Git local mutations are equally hard-denied in a read-only run.
        let git = action("git", &git_input);
        assert_eq!(auth.verdict(&git), IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY });
    }

    #[test]
    fn read_only_intent_denies_shell_redirect_writes() {
        // `echo x > src/main.rs` is now a MutateLocal (any redirect is a write,
        // B-1b), so in a read-only run it is hard-denied — never Allow, never
        // RequireApproval.
        let redirect = serde_json::json!({"command": "echo", "args": ["x", ">", "src/main.rs"]});
        let auth = StatefulAuth::read_only();
        assert_eq!(
            auth.verdict(&action("shell", &redirect)),
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY }
        );
        // `cat a > b` and `sed 's/a/b/' f > g` classify as mutations.
        assert_eq!(
            classify_tier(&action(
                "shell",
                &serde_json::json!({"command": "cat", "args": ["a", ">", "b"]})
            )),
            IntentTier::MutateLocal,
            "cat with a redirect writes b"
        );
        assert_eq!(
            classify_tier(&action(
                "shell",
                &serde_json::json!({"command": "sed", "args": ["s/a/b/", "f", ">", "g"]})
            )),
            IntentTier::MutateLocal,
            "sed with a redirect writes g"
        );
        // Truly read-only invocations of the same verbs stay Observe.
        assert_eq!(
            classify_tier(&action("shell", &serde_json::json!({"command": "cat", "args": ["a"]}))),
            IntentTier::Observe
        );
        assert_eq!(
            classify_tier(&action(
                "shell",
                &serde_json::json!({"command": "sed", "args": ["s/a/b/", "f"]})
            )),
            IntentTier::Observe
        );
    }

    #[test]
    fn mutation_intent_redirect_write_requires_approval_never_grantable() {
        // In a mutation-capable run the same redirect still requires approval:
        // shell MutateLocal is never grantable (ADR-55 §2 shell scope hole).
        let redirect = serde_json::json!({"command": "echo", "args": ["x", ">", "src/main.rs"]});
        let auth = StatefulAuth::granted("shell");
        assert_eq!(
            auth.verdict(&action("shell", &redirect)),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    // ---- Observe tier ------------------------------------------------------

    #[test]
    fn filesystem_read_verbs_are_observe() {
        for op in ["read", "list", "exists", "search", "inspect", "stat"] {
            assert_eq!(
                tier("filesystem", serde_json::json!({"operation": op, "path": "src/main.rs"})),
                IntentTier::Observe,
                "filesystem {op} should be Observe"
            );
        }
    }

    #[test]
    fn git_read_verbs_are_observe() {
        for op in ["status", "log", "diff", "show", "branch_list", "stash_list"] {
            assert_eq!(
                tier("git", serde_json::json!({"operation": op})),
                IntentTier::Observe,
                "git {op} should be Observe"
            );
        }
    }

    #[test]
    fn shell_read_verbs_are_observe() {
        for verb in ["ls", "cat", "grep", "head", "tail", "file", "stat", "which", "echo"] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": verb, "args": ["-la", "src"]})),
                IntentTier::Observe,
                "shell {verb} should be Observe"
            );
        }
        // A bare verb with no args is still Observe.
        assert_eq!(
            tier("shell", serde_json::json!({"command": "pwd"})),
            IntentTier::MutateLocal,
            "pwd is not in the v1 read-verb set"
        );
    }

    #[test]
    fn shell_git_read_verbs_are_observe() {
        for args in
            [vec!["status"], vec!["log", "--oneline"], vec!["diff", "HEAD"], vec!["branch", "-a"]]
        {
            assert_eq!(
                tier("shell", serde_json::json!({"command": "git", "args": args})),
                IntentTier::Observe,
                "git {args:?} via shell should be Observe"
            );
        }
    }

    #[test]
    fn read_only_inspector_tool_names_are_observe() {
        for tool in ["search", "read", "inspect", "diagnose", "diagnostics"] {
            assert_eq!(tier(tool, serde_json::json!({})), IntentTier::Observe);
        }
    }

    #[test]
    fn in_place_write_flags_demote_read_only_verbs() {
        // sed/grep/awk are read-only only while none of their recognized write
        // flags is present (mirrors the containment module's is_read_only).
        assert_eq!(
            tier("shell", serde_json::json!({"command": "sed", "args": ["-i", "s/x/y/g", "f"]})),
            IntentTier::MutateLocal,
            "sed -i writes in place"
        );
        assert_eq!(
            tier("shell", serde_json::json!({"command": "grep", "args": ["-w", "foo", "f"]})),
            IntentTier::MutateLocal,
            "grep -w is conservatively a write marker"
        );
        // Plain read invocations of the same verbs stay Observe.
        assert_eq!(
            tier("shell", serde_json::json!({"command": "sed", "args": ["s/x/y/g", "f"]})),
            IntentTier::Observe
        );
        assert_eq!(
            tier("shell", serde_json::json!({"command": "grep", "args": ["-i", "foo", "f"]})),
            IntentTier::Observe
        );
    }

    // ---- Consequential tier ------------------------------------------------

    #[test]
    fn git_consequential_ops_via_tool() {
        for op in ["push", "fetch", "pull", "clean", "reset"] {
            assert_eq!(
                tier("git", serde_json::json!({"operation": op})),
                IntentTier::Consequential,
                "git {op} should be Consequential"
            );
        }
    }

    #[test]
    fn destructive_filesystem_verbs_are_consequential() {
        for op in ["delete", "remove", "truncate"] {
            assert_eq!(
                tier("filesystem", serde_json::json!({"operation": op, "path": "src/main.rs"})),
                IntentTier::Consequential,
                "filesystem {op} should be Consequential"
            );
        }
    }

    #[test]
    fn destructive_shell_verbs_are_consequential() {
        for (command, args) in [
            ("rm", vec!["-rf", "target"]),
            ("rm", vec!["file.txt"]),
            ("rmdir", vec!["empty_dir"]),
            ("shred", vec!["-u", "file"]),
            ("dd", vec!["if=/dev/zero", "of=/dev/sda"]),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": command, "args": args})),
                IntentTier::Consequential,
                "{command} {args:?} should be Consequential"
            );
        }
    }

    #[test]
    fn network_and_install_shell_verbs_are_consequential() {
        for (command, args) in [
            ("curl", vec!["-o", "/tmp/x", "https://example.com/x"]),
            ("wget", vec!["https://example.com/x"]),
            ("ssh", vec!["user@host", "ls"]),
            ("cargo", vec!["publish"]),
            ("cargo", vec!["install", "some-crate"]),
            ("npm", vec!["publish"]),
            ("npm", vec!["install", "dep"]),
            ("pip", vec!["install", "pkg"]),
            ("git", vec!["push", "origin", "main"]),
            ("git", vec!["push", "--force"]),
            ("git", vec!["fetch", "origin"]),
            ("git", vec!["pull"]),
            ("git", vec!["clone", "https://github.com/x/y"]),
            ("git", vec!["clean", "-fd"]),
            ("git", vec!["reset", "--hard", "HEAD"]),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": command, "args": args})),
                IntentTier::Consequential,
                "{command} {args:?} should be Consequential"
            );
        }
    }

    #[test]
    fn force_flags_are_consequential() {
        assert_eq!(
            tier("shell", serde_json::json!({"command": "cargo", "args": ["install", "--force"]})),
            IntentTier::Consequential
        );
    }

    #[test]
    fn http_tools_are_consequential() {
        for tool in ["http", "fetch", "curl"] {
            assert_eq!(tier(tool, serde_json::json!({})), IntentTier::Consequential);
        }
    }

    #[test]
    fn secrets_adjacent_paths_are_consequential() {
        for path in [".env", "config/.env.local", "credentials.json", "keys/id_rsa.pub"] {
            assert_eq!(
                tier("filesystem", serde_json::json!({"operation": "write", "path": path})),
                IntentTier::Consequential,
                "write to {path} should be Consequential"
            );
        }
        // Reading a secret file is equally consequential (never auto-covered).
        assert_eq!(
            tier("shell", serde_json::json!({"command": "cat", "args": [".env"]})),
            IntentTier::Consequential,
            "cat .env should be Consequential"
        );
    }

    #[test]
    fn escaped_redirect_demotes_read_only_verb_to_consequential() {
        for (args, msg) in [
            (vec!["x", ">", "/tmp/out"], "absolute redirect target"),
            (vec!["x", ">>", "../out"], "climbing redirect target"),
            (vec!["x", "2>", "/tmp/err"], "fd-2 redirect"),
            (vec!["x", ">", "~/out"], "home redirect target"),
            (vec!["x", "2>/tmp/out"], "glued fd redirect"),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": "echo", "args": args})),
                IntentTier::Consequential,
                "{msg}"
            );
        }
    }

    #[test]
    fn in_root_redirect_demotes_read_only_verb_to_mutate_local() {
        // Relative in-root redirects are writes: `echo x > out.txt` creates a
        // file, so the invocation is no longer Observe (B-1b). The containment
        // module is still the execution backstop for redirect scope.
        assert_eq!(
            tier("shell", serde_json::json!({"command": "echo", "args": ["x", ">", "out.txt"]})),
            IntentTier::MutateLocal
        );
        assert_eq!(
            tier(
                "shell",
                serde_json::json!({"command": "echo", "args": ["x", ">", "src/out.txt"]})
            ),
            IntentTier::MutateLocal
        );
        // A glued in-root redirect classifies the same way.
        assert_eq!(
            tier("shell", serde_json::json!({"command": "echo", "args": ["x", ">out.txt"]})),
            IntentTier::MutateLocal
        );
    }

    #[test]
    fn cd_scope_escape_is_consequential() {
        for (args, msg) in [
            (vec!["cd", "/outside", "&&", "ls"], "absolute cd"),
            (vec!["cd", "..", "&&", "ls"], "cd climb"),
            (vec!["pushd", "/etc"], "pushd absolute"),
            (vec!["sh", "-c", "cd /var && pwd"], "wrapped cd"),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": "bash", "args": args})),
                IntentTier::Consequential,
                "{msg}"
            );
        }
        // In-root relative cd stays MutateLocal.
        assert_eq!(
            tier(
                "shell",
                serde_json::json!({"command": "bash", "args": ["-c", "cd src && cargo build"]})
            ),
            IntentTier::MutateLocal
        );
    }

    #[test]
    fn network_requested_facts_are_consequential() {
        let input = serde_json::json!({"command": "script"});
        let facts = CommandPolicyFacts {
            shell_profile_id: None,
            resolved_executable: Some(PathBuf::from("/usr/bin/script")),
            argv: vec!["script".into(), "poke".into()],
            working_directory: None,
            network_requested: true,
            filesystem_scope: FilesystemScope::ProjectOnly,
            destructive_classification: DestructiveClass::NonDestructive,
        };
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: Some(SandboxProfile::None),
            estimated_cost_usd: None,
            command_facts: Some(facts),
        };
        assert_eq!(classify_tier(&action), IntentTier::Consequential);
    }

    // ---- MutateLocal tier --------------------------------------------------

    #[test]
    fn local_filesystem_mutations_are_mutate_local() {
        for op in ["write", "move", "copy"] {
            assert_eq!(
                tier("filesystem", serde_json::json!({"operation": op, "path": "src/main.rs"})),
                IntentTier::MutateLocal,
                "filesystem {op} should be MutateLocal"
            );
        }
    }

    #[test]
    fn local_git_mutations_are_mutate_local() {
        for op in [
            "add",
            "commit",
            "branch_create",
            "branch_switch",
            "restore",
            "stash_push",
            "stash_pop",
        ] {
            assert_eq!(
                tier("git", serde_json::json!({"operation": op})),
                IntentTier::MutateLocal,
                "git {op} should be MutateLocal"
            );
        }
    }

    #[test]
    fn local_shell_commands_are_mutate_local() {
        for (command, args) in [
            ("cargo", vec!["build"]),
            ("cargo", vec!["test"]),
            ("touch", vec!["src/main.rs"]),
            ("git", vec!["add", "src/main.rs"]),
            ("git", vec!["commit", "-m", "msg"]),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": command, "args": args})),
                IntentTier::MutateLocal,
                "{command} {args:?} should be MutateLocal"
            );
        }
    }

    #[test]
    fn unknown_tools_default_to_mutate_local() {
        assert_eq!(tier("provider", serde_json::json!({})), IntentTier::MutateLocal);
        assert_eq!(tier("mcp:some:read", serde_json::json!({})), IntentTier::MutateLocal);
    }

    #[test]
    fn basename_verbs_classify_same_as_plain() {
        let plain = tier("shell", serde_json::json!({"command": "ls", "args": ["-la"]}));
        let full = tier("shell", serde_json::json!({"command": "/usr/bin/ls", "args": ["-la"]}));
        assert_eq!(full, plain);
        assert_eq!(full, IntentTier::Observe);
    }

    // ---- ADR-55 batch 1c fixtures: classification + gate outcome -----------

    #[test]
    fn fixture_in_root_echo_redirect_is_mutate_local_and_never_grantable() {
        let input = serde_json::json!({"command": "echo", "args": ["x", ">", "notes.txt"]});
        let action = action("shell", &input);
        // Any write-redirect makes the read verb a mutation (B-1b): the v1
        // fixture that pinned this as Observe and gate-Allow is superseded.
        assert_eq!(classify_tier(&action), IntentTier::MutateLocal);
        // Shell MutateLocal is never grantable: even an active shell grant
        // keeps it under approval.
        let auth = StatefulAuth::granted("shell");
        assert_eq!(
            auth.verdict(&action),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    #[test]
    fn fixture_sudo_rm_is_mutate_local_and_never_grantable() {
        let input = serde_json::json!({"command": "sudo", "args": ["rm", "f"]});
        let action = action("shell", &input);
        assert_eq!(classify_tier(&action), IntentTier::MutateLocal);
        // A shell grant cannot upgrade a shell mutation — always prompts.
        let auth = StatefulAuth::granted("shell");
        assert_eq!(
            auth.verdict(&action),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    #[test]
    fn fixture_timeout_rm_is_mutate_local_and_never_grantable() {
        let input = serde_json::json!({"command": "timeout", "args": ["5", "rm", "f"]});
        let action = action("shell", &input);
        assert_eq!(classify_tier(&action), IntentTier::MutateLocal);
        let auth = StatefulAuth::granted("shell");
        assert_eq!(
            auth.verdict(&action),
            IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL }
        );
    }

    #[test]
    fn fixture_sh_c_wrapped_cd_escape_is_consequential_and_never_grantable() {
        let input = serde_json::json!({"command": "sh", "args": ["-c", "cd /outside && rm x"]});
        let action = action("shell", &input);
        assert_eq!(classify_tier(&action), IntentTier::Consequential);
        // Consequential is never covered by a grant: stays RequireApproval.
        let auth = StatefulAuth::granted("shell");
        assert_eq!(
            auth.verdict(&action),
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL }
        );
    }
}
