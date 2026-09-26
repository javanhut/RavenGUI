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

// ---------------------------------------------------------------------------
// The glass themes
// ---------------------------------------------------------------------------
// One material, tinted several ways. Every theme is the same construction —
// a translucent ground over the blurred desktop, a hairline, a catch-light,
// wells a shade lighter than the ground — so switching one changes the
// colour of the glass and never the shape of anything. The constants above
// are Black Glass, the look Raven has always had; drawing code reads the
// functions below, which answer for whichever theme is in use.
//
// Every theme keeps light text. The shell's drawing is full of white washes
// and dark shadows chosen for light-on-glass, and a theme with dark text
// would need each of those revisited; until then a light glass is a pale,
// cool tint that white text still reads on, which is what frosted glass
// over a landscape looks like anyway.

/// The glass the panels are made of. Chosen in `desktop.toml` as
/// `appearance.glass_theme`, and stepped from quick settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Theme {
    /// Near-black smoked glass. The original look.
    #[default]
    Black,
    /// Soft blue-grey frosted glass: a misty morning.
    Fog,
    /// Pale, icy blue glass with a cold edge.
    Arctic,
    /// Deep navy glass, bluer and cooler than black.
    Midnight,
    /// Dusky rose-tinted glass.
    Rose,
}

impl Theme {
    pub(crate) const ALL: [Self; 5] = [
        Self::Black,
        Self::Fog,
        Self::Arctic,
        Self::Midnight,
        Self::Rose,
    ];

    /// What quick settings shows.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Black => "Black Glass",
            Self::Fog => "Fog Glass",
            Self::Arctic => "Arctic Glass",
            Self::Midnight => "Midnight Glass",
            Self::Rose => "Rose Glass",
        }
    }

    /// What `desktop.toml` says.
    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Fog => "fog",
            Self::Arctic => "arctic",
            Self::Midnight => "midnight",
            Self::Rose => "rose",
        }
    }

    /// Read either spelling, with or without "glass": `"fog"`, `"Fog Glass"`,
    /// `"fog-glass"`.
    pub(crate) fn from_value(value: &str) -> Option<Self> {
        let squash = |s: &str| {
            s.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase()
        };
        let want = squash(value);
        let want = want.strip_suffix("glass").unwrap_or(&want).to_owned();
        Self::ALL.into_iter().find(|t| squash(t.value()) == want)
    }

    /// The next theme, `delta` steps along [`Self::ALL`], wrapping.
    pub(crate) fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }

    /// The colours this glass is made of.
    pub(crate) const fn palette(self) -> &'static Palette {
        match self {
            Self::Black => &BLACK_GLASS,
            Self::Fog => &FOG_GLASS,
            Self::Arctic => &ARCTIC_GLASS,
            Self::Midnight => &MIDNIGHT_GLASS,
            Self::Rose => &ROSE_GLASS,
        }
    }
}

/// One theme's colours. See the constants of the same names for what each
/// is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    pub background: Color,
    pub text: Color,
    pub text_dim: Color,
    pub border: Color,
    pub hairline: Color,
    pub catch_light: Color,
    pub well: Color,
    pub well_raised: Color,
    pub rule: Color,
    pub panel_alpha: u8,
    /// How far the desktop behind the launcher is dimmed, 0..=255 of black:
    /// enough that the panel is front and centre, not so much that the
    /// desktop disappears.
    pub backdrop: u8,
}

const BLACK_GLASS: Palette = Palette {
    background: BACKGROUND,
    text: TEXT,
    text_dim: TEXT_DIM,
    border: BORDER,
    hairline: HAIRLINE,
    catch_light: CATCH_LIGHT,
    well: WELL,
    well_raised: WELL_RAISED,
    rule: RULE,
    panel_alpha: PANEL_ALPHA,
    backdrop: 0x66,
};

const FOG_GLASS: Palette = Palette {
    background: Color::from_argb(0xFF6E_7D94),
    text: Color::from_argb(0xFFFF_FFFF),
    text_dim: Color::from_argb(0xFFE1_E7F0),
    border: Color::from_argb(0xFF8E_9BB0),
    hairline: Color::from_argb(0x4DFF_FFFF),
    catch_light: Color::from_argb(0x80FF_FFFF),
    well: Color::from_argb(0x26FF_FFFF),
    well_raised: Color::from_argb(0x40FF_FFFF),
    rule: Color::from_argb(0x33FF_FFFF),
    panel_alpha: 0xA8,
    backdrop: 0x40,
};

const ARCTIC_GLASS: Palette = Palette {
    background: Color::from_argb(0xFF4F_7F9F),
    text: Color::from_argb(0xFFFF_FFFF),
    text_dim: Color::from_argb(0xFFDD_EEF8),
    border: Color::from_argb(0xFF86_B2CC),
    hairline: Color::from_argb(0x66E8_F8FF),
    catch_light: Color::from_argb(0xA0F0_FCFF),
    well: Color::from_argb(0x29E8_F8FF),
    well_raised: Color::from_argb(0x45E8_F8FF),
    rule: Color::from_argb(0x38E8_F8FF),
    panel_alpha: 0xA0,
    backdrop: 0x38,
};

const MIDNIGHT_GLASS: Palette = Palette {
    background: Color::from_argb(0xFF0E_1630),
    text: Color::from_argb(0xFFE8_EEFF),
    text_dim: Color::from_argb(0xFFA6_B2D4),
    border: Color::from_argb(0xFF24_3160),
    hairline: Color::from_argb(0x2699_B4FF),
    catch_light: Color::from_argb(0x40B4_C8FF),
    well: Color::from_argb(0x1A99_B4FF),
    well_raised: Color::from_argb(0x2B99_B4FF),
    rule: Color::from_argb(0x1A99_B4FF),
    panel_alpha: 0xD0,
    backdrop: 0x60,
};

const ROSE_GLASS: Palette = Palette {
    background: Color::from_argb(0xFF5A_3A4E),
    text: Color::from_argb(0xFFFF_F4F8),
    text_dim: Color::from_argb(0xFFE8_CBD8),
    border: Color::from_argb(0xFF7E_5670),
    hairline: Color::from_argb(0x40FF_E0EC),
    catch_light: Color::from_argb(0x70FF_E6F0),
    well: Color::from_argb(0x22FF_E0EC),
    well_raised: Color::from_argb(0x38FF_E0EC),
    rule: Color::from_argb(0x2CFF_E0EC),
    panel_alpha: 0xB4,
    backdrop: 0x48,
};

/// The theme in use, as its index in [`Theme::ALL`]. An atomic for the
/// reason [`ACCENT_IN_USE`] is one: every drawing site reads it, and none of
/// them has the compositor state in hand.
static THEME_IN_USE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn theme() -> Theme {
    let at = THEME_IN_USE.load(std::sync::atomic::Ordering::Relaxed) as usize;
    Theme::ALL.get(at).copied().unwrap_or_default()
}

/// Switch themes. Returns whether that was a change, so the caller knows
/// whether everything drawn in the old colours has to be drawn again.
pub(crate) fn set_theme(theme: Theme) -> bool {
    let at = Theme::ALL.iter().position(|t| *t == theme).unwrap_or(0) as u8;
    THEME_IN_USE.swap(at, std::sync::atomic::Ordering::Relaxed) != at
}

fn palette() -> &'static Palette {
    theme().palette()
}

/// Panel, dock and overlay background, in the theme in use.
pub(crate) fn background() -> Color {
    palette().background
}
/// Body text, in the theme in use.
pub(crate) fn text() -> Color {
    palette().text
}
/// Secondary text, in the theme in use.
pub(crate) fn text_dim() -> Color {
    palette().text_dim
}
/// Hairline borders, in the theme in use.
pub(crate) fn border() -> Color {
    palette().border
}
/// A panel's edge, in the theme in use.
pub(crate) fn hairline() -> Color {
    palette().hairline
}
/// A panel's top-edge light, in the theme in use.
pub(crate) fn catch_light() -> Color {
    palette().catch_light
}
/// A well, in the theme in use.
pub(crate) fn well() -> Color {
    palette().well
}
/// A raised well, in the theme in use.
pub(crate) fn well_raised() -> Color {
    palette().well_raised
}
/// A rule inside a panel, in the theme in use.
pub(crate) fn rule() -> Color {
    palette().rule
}
/// Opacity of a panel's ground, in the theme in use.
pub(crate) fn panel_alpha() -> u8 {
    palette().panel_alpha
}
/// How far the desktop is dimmed behind the launcher, in the theme in use.
pub(crate) fn backdrop() -> u8 {
    palette().backdrop
}
/// The title bar's background, in the theme in use: the same glass as the
/// dock and the launcher, so a decorated window reads as part of one
/// desktop rather than as a window wearing somebody else's frame.
pub(crate) fn title_bar_bg() -> Color {
    background()
}

/// The settings application, opened from quick settings and its own chord.
pub(crate) const SETTINGS_APP: &str = "raven-settings";
/// The software store, opened from its own chord.
pub(crate) const STORE_APP: &str = "raven-store";
/// Panel, dock and overlay background.
pub(crate) const BACKGROUND: Color = Color::from_argb(0xFF16_161F);
/// The recording dot. Red, not the accent: it is a warning that the screen is
/// being captured, and it must not be mistaken for a theme colour.
pub(crate) const RECORDING: Color = Color::from_argb(0xFFFF_453A);
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
// Every floating panel — dock, launcher, pin bar, keybinding overlay,
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

/// Corner radius of a card at a 1080p output, in logical pixels.
///
/// Raven Glass has one radius scale — 6 controls, 8 rows, 12 groups, 14
/// cards, 20 heroes — and a notification is a card in exactly the sense the
/// GTK applications draw one. [`PANEL_RADIUS`] is for the large panels.
pub(crate) const CARD_RADIUS: f32 = 14.0;

/// Something critical: a notification marked urgent.
///
/// The error colour of the GTK applications (`raven-glass.css`'s
/// `error_color`), so "critical" is one red across the desktop. Not
/// [`RECORDING`], whose red only ever means the screen is being captured.
pub(crate) const CRITICAL: Color = Color::from_argb(0xFFFB_7185);

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
    fn every_theme_reads_back_from_what_the_file_says() {
        for theme in Theme::ALL {
            assert_eq!(Theme::from_value(theme.value()), Some(theme));
            assert_eq!(Theme::from_value(theme.label()), Some(theme));
        }
        assert_eq!(Theme::from_value("fog-glass"), Some(Theme::Fog));
        assert_eq!(Theme::from_value("sepia"), None);
        assert_eq!(Theme::Black.palette().background, BACKGROUND);
        assert_eq!(Theme::Rose.stepped(1), Theme::Black);
    }

    #[test]
    fn every_theme_keeps_its_text_readable_on_its_ground() {
        // Relative luminance, WCAG's way: text against a ground this far
        // apart stays legible over whatever the blur lets through.
        fn luminance(c: Color) -> f32 {
            let [r, g, b, _] = c.to_rgba_f32().map(|v| {
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            });
            0.2126 * r + 0.7152 * g + 0.0722 * b
        }
        for theme in Theme::ALL {
            let p = theme.palette();
            let (hi, lo) = (luminance(p.text), luminance(p.background));
            let ratio = (hi.max(lo) + 0.05) / (hi.min(lo) + 0.05);
            assert!(ratio >= 4.0, "{} text contrast {ratio}", theme.label());
        }
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
        ] {
            assert_eq!(color.to_rgba_bytes()[3], 0xFF, "{name} is not opaque");
        }
    }
}
