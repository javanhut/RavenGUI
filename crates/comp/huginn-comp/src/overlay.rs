//! The keybinding overlay.
//!
//! Compositor-drawn, like everything else the shell puts on screen. Text goes
//! through [`crate::text`] — real shaping and antialiased rasterization — which
//! is what lets this be a panel someone reads rather than a debugging aid.
//!
//! Every chord is drawn as a row of keycaps that press themselves down in
//! order, hold, and let go, so the list shows the gesture as well as naming
//! it. The panel is painted twice when it opens — every cap up, every cap
//! down — and a frame only copies across the caps whose state changed, so the
//! animation costs a few small copies rather than shaping thirty-odd rows of
//! text sixty times a second.

use std::time::Duration;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Buffer, Rectangle};

use huginn_core::geometry::Rect;

use crate::backend::keymap::BINDINGS;
use crate::canvas::{Canvas, Panel};
use crate::text::Text;
use crate::theme::{self, Color};

/// Padding inside the panel's border, in pixels at 1x.
const PAD: f32 = 20.0;
/// Space between the chord column and the description column.
const COLUMN_GAP: f32 = 24.0;
/// Blank space between one binding and the next.
const LINE_GAP: f32 = 6.0;
/// Text size at a 1080p output, in pixels. Scaled with the output below.
const BASE_SIZE: f32 = 15.0;
/// The smallest text the panel shrinks to when the list would not otherwise
/// fit on the output, at 1x. Below this it is clipped instead: a list too
/// small to read is no better than one that runs off the edge.
const MIN_SIZE: f32 = 11.0;
/// How much of the output the panel may cover, each way.
const FILL: f32 = 0.94;

/// How far you can see through the overlay's background.
///
/// The overlay and the panel are the same background token; only this surface
/// is see-through, so the opacity is applied here rather than carried as a
/// second colour that could drift from the first.
const OVERLAY_ALPHA: u8 = 0xF2;

/// Corner radius at 1×: the panel radius every other floating surface has.
const RADIUS: f32 = theme::PANEL_RADIUS;

/// The shadow under a raised cap: the strip that makes it read as a key
/// standing up from the panel, and that disappears when it is pressed.
const CAP_SHADOW: Color = Color::from_argb(0x7000_0000);

const TITLE: &str = "Huginn keybindings";
const FOOTER: &str =
    "Esc or a click outside closes this. Plain Super belongs to the focused application.";

/// Pause at the top of a cycle, before the first key goes down.
const LEAD: Duration = Duration::from_millis(250);
/// Between one key of a chord going down and the next.
const STEP: Duration = Duration::from_millis(160);
/// How long the whole chord is held once its last key is down.
const HOLD: Duration = Duration::from_millis(550);
/// One press and the rest after it. The same for every row, whatever its
/// length, so the rows stay in step with each other rather than drifting
/// apart into noise.
const CYCLE: Duration = Duration::from_millis(2600);
/// How much later each row starts than the one above it: the presses run
/// down the list as a wave instead of every row stamping at once.
const STAGGER: Duration = Duration::from_millis(40);

/// The keybinding overlay.
pub(crate) struct Overlay {
    panel: Panel,
    /// The canvas's width in pixels, which the buffer shares.
    stride: usize,
    rows: Vec<Row>,
    /// Every keycap on the panel, row by row.
    caps: Vec<Cap>,
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay")
            .field("panel", &self.panel)
            .field("caps", &self.caps.len())
            .finish_non_exhaustive()
    }
}

/// A row's presses, and where its caps start in [`Overlay::caps`].
struct Row {
    presses: Vec<Vec<usize>>,
    first: usize,
}

/// One keycap: where it is, what it looks like either way, and which way the
/// buffer shows it now.
struct Cap {
    at: CapBox,
    up: Vec<u8>,
    down: Vec<u8>,
    is_down: bool,
}

impl Overlay {
    /// Draw the overlay, sized to sit comfortably on `output`, at `density`
    /// pixels per logical one.
    pub(crate) fn render(output: Rect, text: &mut Text, density: u32) -> Self {
        let layout = fit(output, text, density);
        let (up, boxes) = paint(&layout, text, false);
        let (down, _) = paint(&layout, text, true);
        let mut rows = Vec::with_capacity(layout.rows.len());
        let mut caps = Vec::new();
        for (row, boxes) in layout.rows.into_iter().zip(boxes) {
            rows.push(Row {
                presses: row.chord.presses,
                first: caps.len(),
            });
            caps.extend(boxes.into_iter().map(|at| Cap {
                at,
                up: cut(&up, at),
                down: cut(&down, at),
                is_down: false,
            }));
        }
        Self {
            panel: Panel::from_canvas(&up, density),
            stride: up.stride,
            rows,
            caps,
        }
    }

    pub(crate) fn buffer(&self) -> &MemoryRenderBuffer {
        &self.panel.buffer
    }

    /// Where the overlay goes: centred on the output.
    pub(crate) fn placement(&self, output: Rect) -> Rect {
        self.panel.centred_on(output)
    }

    /// Put every keycap where it is `elapsed` after the overlay opened.
    /// Returns whether any of them moved.
    ///
    /// Only the caps that changed are copied into the buffer, and only their
    /// rectangles are damaged, so a frame where nothing is pressed or let go
    /// uploads nothing at all.
    pub(crate) fn animate(&mut self, elapsed: Duration) -> bool {
        let mut wanted = vec![false; self.caps.len()];
        for (index, row) in self.rows.iter().enumerate() {
            for cap in down(index, &row.presses, elapsed) {
                wanted[row.first + cap] = true;
            }
        }
        if self.caps.iter().zip(&wanted).all(|(cap, &want)| cap.is_down == want) {
            return false;
        }
        let (stride, caps) = (self.stride, &mut self.caps);
        let mut context = self.panel.buffer.render();
        let Ok(()) = context.draw(|pixels| {
            let mut damage = Vec::new();
            for (cap, &want) in caps.iter_mut().zip(&wanted) {
                if cap.is_down == want {
                    continue;
                }
                cap.is_down = want;
                paste(pixels, stride, cap.at, if want { &cap.down } else { &cap.up });
                damage.push(Rectangle::<i32, Buffer>::new(
                    (cap.at.x as i32, cap.at.y as i32).into(),
                    (cap.at.w as i32, cap.at.h as i32).into(),
                ));
            }
            Ok::<_, std::convert::Infallible>(damage)
        });
        true
    }
}

/// A piece of a chord as it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A key, drawn as a cap.
    Cap(&'static str),
    /// The `+` between keys, or the `/` between alternatives.
    Sep(&'static str),
}

/// A chord from [`BINDINGS`], taken apart into what to draw and what to press.
#[derive(Debug, PartialEq, Eq)]
struct Chord {
    items: Vec<Item>,
    /// Each way of pressing it, as the caps that go down in order — indices
    /// counting only the caps in `items`. One per alternative, taken in turn
    /// from one cycle to the next.
    presses: Vec<Vec<usize>>,
}

/// Take a chord as the table spells it apart.
///
/// `Super+Ctrl+Q / X` is two chords that share everything but their last key,
/// and is drawn that way — the shared keys once, then the alternatives — so an
/// alternative that is a single key is pressed with the first one's modifiers.
/// One with a `+` of its own is a whole chord and is pressed as written.
fn parse(chord: &'static str) -> Chord {
    let mut items = Vec::new();
    let mut presses = Vec::new();
    let mut caps = 0;
    let mut modifiers = Vec::new();
    for (i, alternative) in chord.split(" / ").enumerate() {
        if i > 0 {
            items.push(Item::Sep("/"));
        }
        let keys: Vec<&'static str> = alternative
            .split('+')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .collect();
        let mut press = if i > 0 && keys.len() == 1 {
            modifiers.clone()
        } else {
            Vec::new()
        };
        for (j, key) in keys.into_iter().enumerate() {
            if j > 0 {
                items.push(Item::Sep("+"));
            }
            items.push(Item::Cap(key));
            press.push(caps);
            caps += 1;
        }
        if i == 0 {
            modifiers = press[..press.len().saturating_sub(1)].to_vec();
        }
        presses.push(press);
    }
    Chord { items, presses }
}

/// The caps of the row at `row` that are down `elapsed` after the overlay
/// opened, out of its `presses`.
///
/// Pure, so the choreography can be tested without a clock: each key goes
/// down [`STEP`] after the one before it, the chord is held for [`HOLD`], and
/// then everything lets go until the next [`CYCLE`].
fn down(row: usize, presses: &[Vec<usize>], elapsed: Duration) -> &[usize] {
    let Some(since) = elapsed.checked_sub(STAGGER * row as u32) else {
        return &[];
    };
    let cycle = since.as_millis() / CYCLE.as_millis();
    let Some(press) = presses.get(cycle as usize % presses.len().max(1)) else {
        return &[];
    };
    let within = Duration::from_millis((since.as_millis() % CYCLE.as_millis()) as u64);
    let Some(into) = within.checked_sub(LEAD) else {
        return &[];
    };
    if into >= STEP * press.len().saturating_sub(1) as u32 + HOLD {
        return &[];
    }
    let keys = (into.as_millis() / STEP.as_millis()) as usize + 1;
    &press[..keys.min(press.len())]
}

/// A keycap's rectangle on the canvas, in pixels, including the depth it
/// travels when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapBox {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// A row of the table, measured.
struct RowLayout {
    chord: Chord,
    /// The width of each of the chord's items, in order.
    widths: Vec<f32>,
}

/// Everything about the panel's geometry, worked out once and painted twice.
/// All in canvas pixels.
struct Layout {
    px: f32,
    size: f32,
    line: f32,
    pad: f32,
    column_gap: f32,
    line_gap: f32,
    rule: f32,
    /// The face of a cap; it stands [`Self::depth`] above the panel.
    cap_h: f32,
    depth: f32,
    cap_radius: f32,
    label_size: f32,
    chord_w: f32,
    body_w: f32,
    w: usize,
    h: usize,
    rows: Vec<RowLayout>,
}

/// Lay the panel out at a size that fits on `output`.
///
/// Text grows with the output rather than the panel being scaled after the
/// fact: rasterizing at the final size is the whole reason for a real font
/// stack, and scaling a bitmap afterwards would throw that away. Keycaps take
/// more height than a line of text, so the list no longer fits a 1080p screen
/// at the size it would like; it is shrunk to fit rather than clipped.
///
/// Everything is in the canvas's own pixels, of which a 2× output has twice
/// as many per logical pixel as a 1× one. `px` is that factor; it goes into
/// every dimension and `Panel::from_canvas` divides it back out, so the panel
/// is placed at the same logical size either way.
fn fit(output: Rect, text: &mut Text, density: u32) -> Layout {
    let px = density.max(1) as f32;
    let floor = MIN_SIZE * px;
    let (room_w, room_h) = (
        output.w() as f32 * px * FILL,
        output.h() as f32 * px * FILL,
    );
    let mut size =
        (BASE_SIZE * (output.h() as f32 / 1080.0)).clamp(BASE_SIZE, BASE_SIZE * 2.5) * px;
    let mut layout = lay_out(text, size, px);
    // Shaped text is not quite linear in its size and the padding does not
    // scale with it at all, so one proportional step can leave the panel a
    // little over. A few more converge.
    for _ in 0..4 {
        let over = (layout.w as f32 / room_w).max(layout.h as f32 / room_h);
        if over <= 1.0 || size <= floor {
            break;
        }
        size = (size / over).max(floor);
        layout = lay_out(text, size, px);
    }
    layout
}

/// Measure the panel at text `size`.
fn lay_out(text: &mut Text, size: f32, px: f32) -> Layout {
    let label_size = size * 0.86;
    let cap_h = (size * 1.7).round();
    let depth = (size * 0.16).round().max(px);
    let cap_pad = (size * 0.55).round();
    let sep_gap = (size * 0.3).round();

    let rows: Vec<RowLayout> = BINDINGS
        .iter()
        .map(|binding| {
            let chord = parse(binding.chord);
            let widths = chord
                .items
                .iter()
                .map(|item| match item {
                    // Never narrower than tall: a single letter is a square
                    // key, not a sliver.
                    Item::Cap(key) => {
                        (text.measure(key, label_size).0 + cap_pad * 2.0).ceil().max(cap_h)
                    }
                    Item::Sep(sep) => text.measure(sep, size).0.ceil() + sep_gap * 2.0,
                })
                .collect();
            RowLayout { chord, widths }
        })
        .collect();

    let chord_w = rows
        .iter()
        .map(|row| row.widths.iter().sum::<f32>())
        .fold(0.0_f32, f32::max);
    let column_gap = COLUMN_GAP * px;
    // Measured into a Vec first: the closure would hold `text` mutably while
    // the chained title and footer measurements need it too.
    let widest_row = BINDINGS
        .iter()
        .map(|b| chord_w + column_gap + text.measure(b.description, size).0)
        .fold(0.0_f32, f32::max);
    let body_w = widest_row
        .max(text.measure(TITLE, size).0)
        .max(text.measure(FOOTER, size).0);

    let line = size * 1.35;
    let (pad, line_gap, rule) = (PAD * px, LINE_GAP * px, px);
    // Title, a rule under it, every binding, then the footer.
    let count = rows.len() as f32;
    let h = pad * 2.0
        + line + line_gap                       // title
        + rule + line_gap                       // rule
        + count * (cap_h + depth + line_gap)    // bindings
        + line_gap
        + line; // footer

    Layout {
        px,
        size,
        line,
        pad,
        column_gap,
        line_gap,
        rule,
        cap_h,
        depth,
        cap_radius: (size * 0.35).round(),
        label_size,
        chord_w,
        body_w,
        w: (body_w + pad * 2.0).ceil() as usize,
        h: h.ceil() as usize,
        rows,
    }
}

/// Paint the panel with every cap up, or every cap `pressed`, and say where
/// the caps went. The two paintings differ only inside those boxes.
fn paint(layout: &Layout, text: &mut Text, pressed: bool) -> (Canvas, Vec<Vec<CapBox>>) {
    let l = layout;
    let mut canvas = Canvas::new(l.w, l.h);
    canvas.material(0, 0, l.w, l.h, RADIUS * l.px, OVERLAY_ALPHA);

    let mut y = l.pad;
    text.draw(&mut canvas, TITLE, l.size, l.pad as i32, y as i32, theme::accent());
    y += l.line + l.line_gap;
    canvas.tint(
        l.pad as usize,
        y as usize,
        l.body_w as usize,
        l.rule as usize,
        theme::RULE,
        0x14,
    );
    y += l.rule + l.line_gap;

    let mut boxes = Vec::with_capacity(l.rows.len());
    for (row, binding) in l.rows.iter().zip(BINDINGS) {
        let mut x = l.pad;
        let mut caps = Vec::new();
        for (item, &width) in row.chord.items.iter().zip(&row.widths) {
            match *item {
                Item::Cap(key) => {
                    let at = CapBox {
                        x: x as usize,
                        y: y as usize,
                        w: width as usize,
                        h: (l.cap_h + l.depth) as usize,
                    };
                    draw_cap(&mut canvas, text, l, at, key, pressed);
                    caps.push(at);
                }
                Item::Sep(sep) => {
                    let (w, h) = text.measure(sep, l.size);
                    text.draw(
                        &mut canvas,
                        sep,
                        l.size,
                        (x + (width - w) / 2.0) as i32,
                        (y + (l.cap_h - h) / 2.0) as i32,
                        theme::TEXT_DIM,
                    );
                }
            }
            x += width;
        }
        boxes.push(caps);
        let h = text.measure(binding.description, l.size).1;
        text.draw(
            &mut canvas,
            binding.description,
            l.size,
            (l.pad + l.chord_w + l.column_gap) as i32,
            (y + (l.cap_h - h) / 2.0) as i32,
            theme::TEXT,
        );
        y += l.cap_h + l.depth + l.line_gap;
    }
    y += l.line_gap;
    text.draw(
        &mut canvas,
        FOOTER,
        l.size,
        l.pad as i32,
        y as i32,
        theme::TEXT_DIM,
    );

    (canvas, boxes)
}

/// One keycap. Raised, it stands on a shadow `depth` deep; pressed, its face
/// drops into the shadow's place and takes the accent.
fn draw_cap(canvas: &mut Canvas, text: &mut Text, l: &Layout, at: CapBox, key: &str, pressed: bool) {
    let (face_h, depth) = (l.cap_h as usize, l.depth as usize);
    let top = if pressed { at.y + depth } else { at.y };
    if !pressed {
        canvas.fill_rounded(at.x, at.y + depth, at.w, face_h, l.cap_radius, CAP_SHADOW);
    }
    // Opaque first: the faces are translucent, and the shadow showing through
    // one would make it look pressed when it is not.
    canvas.fill_rounded(at.x, top, at.w, face_h, l.cap_radius, theme::BACKGROUND);
    let (face, edge) = if pressed {
        (theme::accent().with_alpha(0x50), theme::accent().with_alpha(0xC0))
    } else {
        (theme::WELL_RAISED, theme::HAIRLINE)
    };
    canvas.fill_rounded(at.x, top, at.w, face_h, l.cap_radius, face);
    canvas.stroke_rounded(at.x, top, at.w, face_h, l.cap_radius, l.px, edge);
    let (w, h) = text.measure(key, l.label_size);
    text.draw(
        canvas,
        key,
        l.label_size,
        (at.x as f32 + (at.w as f32 - w) / 2.0) as i32,
        (top as f32 + (face_h as f32 - h) / 2.0) as i32,
        theme::TEXT,
    );
}

/// The pixels of `at`, row by row.
fn cut(canvas: &Canvas, at: CapBox) -> Vec<u8> {
    let mut patch = Vec::with_capacity(at.w * at.h * 4);
    for row in at.y..at.y + at.h {
        let start = (row * canvas.stride + at.x) * 4;
        patch.extend_from_slice(&canvas.pixels[start..start + at.w * 4]);
    }
    patch
}

/// Put a patch from [`cut`] back at `at`, in a buffer `stride` pixels wide.
fn paste(pixels: &mut [u8], stride: usize, at: CapBox, patch: &[u8]) {
    for (row, source) in patch.chunks_exact(at.w * 4).enumerate() {
        let start = ((at.y + row) * stride + at.x) * 4;
        pixels[start..start + source.len()].copy_from_slice(source);
    }
}

/// Lay the panel out and paint it with every cap up. Split from
/// [`Overlay::render`] so a test can get at the pixels without going through a
/// renderer.
#[cfg(test)]
fn compose(output: Rect, text: &mut Text, density: u32) -> Canvas {
    paint(&fit(output, text, density), text, false).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_centred_on_the_output() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let overlay = Overlay::render(output, &mut Text::new(), 1);
        let at = overlay.placement(output);
        // Integer division leaves an odd screen a pixel wider on one side.
        assert!(
            (output.w() - (at.x() * 2 + at.w())).abs() <= 1,
            "not horizontally centred"
        );
        assert!(
            (output.h() - (at.y() * 2 + at.h())).abs() <= 1,
            "not vertically centred"
        );
        assert!(
            at.w() <= output.w() && at.h() <= output.h(),
            "wider than the screen"
        );
    }

    #[test]
    fn it_is_placed_relative_to_the_output_not_the_origin() {
        // A second monitor's area does not start at (0, 0), and an overlay
        // centred on the origin would appear on the wrong screen. 1280x1024
        // also has height to spare and none to waste on width, which is what
        // catches a scale picked from one axis.
        let output = Rect::from_xywh(1920, 0, 1280, 1024);
        let overlay = Overlay::render(output, &mut Text::new(), 1);
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.right() <= output.right());
    }

    #[test]
    fn an_output_smaller_than_the_overlay_still_shows_its_top_left() {
        // Better a clipped list anchored at the corner than one centred so far
        // negative that the beginning of every line is off screen.
        let output = Rect::from_xywh(0, 0, 320, 200);
        let overlay = Overlay::render(output, &mut Text::new(), 1);
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.y() >= output.y());
    }

    #[test]
    fn a_1080p_screen_shows_the_whole_list() {
        // Keycaps are taller than the lines they replaced, and the table is
        // long; unshrunk, the bottom rows would sit off the screen.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let at = Overlay::render(output, &mut text, 1).placement(output);
        assert!(at.h() <= output.h(), "{} tall on a 1080 screen", at.h());
    }

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
        let layout = fit(output, &mut text, 1);
        let footer = text.measure(FOOTER, layout.size).0;
        assert!(
            layout.w as f32 >= footer + PAD * 2.0,
            "panel is {} wide but the footer needs {}",
            layout.w,
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
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1);
        // The corners are rounded, and outside the arc is the desktop by
        // design; everything else inside the panel must be painted.
        let reach = RADIUS.ceil() as usize;
        let (w, h) = (canvas.stride, canvas.height);
        let in_corner = |x: usize, y: usize| {
            (x < reach || x + reach >= w) && (y < reach || y + reach >= h)
        };
        let clear = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(i, p)| p[3] == 0 && !in_corner(i % w, i / w))
            .count();
        assert_eq!(
            clear, 0,
            "{clear} fully transparent pixels inside the panel"
        );
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
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1);
        let accent = theme::accent().to_rgba_bytes();
        let bg = theme::BACKGROUND.to_rgba_bytes();
        let partial = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| {
                // Somewhere strictly between the background and the accent.
                p[0] > bg[0].min(accent[0]) && p[0] < bg[0].max(accent[0]) && p[0] != bg[0]
            })
            .count();
        assert!(
            partial > 0,
            "no partially-covered pixels; text is not antialiased"
        );
    }

    #[test]
    fn a_taller_output_gets_a_larger_panel() {
        // Text scales with the output rather than the panel being blown up
        // afterwards, so a 4K screen gets a readable panel and not a big blur.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let small = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1);
        let large = compose(Rect::from_xywh(0, 0, 3840, 2160), &mut text, 1);
        assert!(
            large.height > small.height,
            "panel did not grow with the output"
        );
    }

    #[test]
    fn a_denser_output_gets_more_pixels_but_the_same_panel() {
        // The point of density: a 2× panel has twice the pixels each way and
        // is told so, so it occupies the same logical rectangle as the 1× one
        // and lands on a 2× screen pixel for pixel instead of being stretched.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let one = compose(output, &mut text, 1);
        let two = compose(output, &mut text, 2);
        // Shaped text does not scale to the pixel, so allow it a little slack.
        let close = |a: usize, b: usize| (a as f32 - b as f32).abs() <= (b as f32 * 0.02).max(2.0);
        assert!(
            close(two.stride, one.stride * 2),
            "{} vs {}",
            two.stride,
            one.stride
        );
        assert!(
            close(two.height, one.height * 2),
            "{} vs {}",
            two.height,
            one.height
        );

        let at_one = Overlay::render(output, &mut text, 1).placement(output);
        let at_two = Overlay::render(output, &mut text, 2).placement(output);
        assert!(close(at_two.w() as usize, at_one.w() as usize));
        assert!(close(at_two.h() as usize, at_one.h() as usize));
    }

    #[test]
    fn a_chord_is_its_keys_joined_by_plus() {
        let chord = parse("Super+Ctrl+H");
        assert_eq!(
            chord.items,
            [
                Item::Cap("Super"),
                Item::Sep("+"),
                Item::Cap("Ctrl"),
                Item::Sep("+"),
                Item::Cap("H"),
            ]
        );
        assert_eq!(chord.presses, [vec![0, 1, 2]]);
    }

    #[test]
    fn a_single_key_alternative_keeps_the_modifiers() {
        // `Super+Ctrl+Q / X` is Super+Ctrl+X the second time round, not a
        // lone X.
        let chord = parse("Super+Ctrl+Q / X");
        assert_eq!(chord.items.last(), Some(&Item::Cap("X")));
        assert!(chord.items.contains(&Item::Sep("/")));
        assert_eq!(chord.presses, [vec![0, 1, 2], vec![0, 1, 3]]);

        let pointer = parse("Super+click / drag");
        assert_eq!(pointer.presses, [vec![0, 1], vec![0, 2]]);
    }

    #[test]
    fn a_lone_key_is_one_cap() {
        assert_eq!(parse("Print").presses, [vec![0]]);
        assert_eq!(parse("Volume keys").items, [Item::Cap("Volume keys")]);
    }

    #[test]
    fn keys_go_down_in_order_hold_and_let_go_together() {
        let presses = [vec![0, 1, 2]];
        let at = |ms: u64| down(0, &presses, Duration::from_millis(ms));
        assert_eq!(at(0), &[] as &[usize], "nothing before the lead-in");
        assert_eq!(at(LEAD.as_millis() as u64), &[0]);
        assert_eq!(at((LEAD + STEP).as_millis() as u64), &[0, 1]);
        assert_eq!(at((LEAD + STEP * 2).as_millis() as u64), &[0, 1, 2]);
        let held = LEAD + STEP * 2 + HOLD;
        assert_eq!(at(held.as_millis() as u64 - 1), &[0, 1, 2], "still held");
        assert_eq!(at(held.as_millis() as u64), &[] as &[usize], "all let go");
    }

    #[test]
    fn alternatives_take_turns_from_one_cycle_to_the_next() {
        let presses = [vec![0, 1, 2], vec![0, 1, 3]];
        let chord = LEAD + STEP * 2;
        assert_eq!(down(0, &presses, chord), &[0, 1, 2]);
        assert_eq!(down(0, &presses, CYCLE + chord), &[0, 1, 3]);
        assert_eq!(down(0, &presses, CYCLE * 2 + chord), &[0, 1, 2]);
    }

    #[test]
    fn lower_rows_start_later() {
        let presses = [vec![0]];
        assert_eq!(down(0, &presses, LEAD), &[0]);
        assert_eq!(down(10, &presses, LEAD), &[] as &[usize]);
        assert_eq!(down(10, &presses, LEAD + STAGGER * 10), &[0]);
    }

    #[test]
    fn every_chord_finishes_before_its_cycle_ends() {
        // A chord still held when the next cycle starts would never be seen
        // let go, and its second alternative would begin half pressed.
        for binding in BINDINGS {
            let chord = parse(binding.chord);
            for press in &chord.presses {
                let busy = LEAD + STEP * press.len().saturating_sub(1) as u32 + HOLD;
                assert!(busy < CYCLE, "{} does not fit a cycle", binding.chord);
            }
        }
    }

    #[test]
    fn a_pressed_cap_looks_different_from_a_raised_one() {
        let mut overlay = Overlay::render(Rect::from_xywh(0, 0, 1920, 1080), &mut Text::new(), 1);
        assert!(!overlay.caps.is_empty());
        assert!(overlay.caps.iter().all(|cap| cap.up != cap.down));
        assert!(!overlay.animate(Duration::ZERO), "nothing is down at the start");
        assert!(overlay.animate(LEAD), "the first key goes down");
        assert!(!overlay.animate(LEAD), "and nothing more happens at the same moment");
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
    /// Set `HUGINN_OVERLAY_PRESSED=1` as well to see every cap down.
    ///
    /// Does nothing when the variable is unset, so it costs a CI run nothing.
    #[test]
    fn overlay_dump() {
        let Ok(path) = std::env::var("HUGINN_OVERLAY_DUMP") else {
            return;
        };
        let pressed = std::env::var_os("HUGINN_OVERLAY_PRESSED").is_some();
        let mut text = Text::new();
        let layout = fit(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1);
        let (canvas, _) = paint(&layout, &mut text, pressed);
        let (w, h) = (canvas.stride, canvas.height);
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        // The canvas is RGBA and PPM is RGB, so the alpha is dropped. Every
        // pixel the overlay draws is opaque enough for that to be honest.
        for pixel in canvas.pixels.as_chunks::<4>().0 {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {w}x{h} to {path}");
    }
}
