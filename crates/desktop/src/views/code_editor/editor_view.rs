use std::collections::HashSet;

use iced::widget::{
    button, column, container, pane_grid, row, scrollable, stack, text, text_editor,
};
use iced::{Alignment, Background, Element, Length};

use crate::theme::AppTheme;
use crate::widgets::confirm_modal::ConfirmMessage;

use super::{BracketStatus, Diagnostic, Message, State};

impl State {
    /// Render the editor view.
    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        // --- Delete confirmation (armed by Message::DeleteFile) ---
        // Destructive actions get a confirm gate before anything is removed
        // (same pattern as the memory view's ConfirmModal).
        if let Some(modal) = &self.pending_delete {
            return modal.view().map(|msg| match msg {
                ConfirmMessage::Confirm => Message::DeleteConfirmed,
                ConfirmMessage::Cancel => Message::DeleteCancelled,
            });
        }

        if let Some(path) = &self.pending_close {
            return container(
                column![
                    text("Discard unsaved changes?").size(20),
                    text(format!("{} has unsaved edits.", path.file_name().unwrap_or("This file")))
                        .size(13),
                    row![
                        button("Discard and close").on_press(Message::CloseTabConfirmed),
                        button("Keep editing").on_press(Message::CloseTabCancelled),
                    ]
                    .spacing(10),
                ]
                .spacing(16)
                .padding(24),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }
        column![
            self.tabs_view(theme),
            self.tools_view(theme),
            self.pane_grid_view(theme),
            self.status_view(theme),
        ]
        .spacing(0)
        .height(Length::Fill)
        .into()
    }
    /// Explorer at left, with the editor and Problems stacked on the right.
    ///
    /// Each divider is clamped in `Message::PaneResized` to its own ratio
    /// regime (see `editor_core.rs`); `min_size` is the secondary pixel floor.
    /// `PaneGrid::min_size` is global — it applies to every pane on both axes —
    /// so it is set to a floor that suits the compact Problems panel without
    /// breaking the tree/editor behavior (whose ratio clamps remain the
    /// primary guard).
    fn pane_grid_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        pane_grid::PaneGrid::new(&self.pane_state, |pane, (), _maximized| {
            // Explorer at left; code above Problems on the right.
            if pane == self.tree_pane {
                pane_grid::Content::new(self.tree_pane_view(theme))
            } else if pane == self.editor_pane {
                pane_grid::Content::new(self.editor_pane(theme))
            } else if pane == self.diag_pane {
                pane_grid::Content::new(self.diag_pane_view(theme))
            } else {
                // Degenerate fallback (single-pane layout): render like the
                // editor so the pane is never blank.
                pane_grid::Content::new(self.editor_pane(theme))
            }
        })
        .on_resize(10.0, Message::PaneResized)
        .min_size(100.0)
        .style(move |_theme: &iced::Theme| pane_grid::Style {
            hovered_region: pane_grid::Highlight {
                background: Background::Color(iced::Color { a: 0.35, ..palette.primary }),
                border: iced::Border { color: palette.primary, width: 2.0, radius: 0.0.into() },
            },
            hovered_split: pane_grid::Line { color: palette.primary, width: 2.0 },
            picked_split: pane_grid::Line { color: palette.primary, width: 2.0 },
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn tree_pane_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        self.explorer_view(theme)
    }

    fn editor_pane<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let mut contents = column![self.breadcrumbs_view(theme)].spacing(0);
        if let Some(banner) = self.staged_banner(theme) {
            contents = contents.push(banner);
        }
        if self.review_open {
            contents = contents.push(self.review_view(theme));
        } else {
            if let Some(hover) = &self.hover {
                contents = contents.push(
                    container(
                        row![
                            text(hover).size(12).width(Length::Fill),
                            button("×")
                                .padding(4)
                                .style(button::text)
                                .on_press(Message::ClearHover),
                        ]
                        .spacing(8),
                    )
                    .padding([6, 12]),
                );
            } else if let Some((line, _)) = self.cursor_position() {
                if let Some(diagnostic) = self.diagnostics.iter().find(|d| d.line == line) {
                    contents = contents.push(
                        container(
                            text(&diagnostic.message)
                                .size(12)
                                .color(diagnostic.severity.color(&theme.palette)),
                        )
                        .padding([6, 12]),
                    );
                }
            }
            contents = contents.push(self.editor_surface(theme));
        }
        contents.height(Length::Fill).into()
    }

    fn review_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        use concerto_api_types::diff::DiffLine;
        let Some(result) = self.staged_file() else {
            return text("No staged changes for this file").into();
        };
        let mut lines: Vec<Element<'a, Message>> = Vec::new();
        use concerto_tools::virtual_fs::VirtualFsEntry;
        let operation = match self.staged_entries.get(&result.path) {
            Some(VirtualFsEntry::Created { .. }) => Some("New file"),
            Some(VirtualFsEntry::Deleted { .. }) => Some("File deletion"),
            _ => None,
        };
        if let Some(operation) = operation {
            lines.push(text(operation).size(14).color(theme.palette.secondary).into());
        }
        for hunk in &result.hunks {
            lines.push(
                text(format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ))
                .size(12)
                .font(theme.font_stack.mono)
                .color(theme.palette.secondary)
                .into(),
            );
            for line in &hunk.lines {
                let (prefix, number, content, color) = match line {
                    DiffLine::Addition { content, line_num } => {
                        ("+", line_num, content, theme.palette.success)
                    }
                    DiffLine::Deletion { content, line_num } => {
                        ("−", line_num, content, theme.palette.danger)
                    }
                    DiffLine::Context { content, line_num } => {
                        (" ", line_num, content, theme.palette.text_muted)
                    }
                    _ => continue,
                };
                lines.push(
                    text(format!("{prefix} {number:>5}  {content}"))
                        .size(13)
                        .font(theme.font_stack.mono)
                        .color(color)
                        .into(),
                );
            }
        }
        scrollable(column(lines).spacing(4).padding(14)).height(Length::Fill).into()
    }

    /// Editing surface with gutter markers and floating search/completion.
    fn editor_surface<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // --- Editor area ---
        let editor_area = if let Some(content) = &self.content {
            let lang = self.lang;
            let tab_mode = self.tab_mode;
            let completion_open = self.completion_open;
            let editor = text_editor(content)
                .placeholder("Select a file to start editing...")
                .on_action(Message::Edit)
                .height(Length::Fill)
                .font(theme.font_stack.mono)
                .size(theme.font_stack.base_size)
                .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(24.0)))
                .padding(8)
                .style(move |_theme, _status| text_editor::Style {
                    background: palette.surface_variant.into(),
                    border: iced::Border { color: palette.border, width: 0.0, radius: 0.0.into() },
                    placeholder: palette.text_muted,
                    value: palette.text,
                    selection: palette.primary,
                })
                .highlight(lang, iced::highlighter::Theme::SolarizedDark)
                .key_binding(move |key_press| {
                    crate::views::code_editor::editor_key_binding(
                        key_press,
                        tab_mode,
                        completion_open,
                    )
                });

            // Gutter marks: diagnostics, bracket-pair target, word occurrences.
            let line_count = content.line_count();
            let diag_lines: HashSet<usize> = self.diagnostics.iter().map(|d| d.line).collect();
            let bracket_line = match self.bracket_status {
                BracketStatus::Matched { other_line, .. } => Some(other_line),
                _ => None,
            };
            let occurrence_lines: HashSet<usize> = self.word_occurrences.iter().copied().collect();
            let fold_anchors: HashSet<usize> = self.folds.iter().map(|f| f.start).collect();
            let region_starts: HashSet<usize> = if self.folds.is_empty() {
                self.region_cache.2.iter().map(|r| r.0).collect()
            } else {
                HashSet::new()
            };
            let any_marks = line_count > 0
                || !diag_lines.is_empty()
                || bracket_line.is_some()
                || !occurrence_lines.is_empty()
                || !fold_anchors.is_empty()
                || !region_starts.is_empty();

            let editor_with_diags: Element<'a, Message> = if !any_marks {
                container(editor).into()
            } else {
                // Gutter with line numbers; fold chevrons on foldable lines;
                // marker priority: diag > bracket > occurrence.
                let mut gutter: Vec<Element<'a, Message>> = Vec::new();
                for line_idx in 0..line_count {
                    let line_num = (line_idx + 1).to_string();
                    let chevron: Element<'a, Message> = if fold_anchors.contains(&line_idx) {
                        button(text("▼").size(9))
                            .padding(0)
                            .on_press(Message::ToggleFold(line_idx))
                            .into()
                    } else if region_starts.contains(&line_idx) {
                        button(text("▶").size(9))
                            .padding(0)
                            .on_press(Message::ToggleFold(line_idx))
                            .into()
                    } else {
                        text(" ").size(9).into()
                    };
                    let marker = if diag_lines.contains(&line_idx) {
                        let diags: Vec<&Diagnostic> =
                            self.diagnostics.iter().filter(|d| d.line == line_idx).collect();
                        let color = diags
                            .first()
                            .map(|d| d.severity.color(palette))
                            .unwrap_or(palette.text_muted);
                        text("●").size(10).color(color)
                    } else if bracket_line == Some(line_idx) {
                        text("⟨⟩").size(10).color(palette.success)
                    } else if occurrence_lines.contains(&line_idx) {
                        text("○").size(10).color(palette.accent)
                    } else {
                        text(" ").size(10)
                    };
                    gutter.push(
                        row![chevron, text(line_num).size(11).color(palette.text_muted), marker]
                            .spacing(4)
                            .align_y(Alignment::Center)
                            .height(Length::Fixed(24.0))
                            .into(),
                    );
                }
                let gutter_col =
                    scrollable(column(gutter).spacing(0).padding([8, 4])).width(Length::Shrink);

                row![gutter_col, editor].spacing(0).into()
            };

            container(editor_with_diags).width(Length::Fill).height(Length::Fill).style(
                move |_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(palette.surface_variant)),
                    ..container::Style::default()
                },
            )
        } else {
            // Empty state
            let hero = column![
                text("📝").size(48),
                text("Code Editor").size(24),
                text("Select a file from the tree to start editing.")
                    .size(14)
                    .color(palette.text_muted),
            ]
            .spacing(12)
            .align_x(iced::Alignment::Center);

            container(hero)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
        };

        // --- Completion popup (if open) ---
        // Docked at the bottom of the editor area. The actual items array is
        // immutable during view(), so we work with references.
        let completion_panel: Option<Element<'a, Message>> = if self.completion_open
            && !self.completion_items.is_empty()
        {
            let items: Vec<(usize, &super::CompletionItem)> = self
                .completion_items
                .iter()
                .enumerate()
                .skip(self.completion_selected.saturating_sub(3))
                .take(8)
                .collect();
            let selected = self.completion_selected;
            let rows: Vec<Element<'a, Message>> = items
                .into_iter()
                .map(|(i, item)| {
                    let is_sel = i == selected;
                    let label = if is_sel {
                        text(format!("▸ {} {}", item.label, item.detail.as_deref().unwrap_or("")))
                            .color(palette.text)
                    } else {
                        text(format!("  {} {}", item.label, item.detail.as_deref().unwrap_or("")))
                            .color(palette.text_muted)
                    };
                    button(label)
                        .padding(4)
                        .width(Length::Fill)
                        .on_press(Message::CompletionPick(i))
                        .style(move |_theme: &iced::Theme, status| {
                            let bg = match (is_sel, status) {
                                (_, button::Status::Hovered) | (true, _) => palette.primary,
                                _ => palette.surface_variant,
                            };
                            button::Style {
                                background: Some(Background::Color(bg)),
                                text_color: palette.text,
                                border: iced::Border::default(),
                                ..button::Style::default()
                            }
                        })
                        .into()
                })
                .collect();
            Some(
                container(
                    column![
                        scrollable(column(rows).spacing(0)).height(Length::Fixed(200.0)),
                        text("Tab / Enter accept · Esc close").size(11).color(palette.text_muted),
                    ]
                    .spacing(6)
                    .padding(8),
                )
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
        };

        let mut layers: Vec<Element<'a, Message>> = vec![editor_area.into()];
        if let Some(panel) = completion_panel {
            layers.push(
                container(container(panel).max_width(440))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Right)
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(12)
                    .into(),
            );
        }
        if let Some(panel) = self.find_widget(theme).or_else(|| self.goto_widget(theme)) {
            layers.push(
                container(container(panel).max_width(540))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Right)
                    .align_y(iced::alignment::Vertical::Top)
                    .padding(12)
                    .into(),
            );
        }
        stack(layers).into()
    }

    fn diag_pane_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        self.problems_view(theme)
    }
}
