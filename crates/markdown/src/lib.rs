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

mod diagram;
mod parse;
mod theme;
mod view;

#[cfg(feature = "highlight")]
mod highlight;

pub use diagram::{DiagramBackend, NullBackend};
pub use theme::MdTheme;
pub use view::{MarkdownView, MarkdownViewState};

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
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            theme: MdTheme::default(),
            highlight: cfg!(feature = "highlight"),
            diagram: Box::new(NullBackend),
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
}

/// Render markdown `source` into a ratatui [`Text`] of styled lines.
///
/// The returned text is unwrapped — wrapping happens at display time (the
/// [`MarkdownView`] widget wraps to the viewport width). For a quick plain-text
/// dump, use [`to_plain_string`].
pub fn render_document(source: &str, opts: &RenderOptions) -> Text<'static> {
    parse::render(source, opts)
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
