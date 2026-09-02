//! What dresses the overview once its windows are spread out.
//!
//! The spread itself lives in `state.rs` — where each window's patch is and
//! how it drifts there. This is the chrome around it, modelled on Mission
//! Control: a row of space labels along the top, a soft shadow under every
//! thumbnail so it sits *on* the dimmed desktop rather than being pasted to
//! it, a rounded ring around the one the highlight is on, and that window's
//! title under it.
//!
//! Everything here is composed once, when the overview opens or its contents
//! change, and drawn scaled after that. The thumbnails move on every frame of
//! the reveal, and re-rasterizing a shadow sixty times a second would cost
//! more than the animation is worth; a shadow scaled by a few percent looks
//! like the same shadow.

use huginn_core::geometry::Rect;

use crate::canvas::{Canvas, Panel};
use crate::text::Text;
use crate::theme::Color;

/// Height of the row of space labels, at 1080p.
const BAR_HEIGHT: f32 = 34.0;
/// Air above the bar, and between the bar and the windows below.
const BAR_MARGIN: f32 = 12.0;
/// Text size of a space label, at 1080p.
const LABEL_SIZE: f32 = 13.0;
/// Horizontal padding inside a label's pill.
const LABEL_PAD: f32 = 14.0;
/// Space between one label and the next.
const LABEL_GAP: f32 = 6.0;
/// The pill behind the front space's label: white, mostly see-through.
const PILL: Color = Color::from_argb(0x3CFF_FFFF);

/// How far the backing stands proud of the window on every side.
const BORDER: f32 = 4.0;
/// How far the shadow reaches beyond the backing.
const SHADOW: f32 = 26.0;
/// How far the highlight ring stands proud of the backing.
const RING: f32 = 3.0;
/// Corner radius of the backing.
const RADIUS: f32 = 10.0;
/// The shadow's opacity where it meets the backing.
const SHADOW_ALPHA: f32 = 0.6;
/// The backing's opacity.
const BACKING_ALPHA: u8 = 0xE6;
/// The backing and its shadow are composed at this fraction of full
/// resolution and scaled up. A shadow is a blur; sharpening it would be a
/// waste, and the backing's edge is soft enough at half size not to show.
const SOFT: u32 = 2;

/// Everything on the desktop grows with the screen, so the overview does too.
pub(crate) fn ui_scale(output: Rect) -> f32 {
    (output.h() as f32 / 1080.0).clamp(1.0, 2.5)
}

/// The strip along the top of the area that the space labels take.
pub(crate) fn bar_room(output: Rect) -> i32 {
    ((BAR_MARGIN * 2.0 + BAR_HEIGHT) * ui_scale(output)) as i32
}

/// The row of space labels.
#[derive(Debug)]
pub(crate) struct SpacesBar {
    pub panel: Panel,
    /// Where the row goes, in logical pixels.
    pub rect: Rect,
    /// One rectangle per label, in logical pixels, in workspace order — what
    /// a click on the row lands in.
    pub labels: Vec<Rect>,
}

/// Compose the row of space labels for `count` workspaces with `front` at the
/// front, centred along the top of `area`.
pub(crate) fn spaces_bar(
    text: &mut Text,
    count: usize,
    front: usize,
    area: Rect,
    density: u32,
) -> Option<SpacesBar> {
    if !text.is_usable() || count == 0 {
        return None;
    }
    let density = density.max(1);
    let scale = ui_scale(area) * density as f32;
    let size = LABEL_SIZE * scale;
    let (pad, gap) = (LABEL_PAD * scale, LABEL_GAP * scale);
    let height = (BAR_HEIGHT * scale).ceil();

    let names: Vec<String> = (1..=count).map(|n| format!("Desktop {n}")).collect();
    let widths: Vec<f32> = names
        .iter()
        .map(|name| text.measure(name, size).0 + pad * 2.0)
        .collect();
    let total = widths.iter().sum::<f32>() + gap * (count as f32 - 1.0);
    let (w, h) = (total.ceil() as usize, height as usize);
    let mut canvas = Canvas::new(w.max(1), h.max(1));

    let mut x = 0.0_f32;
    let mut labels = Vec::with_capacity(count);
    for (index, (name, width)) in names.iter().zip(&widths).enumerate() {
        let (_, text_h) = text.measure(name, size);
        let colour = if index == front {
            canvas.fill_rounded(x as usize, 0, width.ceil() as usize, h, height * 0.5, PILL);
            crate::theme::TEXT
        } else {
            crate::theme::TEXT_DIM
        };
        text.draw(
            &mut canvas,
            name,
            size,
            (x + pad) as i32,
            ((height - text_h) * 0.5) as i32,
            colour,
        );
        labels.push((x, *width));
        x += width + gap;
    }

    let panel = Panel::from_canvas(&canvas, density);
    let (pw, ph) = panel.size();
    let rect = Rect::from_xywh(
        area.x() + (area.w() - pw) / 2,
        area.y() + (BAR_MARGIN * ui_scale(area)) as i32,
        pw,
        ph,
    );
    let labels = labels
        .into_iter()
        .map(|(x, width)| {
            Rect::from_xywh(
                rect.x() + (x / density as f32) as i32,
                rect.y(),
                (width / density as f32).ceil() as i32,
                ph,
            )
        })
        .collect();
    Some(SpacesBar {
        panel,
        rect,
        labels,
    })
}

/// The chrome of one window in the overview.
#[derive(Debug)]
pub(crate) struct Thumb {
    pub workspace: usize,
    pub window: huginn_core::window::WindowId,
    /// The backing and its shadow, composed for the window's settled frame.
    pub backing: Panel,
    /// The highlight ring — composed the first time the highlight lands here,
    /// since most windows never get one.
    pub halo: Option<Panel>,
    /// The window's title, shown while the highlight is on it.
    pub caption: Option<Panel>,
}

/// The backing's visible edge: `frame` plus the border.
pub(crate) fn backing_rect(frame: Rect, output: Rect) -> Rect {
    frame.inset(-((BORDER * ui_scale(output)) as i32))
}

/// Where the backing's panel goes: the backing plus the shadow's reach.
pub(crate) fn shadow_rect(frame: Rect, output: Rect) -> Rect {
    backing_rect(frame, output).inset(-((SHADOW * ui_scale(output)) as i32))
}

/// Where the ring's panel goes: the backing plus the ring.
pub(crate) fn halo_rect(frame: Rect, output: Rect) -> Rect {
    backing_rect(frame, output).inset(-((RING * ui_scale(output)) as i32))
}

/// The backing with its shadow, to the size [`shadow_rect`] gives it.
///
/// Composed at [`SOFT`]th resolution: the renderer stretches it to the
/// rectangle either way, and a shadow does not get sharper for having more
/// pixels.
pub(crate) fn backing(frame: Rect, output: Rect, density: u32) -> Panel {
    let density = density.max(1);
    Panel::from_canvas(
        &compose_backing(frame, output, density),
        density / SOFT.min(density),
    )
}

/// Paint the backing and shadow; see [`backing`].
fn compose_backing(frame: Rect, output: Rect, density: u32) -> Canvas {
    let outer = shadow_rect(frame, output);
    // Canvas pixels per logical pixel.
    let px = density as f32 / SOFT as f32;
    let (w, h) = (
        ((outer.w() as f32 * px).ceil() as usize).max(1),
        ((outer.h() as f32 * px).ceil() as usize).max(1),
    );
    let scale = ui_scale(output) * px;
    let (shadow, radius) = (SHADOW * scale, RADIUS * scale);
    // The backing's box, in canvas pixels, as a centre and half-extents
    // with the corner radius already taken off — the signed-distance form.
    let (cx, cy) = (w as f32 * 0.5, h as f32 * 0.5);
    let (hx, hy) = (cx - shadow - radius, cy - shadow - radius);
    let [br, bg, bb, _] = crate::theme::BACKGROUND.to_rgba_bytes();
    let backing_alpha = f32::from(BACKING_ALPHA) / 255.0;

    let mut canvas = Canvas::new(w, h);
    for row in 0..h {
        for col in 0..w {
            let (px, py) = (col as f32 + 0.5, row as f32 + 0.5);
            let (qx, qy) = ((px - cx).abs() - hx, (py - cy).abs() - hy);
            // Distance to the rounded box: positive outside, negative in.
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
            let cover = (0.5 - d).clamp(0.0, 1.0);
            let shade = if d <= 0.0 {
                SHADOW_ALPHA
            } else {
                let fall = (1.0 - d / shadow).clamp(0.0, 1.0);
                SHADOW_ALPHA * fall * fall
            };
            let over = backing_alpha * cover;
            // The shadow is what the backing does not cover: under the
            // backing it would only make the backing more opaque than it is.
            let alpha = over + shade * (1.0 - cover) * (1.0 - over);
            if alpha <= 0.0 {
                continue;
            }
            // Straight alpha, like every other panel: the colour is the
            // backing's where it covers and black where only the shadow does.
            let mix = |c: u8| (f32::from(c) * over / alpha).round() as u8;
            let offset = (row * w + col) * 4;
            canvas.pixels[offset..offset + 4].copy_from_slice(&[
                mix(br),
                mix(bg),
                mix(bb),
                (alpha * 255.0).round() as u8,
            ]);
        }
    }
    canvas
}

/// The highlight ring, to the size [`halo_rect`] gives it: a rounded slab of
/// accent the window and its backing sit on, so what shows is the rim.
pub(crate) fn halo(frame: Rect, output: Rect, density: u32) -> Panel {
    let density = density.max(1);
    let outer = halo_rect(frame, output);
    let (w, h) = (
        (outer.w() as u32 * density) as usize,
        (outer.h() as u32 * density) as usize,
    );
    let scale = ui_scale(output) * density as f32;
    let mut canvas = Canvas::new(w.max(1), h.max(1));
    canvas.fill_rounded(0, 0, w, h, (RADIUS + RING) * scale, crate::theme::accent());
    Panel::from_canvas(&canvas, density)
}

/// Where a window's title goes: under its backing, and the room that takes.
pub(crate) fn caption_placement(caption: &Panel, frame: Rect, output: Rect) -> Rect {
    crate::dock::caption_placement(caption, backing_rect(frame, output), output)
}

/// Room to leave under every thumbnail for a title, whether or not one is
/// showing, so the highlight landing on a window does not move anything.
pub(crate) fn caption_room(output: Rect) -> i32 {
    (crate::dock::CAPTION_SIZE * 2.0 * ui_scale(output)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    #[test]
    fn the_shadow_fades_out_before_the_panel_edge_and_the_backing_is_solid() {
        let frame = Rect::from_xywh(100, 100, 400, 300);
        let canvas = compose_backing(frame, AREA, 1);
        let (w, h) = (canvas.stride, canvas.height);
        let at = |x: usize, y: usize| canvas.pixels[(y * w + x) * 4 + 3];
        // The very corner is beyond the shadow's reach: clear.
        assert_eq!(at(0, 0), 0);
        // The middle is the backing, at its opacity.
        assert_eq!(at(w / 2, h / 2), BACKING_ALPHA);
        // Just outside the backing there is shadow, fading with distance.
        let shadow = (SHADOW * ui_scale(AREA) / SOFT as f32) as usize;
        let near = at(w / 2, shadow - 2);
        let far = at(w / 2, 2);
        assert!(
            near > far,
            "shadow {near} at the backing, {far} at the edge"
        );
        assert!(far < 40, "the shadow has all but gone by the panel's edge");
    }

    #[test]
    fn the_ring_and_shadow_rects_nest_around_the_frame() {
        let frame = Rect::from_xywh(200, 200, 300, 200);
        let backing = backing_rect(frame, AREA);
        let halo = halo_rect(frame, AREA);
        let shadow = shadow_rect(frame, AREA);
        assert!(backing.x() < frame.x() && backing.right() > frame.right());
        assert!(halo.x() < backing.x() && halo.bottom() > backing.bottom());
        assert!(shadow.x() < halo.x() && shadow.right() > halo.right());
        assert_eq!(backing.center(), frame.center());
        assert_eq!(shadow.center(), frame.center());
    }

    /// Paint a mock overview to a PPM, to look at the chrome without a
    /// desktop: `OVERVIEW_DUMP=/tmp/o.ppm cargo test -p huginn-comp overview_dump`.
    #[test]
    fn overview_dump() {
        let Ok(path) = std::env::var("OVERVIEW_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let (w, h) = (AREA.w() as usize, AREA.h() as usize);
        let mut scene = Canvas::new(w, h);
        // A wallpaper with some variation in it, half-veiled.
        for row in 0..h {
            for col in 0..w {
                let t = (col as f32 / w as f32 + row as f32 / h as f32) * 0.5;
                let (r, g, b) = (40.0 + 60.0 * t, 60.0 + 40.0 * (1.0 - t), 110.0 + 50.0 * t);
                scene.fill(
                    col,
                    row,
                    1,
                    1,
                    [(r * 0.5) as u8, (g * 0.5) as u8, (b * 0.5) as u8, 255],
                );
            }
        }
        // Straight-alpha source over `scene`, stretched to `dst`.
        let over = |scene: &mut Canvas, src: &Canvas, dst: Rect| {
            for row in 0..dst.h().max(0) as usize {
                for col in 0..dst.w().max(0) as usize {
                    let sx = (col * src.stride / dst.w() as usize).min(src.stride - 1);
                    let sy = (row * src.height / dst.h() as usize).min(src.height - 1);
                    let p = &src.pixels[(sy * src.stride + sx) * 4..][..4];
                    let a = f32::from(p[3]) / 255.0;
                    let (x, y) = (dst.x() as usize + col, dst.y() as usize + row);
                    if x >= scene.stride || y >= scene.height || a == 0.0 {
                        continue;
                    }
                    let o = (y * scene.stride + x) * 4;
                    for (c, &value) in p[..3].iter().enumerate() {
                        let under = f32::from(scene.pixels[o + c]);
                        scene.pixels[o + c] =
                            (under + (f32::from(value) - under) * a).round() as u8;
                    }
                }
            }
        };
        let area = Rect::from_xywh(0, 34, 1920, 1080 - 34);
        if let Some(bar) = spaces_bar(&mut text, 3, 1, area, 1) {
            // The panel's canvas is not kept, so compose it again for the dump.
            let (pw, ph) = bar.panel.size();
            let mut tmp = Canvas::new(pw as usize, ph as usize);
            tmp.fill_rounded(0, 0, pw as usize, ph as usize, ph as f32 * 0.5, PILL);
            over(&mut scene, &tmp, bar.labels[1]);
            for (index, label) in bar.labels.iter().enumerate() {
                let mut t = Canvas::new(label.w() as usize, label.h() as usize);
                let name = format!("Desktop {}", index + 1);
                let (_, th) = text.measure(&name, LABEL_SIZE);
                let colour = if index == 1 {
                    crate::theme::TEXT
                } else {
                    crate::theme::TEXT_DIM
                };
                text.draw(
                    &mut t,
                    &name,
                    LABEL_SIZE,
                    LABEL_PAD as i32,
                    ((label.h() as f32 - th) * 0.5) as i32,
                    colour,
                );
                over(&mut scene, &t, *label);
            }
        }
        let frames = [
            Rect::from_xywh(140, 150, 800, 430),
            Rect::from_xywh(1000, 150, 780, 430),
            Rect::from_xywh(560, 640, 800, 380),
        ];
        for (index, frame) in frames.iter().enumerate() {
            let backing = compose_backing(*frame, AREA, 1);
            if index == 0 {
                let halo = halo_rect(*frame, AREA);
                let mut ring = Canvas::new(halo.w() as usize, halo.h() as usize);
                ring.fill_rounded(
                    0,
                    0,
                    halo.w() as usize,
                    halo.h() as usize,
                    (RADIUS + RING) * ui_scale(AREA),
                    crate::theme::accent(),
                );
                over(&mut scene, &backing, shadow_rect(*frame, AREA));
                over(&mut scene, &ring, halo);
                if let Some(caption) = crate::dock::caption(
                    &mut text,
                    "New Tab — Brave",
                    backing_rect(*frame, AREA).w(),
                    AREA,
                    1,
                ) {
                    let at = caption_placement(&caption, *frame, AREA);
                    let mut t = Canvas::new(at.w() as usize, at.h() as usize);
                    t.fill_rounded(
                        0,
                        0,
                        at.w() as usize,
                        at.h() as usize,
                        at.h() as f32 * 0.5,
                        crate::theme::BACKGROUND.with_alpha(0xE6),
                    );
                    let (tw, th) = text.measure("New Tab — Brave", crate::dock::CAPTION_SIZE);
                    text.draw(
                        &mut t,
                        "New Tab — Brave",
                        crate::dock::CAPTION_SIZE,
                        ((at.w() as f32 - tw) * 0.5) as i32,
                        ((at.h() as f32 - th) * 0.5) as i32,
                        crate::theme::TEXT,
                    );
                    over(&mut scene, &t, at);
                }
            } else {
                over(&mut scene, &backing, shadow_rect(*frame, AREA));
            }
            // The window itself: a flat slab with a lighter title strip.
            let mut win = Canvas::new(frame.w() as usize, frame.h() as usize);
            win.fill(
                0,
                0,
                frame.w() as usize,
                frame.h() as usize,
                [30, 32, 44, 255],
            );
            win.fill(0, 0, frame.w() as usize, 36, [52, 54, 70, 255]);
            over(&mut scene, &win, *frame);
        }
        let mut ppm = format!("P6\n{} {}\n255\n", scene.stride, scene.height).into_bytes();
        for pixel in scene.pixels.chunks(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
    }

    #[test]
    fn the_bar_reserves_room_above_the_windows() {
        assert!(bar_room(AREA) > 0);
        assert!(
            bar_room(AREA) < AREA.h() / 8,
            "the bar is a strip, not a band"
        );
    }
}
