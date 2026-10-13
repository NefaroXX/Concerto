//! Harness modal (H13): desktop controls over the shared harness service.
//!
//! The panel issues the same `HarnessService` operations the CLI and
//! in-process paths use (`environment.describe`, `verify.run`,
//! `checkpoint.create/inspect/restore`, `memory.recall/inspect`) and renders
//! the service's [`HarnessInvocation`] verbatim — including refusal, partial,
//! and degraded states — rather than any desktop-only shape.
//!
//! The view never invokes a tool, a store, or a database: it emits a
//! [`HarnessRequest`] that the App resolves through the one service seam.

use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};

use concerto_orchestrator::harness_service::HarnessInvocation;

use crate::theme::AppTheme;

/// One request the panel wants the App to run through the service.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessRequest {
    /// The service operation name.
    pub operation: String,
    /// The operation input.
    pub input: serde_json::Value,
    /// Whether the operation needs the memory subsystem.
    pub memory: bool,
}

/// Messages the harness panel emits.
#[derive(Debug, Clone)]
pub enum Message {
    /// A text field changed.
    PathChanged(String),
    CheckpointIdChanged(String),
    ChunkIdChanged(String),
    QueryChanged(String),
    /// The operator pressed a control: run this operation.
    Request(HarnessRequest),
    /// The App's service call finished.
    Completed(Result<HarnessInvocation, String>),
}

/// Panel state: the last invocation (or refusal) and the input fields.
pub struct State {
    path: String,
    checkpoint_id: String,
    chunk_id: String,
    query: String,
    /// The last completed invocation, or the service's refusal text.
    pub last: Option<Result<HarnessInvocation, String>>,
    /// A service call is in flight.
    pub busy: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// Fresh panel.
    pub fn new() -> Self {
        Self {
            path: String::new(),
            checkpoint_id: String::new(),
            chunk_id: String::new(),
            query: String::new(),
            last: None,
            busy: false,
        }
    }

    /// Apply a panel message. Returns the request to run, when the operator
    /// pressed a control (the App runs it through the service).
    pub fn update(&mut self, message: Message) -> Option<HarnessRequest> {
        match message {
            Message::PathChanged(value) => self.path = value,
            Message::CheckpointIdChanged(value) => self.checkpoint_id = value,
            Message::ChunkIdChanged(value) => self.chunk_id = value,
            Message::QueryChanged(value) => self.query = value,
            Message::Request(request) => {
                self.busy = true;
                return Some(request);
            }
            Message::Completed(result) => {
                self.busy = false;
                self.last = Some(result);
            }
        }
        None
    }

    /// The request a "Describe Environment" press issues.
    pub fn describe_request() -> HarnessRequest {
        HarnessRequest {
            operation: "environment.describe".to_string(),
            input: serde_json::json!({}),
            memory: false,
        }
    }

    /// The request a "Run Verification" press issues.
    pub fn verify_request() -> HarnessRequest {
        HarnessRequest {
            operation: "verify.run".to_string(),
            input: serde_json::json!({}),
            memory: false,
        }
    }

    /// The request a "Create Checkpoint" press issues (needs a path).
    pub fn create_checkpoint_request(&self) -> Option<HarnessRequest> {
        let path = self.path.trim();
        if path.is_empty() {
            return None;
        }
        Some(HarnessRequest {
            operation: "checkpoint.create".to_string(),
            input: serde_json::json!({ "paths": [path] }),
            memory: false,
        })
    }

    /// The request an "Inspect Checkpoint" press issues (needs an id).
    pub fn inspect_checkpoint_request(&self) -> Option<HarnessRequest> {
        let id = self.checkpoint_id.trim();
        if id.is_empty() {
            return None;
        }
        Some(HarnessRequest {
            operation: "checkpoint.inspect".to_string(),
            input: serde_json::json!({ "checkpoint_id": id }),
            memory: false,
        })
    }

    /// The request a "Restore Checkpoint" press issues (needs an id).
    pub fn restore_checkpoint_request(&self) -> Option<HarnessRequest> {
        let id = self.checkpoint_id.trim();
        if id.is_empty() {
            return None;
        }
        Some(HarnessRequest {
            operation: "checkpoint.restore".to_string(),
            input: serde_json::json!({ "checkpoint_id": id }),
            memory: false,
        })
    }

    /// The request a "Recall Memory" press issues (needs a query).
    pub fn recall_request(&self) -> Option<HarnessRequest> {
        let query = self.query.trim();
        if query.is_empty() {
            return None;
        }
        Some(HarnessRequest {
            operation: "memory.recall".to_string(),
            input: serde_json::json!({ "query": query }),
            memory: true,
        })
    }

    /// The request an "Inspect Chunk" press issues (needs an id).
    pub fn inspect_chunk_request(&self) -> Option<HarnessRequest> {
        let id = self.chunk_id.trim();
        if id.is_empty() {
            return None;
        }
        Some(HarnessRequest {
            operation: "memory.inspect".to_string(),
            input: serde_json::json!({ "chunk_id": id }),
            memory: true,
        })
    }

    /// Render the panel.
    pub fn view(&self, theme: &AppTheme) -> Element<'_, Message> {
        let palette = &theme.palette;

        let controls = row![
            button(text("Describe Environment").size(12))
                .style(crate::ui::button::secondary)
                .on_press(Message::Request(Self::describe_request())),
            button(text("Run Verification").size(12))
                .style(crate::ui::button::secondary)
                .on_press(Message::Request(Self::verify_request())),
        ]
        .spacing(8);

        let checkpoint_controls = row![
            text_input("path (project-relative)", &self.path)
                .on_input(Message::PathChanged)
                .width(Length::Fill),
            button(text("Create Checkpoint").size(12))
                .style(crate::ui::button::secondary)
                .on_press_maybe(self.create_checkpoint_request().map(Message::Request)),
        ]
        .spacing(8);

        let checkpoint_id_controls = row![
            text_input("checkpoint id", &self.checkpoint_id)
                .on_input(Message::CheckpointIdChanged)
                .width(Length::Fill),
            button(text("Inspect").size(12))
                .style(crate::ui::button::secondary)
                .on_press_maybe(self.inspect_checkpoint_request().map(Message::Request)),
            button(text("Restore").size(12))
                .style(crate::ui::button::secondary)
                .on_press_maybe(self.restore_checkpoint_request().map(Message::Request)),
        ]
        .spacing(8);

        let memory_controls = row![
            text_input("recall query", &self.query)
                .on_input(Message::QueryChanged)
                .width(Length::Fill),
            button(text("Recall").size(12))
                .style(crate::ui::button::secondary)
                .on_press_maybe(self.recall_request().map(Message::Request)),
        ]
        .spacing(8);

        let chunk_controls = row![
            text_input("chunk id", &self.chunk_id)
                .on_input(Message::ChunkIdChanged)
                .width(Length::Fill),
            button(text("Inspect Chunk").size(12))
                .style(crate::ui::button::secondary)
                .on_press_maybe(self.inspect_chunk_request().map(Message::Request)),
        ]
        .spacing(8);

        let status_line = if self.busy {
            text("running…").size(12).color(palette.text_muted)
        } else {
            text("").size(12)
        };

        let result: Element<'_, Message> = match &self.last {
            None => {
                container(text("No operation run yet.").size(12).color(palette.text_muted)).into()
            }
            Some(Err(error)) => {
                container(text(format!("refused: {error}")).size(12).color(palette.warning)).into()
            }
            Some(Ok(invocation)) => {
                let mut header = format!("{} — {}", invocation.operation, invocation.summary);
                if invocation.is_degraded() {
                    header.push_str("\ndegraded: true (reduced capability)");
                }
                if invocation.is_partial() {
                    header.push_str("\npartial: true (see payload)");
                }
                let payload = serde_json::to_string_pretty(&invocation.data)
                    .unwrap_or_else(|error| format!("(payload render failed: {error})"));
                scrollable(
                    column![
                        text(header).size(12),
                        text(payload).size(11).color(palette.text_muted),
                    ]
                    .spacing(6),
                )
                .height(Length::Fill)
                .into()
            }
        };

        column![
            controls,
            checkpoint_controls,
            checkpoint_id_controls,
            memory_controls,
            chunk_controls,
            status_line,
            result,
        ]
        .spacing(8)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::EvidentChunk;

    fn theme() -> AppTheme {
        AppTheme::by_name("Midnight")
    }

    #[test]
    fn request_builders_require_input() {
        let mut state = State::new();
        assert!(state.create_checkpoint_request().is_none());
        assert!(state.inspect_checkpoint_request().is_none());
        assert!(state.recall_request().is_none());
        assert!(state.inspect_chunk_request().is_none());

        state.update(Message::PathChanged("src/lib.rs".into()));
        let request = state.create_checkpoint_request().expect("path present");
        assert_eq!(request.operation, "checkpoint.create");
        assert_eq!(request.input["paths"][0], "src/lib.rs");

        state.update(Message::QueryChanged("why".into()));
        let request = state.recall_request().expect("query present");
        assert_eq!(request.operation, "memory.recall");
        assert!(request.memory, "recall needs the memory subsystem");
    }

    #[test]
    fn update_returns_requests_and_records_completion() {
        let mut state = State::new();
        let request = HarnessRequest {
            operation: "environment.describe".into(),
            input: serde_json::json!({}),
            memory: false,
        };
        let emitted = state.update(Message::Request(request.clone()));
        assert_eq!(emitted, Some(request));
        assert!(state.busy);

        // A refusal is recorded and never silently dropped.
        state.update(Message::Completed(Err("policy denied".into())));
        assert!(!state.busy);
        assert!(matches!(&state.last, Some(Err(error)) if error == "policy denied"));
    }

    /// The panel renders without panicking in every state (idle, busy,
    /// refusal, and a degraded/partial invocation).
    #[test]
    fn view_renders_without_panic() {
        let mut state = State::new();
        let _ = state.view(&theme());
        let _ = state.update(Message::Request(State::describe_request()));
        let _ = state.view(&theme());
        let _ = state.update(Message::Completed(Err("refused".into())));
        let _ = state.view(&theme());
        let _ = state.update(Message::Completed(Ok(HarnessInvocation {
            operation: "memory.recall".into(),
            summary: "chunks=0 degraded".into(),
            data: serde_json::json!({ "chunks": Vec::<EvidentChunk>::new(), "degraded": true, "interrupted": false }),
        })));
        let _ = state.view(&theme());
    }
}
