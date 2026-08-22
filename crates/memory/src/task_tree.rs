//! Task decomposition tree — tracks the progress of subtasks
//! across sessions, enabling cross-session continuation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use concerto_core::error::MemoryError;
use concerto_core::memory::{TaskNode, TaskNodeId, TaskStatus};

use crate::storage::MemoryDb;

pub struct TaskTreeStore {
    nodes: Mutex<HashMap<TaskNodeId, TaskNode>>,
    db: Option<Arc<MemoryDb>>,
}

impl TaskTreeStore {
    pub fn new() -> Self {
        Self { nodes: Mutex::new(HashMap::new()), db: None }
    }

    pub fn with_db(db: Option<Arc<MemoryDb>>) -> Self {
        Self { nodes: Mutex::new(HashMap::new()), db }
    }

    /// Hydrate existing nodes and mirror subsequent changes to the same DB.
    pub async fn load(db: Arc<MemoryDb>) -> Result<Self, MemoryError> {
        let nodes = db.list_task_nodes().await?.into_iter().map(|node| (node.id, node)).collect();
        Ok(Self { nodes: Mutex::new(nodes), db: Some(db) })
    }

    pub fn upsert(&self, node: TaskNode) -> Result<(), MemoryError> {
        let mut store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        store.insert(node.id, node.clone());
        if let Some(ref db) = self.db {
            let db = db.clone();
            spawn_persistence("upsert task node", async move { db.upsert_task_node(&node).await });
        }
        Ok(())
    }

    pub fn get(&self, id: TaskNodeId) -> Result<Option<TaskNode>, MemoryError> {
        let store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        Ok(store.get(&id).cloned())
    }

    pub fn list_all(&self) -> Result<Vec<TaskNode>, MemoryError> {
        let store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        Ok(store.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: TaskStatus) -> Result<Vec<TaskNode>, MemoryError> {
        let store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        Ok(store.values().filter(|n| n.status == status).cloned().collect())
    }

    pub fn update_status(&self, id: TaskNodeId, new_status: TaskStatus) -> Result<(), MemoryError> {
        let mut store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        if let Some(n) = store.get_mut(&id) {
            n.status = new_status;
            if let Some(ref db) = self.db {
                let db = db.clone();
                spawn_persistence("update task node status", async move {
                    db.update_task_node_status(id, new_status).await
                });
            }
            Ok(())
        } else {
            Err(MemoryError::RetrievalFailed("task node not found".to_string()))
        }
    }

    pub fn list_roots(&self) -> Result<Vec<TaskNode>, MemoryError> {
        let store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        Ok(store.values().filter(|n| n.parent_id.is_none()).cloned().collect())
    }

    pub fn list_children(&self, parent_id: TaskNodeId) -> Result<Vec<TaskNode>, MemoryError> {
        let store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        Ok(store.values().filter(|n| n.parent_id == Some(parent_id)).cloned().collect())
    }

    pub fn delete(&self, id: TaskNodeId) -> Result<(), MemoryError> {
        let mut store = self
            .nodes
            .lock()
            .map_err(|_| MemoryError::Persistence("task tree store lock poisoned".into()))?;
        store.remove(&id);
        if let Some(ref db) = self.db {
            let db = db.clone();
            spawn_persistence("delete task node", async move { db.delete_task_node(id).await });
        }
        Ok(())
    }
}

fn spawn_persistence<F>(operation: &'static str, future: F)
where
    F: std::future::Future<Output = Result<(), MemoryError>> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            std::mem::drop(handle.spawn(async move {
                if let Err(error) = future.await {
                    tracing::warn!(%error, %operation, "SQLite task-tree persistence failed");
                }
            }));
        }
        Err(error) => {
            tracing::warn!(%error, %operation, "task-tree persistence requires an async runtime");
        }
    }
}

impl Default for TaskTreeStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use time::OffsetDateTime;

    fn make_node(id_str: &str, parent: Option<TaskNodeId>) -> TaskNode {
        let mut bytes = [0u8; 16];
        let s = id_str.as_bytes();
        let len = s.len().min(16);
        bytes[..len].copy_from_slice(&s[..len]);
        bytes[0] = bytes[0].max(1);
        let id = TaskNodeId(Ulid::from_bytes(bytes));
        TaskNode {
            id,
            session_id: Ulid::nil(),
            description: format!("task {id_str}"),
            status: TaskStatus::Pending,
            parent_id: parent,
            children: Vec::new(),
            blocking: Vec::new(),
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn upsert_and_get() {
        let store = TaskTreeStore::new();
        let n = make_node("t1", None);
        let id = n.id;
        store.upsert(n).unwrap();
        let retrieved = store.get(id).unwrap().unwrap();
        assert_eq!(retrieved.status, TaskStatus::Pending);
    }

    #[test]
    fn update_status() {
        let store = TaskTreeStore::new();
        let n = make_node("t1", None);
        let id = n.id;
        store.upsert(n).unwrap();
        store.update_status(id, TaskStatus::Running).unwrap();
        let retrieved = store.get(id).unwrap().unwrap();
        assert_eq!(retrieved.status, TaskStatus::Running);
    }

    #[test]
    fn parent_child_relationship() {
        let store = TaskTreeStore::new();
        let parent = make_node("root", None);
        let parent_id = parent.id;
        let child = make_node("child", Some(parent_id));
        store.upsert(parent).unwrap();
        store.upsert(child).unwrap();

        let children = store.list_children(parent_id).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].description, "task child");
    }
}
