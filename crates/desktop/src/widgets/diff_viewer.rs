use iced::widget::{button, container, row, scrollable, text, Column};
use iced::{Color, Element, Length};
use std::collections::HashMap;
use std::ops::Range;

/// Represents a single line in a diff.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub before_number: Option<usize>,
    pub after_number: Option<usize>,
    pub before_content: String,
    pub after_content: String,
    pub kind: DiffKind,
    /// Hunk index this line belongs to (populated only for HunkHeader lines).
    pub hunk_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Unchanged,
    Addition,
    Deletion,
    HunkHeader,
}

/// Diff viewer widget.
///
/// Renders a side-by-side diff with:
/// - A single scrollable so both panes stay in sync
/// - Line numbers on both sides
/// - Background tints for additions/deletions
/// - Optional `visible_range` for viewport-aware rendering
/// - Optional `on_scrolled` callback to track scroll position for virtualization
///
/// Colors are passed from the theme palette to avoid hardcoded values.
pub fn view<'a>(
    diff_lines: &'a [DiffLine],
    success: Color,
    danger: Color,
    secondary: Color,
    visible_range: Option<Range<usize>>,
    on_scrolled: Option<fn(f32) -> crate::views::diff::Message>,
    decisions: &HashMap<usize, crate::views::diff::HunkDecision>,
) -> Element<'a, crate::views::diff::Message> {
    let color_for = |kind: &DiffKind| -> Option<Color> {
        match kind {
            DiffKind::Addition => Some(success),
            DiffKind::Deletion => Some(danger),
            DiffKind::HunkHeader => Some(secondary),
            DiffKind::Unchanged => None,
        }
    };

    let bg_for = |kind: &DiffKind| -> Option<Color> {
        match kind {
            DiffKind::Addition => Some(Color { a: 0.08, ..success }),
            DiffKind::Deletion => Some(Color { a: 0.08, ..danger }),
            DiffKind::HunkHeader => Some(Color { a: 0.12, ..secondary }),
            DiffKind::Unchanged => None,
        }
    };

    let lines_iter: Box<dyn Iterator<Item = &DiffLine>> = if let Some(range) = visible_range {
        let start = range.start.min(diff_lines.len());
        let end = (range.start + range.len()).min(diff_lines.len());
        Box::new(diff_lines[start..end].iter())
    } else {
        Box::new(diff_lines.iter())
    };

    // Build one side-by-side line per diff line inside a single scrollable.
    let rendered_lines: Vec<Element<'a, crate::views::diff::Message>> = lines_iter
        .map(|line| {
            let (before_txt, after_txt) = match line.kind {
                DiffKind::Addition => ("", line.after_content.as_str()),
                DiffKind::Deletion => (line.before_content.as_str(), ""),
                _ => (line.before_content.as_str(), line.after_content.as_str()),
            };

            let line_no_str = |n: Option<usize>| -> String {
                match n {
                    Some(v) => format!("{:>4}", v),
                    None => "    ".to_string(),
                }
            };

            let before_prefix = line_no_str(line.before_number);
            let after_prefix = line_no_str(line.after_number);

            let left_text = text(format!("{} {}", before_prefix, before_txt));
            let right_text = text(format!("{} {}", after_prefix, after_txt));

            // Apply color to text
            let left_elem: Element<'_, _> = if line.kind == DiffKind::Addition {
                // Hides left content for additions
                container(text("")).into()
            } else {
                let mut t = left_text;
                if let Some(c) = color_for(&line.kind) {
                    t = t.color(c);
                }
                t.into()
            };

            let right_elem: Element<'_, _> = if line.kind == DiffKind::Deletion {
                // Hides right content for deletions
                container(text("")).into()
            } else {
                let mut t = right_text;
                if let Some(c) = color_for(&line.kind) {
                    t = t.color(c);
                }
                t.into()
            };

            let line_row = row![
                container(left_elem).width(Length::FillPortion(1)),
                container(right_elem).width(Length::FillPortion(1)),
            ]
            .spacing(5);

            // Per-hunk accept/reject buttons on hunk header lines.
            let with_buttons: Element<'a, crate::views::diff::Message> =
                if line.kind == DiffKind::HunkHeader {
                    if let Some(hunk_idx) = line.hunk_index {
                        let is_accepted = matches!(
                            decisions.get(&(hunk_idx as usize)),
                            Some(crate::views::diff::HunkDecision::Accepted)
                        );
                        let is_rejected = matches!(
                            decisions.get(&(hunk_idx as usize)),
                            Some(crate::views::diff::HunkDecision::Rejected)
                        );
                        row![
                            line_row,
                            button(text("✓"))
                                .style(if is_accepted {
                                    crate::ui::button::primary
                                } else {
                                    crate::ui::button::secondary
                                })
                                .on_press(crate::views::diff::Message::AcceptHunk(hunk_idx)),
                            button(text("✗"))
                                .style(if is_rejected {
                                    crate::ui::button::danger
                                } else {
                                    crate::ui::button::secondary
                                })
                                .on_press(crate::views::diff::Message::RejectHunk(hunk_idx)),
                        ]
                        .spacing(4)
                        .align_y(iced::Alignment::Center)
                        .into()
                    } else {
                        line_row.into()
                    }
                } else {
                    line_row.into()
                };

            // Apply background tint for change lines and hunk headers
            match bg_for(&line.kind) {
                Some(bg) => container(with_buttons)
                    .style(move |_| container::Style {
                        background: Some(bg.into()),
                        ..container::Style::default()
                    })
                    .into(),
                None => with_buttons,
            }
        })
        .collect();

    let mut s = scrollable(Column::with_children(rendered_lines)).spacing(0);
    if let Some(cb) = on_scrolled {
        s = s.on_scroll(move |vp| cb(vp.absolute_offset().y));
    }
    s.into()
}
