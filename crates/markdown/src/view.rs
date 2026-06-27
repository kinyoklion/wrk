//! A scrollable, word-wrapping view widget over rendered markdown [`Text`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};

/// Scroll position for a [`MarkdownView`]. The widget refreshes the viewport and
/// content heights on each render, so the navigation methods clamp correctly to
/// the last laid-out geometry.
#[derive(Debug, Default, Clone)]
pub struct MarkdownViewState {
    scroll: u16,
    viewport_h: u16,
    content_h: u16,
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
}
