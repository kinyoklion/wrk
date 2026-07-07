//! A scrollable, word-wrapping view widget over rendered markdown [`Text`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};

/// A text selection in *viewport* coordinates: `(row, col)` cell positions
/// within the last-rendered visible area. Anchor is where the drag began.
#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: (u16, u16),
    cursor: (u16, u16),
}

impl Selection {
    /// Start/end ordered in reading order (top-to-bottom, left-to-right).
    fn ends(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// The `[start_col, end_col)` selected on visible `row`, given the row width.
    fn col_span(&self, row: u16, width: u16) -> (u16, u16) {
        let ((r0, c0), (r1, c1)) = self.ends();
        if row < r0 || row > r1 {
            return (0, 0);
        }
        let start = if row == r0 { c0 } else { 0 };
        let end = if row == r1 {
            c1.saturating_add(1)
        } else {
            width
        };
        (start.min(width), end.min(width))
    }
}

/// Scroll position and transient selection for a [`MarkdownView`]. The widget
/// refreshes the viewport/content geometry and a snapshot of the visible glyphs
/// on each render, so the navigation and selection methods work off the last
/// laid-out frame.
#[derive(Debug, Default, Clone)]
pub struct MarkdownViewState {
    scroll: u16,
    viewport_h: u16,
    content_h: u16,
    /// Active selection (viewport coords), set by the host on mouse drag.
    selection: Option<Selection>,
    /// Snapshot of the visible rows' text, captured each render for extraction.
    glyphs: Vec<String>,
    /// Width of the captured grid in cells.
    grid_w: u16,
}

impl MarkdownViewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current vertical scroll offset, in wrapped display rows.
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Largest valid scroll offset given the last-rendered geometry.
    pub fn max_scroll(&self) -> u16 {
        self.content_h.saturating_sub(self.viewport_h)
    }

    /// Scroll by `delta` rows (negative = up), clamped to `[0, max_scroll]`.
    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).clamp(0, self.max_scroll() as i32);
        self.scroll = next as u16;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// Scroll up roughly one viewport (keeping one row of overlap).
    pub fn page_up(&mut self) {
        let page = (self.viewport_h.saturating_sub(1)).max(1) as i32;
        self.scroll_by(-page);
    }

    /// Scroll down roughly one viewport (keeping one row of overlap).
    pub fn page_down(&mut self) {
        let page = (self.viewport_h.saturating_sub(1)).max(1) as i32;
        self.scroll_by(page);
    }

    /// Update cached geometry and re-clamp the scroll offset. Called by the
    /// widget during render; exposed so callers can also refresh after a resize
    /// without a full render (e.g. to keep `max_scroll` accurate for key input).
    fn sync(&mut self, content_h: u16, viewport_h: u16) {
        self.content_h = content_h;
        self.viewport_h = viewport_h;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Begin a selection at viewport cell `(col, row)`.
    pub fn selection_anchor(&mut self, col: u16, row: u16) {
        self.selection = Some(Selection {
            anchor: (row, col),
            cursor: (row, col),
        });
    }

    /// Extend the active selection to viewport cell `(col, row)`.
    pub fn selection_update(&mut self, col: u16, row: u16) {
        if let Some(sel) = self.selection.as_mut() {
            sel.cursor = (row, col);
        }
    }

    /// Drop any active selection.
    pub fn selection_clear(&mut self) {
        self.selection = None;
    }

    /// The selected text (rows joined by `\n`, trailing padding trimmed), or
    /// `None` when there is no selection or it is empty. Reads the glyph snapshot
    /// from the last render.
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let ((r0, _), (r1, _)) = sel.ends();
        let mut out = String::new();
        for row in r0..=r1 {
            let Some(line) = self.glyphs.get(row as usize) else {
                break;
            };
            let chars: Vec<char> = line.chars().collect();
            let (c0, c1) = sel.col_span(row, self.grid_w);
            let c1 = (c1 as usize).min(chars.len());
            let c0 = (c0 as usize).min(c1);
            let piece: String = chars[c0..c1].iter().collect();
            out.push_str(piece.trim_end());
            if row != r1 {
                out.push('\n');
            }
        }
        let trimmed = out.trim_end_matches('\n');
        if trimmed.trim().is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Snapshot the visible glyphs from `buf` over `inner` and reverse-video any
    /// selected cells. Called during render, after the content is drawn.
    fn capture_and_highlight(&mut self, inner: Rect, buf: &mut Buffer) {
        let mut glyphs = Vec::with_capacity(inner.height as usize);
        for r in 0..inner.height {
            let mut row = String::with_capacity(inner.width as usize);
            for c in 0..inner.width {
                row.push_str(buf[(inner.x + c, inner.y + r)].symbol());
            }
            glyphs.push(row);
        }
        self.glyphs = glyphs;
        self.grid_w = inner.width;

        if let Some(sel) = self.selection {
            let highlight = Style::default().add_modifier(Modifier::REVERSED);
            for r in 0..inner.height {
                let (c0, c1) = sel.col_span(r, inner.width);
                for c in c0..c1 {
                    buf[(inner.x + c, inner.y + r)].set_style(highlight);
                }
            }
        }
    }
}

/// Widget that draws rendered markdown with vertical scrolling and word wrap.
pub struct MarkdownView<'a> {
    text: &'a Text<'a>,
    block: Option<Block<'a>>,
}

impl<'a> MarkdownView<'a> {
    pub fn new(text: &'a Text<'a>) -> Self {
        Self { text, block: None }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }
}

impl StatefulWidget for MarkdownView<'_> {
    type State = MarkdownViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let inner = match &self.block {
            Some(b) => b.inner(area),
            None => area,
        };
        if let Some(b) = self.block {
            b.render(area, buf);
        }
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let paragraph = Paragraph::new(self.text.clone()).wrap(Wrap { trim: false });
        let content_h = paragraph.line_count(inner.width) as u16;
        state.sync(content_h, inner.height);

        paragraph.scroll((state.scroll, 0)).render(inner, buf);
        state.capture_and_highlight(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    fn long_text(n: usize) -> Text<'static> {
        Text::from(
            (0..n)
                .map(|i| Line::from(format!("line {i}")))
                .collect::<Vec<_>>(),
        )
    }

    fn render_into(text: &Text<'_>, w: u16, h: u16, state: &mut MarkdownViewState) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        MarkdownView::new(text).render(area, &mut buf, state);
    }

    #[test]
    fn clamps_scroll_to_content() {
        let text = long_text(50);
        let mut state = MarkdownViewState::new();
        render_into(&text, 20, 10, &mut state);
        assert_eq!(state.max_scroll(), 40); // 50 lines - 10 visible
        state.scroll_by(1000);
        render_into(&text, 20, 10, &mut state);
        assert_eq!(state.scroll(), 40);
    }

    #[test]
    fn short_doc_does_not_scroll() {
        let text = long_text(3);
        let mut state = MarkdownViewState::new();
        render_into(&text, 20, 10, &mut state);
        assert_eq!(state.max_scroll(), 0);
        state.scroll_by(5);
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn page_and_bottom_navigation() {
        let text = long_text(100);
        let mut state = MarkdownViewState::new();
        render_into(&text, 20, 10, &mut state);
        state.page_down();
        assert_eq!(state.scroll(), 9);
        state.scroll_to_bottom();
        assert_eq!(state.scroll(), 90);
        state.page_up();
        assert_eq!(state.scroll(), 81);
        state.scroll_to_top();
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn selection_extracts_single_and_multi_row_text() {
        let text = Text::from(vec![Line::from("hello world"), Line::from("second line")]);
        let mut state = MarkdownViewState::new();
        render_into(&text, 20, 5, &mut state);

        // Single row: cols 0..=4 → "hello".
        state.selection_anchor(0, 0);
        state.selection_update(4, 0);
        assert_eq!(state.selection_text().as_deref(), Some("hello"));

        // Multi row: (row0,col6) → (row1,col5) = "world" + "second".
        state.selection_anchor(6, 0);
        state.selection_update(5, 1);
        assert_eq!(state.selection_text().as_deref(), Some("world\nsecond"));

        state.selection_clear();
        assert_eq!(state.selection_text(), None);
    }

    #[test]
    fn selection_reverse_video_highlights_only_selected_cells() {
        let text = Text::from("abcdef");
        let mut state = MarkdownViewState::new();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        MarkdownView::new(&text).render(area, &mut buf, &mut state);

        state.selection_anchor(0, 0);
        state.selection_update(2, 0); // cols 0..=2
        // Re-render to apply the highlight over the captured frame.
        MarkdownView::new(&text).render(area, &mut buf, &mut state);

        assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(3, 0)].modifier.contains(Modifier::REVERSED));
    }
}
