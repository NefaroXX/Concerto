//! Explicit, budget-aware model routing per agent.
//!
//! Explicit role assignments are authoritative. When a role has no assignment,
//! the runtime supplies the session's selected provider/model as its pin.

use concerto_config::ModelPinConfig;
use concerto_core::error::OrchestratorError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::types::{AgentId, RoutingProfile, TaskId};
use concerto_sessions::spend::SpendTracker;
use std::sync::Arc;

/// Rough cost estimator for agent runs.
///
/// Uses tunable constants per agent. These are conservative estimates of
/// typical token usage for a single agent run of that agent.
pub struct CostEstimator;

impl CostEstimator {
    /// Estimated typical token usage per agent (input + output).
    fn typical_tokens(role: &AgentId) -> u64 {
        match role.as_str() {
            "architect" => 4_000,
            "researcher" => 6_000,
            "coder" => 8_000,
            "reviewer" => 3_000,
            "validator" => 500,
            "coordinator" => 2_000,
            _ => 2_000,
        }
    }

    /// Estimate cost for running a given agent on the given profile.
    pub fn estimate(role: &AgentId, profile: &RoutingProfile) -> f64 {
        let tokens = Self::typical_tokens(role) as f64;
        normalize_cost((tokens / 1000.0) * profile.cost_per_1k_tokens)
    }
}

/// Normalize a cost value to avoid floating-point edge cases.
fn normalize_cost(cost: f64) -> f64 {
    if cost.is_finite() && cost >= 0.0 {
        cost
    } else {
        0.0
    }
}

/// Explicit, budget-aware model routing engine.
///
/// Selects the best `RoutingProfile` for a given `AgentId` based on:
/// 1. Explicit provider/model assignments (highest priority)
/// 2. Objective compatibility requirements
/// 3. Budget constraints for unassigned fallback selection
pub struct RoutingEngine {
    profiles: Vec<RoutingProfile>,
    spend_tracker: Arc<SpendTracker>,
    pin_config: ModelPinConfig,
    provider_pins: std::collections::HashMap<AgentId, String>,
    /// Roles that require a tool-calling-capable model. `None` keeps the
    /// legacy seed-specialist defaults (researcher, coder, validator);
    /// `Some(set)` is the exact config-derived topology (ADR-35 phase 4).
    tool_calling_roles: Option<std::collections::HashSet<AgentId>>,
    event_bus: EventBus,
}

impl RoutingEngine {
    /// Create a new `RoutingEngine`.
    pub fn new(
        profiles: Vec<RoutingProfile>,
        spend_tracker: Arc<SpendTracker>,
        pin_config: ModelPinConfig,
        event_bus: EventBus,
    ) -> Self {
        Self {
            profiles,
            spend_tracker,
            pin_config,
            provider_pins: std::collections::HashMap::new(),
            tool_calling_roles: None,
            event_bus,
        }
    }

    /// Bind agent model pins to stable provider configuration IDs.
    pub fn with_provider_pins(
        mut self,
        provider_pins: std::collections::HashMap<AgentId, String>,
    ) -> Self {
        self.provider_pins = provider_pins;
        self
    }

    /// Bind the exact set of roles that require tool-calling-capable models.
    /// When unset, the legacy defaults (researcher, coder, validator) apply.
    pub fn with_tool_calling_roles(mut self, roles: std::collections::HashSet<AgentId>) -> Self {
        self.tool_calling_roles = Some(roles);
        self
    }

    /// Select the best routing profile for the given agent.
    ///
    /// # Arguments
    /// * `role` - The agent to select a model for
    /// * `budget_remaining` - Optional remaining budget in USD
    /// * `task_id` - Task ID for event emission
    ///
    /// # Returns
    /// The selected `RoutingProfile` or an error if no affordable model exists.
    pub fn select(
        &self,
        role: &AgentId,
        budget_remaining: Option<f64>,
        task_id: TaskId,
    ) -> Result<RoutingProfile, OrchestratorError> {
        self.select_for_session(role, budget_remaining, task_id, None)
    }

    /// Select a profile while correlating routing visibility with a session.
    pub fn select_for_session(
        &self,
        role: &AgentId,
        budget_remaining: Option<f64>,
        task_id: TaskId,
        session_id: Option<Ulid>,
    ) -> Result<RoutingProfile, OrchestratorError> {
        // Compute effective budget from caller param and spend_tracker caps.
        let effective_budget = self.effective_budget(budget_remaining);

        // Explicit role/session assignment is authoritative.
        if let Some(pinned_model) = self.pin_config.pins.get(role) {
            if let Some(profile) = self.find_pinned_profile(role, pinned_model) {
                if !self.profile_meets_requirements(role, profile) {
                    return Err(OrchestratorError::PinnedModelMissingCapability {
                        role: role.clone(),
                        model: profile.model.clone(),
                        capability: "tool_calling".into(),
                    });
                }
                // Validate pinned model fits budget
                if let Some(budget) = effective_budget {
                    let estimated = CostEstimator::estimate(role, profile);
                    if estimated > budget {
                        return Err(OrchestratorError::PinnedModelBudgetExceeded {
                            model: profile.model.clone(),
                            estimated,
                            remaining: budget,
                        });
                    }
                }
                self.publish_routing_decision(
                    session_id,
                    task_id,
                    EventKind::RoutingDecided {
                        task_id,
                        role: role.clone(),
                        provider: profile.provider.clone(),
                        model: profile.model.clone(),
                        reason: format!("explicit provider/model assignment for {role}"),
                    },
                );
                return Ok(profile.clone());
            }
            return Err(OrchestratorError::PinnedModelNotFound {
                role: role.clone(),
                provider_config_id: self.provider_pins.get(role).cloned(),
                model: pinned_model.clone(),
            });
        }

        let compatible = self.compatible_candidates(role);
        if compatible.is_empty() {
            return Err(OrchestratorError::NoCapableModel {
                role: role.clone(),
                capability: self.required_capability(role).unwrap_or("a configured model").into(),
            });
        }
        let selected = self.select_affordable(&compatible, role, effective_budget)?;

        // 5. Emit routing event
        self.publish_routing_decision(
            session_id,
            task_id,
            EventKind::RoutingDecided {
                task_id,
                role: role.clone(),
                provider: selected.provider.clone(),
                model: selected.model.clone(),
                reason: format!("lowest-cost compatible unassigned model for {role}"),
            },
        );

        Ok(selected.clone())
    }

    fn publish_routing_decision(
        &self,
        session_id: Option<Ulid>,
        task_id: TaskId,
        event: EventKind,
    ) {
        if let Some(session_id) = session_id {
            let _ = self.event_bus.publish_for_session(session_id, task_id.0, event);
        } else {
            let _ = self.event_bus.publish_raw(event);
        }
    }

    /// Handle provider failure by retrying with the next cheaper profile.
    ///
    /// # Arguments
    /// * `role` - The agent
    /// * `current_profile` - The profile that just failed
    /// * `profiles` - All available profiles to choose from
    ///
    /// # Returns
    /// The next cheaper `RoutingProfile` or an error if none exists.
    pub fn retry_or_downgrade(
        &self,
        role: &AgentId,
        current_profile: &RoutingProfile,
        profiles: &[RoutingProfile],
    ) -> Result<RoutingProfile, OrchestratorError> {
        // Find compatible profiles that are cheaper than the current model.
        let mut candidates: Vec<&RoutingProfile> = profiles
            .iter()
            .filter(|profile| self.profile_meets_requirements(role, profile))
            .filter(|p| p.cost_per_1k_tokens < current_profile.cost_per_1k_tokens)
            .collect();

        // Sort by cost (cheapest first)
        candidates.sort_by(|a, b| {
            a.cost_per_1k_tokens
                .partial_cmp(&b.cost_per_1k_tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        candidates
            .first()
            .map(|&p| p.clone())
            .ok_or(OrchestratorError::NoAffordableModel { role: role.clone() })
    }

    /// Resolve the configured global default model for a role.
    ///
    /// Tier 1 of the coordinator fallback ladder: when an agent's pinned or
    /// routed model is exhausted (`LimitReached`), retry the same role on the
    /// configured `default_model`. Deliberately does NOT re-check spend budget —
    /// the ladder runs precisely when budget constraints already blocked the
    /// primary path.
    ///
    /// # Errors
    /// * `NoAffordableModel` when no `default_model` is configured.
    /// * `PinnedModelNotFound` when the configured default is not offered by the
    ///   (optionally pinned) provider config, or does not meet the role's
    ///   capability requirements (e.g. tool_calling).
    pub fn fallback_to_default(&self, role: &AgentId) -> Result<RoutingProfile, OrchestratorError> {
        let model = self
            .pin_config
            .default_model
            .as_deref()
            .ok_or_else(|| OrchestratorError::NoAffordableModel { role: role.clone() })?;
        let provider_id = self.pin_config.default_provider_config_id.as_deref();
        let profile = self
            .profiles
            .iter()
            .find(|p| {
                p.model == model && provider_id.map(|id| p.provider_config_id == id).unwrap_or(true)
            })
            .filter(|p| self.profile_meets_requirements(role, p))
            .cloned();
        profile.ok_or_else(|| OrchestratorError::PinnedModelNotFound {
            role: role.clone(),
            provider_config_id: self.pin_config.default_provider_config_id.clone(),
            model: model.to_string(),
        })
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Find the exact configured provider/model pair pinned for an agent.
    fn find_pinned_profile(&self, role: &AgentId, model: &str) -> Option<&RoutingProfile> {
        let provider_id = self.provider_pins.get(role);
        self.profiles.iter().find(|profile| {
            profile.model == model
                && provider_id.map(|id| profile.provider_config_id == *id).unwrap_or(true)
        })
    }

    fn profile_meets_requirements(&self, role: &AgentId, profile: &RoutingProfile) -> bool {
        match self.required_capability(role) {
            Some("tool_calling") => profile.supports_tool_calling,
            _ => true,
        }
    }

    fn required_capability(&self, role: &AgentId) -> Option<&'static str> {
        let needs_tool_calling = match &self.tool_calling_roles {
            Some(roles) => roles.contains(role),
            // Legacy seed-specialist defaults, preserved when no explicit
            // topology set is configured.
            None => matches!(role.as_str(), "researcher" | "coder" | "validator"),
        };
        needs_tool_calling.then_some("tool_calling")
    }

    fn compatible_candidates(&self, role: &AgentId) -> Vec<&RoutingProfile> {
        let mut candidates: Vec<&RoutingProfile> = self
            .profiles
            .iter()
            .filter(|profile| self.profile_meets_requirements(role, profile))
            .collect();

        candidates.sort_by(|a, b| {
            a.cost_per_1k_tokens
                .partial_cmp(&b.cost_per_1k_tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        candidates
    }

    /// Select the most affordable profile, downgrading if necessary.
    fn select_affordable<'a>(
        &self,
        candidates: &'a [&'a RoutingProfile],
        role: &AgentId,
        budget_remaining: Option<f64>,
    ) -> Result<&'a RoutingProfile, OrchestratorError> {
        if let Some(budget) = budget_remaining {
            // Find the best profile within budget
            for &profile in candidates {
                let estimated_cost = CostEstimator::estimate(role, profile);
                if estimated_cost <= budget {
                    return Ok(profile);
                }
            }

            // All candidates exceed budget — report NoAffordableModel
            return Err(OrchestratorError::NoAffordableModel { role: role.clone() });
        }

        // No budget constraint — return the first (best) candidate
        candidates
            .first()
            .copied()
            .ok_or(OrchestratorError::NoAffordableModel { role: role.clone() })
    }

    /// Compute the effective remaining budget from the caller param
    /// combined with the spend_tracker's session cap.
    fn effective_budget(&self, budget_hint: Option<f64>) -> Option<f64> {
        let cap_remaining =
            self.spend_tracker.session_cap().map(|cap| cap - self.spend_tracker.session_total());
        match (budget_hint, cap_remaining) {
            (Some(hint), Some(cap)) => Some(hint.min(cap)),
            (Some(hint), None) => Some(hint),
            (None, Some(cap)) => Some(cap.max(0.0)),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn mock_profile(model: &str, cost: f64) -> RoutingProfile {
        RoutingProfile {
            provider_config_id: "test-provider".into(),
            provider: "test".into(),
            model: model.into(),
            cost_per_1k_tokens: cost,
            avg_latency_ms: 100,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        }
    }

    fn mock_profiles() -> Vec<RoutingProfile> {
        vec![
            mock_profile("cheap", 0.001),
            mock_profile("mid", 0.005),
            mock_profile("expensive", 0.01),
        ]
    }

    fn mock_engine(profiles: Vec<RoutingProfile>) -> RoutingEngine {
        let spend_tracker = Arc::new(SpendTracker::default());
        let pin_config = ModelPinConfig { pins: HashMap::new(), ..Default::default() };
        let event_bus = EventBus::default();

        RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus)
    }

    fn mock_engine_with_default(
        profiles: Vec<RoutingProfile>,
        default_model: Option<&str>,
        default_provider_config_id: Option<&str>,
    ) -> RoutingEngine {
        let spend_tracker = Arc::new(SpendTracker::default());
        let pin_config = ModelPinConfig {
            pins: HashMap::new(),
            default_model: default_model.map(ToString::to_string),
            default_provider_config_id: default_provider_config_id.map(ToString::to_string),
        };
        let event_bus = EventBus::default();

        RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus)
    }

    fn architect() -> AgentId {
        AgentId::new("architect")
    }

    fn researcher() -> AgentId {
        AgentId::new("researcher")
    }

    fn coder() -> AgentId {
        AgentId::new("coder")
    }

    fn reviewer() -> AgentId {
        AgentId::new("reviewer")
    }

    fn validator() -> AgentId {
        AgentId::new("validator")
    }

    #[test]
    fn unassigned_architect_uses_lowest_cost_compatible_model() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let task_id = TaskId::new();

        let result = engine.select(&architect(), None, task_id).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[tokio::test]
    async fn session_selection_preserves_event_scope() {
        let profiles = mock_profiles();
        let spend_tracker = Arc::new(SpendTracker::default());
        let event_bus = EventBus::default();
        let mut events = event_bus.subscribe_durable();
        let engine = RoutingEngine::new(
            profiles,
            spend_tracker,
            ModelPinConfig { pins: HashMap::new(), ..Default::default() },
            event_bus,
        );
        let task_id = TaskId::new();
        let session_id = Ulid::new();

        engine.select_for_session(&architect(), None, task_id, Some(session_id)).unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.session_id, session_id);
        assert_eq!(event.correlation_id, task_id.0);
        assert!(matches!(event.kind, EventKind::RoutingDecided { .. }));
    }

    #[test]
    fn unassigned_researcher_uses_lowest_cost_compatible_model() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let task_id = TaskId::new();

        let result = engine.select(&researcher(), None, task_id).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn unassigned_coder_uses_lowest_cost_compatible_model() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let task_id = TaskId::new();

        let result = engine.select(&coder(), None, task_id).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn unassigned_reviewer_uses_lowest_cost_compatible_model() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let task_id = TaskId::new();

        let result = engine.select(&reviewer(), None, task_id).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn select_validator_gets_cheapest() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let task_id = TaskId::new();

        let result = engine.select(&validator(), None, task_id).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn pinned_model_override() {
        let profiles = mock_profiles();
        let spend_tracker = Arc::new(SpendTracker::default());
        let mut pins = HashMap::new();
        pins.insert(architect(), "expensive".to_string());
        let pin_config = ModelPinConfig { pins, ..Default::default() };
        let event_bus = EventBus::default();

        let engine = RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus);
        let task_id = TaskId::new();

        let result = engine.select(&architect(), None, task_id).unwrap();
        assert_eq!(result.model, "expensive");
    }

    #[test]
    fn budget_downgrade() {
        // Budget 0.035 makes this affordable.
        let profile = mock_profile("arch-capable", 0.008);
        let profiles = vec![profile];
        let engine = mock_engine(profiles);
        let task_id = TaskId::new();

        let result = engine.select(&architect(), Some(0.035), task_id).unwrap();
        assert_eq!(result.model, "arch-capable");
    }

    #[test]
    fn no_affordable_model_returns_err() {
        let profiles = vec![mock_profile("expensive", 10.0)];
        let engine = mock_engine(profiles);
        let task_id = TaskId::new();

        // Architect estimate for expensive (4k × 10.0/1k) = 40.0 >> 0.0001
        let result = engine.select(&architect(), Some(0.0001), task_id);
        assert!(result.is_err());
    }

    #[test]
    fn retry_or_downgrade_finds_cheaper() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());

        let current = mock_profile("mid", 0.005);
        let result = engine.retry_or_downgrade(&researcher(), &current, &profiles).unwrap();

        assert!(result.cost_per_1k_tokens < current.cost_per_1k_tokens);
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn retry_or_downgrade_fails_when_no_cheaper() {
        let profiles = vec![mock_profile("cheap", 0.001)];
        let engine = mock_engine(profiles.clone());

        let current = mock_profile("cheap", 0.001);
        let result = engine.retry_or_downgrade(&researcher(), &current, &profiles);

        assert!(result.is_err());
    }

    #[test]
    fn explicit_model_assignment_is_authoritative() {
        let profiles = mock_profiles();
        let spend_tracker = Arc::new(SpendTracker::default());
        let mut pins = HashMap::new();
        pins.insert(architect(), "mid".to_string());
        let pin_config = ModelPinConfig { pins, ..Default::default() };
        let event_bus = EventBus::default();

        let engine = RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus);
        let task_id = TaskId::new();

        let result = engine.select(&architect(), None, task_id).unwrap();
        assert_eq!(result.model, "mid");
    }

    #[test]
    fn automatic_routing_uses_available_compatible_model() {
        let engine = mock_engine(vec![mock_profile("available", 0.001)]);
        let result = engine.select(&architect(), None, TaskId::new()).unwrap();
        assert_eq!(result.model, "available");
    }

    #[test]
    fn pinned_model_missing_required_capability_errors() {
        let mut profile = mock_profile("text-only", 0.001);
        profile.supports_tool_calling = false;
        let spend_tracker = Arc::new(SpendTracker::default());
        let mut pins = HashMap::new();
        pins.insert(coder(), "text-only".to_string());
        let engine = RoutingEngine::new(
            vec![profile],
            spend_tracker,
            ModelPinConfig { pins, ..Default::default() },
            EventBus::default(),
        );
        let result = engine.select(&coder(), None, TaskId::new());
        assert!(matches!(result, Err(OrchestratorError::PinnedModelMissingCapability { .. })));
    }

    #[test]
    fn custom_tool_calling_roles_are_enforced() {
        // A single non-tool-calling profile exercises both the pinned and
        // unassigned rejection paths, plus the legacy-set exemption.
        let mut text_only = mock_profile("text-only", 0.001);
        text_only.supports_tool_calling = false;
        let copilot = AgentId::new("copilot");

        let engine_with_roles = |profiles: Vec<RoutingProfile>| {
            RoutingEngine::new(
                profiles,
                Arc::new(SpendTracker::default()),
                ModelPinConfig { pins: HashMap::new(), ..Default::default() },
                EventBus::default(),
            )
            .with_tool_calling_roles(HashSet::from([copilot.clone()]))
        };

        // Pinned model lacking tool calling is rejected for "copilot".
        let mut pins = HashMap::new();
        pins.insert(copilot.clone(), "text-only".to_string());
        let pinned_engine = RoutingEngine::new(
            vec![text_only.clone()],
            Arc::new(SpendTracker::default()),
            ModelPinConfig { pins, ..Default::default() },
            EventBus::default(),
        )
        .with_tool_calling_roles(HashSet::from([copilot.clone()]));
        let result = pinned_engine.select(&copilot, None, TaskId::new());
        assert!(matches!(result, Err(OrchestratorError::PinnedModelMissingCapability { .. })));

        // Unassigned "copilot" is rejected when only a non-tool-calling
        // profile is available.
        let engine = engine_with_roles(vec![text_only.clone()]);
        let result = engine.select(&AgentId::new("copilot"), None, TaskId::new());
        assert!(matches!(result, Err(OrchestratorError::NoCapableModel { .. })));

        // "researcher" is a legacy seed specialist absent from the explicit
        // set, so it does NOT require tool calling: the non-tool-calling
        // profile is a valid unassigned candidate.
        let result = engine.select(&researcher(), None, TaskId::new()).unwrap();
        assert_eq!(result.model, "text-only");
    }

    #[test]
    fn legacy_tool_calling_roles_still_enforced_when_unset() {
        // Plain `new()` keeps the legacy defaults: "coder" requires
        // tool_calling (pinned path), "architect" does not.
        let mut text_only = mock_profile("text-only", 0.001);
        text_only.supports_tool_calling = false;

        let mut pins = HashMap::new();
        pins.insert(coder(), "text-only".to_string());
        let engine = RoutingEngine::new(
            vec![text_only.clone()],
            Arc::new(SpendTracker::default()),
            ModelPinConfig { pins, ..Default::default() },
            EventBus::default(),
        );
        let result = engine.select(&coder(), None, TaskId::new());
        assert!(matches!(result, Err(OrchestratorError::PinnedModelMissingCapability { .. })));

        let engine = RoutingEngine::new(
            vec![text_only],
            Arc::new(SpendTracker::default()),
            ModelPinConfig { pins: HashMap::new(), ..Default::default() },
            EventBus::default(),
        );
        let result = engine.select(&architect(), None, TaskId::new()).unwrap();
        assert_eq!(result.model, "text-only");
    }

    #[test]
    fn pinned_model_exceeds_budget_errors() {
        // Pin expensive model but give a budget too small
        let profiles = mock_profiles();
        let spend_tracker = Arc::new(SpendTracker::default());
        let mut pins = HashMap::new();
        pins.insert(coder(), "expensive".to_string());
        let pin_config = ModelPinConfig { pins, ..Default::default() };
        let event_bus = EventBus::default();

        let engine = RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus);
        let task_id = TaskId::new();

        // Coder estimate for expensive: (8k × 0.01/1k) = 0.08, budget = 0.01
        let result = engine.select(&coder(), Some(0.01), task_id);
        assert!(matches!(result, Err(OrchestratorError::PinnedModelBudgetExceeded { .. })));
    }

    #[test]
    fn pinned_model_within_budget_ok() {
        // Coder estimate: 8k × 0.005/1k = 0.04.
        let profiles = mock_profiles();
        let spend_tracker = Arc::new(SpendTracker::default());
        let mut pins = HashMap::new();
        pins.insert(coder(), "mid".to_string());
        let pin_config = ModelPinConfig { pins, ..Default::default() };
        let event_bus = EventBus::default();

        let engine = RoutingEngine::new(profiles, spend_tracker, pin_config, event_bus);
        let task_id = TaskId::new();

        let result = engine.select(&coder(), Some(0.05), task_id).unwrap();
        assert_eq!(result.model, "mid");
    }

    #[test]
    fn provider_pin_keeps_duplicate_model_names_paired() {
        let mut first = mock_profile("shared-model", 0.001);
        first.provider_config_id = "provider-a".into();
        first.provider = "openai".into();
        let mut second = mock_profile("shared-model", 0.009);
        second.provider_config_id = "provider-b".into();
        second.provider = "openrouter".into();

        let mut pins = HashMap::new();
        pins.insert(coder(), "shared-model".to_string());
        let mut provider_pins = HashMap::new();
        provider_pins.insert(coder(), "provider-b".to_string());
        let engine = RoutingEngine::new(
            vec![first, second],
            Arc::new(SpendTracker::default()),
            ModelPinConfig { pins, ..Default::default() },
            EventBus::default(),
        )
        .with_provider_pins(provider_pins);

        let result = engine.select(&coder(), None, TaskId::new()).unwrap();
        assert_eq!(result.provider_config_id, "provider-b");
        assert_eq!(result.provider, "openrouter");
    }

    #[test]
    fn missing_explicit_pair_does_not_silently_fallback() {
        let mut pins = HashMap::new();
        pins.insert(coder(), "missing-model".to_string());
        let engine = RoutingEngine::new(
            mock_profiles(),
            Arc::new(SpendTracker::default()),
            ModelPinConfig { pins, ..Default::default() },
            EventBus::default(),
        );

        let result = engine.select(&coder(), None, TaskId::new());
        assert!(matches!(result, Err(OrchestratorError::PinnedModelNotFound { .. })));
    }

    #[test]
    fn cheapest_profile_is_selected_when_no_budget() {
        let profiles = mock_profiles();
        let engine = mock_engine(profiles.clone());
        let result = engine.select(&coder(), None, TaskId::new()).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn cheapest_respects_tool_calling() {
        let mut no_tools = mock_profile("no-tools", 0.0001);
        no_tools.supports_tool_calling = false;
        let profiles = vec![no_tools, mock_profile("with-tools", 0.005)];
        let engine = mock_engine(profiles);
        let result = engine.select(&coder(), None, TaskId::new()).unwrap();
        assert_eq!(result.model, "with-tools");
    }

    #[test]
    fn cheapest_returns_none_when_no_compatible() {
        let mut no_tools = mock_profile("no-tools", 0.001);
        no_tools.supports_tool_calling = false;
        let engine = mock_engine(vec![no_tools]);
        let task_id = TaskId::new();
        let result = engine.select(&coder(), None, task_id);
        assert!(result.is_err());
    }

    #[test]
    fn budget_enough_for_expensive_model() {
        let profiles = vec![mock_profile("cheap", 0.001), mock_profile("expensive", 0.01)];
        let engine = mock_engine(profiles);
        let task_id = TaskId::new();
        let result = engine.select(&coder(), Some(1.0), task_id).unwrap();
        // With a generous budget, should still pick cheapest
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn routing_profile_is_copyable() {
        let p = mock_profile("test", 0.5);
        let p2 = p.clone();
        assert_eq!(p.model, p2.model);
        assert_eq!(p.cost_per_1k_tokens, p2.cost_per_1k_tokens);
    }

    #[test]
    fn retry_or_downgrade_same_cost_returns_err() {
        let profiles = vec![mock_profile("same1", 0.005), mock_profile("same2", 0.005)];
        let engine = mock_engine(profiles.clone());
        let current = mock_profile("same1", 0.005);
        let result = engine.retry_or_downgrade(&researcher(), &current, &profiles);
        assert!(result.is_err());
    }

    #[test]
    fn model_pin_config_default_is_empty() {
        let config = ModelPinConfig::default();
        assert!(config.pins.is_empty());
        assert!(config.default_model.is_none());
        assert!(config.default_provider_config_id.is_none());
    }

    #[test]
    fn fallback_to_default_resolves_configured_default() {
        let engine = mock_engine_with_default(mock_profiles(), Some("mid"), None);
        let result = engine.fallback_to_default(&coder()).unwrap();
        assert_eq!(result.model, "mid");
        assert_eq!(result.provider_config_id, "test-provider");
    }

    #[test]
    fn fallback_to_default_none_returns_no_affordable_model() {
        let engine = mock_engine(mock_profiles());
        let result = engine.fallback_to_default(&coder());
        assert!(matches!(result, Err(OrchestratorError::NoAffordableModel { .. })));
    }

    #[test]
    fn fallback_to_default_requires_capability() {
        let mut text_only = mock_profile("text-only", 0.001);
        text_only.supports_tool_calling = false;
        let engine = mock_engine_with_default(vec![text_only], Some("text-only"), None);
        let result = engine.fallback_to_default(&coder());
        assert!(matches!(result, Err(OrchestratorError::PinnedModelNotFound { .. })));
    }

    #[test]
    fn fallback_to_default_respects_provider_pairing() {
        let mut first = mock_profile("shared-model", 0.001);
        first.provider_config_id = "provider-a".into();
        first.provider = "openai".into();
        let mut second = mock_profile("shared-model", 0.009);
        second.provider_config_id = "provider-b".into();
        second.provider = "openrouter".into();

        let engine =
            mock_engine_with_default(vec![first, second], Some("shared-model"), Some("provider-b"));
        let result = engine.fallback_to_default(&coder()).unwrap();
        assert_eq!(result.model, "shared-model");
        assert_eq!(result.provider_config_id, "provider-b");
        assert_eq!(result.provider, "openrouter");
    }

    #[test]
    fn fallback_to_default_unresolvable_model() {
        let engine = mock_engine_with_default(mock_profiles(), Some("missing-model"), None);
        let result = engine.fallback_to_default(&coder());
        let Err(OrchestratorError::PinnedModelNotFound { model, .. }) = result else {
            panic!("expected PinnedModelNotFound");
        };
        assert_eq!(model, "missing-model");
    }
}
