use iced::widget::text::Shaping;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Background, Element, Length};

use crate::app::{App, Message, Page, ProjectTreeNode};
use crate::ui::{list_item, nav_item};

fn nav_bg(app: &App) -> Background {
    Background::Color(app.current_theme.palette.surface)
}

/// Short session id shown in the tree (first 10 chars).
fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(10).collect()
}

/// One project row in the tree: name + expand chevron. The row's active state
/// tracks whether this is the currently-open project.
fn project_row<'a>(app: &'a App, node: &'a ProjectTreeNode) -> Element<'a, Message> {
    let sp = &app.current_theme.spacing;
    let palette = &app.current_theme.palette;
    let is_active = concerto_core::helpers::canonical_project_path(&node.path)
        == concerto_core::helpers::canonical_project_path(&app.project_dir);

    let chevron = text(if node.expanded { "▾" } else { "▸" }).size(10).color(palette.text_muted);
    let label = text(&node.name)
        .size(13)
        .shaping(Shaping::Advanced)
        .style(move |_| crate::theme::sidebar_item_style(palette, is_active));

    list_item(
        &app.current_theme,
        is_active,
        Message::ToggleProjectExpanded(node.path.clone()),
        row![chevron, label].spacing(sp.xs).align_y(iced::Alignment::Center),
    )
}

/// One session row nested under an expanded project: short id + message-count
/// badge. Clicking resumes that session (switching projects first if needed).
fn session_row<'a>(
    app: &'a App,
    node: &'a ProjectTreeNode,
    session: &'a crate::views::chat::SessionRow,
) -> Element<'a, Message> {
    let sp = &app.current_theme.spacing;
    let palette = &app.current_theme.palette;
    let is_active =
        concerto_core::ids::Ulid::from_string(&session.session_id).ok() == app.active_session_id;

    let id = text(short_session_id(&session.session_id))
        .size(11)
        .shaping(Shaping::Advanced)
        .style(move |_| crate::theme::sidebar_item_style(palette, is_active));
    let badge = text(session.message_count.to_string()).size(9).color(palette.text_muted);

    let content = row![id, badge]
        .spacing(sp.xs)
        .align_y(iced::Alignment::Center)
        .padding(iced::Padding::new(0.0).left(14.0));

    list_item(
        &app.current_theme,
        is_active,
        Message::TreeSessionClicked {
            project: node.path.clone(),
            session_id: session.session_id.clone(),
        },
        content,
    )
}

pub fn sidebar_view(app: &App) -> Element<'_, Message> {
    let sp = &app.current_theme.spacing;
    let palette = &app.current_theme.palette;

    // ── Header: section label + "New project" ──
    let header = row![
        text("PROJECTS")
            .size(11)
            .shaping(Shaping::Advanced)
            .style(move |_| crate::theme::sidebar_header_style(palette)),
        button(text("+ New project").size(10).color(palette.text_muted))
            .style(crate::ui::button::secondary)
            .on_press(Message::OpenProjectDirPicker)
            .padding([4, 8]),
    ]
    .spacing(sp.sm)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    // ── Project → session tree ──
    let mut tree = column![].spacing(2).width(Length::Fill);
    for node in &app.project_tree {
        tree = tree.push(project_row(app, node));
        if node.expanded {
            match &node.sessions {
                Some(sessions) if sessions.is_empty() => {
                    tree = tree.push(
                        container(text("No sessions yet").size(10).color(palette.text_muted))
                            .padding(
                                iced::Padding::new(0.0).top(6.0).right(14.0).bottom(6.0).left(26.0),
                            )
                            .width(Length::Fill),
                    );
                }
                Some(sessions) => {
                    for session in sessions {
                        tree = tree.push(session_row(app, node, session));
                    }
                }
                None => {
                    tree = tree.push(
                        container(text("Loading…").size(10).color(palette.text_muted))
                            .padding(
                                iced::Padding::new(0.0).top(6.0).right(14.0).bottom(6.0).left(26.0),
                            )
                            .width(Length::Fill),
                    );
                }
            }
        }
    }
    let tree = scrollable(tree).height(Length::Fill).width(Length::Fill);

    // ── Footer: Settings + Studio ──
    let footer = column![
        container(space::Space::new()).padding([sp.xs, 0.0]),
        nav_item(
            &app.current_theme,
            Some("⚙"),
            "Settings",
            app.page == Page::Settings,
            Message::Navigate(Page::Settings),
        ),
        nav_item(
            &app.current_theme,
            Some("⊡"),
            "Studio",
            app.page == Page::OrchestrationStudio,
            Message::Navigate(Page::OrchestrationStudio),
        ),
    ]
    .spacing(2)
    .width(Length::Fill);

    let col = column![header, tree, footer].spacing(sp.sm).width(224.0);
    container(col)
        .width(Length::Shrink)
        .height(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(nav_bg(app)),
            border: iced::Border { width: 0.0, ..Default::default() },
            ..container::Style::default()
        })
        .into()
}
