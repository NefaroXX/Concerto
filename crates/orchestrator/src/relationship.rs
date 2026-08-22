//! Relational agent collaboration types.
//!
//! Defines the `AgentRelationship`, `CollaborationRule`, and `AgentHandoff`
//! types that describe how agents interact during multi-agent orchestration.
//! Used by `CoordinatorAgent` to govern review/validation cycles and to
//! produce structured handoff events for the audit log.

use concerto_core::types::{AgentId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The nature of the relationship between two agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentRelationship {
    /// Agent reviews and approves the other's work
    Supervises,
    /// Agent provides context/research to support another
    ProvidesContextTo,
    /// Agent reports status/results to the coordinator
    ReportsTo,
    /// Agent owns the design; others implement within it
    OwnsDesign,
}

/// Describes how two agent roles interact during orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationRule {
    pub from: AgentId,
    pub to: AgentId,
    pub relationship: AgentRelationship,
    /// Maximum review/revision cycles before escalation.
    /// `None` means no hard limit.
    pub max_cycles: Option<u32>,
}

/// Validated, queryable relationship topology used by the coordinator.
///
/// Keeping this logic in one place prevents orchestration code from silently
/// accepting duplicate, self-referential, or unusable collaboration rules.
#[derive(Debug, Clone)]
pub struct RelationshipManager {
    rules: HashMap<(AgentId, AgentId), CollaborationRule>,
}

impl RelationshipManager {
    pub fn new(rules: Vec<CollaborationRule>) -> Result<Self, String> {
        let mut manager = Self { rules: HashMap::new() };
        for rule in rules {
            manager.upsert(rule)?;
        }
        Ok(manager)
    }

    pub fn defaults() -> Self {
        // The built-in rules below are validated constants; a failure here
        // would only indicate a programming error in this module. Degrade to
        // an empty rule set rather than panicking in library code.
        match Self::new(default_collaboration_rules()) {
            Ok(manager) => manager,
            Err(error) => {
                tracing::warn!(error = %error, "built-in collaboration rules are invalid; using empty rule set");
                Self { rules: HashMap::new() }
            }
        }
    }

    pub fn upsert(&mut self, rule: CollaborationRule) -> Result<(), String> {
        if rule.from == rule.to {
            return Err(format!("an agent cannot have a relationship with itself: {}", rule.from));
        }
        if matches!(rule.max_cycles, Some(0)) {
            return Err("max_cycles must be at least 1 when specified".into());
        }
        let key = (rule.from.clone(), rule.to.clone());
        self.rules.insert(key, rule);
        Ok(())
    }

    pub fn remove(&mut self, from: &AgentId, to: &AgentId) -> Option<CollaborationRule> {
        self.rules.remove(&(from.clone(), to.clone()))
    }

    pub fn rule(&self, from: &AgentId, to: &AgentId) -> Option<&CollaborationRule> {
        self.rules.get(&(from.clone(), to.clone()))
    }

    pub fn max_cycles(&self, from: &AgentId, to: &AgentId, fallback: u32) -> u32 {
        self.rule(from, to).and_then(|rule| rule.max_cycles).unwrap_or(fallback)
    }

    pub fn rules(&self) -> Vec<CollaborationRule> {
        let mut rules: Vec<_> = self.rules.values().cloned().collect();
        rules.sort_by_key(|rule| (rule.from.as_str().to_string(), rule.to.as_str().to_string()));
        rules
    }
}

/// A structured handoff from one agent to another with a deliverable.
#[derive(Debug, Clone)]
pub struct AgentHandoff {
    pub from: AgentId,
    pub to: AgentId,
    pub task_id: TaskId,
    pub rationale: String,
    pub deliverable: HandoffDeliverable,
}

impl AgentHandoff {
    pub fn new(
        from: AgentId,
        to: AgentId,
        task_id: TaskId,
        rationale: String,
        deliverable: HandoffDeliverable,
    ) -> Self {
        Self { from, to, task_id, rationale, deliverable }
    }
}

/// The artifact produced by one agent and consumed by another.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HandoffDeliverable {
    /// DesignDoc text
    Design(String),
    /// Research findings
    Research(Vec<String>),
    /// Reviewer feedback
    CodeReview(String),
    /// Code/textual diff
    Implementation(String),
}

/// Return the default collaboration rules for the known agent roles.
///
/// These are the rules that govern the standard Architect → Researcher →
/// Coder pipeline with Reviewer and Validator oversight.
pub fn default_collaboration_rules() -> Vec<CollaborationRule> {
    vec![
        CollaborationRule {
            from: AgentId::new("reviewer"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(3),
        },
        CollaborationRule {
            from: AgentId::new("researcher"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::ProvidesContextTo,
            max_cycles: None,
        },
        CollaborationRule {
            from: AgentId::new("architect"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::OwnsDesign,
            max_cycles: None,
        },
        CollaborationRule {
            from: AgentId::new("architect"),
            to: AgentId::new("researcher"),
            relationship: AgentRelationship::OwnsDesign,
            max_cycles: None,
        },
        CollaborationRule {
            from: AgentId::new("validator"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(2),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_self_relationships_and_zero_cycle_limits() {
        let self_rule = CollaborationRule {
            from: AgentId::new("coder"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(1),
        };
        assert!(RelationshipManager::new(vec![self_rule]).is_err());

        let zero_cycles = CollaborationRule {
            from: AgentId::new("reviewer"),
            to: AgentId::new("coder"),
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(0),
        };
        assert!(RelationshipManager::new(vec![zero_cycles]).is_err());
    }

    #[test]
    fn upsert_replaces_the_existing_directed_relationship() {
        let mut manager = RelationshipManager::defaults();
        manager
            .upsert(CollaborationRule {
                from: AgentId::new("reviewer"),
                to: AgentId::new("coder"),
                relationship: AgentRelationship::Supervises,
                max_cycles: Some(7),
            })
            .unwrap();
        assert_eq!(manager.max_cycles(&AgentId::new("reviewer"), &AgentId::new("coder"), 3), 7);
    }
}
