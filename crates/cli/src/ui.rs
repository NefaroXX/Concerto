use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use ratatui::text::Span;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;

use crate::app::{App, Screen, SettingsField};
use concerto_core::intent::RunStage;

pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Chat => draw_chat_screen(frame, app),
        Screen::Settings => draw_settings_screen(frame, app),
        Screen::Sessions => draw_sessions_screen(frame, app),
        Screen::ToolLog => draw_tool_log_screen(frame, app),
        Screen::AgentAssignments => draw_agent_assignments_screen(frame, app),
    }
    if let Some(prompt) = app.approval_prompt() {
        draw_approval_modal(frame, frame.area(), &prompt);
    } else if let Some(intent) = app.intent_prompt() {
        draw_intent_modal(frame, frame.area(), &intent);
    } else if let Some(plan) = app.plan_prompt() {
        draw_plan_modal(frame, frame.area(), &plan, app.plan_scroll);
    }
}

// ---------------------------------------------------------------------------
// Chat screen
// ---------------------------------------------------------------------------

fn draw_chat_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    draw_chat(frame, chunks[0], app);
    draw_input(frame, chunks[1], app);
    draw_status_bar(frame, app);
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title("Chat");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = app.messages.to_vec();
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((app.scroll, 0));
    frame.render_widget(paragraph, inner);
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let style = if app.input_mode {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default().borders(Borders::ALL).title("Input").border_style(style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.project_picker_mode {
        format!("Project path: {}", app.input)
    } else if app.input_mode {
        app.input.clone()
    } else {
        String::from(
            "[Esc] edit | [s] settings | [l] sessions | [t] log | [n] new | [p] project | [q] quit",
        )
    };
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);

    if app.input_mode || app.project_picker_mode {
        let text_len = if app.project_picker_mode {
            "Project path: ".len() + app.input.len()
        } else {
            app.input.len()
        };
        frame.set_cursor_position((inner.x + text_len as u16, inner.y));
    }
}

// ---------------------------------------------------------------------------
// Sessions screen
// ---------------------------------------------------------------------------

fn draw_sessions_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let block =
        Block::default().borders(Borders::ALL).title("Sessions  (Esc to return, Enter to resume)");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.sessions_list.is_empty() {
        let text = Paragraph::new("No sessions found for this project.").wrap(Wrap { trim: false });
        frame.render_widget(text, inner);
        return;
    }

    let items: Vec<ListItem> = app
        .sessions_list
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let selected = i == app.sessions_index;
            let style = if selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Build the format descriptor once. Both formats are well-known
            // time format strings; handle parse failure gracefully so we
            // never panic on a poisoned time crate state.
            let format_desc =
                time::format_description::parse("[year]-[month]-[day] [hour]:[minute]")
                    .unwrap_or_else(|_| {
                        // Fallback to a simple time-only format (still handled
                        // without unwrap).
                        time::format_description::parse("[hour]:[minute]").unwrap_or_default()
                    });
            let date = s.created_at.format(&format_desc).unwrap_or_else(|_| "?".to_string());
            let prefix = if selected { "> " } else { "  " };
            let content = format!(
                "{}{} | {} | {} msg | ${:.4}",
                prefix, date, s.model, s.message_count, s.total_cost_usd
            );
            ListItem::new(Line::from(Span::styled(content, style)))
        })
        .collect();

    let list = List::new(items);
    frame.render_widget(list, inner);
}

// ---------------------------------------------------------------------------
// Tool log screen
// ---------------------------------------------------------------------------

fn draw_tool_log_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Tool Log  ({} entries)  [Esc/t] close  [c] clear", app.tool_log.len()));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.tool_log.is_empty() {
        let text = Paragraph::new("No tool executions yet.").wrap(Wrap { trim: false });
        frame.render_widget(text, inner);
        return;
    }

    let items: Vec<ListItem> = app
        .tool_log
        .iter()
        .rev()
        .map(|entry| {
            let (icon, style) = match entry.status {
                crate::app::ToolStatus::Running => (" ▶", Style::default().fg(Color::Cyan)),
                crate::app::ToolStatus::Success => (" ✓", Style::default().fg(Color::Green)),
                crate::app::ToolStatus::Failure => (" ✗", Style::default().fg(Color::Red)),
                crate::app::ToolStatus::Timeout { .. } => {
                    (" ⏱", Style::default().fg(Color::Yellow))
                }
            };
            let detail = entry.detail.as_deref().unwrap_or("");
            let duration = match entry.duration_ms {
                Some(ms) => format!(" {ms}ms"),
                None => String::new(),
            };
            let line = format!("{icon} {}{}{}", entry.tool_name, duration, detail);
            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();

    let list = List::new(items);
    frame.render_widget(list, inner);
}

// ---------------------------------------------------------------------------
// Agent assignments screen
// ---------------------------------------------------------------------------

fn draw_agent_assignments_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let block = Block::default().borders(Borders::ALL).title("Agent Assignments  (Esc to return)");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.agent_assignments.is_empty() {
        let text = Paragraph::new("No agent assignments configured.").wrap(Wrap { trim: false });
        frame.render_widget(text, inner);
        return;
    }

    let items: Vec<ListItem> = app
        .agent_assignments
        .iter()
        .enumerate()
        .map(|(i, assignment)| {
            let selected = i == app.agent_assignment_index;
            let (indicator, style) = if selected {
                ("> ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::default())
            };
            let model = assignment.model_override.as_deref().unwrap_or("(default)");
            let line = format!("{}{}: {}", indicator, assignment.agent_role, model);
            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();

    let list = List::new(items);
    frame.render_widget(list, inner);

    // Help text at bottom.
    let help_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(3),
        width: inner.width,
        height: 3,
    };
    let help = Paragraph::new(vec![
        Line::from("Up/Down or j/k: navigate"),
        Line::from("Enter/Right: next model  Left: previous model"),
        Line::from("Esc or q: return to Settings"),
    ]);
    frame.render_widget(help, help_area);
}

/// Single-line status bar at the very bottom showing key info.
fn draw_status_bar(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // Status bar is the last row.
    let status_area =
        Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 };

    let status = status_line(app);
    let style = Style::default().fg(Color::DarkGray);
    let paragraph = Paragraph::new(status).style(style);
    frame.render_widget(paragraph, status_area);
}

/// Status-bar text for `app` (pure: extracted for tests).
fn status_line(app: &App) -> String {
    let provider = app.provider_label();
    let model = app.model_label();
    let mode = if app.multi_agent { "multi" } else { "single" };
    let fast = if app.fast { " fast" } else { "" };
    let run = if app.running { " | RUNNING (Ctrl+C cancel)" } else { "" };
    let memory = if app.memory_chunks > 0 {
        format!(" | {} chunks", app.memory_chunks)
    } else {
        String::new()
    };
    // The stage chip is a run-in-flight indicator: it only renders while the
    // run is active, mirroring the desktop chip's `run_status == Running`
    // guard. The run boundaries clear `run_stage`, but the guard is cheap
    // insurance against a stale value leaking onto the line.
    let stage = match (app.running, app.run_stage) {
        (true, Some(stage)) => format!(" | stage: {}", run_stage_label(stage)),
        _ => String::new(),
    };

    format!(
        " {} | {} | mode={}{}{}{}{} | {} ",
        provider,
        model,
        mode,
        fast,
        memory,
        run,
        stage,
        app.project_dir.display()
    )
}

/// User-facing label for an intent-router [`RunStage`] (ADR-55 Phase 2a),
/// mirroring the desktop status-bar chip wording.
fn run_stage_label(stage: RunStage) -> &'static str {
    match stage {
        RunStage::Understand => "Responding",
        RunStage::Inspect => "Inspecting",
        RunStage::Plan => "Planning",
        RunStage::Execute => "Editing",
        RunStage::Verify => "Testing",
        RunStage::Complete => "Complete",
        // `RunStage` is non-exhaustive (concerto-core); unknown future stages
        // get a neutral label rather than a blank status bar.
        _ => "Working",
    }
}

// ---------------------------------------------------------------------------
// Settings screen
// ---------------------------------------------------------------------------

fn draw_settings_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let block = Block::default().borders(Borders::ALL).title("Settings  (Esc to return)");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split into fields list + help.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(inner);

    draw_settings_list(frame, chunks[0], app);
    draw_settings_help(frame, chunks[1]);
}

fn draw_settings_list(frame: &mut Frame, area: Rect, app: &App) {
    let fields = SettingsField::ALL;
    let mut lines: Vec<Line> = Vec::new();

    for (i, &field) in fields.iter().enumerate() {
        let selected = i == app.settings_index;
        let value = field.display_value(app);
        let label = field.label();

        let (style, indicator) = if selected {
            (Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD), "> ")
        } else {
            (Style::default(), "  ")
        };

        let line = Line::from(format!("{}{}: {}", indicator, label, value)).style(style);
        lines.push(line);
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_settings_help(frame: &mut Frame, area: Rect) {
    let help_lines = vec![
        Line::from("Up/Down or j/k: navigate"),
        Line::from("Enter/Right: next value  Left: previous value"),
        Line::from("Esc or q: return to Chat"),
    ];
    let block = Block::default().borders(Borders::TOP).title("Help");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let paragraph = Paragraph::new(help_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn draw_approval_modal(frame: &mut Frame, area: Rect, prompt: &crate::approval::ApprovalPrompt) {
    let modal_width = (area.width.saturating_div(2)).max(40).min(area.width.saturating_sub(4));
    let modal_height = 5;
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + area.height.saturating_sub(modal_height) - 2;
    let modal_area = Rect { x: modal_x, y: modal_y, width: modal_width, height: modal_height };

    // Dim the area behind the modal with a solid background.
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, modal_area);

    // Modal border with title.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if prompt.acknowledgement {
            " Warning ".to_string()
        } else {
            format!(" Approve {}? ", prompt.tool_name)
        })
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let input = Paragraph::new(format!(
        "{}\n[y] allow once  [a] allow for session  [n] deny",
        prompt.detail
    ))
    .style(Style::default().fg(Color::White))
    .wrap(Wrap { trim: false });
    frame.render_widget(input, inner);
}

/// Render the intent-confirmation modal (ADR-55 §1): the question plus a
/// numbered list of the selectable outcomes. Mirrors `draw_approval_modal`:
/// a cyan-bordered modal with a wrapped body. The height grows with the option
/// list so all six choices are visible.
fn draw_intent_modal(frame: &mut Frame, area: Rect, prompt: &crate::approval::IntentPrompt) {
    // `Debug` names are the Phase-0 outcome labels (Answer, Diagnose, ...).
    let options_text: Vec<String> = prompt
        .options
        .iter()
        .enumerate()
        .map(|(index, outcome)| format!("[{}] {:?}", index + 1, outcome))
        .collect();
    let body = if options_text.is_empty() {
        prompt.question.clone()
    } else {
        format!("{}\n{}", prompt.question, options_text.join("\n"))
    };
    let hint = "Enter/[1-6] confirm  q/Esc reject";

    let modal_width = (area.width.saturating_div(2)).max(44).min(area.width.saturating_sub(4));
    let modal_height =
        (prompt.options.len() as u16 + 4).max(6).min(area.height.max(6).saturating_sub(2));
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + area.height.saturating_sub(modal_height) - 2;
    let modal_area = Rect { x: modal_x, y: modal_y, width: modal_width, height: modal_height };

    // Dim the area behind the modal with a solid background.
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, modal_area);

    // Modal border with title.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm intent ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let input = Paragraph::new(format!("{}\n\n{}", body, hint))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, inner);
}

/// Render the plan-approval modal (ADR-55 Phase 1d): the stored-plan question,
/// a "Plan (plan_id)" label, and the plan body — up to 16 KiB — inside a
/// scrollable viewport paged with `j/k` / arrow keys, plus Apply/Re-plan hints
/// and Esc to dismiss. Mirrors `draw_intent_modal`'s cyan-bordered style.
///
/// The hint row signals the stakes: Apply is mutation-capable, Dismiss keeps
/// the run read-only — and reports the scroll position `(scrolled X/Y lines)`
/// whenever the plan body overflows the viewport.
fn draw_plan_modal(
    frame: &mut Frame,
    area: Rect,
    prompt: &crate::approval::PlanPrompt,
    scroll: u16,
) {
    let modal_width = (area.width.saturating_div(2)).max(48).min(area.width.saturating_sub(4));
    let text_width = modal_width.saturating_sub(4);
    // Header: question (may wrap) + plan-id label. The body viewport is sized
    // responsively — at least 5 body lines, grown to use the available
    // terminal height (area minus header, plan-id footer and hint rows) when
    // there is room — so long plans scroll instead of collapsing into a fixed
    // 14-line box.
    let header_lines = wrapped_line_count(&prompt.question, text_width) + 1;
    let body_lines = wrapped_line_count(&prompt.plan_text, text_width);
    // The full-plan-id footer and the hint row reserve two lines below the
    // scrolled body.
    let reserved = header_lines + 2;
    let available = area.height.max(6).saturating_sub(2);
    let viewport = body_lines.clamp(5, available.saturating_sub(reserved).max(5));
    let modal_height = (viewport + reserved).min(available);
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + area.height.saturating_sub(modal_height) - 2;
    let modal_area = Rect { x: modal_x, y: modal_y, width: modal_width, height: modal_height };

    // Dim the area behind the modal with a solid background.
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, modal_area);

    // Modal border with title.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Plan approval ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    // Header (question + id label), scrolled body, full-plan-id footer, hint.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_lines),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(vec![
        Line::from(prompt.question.clone()),
        Line::from(format!("Plan ({})", plan_id_label(&prompt.plan_id))),
    ])
    .style(Style::default().fg(Color::White))
    .wrap(Wrap { trim: false });
    frame.render_widget(header, chunks[0]);

    // Clamp the scroll so the modal never shows past the end of the body; a
    // stale offset (e.g. after a plan was replaced) cannot hide the start.
    let viewport_rows = chunks[1].height;
    let clamped = scroll.min(body_lines.saturating_sub(viewport_rows));
    let body = Paragraph::new(prompt.plan_text.clone())
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false })
        .scroll((clamped, 0));
    frame.render_widget(body, chunks[1]);

    // Dimmed footer repeats the FULL plan id — the header label is truncated
    // to fit, but the audit identity stays copyable and unambiguous.
    let footer = Paragraph::new(prompt.plan_id.clone()).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[2]);

    let hint = Paragraph::new(plan_hint(clamped, body_lines, viewport_rows, chunks[3].width))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[3]);
}

/// Build the plan-modal hint row: the decision stakes (Apply is mutation-
/// capable, Dismiss is read-only), the page keys, and — when the body
/// overflows the viewport — the scroll position `(scrolled X/Y lines)`.
/// Truncated to `width` columns so the stakes stay legible on narrow
/// terminals; the scroll indicator sits before the low-priority `j/k scroll`
/// tail so it survives truncation.
fn plan_hint(scrolled: u16, body_lines: u16, viewport_rows: u16, width: u16) -> String {
    let stakes = "[a]/Enter Apply (mutation)  [r] Replan  q/n/Esc Dismiss (read-only)";
    let mut hint = stakes.to_string();
    if body_lines > viewport_rows {
        hint = format!("{hint}  (scrolled {scrolled}/{body_lines} lines)");
    }
    hint = format!("{hint}  j/k scroll");
    truncate_to(hint, width)
}

/// Truncate `text` to at most `width` columns, adding an ellipsis when cut.
/// A zero width (degenerate layout) returns the text untouched.
fn truncate_to(text: String, width: u16) -> String {
    if width == 0 {
        return text;
    }
    let width = width as usize;
    if text.chars().count() <= width {
        return text;
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Compact display label for a plan id: the id is a ULID that would overflow
/// the modal header, so only its leading run is shown.
fn plan_id_label(plan_id: &str) -> String {
    const MAX_LEN: usize = 12;
    if plan_id.chars().count() <= MAX_LEN {
        return plan_id.to_owned();
    }
    let truncated: String = plan_id.chars().take(MAX_LEN).collect();
    format!("{truncated}…")
}

/// Rough count of terminal rows `text` occupies when wrapped at `width`
/// columns. Characters-per-line is a conservative proxy for glyph width; used
/// only to size and clamp the plan modal's scroll viewport.
fn wrapped_line_count(text: &str, width: u16) -> u16 {
    if width == 0 {
        return text.lines().count().max(1) as u16;
    }
    text.lines()
        .map(|line| {
            let cols = line.chars().count() as u16;
            if cols == 0 {
                1
            } else {
                cols.div_ceil(width)
            }
        })
        .sum::<u16>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // ------------------------------------------------------------------
    // Plan modal helpers (ADR-55 Phase 1d)
    // ------------------------------------------------------------------

    #[test]
    fn wrapped_line_count_bounds_text_rows() {
        assert_eq!(wrapped_line_count("", 40), 1);
        assert_eq!(wrapped_line_count("single", 40), 1);
        // 100 chars / 40 cols → 3 wrapped rows.
        assert_eq!(wrapped_line_count(&"x".repeat(100), 40), 3);
        // Two explicit lines wrap independently and sum.
        assert_eq!(wrapped_line_count(&format!("{}\n{}", "x".repeat(60), "y".repeat(10)), 40), 3);
        // Zero width never divides by zero.
        assert_eq!(wrapped_line_count("a\nb\nc", 0), 3);
    }

    #[test]
    fn plan_id_label_truncates_long_ids() {
        assert_eq!(plan_id_label("short"), "short");
        let long = "01JTESTPLAN0000000000001A";
        let label = plan_id_label(long);
        assert!(label.starts_with("01JTESTPLA"), "leading run of the id is kept");
        assert!(label.ends_with('…'), "long ids are ellipsized");
        assert!(label.chars().count() <= 13, "label stays compact");
    }

    #[test]
    fn plan_hint_lists_stakes_without_scroll_indicator_when_body_fits() {
        let hint = plan_hint(0, 3, 5, 200);
        assert!(hint.contains("Apply (mutation)"), "Apply stakes the mutation capability");
        assert!(hint.contains("Dismiss (read-only)"), "Dismiss stakes the read-only result");
        assert!(!hint.contains("scrolled"), "no scroll indicator when the body fits");
        assert!(hint.contains("j/k scroll"));
    }

    #[test]
    fn plan_hint_appends_scroll_indicator_when_body_overflows_viewport() {
        let hint = plan_hint(2, 20, 10, 200);
        assert!(hint.contains("(scrolled 2/20 lines)"), "indicator reports position over total");
        assert!(hint.contains("j/k scroll"));
    }

    #[test]
    fn plan_hint_truncates_to_row_width() {
        let hint = plan_hint(4, 20, 10, 20);
        assert!(hint.chars().count() <= 20, "hint stays within the modal width");
        assert!(hint.ends_with('…'), "a cut hint ends with an ellipsis");
        assert!(hint.starts_with("[a]/Enter"), "the stakes at the head stay legible");
    }

    #[test]
    fn plan_hint_zero_width_is_returned_untouched() {
        let hint = plan_hint(4, 20, 10, 0);
        assert_eq!(
            hint,
            "[a]/Enter Apply (mutation)  [r] Replan  q/n/Esc Dismiss (read-only)  \
             (scrolled 4/20 lines)  j/k scroll"
        );
    }

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> crossterm::event::Event {
        crossterm::event::Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        })
    }

    #[test]
    fn settings_screen_switch_from_chat() {
        let mut app = App::new();
        assert_eq!(app.screen, Screen::Chat);
        // Enter command mode first.
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert!(!app.input_mode);
        // Press 's' to open settings.
        app.handle_key(key_event(KeyCode::Char('s'), KeyModifiers::empty()));
        assert_eq!(app.screen, Screen::Settings);
    }

    #[test]
    fn settings_screen_escape_returns_to_chat() {
        let mut app = App::new();
        app.screen = Screen::Settings;
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.screen, Screen::Chat);
    }

    #[test]
    fn settings_navigation() {
        let mut app = App::new();
        app.screen = Screen::Settings;
        assert_eq!(app.settings_index, 0);
        app.handle_key(key_event(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.settings_index, 1);
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.settings_index, 0);
        // Can't go below 0.
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.settings_index, 0);
    }

    #[test]
    fn settings_toggle_multi_agent() {
        let mut app = App::new();
        app.screen = Screen::Settings;
        assert!(!app.multi_agent);
        // Navigate to MultiAgent (index 3; InteractionMode was removed in
        // ADR-55 Phase 1e).
        app.settings_index = 3;
        app.handle_key(key_event(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.multi_agent);
        app.handle_key(key_event(KeyCode::Enter, KeyModifiers::empty()));
        assert!(!app.multi_agent);
    }

    #[test]
    fn settings_toggle_fast_mode() {
        let mut app = App::new();
        app.screen = Screen::Settings;
        assert!(!app.fast);
        app.settings_index = 5;
        app.handle_key(key_event(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.fast);
    }

    #[test]
    fn chat_q_in_command_mode_quits() {
        use crate::app::Action;
        let mut app = App::new();
        app.input_mode = false;
        let action = app.handle_key(key_event(KeyCode::Char('q'), KeyModifiers::empty()));
        assert!(matches!(action, Action::Quit));
    }

    // ------------------------------------------------------------------
    // Run-stage status suffix (ADR-55 Phase 2a)
    // ------------------------------------------------------------------

    #[test]
    fn run_stage_labels_map_to_chip_text() {
        assert_eq!(run_stage_label(RunStage::Understand), "Responding");
        assert_eq!(run_stage_label(RunStage::Inspect), "Inspecting");
        assert_eq!(run_stage_label(RunStage::Plan), "Planning");
        assert_eq!(run_stage_label(RunStage::Execute), "Editing");
        assert_eq!(run_stage_label(RunStage::Verify), "Testing");
        assert_eq!(run_stage_label(RunStage::Complete), "Complete");
    }

    #[test]
    fn status_line_shows_stage_only_while_running() {
        let mut app = App::new();
        // No stage known → no suffix.
        assert!(!status_line(&app).contains("stage:"));

        // A stage with the run idle must not leak into the line.
        app.run_stage = Some(RunStage::Execute);
        assert!(
            !status_line(&app).contains("stage:"),
            "idle run must not show a stage: {}",
            status_line(&app)
        );

        // Running with a stage → the suffix appears with the ADR label.
        app.running = true;
        assert!(status_line(&app).contains(" | stage: Editing"), "{}", status_line(&app));
    }

    #[test]
    fn status_line_drops_stage_when_cleared() {
        let mut app = App::new();
        app.running = true;
        app.run_stage = Some(RunStage::Plan);
        assert!(status_line(&app).contains("stage: Planning"));
        // Completion boundary clears the stage (App::handle_key path is
        // separate; the field-level contract is what the status line reads).
        app.run_stage = None;
        assert!(!status_line(&app).contains("stage:"));
    }
}
