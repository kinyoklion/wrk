//! Pluggable rendering for diagram code fences (mermaid and friends).
//!
//! A diagram fence can render two ways: as styled text lines (the default
//! [`NullBackend`], which shows the source verbatim with a hint so documents
//! never break) or as an image. The [`CarcimaidBackend`] (behind the `mermaid`
//! feature) takes the latter path: it turns mermaid source into SVG, which the
//! view rasterizes through the same pipeline as any other image.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::block::ImageSource;
use crate::theme::MdTheme;

/// Languages whose fenced blocks are treated as diagrams (routed to the
/// [`DiagramBackend`]) rather than syntax-highlighted as code.
pub(crate) fn is_diagram_lang(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "mermaid")
}

/// What a [`DiagramBackend`] produces for a diagram fence: either styled text
/// lines to splice into the flow, or an image source to rasterize.
pub enum DiagramOutput {
    /// Render as text lines inline (the fallback / preview-off path).
    Lines(Vec<Line<'static>>),
    /// Render as an image — the source rasterizes via the `images` pipeline;
    /// if that's unavailable, the caller's placeholder line shows instead.
    Image(ImageSource),
}

/// Renders a diagram fence into terminal output.
pub trait DiagramBackend {
    /// Render the `source` of a diagram fence with the given `lang` (e.g.
    /// `"mermaid"`). `theme` is provided so text output can match the
    /// surrounding document's palette.
    fn render(&self, lang: &str, source: &str, theme: &MdTheme) -> DiagramOutput;
}

/// Default backend: renders the diagram source as a dimmed code block prefixed
/// with a note that live preview isn't enabled.
pub struct NullBackend;

impl DiagramBackend for NullBackend {
    fn render(&self, lang: &str, source: &str, theme: &MdTheme) -> DiagramOutput {
        DiagramOutput::Lines(source_dump(
            &format!("[{lang} diagram — preview not enabled]"),
            source,
            theme,
        ))
    }
}

/// Render a hint line followed by the diagram source, all dimmed. Shared by the
/// [`NullBackend`] and the [`CarcimaidBackend`]'s parse-error fallback.
fn source_dump(hint: &str, source: &str, theme: &MdTheme) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(source.lines().count() + 1);
    lines.push(Line::from(Span::styled(
        hint.to_string(),
        Style::default()
            .fg(theme.faint)
            .add_modifier(Modifier::ITALIC),
    )));
    for raw in source.lines() {
        lines.push(Line::from(Span::styled(
            raw.to_string(),
            Style::default().fg(theme.faint),
        )));
    }
    lines
}

/// Backend that renders mermaid fences as diagrams via [`carcimaid`], a
/// pure-Rust mermaid → SVG renderer. The SVG rasterizes through the same resvg
/// path as any `.svg` image. We request a transparent background so the diagram
/// picks up the terminal's own background instead of carcimaid's default white
/// box. On a parse/render error it falls back to the [`NullBackend`]-style
/// source dump with the error as the hint, so a malformed diagram degrades to
/// readable source instead of vanishing.
#[cfg(feature = "mermaid")]
pub struct CarcimaidBackend;

#[cfg(feature = "mermaid")]
impl DiagramBackend for CarcimaidBackend {
    fn render(&self, lang: &str, source: &str, theme: &MdTheme) -> DiagramOutput {
        match carcimaid::render_to_svg_with(source, carcimaid::Background::Transparent) {
            Ok(svg) => DiagramOutput::Image(ImageSource::Svg(svg)),
            Err(e) => DiagramOutput::Lines(source_dump(
                &format!("[{lang} diagram — render failed: {e}]"),
                source,
                theme,
            )),
        }
    }
}
