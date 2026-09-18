//! The menu a panel opens beside itself: the pin bar's and the dock's.
//!
//! Two panels draw the same object — a card with the application at the top,
//! that application's own desktop actions under it, and last, past a rule,
//! the things the desktop does *to* it: pin, unpin, quit. They draw it from
//! here rather than each from its own copy, for the reason [`crate::theme`]
//! gives about the accent: two copies that agree today are two copies that
//! disagree after the next change to one of them.
//!
//! # The material
//!
//! Its own glass at the ordinary panel alpha, not the near-opaque shade the
//! launcher's in-panel menu uses. That one floats *over* a panel and has to
//! read against it; this one floats over the desktop beside the panel that
//! opened it, so it is the same material as that panel.
//!
//! # What the rows are
//!
//! Sections, with a rule between them, because the mockups group the same
//! way: what the application offers, then what the desktop offers. A row that
//! takes something away — Unpin, Quit — is drawn in [`crate::theme::CRITICAL`]
//! and washed in it when chosen, which is the one place that colour appears
//! outside an urgent notification. It has to: those are the only rows here
//! that a person cannot undo by pressing the thing again.

use huginn_core::geometry::Rect;
use raven_desktop::{Icons, Pixmaps};

use crate::canvas::Canvas;
use crate::launcher::{self, Metrics};
use crate::text::{Text, Weight};

/// The menu's width at a 1080p output, in logical pixels.
pub(crate) const WIDTH: f32 = 236.0;
/// The mark beside a row, drawn in a box this wide.
const GLYPH: f32 = 18.0;
/// Padding inside the card.
const PAD: f32 = 8.0;
/// A chosen row's corner radius: a row on Raven Glass's radius scale.
const ROW_RADIUS: f32 = 8.0;

/// What the mark beside a row should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark<'a> {
    /// Something the application offers. Drawn with the action's own `Icon=`
    /// when it named one, and otherwise as a window — which is what a
    /// desktop action opens.
    Action(Option<&'a str>),
    /// Something that takes away. Drawn as a cross, in the critical colour,
    /// and the label with it.
    Danger,
}

/// One row of the menu.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row<'a> {
    pub label: &'a str,
    pub mark: Mark<'a>,
}

impl<'a> Row<'a> {
    pub(crate) fn action(label: &'a str, icon: Option<&'a str>) -> Self {
        Self {
            label,
            mark: Mark::Action(icon),
        }
    }

    pub(crate) fn danger(label: &'a str) -> Self {
        Self {
            label,
            mark: Mark::Danger,
        }
    }

    fn removes(&self) -> bool {
        matches!(self.mark, Mark::Danger)
    }
}

/// How many rows there are across every section.
pub(crate) fn rows(sections: &[&[Row]]) -> usize {
    sections.iter().map(|section| section.len()).sum()
}

/// The card's width and height for `sections`, in canvas pixels.
///
/// The header is a row's worth, there is a rule under it, and a rule between
/// each pair of sections. An empty section takes no room and draws no rule,
/// so a caller may hand over a group it turned out to have nothing for.
pub(crate) fn size(m: &Metrics, sections: &[&[Row]]) -> (f32, f32) {
    let pad = PAD * m.scale;
    let rule = rule_height(m);
    let filled = sections.iter().filter(|s| !s.is_empty()).count();
    // One rule under the header, and one above every section after the first.
    let rules = rule * filled.max(1) as f32;
    (
        WIDTH * m.scale,
        pad * 2.0 + m.row * (rows(sections) + 1) as f32 + rules,
    )
}

/// Draw the card at `(x, y)` with `selected` washed, and say where each row
/// went. The rectangles are in canvas pixels and in flattened row order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    canvas: &mut Canvas,
    text: &mut Text,
    m: &Metrics,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    title: (&str, Option<&str>),
    sections: &[&[Row]],
    selected: Option<usize>,
    (x, y): (f32, f32),
) -> Vec<Rect> {
    let Metrics {
        scale,
        size: text_size,
        row,
        density,
        ..
    } = *m;
    let pad = PAD * scale;
    let rule = rule_height(m);
    let (w, h) = size(m, sections);
    canvas.material(
        x as usize,
        y as usize,
        w as usize,
        h as usize,
        crate::theme::CARD_RADIUS * scale,
        crate::theme::PANEL_ALPHA,
    );

    let glyph = GLYPH * scale;
    let text_left = x + pad + glyph + pad * 0.75;
    let text_room = x + w - pad - text_left;

    // The header: the application's own icon and its name, set heavier than
    // the rows under it. It is not a row — there is nothing to choose here,
    // only the answer to "whose menu is this".
    let mut at = y + pad;
    let (name, icon) = title;
    let head = glyph.max(text_size);
    if let Some(pixmap) = icon
        .and_then(|name| icons.find(name, head as u32 / density, density))
        .and_then(|path| pixmaps.get(&path, head as u32))
    {
        canvas.blit(
            (x + pad) as usize,
            (at + (row - head) / 2.0) as usize,
            pixmap,
        );
    }
    let name = launcher::fit(text, name, text_size, text_room);
    text.draw_weighted(
        canvas,
        &name,
        text_size,
        text_left as i32,
        (at + (row - text_size * 1.35) / 2.0) as i32,
        crate::theme::TEXT,
        Weight::SEMIBOLD,
    );
    at += row;

    let mut hits = Vec::with_capacity(rows(sections));
    for section in sections.iter().filter(|s| !s.is_empty()) {
        draw_rule(canvas, x + pad, at, w - pad * 2.0, rule);
        at += rule;
        for item in section.iter() {
            let n = hits.len();
            hits.push(Rect::from_xywh(
                (x + pad / 2.0) as i32,
                at as i32,
                (w - pad) as i32,
                row as i32,
            ));
            if selected == Some(n) {
                // The selection wash, in the row's own colour: the accent for
                // an action, the critical red for one that takes away.
                let wash = if item.removes() {
                    crate::theme::CRITICAL.with_alpha(0x3A)
                } else {
                    crate::theme::selection()
                };
                canvas.fill_rounded(
                    (x + pad / 2.0) as usize,
                    at as usize,
                    (w - pad) as usize,
                    row as usize,
                    ROW_RADIUS * scale,
                    wash,
                );
            }
            let (gx, gy) = (x + pad, at + (row - glyph) / 2.0);
            match item.mark {
                // An action may carry an `Icon=` of its own, and when it does
                // that is the mark this row wants — it is the one the
                // application chose for exactly this.
                Mark::Action(Some(name))
                    if icons
                        .find(name, glyph as u32 / density, density)
                        .and_then(|path| pixmaps.get(&path, glyph as u32))
                        .map(|pixmap| canvas.blit(gx as usize, gy as usize, pixmap))
                        .is_some() => {}
                Mark::Action(_) => draw_window_mark(canvas, gx, gy, glyph, scale),
                Mark::Danger => draw_cross(canvas, gx, gy, glyph, scale),
            }
            let ink = if item.removes() {
                crate::theme::CRITICAL
            } else {
                crate::theme::TEXT
            };
            let label = launcher::fit(text, item.label, text_size, text_room);
            text.draw(
                canvas,
                &label,
                text_size,
                text_left as i32,
                (at + (row - text_size * 1.35) / 2.0) as i32,
                ink,
            );
            at += row;
        }
    }
    hits
}

fn rule_height(m: &Metrics) -> f32 {
    1.0_f32.max(m.scale * 0.75)
}

/// A hairline across the card at `y`.
fn draw_rule(canvas: &mut Canvas, x: f32, y: f32, w: f32, height: f32) {
    canvas.fill_rounded(
        x as usize,
        y as usize,
        w as usize,
        height.max(1.0) as usize,
        0.0,
        crate::theme::RULE,
    );
}

/// The mark beside a row that removes: a cross, in the critical colour.
///
/// Painted as two anti-aliased segments rather than stepped squares, which is
/// what [`Canvas::paint`] is for — a diagonal stepped a pixel at a time draws
/// a dotted line, because each step lands on the pixel the last one did and
/// a one-pixel rounded square is mostly corner.
fn draw_cross(canvas: &mut Canvas, x: f32, y: f32, size: f32, scale: f32) {
    let half = (1.5 * scale).max(1.5) / 2.0;
    let inset = size * 0.18;
    let (lo, hi) = (x + inset, x + size - inset);
    let (top, bottom) = (y + inset, y + size - inset);
    let ink = crate::theme::CRITICAL;
    canvas.paint(
        (x - 1.0) as i32,
        (y - 1.0) as i32,
        (size + 2.0) as i32,
        (size + 2.0) as i32,
        |px, py| {
            let a = segment_distance(px, py, lo, top, hi, bottom);
            let b = segment_distance(px, py, hi, top, lo, bottom);
            let coverage = (half - a.min(b) + 0.5).clamp(0.0, 1.0);
            (coverage > 0.0).then_some((ink, coverage))
        },
    );
}

/// The mark beside an action with no icon of its own: a window, which is
/// what a desktop action opens.
///
/// Drawn rather than themed, for the reason the dock's launcher glyph is —
/// this is not an application and has no `Icon=` to take one from — and
/// geometric rather than typographic, so a font that lacks a dingbat cannot
/// leave the row with a blank beside it.
fn draw_window_mark(canvas: &mut Canvas, x: f32, y: f32, size: f32, scale: f32) {
    let line = (1.5 * scale).max(1.5);
    let ink = crate::theme::TEXT_DIM;
    let radius = (2.0 * scale).max(1.5);
    canvas.stroke_rounded(
        x as usize,
        y as usize,
        size as usize,
        size as usize,
        radius,
        line,
        ink,
    );
    // The title bar, a clear gap below the frame's top edge so the two do
    // not merge into one thick line at the sizes a menu row uses.
    canvas.fill_rounded(
        (x + line) as usize,
        (y + line * 2.5) as usize,
        (size - line * 2.0) as usize,
        line as usize,
        0.0,
        ink,
    );
}

/// How far `(px, py)` is from the segment `(ax, ay)`–`(bx, by)`.
fn segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    let t = if length <= f32::EPSILON {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / length).clamp(0.0, 1.0)
    };
    (px - (ax + dx * t)).hypot(py - (ay + dy * t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_core::geometry::Rect;

    const OUTPUT: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    fn metrics() -> Metrics {
        Metrics::for_output(OUTPUT, 1)
    }

    #[test]
    fn a_section_that_is_empty_costs_nothing() {
        let m = metrics();
        let actions = [Row::action("New Window", None)];
        let desktop = [Row::danger("Quit")];
        let with_gap = size(&m, &[&actions, &desktop]);
        let without = size(&m, &[&actions, &[], &desktop]);
        assert_eq!(with_gap, without, "an empty section drew a rule");
        assert_eq!(rows(&[&actions, &[], &desktop]), 2);
    }

    #[test]
    fn every_row_is_hit_and_they_sit_inside_the_card() {
        let m = metrics();
        let actions = [
            Row::action("New Window", None),
            Row::action("New Private Window", Some("window-new")),
        ];
        let desktop = [Row::action("Pin", None), Row::danger("Quit")];
        let sections: [&[Row]; 2] = [&actions, &desktop];
        let (w, h) = size(&m, &sections);
        let mut canvas = Canvas::new(w as usize + 20, h as usize + 20);
        let mut text = Text::new();
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let hits = draw(
            &mut canvas,
            &mut text,
            &m,
            &icons,
            &mut pixmaps,
            ("Firefox", None),
            &sections,
            Some(3),
            (10.0, 10.0),
        );
        assert_eq!(hits.len(), 4);
        for (n, rect) in hits.iter().enumerate() {
            assert!(rect.x() >= 10, "row {n} left the card");
            assert!(
                rect.bottom() <= 10 + h as i32,
                "row {n} fell out the bottom"
            );
            assert!(rect.right() <= 10 + w as i32, "row {n} ran past the edge");
        }
        // In order, and no two rows on top of each other.
        for pair in hits.windows(2) {
            assert!(pair[1].y() >= pair[0].bottom(), "rows overlap");
        }
    }

    #[test]
    fn the_row_that_takes_away_is_drawn_in_the_critical_colour() {
        let m = metrics();
        let ordinary: [&[Row]; 1] = [&[Row::action("New Window", None)]];
        let removing: [&[Row]; 1] = [&[Row::danger("Quit")]];
        let paint = |sections: &[&[Row]]| {
            let (w, h) = size(&m, sections);
            let mut canvas = Canvas::new(w as usize, h as usize);
            let mut text = Text::new();
            let icons = Icons::discover(crate::theme::ICON_THEME);
            let mut pixmaps = Pixmaps::new();
            draw(
                &mut canvas,
                &mut text,
                &m,
                &icons,
                &mut pixmaps,
                ("Firefox", None),
                sections,
                None,
                (0.0, 0.0),
            );
            let critical = crate::theme::CRITICAL.to_rgba_bytes();
            canvas
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] == critical[0] && p[1] == critical[1] && p[2] == critical[2])
                .count()
        };
        assert_eq!(paint(&ordinary), 0, "an ordinary row went red");
        assert!(paint(&removing) > 0, "the removing row did not");
    }
}
