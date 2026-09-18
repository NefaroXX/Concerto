//! ADR-69 slice 3 — read-only memory graph modal.
//!
//! Renders the project's memory chunks + links (loaded from
//! `<app data>/memory/memory.db`) as Mermaid `flowchart TD` source in a
//! scrollable, monospace modal. Pure render: keeps the memory modal's
//! "explorer" state completely independent, and never writes to the DB.

use iced::widget::{column, container, row, scrollable, text};
use iced::{Alignment, Element, Length};

use concerto_memory::mermaid::{render_mermaid, GraphCap, MemoryGraph};

use crate::theme::AppTheme;
use crate::ui::empty_state;

/// Messages emitted by the memory graph modal body.
#[derive(Debug, Clone)]
pub enum Message {
    /// Re-read the memory db and re-render.
    Refresh,
}

/// Modal body state: loading, a rendered graph, or a load error.
#[derive(Debug, Clone, Default)]
pub enum State {
    #[default]
    Idle,
    Loading,
    Loaded(MemoryGraph),
    Error(String),
}

impl State {
    /// Initial state before the first load task resolves.
    pub fn new() -> Self {
        Self::Idle
    }

    /// The modal body — a header row plus the Mermaid source (or an
    /// empty/error state). The App wraps this in the modal card + backdrop.
    pub fn modal_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        match self {
            State::Idle | State::Loading => empty_state(
                theme,
                text("◌").size(28),
                "Loading memory graph…",
                "Reading the project's memory.db (read-only).",
                None::<(String, Message)>,
            ),
            State::Error(error) => empty_state(
                theme,
                text("△").size(28),
                "Could not load memory graph",
                error.clone(),
                None::<(String, Message)>,
            ),
            State::Loaded(graph) => {
                let source = render_mermaid(graph, GraphCap::default());
                let summary = format!("{} nodes · {} edges", graph.nodes.len(), graph.edges.len());
                container(
                    column![
                        row![
                            container(text(summary).size(12).color(theme.palette.text_muted))
                                .width(Length::Fill)
                                .align_y(Alignment::Center),
                            text("Mermaid flowchart TD — read only")
                                .size(11)
                                .color(theme.palette.text_muted),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                        scrollable(
                            container(
                                text(source)
                                    .size(12)
                                    .font(theme.font_stack.mono)
                                    .color(theme.palette.text)
                            )
                            .padding(8)
                            .width(Length::Fill),
                        )
                        .height(Length::Fill),
                    ]
                    .spacing(8)
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::MemoryLinkKind;
    use concerto_memory::mermaid::{GraphEdge, GraphNode};

    fn sample_graph() -> MemoryGraph {
        MemoryGraph {
            nodes: vec![
                GraphNode { id: "a".into(), label: "alpha".into(), score: 1.0 },
                GraphNode { id: "b".into(), label: "beta".into(), score: 0.5 },
            ],
            edges: vec![GraphEdge {
                from: "a".into(),
                to: "b".into(),
                kind: MemoryLinkKind::Supports,
                weight: 1.0,
                score: 1.0,
            }],
        }
    }

    #[test]
    fn memory_graph_idle_renders() {
        let theme = AppTheme::by_name("Midnight");
        let state = State::new();
        let _element = state.modal_view(&theme);
    }

    #[test]
    fn memory_graph_loaded_renders() {
        let theme = AppTheme::by_name("Midnight");
        let state = State::Loaded(sample_graph());
        let _element = state.modal_view(&theme);
    }

    #[test]
    fn memory_graph_error_renders() {
        let theme = AppTheme::by_name("Midnight");
        let state = State::Error("boom".into());
        let _element = state.modal_view(&theme);
    }
}
