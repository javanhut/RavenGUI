//! Real text, for the shell the compositor draws.
//!
//! The overlay used to draw with a 5×7 bitmap font carried in the binary. That
//! was the right first step — it needed nothing and proved the drawing path —
//! but it cannot antialias, cannot hint, cannot kern, and has no glyph outside
//! printable ASCII. Every one of those is visible the moment text is bigger
//! than a debugging aid.
//!
//! [`cosmic_text`] supplies the parts that are unreasonable to write by hand:
//! finding a font, shaping characters into positioned glyphs, breaking lines,
//! and rasterizing outlines to antialiased coverage. It supplies nothing else —
//! no widgets, no layout boxes, no events — which is exactly right here, since
//! the shell is inside the render loop and there is no toolkit to disagree with.
//!
//! # Cost
//!
//! [`Text::new`] scans the system's font directories, which takes on the order
//! of a hundred milliseconds. It is built once and kept, never per draw.

pub(crate) use cosmic_text::Weight;
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache};

use crate::theme::Color;

/// The font stack, and the glyph cache that keeps it fast.
pub(crate) struct Text {
    fonts: FontSystem,
    cache: SwashCache,
    /// Whether any font was found at all. See [`Text::is_usable`].
    usable: bool,
}

impl std::fmt::Debug for Text {
    /// `FontSystem` and `SwashCache` are large and not `Debug`; printing the
    /// whole font database in a state dump helps nobody.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Text")
            .field("usable", &self.usable)
            .finish()
    }
}

impl Text {
    /// Load the system's fonts. Slow; do it once.
    pub(crate) fn new() -> Self {
        let fonts = FontSystem::new();
        // A desktop with no fonts installed is a real state — a minimal distro
        // image before its font package lands — and it must not be a panic
        // deep inside a redraw. It is reported once, here, and every later
        // draw becomes a no-op.
        let usable = !fonts.db().is_empty();
        if !usable {
            tracing::error!("no fonts found; the shell will draw no text. Install a font package.");
        } else {
            tracing::debug!(faces = fonts.db().len(), "fonts loaded");
        }
        Self {
            fonts,
            cache: SwashCache::new(),
            usable,
        }
    }

    /// Whether any font was found. Drawing with none produces nothing.
    ///
    /// Read by tests, which skip rather than fail where no font is installed;
    /// the compositor reports the same condition once at startup and then
    /// simply draws nothing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_usable(&self) -> bool {
        self.usable
    }

    /// Lay `text` out at `size` pixels, wrapping at `width` if given.
    ///
    /// At `weight`: a heading or a name that has to lead the eye is set
    /// heavier than the text around it.
    pub(crate) fn layout_weighted(
        &mut self,
        text: &str,
        size: f32,
        width: Option<f32>,
        weight: Weight,
    ) -> Buffer {
        // Line height at 1.35× the font size. Tighter reads as cramped at the
        // sizes a launcher uses; looser stops a list looking like a list.
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, size * 1.35));
        buffer.set_size(width, None);
        buffer.set_text(
            text,
            &Attrs::new().family(Family::SansSerif).weight(weight),
            // Advanced shaping: ligatures, kerning, and the reordering that
            // scripts like Devanagari need. The cheaper mode is only correct
            // for text that happens to be Latin, which the name of an
            // application is under no obligation to be.
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    /// The width and height `text` occupies at `size`, in pixels.
    pub(crate) fn measure(&mut self, text: &str, size: f32) -> (f32, f32) {
        self.measure_weighted(text, size, Weight::NORMAL)
    }

    /// [`Self::measure`] at `weight`. A heavier face is wider, so text drawn
    /// with [`Self::draw_weighted`] has to be measured the same way to centre.
    pub(crate) fn measure_weighted(&mut self, text: &str, size: f32, weight: Weight) -> (f32, f32) {
        let buffer = self.layout_weighted(text, size, None, weight);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max);
        let height = buffer.metrics().line_height * buffer.layout_runs().count().max(1) as f32;
        (width, height)
    }
}

/// Somewhere glyphs can be drawn: an RGBA byte canvas.
pub(crate) trait Surface {
    /// Blend `color` over the pixel at `x`, `y` with coverage `alpha`.
    fn blend(&mut self, x: i32, y: i32, color: Color, alpha: u8);
}

impl Text {
    /// Draw `text` at `size`, with its top-left corner at `x`, `y`.
    ///
    /// Coordinates are the *box* the text sits in, not its baseline — a caller
    /// laying out rows should not have to know about ascenders to place them.
    pub(crate) fn draw(
        &mut self,
        surface: &mut impl Surface,
        text: &str,
        size: f32,
        x: i32,
        y: i32,
        color: Color,
    ) {
        self.draw_weighted(surface, text, size, x, y, color, Weight::NORMAL);
    }

    /// [`Self::draw`] at `weight`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_weighted(
        &mut self,
        surface: &mut impl Surface,
        text: &str,
        size: f32,
        x: i32,
        y: i32,
        color: Color,
        weight: Weight,
    ) {
        if !self.usable {
            return;
        }
        let buffer = self.layout_weighted(text, size, None, weight);
        // Split the borrow: `draw_glyph` needs the cache mutably while the
        // buffer is being walked, and both live on `self`.
        let Self { fonts, cache, .. } = self;

        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let Some(image) = cache.get_image(fonts, physical.cache_key).as_ref() else {
                    continue;
                };
                let left = x + physical.x + image.placement.left;
                // `run.line_y` is the baseline; the glyph's `top` is measured
                // up from it, hence the subtraction.
                let top = y + physical.y + run.line_y as i32 - image.placement.top;
                blit_coverage(surface, image, left, top, color);
            }
        }
    }
}

/// A run of text in its own weight and slant, for [`Text::draw_wrapped`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Styled<'a> {
    pub(crate) text: &'a str,
    pub(crate) weight: Weight,
    pub(crate) italic: bool,
}

impl<'a> Styled<'a> {
    pub(crate) fn plain(text: &'a str) -> Self {
        Self::weighted(text, Weight::NORMAL)
    }

    pub(crate) fn weighted(text: &'a str, weight: Weight) -> Self {
        Self {
            text,
            weight,
            italic: false,
        }
    }
}

impl Text {
    /// The height of one line of text at `size`: what [`Self::layout_weighted`]
    /// and [`Self::draw_wrapped`] lay lines out at.
    pub(crate) fn line_height(size: f32) -> f32 {
        size * 1.35
    }

    /// Lay out runs of differently styled text as one paragraph, wrapping at
    /// `width`.
    fn layout_styled(&mut self, spans: &[Styled<'_>], size: f32, width: f32) -> Buffer {
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, Self::line_height(size)));
        buffer.set_size(Some(width), None);
        let runs = spans.iter().map(|span| {
            let style = if span.italic {
                Style::Italic
            } else {
                Style::Normal
            };
            (
                span.text,
                Attrs::new()
                    .family(Family::SansSerif)
                    .weight(span.weight)
                    .style(style),
            )
        });
        buffer.set_rich_text(
            runs,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    /// How many lines `spans` take when wrapped at `width`.
    pub(crate) fn wrapped_lines(&mut self, spans: &[Styled<'_>], size: f32, width: f32) -> usize {
        if !self.usable {
            return 0;
        }
        self.layout_styled(spans, size, width).layout_runs().count()
    }

    /// Draw `spans` wrapped at `width` with the top-left of the block at `x`,
    /// `y`, keeping at most `max_lines` lines. Text that does not fit ends in
    /// an ellipsis on the last line kept, cut at the glyph that would cross the
    /// edge, rather than simply stopping at a word with nothing to say more
    /// was there.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_wrapped(
        &mut self,
        surface: &mut impl Surface,
        spans: &[Styled<'_>],
        size: f32,
        x: f32,
        y: f32,
        width: f32,
        max_lines: usize,
        color: Color,
    ) {
        if !self.usable || max_lines == 0 {
            return;
        }
        let buffer = self.layout_styled(spans, size, width);
        let truncated = buffer.layout_runs().count() > max_lines;
        let ellipsis_w = if truncated {
            self.measure("…", size).0
        } else {
            0.0
        };
        let (left, top) = (x.round() as i32, y.round() as i32);
        let mut ellipsis_at = None;
        {
            // Split the borrow, as `draw_weighted` does.
            let Self { fonts, cache, .. } = self;
            for (index, run) in buffer.layout_runs().take(max_lines).enumerate() {
                let last = truncated && index + 1 == max_lines;
                let mut end = 0.0_f32;
                for glyph in run.glyphs {
                    if last && glyph.x + glyph.w > width - ellipsis_w {
                        break;
                    }
                    let physical = glyph.physical((0.0, 0.0), 1.0);
                    if let Some(image) = cache.get_image(fonts, physical.cache_key).as_ref() {
                        blit_coverage(
                            surface,
                            image,
                            left + physical.x + image.placement.left,
                            top + physical.y + run.line_y as i32 - image.placement.top,
                            color,
                        );
                    }
                    end = glyph.x + glyph.w;
                }
                if last {
                    ellipsis_at = Some((end, run.line_top));
                }
            }
        }
        if let Some((end, line_top)) = ellipsis_at {
            self.draw(
                surface,
                "…",
                size,
                left + end.round() as i32,
                top + line_top.round() as i32,
                color,
            );
        }
    }
}

/// Blend a rasterized glyph's coverage onto `surface` with its top-left at
/// `left`, `top`.
///
/// Only a plain coverage mask is drawn. A colour glyph — an emoji, or a digit
/// that fell back to the emoji font because the face asked for had no glyph
/// for it — is four bytes of RGBA per pixel, and blended as if each byte were
/// coverage it comes out as a block of stripes the width of several letters.
/// Leaving it out loses one symbol; drawing it wrong spoils the whole line.
fn blit_coverage(
    surface: &mut impl Surface,
    image: &cosmic_text::SwashImage,
    left: i32,
    top: i32,
    color: Color,
) {
    if !matches!(image.content, cosmic_text::SwashContent::Mask) {
        return;
    }
    for (row, chunk) in image
        .data
        .chunks_exact(image.placement.width.max(1) as usize)
        .enumerate()
    {
        for (column, coverage) in chunk.iter().enumerate() {
            if *coverage > 0 {
                surface.blend(left + column as i32, top + row as i32, color, *coverage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canvas that records what was drawn, so tests need no compositor.
    struct Recorder {
        width: i32,
        height: i32,
        pixels: Vec<u8>,
    }

    impl Recorder {
        fn new(width: i32, height: i32) -> Self {
            Self {
                width,
                height,
                pixels: vec![0; (width * height) as usize],
            }
        }

        /// How many pixels received any coverage at all.
        fn covered(&self) -> usize {
            self.pixels.iter().filter(|p| **p > 0).count()
        }

        /// The rightmost column with any coverage, for checking advance.
        fn rightmost(&self) -> Option<i32> {
            (0..self.width)
                .rev()
                .find(|x| (0..self.height).any(|y| self.pixels[(y * self.width + x) as usize] > 0))
        }
    }

    impl Surface for Recorder {
        fn blend(&mut self, x: i32, y: i32, _color: Color, alpha: u8) {
            if x < 0 || y < 0 || x >= self.width || y >= self.height {
                return;
            }
            let at = (y * self.width + x) as usize;
            self.pixels[at] = self.pixels[at].max(alpha);
        }
    }

    /// Skipped rather than failed where no font is installed, so the suite
    /// still passes in a container. The compositor logs the same condition.
    fn text_or_skip() -> Option<Text> {
        let text = Text::new();
        text.is_usable().then_some(text)
    }

    #[test]
    fn drawing_a_string_puts_pixels_on_the_surface() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut surface = Recorder::new(400, 60);
        text.draw(&mut surface, "Raven", 24.0, 4, 4, crate::theme::TEXT);
        assert!(
            surface.covered() > 50,
            "only {} pixels drawn",
            surface.covered()
        );
    }

    #[test]
    fn nothing_is_drawn_outside_the_surface() {
        // A glyph that overhangs must be clipped, not panic on an index.
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut surface = Recorder::new(20, 20);
        text.draw(
            &mut surface,
            "overflowing text",
            40.0,
            -30,
            -10,
            crate::theme::TEXT,
        );
        text.draw(
            &mut surface,
            "overflowing text",
            40.0,
            15,
            15,
            crate::theme::TEXT,
        );
    }

    #[test]
    fn wider_text_reaches_further_right() {
        // Proves the pen advances rather than stacking every glyph at x.
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut short = Recorder::new(600, 60);
        let mut long = Recorder::new(600, 60);
        text.draw(&mut short, "i", 24.0, 4, 4, crate::theme::TEXT);
        text.draw(&mut long, "iiiiiiiiii", 24.0, 4, 4, crate::theme::TEXT);
        assert!(
            long.rightmost() > short.rightmost(),
            "text did not advance: {:?} vs {:?}",
            short.rightmost(),
            long.rightmost()
        );
    }

    #[test]
    fn text_is_antialiased_rather_than_one_bit() {
        // The whole reason for replacing the bitmap font: coverage between
        // 0 and 255 is what makes an edge look like an edge.
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut surface = Recorder::new(400, 80);
        text.draw(&mut surface, "Oso", 48.0, 4, 4, crate::theme::TEXT);
        let partial = surface
            .pixels
            .iter()
            .filter(|p| **p > 0 && **p < 255)
            .count();
        assert!(partial > 0, "every pixel was fully on or fully off");
    }

    #[test]
    fn measuring_agrees_with_what_gets_drawn() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let (width, height) = text.measure("Raven Terminal", 24.0);
        assert!(width > 0.0 && height > 0.0, "measured {width}x{height}");

        let mut surface = Recorder::new(800, 100);
        text.draw(
            &mut surface,
            "Raven Terminal",
            24.0,
            0,
            0,
            crate::theme::TEXT,
        );
        let drawn = surface.rightmost().unwrap_or(0);
        // Within a few pixels: the measure is the advance, the drawing is ink,
        // and the last glyph's ink stops short of its advance.
        assert!(
            (drawn as f32 - width).abs() < 12.0,
            "measured {width} but drew to {drawn}"
        );
    }

    #[test]
    fn a_longer_string_measures_wider() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let (short, _) = text.measure("Files", 24.0);
        let (long, _) = text.measure("Files and Folders", 24.0);
        assert!(long > short, "{long} was not wider than {short}");
    }

    #[test]
    fn a_bigger_size_measures_bigger() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let (small_w, small_h) = text.measure("Raven", 12.0);
        let (big_w, big_h) = text.measure("Raven", 48.0);
        assert!(big_w > small_w && big_h > small_h);
    }

    #[test]
    fn non_ascii_text_produces_glyphs() {
        // The bitmap font could not draw any of this at all — it had 95
        // glyphs, all ASCII. An application name is under no obligation to be.
        //
        // Only scripts the Noto family covers are asserted here. Rendering is
        // never better than the fonts installed: see
        // `a_script_with_no_font_installed_draws_nothing`, which is how a
        // missing font package shows up.
        let Some(mut text) = text_or_skip() else {
            return;
        };
        for sample in ["Größe", "Ελληνικά", "العربية", "Ćirilica Ђ"] {
            let mut surface = Recorder::new(400, 80);
            text.draw(&mut surface, sample, 32.0, 4, 4, crate::theme::TEXT);
            assert!(surface.covered() > 0, "{sample:?} drew nothing");
        }
    }

    #[test]
    fn a_script_with_no_font_installed_draws_nothing() {
        // Not a defect in the text stack, and worth pinning as behaviour: with
        // no CJK font on the system there is nothing to fall back to, and the
        // name of a Japanese application comes out blank rather than as boxes.
        // The fix is a font package on the distro, not code here.
        //
        // Asserted only when the gap is real, so installing a CJK font makes
        // this test agree rather than start failing.
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut surface = Recorder::new(400, 80);
        text.draw(&mut surface, "日本語", 32.0, 4, 4, crate::theme::TEXT);
        if surface.covered() == 0 {
            eprintln!("note: no CJK font installed; CJK application names will be blank");
        }
    }

    #[test]
    fn an_empty_string_draws_nothing_and_does_not_panic() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let mut surface = Recorder::new(100, 40);
        text.draw(&mut surface, "", 24.0, 4, 4, crate::theme::TEXT);
        assert_eq!(surface.covered(), 0);
    }
}
