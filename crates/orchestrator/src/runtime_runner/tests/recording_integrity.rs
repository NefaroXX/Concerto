//! Real runtime recorder overflow, failure and durable integrity reporting.

use super::*;
use concerto_core::recording::{RecorderKind, RecorderOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(super) struct WritePause {
    used: AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl WritePause {
    fn new() -> Self {
        Self {
            used: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    pub(super) async fn wait_once(&self) {
        if !self.used.swap(true, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
    }

    async fn wait_until_started(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.started.notified()).await.unwrap();
    }
}

fn thought() -> EventKind {
    EventKind::AgentThought {
        agent_id: "coder".into(),
        content: "progress".into(),
        kind: concerto_core::event::ThinkingKind::Detail,
    }
}

fn assert_report(store: &EventRecorderStore, outcome: &RecorderOutcome) {
    assert!(store.recorded_events().iter().any(|event| {
        matches!(&event.kind, EventKind::RecordingFinished { outcome: stored } if stored == outcome)
    }), "the returned health must match the durable event payload");
}

async fn overflow(kind: RecorderKind) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.db");
    let durable =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&path).await.unwrap());
    let session = durable
        .create_session(camino::Utf8Path::new("."), "mock", "mock", CancellationToken::new())
        .await
        .unwrap();
    let pause = Arc::new(WritePause::new());
    let mut fixture = EventRecorderStore::new();
    fixture.report_store = Some(durable.clone());
    match kind {
        RecorderKind::EventLog => fixture.event_pause = Some(pause.clone()),
        RecorderKind::Transcript => fixture.transcript_pause = Some(pause.clone()),
    }
    let fixture = Arc::new(fixture);
    let store: Arc<dyn SessionStore> = fixture.clone();
    let bus = EventBus::default();
    // The independent export subscription keeps up even while the session
    // recorder is stalled. Its zero drop count cannot certify database health.
    let mut export = bus.subscribe_durable();
    let event_recorder = (kind == RecorderKind::EventLog)
        .then(|| start_event_recorder(&bus, store.clone(), session.id));
    let transcript_recorder = (kind == RecorderKind::Transcript)
        .then(|| start_transcript_recorder(&bus, store, session.id, GateLabels::default()));
    let initial = if kind == RecorderKind::Transcript { 32 } else { 1 };
    for _ in 0..initial {
        publish(&bus, session.id, thought());
        export.try_recv().unwrap();
    }
    pause.wait_until_started().await;
    for _ in 0..export.capacity() + 7 {
        publish(&bus, session.id, thought());
        export.try_recv().unwrap();
    }
    assert_eq!(export.dropped_events(), 0);
    pause.release.add_permits(1);
    let outcome = match (event_recorder, transcript_recorder) {
        (Some(recorder), None) => recorder.stop().await,
        (None, Some(recorder)) => recorder.stop().await,
        _ => panic!("one recorder per scenario"),
    };
    assert_eq!(outcome.recorder, kind);
    assert_eq!(outcome.dropped_events, 7);
    assert_eq!(outcome.failed_writes, 0);
    assert!(!outcome.recorder_failed);
    assert!(outcome.report_persisted);
    assert!(!outcome.is_complete());
    assert_report(&fixture, &outcome);
    drop(fixture);
    drop(durable);
    let reopened = concerto_sessions::SqliteSessionStore::connect_path(&path).await.unwrap();
    let events = reopened.load_events(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(events.len(), 1, "terminal integrity survives database reopen");
    assert!(matches!(events[0].to_event().unwrap().kind,
        EventKind::RecordingFinished { outcome: stored } if stored == outcome));
}

#[tokio::test]
async fn event_overflow_is_reported_even_when_export_keeps_up() {
    overflow(RecorderKind::EventLog).await;
}

#[tokio::test]
async fn transcript_overflow_is_reported_even_when_export_keeps_up() {
    overflow(RecorderKind::Transcript).await;
}

#[tokio::test]
async fn failed_event_and_final_transcript_writes_degrade_recording() {
    let bus = EventBus::default();
    let session_id = Ulid::new();
    let mut fixture = EventRecorderStore::new();
    fixture.fail_events = true;
    fixture.fail_transcript = true;
    let fixture = Arc::new(fixture);
    let event_recorder = start_event_recorder(&bus, fixture.clone(), session_id);
    let transcript_recorder =
        start_transcript_recorder(&bus, fixture.clone(), session_id, GateLabels::default());
    transcript_recorder.record_user_message("start".into()).await;
    publish(&bus, session_id, EventKind::SessionSaved);
    let events = event_recorder.stop().await;
    let transcript =
        transcript_recorder.finish(&[TranscriptEntry::Assistant { content: "done".into() }]).await;
    assert_eq!(events.failed_writes, 1);
    assert_eq!(transcript.failed_writes, 2, "initial and final transcript writes failed");
    assert!(!events.is_complete() && !transcript.is_complete());
    assert_report(&fixture, &events);
    assert_report(&fixture, &transcript);
}

#[tokio::test]
async fn failed_integrity_persistence_is_visible_to_the_caller() {
    let bus = EventBus::default();
    let mut fixture = EventRecorderStore::new();
    fixture.fail_report = true;
    let recorder = start_event_recorder(&bus, Arc::new(fixture), Ulid::new());
    let outcome = recorder.stop().await;
    assert!(!outcome.report_persisted);
    assert!(!outcome.is_complete());
    assert!(outcome.warning().unwrap().contains("integrity status could not be saved"));
}

#[tokio::test(start_paused = true)]
async fn stalled_event_write_times_out_and_records_incomplete_history() {
    let bus = EventBus::default();
    let session_id = Ulid::new();
    let pause = Arc::new(WritePause::new());
    let mut fixture = EventRecorderStore::new();
    fixture.event_pause = Some(pause.clone());
    let fixture = Arc::new(fixture);
    let recorder = start_event_recorder(&bus, fixture.clone(), session_id);
    publish(&bus, session_id, EventKind::SessionSaved);
    pause.wait_until_started().await;
    // Never release the write. The timeout must drop it and let shutdown
    // persist an explicit failure instead of hanging or claiming success.
    let outcome = recorder.stop().await;
    assert_eq!(outcome.failed_writes, 1);
    assert!(!outcome.recorder_failed);
    assert!(outcome.report_persisted);
    assert!(!outcome.is_complete());
    assert_report(&fixture, &outcome);
}

#[tokio::test]
async fn healthy_recorders_report_complete_and_persist_both_reports() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.db");
    let store = Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&path).await.unwrap());
    let session = store
        .create_session(camino::Utf8Path::new("."), "mock", "mock", CancellationToken::new())
        .await
        .unwrap();
    let bus = EventBus::default();
    let event_recorder = start_event_recorder(&bus, store.clone(), session.id);
    let transcript_recorder =
        start_transcript_recorder(&bus, store.clone(), session.id, GateLabels::default());
    transcript_recorder.record_user_message("start".into()).await;
    publish(&bus, session.id, EventKind::SessionSaved);
    let transcript =
        transcript_recorder.finish(&[TranscriptEntry::Assistant { content: "done".into() }]).await;
    let event_log = event_recorder.stop().await;
    assert!(transcript.is_complete() && event_log.is_complete());
    drop(store);
    let reopened = concerto_sessions::SqliteSessionStore::connect_path(&path).await.unwrap();
    let events = reopened.load_events(session.id, CancellationToken::new()).await.unwrap();
    let reports: Vec<_> = events
        .iter()
        .filter_map(|event| match event.to_event().unwrap().kind {
            EventKind::RecordingFinished { outcome } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(reports, [transcript, event_log]);
    assert_eq!(
        reopened.load_transcript(session.id, CancellationToken::new()).await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn recorder_task_panic_cannot_report_complete_history() {
    let bus = EventBus::default();
    let session_id = Ulid::new();
    let mut fixture = EventRecorderStore::new();
    fixture.panic_event = true;
    let fixture = Arc::new(fixture);
    let recorder = start_event_recorder(&bus, fixture.clone(), session_id);
    publish(&bus, session_id, EventKind::SessionSaved);
    let outcome = recorder.stop().await;
    assert!(outcome.recorder_failed);
    assert!(!outcome.is_complete());
    assert_report(&fixture, &outcome);
}
