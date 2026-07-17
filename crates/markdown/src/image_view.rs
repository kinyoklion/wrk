//! Fullscreen zoom/pan image viewer (the `images` feature).
//!
//! Opened from the inline markdown view to inspect a single image. Zoom and pan
//! are rendered by cropping the sub-rectangle of the *original* image that maps
//! to the viewport and fit-drawing just that crop, so quality is preserved and
//! the encoded protocol is rebuilt only when the zoom/pan/area actually change.

use image::{DynamicImage, GenericImageView};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget;
use ratatui_image::{Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol};

use crate::block::ImageSource;

/// Upper zoom bound (× the fit scale).
const MAX_ZOOM: f32 = 32.0;

/// A single image shown fullscreen with zoom and pan.
pub struct ImageViewer {
    img: DynamicImage,
    /// `1.0` = fit the whole image to the viewport; larger = zoomed in.
    zoom: f32,
    /// View center in normalized image coordinates (`0.0..=1.0`).
    cx: f32,
    cy: f32,
    /// Fraction of the image visible on each axis at the last render — used to
    /// scale keyboard/drag pan steps to the current zoom.
    vis_norm: (f32, f32),
    /// Cached protocol and the (area, zoom, pan) key it was built for.
    proto: Option<StatefulProtocol>,
    key: Option<(u16, u16, u32, u32, u32)>,
}

impl ImageViewer {
    /// Decode `source` and open a viewer fit to the viewport. `None` if the
    /// image can't be decoded.
    pub fn open(source: &ImageSource) -> Option<Self> {
        let img = crate::image::load(source).ok()?;
        Some(Self {
            img,
            zoom: 1.0,
            cx: 0.5,
            cy: 0.5,
            vis_norm: (1.0, 1.0),
            proto: None,
            key: None,
        })
    }

    /// Multiply the zoom by `factor`, clamped to `[1.0, MAX_ZOOM]`.
    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(1.0, MAX_ZOOM);
    }

    /// Pan by a fraction of the currently visible view (`dx`/`dy` in view
    /// widths/heights): positive moves toward the image's right/bottom. The
    /// render clamps to keep the crop inside the image.
    pub fn pan_view(&mut self, dx: f32, dy: f32) {
        self.cx += dx * self.vis_norm.0;
        self.cy += dy * self.vis_norm.1;
    }

    /// Reset to the fit view (zoom 1, centered).
    pub fn reset(&mut self) {
        self.zoom = 1.0;
        self.cx = 0.5;
        self.cy = 0.5;
    }

    /// Current zoom factor (`1.0` = fit).
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Render the image cropped and fit to `area`. The protocol is re-encoded
    /// only when the zoom, pan, or area changed since the last call, so holding
    /// still is cheap.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, picker: &Picker) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let fs = picker.font_size();
        let (cw, ch) = (f32::from(fs.width), f32::from(fs.height));
        let area_pw = f32::from(area.width) * cw;
        let area_ph = f32::from(area.height) * ch;
        let (iw, ih) = self.img.dimensions();
        let (iwf, ihf) = (iw as f32, ih as f32);

        // Fit scale, then the zoomed scale; from it, how much of the source is
        // visible (never more than the whole image).
        let s0 = (area_pw / iwf).min(area_ph / ihf);
        let s = s0 * self.zoom;
        let vw = (area_pw / s).clamp(1.0, iwf);
        let vh = (area_ph / s).clamp(1.0, ihf);
        self.vis_norm = (vw / iwf, vh / ihf);

        // Clamp the center so the visible crop stays within the image bounds.
        let (hx, hy) = (self.vis_norm.0 / 2.0, self.vis_norm.1 / 2.0);
        self.cx = self.cx.clamp(hx, 1.0 - hx);
        self.cy = self.cy.clamp(hy, 1.0 - hy);

        let x0 = (self.cx * iwf - vw / 2.0).clamp(0.0, iwf - vw);
        let y0 = (self.cy * ihf - vh / 2.0).clamp(0.0, ihf - vh);

        let key = (
            area.width,
            area.height,
            self.zoom.to_bits(),
            self.cx.to_bits(),
            self.cy.to_bits(),
        );
        if self.key != Some(key) {
            let cw_px = (vw.ceil() as u32).min(iw.saturating_sub(x0 as u32)).max(1);
            let ch_px = (vh.ceil() as u32).min(ih.saturating_sub(y0 as u32)).max(1);
            let crop = self.img.crop_imm(x0 as u32, y0 as u32, cw_px, ch_px);
            self.proto = Some(picker.new_resize_protocol(crop));
            self.key = Some(key);
        }
        if let Some(proto) = self.proto.as_mut() {
            // `Scale` (unlike `Fit`) upscales the crop to fill the viewport, so
            // zooming in actually magnifies the image instead of shrinking the
            // rendered region to the crop's natural pixel size.
            StatefulImage::default()
                .resize(Resize::Scale(None))
                .render(area, buf, proto);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::ImageSource;

    fn viewer() -> ImageViewer {
        // 100×50 solid image.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
            <rect width="100" height="50" fill="#3050ff"/></svg>"##;
        ImageViewer::open(&ImageSource::Svg(svg.into())).expect("decode")
    }

    #[test]
    fn zoom_clamps_to_range() {
        let mut v = viewer();
        assert_eq!(v.zoom(), 1.0);
        for _ in 0..40 {
            v.zoom_by(2.0);
        }
        assert_eq!(v.zoom(), MAX_ZOOM, "zoom clamps at the max");
        for _ in 0..40 {
            v.zoom_by(0.5);
        }
        assert_eq!(v.zoom(), 1.0, "zoom clamps at fit (1.0)");
    }

    #[test]
    fn pan_is_centered_and_clamped_at_fit() {
        let mut v = viewer();
        // Render once so vis_norm/geometry are populated.
        let picker = crate::Picker::halfblocks();
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf, &picker);
        // At fit the whole image shows, so any pan snaps back to centered.
        v.pan_view(0.5, 0.5);
        v.render(area, &mut buf, &picker);
        assert!((v.cx - 0.5).abs() < 1e-3 && (v.cy - 0.5).abs() < 1e-3);
    }

    #[test]
    fn zoomed_view_fills_the_viewport() {
        use ratatui::style::Color;
        // A deep zoom's crop has the viewport aspect, so `Scale` must upscale it
        // to paint (nearly) every cell — not shrink to the crop's pixel size.
        let mut v = viewer();
        let picker = crate::Picker::halfblocks();
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        v.zoom_by(8.0);
        v.render(area, &mut buf, &picker);
        let painted = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = &buf[(x, y)];
                matches!(c.fg, Color::Rgb(..)) || matches!(c.bg, Color::Rgb(..))
            })
            .count();
        let total = usize::from(area.width) * usize::from(area.height);
        assert!(
            painted * 10 >= total * 9,
            "zoomed image should fill ~all the viewport, painted {painted}/{total}"
        );
    }

    #[test]
    fn pan_moves_when_zoomed_but_stays_in_bounds() {
        let mut v = viewer();
        let picker = crate::Picker::halfblocks();
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        v.zoom_by(4.0);
        v.render(area, &mut buf, &picker);
        // Pan hard right; center moves off 0.5 but never past the edge bound.
        for _ in 0..20 {
            v.pan_view(1.0, 0.0);
            v.render(area, &mut buf, &picker);
        }
        assert!(v.cx > 0.5, "panning right moved the view");
        assert!(
            v.cx <= 1.0 - v.vis_norm.0 / 2.0 + 1e-3,
            "clamped to the edge"
        );
    }
}
