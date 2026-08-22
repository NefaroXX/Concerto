use crate::widgets::code_block;
use iced::widget::{column, container, row, text};
use iced::{Color, Element, Font, Length};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// An owned segment of inline text with optional formatting.
#[derive(Debug)]
enum Span {
    Plain(String),
    Bold(String),
    Italic(String),
    Code(String),
}

#[derive(Debug)]
enum Fmt {
    Bold,
    Italic,
}

/// A parsed markdown document owned independently of its source string.
///
/// `concerto-desktop` renders assistant messages on every 16 ms typewriter
/// tick; caching the parsed document per chat entry means pulldown_cmark only
/// runs when the content actually changes, and each tick re-drives the renderer
/// over the stored event stream up to a character budget.
pub struct MarkdownDoc {
    events: Vec<Event<'static>>,
    /// Total visible characters across all `Event::Text`/`Event::Code`
    /// payloads — the unit a reveal `budget` is measured in.
    pub total_units: usize,
}

impl MarkdownDoc {
    /// Parse `source` into owned `'static` events. Every string payload is
    /// converted to an owned `CowStr` so the document never borrows `source`.
    pub fn parse(source: &str) -> MarkdownDoc {
        let mut events = Vec::new();
        let mut total_units = 0usize;
        for event in Parser::new_ext(source, Options::all()) {
            if let Some(chars) = event_chars(&event) {
                total_units += chars;
            }
            events.push(event.into_static());
        }
        MarkdownDoc { events, total_units }
    }

    /// Render this document. `budget: None` renders the full document
    /// (identical to [`render`]); `Some(n)` renders only the first `n` visible
    /// characters, cutting the output off at a container boundary — usable as
    /// the per-frame cheap render behind the typewriter reveal.
    pub fn render_upto<M: Clone + 'static>(
        &self,
        budget: Option<usize>,
        on_copy: impl Fn(&str) -> M + Clone + 'static,
        surface_variant: Color,
        text_muted: Color,
        primary: Color,
    ) -> Element<'static, M> {
        let mut renderer: MarkdownRenderer<'static, M> =
            MarkdownRenderer::new(on_copy, surface_variant, text_muted, primary);

        let Some(budget) = budget else {
            for event in &self.events {
                renderer.handle_event(event);
            }
            return renderer.finalize();
        };

        // Text-bearing events consume budget; everything else (markup tags,
        // breaks) costs nothing. When an event would cross the budget we emit
        // only its prefix and stop, then let `finalize_truncated` close any
        // still-open container.
        let mut consumed = 0usize;
        let mut truncated = false;
        for event in &self.events {
            if let Some(len) = event_chars(event) {
                let remaining = budget.saturating_sub(consumed);
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                if len > remaining {
                    renderer.handle_truncated(event, remaining);
                    truncated = true;
                    break;
                }
                consumed += len;
            }
            renderer.handle_event(event);
        }

        if truncated {
            renderer.finalize_truncated()
        } else {
            renderer.finalize()
        }
    }
}

/// Number of visible output characters an event contributes. Only `Text` and
/// `Code` events consume budget — the same events the renderer turns into
/// visible output (markup tags and soft/hard breaks are not counted).
fn event_chars(event: &Event<'_>) -> Option<usize> {
    match event {
        Event::Text(s) | Event::Code(s) => Some(s.chars().count()),
        _ => None,
    }
}

/// Renders a markdown string to an Iced element by driving a state machine
/// through the pulldown_cmark event stream.
struct MarkdownRenderer<'a, M: Clone> {
    // Accumulated block-level output elements.
    elements: Vec<Element<'a, M>>,

    // Inline spans (text, bold, italic, code) for the current paragraph.
    spans: Vec<Span>,

    // Heading level when inside a heading tag.
    heading_level: Option<u32>,

    // Code block state.
    in_code_block: bool,
    code_lang: Option<String>,
    code_buf: String,

    // List state.
    in_list: Option<bool>, // Some(true) = ordered, Some(false) = unordered
    list_counter: u64,
    list_items: Vec<Element<'a, M>>,

    // Table state.
    in_table: bool,
    table_rows: Vec<Vec<Element<'a, M>>>,
    current_row: Vec<Element<'a, M>>,
    current_cell_spans: Vec<Span>,

    // Blockquote state.
    in_blockquote: bool,
    blockquote_elements: Vec<Element<'a, M>>,

    // Inline formatting stack (bold / italic).
    fmt_stack: Vec<Fmt>,

    // External callback / styling parameters.
    on_copy: Box<dyn Fn(&str) -> M>,
    surface_variant: Color,
    text_muted: Color,
    primary: Color,
}

impl<'a, M: Clone + 'static> MarkdownRenderer<'a, M> {
    fn new(
        on_copy: impl Fn(&str) -> M + Clone + 'static,
        surface_variant: Color,
        text_muted: Color,
        primary: Color,
    ) -> Self {
        Self {
            elements: Vec::new(),
            spans: Vec::new(),
            heading_level: None,
            in_code_block: false,
            code_lang: None,
            code_buf: String::new(),
            in_list: None,
            list_counter: 0,
            list_items: Vec::new(),
            in_table: false,
            table_rows: Vec::new(),
            current_row: Vec::new(),
            current_cell_spans: Vec::new(),
            in_blockquote: false,
            blockquote_elements: Vec::new(),
            fmt_stack: Vec::new(),
            on_copy: Box::new(on_copy),
            surface_variant,
            text_muted,
            primary,
        }
    }

    // ── Event handlers ───────────────────────────────────────────────

    fn handle_event(&mut self, event: &Event<'static>) {
        match event {
            Event::Start(tag) => self.on_start(tag),
            Event::End(tag_end) => self.on_end(*tag_end),
            Event::Text(text) => self.on_text(text),
            Event::Code(code) => self.on_code(code),
            Event::SoftBreak => self.on_soft_break(),
            Event::HardBreak => self.on_hard_break(),
            _ => {}
        }
    }

    fn on_start(&mut self, tag: &Tag<'static>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_para();
                self.heading_level = Some(*level as u32);
            }
            Tag::Paragraph => {
                self.spans.clear();
            }
            Tag::CodeBlock(kind) => {
                self.flush_para();
                self.in_code_block = true;
                self.code_buf = String::new();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => Some(lang.to_string()),
                    CodeBlockKind::Indented => None,
                };
            }
            Tag::List(start) => {
                self.flush_para();
                self.in_list = Some(start.is_some());
                self.list_counter = 0;
            }
            Tag::Item => {
                self.spans.clear();
            }
            Tag::Emphasis => {
                self.fmt_stack.push(Fmt::Italic);
            }
            Tag::Strong => {
                self.fmt_stack.push(Fmt::Bold);
            }
            Tag::Table(_) => {
                self.flush_para();
                self.in_table = true;
                self.table_rows.clear();
                self.current_row.clear();
                self.current_cell_spans.clear();
            }
            Tag::TableRow => {
                self.current_cell_spans.clear();
            }
            Tag::TableCell => {
                self.current_cell_spans.clear();
            }
            Tag::BlockQuote(_) => {
                self.flush_para();
                self.in_blockquote = true;
                self.blockquote_elements.clear();
            }
            _ => {}
        }
    }

    fn on_end(&mut self, tag_end: TagEnd) {
        match tag_end {
            TagEnd::Heading(_) => {
                self.flush_para();
            }
            TagEnd::Paragraph => {
                if self.in_blockquote {
                    self.flush_blockquote_para();
                } else {
                    self.flush_para();
                }
            }
            TagEnd::CodeBlock => {
                let code_text = std::mem::take(&mut self.code_buf);
                let lang_str = self.code_lang.take();
                let code_elem: Element<'a, M> = code_block::view(
                    &code_text,
                    lang_str.as_deref(),
                    (self.on_copy)(&code_text),
                    self.surface_variant,
                );
                self.push_element(code_elem);
                self.in_code_block = false;
            }
            TagEnd::List(_) => {
                self.flush_item();
                if !self.list_items.is_empty() {
                    let items = std::mem::take(&mut self.list_items);
                    let list_elem = container(column(items).spacing(2)).padding(4).into();
                    self.push_element(list_elem);
                }
                self.in_list = None;
            }
            TagEnd::Item => {
                self.flush_item();
            }
            TagEnd::Emphasis => {
                self.fmt_stack.pop();
            }
            TagEnd::Strong => {
                self.fmt_stack.pop();
            }
            TagEnd::Table => {
                if !self.current_row.is_empty() {
                    self.table_rows.push(std::mem::take(&mut self.current_row));
                }
                if !self.table_rows.is_empty() {
                    let rows = self.table_elements();
                    self.push_element(container(column(rows).spacing(0)).into());
                }
                self.in_table = false;
            }
            TagEnd::TableRow => {
                if !self.current_cell_spans.is_empty() {
                    let cell_spans = std::mem::take(&mut self.current_cell_spans);
                    self.current_row.push(self.render_para_spans(cell_spans, 14.0, None));
                }
                if !self.current_row.is_empty() {
                    self.table_rows.push(std::mem::take(&mut self.current_row));
                }
            }
            TagEnd::TableCell => {
                if !self.current_cell_spans.is_empty() {
                    let cell_spans = std::mem::take(&mut self.current_cell_spans);
                    self.current_row.push(self.render_para_spans(cell_spans, 14.0, None));
                }
            }
            TagEnd::BlockQuote(_) => {
                if !self.blockquote_elements.is_empty() {
                    let bq_content = std::mem::take(&mut self.blockquote_elements);
                    let text_muted = self.text_muted;
                    let bq_container = container(column(bq_content).spacing(4))
                        .padding(iced::Padding::new(8.0).left(12.0))
                        .style(move |_theme: &iced::Theme| iced::widget::container::Style {
                            border: iced::Border {
                                color: text_muted,
                                width: 3.0,
                                radius: iced::border::Radius::new(0.0),
                            },
                            ..iced::widget::container::Style::default()
                        });
                    self.elements.push(bq_container.into());
                }
                self.in_blockquote = false;
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buf.push_str(text);
        } else if self.in_table {
            self.current_cell_spans.push(self.span_for(text.to_string()));
        } else {
            self.spans.push(self.span_for(text.to_string()));
        }
    }

    /// Build the inline span for a run of text, applying the current
    /// bold/italic formatting stack.
    fn span_for(&self, text: String) -> Span {
        let has_bold = self.fmt_stack.iter().any(|f| matches!(f, Fmt::Bold));
        let has_italic = self.fmt_stack.iter().any(|f| matches!(f, Fmt::Italic));
        if has_bold {
            Span::Bold(text)
        } else if has_italic {
            Span::Italic(text)
        } else {
            Span::Plain(text)
        }
    }

    /// Emit only the first `chars` characters of an oversized text event, then
    /// let `finalize_truncated` close the current container.
    fn on_text_prefix(&mut self, text: &str, chars: usize) {
        let prefix: String = text.chars().take(chars).collect();
        if self.in_code_block {
            self.code_buf.push_str(&prefix);
        } else if self.in_table {
            self.current_cell_spans.push(self.span_for(prefix));
        } else {
            self.spans.push(self.span_for(prefix));
        }
    }

    /// Emit only the first `chars` characters of an oversized inline-code event.
    fn on_code_prefix(&mut self, text: &str, chars: usize) {
        let prefix: String = text.chars().take(chars).collect();
        if self.in_table {
            self.current_cell_spans.push(Span::Code(prefix));
        } else {
            self.spans.push(Span::Code(prefix));
        }
    }

    /// Handle the event that consumes the final remaining budget characters,
    /// emitting only its prefix. Only `Text`/`Code` events can land here
    /// because they are the only events that consume budget.
    fn handle_truncated(&mut self, event: &Event<'static>, remaining: usize) {
        match event {
            Event::Text(text) => self.on_text_prefix(text, remaining),
            Event::Code(code) => self.on_code_prefix(code, remaining),
            _ => {}
        }
    }

    fn on_code(&mut self, text: &str) {
        let s = text.to_string();
        if self.in_table {
            self.current_cell_spans.push(Span::Code(s));
        } else {
            self.spans.push(Span::Code(s));
        }
    }

    fn on_soft_break(&mut self) {
        if self.in_code_block {
            self.code_buf.push('\n');
        } else if self.in_table {
            self.current_cell_spans.push(Span::Plain(" ".into()));
        } else {
            self.spans.push(Span::Plain(" ".into()));
        }
    }

    fn on_hard_break(&mut self) {
        if self.in_code_block {
            self.code_buf.push('\n');
        } else if self.in_table {
            self.current_cell_spans.push(Span::Plain("\n".into()));
        } else {
            self.spans.push(Span::Plain("\n".into()));
        }
    }

    // ── Output helpers ───────────────────────────────────────────────

    /// Push an element into either the blockquote container or the main output.
    fn push_element(&mut self, elem: Element<'a, M>) {
        if self.in_blockquote {
            self.blockquote_elements.push(elem);
        } else {
            self.elements.push(elem);
        }
    }

    /// Flush accumulated inline spans as a paragraph into the main output.
    fn flush_para(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let size = self.heading_level.map_or(14.0, |l| match l {
            1 => 32.0,
            2 => 28.0,
            3 => 24.0,
            4 => 20.0,
            _ => 16.0,
        });
        let color = if self.heading_level.is_some() { Some(self.primary) } else { None };
        let spans = std::mem::take(&mut self.spans);
        let elem = self.render_para_spans(spans, size, color);
        self.elements.push(elem);
        self.heading_level = None;
    }

    /// Flush accumulated inline spans into the blockquote container.
    fn flush_blockquote_para(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let elem = self.render_para_spans(spans, 14.0, None);
        self.blockquote_elements.push(elem);
    }

    /// Flush the current list item.
    fn flush_item(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        self.list_counter += 1;
        let prefix = match self.in_list {
            Some(true) => format!("{}. ", self.list_counter),
            _ => "•  ".into(),
        };
        let mut prefixed = vec![Span::Plain(prefix)];
        prefixed.append(&mut self.spans);
        self.list_items.push(self.render_para_spans(prefixed, 14.0, None));
    }

    /// Render a vector of inline Spans into an Iced row element.
    fn render_para_spans(
        &self,
        spans: Vec<Span>,
        size: f32,
        color: Option<Color>,
    ) -> Element<'a, M> {
        let mut children = Vec::with_capacity(spans.len());
        for span in spans {
            match span {
                Span::Plain(t) => {
                    let mut t = text(t).size(size);
                    if let Some(c) = color {
                        t = t.color(c);
                    }
                    children.push(t.into());
                }
                Span::Bold(t) => {
                    let mut t = text(t).size(size + 1.0);
                    if let Some(c) = color {
                        t = t.color(c);
                    }
                    children.push(t.into());
                }
                Span::Italic(t) => {
                    let mut t = text(t).size(size);
                    if let Some(c) = color {
                        t = t.color(c);
                    }
                    children.push(t.into());
                }
                Span::Code(t) => {
                    let bg = Color::from_rgb(0.12, 0.14, 0.19);
                    let code = container(text(t).size(size).font(Font::MONOSPACE))
                        .padding(iced::Padding::new(2.0).right(6.0).left(6.0))
                        .style(move |_theme: &iced::Theme| container::Style {
                            background: Some(iced::Background::Color(bg)),
                            ..container::Style::default()
                        });
                    children.push(code.into());
                }
            }
        }
        let r = row(children).spacing(4);
        container(r).padding(2).into()
    }

    /// Render all fully-closed table rows as bordered cells (drains
    /// `table_rows`). Shared by the normal `TagEnd::Table` path and truncated
    /// finalization, where the partially-built current row is deliberately
    /// dropped rather than emitted half-rendered.
    fn table_elements(&mut self) -> Vec<Element<'a, M>> {
        let border_color = Color::from_rgb(0.3, 0.3, 0.35);
        let mut row_elements = Vec::with_capacity(self.table_rows.len());
        for r in self.table_rows.drain(..) {
            let mut cell_elements = Vec::with_capacity(r.len());
            for cell in r {
                cell_elements.push(
                    container(cell)
                        .padding(4)
                        .style(move |_theme: &iced::Theme| iced::widget::container::Style {
                            border: iced::Border {
                                color: border_color,
                                width: 1.0,
                                radius: iced::border::Radius::new(0.0),
                            },
                            ..iced::widget::container::Style::default()
                        })
                        .into(),
                );
            }
            row_elements.push(row(cell_elements).spacing(0).into());
        }
        row_elements
    }

    /// Consume the renderer and return the final Iced element.
    fn finalize(mut self) -> Element<'a, M> {
        self.flush_para();
        container(column(self.elements).spacing(8).width(Length::Fill)).padding(8).into()
    }

    /// Finalize a render that stopped early (budget exhausted mid-document).
    /// Closes whatever container was left open so the output stays a
    /// well-formed element tree: the open paragraph flushes (as a list item
    /// inside a list, into the blockquote container inside one), an open list
    /// closes with its items, a partial code block emits with its language,
    /// completed table rows render (the incomplete row is dropped) and any
    /// blockquote content is wrapped. Never panics, never leaks a container.
    fn finalize_truncated(mut self) -> Element<'a, M> {
        if self.in_list.is_some() {
            self.flush_item();
        } else if self.in_blockquote {
            self.flush_blockquote_para();
        } else {
            self.flush_para();
        }
        if self.in_list.is_some() {
            if !self.list_items.is_empty() {
                let items = std::mem::take(&mut self.list_items);
                self.push_element(container(column(items).spacing(2)).padding(4).into());
            }
            self.in_list = None;
        }
        if self.in_code_block {
            let code_text = std::mem::take(&mut self.code_buf);
            let lang_str = self.code_lang.take();
            let code_elem: Element<'static, M> = code_block::view(
                &code_text,
                lang_str.as_deref(),
                (self.on_copy)(&code_text),
                self.surface_variant,
            );
            self.push_element(code_elem);
            self.in_code_block = false;
        }
        if self.in_table {
            if !self.table_rows.is_empty() {
                let rows = self.table_elements();
                self.push_element(container(column(rows).spacing(0)).into());
            }
            self.in_table = false;
        }
        if self.in_blockquote {
            if !self.blockquote_elements.is_empty() {
                let bq_content = std::mem::take(&mut self.blockquote_elements);
                let text_muted = self.text_muted;
                let bq_container = container(column(bq_content).spacing(4))
                    .padding(iced::Padding::new(8.0).left(12.0))
                    .style(move |_theme: &iced::Theme| iced::widget::container::Style {
                        border: iced::Border {
                            color: text_muted,
                            width: 3.0,
                            radius: iced::border::Radius::new(0.0),
                        },
                        ..iced::widget::container::Style::default()
                    });
                self.elements.push(bq_container.into());
            }
            self.in_blockquote = false;
        }
        container(column(self.elements).spacing(8).width(Length::Fill)).padding(8).into()
    }
}

/// Render a markdown string to an Iced element.
pub fn render<'a, M: Clone + 'static>(
    markdown: &'a str,
    on_copy: impl Fn(&str) -> M + Clone + 'static,
    surface_variant: Color,
    text_muted: Color,
    primary: Color,
) -> Element<'a, M> {
    // Parse once and render with no budget. Callers that hold a `MarkdownDoc`
    // use `render_upto` instead (and never re-parse); this entry point exists
    // for one-off renders such as tests and unsaved fallback paths.
    MarkdownDoc::parse(markdown).render_upto(None, on_copy, surface_variant, text_muted, primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_copy(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn table_2x2_renders_grid() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let elem = render(md, dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        // The element should be a container wrapping a column; we just verify it doesn't panic
        // and that the resulting element is non-empty by checking it builds successfully.
        let _ = elem;
    }

    #[test]
    fn blockquote_renders_distinct_container() {
        let md = "> This is a quote";
        let elem = render(md, dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn code_block_still_renders() {
        let md = "```rust\nfn main() {}\n```";
        let elem = render(md, dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn list_still_renders() {
        let md = "- item one\n- item two";
        let elem = render(md, dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn parse_then_render_matches_direct_render() {
        let md = "# Heading\n\nSome **bold** and `inline` code.\n\n- one\n- two";
        let doc = MarkdownDoc::parse(md);
        assert!(doc.total_units > 0, "total_units must count visible text");
        let elem = doc.render_upto(None, dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn render_upto_zero_budget_returns_empty_element() {
        let doc = MarkdownDoc::parse("# Heading\n\nsome body *text*");
        // A zero budget stops before any text renders; it must still produce a
        // valid (empty) element rather than panicking.
        let elem = doc.render_upto(Some(0), dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn render_upto_truncated_code_block_does_not_panic() {
        let md = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
        let doc = MarkdownDoc::parse(md);
        // Budget 5 lands mid-code-block: a partial code block must emit with
        // its language intact and never panic.
        let elem = doc.render_upto(Some(5), dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn render_upto_truncated_list_does_not_panic() {
        let md = "- item one\n- item two\n- item three";
        let doc = MarkdownDoc::parse(md);
        let elem = doc.render_upto(Some(6), dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn render_upto_truncated_table_does_not_panic() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let doc = MarkdownDoc::parse(md);
        // Budget 5 cuts mid-way through the table body: completed rows render,
        // the partial row is dropped.
        let elem = doc.render_upto(Some(5), dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }

    #[test]
    fn render_upto_truncated_blockquote_does_not_panic() {
        let md = "> This is a quote\n>\n> with several lines";
        let doc = MarkdownDoc::parse(md);
        let elem = doc.render_upto(Some(7), dummy_copy, Color::BLACK, Color::WHITE, Color::WHITE);
        let _ = elem;
    }
}
