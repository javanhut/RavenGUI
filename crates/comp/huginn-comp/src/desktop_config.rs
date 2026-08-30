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
//! - `appearance.blur` — whether glass windows get the desktop blurred behind
//!   them; see `Huginn::glass_window`.
//! - `appearance.wallpaper` — the compositor's own background, behind whatever
//!   `ravencanvasd` draws when it is running.
//! - `general.terminal` — what the spawn binding launches.
//! - `general.lock_after_minutes` — [`crate::settings::IdleAfter`].
//!
//! The rest of the file (theme mode, blur, shadows, scale, …) is for the
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
    /// Blur the desktop behind translucent ("glass") windows that ask for it.
    pub blur: bool,
    /// Absolute path of an image, or empty for the machine's wallpaper.
    pub wallpaper: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            accent: String::new(),
            smooth_animations: true,
            blur: true,
            wallpaper: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct General {
    pub terminal: String,
    /// 0 is never.
    pub lock_after_minutes: u32,
}

impl Default for General {
    fn default() -> Self {
        Self {
            terminal: crate::theme::TERMINAL.to_owned(),
            lock_after_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DesktopConfig {
    pub appearance: Appearance,
    pub general: General,
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

    pub(crate) fn motion(&self) -> Motion {
        if self.appearance.smooth_animations {
            Motion::Full
        } else {
            Motion::Reduced
        }
    }

    pub(crate) fn idle_after(&self) -> IdleAfter {
        IdleAfter::from_minutes(self.general.lock_after_minutes)
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

    pub(crate) fn wallpaper(&self) -> Option<PathBuf> {
        let w = self.appearance.wallpaper.trim();
        (!w.is_empty()).then(|| PathBuf::from(w))
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
    fn unknown_keys_and_sections_are_fine() {
        // The file carries sections for the bar and the applications too.
        let cfg = DesktopConfig::parse("[privacy]\nx = 1\n[appearance]\nblur = true\ntheme_mode = \"dark\"\n").unwrap();
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
