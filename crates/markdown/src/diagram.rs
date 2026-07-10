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
#[derive(Debug)]
pub enum DiagramOutput {
    /// Render as text lines inline (the fallback / preview-off path).
    Lines(Vec<Line<'static>>),
    /// Render as an image — the source rasterizes via the `images` pipeline;
    /// if that's unavailable, the caller's placeholder line shows instead.
    Image(ImageSource),
}

/// Viewer conditions passed to a diagram backend that affect how a diagram is
/// drawn (as opposed to *what* the diagram is). Backends that render to text
/// (e.g. [`NullBackend`]) ignore it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiagramCtx {
    /// The terminal/viewer background is dark, so a backend should pick a dark
    /// palette (light ink) for a transparent diagram to stay legible.
    pub prefers_dark: bool,
    /// Draw the diagram on an opaque, high-contrast background instead of a
    /// transparent one — the readability escape hatch. When set, the backend
    /// renders its "classic" always-legible card regardless of `prefers_dark`.
    pub opaque_background: bool,
}

/// Renders a diagram fence into terminal output.
pub trait DiagramBackend {
    /// Render the `source` of a diagram fence with the given `lang` (e.g.
    /// `"mermaid"`). `theme` is provided so text output can match the
    /// surrounding document's palette; `ctx` carries viewer conditions (dark
    /// terminal, opaque-background toggle) that image backends honor.
    fn render(&self, lang: &str, source: &str, theme: &MdTheme, ctx: DiagramCtx) -> DiagramOutput;
}

/// Default backend: renders the diagram source as a dimmed code block prefixed
/// with a note that live preview isn't enabled.
pub struct NullBackend;

impl DiagramBackend for NullBackend {
    fn render(&self, lang: &str, source: &str, theme: &MdTheme, _ctx: DiagramCtx) -> DiagramOutput {
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
/// path as any `.svg` image.
///
/// Two rendering modes, chosen by [`DiagramCtx`]:
/// - **Transparent** (default): a transparent background so the diagram picks up
///   the terminal's own color, plus carcimaid's dark theme on dark terminals so
///   the (otherwise dark) ink stays legible.
/// - **Opaque** (`opaque_background`, the readability escape hatch): carcimaid's
///   classic white card — dark ink on white — which is legible on any terminal
///   regardless of theme detection.
///
/// In both modes a diagram's own frontmatter `config.theme` still wins over the
/// theme we pass (carcimaid treats ours as a default), so an author who picks a
/// theme keeps it. On a parse/render error it falls back to the
/// [`NullBackend`]-style source dump with the error as the hint, so a malformed
/// diagram degrades to readable source instead of vanishing.
#[cfg(feature = "mermaid")]
pub struct CarcimaidBackend;

#[cfg(all(test, feature = "mermaid"))]
mod carcimaid_tests {
    use super::*;

    fn svg_of(ctx: DiagramCtx) -> String {
        let out =
            CarcimaidBackend.render("mermaid", "flowchart LR\n A-->B", &MdTheme::default(), ctx);
        match out {
            DiagramOutput::Image(ImageSource::Svg(s)) => s,
            other => panic!("expected an SVG image, got a Lines fallback: {other:?}"),
        }
    }

    #[test]
    fn dark_terminal_picks_a_different_palette() {
        let light = svg_of(DiagramCtx::default());
        let dark = svg_of(DiagramCtx {
            prefers_dark: true,
            opaque_background: false,
        });
        // Both are transparent (no white card)...
        assert!(!light.contains("background-color: white"));
        assert!(!dark.contains("background-color: white"));
        // ...but the dark theme repaints the diagram, so the SVG differs.
        assert_ne!(
            light, dark,
            "dark terminal should apply carcimaid's dark theme"
        );
    }

    #[test]
    fn opaque_toggle_forces_the_white_card() {
        let opaque = svg_of(DiagramCtx {
            prefers_dark: true,
            opaque_background: true,
        });
        // The escape hatch is white-backed regardless of dark detection.
        assert!(
            opaque.contains("background-color: white"),
            "opaque background should be the white card"
        );
    }
}

#[cfg(feature = "mermaid")]
impl DiagramBackend for CarcimaidBackend {
    fn render(&self, lang: &str, source: &str, theme: &MdTheme, ctx: DiagramCtx) -> DiagramOutput {
        use carcimaid::{Background, Theme};

        let (background, diagram_theme) = if ctx.opaque_background {
            // Escape hatch: the classic white card, forced light so dark ink
            // stays legible on the white background.
            (Background::Default, Some(Theme::Default))
        } else if ctx.prefers_dark {
            // Blend with a dark terminal, dark theme → light ink shows through.
            (Background::Transparent, Some(Theme::Dark))
        } else {
            // Blend with a light terminal; leave the theme to the diagram's own
            // frontmatter (defaulting to light).
            (Background::Transparent, None)
        };
        let opts = carcimaid::RenderOptions {
            background,
            theme: diagram_theme,
        };
        match carcimaid::render_to_svg_with(source, &opts) {
            Ok(svg) => DiagramOutput::Image(ImageSource::Svg(svg)),
            Err(e) => DiagramOutput::Lines(source_dump(
                &format!("[{lang} diagram — render failed: {e}]"),
                source,
                theme,
            )),
        }
    }
}
