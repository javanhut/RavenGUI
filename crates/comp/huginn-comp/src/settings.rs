//! Quick settings: the small panel of things a desktop needs to change often.
//!
//! Compositor-drawn like the launcher, and for the same reason — it must never
//! be the thing that failed to appear. Deliberately small: §1's non-goals rule
//! out matching GNOME's settings surface area, so this is audio, network,
//! brightness, and the handful of switches that change how the desktop behaves.
//!
//! # Backends
//!
//! Wi-Fi and Bluetooth talk to NetworkManager and BlueZ over D-Bus, which is a
//! large dependency surface and hardware to test against. Each control sits
//! behind a trait here with a fake implementation, so the panel can be built
//! and judged before any of that exists — and so the real ones swap in without
//! the UI changing. What is fake says so on screen rather than pretending.

use std::time::Duration;

use huginn_core::geometry::Rect;

use crate::anim::{Animated, Curve};
use crate::canvas::{Canvas, Panel};
use crate::text::Text;

/// Whether the desktop animates.
///
/// Runtime state, not a setting read from disk — there is no configuration
/// file and §11 says there will not be. Reduced motion is here because it is
/// an accessibility need rather than a preference: vestibular disorders make
/// large sliding transitions genuinely unpleasant, and "opinionated" is a
/// statement about taste, not about who gets to use the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Motion {
    #[default]
    Full,
    Reduced,
}

impl Motion {
    /// The duration to actually use for an animation nominally `wanted` long.
    ///
    /// Zero under reduced motion, which every animation already handles: a
    /// zero-length [`Animated`] is simply already finished. That is why there
    /// is no `if reduced { skip }` at any call site — the one place motion is
    /// turned off is here.
    pub(crate) fn duration(self, wanted: Duration) -> Duration {
        match self {
            Self::Full => wanted,
            Self::Reduced => Duration::ZERO,
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Full => Self::Reduced,
            Self::Reduced => Self::Full,
        }
    }
}

/// A control's current reading, and whether it is real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    /// What to show on the right of the row.
    pub value: String,
    /// False when this came from a stub rather than from the system.
    ///
    /// Shown, not hidden. A panel that displays invented Wi-Fi networks
    /// indistinguishable from real ones is worse than one that admits it is
    /// not wired up yet — the second is unfinished, the first is wrong.
    pub real: bool,
}

/// Something the panel can read and toggle.
pub(crate) trait Control: std::fmt::Debug {
    /// The row's label.
    fn label(&self) -> &str;
    /// What to show, and whether it is real.
    fn read(&self) -> Reading;
    /// Act on the row. Returns whether anything changed.
    fn activate(&mut self) -> bool;
}

/// Brightness, as a percentage.
///
/// A stub: the real implementation writes `/sys/class/backlight/*/brightness`
/// or asks logind, and needs the session to own the device.
#[derive(Debug)]
struct Brightness {
    percent: u32,
}

impl Control for Brightness {
    fn label(&self) -> &str {
        "Brightness"
    }
    fn read(&self) -> Reading {
        Reading {
            value: format!("{}%", self.percent),
            real: false,
        }
    }
    fn activate(&mut self) -> bool {
        // Steps rather than a slider until there is pointer input to drag one.
        self.percent = match self.percent {
            0..=24 => 25,
            25..=49 => 50,
            50..=74 => 75,
            75..=99 => 100,
            _ => 0,
        };
        true
    }
}

/// Wi-Fi. A stub until NetworkManager is wired up over D-Bus.
#[derive(Debug)]
struct WiFi {
    on: bool,
}

impl Control for WiFi {
    fn label(&self) -> &str {
        "Wi-Fi"
    }
    fn read(&self) -> Reading {
        Reading {
            value: if self.on { "On" } else { "Off" }.to_owned(),
            real: false,
        }
    }
    fn activate(&mut self) -> bool {
        self.on = !self.on;
        true
    }
}

/// Bluetooth. A stub until BlueZ is wired up over D-Bus.
#[derive(Debug)]
struct Bluetooth {
    on: bool,
}

impl Control for Bluetooth {
    fn label(&self) -> &str {
        "Bluetooth"
    }
    fn read(&self) -> Reading {
        Reading {
            value: if self.on { "On" } else { "Off" }.to_owned(),
            real: false,
        }
    }
    fn activate(&mut self) -> bool {
        self.on = !self.on;
        true
    }
}

/// The animations switch. The one control here that is genuinely wired up.
#[derive(Debug)]
struct Animations {
    motion: Motion,
}

impl Control for Animations {
    fn label(&self) -> &str {
        "Animations"
    }
    fn read(&self) -> Reading {
        Reading {
            value: match self.motion {
                Motion::Full => "On",
                Motion::Reduced => "Reduced",
            }
            .to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.motion = self.motion.toggled();
        true
    }
}

/// What a keystroke means to the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    /// Toggle or step the highlighted control.
    Activate,
    Dismiss,
    Ignored,
}

impl Key {
    pub(crate) fn from_keysym(sym: u32) -> Self {
        use smithay::input::keyboard::keysyms;
        match sym {
            keysyms::KEY_Escape => Self::Dismiss,
            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_K => Self::Up,
            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_J => Self::Down,
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => Self::Activate,
            _ => Self::Ignored,
        }
    }
}

/// What the compositor should do after a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Unchanged,
    Redraw,
    Dismissed,
}

/// The quick settings panel.
#[derive(Debug)]
pub(crate) struct Settings {
    open: bool,
    selected: usize,
    controls: Vec<Box<dyn Control>>,
    /// 0 closed, 1 open. Drives the reveal.
    reveal: Animated,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            open: false,
            selected: 0,
            controls: vec![
                Box::new(Animations {
                    motion: Motion::default(),
                }),
                Box::new(Brightness { percent: 75 }),
                Box::new(WiFi { on: true }),
                Box::new(Bluetooth { on: false }),
            ],
            reveal: Animated::settled(0.0),
        }
    }
}

impl Settings {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// How far the panel has slid in, 0..=1.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
        self.reveal.value(now)
    }

    /// Whether the reveal is still moving, so the frame loop keeps going.
    pub(crate) fn is_animating(&self, now: Duration) -> bool {
        !self.reveal.is_settled(now)
    }

    /// Whether the panel is on screen at all, mid-animation included.
    pub(crate) fn is_visible(&self, now: Duration) -> bool {
        self.open || self.reveal(now) > 0.001
    }

    /// The current motion setting, read from the control that owns it.
    pub(crate) fn motion(&self) -> Motion {
        self.controls
            .iter()
            .find_map(|c| match c.read().value.as_str() {
                _ if c.label() != "Animations" => None,
                "Reduced" => Some(Motion::Reduced),
                _ => Some(Motion::Full),
            })
            .unwrap_or_default()
    }

    pub(crate) fn open(&mut self, now: Duration) {
        self.open = true;
        self.selected = 0;
        let motion = self.motion();
        self.reveal.animate_to(
            1.0,
            now,
            motion.duration(crate::anim::LAUNCHER_OPEN),
            Curve::Spring,
        );
    }

    pub(crate) fn close(&mut self, now: Duration) {
        self.open = false;
        let motion = self.motion();
        self.reveal.animate_to(
            0.0,
            now,
            motion.duration(crate::anim::PANEL_CLOSE),
            Curve::EaseOut,
        );
    }

    pub(crate) fn press(&mut self, key: Key, now: Duration) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        match key {
            Key::Dismiss => {
                self.close(now);
                Outcome::Dismissed
            }
            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::Activate => {
                let Some(control) = self.controls.get_mut(self.selected) else {
                    return Outcome::Unchanged;
                };
                if !control.activate() {
                    return Outcome::Unchanged;
                }
                // Turning animations off must take effect on the panel that
                // turned them off, not on the next thing to animate — landing
                // the change one interaction late reads as the switch having
                // done nothing.
                if self.motion() == Motion::Reduced {
                    self.reveal.jump_to(1.0);
                }
                Outcome::Redraw
            }
            Key::Ignored => Outcome::Unchanged,
        }
    }

    fn move_selection(&mut self, delta: isize) -> Outcome {
        if self.controls.is_empty() {
            return Outcome::Unchanged;
        }
        let last = self.controls.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        if next == self.selected {
            return Outcome::Unchanged;
        }
        self.selected = next;
        Outcome::Redraw
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

const WIDTH: f32 = 340.0;
const PAD: f32 = 16.0;
const BASE_SIZE: f32 = 15.0;
const ALPHA: u8 = 0xF2;
const TITLE: &str = "Quick settings";
/// Marks a control that is not wired to anything yet.
const STUB: &str = "not connected";

/// Draw the panel for `output` at `density` pixels per logical one.
pub(crate) fn render(
    settings: &Settings,
    text: &mut Text,
    output: Rect,
    now: Duration,
    density: u32,
) -> Panel {
    Panel::from_canvas(&compose(settings, text, output, now, density), density)
}

fn compose(settings: &Settings, text: &mut Text, output: Rect, now: Duration, density: u32) -> Canvas {
    // In the canvas's own pixels, `density` times the logical ones. See
    // `Panel::from_canvas`.
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density.max(1) as f32;
    let size = BASE_SIZE * scale;
    let pad = PAD * scale;
    let width = (WIDTH * scale) as usize;
    let row = size * 2.4;
    let header = size * 2.2;

    let height = (pad * 2.0 + header + row * settings.controls.len() as f32) as usize;
    let mut canvas = Canvas::new(width, height.max(1));
    canvas.fill(
        0,
        0,
        width,
        height,
        crate::theme::BACKGROUND.with_alpha(ALPHA).to_rgba_bytes(),
    );
    canvas.frame(crate::theme::BORDER.to_rgba_bytes());

    text.draw(&mut canvas, TITLE, size, pad as i32, pad as i32, crate::theme::ACCENT);
    canvas.fill(
        pad as usize,
        (pad + header - size * 0.6) as usize,
        width - (pad * 2.0) as usize,
        1,
        crate::theme::BORDER.to_rgba_bytes(),
    );

    let mut y = pad + header;
    for (index, control) in settings.controls.iter().enumerate() {
        let reading = control.read();
        let highlighted = index == settings.selected;
        if highlighted {
            canvas.tint(1, y as usize, width - 2, row as usize, crate::theme::ACCENT, 0x2E);
            canvas.fill(
                1,
                y as usize,
                (3.0 * scale) as usize,
                row as usize,
                crate::theme::ACCENT.to_rgba_bytes(),
            );
        }

        let text_y = (y + (row - size * 1.35) / 2.0) as i32;
        text.draw(
            &mut canvas,
            control.label(),
            size,
            (pad + 6.0 * scale) as i32,
            text_y,
            if highlighted { crate::theme::TEXT } else { crate::theme::TEXT_DIM },
        );

        // The value, right-aligned. A stub says so instead of showing a
        // plausible reading nobody can tell is invented.
        let shown = if reading.real {
            reading.value.clone()
        } else {
            format!("{} · {STUB}", reading.value)
        };
        let value_w = text.measure(&shown, size * 0.95).0;
        text.draw(
            &mut canvas,
            &shown,
            size * 0.95,
            (width as f32 - pad - value_w) as i32,
            text_y,
            if reading.real { crate::theme::ACCENT } else { crate::theme::TEXT_DIM },
        );
        y += row;
    }

    // The reveal: fade the whole panel. Sliding it as well needs the dock it
    // slides out of, which does not exist yet.
    let reveal = settings.reveal(now).clamp(0.0, 1.0);
    if reveal < 1.0 {
        for pixel in canvas.pixels.chunks_exact_mut(4) {
            pixel[3] = (f32::from(pixel[3]) * reveal) as u8;
        }
    }

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn opened() -> Settings {
        let mut settings = Settings::default();
        settings.open(T0);
        settings
    }

    #[test]
    fn opening_reveals_over_time_rather_than_appearing() {
        let settings = opened();
        assert!(settings.reveal(T0) < 0.5, "it was already there at t=0");
        assert!(settings.is_animating(T0));
        assert!((settings.reveal(ms(200)) - 1.0).abs() < 1e-3);
        assert!(!settings.is_animating(ms(200)));
    }

    #[test]
    fn closing_animates_out_and_then_stops_being_visible() {
        let mut settings = opened();
        assert!(settings.is_visible(ms(200)));
        settings.close(ms(200));
        assert!(settings.is_visible(ms(200)), "it vanished instead of closing");
        assert!(!settings.is_visible(ms(500)));
    }

    #[test]
    fn the_animations_switch_is_the_one_control_that_is_real() {
        // Everything else is a stub and must say so, or the panel shows
        // invented readings indistinguishable from measurements.
        let settings = Settings::default();
        let real: Vec<&str> = settings
            .controls
            .iter()
            .filter(|c| c.read().real)
            .map(|c| c.label())
            .collect();
        assert_eq!(real, ["Animations"]);
    }

    #[test]
    fn turning_animations_off_takes_effect_immediately() {
        // Landing the change on the *next* interaction reads as the switch
        // having done nothing at all.
        let mut settings = opened();
        assert_eq!(settings.motion(), Motion::Full);
        assert_eq!(settings.press(Key::Activate, ms(10)), Outcome::Redraw);
        assert_eq!(settings.motion(), Motion::Reduced);
        assert!(
            (settings.reveal(ms(10)) - 1.0).abs() < 1e-5,
            "the panel was still mid-animation after motion was reduced"
        );
        assert!(!settings.is_animating(ms(10)));
    }

    #[test]
    fn reduced_motion_makes_the_next_reveal_instant() {
        let mut settings = opened();
        settings.press(Key::Activate, ms(10));
        settings.close(ms(20));
        settings.open(ms(30));
        assert_eq!(settings.reveal(ms(30)), 1.0, "it animated despite reduced motion");
    }

    #[test]
    fn reduced_motion_turns_a_duration_into_nothing() {
        assert_eq!(Motion::Full.duration(ms(150)), ms(150));
        assert_eq!(Motion::Reduced.duration(ms(150)), Duration::ZERO);
    }

    #[test]
    fn the_selection_moves_and_stops_at_the_ends() {
        let mut settings = opened();
        assert_eq!(settings.press(Key::Up, T0), Outcome::Unchanged);
        assert_eq!(settings.press(Key::Down, T0), Outcome::Redraw);
        for _ in 0..20 {
            settings.press(Key::Down, T0);
        }
        assert_eq!(settings.selected, settings.controls.len() - 1);
        assert_eq!(settings.press(Key::Down, T0), Outcome::Unchanged);
    }

    #[test]
    fn activating_changes_only_the_highlighted_control() {
        let mut settings = opened();
        settings.press(Key::Down, T0); // Brightness
        let before: Vec<String> = settings.controls.iter().map(|c| c.read().value).collect();
        settings.press(Key::Activate, T0);
        let after: Vec<String> = settings.controls.iter().map(|c| c.read().value).collect();
        let changed: Vec<usize> = (0..before.len()).filter(|i| before[*i] != after[*i]).collect();
        assert_eq!(changed, [1], "activation touched {changed:?}");
    }

    #[test]
    fn brightness_wraps_rather_than_sticking_at_full() {
        let mut settings = opened();
        settings.press(Key::Down, T0);
        let mut seen = Vec::new();
        for _ in 0..6 {
            settings.press(Key::Activate, T0);
            seen.push(settings.controls[1].read().value.clone());
        }
        assert!(seen.contains(&"100%".to_owned()));
        assert!(seen.contains(&"0%".to_owned()), "it stuck at full: {seen:?}");
    }

    #[test]
    fn escape_closes_it() {
        let mut settings = opened();
        assert_eq!(settings.press(Key::Dismiss, T0), Outcome::Dismissed);
        assert!(!settings.is_open());
    }

    #[test]
    fn a_closed_panel_ignores_every_key() {
        let mut settings = Settings::default();
        for key in [Key::Up, Key::Down, Key::Activate, Key::Dismiss] {
            assert_eq!(settings.press(key, T0), Outcome::Unchanged);
        }
    }

    #[test]
    fn every_stub_is_labelled_on_screen() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let settings = opened();
        // Composing at full reveal so the alpha pass does not hide anything.
        let canvas = compose(&settings, &mut text, Rect::from_xywh(0, 0, 1920, 1080), ms(500), 1);
        assert!(canvas.height > 0);
        // The rows that are stubs draw a longer value string than the real one,
        // so the panel must be wide enough for it rather than clipping.
        let widest = settings
            .controls
            .iter()
            .filter(|c| !c.read().real)
            .map(|c| text.measure(&format!("{} · {STUB}", c.read().value), BASE_SIZE * 0.95).0)
            .fold(0.0_f32, f32::max);
        assert!(
            canvas.stride as f32 > widest + PAD * 2.0,
            "the panel is {} wide but a stub label needs {}",
            canvas.stride,
            widest + PAD * 2.0
        );
    }

    #[test]
    fn a_partly_revealed_panel_is_partly_transparent() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let settings = opened();
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let opaque = compose(&settings, &mut text, output, ms(500), 1);
        let faded = compose(&settings, &mut text, output, ms(20), 1);
        let alpha = |c: &Canvas| c.pixels.chunks_exact(4).map(|p| u32::from(p[3])).sum::<u32>();
        assert!(alpha(&faded) < alpha(&opaque), "the reveal did not fade the panel");
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// `SETTINGS_DUMP=/tmp/s.ppm SETTINGS_DOWN=1 cargo test -p huginn-comp settings_dump`
    #[test]
    fn settings_dump() {
        let Ok(path) = std::env::var("SETTINGS_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let mut settings = Settings::default();
        settings.open(Duration::ZERO);
        for _ in 0..std::env::var("SETTINGS_DOWN")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            settings.press(Key::Down, Duration::ZERO);
        }
        let at = Duration::from_millis(
            std::env::var("SETTINGS_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(500),
        );
        let canvas = compose(&settings, &mut text, Rect::from_xywh(0, 0, 1920, 1080), at, 1);
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
