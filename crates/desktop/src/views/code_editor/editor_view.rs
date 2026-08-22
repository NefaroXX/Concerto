use std::collections::HashSet;

use iced::widget::tooltip::Position;
use iced::widget::{
    button, column, container, pane_grid, row, scrollable, text, text_editor, text_input, tooltip,
};
use iced::{Alignment, Background, Element, Length};

use crate::theme::AppTheme;
use crate::widgets::confirm_modal::ConfirmMessage;
use crate::widgets::file_tree;

use super::{
    cursor_line_col, BracketStatus, Diagnostic, Message, State, FIND_INPUT_ID, GOTO_INPUT_ID,
};

impl State {
    /// Render the editor view.
    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // --- Delete confirmation (armed by Message::DeleteFile) ---
        // Destructive actions get a confirm gate before anything is removed
        // (same pattern as the memory view's ConfirmModal).
        if let Some(modal) = &self.pending_delete {
            return modal.view().map(|msg| match msg {
                ConfirmMessage::Confirm => Message::DeleteConfirmed,
                ConfirmMessage::Cancel => Message::DeleteCancelled,
            });
        }

        // --- File tree sidebar ---
        // Built per-pane in `tree_pane_view` (pane_grid renders panes from
        // the closure inside `PaneGrid::new`).

        // --- Toolbar ---
        let toolbar = row![
                button(text("💾 Save").size(12)).padding(6).on_press(Message::Save),
                button(text("✚ New").size(12)).padding(6).on_press(Message::NewFile),
                button(text("🗑 Delete").size(12)).padding(6).on_press(Message::DeleteFile),
                button(text("↻ Refresh").size(12)).padding(6).on_press(Message::RefreshTree),
                button(text(if self.show_diagnostics { "▲ Diag" } else { "▼ Diag" }).size(12))
                    .padding(6)
                    .on_press(Message::ToggleDiagnostics),
                button(text(self.tab_mode.label()).size(12))
                    .padding(6)
                    .on_press(Message::CycleTabMode),
                button(text("Fold all").size(12)).padding(6).on_press(Message::FoldAll),
                button(text("Unfold").size(12)).padding(6).on_press(Message::UnfoldAll),
                button(
                    text(if self.trim_trailing_on_save { "✓ Trim ws" } else { "Trim ws" }).size(12),
                )
                .padding(6)
                .on_press(Message::ToggleTrimTrailing),
            ]
        .spacing(6)
        .align_y(Alignment::Center);

        let toolbar_container =
            container(toolbar).padding(8).style(move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(palette.surface)),
                border: iced::Border { color: palette.border, width: 1.0, radius: 0.0.into() },
                ..container::Style::default()
            });

        // --- Find / replace bar (optional) ---
        let find_bar: Option<Element<'a, Message>> = if self.find_open {
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
        };

        // --- Go-to-line bar (optional) ---
        let goto_bar: Option<Element<'a, Message>> = if self.goto_open {
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
        };

        // --- Layout ---
        let mut layout = column![toolbar_container].spacing(0);
        if let Some(bar) = find_bar {
            layout = layout.push(bar);
        }
        if let Some(bar) = goto_bar {
            layout = layout.push(bar);
        }
        layout.push(self.pane_grid_view(theme)).into()
    }
    /// The resizable tree | editor | diagnostics pane grid (#90, #108).
    ///
    /// Each divider is clamped in `Message::PaneResized` to its own ratio
    /// regime (see `editor_core.rs`); `min_size` is the secondary pixel floor.
    /// `PaneGrid::min_size` is global — it applies to every pane on both axes —
    /// so it is set to a floor that suits the narrow diagnostics pane without
    /// breaking the tree/editor behavior (whose ratio clamps remain the
    /// primary guard).
    fn pane_grid_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        pane_grid::PaneGrid::new(&self.pane_state, |pane, (), _maximized| {
            // Three panes: tree (left) | editor (center) | diagnostics (right).
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

    /// The file-tree pane content.
    fn tree_pane_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let tree_view =
            file_tree::view(&self.tree, self.active_file.as_deref()).map(Message::FileTree);
        container(tree_view)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(palette.surface)),
                ..container::Style::default()
            })
            .into()
    }

    /// The editor pane: the editor area (+ gutter marks and completion popup).
    /// The diagnostics / status column now lives in its own resizable pane
    /// (`diag_pane_view`), so this pane renders only the editing surface.
    fn editor_pane<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
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
            let any_marks = !diag_lines.is_empty()
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
                            .into(),
                    );
                }
                let gutter_col = scrollable(column(gutter).spacing(0)).width(Length::Shrink);

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
            let items: Vec<(usize, &super::CompletionItem)> =
                self.completion_items.iter().enumerate().take(20).collect();
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
                container(scrollable(column(rows).spacing(0)).height(Length::Fixed(180.0)))
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
        };

        // Wrap the editor area with the completion panel when open.
        let editor_el: Element<'a, Message> = match completion_panel {
            Some(panel) => column![editor_area, panel].spacing(0).into(),
            None => editor_area.into(),
        };

        editor_el
    }

    /// The diagnostics / status pane content (#108).
    ///
    /// Rendered by the pane grid for the diagnostics pane. It shows the LSP
    /// diagnostics list, else the current hover tooltip, else a slim status
    /// panel (the empty state when there is nothing to show). Every variant
    /// fills the pane on both axes with a palette background so the column
    /// reads as an intentional panel rather than dead vertical space; width
    /// tracks the pane (`Length::Fill`) instead of `FillPortion` constants.
    fn diag_pane_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // --- Diagnostics list (when enabled and non-empty) ---
        if self.show_diagnostics && !self.diagnostics.is_empty() {
            let mut rows: Vec<Element<'a, Message>> = Vec::new();
            for diag in &self.diagnostics {
                let color = diag.severity.color(palette);
                rows.push(
                    row![
                        text(format!("{}:{} ", diag.line + 1, diag.character + 1))
                            .size(11)
                            .color(palette.text_muted),
                        text(diag.severity.label()).size(11).color(color),
                        text(&diag.message).size(11),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into(),
                );
            }
            container(column(rows).spacing(4).padding(8))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(palette.surface)),
                    ..container::Style::default()
                })
                .into()
        } else if let Some(hover) = &self.hover {
            // --- Hover tooltip (transient) ---
            container(text(hover).size(12).color(palette.text))
                .padding(8)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(palette.surface_variant)),
                    ..container::Style::default()
                })
                .into()
        } else {
            // --- Slim status panel: the empty state when the pane has no
            // diagnostics and no hover (#108). Stacked vertically so it fits
            // the narrow default width instead of overflowing.
            let file_label = self
                .active_file
                .as_ref()
                .map(|p| p.file_name().unwrap_or("?").to_string())
                .unwrap_or_else(|| "No file open".to_string());
            let dirty_label = if self.dirty { "●" } else { "" };
            let diag_count = self.diagnostics.len();
            let (cursor_line, cursor_col) =
                self.content.as_ref().map(cursor_line_col).unwrap_or((1, 1));
            let bracket_label = match self.bracket_status {
                BracketStatus::None => String::new(),
                BracketStatus::Matched { other_line, .. } => {
                    format!(" · ⇄ Ln {}", other_line + 1)
                }
                BracketStatus::Unmatched => " · ⚠ unmatched".to_string(),
            };
            let word_label = match &self.current_word {
                Some(word) if !self.word_occurrences.is_empty() => {
                    format!(" · {word} ×{}", self.word_occurrences.len())
                }
                _ => String::new(),
            };
            let fold_label = if self.folds.is_empty() {
                String::new()
            } else {
                format!(" · {} folded", self.folds.len())
            };

            // The file label shows the full path on hover — the pane is
            // narrow, so on-screen it is clipped without an ellipsis API.
            let file_label_el: Element<'a, Message> = match &self.active_file {
                Some(path) => tooltip(
                    text(format!("{file_label}{dirty_label}")).size(13),
                    text(path.to_string()).size(12).color(palette.text_muted),
                    Position::Top,
                )
                .into(),
                None => text(format!("{file_label}{dirty_label}")).size(13).into(),
            };

            let mut status: Vec<Element<'a, Message>> = vec![
                file_label_el,
                text(format!(" · {diag_count} diagnostics · {}", self.lang))
                    .size(11)
                    .color(palette.text_muted)
                    .into(),
                text(format!(" · Ln {cursor_line}, Col {cursor_col}"))
                    .size(11)
                    .color(palette.text_muted)
                    .into(),
            ];
            if !bracket_label.is_empty() {
                status.push(text(bracket_label).size(11).color(palette.success).into());
            }
            if !word_label.is_empty() {
                status.push(text(word_label).size(11).color(palette.accent).into());
            }
            if !fold_label.is_empty() {
                status.push(text(fold_label).size(11).color(palette.secondary).into());
            }
            status.push(text(self.tab_mode.label()).size(11).color(palette.text_muted).into());

            container(column(status).spacing(4))
                .padding(8)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(palette.surface)),
                    ..container::Style::default()
                })
                .into()
        }
    }
}
