//! Raven Glass: the one look shared by every Raven app.
//!
//! The stylesheets live here, beside the compositor whose glass they match,
//! and nowhere else. The `ravengui` package installs them to
//! [`SYSTEM_DIR`]; an app reads them from there when it starts, so a change
//! to the look is one package update rather than an edit and a rebuild in
//! every app's repository. Each app then lays its own few classes on top.
//!
//! A copy of each sheet is also compiled in, and is what an app gets when the
//! installed one cannot be read (a bare build machine, a broken install): the
//! app still looks like Raven, only as of the release it was built against.
//!
//! What the person picks -- light or dark, accent, glass theme -- is theirs,
//! written by Raven Settings to `~/.config/raven/desktop.toml`; [`tint`]
//! turns the glass theme into CSS over these sheets.
//!
//! No toolkit here: the crate hands out CSS text and each app loads it into a
//! provider of its own.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::OnceLock;

pub mod tint;

/// Where the `ravengui` package installs the sheets.
pub const SYSTEM_DIR: &str = "/usr/share/raven/glass";

/// Overrides [`SYSTEM_DIR`], for trying an edited sheet without installing
/// it: `RAVEN_GLASS_DIR=~/src/RavenGUI/crates/raven-glass/data raven-settings`.
pub const DIR_ENV: &str = "RAVEN_GLASS_DIR";

const BASE_FILE: &str = "raven-glass.css";
const LIGHT_FILE: &str = "raven-glass-light.css";

/// The compiled-in dark sheet, as of this build.
pub const EMBEDDED_BASE: &str = include_str!("../data/raven-glass.css");
/// The compiled-in light sheet, as of this build.
pub const EMBEDDED_LIGHT: &str = include_str!("../data/raven-glass-light.css");

/// Raven Glass itself, the dark sheet every app loads first.
pub fn base_css() -> &'static str {
    static CSS: OnceLock<Cow<'static, str>> = OnceLock::new();
    CSS.get_or_init(|| installed(BASE_FILE, EMBEDDED_BASE))
}

/// Laid over [`base_css`] in light mode.
pub fn light_css() -> &'static str {
    static CSS: OnceLock<Cow<'static, str>> = OnceLock::new();
    CSS.get_or_init(|| installed(LIGHT_FILE, EMBEDDED_LIGHT))
}

/// The directory the sheets are read from.
pub fn dir() -> PathBuf {
    std::env::var_os(DIR_ENV)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(SYSTEM_DIR))
}

/// The installed sheet `name`, or `fallback` when it is missing, unreadable
/// or empty. Read once per process: a sheet changing under a running app is
/// picked up at its next start, as a new binary would be.
fn installed(name: &str, fallback: &'static str) -> Cow<'static, str> {
    match std::fs::read_to_string(dir().join(name)) {
        Ok(css) if !css.trim().is_empty() => Cow::Owned(css),
        _ => Cow::Borrowed(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_sheet_falls_back_to_the_compiled_in_one() {
        let css = installed("no-such-sheet.css", EMBEDDED_BASE);
        assert!(matches!(css, Cow::Borrowed(_)));
        assert_eq!(css, EMBEDDED_BASE);
    }

    #[test]
    fn the_compiled_in_sheets_are_raven_glass() {
        assert!(EMBEDDED_BASE.contains("window.raven"));
        assert!(EMBEDDED_BASE.contains("@define-color window_bg_color"));
        assert!(!EMBEDDED_LIGHT.trim().is_empty());
    }
}
