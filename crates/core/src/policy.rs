use crate::authorization::{IntentAuthorization, IntentVerdict};
use crate::error::PolicyError;
use crate::traits::policy::{AuditEntry, AuditLog, PolicyEngine};
use crate::types::{
    CodeCategory, Condition, PolicyAction, PolicyRule, PolicyVerdict, SandboxProfile,
};
use crate::CancellationToken;
use async_trait::async_trait;
use chrono::Timelike;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tracing::error;

/// Concrete policy engine that walks rules in order; first match wins.
pub struct SimplePolicyEngine {
    rules: Vec<PolicyRule>,
    audit: Arc<dyn AuditLog>,
    compiled: HashMap<String, regex::Regex>,
    spend_tracker: Option<Arc<SpendTracker>>,
    rate_limiter: Option<Arc<RpmLimiter>>,
    /// ADR-55 §2: optional, session-scoped, non-durable source of
    /// authorization state consulted by `Condition::IntentAuthorized`.
    /// `None` (the default) preserves exact pre-ADR-55 behavior.
    intent_auth: Option<Arc<dyn IntentAuthorization>>,
}

impl SimplePolicyEngine {
    pub fn new(rules: Vec<PolicyRule>, audit: Arc<dyn AuditLog>) -> Self {
        let compiled = precompile_command_patterns(&rules);
        Self { rules, audit, compiled, spend_tracker: None, rate_limiter: None, intent_auth: None }
    }

    /// Attach a spend tracker to enforce session/task/daily cost caps.
    pub fn with_spend_tracker(mut self, tracker: Arc<SpendTracker>) -> Self {
        self.spend_tracker = Some(tracker);
        self
    }

    /// Attach a per-provider rate limiter to enforce RPM caps.
    pub fn with_rate_limiter(mut self, limiter: Arc<RpmLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Attach an intent-authorization state source (ADR-55 §2).
    ///
    /// The provider returns the *full* policy outcome for each action:
    /// [`IntentVerdict::Allow`] upgrades `RequireApproval` → `Allow`,
    /// [`IntentVerdict::RequireApproval`] keeps the action under the approval
    /// path, and [`IntentVerdict::Deny`] is final and precedes any approval
    /// sink. Not attaching one (the default) keeps behavior identical to
    /// pre-ADR-55.
    pub fn with_intent_auth(mut self, auth: Arc<dyn IntentAuthorization>) -> Self {
        self.intent_auth = Some(auth);
        self
    }

    fn evaluate_rules(&self, action: &PolicyAction<'_>) -> Option<(PolicyVerdict, String)> {
        for rule in &self.rules {
            match rule {
                PolicyRule::AutoApprove(cond) => {
                    if self.eval_cond(cond, action) {
                        return Some((PolicyVerdict::Allow, "auto_approve".into()));
                    }
                }
                PolicyRule::AutoDeny(cond) => {
                    if self.eval_cond(cond, action) {
                        return Some((PolicyVerdict::Deny, "auto_deny".into()));
                    }
                }
                PolicyRule::RequireApproval(cond) => {
                    if let Some((verdict, rule)) = self.eval_approval_rule(
                        cond,
                        std::time::Duration::from_secs(30),
                        "require_approval",
                        PolicyVerdict::RequireApproval {
                            timeout: std::time::Duration::from_secs(30),
                        },
                        action,
                    ) {
                        return Some((verdict, rule));
                    }
                }
                PolicyRule::RequireApprovalWithTimeout { condition, timeout_secs } => {
                    if let Some((verdict, rule)) = self.eval_approval_rule(
                        condition,
                        std::time::Duration::from_secs(*timeout_secs),
                        "require_approval_with_timeout",
                        PolicyVerdict::RequireApprovalWithTimeout {
                            timeout: std::time::Duration::from_secs(*timeout_secs),
                        },
                        action,
                    ) {
                        return Some((verdict, rule));
                    }
                }
                PolicyRule::RequireManagedToolApproval(cond) => {
                    if let Some((verdict, rule)) = self.eval_approval_rule(
                        cond,
                        std::time::Duration::from_secs(30),
                        "require_managed_tool_approval",
                        PolicyVerdict::RequireApproval {
                            timeout: std::time::Duration::from_secs(30),
                        },
                        action,
                    ) {
                        return Some((verdict, rule));
                    }
                }
                PolicyRule::RequireToolchainApproval(cond) => {
                    if let Some((verdict, rule)) = self.eval_approval_rule(
                        cond,
                        std::time::Duration::from_secs(30),
                        "require_toolchain_approval",
                        PolicyVerdict::RequireApproval {
                            timeout: std::time::Duration::from_secs(30),
                        },
                        action,
                    ) {
                        return Some((verdict, rule));
                    }
                }
                PolicyRule::DenyNetworkEgress(cond) => {
                    if self.eval_cond(cond, action) && action_is_network_op(action) {
                        return Some((PolicyVerdict::Deny, "deny_network_egress".into()));
                    }
                }
            }
        }
        None
    }

    /// Evaluate an approval-producing rule, applying the ADR-55 intent gate
    /// first when the rule's condition is the bare `Condition::IntentAuthorized`.
    ///
    /// Returns `Some((verdict, rule))` when the rule decides (via the gate or
    /// via a normal condition match) and `None` when it should fall through:
    /// the rule does not match, or the gate is inactive because no
    /// authorization provider is attached. In the fall-through case the walk
    /// continues exactly like pre-ADR-55.
    fn eval_approval_rule(
        &self,
        cond: &Condition,
        timeout: std::time::Duration,
        default_rule: &str,
        default_verdict: PolicyVerdict,
        action: &PolicyAction<'_>,
    ) -> Option<(PolicyVerdict, String)> {
        if let Some((verdict, rule)) = self.eval_intent_gate(cond, timeout, action) {
            return Some((verdict, rule));
        }
        if self.eval_cond(cond, action) {
            return Some((default_verdict, default_rule.to_owned()));
        }
        None
    }

    /// ADR-55 §2 intent gate: apply the attached authorization's verdict
    /// mechanically to the rule's normal outcome. `Allow` upgrades
    /// `RequireApproval` → `Allow` (audit `rule_matched` = the verdict's rule);
    /// `RequireApproval` keeps the action under the rule's approval path;
    /// `Deny` is a final, pre-sink denial that no approval decision can
    /// override.
    ///
    /// Only the bare `Condition::IntentAuthorized` triggers the gate; a
    /// condition that *contains* it through a combinator is treated as an
    /// ordinary boolean predicate by [`Self::eval_cond`] (conservative: it can
    /// never upgrade, only match). Returns `None` when no authorization
    /// provider is attached, so the caller's rule walk keeps its pre-ADR-55
    /// semantics.
    fn eval_intent_gate(
        &self,
        cond: &Condition,
        timeout: std::time::Duration,
        action: &PolicyAction<'_>,
    ) -> Option<(PolicyVerdict, String)> {
        if !matches!(cond, Condition::IntentAuthorized) {
            return None;
        }
        let auth = self.intent_auth.as_ref()?;
        match auth.verdict(action) {
            IntentVerdict::Allow { rule } => Some((PolicyVerdict::Allow, rule.to_owned())),
            IntentVerdict::RequireApproval { rule } => {
                Some((PolicyVerdict::RequireApproval { timeout }, rule.to_owned()))
            }
            IntentVerdict::Deny { rule } => Some((PolicyVerdict::Deny, rule.to_owned())),
        }
    }

    fn eval_cond(&self, cond: &Condition, action: &PolicyAction<'_>) -> bool {
        match cond {
            Condition::ToolName(name) => action.tool_name == name,
            Condition::ToolNamePrefix(prefix) => action.tool_name.starts_with(prefix.as_str()),
            Condition::PathGlob(glob) => action
                .input
                .get("path")
                .or_else(|| action.input.get("file_path"))
                .and_then(|v| v.as_str())
                .map(|p| {
                    self.compiled.get(&format!("glob:{glob}")).is_some_and(|re| re.is_match(p))
                })
                .unwrap_or(false),
            Condition::CommandPattern(pattern) => {
                command_with_arguments(action.input).is_some_and(|command| {
                    self.compiled.get(pattern).is_some_and(|regex| regex.is_match(command.as_ref()))
                })
            }
            Condition::Operation(op) => {
                action.input.get("operation").and_then(|v| v.as_str()).is_some_and(|o| o == op)
            }
            Condition::GitOperation(op) => {
                action.tool_name == "git"
                    && action
                        .input
                        .get("operation")
                        .and_then(|v| v.as_str())
                        .is_some_and(|o| o == op)
            }
            Condition::Capability(required_caps) => {
                action.capability_requirements.is_superset(required_caps)
            }
            Condition::CodeType(category) => {
                let path = action
                    .input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .or_else(|| action.input.get("file_path").and_then(|v| v.as_str()));
                path.is_some_and(|p| CodeCategory::classify(p) == *category)
            }
            Condition::SecretPattern(pattern) => {
                let re = match self.compiled.get(pattern) {
                    Some(re) => re,
                    None => return false,
                };
                // Check the file content being written
                if let Some(content) = action.input.get("content").and_then(|v| v.as_str()) {
                    if re.is_match(content) {
                        return true;
                    }
                }
                // Check the file path
                if let Some(path) = action.input.get("path").and_then(|v| v.as_str()) {
                    if re.is_match(path) {
                        return true;
                    }
                }
                // Fallback: serialise the whole input as JSON
                if let Ok(json) = serde_json::to_string(action.input) {
                    return re.is_match(&json);
                }
                false
            }
            Condition::ShellProfile(id) => action
                .command_facts
                .as_ref()
                .and_then(|f| f.shell_profile_id.as_deref())
                .is_some_and(|pid| pid == id),
            Condition::ResolvedExecutable(pattern) => action
                .command_facts
                .as_ref()
                .and_then(|f| f.resolved_executable.as_ref())
                .and_then(|p| p.to_str())
                .map(|p| {
                    self.compiled.get(&format!("rexec:{pattern}")).is_some_and(|re| re.is_match(p))
                })
                .unwrap_or(false),
            Condition::ArgvPattern(pattern) => action
                .command_facts
                .as_ref()
                .map(|f| {
                    let joined = f.argv.join(" ");
                    self.compiled
                        .get(&format!("argv:{pattern}"))
                        .is_some_and(|re| re.is_match(&joined))
                })
                .unwrap_or(false),
            Condition::WorkingDir(glob) => action
                .command_facts
                .as_ref()
                .and_then(|f| f.working_directory.as_ref())
                .and_then(|p| p.to_str())
                .map(|p| self.compiled.get(&format!("wd:{glob}")).is_some_and(|re| re.is_match(p)))
                .unwrap_or(false),
            // ADR-55 §2: as a plain boolean predicate the intent condition
            // matches only when an attached authorization allows the action.
            // Approval-producing rules handle the bare condition through
            // `eval_intent_gate` instead, so a compound condition can never
            // upgrade `RequireApproval` → `Allow`.
            Condition::IntentAuthorized => self
                .intent_auth
                .as_ref()
                .is_some_and(|auth| matches!(auth.verdict(action), IntentVerdict::Allow { .. })),
            Condition::Always => true,
            Condition::Not(inner) => !self.eval_cond(inner, action),
            Condition::All(conds) => conds.iter().all(|c| self.eval_cond(c, action)),
            Condition::Any(conds) => conds.iter().any(|c| self.eval_cond(c, action)),
        }
    }

    /// Check the active sandbox profile against the requested tool operation.
    ///
    /// Sandbox profiles are currently stubs and do not provide real runtime
    /// isolation. Any non-`None` profile produces a deny verdict so callers
    /// cannot accidentally rely on unimplemented sandboxing.
    fn check_sandbox(&self, action: &PolicyAction<'_>) -> Option<(PolicyVerdict, String)> {
        match action.sandbox_profile {
            None | Some(SandboxProfile::None) => None,
            Some(_) => Some((PolicyVerdict::Deny, "sandbox_profiles_not_implemented".into())),
        }
    }

    async fn record_audit(&self, entry: AuditEntry, cancel: CancellationToken) {
        if let Err(e) = self.audit.record(entry, cancel).await {
            error!(?e, "audit log write failed");
        }
    }

    /// Build and persist an audit entry for a verdict produced by one of the
    /// checks. Centralises the four previously-duplicated `AuditEntry` blocks.
    async fn record_decision(
        &self,
        action: &PolicyAction<'_>,
        verdict: &PolicyVerdict,
        rule: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: action.tool_name.to_string(),
            verdict: format!("{verdict:?}"),
            input_hash: compute_input_hash(action.input),
            session_id: action.session_id,
            correlation_id: action.correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: Some(rule.to_string()),
            // ---- ADR-28 §6/§7: carry structured facts forward to the log ----
            profile_id: action.command_facts.as_ref().and_then(|f| f.shell_profile_id.clone()),
            resolved_executable: action
                .command_facts
                .as_ref()
                .and_then(|f| f.resolved_executable.as_ref())
                .and_then(|p| p.to_str())
                .map(str::to_string),
            argv: action.command_facts.as_ref().map(|f| f.argv.clone()),
            working_directory: action
                .command_facts
                .as_ref()
                .and_then(|f| f.working_directory.as_ref())
                .and_then(|p| p.to_str())
                .map(str::to_string),
            network_requested: action.command_facts.as_ref().map(|f| f.network_requested),
            filesystem_scope: action
                .command_facts
                .as_ref()
                .map(|f| format!("{:?}", f.filesystem_scope)),
            destructive_classification: action
                .command_facts
                .as_ref()
                .map(|f| format!("{:?}", f.destructive_classification)),
            // Execution results are unknown at decision time. ToolExecutor
            // appends a correlated completion entry after execution.
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
        };
        self.record_audit(entry, cancel).await;
    }

    /// Fail fast on misconfigured policy rules. Returns an error listing every
    /// `CommandPattern`/`SecretPattern` regex and `PathGlob` that fails to
    /// compile. Callers should invoke this once after construction (e.g. at
    /// startup) rather than discovering invalid patterns only when evaluation
    /// silently skips them.
    pub fn validate(&self) -> Result<(), PolicyError> {
        let mut problems = Vec::new();
        for rule in &self.rules {
            let cond = match rule {
                PolicyRule::AutoApprove(c)
                | PolicyRule::AutoDeny(c)
                | PolicyRule::RequireApproval(c)
                | PolicyRule::RequireManagedToolApproval(c)
                | PolicyRule::RequireToolchainApproval(c)
                | PolicyRule::DenyNetworkEgress(c) => c,
                PolicyRule::RequireApprovalWithTimeout { condition, .. } => condition,
            };
            collect_invalid(cond, &mut problems);
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(PolicyError::InvalidRule(problems.join("; ")))
        }
    }

    /// Check spend caps and rate limits before rule evaluation.
    /// Returns a deny verdict if either check fails.
    async fn check_spend_and_rate_limits(
        &self,
        action: &PolicyAction<'_>,
    ) -> Option<(PolicyVerdict, String)> {
        // 1. Check spend tracker if configured and action has estimated cost.
        if let (Some(tracker), Some(cost)) = (&self.spend_tracker, action.estimated_cost_usd) {
            if let Err(e) = tracker.check(cost) {
                return Some((PolicyVerdict::Deny, format!("spend_cap_exceeded: {e}")));
            }
        }

        // 2. Check rate limiter if configured and this is a provider call.
        if let Some(limiter) = &self.rate_limiter {
            // Provider calls are identified by tool_name == "provider" or
            // by having a provider name in the input's "provider" field.
            let provider = if action.tool_name == "provider" {
                action.input.get("provider").and_then(|v| v.as_str()).unwrap_or("unknown")
            } else {
                // For non-provider tools, skip rate limiting.
                return None;
            };
            if let Err(e) = limiter.check(provider) {
                return Some((PolicyVerdict::Deny, format!("rate_limit_exceeded: {e}")));
            }
        }

        None
    }
}

/// Return the command text policy rules should evaluate.
///
/// The value is always reconstructed from the actual executable and arguments;
/// callers cannot provide a separate, potentially misleading policy string.
fn command_with_arguments(input: &serde_json::Value) -> Option<Cow<'_, str>> {
    let command = input.get("command").and_then(serde_json::Value::as_str)?;
    let args = input
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|values| values.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();

    if args.is_empty() {
        Some(Cow::Borrowed(command))
    } else {
        Some(Cow::Owned(format!("{command} {}", args.join(" "))))
    }
}

/// Pre-compile all `CommandPattern` and `SecretPattern` regexes so they are
/// compiled once at construction time rather than on every `evaluate` call.
fn precompile_command_patterns(rules: &[PolicyRule]) -> HashMap<String, regex::Regex> {
    let mut map = HashMap::new();
    for rule in rules {
        collect_patterns(rule, &mut map);
    }
    map
}

fn collect_patterns(rule: &PolicyRule, map: &mut HashMap<String, regex::Regex>) {
    let cond = match rule {
        PolicyRule::AutoApprove(c)
        | PolicyRule::AutoDeny(c)
        | PolicyRule::RequireApproval(c)
        | PolicyRule::RequireManagedToolApproval(c)
        | PolicyRule::RequireToolchainApproval(c)
        | PolicyRule::DenyNetworkEgress(c) => c,
        PolicyRule::RequireApprovalWithTimeout { condition, .. } => condition,
    };
    collect_from_cond(cond, map);
}

fn collect_from_cond(cond: &Condition, map: &mut HashMap<String, regex::Regex>) {
    match cond {
        Condition::CommandPattern(pattern) | Condition::SecretPattern(pattern) => {
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(pattern.clone()) {
                if let Ok(re) = regex::Regex::new(pattern) {
                    e.insert(re);
                } else {
                    error!(?pattern, "invalid regex in policy pattern");
                }
            }
        }
        Condition::PathGlob(glob) => {
            // Namespace the key so a literal glob can never collide with a
            // command/secret regex that happens to share the same source text.
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(format!("glob:{glob}"))
            {
                let re_str = glob_to_regex(glob);
                match regex::Regex::new(&re_str) {
                    Ok(re) => {
                        e.insert(re);
                    }
                    Err(err) => {
                        error!(?glob, error = %err, "invalid glob in policy pattern");
                    }
                }
            }
        }
        Condition::ResolvedExecutable(pattern) | Condition::ArgvPattern(pattern) => {
            // Namespace the key so a regex that happens to share source text
            // with a CommandPattern/SecretPattern cannot collide.
            let key = if matches!(cond, Condition::ResolvedExecutable(_)) {
                format!("rexec:{pattern}")
            } else {
                format!("argv:{pattern}")
            };
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(key) {
                if let Ok(re) = regex::Regex::new(pattern) {
                    e.insert(re);
                } else {
                    error!(?pattern, "invalid regex in policy pattern");
                }
            }
        }
        Condition::WorkingDir(glob) => {
            // Namespace like PathGlob so a working-dir glob cannot collide.
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(format!("wd:{glob}")) {
                let re_str = glob_to_regex(glob);
                match regex::Regex::new(&re_str) {
                    Ok(re) => {
                        e.insert(re);
                    }
                    Err(err) => {
                        error!(?glob, error = %err, "invalid glob in policy pattern");
                    }
                }
            }
        }
        Condition::Not(inner) => collect_from_cond(inner, map),
        Condition::All(conds) | Condition::Any(conds) => {
            for c in conds {
                collect_from_cond(c, map);
            }
        }
        _ => {}
    }
}

/// Collect every command/secret regex and path glob in `cond` that fails to
/// compile. Used by [`SimplePolicyEngine::validate`] to fail fast on
/// misconfigured rules instead of silently skipping them during evaluation.
fn collect_invalid(cond: &Condition, problems: &mut Vec<String>) {
    match cond {
        Condition::CommandPattern(pattern) | Condition::SecretPattern(pattern) => {
            if regex::Regex::new(pattern).is_err() {
                problems.push(format!("invalid regex pattern: {pattern}"));
            }
        }
        Condition::PathGlob(glob) => {
            if regex::Regex::new(&glob_to_regex(glob)).is_err() {
                problems.push(format!("invalid glob pattern: {glob}"));
            }
        }
        Condition::ResolvedExecutable(pattern) | Condition::ArgvPattern(pattern) => {
            if regex::Regex::new(pattern).is_err() {
                problems.push(format!("invalid regex pattern: {pattern}"));
            }
        }
        Condition::WorkingDir(glob) => {
            if regex::Regex::new(&glob_to_regex(glob)).is_err() {
                problems.push(format!("invalid glob pattern: {glob}"));
            }
        }
        Condition::Not(inner) => collect_invalid(inner, problems),
        Condition::All(conds) | Condition::Any(conds) => {
            for c in conds {
                collect_invalid(c, problems);
            }
        }
        _ => {}
    }
}

/// Convert a simple glob pattern to a regex string.
///
/// Supports:
/// - `**` → matches any sequence of characters, including `/`
/// - `*`  → matches any sequence of characters except `/`
/// - `?`  → matches any single character except `/`
/// - All other characters are matched literally (regex metacharacters are
///   escaped in the ASCII set — `.`, `+`, `(`, `)`, `[`, `]`, `{`, `}`,
///   `^`, `$`, `|`, `\`).
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::with_capacity(glob.len() + 24);
    re.push('^');
    let mut chars = glob.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // consume second *
                              // `**` with no following `/` matches everything.
                              // `**/` matches zero or more directory components,
                              // so the slash is optional.
                if chars.peek() == Some(&'/') {
                    chars.next(); // consume `/`
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '.' => re.push_str("\\."),
            '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(ch);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    re
}

#[cfg(test)]
fn glob_match(glob: &str, path: &str) -> bool {
    let re_str = glob_to_regex(glob);
    regex::Regex::new(&re_str).is_ok_and(|re| re.is_match(path))
}

fn shell_command_text(input: &serde_json::Value) -> Option<String> {
    let command = input.get("command")?.as_str()?;
    let mut full = command.to_string();
    if let Some(args) = input.get("args").and_then(|value| value.as_array()) {
        for arg in args.iter().filter_map(|value| value.as_str()) {
            full.push(' ');
            full.push_str(arg);
        }
    }
    Some(full)
}

/// Returns `true` if the policy action is itself a network-egress operation
/// (an HTTP/fetch tool, or a shell command that reaches the network). Used by
/// the `DenyNetworkEgress` rule so it only blocks actual egress, not every
/// command matched by its condition (ADR-28).
fn action_is_network_op(action: &PolicyAction<'_>) -> bool {
    // A tool may declare network egress explicitly via its structured facts
    // (ADR-28 §6), even when no shell command string is present.
    if action.command_facts.as_ref().is_some_and(|f| f.network_requested) {
        return true;
    }
    match action.tool_name {
        "http" | "fetch" | "curl" => true,
        "shell" => {
            shell_command_text(action.input).as_deref().map(cmd_is_network_op).unwrap_or(false)
        }
        _ => false,
    }
}

/// Heuristic detection of network-reaching shell commands under the
/// `NetworkIsolated` sandbox profile. Matches explicit transport verbs and
/// URL schemes; avoids bare substring scans (`http`, `api.`) that flagged
/// local files such as `my_http_notes.txt`.
fn cmd_is_network_op(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if lower.starts_with("curl ")
        || lower.starts_with("wget ")
        || lower.starts_with("ssh ")
        || ["git clone", "git fetch", "git pull", "git push"]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
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

#[async_trait]
impl PolicyEngine for SimplePolicyEngine {
    async fn evaluate(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> Result<PolicyVerdict, PolicyError> {
        if cancel.is_cancelled() {
            return Err(PolicyError::Cancelled);
        }

        // 1. Check sandbox profile first (always enforced).
        if let Some((verdict, rule)) = self.check_sandbox(action) {
            self.record_decision(action, &verdict, &rule, cancel.clone()).await;
            return Ok(verdict);
        }

        // 2. Check spend caps and rate limits (if configured).
        if let Some((verdict, rule)) = self.check_spend_and_rate_limits(action).await {
            self.record_decision(action, &verdict, &rule, cancel.clone()).await;
            return Ok(verdict);
        }

        // 3. Check configured policy rules.
        match self.evaluate_rules(action) {
            Some((verdict, rule)) => {
                self.record_decision(action, &verdict, &rule, cancel.clone()).await;
                Ok(verdict)
            }
            None => {
                self.record_decision(action, &PolicyVerdict::Deny, "default_deny", cancel.clone())
                    .await;
                Ok(PolicyVerdict::Deny)
            }
        }
    }

    fn audit_log(&self) -> &dyn AuditLog {
        self.audit.as_ref()
    }
}

// ---- Phase 8: Multi-level spend tracker ------------------------------------

/// Tracks spend across session, task, and daily boundaries.
/// Thread-safe via internal `RwLock`.
pub struct SpendTracker {
    inner: std::sync::RwLock<SpendTrackerInner>,
}

struct SpendTrackerInner {
    session_spend_usd: f64,
    task_spend_usd: f64,
    daily_spend_usd: f64,
    last_reset_date: time::Date,
    session_cap_usd: Option<f64>,
    task_cap_usd: Option<f64>,
    daily_cap_usd: Option<f64>,
}

impl SpendTracker {
    pub fn new(
        session_cap_usd: Option<f64>,
        task_cap_usd: Option<f64>,
        daily_cap_usd: Option<f64>,
    ) -> Self {
        Self {
            inner: std::sync::RwLock::new(SpendTrackerInner {
                session_spend_usd: 0.0,
                task_spend_usd: 0.0,
                daily_spend_usd: 0.0,
                last_reset_date: time::OffsetDateTime::now_utc().date(),
                session_cap_usd,
                task_cap_usd,
                daily_cap_usd,
            }),
        }
    }

    /// Check whether adding `cost_usd` would exceed any cap without
    /// changing the tracked totals.
    pub fn check(&self, cost_usd: f64) -> Result<(), PolicyError> {
        Self::validate_cost(cost_usd)?;
        let mut inner = self
            .inner
            .write()
            .map_err(|e| PolicyError::RuleViolation(format!("spend tracker lock poisoned: {e}")))?;
        Self::reset_daily_if_needed(&mut inner);
        Self::check_caps(&inner, cost_usd)
    }

    /// Atomically reserve spend after checking all configured caps.
    ///
    /// Use this as a concurrency reservation and call
    /// [`Self::settle_reservation`] when the provider reports actual cost.
    pub fn check_and_add(&self, cost_usd: f64) -> Result<(), PolicyError> {
        Self::validate_cost(cost_usd)?;
        let mut inner = self
            .inner
            .write()
            .map_err(|e| PolicyError::RuleViolation(format!("spend tracker lock poisoned: {e}")))?;
        Self::reset_daily_if_needed(&mut inner);
        Self::check_caps(&inner, cost_usd)?;

        inner.session_spend_usd += cost_usd;
        inner.task_spend_usd += cost_usd;
        inner.daily_spend_usd += cost_usd;
        Ok(())
    }

    /// Replace an earlier atomic reservation with the provider-reported cost.
    ///
    /// This keeps concurrent dispatch from authorizing each sibling against
    /// the same unreserved balance. Actual spend is retained even when it is
    /// higher than the estimate.
    pub fn settle_reservation(&self, reserved_usd: f64, actual_usd: f64) {
        if Self::validate_cost(reserved_usd).is_err() || Self::validate_cost(actual_usd).is_err() {
            return;
        }
        if let Ok(mut inner) = self.inner.write() {
            Self::reset_daily_if_needed(&mut inner);
            inner.session_spend_usd =
                (inner.session_spend_usd - reserved_usd).max(0.0) + actual_usd;
            inner.task_spend_usd = (inner.task_spend_usd - reserved_usd).max(0.0) + actual_usd;
            inner.daily_spend_usd = (inner.daily_spend_usd - reserved_usd).max(0.0) + actual_usd;
        }
    }

    /// Record the actual cost of a completed provider call.
    ///
    /// Actual spend is retained even when it crosses a cap so subsequent
    /// preflight checks correctly deny more work.
    pub fn record(&self, cost_usd: f64) {
        if Self::validate_cost(cost_usd).is_err() {
            return;
        }
        if let Ok(mut inner) = self.inner.write() {
            Self::reset_daily_if_needed(&mut inner);
            inner.session_spend_usd += cost_usd;
            inner.task_spend_usd += cost_usd;
            inner.daily_spend_usd += cost_usd;
        }
    }

    fn validate_cost(cost_usd: f64) -> Result<(), PolicyError> {
        if cost_usd.is_finite() && cost_usd >= 0.0 {
            Ok(())
        } else {
            Err(PolicyError::RuleViolation(format!("invalid spend amount: {cost_usd}")))
        }
    }

    fn reset_daily_if_needed(inner: &mut SpendTrackerInner) {
        let today = time::OffsetDateTime::now_utc().date();
        if today != inner.last_reset_date {
            inner.daily_spend_usd = 0.0;
            inner.last_reset_date = today;
        }
    }

    fn check_caps(inner: &SpendTrackerInner, cost_usd: f64) -> Result<(), PolicyError> {
        let new_session = inner.session_spend_usd + cost_usd;
        let new_task = inner.task_spend_usd + cost_usd;
        let new_daily = inner.daily_spend_usd + cost_usd;

        if let Some(cap) = inner.session_cap_usd {
            if new_session > cap {
                return Err(PolicyError::RuleViolation(format!(
                    "session spend cap ${cap:.2} exceeded (current=${new_session:.2})"
                )));
            }
        }
        if let Some(cap) = inner.task_cap_usd {
            if new_task > cap {
                return Err(PolicyError::RuleViolation(format!(
                    "task spend cap ${cap:.2} exceeded (current=${new_task:.2})"
                )));
            }
        }
        if let Some(cap) = inner.daily_cap_usd {
            if new_daily > cap {
                return Err(PolicyError::RuleViolation(format!(
                    "daily spend cap ${cap:.2} exceeded (current=${new_daily:.2})"
                )));
            }
        }
        Ok(())
    }

    /// Get current session total.
    pub fn session_total(&self) -> f64 {
        self.inner.read().map(|i| i.session_spend_usd).unwrap_or(0.0)
    }

    /// Get current daily total.
    pub fn daily_total(&self) -> f64 {
        self.inner.read().map(|i| i.daily_spend_usd).unwrap_or(0.0)
    }

    /// Get the session cap, if set.
    pub fn session_cap(&self) -> Option<f64> {
        self.inner.read().map(|i| i.session_cap_usd).unwrap_or(None)
    }

    /// Reset the task-level counter (call at task boundary).
    pub fn reset_task(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.task_spend_usd = 0.0;
        }
    }

    /// Reset the session-level counter (call at session boundary).
    pub fn reset_session(&self) {
        if let Ok(mut inner) = self.inner.write() {
            inner.session_spend_usd = 0.0;
            inner.task_spend_usd = 0.0;
        }
    }
}

impl Default for SpendTracker {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

// ---- Phase 8: Per-provider rate limiter ------------------------------------

/// Token-bucket rate limiter keyed by provider name.
/// Thread-safe via internal `RwLock`.
#[derive(Debug)]
pub struct RpmLimiter {
    buckets: std::sync::RwLock<HashMap<String, RateBucket>>,
    default_rpm: u64,
}

#[derive(Debug)]
struct RateBucket {
    /// Tokens remaining in the current window.
    remaining: u64,
    /// Unix timestamp (seconds) when the window started.
    window_start: i64,
    /// Max requests per minute.
    limit: u64,
}

impl RpmLimiter {
    pub fn new(default_rpm: u64) -> Self {
        Self { buckets: std::sync::RwLock::new(HashMap::new()), default_rpm }
    }

    /// Set a custom RPM limit for a specific provider.
    pub fn set_limit(&self, provider: &str, rpm: u64) {
        if let Ok(mut buckets) = self.buckets.write() {
            buckets.insert(
                provider.to_string(),
                RateBucket {
                    remaining: rpm,
                    window_start: time::OffsetDateTime::now_utc().unix_timestamp(),
                    limit: rpm,
                },
            );
        }
    }

    /// Try to consume one token for the given provider.
    /// Returns `Ok(true)` if allowed, `Err(PolicyError::RuleViolation)` if
    /// rate limited (the caller must deny the operation).
    pub fn check(&self, provider: &str) -> Result<bool, PolicyError> {
        let mut buckets = self
            .buckets
            .write()
            .map_err(|e| PolicyError::RuleViolation(format!("rate limiter lock poisoned: {e}")))?;

        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let bucket = buckets.entry(provider.to_string()).or_insert(RateBucket {
            remaining: self.default_rpm,
            window_start: now,
            limit: self.default_rpm,
        });

        // Reset window if more than 60 seconds have passed.
        if now - bucket.window_start >= 60 {
            bucket.remaining = bucket.limit;
            bucket.window_start = now;
        }

        if bucket.remaining == 0 {
            return Err(PolicyError::RuleViolation(format!(
                "rate limit exceeded for {provider}: {rpm} RPM",
                rpm = bucket.limit
            )));
        }

        bucket.remaining -= 1;
        Ok(true)
    }

    /// Wait until one provider-request slot is available.
    pub async fn acquire(
        &self,
        provider: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PolicyError> {
        loop {
            match self.check(provider) {
                Ok(_) => return Ok(()),
                Err(PolicyError::RuleViolation(message))
                    if message.starts_with("rate limit exceeded") =>
                {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        _ = cancel.cancelled() => return Err(PolicyError::Cancelled),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

// ---- Time-based approval ----------------------------------------------------

/// A time window condition for auto-approval of low-cost operations
/// outside business hours.
#[derive(Debug, Clone)]
pub struct TimeWindowCondition {
    /// Start hour (0-23, inclusive).
    pub start_hour: u8,
    /// End hour (0-23, inclusive).
    pub end_hour: u8,
    /// IANA timezone name, e.g. "America/New_York".
    pub timezone: String,
    /// Operations with estimated cost below this threshold are auto-approved
    /// when the current time is outside the configured window.
    pub auto_approve_below_usd: f64,
}

impl TimeWindowCondition {
    /// Returns `true` if the current time is **outside** the configured
    /// window (i.e., it is after-hours or on a weekend/holiday by default).
    /// When outside the window, operations below `auto_approve_below_usd`
    /// are automatically approved without human intervention.
    pub fn is_outside_window(&self) -> bool {
        // Single source of truth for "now". `chrono` is required anyway for the
        // configured-timezone conversion below, so use it for the UTC fallback
        // too instead of mixing in a second time crate.
        let now = chrono::Utc::now();

        // Convert to the configured timezone.
        let hour = match self.timezone.parse::<chrono_tz::Tz>() {
            Ok(tz) => {
                let local_time = now.with_timezone(&tz);
                local_time.hour() as u8
            }
            Err(_) => {
                // Fallback to UTC if timezone is invalid (should be caught at config load).
                now.hour() as u8
            }
        };

        // If start <= end: window is [start, end] (same day), both ends
        // inclusive (e.g. [9, 17] means 09:00–17:00 inclusive).
        // If start > end: window wraps midnight, e.g. [22, 6] means
        //   22:00–06:00 next day, both ends inclusive (hour 22 and hour 6
        //   are inside the window, not outside).
        if self.start_hour <= self.end_hour {
            hour < self.start_hour || hour > self.end_hour
        } else {
            // Outside the wrap window means strictly between end and start.
            hour > self.end_hour && hour < self.start_hour
        }
    }

    /// Returns `true` if the operation should be auto-approved based on
    /// estimated cost and current time.
    pub fn should_auto_approve(&self, estimated_cost_usd: f64) -> bool {
        self.is_outside_window() && estimated_cost_usd < self.auto_approve_below_usd
    }
}

pub(crate) fn compute_input_hash(input: &serde_json::Value) -> String {
    // `serde_json::Value` always serializes, so this only falls back on a
    // poisoned/cyclic value; hash a sentinel instead of empty bytes to avoid
    // an all-zero collision for the (unreachable) failure case.
    let json_bytes =
        serde_json::to_vec(input).unwrap_or_else(|_| b"<unserializable input>".to_vec());
    let hash = blake3::hash(&json_bytes);
    hash.to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{
        RULE_CONSEQUENTIAL, RULE_INTENT_AUTHORIZED, RULE_INTENT_READONLY_DENY, RULE_OBSERVE,
        RULE_SHELL_REQUIRES_APPROVAL, RULE_UN_GRANTED,
    };
    use crate::ids::Ulid;
    use crate::policy_presets::inject_intent_gate_rule;
    use crate::types::{
        CapabilitySet, CommandPolicyFacts, DestructiveClass, FilesystemScope, SandboxProfile,
    };
    use std::path::PathBuf;

    struct TestAuditLog {
        entries: std::sync::Mutex<Vec<AuditEntry>>,
    }

    #[async_trait]
    impl AuditLog for TestAuditLog {
        async fn record(
            &self,
            entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            self.entries.lock().unwrap().push(entry);
            Ok(())
        }
    }

    fn empty_input() -> serde_json::Value {
        serde_json::json!({})
    }

    fn make_action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
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

    #[tokio::test]
    async fn auto_approve_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[tokio::test]
    async fn auto_deny_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoDeny(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn require_approval_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::RequireApproval(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
    }

    #[tokio::test]
    async fn first_rule_wins() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![
            PolicyRule::AutoDeny(Condition::ToolName("shell".into())),
            PolicyRule::AutoApprove(Condition::ToolName("shell".into())),
        ];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn tool_scoped_operation_requires_tool_and_operation() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::All(vec![
            Condition::ToolName("filesystem".into()),
            Condition::Operation("write".into()),
        ]))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let write = serde_json::json!({ "operation": "write", "path": "src/main.rs" });
        let read = serde_json::json!({ "operation": "read", "path": "src/main.rs" });

        let filesystem_write = engine
            .evaluate(&make_action("filesystem", &write), CancellationToken::new())
            .await
            .unwrap();
        let filesystem_read = engine
            .evaluate(&make_action("filesystem", &read), CancellationToken::new())
            .await
            .unwrap();
        let unrelated_tool =
            engine.evaluate(&make_action("git", &write), CancellationToken::new()).await.unwrap();

        assert_eq!(filesystem_write, PolicyVerdict::Allow);
        assert_eq!(filesystem_read, PolicyVerdict::Deny);
        assert_eq!(unrelated_tool, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn default_deny_when_no_rule_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("filesystem".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn cancellation_returns_error() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = engine.evaluate(&make_action("shell", &input), cancel).await;
        assert!(matches!(result, Err(PolicyError::Cancelled)));
    }

    #[test]
    fn condition_all_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::All(vec![Condition::ToolName("shell".into()), Condition::Always]);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_any_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::Any(vec![
            Condition::ToolName("filesystem".into()),
            Condition::ToolName("shell".into()),
        ]);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_not_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::Not(Box::new(Condition::ToolName("filesystem".into())));
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    // ---- Phase 0: CodeType / SecretPattern / Capability condition tests ----

    #[test]
    fn condition_code_type_matches_path() {
        let input = serde_json::json!({"path": "src/tests/test_auth.rs"});
        let action = make_action("filesystem", &input);
        let cond = Condition::CodeType(CodeCategory::Test);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_code_type_matches_file_path_field() {
        // Some tools use "file_path" instead of "path".
        let input = serde_json::json!({"file_path": "src/auth/login.rs"});
        let action = make_action("filesystem", &input);
        let cond = Condition::CodeType(CodeCategory::Auth);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_code_type_returns_false_for_wrong_category() {
        let input = serde_json::json!({"path": "src/auth/login.rs"});
        let action = make_action("filesystem", &input);
        // Requesting CodeCategory::Test on an auth file should not match.
        let cond = Condition::CodeType(CodeCategory::Test);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_code_type_returns_false_when_no_path_present() {
        let input = serde_json::json!({"command": "echo hello"});
        let action = make_action("shell", &input);
        let cond = Condition::CodeType(CodeCategory::Other);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_secret_pattern_matches_content() {
        // SecretPattern regexes must be pre-compiled from rules
        // before eval_cond can use them.
        let pattern = String::from(r"(?i)api[_-]?key");
        let rules = vec![PolicyRule::AutoDeny(Condition::SecretPattern(pattern.clone()))];
        let input = serde_json::json!({
            "path": "src/config.rs",
            "content": "const API_KEY: &str = \"sk-1234567890\";"
        });
        let action = make_action("filesystem", &input);
        let cond = Condition::SecretPattern(pattern);
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_secret_pattern_matches_path() {
        // If content does not match, fallback: check the path.
        let pattern = String::from("secrets");
        let rules = vec![PolicyRule::AutoDeny(Condition::SecretPattern(pattern.clone()))];
        let input = serde_json::json!({
            "path": "/secrets/prod.env",
            "content": "DB_HOST=localhost"
        });
        let action = make_action("filesystem", &input);
        let cond = Condition::SecretPattern(pattern);
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_secret_pattern_does_not_match_benign_content() {
        let pattern = String::from(r"(?i)(api[_-]?key|password|secret)");
        let rules = vec![PolicyRule::AutoDeny(Condition::SecretPattern(pattern.clone()))];
        let input = serde_json::json!({
            "path": "src/utils.rs",
            "content": "pub fn add(a: i32, b: i32) -> i32 { a + b }"
        });
        let action = make_action("filesystem", &input);
        let cond = Condition::SecretPattern(pattern);
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_capability_matches_when_action_has_required_cap() {
        // Rule requires filesystem(write=true).
        // Action has filesystem(write=true) and filesystem(read=true).
        // Action's capabilities are a superset of rule's → condition matches.
        let input = empty_input();
        let rule_caps = CapabilitySet::default().with_requirement("filesystem(write=true)");
        let action_caps = CapabilitySet::default()
            .with_requirement("filesystem(write=true)")
            .with_requirement("filesystem(read=true)");
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: action_caps,
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let cond = Condition::Capability(rule_caps);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_capability_does_not_match_when_action_lacks_cap() {
        // Rule requires filesystem(write=true).
        // Action only has filesystem(read=true).
        // Action's capabilities are NOT a superset → condition does NOT match.
        let input = empty_input();
        let rule_caps = CapabilitySet::default().with_requirement("filesystem(write=true)");
        let action_caps = CapabilitySet::default().with_requirement("filesystem(read=true)");
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: action_caps,
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let cond = Condition::Capability(rule_caps);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_capability_does_not_match_empty_action() {
        // Rule requires a capability but action has none (default empty set).
        let input = empty_input();
        let rule_caps = CapabilitySet::default().with_requirement("filesystem(write=true)");
        let action = make_action("filesystem", &input);
        let cond = Condition::Capability(rule_caps);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_capability_both_empty_still_matches() {
        // Rule requires nothing (empty set). Action has nothing.
        // Empty set is a superset of empty set → condition matches.
        let input = empty_input();
        let rule_caps = CapabilitySet::default();
        let action = make_action("shell", &input);
        let cond = Condition::Capability(rule_caps);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn glob_matches_literal_path() {
        assert!(glob_match("/home/user/file.rs", "/home/user/file.rs"));
    }

    #[test]
    fn glob_matches_wildcard() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.ts"));
    }

    #[test]
    fn glob_matches_directory_wildcard() {
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/mod.rs"));
        assert!(!glob_match("src/**/*.rs", "lib/main.rs"));
    }

    #[test]
    fn glob_matches_single_char() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
    }

    #[test]
    fn glob_rejects_contains_semantics() {
        // "rs" pattern should not match "workspace/config"
        assert!(!glob_match("*.rs", "project/workspace"));
    }

    #[test]
    fn compiled_command_pattern_cached() {
        let rules = vec![PolicyRule::AutoApprove(Condition::CommandPattern("^git ".into()))];
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(rules, audit);
        assert_eq!(engine.compiled.len(), 1);
        assert!(engine.compiled.contains_key("^git "));
    }

    #[tokio::test]
    async fn command_pattern_matches_command_with_arguments() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::CommandPattern(
                "^cargo check --workspace$".into(),
            ))],
            audit,
        );
        let input = serde_json::json!({
            "command": "cargo",
            "args": ["check", "--workspace"]
        });
        let verdict = engine
            .evaluate(&make_action("shell", &input), CancellationToken::new())
            .await
            .expect("policy evaluation");
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[tokio::test]
    async fn command_pattern_matches_interpreter_and_script() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::CommandPattern(
                r"^/bin/bash -c cargo test$".into(),
            ))],
            audit,
        );
        let input = serde_json::json!({
            "command": "/bin/bash",
            "args": ["-c", "cargo test"]
        });
        let verdict = engine
            .evaluate(&make_action("shell", &input), CancellationToken::new())
            .await
            .expect("policy evaluation");
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn precompile_invalid_regex_does_not_panic() {
        let rules = vec![PolicyRule::AutoApprove(Condition::CommandPattern("[invalid".into()))];
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(rules, audit);
        // Invalid regex is silently skipped — matching will return false.
        assert!(engine.compiled.is_empty());
    }

    // ---- ADR-28 §6: structured command-fact conditions ---------------------

    fn facts_action<'a>(
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

    fn sample_facts() -> CommandPolicyFacts {
        CommandPolicyFacts {
            shell_profile_id: Some("managed-bash".into()),
            resolved_executable: Some(PathBuf::from("/usr/bin/bash")),
            argv: vec!["bash".into(), "-c".into(), "echo hi".into()],
            working_directory: Some(PathBuf::from("/home/user/project")),
            network_requested: false,
            filesystem_scope: FilesystemScope::ProjectOnly,
            destructive_classification: DestructiveClass::NonDestructive,
        }
    }

    #[test]
    fn condition_shell_profile_matches_same_id() {
        let input = empty_input();
        let action = facts_action(&input, sample_facts());
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        assert!(engine.eval_cond(&Condition::ShellProfile("managed-bash".into()), &action));
        assert!(!engine.eval_cond(&Condition::ShellProfile("other".into()), &action));
    }

    #[test]
    fn condition_shell_profile_never_matches_when_absent() {
        let input = empty_input();
        let mut facts = sample_facts();
        facts.shell_profile_id = None;
        let action = facts_action(&input, facts);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        assert!(!engine.eval_cond(&Condition::ShellProfile("managed-bash".into()), &action));
    }

    #[test]
    fn condition_resolved_executable_matches_regex() {
        let input = empty_input();
        let action = facts_action(&input, sample_facts());
        // The pattern must be present in a rule so it is precompiled into the
        // engine's `compiled` map (mirrors how CommandPattern/SecretPattern work).
        let rules = vec![PolicyRule::AutoDeny(Condition::ResolvedExecutable("^/usr/bin/".into()))];
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        assert!(engine.eval_cond(&Condition::ResolvedExecutable("^/usr/bin/".into()), &action));
        assert!(!engine.eval_cond(&Condition::ResolvedExecutable("^/bin/".into()), &action));
    }

    #[test]
    fn condition_argv_pattern_matches_joined_argv() {
        let input = empty_input();
        let action = facts_action(&input, sample_facts());
        let rules = vec![PolicyRule::AutoDeny(Condition::ArgvPattern("bash -c".into()))];
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        // argv is joined with spaces: "bash -c echo hi".
        assert!(engine.eval_cond(&Condition::ArgvPattern("bash -c".into()), &action));
        assert!(!engine.eval_cond(&Condition::ArgvPattern("rm -rf".into()), &action));
    }

    #[test]
    fn condition_working_dir_matches_glob() {
        let input = empty_input();
        let action = facts_action(&input, sample_facts());
        let rules = vec![PolicyRule::AutoDeny(Condition::WorkingDir("/home/user/**".into()))];
        let engine = SimplePolicyEngine::new(
            rules,
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        assert!(engine.eval_cond(&Condition::WorkingDir("/home/user/**".into()), &action));
        assert!(!engine.eval_cond(&Condition::WorkingDir("/tmp/**".into()), &action));
    }

    #[test]
    fn structured_facts_unset_means_conditions_do_not_match() {
        let input = empty_input();
        let action = make_action("shell", &input); // command_facts: None
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );

        assert!(!engine.eval_cond(&Condition::ShellProfile("managed-bash".into()), &action));
        assert!(!engine.eval_cond(&Condition::ArgvPattern(".*".into()), &action));
        assert!(!engine.eval_cond(&Condition::WorkingDir("**".into()), &action));
    }

    // ---- Sandbox profiles (stub — all non-None profiles deny) ----------------

    #[tokio::test]
    async fn sandbox_none_profile_allows_with_auto_approve() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("filesystem".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"action": "read", "path": "/tmp/test.txt"});
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[tokio::test]
    async fn sandbox_read_only_fs_denies_as_stub() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("filesystem".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"action": "read", "path": "/tmp/test.txt"});
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: Some(SandboxProfile::ReadOnlyFs),
            estimated_cost_usd: None,
            command_facts: None,
        };
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn sandbox_network_isolated_denies_as_stub() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"command": "echo hello"});
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: Some(SandboxProfile::NetworkIsolated),
            estimated_cost_usd: None,
            command_facts: None,
        };
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn sandbox_containerized_denies_as_stub() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("filesystem".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &serde_json::json!({}),
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: Some(SandboxProfile::Containerized),
            estimated_cost_usd: None,
            command_facts: None,
        };
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    // ---- validate(): rule-shape validation ----------------------------------

    #[test]
    fn validate_rejects_invalid_regex_rule() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::CommandPattern("[invalid".into()))],
            audit,
        );
        assert!(engine.validate().is_err());
    }

    #[test]
    fn validate_accepts_well_formed_rules() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![
            PolicyRule::AutoApprove(Condition::ToolName("filesystem".into())),
            PolicyRule::RequireApproval(Condition::PathGlob("src/**/*.rs".into())),
        ];
        let engine = SimplePolicyEngine::new(rules, audit);
        assert!(engine.validate().is_ok());
    }

    // ---- SpendTracker tests -------------------------------------------------

    #[tokio::test]
    async fn spend_tracker_allows_under_cap() {
        let tracker = SpendTracker::new(Some(1.0), Some(0.5), Some(5.0));
        assert!(tracker.check_and_add(0.3).is_ok());
    }

    #[tokio::test]
    async fn spend_tracker_denies_over_session_cap() {
        let tracker = SpendTracker::new(Some(1.0), None, None);
        assert!(tracker.check_and_add(0.6).is_ok());
        assert!(tracker.check_and_add(0.5).is_err());
    }

    #[tokio::test]
    async fn spend_tracker_resets_task_boundary() {
        let tracker = SpendTracker::new(None, Some(0.5), None);
        assert!(tracker.check_and_add(0.4).is_ok());
        assert!(tracker.check_and_add(0.2).is_err());
        tracker.reset_task();
        assert!(tracker.check_and_add(0.3).is_ok());
    }

    #[tokio::test]
    async fn spend_tracker_session_total_and_cap() {
        let tracker = SpendTracker::new(Some(2.0), None, None);
        assert_eq!(tracker.session_cap(), Some(2.0));
        assert_eq!(tracker.session_total(), 0.0);
        assert!(tracker.check(0.75).is_ok());
        assert_eq!(tracker.session_total(), 0.0);
        tracker.record(0.75);
        assert!((tracker.session_total() - 0.75).abs() < f64::EPSILON);
        assert!((tracker.daily_total() - 0.75).abs() < f64::EPSILON);
        tracker.record(1.5);
        assert!((tracker.session_total() - 2.25).abs() < f64::EPSILON);
        assert!(tracker.check(0.01).is_err());
    }

    #[test]
    fn spend_tracker_reconciles_concurrent_reservation() {
        let tracker = SpendTracker::new(Some(1.0), None, None);
        assert!(tracker.check_and_add(0.6).is_ok());
        assert!(tracker.check_and_add(0.6).is_err());
        tracker.settle_reservation(0.6, 0.25);
        assert!((tracker.session_total() - 0.25).abs() < f64::EPSILON);
        assert!(tracker.check_and_add(0.6).is_ok());
    }

    // ---- Phase 8: RpmLimiter tests ------------------------------------------

    #[tokio::test]
    async fn rate_limiter_allows_within_limit() {
        let limiter = RpmLimiter::new(5);
        for _ in 0..5 {
            assert!(limiter.check("anthropic").is_ok());
        }
    }

    #[tokio::test]
    async fn rate_limiter_denies_over_limit() {
        let limiter = RpmLimiter::new(3);
        for _ in 0..3 {
            assert!(limiter.check("openai").is_ok());
        }
        assert!(limiter.check("openai").is_err());
    }

    #[tokio::test]
    async fn rate_limiter_separate_buckets() {
        let limiter = RpmLimiter::new(2);
        assert!(limiter.check("anthropic").is_ok());
        assert!(limiter.check("anthropic").is_ok());
        assert!(limiter.check("anthropic").is_err());
        // Different provider is not affected.
        assert!(limiter.check("openai").is_ok());
    }

    // ---- Phase 8: TimeWindowCondition tests ---------------------------------

    #[test]
    fn time_window_outside_returns_true() {
        // This test may be time-sensitive. The window [0, 6] UTC means
        // 00:00-06:00. If run at 14:00 UTC, is_outside_window == true.
        let cond = TimeWindowCondition {
            start_hour: 0,
            end_hour: 6,
            timezone: "UTC".into(),
            auto_approve_below_usd: 0.05,
        };
        // At any hour outside 0-6 (like noon), this should be true.
        let now_hour = time::OffsetDateTime::now_utc().hour();
        if !(0..=6).contains(&now_hour) {
            assert!(cond.is_outside_window());
        }
    }

    #[test]
    fn time_window_inside_returns_false() {
        let cond = TimeWindowCondition {
            start_hour: 0,
            end_hour: 23,
            timezone: "UTC".into(),
            auto_approve_below_usd: 0.05,
        };
        assert!(!cond.is_outside_window());
    }

    #[test]
    fn time_window_auto_approves_low_cost_outside() {
        let cond = TimeWindowCondition {
            start_hour: 9,
            end_hour: 17,
            timezone: "UTC".into(),
            auto_approve_below_usd: 0.10,
        };
        // Use a known-outside hour: midnight UTC.
        // We can't mock time easily, but we can test the logic direction.
        let now_hour = time::OffsetDateTime::now_utc().hour();
        let is_outside = !(9..=17).contains(&now_hour);
        assert_eq!(
            cond.should_auto_approve(0.05),
            is_outside,
            "low cost should be auto-approved outside business hours"
        );
        assert!(!cond.should_auto_approve(0.50), "high cost should never be auto-approved");
    }

    #[test]
    fn time_window_midnight_wrap() {
        // Window [22, 6] means 22:00 – 06:00 next day (overnight).
        let cond = TimeWindowCondition {
            start_hour: 22,
            end_hour: 6,
            timezone: "UTC".into(),
            auto_approve_below_usd: 0.01,
        };
        let now_hour = time::OffsetDateTime::now_utc().hour();
        // Inside the overnight window: 22 <= hour or hour <= 6
        let is_inside = now_hour >= 22 || now_hour <= 6;
        assert_eq!(!cond.is_outside_window(), is_inside, "midnight wrapping window logic");
    }

    // ---- Phase 8: Integration tests for SpendTracker and RpmLimiter --------

    #[tokio::test]
    async fn evaluate_denies_over_spend_cap() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let tracker = Arc::new(SpendTracker::new(Some(1.0), None, None));
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("provider".into()))];
        let engine = SimplePolicyEngine::new(rules, audit).with_spend_tracker(tracker.clone());

        let input = serde_json::json!({"provider": "openai"});
        let action = PolicyAction {
            tool_name: "provider",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: Some(0.6),
            command_facts: None,
        };

        // Preflight succeeds without charging the estimate.
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
        assert_eq!(tracker.session_total(), 0.0);

        // Record the provider's actual cost, then deny the next estimate.
        tracker.record(0.6);
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn evaluate_denies_rate_limited_provider() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let limiter = Arc::new(RpmLimiter::new(3));
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("provider".into()))];
        let engine = SimplePolicyEngine::new(rules, audit).with_rate_limiter(limiter);

        let input = serde_json::json!({"provider": "openai"});
        let action = PolicyAction {
            tool_name: "provider",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };

        // First 3 calls should succeed.
        for _ in 0..3 {
            let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
            assert_eq!(verdict, PolicyVerdict::Allow);
        }

        // 4th call should be denied.
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn evaluate_without_tracker_unaffected() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);

        let input = serde_json::json!({});
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };

        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn time_window_respects_timezone() {
        // Test that different timezones can produce different results.
        // Window [9, 17] means business hours 9am-5pm.
        let utc_cond = TimeWindowCondition {
            start_hour: 9,
            end_hour: 17,
            timezone: "UTC".into(),
            auto_approve_below_usd: 0.10,
        };

        let ny_cond = TimeWindowCondition {
            start_hour: 9,
            end_hour: 17,
            timezone: "America/New_York".into(),
            auto_approve_below_usd: 0.10,
        };

        // Get current time in both timezones
        let now_utc = chrono::Utc::now();
        let utc_hour = now_utc.hour();
        let ny_hour = now_utc.with_timezone(&chrono_tz::America::New_York).hour();

        // Verify that the timezone conversion is working
        // (the actual test is that we can get different hours from different timezones)
        if utc_hour != ny_hour {
            // If UTC and NY hours differ, the window results should differ
            // for at least one of the conditions
            let utc_result = utc_cond.is_outside_window();
            let ny_result = ny_cond.is_outside_window();

            // At least one should be different if the hours differ
            // (unless both are inside or both are outside the window)
            if (9..=17).contains(&utc_hour) && !(9..=17).contains(&ny_hour) {
                assert!(!utc_result, "UTC should be inside window");
                assert!(ny_result, "NY should be outside window");
            } else if !(9..=17).contains(&utc_hour) && (9..=17).contains(&ny_hour) {
                assert!(utc_result, "UTC should be outside window");
                assert!(!ny_result, "NY should be inside window");
            }
        }
    }

    #[test]
    fn time_window_invalid_timezone_falls_back_to_utc() {
        // Invalid timezone should fall back to UTC
        let cond = TimeWindowCondition {
            start_hour: 0,
            end_hour: 23,
            timezone: "Invalid/Timezone".into(),
            auto_approve_below_usd: 0.05,
        };

        // Should not panic, should fall back to UTC
        let result = cond.is_outside_window();
        let utc_hour = time::OffsetDateTime::now_utc().hour();
        let expected = !(0..=23).contains(&utc_hour);
        assert_eq!(result, expected, "Invalid timezone should fall back to UTC");
    }

    // ---- ADR-28: managed-toolchain governance rules ------------------------

    #[tokio::test]
    async fn require_managed_tool_approval_matches_shell() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules =
            vec![PolicyRule::RequireManagedToolApproval(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit.clone());
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("require_managed_tool_approval")
        );
    }

    #[tokio::test]
    async fn deny_network_egress_only_blocks_egress() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::DenyNetworkEgress(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit.clone());

        // Network command → deny from the egress rule specifically.
        let net = engine
            .evaluate(
                &make_action("shell", &serde_json::json!({"command": "curl https://example.com"})),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(net, PolicyVerdict::Deny);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("deny_network_egress")
        );

        // Local command → rule does not apply; falls through to default deny.
        let local = engine
            .evaluate(
                &make_action("shell", &serde_json::json!({"command": "cargo build"})),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(local, PolicyVerdict::Deny);
        assert_eq!(audit.entries.lock().unwrap()[1].rule_matched.as_deref(), Some("default_deny"));
    }

    #[tokio::test]
    async fn deny_role_prompt_rule_blocks_shell() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::All(vec![
            Condition::ToolName("filesystem".into()),
            Condition::Operation("read".into()),
        ]))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let read_input = serde_json::json!({"operation": "read", "path": "src/main.rs"});
        let write_input = serde_json::json!({"operation": "write", "path": "src/main.rs"});
        let read_v = engine
            .evaluate(&make_action("filesystem", &read_input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(read_v, PolicyVerdict::Allow);
        let write_v = engine
            .evaluate(&make_action("filesystem", &write_input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(write_v, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn condition_command_pattern_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoDeny(Condition::CommandPattern("rm -rf".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"command": "rm -rf /tmp"});
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
    }

    #[tokio::test]
    async fn condition_git_operation_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::GitOperation("push".into()))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"operation": "push"});
        let verdict =
            engine.evaluate(&make_action("git", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[tokio::test]
    async fn require_approval_with_timeout_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::RequireApprovalWithTimeout {
            condition: Condition::ToolName("shell".into()),
            timeout_secs: 120,
        }];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApprovalWithTimeout {
                timeout: std::time::Duration::from_secs(120)
            }
        );
    }

    #[tokio::test]
    async fn require_toolchain_approval_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::RequireToolchainApproval(Condition::ToolName("shell".into()))];
        let engine = SimplePolicyEngine::new(rules, audit.clone());
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("require_toolchain_approval")
        );
    }

    #[test]
    fn condition_always_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&Condition::Always, &action));
    }

    #[test]
    fn condition_never_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        // Not(Always) should never match
        assert!(!engine.eval_cond(&Condition::Not(Box::new(Condition::Always)), &action));
    }

    #[tokio::test]
    async fn matched_rule_logged_to_audit() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoDeny(Condition::Always)];
        let engine = SimplePolicyEngine::new(rules, audit.clone());
        let _ =
            engine.evaluate(&make_action("shell", &empty_input()), CancellationToken::new()).await;
        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].rule_matched.is_some());
    }

    #[tokio::test]
    async fn multiple_rules_first_match_wins() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![
            PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            PolicyRule::AutoApprove(Condition::ToolName("shell".into())),
        ];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        // First rule (RequireApproval) should win, not AutoApprove.
        assert!(matches!(verdict, PolicyVerdict::RequireApproval { .. }));
    }

    #[tokio::test]
    async fn auto_approve_with_tool_name_and_operation_matches() {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::All(vec![
            Condition::ToolName("filesystem".into()),
            Condition::Operation("write".into()),
        ]))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = serde_json::json!({"operation": "write", "path": "test.txt"});
        let verdict = engine
            .evaluate(&make_action("filesystem", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn condition_tool_name_does_not_match_different_tool() {
        let input = empty_input();
        let action = make_action("git", &input);
        let cond = Condition::ToolName("filesystem".into());
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_operation_does_not_match_without_operation_field() {
        let input = empty_input();
        let action = make_action("filesystem", &input);
        let cond = Condition::Operation("write".into());
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_any_with_single_element_matches() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::Any(vec![Condition::ToolName("shell".into())]);
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_not_wrapping_always_returns_false() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::Not(Box::new(Condition::Always));
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    // ---- ADR-43 §6: ToolNamePrefix condition tests ----

    #[test]
    fn condition_tool_name_prefix_matches_tools_with_prefix() {
        let input = empty_input();
        let action = make_action("mcp:github:list_repos", &input);
        let cond = Condition::ToolNamePrefix("mcp:".into());
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_tool_name_prefix_is_prefix_not_equality() {
        // The condition is `starts_with`, so a longer prefix must still match
        // and an exact-named tool must not match a shorter prefix.
        let input = empty_input();
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        let broad = Condition::ToolNamePrefix("mcp:".into());
        let narrow = Condition::ToolNamePrefix("mcp:github:".into());
        assert!(engine.eval_cond(&broad, &make_action("mcp:github:list_repos", &input)));
        assert!(engine.eval_cond(&narrow, &make_action("mcp:github:list_repos", &input)));
        // The "github" prefix must not match a different server namespace.
        assert!(!engine.eval_cond(&narrow, &make_action("mcp:filesystem:read", &input)));
    }

    #[test]
    fn condition_tool_name_prefix_does_not_match_other_tools() {
        let input = empty_input();
        let action = make_action("shell", &input);
        let cond = Condition::ToolNamePrefix("mcp:".into());
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!engine.eval_cond(&cond, &action));
    }

    #[test]
    fn condition_tool_name_prefix_does_not_match_partial_segment() {
        // A plain tool named "mcp" or one that merely *contains* the prefix
        // later in the string must not match.
        let input = empty_input();
        let engine = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        let cond = Condition::ToolNamePrefix("mcp:".into());
        assert!(!engine.eval_cond(&cond, &make_action("mcp", &input)));
        assert!(!engine.eval_cond(&cond, &make_action("my_mcp_server:read", &input)));
    }

    #[tokio::test]
    async fn first_match_wins_allow_specific_server_before_deny_all_mcp() {
        // Pins the ADR-43 §6 semantics: with a narrower allow rule placed
        // BEFORE a broad `mcp:*` deny rule, first-match-wins means the allow
        // wins for that server while all other MCP tools are denied.
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![
            PolicyRule::AutoApprove(Condition::ToolNamePrefix("mcp:github:".into())),
            PolicyRule::AutoDeny(Condition::ToolNamePrefix("mcp:".into())),
        ];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();

        let github = engine
            .evaluate(&make_action("mcp:github:list_repos", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            github,
            PolicyVerdict::Allow,
            "earlier allow rule must win over the deny prefix"
        );

        let filesystem = engine
            .evaluate(&make_action("mcp:filesystem:read", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(filesystem, PolicyVerdict::Deny, "unmatched server falls through to the deny");

        // Reversed order flips the outcome: the deny now wins.
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![
            PolicyRule::AutoDeny(Condition::ToolNamePrefix("mcp:".into())),
            PolicyRule::AutoApprove(Condition::ToolNamePrefix("mcp:github:".into())),
        ];
        let engine = SimplePolicyEngine::new(rules, audit);
        let github = engine
            .evaluate(&make_action("mcp:github:list_repos", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(github, PolicyVerdict::Deny, "deny-before-allow must win");
    }

    #[tokio::test]
    async fn prefix_rule_applies_inside_all_and_not() {
        // Prefix conditions compose with the logical combinators like any
        // other condition.
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let rules = vec![PolicyRule::AutoApprove(Condition::All(vec![
            Condition::ToolNamePrefix("mcp:".into()),
            Condition::Not(Box::new(Condition::ToolNamePrefix("mcp:github:".into()))),
        ]))];
        let engine = SimplePolicyEngine::new(rules, audit);
        let input = empty_input();

        let allowed = engine
            .evaluate(&make_action("mcp:filesystem:read", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(allowed, PolicyVerdict::Allow);

        let denied = engine
            .evaluate(&make_action("mcp:github:list_repos", &input), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(denied, PolicyVerdict::Deny);
    }

    // ---- ADR-55 1c: intent-gate (`Condition::IntentAuthorized`) ------------

    /// Stub authorization provider returning a fixed verdict for every action.
    #[derive(Clone, Copy)]
    struct StubIntentAuth(IntentVerdict);

    impl IntentAuthorization for StubIntentAuth {
        fn verdict(&self, _action: &PolicyAction<'_>) -> IntentVerdict {
            self.0
        }
    }

    fn engine_with_auth(
        rules: Vec<PolicyRule>,
        verdict: IntentVerdict,
    ) -> (SimplePolicyEngine, Arc<TestAuditLog>) {
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(rules, audit.clone())
            .with_intent_auth(Arc::new(StubIntentAuth(verdict)));
        (engine, audit)
    }

    #[tokio::test]
    async fn gate_outcome_matrix_maps_each_verdict_mechanically() {
        // The gate maps each authorization verdict to the rule's normal policy
        // outcome and records the verdict's rule name as the audit
        // `rule_matched`. Every 1c verdict is pinned as a regression row
        // (ADR-55 §2).
        let cases: &[(IntentVerdict, PolicyVerdict, &str)] = &[
            (IntentVerdict::Allow { rule: RULE_OBSERVE }, PolicyVerdict::Allow, RULE_OBSERVE),
            (
                IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
                PolicyVerdict::Allow,
                RULE_INTENT_AUTHORIZED,
            ),
            (
                IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
                PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) },
                RULE_CONSEQUENTIAL,
            ),
            (
                IntentVerdict::RequireApproval { rule: RULE_SHELL_REQUIRES_APPROVAL },
                PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) },
                RULE_SHELL_REQUIRES_APPROVAL,
            ),
            (
                IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED },
                PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) },
                RULE_UN_GRANTED,
            ),
            (
                IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
                PolicyVerdict::Deny,
                RULE_INTENT_READONLY_DENY,
            ),
        ];
        for (verdict, expected, rule) in cases {
            let (engine, audit) = engine_with_auth(
                vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
                *verdict,
            );
            let actual = engine
                .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(&actual, expected, "verdict {verdict:?} maps to {expected:?}");
            assert_eq!(audit.entries.lock().unwrap()[0].rule_matched.as_deref(), Some(*rule));
        }
    }

    #[tokio::test]
    async fn deny_wins_over_authorization() {
        // ADR-55 hard requirement: an auto-deny matches first (the danger
        // patterns always precede the approval rules in the presets) and its
        // deny is final — the intent gate never gets to upgrade it.
        let (engine, audit) = engine_with_auth(
            vec![
                PolicyRule::AutoDeny(Condition::ToolName("shell".into())),
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
            ],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert_eq!(audit.entries.lock().unwrap()[0].rule_matched.as_deref(), Some("auto_deny"));
    }

    #[tokio::test]
    async fn consequential_requires_approval_even_when_authorized() {
        // Consequential-tier actions are never covered by a grant (ADR-55 §2):
        // the gate keeps them under RequireApproval and labels the audit row
        // "consequential".
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_CONSEQUENTIAL)
        );
    }

    #[tokio::test]
    async fn authorized_mutate_local_allows_without_approval_and_sets_rule_matched() {
        // An Allow verdict upgrades RequireApproval → Allow and records the
        // verdict's rule ("intent_authorized") in the verdict audit row.
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let verdict = engine
            .evaluate(&make_action("filesystem", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_INTENT_AUTHORIZED)
        );
    }

    #[tokio::test]
    async fn ungranted_mutation_stays_under_approval_with_rule() {
        // An un-granted mutation is still decided by the gate: it keeps the
        // rule's approval path and carries its own rule name rather than
        // silently falling through to a broader rule.
        let (engine, audit) = engine_with_auth(
            vec![
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
                PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            ],
            IntentVerdict::RequireApproval { rule: RULE_UN_GRANTED },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(audit.entries.lock().unwrap()[0].rule_matched.as_deref(), Some(RULE_UN_GRANTED));
    }

    #[tokio::test]
    async fn readonly_intent_deny_is_final_and_audited() {
        // A read-only-intent denial is final and pre-sink: it never becomes an
        // approval request, so even session auto-approve cannot bypass it.
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
        );
        let verdict = engine
            .evaluate(&make_action("filesystem", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_INTENT_READONLY_DENY)
        );
    }

    #[tokio::test]
    async fn deny_network_egress_precedes_intent_gate() {
        // Deny rules evaluated before the gate stay structurally final even
        // when the authorization would allow the action.
        let (engine, audit) = engine_with_auth(
            vec![
                PolicyRule::DenyNetworkEgress(Condition::ToolName("http".into())),
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
            ],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let verdict = engine
            .evaluate(&make_action("http", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("deny_network_egress")
        );
    }

    #[tokio::test]
    async fn sandbox_deny_precedes_intent_gate() {
        // A non-`None` sandbox profile is rejected before rule evaluation, so
        // an authorization grant cannot bypass it.
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let input = empty_input();
        let action = PolicyAction {
            tool_name: "shell",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: Some(SandboxProfile::ReadOnlyFs),
            estimated_cost_usd: None,
            command_facts: None,
        };
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("sandbox_profiles_not_implemented")
        );
    }

    #[tokio::test]
    async fn spend_cap_deny_precedes_intent_gate() {
        // The spend cap is enforced before rule evaluation; a gate grant never
        // bypasses it.
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let tracker = Arc::new(SpendTracker::new(Some(1.0), None, None));
        let engine = SimplePolicyEngine::new(
            vec![PolicyRule::RequireApproval(Condition::IntentAuthorized)],
            audit.clone(),
        )
        .with_spend_tracker(tracker.clone())
        .with_intent_auth(Arc::new(StubIntentAuth(IntentVerdict::Allow {
            rule: RULE_INTENT_AUTHORIZED,
        })));
        let input = serde_json::json!({});
        let action = PolicyAction {
            tool_name: "http",
            input: &input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: Some(0.6),
            command_facts: None,
        };
        tracker.record(0.6);
        let verdict = engine.evaluate(&action, CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert!(audit.entries.lock().unwrap()[0]
            .rule_matched
            .as_deref()
            .is_some_and(|rule| rule.starts_with("spend_cap_exceeded")));
    }

    #[tokio::test]
    async fn no_auth_attached_gate_falls_through_like_today() {
        // Without a provider, IntentAuthorized behaves exactly as before
        // ADR-55: the rule never matches and the walk continues.
        let audit = Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) });
        let engine = SimplePolicyEngine::new(
            vec![
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
                PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            ],
            audit.clone(),
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("require_approval")
        );
    }

    #[tokio::test]
    async fn gate_applies_verdicts_with_rules_own_timeout() {
        // The gate applies to every approval-producing rule kind, using the
        // rule's own timeout for RequireApproval verdicts.
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApprovalWithTimeout {
                condition: Condition::IntentAuthorized,
                timeout_secs: 120,
            }],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(verdict, PolicyVerdict::Allow);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_INTENT_AUTHORIZED)
        );

        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApprovalWithTimeout {
                condition: Condition::IntentAuthorized,
                timeout_secs: 120,
            }],
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(120) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_CONSEQUENTIAL)
        );
    }

    #[tokio::test]
    async fn gate_applies_to_managed_and_toolchain_approval_rules() {
        // Managed-toolchain governance rules (ADR-28) are still routed through
        // the intent gate when they carry the bare IntentAuthorized condition.
        for (rule, expected_timed_out) in [
            (
                PolicyRule::RequireManagedToolApproval(Condition::IntentAuthorized),
                std::time::Duration::from_secs(30),
            ),
            (
                PolicyRule::RequireToolchainApproval(Condition::IntentAuthorized),
                std::time::Duration::from_secs(30),
            ),
        ] {
            let (engine, audit) = engine_with_auth(
                vec![rule],
                IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            );
            let verdict = engine
                .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(verdict, PolicyVerdict::RequireApproval { timeout: expected_timed_out });
            assert_eq!(
                audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
                Some(RULE_CONSEQUENTIAL)
            );
        }
    }

    #[tokio::test]
    async fn compound_intent_condition_never_upgrades() {
        // A condition that *contains* IntentAuthorized through a combinator is
        // a plain boolean predicate: it can match, but never upgrade
        // RequireApproval → Allow.
        let (engine, audit) = engine_with_auth(
            vec![PolicyRule::RequireApproval(Condition::All(vec![
                Condition::IntentAuthorized,
                Condition::ToolName("shell".into()),
            ]))],
            IntentVerdict::Allow { rule: RULE_INTENT_AUTHORIZED },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        // The compound matches (Allow is a true predicate), but the outcome is
        // still the rule's RequireApproval — no upgrade.
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("require_approval")
        );

        // A Deny predicate is false, so the compound rule does not fire and
        // the walk continues to the next shell rule.
        let (engine, audit) = engine_with_auth(
            vec![
                PolicyRule::RequireApproval(Condition::All(vec![
                    Condition::IntentAuthorized,
                    Condition::ToolName("shell".into()),
                ])),
                PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            ],
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
        );
        let verdict = engine
            .evaluate(&make_action("shell", &empty_input()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::RequireApproval { timeout: std::time::Duration::from_secs(30) }
        );
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some("require_approval")
        );
    }

    #[test]
    fn eval_cond_intent_authorized_matches_only_when_allowed() {
        // As a plain predicate (combinators, AutoApprove, ...) the condition
        // is true only for an Allow verdict.
        let input = empty_input();
        let action = make_action("shell", &input);

        let allowed = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        )
        .with_intent_auth(Arc::new(StubIntentAuth(IntentVerdict::Allow { rule: RULE_OBSERVE })));
        assert!(allowed.eval_cond(&Condition::IntentAuthorized, &action));

        for verdict in [
            IntentVerdict::RequireApproval { rule: RULE_CONSEQUENTIAL },
            IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY },
        ] {
            let denied = SimplePolicyEngine::new(
                vec![],
                Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
            )
            .with_intent_auth(Arc::new(StubIntentAuth(verdict)));
            assert!(!denied.eval_cond(&Condition::IntentAuthorized, &action));
        }

        let unattached = SimplePolicyEngine::new(
            vec![],
            Arc::new(TestAuditLog { entries: std::sync::Mutex::new(Vec::new()) }),
        );
        assert!(!unattached.eval_cond(&Condition::IntentAuthorized, &action));
    }

    #[tokio::test]
    async fn injected_gate_rule_fires_readonly_deny_under_custom_rules() {
        // B-3 ties to B-1: custom user rules replace default_rules() wholesale,
        // so the bare IntentAuthorized gate rule is injected back in. With a
        // read-only-intent authorization the shell MutateLocal action is then a
        // final, pre-sink Deny — not an Allow and not a RequireApproval.
        let rules = inject_intent_gate_rule(vec![PolicyRule::RequireApproval(
            Condition::ToolName("shell".into()),
        )]);
        assert_eq!(rules[0], PolicyRule::RequireApproval(Condition::IntentAuthorized));
        let (engine, audit) =
            engine_with_auth(rules, IntentVerdict::Deny { rule: RULE_INTENT_READONLY_DENY });
        let input = serde_json::json!({"command": "touch", "args": ["x"]});
        let verdict =
            engine.evaluate(&make_action("shell", &input), CancellationToken::new()).await.unwrap();
        assert_eq!(verdict, PolicyVerdict::Deny);
        assert_eq!(
            audit.entries.lock().unwrap()[0].rule_matched.as_deref(),
            Some(RULE_INTENT_READONLY_DENY)
        );
    }
}
