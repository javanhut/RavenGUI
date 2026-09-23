//! Screen brightness: the brightness keys, the backlight they drive, and the
//! slider that shows what they did.
//!
//! The twin of [`crate::audio`], and for the same reason in one place: the
//! keymap resolves `XF86MonBrightnessUp` and `XF86MonBrightnessDown` to a
//! [`Key`], the quick settings panel has a row that steps the same level with
//! the arrows, and the on-screen slider is what both of them draw. One
//! [`Brightness`] holds the level, and everything that shows or changes it
//! goes through that.
//!
//! # The backlight
//!
//! Written straight to `/sys/class/backlight/<device>/brightness`. There is
//! no logind on Raven to ask — the seat is seatd's — and the kernel's own
//! file is the interface every brightness tool ends up at anyway. It is
//! root's by default; `data/90-backlight.rules` hands it to group `video`,
//! which the session already holds for DRM. Without that rule the read works
//! and the write does not, and the slider says "not connected" rather than
//! pretending, with the reason in the log.
//!
//! The device is looked for on every press, not once at startup, for the
//! reason the mixer is: a backlight whose driver loads a moment after the
//! compositor would otherwise be missing for the whole session. It is a
//! directory listing and two small reads, once per key press.
//!
//! # Steps
//!
//! The keys move in [`STEP`] percent steps on a grid — 23% goes up to 25%,
//! not 28% — so a few presses land somewhere round. Two things stop that
//! from being the whole story:
//!
//! - The keys never go below [`FLOOR`]. On a `raw` backlight zero is the
//!   light off, and a brightness key that blanks the screen leaves somebody
//!   pressing keys at a black panel to find out whether the machine hung.
//! - A coarse device — some firmware backlights have fifteen levels — can
//!   round a 5% step to the level it is already at. When that happens the
//!   step moves one raw unit instead, so every press does something.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::osd::{Flash, Slider};
use crate::settings::Motion;

/// One key's worth of change, as a percentage.
pub(crate) const STEP: u32 = 5;

/// The dimmest the keys go, as a percentage. Not zero; see the module docs.
const FLOOR: u32 = 1;

/// Where the kernel lists backlights.
const SYSFS: &str = "/sys/class/backlight";

/// What the brightness keys mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Raise,
    Lower,
}

/// The backlight's level, in the device's own units.
///
/// Kept raw rather than as a percentage, because the device is what has the
/// final say on which levels exist: a percentage that is rounded to fifteen
/// levels and back is not the percentage that went in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Level {
    pub raw: u64,
    /// Never zero: [`Level::new`] refuses a device that reports it.
    pub max: u64,
}

impl Level {
    pub(crate) fn new(raw: u64, max: u64) -> Option<Self> {
        (max > 0).then(|| Self {
            raw: raw.min(max),
            max,
        })
    }

    pub(crate) fn percent(self) -> u32 {
        ((self.raw * 100 + self.max / 2) / self.max) as u32
    }

    /// The level as a fraction of full, for a slider.
    pub(crate) fn fraction(self) -> f32 {
        self.raw as f32 / self.max as f32
    }

    /// What to write next to the slider.
    pub(crate) fn caption(self) -> String {
        format!("{}%", self.percent())
    }

    fn raw_for(self, percent: u32) -> u64 {
        (u64::from(percent) * self.max + 50) / 100
    }

    /// The dimmest raw level the keys will set: [`FLOOR`], and never off.
    fn lowest(self) -> u64 {
        self.raw_for(FLOOR).clamp(1, self.max)
    }

    /// The level one key press away.
    fn step(self, key: Key) -> Self {
        let percent = self.percent();
        let target = match key {
            Key::Raise => (percent / STEP + 1) * STEP,
            Key::Lower => percent.div_ceil(STEP).saturating_sub(1) * STEP,
        };
        let mut raw = self.raw_for(target.min(100)).clamp(self.lowest(), self.max);
        // A coarse device can round the step to where it already is.
        if raw == self.raw {
            raw = match key {
                Key::Raise => (self.raw + 1).min(self.max),
                Key::Lower => self.raw.saturating_sub(1).max(self.lowest()),
            };
        }
        // And a key never moves the wrong way: Lower on a backlight somebody
        // else turned right off does not brighten it to the floor.
        let raw = match key {
            Key::Raise => raw.max(self.raw),
            Key::Lower => raw.min(self.raw),
        };
        Self { raw, ..self }
    }
}

/// Something that can tell us the backlight level and set it.
///
/// A trait for the reason [`crate::audio::Mixer`] is one: the real one needs
/// a laptop panel on the machine running the tests.
pub(crate) trait Device: std::fmt::Debug {
    /// The level right now, or `None` if there is no backlight to ask.
    fn read(&self) -> Option<Level>;
    /// Set the raw level. Returns whether it landed.
    fn apply(&self, raw: u64) -> bool;
}

/// No backlight: a desktop monitor, a virtual machine, a test.
#[derive(Debug, Default)]
pub(crate) struct Absent;

impl Device for Absent {
    fn read(&self) -> Option<Level> {
        None
    }
    fn apply(&self, _: u64) -> bool {
        false
    }
}

/// The kernel's backlight class, under `root`.
#[derive(Debug)]
pub(crate) struct Sysfs {
    root: PathBuf,
    /// Whether a failed write has been explained in the log yet. Once is
    /// enough; every key press after it fails for the same reason.
    warned: Cell<bool>,
}

impl Sysfs {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            warned: Cell::new(false),
        }
    }

    /// The backlight to drive, if there is one.
    fn device(&self) -> Option<PathBuf> {
        let found = std::fs::read_dir(&self.root)
            .ok()?
            .flatten()
            .map(|entry| {
                let path = entry.path();
                let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
                (path, kind.trim().to_owned())
            })
            .collect();
        choose(found)
    }
}

/// Which of several backlights is the panel's.
///
/// `firmware` first, then `platform`, then `raw`: the order the kernel's own
/// documentation gives, and the one every desktop uses. A firmware interface
/// knows about the panel it is wired to; a raw one is the GPU's register,
/// which on a machine with two GPUs may not be the one lighting the screen.
/// Ties go to the first name, so the choice is the same on every boot.
fn choose(mut found: Vec<(PathBuf, String)>) -> Option<PathBuf> {
    let rank = |kind: &str| match kind {
        "firmware" => 0,
        "platform" => 1,
        "raw" => 2,
        _ => 3,
    };
    found.sort_by(|a, b| rank(&a.1).cmp(&rank(&b.1)).then_with(|| a.0.cmp(&b.0)));
    found.into_iter().next().map(|(path, _)| path)
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl Device for Sysfs {
    fn read(&self) -> Option<Level> {
        let device = self.device()?;
        Level::new(
            read_number(&device.join("brightness"))?,
            read_number(&device.join("max_brightness"))?,
        )
    }

    fn apply(&self, raw: u64) -> bool {
        let Some(device) = self.device() else {
            return false;
        };
        let path = device.join("brightness");
        match std::fs::write(&path, raw.to_string()) {
            Ok(()) => true,
            Err(e) => {
                if !self.warned.replace(true) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "brightness: cannot write the backlight; the session needs \
                         group video and data/90-backlight.rules installed"
                    );
                }
                false
            }
        }
    }
}

/// The screen brightness, and the slider that shows it.
#[derive(Debug)]
pub(crate) struct Brightness {
    level: Level,
    /// Whether `level` came from a backlight that took the last write, or is
    /// a number being shown to nobody.
    real: bool,
    device: Box<dyn Device>,
    flash: Flash,
}

/// The brightness as the compositor and quick settings both hold it.
/// Shared for the reason [`crate::audio::Shared`] is.
pub(crate) type Shared = Rc<RefCell<Brightness>>;

impl Default for Brightness {
    /// A brightness with nothing behind it. What tests use, and what the
    /// settings panel gets when it is built on its own.
    fn default() -> Self {
        Self::with_device(Box::new(Absent))
    }
}

impl Brightness {
    /// The machine's backlight, whether or not one is there yet.
    pub(crate) fn detect() -> Self {
        let brightness = Self::with_device(Box::new(Sysfs::new(SYSFS)));
        if brightness.real {
            tracing::info!(level = ?brightness.level, "brightness: {SYSFS}");
        } else {
            tracing::info!("brightness: no backlight in {SYSFS}; will look again on a key");
        }
        brightness
    }

    pub(crate) fn with_device(device: Box<dyn Device>) -> Self {
        let level = device.read();
        Self {
            level: level.unwrap_or(Level { raw: 75, max: 100 }),
            real: level.is_some(),
            device,
            flash: Flash::default(),
        }
    }

    pub(crate) fn shared(self) -> Shared {
        Rc::new(RefCell::new(self))
    }

    pub(crate) fn level(&self) -> Level {
        self.level
    }

    /// Whether the level is the backlight's, rather than a number with
    /// nothing behind it.
    pub(crate) fn is_real(&self) -> bool {
        self.real
    }

    /// Act on a brightness key, and show the slider.
    pub(crate) fn press(&mut self, key: Key, now: Duration, motion: Motion) {
        self.adjust(if key == Key::Raise { 1 } else { -1 }, now, motion);
    }

    /// Step the level by `steps` key presses. What the settings row does.
    ///
    /// Re-reads the backlight first, so a level moved from elsewhere — a
    /// firmware hotkey, another program — is stepped from where it actually
    /// is rather than from where this last left it.
    pub(crate) fn adjust(&mut self, steps: i32, now: Duration, motion: Motion) {
        self.sync();
        let key = if steps > 0 { Key::Raise } else { Key::Lower };
        let mut level = self.level;
        for _ in 0..steps.unsigned_abs() {
            level = level.step(key);
        }
        self.level = level;
        self.real = self.device.apply(level.raw);
        self.flash.show(now, motion);
    }

    fn sync(&mut self) {
        match self.device.read() {
            Some(level) => {
                if !self.real {
                    tracing::info!(?level, "brightness: backlight connected");
                }
                self.level = level;
                self.real = true;
            }
            None => self.real = false,
        }
    }

    /// Take the slider off screen at once, for another taking its place.
    pub(crate) fn dismiss(&mut self) {
        self.flash.dismiss();
    }

    /// When the slider's hold ends, while it is held. See [`Flash::held_until`].
    pub(crate) fn held_until(&self) -> Option<Duration> {
        self.flash.held_until()
    }

    /// Start the fade once the hold is over. Called once per frame.
    pub(crate) fn tick(&mut self, now: Duration, motion: Motion) {
        self.flash.tick(now, motion);
    }

    /// How far the slider has faded in, 0..=1.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
        self.flash.reveal(now)
    }

    /// Whether the slider is on screen at all, held or fading.
    pub(crate) fn is_visible(&self, now: Duration) -> bool {
        self.flash.is_visible(now)
    }

    /// Whether the frame loop has to keep going for the slider's sake.
    pub(crate) fn is_animating(&self, now: Duration) -> bool {
        self.flash.is_animating(now)
    }

    /// What the slider shows.
    pub(crate) fn slider(&self) -> Slider {
        Slider {
            label: "Brightness",
            caption: self.level.caption(),
            fraction: self.level.fraction(),
            real: self.real,
            dim: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    /// A backlight that remembers what it was last told.
    #[derive(Debug)]
    struct Panel {
        level: Cell<Level>,
        accepts: bool,
    }

    impl Panel {
        fn at(raw: u64, max: u64) -> Rc<Self> {
            Rc::new(Self {
                level: Cell::new(Level { raw, max }),
                accepts: true,
            })
        }
    }

    #[derive(Debug)]
    struct Via(Rc<Panel>);

    impl Device for Via {
        fn read(&self) -> Option<Level> {
            Some(self.0.level.get())
        }
        fn apply(&self, raw: u64) -> bool {
            if self.0.accepts {
                self.0.level.set(Level {
                    raw,
                    ..self.0.level.get()
                });
            }
            self.0.accepts
        }
    }

    fn on(panel: &Rc<Panel>) -> Brightness {
        Brightness::with_device(Box::new(Via(panel.clone())))
    }

    fn percents(max: u64, from: u64, key: Key, presses: usize) -> Vec<u32> {
        let panel = Panel::at(from, max);
        let mut brightness = on(&panel);
        (0..presses)
            .map(|_| {
                brightness.press(key, T0, Motion::Full);
                panel.level.get().percent()
            })
            .collect()
    }

    #[test]
    fn the_keys_step_by_five_on_the_grid() {
        // 23% is off the grid; one press lands on it either way.
        assert_eq!(percents(100, 23, Key::Raise, 3), [25, 30, 35]);
        assert_eq!(percents(100, 23, Key::Lower, 3), [20, 15, 10]);
    }

    #[test]
    fn the_keys_stop_at_full_and_never_turn_the_light_off() {
        assert_eq!(percents(100, 97, Key::Raise, 2), [100, 100]);
        let panel = Panel::at(96000 * 6 / 100, 96000);
        let mut brightness = on(&panel);
        for _ in 0..5 {
            brightness.press(Key::Lower, T0, Motion::Full);
        }
        assert_eq!(panel.level.get().percent(), FLOOR);
        assert!(panel.level.get().raw > 0, "the keys turned the panel off");
    }

    #[test]
    fn a_coarse_backlight_moves_on_every_press() {
        // Fifteen levels: 5% of that rounds to nothing, so a plain percentage
        // step would leave some presses doing nothing at all.
        let panel = Panel::at(1, 15);
        let mut brightness = on(&panel);
        let mut seen = vec![panel.level.get().raw];
        while panel.level.get().raw < 15 {
            brightness.press(Key::Raise, T0, Motion::Full);
            let raw = panel.level.get().raw;
            assert!(raw > *seen.last().unwrap(), "stuck at {raw}/15: {seen:?}");
            seen.push(raw);
        }
        while panel.level.get().raw > 1 {
            brightness.press(Key::Lower, T0, Motion::Full);
            let raw = panel.level.get().raw;
            assert!(raw < *seen.last().unwrap(), "stuck at {raw}/15: {seen:?}");
            seen.push(raw);
        }
        brightness.press(Key::Lower, T0, Motion::Full);
        assert_eq!(panel.level.get().raw, 1, "the floor let it go dark");
    }

    #[test]
    fn lower_never_brightens_a_backlight_that_was_off() {
        assert_eq!(percents(100, 0, Key::Lower, 1), [0]);
    }

    #[test]
    fn a_key_steps_from_where_the_backlight_is() {
        let panel = Panel::at(50, 100);
        let mut brightness = on(&panel);
        // A firmware hotkey moved it behind our back.
        panel.level.set(Level { raw: 20, max: 100 });
        brightness.press(Key::Raise, T0, Motion::Full);
        assert_eq!(panel.level.get().percent(), 25);
    }

    #[test]
    fn the_settings_row_steps_like_the_keys() {
        let panel = Panel::at(50, 100);
        let mut brightness = on(&panel);
        brightness.adjust(2, T0, Motion::Full);
        assert_eq!(panel.level.get().percent(), 60);
        brightness.adjust(-1, T0, Motion::Full);
        assert_eq!(panel.level.get().percent(), 55);
    }

    #[test]
    fn a_write_that_is_refused_says_not_connected() {
        let panel = Rc::new(Panel {
            level: Cell::new(Level { raw: 50, max: 100 }),
            accepts: false,
        });
        let mut brightness = on(&panel);
        assert!(brightness.is_real(), "the read worked");
        brightness.press(Key::Raise, T0, Motion::Full);
        assert!(
            !brightness.is_real(),
            "the write failed and it said nothing"
        );
        assert!(!brightness.slider().real);
    }

    #[test]
    fn without_a_backlight_it_still_moves_and_says_so() {
        let mut brightness = Brightness::default();
        assert!(!brightness.is_real());
        brightness.press(Key::Raise, T0, Motion::Full);
        assert_eq!(brightness.level().percent(), 80);
        assert!(brightness.is_visible(T0), "no slider for the key");
    }

    #[test]
    fn a_device_reporting_no_range_is_no_device() {
        assert_eq!(Level::new(5, 0), None);
        assert_eq!(Level::new(500, 100), Some(Level { raw: 100, max: 100 }));
    }

    #[test]
    fn firmware_beats_platform_beats_raw() {
        let found = |kinds: &[(&str, &str)]| {
            choose(
                kinds
                    .iter()
                    .map(|(name, kind)| (PathBuf::from(name), kind.to_string()))
                    .collect(),
            )
        };
        assert_eq!(
            found(&[("intel_backlight", "raw"), ("acpi_video0", "firmware")]),
            Some(PathBuf::from("acpi_video0"))
        );
        assert_eq!(
            found(&[("intel_backlight", "raw"), ("dell_backlight", "platform")]),
            Some(PathBuf::from("dell_backlight"))
        );
        assert_eq!(
            found(&[("nv_backlight", "raw"), ("amdgpu_bl0", "raw")]),
            Some(PathBuf::from("amdgpu_bl0")),
            "a tie did not go to the first name"
        );
        assert_eq!(found(&[]), None);
    }

    /// A `/sys/class/backlight` in a temporary directory.
    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("huginn-backlight-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn add(&self, name: &str, kind: &str, raw: u64, max: u64) -> PathBuf {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            std::fs::write(dir.join("brightness"), format!("{raw}\n")).unwrap();
            std::fs::write(dir.join("max_brightness"), format!("{max}\n")).unwrap();
            dir
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sysfs_reads_and_writes_the_backlight() {
        let sysfs = FakeSysfs::new("roundtrip");
        let dir = sysfs.add("intel_backlight", "raw", 19200, 96000);
        let mut brightness = Brightness::with_device(Box::new(Sysfs::new(&sysfs.0)));
        assert!(brightness.is_real());
        assert_eq!(brightness.level().percent(), 20);

        brightness.press(Key::Raise, T0, Motion::Full);
        let written = std::fs::read_to_string(dir.join("brightness")).unwrap();
        assert_eq!(written, "24000");
    }

    #[test]
    fn a_backlight_that_appears_after_startup_is_noticed() {
        let sysfs = FakeSysfs::new("late");
        let mut brightness = Brightness::with_device(Box::new(Sysfs::new(&sysfs.0)));
        assert!(!brightness.is_real(), "nothing there at startup");

        sysfs.add("amdgpu_bl0", "raw", 128, 255);
        brightness.press(Key::Raise, T0, Motion::Full);
        assert!(
            brightness.is_real(),
            "the backlight came up and nobody noticed"
        );
        assert_eq!(brightness.level().percent(), 55);
    }
}
