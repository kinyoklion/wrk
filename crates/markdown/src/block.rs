//! The block sequence a document renders into.
//!
//! Prose renders to styled [`Text`]; image links render to an [`ImageRef`] that
//! the view rasterizes and paints with a terminal graphics protocol (behind the
//! `images` feature). Splitting the document into blocks — rather than one flat
//! [`Text`] — is what lets images sit inline between runs of text while the text
//! stays wrappable and selectable.

use std::path::PathBuf;

use ratatui::text::{Line, Text};

/// A rendered document: an ordered run of text and image blocks.
#[derive(Debug, Clone, Default)]
pub struct RenderedDoc {
    pub blocks: Vec<MdBlock>,
}

/// One block of a [`RenderedDoc`].
#[derive(Debug, Clone)]
pub enum MdBlock {
    /// A run of styled prose (headings, paragraphs, lists, code, tables, …).
    Text(Text<'static>),
    /// An image link, resolved to a source the view can rasterize.
    Image(ImageRef),
}

/// An image link parsed from the document, plus the text to show when it can't
/// be rendered (images feature off, load failure, or no terminal graphics).
#[derive(Debug, Clone)]
pub struct ImageRef {
    /// The image's alt text (may be empty).
    pub alt: String,
    /// Where the image bytes come from.
    pub source: ImageSource,
    /// Fallback line shown in place of the image (`🖼 alt (dest)`), pre-styled
    /// with the document theme.
    pub placeholder: Line<'static>,
}

/// Where an image's bytes come from.
#[derive(Debug, Clone)]
pub enum ImageSource {
    /// A file on disk (`![](x.png)`, `![](diagram.svg)`, …), resolved against
    /// the document's base directory. The rasterizer picks raster vs. SVG
    /// decoding by extension/content.
    Path(PathBuf),
    /// Inline SVG source. This is the seam a diagram backend (e.g. mermaid →
    /// SVG) plugs into: it hands back SVG text that rasterizes the same way a
    /// `.svg` file does.
    Svg(String),
}

impl RenderedDoc {
    /// Wrap a single already-rendered [`Text`] as a one-block document. Used by
    /// the text-only callers (and tests) that never produce images.
    pub fn from_text(text: Text<'static>) -> Self {
        Self {
            blocks: vec![MdBlock::Text(text)],
        }
    }

    /// True if any block is an image (the block-layout render path is only
    /// needed then; otherwise the view takes the flat single-`Paragraph` path).
    pub fn has_images(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, MdBlock::Image(_)))
    }

    /// Flatten to a single [`Text`], images rendered as their placeholder line.
    /// Backs the plain-text API (`render_document`, `--print`).
    pub fn into_text(self) -> Text<'static> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for block in self.blocks {
            match block {
                MdBlock::Text(t) => lines.extend(t.lines),
                MdBlock::Image(img) => lines.push(img.placeholder),
            }
        }
        Text::from(lines)
    }
}
