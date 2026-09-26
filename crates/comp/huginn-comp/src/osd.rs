//! The slider that pops up at the bottom of the screen when a key changes a
//! level: volume, brightness.
//!
//! One widget rather than one per level, because the two are the same thing
//! — a confirmation of a key somebody already pressed — and a brightness
//! slider that looked or lingered differently from the volume one would be a
//! second design for one idea. What differs between them is the number and
//! what it drives; [`Slider`] is the number, [`Flash`] is when it is shown,
//! and each level keeps its own of both.
//!
//! # Timing
//!
//! Shown for a moment whenever a key changes the level, then gone. Nothing
//! here schedules the hiding: [`Flash::tick`] is asked once per frame, the
//! same way every other animation is, and the frame loop keeps going while
//! [`Flash::is_animating`] says the slider is on screen. That is a second
//! and a half of frames per key press, which is what an animation costs.

use std::time::Duration;

use huginn_core::geometry::Rect;

use crate::anim::{Animated, Curve};
use crate::canvas::{Canvas, Panel};
use crate::settings::Motion;
use crate::text::Text;

/// How long the slider stays fully shown after the last change.
const HOLD: Duration = Duration::from_millis(1500);
/// How long it takes to appear. Fast: the key has already been pressed.
const SHOW: Duration = Duration::from_millis(120);

/// Whether a slider is on screen, and how far faded in.
#[derive(Debug)]
pub(crate) struct Flash {
    /// 0 hidden, 1 shown. Drives the fade.
    reveal: Animated,
    /// When the slider should start to fade, while it is being held up.
    hide_at: Option<Duration>,
}

impl Default for Flash {
    fn default() -> Self {
        Self {
            reveal: Animated::settled(0.0),
            hide_at: None,
        }
    }
}

impl Flash {
    /// Put the slider on screen, or keep it there a little longer.
    pub(crate) fn show(&mut self, now: Duration, motion: Motion) {
        self.hide_at = Some(now + HOLD);
        self.reveal
            .animate_to(1.0, now, motion.duration(SHOW), Curve::EaseOut);
    }

    /// Take it off screen at once, without the fade.
    ///
    /// For when another slider is about to appear in the same place: two
    /// confirmations stacked on each other is one too many, and fading the
    /// old one out under the new one is a flicker, not a transition.
    pub(crate) fn dismiss(&mut self) {
        self.hide_at = None;
        self.reveal.jump_to(0.0);
    }

    /// When the hold ends, while the slider is being held up. The later of
    /// two is the one shown more recently.
    pub(crate) fn held_until(&self) -> Option<Duration> {
        self.hide_at
    }

    /// Start the fade once the hold is over. Called once per frame.
    pub(crate) fn tick(&mut self, now: Duration, motion: Motion) {
        if self.hide_at.is_some_and(|at| now >= at) {
            self.hide_at = None;
            self.reveal.animate_to(
                0.0,
                now,
                motion.duration(crate::anim::VOLUME_FADE),
                Curve::EaseOut,
            );
        }
    }

    /// How far the slider has faded in, 0..=1.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
        self.reveal.value(now).clamp(0.0, 1.0)
    }

    /// Whether the slider is on screen at all, held or fading.
    pub(crate) fn is_visible(&self, now: Duration) -> bool {
        self.hide_at.is_some() || self.reveal(now) > 0.001
    }

    /// Whether the frame loop has to keep going for the slider's sake.
    ///
    /// True for the whole time it is shown, not only while it is moving: the
    /// hold ends by the clock, and the clock is only read from `tick`, which
    /// only runs on a frame.
    pub(crate) fn is_animating(&self, now: Duration) -> bool {
        self.is_visible(now)
    }
}

/// What one slider shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Slider {
    /// On the left: what the level is of.
    pub label: &'static str,
    /// On the right: the level, in words.
    pub caption: String,
    /// How much of the track is filled, 0..=1.
    pub fraction: f32,
    /// Whether the level drives anything. A slider with nothing behind it
    /// still moves, and says so rather than silently going nowhere.
    pub real: bool,
    /// Drawn subdued: muted, for volume.
    pub dim: bool,
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The slider's size at a 1080p output, in logical pixels.
const WIDTH: f32 = 300.0;
const HEIGHT: f32 = 64.0;
const PAD: f32 = 18.0;
/// Text size at a 1080p output.
const BASE_SIZE: f32 = 14.0;
/// The track's thickness.
const TRACK: f32 = 6.0;
/// The knob's diameter.
const KNOB: f32 = 14.0;
/// How far above the bottom edge it floats, clear of the dock.
const FROM_BOTTOM: i32 = 120;
const ALPHA: u8 = 0xF2;

/// Where the slider sits: bottom centre of the output.
///
/// Bottom rather than the middle, where the launcher and quick settings go.
/// Those are things somebody opened and is looking at; this is a
/// confirmation of a key they already pressed, and it should not land on the
/// thing they were reading when they pressed it.
pub(crate) fn placement(output: Rect, size: (i32, i32)) -> Rect {
    let (w, h) = size;
    let x = output.x() + (output.w() - w).max(0) / 2;
    let y = (output.y() + output.h() - h - FROM_BOTTOM).max(output.y());
    Rect::from_xywh(x, y, w, h)
}

/// Draw `slider` for `output` at `density` pixels per logical one.
pub(crate) fn render(slider: &Slider, text: &mut Text, output: Rect, density: u32) -> Panel {
    Panel::from_canvas(&compose(slider, text, output, density), density)
}

fn compose(slider: &Slider, text: &mut Text, output: Rect, density: u32) -> Canvas {
    // In the canvas's own pixels, `density` times the logical ones. See
    // `Panel::from_canvas`.
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density.max(1) as f32;
    let size = BASE_SIZE * scale;
    let pad = PAD * scale;
    let width = (WIDTH * scale) as usize;
    let height = (HEIGHT * scale) as usize;

    let mut canvas = Canvas::new(width, height);
    canvas.fill_rounded(
        0,
        0,
        width,
        height,
        12.0 * scale,
        crate::theme::background().with_alpha(ALPHA),
    );

    // The label on the left, the reading on the right, and the track between
    // the two on the line below.
    let text_y = pad * 0.55;
    let label_color = if slider.real {
        crate::theme::text()
    } else {
        crate::theme::text_dim()
    };
    text.draw(
        &mut canvas,
        slider.label,
        size,
        pad as i32,
        text_y as i32,
        label_color,
    );
    let caption = if slider.real {
        slider.caption.clone()
    } else {
        format!("{} · not connected", slider.caption)
    };
    let caption_w = text.measure(&caption, size).0;
    text.draw(
        &mut canvas,
        &caption,
        size,
        (width as f32 - pad - caption_w) as i32,
        text_y as i32,
        if slider.dim {
            crate::theme::text_dim()
        } else {
            crate::theme::accent()
        },
    );

    // The track, and the filled part of it. Both rounded, so the fill's end
    // matches the track's end when the level is full.
    let track_y = height as f32 - pad - TRACK * scale;
    let track_w = width as f32 - pad * 2.0;
    let track_h = TRACK * scale;
    canvas.fill_rounded(
        pad as usize,
        track_y as usize,
        track_w as usize,
        track_h as usize,
        track_h / 2.0,
        crate::theme::border(),
    );
    let filled = (track_w * slider.fraction.clamp(0.0, 1.0)).round();
    if filled >= 1.0 {
        canvas.fill_rounded(
            pad as usize,
            track_y as usize,
            filled as usize,
            track_h as usize,
            track_h / 2.0,
            crate::theme::accent(),
        );
    }

    // The knob, centred on the end of the fill. A slider without a knob is a
    // progress bar, and this is something the arrows move.
    let knob = KNOB * scale;
    let knob_x = (pad + filled - knob / 2.0).clamp(pad, pad + track_w - knob);
    let knob_y = track_y + track_h / 2.0 - knob / 2.0;
    canvas.fill_rounded(
        knob_x as usize,
        knob_y as usize,
        knob as usize,
        knob as usize,
        knob / 2.0,
        if slider.dim {
            crate::theme::text_dim()
        } else {
            crate::theme::text()
        },
    );

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn slider(percent: u32) -> Slider {
        Slider {
            label: "Volume",
            caption: format!("{percent}%"),
            fraction: percent as f32 / 100.0,
            real: true,
            dim: false,
        }
    }

    #[test]
    fn the_slider_appears_holds_and_fades() {
        let mut flash = Flash::default();
        assert!(!flash.is_visible(T0));
        flash.show(T0, Motion::Full);
        assert!(flash.is_visible(T0));
        assert!(flash.reveal(T0) < 0.5, "it was already there at t=0");
        assert!((flash.reveal(ms(200)) - 1.0).abs() < 1e-3);

        // Held, well past the fade-in.
        flash.tick(ms(1000), Motion::Full);
        assert!(flash.is_visible(ms(1000)));
        assert!(flash.is_animating(ms(1000)), "frames must keep coming");

        // The hold ends, and it fades rather than vanishing.
        flash.tick(ms(1500), Motion::Full);
        assert!(flash.is_visible(ms(1500)));
        assert!(flash.reveal(ms(1550)) < 1.0);
        flash.tick(ms(1700), Motion::Full);
        assert!(!flash.is_visible(ms(1700)));
        assert!(!flash.is_animating(ms(1700)));
    }

    #[test]
    fn another_key_during_the_hold_keeps_it_up() {
        let mut flash = Flash::default();
        flash.show(T0, Motion::Full);
        flash.show(ms(1000), Motion::Full);
        flash.tick(ms(1600), Motion::Full);
        assert!(
            (flash.reveal(ms(1600)) - 1.0).abs() < 1e-3,
            "the second press did not restart the hold"
        );
    }

    #[test]
    fn reduced_motion_shows_it_at_once() {
        let mut flash = Flash::default();
        flash.show(T0, Motion::Reduced);
        assert!((flash.reveal(T0) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn dismissing_takes_it_off_at_once() {
        let mut flash = Flash::default();
        flash.show(T0, Motion::Full);
        flash.dismiss();
        assert!(!flash.is_visible(ms(200)));
        assert!(!flash.is_animating(ms(200)));
    }

    #[test]
    fn it_sits_at_the_bottom_centre_of_the_output() {
        let output = Rect::from_xywh(100, 50, 1920, 1080);
        let rect = placement(output, (300, 64));
        assert_eq!(rect.x(), 100 + (1920 - 300) / 2);
        assert_eq!(rect.y(), 50 + 1080 - 64 - FROM_BOTTOM);
    }

    /// `OSD_DUMP=/path/out.ppm cargo test osd_dump -- --nocapture` writes the
    /// slider at `OSD_AT` percent (default 65) to look at.
    #[test]
    fn osd_dump() {
        let Ok(path) = std::env::var("OSD_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let percent = std::env::var("OSD_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(65);
        let canvas = compose(
            &slider(percent),
            &mut text,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        );
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.as_chunks::<4>().0.iter() {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }

    #[test]
    fn the_fill_follows_the_level() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        // Count accent-coloured pixels along the track's centre line.
        let accent = crate::theme::accent().to_rgba_bytes();
        let mut filled = |slider: &Slider| {
            let canvas = compose(slider, &mut text, output, 1);
            let row = (HEIGHT - PAD - TRACK / 2.0) as usize;
            (0..canvas.stride)
                .filter(|col| {
                    let offset = (row * canvas.stride + col) * 4;
                    canvas.pixels[offset..offset + 3] == accent[..3]
                })
                .count()
        };
        let quiet = filled(&slider(20));
        let loud = filled(&slider(80));
        assert!(quiet > 0, "a 20% slider drew no fill at all");
        assert!(
            loud > quiet * 3,
            "80% ({loud}px) should be about four times 20% ({quiet}px)"
        );
        let empty = Slider {
            fraction: 0.0,
            dim: true,
            ..slider(80)
        };
        assert_eq!(filled(&empty), 0, "an empty slider drew a fill");
    }
}
