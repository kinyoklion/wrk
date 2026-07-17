//! A scrollable, word-wrapping view widget over a rendered [`RenderedDoc`].
//!
//! Text-only documents take a fast path: one wrapped [`Paragraph`] with a scroll
//! offset, exactly as a flat `Text` would render. When a document contains image
//! blocks (and the `images` feature is on), the view switches to a block layout
//! that stacks text and image blocks vertically and scrolls across them, drawing
//! images with a terminal graphics protocol.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Paragraph, StatefulWidget, Widget, Wrap};

use crate::block::RenderedDoc;

#[cfg(feature = "images")]
use crate::block::MdBlock;
#[cfg(feature = "images")]
use ratatui::layout::Size;
#[cfg(feature = "images")]
use ratatui_image::{
    Resize,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

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

/// Per-block image protocols, index-aligned with a document's blocks (`None`
/// for text blocks and images that failed to load). Rebuilt by
/// [`MarkdownViewState::prepare_images`] when the document is (re-)rendered.
///
/// Each protocol is a [`SlicedProtocol`] built once at a fixed cell size, so
/// scrolling renders a cropped slice at a stable scale (via [`SlicedImage`])
/// rather than re-fitting — and re-encoding — the whole image every frame.
#[cfg(feature = "images")]
#[derive(Default)]
struct ImageStore {
    protocols: Vec<Option<SlicedProtocol>>,
}

/// Fixed display size for an image: its natural cell size, scaled down to fit
/// `max_w` cells wide (never up). Height follows to preserve aspect. This is the
/// stable size a [`SlicedProtocol`] is built at.
#[cfg(feature = "images")]
fn fit_to_width(natural: Size, max_w: u16) -> Size {
    if max_w == 0 || natural.width == 0 || natural.width <= max_w {
        return natural;
    }
    let height = ((u32::from(natural.height) * u32::from(max_w)) / u32::from(natural.width)).max(1);
    Size::new(max_w, height as u16)
}

/// Scroll position and transient selection for a [`MarkdownView`]. The widget
/// refreshes the viewport/content geometry and a snapshot of the visible glyphs
/// on each render, so the navigation and selection methods work off the last
/// laid-out frame.
#[derive(Default)]
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
    /// Rasterized image protocols for the current document (images feature).
    #[cfg(feature = "images")]
    images: ImageStore,
    /// Visible image blocks and the buffer rect each occupied, captured each
    /// render (top-to-bottom). Backs click-to-open and "open the top image in
    /// view" for the fullscreen viewer.
    #[cfg(feature = "images")]
    image_hits: Vec<ImageHit>,
}

/// A visible image block and where it was drawn, for hit-testing clicks.
#[cfg(feature = "images")]
#[derive(Clone, Copy)]
struct ImageHit {
    /// Index into the document's `blocks`.
    block: usize,
    rect: Rect,
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

    /// (Re-)rasterize the document's image blocks into terminal protocols using
    /// `picker`, sized to fit `width` cells. Call once whenever the document is
    /// (re-)rendered — on open, resize, or reload — not every frame; the
    /// protocols persist between draws and are sliced (not re-fit) while
    /// scrolling. `width` must match the width the view renders at (its inner
    /// width). Text blocks and images that fail to load get `None` (the view
    /// then shows the placeholder line).
    #[cfg(feature = "images")]
    pub fn prepare_images(&mut self, doc: &RenderedDoc, picker: &Picker, width: u16) {
        self.images.protocols = doc
            .blocks
            .iter()
            .map(|block| match block {
                MdBlock::Image(img) => crate::image::load(&img.source).ok().and_then(|dyn_img| {
                    // Fix the size once (natural, capped to the pane width) so
                    // the image has a stable height and scrolling only slices it.
                    let size =
                        fit_to_width(Resize::natural_size(&dyn_img, picker.font_size()), width);
                    SlicedProtocol::new(picker, dyn_img, Some(size)).ok()
                }),
                MdBlock::Text(_) => None,
            })
            .collect();
    }

    /// Document block index of the image drawn under buffer cell `(x, y)`, if
    /// any — for click-to-open. Coordinates are absolute buffer positions, as
    /// captured during the last render.
    #[cfg(feature = "images")]
    pub fn image_at(&self, x: u16, y: u16) -> Option<usize> {
        self.image_hits
            .iter()
            .find(|h| {
                x >= h.rect.x
                    && x < h.rect.x + h.rect.width
                    && y >= h.rect.y
                    && y < h.rect.y + h.rect.height
            })
            .map(|h| h.block)
    }

    /// Document block index of the top-most image currently in view, if any —
    /// for a keyboard "open the image I'm looking at".
    #[cfg(feature = "images")]
    pub fn top_visible_image(&self) -> Option<usize> {
        self.image_hits.first().map(|h| h.block)
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

/// Word-wrap `text` and count how many display rows it occupies at `width`.
#[cfg(feature = "images")]
fn text_rows(text: &ratatui::text::Text<'static>, width: u16) -> u16 {
    Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

/// Widget that draws a rendered markdown document with vertical scrolling, word
/// wrap, and (behind the `images` feature) inline images.
pub struct MarkdownView<'a> {
    doc: &'a RenderedDoc,
    block: Option<Block<'a>>,
}

impl<'a> MarkdownView<'a> {
    pub fn new(doc: &'a RenderedDoc) -> Self {
        Self { doc, block: None }
    }

    pub fn block(mut self, block: Block<'a>) -> Self {
        self.block = Some(block);
        self
    }

    /// Fast path: flatten the whole document to one `Text` and render it as a
    /// single scrolled paragraph. Used when there are no image blocks (or the
    /// `images` feature is off), preserving the original text-only behavior.
    fn render_flat(
        doc: &RenderedDoc,
        inner: Rect,
        buf: &mut Buffer,
        state: &mut MarkdownViewState,
    ) {
        let text = doc.clone().into_text();
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let content_h = paragraph.line_count(inner.width) as u16;
        state.sync(content_h, inner.height);
        paragraph.scroll((state.scroll, 0)).render(inner, buf);
    }
}

impl StatefulWidget for MarkdownView<'_> {
    type State = MarkdownViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let doc = self.doc;
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

        #[cfg(feature = "images")]
        if doc.has_images() {
            Self::render_blocks(doc, inner, buf, state);
            state.capture_and_highlight(inner, buf);
            return;
        }

        Self::render_flat(doc, inner, buf, state);
        state.capture_and_highlight(inner, buf);
    }
}

#[cfg(feature = "images")]
impl MarkdownView<'_> {
    /// Fixed height in rows an image block occupies — the height the
    /// [`SlicedProtocol`] was built at, independent of scroll and viewport. A
    /// taller-than-viewport image keeps its height and is scrolled through.
    /// Falls back to one row (the placeholder line) when the image failed to
    /// load.
    fn image_rows(state: &MarkdownViewState, idx: usize) -> u16 {
        state
            .images
            .protocols
            .get(idx)
            .and_then(|p| p.as_ref())
            .map_or(1, |proto| proto.size().height.max(1))
    }

    /// Block-layout path: stack blocks vertically, scroll across them, and draw
    /// each block clipped to the visible window.
    fn render_blocks(
        doc: &RenderedDoc,
        inner: Rect,
        buf: &mut Buffer,
        state: &mut MarkdownViewState,
    ) {
        // Measure every block first so scrolling can clamp to the full height.
        let heights: Vec<u16> = doc
            .blocks
            .iter()
            .enumerate()
            .map(|(i, block)| match block {
                MdBlock::Text(t) => text_rows(t, inner.width),
                MdBlock::Image(_) => Self::image_rows(state, i),
            })
            .collect();
        let content_h: u16 = heights.iter().copied().fold(0, u16::saturating_add);
        state.sync(content_h, inner.height);
        state.image_hits.clear();

        let scroll = state.scroll;
        let win_bot = scroll.saturating_add(inner.height);
        let mut y_doc: u16 = 0;
        for (i, block) in doc.blocks.iter().enumerate() {
            let h = heights[i];
            let top = y_doc;
            let bot = y_doc.saturating_add(h);
            y_doc = bot;
            // Skip blocks entirely above or below the visible window.
            if bot <= scroll || top >= win_bot {
                continue;
            }
            let clip_top = scroll.saturating_sub(top); // rows hidden above the fold
            let y_in_view = top.saturating_sub(scroll); // first visible row in viewport
            let avail = inner.height.saturating_sub(y_in_view);
            let vis_h = h.saturating_sub(clip_top).min(avail);
            if vis_h == 0 {
                continue;
            }
            let rect = Rect {
                x: inner.x,
                y: inner.y + y_in_view,
                width: inner.width,
                height: vis_h,
            };
            match block {
                MdBlock::Text(t) => {
                    Paragraph::new(t.clone())
                        .wrap(Wrap { trim: false })
                        .scroll((clip_top, 0))
                        .render(rect, buf);
                }
                MdBlock::Image(img) => {
                    let hit_w = match state.images.protocols.get(i).and_then(|p| p.as_ref()) {
                        // Draw the fixed-size image offset up by the scrolled-off
                        // rows: `SlicedImage` crops to the visible slice at a
                        // stable scale (no re-fit), reusing the encoded protocol.
                        Some(proto) => {
                            let pos = SignedPosition {
                                x: 0,
                                y: -(clip_top.min(i16::MAX as u16) as i16),
                            };
                            let w = proto.size().width.min(rect.width);
                            SlicedImage::new(proto, pos).render(rect, buf);
                            w
                        }
                        None => {
                            Paragraph::new(img.placeholder.clone())
                                .scroll((clip_top, 0))
                                .render(rect, buf);
                            rect.width
                        }
                    };
                    // Record where this image drew so a click (or the "top image
                    // in view" key) can open it in the fullscreen viewer.
                    state.image_hits.push(ImageHit {
                        block: i,
                        rect: Rect {
                            width: hit_w,
                            ..rect
                        },
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::{Line, Text};

    fn doc(text: Text<'static>) -> RenderedDoc {
        RenderedDoc::from_text(text)
    }

    fn long_doc(n: usize) -> RenderedDoc {
        doc(Text::from(
            (0..n)
                .map(|i| Line::from(format!("line {i}")))
                .collect::<Vec<_>>(),
        ))
    }

    fn render_into(doc: &RenderedDoc, w: u16, h: u16, state: &mut MarkdownViewState) {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        MarkdownView::new(doc).render(area, &mut buf, state);
    }

    #[test]
    fn clamps_scroll_to_content() {
        let d = long_doc(50);
        let mut state = MarkdownViewState::new();
        render_into(&d, 20, 10, &mut state);
        assert_eq!(state.max_scroll(), 40); // 50 lines - 10 visible
        state.scroll_by(1000);
        render_into(&d, 20, 10, &mut state);
        assert_eq!(state.scroll(), 40);
    }

    #[test]
    fn short_doc_does_not_scroll() {
        let d = long_doc(3);
        let mut state = MarkdownViewState::new();
        render_into(&d, 20, 10, &mut state);
        assert_eq!(state.max_scroll(), 0);
        state.scroll_by(5);
        assert_eq!(state.scroll(), 0);
    }

    #[test]
    fn page_and_bottom_navigation() {
        let d = long_doc(100);
        let mut state = MarkdownViewState::new();
        render_into(&d, 20, 10, &mut state);
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
        let d = doc(Text::from(vec![
            Line::from("hello world"),
            Line::from("second line"),
        ]));
        let mut state = MarkdownViewState::new();
        render_into(&d, 20, 5, &mut state);

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

    /// End-to-end image path: an SVG image block, rasterized through the real
    /// `Picker`/`StatefulProtocol` and drawn with the halfblock protocol (which
    /// needs no terminal), must paint colored cells into the buffer — proving
    /// parse → rasterize → protocol → block-layout render all connect.
    #[cfg(feature = "images")]
    #[test]
    fn svg_image_block_paints_colored_cells() {
        use crate::block::{ImageRef, ImageSource, MdBlock};
        use ratatui::style::Color;

        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
            <rect width="40" height="20" fill="#ff0000"/></svg>"##;
        let d = RenderedDoc {
            blocks: vec![
                MdBlock::Text(Text::from("caption")),
                MdBlock::Image(ImageRef {
                    alt: "red".into(),
                    source: ImageSource::Svg(svg.into()),
                    placeholder: Line::from("[img]"),
                }),
            ],
        };
        assert!(d.has_images());

        let picker = crate::Picker::halfblocks();
        let mut state = MarkdownViewState::new();
        state.prepare_images(&d, &picker, 20);

        let area = Rect::new(0, 0, 20, 12);
        let mut buf = Buffer::empty(area);
        MarkdownView::new(&d).render(area, &mut buf, &mut state);

        // The caption is default-styled, so any RGB color must come from the
        // rasterized image (halfblocks paint the fill as cell fg/bg).
        let colored = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = &buf[(x, y)];
                matches!(c.fg, Color::Rgb(..)) || matches!(c.bg, Color::Rgb(..))
            })
            .count();
        assert!(colored > 0, "expected the SVG image to paint colored cells");
    }

    #[cfg(feature = "images")]
    #[test]
    fn image_height_is_fixed_regardless_of_viewport() {
        use crate::block::{ImageRef, ImageSource, MdBlock};

        // A very tall image so it exceeds any small viewport we render into.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="2000">
            <rect width="20" height="2000" fill="#00ff00"/></svg>"##;
        let d = RenderedDoc {
            blocks: vec![MdBlock::Image(ImageRef {
                alt: "tall".into(),
                source: ImageSource::Svg(svg.into()),
                placeholder: Line::from("[img]"),
            })],
        };
        let picker = crate::Picker::halfblocks();
        let mut state = MarkdownViewState::new();
        state.prepare_images(&d, &picker, 40);

        // Render into a viewport of height `h` and recover the content height.
        // (`max_scroll == content_h - viewport_h` while content exceeds it.)
        let content_h = |state: &mut MarkdownViewState, h: u16| {
            let area = Rect::new(0, 0, 40, h);
            let mut buf = Buffer::empty(area);
            MarkdownView::new(&d).render(area, &mut buf, state);
            state.max_scroll() + h
        };
        let a = content_h(&mut state, 4);
        let b = content_h(&mut state, 8);
        assert_eq!(
            a, b,
            "the image's height must be fixed, not fit to the viewport"
        );
        assert!(
            a > 8,
            "the tall image should be scrollable past a small viewport"
        );
    }

    #[test]
    fn selection_reverse_video_highlights_only_selected_cells() {
        let d = doc(Text::from("abcdef"));
        let mut state = MarkdownViewState::new();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        MarkdownView::new(&d).render(area, &mut buf, &mut state);

        state.selection_anchor(0, 0);
        state.selection_update(2, 0); // cols 0..=2
        // Re-render to apply the highlight over the captured frame.
        MarkdownView::new(&d).render(area, &mut buf, &mut state);

        assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(3, 0)].modifier.contains(Modifier::REVERSED));
    }
}
