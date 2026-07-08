//! Colors for rendered markdown elements.

use ratatui::style::Color;

/// Color palette for markdown rendering. Inline emphasis (bold/italic/strike)
/// is conveyed with text modifiers rather than colors, so this only needs to
/// cover the block- and span-level elements that carry a distinct color.
#[derive(Debug, Clone, Copy)]
pub struct MdTheme {
    /// Heading text (all levels).
    pub heading: Color,
    /// Inline code and code-block text (used when highlighting is off or a
    /// language is unknown).
    pub code: Color,
    /// Optional background for code (inline + blocks). `None` leaves it
    /// transparent so it inherits the surrounding surface.
    pub code_bg: Option<Color>,
    /// Link text.
    pub link: Color,
    /// Block-quote text and its gutter marker.
    pub quote: Color,
    /// Thematic break (`---`) rule.
    pub rule: Color,
    /// List bullet / ordinal markers.
    pub marker: Color,
    /// Dimmed accent for hints and placeholders (e.g. image links, the
    /// diagram-not-rendered note).
    pub faint: Color,
    /// Background for even table body rows (`None` = inherit the surface).
    pub table_row_bg: Option<Color>,
    /// Background for odd table body rows — the alternating "zebra" stripe.
    pub table_row_alt_bg: Option<Color>,
}

impl Default for MdTheme {
    fn default() -> Self {
        Self {
            heading: Color::Cyan,
            code: Color::Rgb(0xd7, 0xd7, 0xaf),
            code_bg: None,
            link: Color::Blue,
            quote: Color::Green,
            rule: Color::DarkGray,
            marker: Color::Yellow,
            faint: Color::DarkGray,
            table_row_bg: None,
            table_row_alt_bg: Some(Color::Rgb(0x26, 0x26, 0x26)),
        }
    }
}
