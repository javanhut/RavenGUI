//! Output volume: the media keys, the mixer they drive, and the slider that
//! shows what they did.
//!
//! Three things live here because they cannot be allowed to disagree. The
//! keymap resolves `XF86AudioRaiseVolume` and friends to a [`Key`]; the
//! quick settings panel has a row that steps the same level with the arrows;
//! and the on-screen slider is what both of them draw. One [`Volume`] holds
//! the level, and everything that shows or changes it goes through that.
//!
//! # The mixer
//!
//! The level is set through `wpctl`, PipeWire's own control tool, rather than
//! by linking a PipeWire client into the compositor. A compositor that speaks
//! the PipeWire protocol has a second event loop to keep alive and a daemon
//! to reconnect to; a compositor that runs `wpctl set-volume` has a process
//! to spawn, which it already knows how to do. The cost is that the level
//! shown is what huginn last asked for, not a live readout: a mixer moved by
//! another program is not noticed until the next key press re-reads it. That
//! is the trade the launcher already made for the application list.
//!
//! When there is no `wpctl` — or no PipeWire behind it — the level still
//! moves on screen, and the panel says so, the way every other stub in quick
//! settings does. A slider that silently goes nowhere is a bug report; one
//! that says "not connected" is a to-do.
//!
//! Whether there is a mixer is asked every time, not once at startup. Huginn
//! is the first thing the session starts, and PipeWire comes up a second or
//! two behind it: a decision made at startup would be "no mixer" on every
//! boot, and the keys would draw a slider all day without ever moving the
//! sound. Each key press re-reads the level anyway, so the same read says
//! whether anyone answered.
//!
//! # The slider
//!
//! Shown for a moment whenever a key changes the level, then gone. Nothing
//! here schedules the hiding: [`Volume::tick`] is asked once per frame, the
//! same way every other animation is, and the frame loop keeps going while
//! [`Volume::is_animating`] says the slider is on screen. That is a second
//! and a half of frames per key press, which is what an animation costs.

use std::cell::RefCell;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use huginn_core::geometry::Rect;

use crate::anim::{Animated, Curve};
use crate::canvas::{Canvas, Panel};
use crate::settings::Motion;
use crate::text::Text;

/// One media key's worth of change, as a percentage.
pub(crate) const STEP: u32 = 5;

/// How long the slider stays fully shown after the last change.
const HOLD: Duration = Duration::from_millis(1500);
/// How long it takes to appear. Fast: the key has already been pressed.
const SHOW: Duration = Duration::from_millis(120);

/// The PipeWire node every command is aimed at.
const SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// What the media keys mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Raise,
    Lower,
    ToggleMute,
}

/// The output level, and whether it is audible.
///
/// `percent` survives muting: unmuting goes back to where it was, which is
/// what a mute key is for. A mute that set the level to zero would be a
/// second way to turn the sound down, not a way to turn it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Level {
    pub percent: u32,
    pub muted: bool,
}

impl Level {
    /// The level as a fraction of full, for a slider. Zero while muted, so
    /// the bar shows what is coming out of the speakers rather than what
    /// would be if they were on.
    pub(crate) fn fraction(self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.percent as f32 / 100.0
        }
    }

    /// What to write next to the slider.
    pub(crate) fn caption(self) -> String {
        if self.muted {
            "Muted".to_owned()
        } else {
            format!("{}%", self.percent)
        }
    }
}

/// Something that can tell us the level and set it.
///
/// A trait for the same reason quick settings has one per control: the real
/// one needs PipeWire on the machine running the tests, and a slider whose
/// arithmetic can only be checked with a sound card is a slider whose
/// arithmetic is not checked.
pub(crate) trait Mixer: std::fmt::Debug {
    /// The level right now, or `None` if there is no mixer to ask.
    fn read(&self) -> Option<Level>;
    /// Set the level. Fire and forget: a key press should not wait on it.
    fn apply(&self, level: Level);
}

/// The mixer that does nothing, for tests and for machines without one.
#[derive(Debug, Default)]
pub(crate) struct Silent;

impl Mixer for Silent {
    fn read(&self) -> Option<Level> {
        None
    }
    fn apply(&self, _: Level) {}
}

/// PipeWire, through `wpctl`.
#[derive(Debug, Default)]
pub(crate) struct Wpctl;

impl Wpctl {
    /// Parse `wpctl get-volume`'s one line: `Volume: 0.40` or
    /// `Volume: 0.40 [MUTED]`.
    fn parse(line: &str) -> Option<Level> {
        let rest = line.trim().strip_prefix("Volume:")?.trim();
        let mut parts = rest.split_whitespace();
        let value: f32 = parts.next()?.parse().ok()?;
        let muted = parts.any(|part| part == "[MUTED]");
        Some(Level {
            percent: (value * 100.0).round().clamp(0.0, 100.0) as u32,
            muted,
        })
    }
}

impl Mixer for Wpctl {
    fn read(&self) -> Option<Level> {
        // Waited for: it is a few milliseconds, it runs once at startup and
        // once per key press, and the alternative is showing a level the
        // machine is not at. `wpctl` writes PipeWire's own warnings to
        // stderr, which are its business rather than the log's.
        let output = Command::new("wpctl")
            .args(["get-volume", SINK])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Self::parse(&String::from_utf8_lossy(&output.stdout))
    }

    fn apply(&self, level: Level) {
        // Two commands, because `wpctl` sets volume and mute separately and
        // there is no reason to be clever about skipping one: each is a
        // process that lives for a frame.
        let percent = format!("{}%", level.percent);
        let mute = if level.muted { "1" } else { "0" };
        for args in [
            // `-l 1.0` caps what the mixer will accept, so a bug here cannot
            // ask the hardware for 300% and hand somebody a fright.
            vec!["set-volume", "-l", "1.0", SINK, percent.as_str()],
            vec!["set-mute", SINK, mute],
        ] {
            let spawned = Command::new("wpctl")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => crate::backend::reap(child, "wpctl"),
                Err(e) => tracing::warn!(?args, error = %e, "wpctl failed to start"),
            }
        }
    }
}

/// The volume, and the slider that shows it.
#[derive(Debug)]
pub(crate) struct Volume {
    level: Level,
    /// Whether `level` came from a mixer, or is a number being shown to
    /// nobody. Read by the settings row so it can say which.
    real: bool,
    mixer: Box<dyn Mixer>,
    /// 0 hidden, 1 shown. Drives the slider's fade.
    reveal: Animated,
    /// When the slider should start to fade, while it is being held up.
    hide_at: Option<Duration>,
}

/// The volume as the compositor and quick settings both hold it.
///
/// Shared rather than owned by one of them, because the settings row and the
/// media keys are two ways of moving one number — a second copy would be a
/// row that shows 40% while the speakers are at 60%.
pub(crate) type Shared = Rc<RefCell<Volume>>;

impl Default for Volume {
    /// A volume with nothing behind it. What tests use, and what the
    /// settings panel gets when it is built on its own.
    fn default() -> Self {
        Self::with_mixer(Box::new(Silent))
    }
}

impl Volume {
    /// The machine's mixer: `wpctl`, whether or not PipeWire is up yet.
    ///
    /// Not a probe. PipeWire usually starts after huginn does, so whether a
    /// read gets an answer at startup says nothing about a minute later; the
    /// mixer is asked again on every key press, and [`Volume::is_real`]
    /// follows the latest answer.
    pub(crate) fn detect() -> Self {
        let volume = Self::with_mixer(Box::new(Wpctl));
        if volume.real {
            tracing::info!(level = ?volume.level, "volume: wpctl");
        } else {
            tracing::info!("volume: wpctl gave no answer yet; will keep asking");
        }
        volume
    }

    pub(crate) fn with_mixer(mixer: Box<dyn Mixer>) -> Self {
        let level = mixer.read();
        Self {
            level: level.unwrap_or(Level {
                percent: 50,
                muted: false,
            }),
            real: level.is_some(),
            mixer,
            reveal: Animated::settled(0.0),
            hide_at: None,
        }
    }

    pub(crate) fn shared(self) -> Shared {
        Rc::new(RefCell::new(self))
    }

    pub(crate) fn level(&self) -> Level {
        self.level
    }

    /// Whether the level is the mixer's, rather than a number with nothing
    /// behind it.
    pub(crate) fn is_real(&self) -> bool {
        self.real
    }

    /// Act on a media key, and show the slider.
    ///
    /// Re-reads the mixer first, so a level moved from elsewhere is stepped
    /// from where it actually is rather than from where this last left it.
    pub(crate) fn press(&mut self, key: Key, now: Duration, motion: Motion) {
        self.sync();
        let level = self.level;
        let next = match key {
            Key::Raise => Level {
                percent: (level.percent + STEP).min(100),
                // Turning it up is a way of saying you want to hear it.
                muted: false,
            },
            Key::Lower => Level {
                percent: level.percent.saturating_sub(STEP),
                muted: false,
            },
            Key::ToggleMute => Level {
                muted: !level.muted,
                ..level
            },
        };
        self.set(next, now, motion);
    }

    /// Step the level by `delta` percent. What the settings row does.
    pub(crate) fn adjust(&mut self, delta: i32, now: Duration, motion: Motion) {
        self.sync();
        let percent = (self.level.percent as i32 + delta).clamp(0, 100) as u32;
        self.set(
            Level {
                percent,
                muted: false,
            },
            now,
            motion,
        );
    }

    pub(crate) fn toggle_mute(&mut self, now: Duration, motion: Motion) {
        self.sync();
        let level = self.level;
        self.set(
            Level {
                muted: !level.muted,
                ..level
            },
            now,
            motion,
        );
    }

    /// Ask the mixer where it is. A level moved from elsewhere is stepped
    /// from where it actually is; a mixer that has come up since the last
    /// look (or gone away) changes what the caption says.
    fn sync(&mut self) {
        match self.mixer.read() {
            Some(level) => {
                if !self.real {
                    tracing::info!(?level, "volume: mixer connected");
                }
                self.level = level;
                self.real = true;
            }
            None => self.real = false,
        }
    }

    fn set(&mut self, level: Level, now: Duration, motion: Motion) {
        self.level = level;
        self.mixer.apply(level);
        self.show(now, motion);
    }

    /// Put the slider on screen, or keep it there a little longer.
    fn show(&mut self, now: Duration, motion: Motion) {
        self.hide_at = Some(now + HOLD);
        self.reveal
            .animate_to(1.0, now, motion.duration(SHOW), Curve::EaseOut);
    }

    /// Start the fade once the hold is over. Called once per frame.
    pub(crate) fn tick(&mut self, now: Duration, motion: Motion) {
        if self.hide_at.is_some_and(|at| now >= at) {
            self.hide_at = None;
            self.reveal.animate_to(
                0.0,
                now,
                motion.duration(crate::anim::PANEL_CLOSE),
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
const LABEL: &str = "Volume";

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

/// Draw the slider for `output` at `density` pixels per logical one.
pub(crate) fn render(volume: &Volume, text: &mut Text, output: Rect, density: u32) -> Panel {
    Panel::from_canvas(&compose(volume, text, output, density), density)
}

fn compose(volume: &Volume, text: &mut Text, output: Rect, density: u32) -> Canvas {
    // In the canvas's own pixels, `density` times the logical ones. See
    // `Panel::from_canvas`.
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density.max(1) as f32;
    let size = BASE_SIZE * scale;
    let pad = PAD * scale;
    let width = (WIDTH * scale) as usize;
    let height = (HEIGHT * scale) as usize;
    let level = volume.level();

    let mut canvas = Canvas::new(width, height);
    canvas.fill_rounded(
        0,
        0,
        width,
        height,
        12.0 * scale,
        crate::theme::BACKGROUND.with_alpha(ALPHA),
    );

    // The label on the left, the reading on the right, and the track between
    // the two on the line below.
    let text_y = pad * 0.55;
    let label_color = if volume.is_real() {
        crate::theme::TEXT
    } else {
        crate::theme::TEXT_DIM
    };
    text.draw(
        &mut canvas,
        LABEL,
        size,
        pad as i32,
        text_y as i32,
        label_color,
    );
    let caption = if volume.is_real() {
        level.caption()
    } else {
        format!("{} · not connected", level.caption())
    };
    let caption_w = text.measure(&caption, size).0;
    text.draw(
        &mut canvas,
        &caption,
        size,
        (width as f32 - pad - caption_w) as i32,
        text_y as i32,
        if level.muted {
            crate::theme::TEXT_DIM
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
        crate::theme::BORDER,
    );
    let filled = (track_w * level.fraction()).round();
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
        if level.muted {
            crate::theme::TEXT_DIM
        } else {
            crate::theme::TEXT
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

    /// A mixer that remembers what it was last told, so a test can see what
    /// would have reached the speakers.
    #[derive(Debug)]
    struct Recording {
        level: RefCell<Level>,
        applied: RefCell<Vec<Level>>,
    }

    impl Recording {
        fn at(percent: u32) -> Self {
            Self {
                level: RefCell::new(Level {
                    percent,
                    muted: false,
                }),
                applied: RefCell::new(Vec::new()),
            }
        }
    }

    impl Mixer for Recording {
        fn read(&self) -> Option<Level> {
            Some(*self.level.borrow())
        }
        fn apply(&self, level: Level) {
            *self.level.borrow_mut() = level;
            self.applied.borrow_mut().push(level);
        }
    }

    fn at(percent: u32) -> Volume {
        Volume::with_mixer(Box::new(Recording::at(percent)))
    }

    #[test]
    fn wpctl_output_is_understood() {
        assert_eq!(
            Wpctl::parse("Volume: 0.40\n"),
            Some(Level {
                percent: 40,
                muted: false
            })
        );
        assert_eq!(
            Wpctl::parse("Volume: 0.65 [MUTED]\n"),
            Some(Level {
                percent: 65,
                muted: true
            })
        );
        assert_eq!(Wpctl::parse("Node not found"), None);
        // Over 100% is something PipeWire allows and this slider does not.
        assert_eq!(Wpctl::parse("Volume: 1.50").map(|l| l.percent), Some(100));
    }

    #[test]
    fn the_keys_step_by_five_and_stop_at_the_ends() {
        let mut volume = at(97);
        volume.press(Key::Raise, T0, Motion::Full);
        assert_eq!(volume.level().percent, 100);
        volume.press(Key::Raise, T0, Motion::Full);
        assert_eq!(volume.level().percent, 100, "it went past full");

        let mut volume = at(3);
        volume.press(Key::Lower, T0, Motion::Full);
        assert_eq!(volume.level().percent, 0);
        volume.press(Key::Lower, T0, Motion::Full);
        assert_eq!(volume.level().percent, 0, "it went below silent");
    }

    #[test]
    fn muting_keeps_the_level_for_when_it_comes_back() {
        let mut volume = at(60);
        volume.press(Key::ToggleMute, T0, Motion::Full);
        assert!(volume.level().muted);
        assert_eq!(volume.level().percent, 60);
        assert_eq!(
            volume.level().fraction(),
            0.0,
            "a muted slider shows silence"
        );
        volume.press(Key::ToggleMute, T0, Motion::Full);
        assert!(!volume.level().muted);
        assert_eq!(volume.level().percent, 60);
    }

    #[test]
    fn turning_it_up_unmutes() {
        let mut volume = at(60);
        volume.press(Key::ToggleMute, T0, Motion::Full);
        volume.press(Key::Raise, T0, Motion::Full);
        assert_eq!(
            volume.level(),
            Level {
                percent: 65,
                muted: false
            }
        );
    }

    #[test]
    fn every_change_reaches_the_mixer() {
        let mixer = Rc::new(Recording::at(50));
        // A `Box<dyn Mixer>` that forwards to the shared recording.
        #[derive(Debug)]
        struct Via(Rc<Recording>);
        impl Mixer for Via {
            fn read(&self) -> Option<Level> {
                self.0.read()
            }
            fn apply(&self, level: Level) {
                self.0.apply(level);
            }
        }
        let mut volume = Volume::with_mixer(Box::new(Via(mixer.clone())));
        volume.press(Key::Raise, T0, Motion::Full);
        volume.adjust(-10, T0, Motion::Full);
        volume.toggle_mute(T0, Motion::Full);
        let applied: Vec<_> = mixer
            .applied
            .borrow()
            .iter()
            .map(|l| (l.percent, l.muted))
            .collect();
        assert_eq!(applied, vec![(55, false), (45, false), (45, true)]);
    }

    #[test]
    fn a_key_steps_from_where_the_mixer_is_not_where_we_left_it() {
        let mixer = Rc::new(Recording::at(50));
        #[derive(Debug)]
        struct Via(Rc<Recording>);
        impl Mixer for Via {
            fn read(&self) -> Option<Level> {
                self.0.read()
            }
            fn apply(&self, level: Level) {
                self.0.apply(level);
            }
        }
        let mut volume = Volume::with_mixer(Box::new(Via(mixer.clone())));
        // Somebody else moved it.
        *mixer.level.borrow_mut() = Level {
            percent: 20,
            muted: false,
        };
        volume.press(Key::Raise, T0, Motion::Full);
        assert_eq!(volume.level().percent, 25);
    }

    #[test]
    fn a_mixer_that_comes_up_after_startup_is_noticed() {
        /// Answers nothing until told PipeWire is up.
        #[derive(Debug)]
        struct Late {
            up: RefCell<bool>,
            applied: RefCell<Vec<Level>>,
        }
        impl Mixer for Late {
            fn read(&self) -> Option<Level> {
                self.up.borrow().then_some(Level {
                    percent: 40,
                    muted: false,
                })
            }
            fn apply(&self, level: Level) {
                self.applied.borrow_mut().push(level);
            }
        }
        let late = Rc::new(Late {
            up: RefCell::new(false),
            applied: RefCell::new(Vec::new()),
        });
        #[derive(Debug)]
        struct Via(Rc<Late>);
        impl Mixer for Via {
            fn read(&self) -> Option<Level> {
                self.0.read()
            }
            fn apply(&self, level: Level) {
                self.0.apply(level);
            }
        }
        let mut volume = Volume::with_mixer(Box::new(Via(late.clone())));
        assert!(!volume.is_real(), "nothing answered at startup");

        *late.up.borrow_mut() = true;
        volume.press(Key::Raise, T0, Motion::Full);
        assert!(volume.is_real(), "the mixer came up and nobody noticed");
        assert_eq!(volume.level().percent, 45, "stepped from the mixer's level");
        assert_eq!(late.applied.borrow().last().map(|l| l.percent), Some(45));
    }

    #[test]
    fn without_a_mixer_it_still_moves_and_says_so() {
        let mut volume = Volume::default();
        assert!(!volume.is_real());
        let before = volume.level().percent;
        volume.press(Key::Raise, T0, Motion::Full);
        assert_eq!(volume.level().percent, before + STEP);
    }

    #[test]
    fn the_slider_appears_holds_and_fades() {
        let mut volume = at(50);
        assert!(!volume.is_visible(T0));
        volume.press(Key::Raise, T0, Motion::Full);
        assert!(volume.is_visible(T0));
        assert!(volume.reveal(T0) < 0.5, "it was already there at t=0");
        assert!((volume.reveal(ms(200)) - 1.0).abs() < 1e-3);

        // Held, well past the fade-in.
        volume.tick(ms(1000), Motion::Full);
        assert!(volume.is_visible(ms(1000)));
        assert!(volume.is_animating(ms(1000)), "frames must keep coming");

        // The hold ends, and it fades rather than vanishing.
        volume.tick(ms(1500), Motion::Full);
        assert!(volume.is_visible(ms(1500)));
        assert!(volume.reveal(ms(1550)) < 1.0);
        volume.tick(ms(1700), Motion::Full);
        assert!(!volume.is_visible(ms(1700)));
        assert!(!volume.is_animating(ms(1700)));
    }

    #[test]
    fn another_key_during_the_hold_keeps_it_up() {
        let mut volume = at(50);
        volume.press(Key::Raise, T0, Motion::Full);
        volume.press(Key::Raise, ms(1000), Motion::Full);
        volume.tick(ms(1600), Motion::Full);
        assert!(
            (volume.reveal(ms(1600)) - 1.0).abs() < 1e-3,
            "the second press did not restart the hold"
        );
    }

    #[test]
    fn reduced_motion_shows_it_at_once() {
        let mut volume = at(50);
        volume.press(Key::Raise, T0, Motion::Reduced);
        assert!((volume.reveal(T0) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn it_sits_at_the_bottom_centre_of_the_output() {
        let output = Rect::from_xywh(100, 50, 1920, 1080);
        let rect = placement(output, (300, 64));
        assert_eq!(rect.x(), 100 + (1920 - 300) / 2);
        assert_eq!(rect.y(), 50 + 1080 - 64 - FROM_BOTTOM);
    }

    /// `VOLUME_DUMP=/path/out.ppm cargo test volume_dump -- --nocapture`
    /// writes the slider at `VOLUME_AT` percent (default 65) to look at.
    #[test]
    fn volume_dump() {
        let Ok(path) = std::env::var("VOLUME_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let percent = std::env::var("VOLUME_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(65);
        let volume = at(percent);
        let canvas = compose(&volume, &mut text, Rect::from_xywh(0, 0, 1920, 1080), 1);
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
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
        let mut filled = |volume: &Volume| {
            let canvas = compose(volume, &mut text, output, 1);
            let row = (HEIGHT - PAD - TRACK / 2.0) as usize;
            (0..canvas.stride)
                .filter(|col| {
                    let offset = (row * canvas.stride + col) * 4;
                    canvas.pixels[offset..offset + 3] == accent[..3]
                })
                .count()
        };
        let quiet = filled(&at(20));
        let loud = filled(&at(80));
        assert!(quiet > 0, "a 20% slider drew no fill at all");
        assert!(
            loud > quiet * 3,
            "80% ({loud}px) should be about four times 20% ({quiet}px)"
        );

        let mut muted = at(80);
        muted.toggle_mute(T0, Motion::Full);
        assert_eq!(filled(&muted), 0, "a muted slider drew a fill");
    }
}
