//! Portable Coordinator inspection, shared by command mode and the TUI.

use std::sync::{mpsc, Arc};

use concerto_core::{ids::Ulid, CancellationToken};
use concerto_orchestrator::studio_runtime::{
    CheckpointStudioRuntimeReader, StudioRuntimeReader, StudioRuntimeSnapshot,
};
use concerto_sessions::SessionStore;
use serde_json::{json, Value};

const USAGE: &str = "usage: concerto sessions runtime <session-id> [--view all|decisions|world|diagnoses|suitability] [--limit 1..1000] [--json]";
const DEFAULT_LIMIT: usize = 50;
const SOURCE_NOTE: &str = "Persisted checkpoint; refresh to reload. This is not live run state.\nCompleted or cleared checkpoints may be unavailable.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    Decisions,
    World,
    Diagnoses,
    Suitability,
}

struct Options {
    session_id: Ulid,
    view: View,
    limit: usize,
    json: bool,
}

fn parse_args(args: &[String]) -> anyhow::Result<Options> {
    let id = args.first().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let session_id =
        id.parse().map_err(|_| anyhow::anyhow!("invalid session id '{id}'; {USAGE}"))?;
    let mut options = Options { session_id, view: View::All, limit: DEFAULT_LIMIT, json: false };
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--view" | "--limit" => {
                let flag = args[index].as_str();
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("{flag} requires a value; {USAGE}"))?;
                if flag == "--view" {
                    options.view = match value.as_str() {
                        "all" => View::All,
                        "decisions" => View::Decisions,
                        "world" => View::World,
                        "diagnoses" => View::Diagnoses,
                        "suitability" => View::Suitability,
                        _ => anyhow::bail!("unknown runtime view '{value}'; {USAGE}"),
                    };
                } else {
                    options.limit =
                        value.parse().map_err(|_| anyhow::anyhow!("invalid limit; {USAGE}"))?;
                    if !(1..=1000).contains(&options.limit) {
                        anyhow::bail!("limit must be between 1 and 1000; {USAGE}");
                    }
                }
            }
            other => anyhow::bail!("unknown runtime option '{other}'; {USAGE}"),
        }
        index += 1;
    }
    Ok(options)
}

pub(crate) fn run(args: &[String]) -> anyhow::Result<()> {
    if args.len() == 1 && matches!(args[0].as_str(), "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let options = parse_args(args)?;
    let rt = tokio::runtime::Runtime::new()?;
    let snapshot = rt.block_on(async {
        let cancel = CancellationToken::new();
        tokio::select! {
            result = async {
                let store = Arc::new(concerto_sessions::SqliteSessionStore::connect_read_only(cancel.clone()).await?);
                if store.load_session(options.session_id, cancel.clone()).await?.is_none() {
                    anyhow::bail!("session {} not found", options.session_id);
                }
                let reader = CheckpointStudioRuntimeReader::new(store);
                Ok::<_, anyhow::Error>(reader.load(options.session_id, cancel.clone()).await?)
            } => result,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                anyhow::bail!("runtime inspection cancelled")
            }
        }
    })?;
    let report = report(&snapshot, &options);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", render_text(&report)?);
    }
    Ok(())
}

/// Limit history newest-first and rankings best-first. Preserve the full world
/// model, including evidence references; do not silently drop nested fields.
fn report(snapshot: &StudioRuntimeSnapshot, options: &Options) -> Value {
    let mut data = serde_json::Map::new();
    if matches!(options.view, View::All | View::Decisions) {
        data.insert(
            "decisions".into(),
            json!(snapshot.decisions.iter().rev().take(options.limit).collect::<Vec<_>>()),
        );
    }
    if matches!(options.view, View::All | View::World) {
        data.insert("world_model".into(), json!(snapshot.world_model));
    }
    if matches!(options.view, View::All | View::Diagnoses) {
        data.insert(
            "diagnoses".into(),
            json!(snapshot.diagnoses.iter().rev().take(options.limit).collect::<Vec<_>>()),
        );
    }
    if matches!(options.view, View::All | View::Suitability) {
        let mut rankings = snapshot.suitability.clone();
        for ranking in &mut rankings {
            ranking.agents.truncate(options.limit);
        }
        data.insert("suitability".into(), json!(rankings));
    }
    json!({
        "schema_version": 1,
        "session_id": options.session_id.to_string(),
        "source": "persisted_checkpoint",
        "note": SOURCE_NOTE,
        "empty": snapshot.is_empty(),
        "limit": options.limit,
        "totals": {
            "decisions": snapshot.decisions.len(),
            "diagnoses": snapshot.diagnoses.len(),
            "suitability": snapshot.suitability.iter().map(|ranking| json!({
                "task_class": ranking.task_class,
                "agents": ranking.agents.len(),
            })).collect::<Vec<_>>(),
        },
        "data": data,
    })
}

fn render_text(report: &Value) -> anyhow::Result<String> {
    let mut text =
        format!("Session {}\n{SOURCE_NOTE}\n", report["session_id"].as_str().unwrap_or("unknown"));
    if report["empty"] == true {
        text.push_str("\nNo Coordinator inspection data in the available checkpoint.\n");
    }
    for (key, title) in [
        ("decisions", "Decision journal (newest first)"),
        ("world_model", "World model"),
        ("diagnoses", "Failure diagnoses (newest first)"),
        ("suitability", "Agent suitability (best first per task class)"),
    ] {
        if let Some(value) = report["data"].get(key) {
            text.push_str(&format!("\n{title}\n{}\n", serde_json::to_string_pretty(value)?));
        }
    }
    text.push_str(&format!(
        "\nLimit: {} rows per history / agents per task class. Totals: {}\n",
        report["limit"], report["totals"]
    ));
    Ok(text)
}

/// One cancellable, non-blocking TUI load. A refresh drops the previous
/// receiver, so late results can never overwrite a newer session's view.
pub(crate) struct RuntimeInspector {
    pub(crate) text: String,
    pub(crate) scroll: u16,
    session_id: Option<Ulid>,
    cancel: CancellationToken,
    receiver: Option<mpsc::Receiver<String>>,
}

impl Default for RuntimeInspector {
    fn default() -> Self {
        Self {
            text: "No active session. Start or resume a session to inspect it.".into(),
            scroll: 0,
            session_id: None,
            cancel: CancellationToken::new(),
            receiver: None,
        }
    }
}

impl RuntimeInspector {
    pub(crate) fn load(&mut self, session_id: Option<Ulid>, store: Option<Arc<dyn SessionStore>>) {
        self.cancel.cancel();
        *self = Self::default();
        let (Some(id), Some(store)) = (session_id, store) else { return };
        self.session_id = Some(id);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.text = "Runtime inspection is unavailable outside the terminal runtime.".into();
            return;
        };
        self.text = format!("Loading persisted checkpoint for session {id}…");
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        runtime.spawn(async move {
            let reader = CheckpointStudioRuntimeReader::new(store);
            let result = async {
                let snapshot = reader.load(id, cancel.clone()).await?;
                let options =
                    Options { session_id: id, view: View::All, limit: DEFAULT_LIMIT, json: false };
                render_text(&report(&snapshot, &options))
            }
            .await;
            if !cancel.is_cancelled() {
                let text = result
                    .unwrap_or_else(|error| format!("Runtime inspection unavailable: {error}"));
                let _ = sender.send(text);
            }
        });
    }

    pub(crate) fn poll(&mut self, active_session: Option<Ulid>) {
        if self.session_id != active_session {
            self.cancel.cancel();
            *self = Self::default();
            return;
        }
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(text) => {
                    self.text = text;
                    self.receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.text = "Runtime inspection interrupted. Press r to retry.".into();
                    self.receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }
}

impl Drop for RuntimeInspector {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_orchestrator::failure_diagnosis::{FailureDiagnosis, FailureKind};
    use concerto_orchestrator::world_model::WorldModel;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn invalid_arguments_fail_before_opening_storage() {
        let id = Ulid::new().to_string();
        for values in [
            vec![],
            vec!["invalid"],
            vec![&id, "--limit", "0"],
            vec![&id, "--limit", "1001"],
            vec![&id, "--limit", "no"],
            vec![&id, "--view"],
            vec![&id, "--view", "live"],
            vec![&id, "--unknown"],
            vec![&id, "extra"],
        ] {
            assert!(parse_args(&args(&values)).is_err(), "{values:?}");
        }
        let options =
            parse_args(&args(&[&id, "--json", "--view", "world", "--limit", "3"])).unwrap();
        assert!(options.json);
        assert_eq!(options.view, View::World);
        assert_eq!(options.limit, 3);
    }

    #[test]
    fn report_limits_newest_history_and_preserves_evidence_and_totals() {
        let diagnoses = ["old", "new"]
            .into_iter()
            .map(|code| FailureDiagnosis {
                kind: FailureKind::Tool,
                code: code.into(),
                transient: true,
                retryable: true,
                same_agent_viable: true,
                alternate_agent_viable: false,
                replan_required: false,
                evidence: "evidence-123".into(),
                decision_id: Some("decision-456".into()),
                artifact_path: None,
            })
            .collect();
        let snapshot = StudioRuntimeSnapshot {
            diagnoses,
            world_model: Some(WorldModel {
                objective: Some("ship it".into()),
                ..WorldModel::default()
            }),
            ..StudioRuntimeSnapshot::default()
        };
        let options =
            Options { session_id: Ulid::new(), view: View::Diagnoses, limit: 1, json: true };
        let value = report(&snapshot, &options);
        assert_eq!(value["totals"]["diagnoses"], 2);
        assert_eq!(value["data"]["diagnoses"].as_array().unwrap().len(), 1);
        assert_eq!(value["data"]["diagnoses"][0]["code"], "new");
        assert_eq!(value["data"]["diagnoses"][0]["evidence"], "evidence-123");
        assert_eq!(value["data"]["diagnoses"][0]["decision_id"], "decision-456");
        assert!(value["data"].get("world_model").is_none());
        assert_eq!(value["source"], "persisted_checkpoint");
        assert!(render_text(&value).unwrap().contains("not live run state"));
    }

    #[test]
    fn session_change_discards_late_inspector_results_and_cancels_load() {
        let (sender, receiver) = mpsc::channel();
        let mut inspector = RuntimeInspector::default();
        inspector.session_id = Some(Ulid::new());
        inspector.receiver = Some(receiver);
        let cancel = inspector.cancel.clone();
        sender.send("other session's state".into()).unwrap();
        inspector.poll(Some(Ulid::new()));
        assert!(cancel.is_cancelled());
        assert!(!inspector.text.contains("other session"));
        assert!(inspector.receiver.is_none());
    }

    #[test]
    fn refresh_and_close_cancel_the_previous_load() {
        let mut inspector = RuntimeInspector::default();
        let previous = inspector.cancel.clone();
        inspector.load(None, None);
        assert!(previous.is_cancelled());
        let current = inspector.cancel.clone();
        drop(inspector);
        assert!(current.is_cancelled());
    }
}
