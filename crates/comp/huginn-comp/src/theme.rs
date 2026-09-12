//! The desktop's one look, compiled in.
//!
//! There is no configuration file and no theming engine. This module is the
//! whole visual language, and it is a set of constants rather than a schema
//! because the design constraint is that Raven ships *one* look — see the
//! design spec, §1: "Opinionated, not configurable", and §11: "Zero
//! user-facing compositor config files".
//!
//! The point is not that configuration is hard. It is that a format which can
//! be written by a user is a format that must not change between releases, and
//! the commonest way a compositor breaks someone's desktop is a config schema
//! drifting under them. A constant cannot drift, because nothing outside this
//! binary ever names it.
//!
//! Everything the shell draws reads from here, which is what keeps the focus
//! ring, the dock, and the launcher one accent rather than three that happen to
//! agree today.

/// A colour, as `0xAARRGGBB`.
///
/// Packed into one integer so it is `Copy` and comparable, and converted at the
/// edges rather than stored three times: the renderer wants normalized f32, a
/// byte canvas wants RGBA bytes, and an `wl_shm` buffer wants packed ARGB. The
/// same accent used to be written down in all three forms in three files, which
/// is exactly the drift this prevents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Color(u32);

impl Color {
    /// From `0xAARRGGBB`.
    pub(crate) const fn from_argb(argb: u32) -> Self {
        Self(argb)
    }

    /// Packed `0xAARRGGBB`, which is what `wl_shm`'s `Argb8888` wants.
    #[allow(dead_code)] // Wanted by the dock and launcher, which are not built yet.
    pub(crate) const fn to_argb_u32(self) -> u32 {
        self.0
    }

    /// `[r, g, b, a]` bytes, for drawing into a byte canvas.
    pub(crate) const fn to_rgba_bytes(self) -> [u8; 4] {
        [
            (self.0 >> 16) as u8,
            (self.0 >> 8) as u8,
            self.0 as u8,
            (self.0 >> 24) as u8,
        ]
    }

    /// `[r, g, b, a]` in 0.0..=1.0, which is what smithay's buffers want.
    pub(crate) fn to_rgba_f32(self) -> [f32; 4] {
        self.to_rgba_bytes().map(|c| f32::from(c) / 255.0)
    }

    /// The same colour at a different opacity.
    ///
    /// The keybinding overlay is translucent and everything else is not, but it
    /// is the same background: one constant, and the surface that wants to see
    /// through it says so at the point of use.
    pub(crate) const fn with_alpha(self, alpha: u8) -> Self {
        Self((self.0 & 0x00FF_FFFF) | ((alpha as u32) << 24))
    }
}

/// Focus ring, overlay headings, the dock's running-app indicator: the
/// compiled-in default. Drawing code reads [`accent`], which is this unless
/// `desktop.toml` chose another.
pub(crate) const ACCENT: Color = Color::from_argb(0xFF7A_A2F7);

/// The accent in use, as `0xAARRGGBB`.
///
/// The one theme value a user may change, and the exception to the rule at the
/// top of this file — kept small on purpose: an accent is a colour the person
/// picked from a short list, not a schema. Read by every drawing site through
/// [`accent`]; set from [`crate::desktop_config`] at startup and on reload.
static ACCENT_IN_USE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xFF7A_A2F7);

pub(crate) fn accent() -> Color {
    Color(ACCENT_IN_USE.load(std::sync::atomic::Ordering::Relaxed))
}

pub(crate) fn set_accent(color: Option<Color>) {
    let color = color.unwrap_or(ACCENT);
    ACCENT_IN_USE.store(color.0, std::sync::atomic::Ordering::Relaxed);
}

/// The settings application, opened from quick settings and its own chord.
pub(crate) const SETTINGS_APP: &str = "raven-settings";
/// The software store, opened from its own chord.
pub(crate) const STORE_APP: &str = "raven-store";
/// Panel, dock and overlay background.
pub(crate) const BACKGROUND: Color = Color::from_argb(0xFF16_161F);
/// Hairline borders.
pub(crate) const BORDER: Color = Color::from_argb(0xFF2A_2A3A);
/// Body text.
pub(crate) const TEXT: Color = Color::from_argb(0xFFE8_E8F0);
/// Secondary text: footers, hints, anything deliberately quieter. This is
/// TEXT at ~70% over BACKGROUND, the same ratio the GTK apps use; RoostBar's
/// `muted` and Settings' `sync_roostbar` carry the same hex.
pub(crate) const TEXT_DIM: Color = Color::from_argb(0xFFAB_ABC2);

// ---------------------------------------------------------------------------
// The material
// ---------------------------------------------------------------------------
// Every floating panel — dock, launcher, pinned panel, keybinding overlay,
// caption pills — is one material: a translucent layer of [`BACKGROUND`]
// over the blurred desktop, edged with a hairline of light rather than a
// drawn border, and lit along its top edge as a real sheet of glass would
// be. The GTK applications describe the same material in CSS
// (`raven-glass.css`), and RoostBar draws it with the same numbers, which
// is what makes a window, a bar and a panel read as one desktop.

/// Opacity of a panel's ground.
///
/// Low enough that the blurred desktop behind shows through as a frosted
/// tint, high enough that text stays legible over a busy wallpaper. The
/// launcher asserts a band around this; see `launcher::ALPHA`.
pub(crate) const PANEL_ALPHA: u8 = 0xD8;
/// Corner radius of a floating panel at a 1080p output, in logical pixels.
pub(crate) const PANEL_RADIUS: f32 = 22.0;
/// The edge: white at a whisper, drawn one pixel wide around the ground.
pub(crate) const HAIRLINE: Color = Color::from_argb(0x1CFF_FFFF);
/// The catch-light: a brighter pixel along the top edge, inside the
/// hairline, between the corner arcs. What says "glass" rather than "grey".
pub(crate) const CATCH_LIGHT: Color = Color::from_argb(0x30FF_FFFF);
/// A well: a field, a tile, a list group, set a shade *lighter* into the
/// ground rather than darker, the way a translucent layer over a
/// translucent layer looks.
pub(crate) const WELL: Color = Color::from_argb(0x14FF_FFFF);
/// A well under the pointer, or otherwise raised.
pub(crate) const WELL_RAISED: Color = Color::from_argb(0x22FF_FFFF);
/// A hairline inside a panel: between rows, under a heading.
pub(crate) const RULE: Color = Color::from_argb(0x14FF_FFFF);

/// The selection wash: the accent at the strength a selected row or tile is
/// tinted with. The accent's one job inside a panel, apart from the caret.
pub(crate) fn selection() -> Color {
    accent().with_alpha(0x3A)
}

/// Thickness of the focus ring, in logical pixels.
///
/// Two is enough to see at a glance and thin enough to sit inside [`GAP`].
pub(crate) const FOCUS_RING_WIDTH: i32 = 2;

/// Height of the title bar the compositor draws for a window that asked for
/// server-side decorations, in logical pixels. See [`crate::decor`].
///
/// Fixed rather than scaled with the screen: it is a layout inset that
/// `huginn-core` subtracts from a pane, and a pane is measured in logical
/// pixels whatever the panel's density. The text inside it grows with the
/// screen the way every other panel's does.
pub(crate) const TITLE_BAR_HEIGHT: i32 = 30;

/// The title bar's background: the same panel colour as the dock and the
/// launcher, so a decorated window reads as part of one desktop rather than
/// as a window wearing somebody else's frame.
pub(crate) const TITLE_BAR_BG: Color = BACKGROUND;

/// The title's size at 1080p, in logical pixels; scaled with the screen.
pub(crate) const TITLE_TEXT_SIZE: f32 = 13.0;

/// Space between tiled windows, and between a window and the screen edge.
///
/// Handed to `Space` at startup rather than read by it: `huginn-core` decides
/// geometry and knows nothing about how the desktop looks, and a gutter is an
/// appearance decision that happens to have geometric consequences.
pub(crate) const GAP: i32 = 8;

/// The icon theme the launcher and dock resolve `Icon=` names against.
///
/// `hicolor` is the spec's universal fallback and every theme inherits it, but
/// it carries only what applications install for themselves: generic names like
/// `network-wired` resolve to nothing against bare hicolor. Measured on the
/// development machine, 10 of 36 installed applications had no icon under
/// `hicolor` and 1 under `breeze-dark`. Whatever RavenLinux ships belongs here.
///
/// RavenLinux ships `breeze-icons`, so this names `breeze-dark`: the light
/// variant is drawn for dark panels, which is what [`BACKGROUND`] is. Naming
/// `hicolor` here was not a smaller choice but an empty one — the image
/// carried three files under that theme, all of them installed by CMake, so
/// every icon in the dock and the launcher resolved to nothing and drew blank.
///
/// `Icons::find` walks this theme, then everything it inherits, then hicolor
/// regardless, so a name this theme happens to lack still resolves the way it
/// did before. Nothing is lost by preferring a theme that has icons in it.
pub(crate) const ICON_THEME: &str = "breeze-dark";

/// How many panes the carousel shows at once.
///
/// Two: wide enough that a pane still holds a readable column of text beside
/// another, narrow enough that the strip is worth scrolling at all. A constant
/// rather than a setting, for the reason at the top of this file — and it sits
/// here rather than in `huginn-core` because how wide a pane should be is an
/// appearance decision that happens to have geometric consequences, which is
/// exactly what [`GAP`] already is.
pub(crate) const CAROUSEL_COLUMNS: u32 = 2;

/// The terminal the spawn binding launches.
///
/// RavenLinux ships its own, so that is what the desktop opens. There is no
/// environment override: an override is a user-facing configuration surface
/// with extra steps, and §11 does not distinguish the two.
pub(crate) const TERMINAL: &str = "raven-terminal";

/// The lock screen the compositor puts up.
///
/// From RavenLogin rather than from this repo, and that is deliberate: it is
/// the login screen's twin, drawn by the same code and authenticating against
/// the same daemon. A lock screen that merely resembles the login screen is one
/// that teaches its owner to type their password into things that look about
/// right.
///
/// Named here with no override, for the reason [`TERMINAL`] gives. A
/// configurable lock screen is a configurable answer to "what is allowed to ask
/// me for my password", which is not a question that should have a knob.
pub(crate) const LOCK_SCREEN: &str = "raven-lock";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_encodings_all_describe_the_same_colour() {
        // The drift this type exists to prevent: the accent was once #7AA2F7
        // written as normalized f32, as RGBA bytes, and as packed ARGB, in
        // three separate files with nothing tying them together.
        assert_eq!(ACCENT.to_argb_u32(), 0xFF7A_A2F7);
        assert_eq!(ACCENT.to_rgba_bytes(), [0x7A, 0xA2, 0xF7, 0xFF]);
        let [r, g, b, a] = ACCENT.to_rgba_f32();
        assert!((r - 0.478).abs() < 0.005, "r was {r}");
        assert!((g - 0.635).abs() < 0.005, "g was {g}");
        assert!((b - 0.969).abs() < 0.005, "b was {b}");
        assert_eq!(a, 1.0);
    }

    #[test]
    fn with_alpha_changes_only_the_alpha() {
        let translucent = BACKGROUND.with_alpha(0xF2);
        assert_eq!(translucent.to_rgba_bytes(), [0x16, 0x16, 0x1F, 0xF2]);
        assert_eq!(translucent.with_alpha(0xFF), BACKGROUND);
    }

    #[test]
    fn every_colour_is_fully_opaque_unless_asked_otherwise() {
        // A theme colour that is accidentally translucent shows as a subtly
        // wrong shade rather than as an obvious bug.
        for (name, color) in [
            ("ACCENT", ACCENT),
            ("BACKGROUND", BACKGROUND),
            ("BORDER", BORDER),
            ("TEXT", TEXT),
            ("TEXT_DIM", TEXT_DIM),
            ("TITLE_BAR_BG", TITLE_BAR_BG),
        ] {
            assert_eq!(color.to_rgba_bytes()[3], 0xFF, "{name} is not opaque");
        }
    }
}
