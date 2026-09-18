//! The arc layout: seven applications on an arc of glass around the search.
//!
//! The arc opens at the bottom. Its hub is the search: a glass disc with the
//! field in it, lit along its rim. The highlighted application sits wherever
//! the arrows put it and the rim glows above it; the best match of a search
//! lands at the top ([`RANK_POS`]). The categories — or, while searching, the
//! kinds of result — are a glass sidebar to the left, and the highlighted
//! application's details and actions a glass card to the right.
//!
//! Every proportion below is at a 1080p output before [`ZOOM`], in logical
//! pixels, and was checked against the label geometry in
//! `labels_stay_on_the_glass`: the slots are spread, and the opening is as
//! wide, as they can be while every label lies on the glass.

use super::paint::*;
use super::*;
use crate::text::Weight;
use crate::theme::{Color, TEXT, TEXT_DIM};

/// The glass's outer radius.
const OUTER: f32 = 232.0;
/// The hub's radius.
const HUB: f32 = 113.0;
/// Where the slots' centres sit.
const ORBIT: f32 = 188.0;
/// A slot's radius.
const SLOT: f32 = 28.0;
/// An icon's size in its slot.
const ICON: f32 = 30.0;
/// A label's text size, and the widest a label may be.
const LABEL: f32 = 11.0;
const LABEL_W: f32 = 80.0;
/// How far a label is drawn towards the middle of the arc from under its
/// slot, which is what keeps the labels at either end on the glass.
const NUDGE: f32 = 8.0;
/// Each slot's angle, from the arc's left end: 0° to the right, clockwise.
pub(super) const ANGLES: [f32; ARC_SLOTS] = [182.0, 211.0, 241.0, 270.0, 299.0, 329.0, 358.0];
/// The opening at the bottom, between these angles.
const OPENING: (f32, f32) = (22.0, 158.0);
/// The sidebar and the card, and how far each stands off the arc.
const SIDEBAR_W: f32 = 230.0;
const SIDEBAR_GAP: f32 = 64.0;
const CARD_W: f32 = 260.0;
const CARD_GAP: f32 = 70.0;
/// How much larger than the list the arc is drawn. It is a centrepiece
/// rather than a panel, and at the list's scale seven slots and their labels
/// are too small to read at a glance.
const ZOOM: f32 = 1.4;
/// The canvas: the arc centred, the card's far edge setting the width on
/// both sides so the arc stays centred on the screen.
const CANVAS_W: f32 = 2.0 * (OUTER + CARD_GAP + CARD_W) + 8.0;
const CANVAS_H: f32 = 540.0;

const SIDE_GROUND: Color = Color::from_argb(0xA30E_111E);
const CARD_GROUND: Color = Color::from_argb(0xBD16_1826);

/// Lay out and paint the arc. See [`super::compose`].
pub(super) fn compose(
    launcher: &Launcher,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Canvas, Layout) {
    let m = Metrics::for_output(output, density);
    let d = m.density as f32;
    // As large as the zoom asks, but never wider or taller than the screen.
    let s = (m.scale * ZOOM)
        .min((output.w() as f32 * d - 32.0 * d) / CANVAS_W)
        .min(output.h() as f32 * d * 0.94 / CANVAS_H)
        .max(0.5);
    let px = |v: f32| v * s;
    let (w, h) = (px(CANVAS_W).ceil() as usize, px(CANVAS_H).ceil() as usize);
    let mut canvas = Canvas::new(w, h);
    let mut layout = Layout {
        size: (w as i32, h as i32),
        ..Layout::default()
    };
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    let (cx, cy) = (w as f32 / 2.0, px(OUTER + 18.0));

    let visible = launcher.visible();
    let n = visible.len();
    let selected = launcher.selected().min(n.saturating_sub(1));
    let page = selected / ARC_SLOTS;
    let on_page = n.saturating_sub(page * ARC_SLOTS).min(ARC_SLOTS);
    let highlight = (n > 0).then(|| ANGLES[RANK_POS[selected - page * ARC_SLOTS]]);
    let typing = !launcher.query().is_empty();

    draw_band(&mut canvas, (cx, cy), px(OUTER), s, highlight, accent);
    draw_hub(&mut canvas, (cx, cy), px(HUB), s, accent);
    if let Some(at) = highlight {
        let (mx, my) = polar((cx, cy), px(OUTER) - 1.5 * s, at);
        draw_marker(&mut canvas, mx, my, s, accent);
    }

    // The slots.
    for (position, angle) in ANGLES.iter().enumerate() {
        let Some(rank) = rank_at(position).filter(|rank| *rank < on_page) else {
            continue;
        };
        let index = page * ARC_SLOTS + rank;
        let (sx, sy) = polar((cx, cy), px(ORBIT), *angle);
        let chosen = index == selected;
        draw_slot(&mut canvas, (sx, sy), px(SLOT), s, chosen, accent);
        let (icon, label) = match visible[index] {
            Target::App(i) => match apps.get(i) {
                Some(entry) => (
                    app_icon(icons, pixmaps, entry, px(ICON) as u32, density),
                    entry.name.clone(),
                ),
                None => (None, String::new()),
            },
            Target::File(i) => (
                file_icon(icons, pixmaps, px(ICON) as u32, density),
                launcher
                    .files()
                    .get(i)
                    .map(|f| f.name.clone())
                    .unwrap_or_default(),
            ),
            Target::Command => (None, "Run".to_owned()),
            Target::Result => (None, String::new()),
        };
        if let Some(icon) = &icon {
            blit_centred(&mut canvas, icon, sx, sy);
        } else if visible[index] == Target::Command {
            let size = px(18.0);
            draw_centred(
                text,
                &mut canvas,
                COMMAND_GLYPH,
                size,
                sx,
                sy - size * 0.7,
                accent,
                EMPHASIS,
            );
        }
        let label_size = px(LABEL);
        let label = fit(text, &label, label_size, px(LABEL_W));
        let nudge = -angle.to_radians().cos() * px(NUDGE);
        draw_centred(
            text,
            &mut canvas,
            &label,
            label_size,
            sx + nudge,
            sy + px(SLOT) + px(2.0),
            if chosen { WHITE } else { TEXT },
            Weight::NORMAL,
        );
        layout.hits.push((
            rect(
                sx - px(SLOT),
                sy - px(SLOT),
                px(SLOT * 2.0),
                px(SLOT * 2.0) + label_size * 1.35 + px(2.0),
            ),
            index,
        ));
    }

    draw_search(&mut canvas, text, launcher, (cx, cy), s, typing, accent);

    let side = draw_sidebar(
        &mut canvas,
        text,
        icons,
        pixmaps,
        &mut layout,
        launcher,
        (cx - px(OUTER + SIDEBAR_GAP + SIDEBAR_W), cy),
        s,
        density,
        typing,
    );
    layout.surfaces.push(side);

    if let Some(target) = visible.get(selected) {
        let card = draw_card(
            &mut canvas,
            text,
            icons,
            pixmaps,
            &mut layout,
            launcher,
            apps,
            *target,
            (cx + px(OUTER + CARD_GAP), cy),
            s,
            density,
        );
        layout.surfaces.push(card);
    }

    let mut hints: Vec<(&str, &str)> = if launcher.menu().is_some() {
        MENU_HINTS.to_vec()
    } else {
        vec![("←→", "Navigate"), ("Enter", "Launch"), ("Tab", "Actions")]
    };
    if launcher.menu().is_none() {
        if typing {
            hints.push(("Ctrl ←→", "Filter"));
        }
        if n > ARC_SLOTS {
            hints.push(("PgDn", "More"));
        }
        hints.push(("Esc", "Close"));
    }
    let keys = draw_hints(
        &mut canvas,
        text,
        &hints,
        Align::Centre(cx),
        cy + px(OUTER * 0.86 + 30.0),
        px(11.5),
        s,
        rgba(14, 16, 26, 0.55),
        Some(rgba(255, 255, 255, 0.24)),
    );
    layout.surfaces.push(keys);
    layout.surfaces.push(rect(
        cx - px(OUTER),
        cy - px(OUTER),
        px(OUTER * 2.0),
        px(OUTER * 2.0),
    ));
    // The blur: the widest rectangle above the opening that lies on the
    // glass or the hub everywhere. Its lower edge is where the hub's rim
    // meets the opening, below which the corners would be bare desktop.
    let below = px(HUB) * OPENING.0.to_radians().sin();
    layout.blur = Some(rect(
        cx - px(OUTER) * 0.7,
        cy - px(OUTER) * 0.7,
        px(OUTER) * 1.4,
        px(OUTER) * 0.7 + below,
    ));

    (canvas, layout)
}

/// The point `radius` from `centre` at `angle` degrees.
fn polar(centre: (f32, f32), radius: f32, angle: f32) -> (f32, f32) {
    let theta = angle.to_radians();
    (
        centre.0 + theta.cos() * radius,
        centre.1 + theta.sin() * radius,
    )
}

/// How much of the pixel at (`dx`, `dy`) from the centre, `distance` out,
/// is glass rather than the opening: 1 on the glass, 0 in the opening, with
/// a pixel's ramp across its straight edges.
fn off_opening(dx: f32, dy: f32, distance: f32) -> f32 {
    let angle = angle_of(dx, dy);
    let (from, to) = OPENING;
    let inside = angle > from && angle < to;
    let degrees = if inside {
        (angle - from).min(to - angle)
    } else {
        angular_gap(angle, from).min(angular_gap(angle, to))
    };
    let across = degrees.to_radians() * distance;
    edge(if inside { across } else { -across })
}

/// The band of glass: a tinted, frosted disc open at the bottom, a mist
/// gathering towards its foot, a hairline rim, and the rim lit around the
/// highlighted slot.
fn draw_band(
    canvas: &mut Canvas,
    (cx, cy): (f32, f32),
    radius: f32,
    s: f32,
    highlight: Option<f32>,
    accent: Color,
) {
    let top = cy - radius;
    let span = radius * 2.0;
    let ground = rgba(14, 17, 30, 0.64);
    let reach = radius + 2.0;
    canvas.paint(
        (cx - reach) as i32,
        (cy - reach) as i32,
        (reach * 2.0) as i32 + 1,
        (reach * 2.0) as i32 + 1,
        |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let distance = dx.hypot(dy);
            let coverage = edge(distance - radius) * off_opening(dx, dy, distance);
            if coverage <= 0.0 {
                return None;
            }
            let t = (y - top) / span;
            let mut color = if t < 0.38 {
                over(ground, rgba(255, 255, 255, 0.07 * (1.0 - t / 0.38)))
            } else {
                over(ground, rgba(150, 172, 222, 0.16 * (t - 0.38) / 0.62))
            };
            let rim = (1.0 - (distance - (radius - 0.5)).abs()).clamp(0.0, 1.0);
            if rim > 0.0 {
                color = over(color, rgba(255, 255, 255, 0.26 * rim));
            }
            if let Some(at) = highlight {
                let width = 2.5 * s;
                let ring = edge((distance - (radius - width / 2.0)).abs() - width / 2.0);
                let strength = (1.0 - angular_gap(angle_of(dx, dy), at) / 70.0).max(0.0);
                if ring > 0.0 && strength > 0.0 {
                    color = over(color, faded(accent, ring * strength));
                }
            }
            Some((color, coverage))
        },
    );
}

/// How much of the hub is left at `t` of the way down it: whole to past the
/// middle, then thinning into the opening, so its foot dissolves into the
/// mist rather than being cut off.
fn hub_fade(t: f32) -> f32 {
    if t < 0.56 {
        1.0
    } else if t < 0.8 {
        1.0 - 0.4 * (t - 0.56) / 0.24
    } else {
        (0.6 * (1.0 - (t - 0.8) / 0.2)).max(0.0)
    }
}

/// The hub: a shaded glass disc with fog rising through it from below, an
/// accent glow around and just inside its edge, and a rim of light that is
/// brightest at the top and stops, in two bright points, where the glass
/// opens.
fn draw_hub(canvas: &mut Canvas, (cx, cy): (f32, f32), radius: f32, s: f32, accent: Color) {
    let glow = 24.0 * s;
    let reach = radius + glow;
    let top = cy - radius;
    let (inner_glow, rim_w) = (28.0 * s, 2.5 * s);
    canvas.paint(
        (cx - reach) as i32,
        (cy - reach) as i32,
        (reach * 2.0) as i32 + 1,
        (reach * 2.0) as i32 + 1,
        |x, y| {
            let (dx, dy) = (x - cx, y - cy);
            let distance = dx.hypot(dy);
            let fade = hub_fade((y - top) / (radius * 2.0));
            if fade <= 0.0 {
                return None;
            }
            if distance > radius + 0.5 {
                let t = ((distance - radius) / glow).min(1.0);
                let a = 0.3 * (1.0 - t).powi(2) * fade;
                return (a > 0.004).then(|| (faded(accent, a), 1.0));
            }
            // A darker ground lit from above centre.
            let lit = dx.hypot(y - (top + radius * 0.64)) / (radius * 1.44);
            let mut color = mix(rgba(24, 32, 56, 0.95), rgba(8, 10, 20, 0.97), lit);
            // Fog: an ellipse centred below the hub, wider than tall.
            let fog_y = top + radius * 2.24;
            let e = ((dx / (radius * 2.6)).powi(2) + ((y - fog_y) / (radius * 1.5)).powi(2)).sqrt();
            let fog = if e < 0.45 {
                mix(
                    rgba(160, 182, 230, 0.38),
                    rgba(115, 140, 195, 0.14),
                    e / 0.45,
                )
            } else if e < 0.72 {
                rgba(115, 140, 195, 0.14 * (1.0 - (e - 0.45) / 0.27))
            } else {
                rgba(0, 0, 0, 0.0)
            };
            color = over(color, fog);
            let inner = ((distance - (radius - inner_glow)) / inner_glow).clamp(0.0, 1.0);
            color = over(color, faded(accent, 0.1 * inner));
            let ring = edge((distance - (radius - rim_w / 2.0)).abs() - rim_w / 2.0)
                * off_opening(dx, dy, distance);
            if ring > 0.0 {
                let brightness = 0.3 + 0.7 * (1.0 - angular_gap(angle_of(dx, dy), 270.0) / 180.0);
                color = over(color, faded(accent, ring * brightness));
            }
            Some((color, edge(distance - radius) * fade))
        },
    );
    for at in [OPENING.0 + 1.5, OPENING.1 - 1.5] {
        let (x, y) = polar((cx, cy), radius - rim_w / 2.0, at);
        glow_dot(
            canvas,
            x,
            y,
            3.0 * s,
            11.0 * s,
            rgba(230, 251, 255, 1.0),
            faded(accent, 0.85),
        );
    }
}

/// The small diamond of light on the rim above the highlighted slot.
fn draw_marker(canvas: &mut Canvas, x: f32, y: f32, s: f32, accent: Color) {
    let half = 5.0 * s;
    let glow = 16.0 * s;
    canvas.paint(
        (x - glow) as i32,
        (y - glow) as i32,
        (glow * 2.0) as i32 + 1,
        (glow * 2.0) as i32 + 1,
        |px, py| {
            let (u, v) = ((px - x).abs(), (py - y).abs());
            let halo = (1.0 - (px - x).hypot(py - y) / glow).max(0.0).powi(2);
            let mut color = faded(accent, 0.8 * halo);
            let body = edge((u + v - half) / std::f32::consts::SQRT_2);
            if body > 0.0 {
                color = over(color, rgba(240, 253, 255, body));
            }
            let [.., a] = color.to_rgba_bytes();
            (a > 0).then_some((color, 1.0))
        },
    );
}

/// A slot: a dark glass disc under the icon with a soft shadow, or, when
/// highlighted, ringed in the accent and glowing.
fn draw_slot(
    canvas: &mut Canvas,
    (x, y): (f32, f32),
    radius: f32,
    s: f32,
    chosen: bool,
    accent: Color,
) {
    let glow = 24.0 * s;
    let reach = radius + glow;
    let border = if chosen { 1.5 * s } else { 1.0 };
    canvas.paint(
        (x - reach) as i32,
        (y - reach) as i32,
        (reach * 2.0) as i32 + 1,
        (reach * 2.0) as i32 + 1,
        |px, py| {
            let (dx, dy) = (px - x, py - y);
            let distance = dx.hypot(dy);
            let shade = dx.hypot(dy - 6.0 * s);
            let shadow = (1.0 - ((shade - radius) / (18.0 * s)).clamp(0.0, 1.0)).powi(2) * 0.3;
            let mut color = rgba(0, 0, 0, shadow);
            if chosen && distance > radius {
                let t = ((distance - radius) / glow).min(1.0);
                color = over(color, faded(accent, 0.45 * (1.0 - t).powi(2)));
                let halo = edge(distance - (radius + 4.0 * s));
                color = over(color, faded(accent, 0.1 * halo));
            }
            let body = edge(distance - radius);
            if body > 0.0 {
                let lit = dx.hypot(dy + radius * 0.4) / (radius * 1.4);
                let mut disc = mix(rgba(40, 46, 70, 0.78), rgba(12, 14, 26, 0.88), lit);
                let ring = edge((distance - (radius - border / 2.0)).abs() - border / 2.0);
                disc = over(
                    disc,
                    if chosen {
                        faded(accent, 0.95 * ring)
                    } else {
                        rgba(255, 255, 255, 0.15 * ring)
                    },
                );
                if chosen {
                    let inner = ((distance - (radius - 16.0 * s)) / (16.0 * s)).clamp(0.0, 1.0);
                    disc = over(disc, faded(accent, 0.2 * inner));
                }
                color = over(color, faded(disc, body));
            }
            let [.., a] = color.to_rgba_bytes();
            (a > 0).then_some((color, 1.0))
        },
    );
}

/// The hub's contents: the magnifier, the query — or "Search" when nothing is
/// typed — and a line under it saying what to do or what was found.
fn draw_search(
    canvas: &mut Canvas,
    text: &mut Text,
    launcher: &Launcher,
    (cx, cy): (f32, f32),
    s: f32,
    typing: bool,
    accent: Color,
) {
    let px = |v: f32| v * s;
    let (input_size, hint_size) = (px(19.0), px(12.0));
    let stack =
        px(22.0) + px(6.0) + input_size * 1.35 + px(6.0) + hint_size * 1.35 + px(14.0) + 1.0;
    let mut y = cy - stack / 2.0 - px(15.0);
    draw_search_glyph(canvas, cx - px(1.5), y + px(9.0), px(7.5), px(1.6), TEXT);
    y += px(22.0) + px(6.0);
    if typing {
        let shown = fit_tail(text, launcher.query(), input_size, px(170.0));
        draw_centred(text, canvas, &shown, input_size, cx, y, WHITE, EMPHASIS);
        let width = text.measure_weighted(&shown, input_size, EMPHASIS).0;
        canvas.fill(
            (cx + width / 2.0 + px(2.0)) as usize,
            (y + px(2.0)) as usize,
            px(2.0).max(1.0) as usize,
            (input_size * 1.2) as usize,
            accent.to_rgba_bytes(),
        );
    } else {
        draw_centred(text, canvas, "Search", input_size, cx, y, WHITE, EMPHASIS);
    }
    y += input_size * 1.35 + px(6.0);
    let found = launcher.results().len() + launcher.file_hits().len();
    let hint = if !typing {
        "Type to launch".to_owned()
    } else if let Some(value) = launcher.result() {
        format!("= {value}")
    } else if found == 0 {
        "No matches".to_owned()
    } else {
        format!("{found} result{}", if found == 1 { "" } else { "s" })
    };
    draw_centred(
        text,
        canvas,
        &hint,
        hint_size,
        cx,
        y,
        TEXT_DIM,
        Weight::NORMAL,
    );
    y += hint_size * 1.35 + px(14.0);
    canvas.fill_rounded(
        (cx - px(11.0)) as usize,
        y as usize,
        px(22.0) as usize,
        1,
        0.0,
        rgba(255, 255, 255, 0.55),
    );
}

/// The theme symbols tried for a category's row, in order.
fn category_symbols(category: Category) -> &'static [&'static str] {
    match category {
        Category::All => &[
            "view-app-grid-symbolic",
            "view-grid-symbolic",
            "applications-all",
        ],
        Category::Development => &[
            "applications-development-symbolic",
            "applications-development",
        ],
        Category::Media => &[
            "applications-multimedia-symbolic",
            "applications-multimedia",
        ],
        Category::Internet => &["applications-internet-symbolic", "applications-internet"],
        Category::Utilities => &["applications-utilities-symbolic", "applications-utilities"],
        Category::System => &["applications-system-symbolic", "applications-system"],
    }
}

fn filter_symbols(filter: Filter) -> &'static [&'static str] {
    match filter {
        Filter::All => category_symbols(Category::All),
        Filter::Apps => &["application-x-executable-symbolic", "applications-other"],
        Filter::Files => &["folder-symbolic", "folder"],
    }
}

/// The sidebar: a glass panel of categories, or of result kinds with their
/// counts while searching, the chosen one a lit pill. Its right edge is at
/// `right`, centred on `cy`. Returns the panel's rectangle.
#[allow(clippy::too_many_arguments)]
fn draw_sidebar(
    canvas: &mut Canvas,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    layout: &mut Layout,
    launcher: &Launcher,
    (left, cy): (f32, f32),
    s: f32,
    density: u32,
    typing: bool,
) -> Rect {
    let px = |v: f32| v * s;
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    let (found_apps, found_files) = launcher.found();
    #[allow(clippy::type_complexity)]
    let rows: Vec<(&str, Option<usize>, Button, bool, &[&str])> = if typing {
        Filter::ALL
            .into_iter()
            .map(|filter| {
                let count = match filter {
                    Filter::All => found_apps + found_files,
                    Filter::Apps => found_apps,
                    Filter::Files => found_files,
                };
                (
                    filter.label(),
                    Some(count),
                    Button::Filter(filter),
                    launcher.filter() == filter,
                    filter_symbols(filter),
                )
            })
            .collect()
    } else {
        launcher
            .categories()
            .iter()
            .enumerate()
            .map(|(i, category)| {
                (
                    category.label(),
                    None,
                    Button::Category(i),
                    launcher.category() == i,
                    category_symbols(*category),
                )
            })
            .collect()
    };
    let (pad, row_h, row_gap) = (px(12.0), px(40.0), px(4.0));
    let (width, height) = (
        px(SIDEBAR_W),
        pad * 2.0 + rows.len() as f32 * row_h + rows.len().saturating_sub(1) as f32 * row_gap,
    );
    let top = cy - height / 2.0;
    draw_glass(
        canvas,
        left,
        top,
        width,
        height,
        px(22.0),
        SIDE_GROUND,
        rgba(255, 255, 255, 0.16),
    );
    let (row_x, row_w) = (left + pad, width - pad * 2.0);
    let label_size = px(13.5);
    let mut y = top + pad;
    for (label, count, button, on, symbols) in rows {
        if on {
            let (xu, yu, wu, hu) = (row_x as usize, y as usize, row_w as usize, row_h as usize);
            canvas.fill_rounded(xu, yu, wu, hu, row_h / 2.0, faded(accent, 0.2));
            canvas.stroke_rounded(xu, yu, wu, hu, row_h / 2.0, 1.0, rgba(255, 255, 255, 0.24));
            glow_dot(
                canvas,
                row_x + px(2.0),
                y + row_h / 2.0,
                0.0,
                px(14.0),
                accent,
                faded(accent, 0.55),
            );
        }
        let glyph = px(17.0);
        let icon_color = if on { accent } else { TEXT_DIM };
        if let Some(icon) = symbol(icons, pixmaps, symbols, glyph as u32, density, icon_color) {
            blit_centred(
                canvas,
                &icon,
                row_x + px(16.0) + glyph / 2.0,
                y + row_h / 2.0,
            );
        }
        let label_x = row_x + px(16.0) + glyph + px(14.0);
        text.draw(
            canvas,
            label,
            label_size,
            label_x as i32,
            (y + (row_h - label_size * 1.35) / 2.0) as i32,
            if on { WHITE } else { TEXT },
        );
        if let Some(count) = count {
            let count = count.to_string();
            let count_size = px(11.0);
            let count_w = text.measure(&count, count_size).0;
            text.draw(
                canvas,
                &count,
                count_size,
                (row_x + row_w - px(16.0) - count_w) as i32,
                (y + (row_h - count_size * 1.35) / 2.0) as i32,
                TEXT_DIM,
            );
        }
        layout.buttons.push((rect(row_x, y, row_w, row_h), button));
        y += row_h + row_gap;
    }
    rect(left, top, width, height)
}

/// Whether `entry` is one of the desktop's own applications, by its desktop
/// file's name.
fn is_ravens_own(entry: &Entry) -> bool {
    entry
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            let stem = stem.to_ascii_lowercase();
            stem.starts_with("com.raven") || stem.starts_with("raven")
        })
}

/// The card: what the highlight is on, what it is and a line about it, a few
/// facts, the primary action as a button and the others as rows. Its left
/// edge at `left`, centred on `cy`. Returns its rectangle.
#[allow(clippy::too_many_arguments)]
fn draw_card(
    canvas: &mut Canvas,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    layout: &mut Layout,
    launcher: &Launcher,
    apps: &[Entry],
    target: Target,
    (left, cy): (f32, f32),
    s: f32,
    density: u32,
) -> Rect {
    let px = |v: f32| v * s;
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let accent = crate::theme::accent();
    let (pad_x, pad_y) = (px(20.0), px(18.0));
    let width = px(CARD_W);
    let inner = width - pad_x * 2.0;
    let icon_size = px(42.0);

    // What to show, by what the highlight is on.
    let clock: &[&str] = &["document-open-recent-symbolic", "appointment-soon-symbolic"];
    let mut icon = None;
    let mut facts: Vec<(&[&str], String)> = Vec::new();
    let (title, kind, about, primary, actions): (String, String, String, &str, Vec<&str>) =
        match target {
            Target::App(i) => {
                let Some(entry) = apps.get(i) else {
                    return Rect::ZERO;
                };
                icon = app_icon(icons, pixmaps, entry, icon_size as u32, density);
                if let Some(category) = Category::ALL[1..].iter().find(|c| c.contains(entry)) {
                    facts.push((category_symbols(*category), category.label().to_owned()));
                }
                facts.push((
                    clock,
                    match launcher.last_used(i) {
                        Some(at) => {
                            let when = ago(launcher.now().saturating_sub(at));
                            format!("Opened {}", when.to_lowercase())
                        }
                        None => "Not opened recently".to_owned(),
                    },
                ));
                if is_ravens_own(entry) {
                    facts.push((
                        &["start-here-symbolic", "emblem-system-symbolic"],
                        "Built for Raven".to_owned(),
                    ));
                }
                if launcher.is_pinned(entry) {
                    facts.push((&["view-pin-symbolic", "pin-symbolic"], "Pinned".to_owned()));
                }
                let items = launcher.menu_items(entry);
                (
                    entry.name.clone(),
                    kind_of(entry)
                        .filter(|kind| Some(*kind) != entry.comment.as_deref())
                        .unwrap_or("Application")
                        .to_owned(),
                    entry
                        .comment
                        .clone()
                        .or_else(|| entry.generic_name.clone())
                        .unwrap_or_default(),
                    "Open",
                    items[1..].to_vec(),
                )
            }
            Target::File(i) => {
                let Some(file) = launcher.files().get(i) else {
                    return Rect::ZERO;
                };
                icon = file_icon(icons, pixmaps, icon_size as u32, density);
                facts.push((
                    &["document-open-symbolic"],
                    "Opens in its usual application".to_owned(),
                ));
                (
                    file.name.clone(),
                    "File".to_owned(),
                    launcher.files().location(i),
                    "Open",
                    Vec::new(),
                )
            }
            Target::Command => {
                facts.push((
                    &["utilities-terminal-symbolic"],
                    "Runs in a shell".to_owned(),
                ));
                (
                    "Run".to_owned(),
                    "Command".to_owned(),
                    launcher.query().to_owned(),
                    "Run",
                    Vec::new(),
                )
            }
            Target::Result => return Rect::ZERO,
        };

    let (title_size, kind_size, about_size, fact_size) = (px(17.0), px(11.5), px(12.5), px(12.0));
    let about = wrap(text, &about, about_size, inner, 3);
    let (fact_h, primary_h, action_h) = (px(26.0), px(38.0), px(34.0));
    let menu = launcher.menu();
    let height = pad_y
        + icon_size
        + px(16.0)
        + 1.0
        + px(14.0)
        + about.len() as f32 * about_size * 1.5
        + if about.is_empty() { 0.0 } else { px(10.0) }
        + facts.len() as f32 * fact_h
        + px(14.0)
        + primary_h
        + px(8.0)
        + actions.len() as f32 * action_h
        + if menu.is_some() { px(24.0) } else { 0.0 }
        + pad_y;
    let top = cy - height / 2.0;
    draw_glass(
        canvas,
        left,
        top,
        width,
        height,
        px(16.0),
        CARD_GROUND,
        rgba(255, 255, 255, 0.12),
    );

    let x = left + pad_x;
    let mut y = top + pad_y;
    if let Some(icon) = &icon {
        blit_centred(canvas, icon, x + icon_size / 2.0, y + icon_size / 2.0);
    }
    let text_x = x + icon_size + px(14.0);
    let heading_room = left + width - pad_x - text_x;
    let title = fit(text, &title, title_size, heading_room);
    let block = title_size * 1.3 + kind_size * 1.35;
    let heading_y = y + (icon_size - block) / 2.0;
    text.draw_weighted(
        canvas,
        &title,
        title_size,
        text_x as i32,
        heading_y as i32,
        WHITE,
        EMPHASIS,
    );
    let kind = fit(text, &kind, kind_size, heading_room);
    text.draw(
        canvas,
        &kind,
        kind_size,
        text_x as i32,
        (heading_y + title_size * 1.3) as i32,
        TEXT_DIM,
    );
    y += icon_size + px(16.0);
    canvas.fill_rounded(
        x as usize,
        y as usize,
        px(26.0) as usize,
        1,
        0.0,
        rgba(255, 255, 255, 0.25),
    );
    y += 1.0 + px(14.0);
    for line in &about {
        text.draw(canvas, line, about_size, x as i32, y as i32, TEXT);
        y += about_size * 1.5;
    }
    if !about.is_empty() {
        y += px(10.0);
    }
    for (symbols, fact) in &facts {
        let glyph = px(16.0);
        match symbol(icons, pixmaps, symbols, glyph as u32, density, accent) {
            Some(icon) => blit_centred(canvas, &icon, x + glyph / 2.0, y + fact_h / 2.0),
            None => glow_dot(
                canvas,
                x + glyph / 2.0,
                y + fact_h / 2.0,
                px(2.5),
                px(3.5),
                accent,
                faded(accent, 0.4),
            ),
        }
        let fact = fit(text, fact, fact_size, inner - glyph - px(12.0));
        text.draw(
            canvas,
            &fact,
            fact_size,
            (x + glyph + px(12.0)) as i32,
            (y + (fact_h - fact_size * 1.35) / 2.0) as i32,
            TEXT_DIM,
        );
        y += fact_h;
    }
    y += px(14.0);

    // The primary action: the menu's first item, as a button.
    let (xu, yu) = (x as usize, y as usize);
    canvas.fill_rounded(xu, yu, inner as usize, primary_h as usize, px(9.0), accent);
    if menu == Some(0) {
        canvas.stroke_rounded(
            xu,
            yu,
            inner as usize,
            primary_h as usize,
            px(9.0),
            px(2.0),
            rgba(255, 255, 255, 0.85),
        );
    }
    let primary_size = px(13.0);
    draw_centred(
        text,
        canvas,
        primary,
        primary_size,
        x + inner / 2.0,
        y + (primary_h - primary_size * 1.35) / 2.0,
        Color::from_argb(0xFF05_262C),
        EMPHASIS,
    );
    layout.menu_hits.push((rect(x, y, inner, primary_h), 0));
    y += primary_h + px(8.0);

    // The rest of the menu — the entry's own actions, then Pin or Unpin.
    for (n, label) in actions.iter().enumerate() {
        let item = n + 1;
        if menu == Some(item) {
            let (xu, yu) = ((x - px(8.0)) as usize, y as usize);
            let w = (inner + px(16.0)) as usize;
            canvas.fill_rounded(xu, yu, w, action_h as usize, px(9.0), faded(accent, 0.18));
            canvas.stroke_rounded(
                xu,
                yu,
                w,
                action_h as usize,
                px(9.0),
                1.0,
                faded(accent, 0.5),
            );
        }
        let pin = *label == PIN || *label == UNPIN;
        let symbols: &[&str] = if pin {
            &["view-pin-symbolic", "pin-symbolic"]
        } else {
            &["window-new-symbolic", "list-add-symbolic"]
        };
        let glyph = px(16.0);
        if let Some(icon) = symbol(icons, pixmaps, symbols, glyph as u32, density, TEXT) {
            blit_centred(canvas, &icon, x + glyph / 2.0, y + action_h / 2.0);
        }
        let size = px(12.5);
        let label = fit(text, label, size, inner - glyph - px(14.0));
        text.draw(
            canvas,
            &label,
            size,
            (x + glyph + px(14.0)) as i32,
            (y + (action_h - size * 1.35) / 2.0) as i32,
            TEXT,
        );
        layout
            .menu_hits
            .push((rect(x - px(8.0), y, inner + px(16.0), action_h), item));
        y += action_h;
    }
    if menu.is_some() {
        text.draw(
            canvas,
            "↑↓ choose · Enter run · Esc back",
            px(11.0),
            x as i32,
            (y + px(8.0)) as i32,
            TEXT_DIM,
        );
    }
    rect(left, top, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The geometry the proportions above were chosen against: every
    /// label's box, drawn widest, lies on the glass — inside the outer edge,
    /// outside the hub, and clear of the opening.
    #[test]
    fn labels_stay_on_the_glass() {
        let label_h = 16.0;
        for angle in ANGLES {
            let (sx, sy) = polar((0.0, 0.0), ORBIT, angle);
            let lx = sx - angle.to_radians().cos() * NUDGE;
            let ly = sy + SLOT + 3.0 + label_h / 2.0;
            let corners = [
                (lx - LABEL_W / 2.0, ly - label_h / 2.0),
                (lx + LABEL_W / 2.0, ly - label_h / 2.0),
                (lx - LABEL_W / 2.0, ly + label_h / 2.0),
                (lx + LABEL_W / 2.0, ly + label_h / 2.0),
            ];
            for (x, y) in corners {
                let r = x.hypot(y);
                assert!(r < OUTER - 4.0, "the label at {angle}° runs past the edge");
                assert!(r > HUB + 2.0, "the label at {angle}° runs onto the hub");
                let a = angle_of(x, y);
                assert!(
                    !(a > OPENING.0 - 2.0 && a < OPENING.1 + 2.0),
                    "the label at {angle}° runs into the opening"
                );
            }
        }
    }

    #[test]
    fn the_best_match_is_at_the_top_and_the_ranks_fan_out() {
        assert_eq!(ANGLES[RANK_POS[0]], 270.0, "rank 0 sits straight up");
        let mut positions = RANK_POS.to_vec();
        positions.sort_unstable();
        assert_eq!(
            positions,
            (0..ARC_SLOTS).collect::<Vec<_>>(),
            "each slot once"
        );
    }
}
