//! Decoding image sources into pixels for terminal display (the `images`
//! feature).
//!
//! Raster files (`png`/`jpeg`/`gif`/`webp`) decode via the `image` crate. SVG —
//! whether a `.svg` file or inline source handed back by a diagram backend — is
//! rasterized with resvg. SVG `<text>` is rendered against a **bundled**
//! Liberation Sans (metric-compatible with Arial), so diagrams and labelled
//! figures render identically regardless of the fonts installed on the host.

use std::sync::{Arc, OnceLock};

use image::DynamicImage;
use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};

use crate::block::ImageSource;

// Bundled OFL-1.1 Liberation Sans faces (see assets/fonts/LICENSE). Loaded once
// into a shared fontdb; resvg substitutes these for the common sans families
// (arial/helvetica/…) that SVGs reference.
const FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Regular.ttf");
/// Bold face, also used by the heading renderer to measure text.
pub(crate) const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Bold.ttf");
const FONT_ITALIC: &[u8] = include_bytes!("../assets/fonts/LiberationSans-Italic.ttf");
const FONT_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/LiberationSans-BoldItalic.ttf");

/// The bundled font family name (matches the faces' internal family).
pub(crate) const SANS_FAMILY: &str = "Liberation Sans";

/// Shared font database, built once from the bundled faces. Parsing ~1.6 MB of
/// fonts per SVG would be wasteful, so we build it lazily and clone the `Arc`
/// into each parse's `usvg::Options`.
fn fontdb() -> Arc<fontdb::Database> {
    static DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_font_data(FONT_REGULAR.to_vec());
        db.load_font_data(FONT_BOLD.to_vec());
        db.load_font_data(FONT_ITALIC.to_vec());
        db.load_font_data(FONT_BOLD_ITALIC.to_vec());
        // Point every generic family at the bundle so SVGs that ask for
        // arial/helvetica/sans-serif all resolve to a face we ship.
        db.set_sans_serif_family(SANS_FAMILY);
        db.set_serif_family(SANS_FAMILY);
        db.set_monospace_family(SANS_FAMILY);
        db.set_cursive_family(SANS_FAMILY);
        db.set_fantasy_family(SANS_FAMILY);
        Arc::new(db)
    })
    .clone()
}

/// Load an image source into pixels. Returns a short error string on failure so
/// the caller can fall back to the placeholder line.
pub(crate) fn load(source: &ImageSource) -> Result<DynamicImage, String> {
    match source {
        ImageSource::Svg(svg) => rasterize_svg(svg),
        ImageSource::Path(path) => {
            let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            if looks_like_svg(path, &bytes) {
                rasterize_svg(&String::from_utf8_lossy(&bytes))
            } else {
                image::load_from_memory(&bytes).map_err(|e| e.to_string())
            }
        }
    }
}

/// Decide whether bytes are SVG: a `.svg` extension, or markup that opens with
/// an XML prolog / `<svg` root (covers extensionless and mislabelled files).
fn looks_like_svg(path: &std::path::Path, bytes: &[u8]) -> bool {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        return true;
    }
    let head = &bytes[..bytes.len().min(512)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    head.starts_with("<?xml") || head.starts_with("<svg") || head.starts_with("<!DOCTYPE svg")
}

/// Rasterize SVG source to an RGBA image at its intrinsic size, using the
/// bundled fonts. The terminal protocol scales the result to the display area.
fn rasterize_svg(svg: &str) -> Result<DynamicImage, String> {
    let mut opt = usvg::Options {
        font_family: SANS_FAMILY.to_string(),
        ..Default::default()
    };
    opt.fontdb = fontdb();

    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| format!("parse svg: {e}"))?;
    let size = tree.size().to_int_size();
    let (w, h) = (size.width().max(1), size.height().max(1));

    let mut pixmap =
        tiny_skia::Pixmap::new(w, h).ok_or_else(|| format!("cannot allocate a {w}x{h} pixmap"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );

    // tiny-skia stores premultiplied RGBA; `image` wants straight alpha, so
    // demultiply each pixel (a no-op for opaque pixels).
    let mut buf = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        buf.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let rgba = image::RgbaImage::from_raw(w, h, buf)
        .ok_or_else(|| "rasterized buffer size mismatch".to_string())?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn rasterizes_svg_at_declared_size() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="24">
            <rect width="40" height="24" fill="#3050ff"/>
        </svg>"##;
        let img = rasterize_svg(svg).expect("rasterize");
        assert_eq!(img.dimensions(), (40, 24));
        // The fill color should survive (opaque blue somewhere in the middle).
        let px = img.get_pixel(20, 12);
        assert!(px[2] > px[0], "expected a blue-dominant pixel, got {px:?}");
        assert_eq!(
            px[3], 255,
            "opaque fill should stay opaque after demultiply"
        );
    }

    #[test]
    fn renders_svg_text_with_bundled_font() {
        // <text> forces the font path: with the bundled Liberation Sans in the
        // db this must parse and rasterize without touching system fonts.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40">
            <text x="4" y="24" font-family="Arial" font-size="20" fill="black">Hi</text>
        </svg>"#;
        let img = rasterize_svg(svg).expect("rasterize text");
        assert_eq!(img.dimensions(), (120, 40));
        // Some glyph pixels must be dark (the text drew), not a blank canvas.
        let dark = img
            .pixels()
            .any(|(_, _, p)| p[3] > 0 && p[0] < 128 && p[1] < 128 && p[2] < 128);
        assert!(dark, "expected rendered glyph pixels from the bundled font");
    }

    #[test]
    fn detects_svg_by_extension_and_content() {
        use std::path::Path;
        assert!(looks_like_svg(Path::new("d.svg"), b""));
        assert!(looks_like_svg(Path::new("d.SVG"), b""));
        assert!(looks_like_svg(Path::new("noext"), b"  <svg xmlns=...>"));
        assert!(looks_like_svg(
            Path::new("x"),
            b"<?xml version=\"1.0\"?><svg/>"
        ));
        assert!(!looks_like_svg(Path::new("photo.png"), b"\x89PNG"));
    }
}
