//! Rendering markdown headings as SVG so they display at a true, larger font
//! size (the `images` feature).
//!
//! A heading (H1–H3) becomes an SVG of its text in the bundled bold Liberation
//! Sans, sized relative to the terminal cell so H1 glyphs are ~2× body text.
//! The SVG width is measured from the font's own advance metrics, so a short
//! heading renders at natural size and a long one scales down to fit the pane
//! (rather than clipping). H4–H6 stay as ordinary styled text — their size is
//! too close to the body to be worth rasterizing.

use ratatui::style::Color;

/// Per-level font scale, as a multiple of the terminal cell height. Index by
/// `level - 1`; only H1–H3 are imaged.
const SCALE: [f32; 3] = [2.1, 1.7, 1.4];

/// Build an SVG for a heading, or `None` if the level isn't imaged (H4–H6) or
/// the text is empty. `cell_w`/`cell_h` are the terminal cell size in pixels,
/// which fixes the heading's size relative to body text.
pub(crate) fn heading_svg(
    text: &str,
    level: usize,
    color: Color,
    cell_w: u16,
    cell_h: u16,
) -> Option<String> {
    let text = text.trim();
    if !(1..=3).contains(&level) || text.is_empty() {
        return None;
    }
    let cell_h = f32::from(cell_h.max(1));
    let cell_w = f32::from(cell_w.max(1));
    let font_px = (SCALE[level - 1] * cell_h).round().max(1.0);

    // Measure the text so the SVG is exactly as wide as the glyphs; pad by one
    // cell so the last glyph never touches the right edge after rasterization.
    let text_w = measure(text, font_px) + cell_w;
    // Room for ascenders/descenders; baseline near the bottom of that box.
    let height_px = (font_px * 1.3).round().max(1.0);
    let baseline = (font_px * 1.02).round();

    Some(format!(
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}">"#,
            r#"<text x="0" y="{b}" font-family="{fam}" font-weight="bold" "#,
            r#"font-size="{fs}" fill="{fill}">{text}</text></svg>"#,
        ),
        w = text_w.ceil().max(1.0),
        h = height_px,
        b = baseline,
        fam = crate::image::SANS_FAMILY,
        fs = font_px,
        fill = css_color(color),
        text = escape_xml(text),
    ))
}

/// Total advance width of `text` in the bundled bold face at `font_px`, summed
/// from the font's horizontal metrics (kerning ignored — negligible for
/// heading-length runs). Characters absent from the face fall back to half an
/// em, which is close enough for the odd symbol.
fn measure(text: &str, font_px: f32) -> f32 {
    let Ok(face) = ttf_parser::Face::parse(crate::image::FONT_BOLD, 0) else {
        // No face → estimate at ~0.55em per char so we don't under-size.
        return text.chars().count() as f32 * font_px * 0.55;
    };
    let upem = f32::from(face.units_per_em());
    let scale = font_px / upem;
    let fallback = upem * 0.5;
    text.chars()
        .map(|ch| {
            let adv = face
                .glyph_index(ch)
                .and_then(|g| face.glyph_hor_advance(g))
                .map_or(fallback, f32::from);
            adv * scale
        })
        .sum()
}

/// Convert a ratatui [`Color`] to a CSS color string for the SVG `fill`.
/// Handles RGB, the 16 named ANSI colors, and 256-color indices; anything else
/// (e.g. `Reset`) falls back to a light grey that reads on either background.
fn css_color(color: Color) -> String {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => indexed_rgb(i),
        Color::Black => (0, 0, 0),
        Color::Red => (0xcd, 0x00, 0x00),
        Color::Green => (0x00, 0xcd, 0x00),
        Color::Yellow => (0xcd, 0xcd, 0x00),
        Color::Blue => (0x00, 0x00, 0xee),
        Color::Magenta => (0xcd, 0x00, 0xcd),
        Color::Cyan => (0x00, 0xcd, 0xcd),
        Color::Gray => (0xe5, 0xe5, 0xe5),
        Color::DarkGray => (0x7f, 0x7f, 0x7f),
        Color::LightRed => (0xff, 0x00, 0x00),
        Color::LightGreen => (0x00, 0xff, 0x00),
        Color::LightYellow => (0xff, 0xff, 0x00),
        Color::LightBlue => (0x5c, 0x5c, 0xff),
        Color::LightMagenta => (0xff, 0x00, 0xff),
        Color::LightCyan => (0x00, 0xff, 0xff),
        Color::White => (0xff, 0xff, 0xff),
        Color::Reset => (0xc0, 0xc0, 0xc0),
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// RGB for an xterm 256-color index: 0–15 named, 16–231 the 6×6×6 cube,
/// 232–255 the grayscale ramp.
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => {
            const BASE: [(u8, u8, u8); 16] = [
                (0, 0, 0),
                (0xcd, 0, 0),
                (0, 0xcd, 0),
                (0xcd, 0xcd, 0),
                (0, 0, 0xee),
                (0xcd, 0, 0xcd),
                (0, 0xcd, 0xcd),
                (0xe5, 0xe5, 0xe5),
                (0x7f, 0x7f, 0x7f),
                (0xff, 0, 0),
                (0, 0xff, 0),
                (0xff, 0xff, 0),
                (0x5c, 0x5c, 0xff),
                (0xff, 0, 0xff),
                (0, 0xff, 0xff),
                (0xff, 0xff, 0xff),
            ];
            BASE[i as usize]
        }
        16..=231 => {
            let i = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            (
                steps[(i / 36) as usize],
                steps[((i / 6) % 6) as usize],
                steps[(i % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Escape the five XML metacharacters so heading text is safe inside `<text>`.
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_h1_to_h3_only() {
        let c = Color::Cyan;
        assert!(heading_svg("Title", 1, c, 8, 16).is_some());
        assert!(heading_svg("Title", 3, c, 8, 16).is_some());
        assert!(heading_svg("Title", 4, c, 8, 16).is_none());
        assert!(heading_svg("   ", 1, c, 8, 16).is_none(), "blank → none");
    }

    #[test]
    fn h1_is_larger_than_h2() {
        let big = heading_svg("Hi", 1, Color::Cyan, 8, 16).unwrap();
        let small = heading_svg("Hi", 2, Color::Cyan, 8, 16).unwrap();
        let fs = |svg: &str| {
            let at = svg.find("font-size=\"").unwrap() + 11;
            svg[at..].split('"').next().unwrap().parse::<f32>().unwrap()
        };
        assert!(fs(&big) > fs(&small), "H1 font must exceed H2");
    }

    #[test]
    fn escapes_and_colors() {
        let svg = heading_svg("A & B <c>", 1, Color::Rgb(0x11, 0x22, 0x33), 8, 16).unwrap();
        assert!(svg.contains("A &amp; B &lt;c&gt;"), "text escaped: {svg}");
        assert!(svg.contains("fill=\"#112233\""), "rgb → hex: {svg}");
        assert!(svg.contains("<svg") && svg.contains("</svg>"));
    }

    #[test]
    fn svg_rasterizes_with_visible_glyphs() {
        use image::GenericImageView;
        let svg = heading_svg("Hello", 1, Color::Rgb(0x00, 0xcd, 0xcd), 8, 16).unwrap();
        let img = crate::image::load(&crate::block::ImageSource::Svg(svg)).expect("rasterize");
        let (w, h) = img.dimensions();
        assert!(w > 0 && h > 0);
        // Some pixels drew (the glyphs), i.e. not a blank transparent canvas.
        assert!(
            img.pixels().any(|(_, _, p)| p[3] > 0),
            "expected rendered heading glyphs"
        );
    }

    #[test]
    fn longer_text_is_wider() {
        let short = heading_svg("Hi", 1, Color::Cyan, 8, 16).unwrap();
        let long = heading_svg("Hello there world", 1, Color::Cyan, 8, 16).unwrap();
        let w = |svg: &str| {
            let at = svg.find("width=\"").unwrap() + 7;
            svg[at..].split('"').next().unwrap().parse::<f32>().unwrap()
        };
        assert!(w(&long) > w(&short), "measured width should grow with text");
    }
}
