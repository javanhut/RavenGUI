//! The desktop's settings file, `~/.config/raven/desktop.toml`.
//!
//! Written by `raven-settings`, the settings application, and read here: this
//! is the one file the compositor takes a user's choices from. It is small on
//! purpose. Every key is optional, an absent file is the compiled-in look, and
//! a file that does not parse is logged and ignored rather than half-applied —
//! so the schema drift that `theme.rs` warns about costs a setting, never the
//! session.
//!
//! What is honoured, and by whom:
//!
//! - `appearance.accent` — [`crate::theme::accent`]: focus ring, dock
//!   indicator, panel highlights.
//! - `appearance.smooth_animations` — [`crate::settings::Motion`].
//! - `appearance.blur` — whether the desktop is blurred behind the launcher,
//!   the pin bar and glass windows; see `Huginn::blur_radius` and
//!   `Huginn::glass_window`. The one key whose default is not compiled in:
//!   when the file does not mention it, the backend's reading of the hardware
//!   decides ([`crate::backend::gpu_class`]), which is why it is kept as an
//!   `Option` here rather than resolved at parse time.
//! - `appearance.wallpaper` — the compositor's own background, behind whatever
//!   `ravencanvasd` draws when it is running.
//! - `appearance.launcher_layout` — `"list"` or `"arc"`,
//!   [`crate::launcher::Style`]; also stepped from quick settings.
//! - `appearance.glass_theme` — `"black"`, `"fog"`, `"arctic"`,
//!   `"midnight"` or `"rose"`, [`crate::theme::Theme`]: the tint of every
//!   panel the compositor draws; also stepped from quick settings.
//! - `dock.icon_size`, `dock.magnification`, `dock.labels`,
//!   `dock.running_dots`, `dock.auto_hide` — [`crate::dock::Prefs`]: how big
//!   the dock's icons are, how far the one under the pointer lifts, whether
//!   it is named, whether a running application is marked, and whether the
//!   dock hides itself.
//! - `general.terminal` — what the spawn binding launches.
//! - `general.lock_after_minutes` — [`crate::settings::IdleAfter`].
//! - `general.lock_screen_off_seconds` — [`crate::screenoff::ScreenOff`]:
//!   whether and when the screens go off while the session is locked.
//! - `notifications.do_not_disturb` — only critical notifications are shown;
//!   also switched from quick settings. See [`crate::notifications`].
//! - `notifications.timeout_seconds` — how long a card stays when its
//!   application leaves that to the desktop; see
//!   [`huginn_core::notify::Timeouts`].
//! - `touch.output` — which screen a touchscreen puts its fingers on, and
//! - `touch.enabled` — whether it is listened to at all. See
//!   [`Huginn::touch_output`] and the note on [`Touch`].
//!
//! The rest of the file (theme mode, shadows, scale, …) is for the
//! applications and the bar, which read it themselves.

use std::path::PathBuf;

use serde::Deserialize;

use crate::settings::{IdleAfter, Motion};
use crate::theme::Color;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Appearance {
    /// `#RRGGBB`.
    pub accent: String,
    pub smooth_animations: bool,
    /// Blur the desktop behind the panels and the translucent ("glass")
    /// windows that ask for it.
    /// `None` is "the file does not say", which serde's `default` gives an
    /// absent key and never a present one — `raven-settings` writes every
    /// key it knows, so a saved file always says.
    pub blur: Option<bool>,
    /// Absolute path of an image, or empty for the machine's wallpaper.
    pub wallpaper: String,
    /// `"list"` or `"arc"`; see [`crate::launcher::Style`]. Empty, or a
    /// value this build does not know, is the list.
    pub launcher_layout: String,
    /// `"black"`, `"fog"`, `"arctic"`, `"midnight"` or `"rose"`; see
    /// [`crate::theme::Theme`]. Empty, or unknown, is Black Glass.
    pub glass_theme: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            accent: String::new(),
            smooth_animations: true,
            blur: None,
            wallpaper: String::new(),
            launcher_layout: String::new(),
            glass_theme: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct General {
    pub terminal: String,
    /// 0 is never.
    pub lock_after_minutes: u32,
    /// While locked: 0 turns the screens off immediately, a negative number
    /// never, anything else after that many seconds.
    pub lock_screen_off_seconds: i64,
}

impl Default for General {
    fn default() -> Self {
        Self {
            terminal: crate::theme::TERMINAL.to_owned(),
            lock_after_minutes: 10,
            lock_screen_off_seconds: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Notifications {
    pub do_not_disturb: bool,
    /// Seconds a normal card stays when its application leaves that to the
    /// desktop. 0 is the compiled-in six.
    pub timeout_seconds: u32,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            do_not_disturb: false,
            timeout_seconds: 6,
        }
    }
}

/// The dock.
///
/// Behaviour, not appearance: how big the icons are, how far they lift under
/// the pointer, whether what is under it is named, whether a running
/// application is marked, and whether the dock gets out of the way. There is
/// deliberately no background or corner radius here — see [`crate::dock::Prefs`].
///
/// Every value is held to a range when it is read, so a hand-edited file
/// cannot make a dock wider than the screen or divide the strip by nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Dock {
    /// Icon size at a 1080p output, in logical pixels.
    pub icon_size: u32,
    /// How far the icon under the pointer grows, as a percentage of that.
    /// 100 is no magnification at all.
    pub magnification: u32,
    /// Whether the item under the pointer is named.
    pub labels: bool,
    /// Whether a running application is marked with a dot.
    pub running_dots: bool,
    /// Whether the dock hides itself when the pointer leaves the edge.
    ///
    /// Turned off, the dock stays on screen — over whatever is behind it. It
    /// does not claim an exclusive zone, so a window is not made shorter to
    /// make room for it; the dock is a strip that floats, and always was.
    pub auto_hide: bool,
}

impl Default for Dock {
    fn default() -> Self {
        let compiled = crate::dock::Prefs::default();
        Self {
            icon_size: compiled.icon as u32,
            magnification: (compiled.magnify * 100.0) as u32,
            labels: compiled.labels,
            running_dots: compiled.dots,
            auto_hide: compiled.auto_hide,
        }
    }
}

/// The touchscreen.
///
/// Two keys, and both exist because the compositor's own answers are guesses
/// that can be wrong on a real machine.
///
/// A touchscreen reports a fraction of its own glass, so a point on the
/// desktop needs to know which panel that glass is stuck to, and nothing in
/// the protocol says. `Huginn::touch_output` matches it by physical size --
/// libinput knows how big the digitizer is, EDID says how big each panel is --
/// which is the only evidence there is and is defeated by two same-size
/// panels, or by EDID millimetres that are simply wrong. When it picks the
/// wrong screen every touch lands on a monitor nobody is touching, and there
/// is no way to discover that from inside the compositor. `output` is the
/// override.
///
/// `enabled` is the other failure. A digitizer that has started reporting
/// contacts nobody made -- a cracked panel, water under the glass, a loose
/// ribbon -- makes the desktop unusable in a way no amount of careful input
/// handling can help with, because the events are real. Turning it off is the
/// only repair that does not involve a screwdriver, and a machine in that
/// state must not need one to become usable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Touch {
    /// Whether touchscreens are listened to at all.
    pub enabled: bool,
    /// The connector name -- `eDP-1`, `HDMI-A-1` -- a touchscreen maps to.
    /// Empty is "work it out", which is what almost every machine wants.
    ///
    /// One name and not a per-device table: a machine with two touchscreens
    /// wants the matching to work rather than to be written out, and the
    /// escape hatch is for the single-touchscreen laptop the heuristic got
    /// wrong. A table can be added the day something has two.
    pub output: String,
}

impl Default for Touch {
    fn default() -> Self {
        Self {
            enabled: true,
            output: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DesktopConfig {
    pub appearance: Appearance,
    pub general: General,
    pub dock: Dock,
    pub notifications: Notifications,
    pub touch: Touch,
}

/// Where the file lives: `$XDG_CONFIG_HOME/raven/desktop.toml`.
pub(crate) fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("raven").join("desktop.toml"))
}

impl DesktopConfig {
    /// The file's contents, or the defaults when there is no file or it will
    /// not parse. Never fails: a session must start whatever is on disk.
    pub(crate) fn load() -> Self {
        let Some(path) = path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), "ignoring desktop.toml: {e}");
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), "could not read desktop.toml: {e}");
                Self::default()
            }
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The accent, when the file names a valid one.
    pub(crate) fn accent(&self) -> Option<Color> {
        parse_hex(&self.appearance.accent)
    }

    /// Blur, with `hardware` standing in when the file is silent.
    pub(crate) fn blur(&self, hardware: bool) -> bool {
        self.appearance.blur.unwrap_or(hardware)
    }

    pub(crate) fn motion(&self) -> Motion {
        if self.appearance.smooth_animations {
            Motion::Full
        } else {
            Motion::Reduced
        }
    }

    /// Whether touchscreens are listened to.
    pub(crate) fn touch_enabled(&self) -> bool {
        self.touch.enabled
    }

    /// The screen the file pins touchscreens to, if it names one.
    ///
    /// Trimmed and emptiness-checked here so that a blank or whitespace value
    /// -- which is what `raven-settings` writes for "no override" -- reads the
    /// same as an absent key rather than as a screen named "".
    pub(crate) fn touch_output(&self) -> Option<&str> {
        let name = self.touch.output.trim();
        (!name.is_empty()).then_some(name)
    }

    pub(crate) fn idle_after(&self) -> IdleAfter {
        IdleAfter::from_minutes(self.general.lock_after_minutes)
    }

    pub(crate) fn screen_off(&self) -> crate::screenoff::ScreenOff {
        crate::screenoff::ScreenOff::from_seconds(self.general.lock_screen_off_seconds)
    }

    /// The terminal to spawn; the compiled-in one when the file is blank.
    pub(crate) fn terminal(&self) -> &str {
        let t = self.general.terminal.trim();
        if t.is_empty() {
            crate::theme::TERMINAL
        } else {
            t
        }
    }

    /// How the launcher is laid out: the list unless the file names another.
    pub(crate) fn launcher_style(&self) -> crate::launcher::Style {
        crate::launcher::Style::from_value(&self.appearance.launcher_layout).unwrap_or_default()
    }

    /// Which glass the panels are made of: Black Glass unless the file
    /// names another.
    pub(crate) fn glass_theme(&self) -> crate::theme::Theme {
        crate::theme::Theme::from_value(&self.appearance.glass_theme).unwrap_or_default()
    }

    pub(crate) fn wallpaper(&self) -> Option<PathBuf> {
        let w = self.appearance.wallpaper.trim();
        (!w.is_empty()).then(|| PathBuf::from(w))
    }

    /// What the dock was told, held to its bounds. See [`crate::dock::Prefs`].
    pub(crate) fn dock(&self) -> crate::dock::Prefs {
        crate::dock::Prefs::new(
            self.dock.icon_size,
            self.dock.magnification,
            self.dock.labels,
            self.dock.running_dots,
            self.dock.auto_hide,
        )
    }

    pub(crate) fn do_not_disturb(&self) -> bool {
        self.notifications.do_not_disturb
    }

    /// How long cards stay by default. The file sets a normal card's time; a
    /// low-urgency card keeps its shorter one unless that would be longer. An
    /// hour is the most the file may ask for: a card that stays longer is one
    /// that should have been critical.
    pub(crate) fn notification_timeouts(&self) -> huginn_core::notify::Timeouts {
        let defaults = huginn_core::notify::Timeouts::default();
        let seconds = self.notifications.timeout_seconds;
        if seconds == 0 {
            return defaults;
        }
        let normal = std::time::Duration::from_secs(u64::from(seconds.min(3600)));
        huginn_core::notify::Timeouts {
            low: defaults.low.min(normal),
            normal,
        }
    }
}

/// `#RRGGBB` to an opaque colour.
fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::from_argb(0xFF00_0000 | rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let cfg = DesktopConfig::parse("").unwrap();
        assert_eq!(cfg.accent(), None);
        assert_eq!(cfg.motion(), Motion::Full);
        assert_eq!(cfg.idle_after(), IdleAfter::Minutes10);
        assert_eq!(cfg.terminal(), crate::theme::TERMINAL);
        assert_eq!(cfg.wallpaper(), None);
        assert_eq!(cfg.appearance.blur, None);
    }

    /// A machine that has never heard of the setting must still listen to its
    /// touchscreen, and must still work the panel out for itself.
    #[test]
    fn touch_is_on_and_unpinned_when_the_file_says_nothing() {
        let cfg = DesktopConfig::parse("").unwrap();
        assert!(cfg.touch_enabled());
        assert_eq!(cfg.touch_output(), None);
    }

    #[test]
    fn a_pinned_screen_is_read_back() {
        let cfg = DesktopConfig::parse("[touch]\noutput = \"eDP-1\"\n").unwrap();
        assert_eq!(cfg.touch_output(), Some("eDP-1"));
        assert!(cfg.touch_enabled(), "pinning a screen does not disable it");
    }

    /// `raven-settings` writes every key it knows, so "no override" reaches
    /// this as a blank string rather than as an absent key. A blank must read
    /// as absent and never as a screen whose name is empty -- which would
    /// match no output and silently fall through to the guess anyway, but by
    /// accident rather than on purpose.
    #[test]
    fn a_blank_screen_name_is_no_override() {
        for text in ["[touch]\noutput = \"\"\n", "[touch]\noutput = \"   \"\n"] {
            assert_eq!(DesktopConfig::parse(text).unwrap().touch_output(), None);
        }
    }

    #[test]
    fn the_screens_go_off_when_locked_unless_told_otherwise() {
        use crate::screenoff::ScreenOff;
        let absent = DesktopConfig::parse("[general]\nlock_after_minutes = 5\n").unwrap();
        assert_eq!(absent.screen_off(), ScreenOff::Immediately);
        let never = DesktopConfig::parse("[general]\nlock_screen_off_seconds = -1\n").unwrap();
        assert_eq!(never.screen_off(), ScreenOff::Never);
        let later = DesktopConfig::parse("[general]\nlock_screen_off_seconds = 30\n").unwrap();
        assert_eq!(
            later.screen_off(),
            ScreenOff::After(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn touch_can_be_switched_off() {
        let cfg = DesktopConfig::parse("[touch]\nenabled = false\n").unwrap();
        assert!(!cfg.touch_enabled());
    }

    /// The whole file is one schema: a `[touch]` section must not cost the
    /// keys around it, and an unknown key in it must not cost the section.
    #[test]
    fn touch_sits_beside_the_other_sections() {
        let cfg = DesktopConfig::parse(
            "[general]\nlock_after_minutes = 5\n\n\
             [touch]\nenabled = false\noutput = \"DP-2\"\n",
        )
        .unwrap();
        assert_eq!(cfg.idle_after(), IdleAfter::Minutes5);
        assert!(!cfg.touch_enabled());
        assert_eq!(cfg.touch_output(), Some("DP-2"));
    }

    #[test]
    fn the_launcher_layout_is_the_list_unless_the_file_names_the_arc() {
        use crate::launcher::Style;
        assert_eq!(
            DesktopConfig::parse("").unwrap().launcher_style(),
            Style::List
        );
        let arc = DesktopConfig::parse("[appearance]\nlauncher_layout = \"arc\"\n").unwrap();
        assert_eq!(arc.launcher_style(), Style::Arc);
        // A layout a later build added falls back rather than failing the file.
        let unknown = DesktopConfig::parse("[appearance]\nlauncher_layout = \"orbit\"\n").unwrap();
        assert_eq!(unknown.launcher_style(), Style::List);
    }

    #[test]
    fn the_glass_is_black_unless_the_file_names_another() {
        use crate::theme::Theme;
        assert_eq!(DesktopConfig::parse("").unwrap().glass_theme(), Theme::Black);
        let fog = DesktopConfig::parse("[appearance]\nglass_theme = \"fog\"\n").unwrap();
        assert_eq!(fog.glass_theme(), Theme::Fog);
        let odd = DesktopConfig::parse("[appearance]\nglass_theme = \"sepia\"\n").unwrap();
        assert_eq!(odd.glass_theme(), Theme::Black);
    }

    #[test]
    fn blur_follows_the_hardware_only_when_unset() {
        let silent = DesktopConfig::parse("[appearance]\naccent = \"#F7768E\"\n").unwrap();
        assert!(silent.blur(true));
        assert!(!silent.blur(false));

        // What the settings application writes, on an integrated GPU: the
        // file wins, both ways.
        let on = DesktopConfig::parse("[appearance]\nblur = true\n").unwrap();
        assert!(on.blur(false));
        let off = DesktopConfig::parse("[appearance]\nblur = false\n").unwrap();
        assert!(!off.blur(true));
    }

    #[test]
    fn what_raven_settings_writes_is_read_back() {
        let cfg = DesktopConfig::parse(
            "[appearance]\naccent = \"#F7768E\"\nsmooth_animations = false\nwallpaper = \"/home/x/.local/share/raven/wallpaper/wallpaper.jpg\"\n\n[general]\nterminal = \"kitty\"\nlock_after_minutes = 0\n",
        )
        .unwrap();
        assert_eq!(cfg.accent(), Some(Color::from_argb(0xFFF7_768E)));
        assert_eq!(cfg.motion(), Motion::Reduced);
        assert_eq!(cfg.idle_after(), IdleAfter::Off);
        assert_eq!(cfg.terminal(), "kitty");
        assert!(cfg.wallpaper().is_some());
    }

    #[test]
    fn notifications_interrupt_and_stay_six_seconds_unless_the_file_says() {
        use std::time::Duration;
        let silent = DesktopConfig::parse("").unwrap();
        assert!(!silent.do_not_disturb());
        assert_eq!(
            silent.notification_timeouts(),
            huginn_core::notify::Timeouts::default()
        );

        let set =
            DesktopConfig::parse("[notifications]\ndo_not_disturb = true\ntimeout_seconds = 10\n")
                .unwrap();
        assert!(set.do_not_disturb());
        let t = set.notification_timeouts();
        assert_eq!(t.normal, Duration::from_secs(10));
        assert_eq!(t.low, Duration::from_secs(4), "low keeps its shorter time");

        let short = DesktopConfig::parse("[notifications]\ntimeout_seconds = 2\n").unwrap();
        assert_eq!(short.notification_timeouts().low, Duration::from_secs(2));

        let zero = DesktopConfig::parse("[notifications]\ntimeout_seconds = 0\n").unwrap();
        assert_eq!(
            zero.notification_timeouts(),
            huginn_core::notify::Timeouts::default()
        );
    }

    #[test]
    fn unknown_keys_and_sections_are_fine() {
        // The file carries sections for the bar and the applications too.
        let cfg = DesktopConfig::parse(
            "[privacy]\nx = 1\n[appearance]\nblur = true\ntheme_mode = \"dark\"\n",
        )
        .unwrap();
        assert_eq!(cfg.motion(), Motion::Full);
    }

    #[test]
    fn a_bad_accent_is_no_accent() {
        for bad in ["", "#12", "red", "#GGGGGG", "7AA2F7"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
        assert_eq!(parse_hex(" #7aa2f7 "), Some(Color::from_argb(0xFF7A_A2F7)));
    }
}
