//! `AgentCostEstimator` — rough cost estimation per agent stage kind.
//!
//! Delegates to the single config-derived estimator
//! ([`concerto_providers::routing::CostEstimator`]), which keys typical token
//! usage on the **stage kind** the role is staffed in (Planning → 4_000,
//! Research → 6_000, Execution → 8_000, Review → 3_000, Acceptance → 500) and
//! falls back to a flat 2_000-token default for an unknown/unstaffed role or a
//! facade-less estimate. Never keyed on the role's name.

use concerto_config::BlueprintFacade;
use concerto_core::types::{AgentId, RoutingProfile};

/// Rough cost estimator for agent runs.
pub struct AgentCostEstimator;

impl AgentCostEstimator {
    /// Estimate cost for running a given agent on the given profile.
    ///
    /// `facade` supplies the role's blueprint stage kind for the heuristic
    /// (ADR-58 R13); pass `None` for a facade-less flat-default estimate.
    pub fn estimate(
        role: &AgentId,
        profile: &RoutingProfile,
        facade: Option<&BlueprintFacade>,
    ) -> f64 {
        concerto_providers::routing::CostEstimator::estimate(role, profile, facade)
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
    fn estimation_is_keyed_on_stage_kind_not_role_name() {
        // ADR-58 R13: the token estimate comes from the role's STAFFED STAGE
        // KIND, so architect (Planning) prices below coder (Execution) on the
        // default blueprint facade.
        let resolved = concerto_config::OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);
        let profile = expensive_profile();
        let architect_cost =
            AgentCostEstimator::estimate(&AgentId::new("architect"), &profile, Some(&facade));
        let coder_cost =
            AgentCostEstimator::estimate(&AgentId::new("coder"), &profile, Some(&facade));
        assert!(coder_cost > architect_cost);
    }

    #[test]
    fn estimates_follow_the_resolved_blueprint_stage_kinds() {
        // ADR-58 R13 parity pin: with the default standard blueprint's facade
        // the five builtin specialists price by their staffed stage kind
        // (architect→Planning, researcher→Research, coder→Execution,
        // reviewer→Review, validator→Acceptance). The coordinator persona is
        // not staffed in a stage, so it takes the flat 2_000 default.
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
                "standard blueprint must price {role} by stage kind"
            );
        }
    }

    #[test]
    fn facade_less_estimates_use_the_flat_default() {
        // No facade → no stage kind → flat 2_000-token default for every role
        // (never a role-name special case).
        let profile = cheap_profile();
        let expected = (2_000.0 / 1000.0) * profile.cost_per_1k_tokens;
        for role in ["architect", "researcher", "coder", "reviewer", "validator", "coordinator"] {
            let estimated = AgentCostEstimator::estimate(&AgentId::new(role), &profile, None);
            assert_eq!(estimated, expected, "facade-less {role} must use the flat default");
        }
    }
}
