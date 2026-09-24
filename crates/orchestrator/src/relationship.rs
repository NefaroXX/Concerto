//! Relational agent collaboration types.
//!
//! Defines the `AgentRelationship`, `CollaborationRule`, and `AgentHandoff`
//! types that describe how agents interact during multi-agent orchestration.
//! Used by `CoordinatorAgent` to govern review/validation cycles and to
//! produce structured handoff events for the audit log.

use concerto_config::blueprint::StageKind;
use concerto_core::types::{AgentId, AgentStage, TaskId};
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

    pub fn defaults_for_agents(agents: &[(AgentId, StageKind)]) -> Self {
        // The default stage-kind pairs always validate (no self pairs, caps
        // >= 1); a failure here would only indicate a programming error in this
        // module. Degrade to an empty rule set rather than panicking in library
        // code.
        let rules = resolve_stage_relationships(&default_stage_relationships(), agents);
        match Self::new(rules) {
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

/// A default collaboration rule expressed over **stage kinds** (ADR-58 D2),
/// never agent role ids. Resolved to concrete agent ids at runtime by
/// [`resolve_stage_relationships`] against the agents actually staffing each
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageRelationship {
    /// The stage kind the supervising/context-providing agent staffs.
    pub from: StageKind,
    /// The stage kind whose agents are the target of the relationship.
    pub to: StageKind,
    /// The relationship the pair expresses.
    pub relationship: AgentRelationship,
    /// Maximum review/revision cycles before escalation (`None` = no limit).
    pub max_cycles: Option<u32>,
}

/// The engine-default collaboration topology as stage-kind pairs.
///
/// These reproduce the historical id-based seed exactly on the standard
/// five-agent roster (one agent per kind): Review→Execution supervises (cap 6),
/// Research→Execution provides context, Planning→Execution and
/// Planning→Research own the design, and Acceptance→Execution supervises
/// (cap 5). A roster that omits a kind simply yields no edge for pairs that
/// reference it — never an error.
pub fn default_stage_relationships() -> Vec<StageRelationship> {
    vec![
        StageRelationship {
            from: StageKind::Review,
            to: StageKind::Execution,
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(6),
        },
        StageRelationship {
            from: StageKind::Research,
            to: StageKind::Execution,
            relationship: AgentRelationship::ProvidesContextTo,
            max_cycles: None,
        },
        StageRelationship {
            from: StageKind::Planning,
            to: StageKind::Execution,
            relationship: AgentRelationship::OwnsDesign,
            max_cycles: None,
        },
        StageRelationship {
            from: StageKind::Planning,
            to: StageKind::Research,
            relationship: AgentRelationship::OwnsDesign,
            max_cycles: None,
        },
        StageRelationship {
            from: StageKind::Acceptance,
            to: StageKind::Execution,
            relationship: AgentRelationship::Supervises,
            max_cycles: Some(5),
        },
    ]
}

/// Resolve stage-kind pairs against the agents staffing those kinds.
///
/// `agents` carries each registered agent together with the stage kind it
/// staffs (resolved by the caller from the blueprint facade, or from the
/// agent's stage tag via [`stage_kind_for_tag`] when no blueprint is attached).
/// Multiple agents of one kind produce the cross-product of from/to agents, so
/// a one-agent-per-kind roster yields exactly one rule per pair. Self pairs are
/// skipped; a pair whose kind is unstaffed contributes nothing (no error).
pub fn resolve_stage_relationships(
    pairs: &[StageRelationship],
    agents: &[(AgentId, StageKind)],
) -> Vec<CollaborationRule> {
    let mut rules = Vec::new();
    for pair in pairs {
        for (from, from_kind) in agents {
            if *from_kind != pair.from {
                continue;
            }
            for (to, to_kind) in agents {
                if *to_kind != pair.to || from == to {
                    continue;
                }
                rules.push(CollaborationRule {
                    from: from.clone(),
                    to: to.clone(),
                    relationship: pair.relationship,
                    max_cycles: pair.max_cycles,
                });
            }
        }
    }
    rules
}

/// Map a canonical stage tag to its known stage kind.
///
/// The standard tags are authoritative only as a **fallback** for callers with
/// no resolved blueprint: `design`→Planning, `research`→Research,
/// `implement`→Execution, `review`→Review, `validate`→Acceptance. A custom tag
/// has no known kind here (`None`) — with a blueprint attached the caller
/// resolves the kind from the facade instead, which honors renamed and custom
/// stage tags.
pub fn stage_kind_for_tag(tag: &AgentStage) -> Option<StageKind> {
    if tag.is_design() {
        Some(StageKind::Planning)
    } else if tag.is_research() {
        Some(StageKind::Research)
    } else if tag.is_implement() {
        Some(StageKind::Execution)
    } else if tag.is_review() {
        Some(StageKind::Review)
    } else if tag.is_validate() {
        Some(StageKind::Acceptance)
    } else {
        None
    }
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
        // A roster with one Review-kind agent and one Execution-kind agent
        // resolves the Review→Execution pair; upsert replaces its cap.
        let reviewer = AgentId::new("gatekeeper");
        let executor = AgentId::new("builder");
        let mut manager = RelationshipManager::defaults_for_agents(&[
            (reviewer.clone(), StageKind::Review),
            (executor.clone(), StageKind::Execution),
        ]);
        manager
            .upsert(CollaborationRule {
                from: reviewer.clone(),
                to: executor.clone(),
                relationship: AgentRelationship::Supervises,
                max_cycles: Some(7),
            })
            .unwrap();
        assert_eq!(manager.max_cycles(&reviewer, &executor, 3), 7);
    }

    #[test]
    fn defaults_resolve_only_against_staffed_kinds() {
        // The standard five-agent roster resolves exactly the five historical
        // edges; a roster missing the Review kind yields no Review→Execution
        // edge and no error.
        let standard = [
            (AgentId::new("architect"), StageKind::Planning),
            (AgentId::new("researcher"), StageKind::Research),
            (AgentId::new("coder"), StageKind::Execution),
            (AgentId::new("reviewer"), StageKind::Review),
            (AgentId::new("validator"), StageKind::Acceptance),
        ];
        let manager = RelationshipManager::defaults_for_agents(&standard);
        assert_eq!(manager.rules().len(), 5, "one rule per stage-kind pair on the standard roster");

        let without_review: Vec<_> =
            standard.iter().filter(|(id, _)| id.as_str() != "reviewer").cloned().collect();
        let reduced = RelationshipManager::defaults_for_agents(&without_review);
        assert!(
            !reduced.rules().iter().any(|rule| rule.from.as_str() == "reviewer"),
            "an unstaffed Review kind yields no edge"
        );
        assert_eq!(reduced.rules().len(), 4);
    }

    #[test]
    fn stage_kind_for_tag_maps_the_canonical_tags() {
        assert_eq!(stage_kind_for_tag(&AgentStage::new("design")), Some(StageKind::Planning));
        assert_eq!(stage_kind_for_tag(&AgentStage::new("research")), Some(StageKind::Research));
        assert_eq!(stage_kind_for_tag(&AgentStage::new("implement")), Some(StageKind::Execution));
        assert_eq!(stage_kind_for_tag(&AgentStage::new("review")), Some(StageKind::Review));
        assert_eq!(stage_kind_for_tag(&AgentStage::new("validate")), Some(StageKind::Acceptance));
        assert_eq!(stage_kind_for_tag(&AgentStage::new("custom")), None);
    }
}
