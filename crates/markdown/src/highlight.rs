//! Code-block syntax highlighting via syntect (feature `highlight`).

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

struct Assets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        // A dark, widely-recognizable default that ships with syntect.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .expect("syntect ships at least one default theme");
        Assets { syntaxes, theme }
    })
}

/// Highlight a fenced code block. Returns `None` when the language is unknown,
/// so the caller can fall back to plain rendering.
pub fn highlight_block(source: &str, lang: &str) -> Option<Vec<Line<'static>>> {
    let assets = assets();
    let syntax = if lang.is_empty() {
        return None;
    } else {
        assets.syntaxes.find_syntax_by_token(lang)?
    };

    let mut hl = HighlightLines::new(syntax, &assets.theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(source) {
        let Ok(ranges) = hl.highlight_line(raw, &assets.syntaxes) else {
            return None;
        };
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                Span::styled(
                    text.trim_end_matches('\n').to_string(),
                    syn_to_ratatui(style),
                )
            })
            .collect();
        lines.push(Line::from(spans));
    }
    Some(lines)
}

fn syn_to_ratatui(style: SynStyle) -> Style {
    let fg = style.foreground;
    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b))
}
