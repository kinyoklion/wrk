//! Pluggable rendering for diagram code fences (mermaid and friends).
//!
//! Real diagram rendering (rasterizing mermaid via an external tool and showing
//! it with a terminal graphics protocol) is intentionally out of scope here —
//! this trait is the seam it will plug into. The default [`NullBackend`] shows
//! the diagram source verbatim with a hint, so documents containing mermaid
//! never break, and nothing regresses on terminals without graphics support.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::MdTheme;

/// Languages whose fenced blocks are treated as diagrams (routed to the
/// [`DiagramBackend`]) rather than syntax-highlighted as code.
pub(crate) fn is_diagram_lang(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "mermaid")
}

/// Renders a diagram fence into terminal lines.
pub trait DiagramBackend {
    /// Render the `source` of a diagram fence with the given `lang` (e.g.
    /// `"mermaid"`) into styled lines. `theme` is provided so backends can match
    /// the surrounding document's palette.
    fn render(&self, lang: &str, source: &str, theme: &MdTheme) -> Vec<Line<'static>>;
}

/// Default backend: renders the diagram source as a dimmed code block prefixed
/// with a note that live preview isn't enabled.
pub struct NullBackend;

impl DiagramBackend for NullBackend {
    fn render(&self, lang: &str, source: &str, theme: &MdTheme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("[{lang} diagram — preview not enabled]"),
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
}
