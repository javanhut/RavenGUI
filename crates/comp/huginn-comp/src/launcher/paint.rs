//! Drawing the list and the arc share: colour arithmetic, icons, the small
//! vector glyphs, key hints, wrapped text and glass.
//!
//! Everything here is in canvas pixels and paints through [`Canvas`], so the
//! blending rules stay the canvas's own.

use raven_desktop::Pixmap;

use super::*;
use crate::text::Weight;
use crate::theme::Color;

/// The weight headings and names are set in: size and colour carry the
/// emphasis, not the weight.
///
/// A heavier weight is only as good as the faces installed, and a desktop
/// with Noto Sans in Regular and Black alone — a common install — resolves
/// "semibold" to a face from another family whose spaces and metrics do not
/// match, which is how "Raven Store" came out with a word-wide gap in it.
pub(super) const EMPHASIS: Weight = Weight::NORMAL;

/// Opaque white, for the label under the highlight and the arc's hub text.
pub(super) const WHITE: Color = Color::from_argb(0xFFFF_FFFF);

/// A colour from straight RGB and an opacity in 0..=1.
pub(super) fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    let a = (a.clamp(0.0, 1.0) * 255.0).round() as u32;
    Color::from_argb(a << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))
}

/// `color` at `amount` of its own opacity.
pub(super) fn faded(color: Color, amount: f32) -> Color {
    let [r, g, b, a] = color.to_rgba_bytes();
    rgba(r, g, b, f32::from(a) / 255.0 * amount)
}

/// `top` laid over `under`, both straight alpha: what the pixel would be if
/// the two were painted one after the other. Lets a shader build one pixel
/// from several layers — ground, fog, rim, glow — and blend the result once.
pub(super) fn over(under: Color, top: Color) -> Color {
    let [ur, ug, ub, ua] = under.to_rgba_bytes().map(|c| f32::from(c) / 255.0);
    let [tr, tg, tb, ta] = top.to_rgba_bytes().map(|c| f32::from(c) / 255.0);
    let a = ta + ua * (1.0 - ta);
    if a <= 0.0 {
        return rgba(0, 0, 0, 0.0);
    }
    let channel = |t: f32, u: f32| ((t * ta + u * ua * (1.0 - ta)) / a * 255.0).round() as u8;
    rgba(channel(tr, ur), channel(tg, ug), channel(tb, ub), a)
}

/// `t` of the way from `a` to `b`, every channel including alpha.
pub(super) fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_rgba_bytes().map(f32::from);
    let [br, bg, bb, ba] = b.to_rgba_bytes().map(f32::from);
    let lerp = |x: f32, y: f32| (x + (y - x) * t).round() as u8;
    rgba(
        lerp(ar, br),
        lerp(ag, bg),
        lerp(ab, bb),
        f32::from(lerp(aa, ba)) / 255.0,
    )
}

/// Coverage of a pixel whose centre is `distance` past a shape's edge —
/// negative inside — with a one-pixel ramp across the edge.
pub(super) fn edge(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// The angle of (`dx`, `dy`) in degrees, 0 to the right and clockwise, the
/// way the arc's slots are placed on a screen whose y grows downwards.
pub(super) fn angle_of(dx: f32, dy: f32) -> f32 {
    dy.atan2(dx).to_degrees().rem_euclid(360.0)
}

/// How far apart two angles are, in degrees, the short way round.
pub(super) fn angular_gap(a: f32, b: f32) -> f32 {
    ((a - b + 540.0).rem_euclid(360.0) - 180.0).abs()
}

/// Distance from (`px`, `py`) to the segment from `a` to `b`.
pub(super) fn segment_distance(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let length = abx * abx + aby * aby;
    let t = if length > 0.0 {
        (((px - a.0) * abx + (py - a.1) * aby) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (px - (a.0 + abx * t)).hypot(py - (a.1 + aby * t))
}

/// An application's icon at `size` canvas pixels, tinted the launcher's way.
pub(super) fn app_icon(
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    entry: &Entry,
    size: u32,
    density: u32,
) -> Option<Pixmap> {
    let name = entry.icon.as_deref()?;
    let path = launcher_icon(icons, name, (size / density.max(1)).max(1), density)?;
    pixmaps.get(&path, size).map(tinted)
}

/// The icon drawn for a file.
pub(super) fn file_icon(
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    size: u32,
    density: u32,
) -> Option<Pixmap> {
    let path = launcher_icon(icons, FILE_ICON, (size / density.max(1)).max(1), density)?;
    pixmaps.get(&path, size).map(tinted)
}

/// The first of `names` the theme has, painted flat in `color`.
///
/// For the glyphs beside a category or a detail, which follow the text
/// colour — dim, or the accent when chosen — rather than an artwork's own
/// hue. Several names because themes disagree on what a symbol is called;
/// none found draws nothing, and the text beside it stands on its own.
pub(super) fn symbol(
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    names: &[&str],
    size: u32,
    density: u32,
    color: Color,
) -> Option<Pixmap> {
    let logical = (size / density.max(1)).max(1);
    let path = names.iter().find_map(|name| {
        icons
            .find_symbolic(name, logical, density)
            .or_else(|| icons.find(name, logical, density))
    })?;
    let [r, g, b, _] = color.to_rgba_bytes();
    pixmaps
        .get(&path, size)
        .map(|icon| icon.tinted([r, g, b], [r, g, b]))
}

/// Blit `image` centred on (`cx`, `cy`).
pub(super) fn blit_centred(canvas: &mut Canvas, image: &Pixmap, cx: f32, cy: f32) {
    let x = (cx - image.width as f32 / 2.0).round().max(0.0) as usize;
    let y = (cy - image.height as f32 / 2.0).round().max(0.0) as usize;
    canvas.blit(x, y, image);
}

/// `label` at `size`, centred on `cx`, its box's top at `y`.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_centred(
    text: &mut Text,
    canvas: &mut Canvas,
    label: &str,
    size: f32,
    cx: f32,
    y: f32,
    color: Color,
    weight: Weight,
) {
    let (w, _) = text.measure_weighted(label, size, weight);
    text.draw_weighted(
        canvas,
        label,
        size,
        (cx - w / 2.0).round() as i32,
        y.round() as i32,
        color,
        weight,
    );
}

/// `query`, cut from the front with an ellipsis to fit `max_w`: the end of
/// what is being typed is where the caret is, and what the eye is on.
pub(super) fn fit_tail(text: &mut Text, query: &str, size: f32, max_w: f32) -> String {
    if text.measure(query, size).0 <= max_w {
        return query.to_owned();
    }
    let chars: Vec<char> = query.chars().collect();
    let cut = |skip: usize| format!("…{}", chars[skip..].iter().collect::<String>());
    let (mut low, mut high) = (1, chars.len());
    while low < high {
        let mid = (low + high) / 2;
        if text.measure(&cut(mid), size).0 <= max_w {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    cut(low.min(chars.len()))
}

/// `words` broken into at most `max_lines` lines no wider than `width`, the
/// last cut with an ellipsis if the words ran out of lines.
pub(super) fn wrap(
    text: &mut Text,
    words: &str,
    size: f32,
    width: f32,
    max_lines: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut spilled = false;
    for word in words.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || text.measure(&candidate, size).0 <= width {
            line = candidate;
            continue;
        }
        lines.push(std::mem::replace(&mut line, word.to_owned()));
        if lines.len() == max_lines {
            spilled = true;
            break;
        }
    }
    if !spilled && !line.is_empty() {
        lines.push(line);
    }
    if spilled && let Some(last) = lines.last_mut() {
        *last = fit(text, &format!("{last} …"), size, width);
    }
    lines
        .into_iter()
        .map(|line| fit(text, &line, size, width))
        .collect()
}

/// A magnifying glass: a ring of radius `r` and a handle out of its lower
/// right, strokes `width` wide.
pub(super) fn draw_search_glyph(
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    r: f32,
    width: f32,
    color: Color,
) {
    let reach = r * 2.6;
    let handle = ((cx + r * 0.72, cy + r * 0.72), (cx + r * 1.8, cy + r * 1.8));
    canvas.paint(
        (cx - reach) as i32,
        (cy - reach) as i32,
        (reach * 2.0) as i32 + 2,
        (reach * 2.0) as i32 + 2,
        |x, y| {
            let ring = ((x - cx).hypot(y - cy) - r).abs() - width / 2.0;
            let stick = segment_distance(x, y, handle.0, handle.1) - width / 2.0;
            let coverage = edge(ring.min(stick));
            (coverage > 0.0).then_some((color, coverage))
        },
    );
}

/// A chevron centred on (`cx`, `cy`), `half` wide each side, pointing down,
/// or up when `up`.
pub(super) fn draw_chevron(
    canvas: &mut Canvas,
    cx: f32,
    cy: f32,
    half: f32,
    width: f32,
    up: bool,
    color: Color,
) {
    let lift = if up { -half / 2.0 } else { half / 2.0 };
    let (left, tip, right) = (
        (cx - half, cy - lift),
        (cx, cy + lift),
        (cx + half, cy - lift),
    );
    let reach = half + width * 2.0;
    canvas.paint(
        (cx - reach) as i32,
        (cy - reach) as i32,
        (reach * 2.0) as i32 + 2,
        (reach * 2.0) as i32 + 2,
        |x, y| {
            let d = segment_distance(x, y, left, tip).min(segment_distance(x, y, tip, right));
            let coverage = edge(d - width / 2.0);
            (coverage > 0.0).then_some((color, coverage))
        },
    );
}

/// A soft glowing dot: a bright core of radius `core` inside a halo that
/// falls off to nothing at `glow`.
pub(super) fn glow_dot(
    canvas: &mut Canvas,
    x: f32,
    y: f32,
    core: f32,
    glow: f32,
    core_color: Color,
    glow_color: Color,
) {
    canvas.paint(
        (x - glow) as i32,
        (y - glow) as i32,
        (glow * 2.0) as i32 + 2,
        (glow * 2.0) as i32 + 2,
        |px, py| {
            let d = (px - x).hypot(py - y);
            let halo = (1.0 - d / glow).max(0.0).powi(2);
            let mut color = faded(glow_color, halo);
            let body = edge(d - core);
            if body > 0.0 {
                color = over(color, faded(core_color, body));
            }
            let [.., a] = color.to_rgba_bytes();
            (a > 0).then_some((color, 1.0))
        },
    );
}

/// A glass panel: a translucent `ground`, a hairline `rim`, and the
/// catch-light along its top edge that every panel of the desktop has.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_glass(
    canvas: &mut Canvas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    ground: Color,
    rim: Color,
) {
    let (xu, yu, wu, hu) = (x as usize, y as usize, w as usize, h as usize);
    canvas.fill_rounded(xu, yu, wu, hu, radius, ground);
    canvas.stroke_rounded(xu, yu, wu, hu, radius, 1.0, rim);
    let inset = radius.ceil();
    if h > 2.0 && w > inset * 2.0 {
        canvas.paint(
            (x + inset) as i32,
            y as i32 + 1,
            (w - inset * 2.0) as i32,
            1,
            |_, _| Some((crate::theme::catch_light(), 1.0)),
        );
    }
}

/// Where a row of key hints is anchored.
pub(super) enum Align {
    /// Its right edge at this x.
    Right(f32),
    /// Centred on this x.
    Centre(f32),
}

/// A row of key hints — a chip for the key, the verb dim beside it — with
/// its top at `y`. Returns the rectangle the row covers.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_hints(
    canvas: &mut Canvas,
    text: &mut Text,
    hints: &[(&str, &str)],
    align: Align,
    y: f32,
    size: f32,
    scale: f32,
    chip: Color,
    chip_edge: Option<Color>,
) -> Rect {
    let pad = 6.0 * scale;
    let chip_h = size * 1.75;
    let between = 16.0 * scale;
    let widths: Vec<(f32, f32)> = hints
        .iter()
        .map(|(key, verb)| {
            (
                text.measure(key, size).0 + pad * 2.0,
                text.measure(verb, size).0,
            )
        })
        .collect();
    let total = widths.iter().map(|(k, v)| k + pad + v).sum::<f32>()
        + between * hints.len().saturating_sub(1) as f32;
    let mut x = match align {
        Align::Right(right) => right - total,
        Align::Centre(centre) => centre - total / 2.0,
    };
    let left = x;
    let text_y = (y + (chip_h - size * 1.35) / 2.0) as i32;
    for ((key, verb), (key_w, verb_w)) in hints.iter().zip(widths) {
        let (xu, yu) = (x.max(0.0) as usize, y.max(0.0) as usize);
        canvas.fill_rounded(xu, yu, key_w as usize, chip_h as usize, 5.0 * scale, chip);
        if let Some(rim) = chip_edge {
            canvas.stroke_rounded(
                xu,
                yu,
                key_w as usize,
                chip_h as usize,
                5.0 * scale,
                1.0,
                rim,
            );
        }
        text.draw(canvas, key, size, (x + pad) as i32, text_y, crate::theme::text());
        x += key_w + pad;
        text.draw(canvas, verb, size, x as i32, text_y, crate::theme::text_dim());
        x += verb_w + between;
    }
    Rect::from_xywh(left as i32, y as i32, total as i32, chip_h as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layering_over_nothing_is_the_top_layer_and_over_opaque_stays_opaque() {
        let top = rgba(200, 100, 50, 0.5);
        assert_eq!(
            over(rgba(0, 0, 0, 0.0), top).to_rgba_bytes(),
            top.to_rgba_bytes()
        );
        let [.., a] = over(rgba(10, 10, 10, 1.0), top).to_rgba_bytes();
        assert_eq!(a, 255);
    }

    #[test]
    fn angles_run_clockwise_from_the_right_on_a_downward_y() {
        assert!((angle_of(1.0, 0.0) - 0.0).abs() < 1e-3);
        assert!((angle_of(0.0, 1.0) - 90.0).abs() < 1e-3, "down is 90°");
        assert!((angle_of(0.0, -1.0) - 270.0).abs() < 1e-3, "up is 270°");
        assert!((angular_gap(350.0, 10.0) - 20.0).abs() < 1e-3);
    }
}
