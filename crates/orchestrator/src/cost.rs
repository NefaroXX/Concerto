//! `AgentCostEstimator` — rough cost estimation per agent stage kind.
//!
//! Uses tunable constants per stage kind (ADR-58 D2). These are conservative
//! estimates of typical token usage for a single agent run of that stage.

use concerto_config::blueprint::StageKind;
use concerto_config::BlueprintFacade;
use concerto_core::types::{AgentId, RoutingProfile};

/// Rough cost estimator for agent runs.
pub struct AgentCostEstimator;

impl AgentCostEstimator {
    /// Estimated typical token usage per agent (input + output).
    ///
    /// ADR-58 R13: keyed by the **stage kind** the role is staffed in (from
    /// the resolved blueprint facade) rather than the role's name, so a
    /// renamed or custom stage-staffed specialist prices like its stage does
    /// (Planning → 4_000, Research → 6_000, Execution → 8_000, Review →
    /// 3_000, Acceptance → 500). The stage's open kind string is parsed into
    /// the closed [`StageKind`] vocabulary; an unknown kind string (and an
    /// absent kind) falls back to the legacy role-name table, so the default
    /// `standard` blueprint prices byte-identically to pre-ADR-58.
    fn typical_tokens(role: &AgentId, kind: Option<String>) -> u64 {
        match kind.as_deref().and_then(StageKind::parse) {
            Some(StageKind::Planning) => 4_000,
            Some(StageKind::Research) => 6_000,
            Some(StageKind::Execution) => 8_000,
            Some(StageKind::Review) => 3_000,
            Some(StageKind::Acceptance) => 500, // no LLM call
            // Role fallthrough (unknown kind strings, custom/freeform,
            // `coordinator`, facade-less estimates): the pre-ADR-58 role-name
            // table, byte-identical on the default blueprint. `RunOnce`
            // stages and genuinely unknown roles also land here.
            _ => match role.as_str() {
                "architect" => 4_000,
                "researcher" => 6_000,
                "coder" => 8_000,
                "reviewer" => 3_000,
                "validator" => 500, // no LLM call
                "coordinator" => 2_000,
                _ => 2_000,
            },
        }
    }

    /// Estimate cost for running a given agent on the given profile.
    ///
    /// `facade` supplies the role's blueprint stage kind for the heuristic
    /// (R13); pass `None` for a facade-less estimate to keep the legacy
    /// role-name table.
    pub fn estimate(
        role: &AgentId,
        profile: &RoutingProfile,
        facade: Option<&BlueprintFacade>,
    ) -> f64 {
        let kind =
            facade.and_then(|facade| facade.stage_for_agent(role)).map(|s| s.def.kind.clone());
        let tokens = Self::typical_tokens(role, kind) as f64;
        (tokens / 1000.0) * profile.cost_per_1k_tokens
    }

    /// Returns true if `budget` is sufficient for a single run of `role`
    /// at the cheapest available profile.
    pub fn budget_sufficient(
        role: &AgentId,
        budget: f64,
        profiles: &[RoutingProfile],
        facade: Option<&BlueprintFacade>,
    ) -> bool {
        profiles
            .iter()
            .map(|p| Self::estimate(role, p, facade))
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|min| budget >= min)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap_profile() -> RoutingProfile {
        RoutingProfile {
            provider_config_id: "groq".into(),
            provider: "groq".into(),
            model: "llama-3-8b".into(),
            cost_per_1k_tokens: 0.0001,
            avg_latency_ms: 100,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        }
    }

    fn expensive_profile() -> RoutingProfile {
        RoutingProfile {
            provider_config_id: "anthropic".into(),
            provider: "anthropic".into(),
            model: "claude-opus-4".into(),
            cost_per_1k_tokens: 0.015,
            avg_latency_ms: 2000,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        }
    }

    #[test]
    fn zero_budget_insufficient() {
        let profiles = vec![cheap_profile()];
        assert!(!AgentCostEstimator::budget_sufficient(
            &AgentId::new("coder"),
            0.0,
            &profiles,
            None
        ));
    }

    #[test]
    fn generous_budget_sufficient() {
        let profiles = vec![cheap_profile()];
        assert!(AgentCostEstimator::budget_sufficient(
            &AgentId::new("coder"),
            10.0,
            &profiles,
            None
        ));
    }

    #[test]
    fn estimation_matches_role() {
        let profile = expensive_profile();
        let architect_cost =
            AgentCostEstimator::estimate(&AgentId::new("architect"), &profile, None);
        let coder_cost = AgentCostEstimator::estimate(&AgentId::new("coder"), &profile, None);
        assert!(coder_cost > architect_cost);
    }

    #[test]
    fn default_blueprint_estimates_match_legacy_role_table() {
        // ADR-58 R13 parity pin: with the default standard blueprint's
        // facade, the five builtin specialists (and the coordinator persona)
        // price byte-identically to the pre-ADR-58 role-name table — both
        // through the stage-kind key and through the facade-less fallthrough.
        let resolved = concerto_config::OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);
        let profile = cheap_profile();

        let expected = |tokens: u64| (tokens as f64 / 1000.0) * profile.cost_per_1k_tokens;
        for (role, tokens) in [
            ("architect", 4_000),
            ("researcher", 6_000),
            ("coder", 8_000),
            ("reviewer", 3_000),
            ("validator", 500),
            ("coordinator", 2_000),
        ] {
            let estimated =
                AgentCostEstimator::estimate(&AgentId::new(role), &profile, Some(&facade));
            assert_eq!(
                estimated,
                expected(tokens),
                "standard blueprint must price {role} byte-identically to the legacy table"
            );
            let fallback = AgentCostEstimator::estimate(&AgentId::new(role), &profile, None);
            assert_eq!(
                fallback,
                expected(tokens),
                "facade-less fallthrough must keep the legacy price for {role}"
            );
        }
    }
}
