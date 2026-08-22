//! `ModelSelector` — resolves the configured `ModelProfile` for an agent.
//!
//! Provider configuration ID and model name remain paired throughout dispatch.

use std::sync::Arc;

use concerto_core::error::OrchestratorError;
use concerto_core::ids::Ulid;
use concerto_core::types::{AgentId, RoutingProfile, TaskId};

use crate::model::ModelProfile;
use crate::model_registry::ModelRegistry;
use crate::routing::RoutingEngine;

/// Resolves the explicit or budget-compatible model for a given agent.
///
/// This is the public API for per-agent model selection.
pub struct ModelSelector {
    registry: Arc<ModelRegistry>,
    routing: Arc<RoutingEngine>,
}

impl ModelSelector {
    /// Create a new `ModelSelector`.
    pub fn new(registry: Arc<ModelRegistry>, routing: Arc<RoutingEngine>) -> Self {
        Self { registry, routing }
    }

    /// Select the best `ModelProfile` for the given agent.
    ///
    /// Uses `RoutingEngine::select()` and resolves the exact provider/model
    /// pair to its full `ModelProfile`.
    pub fn select(
        &self,
        role: &AgentId,
        budget_remaining: Option<f64>,
        task_id: TaskId,
    ) -> Result<ModelProfile, OrchestratorError> {
        self.select_for_session(role, budget_remaining, task_id, None)
    }

    /// Select a profile and preserve the active session on routing events.
    pub fn select_for_session(
        &self,
        role: &AgentId,
        budget_remaining: Option<f64>,
        task_id: TaskId,
        session_id: Option<Ulid>,
    ) -> Result<ModelProfile, OrchestratorError> {
        let rp = self.routing.select_for_session(role, budget_remaining, task_id, session_id)?;
        Ok(self.wrap(rp))
    }

    /// Resolve the configured global default model for a role, wrapped as a
    /// full `ModelProfile` (same registry/fallback wrap as `select_for_session`).
    pub fn fallback_to_default(&self, role: &AgentId) -> Result<ModelProfile, OrchestratorError> {
        let rp = self.routing.fallback_to_default(role)?;
        Ok(self.wrap(rp))
    }

    /// Wrap a resolved `RoutingProfile` into a full `ModelProfile`, preferring
    /// the exact provider/model pair registered in the registry and falling
    /// back to a manual construction from the routing profile's metadata.
    fn wrap(&self, rp: RoutingProfile) -> ModelProfile {
        if let Some(profile) = self.registry.get(&rp.provider_config_id, &rp.model) {
            return profile.clone();
        }
        ModelProfile {
            context_window: rp.context_window,
            supports_tool_calling: rp.supports_tool_calling,
            base_url: rp.base_url.clone(),
            description: rp.description.clone(),
            profile: rp,
        }
    }
}
