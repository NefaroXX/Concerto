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

/// A model name usable as a picker selection: present and not blank.
///
/// Blank strings read as "unset" so they behave exactly like `None` — the
/// same rule the Settings "Default Model" picker applies to its own value.
fn non_blank(model: Option<&String>) -> Option<&String> {
    model.filter(|model| !model.trim().is_empty())
}

/// The model an agent picker must *display*: the per-agent override when one
/// exists, otherwise the global default from Settings. Resolving to the
/// global default keeps the card in agreement with Settings → Default Model
/// (`views::settings::mod.rs`), so an agent with no override reads as the
/// model it will actually run instead of the "Default model" placeholder.
/// `None` — and therefore the placeholder — only when neither is set.
fn effective_agent_model<'a>(
    model_override: Option<&'a String>,
    global_default_model: Option<&'a String>,
) -> Option<&'a String> {
    non_blank(model_override).or_else(|| non_blank(global_default_model))
}

/// Union `model` into the picker's option list, prepended when absent, so a
/// value that belongs to another provider (and is therefore missing from the
/// active provider's list) still resolves as `selected`. Blank/absent values
/// are ignored.
fn union_model_option(options: &mut Vec<String>, model: Option<&String>) {
    if let Some(model) = non_blank(model) {
        if !options.iter().any(|option| option == model) {
            options.insert(0, model.clone());
        }
    }
}

/// Build the per-agent model picker shared by roster and coordinator cards.
///
/// The agent's current override **and** the Settings global default are both
/// unioned into the shared option list (prepended when absent) so `selected`
/// resolves to the effective model even when it belongs to another provider
/// and is therefore missing from the active provider's list. Without the
/// union the picker silently falls back to the "Default model" placeholder.
/// Passing an owned `Vec` keeps the list alive inside the widget (no borrow
/// of the per-frame view).
///
/// Display-only: the emitted [`Message::SetAgentModel`] still writes exactly
/// the per-agent override and never touches `global_default_model`, so
/// picking a model here cannot silently repoint single-agent chat.
fn agent_model_picker<'a>(
    app: &App,
    agent_id: &str,
    model_override: Option<&String>,
    global_default_model: Option<&String>,
) -> Element<'a, Message> {
    let ts = app.current_theme.type_scale;
    let mut options = app.chat_model_options.clone();
    // Global default first so a present override lands ahead of it.
    union_model_option(&mut options, global_default_model);
    union_model_option(&mut options, model_override);
    let selected = effective_agent_model(model_override, global_default_model).cloned();
    let agent_id = agent_id.to_string();
    pick_list(options, selected, move |model| Message::SetAgentModel {
        agent_id: agent_id.clone(),
        model,
    })
    .placeholder("Default model")
    .text_size(ts.caption)
    .padding([2, 6])
    .width(Length::Fill)
    .into()
}

/// One agent card: avatar + name + model dropdown + per-agent status light.
/// The dropdown displays the agent's *effective* model (override, else the
/// Settings global default) and quick-swaps that agent's override via
/// `SetAgentModel` using the same unified model list as the composer used to.
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
    let role_color_val = role_color(app, role);
    let running = app.chat.agent_running(role);

    let picker =
        agent_model_picker(app, id, model_override, app.settings.global_default_model.as_ref());

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

/// Coordinator card: identical container and model picker to [`agent_card`],
/// so the engine-owned coordinator reads as a peer roster row — including the
/// same effective-model resolution (override, else the Settings global
/// default). The "engine-owned" note is preserved as a tooltip on the picker
/// rather than inline text.
///
/// Both roster states render this same card. `Some(coord)` is the engine-owned
/// row already present in `State::agents`; `None` is a config-owned roster,
/// whose persist path never writes the coordinator back, so the card falls
/// back to the frozen defaults (name "Coordinator", default model, idle light)
/// instead of degrading to bare text. The fallback picker still emits
/// `SetAgentModel` for the `coordinator` id, which creates the assignment and
/// the missing row — flipping the card into the `Some` state on first pick.
fn coordinator_card<'a>(
    app: &'a App,
    coord: Option<&'a crate::views::orchestration_studio::AgentConfig>,
) -> Element<'a, Message> {
    let theme = &app.current_theme;
    let palette = &theme.palette;
    let sp = &theme.spacing;
    let (id, name, role, model_override) = match coord {
        Some(coord) => (
            coord.id.as_str(),
            coord.name.as_str(),
            coord.role.as_str(),
            coord.model_override.as_ref(),
        ),
        // Fallback: the frozen definition's identity, so the picker targets
        // exactly the row `set_agent_model_override` creates.
        None => ("coordinator", "Coordinator", "coordinator", None),
    };
    let role_color_val = role_color(app, role);
    // The coordinator is active while the run is in flight and no specialist
    // currently owns the work; an absent row never runs, so it stays idle.
    let running =
        coord.is_some() && app.run_status == RunStatus::Running && !app.chat.has_running_subagent();
    let picker = tooltip(
        agent_model_picker(app, id, model_override, app.settings.global_default_model.as_ref()),
        text("Engine-owned coordinator model").size(11),
        Position::Left,
    );

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
    // The run-status header keeps only the status + collapse control: the
    // shortcuts button lives in the footer toolbar below, where it cannot be
    // squeezed behind the divider by the `Fill` run label.
    let header = row![
        text("●").size(ts.caption).color(run_color),
        text(run_label).size(ts.body).width(Length::Fill),
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

    // --- Coordinator card (same card + model picker as the roster) ---
    // Both states render the full card: `None` (config-owned roster) falls
    // back to the frozen defaults rather than degrading to bare text.
    let coordinator: Element<'_, Message> =
        coordinator_card(app, app.orchestration_studio.coordinator_agent());

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
    // Shortcuts toolbar: relocated out of the crowded run-status header. The
    // fixed width guarantees it can never be clipped behind the panel divider
    // regardless of the run label's length.
    let shortcuts_btn = tooltip(
        button(text("⌨").size(ts.body))
            .style(button::text)
            .padding([2, 6])
            .width(Length::Fixed(28.0))
            .on_press(Message::HelpToggled),
        text("Keyboard shortcuts (?)").size(11),
        Position::Left,
    );
    let footer_tools = row![memory_btn, shortcuts_btn]
        .spacing(sp.sm)
        .align_y(Alignment::Center)
        .width(Length::Fill);

    // The agent list scrolls; a right-side inner gutter (>= the 10px default
    // scrollbar width) keeps its vertical scrollbar from overlapping the
    // cards' right edges. Cards stay `Fill` inside the padded content.
    let agent_scroll = scrollable(
        container(agent_rows).width(Length::Fill).padding(iced::Padding::ZERO.right(sp.md)),
    )
    .height(Length::FillPortion(2));

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
        agent_scroll,
        rule::horizontal(1),
        footer_tools,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// given: an agent with a per-agent override and a Settings global default
    /// then: the override wins — it is what the agent actually runs
    #[test]
    fn effective_model_prefers_the_agent_override() {
        let agent_override = "anthropic/claude-sonnet-4".to_string();
        let global_default = "openai/gpt-4.1".to_string();
        let effective = effective_agent_model(Some(&agent_override), Some(&global_default));
        assert_eq!(effective.map(|model| model.as_str()), Some("anthropic/claude-sonnet-4"));
    }

    /// given: an agent with NO per-agent override but a Settings global default
    /// then: the global default resolves (never the "Default model" placeholder)
    #[test]
    fn effective_model_falls_back_to_the_settings_global_default() {
        let global_default = "openai/gpt-4.1".to_string();
        let effective = effective_agent_model(None, Some(&global_default));
        assert_eq!(effective.map(|model| model.as_str()), Some("openai/gpt-4.1"));
    }

    /// given: neither an override nor a global default (or blank values only)
    /// then: the model is unset, so only this state shows the placeholder
    #[test]
    fn effective_model_is_unset_only_when_both_are_missing_or_blank() {
        let blank = "   ".to_string();
        assert!(effective_agent_model(None, None).is_none());
        assert!(effective_agent_model(Some(&blank), None).is_none());
        assert!(effective_agent_model(None, Some(&blank)).is_none());
        // A blank override must not mask a real global default.
        let global_default = "openai/gpt-4.1".to_string();
        let effective = effective_agent_model(Some(&blank), Some(&global_default));
        assert_eq!(effective.map(|model| model.as_str()), Some("openai/gpt-4.1"));
    }

    /// given: an option list holding only the active provider's models
    /// then: both the override and the global default are unioned in, and a
    ///       repeated union never duplicates an entry
    #[test]
    fn option_list_unions_the_override_and_the_global_default() {
        let mut options = vec!["openai/gpt-4.1".to_string()];
        let agent_override = "anthropic/claude-sonnet-4".to_string();
        let other_global = "google/gemini-2.5-pro".to_string();

        union_model_option(&mut options, Some(&other_global));
        union_model_option(&mut options, Some(&agent_override));

        // Prepended in union order, so the override (the effective value when
        // present) sits first.
        assert_eq!(
            options,
            vec![
                "anthropic/claude-sonnet-4".to_string(),
                "google/gemini-2.5-pro".to_string(),
                "openai/gpt-4.1".to_string(),
            ]
        );

        union_model_option(&mut options, Some(&agent_override));
        union_model_option(&mut options, Some(&other_global));
        assert_eq!(options.len(), 3, "union must be idempotent");

        let blank = "  ".to_string();
        union_model_option(&mut options, Some(&blank));
        union_model_option(&mut options, None);
        assert_eq!(options.len(), 3, "blank/absent values must not be added");
    }
}
