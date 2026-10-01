use iced::widget::tooltip::Position;
use iced::widget::{
    button, column, container, pick_list, row, rule, scrollable, space, text, toggler, tooltip,
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

/// A small coloured dot indicating agent run status. Lights up (success)
/// only for the agent actually running; every other agent shows a muted
/// border dot, so the light is a real per-agent signal rather than a global
/// "something is running" indicator.
fn status_dot<'a>(running: bool, app: &'a App) -> Element<'a, Message> {
    let palette = &app.current_theme.palette;
    let color = if running { palette.success } else { palette.border };
    container(space::Space::new())
        .width(8.0)
        .height(8.0)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 999.0.into(), ..Default::default() },
            shadow: if running {
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

/// One agent card: avatar + name + model dropdown + per-agent status light.
/// The model picker quick-swaps that agent's override via `SetAgentModel`
/// using the same unified model list as the composer used to.
fn agent_card<'a>(
    app: &'a App,
    id: &'a str,
    name: &'a str,
    role: &'a str,
    model_override: Option<&'a String>,
) -> Element<'a, Message> {
    let theme = &app.current_theme;
    let palette = &theme.palette;
    let sp = &theme.spacing;
    let ts = &theme.type_scale;
    let role_color_val = role_color(app, role);
    let running = app.chat.agent_running(role);

    let selected =
        model_override.filter(|model| app.chat_model_options.iter().any(|option| option == *model));
    let agent_id = id.to_string();
    let picker = pick_list(app.chat_model_options.as_slice(), selected, move |model| {
        Message::SetAgentModel { agent_id: agent_id.clone(), model }
    })
    .placeholder("Default model")
    .text_size(ts.caption)
    .padding([2, 6])
    .width(Length::Fill);

    container(
        column![
            row![
                agent_avatar(app, role),
                column![text(name)
                    .size(14)
                    .style(move |_| iced::widget::text::Style { color: Some(role_color_val) }),]
                .width(Length::Fill),
                status_dot(running, app),
            ]
            .align_y(Alignment::Center)
            .spacing(sp.sm),
            picker,
        ]
        .spacing(sp.xs),
    )
    .padding(10)
    .style(move |_| crate::theme::card_style(palette))
    .width(Length::Fill)
    .into()
}

/// Read-only coordinator info card: avatar + name + model, no dropdown (the
/// coordinator is engine-owned, not a roster row).
fn coordinator_card<'a>(
    app: &'a App,
    coord: &'a crate::views::orchestration_studio::AgentConfig,
) -> Element<'a, Message> {
    let theme = &app.current_theme;
    let palette = &theme.palette;
    let sp = &theme.spacing;
    let role_color_val = role_color(app, &coord.role);
    // The coordinator is active while the run is in flight and no specialist
    // currently owns the work.
    let running = app.run_status == RunStatus::Running && !app.chat.has_running_subagent();
    let model = coord.model_override.as_deref().unwrap_or("Default model");

    container(
        row![
            agent_avatar(app, &coord.role),
            column![
                text(&coord.name)
                    .size(14)
                    .style(move |_| iced::widget::text::Style { color: Some(role_color_val) }),
                text(model).size(12).color(palette.text_muted),
            ]
            .spacing(2)
            .width(Length::Fill),
            status_dot(running, app),
        ]
        .align_y(Alignment::Center)
        .spacing(sp.sm),
    )
    .padding(10)
    .style(move |_| crate::theme::card_style(palette))
    .width(Length::Fill)
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
    let shortcuts_btn = tooltip(
        button(text("⌨").size(ts.body))
            .style(button::text)
            .padding([2, 6])
            .on_press(Message::HelpToggled),
        text("Keyboard shortcuts (?)").size(11),
        Position::Left,
    );
    let header = row![
        text("●").size(ts.caption).color(run_color),
        text(run_label).size(ts.body).width(Length::Fill),
        shortcuts_btn,
        button(text("»").size(ts.caption)).style(button::text).on_press(Message::ToggleQuickPanel),
    ]
    .spacing(sp.sm)
    .align_y(Alignment::Center);

    // Session toggles relocated from the composer (the message box is now
    // write-only: text + Send + New Session).
    let session_section = column![
        text("SESSION")
            .size(11)
            .shaping(iced::widget::text::Shaping::Advanced)
            .style(move |_| crate::theme::sidebar_header_style(palette)),
        row![
            text("Multi-agent").size(ts.body).width(Length::Fill),
            toggler(app.multi_agent)
                .on_toggle(|_| Message::Chat(crate::views::chat::Message::ToggleMultiAgent))
                .size(18),
        ]
        .align_y(Alignment::Center),
        row![
            text("Fast mode").size(ts.body).width(Length::Fill),
            toggler(app.fast)
                .on_toggle(|_| Message::Chat(crate::views::chat::Message::ToggleFastMode))
                .size(18),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(sp.sm)
    .width(Length::Fill);

    // --- Coordinator info box (read-only) ---
    let coordinator: Element<'_, Message> = match app.orchestration_studio.coordinator_agent() {
        Some(coord) => coordinator_card(app, coord),
        None => text("Coordinator unavailable").size(ts.caption).color(palette.text_muted).into(),
    };

    // --- Agent cards (one model dropdown + status light each) ---
    let mut agent_rows = column![].spacing(sp.sm);
    for agent in app.orchestration_studio.roster_agents() {
        agent_rows = agent_rows.push(agent_card(
            app,
            &agent.id,
            &agent.name,
            &agent.role,
            agent.model_override.as_ref(),
        ));
    }
    if app.orchestration_studio.roster_agents().next().is_none() {
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
        session_section,
        rule::horizontal(1),
        text("COORDINATOR")
            .size(11)
            .shaping(iced::widget::text::Shaping::Advanced)
            .style(move |_| crate::theme::sidebar_header_style(palette)),
        coordinator,
        rule::horizontal(1),
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
