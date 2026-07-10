//! Markdown rendering for wrk.
//!
//! Parses CommonMark + GitHub-flavored markdown (tables, strikethrough, task
//! lists, footnotes) into ratatui [`Text`], and provides a scrollable
//! [`MarkdownView`] widget. The same crate backs both the embedded markdown tab
//! in the `wrk` TUI and the standalone `wrk-md` viewer, so the two render
//! identically.
//!
//! Fenced code blocks are syntax-highlighted via syntect when the `highlight`
//! feature is on (the default). Diagram fences (e.g. ```` ```mermaid ````) are
//! routed through a pluggable [`DiagramBackend`]; the default [`NullBackend`]
//! renders them as a code block plus a hint, leaving a seam for real diagram
//! rendering later.

mod block;
mod diagram;
mod parse;
mod theme;
mod view;

#[cfg(feature = "highlight")]
mod highlight;

#[cfg(feature = "images")]
mod image;

pub use block::{ImageRef, ImageSource, MdBlock, RenderedDoc};
pub use diagram::{DiagramBackend, DiagramOutput, NullBackend};

#[cfg(feature = "mermaid")]
pub use diagram::CarcimaidBackend;
pub use theme::MdTheme;
pub use view::{MarkdownView, MarkdownViewState};

/// Terminal graphics-protocol picker, re-exported so consumers can build one
/// (`Picker::from_query_stdio()`) without depending on `ratatui-image`
/// directly. Pass it to [`MarkdownViewState::prepare_images`].
#[cfg(feature = "images")]
pub use ratatui_image::picker::Picker;

use std::path::PathBuf;

use ratatui::text::Text;

/// Options controlling how markdown is rendered to [`Text`].
pub struct RenderOptions {
    /// Colors used for the various markdown elements.
    pub theme: MdTheme,
    /// Whether to syntax-highlight fenced code blocks. Ignored (treated as
    /// `false`) when the crate is built without the `highlight` feature.
    pub highlight: bool,
    /// Backend used to render diagram fences (mermaid, …). Defaults to
    /// [`NullBackend`].
    pub diagram: Box<dyn DiagramBackend>,
    /// Directory relative image links resolve against (the document's own
    /// directory). `None` leaves relative paths unresolved, so only absolute
    /// links produce image blocks.
    pub base_dir: Option<PathBuf>,
}

/// The diagram backend used unless a caller overrides it: [`CarcimaidBackend`]
/// (mermaid → SVG) when the `mermaid` feature is on, else the source-dumping
/// [`NullBackend`]. Consumers get real mermaid rendering just by enabling the
/// feature — no code change at the call site.
fn default_diagram_backend() -> Box<dyn DiagramBackend> {
    #[cfg(feature = "mermaid")]
    {
        Box::new(diagram::CarcimaidBackend)
    }
    #[cfg(not(feature = "mermaid"))]
    {
        Box::new(NullBackend)
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            theme: MdTheme::default(),
            highlight: cfg!(feature = "highlight"),
            diagram: default_diagram_backend(),
            base_dir: None,
        }
    }
}

impl RenderOptions {
    /// Convenience constructor with the default theme and the given highlight
    /// preference.
    pub fn new(highlight: bool) -> Self {
        Self {
            highlight,
            ..Self::default()
        }
    }

    /// Set the color theme (builder style).
    pub fn with_theme(mut self, theme: MdTheme) -> Self {
        self.theme = theme;
        self
    }

    /// Set the base directory for resolving relative image links (builder
    /// style).
    pub fn with_base_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(dir.into());
        self
    }
}

/// Render markdown `source` into a ratatui [`Text`] of styled lines.
///
/// `width` is the display width in terminal cells the output will be shown at.
/// It drives table column layout — columns are sized and their contents wrapped
/// to fit. Prose lines are left long for the [`MarkdownView`] widget to word-wrap
/// at display time, so pass the same width you render the view at. For a quick
/// plain-text dump, use [`to_plain_string`].
pub fn render_document(source: &str, width: usize, opts: &RenderOptions) -> Text<'static> {
    parse::render_blocks(source, width, opts).into_text()
}

/// Render markdown `source` into a [`RenderedDoc`] — the block sequence (text +
/// image references) backing the image-capable [`MarkdownView`]. Prefer this
/// over [`render_document`] when images should render as graphics rather than
/// placeholder text. `width` drives table layout as in [`render_document`].
pub fn render_blocks(source: &str, width: usize, opts: &RenderOptions) -> RenderedDoc {
    parse::render_blocks(source, width, opts)
}

/// Flatten a rendered [`Text`] into a plain (unstyled) string, one logical line
/// per row. Used by the standalone viewer's `--print` mode for piping.
pub fn to_plain_string(text: &Text<'_>) -> String {
    let mut out = String::new();
    for line in &text.lines {
        for span in &line.spans {
            out.push_str(&span.content);
        }
        out.push('\n');
    }
    out
}
