//! On-demand evidence details. Switching or closing drops the previous load.

use std::collections::BTreeSet;
use std::sync::{mpsc, Arc};

use concerto_core::{ids::Ulid, CancellationToken};
use concerto_orchestrator::studio_runtime::StudioRuntimeSnapshot;
use concerto_sessions::{SessionStore, WhiteboardEvent};
use serde_json::{json, Value};

pub(super) fn report(session_id: Ulid, event_id: &str, event: Option<WhiteboardEvent>) -> Value {
    json!({
        "schema_version": 1,
        "session_id": session_id,
        "event_id": event_id,
        "source": "whiteboard_event",
        "note": "Stored log record, not live state or a fresh verification. It may be newer than the saved checkpoint.",
        "found": event.is_some(),
        "event": event,
    })
}

pub(super) fn render_report(report: &Value) -> anyhow::Result<String> {
    let mut text = format!("Evidence event {}\n{}\n", report["event_id"], report["note"]);
    if report["found"] == false {
        text.push_str("\nEvent not found in this session. The reference may identify a decision rather than a log event, or the record may be unavailable.\n");
    } else {
        text.push_str(&serde_json::to_string_pretty(&report["event"])?);
    }
    Ok(text)
}

/// Collect explicit reference fields; never mistake arbitrary payload strings
/// for event IDs. Keep decision IDs available even when they do not resolve.
fn references(snapshot: &StudioRuntimeSnapshot) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for decision in &snapshot.decisions {
        ids.extend(decision.supporting_evidence_ids.iter().cloned());
    }
    if let Some(world) = &snapshot.world_model {
        for fact in &world.facts {
            ids.insert(fact.ref_id.clone());
            ids.extend(fact.grounded_by.iter().cloned());
            ids.extend(fact.contradicted_by.iter().cloned());
        }
        for artifact in &world.artifacts {
            ids.extend(artifact.last_ref.iter().cloned());
        }
        for question in &world.questions {
            ids.extend(question.opened_ref.iter().cloned());
            ids.extend(question.resolved_by.iter().cloned());
        }
    }
    ids.into_iter().filter(|id| !id.trim().is_empty()).collect()
}

#[derive(Default)]
pub(crate) struct EvidenceDetails {
    pub(crate) ids: Vec<String>,
    pub(crate) index: usize,
    pub(crate) text: Option<String>,
    session_id: Option<Ulid>,
    store: Option<Arc<dyn SessionStore>>,
    cancel: CancellationToken,
    receiver: Option<mpsc::Receiver<Result<Value, String>>>,
}

impl EvidenceDetails {
    pub(super) fn new(session_id: Ulid, store: Arc<dyn SessionStore>) -> Self {
        Self {
            ids: Vec::new(), index: 0, text: None,
            session_id: Some(session_id), store: Some(store),
            cancel: CancellationToken::new(), receiver: None,
        }
    }

    pub(super) fn set_snapshot(&mut self, snapshot: &StudioRuntimeSnapshot) {
        self.close();
        self.ids = references(snapshot);
        self.index = 0;
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.ids.get(self.index).map(String::as_str)
    }

    pub(crate) fn select_next(&mut self, backwards: bool) {
        self.close();
        if !self.ids.is_empty() {
            let count = self.ids.len();
            let step = if backwards { count - 1 } else { 1 };
            self.index = (self.index + step) % count;
        }
    }

    pub(crate) fn open(&mut self) {
        self.close();
        let (Some(session_id), Some(store), Some(event_id)) =
            (self.session_id, self.store.clone(), self.selected().map(str::to_owned))
        else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.text = Some("Evidence inspection is unavailable outside the terminal runtime.".into());
            return;
        };
        self.text = Some(format!("Loading evidence {}…", json!(event_id)));
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        runtime.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = store.load_evidence_event(session_id, &event_id, cancel.clone()) => {
                    result.map(|event| report(session_id, &event_id, event))
                        .map_err(|error| error.to_string())
                }
            };
            if !cancel.is_cancelled() {
                let _ = sender.send(result);
            }
        });
    }

    pub(super) fn poll(&mut self) {
        let Some(receiver) = &self.receiver else { return };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Evidence load interrupted; press r to retry.".into()),
        };
        self.receiver = None;
        self.text = Some(match result {
            Ok(value) => render_report(&value).unwrap_or_else(|error| error.to_string()),
            Err(error) => format!("Evidence inspection unavailable: {}", error.escape_debug()),
        });
    }

    pub(crate) fn close(&mut self) {
        self.cancel.cancel();
        self.receiver = None;
        self.text = None;
    }
}

impl Drop for EvidenceDetails {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_orchestrator::world_model::{FactStatus, WorldFact, WorldModel};

    #[test]
    fn references_are_sorted_deduplicated_and_keep_contradictions() {
        let snapshot = StudioRuntimeSnapshot {
            world_model: Some(WorldModel {
                facts: vec![WorldFact {
                    ref_id: "fact".into(), label: "not-an-event-id".into(),
                    status: FactStatus::Contradicted, artifact: None, seq: 2,
                    grounded_by: vec!["grounding".into(), "fact".into(), "".into()],
                    contradicted_by: Some("contradiction".into()), contradicted_exit_code: Some(1),
                }],
                ..WorldModel::default()
            }),
            ..StudioRuntimeSnapshot::default()
        };
        assert_eq!(references(&snapshot), vec!["contradiction", "fact", "grounding"]);
    }

    #[test]
    fn closing_or_changing_selection_discards_late_results() {
        let mut details = EvidenceDetails::default();
        details.ids.extend(["a".into(), "b".into()]);
        let (sender, receiver) = mpsc::channel();
        details.receiver = Some(receiver);
        details.text = Some("Loading".into());
        let cancelled = details.cancel.clone();
        details.select_next(false);
        assert!(cancelled.is_cancelled());
        assert_eq!(details.selected(), Some("b"));
        assert!(sender.send(Ok(report(Ulid::new(), "a", None))).is_err());
        details.poll();
        assert!(details.text.is_none());
        details.select_next(false);
        assert_eq!(details.selected(), Some("a"));
        details.select_next(true);
        assert_eq!(details.selected(), Some("b"));
        let (sender, receiver) = mpsc::channel();
        details.receiver = Some(receiver);
        sender.send(Ok(report(Ulid::new(), "b", None))).unwrap();
        details.poll();
        assert!(details.text.as_deref().unwrap().contains("not found in this session"));
        details.close();
        assert!(details.text.is_none());
        assert!(details.receiver.is_none());
    }

    #[test]
    fn report_preserves_payload_provenance_and_escapes_terminal_controls() {
        let event = WhiteboardEvent {
            event_id: "event-1".into(), gate_seq: 12, agent_id: "coder".into(), agent_seq: 3,
            kind: concerto_sessions::WhiteboardKind::Finding, scope: "src".into(),
            session_id: Some("session".into()), plan_id: Some("plan".into()),
            causation: Some("event-0".into()), payload: json!({"text": "\u{1b}[31m\nresult"}),
            content_hash: "hash".into(), pre_image_hash: None, created_at: 42,
        };
        let value = report(Ulid::new(), "event-1", Some(event.clone()));
        assert_eq!(value["event"], json!(event));
        assert_eq!(value["found"], true);
        let rendered = render_report(&value).unwrap();
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("not live state"));
        let missing = report(Ulid::new(), "missing", None);
        assert_eq!(missing["found"], false);
        assert!(missing["event"].is_null());
    }
}
