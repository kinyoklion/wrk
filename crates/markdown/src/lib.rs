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
pub use diagram::{DiagramBackend, DiagramCtx, DiagramOutput, NullBackend};

#[cfg(feature = "mermaid")]
pub use diagram::CarcimaidBackend;
pub use theme::MdTheme;
pub use view::{MarkdownView, MarkdownViewState};

/// Terminal graphics-protocol picker, re-exported so consumers can build one
/// (`Picker::from_query_stdio()`) without depending on `ratatui-image`
/// directly. Pass it to [`MarkdownViewState::prepare_images`].
#[cfg(feature = "images")]
pub use ratatui_image::picker::Picker;

#[cfg(feature = "images")]
pub use detect::{query_picker, terminal_prefers_dark};

/// Terminal light/dark detection, used to auto-theme diagrams to match the
/// terminal background (the `images` feature; a diagram backend consumes the
/// result via [`RenderOptions::diagram_ctx`]).
#[cfg(feature = "images")]
mod detect {
    use ratatui_image::picker::cap_parser::QueryStdioOptions;
    use ratatui_image::picker::{Capability, Picker};

    /// Build a [`Picker`] by querying the terminal, additionally requesting its
    /// background color (OSC 11) so [`terminal_prefers_dark`] can read it. Like
    /// `Picker::from_query_stdio`, this must run just after entering the
    /// alternate screen. Returns `None` if the query fails.
    pub fn query_picker() -> Option<Picker> {
        let opts = QueryStdioOptions {
            terminal_background_color_osc: true,
            ..Default::default()
        };
        Picker::from_query_stdio_with_options(opts).ok()
    }

    /// Whether the terminal reported a dark background, from the OSC 11
    /// capability captured by [`query_picker`]. `None` if the terminal did not
    /// report a background color (e.g. the picker was built without the query).
    pub fn terminal_prefers_dark(picker: &Picker) -> Option<bool> {
        picker.capabilities().iter().find_map(|c| match c {
            Capability::Background(r, g, b) => Some(is_dark(*r, *g, *b)),
            _ => None,
        })
    }

    /// Dark-vs-light test on a background color, by Rec. 601 perceptual
    /// luminance against a mid-scale threshold.
    fn is_dark(r: u8, g: u8, b: u8) -> bool {
        let lum = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
        lum < 128.0
    }

    #[cfg(test)]
    mod tests {
        use super::is_dark;

        #[test]
        fn classifies_common_backgrounds() {
            assert!(is_dark(0x00, 0x00, 0x00), "black");
            assert!(is_dark(0x1e, 0x1e, 0x2e), "typical dark theme surface");
            assert!(is_dark(0x28, 0x2c, 0x34), "one dark grey");
            assert!(!is_dark(0xff, 0xff, 0xff), "white");
            assert!(!is_dark(0xfa, 0xf0, 0xe6), "light cream");
            assert!(!is_dark(0xdd, 0xdd, 0xdd), "light grey");
        }
    }
}

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
    /// Viewer conditions (dark terminal, opaque-background toggle) forwarded to
    /// the [`DiagramBackend`] for diagram fences. Defaults to all-`false`.
    pub diagram_ctx: DiagramCtx,
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
            diagram_ctx: DiagramCtx::default(),
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

    /// Set the diagram rendering context — dark-terminal / opaque-background
    /// conditions forwarded to the diagram backend (builder style).
    pub fn with_diagram_ctx(mut self, ctx: DiagramCtx) -> Self {
        self.diagram_ctx = ctx;
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
