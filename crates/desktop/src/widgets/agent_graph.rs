use iced::widget::canvas::Action;
use iced::widget::canvas::{self, Canvas, Geometry, Path, Program, Text};
use serde::{Deserialize, Serialize};

use concerto_core::AgentId;
use iced::{mouse, Color, Element, Length, Point, Rectangle, Size, Theme};
use petgraph::algo::toposort;
use petgraph::graph::{Graph, NodeIndex};
use std::collections::HashMap;

/// Re-export NodeState for use in the view layer.
pub use AgentState as NodeState;

pub type NodeId = usize;

/// Canonical key for an undirected node pair, so edges in both directions
/// between the same two nodes share one key.
fn unordered_pair(a: usize, b: usize) -> (usize, usize) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Fixed node size in screen pixels. Layout and hit-testing both need it; the
/// graph is drawn at layout positions (no pan/zoom), so world == screen.
const NODE_SIZE: Size = Size::new(120.0, 40.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Active,
    Completed,
    NeedsRevision,
    Blocked,
    Failed,
    Queued,
    WaitingForApproval,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Delegation,
    Dependency,
}

#[derive(Debug, Clone)]
pub struct AgentNode {
    pub id: NodeId,
    pub role: AgentId,
    pub label: String,
    pub state: AgentState,
    pub task_summary: String,
    pub position: Point,
}

#[derive(Debug, Clone)]
pub struct AgentEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    /// Optional human-readable label (e.g. "supervises · 3") drawn at the edge
    /// midpoint. `None` keeps the historical unlabeled edge.
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentGraphModel {
    pub nodes: Vec<AgentNode>,
    pub edges: Vec<AgentEdge>,
    next_id: NodeId,
}

impl Default for AgentGraphModel {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentGraphModel {
    /// Create a new empty graph model.
    pub fn new() -> Self {
        Self { nodes: Vec::new(), edges: Vec::new(), next_id: 0 }
    }

    /// Reconstruct a model from previously persisted nodes.
    ///
    /// Ids are reassigned sequentially in the order given and the layout is
    /// recomputed. Edges are not currently persisted by the view layer.
    pub fn from_persisted(nodes: Vec<AgentNode>) -> Self {
        let next_id = nodes.len();
        let mut model = Self { nodes, edges: Vec::new(), next_id };
        model.layout();
        model
    }

    /// Add a node to the graph. Returns the node ID.
    pub fn add_node(&mut self, label: String, state: AgentState, role: AgentId) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(AgentNode {
            id,
            role,
            label,
            state,
            task_summary: String::new(),
            position: Point::new(0.0, 0.0),
        });
        self.layout();
        id
    }

    /// Add a directed edge between two node ids (indices into `nodes`).
    pub fn add_edge(&mut self, from: NodeId, to: NodeId, kind: EdgeKind) {
        self.edges.push(AgentEdge { from, to, kind, label: None });
        self.layout();
    }

    /// Add a directed edge carrying a midpoint label (relationship editors and
    /// similar configuration surfaces use this to annotate hand-offs).
    pub fn add_labeled_edge(
        &mut self,
        from: NodeId,
        to: NodeId,
        kind: EdgeKind,
        label: impl Into<String>,
    ) {
        self.edges.push(AgentEdge { from, to, kind, label: Some(label.into()) });
        self.layout();
    }

    /// Synthetic placeholder graph with five agents.
    pub fn placeholder() -> Self {
        let roles = [
            AgentId::new("architect"),
            AgentId::new("researcher"),
            AgentId::new("coder"),
            AgentId::new("reviewer"),
            AgentId::new("validator"),
        ];
        let mut nodes = Vec::new();
        for (i, role) in roles.iter().enumerate() {
            nodes.push(AgentNode {
                id: i,
                role: role.clone(),
                label: role.as_str().to_string(),
                state: AgentState::Idle,
                task_summary: format!("{} task", role.as_str()),
                position: Point::new(0.0, 0.0),
            });
        }
        let mut edges = Vec::new();
        for i in 0..nodes.len() - 1 {
            edges.push(AgentEdge { from: i, to: i + 1, kind: EdgeKind::Delegation, label: None });
        }
        edges.push(AgentEdge { from: 2, to: 4, kind: EdgeKind::Dependency, label: None });
        let mut model = Self { nodes, edges, next_id: roles.len() };
        model.layout();
        model
    }

    fn layout(&mut self) {
        let mut graph: Graph<NodeId, EdgeKind> = Graph::new();
        let mut index_map = Vec::new();
        for node in &self.nodes {
            let idx = graph.add_node(node.id);
            index_map.push(idx);
        }
        for edge in &self.edges {
            let from = index_map[edge.from];
            let to = index_map[edge.to];
            graph.add_edge(from, to, edge.kind);
        }
        let order = toposort(&graph, None)
            .unwrap_or_else(|_| (0..self.nodes.len()).map(NodeIndex::new).collect());
        let mut ranks = vec![0usize; self.nodes.len()];
        for (rank, idx) in order.iter().enumerate() {
            let node_id = graph[*idx];
            ranks[node_id] = rank;
        }
        let h_spacing = 200.0;
        let v_spacing = 120.0;
        let max_rank = *ranks.iter().max().unwrap_or(&0);
        let mut rank_buckets: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
        for (i, &r) in ranks.iter().enumerate() {
            rank_buckets[r].push(i);
        }
        for (rank, bucket) in rank_buckets.iter().enumerate() {
            let y = rank as f32 * v_spacing + 50.0;
            for (i, &node_idx) in bucket.iter().enumerate() {
                let x = i as f32 * h_spacing + 50.0;
                if let Some(node) = self.nodes.get_mut(node_idx) {
                    node.position = Point::new(x, y);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A node was clicked; the payload is the index into `model.nodes`, which
    /// equals the index into the studio's `agents`.
    NodeClicked(usize),
}

/// Whether the cursor is over a node's rectangle. The node rect is positioned
/// in screen space because the pure-visualization canvas draws nodes at their
/// layout positions with no pan/zoom transform (world == screen).
fn node_contains(node: &AgentNode, cursor: Point) -> bool {
    Rectangle::new(node.position, NODE_SIZE).contains(cursor)
}

pub struct GraphProgram {
    model: AgentGraphModel,
    agent_roles: HashMap<AgentId, Color>,
    text_color: Color,
}

impl GraphProgram {
    pub fn new(
        model: AgentGraphModel,
        agent_roles: HashMap<AgentId, Color>,
        text_color: Color,
    ) -> Self {
        Self { model, agent_roles, text_color }
    }

    fn node_color(&self, role: &AgentId) -> Color {
        self.agent_roles.get(role).copied().unwrap_or(Color::from_rgb(0.5, 0.5, 0.5))
    }

    fn state_glyph(state: AgentState) -> &'static str {
        match state {
            AgentState::Idle => "⏸",
            AgentState::Active => "▶",
            AgentState::Completed => "✔",
            AgentState::NeedsRevision => "⟳",
            AgentState::Blocked => "⛔",
            AgentState::Failed => "✖",
            AgentState::Queued => "⏳",
            AgentState::WaitingForApproval => "⏱",
            AgentState::Cancelled => "✖",
        }
    }
}

impl Program<Message> for GraphProgram {
    // The canvas is a pure visualization now: no pan/zoom/drag state.
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        // Bidirectional pairs draw two labels at the same midpoint, so
        // they overlap. Count edges per unordered node pair and push the
        // pair's labels to opposite sides of the line via a per-edge
        // perpendicular offset (10 px along the pair's canonical normal).
        let mut pair_counts: HashMap<(usize, usize), u32> = HashMap::new();
        for edge in &self.model.edges {
            let key = unordered_pair(edge.from, edge.to);
            *pair_counts.entry(key).or_insert(0) += 1;
        }
        let mut pair_normals: HashMap<(usize, usize), (f32, f32)> = HashMap::new();
        let mut pair_side: HashMap<(usize, usize), f32> = HashMap::new();
        for edge in &self.model.edges {
            let key = unordered_pair(edge.from, edge.to);
            if pair_counts.get(&key).copied().unwrap_or(0) < 2 || pair_normals.contains_key(&key) {
                continue;
            }
            // Canonical direction: from the lower-index node to the
            // higher-index node, so both edges of a pair share the same
            // normal orientation and get pushed to opposite sides.
            let (from, to) =
                if edge.from <= edge.to { (edge.from, edge.to) } else { (edge.to, edge.from) };
            let start = Point::new(
                self.model.nodes[from].position.x + 60.0,
                self.model.nodes[from].position.y + 20.0,
            );
            let end = Point::new(
                self.model.nodes[to].position.x + 60.0,
                self.model.nodes[to].position.y + 20.0,
            );
            let dx = end.x - start.x;
            let dy = end.y - start.y;
            let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
            pair_normals.insert(key, (-dy / len, dx / len));
        }
        for edge in &self.model.edges {
            let from_node = &self.model.nodes[edge.from];
            let to_node = &self.model.nodes[edge.to];
            let start = Point::new(from_node.position.x + 60.0, from_node.position.y + 20.0);
            let end = Point::new(to_node.position.x + 60.0, to_node.position.y + 20.0);
            let path = Path::new(|p| {
                p.move_to(start);
                p.line_to(end);
            });
            let stroke = canvas::Stroke::default().with_width(2.0);
            frame.stroke(&path, stroke);
            if let Some(label) = &edge.label {
                let midpoint = Point::new((start.x + end.x) / 2.0, (start.y + end.y) / 2.0);
                let label_position = match pair_normals.get(&unordered_pair(edge.from, edge.to)) {
                    Some((nx, ny)) => {
                        let side =
                            pair_side.entry(unordered_pair(edge.from, edge.to)).or_insert(1.0);
                        let offset = *side;
                        *side = -offset;
                        Point::new(midpoint.x + nx * 10.0 * offset, midpoint.y + ny * 10.0 * offset)
                    }
                    None => midpoint,
                };
                frame.fill_text(Text {
                    content: label.clone(),
                    position: label_position,
                    // Derive from the theme text color so labels stay
                    // legible on light themes too (hardcoded white
                    // vanished on Chalk).
                    color: Color::from_rgba(
                        self.text_color.r,
                        self.text_color.g,
                        self.text_color.b,
                        0.85,
                    ),
                    size: iced::Pixels(11.0),
                    ..Text::default()
                });
            }
        }
        for node in &self.model.nodes {
            let top_left = node.position;
            let _rect = Rectangle::new(top_left, NODE_SIZE);

            let mut fill = self.node_color(&node.role);

            match node.state {
                AgentState::Idle | AgentState::Cancelled | AgentState::Queued => {
                    fill = Color::from_rgba(fill.r, fill.g, fill.b, fill.a * 0.5);
                }
                _ => {}
            }
            frame.fill_rectangle(top_left, NODE_SIZE, fill);
            frame.stroke_rectangle(top_left, NODE_SIZE, canvas::Stroke::default().with_width(2.0));

            let txt = Text {
                content: node.label.clone(),
                position: Point::new(top_left.x + 10.0, top_left.y + 10.0),
                color: self.text_color,
                size: iced::Pixels(16.0),
                ..Text::default()
            };
            frame.fill_text(txt);

            let glyph = Self::state_glyph(node.state);
            let glyph_txt = Text {
                content: glyph.to_string(),
                position: Point::new(top_left.x + 120.0 - 14.0, top_left.y + 6.0),
                color: self.text_color,
                size: iced::Pixels(14.0),
                ..Text::default()
            };
            frame.fill_text(glyph_txt);
        }
        // Hover highlight: a fresh overlay layer built on every draw, so it
        // stays correct across renders. A ring around the hovered node keeps
        // the draw-time cursor check that the previous edge-highlight overlay
        // provided, now that edges are no longer interactive.
        let mut geometries = vec![frame.into_geometry()];
        if let Some(pos) = cursor.position() {
            if let Some(node) = self.model.nodes.iter().find(|node| node_contains(node, pos)) {
                let mut overlay = canvas::Frame::new(renderer, bounds.size());
                overlay.stroke_rectangle(
                    node.position,
                    NODE_SIZE,
                    canvas::Stroke::default().with_width(3.0).with_color(self.text_color),
                );
                geometries.push(overlay.into_geometry());
            }
        }
        geometries
    }

    fn update(
        &self,
        _state: &mut Self::State,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        match event {
            iced::Event::Mouse(mouse_event) => {
                // Only interact while the cursor is over the canvas; a click
                // elsewhere in the window (e.g. over the Hand-offs or Run
                // Limits panels) must pass through untouched.
                if !cursor.is_over(bounds) {
                    return None;
                }
                match mouse_event {
                    mouse::Event::ButtonPressed(mouse::Button::Left) => {
                        let pos = cursor.position()?;
                        for (idx, node) in self.model.nodes.iter().enumerate() {
                            if node_contains(node, pos) {
                                return Some(Action::publish(Message::NodeClicked(idx)));
                            }
                        }
                        None
                    }
                    // All wheel and other events return `None` (no capture), so
                    // they bubble to the enclosing scrollable (issue #113).
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if !cursor.is_over(bounds) {
            return mouse::Interaction::default();
        }
        if let Some(pos) = cursor.position() {
            if self.model.nodes.iter().any(|node| node_contains(node, pos)) {
                return mouse::Interaction::Pointer;
            }
        }
        mouse::Interaction::default()
    }
}

/// Build a canvas element for the model. The model is cloned into the widget's
/// internal program, so it may be constructed inline without outliving the
/// returned element. `text_color` should come from the active theme palette so
/// labels stay legible on dark and light themes alike.
pub fn view<'a>(
    model: AgentGraphModel,
    agent_roles: HashMap<AgentId, Color>,
    text_color: Color,
) -> Element<'a, Message> {
    Canvas::new(GraphProgram::new(model, agent_roles, text_color))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel_event() -> iced::Event {
        iced::Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        })
    }

    fn test_program() -> GraphProgram {
        GraphProgram::new(AgentGraphModel::new(), HashMap::new(), iced::Color::WHITE)
    }

    /// Two nodes 200px apart with one delegation edge between them. Positions
    /// are laid out by hand (screen space; the pure canvas draws at layout
    /// positions with no transform).
    fn sample_model() -> AgentGraphModel {
        AgentGraphModel {
            nodes: vec![
                AgentNode {
                    id: 0,
                    role: AgentId::new("architect"),
                    label: "architect".into(),
                    state: AgentState::Idle,
                    task_summary: String::new(),
                    position: Point::new(0.0, 0.0),
                },
                AgentNode {
                    id: 1,
                    role: AgentId::new("coder"),
                    label: "coder".into(),
                    state: AgentState::Idle,
                    task_summary: String::new(),
                    position: Point::new(200.0, 0.0),
                },
            ],
            edges: vec![AgentEdge { from: 0, to: 1, kind: EdgeKind::Delegation, label: None }],
            next_id: 2,
        }
    }

    /// A plain wheel scroll over the graph must not be captured: returning no
    /// action lets it bubble to the enclosing scrollable, so the page still
    /// scrolls (issue #113).
    #[test]
    fn wheel_scroll_events_return_none_action() {
        let program = test_program();
        let bounds = Rectangle::new(Point::new(0.0, 0.0), Size::new(1000.0, 1000.0));
        let cursor = mouse::Cursor::Available(Point::new(500.0, 500.0));

        let action = program.update(&mut (), &wheel_event(), bounds, cursor);

        assert!(action.is_none(), "wheel must not be captured by the canvas");
    }

    /// A left click over a node publishes `NodeClicked` without capturing the
    /// event (status stays `Ignored`), so the parent scrollable stays functional.
    #[test]
    fn click_on_node_publishes_without_capture() {
        let program = GraphProgram::new(sample_model(), HashMap::new(), Color::WHITE);
        let bounds = Rectangle::new(Point::new(0.0, 0.0), Size::new(400.0, 300.0));
        // Node 0 occupies (0,0)-(120,40); click inside it.
        let action = program
            .update(
                &mut (),
                &iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point::new(60.0, 20.0)),
            )
            .expect("click on a node must produce an action");

        let (message, _, status) = action.into_inner();
        assert!(matches!(message, Some(Message::NodeClicked(0))));
        assert_eq!(status, iced::event::Status::Ignored);
    }

    /// A click outside any node returns no action (and thus does not capture).
    #[test]
    fn click_outside_any_node_returns_none() {
        let program = GraphProgram::new(sample_model(), HashMap::new(), Color::WHITE);
        let bounds = Rectangle::new(Point::new(0.0, 0.0), Size::new(400.0, 300.0));
        // Between the two nodes' rects — not over either node.
        let action = program.update(
            &mut (),
            &iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point::new(160.0, 10.0)),
        );

        assert!(action.is_none());
    }

    /// Over a node the mouse interaction is a pointer; elsewhere it is default.
    #[test]
    fn mouse_interaction_shows_pointer_over_nodes() {
        let program = GraphProgram::new(sample_model(), HashMap::new(), Color::WHITE);
        let bounds = Rectangle::new(Point::new(0.0, 0.0), Size::new(1000.0, 1000.0));
        // Over a node.
        assert_eq!(
            program.mouse_interaction(
                &(),
                bounds,
                mouse::Cursor::Available(Point::new(60.0, 20.0))
            ),
            mouse::Interaction::Pointer
        );
        // Far away from any node.
        assert_eq!(
            program.mouse_interaction(
                &(),
                bounds,
                mouse::Cursor::Available(Point::new(500.0, 300.0))
            ),
            mouse::Interaction::default()
        );
        // Outside the canvas bounds entirely.
        assert_eq!(
            program.mouse_interaction(
                &(),
                Rectangle::new(Point::new(0.0, 0.0), Size::new(100.0, 100.0)),
                mouse::Cursor::Available(Point::new(500.0, 500.0)),
            ),
            mouse::Interaction::default()
        );
    }

    /// Toposort layout: single chain of two nodes keeps node 0 at rank 0 and
    /// node 1 at rank 1 (the vertical axis), preserving the layout math used by
    /// the streaming studio.
    #[test]
    fn toposort_layout_assigns_positions_by_rank() {
        let model = AgentGraphModel::placeholder();
        let node_ids: Vec<usize> = model.nodes.iter().map(|n| n.id).collect();
        assert_eq!(node_ids, vec![0, 1, 2, 3, 4]);
        // Architect (rank 0) sits above coder (rank 1).
        let arch = &model.nodes[0];
        let coder = &model.nodes[2];
        assert!(coder.position.y > arch.position.y);
        assert_eq!(arch.position.y, 50.0);
    }
}
