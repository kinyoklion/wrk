//! Drives pulldown-cmark over the source and builds styled ratatui lines.
//!
//! The renderer is a small event-driven state machine: inline emphasis is
//! tracked with nesting counters, block nesting (lists, quotes) with stacks,
//! and tables / code blocks are buffered and emitted on their closing tag.
//! Output lines are left *unwrapped* — the view widget wraps to the viewport.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::RenderOptions;
use crate::diagram::is_diagram_lang;
use crate::theme::MdTheme;

pub(crate) fn render(source: &str, opts: &RenderOptions) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(source, options);
    let mut r = Renderer::new(opts);
    for event in parser {
        r.handle(event);
    }
    r.finish()
}

struct ListCtx {
    /// `Some(n)` = ordered list, next ordinal `n`; `None` = bullet list.
    ordinal: Option<u64>,
}

#[derive(Default)]
struct TableBuilder {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cur_cell: String,
    head_rows: usize,
}

struct Renderer<'o> {
    theme: MdTheme,
    highlight: bool,
    opts: &'o RenderOptions,

    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,

    // Inline emphasis nesting counters.
    bold: u32,
    italic: u32,
    strike: u32,
    link_depth: u32,

    // Block nesting.
    lists: Vec<ListCtx>,
    quote_depth: usize,
    heading: Option<usize>,

    // Buffered constructs.
    in_code: bool,
    code_lang: String,
    code_buf: String,
    in_image: bool,
    image_alt: String,
    image_dest: String,
    table: Option<TableBuilder>,

    /// Whether a blank separator line is owed before the next top-level block.
    need_gap: bool,
}

impl<'o> Renderer<'o> {
    fn new(opts: &'o RenderOptions) -> Self {
        Self {
            theme: opts.theme,
            highlight: opts.highlight,
            opts,
            lines: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            link_depth: 0,
            lists: Vec::new(),
            quote_depth: 0,
            heading: None,
            in_code: false,
            code_lang: String::new(),
            code_buf: String::new(),
            in_image: false,
            image_alt: String::new(),
            image_dest: String::new(),
            table: None,
            need_gap: false,
        }
    }

    fn finish(mut self) -> Text<'static> {
        self.flush_line();
        Text::from(self.lines)
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.inline_code(&t),
            Event::Html(t) | Event::InlineHtml(t) => self.push_span(
                t.trim_end_matches('\n').to_string(),
                Style::default().fg(self.theme.faint),
            ),
            Event::SoftBreak => self.push_span(" ".to_string(), self.inline_style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.gap();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(24),
                    Style::default().fg(self.theme.rule),
                )));
                self.need_gap = true;
            }
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.push_span(mark.to_string(), Style::default().fg(self.theme.marker));
            }
            Event::FootnoteReference(name) => {
                self.push_span(format!("[^{name}]"), Style::default().fg(self.theme.link));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.gap(),
            Tag::Heading { level, .. } => {
                self.gap();
                self.heading = Some(heading_num(level));
                let prefix = "#".repeat(heading_num(level));
                self.push_span(
                    format!("{prefix} "),
                    Style::default()
                        .fg(self.theme.heading)
                        .add_modifier(Modifier::BOLD),
                );
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.gap();
                self.in_code = true;
                self.code_buf.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.gap();
                }
                self.lists.push(ListCtx { ordinal: start });
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.lists.len().max(1);
                let indent = "  ".repeat(depth - 1);
                self.push_span(indent, Style::default());
                let marker = match self.lists.last_mut() {
                    Some(ListCtx {
                        ordinal: Some(n), ..
                    }) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.push_span(marker, Style::default().fg(self.theme.marker));
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link_depth += 1,
            Tag::Image { dest_url, .. } => {
                self.in_image = true;
                self.image_alt.clear();
                self.image_dest = dest_url.into_string();
            }
            Tag::Table(aligns) => {
                self.gap();
                self.table = Some(TableBuilder {
                    aligns,
                    ..Default::default()
                });
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cur_cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.need_gap = true;
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.heading = None;
                self.need_gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.need_gap = true;
            }
            TagEnd::CodeBlock => {
                self.emit_code_block();
                self.in_code = false;
                self.need_gap = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.need_gap = true;
                }
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link_depth = self.link_depth.saturating_sub(1),
            TagEnd::Image => {
                self.in_image = false;
                let alt = std::mem::take(&mut self.image_alt);
                let dest = std::mem::take(&mut self.image_dest);
                let label = if alt.is_empty() {
                    format!("🖼 {dest}")
                } else {
                    format!("🖼 {alt} ({dest})")
                };
                self.push_span(label, Style::default().fg(self.theme.faint));
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = t.cur_cell.trim().to_string();
                    t.cur_row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(std::mem::take(&mut t.cur_row));
                    t.head_rows = 1;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(std::mem::take(&mut t.cur_row));
                }
            }
            TagEnd::Table => {
                self.emit_table();
                self.need_gap = true;
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if self.in_code {
            self.code_buf.push_str(t);
        } else if self.in_image {
            self.image_alt.push_str(t);
        } else if let Some(tb) = self.table.as_mut() {
            tb.cur_cell.push_str(t);
        } else {
            let style = self.inline_style();
            self.push_span(t.to_string(), style);
        }
    }

    fn inline_code(&mut self, t: &str) {
        if let Some(tb) = self.table.as_mut() {
            tb.cur_cell.push_str(t);
            return;
        }
        let mut style = Style::default().fg(self.theme.code);
        if let Some(bg) = self.theme.code_bg {
            style = style.bg(bg);
        }
        self.push_span(t.to_string(), style);
    }

    /// Style for ordinary inline text given the current emphasis/link/heading
    /// context.
    fn inline_style(&self) -> Style {
        let mut m = Modifier::empty();
        if self.bold > 0 {
            m |= Modifier::BOLD;
        }
        if self.italic > 0 {
            m |= Modifier::ITALIC;
        }
        if self.strike > 0 {
            m |= Modifier::CROSSED_OUT;
        }
        let mut s = Style::default().add_modifier(m);
        if self.link_depth > 0 {
            s = s.fg(self.theme.link).add_modifier(Modifier::UNDERLINED);
        } else if self.heading.is_some() {
            s = s.fg(self.theme.heading).add_modifier(Modifier::BOLD);
        }
        s
    }

    fn push_span(&mut self, content: String, style: Style) {
        if content.is_empty() {
            return;
        }
        self.cur.push(Span::styled(content, style));
    }

    /// Push the in-progress spans as a line, prefixing the block-quote gutter
    /// when inside a quote. No-op when there's nothing buffered.
    fn flush_line(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.cur.len() + 1);
        if self.quote_depth > 0 {
            spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                Style::default().fg(self.theme.quote),
            ));
        }
        spans.append(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    /// Emit a blank separator before a new top-level block, if one is owed and
    /// we're not at the very top of the document.
    fn gap(&mut self) {
        if self.need_gap && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.need_gap = false;
    }

    fn emit_code_block(&mut self) {
        let source = std::mem::take(&mut self.code_buf);
        let lang = std::mem::take(&mut self.code_lang);
        let source = source.strip_suffix('\n').unwrap_or(&source).to_string();

        if is_diagram_lang(&lang) {
            let rendered = self.opts.diagram.render(&lang, &source, &self.theme);
            self.lines.extend(rendered);
            return;
        }

        let highlighted = if self.highlight {
            highlight_code(&source, &lang)
        } else {
            None
        };
        match highlighted {
            Some(lines) => self.lines.extend(lines),
            None => {
                for raw in source.lines() {
                    self.lines.push(Line::from(Span::styled(
                        raw.to_string(),
                        Style::default().fg(self.theme.code),
                    )));
                }
            }
        }
    }

    fn emit_table(&mut self) {
        let Some(t) = self.table.take() else {
            return;
        };
        if t.rows.is_empty() {
            return;
        }
        let cols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        for (ri, row) in t.rows.iter().enumerate() {
            let mut rendered = String::new();
            for (i, &col_w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let pad = col_w.saturating_sub(cell.chars().count());
                let align = t.aligns.get(i).copied().unwrap_or(Alignment::None);
                rendered.push_str("│ ");
                match align {
                    Alignment::Right => {
                        rendered.push_str(&" ".repeat(pad));
                        rendered.push_str(cell);
                    }
                    Alignment::Center => {
                        let left = pad / 2;
                        rendered.push_str(&" ".repeat(left));
                        rendered.push_str(cell);
                        rendered.push_str(&" ".repeat(pad - left));
                    }
                    _ => {
                        rendered.push_str(cell);
                        rendered.push_str(&" ".repeat(pad));
                    }
                }
                rendered.push(' ');
            }
            rendered.push('│');
            let style = if ri < t.head_rows {
                Style::default()
                    .fg(self.theme.heading)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            self.lines.push(Line::from(Span::styled(rendered, style)));

            // Separator under the header row.
            if ri + 1 == t.head_rows {
                let mut sep = String::new();
                for w in &widths {
                    sep.push_str("├─");
                    sep.push_str(&"─".repeat(*w));
                    sep.push('─');
                }
                sep.push('┤');
                self.lines.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(self.theme.rule),
                )));
            }
        }
    }
}

#[cfg(feature = "highlight")]
fn highlight_code(source: &str, lang: &str) -> Option<Vec<Line<'static>>> {
    crate::highlight::highlight_block(source, lang)
}

#[cfg(not(feature = "highlight"))]
fn highlight_code(_source: &str, _lang: &str) -> Option<Vec<Line<'static>>> {
    None
}

fn heading_num(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render_default(src: &str) -> Text<'static> {
        // Highlighting off keeps code-block assertions deterministic.
        render(src, &RenderOptions::new(false))
    }

    #[cfg(feature = "highlight")]
    #[test]
    fn rust_code_block_is_highlighted() {
        let text = render("```rust\nlet x = 1;\n```", &RenderOptions::new(true));
        // syntect splits the line into several colored spans, vs the single
        // dim span produced by the plain fallback.
        let line = &text.lines[0];
        assert!(
            line.spans.len() > 1,
            "expected highlighted spans, got {line:?}"
        );
        assert!(line.spans.iter().any(|s| s.style.fg.is_some()));
    }

    #[test]
    fn heading_gets_prefix_and_style() {
        let text = render_default("# Title");
        let lines = plain(&text);
        assert_eq!(lines, vec!["# Title".to_string()]);
        // The heading span carries the heading color + bold.
        let span = &text.lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bullet_list_markers() {
        let text = render_default("- one\n- two");
        let lines = plain(&text);
        assert_eq!(lines, vec!["• one".to_string(), "• two".to_string()]);
    }

    #[test]
    fn ordered_list_counts() {
        let text = render_default("1. a\n2. b\n3. c");
        let lines = plain(&text);
        assert_eq!(
            lines,
            vec!["1. a".to_string(), "2. b".to_string(), "3. c".to_string()]
        );
    }

    #[test]
    fn blockquote_gets_gutter() {
        let text = render_default("> quoted");
        let lines = plain(&text);
        assert_eq!(lines, vec!["│ quoted".to_string()]);
    }

    #[test]
    fn code_block_plain_when_highlight_off() {
        let text = render_default("```rust\nlet x = 1;\n```");
        let lines = plain(&text);
        assert_eq!(lines, vec!["let x = 1;".to_string()]);
    }

    #[test]
    fn mermaid_routes_to_null_backend() {
        let text = render_default("```mermaid\ngraph TD\nA-->B\n```");
        let lines = plain(&text);
        assert_eq!(lines[0], "[mermaid diagram — preview not enabled]");
        assert!(lines.iter().any(|l| l == "graph TD"));
    }

    #[test]
    fn table_renders_aligned_rows() {
        let text = render_default("| a | b |\n|---|---|\n| 1 | 2 |");
        let lines = plain(&text);
        // Header, separator, one body row.
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("a") && lines[0].contains("b"));
        assert!(lines[2].contains('1') && lines[2].contains('2'));
    }

    #[test]
    fn emphasis_sets_modifiers() {
        let text = render("**bold** and *italic*", &RenderOptions::new(false));
        let bold = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "italic")
            .unwrap();
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }
}
