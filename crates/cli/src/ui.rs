use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use ratatui::text::Span;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;

use crate::app::{App, ReviewStatus, Screen, SettingsField};
use crate::theme::CliTheme;
use concerto_api_types::diff::{DiffLine, DiffResult};
use concerto_core::intent::RunStage;

pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Chat => draw_chat_screen(frame, app),
        Screen::Settings => draw_settings_screen(frame, app),
        Screen::Sessions => draw_sessions_screen(frame, app),
        Screen::ToolLog => draw_tool_log_screen(frame, app),
        Screen::AgentAssignments => draw_agent_assignments_screen(frame, app),
        Screen::DiffReview => draw_diff_review_screen(frame, app),
    }
    if let Some(prompt) = app.approval_prompt() {
        draw_approval_modal(frame, frame.area(), &prompt, &app.cli_theme);
    } else if let Some(intent) = app.intent_prompt() {
        draw_intent_modal(frame, frame.area(), &intent, &app.cli_theme);
    } else if let Some(plan) = app.plan_prompt() {
        draw_plan_modal(frame, frame.area(), &plan, app.plan_scroll, &app.cli_theme);
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
        Style::default().fg(app.cli_theme.success)
    } else {
        Style::default().fg(app.cli_theme.muted)
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
            "[Esc] edit | [s] settings | [l] sessions | [t] log | [d] diff | [n] new | [p] project | [q] quit",
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
                Style::default().fg(app.cli_theme.warning).add_modifier(Modifier::BOLD)
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

    let styling = crate::app::styling_enabled();
    let theme = &app.cli_theme;
    let items: Vec<ListItem> = app
        .tool_log
        .iter()
        .rev()
        .map(|entry| ListItem::new(tool_log_line_with_theme(entry, styling, theme)))
        .collect();

    let list = List::new(items);
    frame.render_widget(list, inner);
}

/// One tool-log row: status icon + name + duration + detail + provenance rail.
///
/// The rail suffix is read-only from the stored [`crate::app::ToolStatus`]
/// (never the policy engine). Rail text is always appended — it is state, so
/// `reduced_motion` never gates it; only the colors follow `styling` (plain
/// symbols under `NO_COLOR`/off-TTY so nothing leaks into pipes).
pub(crate) fn tool_log_line_with_theme(
    entry: &crate::app::ToolLogEntry,
    styling: bool,
    theme: &CliTheme,
) -> Line<'static> {
    let (icon, style) = match entry.status {
        crate::app::ToolStatus::Running => (" ▶", Style::default().fg(theme.user)),
        crate::app::ToolStatus::Success => (" ✓", Style::default().fg(theme.success)),
        crate::app::ToolStatus::Failure => (" ✗", Style::default().fg(theme.danger)),
        crate::app::ToolStatus::Timeout { .. } => (" ⏱", Style::default().fg(theme.warning)),
    };
    let detail = entry.detail.as_deref().unwrap_or("");
    let duration = match entry.duration_ms {
        Some(ms) => format!(" {ms}ms"),
        None => String::new(),
    };
    let base = format!("{icon} {}{}{}", entry.tool_name, duration, detail);
    let rail = entry.status.rail_text();
    if !styling {
        return Line::from(Span::raw(format!("{base}{rail}")));
    }
    Line::from(vec![
        Span::styled(base, style),
        Span::styled(rail.to_string(), Style::default().fg(entry.status.rail_color(theme))),
    ])
}

// ---------------------------------------------------------------------------
// Staged unified-diff review screen (P02)
// ---------------------------------------------------------------------------

/// Review modal for the staged unified diffs (CLI/parity slice of the
/// Desktop staged review). Geometry follows `draw_approval_modal` — centered,
/// half the terminal wide (min 40) — but grown to the full frame height so a
/// real diff is visible without paging.
///
/// The body never wraps: every row is pre-truncated to the viewport width
/// with an ellipsis, so a long source line is cut rather than re-wrapped
/// into a misleading second row, and the `@@`/`+`/`-` prefix at column 0
/// stays the non-color cue for every diff row.
fn draw_diff_review_screen(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let state = &app.diff_review;
    let theme = &app.cli_theme;

    // Same backdrop idiom as the other modals, but full-frame: this screen
    // replaces the chat instead of floating over it.
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, area);

    let modal_width = (area.width.saturating_div(2)).max(40).min(area.width.saturating_sub(4));
    let modal_height = area.height.saturating_sub(2).max(3).min(area.height);
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect { x: modal_x, y: modal_y, width: modal_width, height: modal_height };

    let title = if state.results.is_empty() {
        " Diff review ".to_string()
    } else {
        format!(" Diff review — {}/{} ", state.index + 1, state.results.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Selected file: the title carries the position, this row the path
    // (truncated like every other single-row field).
    let header = state
        .results
        .get(state.index)
        .map(|result| truncate_to(result.path.to_string(), chunks[0].width))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(header)
            .style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let body_lines = match state.results.get(state.index) {
        Some(result) => diff_review_lines(result, chunks[1].width, theme),
        None => vec![Line::from(Span::styled(
            truncate_to("No staged changes.".to_string(), chunks[1].width),
            Style::default().fg(theme.muted),
        ))],
    };
    // Clamp the scroll so a stale offset (e.g. after the list shrank)
    // cannot show past the end of the body — same rule as the plan modal.
    let rows = body_lines.len();
    let viewport = usize::from(chunks[1].height);
    let clamped = usize::from(state.scroll).min(rows.saturating_sub(viewport));
    let scroll = u16::try_from(clamped).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(body_lines).scroll((scroll, 0)), chunks[1]);

    let (status, status_style) = match &state.status {
        None => (String::new(), Style::default().fg(theme.muted)),
        Some(ReviewStatus::Decided { path, accept }) => (
            truncate_to(
                format!("{} {path}", if *accept { "Accepted" } else { "Rejected" }),
                chunks[2].width,
            ),
            Style::default().fg(theme.success),
        ),
        Some(ReviewStatus::Blocked(message)) => (
            truncate_to(format!("Blocked: {message}"), chunks[2].width),
            Style::default().fg(theme.danger),
        ),
    };
    frame.render_widget(Paragraph::new(status).style(status_style), chunks[2]);

    let hint = truncate_to(
        "[a] accept  [r] reject  Esc close  h/l: file  j/k: scroll".to_string(),
        chunks[3].width,
    );
    frame.render_widget(Paragraph::new(hint).style(Style::default().fg(theme.muted)), chunks[3]);
}

/// Unified-diff rows for one file, pre-truncated to `width` columns.
///
/// The `@@`/`+`/`-`/space prefix and the colors (green additions, red
/// deletions, dim hunk headers) are derived purely from the shared
/// [`DiffResult`] — no diff is computed here. A file staged as a pure
/// create/delete (no text hunks) gets an explanatory row instead of an
/// empty viewport.
fn diff_review_lines(result: &DiffResult, width: u16, theme: &CliTheme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for hunk in &result.hunks {
        lines.push(Line::from(Span::styled(
            truncate_to(
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ),
                width,
            ),
            Style::default().fg(theme.muted),
        )));
        for line in &hunk.lines {
            let (prefix, content, style) = match line {
                DiffLine::Addition { content, .. } => {
                    ("+", content.as_str(), Style::default().fg(theme.success))
                }
                DiffLine::Deletion { content, .. } => {
                    ("-", content.as_str(), Style::default().fg(theme.danger))
                }
                DiffLine::Context { content, .. } => (" ", content.as_str(), Style::default()),
                // `DiffLine` is `#[non_exhaustive]`: a future variant must
                // never blank the review, so it renders as context.
                _ => (" ", "", Style::default()),
            };
            lines.push(Line::from(Span::styled(
                truncate_to(format!("{prefix}{content}"), width),
                style,
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to("No textual change (empty file operation).".to_string(), width),
            Style::default().fg(theme.muted),
        )));
    }
    lines
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
                ("> ", Style::default().fg(app.cli_theme.warning).add_modifier(Modifier::BOLD))
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
    let style = Style::default().fg(app.cli_theme.muted);
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
    // Thinking-accordion state (V2): only shown once the movement logged a
    // thought, so idle status lines stay unchanged.
    let thinking = if app.thought_log.is_empty() {
        String::new()
    } else if app.thinking_expanded {
        " | thinking: expanded".to_string()
    } else {
        " | thinking: collapsed".to_string()
    };

    format!(
        " {} | {} | mode={}{}{}{}{}{} | {} ",
        provider,
        model,
        mode,
        fast,
        memory,
        run,
        stage,
        thinking,
        app.project_dir.display()
    )
}

/// User-facing label for an intent-router [`RunStage`] (ADR-55 §9),
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
    let block = Block::default().borders(Borders::ALL).title("Global settings  (Esc to return)");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split into fields list + help.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(if inner.height < 10 { 2 } else { 6 }),
        ])
        .split(inner);

    draw_settings_list(frame, chunks[0], app);
    draw_settings_help(frame, chunks[1]);
}

fn draw_settings_list(frame: &mut Frame, area: Rect, app: &App) {
    let fields = SettingsField::ALL;
    let mut lines: Vec<Line> = Vec::new();

    // Viewport contract: exactly `visible` rows are emitted, so entries and
    // screen rows are interchangeable units. That is what makes the scroll
    // clamp below exact — `first = selected - (visible - 1)` always lands the
    // selected entry on the last visible row. Word-wrapping broke the 1:1
    // mapping: a wrapped entry took two rows, the `take(visible)` window still
    // counted it once, and the selection was pushed past the bottom edge
    // (regression: `settings_selection_stays_visible_...` at 24x8). Long
    // entries are therefore truncated to the viewport width with an ellipsis
    // (same convention as `plan_hint`) instead of wrapped, keeping each entry
    // on a single row at every terminal size.
    let visible = usize::from(area.height).max(1);
    let first = app.settings_index.saturating_sub(visible.saturating_sub(1));
    for (i, &field) in fields.iter().enumerate().skip(first).take(visible) {
        let selected = i == app.settings_index;
        let value = field.display_value(app);
        let label = field.label();

        let (style, indicator) = if selected {
            (Style::default().fg(app.cli_theme.warning).add_modifier(Modifier::BOLD), "> ")
        } else {
            (Style::default(), "  ")
        };

        let text = truncate_to(format!("{}{}: {}", indicator, label, value), area.width);
        let line = Line::from(text).style(style);
        lines.push(line);
    }

    // No `.wrap()`: the rows were already fitted to `area.width` above, and
    // un-wrapped lines are clipped rather than re-flowed, so the row count the
    // scroll clamp relies on is preserved at render time.
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

fn draw_settings_help(frame: &mut Frame, area: Rect) {
    let help_lines = if area.height < 5 {
        vec![Line::from("j/k select · Enter change · Esc back")]
    } else {
        vec![
            Line::from("Up/Down or j/k: navigate"),
            Line::from("Enter/Right: next value  Left: previous value"),
            Line::from("Esc or q: return to Chat"),
            Line::from("Structured settings: concerto config help"),
            Line::from("Saved changes apply next run; project/env/flags can override."),
        ]
    };
    let block = Block::default().borders(Borders::TOP).title("Help");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let paragraph = Paragraph::new(help_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn draw_approval_modal(
    frame: &mut Frame,
    area: Rect,
    prompt: &crate::approval::ApprovalPrompt,
    theme: &CliTheme,
) {
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
        .border_style(Style::default().fg(theme.border));
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

/// Render the intent-confirmation modal (ADR-55 §2): the question plus a
/// numbered list of the selectable outcomes. Mirrors `draw_approval_modal`:
/// a cyan-bordered modal with a wrapped body. The height grows with the option
/// list so all six choices are visible.
fn draw_intent_modal(
    frame: &mut Frame,
    area: Rect,
    prompt: &crate::approval::IntentPrompt,
    theme: &CliTheme,
) {
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
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let input = Paragraph::new(format!("{}\n\n{}", body, hint))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, inner);
}

/// Render the plan-approval modal (ADR-55 §4): the stored-plan question,
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
    theme: &CliTheme,
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
        .border_style(Style::default().fg(theme.border));
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
    let footer = Paragraph::new(prompt.plan_id.clone()).style(Style::default().fg(theme.muted));
    frame.render_widget(footer, chunks[2]);

    let hint = Paragraph::new(plan_hint(clamped, body_lines, viewport_rows, chunks[3].width))
        .style(Style::default().fg(theme.muted));
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

// ---------------------------------------------------------------------------
// Chat role gutters + markdown-lite (Score signature, CLI slice)
// ---------------------------------------------------------------------------

/// Chat-line role: one stable gutter symbol + ANSI color each, mirroring the
/// desktop `agent_roles` palette-map contract (one stable color per bucket).
/// Under `NO_COLOR`/off-TTY the symbols render plain (no ANSI) so nothing
/// leaks into pipes — the same contract as `thinking_header`/`reveal_line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRole {
    User,
    Assistant,
    Policy,
}

impl ChatRole {
    pub(crate) fn gutter(self) -> &'static str {
        match self {
            ChatRole::User => "› ",
            ChatRole::Assistant => "♪ ",
            ChatRole::Policy => "‖ ",
        }
    }

    /// Historical `Midnight` gutter color (pre-theme contract).
    #[allow(dead_code)]
    pub(crate) fn color(self) -> Color {
        self.color_for(&CliTheme::by_name("Midnight"))
    }

    /// Theme-resolved gutter color from the CLI theme bridge.
    pub(crate) fn color_for(self, theme: &CliTheme) -> Color {
        match self {
            ChatRole::User => theme.user,
            ChatRole::Assistant => theme.assistant,
            ChatRole::Policy => theme.policy,
        }
    }
}

/// A chat line: role gutter + markdown-lite body. Plain (symbols only) when
/// `styling` is false (`NO_COLOR`/off-TTY, caller-gated via
/// `styling_enabled`); gutter color + markdown spans otherwise.
#[allow(dead_code)]
pub(crate) fn chat_line(role: ChatRole, body: &str, styling: bool) -> Line<'static> {
    chat_line_with_theme(role, body, styling, &CliTheme::by_name("Midnight"))
}

/// Theme-resolved chat line; `chat_line` keeps the `Midnight` contract.
pub(crate) fn chat_line_with_theme(
    role: ChatRole,
    body: &str,
    styling: bool,
    theme: &CliTheme,
) -> Line<'static> {
    if !styling {
        return Line::from(vec![Span::raw(role.gutter().to_string()), Span::raw(body.to_string())]);
    }
    let mut spans = vec![Span::styled(
        role.gutter().to_string(),
        Style::default().fg(role.color_for(theme)).add_modifier(Modifier::BOLD),
    )];
    spans.extend(markdown_spans_with_theme(body, true, theme));
    Line::from(spans)
}

/// Markdown-lite spans for assistant/user bodies: `**bold**`, `` `code` ``
/// (reversed + dim), `#` headings (bold whole line), fenced blocks (indented
/// `  │ ` gutter in `DarkGray`, dimmed body). Unclosed markers and fences
/// render literally so a mid-reveal prefix never panics or drops text.
#[allow(dead_code)]
pub(crate) fn markdown_spans(text: &str, styling: bool) -> Vec<Span<'static>> {
    markdown_spans_with_theme(text, styling, &CliTheme::by_name("Midnight"))
}

/// Theme-resolved markdown-lite spans; `markdown_spans` keeps the `Midnight`
/// contract. Only the fenced-block gutter color follows the theme (the muted
/// chrome); bold/code/heading logic is untouched.
///
/// Streaming-reveal safety: the reveal renders ever-longer prefixes of one
/// message through this path, so a prefix can end mid-fence (opener seen,
/// closer not yet). An odd marker count means the last opener is unmatched:
/// it renders literally (raw, never inline-parsed) while the fenced body
/// after it keeps the dim style + `│` gutter — so no fence marker styling
/// leaks into a partial chunk, and closing the fence settles the block
/// without dropping text.
pub(crate) fn markdown_spans_with_theme(
    text: &str,
    styling: bool,
    theme: &CliTheme,
) -> Vec<Span<'static>> {
    if !styling {
        return vec![Span::raw(text.to_string())];
    }
    let mut out = Vec::new();
    let mut in_fence = false;
    let lines: Vec<&str> = text.split('\n').collect();
    let dangling_from =
        if lines.iter().filter(|line| line.trim_start().starts_with("```")).count() % 2 == 1 {
            lines.iter().rposition(|line| line.trim_start().starts_with("```"))
        } else {
            None
        };
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push(Span::raw("\n".to_string()));
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            if dangling_from == Some(index) {
                // Unmatched opener in a partial prefix: literal raw text (per
                // the unclosed-marker contract), never dimmed as a boundary —
                // but the fence still opens so the body below stays guttered.
                out.push(Span::raw((*line).to_string()));
            } else {
                out.push(Span::styled(
                    (*line).to_string(),
                    Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                ));
            }
            continue;
        }
        if in_fence {
            out.push(Span::styled("  │ ".to_string(), Style::default().fg(theme.muted)));
            out.push(Span::styled(
                (*line).to_string(),
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            ));
            continue;
        }
        out.extend(inline_spans(line, is_heading(line)));
    }
    out
}

/// A `#` heading: 1–6 `#` followed by a space (leading whitespace allowed).
fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes) && trimmed.chars().nth(hashes).is_some_and(|c| c == ' ')
}

/// Inline `**bold**` / `` `code` `` spans for one fence-free line. Looks for
/// a closing marker before consuming an opener, so unclosed markers stay
/// literal. `force_bold` (headings) bolds the whole line.
fn inline_spans(segment: &str, force_bold: bool) -> Vec<Span<'static>> {
    let chars: Vec<char> = segment.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut index = 0;
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        if force_bold {
            spans.push(Span::styled(text, Style::default().add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::raw(text));
        }
    };
    while index < chars.len() {
        if chars[index] == '*'
            && index + 1 < chars.len()
            && chars[index + 1] == '*'
            && closes(&chars, index + 2, "**")
        {
            flush(&mut buf, &mut spans);
            let inner: String = chars[index + 2..].iter().collect();
            let end = inner.find("**").expect("closing marker checked above");
            let end_chars = inner[..end].chars().count();
            let content: String = chars[index + 2..index + 2 + end_chars].iter().collect();
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if force_bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(content, style));
            index += 2 + end_chars + 2;
        } else if chars[index] == '`' && closes(&chars, index + 1, "`") {
            flush(&mut buf, &mut spans);
            let rest: String = chars[index + 1..].iter().collect();
            let end = rest.find('`').expect("closing backtick checked above");
            let end_chars = rest[..end].chars().count();
            let content: String = chars[index + 1..index + 1 + end_chars].iter().collect();
            let style =
                Style::default().add_modifier(Modifier::REVERSED).add_modifier(Modifier::DIM);
            spans.push(Span::styled(content, style));
            index += 1 + end_chars + 1;
        } else {
            buf.push(chars[index]);
            index += 1;
        }
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Whether `marker` closes later in `chars` from `from` (char index).
fn closes(chars: &[char], from: usize, marker: &str) -> bool {
    if from > chars.len() {
        return false;
    }
    let rest: String = chars[from..].iter().collect();
    rest.contains(marker)
}

#[cfg(test)]
mod tests {
    #[test]
    fn settings_selection_stays_visible_at_narrow_and_short_terminal_sizes() {
        use crate::app::{App, Screen, SettingsField};
        for (width, height) in [(24, 8), (40, 12), (80, 24), (120, 40)] {
            let mut app = App::new();
            app.screen = Screen::Settings;
            app.settings_index = SettingsField::ALL.len() - 1;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| super::draw(frame, &app)).unwrap();
            let text: String =
                terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert!(
                text.contains("> Check updates"),
                "selected setting is outside {width}x{height}: {text}"
            );
        }
    }

    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};

    // ------------------------------------------------------------------
    // Plan modal helpers (ADR-55 §4)
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
        // ADR-55 §7).
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
    // Run-stage status suffix (ADR-55 §9)
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

    // ------------------------------------------------------------------
    // Role gutters + markdown-lite
    // ------------------------------------------------------------------

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn chat_line_plain_is_symbols_only() {
        assert_eq!(line_text(&chat_line(ChatRole::User, "hi", false)), "› hi");
        assert_eq!(line_text(&chat_line(ChatRole::Assistant, "hi", false)), "♪ hi");
        assert_eq!(line_text(&chat_line(ChatRole::Policy, "hi", false)), "‖ hi");
        for line in
            [chat_line(ChatRole::User, "hi", false), chat_line(ChatRole::Assistant, "hi", false)]
        {
            assert!(line.spans.iter().all(|span| span.style.add_modifier.is_empty()));
        }
    }

    #[test]
    fn chat_line_styled_carries_role_colors() {
        let user = chat_line(ChatRole::User, "hi", true);
        assert_eq!(user.spans[0].content.as_ref(), "› ");
        assert_eq!(user.spans[0].style.fg, Some(Color::Cyan));
        let assistant = chat_line(ChatRole::Assistant, "hi", true);
        assert_eq!(assistant.spans[0].style.fg, Some(Color::Green));
        let policy = chat_line(ChatRole::Policy, "hi", true);
        assert_eq!(policy.spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn markdown_bold_and_code_spans() {
        let spans = markdown_spans("a **bold** and `code` end", true);
        let text: String = spans.iter().map(|span| span.content.to_string()).collect();
        assert_eq!(text, "a bold and code end");
        assert!(spans.iter().any(|span| span.content.as_ref() == "bold"
            && span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans.iter().any(|span| span.content.as_ref() == "code"
            && span.style.add_modifier.contains(Modifier::REVERSED)
            && span.style.add_modifier.contains(Modifier::DIM)));
    }

    #[test]
    fn markdown_unclosed_markers_stay_literal() {
        assert_eq!(
            line_text(&Line::from(markdown_spans("a **dangling and `tick", true))),
            "a **dangling and `tick"
        );
    }

    #[test]
    fn markdown_heading_is_bold_and_fence_gets_gutter() {
        let head = Line::from(markdown_spans("# Title", true));
        assert_eq!(line_text(&head), "# Title");
        assert!(head.spans.iter().all(|span| span.style.add_modifier.contains(Modifier::BOLD)));
        let fence = Line::from(markdown_spans("```\nlet x = 1;\n```", true));
        let text = line_text(&fence);
        assert!(text.contains("  │ "), "fenced body carries the │ gutter: {text:?}");
        assert!(text.contains("let x = 1;"));
    }

    #[test]
    fn themed_chat_line_plain_carries_no_ansi_and_nebula_differs() {
        use crate::theme::CliTheme;
        // NO_COLOR / off-TTY plain: symbols only, no colors or modifiers on
        // any theme — the theme never leaks ANSI into pipes.
        for name in ["Midnight", "Slate", "Chalk", "Nebula"] {
            let theme = CliTheme::by_name(name);
            let line = chat_line_with_theme(ChatRole::Assistant, "hi", false, &theme);
            assert_eq!(line_text(&line), "♪ hi", "{name} plain text stays symbols-only");
            assert!(
                line.spans.iter().all(|span| span.style.fg.is_none()),
                "{name} plain carries no fg"
            );
            assert!(
                line.spans.iter().all(|span| span.style.add_modifier.is_empty()),
                "{name} plain carries no modifiers"
            );
        }
        // The bridge actually varies by theme: Nebula's neon gutters differ
        // from Midnight's historical hues.
        let midnight = CliTheme::by_name("Midnight");
        let nebula = CliTheme::by_name("Nebula");
        assert_ne!(midnight.user, nebula.user);
        let styled = chat_line_with_theme(ChatRole::User, "hi", true, &nebula);
        assert_eq!(styled.spans[0].style.fg, Some(nebula.user));
    }

    // ------------------------------------------------------------------
    // Tool-log provenance rail (`‖ policy ok` / `‖ approval needed` /
    // `‖ denied`): text always, color only when styled.
    // ------------------------------------------------------------------

    #[test]
    fn tool_log_rail_themed_carries_policy_colors() {
        use crate::app::{ToolLogEntry, ToolStatus};
        let theme = CliTheme::by_name("Midnight");
        let entry = |status| ToolLogEntry {
            tool_name: "shell".into(),
            status,
            detail: None,
            duration_ms: Some(12),
        };
        let running = tool_log_line_with_theme(&entry(ToolStatus::Running), true, &theme);
        assert_eq!(line_text(&running), " ▶ shell 12ms ‖ approval needed");
        assert_eq!(running.spans[1].style.fg, Some(Color::Yellow));
        let ok = tool_log_line_with_theme(&entry(ToolStatus::Success), true, &theme);
        assert!(line_text(&ok).ends_with(" ‖ policy ok"), "{}", line_text(&ok));
        assert_eq!(ok.spans[1].style.fg, Some(Color::Green));
        let failed = tool_log_line_with_theme(&entry(ToolStatus::Failure), true, &theme);
        assert!(line_text(&failed).ends_with(" ‖ policy ok"), "{}", line_text(&failed));
        assert_eq!(failed.spans[1].style.fg, Some(Color::Green));
        let denied = tool_log_line_with_theme(
            &entry(ToolStatus::Timeout { timeout_secs: 30 }),
            true,
            &theme,
        );
        assert!(line_text(&denied).ends_with(" ‖ denied"), "{}", line_text(&denied));
        assert_eq!(denied.spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn tool_log_rail_plain_is_symbols_only() {
        use crate::app::{ToolLogEntry, ToolStatus};
        // Plain on every theme: the rail text stays, no ANSI anywhere.
        for name in ["Midnight", "Slate", "Chalk", "Nebula"] {
            let theme = CliTheme::by_name(name);
            for status in [
                ToolStatus::Running,
                ToolStatus::Success,
                ToolStatus::Failure,
                ToolStatus::Timeout { timeout_secs: 5 },
            ] {
                let line = tool_log_line_with_theme(
                    &ToolLogEntry {
                        tool_name: "fs_write".into(),
                        status,
                        detail: None,
                        duration_ms: None,
                    },
                    false,
                    &theme,
                );
                assert!(line_text(&line).contains(" ‖ "), "{name}: rail text present");
                assert!(
                    line.spans.iter().all(|span| span.style.fg.is_none()),
                    "{name}: plain carries no fg"
                );
                assert!(
                    line.spans.iter().all(|span| span.style.add_modifier.is_empty()),
                    "{name}: plain carries no modifiers"
                );
            }
        }
        let theme = CliTheme::by_name("Midnight");
        let line = tool_log_line_with_theme(
            &crate::app::ToolLogEntry {
                tool_name: "shell".into(),
                status: crate::app::ToolStatus::Running,
                detail: None,
                duration_ms: None,
            },
            false,
            &theme,
        );
        assert_eq!(line_text(&line), " ▶ shell ‖ approval needed");
    }

    // ------------------------------------------------------------------
    // Fence hardening: partial streaming prefixes must not leak fence
    // marker styling; the unmatched opener stays literal while the body
    // keeps the dim style + `│` gutter.
    // ------------------------------------------------------------------

    #[test]
    fn markdown_partial_fence_keeps_gutter_with_literal_opener() {
        let theme = CliTheme::by_name("Midnight");
        let spans = markdown_spans_with_theme("```\nlet x = 1;", true, &theme);
        let text: String = spans.iter().map(|span| span.content.to_string()).collect();
        assert_eq!(text, "```\n  │ let x = 1;");
        // The unmatched opener is literal raw text — never a dimmed boundary.
        assert_eq!(spans[0].content.as_ref(), "```");
        assert!(spans[0].style.add_modifier.is_empty());
        assert!(spans[0].style.fg.is_none());
        // The fenced body keeps the │ gutter and the dimmed body style.
        assert!(spans.iter().any(|span| span.content.as_ref() == "  │ "));
        let body = spans
            .iter()
            .find(|span| span.content.as_ref() == "let x = 1;")
            .expect("fenced body present");
        assert!(body.style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn markdown_fence_across_reveal_chunks_settles_without_marker_leak() {
        let theme = CliTheme::by_name("Midnight");
        // Chunk 1: opener seen, closer not yet — the opener stays literal
        // while the partial body already carries the gutter.
        let partial = markdown_spans_with_theme("intro\n```\nlet x", true, &theme);
        assert!(
            partial
                .iter()
                .filter(|span| span.content.as_ref() == "```")
                .all(|span| span.style.add_modifier.is_empty()),
            "partial opener must not render as a fence boundary"
        );
        assert!(partial.iter().any(|span| span.content.as_ref() == "  │ "));
        // Chunk 2 (full text): the closed fence renders both boundaries
        // dimmed and the body guttered — the settled contract is unchanged.
        let full = markdown_spans_with_theme("intro\n```\nlet x = 1;\n```\ndone", true, &theme);
        let markers: Vec<_> = full.iter().filter(|span| span.content.as_ref() == "```").collect();
        assert_eq!(markers.len(), 2);
        assert!(
            markers.iter().all(|span| span.style.add_modifier.contains(Modifier::DIM)),
            "closed boundaries stay dimmed"
        );
        let text: String = full.iter().map(|span| span.content.to_string()).collect();
        assert!(text.contains("  │ let x = 1;"));
        assert!(text.contains("done"));
    }

    // ------------------------------------------------------------------
    // Staged unified-diff review screen (P02)
    // ------------------------------------------------------------------

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect()
    }

    /// Review screen showing one staged file with a two-line change.
    fn populated_diff_review_app() -> App {
        use concerto_api_types::diff::Hunk;
        let mut app = App::new();
        app.screen = Screen::DiffReview;
        app.diff_review.results = vec![DiffResult {
            path: "src/lib.rs".into(),
            hunks: vec![Hunk {
                old_start: 10,
                old_len: 2,
                new_start: 10,
                new_len: 3,
                lines: vec![
                    DiffLine::Context { content: "fn kept() {}".into(), line_num: 10 },
                    DiffLine::Deletion { content: "fn old() {}".into(), line_num: 11 },
                    DiffLine::Addition { content: "fn new() {}".into(), line_num: 11 },
                ],
            }],
        }];
        app.diff_review.index = 0;
        app
    }

    #[test]
    fn diff_review_renders_at_all_supported_terminal_sizes() {
        for (width, height) in [(24, 8), (40, 12), (80, 24), (120, 40)] {
            let app = populated_diff_review_app();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| super::draw(frame, &app)).unwrap();
            let text = buffer_text(&terminal);
            assert!(
                text.contains("Diff review"),
                "review title missing at {width}x{height}: {text}"
            );
            assert!(text.contains("1/1"), "file position missing at {width}x{height}: {text}");
            assert!(
                text.contains("src/lib.rs"),
                "selected file missing at {width}x{height}: {text}"
            );
        }
    }

    #[test]
    fn diff_review_renders_unified_diff_rows() {
        let app = populated_diff_review_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("@@ -10,2 +10,3 @@"), "hunk header missing: {text}");
        assert!(text.contains(" fn kept() {}"), "context row missing: {text}");
        assert!(text.contains("-fn old() {}"), "deletion row missing: {text}");
        assert!(text.contains("+fn new() {}"), "addition row missing: {text}");
    }

    #[test]
    fn diff_review_shows_the_decision_hints() {
        let app = populated_diff_review_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("[a] accept"), "accept stake missing: {text}");
        assert!(text.contains("[r] reject"), "reject stake missing: {text}");
        assert!(text.contains("Esc close"), "close key missing: {text}");
    }

    #[test]
    fn diff_review_empty_state_renders() {
        let mut app = App::new();
        app.screen = Screen::DiffReview;
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Diff review"), "title missing on the empty screen: {text}");
        assert!(text.contains("No staged changes."), "empty state missing: {text}");
    }

    #[test]
    fn diff_review_status_row_reports_feedback() {
        let mut app = populated_diff_review_app();
        app.diff_review.status =
            Some(ReviewStatus::Decided { path: "src/lib.rs".into(), accept: true });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Accepted src/lib.rs"), "decision feedback missing: {text}");

        app.diff_review.status = Some(ReviewStatus::Blocked(
            "Staged changes changed. Review the latest diff before deciding.".into(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Blocked: Staged changes changed."),
            "blocked message missing (stakes head must survive truncation): {text}"
        );
    }

    #[test]
    fn diff_review_render_is_stable_with_a_stale_scroll_offset() {
        let mut app = populated_diff_review_app();
        app.diff_review.scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| super::draw(frame, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Diff review"), "a stale offset must not blank the review: {text}");
        assert!(text.contains("src/lib.rs"), "the header stays visible: {text}");
    }

    #[test]
    fn diff_review_lines_truncate_to_width_without_wrapping() {
        use concerto_api_types::diff::Hunk;
        let theme = CliTheme::by_name("Midnight");
        let result = DiffResult {
            path: "src/lib.rs".into(),
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 1,
                lines: vec![DiffLine::Addition {
                    content: format!("{}TAIL_MARKER", "x".repeat(300)),
                    line_num: 1,
                }],
            }],
        };
        let lines = diff_review_lines(&result, 20, &theme);
        // Hunk header + the single row: a long line is cut, never re-wrapped
        // into extra rows.
        assert_eq!(lines.len(), 2, "rows must not wrap");
        for line in &lines {
            let text = line_text(line);
            assert!(text.chars().count() <= 20, "row exceeds the viewport: {text:?}");
        }
        let addition = line_text(&lines[1]);
        assert!(addition.starts_with('+'), "the prefix is the non-color cue: {addition:?}");
        assert!(addition.ends_with('…'), "a cut row ends with an ellipsis: {addition:?}");
        assert!(!addition.contains("TAIL_MARKER"), "the cut tail stays hidden: {addition:?}");
    }

    #[test]
    fn diff_review_lines_explain_files_without_text_hunks() {
        let theme = CliTheme::by_name("Midnight");
        let result = DiffResult { path: "new.txt".into(), hunks: Vec::new() };
        let lines = diff_review_lines(&result, 40, &theme);
        assert_eq!(lines.len(), 1, "an empty create/delete still gets a row");
        assert!(line_text(&lines[0]).contains("No textual change"));
    }
}
