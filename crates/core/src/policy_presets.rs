use crate::types::{CodeCategory, Condition, PolicyRule};

/// Ensure a user-supplied policy rule list still carries the ADR-55 intent
/// gate (`RequireApproval(Condition::IntentAuthorized)`).
///
/// Custom user rules replace [`PolicyPresets::default_rules`] wholesale, which
/// would drop the bare gate rule and leave the gate inert — still prompting
/// and auditing but never deciding (B-3). This helper re-establishes it:
///
/// - If the list already contains a rule whose condition references
///   `Condition::IntentAuthorized` (bare or through a combinator), the list is
///   returned unchanged — no duplicate gate.
/// - Otherwise the gate rule is inserted immediately after the leading run of
///   deny-class rules (`AutoDeny` / `DenyNetworkEgress`) so the engine's
///   first-match-wins deny ordering is preserved and the gate still sits
///   before any approval/auto-approve rules.
/// - An empty list is returned unchanged: there is no deny run ahead of the
///   gate, and in practice an empty user list never reaches this helper (the
///   caller falls back to [`PolicyPresets::default_rules`], which already
///   ships the gate).
pub fn inject_intent_gate_rule(rules: Vec<PolicyRule>) -> Vec<PolicyRule> {
    if rules.is_empty() {
        return rules;
    }
    let already_present =
        rules.iter().filter_map(policy_rule_condition).any(condition_references_intent_authorized);
    if already_present {
        return rules;
    }
    let mut insert_at = 0;
    while insert_at < rules.len() && is_deny_class_rule(&rules[insert_at]) {
        insert_at += 1;
    }
    let mut injected = rules;
    injected.insert(insert_at, PolicyRule::RequireApproval(Condition::IntentAuthorized));
    injected
}

/// The condition a rule is built on, if it carries one.
fn policy_rule_condition(rule: &PolicyRule) -> Option<&Condition> {
    match rule {
        PolicyRule::AutoApprove(cond)
        | PolicyRule::AutoDeny(cond)
        | PolicyRule::RequireApproval(cond)
        | PolicyRule::RequireManagedToolApproval(cond)
        | PolicyRule::RequireToolchainApproval(cond)
        | PolicyRule::DenyNetworkEgress(cond) => Some(cond),
        PolicyRule::RequireApprovalWithTimeout { condition, .. } => Some(condition),
    }
}

/// True when `cond` is the bare `Condition::IntentAuthorized` or contains it
/// through a combinator (`All`/`Any`/`Not`). Only the bare form fires the gate
/// in the engine, but a reference anywhere means the rule set already addresses
/// the gate and must not be re-injected.
fn condition_references_intent_authorized(cond: &Condition) -> bool {
    match cond {
        Condition::IntentAuthorized => true,
        Condition::Not(inner) => condition_references_intent_authorized(inner),
        Condition::All(conds) | Condition::Any(conds) => {
            conds.iter().any(condition_references_intent_authorized)
        }
        _ => false,
    }
}

/// True when `rule` is a deny-class rule that must precede the gate rule for
/// first-match-wins ordering (the engine maps `AutoDeny` and `DenyNetworkEgress`
/// to a final `PolicyVerdict::Deny`).
fn is_deny_class_rule(rule: &PolicyRule) -> bool {
    matches!(rule, PolicyRule::AutoDeny(_) | PolicyRule::DenyNetworkEgress(_))
}

/// Centralised default policy rules shared by both `SimplePolicyEngine`
/// construction and the config layer.
pub struct PolicyPresets;

impl PolicyPresets {
    /// Default rules shipped with concerto on a fresh install.
    ///
    /// The policy engine is first-match-wins, so absolute denials must precede
    /// the broader shell approval rule.  File inspection is allowed without
    /// prompting; mutations and commands require explicit approval.  A catch-all
    /// ensures that any unrecognised or future tool also requires approval
    /// rather than being silently denied or approved.
    pub fn default_rules() -> Vec<PolicyRule> {
        vec![
            // Auto-deny dangerous shell commands — must come first so the
            // broader shell rule below can never accidentally approve them.
            PolicyRule::AutoDeny(Condition::Any(vec![
                Condition::CommandPattern(r"rm\s+(-rf\s+)?/".into()),
                Condition::CommandPattern(r"dd\s+if=".into()),
                Condition::CommandPattern(r"mkfs".into()),
                Condition::CommandPattern(r":\(\)\{\s*:\|:&\s*\};:".into()),
            ])),
            // ADR-55 §2: the intent gate. A run-scoped authorization provider
            // (when attached) decides the action on this bare condition:
            // Allow upgrades `RequireApproval` → `Allow`, RequireApproval keeps
            // the action under approval, and Deny is final. With no provider
            // attached the rule never matches and the pre-ADR-55 defaults
            // below apply unchanged. Must sit immediately after the mandatory
            // denials (so it can never upgrade them) and before the shell /
            // network / catch-all approval rules (so its deny and observe
            // outcomes take precedence), with the read-only inspection
            // AutoApprove preserved ahead of it.
            PolicyRule::RequireApproval(Condition::IntentAuthorized),
            // Read-only project inspection is safe enough to keep normal
            // agent reasoning fluid without prompting on every lookup.
            PolicyRule::AutoApprove(Condition::All(vec![
                Condition::ToolName("filesystem".into()),
                Condition::Any(vec![
                    Condition::Operation("read".into()),
                    Condition::Operation("list".into()),
                    Condition::Operation("exists".into()),
                ]),
            ])),
            // Everything else is interactive by default. The final catch-all
            // is important for newly installed plugin/custom tools: they do
            // not acquire silent authority, but they also do not become
            // unusable merely because the preset predates their tool name.
            PolicyRule::RequireApproval(Condition::ToolName("filesystem".into())),
            PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            PolicyRule::RequireApproval(Condition::ToolName("git".into())),
            PolicyRule::RequireApproval(Condition::Always),
        ]
    }

    /// Strict preset: require approval for every tool call.
    pub fn strict() -> Vec<PolicyRule> {
        vec![PolicyRule::RequireApproval(Condition::Always)]
    }

    /// Semantic Trust Domain preset: treat test code as low-risk.
    /// Auto-approves file writes when the target path is classified as
    /// test code (path contains `tests/`, `*_test.rs`, etc.).
    pub fn tests_are_safe() -> Vec<PolicyRule> {
        vec![
            PolicyRule::AutoApprove(Condition::CodeType(CodeCategory::Test)),
            // Falls back to default behavior for non-test code.
        ]
    }

    /// Semantic Trust Domain preset: treat auth/session code as critical.
    /// Requires approval for any operation targeting auth-related paths.
    pub fn auth_is_critical() -> Vec<PolicyRule> {
        vec![PolicyRule::RequireApproval(Condition::CodeType(CodeCategory::Auth))]
    }

    /// Security preset: auto-deny operations whose content matches
    /// common secret/credential patterns. Catches hardcoded API keys,
    /// passwords, tokens, and credentials in diffs.
    pub fn no_secrets() -> Vec<PolicyRule> {
        vec![PolicyRule::AutoDeny(Condition::SecretPattern(
            r"(?i)(api[_-]?key|password|secret|token|credential|auth[_-]?token)".into(),
        ))]
    }

    /// Permissive preset: auto-approve all known tools, deny only
    /// hardcoded danger patterns. Useful for trusted local dev.
    pub fn permissive() -> Vec<PolicyRule> {
        vec![
            PolicyRule::AutoDeny(Condition::Any(vec![
                Condition::CommandPattern(r"rm\s+(-rf\s+)?/".into()),
                Condition::CommandPattern(r"dd\s+if=".into()),
                Condition::CommandPattern(r"mkfs".into()),
                Condition::CommandPattern(r":\(\)\{\s*:\|:&\s*\};:".into()),
            ])),
            PolicyRule::AutoApprove(Condition::ToolName("filesystem".into())),
            PolicyRule::AutoApprove(Condition::ToolName("shell".into())),
            PolicyRule::AutoApprove(Condition::ToolName("git".into())),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rules_have_correct_structure() {
        let rules = PolicyPresets::default_rules();
        assert!(!rules.is_empty());
        // AutoDeny (danger patterns) must come first (first-match-wins engine).
        assert!(matches!(rules.first(), Some(PolicyRule::AutoDeny(_))));
        // Should have approval rules for filesystem, shell, git.
        assert!(rules.iter().any(|rule| matches!(
            rule,
            PolicyRule::RequireApproval(Condition::ToolName(tool)) if tool == "filesystem"
        )));
        assert!(rules.iter().any(|rule| matches!(
            rule,
            PolicyRule::RequireApproval(Condition::ToolName(tool)) if tool == "shell"
        )));
        assert!(rules.iter().any(|rule| matches!(
            rule,
            PolicyRule::RequireApproval(Condition::ToolName(tool)) if tool == "git"
        )));
        // Catch-all is the last rule.
        assert!(matches!(rules.last(), Some(PolicyRule::RequireApproval(Condition::Always))));
    }

    #[test]
    fn default_rules_place_one_intent_gate_after_denials_before_approvals() {
        let rules = PolicyPresets::default_rules();
        // Exactly one bare IntentAuthorized gate rule.
        assert_eq!(
            rules
                .iter()
                .filter(|r| matches!(r, PolicyRule::RequireApproval(Condition::IntentAuthorized)))
                .count(),
            1,
            "the default rule set must ship exactly one bare IntentAuthorized rule"
        );
        let gate_pos = rules
            .iter()
            .position(|r| matches!(r, PolicyRule::RequireApproval(Condition::IntentAuthorized)))
            .expect("gate rule must exist");
        // Immediately after the AutoDeny danger family (first-match-wins).
        if gate_pos == 0 {
            panic!("the IntentAuthorized gate rule must come after the AutoDeny rule, not first");
        }
        assert!(
            matches!(&rules[gate_pos - 1], PolicyRule::AutoDeny(_)),
            "gate must sit immediately after the AutoDeny danger family"
        );
        // Before the shell/network/catch-all approval rules.
        let shell_pos = rules
            .iter()
            .position(|r| {
                matches!(r, PolicyRule::RequireApproval(Condition::ToolName(tool)) if tool == "shell")
            })
            .expect("shell approval rule must exist");
        assert!(gate_pos < shell_pos, "gate rule must precede the shell RequireApproval rule");
    }

    #[test]
    fn strict_rules_require_approval() {
        let rules = PolicyPresets::strict();
        assert_eq!(rules.len(), 1);
        assert!(matches!(rules.first(), Some(PolicyRule::RequireApproval(Condition::Always))));
    }

    #[test]
    fn permissive_rules_auto_approve() {
        let rules = PolicyPresets::permissive();
        let auto_approve_count =
            rules.iter().filter(|r| matches!(r, PolicyRule::AutoApprove(_))).count();
        assert_eq!(auto_approve_count, 3);
    }

    #[test]
    fn tests_are_safe_uses_code_type_condition() {
        let rules = PolicyPresets::tests_are_safe();
        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| matches!(
            r,
            PolicyRule::AutoApprove(Condition::CodeType(CodeCategory::Test))
        )));
    }

    #[test]
    fn auth_is_critical_uses_code_type_condition() {
        let rules = PolicyPresets::auth_is_critical();
        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| matches!(
            r,
            PolicyRule::RequireApproval(Condition::CodeType(CodeCategory::Auth))
        )));
    }

    #[test]
    fn no_secrets_uses_secret_pattern_condition() {
        let rules = PolicyPresets::no_secrets();
        assert!(!rules.is_empty());
        assert!(rules
            .iter()
            .any(|r| matches!(r, PolicyRule::AutoDeny(Condition::SecretPattern(_)))));
    }

    #[test]
    fn no_secrets_pattern_matches_common_secrets() {
        // Verify the no-secrets regex matches common credential patterns.
        let rules = PolicyPresets::no_secrets();
        for rule in &rules {
            if let PolicyRule::AutoDeny(Condition::SecretPattern(pattern)) = rule {
                let re = regex::Regex::new(pattern).expect("valid regex");
                assert!(re.is_match("api_key = \"sk-1234\""), "should match api_key");
                assert!(re.is_match("password = \"hunter2\""), "should match password");
                assert!(re.is_match("secret = \"my-secret\""), "should match secret");
                assert!(re.is_match("auth_token = \"abc\""), "should match auth_token");
                // False positive check: harmless words should not match
                assert!(!re.is_match("const x = 42;"), "should not match arbitrary code");
            }
        }
    }

    // ---- inject_intent_gate_rule (B-3) ------------------------------------

    #[test]
    fn injection_adds_gate_to_custom_rules_after_denies() {
        // A single user RequireApproval(shell) rule replacing default_rules()
        // would drop the gate; injection restores it ahead of the shell rule.
        let injected = inject_intent_gate_rule(vec![PolicyRule::RequireApproval(
            Condition::ToolName("shell".into()),
        )]);
        assert_eq!(
            injected,
            vec![
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
                PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            ],
            "the gate rule must be injected before the custom approval rule"
        );
    }

    #[test]
    fn injection_preserves_demoted_deny_run_at_head() {
        // Deny-class rules keep their first-match-wins position: the gate is
        // inserted after the leading AutoDeny/DenyNetworkEgress run.
        let injected = inject_intent_gate_rule(vec![
            PolicyRule::AutoDeny(Condition::ToolName("shell".into())),
            PolicyRule::AutoDeny(Condition::ToolName("http".into())),
            PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
        ]);
        assert_eq!(
            injected,
            vec![
                PolicyRule::AutoDeny(Condition::ToolName("shell".into())),
                PolicyRule::AutoDeny(Condition::ToolName("http".into())),
                PolicyRule::RequireApproval(Condition::IntentAuthorized),
                PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
            ],
            "deny-first ordering must be preserved, gate right after the denies"
        );
    }

    #[test]
    fn injection_leaves_default_rules_unchanged() {
        // default_rules() already ships the gate; injection must not duplicate
        // it (the returned list is byte-for-byte the input).
        let defaults = PolicyPresets::default_rules();
        let injected = inject_intent_gate_rule(defaults.clone());
        assert_eq!(injected, defaults, "defaults already carry the gate — no duplicate");
    }

    #[test]
    fn injection_leaves_existing_bare_gate_rule_unchanged() {
        let rules = vec![
            PolicyRule::AutoDeny(Condition::ToolName("shell".into())),
            PolicyRule::RequireApproval(Condition::IntentAuthorized),
            PolicyRule::RequireApproval(Condition::ToolName("shell".into())),
        ];
        assert_eq!(inject_intent_gate_rule(rules.clone()), rules);
    }

    #[test]
    fn injection_skips_when_gate_referenced_through_combinator() {
        // A rule whose condition references IntentAuthorized through a
        // combinator still counts as "the rule set addresses the gate".
        let rules = vec![PolicyRule::RequireApproval(Condition::All(vec![
            Condition::IntentAuthorized,
            Condition::ToolName("shell".into()),
        ]))];
        assert_eq!(inject_intent_gate_rule(rules.clone()), rules);
    }

    #[test]
    fn injection_leaves_empty_list_unchanged() {
        // An empty list needs no injection — and in practice never reaches the
        // helper (the caller falls back to default_rules(), which has the gate).
        let injected = inject_intent_gate_rule(Vec::new());
        assert!(injected.is_empty());
    }
}
