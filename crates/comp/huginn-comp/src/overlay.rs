//! The keybinding overlay.
//!
//! Compositor-drawn, like everything else the shell puts on screen. Text goes
//! through [`crate::text`] — real shaping and antialiased rasterization — which
//! is what lets this be a panel someone reads rather than a debugging aid.
//!
//! Keycaps and labels are painted once when the panel opens.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use huginn_core::geometry::Rect;

use crate::backend::keymap::BINDINGS;
use crate::canvas::{Canvas, Panel};
use crate::text::{Text, Weight};
use crate::theme::{self, Color};

/// Padding inside the panel's border, in pixels at 1x.
const PAD: f32 = 20.0;
/// Space between the chord column and the description column.
const COLUMN_GAP: f32 = 24.0;
/// Space between one column of bindings and the next.
const COLUMNS_GAP: f32 = 40.0;
/// Blank space around the title rule and above the footer.
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

/// The top of a key, where the legend is printed.
const CAP_FACE: Color = Color::from_argb(0xFFE4_E7EC);
/// The sloped sides between the face and the skirt.
const CAP_RIM: Color = Color::from_argb(0xFFC3_C8D1);
/// The fixed base below the key face.
const CAP_SKIRT: Color = Color::from_argb(0xFF96_9CA8);
const CAP_LABEL: Color = Color::from_argb(0xFF1D_2027);
/// The light along the top edge of the face.
const CAP_SHINE: Color = Color::from_argb(0xFFFF_FFFF);
/// The fixed shadow below a key.
const CAP_SHADOW: Color = Color::from_argb(0x5A00_0000);

const TITLE: &str = "Huginn keybindings";
const FOOTER: &str =
    "Esc or a click outside closes this. Plain Super belongs to the focused application.";

/// The keybinding overlay.
#[derive(Debug)]
pub(crate) struct Overlay {
    panel: Panel,
}

impl Overlay {
    /// Draw the overlay, sized to sit comfortably on `output`, at `density`
    /// pixels per logical one.
    pub(crate) fn render(output: Rect, text: &mut Text, density: u32) -> Self {
        Self {
            panel: Panel::from_canvas(&compose(output, text, density), density),
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

/// A piece of a chord as it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A key, drawn as a cap.
    Cap(&'static str),
    /// The `+` between keys, or the `/` between alternatives.
    Sep(&'static str),
}

/// A chord from [`BINDINGS`], taken apart into what to draw.
#[derive(Debug, PartialEq, Eq)]
struct Chord {
    items: Vec<Item>,
}

/// Keep shared modifiers and alternatives as written in the bindings table.
fn parse(chord: &'static str) -> Chord {
    let mut items = Vec::new();
    for (i, alternative) in chord.split(" / ").enumerate() {
        if i > 0 {
            items.push(Item::Sep("/"));
        }
        for (j, key) in alternative
            .split('+')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .enumerate()
        {
            if j > 0 {
                items.push(Item::Sep("+"));
            }
            items.push(Item::Cap(key));
        }
    }
    Chord { items }
}

/// A keycap and its padding, in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapBox {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// A row of the table, placed.
struct RowLayout {
    chord: Chord,
    /// The left edge and width of each of the chord's items, in order. For a
    /// cap that is the key itself, without its padding.
    placed: Vec<(f32, f32)>,
    caps: Vec<CapBox>,
    /// The top of the row's boxes.
    y: f32,
    desc_x: f32,
}

/// Everything about the panel's geometry, worked out once and painted from.
/// All in canvas pixels.
struct Layout {
    px: f32,
    size: f32,
    /// How many columns the bindings were split into. Only a test reads it,
    /// to check that a 1080p screen gets the two that make the list fit.
    #[cfg_attr(not(test), allow(dead_code))]
    columns: usize,
    line: f32,
    pad: f32,
    line_gap: f32,
    rule: f32,
    /// A key's face and sides; the skirt below adds [`Self::lip`].
    cap_h: f32,
    lip: f32,
    bevel: f32,
    radius: f32,
    /// Padding around each key.
    cap_pad: f32,
    /// Space above the key.
    cap_top: f32,
    label_size: f32,
    body_w: f32,
    footer_y: f32,
    w: usize,
    h: usize,
    rows: Vec<RowLayout>,
}

/// Lay the panel out at a size and column count that fits on `output`.
///
/// Text grows with the output rather than the panel being scaled after the
/// fact: rasterizing at the final size is the whole reason for a real font
/// stack, and scaling a bitmap afterwards would throw that away. Keycaps take
/// far more height than a line of text, so the list is split into two
/// columns where the screen is wide enough, and shrunk only as far as it has
/// to be to fit.
///
/// Everything is in the canvas's own pixels, of which a 2× output has twice
/// as many per logical pixel as a 1× one. `px` is that factor; it goes into
/// every dimension and `Panel::from_canvas` divides it back out, so the panel
/// is placed at the same logical size either way.
fn fit(output: Rect, text: &mut Text, density: u32) -> Layout {
    let px = density.max(1) as f32;
    let room = (output.w() as f32 * px * FILL, output.h() as f32 * px * FILL);
    let wanted = (BASE_SIZE * (output.h() as f32 / 1080.0)).clamp(BASE_SIZE, BASE_SIZE * 2.5) * px;
    // Best first: fits both ways, then fits across (a list clipped at the
    // bottom still reads from the top; one clipped at the side loses the end
    // of every line), then whichever overflows least — and among equals the
    // larger text.
    let rank = |layout: &Layout| {
        let (over_w, over_h) = (layout.w as f32 / room.0, layout.h as f32 / room.1);
        (
            over_w <= 1.0 && over_h <= 1.0,
            over_w <= 1.0,
            -over_w.max(over_h),
            layout.size,
        )
    };
    let mut best: Option<Layout> = None;
    for columns in [1, 2] {
        let layout = shrink_to_fit(text, wanted, px, room, columns);
        let better = best.as_ref().is_none_or(|best| {
            rank(&layout).partial_cmp(&rank(best)) == Some(std::cmp::Ordering::Greater)
        });
        if better {
            best = Some(layout);
        }
    }
    best.expect("at least one column count is tried")
}

/// [`lay_out`] at `size`, shrunk until it fits `room` or reaches the floor.
fn shrink_to_fit(text: &mut Text, size: f32, px: f32, room: (f32, f32), columns: usize) -> Layout {
    let floor = MIN_SIZE * px;
    let mut size = size;
    let mut layout = lay_out(text, size, px, columns);
    // Shaped text is not quite linear in its size and the padding does not
    // scale with it at all, so one proportional step can leave the panel a
    // little over. A few more converge.
    for _ in 0..4 {
        let over = (layout.w as f32 / room.0).max(layout.h as f32 / room.1);
        if over <= 1.0 || size <= floor {
            break;
        }
        size = (size / over).max(floor);
        layout = lay_out(text, size, px, columns);
    }
    layout
}

/// Measure and place the panel at text `size`, in `columns` columns.
fn lay_out(text: &mut Text, size: f32, px: f32, columns: usize) -> Layout {
    // Key dimensions follow the text, rounded at 1x and then multiplied out,
    // so a 2× panel is exactly twice a 1× one rather than rounding apart.
    let unit = size / px;
    let m = |k: f32| (unit * k).round().max(1.0) * px;
    let cap_h = m(1.75);
    let lip = m(0.2).max(2.0 * px);
    let bevel = m(0.14);
    let radius = m(0.36);
    let cap_pad = m(0.3).max(m(0.11) + 2.0 * px);
    let cap_top = cap_pad;
    let label_pad = m(0.6);
    let sep_gap = m(0.3);
    let row_gap = m(0.15);
    let label_size = size * 0.88;
    let box_h = cap_top + cap_h + lip + cap_pad;

    let line = size * 1.35;
    let (pad, column_gap, columns_gap, line_gap, rule) = (
        PAD * px,
        COLUMN_GAP * px,
        COLUMNS_GAP * px,
        LINE_GAP * px,
        px,
    );

    let mut measured = Vec::with_capacity(BINDINGS.len());
    for binding in BINDINGS {
        let chord = parse(binding.chord);
        let mut widths = Vec::with_capacity(chord.items.len());
        for item in &chord.items {
            widths.push(match *item {
                // Never much narrower than tall: a single letter is a square
                // key, not a sliver.
                Item::Cap(key) => (text.measure_weighted(key, label_size, Weight::BOLD).0
                    + label_pad * 2.0)
                    .max(cap_h * 1.05)
                    .ceil(),
                Item::Sep(sep) => {
                    text.measure_weighted(sep, size, Weight::BOLD).0.ceil() + sep_gap * 2.0
                }
            });
        }
        let description = text.measure(binding.description, size).0;
        measured.push((chord, widths, description));
    }

    let top = pad + line + line_gap + rule + line_gap;
    let per_column = BINDINGS.len().div_ceil(columns.max(1));
    let mut rows = Vec::with_capacity(BINDINGS.len());
    let mut longest = 0;
    let mut x = pad;
    let mut remaining = measured.into_iter();
    loop {
        let column: Vec<_> = remaining.by_ref().take(per_column).collect();
        if column.is_empty() {
            break;
        }
        let chord_w = column
            .iter()
            .map(|(_, widths, _)| widths.iter().sum::<f32>())
            .fold(0.0_f32, f32::max)
            + cap_pad * 2.0;
        let description_w = column.iter().map(|(_, _, w)| *w).fold(0.0_f32, f32::max);
        let desc_x = x + chord_w + column_gap;
        longest = longest.max(column.len());
        for (i, (chord, widths, _)) in column.into_iter().enumerate() {
            let y = (top + i as f32 * (box_h + row_gap)).round();
            let mut left = x + cap_pad;
            let mut placed = Vec::with_capacity(widths.len());
            let mut caps = Vec::new();
            for (item, width) in chord.items.iter().zip(widths) {
                if let Item::Cap(_) = item {
                    caps.push(CapBox {
                        x: (left - cap_pad) as usize,
                        y: y as usize,
                        w: (width + cap_pad * 2.0) as usize,
                        h: box_h as usize,
                    });
                }
                placed.push((left, width));
                left += width;
            }
            rows.push(RowLayout {
                chord,
                placed,
                caps,
                y,
                desc_x,
            });
        }
        x = (desc_x + description_w + columns_gap).ceil();
    }
    let columns_w = x - columns_gap - pad;
    let body_w = columns_w
        .max(text.measure(TITLE, size).0)
        .max(text.measure(FOOTER, size).0);
    let rows_h = longest as f32 * box_h + longest.saturating_sub(1) as f32 * row_gap;
    let footer_y = top + rows_h + line_gap * 2.0;

    Layout {
        px,
        size,
        columns,
        line,
        pad,
        line_gap,
        rule,
        cap_h,
        lip,
        bevel,
        radius,
        cap_pad,
        cap_top,
        label_size,
        body_w,
        footer_y,
        w: (body_w + pad * 2.0).ceil() as usize,
        h: (footer_y + line + pad).ceil() as usize,
        rows,
    }
}

/// Everything on the panel but the keys: the ground, the title, the `+` and
/// `/` between keys, the descriptions and the footer.
fn paint_base(l: &Layout, text: &mut Text) -> Canvas {
    let mut canvas = Canvas::new(l.w, l.h);
    canvas.material(0, 0, l.w, l.h, RADIUS * l.px, OVERLAY_ALPHA);

    let y = l.pad;
    text.draw(
        &mut canvas,
        TITLE,
        l.size,
        l.pad as i32,
        y as i32,
        theme::accent(),
    );
    canvas.tint(
        l.pad as usize,
        (y + l.line + l.line_gap) as usize,
        l.body_w as usize,
        l.rule as usize,
        theme::RULE,
        0x14,
    );

    for (row, binding) in l.rows.iter().zip(BINDINGS) {
        let face = row.y + l.cap_top;
        for (item, &(x, width)) in row.chord.items.iter().zip(&row.placed) {
            if let Item::Sep(sep) = *item {
                let (w, h) = text.measure_weighted(sep, l.size, Weight::BOLD);
                text.draw_weighted(
                    &mut canvas,
                    sep,
                    l.size,
                    (x + (width - w) / 2.0).round() as i32,
                    (face + (l.cap_h - h) / 2.0).round() as i32,
                    theme::TEXT,
                    Weight::BOLD,
                );
            }
        }
        let h = text.measure(binding.description, l.size).1;
        text.draw(
            &mut canvas,
            binding.description,
            l.size,
            row.desc_x as i32,
            (face + (l.cap_h - h) / 2.0).round() as i32,
            theme::TEXT,
        );
    }

    text.draw(
        &mut canvas,
        FOOTER,
        l.size,
        l.pad as i32,
        l.footer_y as i32,
        theme::TEXT_DIM,
    );
    canvas
}

/// Every key on the panel.
fn draw_caps(canvas: &mut Canvas, l: &Layout, text: &mut Text) {
    for row in &l.rows {
        let mut caps = row.caps.iter();
        for item in &row.chord.items {
            if let Item::Cap(key) = *item
                && let Some(&at) = caps.next()
            {
                draw_cap(canvas, text, l, at, key);
            }
        }
    }
}

/// A stationary keycap.
fn draw_cap(canvas: &mut Canvas, text: &mut Text, l: &Layout, at: CapBox, key: &str) {
    let w = at.w as f32 - l.cap_pad * 2.0;
    let fx = at.x as f32 + l.cap_pad;
    let fy = at.y as f32 + l.cap_top;
    let top = fy;
    let body_h = l.cap_h + l.lip;
    let (x, wu) = (fx as usize, w as usize);

    canvas.fill_rounded(
        x,
        (fy + l.px * 2.0) as usize,
        wu,
        (l.cap_h + l.lip) as usize,
        l.radius,
        CAP_SHADOW,
    );
    canvas.fill_rounded(x, top as usize, wu, body_h as usize, l.radius, CAP_SKIRT);
    canvas.fill_rounded(x, top as usize, wu, l.cap_h as usize, l.radius, CAP_RIM);
    let face_x = fx + l.bevel;
    let face_y = top + (l.bevel * 0.5).round();
    let face_w = w - l.bevel * 2.0;
    let face_h = l.cap_h - (l.bevel * 1.5).round();
    let face_r = (l.radius - l.bevel).max(l.px);
    canvas.fill_rounded(
        face_x as usize,
        face_y as usize,
        face_w as usize,
        face_h as usize,
        face_r,
        CAP_FACE,
    );
    if face_w > face_r * 2.0 {
        canvas.tint(
            (face_x + face_r) as usize,
            face_y as usize,
            (face_w - face_r * 2.0) as usize,
            l.px as usize,
            CAP_SHINE,
            0x90,
        );
    }
    let (tw, th) = text.measure_weighted(key, l.label_size, Weight::BOLD);
    text.draw_weighted(
        canvas,
        key,
        l.label_size,
        (face_x + (face_w - tw) / 2.0).round() as i32,
        (face_y + (face_h - th) / 2.0).round() as i32,
        CAP_LABEL,
        Weight::BOLD,
    );
}

/// Paint the complete static panel.
fn compose(output: Rect, text: &mut Text, density: u32) -> Canvas {
    let layout = fit(output, text, density);
    let mut canvas = paint_base(&layout, text);
    draw_caps(&mut canvas, &layout, text);
    canvas
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
    fn a_1080p_screen_shows_the_whole_list_in_two_columns() {
        // Keycaps are much taller than lines of text, and the table is long;
        // in one column the bottom rows would sit off the screen.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let layout = fit(output, &mut text, 1);
        assert_eq!(layout.columns, 2);
        let at = Overlay::render(output, &mut text, 1).placement(output);
        assert!(at.h() <= output.h(), "{} tall on a 1080 screen", at.h());
        assert!(at.w() <= output.w(), "{} wide on a 1920 screen", at.w());
    }

    #[test]
    fn caps_never_overlap_each_other_or_the_descriptions() {
        // Keep keycaps separate from neighboring caps and descriptions.
        let mut text = Text::new();
        for output in [
            Rect::from_xywh(0, 0, 1920, 1080),
            Rect::from_xywh(0, 0, 1280, 1024),
            Rect::from_xywh(0, 0, 3840, 2160),
        ] {
            for density in [1, 2] {
                let layout = fit(output, &mut text, density);
                let boxes: Vec<(CapBox, f32)> = layout
                    .rows
                    .iter()
                    .flat_map(|row| row.caps.iter().map(|at| (*at, row.desc_x)))
                    .collect();
                for (i, (a, desc_x)) in boxes.iter().enumerate() {
                    assert!(
                        a.x + a.w <= layout.w && a.y + a.h <= layout.h,
                        "{a:?} off the panel"
                    );
                    assert!(
                        ((a.x + a.w) as f32) < *desc_x,
                        "{a:?} runs into its description"
                    );
                    for (b, _) in &boxes[i + 1..] {
                        let overlap = a.x < b.x + b.w
                            && b.x < a.x + a.w
                            && a.y < b.y + b.h
                            && b.y < a.y + a.h;
                        assert!(!overlap, "{a:?} overlaps {b:?}");
                    }
                }
            }
        }
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
        let in_corner =
            |x: usize, y: usize| (x < reach || x + reach >= w) && (y < reach || y + reach >= h);
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
    }

    #[test]
    fn a_single_key_alternative_keeps_the_modifiers() {
        // `Super+Ctrl+Q / X` is Super+Ctrl+X the second time round, not a
        // lone X.
        let chord = parse("Super+Ctrl+Q / X");
        assert_eq!(chord.items.last(), Some(&Item::Cap("X")));
        assert!(chord.items.contains(&Item::Sep("/")));

        let pointer = parse("Super+click / drag");
        assert_eq!(pointer.items.last(), Some(&Item::Cap("drag")));
    }

    #[test]
    fn a_lone_key_is_one_cap() {
        assert_eq!(parse("Print").items, [Item::Cap("Print")]);
        assert_eq!(parse("Volume keys").items, [Item::Cap("Volume keys")]);
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
        let mut text = Text::new();
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1);
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
