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

/// Audit `rule_matched` value when the intent gate upgrades
/// `RequireApproval` → `Allow` for a project-bounded `shell` command under
/// an Acting grant (ADR-55 shell scope amendment; F4 — security review 2026-
/// 09-09). Deliberately DISTINCT from [`RULE_INTENT_AUTHORIZED`]: a shell
/// auto-approval must be individually auditable, and filesystem upgrades
/// keep the original shared rule.
pub const RULE_INTENT_AUTHORIZED_SHELL: &str = "intent_authorized_shell";

/// Audit `rule_matched` value when the intent gate upgrades
/// `RequireApproval` → `Allow` for an ORCHESTRATION/delegation tool call
/// (`call_specialist`) under an Acting grant (ADR-55 scope amendment,
/// delegation coverage). Deliberately DISTINCT from [`RULE_INTENT_AUTHORIZED`]
/// (files/git) and [`RULE_INTENT_AUTHORIZED_SHELL`]: a delegation
/// auto-approval is its own forensic row — the audit must be able to
/// reconstruct *which* coordinator dispatched *which* specialist without
/// conflating it with a filesystem write or a shell command approval. The
/// upgrade carries no side effect: the dispatched specialist's own tool calls
/// are still individually policy+grant-gated, and the run's spend/task caps
/// still bound the fan-out.
pub const RULE_INTENT_AUTHORIZED_DELEGATION: &str = "intent_authorized_delegation";

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
    ///   (`rule = "un_granted"`); and shell mutations (never blanket-granted,
    ///   ADR-55 §2 shell scope hole — project-bounded shell commands are the
    ///   scoped upgrade exception, ADR-55 shell scope amendment) →
    ///   [`IntentVerdict::RequireApproval`]
    ///   (`rule = "shell_requires_approval"`).
    fn verdict(&self, action: &PolicyAction<'_>) -> IntentVerdict {
        self.default_gate_verdict(action)
    }

    /// The tier → verdict arms [`Self::verdict`] derives by default, exposed
    /// so composite providers can layer a scoped upgrade on top WITHOUT
    /// duplicating the arms (a scoped shell auto-approval composes over
    /// exactly this function, never replaces it). Callers that layer an
    /// upgrade MUST keep the upgrade below the hard invariants: it may only
    /// run after the Consequential/Observe tiers have their say, may only
    /// fire in a non-read-only run, and may only produce
    /// [`IntentVerdict::Allow`] — never touch a `Deny`.
    fn default_gate_verdict(&self, action: &PolicyAction<'_>) -> IntentVerdict {
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
                    // Shell MutateLocal has no blanket grant (ADR-55 §2 shell
                    // scope hole): the command stays under approval. The
                    // scoped project-bounded upgrade is layered by providers
                    // (see [`is_project_bounded_shell`]) on top of this
                    // default — never inside it.
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

// Security-review hardening of the project-bounded shell upgrade (2026-09-09,
// F1/F2/F3; recast 2026-09-10). These tables feed BOTH the Consequential
// classifier and (via the recast's positive allowlist, see
// [`SHELL_UPGRADE_ALLOWLIST`]) the [`is_project_bounded_shell`] upgrade
// predicate: a token at the tier level keeps the command under its existing
// approval path wherever it appears in the command text — not just as the
// first verb.

/// Env-indirection metacharacters (F1). ANY token carrying one means the
/// command text the classifier sees is NOT the text the shell will run:
/// `$HOME`, `${HOME}`, `$(…)` run outside the scanner's sight, `%USERPROFILE%`
/// is a second expansion dialect, and quotes hide argument content from the
/// whitespace split. Never eligible for auto-approval.
const SHELL_INTERPOLATION_CHARS: &[char] = &['$', '`', '%', '\'', '"'];

/// Interpreter verbs (F3, extended by the 2026-09-10 security recast): with
/// a code flag (`-c`, `-e`, `-m`, `-r`) they run attacker-chosen code no
/// token scan can see into (network calls, escapes assembled at runtime).
/// Such invocations are Consequential everywhere and never take the
/// project-bounded upgrade. Under the recast the upgrade predicate goes
/// further: these verbs NEVER upgrade in ANY form — including the plain
/// script-file form (`python pwn.py` executes model-authored code whose
/// network/write effects are invisible to every text scan). No exception.
const SHELL_INTERPRETERS: &[&str] =
    &["python", "python3", "node", "perl", "ruby", "php", "lua", "rscript", "powershell", "pwsh"];

/// Interpreter shells (upgrade predicate only, 2026-09-10): shell-family
/// interpreters (`sh pwn.sh`, `bash pwn.sh`). Kept OUT of
/// [`SHELL_INTERPRETERS`] so the tier classifier's code-flag rule stays
/// exactly as shipped (`bash -c "cd src && cargo build"` remains
/// MutateLocal); here they simply never upgrade.
const SHELL_INTERPRETER_INVOKERS: &[&str] = &["sh", "bash", "zsh", "dash", "ash", "fish"];

/// Interpreter code flags: any argument starting with one of these prefixes
/// counts (exact forms and glued forms like `-e'print(1)'` alike).
const INTERPRETER_CODE_FLAGS: &[&str] = &["-c", "-e", "-m", "-r"];

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
    // Interpreter invocations carrying a code flag (`python -c …`, `node -e …`,
    // `python -m http.server`) run attacker-chosen code no token scan can see
    // into — always Consequential so they can never take any auto-approval
    // (F3).
    if is_interpreter_code_invocation(&tokens) {
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
/// [`WRITE_REDIRECT_OPERATORS`] member (`>`, `>>`, `2>`, ...), a glued form
/// (`2>/tmp/out`, `>out`), or a `>` at any NON-INITIAL position of a token
/// (`pwned>~/.bashrc`, `a>>b`) — mirroring the execution-time containment
/// canon (`containment.rs scan_redirects`' `rfind('>')` scan). Mirrors the
/// operator scan in [`has_escaping_redirect`] minus the escape check: any
/// redirect target means the invocation writes.
fn has_write_redirect(tokens: &[&str]) -> bool {
    tokens.iter().any(|token| {
        WRITE_REDIRECT_OPERATORS.contains(token)
            || WRITE_REDIRECT_OPERATORS
                .iter()
                .any(|op| token.strip_prefix(*op).is_some_and(|rest| !rest.is_empty()))
            || token.rfind('>').is_some_and(|idx| idx > 0)
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
/// home-relative, ANY `..`-containing climb, or an interpolation metachar
/// (`$`/`%` env expansion is resolved by the SHELL against outside roots —
/// a target carrying `$HOME`/`${HOME}`/`%USERPROFILE%` escapes by
/// construction). Mirrors the containment module's boundary without needing
/// the root value — any such target is outside for a scoped run.
fn is_escaping_path(target: &str) -> bool {
    target.starts_with('/')
        || target.starts_with('~')
        || target.contains("..")
        || target.chars().any(|c| SHELL_INTERPOLATION_CHARS.contains(&c))
}

/// True when `action` belongs to a grantable mutation class: filesystem
/// write/edit tools and git local-mutate tools. Shell MutateLocal is NEVER
/// blanket-grantable (ADR-55 §2 shell scope hole); other tools are not
/// grantable either. Only meaningful within the MutateLocal tier arm, where
/// the tier is already established.
fn is_grantable_class(action: &PolicyAction<'_>) -> bool {
    matches!(action.tool_name, "filesystem" | "git")
}

/// ADR-55 shell scope amendment (2026-09-10 security recast): is `action`
/// a project-bounded `shell` command whose policy verdict may be
/// auto-approved under an Acting grant, exactly like in-scope filesystem
/// writes?
///
/// Conservative and pure — the upgrade predicate, NOT a bypass: it may only
/// ever explain why a command should upgrade, never why to widen one. Any
/// doubt answers `false` (the command keeps its existing approval path),
/// because an upgrade may only ever flip `RequireApproval` → `Allow`, never
/// a `Deny` (§Decision 2). The EXISTING denylist/Consequential/network/
/// writer rules still run first in the engine; this predicate only gates the
/// shell arm otherwise headed to [`RULE_SHELL_REQUIRES_APPROVAL`].
///
/// Required:
/// - the tool is `shell` **and** structured `CommandPolicyFacts` came from
///   the producing tool (facts are executor-produced at the policy-action
///   boundary, never model text — a raw command string without facts cannot
///   prove its scope, so without facts this answers `false`);
/// - the facts' `FilesystemScope` is `FilesystemScope::ProjectOnly`: the
///   shell runtime resolved the working directory against the session
///   project root at facts time (cwd containment);
/// - the facts did not request network egress (belt with the Consequential
///   network classification, which already precedes this predicate);
/// - **segment independence**: the command text is first split into
///   shell-list segments on `&&`, `||`, `;`, unspaced and spaced `|`, `&`
///   (background), and newlines — both between spaced tokens and glued
///   inside one (`cargo build&&rm -rf src` splits into `cargo build` and
///   `rm -rf src`). EVERY segment is then evaluated independently, and the
///   command upgrades only if **every** segment proves it:
///   - a bare `cd`/`pushd` at a segment boundary or end-of-segment is
///     UNBOUNDED (`cd && touch pwned` pivots to `$HOME`) — dead by
///     construction;
///   - the segment's leading verb must be on the positive allowlist
///     ([`SHELL_UPGRADE_ALLOWLIST`], `cd`/`pushd` with their own bounded
///     shape) — the prior negative destructive/network scan is replaced
///     wholesale: anything not allowlisted (`sudo`, `rm`, `find`, `xargs`,
///     `env`, `make`, `cmake`, `npm`, `tee`, interpreters, package
///     managers) keeps the approval path;
///   - interpreter verbs ([`SHELL_INTERPRETERS`] +
///     [`SHELL_INTERPRETER_INVOKERS`], plus any `python*` spelling) never
///     upgrade — code-flag (`php -r '…'`) and script-file (`python pwn.py`)
///     forms alike execute model-authored code whose effects are invisible
///     to text scanning;
///   - [F1] no token in the segment carries an interpolation metachar
///     (`$`, backtick, `%`, quotes) — expansion and quoted content mean the
///     scanned text is not the executed text;
///   - the segment carries no write-redirect (any redirect writes a file;
///     writes go through the filesystem tools, not the upgrade);
///   - no unbounded `cd`/`pushd` anywhere in the segment: only a relative,
///     interpolation-free, `..`-free single target is accepted (`cd src`);
///     bare `cd`, `cd -`, and anything else unprovable are rejected;
///   - no token resolves outside the session project root: absolute path,
///     `~`-pivot, `..` climb, backslash/UNC or drive-colon form.
/// - the audit row the upgrade produces is `intent_authorized_shell`
///   ([`RULE_INTENT_AUTHORIZED_SHELL`], F4), distinct from the filesystem
///   row, so shell auto-approvals are individually auditable.
pub fn is_project_bounded_shell(action: &PolicyAction<'_>) -> bool {
    if action.tool_name != "shell" {
        return false;
    }
    let Some(facts) = action.command_facts.as_ref() else {
        return false;
    };
    if facts.filesystem_scope != crate::types::FilesystemScope::ProjectOnly {
        return false;
    }
    if facts.network_requested {
        return false;
    }
    let Some(text) = shell_command_text(action) else {
        return false;
    };
    // The scan tables are lowercase; lowercasing once is safe for every
    // check below (escape/redirect/`cd` scans are defined over symbolic
    // characters that lowercase does not change).
    let lower = text.to_ascii_lowercase();
    // Segment independence: the command upgrades ONLY if every
    // separator-delimited segment independently proves project bounds. An
    // iteration (not `.all(fn)`): the segment iterator borrows `lower`.
    let mut all_segments_bounded = true;
    for segment in command_segments(&lower) {
        if !segment_is_project_bounded(segment) {
            all_segments_bounded = false;
            break;
        }
    }
    all_segments_bounded
}

/// Shell list-separator characters: `&`, `|`, `;`, and newlines. Splitting on
/// each character (not just the spaced operators) is deliberately stricter —
/// it also cuts GLUED separators out of a single token (`cargo build&&rm`,
/// `ls|rm`; glued `&&`, `||`, spaced `;`, lone `&` background, FD-redirection
/// continuation). A false cut costs at most an approval prompt; a missed
/// separator would let a second segment hide its verb from the allowlist.
const SHELL_SEGMENT_SEPARATORS: &[char] = &['&', '|', ';', '\n', '\r'];

/// Yields the shell command's list segments: the text split on every
/// [`SHELL_SEGMENT_SEPARATORS`] character, so command lists the shell will
/// sequence (`a && b`, `a || b`, `a ; b`, `a | b`, `a & b`, newline lists)
/// become independent segments — spaced or glued alike.
fn command_segments(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c| SHELL_SEGMENT_SEPARATORS.contains(&c))
}

/// The positive allowlist a segment's leading verb must name for the shell
/// upgrade to apply (2026-09-10): provably project-bounded verbs only.
///
/// Per-verb argument bounds (applied to the whole segment):
/// - `cargo` / `rustc`: build/test/lint/format/compile of workspace sources —
///   every argument must resolve in-root and metachar-free (`-p concerto-core`,
///   `--lib`, `-- -n` pass; `--target-dir /x` is an absolute-token escape and
///   keeps approval);
/// - `mkdir` / `touch` / `mv` / `cp`: the bounded general-purpose mutation
///   set shell MutateLocal already carries — in-root paths only;
/// - `echo` / `ls` / `cat`: read/report verbs (and `echo` without a
///   redirect — any redirect token fails the segment before this point).
///
/// Everything else is NOT allowlisted and keeps the existing approval path:
/// `rm`/`rmdir`/`unlink` (destructive table, tier-Consequential or prompted),
/// `ln` (symlinks pivot out of the root through the link, so the link target
/// and its dereference cannot both be proven in-root), `git` (subcommand
/// surface too broad to bound by verb: `branch -D`, `checkout` discarding
/// edits, `config` writes; global flags like `--work-tree`/`-C` add a second
/// root-scoping dialect — git mutations go through the purpose-built
/// `git` tool, which is grant-reviewed in scope), `find`, `xargs`, `env`,
/// `make`, `cmake`, `npm`/`pip`/… (runners and package managers), each
/// interpreter verb, and every unknown verb — all keep approval.
const SHELL_UPGRADE_ALLOWLIST: &[&str] =
    &["cargo", "rustc", "mkdir", "touch", "mv", "cp", "echo", "ls", "cat"];

/// True when one whitespace-tokenized [`command_segments`] segment proves it
/// stays inside the project root: `false` (keep approval) for anything that
/// cannot positively be proven bounded. An empty segment runs nothing.
fn segment_is_project_bounded(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.is_empty() {
        // `a ; ; b`: an empty segment between separators runs nothing extra
        // beyond what the surviving tokens already scanned as.
        return true;
    }
    // F1 (env indirection): interpolation metacharacters in any token mean
    // the text the scanner sees is NOT the text the shell runs — env
    // expansion (`$HOME`, `${HOME}`, `$(…)`, `%USERPROFILE%`) and quoted
    // argument content escape the token scan. Never auto-approved.
    if tokens.iter().any(|token| token.chars().any(|c| SHELL_INTERPOLATION_CHARS.contains(&c))) {
        return false;
    }
    // Any write-redirect (`>`, `>>`, `2>`, ...) writes a file — the smoke
    // allowlist does not carry redirect forms at all (file creation belongs
    // to the filesystem tools).
    if has_write_redirect(&tokens) {
        return false;
    }
    // Executing an absolute verb path (or a drive/UNC verb) keeps approval —
    // the basename allowlist alone cannot prove the executable sits inside
    // the project.
    if is_escaping_shell_token(tokens[0]) {
        return false;
    }
    // The allowlist matches a BARE verb only: a relative verb path
    // (`./cargo`, `target/debug/ls`) is a DIFFERENT executable from the
    // PATH-resolved allowlisted name its basename reduction would mask, so
    // the reduction must not launder it onto [`SHELL_UPGRADE_ALLOWLIST`].
    // (`../bin/cargo`-class climbs are already rejected above by the
    // `..`-containment scan.)
    if tokens[0].contains('/') {
        return false;
    }
    let verb = verb_basename(tokens[0]);
    // Interpreter bound (F3, recast): interpreter invocations never upgrade —
    // code-flag OR plain script-file form. `python pwn.py` runs model-authored
    // code with effects no text scan can see.
    if is_upgrade_interpreter(verb) {
        return false;
    }
    // `cd`/`pushd`: the ONLY bounded shape is a single relative, metachar-
    // free, `..`-free target resolving against the in-project working
    // directory (`cd src`). Nothing else (`cd` bare → `$HOME`, `cd -` →
    // unknown previous dir, `cd /abs`, `cd ..`) can be proven in-project.
    if matches!(verb, "cd" | "pushd") {
        return tokens.len() == 2 && tokens[1] != "-" && !is_escaping_shell_token(tokens[1]);
    }
    // Positive allowlist: a leading verb not in the table keeps approval —
    // this is what replaces the negative destructive/network verb scan.
    if !SHELL_UPGRADE_ALLOWLIST.contains(&verb) {
        return false;
    }
    // Every argument (and any other token) must resolve inside the project:
    // no absolute path, `~`, `..` climb, backslash, or drive/UNC colon form.
    // Belt-and-braces: an unbounded `cd`/`pushd` appearing beyond the verb
    // position still rejects the segment.
    tokens.iter().all(|token| !is_escaping_shell_token(token)) && !has_unbounded_cd(&tokens)
}

/// F1: true when `tokens` contains a `cd`/`pushd` whose target cannot be
/// proven to resolve inside the session project root:
///
/// - a bare `cd` sends the shell to `$HOME` in bash — outside by
///   construction for a project-scoped run;
/// - `cd -` re-enters the previous directory the run cannot know;
/// - an absolute / home / `..` target fails [`is_escaping_shell_token`].
///
/// The only target this accepts is a relative, interpolation-free,
/// `..`-free token, which lexically resolves against the in-project working
/// directory and therefore provably stays inside it (`cd src`). A false
/// positive here only costs an approval prompt; a false negative would
/// auto-approve a working-directory pivot outside the root.
fn has_unbounded_cd(tokens: &[&str]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        matches!(*token, "cd" | "pushd")
            && match tokens.get(i + 1) {
                None => true,
                Some(target) => *target == "-" || is_escaping_shell_token(target),
            }
    })
}

/// F3 (recast): true when `verb` names ANY code-execution interpreter —
/// [`SHELL_INTERPRETERS`] (extended per the 2026-09-10 review), the
/// shell-family invokers [`SHELL_INTERPRETER_INVOKERS`], or any `python*`
/// dialect (`python3.11`). The upgrade predicate NEVER upgrades these,
/// code-flag or script-file form alike.
fn is_upgrade_interpreter(verb: &str) -> bool {
    SHELL_INTERPRETERS.contains(&verb)
        || SHELL_INTERPRETER_INVOKERS.contains(&verb)
        || verb.starts_with("python")
}

/// True when `token` could name a target outside the project root: an
/// absolute path, a home pivot, or ANY `..`-containing token (`..`, `../x`,
/// `x/../y`, backslash variants, glued forms). Deliberately coarser than the
/// execution-time containment canon — a false *positive* here only costs an
/// approval prompt; a false negative would upgrade an escape.
fn is_escaping_shell_token(token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with('\\')
        || token.starts_with('~')
        || token.contains("..")
        || token.contains('\\')
        || token.contains(':')
}

/// F3: true when `tokens` contains an interpreter verb AND a code flag
/// (`-c`, `-e`, `-m`, `-r`, exact or glued). Position-independent: a
/// wrapper verb (`sudo python -c …`, `env python -m …`) is still an
/// interpreter invocation.
fn is_interpreter_code_invocation(tokens: &[&str]) -> bool {
    tokens.iter().copied().map(verb_basename).any(|verb| SHELL_INTERPRETERS.contains(&verb))
        && tokens
            .iter()
            .any(|token| INTERPRETER_CODE_FLAGS.iter().any(|flag| token.starts_with(flag)))
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

    // ------------------------------------------------------------------
    // ADR-55 shell scope amendment: is_project_bounded_shell
    // ------------------------------------------------------------------

    /// Shell facts as the tool itself derives them: the working directory
    /// resolved against the session project root, egress flag per argument.
    fn facts(in_root: bool, network: bool) -> CommandPolicyFacts {
        crate::types::CommandPolicyFacts {
            shell_profile_id: None,
            resolved_executable: Some(PathBuf::from("/usr/bin/bash")),
            argv: vec!["/bin/bash".to_owned(), "-c".to_owned(), "resolved".to_owned()],
            working_directory: Some(PathBuf::from(if in_root {
                "/proj/sub"
            } else {
                "/home/other"
            })),
            network_requested: network,
            filesystem_scope: if in_root {
                FilesystemScope::ProjectOnly
            } else {
                FilesystemScope::Anywhere
            },
            destructive_classification: DestructiveClass::NonDestructive,
        }
    }

    /// An action carrying shell facts; the input lives as long as the test
    /// local that builds it (`shell_action_with(&value, facts)`).
    fn shell_action_with<'a>(
        input: &'a serde_json::Value,
        facts: CommandPolicyFacts,
    ) -> PolicyAction<'a> {
        PolicyAction {
            tool_name: "shell",
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: Some(facts),
        }
    }

    fn shell_command(command: &str) -> serde_json::Value {
        let mut parts = command.split_whitespace();
        let head = parts.next().unwrap_or_default().to_owned();
        let args: Vec<&str> = parts.collect();
        serde_json::json!({ "command": head, "args": args })
    }

    /// In-project `cargo build`-class commands with in-root facts classify
    /// project-bounded. Quoted text is NOT (F1) — `sh -c 'cd src'` carries a
    /// quote, so the smoke allowlist sticks to metachar-free commands and an
    /// in-project `cd src` target (its only provably in-`cd`-able shape).
    #[test]
    fn in_root_commands_with_in_root_facts_are_project_bounded() {
        for command in
            ["cargo build", "cargo test", "cargo test --lib", "cd src && cargo test", "mkdir src"]
        {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                is_project_bounded_shell(&act),
                "in-project command must be project-bounded: {command}"
            );
            // MutateLocal tier as classified before the predicate ever runs.
            assert_eq!(classify_tier(&act), IntentTier::MutateLocal);
        }
    }

    /// F1 (env indirection): interpolation metacharacters (`$`, backtick,
    /// `%`, quotes) in ANY token keep the command on its approval path, as do
    /// bare `cd`, `cd -`, and quoted targets the word split cannot see into.
    #[test]
    fn interpolated_and_quoted_commands_are_never_project_bounded() {
        for command in [
            "cd $HOME && rm -rf Documents",
            "echo pwned > $HOME/.bashrc",
            "touch $(echo $HOME)/pwned",
            "cd %USERPROFILE%",
            "cd -",
            "cd",
            "sh -c 'cd src'", // quote hides the cd target from the scan
            "echo hi && printf '%s' x",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "interpolation/quote/bare-cd must keep the approval path: {command}"
            );
        }
    }

    /// F2 (verb hiding): a destructive or network verb in ANY position or
    /// segment — not just the first verb — keeps the existing approval path.
    #[test]
    fn hidden_destructive_and_network_verbs_are_never_project_bounded() {
        for command in [
            "cargo build && rm -rf src",
            "sudo rm f",
            "timeout 5 rm f",
            "env rm",
            "xargs rm",
            "find . -delete",
            "find . -name '*.rs' -delete",
            "cat a | xargs rm",
            "cargo build && curl https://example.com",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "hidden verb must keep the approval path: {command}"
            );
        }
    }

    /// F3: network facts word list aligned with `SHELL_NETWORK_VERBS` — the
    /// missing `ncat`/`socat`/`sftp` transport clients are Consequential at
    /// the tier level too, so they can never take the upgrade.
    #[test]
    fn aligned_network_clients_are_consequential() {
        for (command, args) in [
            ("ncat", vec!["evil.example", "9000"]),
            ("socat", vec!["TCP-LISTEN:9000", "EXEC:sh"]),
            ("sftp", vec!["host:file"]),
        ] {
            assert_eq!(
                tier("shell", serde_json::json!({"command": command, "args": args})),
                IntentTier::Consequential,
                "{command} should be Consequential"
            );
        }
    }

    /// F3: interpreter invocations carrying a code flag (`-c`, `-e`, `-m`,
    /// `-r`) classify Consequential wherever they sit (`sudo`, `env`
    /// wrappers included), so they can never take the upgrade; an
    /// interpreter without a code flag stays MutateLocal.
    #[test]
    fn interpreter_code_invocations_are_consequential() {
        for command in [
            ("python", vec!["-m", "http.server"]),
            ("python3", vec!["-c", "import os"]),
            ("node", vec!["-e", "fetch('https://x')"]),
            ("perl", vec!["-e", "print 1"]),
            ("ruby", vec!["-e", "puts 1"]),
            ("env", vec!["python", "-r"]),
            ("sudo", vec!["node", "-c", "1"]),
        ] {
            let input = serde_json::json!({"command": command.0, "args": command.1});
            assert_eq!(
                tier("shell", input),
                IntentTier::Consequential,
                "{command:?} with a code flag should be Consequential"
            );
        }
        // No code flag: stays MutateLocal — the interpreter verbs are only
        // consequential when attacker-chosen code rides a flag.
        assert_eq!(
            tier("shell", serde_json::json!({"command": "node", "args": ["script.js"]})),
            IntentTier::MutateLocal
        );
    }

    /// Any doubt keeps the existing approval path: no facts, an outside-root
    /// working scope, or network egress is never project-bounded.
    #[test]
    fn doubtful_commands_are_never_project_bounded() {
        let command = "cargo build";
        let without_facts = serde_json::json!({ "command": "cargo", "args": ["build"] });
        let no_facts = PolicyAction {
            tool_name: "shell",
            input: &without_facts,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        assert!(
            !is_project_bounded_shell(&no_facts),
            "no facts: the raw text cannot prove its scope"
        );

        let input_cargo = shell_command(command);
        let outside_root = shell_action_with(&input_cargo, facts(false, false));
        assert!(
            !is_project_bounded_shell(&outside_root),
            "outside-root working directory is never project-bounded"
        );

        let networked = shell_action_with(&input_cargo, facts(true, true));
        assert!(!is_project_bounded_shell(&networked), "network egress never auto-upgrades");
    }

    /// Escapes in the command text, `cd` targets, and redirect targets keep
    /// the command under its existing approval path (`../` climb, absolute
    /// outside-root target).
    #[test]
    fn escapes_keep_the_existing_approval_path() {
        for command in [
            "cargo build ../other",
            "cat /etc/os-release", // absolute outside-root token
            "echo x > ../out",
            "cd .. && cargo build",
            "bash -c \"cd /var && pwd\"",
            "cat ~/notes",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "escape must keep the approval path: {command}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Security recast (2026-09-10): segment-aware positive allowlist
    // ------------------------------------------------------------------

    /// Recast: a bare `cd`/`pushd` hitting a segment separator
    /// (`cd && …` — including the GLUED `cd&&touch` form the whitespace
    /// split used to hide behind) or the end of a segment is UNBOUNDED —
    /// it pivots the working directory to `$HOME`, outside any
    /// project-scoped run. The old whitespace-only tokenization let
    /// `cd && touch pwned` through (`cd` scanned with target `&&`).
    #[test]
    fn recast_bare_cd_at_segment_boundaries_is_never_project_bounded() {
        for command in [
            "cd && touch pwned",
            "cd&&touch pwned",          // glued separator
            "cd; echo pwned > .bashrc", // bare cd then a redirect descend
            "cd&&echo pwned > .bashrc",
            "cd||touch pwned",
            "cd",
            "cd -",
            "pushd&&ls",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "bare cd at a segment boundary must keep the approval path: {command}"
            );
        }
    }

    /// Recast: glued/separated compound commands — the second segment's
    /// destructive verb must kill the upgrade no matter how glued the
    /// separators are (`cargo build&&rm -rf src` used to hide `rm` inside
    /// the token `build&&rm`).
    #[test]
    fn recast_glued_compound_commands_with_hiding_verbs_are_never_project_bounded() {
        for command in [
            "cargo build && rm -rf src",
            "cargo build&&rm -rf src",
            "cargo build; rm -rf src",
            "cargo build;rm -rf src",
            "ls|rm",
            "ls | rm",
            "cargo build&&rm -rf src&&echo ok",
            "cargo build&&rm -rf src;rmdir target",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "hidden verb in any glued segment must keep the approval path: {command}"
            );
        }
    }

    /// Recast: interpreter execution NEVER upgrades — the plain
    /// script-file form (`python pwn.py`) executes model-authored code
    /// whose network/out-of-root effects no text scan can see. Spaced,
    /// glued, and either-position forms alike; the shell-family
    /// invokers (`sh`/`bash`/…) are covered too.
    #[test]
    fn recast_interpreter_script_execution_is_never_project_bounded() {
        for command in [
            "python pwn.py",
            "python3 pwn.py",
            "python3.11 pwn.py",
            "node pwn.js",
            "perl x.pl",
            "ruby x.rb",
            "php script.php",
            "lua script.lua",
            "Rscript x.R",
            "powershell script.ps1",
            "pwsh script.ps1",
            "bash pwn.sh",
            "sh pwn.sh",
            "python pwn.py && ls",
            "ls && python pwn.py",
            "ls||python pwn.py",
            "ls|python pwn.py",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "interpreter execution must keep the approval path: {command}"
            );
        }
    }

    /// Recast: verbs outside the positive allowlist keep the existing
    /// approval path — interpreters with code flags, runners, package
    /// managers, exporters, and symlink pivots.
    #[test]
    fn recast_non_allowlisted_verbs_keep_the_approval_path() {
        for command in [
            "make install",
            "cmake --install build",
            "cmake --install build;ls",
            "sudo make install",
            "powershell -c Remove-Item src",
            "php -r 'print 1'",
            "python -m http.server",
            "node -e fetch pwn",
            "xargs rm",
            "env cargo build",
            "find . -delete",
            "ln -s /etc/passwd here",
            "npm install x",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "non-allowlisted verb must keep the approval path: {command}"
            );
        }
    }

    /// Recast follow-up (glued redirect): a `>` at any NON-INITIAL position
    /// of a token is still a write redirect — `echo pwned>~/.bashrc` writes
    /// `~/.bashrc` (mirroring the containment canon, `rfind('>')`), yet the
    /// operator-prefix-only scan used to miss the word-glued form. In-root
    /// forms stay governed by the existing no-redirect rule (asserted, not
    /// loosened): the allowlist carries no redirect shapes at all, so even
    /// `2>err.log` and glued `a>>b` keep the approval path.
    #[test]
    fn recast_glued_word_redirects_are_never_project_bounded() {
        for command in [
            "echo pwned>~/.bashrc",
            "cat f>~/.ssh/authorized_keys",
            "echo x>/tmp/y",
            "echo a>>b",      // glued append, in-root target
            "ls x 2>err.log", // in-root FD form — existing prefix rule already fails it
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "redirect in any glued form must keep the approval path: {command}"
            );
        }
        // No-redirect controls are unaffected.
        for command in ["echo ok", "cargo build"] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                is_project_bounded_shell(&act),
                "no-redirect control must stay project-bounded: {command}"
            );
        }
    }

    /// Recast follow-up (relative verb): the allowlist matches a BARE verb
    /// only — `./cargo` and `target/debug/ls` are relative executables
    /// distinct from the PATH-resolved allowlisted verbs their basename
    /// reduction would mask, so reduction must not launder them onto the
    /// allowlist. `../bin/cargo` climbs regardless of its verb name.
    #[test]
    fn recast_relative_verb_paths_are_never_project_bounded() {
        for command in ["./cargo build", "target/debug/ls x", "../bin/cargo build"] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                !is_project_bounded_shell(&act),
                "relative verb path must keep the approval path: {command}"
            );
        }
        // Bare verbs still upgrade.
        for command in ["cargo build", "ls x"] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                is_project_bounded_shell(&act),
                "bare verb control must stay project-bounded: {command}"
            );
        }
    }

    /// Recast: the smoke-path controls STILL upgrade — the bounded verbs
    /// of the fixture set, in plain and compound form.
    #[test]
    fn recast_smoke_path_controls_still_upgrade() {
        for command in [
            "cargo build",
            "cargo test",
            "cargo test --lib",
            "mkdir src",
            "cd src && cargo build",
            "cd src && cargo test --lib",
            "ls",
            "ls -la src",
            "cat Cargo.toml",
        ] {
            let input = shell_command(command);
            let act = shell_action_with(&input, facts(true, false));
            assert!(
                is_project_bounded_shell(&act),
                "smoke-path command must remain project-bounded: {command}"
            );
        }
    }
}
