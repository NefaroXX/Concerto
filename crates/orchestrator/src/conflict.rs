//! Memory conflict detector — tracks per-key writers and emits
//! `MemoryConflict` events when a different role overwrites a key.
//!
//! Wraps a `MemoryWriteSerializer` so all writes are already serialised.
//! The last-write-wins semantics are enforced by the underlying store.

use std::collections::HashMap;
use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::memory::{MemoryEntry, MemoryId};
use concerto_core::types::AgentId;
use concerto_core::CancellationToken;

use crate::memory_serial::MemoryWriteSerializer;

/// Detects cross-role memory write conflicts.
///
/// Tracks which `AgentId` last wrote each key. When a different agent
/// writes the same key, a `MemoryConflict` event is emitted on the bus.
/// Thread-safe via `tokio::sync::Mutex`.
pub struct MemoryConflictDetector {
    inner: Arc<MemoryWriteSerializer>,
    bus: EventBus,
    last_writer: tokio::sync::Mutex<HashMap<String, AgentId>>,
}

impl MemoryConflictDetector {
    /// Create a new conflict detector.
    pub fn new(inner: Arc<MemoryWriteSerializer>, bus: EventBus) -> Self {
        Self { inner, bus, last_writer: tokio::sync::Mutex::new(HashMap::new()) }
    }

    /// Store a memory entry, checking for role conflicts.
    ///
    /// If the entry's content key was previously written by a different
    /// role, a `MemoryConflict` event is emitted before the store
    /// proceeds. The write itself is delegated to the wrapped
    /// `MemoryWriteSerializer`.
    pub async fn store(
        &self,
        entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        let key = Self::extract_key(&entry);
        let current_role = Self::extract_role(&entry);

        let mut guard = self.last_writer.lock().await;

        if let Some(previous_role) = guard.get(&key) {
            if *previous_role != current_role {
                let event = EventKind::MemoryConflict {
                    key: key.clone(),
                    agent_role: current_role.clone(),
                    previous_agent: Some(previous_role.clone()),
                };
                if let Some(session_id) = entry
                    .metadata
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .and_then(|value| Ulid::from_string(value).ok())
                {
                    let _ = self.bus.publish_for_session(session_id, session_id, event);
                } else {
                    // Global event: intentionally unscoped (entry carries no
                    // session_id metadata to bind the conflict to a session).
                    let _ = self.bus.publish_raw(event);
                }
            }
        }

        guard.insert(key, current_role);
        drop(guard);

        self.inner.store(entry, _cancel.clone()).await
    }

    /// Extract a stable key from a `MemoryEntry`.
    ///
    /// Uses the entry id as the key for conflict tracking.
    fn extract_key(entry: &MemoryEntry) -> String {
        entry.id.to_string()
    }

    /// Extract the `AgentId` from entry metadata.
    ///
    /// Expects the metadata to contain a `"role"` field with the agent
    /// id. Falls back to `coordinator` if missing or unparseable.
    /// `AgentId::new` lowercases, so legacy PascalCase metadata values
    /// (e.g. `"Architect"`) still resolve correctly.
    fn extract_role(entry: &MemoryEntry) -> AgentId {
        entry
            .metadata
            .get("role")
            .and_then(|v| v.as_str())
            .map(AgentId::new)
            .unwrap_or_else(|| AgentId::new("coordinator"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::memory::{ChunkType, MemoryNamespace, ProjectId};
    use concerto_core::traits::memory::MemoryStore;
    use std::time::Duration;
    use tokio::time::sleep;

    /// A mock `MemoryStore` that tracks store calls and simulates delay.
    struct MockStore {
        store_count: std::sync::atomic::AtomicUsize,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl MemoryStore for MockStore {
        async fn retrieve(
            &self,
            _query: &concerto_core::memory::MemoryQuery,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::memory::MemoryChunk>, MemoryError> {
            Ok(vec![])
        }

        async fn store(
            &self,
            _entry: MemoryEntry,
            _cancel: CancellationToken,
        ) -> Result<MemoryId, MemoryError> {
            self.store_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.delay_ms > 0 {
                sleep(Duration::from_millis(self.delay_ms)).await;
            }
            Ok(MemoryId(Ulid::new()))
        }

        async fn invalidate(
            &self,
            _id: MemoryId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn make_entry(role: AgentId) -> MemoryEntry {
        let mut metadata = serde_json::Map::new();
        metadata.insert("role".into(), serde_json::Value::String(role.as_str().to_string()));

        MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Global { user_id_hash: "test".into() },
            content: "test content".into(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::Value::Object(metadata),
            expires_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn conflict_event_emitted_on_role_change() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 0,
        });
        let serializer = Arc::new(MemoryWriteSerializer::new(store));
        let bus = EventBus::new(1024);
        let detector = MemoryConflictDetector::new(serializer, bus.clone());

        let mut rx = bus.subscribe();

        // First write by Coder
        let entry1 = make_entry(AgentId::new("coder"));
        let id1 = entry1.id;
        detector.store(entry1, CancellationToken::new()).await.unwrap();

        // Second write by Architect with same id (simulating overwrite)
        let mut entry2 = make_entry(AgentId::new("architect"));
        entry2.id = id1;
        detector.store(entry2, CancellationToken::new()).await.unwrap();

        // Check that a MemoryConflict event was emitted
        let event = rx.recv().await.unwrap();
        match &event.kind {
            EventKind::MemoryConflict { key, agent_role, previous_agent } => {
                assert_eq!(key, &id1.to_string());
                assert_eq!(*agent_role, AgentId::new("architect"));
                assert_eq!(*previous_agent, Some(AgentId::new("coder")));
            }
            other => panic!("expected MemoryConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_conflict_when_same_role_writes() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 0,
        });
        let serializer = Arc::new(MemoryWriteSerializer::new(store));
        let bus = EventBus::new(1024);
        let detector = MemoryConflictDetector::new(serializer, bus.clone());

        let mut rx = bus.subscribe();

        // First write by Coder
        let entry1 = make_entry(AgentId::new("coder"));
        let id1 = entry1.id;
        detector.store(entry1, CancellationToken::new()).await.unwrap();

        // Second write by Coder with same id
        let mut entry2 = make_entry(AgentId::new("coder"));
        entry2.id = id1;
        detector.store(entry2, CancellationToken::new()).await.unwrap();

        // No MemoryConflict event should be emitted
        // Use timeout to avoid hanging forever
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err(), "expected no event, but got one");
    }

    #[tokio::test]
    async fn store_passes_through_to_inner() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 0,
        });
        let serializer = Arc::new(MemoryWriteSerializer::new(store));
        let bus = EventBus::new(1024);
        let detector = MemoryConflictDetector::new(serializer, bus);

        let entry = make_entry(AgentId::new("coder"));
        let result = detector.store(entry, CancellationToken::new()).await;
        assert!(result.is_ok());
    }
}
