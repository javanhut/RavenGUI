//! The keybinding overlay, and the bitmap font that draws it.
//!
//! A compositor is the one program on the desktop that cannot ask anything else
//! to draw for it. The panel is a separate process on purpose — a crashed
//! Muninn costs you the panel and nothing else — but that same separation is
//! why the keybinding list cannot live there. If you are lost enough to need
//! the list, the shell being dead is one of the likelier reasons, and a help
//! screen that disappears exactly when it is needed is worse than none.
//!
//! So Huginn draws it, which means Huginn needs glyphs. Rather than take on a
//! font stack and a runtime dependency on a font actually being installed, the
//! overlay carries a 5x7 ASCII bitmap font of its own: about a hundred lines of
//! data that render identically on every machine and cannot fail to load. It is
//! scaled by whole pixels, so it stays crisp instead of going soft the way a
//! filtered bitmap would.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

use huginn_core::geometry::Rect;

use crate::backend::keymap::{BINDINGS, Binding};

/// Glyph cell, in font pixels. The 8th row exists for the descenders on `g`,
/// `j`, `p`, `q` and `y`; nothing else reaches into it.
const GLYPH_W: usize = 5;
const GLYPH_H: usize = 8;
/// One blank column between glyphs, so text does not run together.
const ADVANCE: usize = GLYPH_W + 1;

/// Padding inside the panel, and the gap between the two columns, in font
/// pixels before scaling.
const PAD: usize = 8;
const COLUMN_GAP: usize = 4;
/// Blank rows between one binding and the next.
const LINE_GAP: usize = 3;
/// Largest whole-pixel scale the overlay is drawn at. Three is comfortable on
/// a 1080p screen; beyond that the panel starts to dominate rather than inform.
const MAX_SCALE: usize = 3;

// Palette, matching Muninn's so the two read as one desktop rather than as two
// programs that happen to share a screen.
const BG: [u8; 4] = [0x16, 0x16, 0x1F, 0xF2];
const BORDER: [u8; 4] = [0x2A, 0x2A, 0x3A, 0xFF];
const ACCENT: [u8; 4] = [0x7A, 0xA2, 0xF7, 0xFF];
const TEXT: [u8; 4] = [0xD0, 0xD0, 0xE0, 0xFF];
const DIM: [u8; 4] = [0x8A, 0x8A, 0xA0, 0xFF];

const TITLE: &str = "Huginn keybindings";
const FOOTER: &str = "Super+Shift+H closes this. Plain Super belongs to the focused application.";

/// The overlay's pixels, and the size they were drawn at.
#[derive(Debug)]
pub(crate) struct Overlay {
    pub buffer: MemoryRenderBuffer,
    width: i32,
    height: i32,
}

impl Overlay {
    /// Draw the overlay, sized to sit comfortably on `output`.
    pub(crate) fn render(output: Rect) -> Self {
        let canvas = compose(output);
        let (width, height) = (canvas.stride as i32, canvas.height as i32);
        Self {
            // Not quite opaque — the background carries a little alpha so the
            // desktop shows faintly through — so no opaque region is claimed.
            // Promising opacity the alpha channel does not deliver leaves
            // whatever is behind the panel unpainted.
            buffer: MemoryRenderBuffer::from_slice(
                &canvas.pixels,
                // Bytes are R,G,B,A in that order, which is DRM's ABGR8888 on a
                // little-endian machine. Reading this as Argb8888 tints the
                // whole panel blue and puts the alpha in the wrong place.
                Fourcc::Abgr8888,
                (width, height),
                1,
                Transform::Normal,
                None,
            ),
            width,
            height,
        }
    }

    /// Where the overlay goes: centred on the output, and never off the top or
    /// left edge if it happens to be larger than the screen.
    pub(crate) fn placement(&self, output: Rect) -> Rect {
        let x = output.x() + (output.w() - self.width).max(0) / 2;
        let y = output.y() + (output.h() - self.height).max(0) / 2;
        Rect::from_xywh(x, y, self.width, self.height)
    }
}

/// Lay the panel out and paint it. Split from [`Overlay::render`] so a test can
/// get at the pixels without going through a renderer.
fn compose(output: Rect) -> Canvas {
    let chord_w = BINDINGS.iter().map(|b| text_width(b.chord)).max().unwrap_or(0);
    let body_w = BINDINGS
        .iter()
        .map(|b| chord_w + COLUMN_GAP * ADVANCE + text_width(b.description))
        .chain([text_width(TITLE), text_width(FOOTER)])
        .max()
        .unwrap_or(0);

    let w = body_w + PAD * 2;
    // Title, a rule under it, every binding, then the footer — each with
    // its own gap below.
    let rows = BINDINGS.len();
    let h = PAD
        + GLYPH_H + LINE_GAP          // title
        + 1 + LINE_GAP                // rule
        + rows * (GLYPH_H + LINE_GAP) // bindings
        + LINE_GAP
        + GLYPH_H                     // footer
        + PAD;

    // The largest whole-pixel scale that still fits on the screen, with a
    // margin so the panel reads as sitting on the desktop rather than as
    // having been jammed into it.
    //
    // Whole pixels only: a 5x7 font at a fractional scale is a smeared 5x7
    // font, where doubling it is just a bigger one. And both axes have to be
    // consulted — picking the scale from the height alone puts a panel wider
    // than the screen on a 1280x1024 monitor, where there is height to spare
    // and no width at all.
    let margin = 16;
    let fits = |s: usize| {
        (w * s) as i32 + margin <= output.w() && (h * s) as i32 + margin <= output.h()
    };
    let scale = (1..=MAX_SCALE).rev().find(|s| fits(*s)).unwrap_or(1);

    let mut canvas = Canvas::new(w, h, scale);
    canvas.fill(0, 0, w, h, BG);
    canvas.frame(BORDER);

    let mut y = PAD;
    canvas.text(PAD, y, TITLE, ACCENT);
    y += GLYPH_H + LINE_GAP;
    canvas.fill(PAD, y, body_w, 1, BORDER);
    y += 1 + LINE_GAP;

    for binding in BINDINGS {
        row(&mut canvas, PAD, y, chord_w, binding);
        y += GLYPH_H + LINE_GAP;
    }
    y += LINE_GAP;
    canvas.text(PAD, y, FOOTER, DIM);

    canvas
}

/// One binding: chord in the accent colour, description beside it.
fn row(canvas: &mut Canvas, x: usize, y: usize, chord_w: usize, binding: &Binding) {
    canvas.text(x, y, binding.chord, ACCENT);
    canvas.text(x + chord_w + COLUMN_GAP * ADVANCE, y, binding.description, TEXT);
}

/// Width of `text` in unscaled font pixels, without the trailing blank column.
fn text_width(text: &str) -> usize {
    (text.chars().count() * ADVANCE).saturating_sub(1)
}

/// An RGBA canvas that writes every pixel `scale` times in each direction.
///
/// Scaling on the way in rather than on the way out is what keeps the glyphs
/// square-edged: the renderer never sees anything to interpolate.
struct Canvas {
    pixels: Vec<u8>,
    /// Width in scaled pixels; the stride is this times four.
    stride: usize,
    height: usize,
    scale: usize,
}

impl Canvas {
    fn new(w: usize, h: usize, scale: usize) -> Self {
        Self {
            pixels: vec![0; w * scale * h * scale * 4],
            stride: w * scale,
            height: h * scale,
            scale,
        }
    }

    /// Fill a rectangle given in unscaled font pixels.
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: [u8; 4]) {
        for row in y * self.scale..(y + h) * self.scale {
            for col in x * self.scale..(x + w) * self.scale {
                self.set(col, row, color);
            }
        }
    }

    /// A one-pixel border around the whole canvas, so the panel has an edge
    /// rather than bleeding into whatever is behind it.
    fn frame(&mut self, color: [u8; 4]) {
        for col in 0..self.stride {
            self.set(col, 0, color);
            self.set(col, self.height - 1, color);
        }
        for row in 0..self.height {
            self.set(0, row, color);
            self.set(self.stride - 1, row, color);
        }
    }

    /// Draw `text` with its top-left at unscaled `(x, y)`.
    fn text(&mut self, x: usize, y: usize, text: &str, color: [u8; 4]) {
        for (index, ch) in text.chars().enumerate() {
            self.glyph(x + index * ADVANCE, y, ch, color);
        }
    }

    fn glyph(&mut self, x: usize, y: usize, ch: char, color: [u8; 4]) {
        // Anything outside the font's range is left blank rather than drawn as
        // a substitute. A row of boxes is harder to read past than a gap, and
        // `binding_text_is_ascii` keeps it from happening in the first place.
        let Some(rows) = glyph(ch) else { return };
        for (dy, bits) in rows.iter().enumerate() {
            for dx in 0..GLYPH_W {
                // Bit GLYPH_W-1 is the leftmost column.
                if bits & (1 << (GLYPH_W - 1 - dx)) != 0 {
                    self.fill(x + dx, y + dy, 1, 1, color);
                }
            }
        }
    }

    /// Write one scaled pixel, ignoring anything off the canvas.
    fn set(&mut self, col: usize, row: usize, color: [u8; 4]) {
        if col >= self.stride || row >= self.height {
            return;
        }
        let offset = (row * self.stride + col) * 4;
        self.pixels[offset..offset + 4].copy_from_slice(&color);
    }
}

/// The bitmap for `ch`, or `None` if the font has no glyph for it.
fn glyph(ch: char) -> Option<&'static [u8; GLYPH_H]> {
    let index = (ch as usize).checked_sub(0x20)?;
    FONT.get(index)
}

/// A 5x7 ASCII font, one byte per row, low five bits, leftmost column first.
///
/// Covers printable ASCII from space to `~`. Written out rather than loaded so
/// the compositor's help screen cannot be taken away by a missing font package.
#[rustfmt::skip]
const FONT: [[u8; GLYPH_H]; 95] = [
    [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00], // space
    [0x04,0x04,0x04,0x04,0x04,0x00,0x04,0x00], // !
    [0x0A,0x0A,0x00,0x00,0x00,0x00,0x00,0x00], // "
    [0x0A,0x0A,0x1F,0x0A,0x1F,0x0A,0x0A,0x00], // #
    [0x04,0x0F,0x14,0x0E,0x05,0x1E,0x04,0x00], // $
    [0x18,0x19,0x02,0x04,0x08,0x13,0x03,0x00], // %
    [0x0C,0x12,0x14,0x08,0x15,0x12,0x0D,0x00], // &
    [0x04,0x04,0x00,0x00,0x00,0x00,0x00,0x00], // '
    [0x02,0x04,0x08,0x08,0x08,0x04,0x02,0x00], // (
    [0x08,0x04,0x02,0x02,0x02,0x04,0x08,0x00], // )
    [0x00,0x04,0x15,0x0E,0x15,0x04,0x00,0x00], // *
    [0x00,0x04,0x04,0x1F,0x04,0x04,0x00,0x00], // +
    [0x00,0x00,0x00,0x00,0x00,0x04,0x04,0x08], // ,
    [0x00,0x00,0x00,0x1F,0x00,0x00,0x00,0x00], // -
    [0x00,0x00,0x00,0x00,0x00,0x0C,0x0C,0x00], // .
    [0x00,0x01,0x02,0x04,0x08,0x10,0x00,0x00], // /
    [0x0E,0x11,0x13,0x15,0x19,0x11,0x0E,0x00], // 0
    [0x04,0x0C,0x04,0x04,0x04,0x04,0x0E,0x00], // 1
    [0x0E,0x11,0x01,0x02,0x04,0x08,0x1F,0x00], // 2
    [0x1F,0x02,0x04,0x02,0x01,0x11,0x0E,0x00], // 3
    [0x02,0x06,0x0A,0x12,0x1F,0x02,0x02,0x00], // 4
    [0x1F,0x10,0x1E,0x01,0x01,0x11,0x0E,0x00], // 5
    [0x06,0x08,0x10,0x1E,0x11,0x11,0x0E,0x00], // 6
    [0x1F,0x01,0x02,0x04,0x08,0x08,0x08,0x00], // 7
    [0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E,0x00], // 8
    [0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C,0x00], // 9
    [0x00,0x0C,0x0C,0x00,0x0C,0x0C,0x00,0x00], // :
    [0x00,0x0C,0x0C,0x00,0x0C,0x04,0x08,0x00], // ;
    [0x02,0x04,0x08,0x10,0x08,0x04,0x02,0x00], // <
    [0x00,0x00,0x1F,0x00,0x1F,0x00,0x00,0x00], // =
    [0x08,0x04,0x02,0x01,0x02,0x04,0x08,0x00], // >
    [0x0E,0x11,0x01,0x02,0x04,0x00,0x04,0x00], // ?
    [0x0E,0x11,0x17,0x15,0x17,0x10,0x0E,0x00], // @
    [0x0E,0x11,0x11,0x1F,0x11,0x11,0x11,0x00], // A
    [0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E,0x00], // B
    [0x0E,0x11,0x10,0x10,0x10,0x11,0x0E,0x00], // C
    [0x1E,0x11,0x11,0x11,0x11,0x11,0x1E,0x00], // D
    [0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F,0x00], // E
    [0x1F,0x10,0x10,0x1E,0x10,0x10,0x10,0x00], // F
    [0x0E,0x11,0x10,0x17,0x11,0x11,0x0F,0x00], // G
    [0x11,0x11,0x11,0x1F,0x11,0x11,0x11,0x00], // H
    [0x0E,0x04,0x04,0x04,0x04,0x04,0x0E,0x00], // I
    [0x07,0x02,0x02,0x02,0x02,0x12,0x0C,0x00], // J
    [0x11,0x12,0x14,0x18,0x14,0x12,0x11,0x00], // K
    [0x10,0x10,0x10,0x10,0x10,0x10,0x1F,0x00], // L
    [0x11,0x1B,0x15,0x15,0x11,0x11,0x11,0x00], // M
    [0x11,0x11,0x19,0x15,0x13,0x11,0x11,0x00], // N
    [0x0E,0x11,0x11,0x11,0x11,0x11,0x0E,0x00], // O
    [0x1E,0x11,0x11,0x1E,0x10,0x10,0x10,0x00], // P
    [0x0E,0x11,0x11,0x11,0x15,0x12,0x0D,0x00], // Q
    [0x1E,0x11,0x11,0x1E,0x14,0x12,0x11,0x00], // R
    [0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E,0x00], // S
    [0x1F,0x04,0x04,0x04,0x04,0x04,0x04,0x00], // T
    [0x11,0x11,0x11,0x11,0x11,0x11,0x0E,0x00], // U
    [0x11,0x11,0x11,0x11,0x11,0x0A,0x04,0x00], // V
    [0x11,0x11,0x11,0x15,0x15,0x15,0x0A,0x00], // W
    [0x11,0x11,0x0A,0x04,0x0A,0x11,0x11,0x00], // X
    [0x11,0x11,0x0A,0x04,0x04,0x04,0x04,0x00], // Y
    [0x1F,0x01,0x02,0x04,0x08,0x10,0x1F,0x00], // Z
    [0x0E,0x08,0x08,0x08,0x08,0x08,0x0E,0x00], // [
    [0x00,0x10,0x08,0x04,0x02,0x01,0x00,0x00], // backslash
    [0x0E,0x02,0x02,0x02,0x02,0x02,0x0E,0x00], // ]
    [0x04,0x0A,0x11,0x00,0x00,0x00,0x00,0x00], // ^
    [0x00,0x00,0x00,0x00,0x00,0x00,0x1F,0x00], // _
    [0x08,0x04,0x00,0x00,0x00,0x00,0x00,0x00], // `
    [0x00,0x00,0x0E,0x01,0x0F,0x11,0x0F,0x00], // a
    [0x10,0x10,0x1E,0x11,0x11,0x11,0x1E,0x00], // b
    [0x00,0x00,0x0E,0x11,0x10,0x11,0x0E,0x00], // c
    [0x01,0x01,0x0F,0x11,0x11,0x11,0x0F,0x00], // d
    [0x00,0x00,0x0E,0x11,0x1F,0x10,0x0E,0x00], // e
    [0x06,0x09,0x08,0x1C,0x08,0x08,0x08,0x00], // f
    [0x00,0x00,0x0F,0x11,0x11,0x0F,0x01,0x0E], // g
    [0x10,0x10,0x1E,0x11,0x11,0x11,0x11,0x00], // h
    [0x04,0x00,0x0C,0x04,0x04,0x04,0x0E,0x00], // i
    [0x02,0x00,0x06,0x02,0x02,0x02,0x12,0x0C], // j
    [0x10,0x10,0x12,0x14,0x18,0x14,0x12,0x00], // k
    [0x0C,0x04,0x04,0x04,0x04,0x04,0x0E,0x00], // l
    [0x00,0x00,0x1A,0x15,0x15,0x15,0x15,0x00], // m
    [0x00,0x00,0x1E,0x11,0x11,0x11,0x11,0x00], // n
    [0x00,0x00,0x0E,0x11,0x11,0x11,0x0E,0x00], // o
    [0x00,0x00,0x1E,0x11,0x11,0x1E,0x10,0x10], // p
    [0x00,0x00,0x0F,0x11,0x11,0x0F,0x01,0x01], // q
    [0x00,0x00,0x16,0x19,0x10,0x10,0x10,0x00], // r
    [0x00,0x00,0x0F,0x10,0x0E,0x01,0x1E,0x00], // s
    [0x08,0x08,0x1C,0x08,0x08,0x09,0x06,0x00], // t
    [0x00,0x00,0x11,0x11,0x11,0x13,0x0D,0x00], // u
    [0x00,0x00,0x11,0x11,0x11,0x0A,0x04,0x00], // v
    [0x00,0x00,0x11,0x15,0x15,0x15,0x0A,0x00], // w
    [0x00,0x00,0x11,0x0A,0x04,0x0A,0x11,0x00], // x
    [0x00,0x00,0x11,0x11,0x11,0x0F,0x01,0x0E], // y
    [0x00,0x00,0x1F,0x02,0x04,0x08,0x1F,0x00], // z
    [0x06,0x08,0x08,0x10,0x08,0x08,0x06,0x00], // {
    [0x04,0x04,0x04,0x04,0x04,0x04,0x04,0x00], // |
    [0x0C,0x02,0x02,0x01,0x02,0x02,0x0C,0x00], // }
    [0x00,0x00,0x08,0x15,0x02,0x00,0x00,0x00], // ~
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_centred_on_the_output() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let overlay = Overlay::render(output);
        let at = overlay.placement(output);
        // Integer division leaves an odd screen a pixel wider on one side.
        assert!((output.w() - (at.x() * 2 + at.w())).abs() <= 1, "not horizontally centred");
        assert!((output.h() - (at.y() * 2 + at.h())).abs() <= 1, "not vertically centred");
        assert!(at.w() <= output.w() && at.h() <= output.h(), "wider than the screen");
    }

    #[test]
    fn it_is_placed_relative_to_the_output_not_the_origin() {
        // A second monitor's area does not start at (0, 0), and an overlay
        // centred on the origin would appear on the wrong screen. 1280x1024
        // also has height to spare and none to waste on width, which is what
        // catches a scale picked from one axis.
        let output = Rect::from_xywh(1920, 0, 1280, 1024);
        let overlay = Overlay::render(output);
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.right() <= output.right());
    }

    #[test]
    fn an_output_smaller_than_the_overlay_still_shows_its_top_left() {
        // Better a clipped list anchored at the corner than one centred so far
        // negative that the beginning of every line is off screen.
        let output = Rect::from_xywh(0, 0, 320, 200);
        let overlay = Overlay::render(output);
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.y() >= output.y());
    }

    #[test]
    fn the_font_covers_printable_ascii() {
        for ch in ' '..='~' {
            assert!(glyph(ch).is_some(), "no glyph for {ch:?}");
        }
        // Nothing outside the printable range, so a stray byte draws a gap
        // rather than reading past the table.
        assert!(glyph('\n').is_none());
        assert!(glyph('·').is_none());
    }

    #[test]
    fn every_glyph_stays_inside_its_cell() {
        // A row with a bit above the fifth column would bleed into the glyph
        // to its right, which reads as a font bug and is a data typo.
        for (index, rows) in FONT.iter().enumerate() {
            for (row, bits) in rows.iter().enumerate() {
                assert!(
                    bits >> GLYPH_W == 0,
                    "glyph {:?} row {row} sets a bit outside the cell: {bits:#07b}",
                    char::from(0x20 + index as u8)
                );
            }
        }
    }

    /// Write the overlay to a binary PPM so a person can look at it.
    ///
    /// A font typo is not something an assertion catches — every glyph in a
    /// bitmap font is plausible data, and a wrong bit just makes an `S` look
    /// slightly off. The only real check is a pair of eyes, so this exists to
    /// make that cheap:
    ///
    /// ```sh
    /// HUGINN_OVERLAY_DUMP=/tmp/overlay.ppm cargo test -p huginn-comp overlay_dump
    /// ```
    ///
    /// Does nothing when the variable is unset, so it costs a CI run nothing.
    #[test]
    fn overlay_dump() {
        let Ok(path) = std::env::var("HUGINN_OVERLAY_DUMP") else {
            return;
        };
        let overlay = Overlay::render(Rect::from_xywh(0, 0, 1920, 1080));
        let (w, h) = (overlay.width as usize, overlay.height as usize);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        // The canvas is RGBA and PPM is RGB, so the alpha is dropped. Every
        // pixel the overlay draws is opaque enough for that to be honest.
        for pixel in compose(Rect::from_xywh(0, 0, 1920, 1080)).pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {w}x{h} to {path}");
    }

    #[test]
    fn only_descenders_use_the_last_row() {
        let descenders = "gjpqy,";
        for (index, rows) in FONT.iter().enumerate() {
            let ch = char::from(0x20 + index as u8);
            if rows[GLYPH_H - 1] != 0 {
                assert!(descenders.contains(ch), "{ch:?} reaches below the baseline");
            }
        }
    }
}
