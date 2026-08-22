use crate::theme::AppTheme;
use iced::widget::{button, column, container, pick_list, row, scrollable, text, Column};
use iced::{Element, Length};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum Message {
    ToggleRow(usize),
    FilterChanged(ToolLogFilter),
}

/// Filter options for the tool log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLogFilter {
    All,
    Allowed,
    Denied,
    Running,
}

impl ToolLogFilter {
    fn all() -> &'static [Self] {
        &[Self::All, Self::Allowed, Self::Denied, Self::Running]
    }
}

impl std::fmt::Display for ToolLogFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => write!(f, "All"),
            Self::Allowed => write!(f, "Allowed"),
            Self::Denied => write!(f, "Denied"),
            Self::Running => write!(f, "Running"),
        }
    }
}

/// A row in the tool log.
#[derive(Debug, Clone)]
pub struct ToolLogRow {
    pub id: usize,
    pub timestamp: String,
    pub tool_name: String,
    pub input_summary: String,
    pub full_input: String,
    pub output_summary: String,
    pub full_output: String,
    pub duration_ms: u64,
    pub verdict: ToolVerdict,
    pub policy_rule: String,
    pub expanded: bool,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolVerdict {
    Allowed,
    Denied,
    Running,
    Pending,
}

/// Update operations for the tool log state (fed from event bus).
#[derive(Debug, Clone)]
pub enum ToolLogUpdate {
    Started { tool_name: String, input_summary: String, full_input: String },
    Completed { tool_name: String, duration_ms: u64, success: bool },
    OutputChunk { tool_name: String, chunk: String, is_stderr: bool },
    Failed { tool_name: String, error: String },
    Verdict { tool_name: String, approved: bool },
    PolicyMatched { tool_name: String, rule: Option<String> },
}

pub struct State {
    pub rows: Vec<ToolLogRow>,
    next_id: usize,
    filter: ToolLogFilter,
    /// Map from tool_name to row index for quick lookup.
    tool_index: HashMap<String, usize>,
}

const MAX_LIVE_TOOL_ROWS: usize = 2_000;
const MAX_LIVE_TOOL_OUTPUT_CHARS: usize = 256_000;

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            next_id: 1,
            filter: ToolLogFilter::All,
            tool_index: HashMap::new(),
        }
    }

    /// Rebuild this session's tool log from the durable event stream.
    pub fn load_stored_events(&mut self, events: &[concerto_sessions::replay::StoredEvent]) {
        *self = Self::new();
        for stored in events {
            let Ok(event) = stored.to_event() else { continue };
            match event.kind {
                concerto_core::event::EventKind::ToolExecutionStarted {
                    tool_name,
                    input_hash,
                    detail,
                } => self.add_or_update(&ToolLogUpdate::Started {
                    tool_name,
                    input_summary: detail.unwrap_or_default(),
                    full_input: input_hash,
                }),
                concerto_core::event::EventKind::ToolExecutionFinished {
                    tool_name,
                    duration_ms,
                    success,
                    ..
                } => self.add_or_update(&ToolLogUpdate::Completed {
                    tool_name,
                    duration_ms,
                    success,
                }),
                concerto_core::event::EventKind::ShellOutputChunk { chunk, is_stderr } => {
                    self.add_or_update(&ToolLogUpdate::OutputChunk {
                        tool_name: "shell".to_string(),
                        chunk,
                        is_stderr,
                    });
                }
                concerto_core::event::EventKind::ToolTimeout { tool_name, .. } => {
                    self.add_or_update(&ToolLogUpdate::Failed {
                        tool_name,
                        error: "Tool timed out".to_string(),
                    });
                }
                concerto_core::event::EventKind::ApprovalResolved { tool_name, approved } => {
                    self.add_or_update(&ToolLogUpdate::Verdict { tool_name, approved });
                }
                concerto_core::event::EventKind::PolicyEvaluated {
                    tool_name,
                    rule_matched,
                    ..
                } => {
                    self.add_or_update(&ToolLogUpdate::PolicyMatched {
                        tool_name: tool_name.clone(),
                        rule: rule_matched.clone(),
                    });
                }
                _ => {}
            }
        }
    }

    /// Add or update a tool log row based on an event update.
    pub fn add_or_update(&mut self, update: &ToolLogUpdate) {
        match update {
            ToolLogUpdate::Started { tool_name, input_summary, full_input } => {
                let id = self.next_id;
                self.next_id += 1;
                self.tool_index.insert(tool_name.clone(), self.rows.len());
                self.rows.push(ToolLogRow {
                    id,
                    timestamp: chrono_or_time(),
                    tool_name: tool_name.clone(),
                    input_summary: input_summary.clone(),
                    full_input: full_input.clone(),
                    output_summary: String::new(),
                    full_output: String::new(),
                    duration_ms: 0,
                    verdict: ToolVerdict::Running,
                    policy_rule: String::new(),
                    expanded: false,
                    error: String::new(),
                });
                if self.rows.len() > MAX_LIVE_TOOL_ROWS {
                    self.rows.remove(0);
                    self.tool_index.clear();
                    for (index, row) in self.rows.iter().enumerate() {
                        self.tool_index.insert(row.tool_name.clone(), index);
                    }
                }
            }
            ToolLogUpdate::Completed { tool_name, duration_ms, success } => {
                if let Some(&idx) = self.tool_index.get(tool_name) {
                    if let Some(row) = self.rows.get_mut(idx) {
                        row.duration_ms = *duration_ms;
                        row.verdict =
                            if *success { ToolVerdict::Allowed } else { ToolVerdict::Denied };
                    }
                }
            }
            ToolLogUpdate::OutputChunk { tool_name, chunk, is_stderr } => {
                if let Some(&idx) = self.tool_index.get(tool_name) {
                    if let Some(row) = self.rows.get_mut(idx) {
                        if *is_stderr {
                            row.full_output.push_str("[stderr] ");
                        }
                        row.full_output.push_str(chunk);
                        if row.full_output.chars().count() > MAX_LIVE_TOOL_OUTPUT_CHARS {
                            let skip = row.full_output.chars().count() - MAX_LIVE_TOOL_OUTPUT_CHARS;
                            row.full_output = format!(
                                "[older output omitted; full output remains in session events]\n{}",
                                row.full_output.chars().skip(skip).collect::<String>()
                            );
                        }
                        row.output_summary = row.full_output.chars().take(200).collect();
                    }
                }
            }
            ToolLogUpdate::Failed { tool_name, error } => {
                if let Some(&idx) = self.tool_index.get(tool_name) {
                    if let Some(row) = self.rows.get_mut(idx) {
                        row.verdict = ToolVerdict::Denied;
                        row.error = error.clone();
                    }
                }
            }
            ToolLogUpdate::Verdict { tool_name, approved } => {
                if let Some(&idx) = self.tool_index.get(tool_name) {
                    if let Some(row) = self.rows.get_mut(idx) {
                        row.verdict =
                            if *approved { ToolVerdict::Allowed } else { ToolVerdict::Denied };
                    }
                }
            }
            ToolLogUpdate::PolicyMatched { tool_name, rule } => {
                if let Some(&idx) = self.tool_index.get(tool_name) {
                    if let Some(row) = self.rows.get_mut(idx) {
                        row.policy_rule = rule.clone().unwrap_or_default();
                    }
                }
            }
        }
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ToggleRow(idx) => {
                if let Some(row) = self.rows.get_mut(idx) {
                    row.expanded = !row.expanded;
                }
            }
            Message::FilterChanged(filter) => {
                self.filter = filter;
            }
        }
        iced::Task::none()
    }

    fn filtered_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match self.filter {
                ToolLogFilter::All => true,
                ToolLogFilter::Allowed => row.verdict == ToolVerdict::Allowed,
                ToolLogFilter::Denied => row.verdict == ToolVerdict::Denied,
                ToolLogFilter::Running => row.verdict == ToolVerdict::Running,
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        if self.rows.is_empty() {
            return crate::ui::empty_state(
                theme,
                "⚙",
                "No tool events recorded",
                "Tool calls will appear here when the agent runs.",
                None,
            );
        }

        // Filter bar
        let filter = pick_list(ToolLogFilter::all(), Some(self.filter), Message::FilterChanged);

        let filtered = self.filtered_rows();
        let rows: Vec<Element<'_, Message>> = filtered
            .into_iter()
            .map(|i| {
                let row = &self.rows[i];
                let (verdict_icon, verdict_color) = match row.verdict {
                    ToolVerdict::Allowed => ("✓", palette.success),
                    ToolVerdict::Denied => ("✗", palette.danger),
                    ToolVerdict::Running => ("⟳", palette.warning),
                    ToolVerdict::Pending => ("?", palette.text_muted),
                };

                let header = row![
                    text(&row.timestamp).size(11).width(80),
                    text(verdict_icon).size(13).color(verdict_color).width(20),
                    text(&row.tool_name).width(120).style(move |_theme: &iced::Theme| {
                        text::Style { color: Some(verdict_color) }
                    }),
                    text(&row.input_summary).width(Length::Fill),
                    if row.duration_ms > 0 {
                        text(format!("{}ms", row.duration_ms)).size(11).width(60)
                    } else {
                        text("").width(60)
                    },
                    button(if row.expanded { text("▲") } else { text("▼") })
                        .style(crate::ui::button::secondary)
                        .on_press(Message::ToggleRow(i)),
                ]
                .spacing(6)
                .padding(4);

                if row.expanded {
                    let mut details: Vec<Element<'_, Message>> =
                        vec![text(format!("Input: {}", row.input_summary)).size(12).into()];
                    if !row.full_input.is_empty() {
                        details
                            .push(text(format!("Full input: {}", row.full_input)).size(12).into());
                    }
                    details.push(text(format!("Output: {}", row.full_output)).size(12).into());
                    if !row.error.is_empty() {
                        details.push(text(format!("Error: {}", row.error)).size(12).into());
                    }
                    if !row.policy_rule.is_empty() {
                        details.push(
                            text(format!("Matched policy rule: {}", row.policy_rule))
                                .size(11)
                                .into(),
                        );
                    }

                    column![header, column(details).spacing(2).padding(8)].spacing(2).into()
                } else {
                    header.into()
                }
            })
            .collect();

        column![
            container(filter).padding(4).width(Length::Fill),
            scrollable(Column::with_children(rows).spacing(2)),
        ]
        .into()
    }
}

/// Get a formatted timestamp string.
fn chrono_or_time() -> String {
    // Use `time` crate which is the project standard
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second(),)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_then_completed_produces_single_row() {
        let mut state = State::new();

        state.add_or_update(&ToolLogUpdate::Started {
            tool_name: "test_tool".to_string(),
            input_summary: "...".to_string(),
            full_input: "abc123".to_string(),
        });

        assert_eq!(state.rows.len(), 1);
        assert_eq!(state.rows[0].verdict, ToolVerdict::Running);

        state.add_or_update(&ToolLogUpdate::Completed {
            tool_name: "test_tool".to_string(),
            duration_ms: 42,
            success: true,
        });

        assert_eq!(state.rows.len(), 1);
        assert_eq!(state.rows[0].verdict, ToolVerdict::Allowed);
        assert_eq!(state.rows[0].duration_ms, 42);
    }

    #[test]
    fn policy_matched_update_sets_policy_rule() {
        let mut state = State::new();

        // First create a row for a tool
        state.add_or_update(&ToolLogUpdate::Started {
            tool_name: "test_tool".to_string(),
            input_summary: "...".to_string(),
            full_input: String::new(),
        });

        // Feed a policy-matched update
        state.add_or_update(&ToolLogUpdate::PolicyMatched {
            tool_name: "test_tool".to_string(),
            rule: Some("allow read-only".to_string()),
        });

        assert_eq!(state.rows[0].policy_rule, "allow read-only");
    }

    #[test]
    fn started_update_preserves_full_input() {
        let mut state = State::new();

        state.add_or_update(&ToolLogUpdate::Started {
            tool_name: "test_tool".to_string(),
            input_summary: "summary text".to_string(),
            full_input: "the quick brown fox".to_string(),
        });

        assert_eq!(state.rows.len(), 1);
        assert_eq!(state.rows[0].full_input, "the quick brown fox");
        assert_eq!(state.rows[0].input_summary, "summary text");
    }
}
