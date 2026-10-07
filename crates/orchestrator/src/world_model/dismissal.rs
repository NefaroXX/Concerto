//! The dismissal advisory cluster (W2: Q-DISMISS-KIND, Q-DISMISS-EVIDENCE) —
//! the pure rules the coordinator consults BEFORE accepting a
//! `dismiss_question` decision, extracted verbatim from `world_model.rs`.
//!
//! Every item here is deterministic over its value inputs only: no store, no
//! event log, no policy engine, no async I/O. The coordinator resolves cited
//! ids against the whiteboard log elsewhere and passes the resolved values in
//! (`opened_gate_seq`, `&[CitedEvidence]`), so the rules themselves stay
//! testable in isolation and fail closed when the log could not be read.
//!
//! Behavior is byte-identical to the pre-extraction bodies: refusal codes,
//! messages, evidence-class handling and the freshness boundary are unchanged.

use concerto_sessions::whiteboard::ToolOutcome;
use concerto_sessions::WhiteboardKind;

use super::QuestionKind;

/// W2 (Q-DISMISS-KIND): why `kind` refuses coordinator dismissal — or
/// `None` when judgment may dismiss it.
///
/// Only `OpenProblem` and `MissingEvidence` exit through judgment: both ask
/// something no observation in the run can settle on its own (a recovery
/// nobody has produced, a corrected approach nobody has recorded), so the
/// coordinator weighs them. `BlockedPath` and `AmbiguousRecovery` are
/// answerable by observation — a fresh clean observation of the path, or a
/// pending dispatch that is no longer ambiguous — so an observation they
/// cannot be judged away: they answer a structured `not_dismissable`
/// naming why instead. Pure and deterministic: the kind alone.
#[must_use]
pub fn dismissal_kind_refusal(kind: QuestionKind) -> Option<crate::decisions::DecisionRejection> {
    match kind {
        QuestionKind::OpenProblem | QuestionKind::MissingEvidence => None,
        QuestionKind::BlockedPath => Some(crate::decisions::DecisionRejection {
            code: "not_dismissable",
            message: format!(
                "a {kind:?} question is not dismissable: it resolves from observation, never \
                 from judgment — a fresh clean observation of the blocked path supersedes the \
                 standing dirt and closes it, so re-verify the path instead of dismissing"
            ),
        }),
        QuestionKind::AmbiguousRecovery => Some(crate::decisions::DecisionRejection {
            code: "not_dismissable",
            message: format!(
                "a {kind:?} question is not dismissable: it resolves from observation, never \
                 from judgment — whether the pending dispatch completed is observed, not \
                 decided, so verify the dispatch instead of dismissing"
            ),
        }),
    }
}

/// W2 (Q-DISMISS-EVIDENCE): one cited dismissal-evidence id resolved against
/// the whiteboard log — the shape the evidence rule reads. `kind` is `None`
/// when the row is missing or carries a kind this build does not recognize;
/// neither is ever observed-class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitedEvidence {
    /// The cited event id, echoed in a refusal so the model can fix it.
    pub id: String,
    /// The recorded event kind, `None` for an unreadable or unknown row.
    pub kind: Option<WhiteboardKind>,
    /// The recorded global order coordinate (the log assigns it).
    pub gate_seq: u64,
    /// The W1-classified outcome of a `ToolExecuted` payload; the class
    /// check reads it only for that kind.
    pub outcome: ToolOutcome,
}

impl CitedEvidence {
    /// W2 (Q-DISMISS-EVIDENCE): observed-class — a `WriteApplied`, or a
    /// `ToolExecuted` whose recorded outcome is `ok`. `Finding`,
    /// `Decision`, `DesignDoc` and every other kind are assertions or
    /// records of judgment, never observations, so they never count as
    /// dismissal evidence.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        match self.kind {
            Some(WhiteboardKind::WriteApplied) => true,
            Some(WhiteboardKind::ToolExecuted) => self.outcome == ToolOutcome::Ok,
            _ => false,
        }
    }
}

/// W2 (Q-DISMISS-EVIDENCE): the evidence rule for a dismissal, evaluated
/// pure over cited ids already resolved against the log: at least one id,
/// every id observed-class ([`CitedEvidence::is_observed`]), and — when the
/// question carries its first-sighting `opened_gate_seq` — at least one
/// cited event NEWER than that coordinate. An old checkpoint's `None`
/// relaxes ONLY the freshness clause; `cited = None` (the log could not be
/// read) fails closed, so a dismissal never rides unverifiable evidence.
/// Deterministic over its inputs: ids, kinds, outcomes, seqs.
#[must_use]
pub fn dismissal_evidence_refusal(
    opened_gate_seq: Option<u64>,
    cited: Option<&[CitedEvidence]>,
) -> Option<crate::decisions::DecisionRejection> {
    const CODE: &str = "dismissal_requires_observed_evidence";
    let Some(cited) = cited else {
        return Some(crate::decisions::DecisionRejection {
            code: CODE,
            message: "the cited evidence could not be verified against this run's whiteboard \
                      log; cite a real observed event id from the context"
                .to_owned(),
        });
    };
    if cited.is_empty() {
        return Some(crate::decisions::DecisionRejection {
            code: CODE,
            message: "dismiss_question requires at least one supporting evidence id: cite a \
                      real write-applied or tool-executed (outcome ok) event recorded after \
                      the question opened — a reason alone never dismisses a question"
                .to_owned(),
        });
    }
    let observed: Vec<&CitedEvidence> = cited.iter().filter(|entry| entry.is_observed()).collect();
    if observed.is_empty() {
        // Bounded echo (cites are already count/length-bounded upstream, and
        // the message travels back to the model, never into the render).
        let listed = cited
            .iter()
            .take(4)
            .map(|entry| {
                let kind = entry
                    .kind
                    .map_or_else(|| "unknown".to_owned(), |kind| kind.as_str().to_owned());
                format!("{} ({kind})", entry.id)
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Some(crate::decisions::DecisionRejection {
            code: CODE,
            message: format!(
                "cited evidence [{listed}] is not observed-class: only a write-applied or a \
                 tool-executed event with outcome ok counts — findings, decisions and design \
                 docs are assertions, not observations; cite an observed event"
            ),
        });
    }
    if let Some(opened) = opened_gate_seq {
        let newest = observed.iter().map(|entry| entry.gate_seq).max().unwrap_or(opened);
        if newest <= opened {
            return Some(crate::decisions::DecisionRejection {
                code: "dismissal_evidence_not_newer",
                message: format!(
                    "the cited observed evidence is not newer than the question's first \
                     sighting (opened at gate_seq {opened}, newest cited at gate_seq \
                     {newest}); cite an event recorded after the question opened"
                ),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W2 (Q-DISMISS-KIND): only the two judgment kinds pass; the two
    /// observation-answerable kinds refuse with a code and a reason that
    /// names the observation that would close them. Pure over the kind.
    #[test]
    fn dismissal_kind_refusal_covers_only_judgment_kinds() {
        for kind in [QuestionKind::OpenProblem, QuestionKind::MissingEvidence] {
            assert_eq!(dismissal_kind_refusal(kind), None, "{kind:?} may be judged");
        }
        for kind in [QuestionKind::BlockedPath, QuestionKind::AmbiguousRecovery] {
            let refusal = dismissal_kind_refusal(kind).expect("observation kinds refuse");
            assert_eq!(refusal.code, "not_dismissable");
            assert!(
                refusal.message.contains("observation"),
                "{kind:?} names why it cannot be judged away: {}",
                refusal.message
            );
            assert!(refusal.message.contains(&format!("{kind:?}")));
        }
    }

    /// W2 (Q-DISMISS-EVIDENCE): the observed-class rule — a `WriteApplied`
    /// and a `ToolExecuted` with outcome `ok` count; a failed execution,
    /// every assertion kind, and an unreadable row never do.
    #[test]
    fn dismissal_evidence_class_counts_only_observed_rows() {
        let cited = |kind: Option<WhiteboardKind>, outcome| CitedEvidence {
            id: "ev-1".to_owned(),
            kind,
            gate_seq: 7,
            outcome,
        };
        assert!(cited(Some(WhiteboardKind::WriteApplied), ToolOutcome::Unknown).is_observed());
        assert!(cited(Some(WhiteboardKind::ToolExecuted), ToolOutcome::Ok).is_observed());
        assert!(!cited(Some(WhiteboardKind::ToolExecuted), ToolOutcome::Failed).is_observed());
        assert!(!cited(None, ToolOutcome::Ok).is_observed(), "an unreadable row is never observed");
        for kind in [WhiteboardKind::Finding, WhiteboardKind::Decision, WhiteboardKind::DesignDoc] {
            assert!(
                !cited(Some(kind), ToolOutcome::Ok).is_observed(),
                "{kind:?} is an assertion, never an observation"
            );
        }
    }

    /// W2 (Q-DISMISS-EVIDENCE): the evidence rule over its inputs alone —
    /// empty cites, non-class cites and unverifiable cites refuse; observed
    /// cites newer than the opening coordinate pass; `None` coordinates
    /// relax freshness only; every refusal names its own code.
    #[test]
    fn dismissal_evidence_refusal_rules_are_pure() {
        let observed = |gate_seq| CitedEvidence {
            id: "ev-ok".to_owned(),
            kind: Some(WhiteboardKind::ToolExecuted),
            gate_seq,
            outcome: ToolOutcome::Ok,
        };
        let asserted = |gate_seq| CitedEvidence {
            id: "ev-finding".to_owned(),
            kind: Some(WhiteboardKind::Finding),
            gate_seq,
            outcome: ToolOutcome::Ok,
        };

        // Fail closed: an unreadable log never justifies a dismissal.
        let unverifiable =
            dismissal_evidence_refusal(Some(0), None).expect("no log means no dismissal");
        assert_eq!(unverifiable.code, "dismissal_requires_observed_evidence");

        // Count rule: no ids at all.
        let empty = dismissal_evidence_refusal(Some(0), Some(&[])).expect("empty refuses");
        assert_eq!(empty.code, "dismissal_requires_observed_evidence");
        assert!(empty.message.contains("at least one supporting evidence id"));

        // Class rule: only assertions cited, however recent.
        let asserted_only = dismissal_evidence_refusal(Some(0), Some(&[asserted(99)]))
            .expect("assertions never dismiss");
        assert_eq!(asserted_only.code, "dismissal_requires_observed_evidence");
        assert!(asserted_only.message.contains("not observed-class"));

        // Freshness rule: observed, but from before the question opened.
        let stale = dismissal_evidence_refusal(Some(10), Some(&[observed(10), observed(4)]))
            .expect("nothing newer than the opening");
        assert_eq!(stale.code, "dismissal_evidence_not_newer");

        // Mixed citations: every id must be observed AND one must be newer.
        let mixed = dismissal_evidence_refusal(Some(10), Some(&[asserted(99), observed(4)]))
            .expect("a stale observed cite is still stale");
        assert_eq!(mixed.code, "dismissal_evidence_not_newer");

        // Acceptance: one observed id recorded after the opening coordinate.
        assert_eq!(dismissal_evidence_refusal(Some(10), Some(&[observed(11)])), None);
        assert_eq!(dismissal_evidence_refusal(Some(10), Some(&[asserted(99), observed(11)])), None);

        // Transition: no coordinate relaxes freshness, never the class.
        assert_eq!(dismissal_evidence_refusal(None, Some(&[observed(0)])), None);
        assert_eq!(
            dismissal_evidence_refusal(None, Some(&[asserted(99)]))
                .expect("class still binds without a coordinate")
                .code,
            "dismissal_requires_observed_evidence"
        );
    }
}
