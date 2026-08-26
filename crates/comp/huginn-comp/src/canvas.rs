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
        let mix = f32::from(alpha) / 255.0;
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.stride) {
                let offset = (row * self.stride + col) * 4;
                for (channel, value) in [r, g, b].into_iter().enumerate() {
                    let under = f32::from(self.pixels[offset + channel]);
                    self.pixels[offset + channel] =
                        (under + (f32::from(value) - under) * mix).round() as u8;
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
                self.pixels[offset + 3] =
                    (u32::from(sa) + under * inverse / 255).min(255) as u8;
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
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.stride) {
                let (lx, ly) = ((col - x) as f32 + 0.5, (row - y) as f32 + 0.5);
                // Distance past the corner arc, negative inside it.
                let dx = (radius - lx).max(lx - (w as f32 - radius)).max(0.0);
                let dy = (radius - ly).max(ly - (h as f32 - radius)).max(0.0);
                let coverage = if dx == 0.0 || dy == 0.0 {
                    1.0
                } else {
                    (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0)
                };
                if coverage > 0.0 {
                    self.blend_over(col, row, color, (coverage * 255.0) as u8);
                }
            }
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
        let coverage = f32::from(alpha) / 255.0 * (f32::from(a) / 255.0);
        for (channel, value) in [r, g, b].into_iter().enumerate() {
            let under = f32::from(self.pixels[offset + channel]);
            self.pixels[offset + channel] =
                (under + (f32::from(value) - under) * coverage).round() as u8;
        }
        let under = f32::from(self.pixels[offset + 3]);
        self.pixels[offset + 3] =
            (under + (255.0 - under) * coverage).round() as u8;
    }

    /// A one-pixel border around the whole canvas, so the panel has an edge
    /// rather than bleeding into whatever is behind it.
    pub(crate) fn frame(&mut self, color: [u8; 4]) {
        for col in 0..self.stride {
            self.set(col, 0, color);
            self.set(col, self.height - 1, color);
        }
        for row in 0..self.height {
            self.set(0, row, color);
            self.set(self.stride - 1, row, color);
        }
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
        let coverage = f32::from(alpha) / 255.0;
        for (channel, value) in [r, g, b].into_iter().enumerate() {
            let under = f32::from(self.pixels[offset + channel]);
            self.pixels[offset + channel] =
                (under + (f32::from(value) - under) * coverage).round() as u8;
        }
        // Text is opaque where it covers, so the alpha channel takes the
        // greater of what is there and the coverage — otherwise a glyph drawn
        // on the translucent background would punch a hole through it.
        let existing = self.pixels[offset + 3];
        self.pixels[offset + 3] = existing.max(alpha);
    }
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
