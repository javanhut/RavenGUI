//! The keybinding overlay.
//!
//! Compositor-drawn, like everything else the shell puts on screen. Text goes
//! through [`crate::text`] — real shaping and antialiased rasterization — which
//! is what lets this be a panel someone reads rather than a debugging aid.
//!
//! Keycaps and labels are painted when the panel opens, and again on every
//! keystroke that changes the filter — the table is thirty-odd rows and grew
//! past the point where "read the whole thing" is how anybody finds a chord,
//! so typing narrows it to the rows that match.

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::keyboard::keysyms;

use std::collections::HashMap;

use huginn_core::geometry::Rect;

use crate::backend::keymap::{BINDINGS, Binding};
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
const FOOTER: &str = "Type to filter. Esc clears the filter, then closes this — \
so does a click outside. Plain Super belongs to the focused application.";

/// Shown in place of the table when the filter matches nothing. Better than an
/// empty panel, which reads as the overlay having broken rather than as a
/// query having come up short.
const NO_MATCH: &str = "No binding matches that.";
/// Between the title and the filter at the other end of its row.
const HEAD_GAP: f32 = 32.0;

/// The footer, given how many rows the screen had no room for and how wide
/// the line may be.
///
/// A list that simply stops is one you have no reason to think has stopped —
/// you read to the bottom and take it for the whole table. Saying how many are
/// missing is what turns that into a filter you know to reach for.
///
/// Two lengths, because the footer is a line of prose and on a small screen it
/// is the widest thing on the panel: at 640 pixels the long form alone made
/// the panel wider than the screen it was explaining. It steps down rather
/// than being clipped — half a sentence is worse than a short one — and what
/// it gives up first is the note about the `Super` layer, which is the one
/// part also written down in the startup log and the docs.
fn footer(hidden: usize, room: f32, size: f32, text: &mut Text) -> String {
    let long = if hidden == 0 {
        FOOTER.to_owned()
    } else {
        format!("{hidden} more — type to filter. Esc clears the filter, then closes this.")
    };
    if text.measure(&long, size).0 <= room {
        return long;
    }
    if hidden == 0 {
        "Type to filter. Esc closes this.".to_owned()
    } else {
        format!("{hidden} more — type to filter.")
    }
}

/// A keystroke the overlay takes while it is up.
///
/// Only the keys a filter needs. Everything else falls through to the chord it
/// would otherwise have been, which is the point of the panel: the list is
/// there to be read *while* the chords on it are tried, so `Super`+`Ctrl`+`Q`
/// still closes a window with the overlay open over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// Add a character to the filter.
    Insert(char),
    /// Drop the last one.
    Backspace,
    /// Clear the filter, or — with nothing to clear — close the overlay.
    Escape,
    /// Not the overlay's. See [`Self::from_keysym`].
    Ignored,
}

impl Key {
    /// What a keysym means to the overlay. `character` is what the layout
    /// produces, so the filter takes what was pressed rather than what a US
    /// keyboard would have made of it.
    ///
    /// The caller has already established that no modifier is held, so a
    /// control character here is a key with no printable form — Insert, F5 —
    /// rather than a chord, and either way not something to type.
    pub(crate) fn from_keysym(sym: u32, character: Option<char>) -> Self {
        match sym {
            keysyms::KEY_Escape => Self::Escape,
            keysyms::KEY_BackSpace => Self::Backspace,
            _ => match character {
                Some(c) if !c.is_control() => Self::Insert(c),
                _ => Self::Ignored,
            },
        }
    }
}

/// The bindings a filter leaves, in table order.
///
/// Every whitespace-separated word of `query` has to appear somewhere in the
/// row — in the chord or in the description, case ignored. Words rather than
/// the whole string so that "super wheel" finds the row written
/// `Super+Ctrl+wheel`, where a plain substring search would not: nobody types
/// a chord's punctuation, and the two halves of a row are two different kinds
/// of thing to be searching.
///
/// Substring rather than fuzzy on purpose. A filter that quietly keeps a row
/// because its letters appear in order somewhere is one you cannot trust to
/// have excluded anything, and the whole value here is in what is *left*.
fn matching(query: &str) -> Vec<&'static Binding> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    BINDINGS
        .iter()
        .filter(|binding| {
            if terms.is_empty() {
                return true;
            }
            let haystack = format!("{} {}", binding.chord, binding.description).to_lowercase();
            terms.iter().all(|term| haystack.contains(term.as_str()))
        })
        .collect()
}

/// The right-hand end of the title row while a filter is on: what was typed,
/// and how much of the table it has left.
fn head(query: &str, shown: usize) -> Option<String> {
    (!query.is_empty()).then(|| format!("{query}   ·   {shown} of {}", BINDINGS.len()))
}

/// The keybinding overlay.
#[derive(Debug)]
pub(crate) struct Overlay {
    panel: Panel,
}

impl Overlay {
    /// Draw the overlay, sized to sit comfortably on `output`, at `density`
    /// pixels per logical one, showing the rows `query` leaves.
    ///
    /// An empty `query` is the whole table, which is what opening it gives
    /// you: the filter is there for when reading the list is slower than
    /// describing the thing you want.
    pub(crate) fn render(output: Rect, text: &mut Text, density: u32, query: &str) -> Self {
        Self {
            panel: Panel::from_canvas(&compose(output, text, density, query), density),
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
    /// The bindings these rows are for: the whole table, or what the filter
    /// left of it. Held here rather than re-derived at paint time, so the
    /// geometry and the text can never be measured from two different lists.
    shown: Vec<&'static Binding>,
    /// What to print at the other end of the title row. `None` with no filter.
    head: Option<String>,
    /// The footer line. Not a constant, because it says how many rows the
    /// screen had no room for. See [`footer`].
    foot: String,
    /// How many matching rows did not fit. Only a test reads it; the footer
    /// it produced is what the panel shows.
    #[cfg_attr(not(test), allow(dead_code))]
    hidden: usize,
}

/// Text measurements, kept across the passes one [`fit`] makes.
///
/// `fit` lays the panel out up to ten times — two column counts, each shrunk
/// until it fits — over the same few dozen strings, and shaping them is most
/// of what that costs. That was paid once per opening before the panel had a
/// filter; now it is paid on every keystroke, which is the difference between
/// measuring each string once per size and dropping a frame per character.
///
/// Keyed by the string, the size and the weight, which is everything a
/// measurement depends on. Every string here is `&'static str` from
/// [`BINDINGS`] or a constant above, so the key borrows rather than copies.
type Measures = HashMap<(&'static str, u32, bool), (f32, f32)>;

/// What the panel is being laid out *for*, which is the same for every pass
/// [`fit`] makes: the rows to show, the filter that chose them, and how much
/// of the screen there is to fill.
///
/// Gathered rather than passed as four more arguments, because only `size` and
/// `columns` vary between passes and a call that took all six positionally was
/// one nobody could read or check.
#[derive(Clone, Copy)]
struct Request<'a> {
    shown: &'a [&'static Binding],
    query: &'a str,
    /// Canvas pixels per logical pixel. See [`fit`].
    px: f32,
    /// The width and height the panel may fill, in canvas pixels.
    room: (f32, f32),
}

/// [`Text::measure_weighted`], memoized in `seen`.
fn measure(
    seen: &mut Measures,
    text: &mut Text,
    s: &'static str,
    size: f32,
    bold: bool,
) -> (f32, f32) {
    *seen.entry((s, size.to_bits(), bold)).or_insert_with(|| {
        if bold {
            text.measure_weighted(s, size, Weight::BOLD)
        } else {
            text.measure(s, size)
        }
    })
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
fn fit(output: Rect, text: &mut Text, density: u32, query: &str) -> Layout {
    let shown = matching(query);
    let px = density.max(1) as f32;
    let room = (output.w() as f32 * px * FILL, output.h() as f32 * px * FILL);
    let wanted = (BASE_SIZE * (output.h() as f32 / 1080.0)).clamp(BASE_SIZE, BASE_SIZE * 2.5) * px;
    // Best first: fitting, then showing the most of the table, then — for a
    // screen too small for any of them to fit — overflowing least, and among
    // equals the larger text.
    //
    // Fitting comes first on its own, with nothing above it. The ranking used
    // to prefer a layout that fitted *across* over one that fitted both ways,
    // on the grounds that a list clipped at the bottom still reads from the
    // top. That is true of the first screenful and false of everything after
    // it: on a 1366x768 panel it chose a single column 1195 pixels tall, and a
    // third of the bindings were drawn past the bottom of the screen with
    // nothing able to scroll to them. Now every candidate fits vertically by
    // construction — `lay_out` leaves out what there is no room for — so the
    // question is no longer whether the list is clipped but how much of it
    // each shape can show, which is what the second key asks.
    let rank = |layout: &Layout| {
        let (over_w, over_h) = (layout.w as f32 / room.0, layout.h as f32 / room.1);
        let fits = over_w <= 1.0 && over_h <= 1.0;
        (
            fits,
            // Only among the shapes that fit. A layout that does not fit shows
            // whatever the screen can hold of it and no more, so counting its
            // rows would rate it on rows nobody can see — which is how a 640
            // wide screen came to prefer a panel twice that wide for the extra
            // column it would have had.
            if fits { layout.rows.len() } else { 0 },
            -over_w.max(over_h),
            layout.size,
        )
    };
    let mut best: Option<Layout> = None;
    let mut seen = Measures::new();
    let request = Request {
        shown: &shown,
        query,
        px,
        room,
    };
    for columns in [1, 2] {
        let layout = shrink_to_fit(&mut seen, text, wanted, columns, request);
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
fn shrink_to_fit(
    seen: &mut Measures,
    text: &mut Text,
    size: f32,
    columns: usize,
    request: Request<'_>,
) -> Layout {
    let (px, room) = (request.px, request.room);
    let floor = MIN_SIZE * px;
    let mut size = size;
    let mut layout = lay_out(seen, text, size, columns, request);
    // Shaped text is not quite linear in its size and the padding does not
    // scale with it at all, so one proportional step can leave the panel a
    // little over. A few more converge.
    for _ in 0..4 {
        let over = (layout.w as f32 / room.0).max(layout.h as f32 / room.1);
        if over <= 1.0 || size <= floor {
            break;
        }
        size = (size / over).max(floor);
        layout = lay_out(seen, text, size, columns, request);
    }
    layout
}

/// Measure and place the panel at text `size`, in `columns` columns, for the
/// rows in `shown`.
fn lay_out(
    seen: &mut Measures,
    text: &mut Text,
    size: f32,
    columns: usize,
    request: Request<'_>,
) -> Layout {
    let Request {
        shown, query, px, ..
    } = request;
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
    let top = pad + line + line_gap + rule + line_gap;

    // How many rows there is height for, and so how many of them get drawn.
    //
    // The text stops shrinking at `MIN_SIZE`, because a list too small to read
    // is no better than one that runs off the edge — so on a screen that
    // cannot hold the table even at the floor, what does not fit is *left out*
    // rather than pushed past the bottom. A 1366x768 laptop panel is one of
    // those, which is not an edge case: before this, a third of the bindings
    // were drawn below the screen, where nothing could scroll to them.
    //
    // Everything above and below the rows is fixed — title, rule, footer and
    // the padding around them — so the room left for rows is what is left of
    // the panel's height after them.
    let columns = columns.max(1);
    let chrome = top + line_gap * 2.0 + line + pad;
    let for_rows = (request.room.1 - chrome).max(box_h);
    let fits = (((for_rows + row_gap) / (box_h + row_gap)).floor() as usize).max(1);
    let per_column = shown.len().div_ceil(columns).max(1).min(fits);
    let hidden = shown.len().saturating_sub(per_column * columns);
    let shown = &shown[..shown.len() - hidden];

    let mut measured = Vec::with_capacity(shown.len());
    for binding in shown {
        let chord = parse(binding.chord);
        let mut widths = Vec::with_capacity(chord.items.len());
        for item in &chord.items {
            widths.push(match *item {
                // Never much narrower than tall: a single letter is a square
                // key, not a sliver.
                Item::Cap(key) => (measure(seen, text, key, label_size, true).0 + label_pad * 2.0)
                    .max(cap_h * 1.05)
                    .ceil(),
                Item::Sep(sep) => measure(seen, text, sep, size, true).0.ceil() + sep_gap * 2.0,
            });
        }
        let description = measure(seen, text, binding.description, size, false).0;
        measured.push((chord, widths, description));
    }

    let mut rows = Vec::with_capacity(shown.len());
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
    // With nothing matched there are no columns at all, and `x` never moved
    // off the padding — so the table's width is the message that stands in
    // for it.
    let head = head(query, shown.len());
    let columns_w = if shown.is_empty() {
        measure(seen, text, NO_MATCH, size, false).0
    } else {
        x - columns_gap - pad
    };
    // The filter's own text is the one string here that is not `'static`, and
    // it is one string: measured outright rather than given a key of its own.
    let title_w = measure(seen, text, TITLE, size, false).0
        + head
            .as_deref()
            .map_or(0.0, |head| HEAD_GAP * px + text.measure(head, size).0);
    // Measured against the screen's own room rather than against the rest of
    // the panel: the footer may be as long as the panel is allowed to be, and
    // a filter that matched one row must not drag the note down to its width.
    let foot = footer(hidden, (request.room.0 - pad * 2.0).max(0.0), size, text);
    let body_w = columns_w.max(title_w).max(text.measure(&foot, size).0);
    // The empty state still needs a line's worth of room, or the footer would
    // come up under the rule and the panel would look like it had lost its
    // middle rather than like it had nothing to show.
    let rows_h = if shown.is_empty() {
        line
    } else {
        longest as f32 * box_h + longest.saturating_sub(1) as f32 * row_gap
    };
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
        shown: shown.to_vec(),
        head,
        foot,
        hidden,
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
    // What was typed, at the far end of the title's own row: it belongs with
    // the heading rather than above the list, where it would read as a row of
    // the table.
    if let Some(head) = &l.head {
        let w = text.measure(head, l.size).0;
        text.draw(
            &mut canvas,
            head,
            l.size,
            (l.pad + l.body_w - w).round() as i32,
            y as i32,
            theme::TEXT,
        );
    }
    canvas.tint(
        l.pad as usize,
        (y + l.line + l.line_gap) as usize,
        l.body_w as usize,
        l.rule as usize,
        theme::RULE,
        0x14,
    );

    if l.shown.is_empty() {
        let top = l.pad + l.line + l.line_gap + l.rule + l.line_gap;
        text.draw(
            &mut canvas,
            NO_MATCH,
            l.size,
            l.pad as i32,
            top as i32,
            theme::TEXT_DIM,
        );
    }

    for (row, binding) in l.rows.iter().zip(&l.shown) {
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
        &l.foot,
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
fn compose(output: Rect, text: &mut Text, density: u32, query: &str) -> Canvas {
    let layout = fit(output, text, density, query);
    let mut canvas = paint_base(&layout, text);
    draw_caps(&mut canvas, &layout, text);
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chords a filter leaves, which is what the panel then draws.
    fn chords(query: &str) -> Vec<&'static str> {
        matching(query).iter().map(|b| b.chord).collect()
    }

    #[test]
    fn an_empty_filter_is_the_whole_table() {
        assert_eq!(matching("").len(), BINDINGS.len());
        // Whitespace is no filter either: a stray space must not empty the
        // panel out.
        assert_eq!(matching("   ").len(), BINDINGS.len());
    }

    #[test]
    fn a_filter_keeps_only_the_rows_that_match() {
        let left = chords("workspace");
        assert!(!left.is_empty(), "nothing matched 'workspace'");
        assert!(left.len() < BINDINGS.len(), "everything matched");
        for binding in matching("workspace") {
            let row = format!("{} {}", binding.chord, binding.description).to_lowercase();
            assert!(row.contains("workspace"), "{row:?} does not match");
        }
    }

    #[test]
    fn the_filter_reads_the_chord_as_well_as_the_description() {
        // "print" is in no description, only in the key's own name — and
        // looking up what a key you can see does is at least as common as
        // looking up the key for a thing you can describe.
        let left = chords("print");
        assert!(
            left.iter().any(|chord| chord.contains("Print")),
            "the Print rows are missing: {left:?}"
        );
    }

    #[test]
    fn every_word_of_the_filter_has_to_match() {
        // Words rather than one substring: nobody types a chord's `+`, and
        // "super wheel" has to find `Super+Ctrl+wheel`.
        let both = chords("super wheel");
        assert!(!both.is_empty(), "nothing matched 'super wheel'");
        for chord in &both {
            let chord = chord.to_lowercase();
            assert!(
                chord.contains("super") && chord.contains("wheel"),
                "{chord}"
            );
        }
        // And a second word narrows rather than widens.
        assert!(both.len() <= chords("wheel").len());
    }

    #[test]
    fn the_filter_ignores_case() {
        assert_eq!(chords("WORKSPACE"), chords("workspace"));
        assert_eq!(chords("Super+Ctrl+Q"), chords("super+ctrl+q"));
    }

    #[test]
    fn a_filter_that_matches_nothing_still_draws_a_readable_panel() {
        // An empty panel reads as the overlay having broken. It keeps its
        // title, its footer and a line saying so.
        let mut text = Text::new();
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let nonsense = "zzzzz";
        assert!(matching(nonsense).is_empty(), "'{nonsense}' matched a row");
        let layout = fit(output, &mut text, 1, nonsense);
        assert!(layout.rows.is_empty());
        assert!(layout.h > 0 && layout.w > 0, "the panel collapsed");
        let at = Overlay::render(output, &mut text, 1, nonsense).placement(output);
        assert!(at.w() <= output.w() && at.h() <= output.h());
        assert!(at.x() >= output.x() && at.y() >= output.y());
    }

    #[test]
    fn a_filtered_panel_is_no_taller_than_the_unfiltered_one() {
        // The list shrinks as it narrows; a filter that made the panel grow
        // downwards would be one that pushed rows off the screen.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let whole = fit(output, &mut text, 1, "");
        for query in ["workspace", "window", "print", "zzzzz"] {
            let filtered = fit(output, &mut text, 1, query);
            assert!(
                filtered.h <= whole.h,
                "{query:?} made the panel taller: {} vs {}",
                filtered.h,
                whole.h
            );
        }
    }

    #[test]
    fn the_filter_takes_typing_and_leaves_the_rest_alone() {
        use smithay::input::keyboard::keysyms;
        assert_eq!(
            Key::from_keysym(keysyms::KEY_a, Some('a')),
            Key::Insert('a')
        );
        // What the layout produces, not what a US keyboard would have.
        assert_eq!(
            Key::from_keysym(keysyms::KEY_a, Some('ä')),
            Key::Insert('ä')
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_space, Some(' ')),
            Key::Insert(' ')
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_BackSpace, None),
            Key::Backspace
        );
        assert_eq!(Key::from_keysym(keysyms::KEY_Escape, None), Key::Escape);
        // A key with no printable form is nobody's filter. Escape and
        // Backspace both produce control characters, so they are matched by
        // keysym above rather than being left to this.
        assert_eq!(Key::from_keysym(keysyms::KEY_F5, None), Key::Ignored);
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Return, Some('\r')),
            Key::Ignored
        );
    }

    /// Every screen the panel has to work on, smallest first. 640x480 is the
    /// floor this promises to fit; below it there is no readable way to put a
    /// chord and a sentence side by side, and the panel falls back to showing
    /// its top-left corner.
    const SCREENS: &[(i32, i32)] = &[
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1280, 1024),
        (1366, 768),
        (1440, 900),
        (1600, 900),
        (1680, 1050),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];

    #[test]
    fn the_panel_never_outgrows_the_screen_it_is_on() {
        // The one thing the overlay has to get right: a list drawn past the
        // bottom of the screen is a list nothing can scroll to. A 1366x768
        // laptop panel used to get a single column 1195 pixels tall.
        let mut text = Text::new();
        for &(w, h) in SCREENS {
            let output = Rect::from_xywh(0, 0, w, h);
            for density in [1, 2] {
                for query in ["", "w", "window", "zzzzz"] {
                    let at = Overlay::render(output, &mut text, density, query).placement(output);
                    assert!(
                        at.w() <= output.w() && at.h() <= output.h(),
                        "{w}x{h} @{density}x {query:?}: panel is {}x{}",
                        at.w(),
                        at.h()
                    );
                    assert!(
                        at.x() >= output.x()
                            && at.y() >= output.y()
                            && at.right() <= output.right()
                            && at.y() + at.h() <= output.y() + output.h(),
                        "{w}x{h} @{density}x {query:?}: panel sits at {at:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_screen_with_no_room_for_the_table_says_how_much_it_left_out() {
        // A list that simply stops reads as the whole list. The count is what
        // sends somebody to the filter for the rest.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1280, 720);
        let layout = fit(output, &mut text, 1, "");
        assert!(layout.hidden > 0, "1280x720 fitted the whole table");
        assert_eq!(
            layout.rows.len() + layout.hidden,
            BINDINGS.len(),
            "the rows drawn and the rows left out are not the whole table"
        );
        assert!(
            layout.foot.starts_with(&format!("{} more", layout.hidden)),
            "the footer does not say what was left out: {:?}",
            layout.foot
        );
    }

    #[test]
    fn a_screen_with_room_for_the_table_leaves_nothing_out() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let layout = fit(output, &mut text, 1, "");
        assert_eq!(layout.hidden, 0);
        assert_eq!(layout.rows.len(), BINDINGS.len());
        assert_eq!(layout.foot, FOOTER);
    }

    #[test]
    fn a_filter_reaches_the_rows_a_small_screen_left_out() {
        // The point of the count in the footer: what did not fit is a query
        // away, not lost.
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1280, 720);
        let whole = fit(output, &mut text, 1, "");
        let dropped = BINDINGS[whole.rows.len()..]
            .first()
            .expect("some row fell off");
        let found = fit(output, &mut text, 1, dropped.description);
        assert_eq!(found.hidden, 0, "the filtered list did not fit either");
        assert!(
            found.shown.iter().any(|b| b.chord == dropped.chord),
            "{:?} is off the panel and the filter cannot reach it",
            dropped.chord
        );
    }

    #[test]
    fn the_filter_searches_the_description() {
        // Looking up "how do I lock the screen" is at least as common as
        // looking up what a key you can see does, and the words somebody
        // reaches for are the ones in the description.
        for (query, chord) in [
            ("terminal", "Super+Ctrl+E / T"),
            ("lock", "Super+L"),
            ("volume", "Volume keys"),
            ("paste", "Super+V"),
            // Two words from the description, in neither the chord nor next
            // to each other in the sentence.
            ("put away", "Super+Ctrl+M"),
        ] {
            let left = chords(query);
            assert!(
                left.contains(&chord),
                "{query:?} did not find {chord:?}; it found {left:?}"
            );
        }
    }

    #[test]
    fn it_is_centred_on_the_output() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let overlay = Overlay::render(output, &mut Text::new(), 1, "");
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
        let overlay = Overlay::render(output, &mut Text::new(), 1, "");
        let at = overlay.placement(output);
        assert!(at.x() >= output.x() && at.right() <= output.right());
    }

    #[test]
    fn an_output_smaller_than_the_overlay_still_shows_its_top_left() {
        // Better a clipped list anchored at the corner than one centred so far
        // negative that the beginning of every line is off screen.
        let output = Rect::from_xywh(0, 0, 320, 200);
        let overlay = Overlay::render(output, &mut Text::new(), 1, "");
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
        let layout = fit(output, &mut text, 1, "");
        assert_eq!(layout.columns, 2);
        let at = Overlay::render(output, &mut text, 1, "").placement(output);
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
                let layout = fit(output, &mut text, density, "");
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
        let layout = fit(output, &mut text, 1, "");
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
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1, "");
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
        let canvas = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1, "");
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
        let small = compose(Rect::from_xywh(0, 0, 1920, 1080), &mut text, 1, "");
        let large = compose(Rect::from_xywh(0, 0, 3840, 2160), &mut text, 1, "");
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
        let one = compose(output, &mut text, 1, "");
        let two = compose(output, &mut text, 2, "");
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

        let at_one = Overlay::render(output, &mut text, 1, "").placement(output);
        let at_two = Overlay::render(output, &mut text, 2, "").placement(output);
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
        // `HUGINN_OVERLAY_FILTER` dumps the panel as the filter leaves it,
        // which is the only way to see the narrowed layout without a session.
        let query = std::env::var("HUGINN_OVERLAY_FILTER").unwrap_or_default();
        // `HUGINN_OVERLAY_OUTPUT=1366x768` dumps the panel as a screen that
        // size gets it, which is how the row cap is looked at rather than
        // reasoned about.
        let (w, h) = std::env::var("HUGINN_OVERLAY_OUTPUT")
            .ok()
            .and_then(|spec| {
                let (w, h) = spec.split_once('x')?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            })
            .unwrap_or((1920, 1080));
        let mut text = Text::new();
        let canvas = compose(Rect::from_xywh(0, 0, w, h), &mut text, 1, &query);
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
