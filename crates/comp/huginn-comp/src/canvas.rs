//! The RGBA canvas the compositor-drawn shell composes into, and the buffer it
//! becomes.
//!
//! Shared by every panel the shell draws — the keybinding overlay, the
//! launcher, and the dock when it lands — so that they cannot drift on how a
//! glyph is blended or how a panel is placed.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

use huginn_core::geometry::Rect;

use crate::theme::Color;

/// An RGBA canvas the overlay is composed into before it becomes a buffer.
///
/// Composed at final size rather than drawn small and scaled up, so the
/// renderer never sees anything to interpolate. "Final size" includes the
/// output's density: on a 2× output a panel is composed with twice the pixels
/// in each direction and [`Panel::from_canvas`] marks the buffer as 2×, so it
/// lands on the panel pixel for pixel, the same as a 2× client's window.
pub(crate) struct Canvas {
    pub(crate) pixels: Vec<u8>,
    /// Width in pixels; the stride in bytes is this times four.
    pub(crate) stride: usize,
    pub(crate) height: usize,
}

impl Canvas {
    pub(crate) fn new(w: usize, h: usize) -> Self {
        Self {
            pixels: vec![0; w * h * 4],
            stride: w,
            height: h,
        }
    }

    /// Fill a rectangle, in pixels.
    pub(crate) fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.stride) {
                self.set(col, row, color);
            }
        }
    }

    /// Mix `color` into a rectangle at `alpha`, leaving the panel opaque.
    ///
    /// Distinct from [`Self::fill`], which *replaces* pixels including their
    /// alpha. Filling with a translucent colour does not tint what is beneath
    /// it — it punches a hole of that opacity through the panel, so the
    /// desktop shows through wherever the highlight is. A wash has to be
    /// blended in, exactly as a glyph's coverage is.
    pub(crate) fn tint(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color, alpha: u8) {
        let [r, g, b, _] = color.to_rgba_bytes();
        let mix = u32::from(alpha);
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.stride) {
                let offset = (row * self.stride + col) * 4;
                for (channel, value) in [r, g, b].into_iter().enumerate() {
                    let under = u32::from(self.pixels[offset + channel]);
                    self.pixels[offset + channel] = lerp(under, u32::from(value), mix, 255) as u8;
                }
                // Alpha is left alone: the panel's own opacity is what it is,
                // and a highlight must not make it more see-through.
            }
        }
    }

    /// Composite a premultiplied-RGBA image at `x`, `y`.
    ///
    /// Source-over with premultiplied source, which is what both rasterizers
    /// produce and what an icon's antialiased outline needs: un-premultiplying
    /// to blend and re-multiplying to store would lose precision on every
    /// partially-transparent edge, and on an icon that is most of the outline.
    pub(crate) fn blit(&mut self, x: usize, y: usize, image: &raven_desktop::Pixmap) {
        for row in 0..image.height as usize {
            for col in 0..image.width as usize {
                let (dx, dy) = (x + col, y + row);
                if dx >= self.stride || dy >= self.height {
                    continue;
                }
                let Some([sr, sg, sb, sa]) = image.pixel(col as u32, row as u32) else {
                    continue;
                };
                if sa == 0 {
                    continue;
                }
                let offset = (dy * self.stride + dx) * 4;
                let inverse = 255 - u32::from(sa);
                for (channel, source) in [sr, sg, sb].into_iter().enumerate() {
                    let under = u32::from(self.pixels[offset + channel]);
                    self.pixels[offset + channel] =
                        (u32::from(source) + under * inverse / 255).min(255) as u8;
                }
                let under = u32::from(self.pixels[offset + 3]);
                self.pixels[offset + 3] = (u32::from(sa) + under * inverse / 255).min(255) as u8;
            }
        }
    }

    /// Fill a rectangle with rounded corners, antialiased.
    ///
    /// The corners are covered by sampling distance from the corner's centre
    /// rather than by stepping a scanline: a hard cutoff gives a staircase that
    /// is plainly visible at the radii a floating panel uses, and a rounded
    /// rectangle with jagged corners looks worse than a square one.
    pub(crate) fn fill_rounded(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        radius: f32,
        color: Color,
    ) {
        let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        let (right, bottom) = ((x + w).min(self.stride), (y + h).min(self.height));
        // The columns the corners reach into, on either side. Everything
        // between is fully covered on every row, and a row clear of the
        // corners top and bottom is fully covered from edge to edge: the
        // distance is only worked out where an arc can pass, which for a
        // launcher-sized panel is a few percent of its pixels.
        let reach = radius.ceil() as usize;
        let (arc_left, arc_right) = ((x + reach).min(right), right.saturating_sub(reach).max(x));
        for row in y..bottom {
            let ly = (row - y) as f32 + 0.5;
            let dy = (radius - ly).max(ly - (h as f32 - radius)).max(0.0);
            if dy == 0.0 {
                self.blend_span(x, right, row, color, 255);
                continue;
            }
            let corner = |canvas: &mut Self, cols: std::ops::Range<usize>| {
                for col in cols {
                    let lx = (col - x) as f32 + 0.5;
                    // Distance past the corner arc, negative inside it.
                    let dx = (radius - lx).max(lx - (w as f32 - radius)).max(0.0);
                    let coverage = if dx == 0.0 {
                        1.0
                    } else {
                        (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0)
                    };
                    if coverage > 0.0 {
                        canvas.blend_over(col, row, color, (coverage * 255.0) as u8);
                    }
                }
            };
            corner(self, x..arc_left);
            if arc_left < arc_right {
                self.blend_span(arc_left, arc_right, row, color, 255);
            }
            corner(self, arc_left.max(arc_right)..right);
        }
    }

    /// Outline a rectangle with rounded corners, `width` pixels thick,
    /// antialiased on both edges.
    ///
    /// The stroke is the difference of two rounded rectangles — the shape and
    /// the same shape inset by `width` — evaluated per pixel as a signed
    /// distance, so the arcs and the straight runs get the same coverage
    /// ramp. Only the band a stroke can pass through is visited: the rows
    /// near the top and bottom edges in full, and for the rows between, the
    /// few columns at either side.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stroke_rounded(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        radius: f32,
        width: f32,
        color: Color,
    ) {
        if w == 0 || h == 0 || width <= 0.0 {
            return;
        }
        let (wf, hf) = (w as f32, h as f32);
        let radius = radius.min(wf / 2.0).min(hf / 2.0).max(0.0);
        let inner_r = (radius - width).max(0.0);
        let reach = (radius.max(width) + 1.5).ceil() as usize;
        let (right, bottom) = ((x + w).min(self.stride), (y + h).min(self.height));
        let side = (width + 1.5).ceil() as usize;
        for row in y..bottom {
            let ly = (row - y) as f32 + 0.5;
            let full = row < y + reach || row + reach >= y + h;
            let paint = |canvas: &mut Self, cols: std::ops::Range<usize>| {
                for col in cols {
                    let lx = (col - x) as f32 + 0.5;
                    let outer = rounded_coverage(lx, ly, wf, hf, radius);
                    let inner = rounded_coverage(
                        lx - width,
                        ly - width,
                        wf - width * 2.0,
                        hf - width * 2.0,
                        inner_r,
                    );
                    let coverage = (outer - inner).clamp(0.0, 1.0);
                    if coverage > 0.0 {
                        canvas.blend_over(col, row, color, (coverage * 255.0) as u8);
                    }
                }
            };
            if full {
                paint(self, x..right);
            } else {
                paint(self, x..(x + side).min(right));
                paint(self, right.saturating_sub(side).max(x)..right);
            }
        }
    }

    /// Paint the desktop's one material into a rectangle: the translucent
    /// ground, a hairline of light around it, and a brighter catch-light
    /// along the top edge between the corner arcs.
    ///
    /// Every floating panel goes through here — the dock, the launcher, the
    /// pinned panel, the overlay, a caption — which is what keeps them one
    /// surface rather than five that agree today. `alpha` is the ground's
    /// opacity; see [`crate::theme::PANEL_ALPHA`].
    pub(crate) fn material(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        radius: f32,
        alpha: u8,
    ) {
        use crate::theme;
        self.fill_rounded(x, y, w, h, radius, theme::BACKGROUND.with_alpha(alpha));
        self.stroke_rounded(x, y, w, h, radius, 1.0, theme::HAIRLINE);
        // The catch-light sits just inside the hairline, clear of the arcs,
        // so it reads as light on the top edge and not as a second border.
        let inset = radius.ceil() as usize;
        if h > 2 && w > inset * 2 {
            self.blend_span(x + inset, x + w - inset, y + 1, theme::CATCH_LIGHT, 255);
        }
    }

    /// [`Self::blend_over`] across the columns `from..to` of one row.
    fn blend_span(&mut self, from: usize, to: usize, row: usize, color: Color, alpha: u8) {
        for col in from..to {
            self.blend_over(col, row, color, alpha);
        }
    }

    /// Blend `color` over one pixel at `alpha`, alpha included.
    ///
    /// Distinct from [`Self::tint`], which leaves the destination alpha alone
    /// because it washes something already opaque. A rounded corner is drawn
    /// onto nothing, so its alpha has to accumulate or the panel has no edge.
    fn blend_over(&mut self, x: usize, y: usize, color: Color, alpha: u8) {
        if x >= self.stride || y >= self.height {
            return;
        }
        let offset = (y * self.stride + x) * 4;
        let [r, g, b, a] = color.to_rgba_bytes();
        // Coverage times the colour's own alpha, out of 255 × 255.
        let coverage = u32::from(alpha) * u32::from(a);
        for (channel, value) in [r, g, b].into_iter().enumerate() {
            let under = u32::from(self.pixels[offset + channel]);
            self.pixels[offset + channel] =
                lerp(under, u32::from(value), coverage, 255 * 255) as u8;
        }
        let under = u32::from(self.pixels[offset + 3]);
        self.pixels[offset + 3] = lerp(under, 255, coverage, 255 * 255) as u8;
    }

    /// Write one pixel, ignoring anything off the canvas.
    fn set(&mut self, col: usize, row: usize, color: [u8; 4]) {
        if col >= self.stride || row >= self.height {
            return;
        }
        let offset = (row * self.stride + col) * 4;
        self.pixels[offset..offset + 4].copy_from_slice(&color);
    }
}

impl crate::text::Surface for Canvas {
    /// Blend a glyph's coverage over the background already painted.
    ///
    /// Antialiasing is the whole point of moving off a bitmap font, and it only
    /// works if partial coverage is *mixed* with what is underneath. Writing
    /// the colour at full strength wherever coverage is non-zero gives back
    /// exactly the hard one-bit edges this replaced.
    fn blend(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        if x < 0 || y < 0 || x as usize >= self.stride || y as usize >= self.height {
            return;
        }
        let offset = (y as usize * self.stride + x as usize) * 4;
        let [r, g, b, _] = color.to_rgba_bytes();
        let coverage = u32::from(alpha);
        for (channel, value) in [r, g, b].into_iter().enumerate() {
            let under = u32::from(self.pixels[offset + channel]);
            self.pixels[offset + channel] = lerp(under, u32::from(value), coverage, 255) as u8;
        }
        // Text is opaque where it covers, so the alpha channel takes the
        // greater of what is there and the coverage — otherwise a glyph drawn
        // on the translucent background would punch a hole through it.
        let existing = self.pixels[offset + 3];
        self.pixels[offset + 3] = existing.max(alpha);
    }
}

/// How much of the pixel centred at (`lx`, `ly`) a `w`×`h` rectangle with
/// corners of `radius`, its top-left at the origin, covers: 1 inside, 0
/// outside, and a one-pixel ramp across the edge.
///
/// A signed distance to the rounded box, which handles the arcs and the
/// straight runs with one formula, so a stroke built from two of these has
/// the same weight all the way round.
fn rounded_coverage(lx: f32, ly: f32, w: f32, h: f32, radius: f32) -> f32 {
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let (cx, cy) = (lx - w / 2.0, ly - h / 2.0);
    let (qx, qy) = (cx.abs() - (w / 2.0 - radius), cy.abs() - (h / 2.0 - radius));
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    let inside = qx.max(qy).min(0.0);
    let distance = outside + inside - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

/// `from` moved towards `to` by `amount` parts in `whole`, rounded.
///
/// Integer throughout: a panel is a million or so pixels at 2×, blended
/// several times over per keystroke, and the float version with its round
/// at the end was the most expensive thing about drawing one. The result
/// is within one of the float answer, which is below what the eye can tell
/// apart and what the antialiasing was already rounding away.
fn lerp(from: u32, to: u32, amount: u32, whole: u32) -> u32 {
    (from * (whole - amount) + to * amount + whole / 2) / whole
}

/// A finished panel: its pixels, and the logical size it occupies.
#[derive(Debug)]
pub(crate) struct Panel {
    pub buffer: MemoryRenderBuffer,
    width: i32,
    height: i32,
}

impl Panel {
    /// Turn a composed canvas into something the renderer can draw.
    ///
    /// `density` is the integer scale the canvas was composed at — the
    /// output's advertised scale — and becomes the buffer's scale, so the
    /// panel's logical size is the canvas divided by it. Rounded up: a canvas
    /// an odd pixel wide at 2× would otherwise lose half a pixel off its edge.
    pub(crate) fn from_canvas(canvas: &Canvas, density: u32) -> Self {
        let density = density.max(1) as usize;
        let (width, height) = (
            canvas.stride.div_ceil(density) as i32,
            canvas.height.div_ceil(density) as i32,
        );
        Self {
            // Not quite opaque — the background carries a little alpha so the
            // desktop shows faintly through — so no opaque region is claimed.
            // Promising opacity the alpha channel does not deliver leaves
            // whatever is behind the panel unpainted.
            buffer: MemoryRenderBuffer::from_slice(
                &canvas.pixels,
                // Bytes are R,G,B,A in that order, which is DRM's ABGR8888 on
                // a little-endian machine. Reading this as Argb8888 tints the
                // whole panel blue and puts the alpha in the wrong place.
                Fourcc::Abgr8888,
                (canvas.stride as i32, canvas.height as i32),
                density as i32,
                Transform::Normal,
                None,
            ),
            width,
            height,
        }
    }

    pub(crate) fn buffer(&self) -> &MemoryRenderBuffer {
        &self.buffer
    }

    /// The panel's size in logical pixels.
    pub(crate) fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// Centred on the output, and never off the top or left edge if it happens
    /// to be larger than the screen.
    pub(crate) fn centred_on(&self, output: Rect) -> Rect {
        let x = output.x() + (output.w() - self.width).max(0) / 2;
        let y = output.y() + (output.h() - self.height).max(0) / 2;
        Rect::from_xywh(x, y, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_coverage_is_full_inside_and_empty_outside() {
        assert_eq!(rounded_coverage(10.0, 10.0, 40.0, 30.0, 8.0), 1.0);
        assert_eq!(rounded_coverage(-5.0, 10.0, 40.0, 30.0, 8.0), 0.0);
        // The very corner of the bounding box lies outside the arc.
        assert_eq!(rounded_coverage(0.5, 0.5, 40.0, 30.0, 8.0), 0.0);
        // A straight edge gets a one-pixel ramp centred on the boundary.
        let edge = rounded_coverage(20.0, 0.0, 40.0, 30.0, 8.0);
        assert!((edge - 0.5).abs() < 0.01, "edge coverage was {edge}");
    }

    #[test]
    fn stroke_paints_the_edge_and_leaves_the_middle_clear() {
        let mut canvas = Canvas::new(40, 30);
        canvas.stroke_rounded(0, 0, 40, 30, 6.0, 1.0, Color::from_argb(0xFFFF_FFFF));
        let alpha = |x: usize, y: usize| canvas.pixels[(y * 40 + x) * 4 + 3];
        assert!(alpha(20, 0) > 200, "top edge was {}", alpha(20, 0));
        assert!(alpha(0, 15) > 200, "left edge was {}", alpha(0, 15));
        assert_eq!(alpha(20, 15), 0, "the middle must stay clear");
        assert_eq!(alpha(20, 3), 0, "one pixel in from the edge must stay clear");
        // The corner pixel is outside the arc and gets nothing.
        assert_eq!(alpha(0, 0), 0);
    }

    #[test]
    fn material_is_opaque_where_the_ground_is_and_lit_along_the_top() {
        let mut canvas = Canvas::new(60, 40);
        canvas.material(0, 0, 60, 40, 8.0, 0xD8);
        let px = |x: usize, y: usize| {
            let o = (y * 60 + x) * 4;
            [
                canvas.pixels[o],
                canvas.pixels[o + 1],
                canvas.pixels[o + 2],
                canvas.pixels[o + 3],
            ]
        };
        // The ground carries the panel alpha.
        assert_eq!(px(30, 20)[3], 0xD8);
        // The catch-light row is brighter than the ground beneath it.
        assert!(px(30, 1)[0] > px(30, 20)[0]);
        // Outside the arc nothing is painted.
        assert_eq!(px(0, 0)[3], 0);
    }
}
