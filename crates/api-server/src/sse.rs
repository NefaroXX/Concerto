//! SSE (Server-Sent Events) adapter — bridges the `EventBus` to an axum SSE
//! stream, filtering events by `TaskId`.

use std::convert::Infallible;

use axum::response::sse::Event;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::types::TaskId;
use futures::stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// Adapts an `EventBus` to an axum SSE stream, filtering by `TaskId`.
pub struct SseAdapter;

impl SseAdapter {
    pub fn from_bus(
        bus: &EventBus,
        task_id: TaskId,
    ) -> impl Stream<Item = Result<Event, Infallible>> {
        let rx = bus.subscribe();
        let stream = BroadcastStream::new(rx.into_inner());

        stream.filter_map(move |result| {
            let event = match result {
                Ok(ev) => ev,
                Err(_) => return None,
            };
            let matches = matches!(
                event.kind,
                EventKind::TaskStarted { task_id: tid, .. }
                | EventKind::TaskCompleted { task_id: tid, .. }
                | EventKind::TaskFailed { task_id: tid, .. }
                | EventKind::AgentStateChanged { task_id: tid, .. }
                | EventKind::CycleBudgetExceeded { task_id: tid, .. }
                | EventKind::EvalStarted { task_id: tid, .. }
                | EventKind::EvalCompleted { task_id: tid, .. }
                if tid == task_id
            );

            if matches {
                let data = serde_json::to_string(&event.kind).unwrap_or_default();
                Some(Ok(Event::default().data(data)))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::{EventBus, EventKind};
    use concerto_core::types::TaskId;
    use futures::StreamExt;
    use std::time::Duration;

    /// SseAdapter::from_bus creates a stream without panicking.
    #[tokio::test]
    async fn sse_adapter_creation() {
        let bus = EventBus::default();
        let task_id = TaskId::new();
        let _stream = SseAdapter::from_bus(&bus, task_id);
        // Stream created successfully — no panic.
    }

    /// The adapter only yields events whose `TaskId` matches the requested one.
    /// Events for other tasks are silently filtered out.
    #[tokio::test]
    async fn sse_adapter_event_filtering_by_task_id() {
        let bus = EventBus::new(16);
        let task_id = TaskId::new();
        let other_id = TaskId::new();

        // Subscribe BEFORE publishing so no events are missed.
        let stream = SseAdapter::from_bus(&bus, task_id);
        tokio::pin!(stream);

        // Publish one matching event.
        bus.publish_raw(EventKind::TaskStarted { task_id, description: "my task".into() }).unwrap();

        // Publish a non-matching event (different task_id).
        bus.publish_raw(EventKind::TaskStarted {
            task_id: other_id,
            description: "other task".into(),
        })
        .unwrap();

        // Should receive exactly one event (the matching one).
        let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("timeout waiting for first event")
            .expect("stream should yield Some");
        assert!(first.is_ok(), "event should be Ok");

        // No more events buffered — stream would hang, so drop the bus to
        // make it terminate and collect remaining items.
        drop(bus);
        let rest: Vec<_> = stream.collect().await;
        // Only the matching event was yielded.
        assert!(rest.is_empty(), "no more events should be yielded");
    }

    /// When the `EventBus` sender handle is dropped, the stream terminates
    /// cleanly (returns `None`).
    #[tokio::test]
    async fn sse_adapter_stream_termination() {
        let bus = EventBus::new(16);
        let task_id = TaskId::new();
        let stream = SseAdapter::from_bus(&bus, task_id);
        tokio::pin!(stream);

        // Drop the bus (drops the sender handle).
        drop(bus);

        // The stream should end without yielding any items
        // (broadcast channel closes, filter_map returns None, stream ends).
        let result = stream.next().await;
        assert!(result.is_none(), "stream should terminate after bus is dropped");
    }

    /// Dropping all `EventBus` handles (the only sender) causes the stream
    /// to receive a `Closed` error, which is handled gracefully (filtered out
    /// by the `filter_map`) and the stream ends.
    #[tokio::test]
    async fn sse_adapter_with_closed_event_bus() {
        let bus = EventBus::new(16);
        let task_id = TaskId::new();
        let stream = SseAdapter::from_bus(&bus, task_id);
        tokio::pin!(stream);

        // Drop the bus — sends the channel into a closed state.
        drop(bus);

        // The receiver gets RecvError::Closed; filter_map returns None;
        // the stream should now terminate.
        let result = stream.next().await;
        assert!(result.is_none(), "stream should end after bus is closed");
    }

    /// When the receiver lags behind (channel buffer overflows before the
    /// adapter subscribes), the adapter gracefully drops the lagged events
    /// and continues rather than panicking or propagating the error.
    #[tokio::test]
    async fn sse_adapter_with_lagged_receiver() {
        // Tiny buffer so we can overflow it easily.
        let bus = EventBus::new(4);
        let task_id = TaskId::new();

        // Publish enough events to overflow the buffer BEFORE subscribing.
        for _ in 0..10 {
            bus.publish_raw(EventKind::SessionSaved).unwrap();
        }

        // Subscribe *after* the overflow — the receiver will be lagged.
        let stream = SseAdapter::from_bus(&bus, task_id);
        tokio::pin!(stream);

        // Close the bus so the stream terminates.
        drop(bus);

        // The adapter should handle the lagged error gracefully (filter_map
        // returns None) and then end the stream.
        let result = stream.next().await;
        // After dropping the bus and handling lag, the stream terminates.
        // The exact return depends on timing, but the important assertion
        // is that we did NOT panic and the stream does not yield an error.
        assert!(result.is_none(), "stream should terminate cleanly after lag + close");
    }
}
