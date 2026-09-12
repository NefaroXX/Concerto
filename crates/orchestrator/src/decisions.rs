//! Typed Coordinator decisions (issue #52, parent #51) — a hard boundary
//! between the strategy output the Coordinator's model produces and the
//! deterministic orchestration machinery that executes it.
//!
//! The model proposes a decision; a [`DecisionValidator`] proves it against
//! the real infrastructure invariants (the registry roster, the
//! cited-whiteboard-evidence set, the project-root path space, and length
//! bounds) BEFORE anything is materialized, appended, dispatched, or
//! persisted. A rejected decision is a structured failure the model can read
//! and retry — it never mutates execution state, never crashes the
//! coordinator, and never lets the model control infrastructure invariants:
//!
//! - a dispatched agent id cannot bypass the graph (it must be registered);
//! - cited supporting-evidence ids cannot be fabricated (they must exist in
//!   the whiteboard log — checked at DECISION time, not deferred to the
//!   append-transaction check, which remains as defense-in-depth);
//! - artifact paths cannot escape the project root (lexical
//!   canonicalization at validation time — model paths are never trusted);
//! - malformed, incomplete, or conflicting decisions fail structurally.
//!
//! Decision state ([`DecisionJournal`]) is intentionally separate from the
//! execution accumulators (`DispatchLedger` / graph state): it is carried
//! through checkpoints additively and can be inspected independently of the
//! execution record.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use concerto_core::types::AgentId;

/// Maximum characters accepted for a decision's task description. The model
/// output is untrusted: anything longer is a rejection, not a silent bound
/// applied after the fact.
pub const MAX_DECISION_TASK_CHARS: usize = 8_000;

/// Maximum characters accepted for a decision's notes.
pub const MAX_DECISION_NOTES_CHARS: usize = 4_000;

/// Maximum evidence ids one decision may cite.
pub const MAX_DECISION_EVIDENCE_IDS: usize = 32;

/// Maximum artifact paths one decision may carry.
pub const MAX_DECISION_ARTIFACT_PATHS: usize = 64;

/// What the Coordinator proposed. Closed set: every variant maps onto an
/// existing dispatch surface, never a new one (issue non-goal: no prompt-
/// system redesign, no new personas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionKind {
    /// Dispatch one registered specialist (`call_specialist`).
    DispatchSpecialist,
    /// Request the bounded work-breakdown (advisory `draft_plan`).
    DraftPlan,
    /// Perform work on the coordinator's own executor (ADR-35 §8).
    SelfExecute,
    /// Abandon the held plan and re-decompose.
    Replan,
    /// Re-dispatch a specialist that failed or returned needs-revision.
    Retry,
    /// A fallback-ladder tier dispatch (ADR-42/ADR-45 — deterministic).
    FallbackTier,
    /// Issue #57: re-cut one OPEN task into ordered children. The children
    /// inherit the parent's specialist role, so no target is named; the
    /// payload rides the decision's `transform` field.
    Split,
    /// Issue #57: fold two or more compatible OPEN tasks into one survivor.
    /// Payload rides `transform`; no target is named (the survivor's role
    /// is the group's shared role).
    Merge,
}

impl DecisionKind {
    /// Whether this kind names a dispatch target.
    pub fn requires_target(self) -> bool {
        matches!(
            self,
            DecisionKind::DispatchSpecialist | DecisionKind::Retry | DecisionKind::FallbackTier
        )
    }

    /// Whether this kind must NOT carry a target (a carry-over makes the
    /// decision self-contradictory — e.g. a Replan naming an agent, or a
    /// split/merge naming a specialist the transform cannot use: children
    /// INHERIT the split parent's role and the merge survivor keeps the
    /// group's).
    pub fn rejects_target(self) -> bool {
        matches!(
            self,
            DecisionKind::DraftPlan
                | DecisionKind::SelfExecute
                | DecisionKind::Replan
                | DecisionKind::Split
                | DecisionKind::Merge
        )
    }

    /// Whether this kind requires a non-empty task description.
    pub fn requires_task(self) -> bool {
        matches!(self, DecisionKind::DispatchSpecialist | DecisionKind::Retry)
    }
}

/// Lifecycle of a decision. Forward transitions:
/// `Pending → Validated → Dispatched → Settled`; any state may go
/// `Rejected` when validation or execution fails to accept or make the
/// decision. The status is decision state — persisted independently of the
/// execution ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionStatus {
    Pending,
    Validated,
    Dispatched,
    Settled,
    Rejected,
}

/// One strategy decision the Coordinator's model produced, reduced to the
/// fields the deterministic machinery acts on. Model-supplied strings are
/// length-bounded; artifact paths are canonical workspace-root-relative keys
/// resolved AT VALIDATION time (never the model's raw spelling).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorDecision {
    /// Unique id (`new_id()`) — the journal and checkpoints address
    /// decisions by this id.
    pub id: String,
    pub kind: DecisionKind,
    /// The agent the decision names, when the kind requires one. `None` is
    /// structural: `Replan`/`DraftPlan`/`SelfExecute` never carry one.
    pub target_agent: Option<AgentId>,
    /// The complete self-contained work text the target receives.
    pub task_description: String,
    pub notes: Option<String>,
    /// Cited whiteboard event ids — every id provably exists in the log
    /// (deduplicated, bounded).
    pub supporting_evidence_ids: Vec<String>,
    /// Canonical workspace-root-relative artifact keys (deduplicated,
    /// bounded).
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    /// Issue #57: the split/merge payload for the transform kinds.
    /// `None` for every non-transform kind. Additive serde (`default`),
    /// so old journal entries and old checkpoints (no field) still load.
    #[serde(default)]
    pub transform: Option<crate::task_transform::TaskTransformSpec>,
    pub created_at: time::OffsetDateTime,
    pub status: DecisionStatus,
}

/// Structured rejection returned to the Coordinator's model on any
/// validation failure — never a panic, never a coordinator crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRejection {
    /// Machine code, also the tool-result `error` key (stable strings the
    /// decision loop and the tests key on).
    pub code: &'static str,
    /// What exactly failed and how to fix it.
    pub message: String,
}

impl DecisionRejection {
    /// The tool-result JSON the Coordinator's model reads.
    pub fn tool_value(&self) -> serde_json::Value {
        serde_json::json!({ "error": self.code, "message": self.message })
    }
}

/// Validate deterministically before any state mutation or dispatch. Pure
/// (no I/O): the caller supplies the authoritative inputs — the registry
/// roster as an id set, the real whiteboard event ids the decision may cite
/// as the evidence set, and the project root for artifact canonicalization.
pub struct DecisionValidator<'a> {
    /// The registry's registered agent ids.
    pub roster_ids: &'a HashSet<String>,
    /// Event ids that genuinely exist in the whiteboard log.
    pub known_event_ids: &'a HashSet<String>,
    /// The project root. `None` is the pure-test fallback: only clean
    /// relative spellings without `..`/absolute prefixes pass (defense in
    /// depth; production always supplies the session's project dir).
    pub project_root: Option<&'a Path>,
}

impl<'a> DecisionValidator<'a> {
    /// Validate one decision proposition into a `Validated`
    /// [`CoordinatorDecision`]. An unregistered target, fabricated evidence,
    /// an escaping artifact path, a length/quantity bound, and any
    /// kind/target conflict all reject with a structured
    /// [`DecisionRejection`].
    pub fn validate(
        &self,
        kind: DecisionKind,
        target: Option<&str>,
        task: &str,
        notes: Option<&str>,
        evidence_ids: &[String],
        artifact_paths: &[String],
    ) -> Result<CoordinatorDecision, DecisionRejection> {
        // ── kind/target agreement (conflicting decisions reject) ───────
        let target = match (kind, target) {
            (kind, Some(_)) if kind.rejects_target() => {
                return Err(DecisionRejection {
                    code: "conflicting_decision",
                    message: format!(
                        "{kind:?} decisions never name a target agent; drop agent_id and \
                         re-decide"
                    ),
                });
            }
            (kind, target) if kind.requires_target() => {
                let Some(id) = target else {
                    return Err(DecisionRejection {
                        code: "incomplete_decision",
                        message: format!(
                            "{kind:?} requires a target agent_id; name a specialist from \
                             the roster"
                        ),
                    });
                };
                let trimmed = id.trim();
                if trimmed.is_empty() {
                    return Err(DecisionRejection {
                        code: "incomplete_decision",
                        message: format!("{kind:?} requires a non-empty target agent_id"),
                    });
                }
                if !self.roster_ids.contains(trimmed) {
                    return Err(DecisionRejection {
                        code: "unknown_agent",
                        message: format!(
                            "no specialist registered for id {trimmed}; the roster in \
                             your context lists every callable agent"
                        ),
                    });
                }
                Some(AgentId::new(trimmed))
            }
            (_, _) => None,
        };

        // ── task description (required kinds + length cap) ─────────────
        let task_trimmed = task.trim();
        if kind.requires_task() && task_trimmed.is_empty() {
            return Err(DecisionRejection {
                code: "incomplete_decision",
                message: format!("{kind:?} requires a non-empty task description"),
            });
        }
        if task_trimmed.chars().count() > MAX_DECISION_TASK_CHARS {
            return Err(DecisionRejection {
                code: "task_too_long",
                message: format!(
                    "the task description exceeds {MAX_DECISION_TASK_CHARS} characters \
                     ({}); shorten it and re-decide",
                    task_trimmed.chars().count(),
                ),
            });
        }

        // ── notes length cap ────────────────────────────────────────────
        if let Some(notes) = notes {
            if notes.chars().count() > MAX_DECISION_NOTES_CHARS {
                return Err(DecisionRejection {
                    code: "notes_too_long",
                    message: format!(
                        "notes exceed {MAX_DECISION_NOTES_CHARS} characters; shorten \
                         them and re-decide"
                    ),
                });
            }
        }

        // ── evidence ids exist in the whiteboard log ───────────────────
        if evidence_ids.len() > MAX_DECISION_EVIDENCE_IDS {
            return Err(DecisionRejection {
                code: "too_many_evidence_ids",
                message: format!(
                    "more than {MAX_DECISION_EVIDENCE_IDS} evidence ids cited ({}); \
                     lower it and re-decide",
                    evidence_ids.len(),
                ),
            });
        }
        let missing: Vec<&String> =
            evidence_ids.iter().filter(|id| !self.known_event_ids.contains(*id)).collect();
        if !missing.is_empty() {
            return Err(DecisionRejection {
                code: "fabricated_evidence",
                message: format!(
                    "evidence ids {missing:?} do not exist in the session's recorded \
                     evidence; cite real event ids from the context (fabricated ids are \
                     rejected)"
                ),
            });
        }

        // ── artifact paths canonicalize inside the project root ────────
        if artifact_paths.len() > MAX_DECISION_ARTIFACT_PATHS {
            return Err(DecisionRejection {
                code: "too_many_artifacts",
                message: format!(
                    "more than {MAX_DECISION_ARTIFACT_PATHS} expected-artifact paths \
                     ({}); lower it and re-decide",
                    artifact_paths.len(),
                ),
            });
        }
        let mut expected_artifacts: Vec<String> = Vec::new();
        for raw in artifact_paths {
            let Some(canonical) = self.canonical_artifact_path(raw) else {
                return Err(DecisionRejection {
                    code: "invalid_artifact_path",
                    message: format!(
                        "expected-artifact path {raw:?} escapes the workspace or is not \
                         resolvable inside it; give a workspace-root-relative path"
                    ),
                });
            };
            if !expected_artifacts.contains(&canonical) {
                expected_artifacts.push(canonical);
            }
        }

        Ok(CoordinatorDecision {
            id: concerto_core::ids::new_id().to_string(),
            kind,
            target_agent: target,
            task_description: task_trimmed.to_owned(),
            notes: notes.map(str::to_owned),
            supporting_evidence_ids: dedupe(evidence_ids),
            expected_artifacts,
            // A transform payload is attached by the coordinator handler
            // AFTER structural validation (the payload's own shape is
            // validated by the task-transform machinery against the real
            // graph state).
            transform: None,
            created_at: time::OffsetDateTime::now_utc(),
            status: DecisionStatus::Validated,
        })
    }

    /// Whether a checkpoint's pending decision has become stale — it names
    /// an agent the roster no longer holds, or cites evidence the log does
    /// not hold. A stale pending decision forces Replan on resume
    /// (deterministic recovery: the model is never asked to repair
    /// checkpoint state).
    pub fn pending_decision_is_stale(
        &self,
        selected_agent: &str,
        supporting_evidence_ids: &[String],
    ) -> bool {
        let agent_unknown = !selected_agent.is_empty() && !self.roster_ids.contains(selected_agent);
        let evidence_unknown =
            supporting_evidence_ids.iter().any(|id| !self.known_event_ids.contains(id));
        agent_unknown || evidence_unknown
    }

    /// The journal record for a structurally rejected pending-decision
    /// continuation (issue #52): a Replan forced on resume because the
    /// checked decision named an unregistered target or cited evidence the
    /// log does not hold. Recorded so the decision state shows WHY the run
    /// replanned.
    #[must_use]
    pub fn replan_rejection_decision(
        selected_agent: &str,
        missing_evidence: &[String],
    ) -> CoordinatorDecision {
        CoordinatorDecision {
            id: concerto_core::ids::new_id().to_string(),
            kind: DecisionKind::Replan,
            target_agent: None,
            task_description: format!(
                "for the pending decision's continuation behind {selected_agent}"
            ),
            notes: Some(format!(
                "issue #52: pending decision is stale ({} unverifiable evidence id(s) \
                 or an unregistered target); the resume cannot stand behind it",
                missing_evidence.len(),
            )),
            supporting_evidence_ids: Vec::new(),
            expected_artifacts: Vec::new(),
            transform: None,
            created_at: time::OffsetDateTime::now_utc(),
            status: DecisionStatus::Rejected,
        }
    }

    /// The journal record for a fallback-ladder attempt that cannot run
    /// (issue #52): the tier target is asserted registered BEFORE any tier
    /// executes (deterministic — the ladder never consults the model). When
    /// the assertion fails the ladder attempt is journaled as Rejected.
    #[must_use]
    pub fn ladder_rejected_decision(original_role: &str) -> CoordinatorDecision {
        CoordinatorDecision {
            id: concerto_core::ids::new_id().to_string(),
            kind: DecisionKind::FallbackTier,
            target_agent: Some(AgentId::new(original_role)),
            task_description: format!(
                "the fallback ladder for {original_role} — assertive tier validation"
            ),
            notes: Some(
                "issue #52: skipped before any tier ran — no registered agent for the \
                 tier target (structured rejection, no execution)"
                    .to_owned(),
            ),
            supporting_evidence_ids: Vec::new(),
            expected_artifacts: Vec::new(),
            transform: None,
            created_at: time::OffsetDateTime::now_utc(),
            status: DecisionStatus::Rejected,
        }
    }

    /// Lexically canonicalize one model-supplied artifact path against the
    /// validator's project root. Absolute paths inside the root, `.`/`..`
    /// segments, and backslashes all normalize; anything escaping the root
    /// yields `None` (rejected by the caller). The validation is the
    /// lexical join — no filesystem access, matching the project's
    /// no-filesystem-touch path-identity convention.
    /// `pub(crate)`: the issue-#57 split/merge handlers reuse the exact
    /// same normalizer for per-child artifact canonicalization so the
    /// decision's path discipline and the transform's artifacts can never
    /// diverge.
    pub(crate) fn canonical_artifact_path(&self, raw: &str) -> Option<String> {
        match self.project_root {
            Some(root) => crate::tool_facts::canonical_project_path(root, raw),
            // Pure-unit fallback: no root supplied — allow only relative
            // spellings that neither escape nor resolve above anything.
            None => lexical_relative_without_escape(raw),
        }
    }
}

/// Deduplicate while preserving order.
fn dedupe(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for id in ids {
        if seen.insert(id.as_str()) {
            out.push(id.clone());
        }
    }
    out
}

/// `Ok(relative)` only when `raw` is relative and carries no `..` component
/// (the no-root fallback used by pure unit tests).
fn lexical_relative_without_escape(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for component in normalized.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if component == ".." {
            stack.pop()?;
        } else {
            stack.push(component);
        }
    }
    if stack.is_empty() {
        None
    } else {
        Some(stack.join("/"))
    }
}

/// The decision journal: the decision-context record, separate from the
/// execution accumulators (`DispatchLedger`, `TaskGraph`). It is order-
/// stable, deduplicated by id, checkpoint-persistable (additively), and
/// inspectable without the execution state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecisionJournal {
    entries: Vec<CoordinatorDecision>,
}

impl DecisionJournal {
    /// Record a decision. An id already journaled (a retry re-recording the
    /// same decision) at the same status is a no-op; a status transition
    /// updates the existing entry in place.
    pub fn record(&mut self, decision: CoordinatorDecision) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == decision.id) {
            existing.status = decision.status;
            return;
        }
        self.entries.push(decision);
    }

    /// Forward status transition on the entry with `id`; no-op when absent
    /// or not forward.
    pub fn transition(&mut self, id: &str, status: DecisionStatus) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if is_forward_status(entry.status, status) {
                entry.status = status;
            }
        }
    }

    pub fn entries(&self) -> &[CoordinatorDecision] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Deserialization from the checkpoint's additive `decision_journal`
    /// field (decision state restored independently of execution state).
    pub fn from_entries(entries: Vec<CoordinatorDecision>) -> Self {
        Self { entries }
    }
}

/// Strict forward-only status ordering; any "backwards" proposal is ignored
/// (a Settled decision can never become Pending again).
fn is_forward_status(from: DecisionStatus, to: DecisionStatus) -> bool {
    let rank = |status: DecisionStatus| match status {
        DecisionStatus::Pending => 0,
        DecisionStatus::Validated => 1,
        DecisionStatus::Dispatched => 2,
        DecisionStatus::Settled => 3,
        DecisionStatus::Rejected => 4,
    };
    rank(to) > rank(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn events(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn validator<'a>(
        roster_ids: &'a HashSet<String>,
        known: &'a HashSet<String>,
        root: Option<&'a Path>,
    ) -> DecisionValidator<'a> {
        DecisionValidator { roster_ids, known_event_ids: known, project_root: root }
    }

    fn dispatch(
        evidence: &[String],
        artifacts: &[String],
    ) -> Result<CoordinatorDecision, DecisionRejection> {
        let roster_ids = roster(&["researcher", "coder"]);
        let known = events(&["ev-0001", "ev-0002"]);
        validator(&roster_ids, &known, Some(Path::new("/tmp/proj"))).validate(
            DecisionKind::DispatchSpecialist,
            Some("coder"),
            "implement the thing",
            Some("use the existing module"),
            evidence,
            artifacts,
        )
    }

    #[test]
    fn valid_dispatch_decision_validates_with_canonical_artifacts() {
        let binding = "src/main.rs";
        let raw = vec!["./src/lib.rs".to_owned(), binding.to_owned()];
        let decision = dispatch(&["ev-0001".to_owned()], &raw).expect("valid");
        assert_eq!(decision.kind, DecisionKind::DispatchSpecialist);
        assert_eq!(decision.target_agent.as_ref().map(AgentId::as_str), Some("coder"));
        assert_eq!(decision.supporting_evidence_ids, vec!["ev-0001"]);
        assert_eq!(decision.expected_artifacts.first().map(String::as_str), Some("src/lib.rs"));
        assert_eq!(decision.status, DecisionStatus::Validated);
    }

    #[test]
    fn incomplete_dispatch_missing_target_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::DispatchSpecialist, None, "work", None, &[], &[])
            .expect_err("missing target");
        assert_eq!(error.code, "incomplete_decision");
    }

    #[test]
    fn unknown_agent_target_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::DispatchSpecialist, Some("ghost"), "work", None, &[], &[])
            .expect_err("outside the roster");
        assert_eq!(error.code, "unknown_agent");
    }

    #[test]
    fn empty_task_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::DispatchSpecialist, Some("coder"), "   ", None, &[], &[])
            .expect_err("empty task");
        assert_eq!(error.code, "incomplete_decision");
    }

    #[test]
    fn oversized_task_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let oversized = "x".repeat(MAX_DECISION_TASK_CHARS + 1);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::DispatchSpecialist, Some("coder"), &oversized, None, &[], &[])
            .expect_err("too long");
        assert_eq!(error.code, "task_too_long");
    }

    #[test]
    fn conflicting_target_on_replan_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::Replan, Some("coder"), "start over", None, &[], &[])
            .expect_err("conflicting");
        assert_eq!(error.code, "conflicting_decision");
    }

    #[test]
    fn incomplete_replan_without_task_is_accepted() {
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let decision = validator(&roster_ids, &known, None)
            .validate(DecisionKind::Replan, None, "", None, &[], &[])
            .expect("replan carries no task requirement");
        assert_eq!(decision.target_agent, None);
    }

    #[test]
    fn fabricated_evidence_rejects() {
        let roster_ids = roster(&["coder"]);
        let known = events(&["ev-real"]);
        let error = validator(&roster_ids, &known, None)
            .validate(
                DecisionKind::DispatchSpecialist,
                Some("coder"),
                "work",
                None,
                &["ev-real".to_owned(), "ev-fabricated".to_owned()],
                &[],
            )
            .expect_err("fabricated");
        assert_eq!(error.code, "fabricated_evidence");
    }

    #[test]
    fn too_many_evidence_ids_reject() {
        let roster_ids = roster(&["coder"]);
        let known =
            (0..MAX_DECISION_EVIDENCE_IDS).map(|i| format!("ev-{i}")).collect::<HashSet<_>>();
        let the_ids: Vec<String> = known.iter().cloned().collect();
        let overflow: String = "ev-overflow".to_owned();
        let mut cited = the_ids.clone();
        cited.push(overflow);
        let error = validator(&roster_ids, &known, None)
            .validate(DecisionKind::DispatchSpecialist, Some("coder"), "work", None, &cited, &[])
            .expect_err("too many");
        assert_eq!(error.code, "too_many_evidence_ids");
    }

    #[test]
    fn traversal_artifact_rejects() {
        let error = dispatch(&[], &["../../etc/passwd".to_owned()]).expect_err("traversal");
        assert_eq!(error.code, "invalid_artifact_path");
    }

    #[test]
    fn absolute_artifact_outside_root_rejects() {
        let error = dispatch(&[], &["/etc/passwd".to_owned()]).expect_err("outside");
        assert_eq!(error.code, "invalid_artifact_path");
    }

    #[test]
    fn absolute_artifact_inside_root_covers() {
        let root = Path::new("/tmp/some-project");
        let roster_ids = roster(&["coder"]);
        let known = events(&[]);
        let decision = validator(&roster_ids, &known, Some(root))
            .validate(
                DecisionKind::DispatchSpecialist,
                Some("coder"),
                "work",
                None,
                &[],
                &["/tmp/some-project/src/main.rs".to_owned()],
            )
            .expect("inside-the-root absolute paths normalize");
        assert_eq!(decision.expected_artifacts, vec!["src/main.rs"]);
    }

    #[test]
    fn duplicate_evidence_and_artifacts_dedupe() {
        let raw =
            vec!["src/main.rs".to_owned(), "src/main.rs".to_owned(), "./src/lib.rs".to_owned()];
        let cited = vec!["ev-0001".to_owned(), "ev-0001".to_owned()];
        let decision = dispatch(&cited, &raw).expect("dupes allowed");
        assert_eq!(decision.supporting_evidence_ids, vec!["ev-0001"]);
        assert_eq!(decision.expected_artifacts.len(), 2, "two distinct keys");
    }

    #[test]
    fn transform_kinds_reject_targets_and_round_trip() {
        // Split/merge never name a target (children inherit the role).
        let error = validator(&roster(&["coder"]), &events(&[]), None)
            .validate(DecisionKind::Split, Some("coder"), "split", None, &[], &[])
            .expect_err("the split kind must reject a target");
        assert_eq!(error.code, "conflicting_decision");

        // The transform payload rides the decision additively.
        let parent = concerto_core::types::TaskId(concerto_core::ids::Ulid::from(0x57u128));
        let decision = validator(&roster(&["coder"]), &events(&[]), None)
            .validate(DecisionKind::Split, None, "split of the parent", None, &[], &[])
            .expect("valid split decision shape");
        let mut decision = decision;
        decision.transform = Some(crate::task_transform::TaskTransformSpec::Split {
            parent,
            children: vec![crate::task_transform::SplitChildSpec {
                description: "child".to_owned(),
                expected_artifacts: vec![],
                after: vec![],
            }],
        });
        let json = serde_json::to_string(&decision).expect("serialize");
        assert!(json.contains("\"type\":\"split\""), "the payload is a tagged split: {json}");
        let back: CoordinatorDecision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, decision, "the payload survives the round trip");

        // Merge kind.
        let decision = validator(&roster(&["coder"]), &events(&[]), None).validate(
            DecisionKind::Merge,
            None,
            "merged work",
            None,
            &[],
            &[],
        );
        assert!(decision.is_ok());
    }

    #[test]
    fn old_journal_entry_without_transform_deserializes_to_none() {
        // A pre-#57 entry: no `transform` key → additive default None.
        // The record is built from a REAL current serialization so every
        // other field matches the wire format exactly; then the additive
        // `transform` key is dropped to simulate the old writer.
        let base = validator(&roster(&["coder"]), &events(&[]), None)
            .validate(DecisionKind::DispatchSpecialist, Some("coder"), "work", None, &[], &[])
            .expect("valid");
        let mut value = serde_json::to_value(&base).expect("serialize");
        assert!(value.get("transform").is_some(), "current entries carry the key");
        let object = value.as_object_mut().expect("an object");
        object.remove("transform");
        let decision: CoordinatorDecision =
            serde_json::from_value(value).expect("an old entry loads");
        assert_eq!(decision.transform, None, "the additive transform defaults");
    }

    #[test]
    fn stale_pending_decision_detected() {
        let roster_ids = roster(&["coder"]);
        let known = events(&["ev-real"]);
        let validator = validator(&roster_ids, &known, None);
        assert!(
            validator.pending_decision_is_stale("removed-agent", &[]),
            "an unregistered target is stale"
        );
        assert!(
            validator.pending_decision_is_stale("coder", &["ev-fabricated".to_owned()]),
            "unknown evidence is stale"
        );
        assert!(
            !validator.pending_decision_is_stale("coder", &["ev-real".to_owned()]),
            "a grounded pending decision is fresh"
        );
    }

    #[test]
    fn journal_transitions_are_forward_only() {
        let mut journal = DecisionJournal::default();
        let decision = dispatch(&[], &[]).expect("valid");
        let id = decision.id.clone();
        journal.record(decision);
        journal.transition(&id, DecisionStatus::Dispatched);
        journal.transition(&id, DecisionStatus::Validated); // backwards — ignored
        journal.transition(&id, DecisionStatus::Settled);
        assert_eq!(journal.entries()[0].status, DecisionStatus::Settled);

        // A second record with the same id updates status forward.
        let mut update = dispatch(&[], &[]).expect("valid again");
        update.id = id.clone();
        update.status = DecisionStatus::Rejected;
        journal.record(update);
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.entries()[0].status, DecisionStatus::Rejected);
    }

    #[test]
    fn decision_round_trips_through_serde() {
        let decision = dispatch(&["ev-0001".to_owned()], &[]).expect("valid");
        let json = serde_json::to_string(&decision).expect("serialize");
        let back: CoordinatorDecision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, decision);
        assert!(json.contains("\"dispatch-specialist\""), "kebab-case kind: {json}");
        assert!(json.contains("\"validated\""), "kebab-case status: {json}");
    }
}
