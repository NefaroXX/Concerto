//! Durable event capture with an awaited drain before querying benchmark evidence.

use crate::{SessionError, SessionStore};
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use std::sync::Arc;

pub struct EventRecording {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), SessionError>>>,
}

impl EventRecording {
    pub fn start(bus: &EventBus, store: Arc<dyn SessionStore>, session_id: Ulid) -> Self {
        let mut receiver = bus.subscribe_durable();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    _ = stopped.cancelled() => {
                        while let Ok(event) = receiver.try_recv() {
                            if event.session_id == session_id {
                                store.record_event(session_id, &event, CancellationToken::new()).await?;
                            }
                        }
                        return Ok(());
                    }
                    event = receiver.recv() => event,
                };
                let Some(event) = next else { return Ok(()) };
                if event.session_id == session_id {
                    store.record_event(session_id, &event, CancellationToken::new()).await?;
                }
            }
        });
        Self { stop, task: Some(task) }
    }

    pub async fn finish(mut self) -> Result<(), SessionError> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| SessionError::Storage(error.to_string()))??;
        }
        Ok(())
    }
}

impl Drop for EventRecording {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
