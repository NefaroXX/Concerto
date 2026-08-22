use iced::border::Radius;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Alignment, Background, Border, Element, Length, Point};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::path::Path;

use crate::theme::AppTheme;
use crate::widgets::agent_graph::{
    self, AgentGraphModel, AgentNode, Message as GraphMessage, NodeState,
};
use concerto_core::{AgentId, TaskId};

/// Which log panel is currently expanded for the selected agent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogPanel {
    None,
    Thinking,
    Work,
}

/// Serialisable mirror of an agent node plus its accumulated logs.
///
/// `iced::Point` is intentionally omitted because layout is recomputed on
/// restore; persisting screen coordinates would be meaningless after a resize
/// or on a different machine.
#[derive(Serialize, Deserialize)]
struct PersistedNode {
    #[serde(default)]
    task_id: Option<TaskId>,
    role: AgentId,
    label: String,
    state: NodeState,
    task_summary: String,
    thinking_logs: Vec<String>,
    work_logs: Vec<String>,
}

/// Serialisable snapshot of the agent-graph view for a single session.
#[derive(Serialize, Deserialize)]
struct PersistedGraph {
    has_multi_agent_activity: bool,
    active_panel: LogPanel,
    nodes: Vec<PersistedNode>,
}

#[derive(Debug, Clone)]
pub enum Message {
    AgentSelected(Option<usize>),
    ViewThinkingLogs(usize),
    ViewWorkLogs(usize),
}

impl From<GraphMessage> for Message {
    fn from(msg: GraphMessage) -> Self {
        match msg {
            // The canvas is a pure visualization: the only interaction it
            // publishes is a node click, which selects that node's log panel.
            GraphMessage::NodeClicked(idx) => Message::AgentSelected(Some(idx)),
        }
    }
}

/// Events that update the agent graph state.
#[derive(Debug, Clone)]
pub enum SubtaskEvent {
    Created { task_id: TaskId, description: String, role: AgentId },
    Completed { task_id: TaskId, outcome: String, role: String },
    NeedsRevision { task_id: TaskId, reason: String, role: String },
    Blocked { task_id: TaskId, role: String, on: Vec<TaskId> },
    Cancelled { task_id: TaskId, role: String, reason: String },
    Failed { task_id: TaskId, error: String, role: String },
}

pub struct State {
    pub model: AgentGraphModel,
    pub selected: Option<usize>,
    pub has_multi_agent_activity: bool,
    /// Per-node thinking logs (agent intent / planning lines).
    thinking_logs: HashMap<usize, Vec<String>>,
    /// Per-node work logs (created / completed / failed events).
    work_logs: HashMap<usize, Vec<String>>,
    /// Stable runtime task IDs mapped to their exact timeline row. Several
    /// repair attempts can use the same role, so role labels are not unique.
    task_nodes: HashMap<TaskId, usize>,
    /// Which log panel is currently expanded for the selected node.
    active_panel: LogPanel,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            model: AgentGraphModel::new(),
            selected: None,
            has_multi_agent_activity: false,
            thinking_logs: HashMap::new(),
            work_logs: HashMap::new(),
            task_nodes: HashMap::new(),
            active_panel: LogPanel::None,
        }
    }

    /// Serialise the current graph + per-node logs to a per-session file.
    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
        let persisted = PersistedGraph {
            has_multi_agent_activity: self.has_multi_agent_activity,
            active_panel: self.active_panel,
            nodes: self
                .model
                .nodes
                .iter()
                .map(|n| PersistedNode {
                    task_id: self
                        .task_nodes
                        .iter()
                        .find_map(|(task_id, node_id)| (*node_id == n.id).then_some(*task_id)),
                    role: n.role.clone(),
                    label: n.label.clone(),
                    state: n.state,
                    task_summary: n.task_summary.clone(),
                    thinking_logs: self.thinking_logs.get(&n.id).cloned().unwrap_or_default(),
                    work_logs: self.work_logs.get(&n.id).cloned().unwrap_or_default(),
                })
                .collect(),
        };
        let json = serde_json::to_string(&persisted)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Load a previously saved graph + logs for a session.
    /// Returns `None` when the file is missing or cannot be parsed.
    pub fn load_from<P: AsRef<Path>>(path: P) -> Option<State> {
        let raw = std::fs::read_to_string(path).ok()?;
        let persisted: PersistedGraph = serde_json::from_str(&raw).ok()?;
        let mut thinking_logs: HashMap<usize, Vec<String>> = HashMap::new();
        let mut work_logs: HashMap<usize, Vec<String>> = HashMap::new();
        let mut task_nodes: HashMap<TaskId, usize> = HashMap::new();
        let mut nodes = Vec::with_capacity(persisted.nodes.len());
        for (id, n) in persisted.nodes.into_iter().enumerate() {
            let state = if Self::is_incomplete(n.state) { NodeState::Cancelled } else { n.state };
            nodes.push(AgentNode {
                id,
                role: n.role,
                label: n.label,
                state,
                task_summary: n.task_summary,
                position: Point::new(0.0, 0.0),
            });
            if let Some(task_id) = n.task_id {
                task_nodes.insert(task_id, id);
            }
            if !n.thinking_logs.is_empty() {
                thinking_logs.insert(id, n.thinking_logs);
            }
            if !n.work_logs.is_empty() {
                work_logs.insert(id, n.work_logs);
            }
        }
        let model = AgentGraphModel::from_persisted(nodes);
        Some(State {
            model,
            selected: None,
            has_multi_agent_activity: persisted.has_multi_agent_activity,
            thinking_logs,
            work_logs,
            task_nodes,
            active_panel: persisted.active_panel,
        })
    }

    fn is_incomplete(state: NodeState) -> bool {
        matches!(
            state,
            NodeState::Idle | NodeState::Active | NodeState::Queued | NodeState::WaitingForApproval
        )
    }

    /// Close any rows that did not receive a terminal event. This prevents a
    /// completed, failed, cancelled, or restored run from displaying agents as
    /// still running while the application itself is idle.
    pub fn settle_incomplete(&mut self, state: NodeState) {
        for node in &mut self.model.nodes {
            if Self::is_incomplete(node.state) {
                node.state = state;
            }
        }
    }

    /// Append a thinking line for a specific agent node (external source).
    pub fn add_thinking_log(&mut self, agent_id: usize, line: String) {
        self.thinking_logs.entry(agent_id).or_default().push(line);
    }

    /// Append a work-log line for a specific agent node (external source).
    pub fn add_work_log(&mut self, agent_id: usize, line: String) {
        self.work_logs.entry(agent_id).or_default().push(line);
    }

    /// Fold a role-scoped runtime thought into the corresponding phase row.
    /// Returns `true` when the event belongs to a known multi-agent node so the
    /// chat view can avoid duplicating it as an unstructured thinking block.
    pub fn on_agent_thought(&mut self, agent_id: &str, content: &str) -> bool {
        let Some(node) = self
            .model
            .nodes
            .iter_mut()
            .rev()
            .find(|node| node.role.as_str().eq_ignore_ascii_case(agent_id))
        else {
            return false;
        };
        node.task_summary = content.to_string();
        let id = node.id;
        self.add_thinking_log(id, content.to_string());
        true
    }

    /// Called when a tool call completes.
    pub fn on_tool_completed(&mut self, _tool_name: &str) {
        // Could update node status if we tracked per-tool
    }

    /// Called when a subtask event arrives from the runtime bridge.
    pub fn on_subtask_created(&mut self, event: SubtaskEvent) {
        self.has_multi_agent_activity = true;
        match event {
            SubtaskEvent::Created { task_id, description, role } => {
                let role_label = role.as_str().to_string();
                let compact_description: String = description.chars().take(40).collect();
                let node_id = self.model.add_node(
                    format!("{role_label}: {compact_description}"),
                    NodeState::Active,
                    role,
                );
                self.task_nodes.insert(task_id, node_id);
                self.selected = Some(node_id);
                // The agent's stated task is its intent (thinking) and the
                // start of its work trail (work log).
                self.add_thinking_log(node_id, format!("Intent: {}", description));
                self.add_work_log(node_id, format!("Started: {}", description));
            }
            SubtaskEvent::Completed { task_id, outcome, role } => {
                if let Some(node) = self.node_for_event(task_id, &role) {
                    node.state = NodeState::Completed;
                    if node.task_summary.is_empty() {
                        node.task_summary = outcome.clone();
                    }
                    let id = node.id;
                    self.add_work_log(id, format!("Completed: {}", outcome));
                }
            }
            SubtaskEvent::NeedsRevision { task_id, reason, role } => {
                if let Some(node) = self.node_for_event(task_id, &role) {
                    node.state = NodeState::NeedsRevision;
                    node.task_summary = reason.clone();
                    let id = node.id;
                    self.add_work_log(id, format!("Needs revision: {}", reason));
                }
            }
            SubtaskEvent::Blocked { task_id, role, on } => {
                if let Some(node) = self.node_for_event(task_id, &role) {
                    node.state = NodeState::Blocked;
                    node.task_summary = format!("Blocked on {on:?}");
                    let id = node.id;
                    self.add_work_log(id, format!("Blocked on {on:?}"));
                }
            }
            SubtaskEvent::Cancelled { task_id, role, reason } => {
                if let Some(node) = self.node_for_event(task_id, &role) {
                    node.state = NodeState::Cancelled;
                    node.task_summary = reason.clone();
                    let id = node.id;
                    self.add_work_log(id, format!("Cancelled: {}", reason));
                }
            }
            SubtaskEvent::Failed { task_id, error, role } => {
                if let Some(node) = self.node_for_event(task_id, &role) {
                    node.state = NodeState::Failed;
                    node.task_summary = error.clone();
                    let id = node.id;
                    self.add_work_log(id, format!("Failed: {}", error));
                }
            }
        }
    }

    fn node_for_event(&mut self, task_id: TaskId, role: &str) -> Option<&mut AgentNode> {
        let node_id = self.task_nodes.get(&task_id).copied().or_else(|| {
            // Compatibility fallback for events or persisted graphs produced
            // by older Concerto versions. Pick the newest matching
            // non-terminal row, never the first role match, because repairs
            // reuse roles.
            let role_prefix = format!("{role}:");
            self.model
                .nodes
                .iter()
                .rev()
                .find(|node| {
                    Self::is_incomplete(node.state) && node.label.starts_with(&role_prefix)
                })
                .map(|node| node.id)
        })?;
        self.model.nodes.iter_mut().find(|node| node.id == node_id)
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::AgentSelected(opt) => {
                self.selected = opt;
                // Reset the open panel when switching agents.
                self.active_panel = LogPanel::None;
                iced::Task::none()
            }
            Message::ViewThinkingLogs(id) => {
                self.selected = Some(id);
                self.active_panel = LogPanel::Thinking;
                iced::Task::none()
            }
            Message::ViewWorkLogs(id) => {
                self.selected = Some(id);
                self.active_panel = LogPanel::Work;
                iced::Task::none()
            }
        }
    }

    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // Show empty state when no multi-agent activity
        if !self.has_multi_agent_activity || self.model.nodes.is_empty() {
            return crate::ui::empty_state(
                theme,
                "⟳",
                "No agent activity",
                "Agent graph is visible during multi-agent tasks.\nSubmit a task with multi-agent mode enabled.",
                None,
            );
        }

        let graph =
            agent_graph::view(self.model.clone(), palette.agent_roles.clone(), palette.text)
                .map(Message::from);

        let side: Element<'_, Message> = if let Some(id) = self.selected {
            if let Some(node) = self.model.nodes.iter().find(|n| n.id == id) {
                let thinking_button =
                    log_button(palette, "View Thinking Logs", Message::ViewThinkingLogs(id));
                let work_button = log_button(palette, "View Work Logs", Message::ViewWorkLogs(id));
                let details = column![
                    text("Agent Details").size(20).color(palette.text),
                    text(format!("Label: {}", node.label)).size(16).color(palette.text),
                    text(format!("State: {:?}", node.state)).size(16).color(palette.text),
                    text("Logs").size(16).color(palette.primary),
                    thinking_button,
                    work_button,
                    self.log_panel(palette, id),
                ]
                .spacing(12)
                .padding(12);
                container(details).width(Length::FillPortion(1)).into()
            } else {
                container(text("No details selected")).into()
            }
        } else {
            container(
                column![
                    text("Select an agent node").size(16).color(palette.text_muted),
                    text("Click a node in the graph to view its details and logs")
                        .size(14)
                        .color(palette.text_muted),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            )
            .width(Length::FillPortion(1))
            .into()
        };

        row![container(graph).width(Length::FillPortion(3)), side].into()
    }

    fn log_panel<'a>(
        &'a self,
        palette: &'a crate::theme::Palette,
        id: usize,
    ) -> Element<'a, Message> {
        match self.active_panel {
            LogPanel::None => container(
                text("Click 'View Thinking Logs' or 'View Work Logs' to inspect this agent.")
                    .size(14)
                    .color(palette.text_muted),
            )
            .padding(8)
            .into(),
            LogPanel::Thinking => self.render_logs(
                palette,
                "Thinking Logs",
                self.thinking_logs.get(&id).map(|v| v.as_slice()).unwrap_or(&[]),
            ),
            LogPanel::Work => self.render_logs(
                palette,
                "Work Logs",
                self.work_logs.get(&id).map(|v| v.as_slice()).unwrap_or(&[]),
            ),
        }
    }

    fn render_logs<'a>(
        &'a self,
        palette: &'a crate::theme::Palette,
        title: &'static str,
        lines: &'a [String],
    ) -> Element<'a, Message> {
        let body: Element<'a, Message> = if lines.is_empty() {
            text("No logs recorded for this agent yet.").size(14).color(palette.text_muted).into()
        } else {
            column(lines.iter().map(|l| text(l.clone()).size(14).color(palette.text).into()))
                .spacing(6)
                .into()
        };
        let content = scrollable(container(body).padding(12).width(Length::Fill))
            .height(Length::FillPortion(2))
            .width(Length::Fill);
        column![text(title).size(16).color(palette.primary), content].spacing(8).into()
    }
}

/// Build a rounded, theme-aware button used to open a log panel.
fn log_button<'a>(
    palette: &'a crate::theme::Palette,
    label: &'a str,
    on_press: Message,
) -> Element<'a, Message> {
    button(
        container(text(label).size(14).color(palette.text)).padding(8).width(Length::Fill).style(
            move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { radius: Radius::from(8.0), ..Default::default() },
                ..Default::default()
            },
        ),
    )
    .style(crate::ui::button::secondary)
    .width(Length::Fill)
    .on_press(on_press)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique temp path so concurrent test runs don't collide.
    fn temp_path() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "concerto-test-agent-graph-{}-{}.json",
            std::process::id(),
            n
        ))
    }

    #[test]
    fn save_and_load_round_trip_preserves_graph_and_logs() {
        let mut state = State::new();
        let task_id = TaskId::new();
        state.on_subtask_created(SubtaskEvent::Created {
            task_id,
            description: "Explore the codebase".to_string(),
            role: AgentId::new("coder"),
        });
        // Created selects node 0 and seeds one thinking + one work line; add more.
        state.add_thinking_log(0, "Considering modules".to_string());
        state.add_work_log(0, "Read agent_loop.rs".to_string());

        let path = temp_path();
        state.save_to(&path).expect("save should succeed");
        let loaded = State::load_from(&path).expect("load should succeed");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded.model.nodes.len(), 1);
        assert_eq!(loaded.model.nodes[0].role, AgentId::new("coder"));
        assert!(loaded.has_multi_agent_activity);
        // thinking: "Intent:" + "Considering modules"; work: "Started:" + "Read agent_loop.rs"
        assert_eq!(loaded.thinking_logs.get(&0).map(Vec::len), Some(2));
        assert_eq!(loaded.work_logs.get(&0).map(Vec::len), Some(2));
        assert_eq!(loaded.active_panel, LogPanel::None);
    }

    #[test]
    fn completed_event_updates_node_state_and_log() {
        let mut state = State::new();
        let task_id = TaskId::new();
        state.on_subtask_created(SubtaskEvent::Created {
            task_id,
            description: "Implement feature".to_string(),
            role: AgentId::new("coder"),
        });
        state.on_subtask_created(SubtaskEvent::Completed {
            task_id,
            outcome: "done".to_string(),
            role: "Coder".to_string(),
        });
        assert_eq!(state.model.nodes[0].state, NodeState::Completed);
        assert!(state.work_logs.get(&0).unwrap().iter().any(|l| l.contains("Completed: done")));

        let path = temp_path();
        state.save_to(&path).unwrap();
        let loaded = State::load_from(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.model.nodes[0].state, NodeState::Completed);
        assert!(loaded.work_logs.get(&0).unwrap().iter().any(|l| l.contains("Completed: done")));
    }

    #[test]
    fn role_thought_updates_structured_phase_instead_of_becoming_orphan_text() {
        let mut state = State::new();
        state.on_subtask_created(SubtaskEvent::Created {
            task_id: TaskId::new(),
            description: "Implement feature".to_string(),
            role: AgentId::new("coder"),
        });

        assert!(state.on_agent_thought("coder", "Coding completed (120 in, 40 out)"));
        assert_eq!(state.model.nodes[0].task_summary, "Coding completed (120 in, 40 out)");
        assert!(!state.on_agent_thought("single-agent", "Still working"));
    }

    #[test]
    fn load_from_missing_file_returns_none() {
        let path = std::env::temp_dir().join("concerto-test-does-not-exist-xyz.json");
        std::fs::remove_file(&path).ok();
        assert!(State::load_from(&path).is_none());
    }

    #[test]
    fn repeated_roles_are_completed_by_task_id() {
        let mut state = State::new();
        let first = TaskId::new();
        let repair = TaskId::new();
        for (task_id, description) in [(first, "Initial pass"), (repair, "Repair pass")] {
            state.on_subtask_created(SubtaskEvent::Created {
                task_id,
                description: description.to_string(),
                role: AgentId::new("coder"),
            });
        }

        state.on_subtask_created(SubtaskEvent::Completed {
            task_id: repair,
            outcome: "repair completed".to_string(),
            role: "Coder".to_string(),
        });

        assert_eq!(state.model.nodes[0].state, NodeState::Active);
        assert_eq!(state.model.nodes[1].state, NodeState::Completed);
    }

    #[test]
    fn restored_incomplete_rows_are_cancelled() {
        let mut state = State::new();
        state.on_subtask_created(SubtaskEvent::Created {
            task_id: TaskId::new(),
            description: "Interrupted work".to_string(),
            role: AgentId::new("researcher"),
        });
        let path = temp_path();
        state.save_to(&path).expect("save should succeed");

        let loaded = State::load_from(&path).expect("load should succeed");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded.model.nodes[0].state, NodeState::Cancelled);
    }
}
