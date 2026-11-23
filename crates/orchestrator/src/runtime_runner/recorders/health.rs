//! Terminal recorder health is saved directly, outside the bounded event queue.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use concerto_core::event::{Event, EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::recording::{RecorderKind, RecorderOutcome};
use concerto_core::CancellationToken;
use concerto_sessions::SessionStore;

pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cancelling the stop future must not detach a still-running recorder.
struct DrainingTask(tokio::task::JoinHandle<()>);

impl Drop for DrainingTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct RecorderHealth(Mutex<RecorderOutcome>);

impl RecorderHealth {
    pub(super) fn new(recorder: RecorderKind) -> Self {
        Self(Mutex::new(RecorderOutcome {
            recording_id: Ulid::new(),
            recorder,
            dropped_events: 0,
            failed_writes: 0,
            recorder_failed: false,
            report_persisted: false,
        }))
    }

    pub(super) fn dropped(&self, count: u64) {
        let mut health = self.0.lock().unwrap_or_else(|error| error.into_inner());
        health.dropped_events = health.dropped_events.max(count);
    }

    pub(super) fn failed_write(&self) {
        let mut health = self.0.lock().unwrap_or_else(|error| error.into_inner());
        health.failed_writes = health.failed_writes.saturating_add(1);
    }

    pub(super) fn failed_task(&self) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).recorder_failed = true;
    }

    pub(super) async fn persist(
        &self,
        store: &Arc<dyn SessionStore>,
        session_id: Ulid,
        bus: &EventBus,
        cancel: CancellationToken,
    ) -> RecorderOutcome {
        let _cancel_on_drop = cancel.clone().drop_guard();
        let mut outcome = self.0.lock().unwrap_or_else(|error| error.into_inner()).clone();
        // The stored payload describes the successful write. If the write
        // fails, the returned outcome explicitly reports that it was not saved.
        outcome.report_persisted = true;
        let event = Event::new(
            outcome.recording_id,
            session_id,
            EventKind::RecordingFinished { outcome: outcome.clone() },
        );
        match tokio::time::timeout(WRITE_TIMEOUT, store.record_event(session_id, &event, cancel))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                outcome.report_persisted = false;
                tracing::error!(%session_id, %error, "failed to save recorder integrity");
            }
            Err(_) => {
                outcome.report_persisted = false;
                tracing::error!(%session_id, "saving recorder integrity timed out");
            }
        }
        if let Some(warning) = outcome.warning() {
            let _ = bus.publish_for_session(
                session_id,
                outcome.recording_id,
                EventKind::ErrorOccurred { message: format!("Recording incomplete: {warning}.") },
            );
        }
        outcome
    }
}

/// Wait for a bounded drain. Aborting a stuck task is an explicit integrity
/// failure; it cannot silently become a successful recording.
pub(super) async fn drain_task(
    task: &mut Option<tokio::task::JoinHandle<()>>,
    health: &RecorderHealth,
    cancel: &CancellationToken,
) {
    cancel.cancel();
    let Some(task) = task.take() else { return };
    let mut task = DrainingTask(task);
    match tokio::time::timeout(Duration::from_secs(30), &mut task.0).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            health.failed_task();
            tracing::error!(%error, "recorder task failed while draining");
        }
        Err(_) => {
            health.failed_task();
            task.0.abort();
            let _ = (&mut task.0).await;
            tracing::error!("recorder drain timed out");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NotifyDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for NotifyDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_drain_aborts_worker_and_marks_recording_failed() {
        let (sender, dropped) = tokio::sync::oneshot::channel();
        let on_drop = NotifyDrop(Some(sender));
        let mut task = Some(tokio::spawn(async move {
            let _on_drop = on_drop;
            std::future::pending::<()>().await;
        }));
        let health = RecorderHealth::new(RecorderKind::EventLog);
        drain_task(&mut task, &health, &CancellationToken::new()).await;
        assert!(task.is_none());
        assert!(health.0.lock().unwrap().recorder_failed);
        dropped.await.expect("worker must be aborted before stop returns");
    }

    #[tokio::test]
    async fn cancelling_stop_does_not_detach_the_recorder() {
        let (sender, dropped) = tokio::sync::oneshot::channel();
        let (started, draining) = tokio::sync::oneshot::channel();
        let on_drop = NotifyDrop(Some(sender));
        let worker = tokio::spawn(async move {
            let _on_drop = on_drop;
            std::future::pending::<()>().await;
        });
        let stop = tokio::spawn(async move {
            let mut task = Some(worker);
            let health = RecorderHealth::new(RecorderKind::EventLog);
            let _ = started.send(());
            drain_task(&mut task, &health, &CancellationToken::new()).await;
        });
        draining.await.unwrap();
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), dropped).await.unwrap().unwrap();
    }
}
