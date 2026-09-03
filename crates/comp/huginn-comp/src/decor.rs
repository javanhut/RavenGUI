//! Server-side window decorations: the title bar.
//!
//! A client that binds `zxdg_decoration_manager_v1` and does not insist on
//! drawing its own gets a bar from the compositor, in the desktop's own
//! palette: the window's title on the left, a close button on the right, and
//! nothing else. There is no dragging — the layout owns placement — and no
//! maximize, because a tile is already as large as the layout will make it.
//!
//! A toplevel that never binds the decoration manager is assumed to draw its
//! own, which is what the protocol says and what GTK and Firefox do; they get
//! no bar, and the whole pane. X11 windows are the other way round: they get a
//! bar unless their Motif hints say the client decorates itself.
//!
//! The geometry lives in `huginn-core` — [`huginn_core::window::Window::frame_top`]
//! is the inset and [`huginn_core::window::Window::content`] what the client is
//! configured to. This module is the pixels and the hit-testing: pure
//! functions over rectangles and a canvas, so every rule is unit-tested
//! without a display.

use huginn_core::geometry::{Point, Rect};

use crate::canvas::{Canvas, Panel};
use crate::text::Text;
use crate::theme;

/// Who draws a window's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecorMode {
    /// The compositor: a bar is drawn above the content and the client is
    /// configured to the pane less the bar.
    Server,
    /// The client: no bar, and the whole pane.
    Client,
}

/// What part of a bar a point landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    /// The bar itself. A click here focuses the window.
    Bar,
    /// The close button at the bar's right end.
    Close,
}

/// The bar directly above `content`, `top` tall and as wide as the content.
/// Zero-height when there is no frame.
pub(crate) fn bar_rect(content: Rect, top: i32) -> Rect {
    Rect::from_xywh(
        content.x(),
        content.y() - top.max(0),
        content.w(),
        top.max(0),
    )
}

/// The close button: the square at the bar's right end, the bar's height on
/// a side.
pub(crate) fn close_rect(bar: Rect) -> Rect {
    let side = bar.h().min(bar.w());
    Rect::from_xywh(bar.right() - side, bar.y(), side, side)
}

/// What `p` lands on in `bar`, if anything.
pub(crate) fn hit(bar: Rect, p: Point) -> Option<Hit> {
    if !bar.contains(p) {
        return None;
    }
    if close_rect(bar).contains(p) {
        return Some(Hit::Close);
    }
    Some(Hit::Bar)
}

/// `pane` with a frame `top` tall added back above it: the rectangle a bar and
/// its content take together. What the focus ring goes around.
pub(crate) fn with_frame(pane: Rect, top: i32) -> Rect {
    let top = top.max(0);
    Rect::from_xywh(pane.x(), pane.y() - top, pane.w(), pane.h() + top)
}

/// Everything a bar's pixels depend on. Two bars with equal keys look the
/// same, so a bar is recomposed only when its key changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarKey {
    pub title: Option<String>,
    /// The content's width in logical pixels — the layout's, not the drawn
    /// one, so a tile easing into place does not re-shape its text every
    /// frame. The renderer stretches the panel to wherever the bar is drawn.
    pub width: i32,
    /// The output's advertised scale: pixels per logical pixel.
    pub density: u32,
    /// The output the window is on. Text grows with the screen, the way every
    /// other panel's does.
    pub output: Rect,
    /// A focused window's title is drawn brighter than the others'.
    pub focused: bool,
}

/// A composed bar, and the key it was composed for.
#[derive(Debug)]
pub(crate) struct Bar {
    pub panel: Panel,
    pub key: BarKey,
}

/// Padding either side of the title, in logical pixels.
const PAD: f32 = 10.0;
/// The bar's background: the panel colour, the same as the dock and launcher.
const BG: [u8; 4] = theme::TITLE_BAR_BG.to_rgba_bytes();
const RULE: [u8; 4] = theme::BORDER.to_rgba_bytes();
/// The close glyph. A multiplication sign rather than an `x`: it is
/// symmetric, and every sans-serif font has one.
const CLOSE: &str = "\u{00D7}";

/// Everything on the desktop grows with the screen, so the bar's text does too.
fn ui_scale(output: Rect) -> f32 {
    (output.h() as f32 / 1080.0).clamp(1.0, 2.5)
}

/// `title` cut to fit `max_w`, with an ellipsis when it had to be.
///
/// Cut rather than wrapped: a bar is a label, and a label that wrapped would
/// need a taller bar, which would move the content.
fn fit(text: &mut Text, title: &str, size: f32, max_w: f32) -> String {
    let mut title = title.to_owned();
    let (mut w, _) = text.measure(&title, size);
    while w > max_w && title.chars().count() > 1 {
        title.pop();
        while !title.is_char_boundary(title.len()) {
            title.pop();
        }
        let (tw, _) = text.measure(&format!("{title}…"), size);
        w = tw;
        if w <= max_w {
            title.push('…');
            break;
        }
    }
    title
}

/// Lay the bar out and paint it. Split from [`render`] so a test can get at
/// the pixels without a renderer.
///
/// With no font installed the bar is drawn without its text: a blank bar
/// still shows which pane is which and its close button still works.
pub(crate) fn compose(text: &mut Text, key: &BarKey) -> Canvas {
    let px = key.density.max(1) as f32;
    let w = (key.width.max(1) as f32 * px) as usize;
    let h = (theme::TITLE_BAR_HEIGHT as f32 * px) as usize;
    let mut canvas = Canvas::new(w.max(1), h.max(1));
    canvas.fill(0, 0, w, h, BG);
    // A hairline along the bottom, where the bar meets the content.
    let rule = (px as usize).max(1);
    canvas.fill(0, h.saturating_sub(rule), w, rule, RULE);

    if !text.is_usable() {
        return canvas;
    }
    let colour = if key.focused {
        theme::TEXT
    } else {
        theme::TEXT_DIM
    };
    let size = theme::TITLE_TEXT_SIZE * ui_scale(key.output) * px;
    let pad = PAD * px;
    // The close button takes a square at the right end; the title has the rest.
    // The glyph is drawn a size up from the title: a multiplication sign is
    // small for its em, and a button wants to be seen before it is looked for.
    let close_side = h as f32;
    let close_size = size * 1.35;
    let (cw, ch) = text.measure(CLOSE, close_size);
    text.draw(
        &mut canvas,
        CLOSE,
        close_size,
        (w as f32 - close_side + (close_side - cw) / 2.0) as i32,
        ((h as f32 - ch) / 2.0) as i32,
        colour,
    );

    let Some(title) = key.title.as_deref().filter(|t| !t.is_empty()) else {
        return canvas;
    };
    let max_w = (w as f32 - close_side - pad * 2.0).max(0.0);
    if max_w <= 0.0 {
        return canvas;
    }
    let title = fit(text, title, size, max_w);
    let (_, th) = text.measure(&title, size);
    text.draw(
        &mut canvas,
        &title,
        size,
        pad as i32,
        ((h as f32 - th) / 2.0) as i32,
        colour,
    );
    canvas
}

/// The bar for `key`, ready to draw.
pub(crate) fn render(text: &mut Text, key: BarKey) -> Bar {
    let panel = Panel::from_canvas(&compose(text, &key), key.density);
    Bar { panel, key }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: Rect = Rect::from_xywh(100, 130, 600, 400);
    const TOP: i32 = theme::TITLE_BAR_HEIGHT;

    fn key(title: Option<&str>, focused: bool, density: u32) -> BarKey {
        BarKey {
            title: title.map(str::to_owned),
            width: 600,
            density,
            output: Rect::from_xywh(0, 0, 1920, 1080),
            focused,
        }
    }

    /// Skipped rather than failed where no font is installed, the same way
    /// the text module's own tests are.
    fn text_or_skip() -> Option<Text> {
        let text = Text::new();
        text.is_usable().then_some(text)
    }

    #[test]
    fn the_bar_sits_directly_above_the_content() {
        let bar = bar_rect(CONTENT, TOP);
        assert_eq!(bar.x(), CONTENT.x());
        assert_eq!(bar.w(), CONTENT.w());
        assert_eq!(bar.h(), TOP);
        assert_eq!(bar.bottom(), CONTENT.y(), "no gap and no overlap");
        assert_eq!(
            with_frame(CONTENT, TOP),
            Rect::from_xywh(100, 100, 600, 430)
        );
        assert_eq!(bar_rect(CONTENT, 0).h(), 0, "no frame, no bar");
    }

    #[test]
    fn close_button_is_the_rightmost_square_of_the_bar() {
        let bar = bar_rect(CONTENT, TOP);
        let close = close_rect(bar);
        assert_eq!(close.right(), bar.right());
        assert_eq!(close.y(), bar.y());
        assert_eq!(close.w(), TOP);
        assert_eq!(close.h(), TOP);
        assert_eq!(
            hit(bar, Point::new(close.x() + 1, close.y() + 1)),
            Some(Hit::Close)
        );
    }

    #[test]
    fn a_click_left_of_the_button_is_on_the_bar() {
        let bar = bar_rect(CONTENT, TOP);
        assert_eq!(
            hit(bar, Point::new(bar.x() + 5, bar.y() + 5)),
            Some(Hit::Bar)
        );
        let close = close_rect(bar);
        assert_eq!(
            hit(bar, Point::new(close.x() - 1, bar.y() + 5)),
            Some(Hit::Bar)
        );
    }

    #[test]
    fn a_click_below_the_bar_hits_nothing() {
        let bar = bar_rect(CONTENT, TOP);
        assert_eq!(hit(bar, Point::new(CONTENT.x() + 5, CONTENT.y() + 1)), None);
        assert_eq!(hit(bar, Point::new(bar.x() - 1, bar.y() + 5)), None);
        assert_eq!(hit(bar, Point::new(bar.x() + 5, bar.y() - 1)), None);
    }

    #[test]
    fn compose_is_the_bar_size_at_density() {
        let mut text = Text::new();
        let canvas = compose(&mut text, &key(Some("hello"), true, 2));
        assert_eq!(canvas.stride, 600 * 2);
        assert_eq!(canvas.height, (theme::TITLE_BAR_HEIGHT * 2) as usize);
        // The bottom rows are the rule, whatever font there is.
        let last = (canvas.height - 1) * canvas.stride * 4;
        assert_eq!(&canvas.pixels[last..last + 4], &RULE);
        // The top-left corner is background: no text starts flush with the edge.
        assert_eq!(&canvas.pixels[0..4], &BG);
        let panel = render(&mut text, key(Some("hello"), true, 2)).panel;
        assert_eq!(panel.size(), (600, theme::TITLE_BAR_HEIGHT));
    }

    #[test]
    fn an_unfocused_bar_is_dimmer() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let light: u64 = compose(&mut text, &key(Some("Raven Terminal"), true, 1))
            .pixels
            .iter()
            .map(|&b| u64::from(b))
            .sum();
        let dim: u64 = compose(&mut text, &key(Some("Raven Terminal"), false, 1))
            .pixels
            .iter()
            .map(|&b| u64::from(b))
            .sum();
        assert!(
            light > dim,
            "focused {light} should out-shine unfocused {dim}"
        );
        let blank: u64 = compose(&mut text, &key(None, true, 1))
            .pixels
            .iter()
            .map(|&b| u64::from(b))
            .sum();
        assert!(dim > blank, "even a dim title draws something");
    }

    /// Paint a focused and an unfocused bar to a PPM, to look at them without
    /// a desktop: `DECOR_DUMP=/tmp/d.ppm cargo test -p huginn-comp decor_dump`.
    #[test]
    fn decor_dump() {
        let Ok(path) = std::env::var("DECOR_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let bars = [
            compose(
                &mut text,
                &key(Some("Raven Terminal — ~/Development/RavenGUI"), true, 1),
            ),
            compose(&mut text, &key(Some("xterm"), false, 1)),
            compose(
                &mut text,
                &key(Some(&"a very long title ".repeat(20)), true, 2),
            ),
        ];
        let w = bars.iter().map(|b| b.stride).max().unwrap_or(1);
        let gap = 8;
        let h: usize = bars.iter().map(|b| b.height + gap).sum();
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for bar in &bars {
            for row in 0..bar.height + gap {
                for col in 0..w {
                    let px = if row < bar.height && col < bar.stride {
                        let p = &bar.pixels[(row * bar.stride + col) * 4..][..3];
                        [p[0], p[1], p[2]]
                    } else {
                        [0x40, 0x40, 0x48]
                    };
                    out.extend_from_slice(&px);
                }
            }
        }
        std::fs::write(path, out).expect("write dump");
    }

    #[test]
    fn a_title_that_does_not_fit_is_cut_with_an_ellipsis() {
        let Some(mut text) = text_or_skip() else {
            return;
        };
        let long = "a".repeat(400);
        let cut = fit(&mut text, &long, 13.0, 200.0);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() < long.chars().count());
        assert_eq!(fit(&mut text, "short", 13.0, 1000.0), "short");
    }
}
