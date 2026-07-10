//! Drives pulldown-cmark over the source and builds styled ratatui lines.
//!
//! The renderer is a small event-driven state machine: inline emphasis is
//! tracked with nesting counters, block nesting (lists, quotes) with stacks,
//! and tables / code blocks are buffered and emitted on their closing tag.
//! Output lines are left *unwrapped* — the view widget wraps to the viewport.

use std::path::PathBuf;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::RenderOptions;
use crate::block::{ImageRef, ImageSource, MdBlock, RenderedDoc};
use crate::diagram::{DiagramOutput, is_diagram_lang};
use crate::theme::MdTheme;

pub(crate) fn render_blocks(source: &str, width: usize, opts: &RenderOptions) -> RenderedDoc {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(source, options);
    let mut r = Renderer::new(width, opts);
    for event in parser {
        r.handle(event);
    }
    r.finish()
}

/// Extensions we can rasterize; anything else stays a placeholder link.
const RENDERABLE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg"];

/// Resolve an image link to a local file [`ImageSource`], or `None` if it is a
/// remote/data URL or lacks a renderable extension (those keep their inline
/// placeholder text). Relative paths resolve against `base_dir`.
fn resolve_image(dest: &str, base_dir: Option<&std::path::Path>) -> Option<ImageSource> {
    // Skip URLs with a scheme (http:, https:, data:, …); `file:` is stripped.
    let dest = dest.strip_prefix("file://").unwrap_or(dest);
    if dest.contains("://") || dest.starts_with("data:") {
        return None;
    }
    let path = PathBuf::from(dest);
    let ext_ok = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RENDERABLE_EXTS.iter().any(|r| e.eq_ignore_ascii_case(r)));
    if !ext_ok {
        return None;
    }
    let resolved = if path.is_absolute() {
        path
    } else {
        base_dir?.join(path)
    };
    Some(ImageSource::Path(resolved))
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
    /// Display width in cells; used to lay out tables.
    width: usize,
    opts: &'o RenderOptions,

    /// Finished blocks. Text accumulates in `lines`/`cur` and is flushed into a
    /// `MdBlock::Text` whenever an image splits the flow (and at the end).
    blocks: Vec<MdBlock>,
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
    /// Plain heading text (no `#` prefix), accumulated while inside a heading so
    /// it can be rendered as a true-size image.
    heading_text: String,

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
    fn new(width: usize, opts: &'o RenderOptions) -> Self {
        Self {
            theme: opts.theme,
            highlight: opts.highlight,
            width,
            opts,
            blocks: Vec::new(),
            lines: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            link_depth: 0,
            lists: Vec::new(),
            quote_depth: 0,
            heading: None,
            heading_text: String::new(),
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

    fn finish(mut self) -> RenderedDoc {
        self.flush_text_block();
        RenderedDoc {
            blocks: self.blocks,
        }
    }

    /// Push the accumulated text (if any) as a `MdBlock::Text`, ending the
    /// current run so an image block can follow.
    fn flush_text_block(&mut self) {
        self.flush_line();
        if !self.lines.is_empty() {
            let lines = std::mem::take(&mut self.lines);
            self.blocks.push(MdBlock::Text(Text::from(lines)));
        }
    }

    /// Build a true-size heading image block for the just-closed heading, or
    /// `None` to keep it as styled text (H4–H6, images off, or the SVG couldn't
    /// be built). Consumes the buffered heading spans as the fallback line.
    #[cfg(feature = "images")]
    fn heading_image_block(&mut self, level: usize) -> Option<MdBlock> {
        if !self.opts.heading_images {
            return None;
        }
        let (cw, ch) = self.opts.cell_size;
        let svg =
            crate::heading::heading_svg(&self.heading_text, level, self.theme.heading, cw, ch)?;
        // The styled `# Text` line is the fallback shown when the image can't be
        // drawn (no graphics protocol, or rasterization fails).
        let placeholder = self.cur_to_line()?;
        let alt = std::mem::take(&mut self.heading_text);
        Some(MdBlock::Image(ImageRef {
            alt,
            source: ImageSource::Svg(svg),
            placeholder,
        }))
    }

    #[cfg(not(feature = "images"))]
    fn heading_image_block(&mut self, _level: usize) -> Option<MdBlock> {
        None
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
                self.heading_text.clear();
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
                let level = self.heading.take().unwrap_or(1);
                match self.heading_image_block(level) {
                    // H1–H3 with images on: emit a true-size heading image,
                    // splitting the text flow like any image block.
                    Some(block) => {
                        self.flush_text_block();
                        self.blocks.push(block);
                    }
                    // H4–H6, images off, or no graphics font: styled text.
                    None => self.flush_line(),
                }
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
                let placeholder =
                    Line::from(Span::styled(label, Style::default().fg(self.theme.faint)));
                match resolve_image(&dest, self.opts.base_dir.as_deref()) {
                    // A renderable local file: break the text flow and emit an
                    // image block (the view rasterizes it, or shows the
                    // placeholder if that fails / images are off).
                    Some(source) => {
                        self.flush_text_block();
                        self.blocks.push(MdBlock::Image(ImageRef {
                            alt,
                            source,
                            placeholder,
                        }));
                    }
                    // Remote/data/unsupported: keep the inline placeholder text.
                    None => self.cur.extend(placeholder.spans),
                }
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
            if self.heading.is_some() {
                self.heading_text.push_str(t);
            }
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

    /// Take the in-progress spans as a line, prefixing the block-quote gutter
    /// when inside a quote. `None` (leaving `cur` untouched) when nothing is
    /// buffered.
    fn cur_to_line(&mut self) -> Option<Line<'static>> {
        if self.cur.is_empty() {
            return None;
        }
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.cur.len() + 1);
        if self.quote_depth > 0 {
            spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                Style::default().fg(self.theme.quote),
            ));
        }
        spans.append(&mut self.cur);
        Some(Line::from(spans))
    }

    /// Push the in-progress spans as a line. No-op when there's nothing buffered.
    fn flush_line(&mut self) {
        if let Some(line) = self.cur_to_line() {
            self.lines.push(line);
        }
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
            match self
                .opts
                .diagram
                .render(&lang, &source, &self.theme, self.opts.diagram_ctx)
            {
                DiagramOutput::Lines(rendered) => self.lines.extend(rendered),
                // A backend that rasterizes the diagram (e.g. mermaid → SVG):
                // break the text flow and emit an image block, exactly like a
                // linked `.svg`. The placeholder shows if graphics are absent.
                DiagramOutput::Image(source) => {
                    let placeholder = Line::from(Span::styled(
                        format!("[{lang} diagram]"),
                        Style::default().fg(self.theme.faint),
                    ));
                    self.flush_text_block();
                    self.blocks.push(MdBlock::Image(ImageRef {
                        alt: lang,
                        source,
                        placeholder,
                    }));
                }
            }
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
        if cols == 0 {
            return;
        }

        // Natural (unconstrained) width per column, then fit to the display
        // width so wide tables wrap within their columns instead of overflowing.
        let mut natural = vec![1usize; cols];
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                natural[i] = natural[i].max(cell.chars().count().max(1));
            }
        }
        let widths = fit_columns(&natural, self.width);

        let rule = Style::default().fg(self.theme.rule);
        self.lines.push(border_line(&widths, '┌', '┬', '┐', rule));

        let with_bg = |style: Style, bg: Option<Color>| match bg {
            Some(c) => style.bg(c),
            None => style,
        };

        let last_ri = t.rows.len().saturating_sub(1);
        let mut body_idx = 0usize;
        for (ri, row) in t.rows.iter().enumerate() {
            let is_header = ri < t.head_rows;
            // Zebra-stripe body rows so adjacent rows read apart. The stripe is
            // applied to the border/padding spans too, for a continuous band.
            let row_bg = if is_header {
                None
            } else if body_idx % 2 == 0 {
                self.theme.table_row_bg
            } else {
                self.theme.table_row_alt_bg
            };

            // Each cell is wrapped to its column width; the row is as tall as
            // its tallest cell.
            let wrapped: Vec<Vec<String>> = (0..cols)
                .map(|i| wrap_text(row.get(i).map(String::as_str).unwrap_or(""), widths[i]))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let base_cell = if is_header {
                Style::default()
                    .fg(self.theme.heading)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let cell_style = with_bg(base_cell, row_bg);
            let border_style = with_bg(rule, row_bg);

            for k in 0..height {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols * 3 + 1);
                for (i, &w) in widths.iter().enumerate() {
                    let cell = wrapped[i].get(k).map(String::as_str).unwrap_or("");
                    let align = t.aligns.get(i).copied().unwrap_or(Alignment::None);
                    spans.push(Span::styled("│ ".to_string(), border_style));
                    spans.push(Span::styled(pad_align(cell, w, align), cell_style));
                    spans.push(Span::styled(" ".to_string(), border_style));
                }
                spans.push(Span::styled("│".to_string(), border_style));
                self.lines.push(Line::from(spans));
            }

            if is_header {
                // Heavier divider under the header row.
                if ri + 1 == t.head_rows {
                    self.lines.push(border_line(&widths, '├', '┼', '┤', rule));
                }
            } else {
                body_idx += 1;
                // Thin line between adjacent body rows; the bottom border
                // closes the last one.
                if ri != last_ri {
                    self.lines.push(border_line(&widths, '├', '┼', '┤', rule));
                }
            }
        }

        self.lines.push(border_line(&widths, '└', '┴', '┘', rule));
    }
}

/// Choose per-column widths so the whole table fits in `width` cells. Each
/// column costs its width plus 3 (`"│ "` + trailing space); the table adds a
/// final `"│"`.
///
/// When the natural widths don't fit, the *widest* column is shrunk one cell at
/// a time until they do. This keeps narrow columns at their natural width and
/// only wraps the genuinely-long ones, rather than squeezing every column
/// proportionally (which mangles short headers next to one big column).
fn fit_columns(natural: &[usize], width: usize) -> Vec<usize> {
    let cols = natural.len();
    let overhead = 3 * cols + 1;
    let budget = width.saturating_sub(overhead).max(cols);

    let mut widths = natural.to_vec();
    let mut sum: usize = widths.iter().sum();
    while sum > budget {
        let idx = widest_index(&widths);
        if widths[idx] <= 1 {
            break; // every column is already at the 1-cell minimum
        }
        widths[idx] -= 1;
        sum -= 1;
    }
    widths
}

/// Index of the widest column (ties resolve to the leftmost).
fn widest_index(widths: &[usize]) -> usize {
    let mut best = 0;
    for (i, &w) in widths.iter().enumerate() {
        if w > widths[best] {
            best = i;
        }
    }
    best
}

/// Pad/truncate `cell` to exactly `w` chars with the given alignment.
fn pad_align(cell: &str, w: usize, align: Alignment) -> String {
    let len = cell.chars().count();
    if len >= w {
        return cell.chars().take(w).collect();
    }
    let pad = w - len;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), cell),
        Alignment::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), cell, " ".repeat(pad - left))
        }
        _ => format!("{}{}", cell, " ".repeat(pad)),
    }
}

/// A horizontal table border/junction line for the given column widths.
fn border_line(
    widths: &[usize],
    left: char,
    mid: char,
    right: char,
    style: Style,
) -> Line<'static> {
    let mut s = String::new();
    s.push(left);
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        // Column span = 1 leading + w + 1 trailing space.
        s.push_str(&"─".repeat(w + 2));
    }
    s.push(right);
    Line::from(Span::styled(s, style))
}

/// Word-wrap `s` to `w` columns, hard-breaking any word longer than `w`.
/// Always returns at least one (possibly empty) line.
fn wrap_text(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in s.split(' ') {
        let wl = word.chars().count();
        if wl > w {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                let chunk: String = chars.by_ref().take(w).collect();
                if chunk.chars().count() == w {
                    lines.push(chunk);
                } else {
                    cur_w = chunk.chars().count();
                    cur = chunk;
                }
            }
            continue;
        }
        let need = if cur.is_empty() { wl } else { cur_w + 1 + wl };
        if need > w {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
            cur_w = wl;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += wl;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
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

    /// Flatten to `Text` (images → placeholder) for the assertions below.
    fn render(src: &str, width: usize, opts: &RenderOptions) -> Text<'static> {
        render_blocks(src, width, opts).into_text()
    }

    fn render_default(src: &str) -> Text<'static> {
        // Highlighting off keeps code-block assertions deterministic.
        render(src, 80, &RenderOptions::new(false))
    }

    #[cfg(feature = "highlight")]
    #[test]
    fn rust_code_block_is_highlighted() {
        let text = render("```rust\nlet x = 1;\n```", 80, &RenderOptions::new(true));
        // syntect splits the line into several colored spans, vs the single
        // dim span produced by the plain fallback.
        let line = &text.lines[0];
        assert!(
            line.spans.len() > 1,
            "expected highlighted spans, got {line:?}"
        );
        assert!(line.spans.iter().any(|s| s.style.fg.is_some()));
    }

    #[cfg(feature = "images")]
    #[test]
    fn h1_to_h3_become_image_blocks_h4_stays_text() {
        // Images default-on: H1 is a true-size image; the body stays text.
        let doc = render_blocks("# Big Title\n\nbody text", 80, &RenderOptions::new(false));
        let imgs: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Image(img) => Some(img),
                _ => None,
            })
            .collect();
        assert_eq!(imgs.len(), 1, "H1 should be one image block");
        assert_eq!(imgs[0].alt, "Big Title", "alt is the clean heading text");
        assert!(matches!(imgs[0].source, ImageSource::Svg(_)));
        // The fallback line keeps the `#` prefix so non-graphics viewers still
        // read it as a heading.
        assert!(crate::to_plain_string(&doc.into_text()).contains("# Big Title"));

        // H4 is below the imaged range → plain styled text, no image.
        let h4 = render_blocks("#### Small", 80, &RenderOptions::new(false));
        assert!(!h4.has_images(), "H4 should stay text");
    }

    #[cfg(feature = "images")]
    #[test]
    fn heading_images_can_be_disabled() {
        let opts = RenderOptions::new(false).with_heading_images(false);
        let doc = render_blocks("# Title", 80, &opts);
        assert!(!doc.has_images(), "disabled → heading stays text");
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

    #[cfg(not(feature = "mermaid"))]
    #[test]
    fn mermaid_routes_to_null_backend() {
        let text = render_default("```mermaid\ngraph TD\nA-->B\n```");
        let lines = plain(&text);
        assert_eq!(lines[0], "[mermaid diagram — preview not enabled]");
        assert!(lines.iter().any(|l| l == "graph TD"));
    }

    #[cfg(feature = "mermaid")]
    #[test]
    fn mermaid_becomes_svg_image_block() {
        // With the carcimaid backend the fence renders to an SVG image block
        // that splits the surrounding prose, rather than a source dump.
        let doc = render_blocks(
            "before\n\n```mermaid\ngraph TD\nA-->B\n```\n\nafter",
            80,
            &RenderOptions::new(false),
        );
        let images: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Image(img) => Some(img),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1, "the mermaid fence should be one image");
        match &images[0].source {
            ImageSource::Svg(svg) => {
                assert!(svg.contains("<svg"), "expected SVG markup, got {svg:.80}");
                // We request a transparent background so the diagram picks up
                // the terminal color — carcimaid's default white box must be gone.
                assert!(
                    !svg.contains("background-color: white"),
                    "diagram SVG should have a transparent background, not white"
                );
            }
            other => panic!("expected an inline SVG source, got {other:?}"),
        }
        // Surrounding prose survives as text blocks around the image.
        let text: String = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Text(t) => Some(crate::to_plain_string(t)),
                _ => None,
            })
            .collect();
        assert!(text.contains("before") && text.contains("after"));
    }

    #[cfg(feature = "mermaid")]
    #[test]
    fn malformed_mermaid_falls_back_to_source_dump() {
        // A fence carcimaid can't parse must degrade to readable source text,
        // not vanish or become an image.
        let doc = render_blocks(
            "```mermaid\nnot a real diagram %%%\n```",
            80,
            &RenderOptions::new(false),
        );
        assert!(!doc.has_images(), "a parse failure must not yield an image");
        let text = crate::to_plain_string(&doc.into_text());
        assert!(
            text.contains("render failed"),
            "expected a fallback hint, got: {text}"
        );
        assert!(
            text.contains("not a real diagram"),
            "source should be shown"
        );
    }

    #[test]
    fn table_renders_boxed_rows() {
        let text = render_default("| a | b |\n|---|---|\n| 1 | 2 |");
        let lines = plain(&text);
        // Top border, header, header separator, body row, bottom border.
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[1].contains('a') && lines[1].contains('b'));
        assert!(lines[2].starts_with('├'));
        assert!(lines[3].contains('1') && lines[3].contains('2'));
        assert!(lines[4].starts_with('└') && lines[4].ends_with('┘'));
        // Every rendered row is the same width and fits the display width.
        let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
        assert!(widths.iter().all(|&w| w == widths[0] && w <= 80));
    }

    #[test]
    fn table_body_rows_zebra_stripe_and_separate() {
        // Two body rows: expect top border, header, header rule, row 1,
        // inter-row separator, row 2, bottom border — 7 lines.
        let text = render_default("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");
        let lines = plain(&text);
        assert_eq!(lines.len(), 7, "{lines:?}");
        assert!(lines[3].contains('1'));
        assert!(lines[4].starts_with('├'), "separator between body rows");
        assert!(lines[5].contains('3'));

        // The default theme leaves even rows bare and stripes odd rows, so the
        // second body row's cell carries the alt background.
        let alt = MdTheme::default().table_row_alt_bg;
        assert!(alt.is_some());
        let row2 = &text.lines[5];
        assert!(
            row2.spans.iter().all(|s| s.style.bg == alt),
            "odd body row (border + cells) should carry the zebra bg: {row2:?}"
        );
        let row1 = &text.lines[3];
        assert!(
            row1.spans.iter().all(|s| s.style.bg.is_none()),
            "even body row should inherit the surface: {row1:?}"
        );
    }

    #[test]
    fn wide_table_wraps_within_columns_and_fits_width() {
        // A cell far wider than the display width must wrap, not overflow.
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let src = format!("| word | text |\n|---|---|\n| x | {long} |");
        let text = render(&src, 30, &RenderOptions::new(false));
        let lines = plain(&text);
        assert!(
            lines.iter().all(|l| l.chars().count() <= 30),
            "a line exceeded the 30-col width: {lines:?}"
        );
        // The long cell spans multiple physical rows (wrapped), so the table is
        // taller than the un-wrapped 5 lines.
        assert!(
            lines.len() > 5,
            "expected wrapped rows, got {}",
            lines.len()
        );
    }

    #[test]
    fn local_image_becomes_image_block_split_from_text() {
        let opts = RenderOptions::new(false).with_base_dir("/docs");
        let doc = render_blocks("before\n\n![a cat](cat.png)\n\nafter", 80, &opts);
        // text (before) | image | text (after)
        let images: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Image(img) => Some(img),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        match &images[0].source {
            ImageSource::Path(p) => assert_eq!(p, &PathBuf::from("/docs/cat.png")),
            other => panic!("expected a path source, got {other:?}"),
        }
        assert_eq!(images[0].alt, "a cat");
        // The surrounding prose is still present as text blocks.
        let text: String = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Text(t) => Some(crate::to_plain_string(t)),
                _ => None,
            })
            .collect();
        assert!(text.contains("before") && text.contains("after"));
    }

    #[test]
    fn remote_and_unsupported_images_stay_placeholder_text() {
        let opts = RenderOptions::new(false).with_base_dir("/docs");
        // Remote URL and a non-image extension both keep inline placeholder text.
        let doc = render_blocks("![x](https://e.com/a.png) ![y](notes.txt)", 80, &opts);
        assert!(
            !doc.has_images(),
            "remote/unsupported links must not become image blocks"
        );
        let text = crate::to_plain_string(&doc.into_text());
        assert!(text.contains("🖼"));
    }

    #[test]
    fn wrap_text_hard_breaks_long_words() {
        assert_eq!(wrap_text("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_text("supercalifragilistic", 5).len(), 4);
        assert_eq!(wrap_text("", 5), vec![String::new()]);
    }

    #[test]
    fn fit_columns_shrinks_widest_and_keeps_short_columns() {
        // Two short columns beside one very wide one, at width 54.
        // overhead = 3*3+1 = 10, so budget = 44.
        let widths = fit_columns(&[8, 90, 6], 54);
        assert_eq!(widths[0], 8, "short column kept its natural width");
        assert_eq!(widths[2], 6, "short column kept its natural width");
        assert!(widths[1] < 90, "the widest column absorbed the shrink");
        assert_eq!(widths.iter().sum::<usize>(), 44);

        // When everything fits, widths are left at natural.
        assert_eq!(fit_columns(&[3, 4, 5], 80), vec![3, 4, 5]);
    }

    #[test]
    fn emphasis_sets_modifiers() {
        let text = render("**bold** and *italic*", 80, &RenderOptions::new(false));
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
