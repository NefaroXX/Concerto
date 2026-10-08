//! Event-recorder / transcript-recorder coverage for `EventRecorderStore`.
//!
//! Mechanical extraction (NORM S15): the `EventRecorderStore` mock — its 28
//! `unimplemented!` arms of the `SessionStore` stub wall — and the tests that
//! drive it moved verbatim out of `runtime_runner::runtime_runner_tests`, so
//! test names and assertions are unchanged. `use super::*;` keeps the
//! production recorders in scope. The mock is `pub(super)` because the two
//! stage-tracker tests still in `runtime_runner_tests` construct it.

use super::*;
use concerto_core::event::Event;
use concerto_core::transcript::{GateLabels, TranscriptToolStatus};

// NORM S24A: the memory-init production-path cluster (the two `MEMORY_INIT_*`
// serial locks, `XdgDataHomeGuard`, and the two `init_path_*` tests) lives in
// `memory_init.rs`. This file is loaded via `#[path]` as
// `runtime_runner::tests`, so the submodule needs an explicit `#[path]` too.
#[path = "memory_init.rs"]
mod memory_init;

// ------------------------------------------------------------------
// Mock SessionStore for event-recorder testing
// ------------------------------------------------------------------

/// Records `record_event` and `append_transcript` calls; all other methods
/// panic if invoked.
pub(super) struct EventRecorderStore {
    events: Arc<Mutex<Vec<concerto_core::event::Event>>>,
    transcript: Arc<Mutex<Vec<concerto_core::transcript::TranscriptEntry>>>,
}

impl EventRecorderStore {
    pub(super) fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            transcript: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn recorded_events(&self) -> Vec<concerto_core::event::Event> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn recorded_transcript(&self) -> Vec<concerto_core::transcript::TranscriptEntry> {
        self.transcript.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait::async_trait]
impl SessionStore for EventRecorderStore {
    async fn record_event(
        &self,
        _session_id: Ulid,
        event: &concerto_core::event::Event,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event.clone());
        Ok(())
    }

    async fn create_session(
        &self,
        _project_dir: &camino::Utf8Path,
        _provider: &str,
        _model: &str,
        _cancel: CancellationToken,
    ) -> Result<concerto_sessions::Session, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_session(
        &self,
        _id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Option<concerto_sessions::Session>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn save_message(
        &self,
        _session_id: Ulid,
        _msg: &Message,
        _tokens_in: u64,
        _tokens_out: u64,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn append_messages(
        &self,
        _session_id: Ulid,
        _messages: &[Message],
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_messages(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<Message>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_recent_sessions(
        &self,
        _limit: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_sessions_older_than(
        &self,
        _before_unix: i64,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn delete_session(
        &self,
        _id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<bool, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn active_session_ids(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<Ulid>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_sessions_for_project(
        &self,
        _project_dir: &camino::Utf8Path,
        _limit: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn get_active_session_for_project(
        &self,
        _project_dir: &camino::Utf8Path,
        _cancel: CancellationToken,
    ) -> Result<Option<Ulid>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn set_active_session_for_project(
        &self,
        _project_dir: &camino::Utf8Path,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn record_metrics(
        &self,
        _session_id: Ulid,
        _metrics: ProviderMetrics,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_events(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::replay::StoredEvent>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_events_until(
        &self,
        _session_id: Ulid,
        _max_seq: u64,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::replay::StoredEvent>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn record_spend(
        &self,
        _record: concerto_sessions::spend::SpendRecord,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_spend_records(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::spend::SpendRecord>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn spend_summary(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<concerto_sessions::spend::SpendSummary, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn create_task(
        &self,
        _task: &concerto_core::types::AgentTask,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn update_task_status(
        &self,
        _task_id: concerto_core::types::TaskId,
        _status: &str,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn get_task(
        &self,
        _task_id: concerto_core::types::TaskId,
        _cancel: CancellationToken,
    ) -> Result<Option<concerto_core::types::AgentTask>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_tasks(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::types::AgentTask>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn create_checkpoint(
        &self,
        _session_id: Ulid,
        _task_id: concerto_core::types::TaskId,
        _label: &str,
        _vfs_snapshot: &str,
        _sequence_num: u64,
        _cancel: CancellationToken,
    ) -> Result<Ulid, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_checkpoint(
        &self,
        _checkpoint_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<(String, u64), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn list_checkpoints(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_sessions::CheckpointSummary>, concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn save_orchestration_checkpoint(
        &self,
        _record: &concerto_sessions::OrchestrationCheckpointRecord,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn load_orchestration_checkpoint(
        &self,
        _session_id: Ulid,
    ) -> Result<
        Option<concerto_sessions::OrchestrationCheckpointRecord>,
        concerto_sessions::SessionError,
    > {
        unimplemented!("not expected in this test")
    }

    async fn clear_orchestration_checkpoint(
        &self,
        _session_id: Ulid,
    ) -> Result<(), concerto_sessions::SessionError> {
        unimplemented!("not expected in this test")
    }

    async fn append_transcript(
        &self,
        _session_id: Ulid,
        entries: &[concerto_core::transcript::TranscriptEntry],
        _cancel: CancellationToken,
    ) -> Result<(), concerto_sessions::SessionError> {
        self.transcript.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(entries);
        Ok(())
    }

    async fn load_transcript(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::transcript::TranscriptEntry>, concerto_sessions::SessionError>
    {
        Ok(self.recorded_transcript())
    }
}

#[tokio::test]
async fn event_recorder_filters_cross_session_events() {
    let bus = EventBus::default();
    let recorder_store = Arc::new(EventRecorderStore::new());
    let store: Arc<dyn SessionStore> = recorder_store.clone();

    let session_a = Ulid::new();
    let session_b = Ulid::new();

    let recorder = start_event_recorder(&bus, store, session_a);

    // Publish an event for session A — should be persisted.
    bus.publish(concerto_core::event::Event::new(
        Ulid::new(),
        session_a,
        concerto_core::event::EventKind::SessionSaved,
    ))
    .ok();

    // Publish an event for session B — should be filtered out.
    bus.publish(concerto_core::event::Event::new(
        Ulid::new(),
        session_b,
        concerto_core::event::EventKind::SessionSaved,
    ))
    .ok();

    // Stop the recorder — this flushes any buffered events and waits for
    // the background task to complete.
    recorder.stop().await;

    let recorded = recorder_store.recorded_events();
    assert_eq!(recorded.len(), 1, "only one event should be recorded");
    assert_eq!(recorded[0].session_id, session_a, "recorded event must belong to session A");
}

// ------------------------------------------------------------------
// Transcript recorder (ADR-36, stage 2)
// ------------------------------------------------------------------

fn transcript_store() -> (Arc<EventRecorderStore>, Arc<dyn SessionStore>, Ulid, EventBus) {
    let recorder_store = Arc::new(EventRecorderStore::new());
    let store: Arc<dyn SessionStore> = recorder_store.clone();
    (recorder_store, store, Ulid::new(), EventBus::default())
}

/// Publish a tool-lifecycle event for `session_id` on `bus` (helper).
fn publish(bus: &EventBus, session_id: Ulid, kind: EventKind) {
    bus.publish(Event::new(Ulid::new(), session_id, kind)).ok();
}

#[tokio::test]
async fn transcript_recorder_correlates_tool_calls_into_single_entries() {
    let (recorder_store, store, session_id, bus) = transcript_store();
    let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

    // Started + Finished(success) → one Completed entry.
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "read_file".into(),
            input_hash: "h1".into(),
            detail: Some("read src/main.rs".into()),
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionFinished {
            tool_name: "read_file".into(),
            duration_ms: 3,
            success: true,
            detail: Some("read 42 bytes".into()),
        },
    );

    // Started + ApprovalResolved(approved=true) → one Allowed entry.
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "write_file".into(),
            input_hash: "h2".into(),
            detail: Some("write notes.md".into()),
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ApprovalResolved { tool_name: "write_file".into(), approved: true },
    );

    // Started + ToolTimeout → one Failed entry.
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "shell".into(),
            input_hash: "h3".into(),
            detail: Some("cargo build".into()),
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ToolTimeout { tool_name: "shell".into(), timeout_secs: 30 },
    );

    // Started + ApprovalTimeout → one Cancelled entry.
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "git".into(),
            input_hash: "h4".into(),
            detail: Some("git status".into()),
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ApprovalTimeout { tool_name: "git".into(), timeout_secs: 60 },
    );

    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(
        transcript,
        vec![
            TranscriptEntry::ToolCall {
                tool_name: "read_file".into(),
                detail: "read src/main.rs\nread 42 bytes".into(),
                status: TranscriptToolStatus::Completed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "write_file".into(),
                detail: "write notes.md".into(),
                status: TranscriptToolStatus::Allowed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: "cargo build".into(),
                status: TranscriptToolStatus::Failed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "git".into(),
                detail: "git status".into(),
                status: TranscriptToolStatus::Cancelled,
            },
        ],
        "one entry per invocation with the terminal status merged in"
    );
}

#[tokio::test]
async fn transcript_recorder_preserves_event_order_across_merge() {
    // A Running tool call holds its position: a thinking line published
    // after the tool started must appear after the merged ToolCall entry.
    let (recorder_store, store, session_id, bus) = transcript_store();
    let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

    publish(
        &bus,
        session_id,
        EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "plan".into(),
            kind: concerto_core::event::ThinkingKind::Detail,
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "fs_write".into(),
            input_hash: "h1".into(),
            detail: Some("write main.rs".into()),
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "observing result".into(),
            kind: concerto_core::event::ThinkingKind::Detail,
        },
    );
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionFinished {
            tool_name: "fs_write".into(),
            duration_ms: 5,
            success: true,
            detail: Some("wrote 42 bytes".into()),
        },
    );

    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(
        transcript,
        vec![
            TranscriptEntry::Thinking {
                agent: "coder".into(),
                content: "plan".into(),
                kind: concerto_core::event::ThinkingKind::Detail,
            },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs\nwrote 42 bytes".into(),
                status: TranscriptToolStatus::Completed,
            },
            TranscriptEntry::Thinking {
                agent: "coder".into(),
                content: "observing result".into(),
                kind: concerto_core::event::ThinkingKind::Detail,
            },
        ],
        "terminal event merges in place; interleaved lines keep their position"
    );
}

#[tokio::test]
async fn transcript_recorder_settles_running_tool_calls_on_stop() {
    let (recorder_store, store, session_id, bus) = transcript_store();
    let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

    // Only the start event is published: at stop the Running entry must
    // settle to Cancelled (ADR-36 settle-on-stop).
    publish(
        &bus,
        session_id,
        EventKind::ToolExecutionStarted {
            tool_name: "fs_write".into(),
            input_hash: "h1".into(),
            detail: Some("write main.rs".into()),
        },
    );

    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(
        transcript,
        vec![TranscriptEntry::ToolCall {
            tool_name: "fs_write".into(),
            detail: "write main.rs".into(),
            status: TranscriptToolStatus::Cancelled,
        }]
    );
}

#[tokio::test]
async fn transcript_recorder_records_user_and_final_entries() {
    // Guard-API level: the run-start user prompt and the run-end assistant
    // + completion entries are recorded explicitly (ADR-36 §4).
    let (recorder_store, store, session_id, bus) = transcript_store();
    let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

    recorder.record_user_message("build the widget".to_string()).await;
    recorder
        .append_entries(&[
            TranscriptEntry::Assistant { content: "done".into() },
            TranscriptEntry::Completion {
                multi_agent: false,
                completed: true,
                files: vec!["main.rs".into()],
                project_root: Some("/tmp/proj".into()),
            },
        ])
        .await;
    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(
        transcript,
        vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Assistant { content: "done".into() },
            TranscriptEntry::Completion {
                multi_agent: false,
                completed: true,
                files: vec!["main.rs".into()],
                project_root: Some("/tmp/proj".into()),
            },
        ],
        "transcript shape is [User, Assistant, Completion]"
    );
}

#[tokio::test]
async fn transcript_recorder_flushes_batches_in_order() {
    // More than the batch threshold of transcript-relevant events: the
    // recorder flushes an in-order mid-run batch and the stop flush must
    // not reorder or drop any entry.
    let (recorder_store, store, session_id, bus) = transcript_store();
    let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

    for i in 0..40 {
        publish(
            &bus,
            session_id,
            EventKind::AgentThought {
                agent_id: "coder".into(),
                content: format!("step {i}"),
                kind: concerto_core::event::ThinkingKind::Detail,
            },
        );
    }
    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(transcript.len(), 40, "no entry may be lost across batch flush");
    for (i, entry) in transcript.iter().enumerate() {
        assert_eq!(
            *entry,
            TranscriptEntry::Thinking {
                agent: "coder".into(),
                content: format!("step {i}"),
                kind: concerto_core::event::ThinkingKind::Detail,
            }
        );
    }
}

#[tokio::test]
async fn transcript_recorder_filters_cross_session_events() {
    let (recorder_store, store, _session_id, bus) = transcript_store();
    let session_a = Ulid::new();
    let session_b = Ulid::new();

    let recorder = start_transcript_recorder(&bus, store, session_a, GateLabels::default());

    publish(
        &bus,
        session_a,
        EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "step one".into(),
            kind: concerto_core::event::ThinkingKind::Detail,
        },
    );
    publish(
        &bus,
        session_b,
        EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "other session".into(),
            kind: concerto_core::event::ThinkingKind::Detail,
        },
    );

    recorder.stop().await;

    let transcript = recorder_store.recorded_transcript();
    assert_eq!(transcript.len(), 1, "only this session's events should be recorded");
    assert_eq!(
        transcript[0],
        TranscriptEntry::Thinking {
            agent: "coder".into(),
            content: "step one".into(),
            kind: concerto_core::event::ThinkingKind::Detail,
        }
    );
}

// NORM S24B: the project-switch memory-isolation / memory-optional test
// cluster (the three `Dummy*` stand-ins, `active_memory_for`, and the six
// tests they drive) lives in `memory_cache.rs`. This file is loaded via
// `#[path]` as `runtime_runner::tests`, so the submodule needs an explicit
// `#[path]` too.
#[path = "memory_cache.rs"]
mod memory_cache;
