//! Durable event capture with an awaited drain before querying benchmark evidence.

use crate::{SessionError, SessionStore};
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct EventRecording {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), SessionError>>>,
    dropped: Arc<AtomicU64>,
    session_id: Ulid,
}

/// Tracks durable-queue overflow observed by the recording task.
///
/// The core queue is allowed to drop its oldest events when the recorder falls
/// behind (bounded-memory contract). Each increase in the cumulative drop count
/// is a gap in the session event history, so we accumulate the total and log at
/// most one error per observed gap, rate-limited to the first gap and each
/// doubling of the cumulative count.
struct GapObserver {
    observed: u64,
    logged: u64,
    total: Arc<AtomicU64>,
    session_id: Ulid,
}

impl GapObserver {
    fn new(total: Arc<AtomicU64>, session_id: Ulid) -> Self {
        Self { observed: 0, logged: 0, total, session_id }
    }

    fn should_log(&self, current: u64) -> bool {
        self.logged == 0 || current >= self.logged.saturating_mul(2)
    }

    fn observe(&mut self, current: u64, capacity: usize) {
        if current <= self.observed {
            return;
        }
        let gap = current - self.observed;
        self.observed = current;
        let total = self.total.fetch_add(gap, Ordering::Relaxed).saturating_add(gap);
        if self.should_log(current) {
            self.logged = current;
            tracing::error!(
                session_id = %self.session_id,
                dropped = current,
                gap,
                total,
                capacity,
                "session event history is incomplete: durable queue overflowed"
            );
        }
    }
}

/// Fail-closed error returned by [`EventRecording::finish`] when the durable
/// recorder observed overflow. Naming both the session and the count lets the
/// run boundary report that the audit trail is incomplete instead of silently
/// accepting the gap.
fn gap_error(session_id: Ulid, dropped: u64) -> SessionError {
    SessionError::Storage(format!(
        "session {session_id} event history is incomplete: durable queue overflowed and dropped {dropped} event(s)"
    ))
}

impl EventRecording {
    pub fn start(bus: &EventBus, store: Arc<dyn SessionStore>, session_id: Ulid) -> Self {
        let mut receiver = bus.subscribe_durable();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let task_dropped = Arc::clone(&dropped);
        let task = tokio::spawn(async move {
            let mut gap = GapObserver::new(task_dropped, session_id);
            loop {
                let next = tokio::select! {
                    _ = stopped.cancelled() => {
                        while let Ok(event) = receiver.try_recv() {
                            gap.observe(receiver.dropped_events(), receiver.capacity());
                            if event.session_id == session_id {
                                store.record_event(session_id, &event, CancellationToken::new()).await?;
                            }
                        }
                        // Capture overflow that occurred during the final drain.
                        gap.observe(receiver.dropped_events(), receiver.capacity());
                        return Ok(());
                    }
                    event = receiver.recv() => event,
                };
                gap.observe(receiver.dropped_events(), receiver.capacity());
                let Some(event) = next else { return Ok(()) };
                if event.session_id == session_id {
                    store.record_event(session_id, &event, CancellationToken::new()).await?;
                }
            }
        });
        Self { stop, task: Some(task), dropped, session_id }
    }

    /// Cumulative overflow the recorder observed for this subscription.
    ///
    /// Compare across iterations to detect a gap; the value never decreases.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop recording, await the final drain, and fail closed if the queue
    /// overflowed. Dropping the guard instead just cancels without reporting
    /// (the run never claimed a complete audit trail in that path).
    pub async fn finish(mut self) -> Result<(), SessionError> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| SessionError::Storage(error.to_string()))??;
        }
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            return Err(gap_error(self.session_id, dropped));
        }
        Ok(())
    }
}

impl Drop for EventRecording {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::EventKind;

    #[tokio::test]
    async fn normal_short_run_reports_no_drops_and_finishes_cleanly() {
        let store = Arc::new(crate::SqliteSessionStore::connect_in_memory().await.expect("store"));
        let session = store
            .create_session(
                &camino::Utf8PathBuf::from("fixture"),
                "fixture",
                "model",
                CancellationToken::new(),
            )
            .await
            .expect("session");
        let bus = EventBus::new(8);
        let recording = EventRecording::start(&bus, store.clone(), session.id);

        for _ in 0..3 {
            bus.publish_for_session(session.id, session.id, EventKind::SessionSaved)
                .expect("publish");
        }

        assert_eq!(recording.dropped_events(), 0, "short run stays within the queue bound");
        recording.finish().await.expect("clean finish");

        let events =
            store.load_events(session.id, CancellationToken::new()).await.expect("load events");
        assert_eq!(events.len(), 3, "all short-run events were recorded");
    }

    #[test]
    fn dropped_events_accessor_is_cumulative() {
        let recording = EventRecording {
            stop: CancellationToken::new(),
            task: None,
            dropped: Arc::new(AtomicU64::new(3)),
            session_id: Ulid::new(),
        };
        assert_eq!(recording.dropped_events(), 3);
        recording.dropped.fetch_add(4, Ordering::Relaxed);
        assert_eq!(recording.dropped_events(), 7);
    }

    #[test]
    fn gap_observer_accumulates_each_new_gap_once() {
        let total = Arc::new(AtomicU64::new(0));
        let mut gap = GapObserver::new(Arc::clone(&total), Ulid::new());

        gap.observe(3, 8192);
        assert_eq!(total.load(Ordering::Relaxed), 3);
        gap.observe(5, 8192);
        assert_eq!(total.load(Ordering::Relaxed), 5);
        gap.observe(5, 8192);
        assert_eq!(total.load(Ordering::Relaxed), 5, "unchanged count is not a new gap");
    }

    #[tokio::test]
    async fn finish_fails_closed_when_drops_were_reported() {
        let session_id = Ulid::new();
        let recording = EventRecording {
            stop: CancellationToken::new(),
            task: None,
            dropped: Arc::new(AtomicU64::new(7)),
            session_id,
        };

        let error = recording.finish().await.expect_err("drops must fail finish");
        match error {
            SessionError::Storage(message) => {
                assert!(message.contains(&session_id.to_string()), "{message}");
                assert!(message.contains('7'), "{message}");
                assert!(message.contains("incomplete"), "{message}");
            }
            other => panic!("expected storage error, got {other:?}"),
        }
    }
}
