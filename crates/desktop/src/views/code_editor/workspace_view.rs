//! Workspace chrome kept separate from the editing surface.
use iced::widget::tooltip::Position;
use iced::widget::{button, column, container, row, scrollable, space, text, text_input, tooltip};
use iced::{Alignment, Background, Element, Length};

use super::{cursor_line_col, DiagnosticSeverity, Message, State, FIND_INPUT_ID, GOTO_INPUT_ID};
use crate::theme::AppTheme;
use crate::widgets::file_tree::TreeNode;

impl State {
    pub(super) fn tabs_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let mut tabs = Vec::new();
        for path in &self.tabs {
            let active = self.active_file.as_ref() == Some(path);
            let marker = if self.tab_dirty(path) {
                " ●"
            } else if self.staged_hunks(path) > 0 {
                " M"
            } else {
                ""
            };
            let label = format!("{}{}", path.file_name().unwrap_or("?"), marker);
            let select = button(text(label).size(13))
                .padding([10, 14])
                .on_press(Message::FileSelected(path.clone()))
                .style(move |_theme: &iced::Theme, status| button::Style {
                    background: Some(Background::Color(
                        if active || status == button::Status::Hovered {
                            palette.surface_variant
                        } else {
                            palette.surface
                        },
                    )),
                    text_color: if active { palette.text } else { palette.text_muted },
                    border: iced::Border {
                        color: if active { palette.primary } else { palette.border },
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..button::Style::default()
                });
            tabs.push(
                row![
                    tooltip(select, text(path.as_str()).size(11), Position::Bottom),
                    button(text("×").size(14))
                        .padding(6)
                        .style(button::text)
                        .on_press(Message::CloseTab(path.clone())),
                ]
                .align_y(Alignment::Center)
                .into(),
            );
        }
        if tabs.is_empty() {
            tabs.push(
                container(text("Editor").size(13).color(palette.text_muted)).padding(12).into(),
            );
        }
        container(
            scrollable(row(tabs).spacing(0))
                .direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new())),
        )
        .width(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(palette.surface.into()),
            ..container::Style::default()
        })
        .into()
    }

    pub(super) fn tools_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let mut save = button(text("Save").size(12)).padding(6);
        let mut delete = button(text("Delete").size(12)).padding(6);
        if self.active_file.is_some() {
            save = save.on_press(Message::Save);
            delete = delete.on_press(Message::DeleteFile);
        }
        let mut controls = row![
            save,
            button(text("Find").size(12)).padding(6).on_press(Message::OpenFind),
            button(text("Replace").size(12)).padding(6).on_press(Message::OpenReplace),
            button(text("Go to line").size(12)).padding(6).on_press(Message::OpenGoto),
            button(text("Screenshot").size(12)).padding(6).on_press(Message::TakeScreenshot),
            button(text(if self.editor_tools_open { "Less" } else { "More" }).size(12))
                .padding(6)
                .on_press(Message::ToggleEditorTools),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        if self.editor_tools_open {
            controls = controls
                .push(button("New").padding(6).on_press(Message::NewFile))
                .push(delete)
                .push(button("Fold all").padding(6).on_press(Message::FoldAll))
                .push(button("Unfold").padding(6).on_press(Message::UnfoldAll))
                .push(
                    button(if self.trim_trailing_on_save {
                        "Trim on save: on"
                    } else {
                        "Trim on save: off"
                    })
                    .padding(6)
                    .on_press(Message::ToggleTrimTrailing),
                );
        }
        container(
            scrollable(controls)
                .direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new())),
        )
        .padding([4, 8])
        .width(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(theme.palette.surface.into()),
            ..container::Style::default()
        })
        .into()
    }

    pub(super) fn breadcrumbs_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let path =
            self.active_file.as_deref().map(|p| p.strip_prefix(self.tree.path()).unwrap_or(p));
        let label = path
            .map(|p| p.as_str().replace(['/', '\\'], "  ›  "))
            .unwrap_or_else(|| "Open a file in Explorer".into());
        container(text(label).size(12).color(theme.palette.text_muted))
            .padding([10, 14])
            .width(Length::Fill)
            .into()
    }

    pub(super) fn staged_banner<'a>(&'a self, theme: &'a AppTheme) -> Option<Element<'a, Message>> {
        let path = self.active_file.as_ref()?;
        let count = self.staged_hunks(path);
        if count == 0 {
            return None;
        }
        let mut accept = button(text("Accept all").size(12)).padding(6);
        let mut discard = button(text("Discard").size(12)).padding(6);
        if !self.dirty {
            accept = accept.on_press(Message::AcceptStaged);
            discard = discard.on_press(Message::DiscardStaged);
        }
        let label = if self.dirty {
            "Save your edits before reviewing staged changes".into()
        } else {
            format!(
                "Concerto staged {count} change{} in this file",
                if count == 1 { "" } else { "s" }
            )
        };
        Some(
            container(
                row![
                    text(label).size(12).color(theme.palette.text),
                    space::horizontal(),
                    button(
                        text(if self.review_open { "Back to code" } else { "Review diff" })
                            .size(12)
                    )
                    .padding(6)
                    .on_press(if self.review_open {
                        Message::CloseReview
                    } else {
                        Message::ReviewStaged
                    }),
                    accept,
                    discard,
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .width(Length::Fill)
            .style(move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(iced::Color {
                    a: 0.18,
                    ..theme.palette.secondary
                })),
                ..container::Style::default()
            })
            .into(),
        )
    }

    pub(super) fn status_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let diagnostics = self.document_diagnostics();
        let errors =
            diagnostics.iter().filter(|(_, d)| d.severity == DiagnosticSeverity::Error).count();
        let warnings =
            diagnostics.iter().filter(|(_, d)| d.severity == DiagnosticSeverity::Warning).count();
        let (line, column) = self.content.as_ref().map(cursor_line_col).unwrap_or((1, 1));
        let content = row![
            text(&self.lsp_status).size(11).color(palette.text_muted),
            button(text(format!("Problems  {errors} errors · {warnings} warnings")).size(11))
                .padding(4)
                .style(button::text)
                .on_press(Message::ToggleDiagnostics),
            space::horizontal(),
            button(text(format!("Ln {line}, Col {column}")).size(11))
                .padding(4)
                .style(button::text)
                .on_press(Message::OpenGoto),
            button(text(self.tab_mode.label()).size(11))
                .padding(4)
                .style(button::text)
                .on_press(Message::CycleTabMode),
            text(format!("UTF-8  {}  {}", self.line_ending, self.lang))
                .size(11)
                .color(palette.text_muted),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        container(
            scrollable(content)
                .direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new())),
        )
        .width(Length::Fill)
        .padding([4, 10])
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(palette.surface_variant.into()),
            border: iced::Border { color: palette.border, width: 1.0, radius: 0.0.into() },
            ..container::Style::default()
        })
        .into()
    }

    pub(super) fn explorer_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let header = row![
            text("Explorer").size(13).color(theme.palette.text_muted),
            space::horizontal(),
            tooltip(
                button(text("↻").size(15))
                    .style(button::text)
                    .padding(4)
                    .on_press(Message::RefreshTree),
                text("Refresh files and staged changes").size(11),
                Position::Bottom
            ),
        ]
        .align_y(Alignment::Center);
        let query = self.explorer_filter.trim().to_lowercase();
        let mut entries = Vec::new();
        self.explorer_rows(&self.tree, 0, &query, theme, &mut entries);
        for result in &self.staged {
            if result.path.as_std_path().exists()
                || (!query.is_empty() && !result.path.as_str().to_lowercase().contains(&query))
            {
                continue;
            }
            let label = result.path.strip_prefix(self.tree.path()).unwrap_or(&result.path);
            entries.push(
                button(text(format!("{label}  (staged)")).size(12).color(theme.palette.warning))
                    .width(Length::Fill)
                    .padding([6, 8])
                    .style(button::text)
                    .on_press(Message::FileSelected(result.path.clone()))
                    .into(),
            );
        }
        if entries.is_empty() {
            entries.push(
                text("No files match this filter").size(12).color(theme.palette.text_muted).into(),
            );
        }
        container(
            column![
                header,
                text_input("Filter files", &self.explorer_filter)
                    .padding(8)
                    .on_input(Message::ExplorerFilterChanged),
                scrollable(column(entries).spacing(1)).height(Length::Fill),
            ]
            .spacing(8)
            .padding(10),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(theme.palette.surface.into()),
            ..container::Style::default()
        })
        .into()
    }

    fn explorer_rows<'a>(
        &'a self,
        node: &'a TreeNode,
        depth: usize,
        query: &str,
        theme: &'a AppTheme,
        entries: &mut Vec<Element<'a, Message>>,
    ) {
        if !tree_matches(node, query) {
            return;
        }
        let palette = &theme.palette;
        let active = self.active_file.as_deref() == Some(node.path());
        let staged = self.staged_hunks(node.path()) > 0;
        let (label, message) = match node {
            TreeNode::Dir { name, path, expanded, .. } => (
                format!("{} {name}", if *expanded || !query.is_empty() { "▾" } else { "▸" }),
                Message::DirToggled(path.clone()),
            ),
            TreeNode::File { name, path, .. } => (
                format!("{name}{}", if staged { "  M" } else { "" }),
                Message::FileSelected(path.clone()),
            ),
        };
        let entry = button(text(label).size(12))
            .padding([6, 8])
            .width(Length::Fill)
            .on_press(message)
            .style(move |_theme: &iced::Theme, status| button::Style {
                background: if active {
                    Some(Background::Color(iced::Color { a: 0.24, ..palette.primary }))
                } else if status == button::Status::Hovered {
                    Some(palette.surface_variant.into())
                } else {
                    None
                },
                text_color: if active {
                    palette.text
                } else if staged {
                    palette.warning
                } else {
                    palette.text_muted
                },
                ..button::Style::default()
            });
        entries.push(
            container(entry)
                .padding(iced::Padding { left: (depth * 12) as f32, ..iced::Padding::ZERO })
                .into(),
        );
        if let TreeNode::Dir { children, expanded, .. } = node {
            if *expanded || !query.is_empty() {
                for child in children {
                    self.explorer_rows(child, depth + 1, query, theme, entries);
                }
            }
        }
    }

    pub(super) fn problems_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let diagnostics = self.document_diagnostics();
        let header = row![
            text(format!("Problems  {}", diagnostics.len())).size(13),
            text("Open documents").size(11).color(theme.palette.text_muted),
            space::horizontal(),
            button(text("×").size(14))
                .style(button::text)
                .padding(4)
                .on_press(Message::ToggleDiagnostics),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut rows = Vec::new();
        for (path, diagnostic) in diagnostics {
            let location = path.strip_prefix(self.tree.path()).unwrap_or(path);
            rows.push(
                button(
                    row![
                        text(diagnostic.severity.label())
                            .size(11)
                            .color(diagnostic.severity.color(&theme.palette)),
                        text(&diagnostic.message).size(12).width(Length::Fill),
                        text(format!(
                            "{} {}:{}",
                            location,
                            diagnostic.line + 1,
                            diagnostic.character + 1
                        ))
                        .size(11)
                        .color(theme.palette.text_muted),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .padding([6, 10])
                .style(button::text)
                .on_press(Message::DiagnosticSelected(
                    path.to_path_buf(),
                    diagnostic.line,
                    diagnostic.character,
                ))
                .into(),
            );
        }
        if rows.is_empty() {
            rows.push(
                text("No reported problems in open documents")
                    .size(12)
                    .color(theme.palette.text_muted)
                    .into(),
            );
        }
        container(column![
            container(header).padding([6, 10]),
            scrollable(column(rows).spacing(1)).height(Length::Fill),
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(theme.palette.surface.into()),
            border: iced::Border { color: theme.palette.border, width: 1.0, radius: 0.0.into() },
            ..container::Style::default()
        })
        .into()
    }

    pub(super) fn find_widget<'a>(&'a self, theme: &'a AppTheme) -> Option<Element<'a, Message>> {
        let palette = &theme.palette;
        // --- Find / replace bar (optional) ---
        if self.find_open {
            let match_label = if self.find_query.is_empty() {
                String::new()
            } else if self.find_matches.is_empty() {
                "No matches".to_string()
            } else {
                let current = self.find_current.map(|i| i + 1).unwrap_or(0);
                let overflow = if self.find_overflow { "+" } else { "" };
                format!("{current}/{}{overflow}", self.find_matches.len())
            };
            let find_row = row![
                text_input("Find...", &self.find_query)
                    .id(iced::widget::Id::from(FIND_INPUT_ID))
                    .on_input(Message::FindQueryChanged)
                    .on_submit(Message::FindNext)
                    .padding(6)
                    .width(Length::Fixed(240.0)),
                text(match_label).size(11).color(palette.text_muted),
                button(text("↑").size(12)).padding(6).on_press(Message::FindPrev),
                button(text("↓").size(12)).padding(6).on_press(Message::FindNext),
                button(text(if self.find_case_sensitive { "✓ Aa" } else { "Aa" }).size(12))
                    .padding(6)
                    .on_press(Message::ToggleFindCase),
                button(text("✕").size(12)).padding(6).on_press(Message::CloseFind),
            ]
            .spacing(6)
            .align_y(Alignment::Center);

            let bar_content: Element<'a, Message> = if self.replace_open {
                let replace_row = row![
                    text_input("Replace...", &self.replace_query)
                        .on_input(Message::ReplaceQueryChanged)
                        .on_submit(Message::ReplaceCurrent)
                        .padding(6)
                        .width(Length::Fixed(240.0)),
                    button(text("Replace").size(12)).padding(6).on_press(Message::ReplaceCurrent),
                    button(text("All").size(12)).padding(6).on_press(Message::ReplaceAll),
                ]
                .spacing(6)
                .align_y(Alignment::Center);
                column![find_row, replace_row].spacing(4).into()
            } else {
                find_row.into()
            };

            Some(
                container(bar_content)
                    .padding(6)
                    .width(Length::Fill)
                    .style(move |_theme: &iced::Theme| container::Style {
                        background: Some(Background::Color(palette.surface)),
                        border: iced::Border {
                            color: palette.border,
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        ..container::Style::default()
                    })
                    .into(),
            )
        } else {
            None
        }
    }

    pub(super) fn goto_widget<'a>(&'a self, theme: &'a AppTheme) -> Option<Element<'a, Message>> {
        let palette = &theme.palette;
        // --- Go-to-line bar (optional) ---
        if self.goto_open {
            Some(
                container(
                    row![
                        text("Go to line:").size(12).color(palette.text_muted),
                        text_input("Line number", &self.goto_input)
                            .id(iced::widget::Id::from(GOTO_INPUT_ID))
                            .on_input(Message::GotoInputChanged)
                            .on_submit(Message::GotoSubmit)
                            .padding(6)
                            .width(Length::Fixed(160.0)),
                        button(text("Go").size(12)).padding(6).on_press(Message::GotoSubmit),
                        button(text("✕").size(12)).padding(6).on_press(Message::CloseGoto),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .padding(6)
                .width(Length::Fill)
                .style(move |_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(palette.surface)),
                    border: iced::Border { color: palette.border, width: 1.0, radius: 0.0.into() },
                    ..container::Style::default()
                })
                .into(),
            )
        } else {
            None
        }
    }
}

/// A filter exposes matching descendants even when their ancestors are collapsed.
fn tree_matches(node: &TreeNode, query: &str) -> bool {
    if query.is_empty() || node.path().as_str().to_lowercase().contains(query) {
        return true;
    }
    match node {
        TreeNode::Dir { children, .. } => children.iter().any(|child| tree_matches(child, query)),
        TreeNode::File { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    /// Filtering sees descendants of collapsed directories and rejects missing queries safely.
    #[test]
    fn explorer_filter_finds_collapsed_descendants() {
        let node = TreeNode::Dir {
            name: "root".into(),
            path: Utf8PathBuf::from("/root"),
            expanded: false,
            children: vec![TreeNode::File {
                name: "Subscriptions.rs".into(),
                path: Utf8PathBuf::from("/root/Subscriptions.rs"),
                lang: "rust",
            }],
        };
        assert!(tree_matches(&node, "subscriptions"));
        assert!(tree_matches(&node, ""));
        assert!(!tree_matches(&node, "missing-file"));
    }
}
