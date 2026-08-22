//! Task graph — a DAG of `SubTask` nodes used by `CoordinatorAgent` for
//! multi-agent orchestration planning and execution.
//!
//! The graph is stored in memory as a `petgraph::DiGraph` during execution
//! and serialised to/from a JSON adjacency list for persistence in SQLite.

use concerto_core::types::{SubTask, SubTaskStatus, TaskId};
use concerto_core::OrchestratorError;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::relationship::AgentRelationship;
use tracing::warn;

/// Dependency relationship between two subtasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Dependency {
    MustFinishBefore,
    ProvidesContextFor,
}

/// Edge in the task graph: carries both the execution dependency type
/// and the agent relationship (when applicable).
#[derive(Debug, Clone)]
pub struct TaskEdge {
    pub dependency: Dependency,
    pub relationship: Option<AgentRelationship>,
}

/// In-memory task graph for multi-agent orchestration.
pub struct TaskGraph {
    graph: DiGraph<TaskId, TaskEdge>,
    tasks: HashMap<TaskId, SubTask>,
    node_indices: HashMap<TaskId, NodeIndex>,
}

impl TaskGraph {
    /// Create an empty task graph.
    pub fn new() -> Self {
        Self { graph: DiGraph::new(), tasks: HashMap::new(), node_indices: HashMap::new() }
    }

    /// Add a root subtask (no parent, no dependencies).
    pub fn add_root(&mut self, task: SubTask) {
        let id = task.id;
        let idx = self.graph.add_node(id);
        self.tasks.insert(id, task);
        self.node_indices.insert(id, idx);
    }

    /// Add a child subtask with a dependency on `depends_on`.
    pub fn add_child(&mut self, task: SubTask, depends_on: TaskId, dep: Dependency) {
        let id = task.id;
        let idx = self.graph.add_node(id);
        self.tasks.insert(id, task);
        self.node_indices.insert(id, idx);
        if let Some(&parent_idx) = self.node_indices.get(&depends_on) {
            self.graph.add_edge(parent_idx, idx, TaskEdge { dependency: dep, relationship: None });
        }
    }

    /// Add a child subtask with a dependency and an explicit agent relationship.
    pub fn add_child_with_relationship(
        &mut self,
        task: SubTask,
        depends_on: TaskId,
        dep: Dependency,
        rel: AgentRelationship,
    ) {
        let id = task.id;
        let idx = self.graph.add_node(id);
        self.tasks.insert(id, task);
        self.node_indices.insert(id, idx);
        if let Some(&parent_idx) = self.node_indices.get(&depends_on) {
            self.graph.add_edge(
                parent_idx,
                idx,
                TaskEdge { dependency: dep, relationship: Some(rel) },
            );
        }
    }

    /// Add a dependency edge between two already-added tasks.
    pub fn add_dependency(
        &mut self,
        task_id: TaskId,
        depends_on: TaskId,
        dep: Dependency,
    ) -> Result<(), OrchestratorError> {
        match (self.node_indices.get(&task_id), self.node_indices.get(&depends_on)) {
            (Some(&task_idx), Some(&dep_idx)) => {
                self.graph.add_edge(
                    dep_idx,
                    task_idx,
                    TaskEdge { dependency: dep, relationship: None },
                );
                Ok(())
            }
            _ => {
                let msg = format!(
                    "add_dependency: task {task_id} or dependency {depends_on} not in graph"
                );
                warn!(target: "orchestrator::graph", "add_dependency: unknown task id, edge dropped: task_id {:?}, depends_on {:?}: {}", task_id, depends_on, msg);
                Err(OrchestratorError::TaskGraphError(msg))
            }
        }
    }

    /// Add a dependency between existing tasks and retain the collaboration
    /// relationship that caused the handoff.
    pub fn add_dependency_with_relationship(
        &mut self,
        task_id: TaskId,
        depends_on: TaskId,
        dep: Dependency,
        relationship: AgentRelationship,
    ) -> Result<(), OrchestratorError> {
        match (self.node_indices.get(&task_id), self.node_indices.get(&depends_on)) {
            (Some(&task_idx), Some(&dep_idx)) => {
                self.graph.add_edge(
                    dep_idx,
                    task_idx,
                    TaskEdge { dependency: dep, relationship: Some(relationship) },
                );
                Ok(())
            }
            _ => {
                let msg = format!("add_dependency_with_relationship: task {task_id} or dependency {depends_on} not in graph");
                warn!(target: "orchestrator::graph", "add_dependency_with_relationship: unknown task id, edge dropped: task_id {:?}, depends_on {:?}: {}", task_id, depends_on, msg);
                Err(OrchestratorError::TaskGraphError(msg))
            }
        }
    }

    /// Mark a task as running.
    pub fn mark_running(&mut self, id: &TaskId) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = SubTaskStatus::Running;
        }
    }

    /// Return a recoverable failed task to the ready queue.
    pub fn mark_pending(&mut self, id: &TaskId) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = SubTaskStatus::Pending;
        }
    }

    /// Mark a task as completed.
    pub fn mark_done(&mut self, id: &TaskId) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = SubTaskStatus::Completed;
        }
    }

    /// Mark a task as blocked.
    pub fn mark_blocked(&mut self, id: &TaskId) {
        if let Some(task) = self.tasks.get_mut(id) {
            task.status = SubTaskStatus::Blocked;
        }
    }

    /// Return the task IDs that a given task is blocked on (has unfinished
    /// incoming edges).
    pub fn blocked_on(&self, id: &TaskId) -> Vec<TaskId> {
        self.dependencies_of(id)
            .into_iter()
            .filter(|dependency_id| {
                self.tasks
                    .get(dependency_id)
                    .is_some_and(|task| task.status != SubTaskStatus::Completed)
            })
            .collect()
    }

    /// Return every direct dependency of a task, including completed ones.
    pub fn dependencies_of(&self, id: &TaskId) -> Vec<TaskId> {
        let Some(&idx) = self.node_indices.get(id) else {
            return Vec::new();
        };
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .map(|neighbor| self.graph[neighbor])
            .collect()
    }

    /// Return tasks that are ready to execute (all dependencies completed).
    pub fn ready_tasks(&self) -> Vec<&SubTask> {
        let mut ready = Vec::new();
        for task in self.tasks.values() {
            if task.status != SubTaskStatus::Pending {
                continue;
            }
            if self.blocked_on(&task.id).is_empty() {
                ready.push(task);
            }
        }
        ready
    }

    /// Check if all tasks are completed.
    pub fn all_completed(&self) -> bool {
        self.tasks.values().all(|t| t.status == SubTaskStatus::Completed)
    }

    /// Get a task by ID.
    pub fn get(&self, id: &TaskId) -> Option<&SubTask> {
        self.tasks.get(id)
    }

    /// Get mutable task by ID.
    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut SubTask> {
        self.tasks.get_mut(id)
    }

    /// Return all tasks in the graph.
    pub fn all_tasks(&self) -> Vec<&SubTask> {
        self.tasks.values().collect()
    }

    /// Add a subtask to the graph without specifying parent or dependencies.
    /// Used when restoring from a checkpoint where edges are provided separately.
    pub fn add_subtask(&mut self, task: SubTask) {
        let id = task.id;
        let idx = self.graph.add_node(id);
        self.tasks.insert(id, task);
        self.node_indices.insert(id, idx);
    }

    /// Return all edges as `(from, to, dependency)` tuples.
    /// Used for checkpoint serialisation.
    pub fn all_edges(&self) -> Vec<(TaskId, TaskId, Dependency)> {
        let mut edges = Vec::new();
        // `edge_references` yields source/target/weight directly, so no
        // Option handling is needed (each reference is guaranteed valid).
        for edge in self.graph.edge_references() {
            let from = self.graph[edge.source()];
            let to = self.graph[edge.target()];
            edges.push((from, to, edge.weight().dependency));
        }
        edges
    }

    /// Check if the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Number of tasks in the graph.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Mark every task with the given status as a new terminal state.
    /// Used during coordinator cleanup to ensure no running task remains
    /// in limbo when the orchestration exits early.
    pub fn mark_all_with_status(&mut self, from: SubTaskStatus, to: SubTaskStatus) {
        for task in self.tasks.values_mut() {
            if task.status == from {
                task.status = to;
            }
        }
    }
}

impl Default for TaskGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TaskGraphSerializer
// ---------------------------------------------------------------------------

/// Converts between in-memory `TaskGraph` and a JSON adjacency list for
/// SQLite persistence.
pub struct TaskGraphSerializer;

impl TaskGraphSerializer {
    /// Serialise the graph to a JSON adjacency list `[(task_id, [dep_ids])]`.
    pub fn to_json(graph: &TaskGraph) -> Result<String, serde_json::Error> {
        let mut adjacency: Vec<(String, Vec<String>)> = Vec::new();
        // We need access to the internal graph structure to export edges.
        // Since we store the petgraph internally, we use a serialisation
        // approach via the tasks and their dependency info.
        for task in graph.all_tasks() {
            let deps: Vec<String> =
                graph.dependencies_of(&task.id).iter().map(|id| id.to_string()).collect();
            adjacency.push((task.id.to_string(), deps));
        }
        serde_json::to_string(&adjacency)
    }

    /// Deserialise from JSON adjacency list + subtask rows back to TaskGraph.
    pub fn from_json(json: &str, tasks: Vec<SubTask>) -> Result<TaskGraph, OrchestratorError> {
        let adjacency: Vec<(String, Vec<String>)> =
            serde_json::from_str(json).map_err(|e| OrchestratorError::InvalidTaskGraph {
                reason: format!("failed to parse graph JSON: {e}"),
            })?;

        let mut graph = TaskGraph::new();
        let dep_map: HashMap<String, Vec<String>> = adjacency.into_iter().collect();

        // First pass: add all tasks as roots
        for task in &tasks {
            graph.add_root(task.clone());
        }

        // Second pass: add dependency edges
        for task in &tasks {
            let id_str = task.id.to_string();
            if let Some(dep_ids) = dep_map.get(&id_str) {
                for dep_id_str in dep_ids {
                    if let Some(dep_task_id) =
                        tasks.iter().find(|t| t.id.to_string() == *dep_id_str)
                    {
                        if let Err(e) = graph.add_dependency(
                            task.id,
                            dep_task_id.id,
                            Dependency::MustFinishBefore,
                        ) {
                            warn!(target: "orchestrator::graph", %e, "from_json: dropped edge");
                        }
                    }
                }
            }
        }

        Ok(graph)
    }
}

// ---------------------------------------------------------------------------
// TaskGraphValidator
// ---------------------------------------------------------------------------

/// Validates a task graph before execution.
pub struct TaskGraphValidator;

impl TaskGraphValidator {
    /// Validate the graph — checks for cycles, missing dep references, and
    /// ensures there is at least one root node.
    pub fn validate(graph: &TaskGraph) -> Result<(), OrchestratorError> {
        if graph.is_empty() {
            return Err(OrchestratorError::InvalidTaskGraph {
                reason: "graph has no nodes".into(),
            });
        }

        // Check for root nodes (tasks with no incoming deps and no parent)
        let has_root = graph
            .all_tasks()
            .iter()
            .any(|t| t.parent_id.is_none() && graph.dependencies_of(&t.id).is_empty());
        if !has_root {
            return Err(OrchestratorError::InvalidTaskGraph {
                reason: "graph has no root node".into(),
            });
        }

        // Cycle detection using DFS
        let mut visited: HashMap<TaskId, bool> = HashMap::new();
        for task in graph.all_tasks() {
            visit(task.id, graph, &mut visited)?;
        }

        Ok(())
    }
}

/// DFS-based cycle detection. `visited` tracks nodes in the current path
/// (true = in current path, false = fully processed).
fn visit(
    id: TaskId,
    graph: &TaskGraph,
    visited: &mut HashMap<TaskId, bool>,
) -> Result<(), OrchestratorError> {
    match visited.get(&id) {
        Some(true) => Err(OrchestratorError::InvalidTaskGraph {
            reason: format!("cycle detected involving task {id}"),
        }),
        Some(false) => Ok(()),
        None => {
            visited.insert(id, true);
            let blockers = graph.dependencies_of(&id);
            for blocker in blockers {
                visit(blocker, graph, visited)?;
            }
            visited.insert(id, false);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{AgentId, SubTask};

    fn make_subtask(id: TaskId, role: AgentId, parent: Option<TaskId>) -> SubTask {
        let mut task = SubTask::new(Ulid::new(), role, "test task");
        task.id = id;
        task.parent_id = parent;
        task
    }

    #[test]
    fn empty_graph_validator_fails() {
        let graph = TaskGraph::new();
        let result = TaskGraphValidator::validate(&graph);
        assert!(result.is_err());
    }

    #[test]
    fn single_root_valid() {
        let mut graph = TaskGraph::new();
        graph.add_root(make_subtask(TaskId::new(), AgentId::new("architect"), None));
        assert!(TaskGraphValidator::validate(&graph).is_ok());
    }

    #[test]
    fn ready_tasks_returns_only_unblocked() {
        let mut graph = TaskGraph::new();
        let root_id = TaskId::new();
        let child_id = TaskId::new();
        graph.add_root(make_subtask(root_id, AgentId::new("architect"), None));
        graph.add_child(
            make_subtask(child_id, AgentId::new("coder"), Some(root_id)),
            root_id,
            Dependency::MustFinishBefore,
        );

        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, root_id);

        graph.mark_done(&root_id);
        assert_eq!(graph.dependencies_of(&child_id), vec![root_id]);
        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, child_id);
    }

    #[test]
    fn recoverable_task_can_return_to_ready_queue() {
        let mut graph = TaskGraph::new();
        let task_id = TaskId::new();
        graph.add_root(make_subtask(task_id, AgentId::new("coder"), None));
        graph.mark_running(&task_id);
        assert!(graph.ready_tasks().is_empty());

        graph.mark_pending(&task_id);
        assert_eq!(graph.ready_tasks()[0].id, task_id);
    }

    #[test]
    fn serialization_round_trip() {
        let mut graph = TaskGraph::new();
        let root_id = TaskId::new();
        let child_id = TaskId::new();
        graph.add_root(make_subtask(root_id, AgentId::new("architect"), None));
        graph.add_child(
            make_subtask(child_id, AgentId::new("coder"), Some(root_id)),
            root_id,
            Dependency::MustFinishBefore,
        );
        graph.mark_done(&root_id);
        let json = TaskGraphSerializer::to_json(&graph).unwrap();
        let tasks = graph.all_tasks().into_iter().cloned().collect::<Vec<_>>();
        let restored = TaskGraphSerializer::from_json(&json, tasks).unwrap();
        assert_eq!(restored.dependencies_of(&child_id), vec![root_id]);
    }

    #[test]
    fn add_dependency_returns_error_on_unknown_task() {
        let mut graph = TaskGraph::new();
        let task = make_subtask(TaskId::new(), AgentId::new("architect"), None);
        graph.add_root(task.clone());
        let bogus = TaskId::new();
        let result = graph.add_dependency(task.id, bogus, Dependency::MustFinishBefore);
        assert!(
            result.is_err(),
            "unknown dependency id must surface as an error, not be silently dropped"
        );
        match result.unwrap_err() {
            OrchestratorError::TaskGraphError(_) => {}
            other => panic!("expected TaskGraphError, got {other:?}"),
        }
    }

    #[test]
    fn add_dependency_with_relationship_returns_error_on_unknown_task() {
        let mut graph = TaskGraph::new();
        let task = make_subtask(TaskId::new(), AgentId::new("architect"), None);
        graph.add_root(task.clone());
        let bogus = TaskId::new();
        let result = graph.add_dependency_with_relationship(
            task.id,
            bogus,
            Dependency::MustFinishBefore,
            crate::relationship::AgentRelationship::ProvidesContextTo,
        );
        assert!(
            result.is_err(),
            "unknown dependency id must surface as an error, not be silently dropped"
        );
        match result.unwrap_err() {
            OrchestratorError::TaskGraphError(_) => {}
            other => panic!("expected TaskGraphError, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Regression: Fix 2 — zombie lifecycle (mark_all_with_status)
    // ------------------------------------------------------------------

    #[test]
    fn mark_all_with_status_fails_running_tasks() {
        let mut graph = TaskGraph::new();
        let root_id = TaskId::new();
        let child_id = TaskId::new();
        graph.add_root(make_subtask(root_id, AgentId::new("architect"), None));
        graph.add_child(
            make_subtask(child_id, AgentId::new("coder"), Some(root_id)),
            root_id,
            Dependency::MustFinishBefore,
        );
        // Mark the child as running (simulating in-flight work).
        graph.mark_running(&child_id);

        // Zombie-kill: mark all Running tasks as Failed.
        graph.mark_all_with_status(SubTaskStatus::Running, SubTaskStatus::Failed);

        // Child should now be Failed.
        let child = graph.get(&child_id).unwrap();
        assert_eq!(child.status, SubTaskStatus::Failed);

        // Root (Pending) should not be affected.
        let root = graph.get(&root_id).unwrap();
        assert_eq!(root.status, SubTaskStatus::Pending);
    }

    #[test]
    fn mark_all_with_status_does_not_affect_completed_tasks() {
        let mut graph = TaskGraph::new();
        let a = TaskId::new();
        let b = TaskId::new();
        graph.add_root(make_subtask(a, AgentId::new("coder"), None));
        graph.add_root(make_subtask(b, AgentId::new("reviewer"), None));
        graph.mark_done(&a);
        graph.mark_running(&b);

        graph.mark_all_with_status(SubTaskStatus::Running, SubTaskStatus::Failed);

        // Completed task stays completed.
        assert_eq!(graph.get(&a).unwrap().status, SubTaskStatus::Completed);
        // Running task becomes Failed.
        assert_eq!(graph.get(&b).unwrap().status, SubTaskStatus::Failed);
    }

    #[test]
    fn mark_all_with_status_noop_when_no_matching() {
        let mut graph = TaskGraph::new();
        let a = TaskId::new();
        graph.add_root(make_subtask(a, AgentId::new("architect"), None));
        graph.mark_done(&a);

        // No tasks in Running state — should be a no-op.
        graph.mark_all_with_status(SubTaskStatus::Running, SubTaskStatus::Failed);
        assert_eq!(graph.get(&a).unwrap().status, SubTaskStatus::Completed);
    }

    // ------------------------------------------------------------------
    // Regression: Fix 1 — checkpoint round-trip via GraphCheckpoint
    // ------------------------------------------------------------------

    #[test]
    fn checkpoint_build_and_restore_preserves_graph_structure() {
        use crate::checkpoint::{
            build_checkpoint, restore_graph, CheckpointScope, CheckpointStage, GraphCheckpoint,
        };
        use std::collections::HashMap;

        // Build a graph with several tasks and a known dependency tree.
        let mut graph = TaskGraph::new();
        let arch = TaskId::new();
        let coder = TaskId::new();
        let reviewer = TaskId::new();
        graph.add_root(make_subtask(arch, AgentId::new("architect"), None));
        graph.add_child(
            make_subtask(coder, AgentId::new("coder"), Some(arch)),
            arch,
            Dependency::MustFinishBefore,
        );
        graph.add_child(
            make_subtask(reviewer, AgentId::new("reviewer"), Some(coder)),
            coder,
            Dependency::MustFinishBefore,
        );

        // Mark some progress.
        graph.mark_done(&arch);
        graph.mark_running(&coder);

        // Build a checkpoint.
        let session_id = graph.get(&arch).unwrap().session_id;
        let scope = CheckpointScope {
            run_id: concerto_core::ids::Ulid::new(),
            session_id,
            root_task_id: TaskId::new(),
            project_id: "test".into(),
            objective: "test".into(),
            objective_hash: "hash".into(),
            source_revision: Some("abc123".into()),
            sequence_num: 1,
        };
        let working_memory = concerto_core::types::AgentContext::new(
            concerto_core::types::SessionContext::new(session_id, std::path::PathBuf::from(".")),
        )
        .working_memory;
        let checkpoint = build_checkpoint(
            &scope,
            CheckpointStage::Executing,
            None,
            &working_memory,
            &graph,
            &HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::checkpoint::CheckpointContext::default(),
        );

        // Serialize to JSON and back (the actual round-trip that crosses
        // process boundaries via AgentOutput.checkpoint_json).
        let json = serde_json::to_value(&checkpoint).unwrap();
        let deserialized: GraphCheckpoint = serde_json::from_value(json).unwrap();
        assert!(deserialized.validate_scope(session_id, "test", Some("abc123")).is_ok());
        assert!(deserialized.validate_scope(session_id, "test", Some("different")).is_err());

        // Restore the graph.
        let restored = restore_graph(&deserialized).unwrap();

        // The restored graph should have all the same tasks.
        assert_eq!(restored.all_tasks().len(), 3);
        assert!(restored.get(&arch).is_some());
        assert!(restored.get(&coder).is_some());
        assert!(restored.get(&reviewer).is_some());
        assert_eq!(restored.get(&coder).unwrap().status, SubTaskStatus::Pending);

        // Completed status is preserved; in-flight work becomes pending so a
        // crashed process can safely schedule it again.
        assert_eq!(restored.get(&arch).unwrap().status, SubTaskStatus::Completed);
        assert_eq!(restored.get(&coder).unwrap().status, SubTaskStatus::Pending);

        // Dependencies should be preserved.
        let coder_deps = restored.dependencies_of(&coder);
        assert_eq!(coder_deps, vec![arch]);
        assert_eq!(restored.ready_tasks().len(), 1);
        assert_eq!(restored.ready_tasks()[0].id, coder);
    }

    // ------------------------------------------------------------------
    // Regression: Fix 1 — TaskGraphSerializer preserves all statuses
    // ------------------------------------------------------------------

    #[test]
    fn graph_serializer_preserves_complex_statuses() {
        let mut graph = TaskGraph::new();
        let pending = TaskId::new();
        let running_id = TaskId::new();
        let done = TaskId::new();
        graph.add_root(make_subtask(pending, AgentId::new("architect"), None));
        graph.add_root(make_subtask(running_id, AgentId::new("coder"), None));
        graph.add_root(make_subtask(done, AgentId::new("reviewer"), None));

        graph.mark_running(&running_id);
        graph.mark_done(&done);

        let json = TaskGraphSerializer::to_json(&graph).unwrap();
        let tasks = graph.all_tasks().into_iter().cloned().collect::<Vec<_>>();
        let restored = TaskGraphSerializer::from_json(&json, tasks).unwrap();

        assert_eq!(restored.get(&pending).unwrap().status, SubTaskStatus::Pending);
        assert_eq!(restored.get(&running_id).unwrap().status, SubTaskStatus::Running);
        assert_eq!(restored.get(&done).unwrap().status, SubTaskStatus::Completed);
    }

    #[test]
    fn graph_serializer_round_trip_maintains_empty_edge_set() {
        let mut graph = TaskGraph::new();
        let a = TaskId::new();
        let b = TaskId::new();
        graph.add_root(make_subtask(a, AgentId::new("coder"), None));
        graph.add_root(make_subtask(b, AgentId::new("reviewer"), None));

        let json = TaskGraphSerializer::to_json(&graph).unwrap();
        let tasks = graph.all_tasks().into_iter().cloned().collect::<Vec<_>>();
        let restored = TaskGraphSerializer::from_json(&json, tasks).unwrap();

        // Two independent roots with no edges.
        assert!(restored.dependencies_of(&a).is_empty());
        assert!(restored.dependencies_of(&b).is_empty());
    }
}
