//! Phase 0 exit criterion demo:
//! "Event loop demo: emit 10 events with correlation IDs, subscribe and
//! print them with timestamps."
//!
//! Run with: `cargo run -p concerto-core --example event_loop_demo`

use concerto_core::event::{Event, EventBus, EventKind};
use concerto_core::ids::new_id;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let bus = EventBus::default();
    let mut rx = bus.subscribe();

    let correlation_id = new_id();
    let session_id = new_id();

    for i in 0..10 {
        let event = Event::new(
            correlation_id,
            session_id,
            EventKind::AgentThought {
                agent_id: "demo-agent".into(),
                content: format!("step {i} of the demo loop"),
            },
        );
        bus.publish(event).expect("subscriber is listening");
    }

    for _ in 0..10 {
        let event = rx.recv().await.expect("channel should not close mid-demo");
        println!(
            "[{}] correlation={} session={} kind={:?}",
            event.timestamp, event.correlation_id, event.session_id, event.kind
        );
    }
}
