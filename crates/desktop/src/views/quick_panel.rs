use iced::widget::tooltip::Position;
use iced::widget::{
    button, column, container, pick_list, row, rule, scrollable, space, text, tooltip,
};
use iced::{Alignment, Background, Border, Element, Length, Shadow, Vector};

use crate::app::{App, Message, Page, RunStatus};

/// Width of the expanded quick panel.
const PANEL_WIDTH: u16 = 280;

/// Map a role string to its color from the agent_roles palette.
fn role_color(app: &App, role: &str) -> iced::Color {
    let palette = &app.current_theme.palette;
    palette
        .agent_roles
        .iter()
        .find(|(r, _)| format!("{:?}", r).eq_ignore_ascii_case(role))
        .map(|(_, c)| *c)
        .unwrap_or(palette.accent)
}

/// Render a small circular avatar coloured by the agent's role.
fn agent_avatar<'a>(app: &'a App, role: &str) -> Element<'a, Message> {
    let color = role_color(app, role);
    container(space::Space::new())
        .width(28.0)
        .height(28.0)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 999.0.into(), ..Default::default() },
            shadow: Shadow {
                color: iced::Color { a: 0.35, ..color },
                offset: Vector::new(0.0, 0.0),
                blur_radius: 10.0,
            },
            ..container::Style::default()
        })
        .into()
}

/// A small coloured dot indicating agent run status.
fn status_dot<'a>(_running: bool, app: &'a App) -> Element<'a, Message> {
    let color = if _running {
        app.current_theme.palette.success
    } else {
        app.current_theme.palette.text_muted
    };
    container(space::Space::new())
        .width(8.0)
        .height(8.0)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 999.0.into(), ..Default::default() },
            shadow: if _running {
                Shadow {
                    color: iced::Color { a: 0.4, ..color },
                    offset: Vector::new(0.0, 0.0),
                    blur_radius: 6.0,
                }
            } else {
                Shadow::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// Compact, status-first right rail. Every interactive control routes to a
/// real application action; the Evolution surface is described as unavailable
/// instead of being rendered as a dead button.
pub fn quick_panel_view(app: &App) -> Element<'_, Message> {
    let theme = &app.current_theme;
    let palette = &theme.palette;
    let ts = &theme.type_scale;
    let sp = &theme.spacing;

    let (run_label, run_color) = match app.run_status {
        RunStatus::Idle => ("Idle", palette.text_muted),
        RunStatus::Running => ("Running", palette.success),
        RunStatus::Cancelling => ("Cancelling", palette.warning),
    };
    let header = row![
        text("●").size(ts.caption).color(run_color),
        text(run_label).size(ts.body).width(Length::Fill),
        button(text("»").size(ts.caption)).style(button::text).on_press(Message::ToggleQuickPanel),
    ]
    .spacing(sp.sm)
    .align_y(Alignment::Center);

    let selected_model = app.chat_model_options.iter().find(|model| *model == &app.active_model);
    let model_pick = pick_list(app.chat_model_options.as_slice(), selected_model, |model| {
        Message::SetActiveModel(model.clone())
    })
    .width(Length::Fill);

    // --- Agent cards ---
    let mut agent_rows = column![].spacing(sp.sm);
    for agent in &app.orchestration_studio.agents {
        let role = &agent.role;
        let role_color_val = role_color(app, role);
        let model = agent.model_override.as_deref().unwrap_or("Default model");
        let is_running = app.run_status == RunStatus::Running;
        let agent_card = container(
            row![
                agent_avatar(app, role),
                column![
                    text(&agent.name).size(15).style(move |_| {
                        iced::widget::text::Style { color: Some(role_color_val) }
                    }),
                    text(model).size(12).color(palette.text_muted),
                ]
                .spacing(2)
                .width(Length::Fill),
                status_dot(is_running, app),
            ]
            .align_y(Alignment::Center)
            .spacing(10),
        )
        .padding(10)
        .style(move |_| crate::theme::card_style(palette))
        .width(Length::Fill);
        agent_rows = agent_rows.push(agent_card);
    }
    if app.orchestration_studio.agents.is_empty() {
        agent_rows = agent_rows
            .push(text("Configure agents in Studio").size(ts.caption).color(palette.text_muted));
    }

    let git_block: Element<'_, Message> = if let Some(summary) = &app.git_summary {
        let change_color =
            if summary.changed_files == 0 { palette.success } else { palette.warning };
        column![
            row![
                text("⑂").size(ts.caption).color(palette.text_muted),
                text(&summary.branch).size(ts.body).width(Length::Fill),
                text(format!("{} changed", summary.changed_files))
                    .size(ts.caption)
                    .color(change_color),
            ]
            .spacing(sp.sm)
            .align_y(Alignment::Center),
            button(text("Open Diff").size(ts.caption))
                .style(button::secondary)
                .width(Length::Fill)
                .on_press(Message::Navigate(Page::DiffViewer)),
        ]
        .spacing(sp.sm)
        .into()
    } else {
        text("The selected project is not a Git repository.")
            .size(ts.caption)
            .color(palette.text_muted)
            .into()
    };

    // Memory moved out of the quick panel into a modal (issue #110): this
    // button opens the Memory explorer dialog instead of expanding a section
    // inside the 280px rail. Same visual language as the other full-width
    // quick-panel buttons (Open Diff / view switcher).
    let memory_btn = tooltip(
        button(text("Memory").size(ts.caption))
            .style(button::secondary)
            .width(Length::Fill)
            .on_press(Message::OpenMemoryModal),
        text("Open Memory (Ctrl+M)").size(11),
        Position::Left,
    );

    let panel = column![
        header,
        rule::horizontal(1),
        model_pick,
        text("AGENTS")
            .size(11)
            .shaping(iced::widget::text::Shaping::Advanced)
            .style(move |_| crate::theme::sidebar_header_style(palette)),
        scrollable(agent_rows).height(Length::FillPortion(2)),
        rule::horizontal(1),
        memory_btn,
        rule::horizontal(1),
        text("GIT")
            .size(11)
            .shaping(iced::widget::text::Shaping::Advanced)
            .style(move |_| crate::theme::sidebar_header_style(palette)),
        git_block,
        rule::horizontal(1),
    ]
    .spacing(sp.sm)
    .padding(sp.lg);

    container(panel)
        .width(Length::Fixed(PANEL_WIDTH as f32))
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(palette.surface)),
            border: iced::Border { width: 0.0, ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

/// Collapsed thin strip with an expand toggle.
pub fn quick_panel_collapsed(app: &App) -> Element<'_, Message> {
    let palette = &app.current_theme.palette;
    let ts = &app.current_theme.type_scale;
    let sp = &app.current_theme.spacing;
    let strip_bg = Background::Color(palette.surface_variant);
    let toggle = button(text("«").size(ts.caption))
        .style(crate::ui::button::secondary)
        .on_press(Message::ToggleQuickPanel);
    container(column![toggle].spacing(sp.xs).padding(sp.sm))
        .width(Length::Shrink)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(strip_bg),
            ..container::Style::default()
        })
        .into()
}
