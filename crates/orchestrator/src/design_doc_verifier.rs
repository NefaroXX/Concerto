//! ADR-65 §5 — deterministic, model-free DesignDoc claim verification.
//!
//! A DesignDoc is an optional CLAIM about the workspace contract a planning
//! stage proposes. The verifier in this module resolves that claim against
//! grounded observations only — the pre-planning `WorkspaceSnapshot` inventory
//! and the session's `ToolExecuted` facts — and decides, with no model, no
//! language detection and no filesystem access, whether the doc binds:
//!
//! - **Verified** — every proposed path resolves to an observed key
//!   (`contract_paths`), so the doc binds. "The design is the repo": a doc
//!   fully grounded by the snapshot / other agents' facts binds even with zero
//!   author reads (ADR-65 §5).
//! - **Quarantined** — at least one proposed path is ungrounded or tree-
//!   conflicting (`UNGROUNDED_PATH` / `TREE_CONFLICT`), or a non-empty doc was
//!   authored against zero observations (`NO_OBSERVATIONS`). The doc stays
//!   advisory; the pipeline degrades, never crashes.
//! - **Skipped** — the claim is empty (no proposed files). An empty doc with
//!   observed workspace is `NO_DESIGN_NEEDED`; with no observations at all it
//!   is `NO_OBSERVATIONS_NO_DESIGN`. Skipping is a decision, not an error.
//!
//! Determinism contract: every rule below is a pure function of
//! `{proposed paths, grounded keys, author read count, cited/known ids}`.
//! The same inputs always yield the same verdict; event ordering does not
//! matter (grounding is a set, reads are counted).

use std::collections::BTreeSet;

use concerto_core::ids::Ulid;
use concerto_core::types::AgentId;
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::{load_whiteboard_events, WhiteboardLoadOpts};
use concerto_sessions::{SessionError, ToolExecutedPayload, WhiteboardKind};
use serde::{Deserialize, Serialize};

use crate::tool_facts::{is_file_affecting_tool, project_root_hash};
use crate::workspace_snapshot::WorkspaceSnapshotRecord;

/// Verdict state of a DesignDoc after deterministic resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesignDocState {
    /// Every proposed path resolved to an observed key; the doc binds.
    Verified,
    /// At least one proposed path is unsupported by observations; the doc is
    /// advisory only.
    Quarantined,
    /// No claim to bind (empty proposed files); a decision, never an error.
    Skipped,
}

impl DesignDocState {
    /// Whether this state makes the DesignDoc a binding contract. Only
    /// `Verified` binds; a Quarantined or Skipped doc never does.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Reason codes for a verdict (kebab-case on the wire, ADR-65 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesignDocReasonCode {
    /// A proposed path resolves to nothing observed in the workspace.
    UngroundedPath,
    /// A proposed path conflicts with an observed path in the same tree
    /// (parent-vs-child collision).
    TreeConflict,
    /// A non-empty doc authored with zero assigned workspace reads.
    NoObservations,
    /// An empty claim documented against an observed workspace (no design
    /// work needed).
    NoDesignNeeded,
    /// An empty claim with no observations at all.
    NoObservationsNoDesign,
    /// The doc cites an evidence id that does not exist in the log
    /// (weight-zero: recorded, never granting).
    UnknownEvidenceRef,
    /// The evidence store was unavailable, so the claim could not be resolved
    /// (fail-soft degraded verdict).
    EvidenceUnavailable,
}

/// One reason attached to a [`DesignDocVerdict`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignDocReason {
    /// The machine-stable reason code.
    pub code: DesignDocReasonCode,
    /// Evidence ids implicated by this reason (empty when not applicable).
    pub evidence_event_ids: Vec<String>,
    /// Human-readable explanation ("why").
    pub note: String,
}

/// The deterministic verdict for one DesignDoc claim (ADR-65 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignDocVerdict {
    /// The resolved state.
    pub state: DesignDocState,
    /// Every reason, in deterministic order.
    pub reasons: Vec<DesignDocReason>,
    /// Number of the doc author's `ToolExecuted` facts classified as reads
    /// (ADR-65 F7).
    pub author_read_count: u64,
    /// Number of proposed paths rejected (ungrounded / tree-conflict /
    /// uncanonicalizable). Machine count for the quarantine report.
    pub reject_count: u64,
    /// The grounded proposed paths. Non-empty **only** when the verdict is
    /// Active (Verified); empty otherwise.
    pub contract_paths: Vec<String>,
}

/// Everything the pure verifier needs — gathered from evidence, but shaped so
/// the model-free decision stays a pure function.
#[derive(Debug, Clone)]
pub struct VerifierInput {
    /// Raw proposed file paths exactly as authored in the DesignDoc.
    pub proposed_paths: Vec<String>,
    /// Canonical project-relative keys observed in the workspace: snapshot
    /// inventory entries + every `ToolExecuted` payload path.
    pub grounded_paths: BTreeSet<String>,
    /// Number of the doc author's attributed read actions.
    pub author_read_count: u64,
    /// Evidence ids the doc cites (the current schema has no citation field,
    /// so this is empty in practice; the rule exists to keep fabricated
    /// citations cost-free when a future doc grows one).
    pub cited_event_ids: Vec<String>,
    /// Every evidence id known to the log, for fabricated-reference detection.
    pub known_event_ids: BTreeSet<String>,
}

/// Lexically normalize a DesignDoc's proposed path into the canonical
/// project-relative key space (ADR-65 F5d, relative branch), WITHOUT a project
/// root: the verifier must stay Independent of the filesystem, so absolute
/// paths (which require the root to interpret) are rejected as
/// unverifiable/ungrounded. Pure and deterministic: no `canonicalize()`, no
/// I/O.
///
/// Returns `None` when the path cannot be expressed as a project-relative key
/// (absolute, `..` escapes above the root, or empty after normalization).
pub fn canonical_claim_path(raw: &str) -> Option<String> {
    let raw_norm = raw.replace('\\', "/");
    let components: Vec<&str> = raw_norm
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    if raw_norm.starts_with('/') {
        // Absolute: interpreting it needs the project root, which the
        // rootless verifier must not consult.
        return None;
    }
    let mut stack: Vec<&str> = Vec::with_capacity(components.len());
    for component in components {
        if component == ".." {
            stack.pop()?;
        } else {
            stack.push(component);
        }
    }
    if stack.is_empty() {
        return None;
    }
    Some(stack.join("/"))
}

/// `true` when the claim lists no proposed files at all — the empty-claim
/// case the verifier skips (never quarantines and never errors).
pub fn is_empty_claim(proposed_paths: &[String]) -> bool {
    proposed_paths.is_empty()
}

/// Whether a `ToolExecuted` command counts as a READ for the author's read
/// count (ADR-65 F7): a served read (served from the read-dedupe cache) or a
/// non-file-affecting tool call. Write/delete/edit tools never count as reads
/// — they mutate, they do not observe.
pub fn classifies_as_read(tool: &str, args: &serde_json::Value, served_from: Option<&str>) -> bool {
    served_from.is_some() || !is_file_affecting_tool(tool, args)
}

/// `true` when a canonical proposed path conflicts with the observed tree: the
/// proposal claims a directory where an observed path nests under it (a
/// grounded `proposed/…` exists — the observed entry proves `proposed` is a
/// real directory), or the proposed path nests under an observed *file*
/// (proposed `dir/file/x` while `dir/file` is observed as a file).
pub fn has_tree_conflict(canonical: &str, grounded_paths: &BTreeSet<String>) -> bool {
    let directory_claimed = format!("{canonical}/");
    if grounded_paths.iter().any(|grounded| grounded.starts_with(&directory_claimed)) {
        return true;
    }
    grounded_paths.iter().any(|grounded| canonical.starts_with(&format!("{grounded}/")))
}

/// The subset of `cited_event_ids` that do not exist in `known_event_ids`.
///
/// Ordered deterministically (the caller's citation order) and weight-zero by
/// contract: a fabricated citation can never turn a verdict into Verified on
/// its own — grounding does that; it only adds an informational reason.
pub fn unknown_cited_refs(
    cited_event_ids: &[String],
    known_event_ids: &BTreeSet<String>,
) -> Vec<String> {
    cited_event_ids.iter().filter(|id| !known_event_ids.contains(*id)).cloned().collect()
}

/// Resolve a DesignDoc claim against grounded evidence.
///
/// Pure and order-independent: only the sets and counts in `input` matter.
/// Returns a verdict for every input — skipped claims never produce an error.
pub fn verify_design_doc(input: &VerifierInput) -> DesignDocVerdict {
    let mut reasons = Vec::new();

    // Fabricated citations: recorded, never granting (rule (e)).
    for unknown in unknown_cited_refs(&input.cited_event_ids, &input.known_event_ids) {
        reasons.push(DesignDocReason {
            code: DesignDocReasonCode::UnknownEvidenceRef,
            evidence_event_ids: vec![unknown],
            note: "the doc cites an evidence id that does not exist in the log; \
                   the citation is recorded and ignored"
                .to_owned(),
        });
    }

    // Empty claim: nothing to contract. Skipping is the decision — never an
    // error, never a quarantine.
    if is_empty_claim(&input.proposed_paths) {
        let (code, note) = if input.author_read_count == 0 {
            (
                DesignDocReasonCode::NoObservationsNoDesign,
                "empty claim and no workspace observations: there is nothing to \
                 design — skipped"
                    .to_owned(),
            )
        } else {
            (
                DesignDocReasonCode::NoDesignNeeded,
                "empty claim with an observed workspace: no design work needed — \
                 skipped"
                    .to_owned(),
            )
        };
        reasons.push(DesignDocReason { code, evidence_event_ids: Vec::new(), note });
        return DesignDocVerdict {
            state: DesignDocState::Skipped,
            reasons,
            author_read_count: input.author_read_count,
            reject_count: 0,
            contract_paths: Vec::new(),
        };
    }

    // Resolve each proposed path against grounding.
    let mut reject_count = 0u64;
    let mut contract_paths = Vec::new();
    for raw in &input.proposed_paths {
        let Some(canonical) = canonical_claim_path(raw) else {
            reject_count += 1;
            reasons.push(DesignDocReason {
                code: DesignDocReasonCode::UngroundedPath,
                evidence_event_ids: Vec::new(),
                note: format!(
                    "proposed path {raw:?} cannot be canonicalized to a \
                               project-relative key and matches nothing observed"
                ),
            });
            continue;
        };
        if input.grounded_paths.contains(&canonical) {
            contract_paths.push(canonical);
            continue;
        }
        reject_count += 1;
        let (code, note) = if has_tree_conflict(&canonical, &input.grounded_paths) {
            (
                DesignDocReasonCode::TreeConflict,
                format!(
                    "proposed path {raw:?} (canonical {canonical}) conflicts with an \
                     observed path in the same tree"
                ),
            )
        } else {
            (
                DesignDocReasonCode::UngroundedPath,
                format!(
                    "proposed path {raw:?} (canonical {canonical}) matches nothing \
                     observed in the workspace"
                ),
            )
        };
        reasons.push(DesignDocReason { code, evidence_event_ids: Vec::new(), note });
    }

    if reject_count > 0 {
        if input.author_read_count == 0 {
            reasons.push(DesignDocReason {
                code: DesignDocReasonCode::NoObservations,
                evidence_event_ids: Vec::new(),
                note: "a non-empty design was produced without any attributed \
                       workspace read; the unsupported paths above are quarantined"
                    .to_owned(),
            });
        }
        // A Quarantined verdict never binds, so no contract paths are revealed.
        return DesignDocVerdict {
            state: DesignDocState::Quarantined,
            reasons,
            author_read_count: input.author_read_count,
            reject_count,
            contract_paths: Vec::new(),
        };
    }

    // Every proposed path is grounded. Zero author reads are fine here — the
    // grounding came from the snapshot or other agents' facts, i.e. "the
    // design is the repo" (ADR-65 §5); with all paths grounded the doc binds.
    DesignDocVerdict {
        state: DesignDocState::Verified,
        reasons,
        author_read_count: input.author_read_count,
        reject_count: 0,
        contract_paths,
    }
}

/// The fail-soft verdict for an unresolvable claim: evidence could not be
/// gathered (store unavailable), so the verifier cannot ground anything. An
/// empty claim is still Skipped; a non-empty claim is Quarantined as
/// `EVIDENCE_UNAVAILABLE` — advisory, never a run failure.
pub fn degraded_verdict(proposed_paths: &[String], author_read_count: u64) -> DesignDocVerdict {
    let unavailable = DesignDocReason {
        code: DesignDocReasonCode::EvidenceUnavailable,
        evidence_event_ids: Vec::new(),
        note: "the evidence store was unavailable, so the claim could not be \
               resolved against any observations; degraded fail-soft"
            .to_owned(),
    };
    if is_empty_claim(proposed_paths) {
        let (code, note) = if author_read_count == 0 {
            (
                DesignDocReasonCode::NoObservationsNoDesign,
                "empty claim and no workspace observations: skipped".to_owned(),
            )
        } else {
            (DesignDocReasonCode::NoDesignNeeded, "empty claim: skipped".to_owned())
        };
        return DesignDocVerdict {
            state: DesignDocState::Skipped,
            reasons: vec![
                unavailable,
                DesignDocReason { code, evidence_event_ids: Vec::new(), note },
            ],
            author_read_count,
            reject_count: 0,
            contract_paths: Vec::new(),
        };
    }
    DesignDocVerdict {
        state: DesignDocState::Quarantined,
        reasons: vec![unavailable],
        author_read_count,
        reject_count: proposed_paths.len() as u64,
        contract_paths: Vec::new(),
    }
}

/// Gather the evidence needed to resolve a DesignDoc claim (ADR-65 §5 read
/// side): snapshot inventory grounding, every `ToolExecuted` fact in the
/// session (path grounding), and the author's attributed read count.
///
/// Scoping rules:
/// - Paths from `ToolExecuted` facts are included only when the fact is scoped
///   to the snapshot's project root (ADR-65 F5c); unrooted legacy facts
///   (`project_root_hash == ""`) and facts from other roots are never used.
///   With no snapshot there is still no cross-root leak: only rooted facts
///   ground.
/// - The author's read count counts only facts attributed to `author`
///   (`event.agent_id`, falling back to the payload's own `agent_id`), marked
///   successful, and classified as reads ([`classifies_as_read`]).
///
/// Fail-soft by contract: the caller may call [`degraded_verdict`] on any
/// `Err` return. Cancellation surfaces as an error so the caller can
/// distinguish it from a store outage.
pub async fn collect_design_doc_evidence(
    pool: Option<&sqlx::SqlitePool>,
    session_id: Ulid,
    author: Option<&AgentId>,
    snapshot: Option<&WorkspaceSnapshotRecord>,
    cancel: &CancellationToken,
) -> Result<VerifierInput, SessionError> {
    let mut grounded_paths = BTreeSet::new();

    // Snapshot grounding: the pre-planning inventory is observation truth the
    // verifier can trust without touching the disk.
    let scope_root = snapshot.map(|s| project_root_hash(s.project_root.as_std_path()));
    if let Some(snapshot) = snapshot {
        for entry in &snapshot.entries {
            grounded_paths.insert(entry.path.clone());
        }
    }

    let mut known_event_ids = BTreeSet::new();
    let mut author_read_count = 0u64;
    let author_id: Option<&str> = author.map(AgentId::as_str);

    if let Some(pool) = pool {
        let events = load_whiteboard_events(
            pool,
            &WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: Some(session_id.to_string()),
                scope: None,
                limit: usize::MAX,
            },
        )
        .await?;
        for event in events {
            if cancel.is_cancelled() {
                return Err(SessionError::Storage("evidence collection cancelled".to_owned()));
            }
            known_event_ids.insert(event.event_id.clone());
            if event.kind != WhiteboardKind::ToolExecuted {
                continue;
            }
            // An undecodable payload is opaque, not evidence (ADR-65 §3):
            // skip it, keep the rest.
            let payload: ToolExecutedPayload = match serde_json::from_value(event.payload.clone()) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            // Root scoping: only facts from THIS project root ground the
            // claim. Unrooted legacy rows are preserved in the log but never
            // used as evidence (ADR-65 F5c).
            let in_scope = match &scope_root {
                Some(root) => payload.project_root_hash == *root,
                None => !payload.project_root_hash.is_empty(),
            };
            if !in_scope {
                continue;
            }
            for entry in &payload.paths {
                grounded_paths.insert(entry.path.clone());
            }
            // Author read count: the author's own successful read-classified
            // facts (ADR-65 F7). Attribution never inferred — check both the
            // event column and the payload's self-declared agent.
            let is_author = author_id.is_some_and(|id| {
                id == payload.agent_id.as_deref().unwrap_or_default() || id == event.agent_id
            });
            if !is_author || !payload.success {
                continue;
            }
            if classifies_as_read(&payload.tool, &payload.args, payload.served_from.as_deref()) {
                author_read_count += 1;
            }
        }
    }

    Ok(VerifierInput {
        proposed_paths: Vec::new(), // filled by the caller's doc
        grounded_paths,
        author_read_count,
        cited_event_ids: Vec::new(),
        known_event_ids,
    })
}

#[cfg(test)]
mod tests {
    use concerto_sessions::ObservedPath;

    use super::*;

    fn grounded(entries: &[&str]) -> BTreeSet<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    fn input(
        proposed: &[&str],
        grounded_paths: BTreeSet<String>,
        author_read_count: u64,
    ) -> VerifierInput {
        VerifierInput {
            proposed_paths: proposed.iter().map(|p| p.to_string()).collect(),
            grounded_paths,
            author_read_count,
            cited_event_ids: Vec::new(),
            known_event_ids: BTreeSet::new(),
        }
    }

    fn skipped_state(verdict: &DesignDocVerdict, code: DesignDocReasonCode) {
        assert_eq!(verdict.state, DesignDocState::Skipped, "state: {:?}", verdict);
        assert!(
            verdict.reasons.iter().any(|reason| reason.code == code),
            "expected reason {code:?}, got: {:?}",
            verdict.reasons
        );
        assert_eq!(verdict.contract_paths, Vec::<String>::new());
        assert_eq!(verdict.reject_count, 0);
    }

    #[test]
    fn canonical_claim_path_normalizes_relative_and_rejects_absolute() {
        assert_eq!(canonical_claim_path("src/main.rs"), Some("src/main.rs".into()));
        assert_eq!(canonical_claim_path("./src/main.rs"), Some("src/main.rs".into()));
        assert_eq!(canonical_claim_path("src\\main.rs"), Some("src/main.rs".into()));
        assert_eq!(canonical_claim_path("a/../b/c.rs"), Some("b/c.rs".into()));
        assert_eq!(canonical_claim_path("a/./b"), Some("a/b".into()));
        // Absolute paths need the project root — the rootless verifier rejects
        // them as ungroundable.
        assert_eq!(canonical_claim_path("/abs/out.rs"), None);
        // `..` escaping above the (rootless) space.
        assert_eq!(canonical_claim_path("../escape.rs"), None);
        // Empty / dot-only / root-only results.
        assert_eq!(canonical_claim_path(""), None);
        assert_eq!(canonical_claim_path("."), None);
        assert_eq!(canonical_claim_path("a/.."), None);
    }

    #[test]
    fn claim_path_matches_project_path_for_relative_inputs() {
        let root = std::path::Path::new("/proj");
        for raw in ["src/main.rs", "./src/main.rs", "a/b/../c.rs", "src\\lib.rs", "deep/nested.rs"]
        {
            let claim = canonical_claim_path(raw).expect("relative claims canonicalize");
            let project =
                crate::tool_facts::canonical_project_path(root, raw).expect("project key");
            assert_eq!(claim, project, "parity for {raw}");
        }
    }

    #[test]
    fn empty_claim_skips_without_observations() {
        let verdict = verify_design_doc(&input(&[], grounded(&[]), 0));
        skipped_state(&verdict, DesignDocReasonCode::NoObservationsNoDesign);
    }

    #[test]
    fn empty_claim_skips_when_agent_read_but_no_design() {
        let verdict = verify_design_doc(&input(&[], grounded(&["src/main.rs"]), 3));
        skipped_state(&verdict, DesignDocReasonCode::NoDesignNeeded);
        assert_eq!(verdict.author_read_count, 3);
    }

    #[test]
    fn cold_doc_quarantines_ungrounded_paths() {
        let verdict = verify_design_doc(&input(&["src/hallucinated.rs"], grounded(&[]), 0));
        assert_eq!(verdict.state, DesignDocState::Quarantined);
        assert_eq!(verdict.reject_count, 1);
        assert!(verdict.contract_paths.is_empty());
        // The Quarantined reason must list the EXACT offending path so a
        // human (or the plan gate) can see what was unsupported — not just a
        // generic "something ungrounded" code.
        let ungrounded = verdict
            .reasons
            .iter()
            .find(|reason| reason.code == DesignDocReasonCode::UngroundedPath)
            .unwrap_or_else(|| panic!("expected UngroundedPath reason: {:?}", verdict.reasons));
        assert!(
            ungrounded.note.contains("src/hallucinated.rs"),
            "the reason note must name the exact ungrounded path: {:?}",
            ungrounded.note
        );
        assert!(
            verdict.reasons.iter().any(|reason| reason.code == DesignDocReasonCode::NoObservations),
            "no-observations context should be recorded: {:?}",
            verdict.reasons
        );
    }

    #[test]
    fn tree_conflict_quarantines_directory_vs_observed_file() {
        // The proposal claims `src` as a (file?) while `src/main.rs` is
        // observed — `src` cannot be a contract path in that tree.
        let verdict = verify_design_doc(&input(&["src"], grounded(&["src/main.rs"]), 2));
        assert_eq!(verdict.state, DesignDocState::Quarantined);
        assert_eq!(verdict.reject_count, 1);
        assert!(
            verdict.reasons.iter().any(|reason| reason.code == DesignDocReasonCode::TreeConflict),
            "got: {:?}",
            verdict.reasons
        );
    }

    #[test]
    fn nested_path_under_observed_file_is_also_a_conflict() {
        let verdict =
            verify_design_doc(&input(&["src/main.rs/x.rs"], grounded(&["src/main.rs"]), 2));
        assert_eq!(verdict.state, DesignDocState::Quarantined);
        assert_eq!(verdict.reject_count, 1);
        assert!(
            verdict.reasons.iter().any(|reason| reason.code == DesignDocReasonCode::TreeConflict),
            "got: {:?}",
            verdict.reasons
        );
    }

    #[test]
    fn grounded_via_snapshot_verifies_under_design_is_the_repo_rule() {
        // Zero author reads, but every path is grounded by the snapshot:
        // "the design is the repo" — binds.
        let verdict = verify_design_doc(&input(
            &["src/main.rs", "src/lib.rs"],
            grounded(&["src/main.rs", "src/lib.rs"]),
            0,
        ));
        assert_eq!(verdict.state, DesignDocState::Verified, "verdict: {:?}", verdict);
        assert_eq!(verdict.reject_count, 0);
        assert_eq!(verdict.contract_paths, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn verified_when_author_read_the_ground() {
        let verdict =
            verify_design_doc(&input(&["src/main.rs"], grounded(&["src/main.rs", "old.rs"]), 4));
        assert_eq!(verdict.state, DesignDocState::Verified);
        assert_eq!(verdict.author_read_count, 4);
        assert_eq!(verdict.contract_paths, vec!["src/main.rs"]);
    }

    #[test]
    fn contract_paths_reveal_only_grounded_and_active() {
        let verdict = verify_design_doc(&input(
            &["src/grounded.rs", "src/phantom.rs"],
            grounded(&["src/grounded.rs"]),
            1,
        ));
        assert_eq!(verdict.state, DesignDocState::Quarantined);
        assert_eq!(verdict.reject_count, 1);
        assert!(verdict.contract_paths.is_empty(), "quarantined verdict reveals no contract paths");
    }

    #[test]
    fn fabricated_citation_never_grants() {
        // Cited id unknown to the log, but grounding is real: the citation is
        // recorded as a weight-zero note and the verdict stays Verified.
        let mut input = input(&["src/main.rs"], grounded(&["src/main.rs"]), 1);
        input.cited_event_ids = vec!["ev-fabricated".to_owned()];
        input.known_event_ids = grounded(&["ev-real"]);
        let verdict = verify_design_doc(&input);
        assert_eq!(verdict.state, DesignDocState::Verified);
        assert!(
            verdict
                .reasons
                .iter()
                .any(|reason| reason.code == DesignDocReasonCode::UnknownEvidenceRef),
            "got: {:?}",
            verdict.reasons
        );
    }

    #[test]
    fn degraded_verdict_quarantines_non_empty_and_skips_empty() {
        let quarantine = degraded_verdict(&["src/main.rs".to_owned()], 0);
        assert_eq!(quarantine.state, DesignDocState::Quarantined);
        assert_eq!(quarantine.reject_count, 1);
        assert!(quarantine.contract_paths.is_empty());
        assert!(quarantine
            .reasons
            .iter()
            .any(|reason| reason.code == DesignDocReasonCode::EvidenceUnavailable));

        let skip = degraded_verdict(&[], 0);
        assert_eq!(skip.state, DesignDocState::Skipped);
        assert_eq!(skip.reject_count, 0);
    }

    #[test]
    fn reads_count_served_and_read_tools_but_not_writes() {
        assert!(classifies_as_read("read_file", &serde_json::json!({}), None));
        assert!(classifies_as_read("grep", &serde_json::json!({}), None));
        assert!(!classifies_as_read("write_file", &serde_json::json!({}), None));
        assert!(!classifies_as_read(
            "filesystem",
            &serde_json::json!({ "operation": "write" }),
            None
        ));
        // A served read is a successful observation regardless of the tool.
        assert!(classifies_as_read("write_file", &serde_json::json!({}), Some("ev-1")));
    }

    #[test]
    fn tree_conflict_helper_distinguishes_collisions() {
        let tree = grounded(&["src/main.rs", "docs/readme.md"]);
        // A proposal of `src` / `docs` claims a directory where observed
        // files nest under it — the observed entries prove those are real
        // directories, so claiming the directory itself is a conflict (it
        // resolves to nothing that exists on disk).
        assert!(has_tree_conflict("src", &tree), "src is a directory over main.rs");
        assert!(has_tree_conflict("docs", &tree), "docs is a directory over readme.md");
        assert!(has_tree_conflict("src/main.rs/x", &tree), "nests under an observed file");
        assert!(!has_tree_conflict("src/lib.rs", &tree), "sibling is fine");
        assert!(!has_tree_conflict("docs/readme.md", &tree), "an exact observed file is fine");
    }

    /// A SQLite whiteboard store with migrations applied, teed to a tempdir so
    /// the DB file lives on disk (never `/tmp`; the repo's target/ layout) and
    /// is cleaned up automatically on drop.
    async fn evidence_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("evidence.db"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    /// Append a `ToolExecuted` fact to the whiteboard log for `session_id`.
    async fn append_executed(
        pool: &sqlx::SqlitePool,
        session_id: &Ulid,
        event_id: &str,
        agent_id: &str,
        payload: concerto_sessions::ToolExecutedPayload,
    ) {
        concerto_sessions::whiteboard::append_whiteboard_event(
            pool,
            &concerto_sessions::whiteboard::NewWhiteboardEvent {
                event_id: event_id.to_owned(),
                agent_id: agent_id.to_owned(),
                kind: WhiteboardKind::ToolExecuted,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload: serde_json::to_value(payload).expect("tool payload serializes"),
                pre_image_hash: None,
                created_at: 100 + event_id.len() as i64,
            },
        )
        .await
        .expect("append ToolExecuted event");
    }

    /// Served-read counting happens in evidence collection (ADR-65 F7): a
    /// served read is a successful observation regardless of the tool, and a
    /// non-file-affecting tool call counts as a read, while a genuine write
    /// never does. Reads attributed to OTHER agents and unrooted facts never
    /// count toward the author.
    #[tokio::test]
    async fn collect_design_doc_evidence_counts_served_reads_and_scopes_by_author() {
        let (_dir, pool) = evidence_pool().await;
        let root = std::path::Path::new("/proj/binding");
        let root_hash = project_root_hash(root);
        let session_id = Ulid::new();
        let cancel = CancellationToken::new();
        let snapshot = WorkspaceSnapshotRecord {
            generation: "gen-1".to_owned(),
            entries: vec![],
            captured_at_ms: 0,
            project_root: camino::Utf8PathBuf::from(root.to_string_lossy().into_owned()),
        };

        // 1. A genuine read counts.
        append_executed(
            &pool,
            &session_id,
            "ev-read",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "read_file".to_owned(),
                args: serde_json::json!({ "path": "src/main.rs" }),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: None,
                paths: vec![ObservedPath {
                    path: "src/main.rs".to_owned(),
                    size_bytes: Some(10),
                    mtime_ms: Some(1),
                    content_hash: None,
                }],
            },
        )
        .await;
        // 2. A genuine WRITE never counts as a read.
        append_executed(
            &pool,
            &session_id,
            "ev-write",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "write_file".to_owned(),
                args: serde_json::json!({ "path": "src/out.rs" }),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: None,
                paths: vec![ObservedPath {
                    path: "src/out.rs".to_owned(),
                    size_bytes: Some(5),
                    mtime_ms: Some(2),
                    content_hash: None,
                }],
            },
        )
        .await;
        // 3. A SERVED read (write tool but served-from a cache) counts — even
        // with an empty paths list, because the observation was served, not
        // executed.
        append_executed(
            &pool,
            &session_id,
            "ev-served",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "write_file".to_owned(),
                args: serde_json::json!({ "path": "src/main.rs" }),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: Some("ev-cache".to_owned()),
                paths: vec![],
            },
        )
        .await;
        // 4. A read by ANOTHER agent never counts toward the author.
        append_executed(
            &pool,
            &session_id,
            "ev-other",
            "researcher",
            ToolExecutedPayload {
                agent_id: Some("researcher".to_owned()),
                task_id: None,
                run_id: None,
                tool: "read_file".to_owned(),
                args: serde_json::json!({}),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: None,
                paths: vec![],
            },
        )
        .await;

        let input = collect_design_doc_evidence(
            Some(&pool),
            session_id,
            Some(&AgentId::new("coder")),
            Some(&snapshot),
            &cancel,
        )
        .await
        .expect("evidence collected");

        // Reads counted: ev-read (read_file), ev-served (served write). The
        // genuine write and the other-agent read are excluded.
        assert_eq!(input.author_read_count, 2, "author_read_count: {:?}", input.author_read_count);
        // Only the two grounded paths from in-scope facts are in the ground
        // (the unrooted/missing ones are excluded by root scoping).
        assert!(
            input.grounded_paths.contains("src/main.rs"),
            "served read's payloads still ground the path: {:?}",
            input.grounded_paths
        );
        assert!(
            input.grounded_paths.contains("src/out.rs"),
            "write facts still ground the path: {:?}",
            input.grounded_paths
        );
    }

    /// Snapshot inventory grounds every entry regardless of author reads, and
    /// ToolExecuted facts scoped to the same project root add further ground
    /// while cross-root facts never leak in (ADR-65 F5c).
    #[tokio::test]
    async fn collect_design_doc_evidence_grounds_from_snapshot_and_scoped_facts() {
        let (_dir, pool) = evidence_pool().await;
        let root = std::path::Path::new("/proj/snapshot");
        let root_hash = project_root_hash(root);
        let session_id = Ulid::new();
        let cancel = CancellationToken::new();
        let snapshot = WorkspaceSnapshotRecord {
            generation: "gen-1".to_owned(),
            entries: vec![ObservedPath {
                path: "src/existing.rs".to_owned(),
                size_bytes: Some(42),
                mtime_ms: Some(1),
                content_hash: None,
            }],
            captured_at_ms: 0,
            project_root: camino::Utf8PathBuf::from(root.to_string_lossy().into_owned()),
        };

        // In-scope fact that grounds an ADDITIONAL path not in the snapshot.
        append_executed(
            &pool,
            &session_id,
            "ev-fact",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "read_file".to_owned(),
                args: serde_json::json!({}),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: None,
                paths: vec![ObservedPath {
                    path: "src/observed.rs".to_owned(),
                    size_bytes: Some(1),
                    mtime_ms: Some(2),
                    content_hash: None,
                }],
            },
        )
        .await;
        // Cross-root fact (wrong project_root_hash) must NOT ground.
        append_executed(
            &pool,
            &session_id,
            "ev-foreign",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "read_file".to_owned(),
                args: serde_json::json!({}),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: "foreign-root-hash".to_owned(),
                served_from: None,
                paths: vec![ObservedPath {
                    path: "elsewhere.rs".to_owned(),
                    size_bytes: Some(1),
                    mtime_ms: Some(2),
                    content_hash: None,
                }],
            },
        )
        .await;

        let input = collect_design_doc_evidence(
            Some(&pool),
            session_id,
            Some(&AgentId::new("coder")),
            Some(&snapshot),
            &cancel,
        )
        .await
        .expect("evidence collected");

        assert!(input.grounded_paths.contains("src/existing.rs"), "snapshot grounds");
        assert!(input.grounded_paths.contains("src/observed.rs"), "scoped fact grounds");
        assert!(
            !input.grounded_paths.contains("elsewhere.rs"),
            "cross-root facts never leak into grounding: {:?}",
            input.grounded_paths
        );
    }

    /// The full claim route end-to-end: snapshot grounding + the author's
    /// served read are combined by the coordinator-facing evidence gatherer,
    /// and `verify_design_doc` binds the doc (ADR-65 §5 "the design is the
    /// repo").
    #[tokio::test]
    async fn snapshot_grounding_drives_a_binding_verdict_through_evidence() {
        let (_dir, pool) = evidence_pool().await;
        let root = std::path::Path::new("/proj/grounded");
        let root_hash = project_root_hash(root);
        let session_id = Ulid::new();
        let cancel = CancellationToken::new();
        let snapshot = WorkspaceSnapshotRecord {
            generation: "gen-1".to_owned(),
            entries: vec![ObservedPath {
                path: "src/main.rs".to_owned(),
                size_bytes: Some(7),
                mtime_ms: Some(1),
                content_hash: None,
            }],
            captured_at_ms: 0,
            project_root: camino::Utf8PathBuf::from(root.to_string_lossy().into_owned()),
        };
        append_executed(
            &pool,
            &session_id,
            "ev-served",
            "coder",
            ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "write_file".to_owned(),
                args: serde_json::json!({}),
                success: true,
                exit_code: Some(0),
                generation: "gen-1".to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: Some("ev-cache".to_owned()),
                paths: vec![],
            },
        )
        .await;

        let mut input = collect_design_doc_evidence(
            Some(&pool),
            session_id,
            Some(&AgentId::new("coder")),
            Some(&snapshot),
            &cancel,
        )
        .await
        .expect("evidence collected");
        input.proposed_paths = vec!["src/main.rs".to_owned()];

        let verdict = verify_design_doc(&input);
        assert_eq!(verdict.state, DesignDocState::Verified, "verdict: {:?}", verdict);
        assert!(verdict.state.is_active());
        assert_eq!(verdict.contract_paths, vec!["src/main.rs"]);
        assert_eq!(verdict.author_read_count, 1, "the served read counted");
    }
}
