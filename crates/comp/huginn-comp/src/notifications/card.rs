//! A notification card: how one notification looks, where the stack of them
//! sits, and what on a card the pointer is over.
//!
//! One card is one [`Panel`], in the desktop's material like every other
//! floating surface the shell draws, and laid out the way Raven Glass lays out
//! a card in the GTK applications: an icon, a quiet line naming the
//! application, the summary set heavier than anything around it, and up to
//! three lines of body in the dim text colour. The accent is not used. In
//! Raven Glass it marks the selected item and the primary action, and a card
//! is neither. A critical notification is marked with the error colour down
//! its left edge instead.
//!
//! A card's actions are a row of controls under the text, wells like the ones
//! quick settings draws, a shade lighter under the pointer. The close control
//! appears in the top-right corner while the pointer is on the card, and not
//! otherwise: a card nobody is pointing at is something to read, not something
//! to operate.
//!
//! Composed when a notification arrives or changes, or when the pointer moves
//! onto a different part of it, then kept. Sliding and fading happen at draw
//! time from the card's [`crate::anim::Reveal`], so motion never recomposes
//! pixels.

use huginn_core::geometry::Rect;
use huginn_core::notify::{Action, Notification, Span, Urgency};
use raven_desktop::{Entry, Icons, Pixmap, Pixmaps};

use crate::canvas::{Canvas, Panel};
use crate::text::{Styled, Text, Weight};
use crate::theme;

/// A card's width at a 1080p output, in logical pixels.
const WIDTH: f32 = 380.0;
/// Padding inside the card: what quick settings uses.
const PAD: f32 = 16.0;
/// The application icon's size.
const ICON: f32 = 40.0;
/// Between the icon and the text.
const ICON_GAP: f32 = 12.0;
/// Type sizes at a 1080p output. The summary leads by weight as much as by
/// size, as Raven Glass has it; the application name and the body step back to
/// the dim colour.
const APP_SIZE: f32 = 12.0;
const SUMMARY_SIZE: f32 = 15.0;
const BODY_SIZE: f32 = 14.0;
/// Lines of body shown before the rest is cut off with an ellipsis.
const BODY_LINES: usize = 3;
/// The width of the critical mark down the left edge.
const EDGE: f32 = 3.0;
/// The most actions drawn. A notification offering more is offering a menu,
/// and a card is not the place for one.
const MAX_BUTTONS: usize = 3;
/// An action control's height, the space above the row of them, and the space
/// between two.
const BUTTON_H: f32 = 30.0;
const BUTTON_TOP: f32 = 12.0;
const BUTTON_GAP: f32 = 8.0;
/// A control's corner radius: the first step of Raven Glass's radius scale.
const BUTTON_RADIUS: f32 = 6.0;
/// A control's label size, and the room kept either side of it.
const BUTTON_SIZE: f32 = 13.0;
const BUTTON_PAD: f32 = 8.0;
/// The close control: its diameter, how far it sits in from the corner, and
/// the size of the cross.
const CLOSE: f32 = 22.0;
const CLOSE_INSET: f32 = 8.0;
const CLOSE_GLYPH: f32 = 15.0;
/// How far a card slides in from, as a share of its width.
const SLIDE: f32 = 0.35;
/// Between the stack and the edges of the area windows may use, in logical
/// pixels.
const MARGIN: i32 = 12;
/// Between one card and the next.
const SPACING: i32 = theme::GAP;
/// What is drawn for a notification that names no icon the theme has.
const FALLBACK_ICON: &str = "preferences-desktop-notification";

/// What on a card the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// Anywhere that is not a control.
    Body,
    Close,
    /// One of the action controls, counted from the left.
    Button(usize),
}

/// Where a card's controls are, in logical pixels from its top-left corner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Hits {
    close: Option<Rect>,
    buttons: Vec<Rect>,
}

impl Hits {
    /// What is at `x`, `y`, measured from the card's top-left corner.
    pub(crate) fn target(&self, x: i32, y: i32) -> Target {
        let inside = |r: &Rect| x >= r.x() && y >= r.y() && x < r.x() + r.w() && y < r.y() + r.h();
        if self.close.as_ref().is_some_and(inside) {
            return Target::Close;
        }
        self.buttons
            .iter()
            .position(inside)
            .map_or(Target::Body, Target::Button)
    }

    /// From the canvas's pixels to logical ones.
    fn logical(self, density: u32) -> Self {
        let density = density.max(1) as i32;
        let down = |r: Rect| {
            Rect::from_xywh(
                r.x() / density,
                r.y() / density,
                r.w() / density,
                r.h() / density,
            )
        };
        Self {
            close: self.close.map(down),
            buttons: self.buttons.into_iter().map(down).collect(),
        }
    }
}

/// Draw `notification`'s card for `output` at `density` pixels per logical
/// one, with `hover` saying what on it the pointer is over, if anything.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    notification: &Notification,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    apps: &[Entry],
    output: Rect,
    density: u32,
    hover: Option<Target>,
) -> (Panel, Hits) {
    let density = density.max(1);
    let (canvas, hits) = compose(
        notification,
        text,
        icons,
        pixmaps,
        apps,
        output,
        density,
        hover,
    );
    (Panel::from_canvas(&canvas, density), hits.logical(density))
}

#[allow(clippy::too_many_arguments)]
fn compose(
    notification: &Notification,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    apps: &[Entry],
    output: Rect,
    density: u32,
    hover: Option<Target>,
) -> (Canvas, Hits) {
    // In the canvas's own pixels, `density` times the logical ones. See
    // `Panel::from_canvas`.
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let width = WIDTH * scale;
    let pad = PAD * scale;
    let icon_px = (ICON * scale).round() as u32;
    let icon = icon_for(notification, icons, pixmaps, apps, icon_px, density);

    let text_x = match icon {
        Some(_) => pad + icon_px as f32 + ICON_GAP * scale,
        None => pad,
    };
    let text_w = (width - text_x - pad).max(1.0);
    let (app_size, summary_size, body_size) =
        (APP_SIZE * scale, SUMMARY_SIZE * scale, BODY_SIZE * scale);

    // The close control sits over the end of the first line, so that line
    // stops short of it whether or not it is showing: text that moved when the
    // pointer arrived would be text that moved while somebody read it.
    let (close, inset) = (CLOSE * scale, CLOSE_INSET * scale);
    let clear = (close + inset + 4.0 * scale - pad).max(0.0);
    let first_w = (text_w - clear).max(1.0);

    // One line each: a summary that wraps is a body, and belongs there.
    let summary = one_line(&notification.summary);
    let app_name = one_line(&notification.app_name);
    let body = styled(&notification.body);
    let body_lines = if body.is_empty() {
        0
    } else {
        text.wrapped_lines(&body, body_size, text_w).min(BODY_LINES)
    };
    let buttons: Vec<&Action> = notification.buttons().take(MAX_BUTTONS).collect();

    let mut text_h = body_lines as f32 * Text::line_height(body_size);
    if !app_name.is_empty() {
        text_h += Text::line_height(app_size);
    }
    if !summary.is_empty() {
        text_h += Text::line_height(summary_size);
    }
    let icon_h = if icon.is_some() { icon_px as f32 } else { 0.0 };
    let content_h = text_h.max(icon_h);
    let button_h = BUTTON_H * scale;
    let row_h = if buttons.is_empty() {
        0.0
    } else {
        BUTTON_TOP * scale + button_h
    };
    let height = pad * 2.0 + content_h + row_h;

    let (w, h) = (width.round() as usize, height.round() as usize);
    let mut canvas = Canvas::new(w, h);
    canvas.material(0, 0, w, h, theme::CARD_RADIUS * scale, theme::panel_alpha());

    if notification.urgency == Urgency::Critical {
        let edge = (EDGE * scale).max(2.0);
        canvas.fill_rounded(
            (pad * 0.4) as usize,
            pad as usize,
            edge as usize,
            h.saturating_sub((pad * 2.0) as usize),
            edge / 2.0,
            theme::CRITICAL,
        );
    }

    if let Some(icon) = &icon {
        let top = pad + ((text_h - icon_h) / 2.0).max(0.0);
        canvas.blit(pad as usize, top as usize, icon);
    }

    // The text block sits level with the middle of the icon when it is the
    // shorter of the two, so a one-line card does not hang off the icon's top.
    let mut y = pad + ((icon_h - text_h) / 2.0).max(0.0);
    if !app_name.is_empty() {
        text.draw_wrapped(
            &mut canvas,
            &[Styled::plain(&app_name)],
            app_size,
            text_x,
            y,
            first_w,
            1,
            theme::text_dim(),
        );
        y += Text::line_height(app_size);
    }
    if !summary.is_empty() {
        let summary_w = if app_name.is_empty() { first_w } else { text_w };
        text.draw_wrapped(
            &mut canvas,
            // Bold, as the overlay sets its emphasis: the faces Raven ships
            // are regular and bold, and a weight between them falls back to
            // other fonts glyph by glyph.
            &[Styled::weighted(&summary, Weight::BOLD)],
            summary_size,
            text_x,
            y,
            summary_w,
            1,
            theme::text(),
        );
        y += Text::line_height(summary_size);
    }
    if body_lines > 0 {
        text.draw_wrapped(
            &mut canvas,
            &body,
            body_size,
            text_x,
            y,
            text_w,
            BODY_LINES,
            theme::text_dim(),
        );
    }

    // The actions, sharing the width under the text between them.
    let mut button_rects = Vec::with_capacity(buttons.len());
    if !buttons.is_empty() {
        let row_y = pad + content_h + BUTTON_TOP * scale;
        let row_w = width - pad - text_x;
        let gap = BUTTON_GAP * scale;
        let count = buttons.len() as f32;
        let each = ((row_w - gap * (count - 1.0)) / count).max(1.0);
        let label_size = BUTTON_SIZE * scale;
        for (index, action) in buttons.iter().enumerate() {
            let x = text_x + index as f32 * (each + gap);
            let raised = hover == Some(Target::Button(index));
            canvas.fill_rounded(
                x as usize,
                row_y as usize,
                each as usize,
                button_h as usize,
                BUTTON_RADIUS * scale,
                if raised {
                    theme::well_raised()
                } else {
                    theme::well()
                },
            );
            let label = one_line(&action.label);
            let room = (each - BUTTON_PAD * scale * 2.0).max(1.0);
            let label_w = text.measure(&label, label_size).0.min(room);
            text.draw_wrapped(
                &mut canvas,
                &[Styled::plain(&label)],
                label_size,
                x + (each - label_w) / 2.0,
                row_y + (button_h - Text::line_height(label_size)) / 2.0,
                room,
                1,
                theme::text(),
            );
            button_rects.push(pixel_rect(x, row_y, each, button_h));
        }
    }

    let (close_x, close_y) = (width - inset - close, inset);
    if hover.is_some() {
        let on = hover == Some(Target::Close);
        canvas.fill_rounded(
            close_x as usize,
            close_y as usize,
            close as usize,
            close as usize,
            close / 2.0,
            if on { theme::well_raised() } else { theme::well() },
        );
        let glyph_size = CLOSE_GLYPH * scale;
        let (glyph_w, glyph_h) = text.measure("×", glyph_size);
        text.draw(
            &mut canvas,
            "×",
            glyph_size,
            (close_x + (close - glyph_w) / 2.0).round() as i32,
            (close_y + (close - glyph_h) / 2.0).round() as i32,
            if on { theme::text() } else { theme::text_dim() },
        );
    }

    let hits = Hits {
        close: Some(pixel_rect(close_x, close_y, close, close)),
        buttons: button_rects,
    };
    (canvas, hits)
}

fn pixel_rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::from_xywh(
        x.round() as i32,
        y.round() as i32,
        w.round() as i32,
        h.round() as i32,
    )
}

/// Where each card goes: stacked down from the top-right corner of `area`,
/// in drawing order, each given as its logical size and its reveal.
///
/// A card on its way in or out slides from the right as it fades, and its slot
/// in the stack grows or shrinks with it, so the cards below close the gap
/// smoothly when one leaves rather than jumping up.
pub(crate) fn stack(area: Rect, cards: &[((i32, i32), f32)]) -> Vec<Rect> {
    let mut y = area.y() + MARGIN;
    cards
        .iter()
        .map(|&((w, h), reveal)| {
            let reveal = reveal.clamp(0.0, 1.0);
            let slide = ((1.0 - reveal) * w as f32 * SLIDE).round() as i32;
            let x = area.x() + area.w() - w - MARGIN + slide;
            let rect = Rect::from_xywh(x, y, w, h);
            y += ((h + SPACING) as f32 * reveal).round() as i32;
            rect
        })
        .collect()
}

/// The icon to draw, at `size` pixels.
///
/// What the notification names first — a theme name, a path, or a `file://`
/// URI — then the icon of the application its `desktop-entry` hint names,
/// then a symbolic bell tinted the way the launcher tints its glyphs. A named
/// application icon keeps its own colours, as it does in the dock.
fn icon_for(
    notification: &Notification,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    apps: &[Entry],
    size: u32,
    density: u32,
) -> Option<Pixmap> {
    let logical = size / density;
    let own = Some(notification.app_icon.trim())
        .filter(|name| !name.is_empty())
        .map(|name| name.strip_prefix("file://").unwrap_or(name));
    let from_entry = notification
        .desktop_entry
        .as_deref()
        .and_then(|id| entry_for(apps, id))
        .and_then(|entry| entry.icon.as_deref());
    let found = own
        .and_then(|name| icons.find(name, logical, density))
        .or_else(|| from_entry.and_then(|name| icons.find(name, logical, density)));
    if let Some(path) = found {
        return pixmaps.get(&path, size).cloned();
    }
    let path = crate::launcher::launcher_icon(icons, FALLBACK_ICON, logical, density)?;
    pixmaps.get(&path, size).map(crate::launcher::tinted)
}

/// The installed application a `desktop-entry` hint names.
///
/// Matched the way the dock matches a window's `app_id` to its entry, so a
/// notification and the window it came from resolve to the same icon. Clients
/// send the desktop file's name with or without its `.desktop` suffix.
pub(crate) fn entry_for<'a>(apps: &'a [Entry], id: &str) -> Option<&'a Entry> {
    let id = id.strip_suffix(".desktop").unwrap_or(id);
    apps.iter().find(|entry| crate::dock::matches(entry, id))
}

/// `text` with every run of whitespace, line breaks included, made one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The body's spans as text runs. Underline and links are not drawn: the text
/// renderer has no underline, and a link cannot be followed from a card yet.
fn styled(spans: &[Span]) -> Vec<Styled<'_>> {
    spans
        .iter()
        .filter(|span| !span.text.is_empty())
        .map(|span| Styled {
            text: &span.text,
            weight: if span.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            },
            italic: span.italic,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use huginn_core::notify::Request;
    use std::time::Duration;

    const AREA: Rect = Rect::from_xywh(0, 40, 1920, 1040);
    const OUTPUT: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    /// Draw sample cards over a dark desktop into a PNG, for looking at the
    /// design from a shell. Writes nothing unless `HUGINN_CARD_PREVIEW` names
    /// a directory:
    ///
    /// ```text
    /// HUGINN_CARD_PREVIEW=/tmp cargo test -p huginn-comp card_preview -- --ignored
    /// ```
    #[test]
    #[ignore = "writes a picture; see the comment above"]
    fn card_preview() {
        let Some(dir) = std::env::var_os("HUGINN_CARD_PREVIEW") else {
            eprintln!("skipped: HUGINN_CARD_PREVIEW is not set");
            return;
        };
        let mut text = Text::new();
        let icons = Icons::discover(theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let sample = |app: &str, icon: &str, summary: &str, body: &str, urgency: u8| {
            let request = Request {
                app_name: app.into(),
                app_icon: icon.into(),
                summary: summary.into(),
                body: body.into(),
                urgency: Some(urgency),
                ..Request::default()
            };
            Notification::from_request(1, request, Duration::ZERO)
        };
        let mut store = sample(
            "Raven Store",
            "system-software-install",
            "3 updates are ready",
            "Firefox, Mesa and linux-raven can be updated.",
            1,
        );
        store.actions = Action::from_pairs(
            &["default", "Open", "update", "Update now", "later", "Later"].map(String::from),
        );
        let samples = [
            (
                sample(
                    "Raven Oracle",
                    "",
                    "Answer ready: raven-keycast isn't working",
                    "I can't see <b>raven-keycast</b> in anything I can see here: no service, no \
                     log entry, no package reference. Start by confirming it is installed with \
                     rvn find raven-keycast, then check its log.",
                    1,
                ),
                None,
            ),
            (store, Some(Target::Button(0))),
            (
                sample(
                    "Power",
                    "battery-caution",
                    "Battery at 5%",
                    "Plug in soon. The machine suspends at 3%.",
                    2,
                ),
                Some(Target::Close),
            ),
            (
                sample("", "edit-copy", "Copied to the clipboard", "", 0),
                None,
            ),
        ];
        let cards: Vec<Canvas> = samples
            .iter()
            .map(|(n, hover)| compose(n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 1, *hover).0)
            .collect();

        let border = 24;
        let width = WIDTH as usize + border * 2;
        let height = cards
            .iter()
            .map(|card| card.height + SPACING as usize)
            .sum::<usize>()
            + border * 2;
        // A dark, slightly purple desktop, darker at the top, for the glass to
        // sit on.
        let mut pixels = vec![0u8; width * height * 4];
        for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let t = (index / width) as f32 / height as f32;
            *pixel = [
                (34.0 + 40.0 * t) as u8,
                (32.0 + 18.0 * t) as u8,
                (52.0 + 30.0 * t) as u8,
                255,
            ];
        }
        // The canvas holds premultiplied colour, so it goes over the desktop
        // as `source + destination × (1 − alpha)`.
        let mut top = border;
        for card in &cards {
            for row in 0..card.height {
                for col in 0..card.stride {
                    let source = (row * card.stride + col) * 4;
                    let target = ((top + row) * width + border + col) * 4;
                    let alpha = u32::from(card.pixels[source + 3]);
                    for channel in 0..3 {
                        let under = u32::from(pixels[target + channel]);
                        let over = u32::from(card.pixels[source + channel]);
                        pixels[target + channel] =
                            (over + under * (255 - alpha) / 255).min(255) as u8;
                    }
                }
            }
            top += card.height + SPACING as usize;
        }

        let path = std::path::Path::new(&dir).join("notification-cards.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
        eprintln!("wrote {}", path.display());
    }

    #[test]
    fn the_stack_starts_in_the_top_right_corner_of_the_usable_area() {
        let rects = stack(AREA, &[((380, 90), 1.0)]);
        assert_eq!(
            rects,
            [Rect::from_xywh(1920 - 380 - MARGIN, 40 + MARGIN, 380, 90)]
        );
    }

    #[test]
    fn cards_stack_downwards_with_spacing_between_them() {
        let rects = stack(
            AREA,
            &[((380, 90), 1.0), ((380, 120), 1.0), ((380, 60), 1.0)],
        );
        assert_eq!(rects[1].y(), rects[0].y() + 90 + SPACING);
        assert_eq!(rects[2].y(), rects[1].y() + 120 + SPACING);
        assert!(rects.iter().all(|r| r.x() == rects[0].x()));
    }

    #[test]
    fn a_card_on_its_way_slides_from_the_right_and_the_gap_it_leaves_closes_with_it() {
        let shown = stack(AREA, &[((380, 90), 1.0), ((380, 90), 1.0)]);
        let half = stack(AREA, &[((380, 90), 0.5), ((380, 90), 1.0)]);
        let gone = stack(AREA, &[((380, 90), 0.0), ((380, 90), 1.0)]);

        assert!(half[0].x() > shown[0].x(), "slides out to the right");
        assert_eq!(shown[1].y(), shown[0].y() + 90 + SPACING);
        assert_eq!(half[1].y(), shown[0].y() + (90 + SPACING) / 2);
        assert_eq!(
            gone[1].y(),
            shown[0].y(),
            "the card below has taken its place"
        );
    }

    #[test]
    fn the_pointer_is_on_a_control_only_inside_it() {
        let hits = Hits {
            close: Some(Rect::from_xywh(350, 8, 22, 22)),
            buttons: vec![
                Rect::from_xywh(68, 90, 100, 30),
                Rect::from_xywh(176, 90, 100, 30),
            ],
        };
        assert_eq!(hits.target(360, 18), Target::Close);
        assert_eq!(hits.target(68, 90), Target::Button(0));
        assert_eq!(hits.target(275, 119), Target::Button(1));
        assert_eq!(hits.target(170, 100), Target::Body, "between two controls");
        assert_eq!(hits.target(276, 100), Target::Body, "just past the edge");
        assert_eq!(hits.target(20, 20), Target::Body);
    }

    fn note(summary: &str, body: &str, urgency: u8) -> Notification {
        let request = Request {
            app_name: "Oracle".into(),
            summary: summary.into(),
            body: body.into(),
            urgency: Some(urgency),
            ..Request::default()
        };
        Notification::from_request(1, request, Duration::ZERO)
    }

    fn with_actions(pairs: &[&str]) -> Notification {
        let mut n = note("Answer ready", "", 1);
        n.actions = Action::from_pairs(&pairs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        n
    }

    /// A card composed without an icon theme, at 1080p and density 1.
    fn composed(n: &Notification, hover: Option<Target>) -> (Canvas, Hits) {
        let mut text = Text::new();
        let icons = Icons::with_bases("hicolor", Vec::new());
        let mut pixmaps = Pixmaps::new();
        compose(n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 1, hover)
    }

    #[test]
    fn a_card_is_as_tall_as_its_body_up_to_three_lines() {
        if !Text::new().is_usable() {
            eprintln!("skipped: no fonts on this machine");
            return;
        }
        let height = |n: &Notification| composed(n, None).0.height;

        let short = height(&note("Answer ready", "", 1));
        let one = height(&note("Answer ready", "The disk is full.", 1));
        let long = height(&note("Answer ready", &"word ".repeat(200), 1));
        let longer = height(&note("Answer ready", &"word ".repeat(400), 1));

        assert!(one > short, "a body adds to the card");
        assert!(long > one, "a longer body adds more");
        assert_eq!(long, longer, "past three lines the card stops growing");
    }

    #[test]
    fn actions_add_a_row_of_controls_but_the_default_action_is_the_card_itself() {
        let plain = composed(&note("Answer ready", "", 1), None);
        let only_default = composed(&with_actions(&["default", "Open"]), None);
        let two = composed(
            &with_actions(&["default", "Open", "a", "Reply", "b", "Mute"]),
            None,
        );
        let five = composed(
            &with_actions(&["a", "1", "b", "2", "c", "3", "d", "4", "e", "5"]),
            None,
        );

        assert_eq!(only_default.0.height, plain.0.height);
        assert!(only_default.1.buttons.is_empty());
        assert!(two.0.height > plain.0.height);
        assert_eq!(two.1.buttons.len(), 2);
        assert_eq!(five.1.buttons.len(), MAX_BUTTONS, "no more than three");

        let [first, second] = [two.1.buttons[0], two.1.buttons[1]];
        assert_eq!(first.y(), second.y(), "one row");
        assert!(second.x() > first.x() + first.w(), "with a gap between");
        assert!(second.x() + second.w() <= WIDTH as i32, "inside the card");
    }

    #[test]
    fn the_close_control_is_in_the_top_right_corner_and_drawn_only_on_hover() {
        let n = note("Answer ready", "The disk is full.", 1);
        let (idle, hits) = composed(&n, None);
        let (hovered, _) = composed(&n, Some(Target::Body));
        let close = hits.close.unwrap();
        assert!(close.x() > WIDTH as i32 / 2 && close.y() < 20);

        let centre = |canvas: &Canvas| {
            let (x, y) = (
                (close.x() + close.w() / 2) as usize,
                (close.y() + close.h() / 2) as usize,
            );
            canvas.pixels[(y * canvas.stride + x) * 4..][..4].to_vec()
        };
        assert_ne!(centre(&idle), centre(&hovered));
    }

    #[test]
    fn hit_areas_are_given_in_logical_pixels_at_any_density() {
        let n = with_actions(&["a", "Reply"]);
        let mut text = Text::new();
        let icons = Icons::with_bases("hicolor", Vec::new());
        let mut pixmaps = Pixmaps::new();
        let (_, one) = render(&n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 1, None);
        let (_, two) = render(&n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 2, None);
        assert_eq!(one, two);
    }

    #[test]
    fn a_card_is_drawn_at_its_width_and_the_output_density() {
        let mut text = Text::new();
        let icons = Icons::with_bases("hicolor", Vec::new());
        let mut pixmaps = Pixmaps::new();
        let n = note("Hi", "", 1);
        let (one, _) = compose(&n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 1, None);
        let (two, _) = compose(&n, &mut text, &icons, &mut pixmaps, &[], OUTPUT, 2, None);
        assert_eq!(one.stride, WIDTH as usize);
        assert_eq!(two.stride, WIDTH as usize * 2);
        assert_eq!(
            Panel::from_canvas(&two, 2).size().0,
            WIDTH as i32,
            "the same logical width at any density"
        );
    }

    #[test]
    fn a_critical_card_carries_the_error_colour_down_its_left_edge() {
        let edge_pixel = |canvas: &Canvas| {
            let (x, y) = ((PAD * 0.4) as usize + 1, canvas.height / 2);
            let offset = (y * canvas.stride + x) * 4;
            [
                canvas.pixels[offset],
                canvas.pixels[offset + 1],
                canvas.pixels[offset + 2],
            ]
        };
        let (critical, _) = composed(&note("Disk full", "", 2), None);
        let (normal, _) = composed(&note("Disk full", "", 1), None);
        let [r, g, b, _] = theme::CRITICAL.to_rgba_bytes();
        assert_eq!(edge_pixel(&critical), [r, g, b]);
        assert_ne!(edge_pixel(&normal), [r, g, b]);
    }
}
