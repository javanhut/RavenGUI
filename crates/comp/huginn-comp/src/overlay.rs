//! The keybinding overlay.
//!
//! Compositor-drawn, like everything else the shell puts on screen. Text goes
//! through [`crate::text`] — real shaping and antialiased rasterization — which
//! is what lets this be a panel someone reads rather than a debugging aid.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use huginn_core::geometry::Rect;

use crate::backend::keymap::BINDINGS;
use crate::canvas::{Canvas, Panel};
use crate::text::Text;

/// Padding inside the panel's border, in pixels at 1x.
const PAD: f32 = 20.0;
/// Space between the chord column and the description column.
const COLUMN_GAP: f32 = 24.0;
/// Blank space between one binding and the next.
const LINE_GAP: f32 = 6.0;
/// Text size at a 1080p output, in pixels. Scaled with the output below.
const BASE_SIZE: f32 = 15.0;

/// How far you can see through the overlay's background.
///
/// The overlay and the panel are the same background token; only this surface
/// is see-through, so the opacity is applied here rather than carried as a
/// second colour that could drift from the first.
const OVERLAY_ALPHA: u8 = 0xF2;

// The palette, resolved from the one theme at compile time. There is nothing
// to vary: the desktop ships one look, so this is a constant rather than a
// value threaded down from a caller.
const BG: [u8; 4] = crate::theme::BACKGROUND.with_alpha(OVERLAY_ALPHA).to_rgba_bytes();
const BORDER: [u8; 4] = crate::theme::BORDER.to_rgba_bytes();

const TITLE: &str = "Huginn keybindings";
const FOOTER: &str = "Super+Shift+H closes this. Plain Super belongs to the focused application.";

/// The keybinding overlay.
#[derive(Debug)]
pub(crate) struct Overlay {
    panel: Panel,
}

impl Overlay {
    /// Draw the overlay, sized to sit comfortably on `output`.
    pub(crate) fn render(output: Rect, text: &mut Text) -> Self {
        Self {
            panel: Panel::from_canvas(&compose(output, text)),
        }
    }

    pub(crate) fn buffer(&self) -> &MemoryRenderBuffer {
        &self.panel.buffer
    }

    /// Where the overlay goes: centred on the output.
    pub(crate) fn placement(&self, output: Rect) -> Rect {
        self.panel.centred_on(output)
    }
}

/// Lay the panel out and paint it. Split from [`Overlay::render`] so a test can
/// get at the pixels without going through a renderer.
fn compose(output: Rect, text: &mut Text) -> Canvas {
    // Text grows with the output rather than the panel being scaled after the
    // fact: rasterizing at the final size is the whole reason for a real font
    // stack, and scaling a bitmap afterwards would throw that away.
    let size = (BASE_SIZE * (output.h() as f32 / 1080.0)).clamp(BASE_SIZE, BASE_SIZE * 2.5);
    let line = size * 1.35;

    let chord_w = BINDINGS
        .iter()
        .map(|b| text.measure(b.chord, size).0)
        .fold(0.0_f32, f32::max);
    // Measured into a Vec first: the closure would hold `text` mutably while
    // the chained title and footer measurements need it too.
    let widest_row = BINDINGS
        .iter()
        .map(|b| chord_w + COLUMN_GAP + text.measure(b.description, size).0)
        .fold(0.0_f32, f32::max);
    let body_w = widest_row
        .max(text.measure(TITLE, size).0)
        .max(text.measure(FOOTER, size).0);

    let w = (body_w + PAD * 2.0).ceil() as usize;
    // Title, a rule under it, every binding, then the footer.
    let rows = BINDINGS.len() as f32;
    let h = (PAD * 2.0
        + line + LINE_GAP          // title
        + 1.0 + LINE_GAP           // rule
        + rows * (line + LINE_GAP) // bindings
        + LINE_GAP
        + line)                    // footer
        .ceil() as usize;

    let mut canvas = Canvas::new(w, h);
    canvas.fill(0, 0, w, h, BG);
    canvas.frame(BORDER);

    let mut y = PAD;
    text.draw(&mut canvas, TITLE, size, PAD as i32, y as i32, crate::theme::ACCENT);
    y += line + LINE_GAP;
    canvas.fill(PAD as usize, y as usize, body_w as usize, 1, BORDER);
    y += 1.0 + LINE_GAP;

    for binding in BINDINGS {
        text.draw(&mut canvas, binding.chord, size, PAD as i32, y as i32, crate::theme::ACCENT);
        text.draw(
            &mut canvas,
            binding.description,
            size,
            (PAD + chord_w + COLUMN_GAP) as i32,
            y as i32,
            crate::theme::TEXT,
        );
        y += line + LINE_GAP;
    }
    y += LINE_GAP;
    text.draw(&mut canvas, FOOTER, size, PAD as i32, y as i32, crate::theme::TEXT_DIM);

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_centred_on_the_output() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let overlay = Overlay::render(output, &mut Text::new());
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
        let overlay = Overlay::render(output, &mut Text::new());
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.right() <= output.right());
    }

    #[test]
    fn an_output_smaller_than_the_overlay_still_shows_its_top_left() {
        // Better a clipped list anchored at the corner than one centred so far
        // negative that the beginning of every line is off screen.
        let output = Rect::from_xywh(0, 0, 320, 200);
        let overlay = Overlay::render(output, &mut Text::new());
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.y() >= output.y());
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
    fn the_overlay_is_wide_enough_for_its_longest_line() {
        // Measured rather than assumed: the panel is sized from real shaped
        // text now, so a font whose metrics differ from the last one must
        // still produce a panel that fits its own footer.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let canvas = compose(output, &mut text);
        let size = BASE_SIZE;
        let footer = text.measure(FOOTER, size).0;
        assert!(
            canvas.stride as f32 >= footer + PAD * 2.0,
            "panel is {} wide but the footer needs {}",
            canvas.stride,
            footer + PAD * 2.0
        );
    }

    #[test]
    fn the_overlay_paints_every_pixel_it_claims() {
        // A transparent pixel inside the panel is a hole onto the desktop.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text);
        let clear = canvas.pixels.chunks_exact(4).filter(|p| p[3] == 0).count();
        assert_eq!(clear, 0, "{clear} fully transparent pixels inside the panel");
    }

    #[test]
    fn text_is_blended_rather_than_stamped() {
        // The point of the change: glyph edges carry partial coverage. A
        // canvas with only the background and border values in it means the
        // blend collapsed back to one-bit stamping.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text);
        let accent = crate::theme::ACCENT.to_rgba_bytes();
        let partial = canvas
            .pixels
            .chunks_exact(4)
            .filter(|p| {
                // Somewhere strictly between the background and the accent.
                p[0] > BG[0].min(accent[0]) && p[0] < BG[0].max(accent[0]) && p[0] != BG[0]
            })
            .count();
        assert!(partial > 0, "no partially-covered pixels; text is not antialiased");
    }

    #[test]
    fn a_taller_output_gets_a_larger_panel() {
        // Text scales with the output rather than the panel being blown up
        // afterwards, so a 4K screen gets a readable panel and not a big blur.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let small = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text);
        let large = compose(Rect::from_xywh(0, 0, 3840, 2160), &mut text);
        assert!(large.height > small.height, "panel did not grow with the output");
    }

    #[test]
    fn overlay_dump() {
        let Ok(path) = std::env::var("HUGINN_OVERLAY_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text);
        let (w, h) = (canvas.stride, canvas.height);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        // The canvas is RGBA and PPM is RGB, so the alpha is dropped. Every
        // pixel the overlay draws is opaque enough for that to be honest.
        for pixel in compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text).pixels.chunks_exact(4)
        {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {w}x{h} to {path}");
    }

}
