//! Phase 0 intent-routing vocabulary (ADR-55).
//!
//! This module retains the intent **vocabulary** the rest of the workspace
//! still speaks: [`RequestedOutcome`], [`TaskScope`], [`RouterOutput`],
//! [`RouterRoute`], [`RunStage`], [`PlanDecision`], and
//! [`LOW_CONFIDENCE_THRESHOLD`]. The deterministic rule-based `route()`
//! function and its keyword corpora were removed once the unified agent loop
//! stopped consulting routing as control flow (the coordinator owns run-shape
//! triage); only the types and the audit rule-name round-trip remain.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Confidence below which the router must not proceed with a rule- or
/// classifier-derived outcome.
///
/// Confidence is used ONLY for router-path selection (rule vs classifier vs
/// ask): low confidence ⇒ keep the run read-only and ask the user. This module
/// is deterministic — when no rule matches, the route MUST be
/// [`RouterRoute::AskUser`] with confidence `0.0`. The orchestrator's LLM
/// classifier (ADR-56) re-uses the same constant as its re-route threshold so
/// a classifier Execute re-route always clears the intent gate's arm-1
/// confirmation dialog.
pub const LOW_CONFIDENCE_THRESHOLD: f32 = 0.7;

/// The outcome the user requested for the current request.
///
/// The variant list is deliberately the Phase 0 set; future phases may extend
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestedOutcome {
    /// A text answer; no tool use implied.
    Answer,
    /// Investigate why something is broken and explain it.
    Diagnose,
    /// Read-only review/critique of existing code.
    Review,
    /// Produce a plan/design as text; no writes.
    Plan,
    /// Implement/change code (write path). A negation override drops this.
    Execute,
    /// Run tests/checks to verify a change.
    Verify,
}

/// User decision on a previously approved plan (ADR-55 Phase 1d).
///
/// The run loop asks this through
/// [`ApprovalSink::request_plan_approval`] when an action-required Execute
/// request matches a stored plan binding for the same objective. The
/// decision is kept in the intent vocabulary so the approval sink interface
/// (core) and the registry decision helper (orchestrator) share one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanDecision {
    /// Apply the previously approved plan now (audited authority; grants
    /// filesystem + git like a confirmed Execute).
    Apply,
    /// Discard the stored plan and plan this objective anew (read-only).
    Replan,
}

impl PlanDecision {
    /// Stable audit label for the decision (`apply` | `replan`). The
    /// `None`/dismissed case is rendered by the caller.
    pub fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Replan => "replan",
        }
    }
}

/// Minimal Phase-0 task scope: the project a request targets and any file
/// paths it hints at.
///
/// Capability tiers (read-only vs write scopes), glob sets, and deeper scope
/// modelling are Phase 1; Phase 0 only carries the project root plus candidate
/// file paths extracted heuristically from the request text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskScope {
    /// Project root the request is scoped to.
    pub project_dir: PathBuf,
    /// Heuristically hinted files, normalized to absolute paths inside
    /// `project_dir`. May be empty when the request names no files.
    pub hinted_paths: Vec<PathBuf>,
}

/// Which routing path produced the final outcome.
///
/// [`RouterRoute::RuleHit`] names a deterministic corpus that matched;
/// [`RouterRoute::AskUser`] means nothing matched; and
/// [`RouterRoute::LlmClassifier`] is produced only by the orchestrator's LLM
/// classifier wrapper (ADR-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RouterRoute {
    /// A deterministic Phase-0 corpus matched; `rule` names the corpus that
    /// won (see the `RULE_*` constants).
    RuleHit { rule: &'static str },
    /// LLM classifier path — emitted only by the orchestrator's classifier
    /// wrapper re-routing a result above threshold (ADR-56).
    LlmClassifier,
    /// No rule matched — ask the user for clarification.
    AskUser,
}

/// Serialized rule-name constants referenced by [`RouterRoute::RuleHit`].
///
/// [`RouterRoute`] carries `rule` as a `&'static str`, which the serde derive
/// cannot deserialize, so rule names resolve onto these constants and
/// serialization maps them back onto the same constants (see the manual serde
/// impl below). Kept private.
const RULE_NEGATION_OVERRIDE: &str = "negation_override";
const RULE_QUESTION: &str = "question";
const RULE_VERIFY: &str = "verify_keyword";
const RULE_PLAN: &str = "plan_keyword";
const RULE_REVIEW: &str = "review_keyword";
const RULE_DIAGNOSE: &str = "diagnose_keyword";
const RULE_EXECUTE: &str = "execute_keyword";
const RULE_SMALLTALK: &str = "smalltalk";

/// The full routing result for one request string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouterOutput {
    /// The outcome the router settled on.
    pub outcome: RequestedOutcome,
    /// Project root + hinted file paths extracted from the request.
    pub scope: TaskScope,
    /// Confidence in the outcome, consumed only for router-path selection
    /// (see [`LOW_CONFIDENCE_THRESHOLD`]).
    pub confidence: f32,
    /// Which routing path produced this output.
    pub route: RouterRoute,
}

/// Lifecycle stage of an agent run, as delineated by the intent router.
///
/// `Display` is the bare enum name in Phase 0; chip/label presentation is a UI
/// concern that lands in a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStage {
    /// Grounding the request in the conversation/project context.
    Understand,
    /// Reading the relevant files and gathering evidence.
    Inspect,
    /// Designing the change before touching anything.
    Plan,
    /// Making the change.
    Execute,
    /// Running tests/checks against the change.
    Verify,
    /// The run is finished.
    Complete,
}

impl fmt::Display for RunStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Understand => "Understand",
            Self::Inspect => "Inspect",
            Self::Plan => "Plan",
            Self::Execute => "Execute",
            Self::Verify => "Verify",
            Self::Complete => "Complete",
        };
        f.write_str(label)
    }
}

/// Resolve a serialized rule name back to the matching `'static` constant so
/// [`RouterRoute`] can round-trip with a `&'static str` field (the serde
/// derive cannot deserialize `&'static str` directly).
fn rule_name_to_static(rule: &str) -> Option<&'static str> {
    match rule {
        RULE_NEGATION_OVERRIDE => Some(RULE_NEGATION_OVERRIDE),
        RULE_QUESTION => Some(RULE_QUESTION),
        RULE_VERIFY => Some(RULE_VERIFY),
        RULE_PLAN => Some(RULE_PLAN),
        RULE_REVIEW => Some(RULE_REVIEW),
        RULE_DIAGNOSE => Some(RULE_DIAGNOSE),
        RULE_EXECUTE => Some(RULE_EXECUTE),
        RULE_SMALLTALK => Some(RULE_SMALLTALK),
        _ => None,
    }
}

impl Serialize for RouterRoute {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStructVariant;
        match self {
            // Externally-tagged representation matching what the serde derive
            // would emit: `{"RuleHit":{"rule":"..."}}`.
            RouterRoute::RuleHit { rule } => {
                let mut state =
                    serializer.serialize_struct_variant("RouterRoute", 0, "RuleHit", 1)?;
                state.serialize_field("rule", rule)?;
                state.end()
            }
            RouterRoute::LlmClassifier => {
                serializer.serialize_unit_variant("RouterRoute", 1, "LlmClassifier")
            }
            RouterRoute::AskUser => serializer.serialize_unit_variant("RouterRoute", 2, "AskUser"),
        }
    }
}

impl<'de> Deserialize<'de> for RouterRoute {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        // Parse the externally-tagged representation produced by the
        // `Serialize` impl: `"AskUser"` / `"LlmClassifier"` for unit variants
        // and `{"RuleHit":{"rule":"..."}}` for the struct variant. `rule`
        // arrives as an owned string and is mapped back onto the closed set of
        // `'static` rule-name constants.
        let tagged = serde_json::Value::deserialize(deserializer)?;
        match tagged {
            serde_json::Value::String(variant) => match variant.as_str() {
                "LlmClassifier" => Ok(RouterRoute::LlmClassifier),
                "AskUser" => Ok(RouterRoute::AskUser),
                other => {
                    Err(D::Error::custom(format!("unknown RouterRoute unit variant: {other}")))
                }
            },
            serde_json::Value::Object(map) => {
                let Some((name, inner)) = map.into_iter().next() else {
                    return Err(D::Error::custom("RouterRoute object must not be empty"));
                };
                if name != "RuleHit" {
                    return Err(D::Error::custom(format!("unknown RouterRoute variant: {name}")));
                }
                let Some(rule) = inner.get("rule").and_then(serde_json::Value::as_str) else {
                    return Err(D::Error::custom("RuleHit is missing a string 'rule' field"));
                };
                let rule = rule_name_to_static(rule)
                    .ok_or_else(|| D::Error::custom(format!("unknown router rule: {rule}")))?;
                Ok(RouterRoute::RuleHit { rule })
            }
            _ => Err(D::Error::custom(
                "unexpected RouterRoute shape; expected a variant name or a single-key object",
            )),
        }
    }
}
