//! The dock: a floating strip of applications near the bottom of the screen.
//!
//! §4: **the dock is the taskbar.** Pinned applications and running ones share
//! one strip, and being running is a small indicator rather than a separate
//! region. A desktop with a launcher bar *and* a window list has told the user
//! that those are different kinds of thing, which they are not — they are the
//! same application, before and after you started it.
//!
//! # Revealing
//!
//! Hidden until the pointer reaches the bottom edge, then held there for
//! [`HOVER_DELAY`] before it comes up. The delay is the whole difference
//! between a dock and a nuisance: without it, every pointer movement that
//! crosses the bottom of the screen — dragging a scrollbar, reaching for a
//! window edge — summons it.

use std::time::Duration;

use huginn_core::geometry::Rect;
use huginn_core::window::WindowId;
use raven_desktop::{Entry, Icons, Pixmaps};

use crate::anim::Reveal;
use crate::canvas::{Canvas, Panel};
use crate::launcher::{Layout, Metrics};
use crate::text::Text;

/// How long the pointer must stay at the edge before the dock appears.
///
/// Long enough not to fire on a pointer passing through, short enough that
/// someone reaching for the dock does not think it is broken.
const HOVER_DELAY: Duration = Duration::from_millis(220);

/// How tall the strip at the bottom edge is that counts as "at the dock".
///
/// A band rather than the last row of pixels: a pointer moved quickly can jump
/// several pixels between motion events and never land on row `height - 1`.
const EDGE_BAND: i32 = 4;

/// How long the pointer must rest on a tile before its windows are pictured.
///
/// Longer than [`HOVER_DELAY`]: the pointer crosses tiles on its way to the
/// one it wants, and a picture for each of them on the way is a flicker, not
/// a preview. Once one is up, moving to the next tile switches at once — the
/// delay is for arriving at the dock, not for browsing along it.
pub(crate) const PREVIEW_DELAY: Duration = Duration::from_millis(400);

/// One thing in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// Index into the application list, or `None` for the launcher button.
    pub entry: Option<usize>,
    /// Whether a window of this application is open.
    pub running: bool,
    /// One particular window, when the strip is listing windows rather than
    /// applications — the switcher's tiles. `None` in the ordinary dock.
    pub window: Option<WindowId>,
}

impl Item {
    /// The launcher button, which is always leftmost. §4.
    pub(crate) const fn launcher() -> Self {
        Self {
            entry: None,
            running: false,
            window: None,
        }
    }

    /// The launcher button: no application and no window. A window tile
    /// whose application has no desktop entry is not it, though it too has
    /// no entry — see [`alt_tab_items`].
    pub(crate) const fn is_launcher(&self) -> bool {
        self.entry.is_none() && self.window.is_none()
    }
}

/// Applications pinned to the dock, by desktop file stem.
///
/// Compiled in, like everything else — there is no configuration. Whatever
/// RavenLinux ships as its defaults belongs here.
///
/// By *stem*, which is why the file manager appears here under a reverse-DNS
/// name and the terminal does not: `scan_applications` keys entries by their
/// file name, and RavenLinux installs
/// `/usr/share/applications/com.ravenfilemanager.Raven.desktop` — the name GTK
/// requires of an application's entry, since it must match the application id.
/// Writing `ravenfilemanager` here, the binary's name, matches nothing.
///
/// A name that resolves to no entry is skipped rather than drawn dead: the
/// loop below only pushes an item when `position` finds one, so an image built
/// with `FILEMANAGER_SKIP=1` gets a dock with one icon instead of a dock with
/// an icon that launches nothing.
const PINNED: &[&str] = &["raven-terminal", "com.ravenfilemanager.Raven"];

/// Whether `entry` is the application that owns a window with `app_id`.
///
/// Three ways, because none of them is reliable alone:
///
/// - `StartupWMClass`, which exists precisely to say "my windows call
///   themselves this". Authoritative when present, and often absent.
/// - The desktop file's stem, which matches for the many applications whose
///   `app_id` is their file name.
/// - The last dotted component, so `org.gnome.Nautilus` matches `nautilus.desktop`.
///
/// All case-insensitive: an `app_id` is whatever a toolkit felt like sending,
/// and the same application can capitalise it differently between versions.
pub(crate) fn matches(entry: &Entry, app_id: &str) -> bool {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    if entry
        .startup_wm_class
        .as_deref()
        .is_some_and(|class| same(class, app_id))
    {
        return true;
    }
    let Some(stem) = entry.path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    same(stem, app_id)
        || app_id
            .rsplit('.')
            .next()
            .is_some_and(|tail| same(stem, tail))
}

/// Build the strip: the launcher, then pinned applications, then anything else
/// that is running.
///
/// Pinned applications keep their place whether or not they are running, so the
/// dock does not reshuffle under the pointer when a window opens — a strip
/// whose contents move as you reach for them is worse than no strip.
pub(crate) fn items(apps: &[Entry], running: &[String]) -> Vec<Item> {
    let is_running = |entry: &Entry| running.iter().any(|id| matches(entry, id));

    let mut items = vec![Item::launcher()];
    let mut placed: Vec<usize> = Vec::new();

    for name in PINNED {
        if let Some(index) = apps.iter().position(|e| {
            e.path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
        }) {
            placed.push(index);
            items.push(Item {
                entry: Some(index),
                running: is_running(&apps[index]),
                window: None,
            });
        }
    }

    // One item per running window, not one per entry that could have started
    // it. The same application is often installed twice — a flatpak and a
    // native package both ship `Brave-browser` — and both entries match the
    // one running `app_id`, so an unclaimed sweep puts two Braves in the dock
    // for a single window.
    let mut claimed: Vec<&str> = Vec::new();
    for index in &placed {
        for id in running {
            if matches(&apps[*index], id) && !claimed.contains(&id.as_str()) {
                claimed.push(id);
            }
        }
    }

    for (index, entry) in apps.iter().enumerate() {
        if placed.contains(&index) {
            continue;
        }
        let Some(id) = running
            .iter()
            .find(|id| matches(entry, id) && !claimed.contains(&id.as_str()))
        else {
            continue;
        };
        claimed.push(id);
        items.push(Item {
            entry: Some(index),
            running: true,
            window: None,
        });
    }
    items
}

/// Build the switcher's strip: the launcher, then one tile per window.
///
/// The ordinary dock is one item per *application* — §4 does not want a
/// window list. The switcher is the exception, because its whole job is to
/// bring back something put away, and two minimized windows of one browser
/// are two different things to bring back. Tiles keep the dock's order —
/// pinned applications first, then the rest in application-list order — and
/// windows of one application sit together in the order they were minimized.
///
/// A window whose `app_id` matches no installed application has no icon to
/// draw and is skipped, as it is from the dock.
pub(crate) fn window_items(apps: &[Entry], windows: &[(WindowId, String)]) -> Vec<Item> {
    let mut items = vec![Item::launcher()];
    let mut claimed: Vec<WindowId> = Vec::new();

    let place = |index: usize, items: &mut Vec<Item>, claimed: &mut Vec<WindowId>| {
        for (id, app_id) in windows {
            if !claimed.contains(id) && matches(&apps[index], app_id) {
                claimed.push(*id);
                items.push(Item {
                    entry: Some(index),
                    running: true,
                    window: Some(*id),
                });
            }
        }
    };

    let mut placed: Vec<usize> = Vec::new();
    for name in PINNED {
        if let Some(index) = apps.iter().position(|e| {
            e.path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
        }) {
            placed.push(index);
            place(index, &mut items, &mut claimed);
        }
    }
    for index in 0..apps.len() {
        if !placed.contains(&index) {
            place(index, &mut items, &mut claimed);
        }
    }
    items
}

/// Build the Alt-Tab strip: one tile per window, in the order given, and no
/// launcher button.
///
/// The order is the caller's — most recently focused first — and is kept,
/// since the whole point of the strip is that the first tile is the window
/// you were just in. Unlike [`window_items`] a window whose `app_id` matches
/// no installed application keeps its tile: the strip exists to reach every
/// window, and a tile with no icon still has a caption to name it.
pub(crate) fn alt_tab_items(apps: &[Entry], windows: &[(WindowId, Option<String>)]) -> Vec<Item> {
    windows
        .iter()
        .map(|(id, app_id)| Item {
            entry: app_id
                .as_deref()
                .and_then(|app_id| apps.iter().position(|entry| matches(entry, app_id))),
            running: true,
            window: Some(*id),
        })
        .collect()
}

/// The dock's visibility, and what the pointer is doing about it.
#[derive(Debug)]
pub(crate) struct Dock {
    /// 0 hidden, 1 fully up.
    reveal: Reveal,
    /// When the pointer arrived at the bottom edge, if it is still there.
    at_edge_since: Option<Duration>,
    /// Whether the pointer is over the dock itself, which keeps it up.
    hovered: bool,
    /// What `desktop.toml` asked for. Kept here because the dock's own
    /// behaviour depends on one of them — see [`Prefs::auto_hide`] — and
    /// because it is then the one place the drawing reads them from.
    prefs: Prefs,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            reveal: Reveal::hidden(),
            at_edge_since: None,
            hovered: false,
            prefs: Prefs::default(),
        }
    }
}

impl Dock {
    pub(crate) fn prefs(&self) -> Prefs {
        self.prefs
    }

    /// Take what the settings file says. Returns whether anything changed,
    /// since a changed dock has to be laid out and painted again.
    pub(crate) fn set_prefs(&mut self, prefs: Prefs) -> bool {
        let changed = self.prefs != prefs;
        self.prefs = prefs;
        changed
    }

    /// How far up the dock is, 0..=1.
    ///
    /// Always fully up when it does not hide itself. A dock that was told to
    /// stay has nothing to animate, and the reveal is left where it was so
    /// that turning the setting back on carries on from there.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
        if !self.prefs.auto_hide {
            return 1.0;
        }
        self.reveal.value(now)
    }

    pub(crate) fn is_visible(&self, now: Duration) -> bool {
        self.reveal(now) > 0.001
    }

    pub(crate) fn is_animating(&self, now: Duration) -> bool {
        !self.reveal.is_settled(now)
    }

    /// Tell the dock where the pointer is. Returns whether anything changed.
    ///
    /// `over_dock` is whether the pointer is inside the dock's own rectangle,
    /// which keeps it up once it is up — otherwise moving onto the dock to
    /// click something would take it away, since the pointer has left the edge
    /// band by definition.
    pub(crate) fn pointer_moved(
        &mut self,
        y: i32,
        output: Rect,
        over_dock: bool,
        now: Duration,
        motion: crate::settings::Motion,
    ) -> bool {
        self.hovered = over_dock;
        if !self.prefs.auto_hide {
            return false;
        }
        let at_edge = y >= output.y() + output.h() - EDGE_BAND;

        if at_edge {
            // Timed from arrival, not restarted on every motion event: a
            // pointer resting at the edge produces a stream of them, and
            // restarting would mean the dock never appeared at all.
            self.at_edge_since.get_or_insert(now);
        } else {
            self.at_edge_since = None;
        }

        let should_show = over_dock
            || self
                .at_edge_since
                .is_some_and(|since| now.saturating_sub(since) >= HOVER_DELAY);

        if self.reveal.is_showing() == should_show {
            return false;
        }
        // The same spring as every panel, critically damped both ways: a
        // pointer resting at an edge did not throw anything, and a dock that
        // leaves with a bounce reads as trying to follow the pointer off the
        // screen. What the pointer gets is a dock that turns round mid-rise
        // if it leaves, with the speed it had.
        if should_show {
            self.reveal.open(now, motion.is_reduced());
        } else {
            self.reveal.close(now, motion.is_reduced());
        }
        true
    }

    /// Hide immediately, without animating. For a window going fullscreen.
    pub(crate) fn hide_now(&mut self) {
        self.reveal.hide_now();
        self.at_edge_since = None;
        self.hovered = false;
    }

    /// Which item is under `x`, given the dock's rectangle.
    ///
    /// Derives the icon pitch from the rectangle's *height* rather than
    /// dividing its width into equal slots. The width is not a whole number of
    /// slots — there is a leading gap — so dividing by the count puts the
    /// right-hand pixels in a slot past the end, and the last item becomes
    /// unclickable. That is a bug you find by clicking, not by reading.
    pub(crate) fn item_at(&self, x: i32, rect: Rect, count: usize) -> Option<usize> {
        if count == 0 || x < rect.x() || x >= rect.x() + rect.w() {
            return None;
        }
        // `placement` builds the height as icon + gap*2 from one scale, so the
        // scale can be recovered from it exactly.
        let scale = rect.h() as f32 / (self.prefs.icon + GAP * 2.0);
        let pitch = (self.prefs.icon + GAP) * scale;
        if pitch <= 0.0 {
            return None;
        }
        let offset = (x - rect.x()) as f32 - GAP * scale;
        let index = (offset / pitch).floor().max(0.0) as usize;
        Some(index.min(count - 1))
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Icon size at a 1080p output, when nothing says otherwise.
///
/// The compiled-in default; [`Prefs::icon`] is what the drawing actually
/// uses, and `desktop.toml` may move it within [`ICON_RANGE`].
const ICON: f32 = 44.0;
/// Space around each icon.
const GAP: f32 = 10.0;
/// Corner radius, as a fraction of the dock's height.
const RADIUS: f32 = 0.28;
/// Distance from the bottom of the screen when fully up.
const MARGIN: f32 = 12.0;
const ALPHA: u8 = crate::theme::PANEL_ALPHA;
/// Text size of the switcher's title caption at a 1080p output.
pub(crate) const CAPTION_SIZE: f32 = 14.0;
/// The most of the screen, each way, the switcher's thumbnail may take.
///
/// Big enough to recognise a page by; small enough that the strip beneath it
/// is still the thing you are looking at.
const PREVIEW: f32 = 0.32;
/// Border around the thumbnail at a 1080p output.
const PREVIEW_BORDER: f32 = 6.0;
/// What the menu row that closes an application says.
///
/// "Quit", not "Close": closing is what a window does, and this asks every
/// window the application has. The label is the one the mockups use and the
/// one every other desktop uses for the same thing.
pub(crate) const QUIT: &str = "Quit";
/// How far from the pointer, in slots, magnification reaches.
///
/// Two: the icon under the pointer and its neighbours lift, and the ones past
/// them stay put. Reaching further makes the whole dock heave whenever the
/// pointer crosses it, which is motion that tells you nothing.
const MAG_REACH: f32 = 2.0;
/// How far the pointer moves along the dock before the strip is painted
/// again, in logical pixels.
///
/// The icons are magnified around the pointer, so tracking it exactly would
/// repaint on every motion event a device cared to send. Three pixels is
/// some twenty steps across a slot, which no eye reads as stepping.
pub(crate) const POINTER_STEP: f32 = 3.0;
/// The hover label's text size at a 1080p output.
const LABEL_SIZE: f32 = 13.0;
/// Air between the magnified icon and the label above it.
const LABEL_GAP: f32 = 8.0;

/// What the dock takes from `desktop.toml`, resolved and held to its bounds.
///
/// Behaviour, not appearance: how big the icons are, how much they lift, and
/// whether the dock gets out of the way. There is deliberately no background
/// or corner-radius setting — the dock is [`crate::theme`]'s one material at
/// the one alpha, the same as the launcher and the pin bar, and a dock that
/// could stop matching them would be a dock that no longer looked like part
/// of this desktop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Prefs {
    /// Icon size at a 1080p output, in logical pixels.
    pub icon: f32,
    /// How much the icon under the pointer grows. 1.0 is no magnification.
    pub magnify: f32,
    /// Whether the item under the pointer is named.
    pub labels: bool,
    /// Whether a running application is marked with a dot.
    pub dots: bool,
    /// Whether the dock hides itself when the pointer leaves the edge.
    pub auto_hide: bool,
}

/// What an icon size may be set to, in logical pixels at 1080p.
///
/// Small enough at the bottom that a full dock still fits a narrow screen,
/// large enough at the top to be reachable, and never so large that the
/// magnified icon would not fit the headroom the panel reserves.
pub(crate) const ICON_RANGE: std::ops::RangeInclusive<u32> = 28..=72;
/// What magnification may be set to, as a percentage of the icon size.
pub(crate) const MAGNIFY_RANGE: std::ops::RangeInclusive<u32> = 100..=200;

impl Default for Prefs {
    fn default() -> Self {
        Self {
            icon: ICON,
            magnify: 1.35,
            labels: true,
            dots: true,
            auto_hide: true,
        }
    }
}

impl Prefs {
    /// From what the file said, each value held to its range. A file is not
    /// a trusted source of geometry: an icon size of 4000 is a dock wider
    /// than the screen, and one of 0 is a division by nothing.
    pub(crate) fn new(icon: u32, magnify: u32, labels: bool, dots: bool, auto_hide: bool) -> Self {
        Self {
            icon: icon.clamp(*ICON_RANGE.start(), *ICON_RANGE.end()) as f32,
            magnify: magnify.clamp(*MAGNIFY_RANGE.start(), *MAGNIFY_RANGE.end()) as f32 / 100.0,
            labels,
            dots,
            auto_hide,
        }
    }
}

/// Which strip is being drawn, and what it needs to know.
///
/// One function draws three things — the dock, the switcher and the Alt-Tab
/// strip — because they are one strip of icons in one material. They differ
/// in exactly two ways, and this is those two: the dock magnifies around a
/// pointer and names what is under it, and the other two highlight a tile the
/// keyboard chose and never magnify. A switcher whose tiles grew under a
/// passing pointer would be a switcher that moved the thing you were aiming
/// at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Strip {
    /// The dock proper. `pointer` is where the pointer is along the dock, in
    /// logical pixels from its left edge, or `None` when it is elsewhere.
    Dock { pointer: Option<f32> },
    /// The application switcher or the Alt-Tab strip.
    Switcher { selected: Option<usize> },
}

/// How much room above the bar the dock's panel keeps for a magnified icon
/// and the label over it, in logical pixels at 1080p.
///
/// Always reserved, whether or not the pointer is on the dock: the panel is
/// placed by its own size, and a canvas that grew the moment the pointer
/// arrived would move the dock out from under it.
pub(crate) fn headroom(prefs: Prefs) -> f32 {
    lift(prefs)
        + if prefs.labels {
            LABEL_GAP + label_height()
        } else {
            0.0
        }
}

/// How much room the panel keeps at each *end* of the bar, in logical pixels
/// at 1080p.
///
/// An icon grows about its own centre, so the first and last ones grow past
/// the ends of the bar by half their growth. Without this they are cut off at
/// the edge of the canvas — which is the one place magnification would look
/// broken rather than merely absent.
pub(crate) fn sideroom(prefs: Prefs) -> f32 {
    lift(prefs) / 2.0
}

/// How much taller a fully magnified icon is than a resting one.
fn lift(prefs: Prefs) -> f32 {
    prefs.icon * prefs.magnify - prefs.icon
}

/// The hover label's own height at 1080p: the text with air above and below.
fn label_height() -> f32 {
    LABEL_SIZE * 1.9
}

/// How much the icon `slots` away from the pointer grows.
///
/// A raised cosine, which is 1 under the pointer and eases to 0 at
/// [`MAG_REACH`] with no corner at either end — a linear falloff puts a
/// visible crease in the row of icons where the ramp starts.
fn magnification(slots: f32, magnify: f32) -> f32 {
    if magnify <= 1.0 {
        return 1.0;
    }
    let t = (slots.abs() / MAG_REACH).clamp(0.0, 1.0);
    let falloff = 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
    1.0 + (magnify - 1.0) * falloff
}

/// The dock's rectangle on `output` at the current reveal.
///
/// The *bar*: what the pointer hits, what previews are anchored to, and what
/// the launcher grows out of. The panel drawn for it is taller — see
/// [`panel_placement`].
///
/// Slides up from below the bottom edge, so a partly-revealed dock is partly
/// off screen rather than partly transparent — an object arriving, not an
/// image fading in.
pub(crate) fn placement(output: Rect, items: usize, reveal: f32, prefs: Prefs) -> Rect {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let (icon, gap, margin) = (prefs.icon * scale, GAP * scale, MARGIN * scale);
    let h = (icon + gap * 2.0) as i32;
    let w = ((icon + gap) * items as f32 + gap) as i32;

    let x = output.x() + (output.w() - w) / 2;
    let resting = output.y() + output.h() - h - margin as i32;
    let hidden = output.y() + output.h();
    let y = hidden + ((resting - hidden) as f32 * reveal.clamp(0.0, 1.0)) as i32;
    Rect::from_xywh(x, y, w, h)
}

/// The rectangle the dock's *panel* is drawn into: the bar, plus the room
/// above it a magnified icon and its label need.
pub(crate) fn panel_placement(output: Rect, items: usize, reveal: f32, prefs: Prefs) -> Rect {
    let bar = placement(output, items, reveal, prefs);
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let head = (headroom(prefs) * scale) as i32;
    let side = (sideroom(prefs) * scale) as i32;
    Rect::from_xywh(
        bar.x() - side,
        bar.y() - head,
        bar.w() + side * 2,
        bar.h() + head,
    )
}

/// The dock's rectangle while it is acting as the application switcher.
pub(crate) fn centred_placement(output: Rect, items: usize, prefs: Prefs) -> Rect {
    let mut rect = placement(output, items, 1.0, prefs);
    rect.origin.y = output.y() + (output.h() - rect.h()) / 2;
    rect
}

/// The rectangle one item occupies, given the dock's own rectangle.
///
/// Where the launcher grows out of, and what a preview is anchored to. The
/// unmagnified slot in both cases: a preview that moved because the pointer
/// drifted would be a preview you cannot reach.
///
/// Derived from the dock's height rather than divided out of its width, for
/// the same reason [`Dock::item_at`] is — the width is not a whole number of
/// slots.
pub(crate) fn item_rect(dock: Rect, index: usize, prefs: Prefs) -> Rect {
    let scale = dock.h() as f32 / (prefs.icon + GAP * 2.0);
    let (icon, gap) = (prefs.icon * scale, GAP * scale);
    Rect::from_xywh(
        dock.x() + (gap + (icon + gap) * index as f32) as i32,
        dock.y() + gap as i32,
        icon as i32,
        icon as i32,
    )
}

/// Paint the dock for `output` at `density` pixels per logical one.
///
/// The rectangle it is drawn into comes from [`panel_placement`], in logical
/// pixels; this composes the same shape with `density` times the pixels each
/// way.
// One argument per thing painted; bundling them into a struct would name a
// type whose only meaning is "the arguments of this function".
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    items: &[Item],
    apps: &[Entry],
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    text: &mut Text,
    output: Rect,
    density: u32,
    strip: Strip,
    prefs: Prefs,
) -> Panel {
    let canvas = compose(
        items, apps, icons, pixmaps, text, output, density, strip, prefs,
    );
    Panel::from_canvas(&canvas, density.max(1))
}

/// Paint the strip and hand back the pixels.
///
/// Split from [`render`] so a test can look at what was drawn without going
/// through a renderer — and so that there is one drawing of the dock rather
/// than one for the screen and a second, quietly drifting one for the dump.
#[allow(clippy::too_many_arguments)]
fn compose(
    items: &[Item],
    apps: &[Entry],
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    text: &mut Text,
    output: Rect,
    density: u32,
    strip: Strip,
    prefs: Prefs,
) -> Canvas {
    let density = density.max(1);
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let (icon, gap) = (prefs.icon * scale, GAP * scale);
    let bar_h = icon + gap * 2.0;
    let bar_w = (icon + gap) * items.len() as f32 + gap;
    // The switcher is the bar and nothing else; only the dock lifts icons out
    // of it, upward and — at the two ends — outward.
    let (head, side) = match strip {
        Strip::Dock { .. } => (headroom(prefs) * scale, sideroom(prefs) * scale),
        Strip::Switcher { .. } => (0.0, 0.0),
    };
    let w = (bar_w + side * 2.0) as usize;
    let h = (head + bar_h) as usize;

    // The room around the bar is left transparent: it belongs to the desktop
    // until an icon grows into it.
    let mut canvas = Canvas::new(w.max(1), h.max(1));
    canvas.material(
        side as usize,
        head as usize,
        bar_w as usize,
        bar_h as usize,
        bar_h * RADIUS,
        ALPHA,
    );

    // Where the pointer is along the strip, as a fractional slot: slot `n`
    // covers `n..n+1`, which is exactly how [`Dock::item_at`] divides it.
    //
    // Magnifying about the slot's centre and not the icon's is what keeps
    // the icon that grows most and the item a click would take the *same*
    // item. A slot is an icon and the gap after it, so the two centres are
    // five pixels apart, and a pointer resting in that gap would otherwise
    // swell one icon while naming — and launching — its neighbour.
    let pointer = match strip {
        Strip::Dock { pointer } => pointer.map(|x| x * scale / density as f32),
        Strip::Switcher { .. } => None,
    };
    let pitch = icon + gap;
    let at = pointer.map(|px| (px - gap) / pitch);
    let hovered =
        at.and_then(|at| (at >= 0.0 && (at as usize) < items.len()).then_some(at as usize));
    let selected = match strip {
        Strip::Switcher { selected } => selected,
        Strip::Dock { .. } => None,
    };

    for (index, item) in items.iter().enumerate() {
        let x = side + gap + pitch * index as f32;
        let factor = match at {
            Some(at) => magnification(index as f32 + 0.5 - at, prefs.magnify),
            None => 1.0,
        };
        if selected == Some(index) {
            // The accent wash, ringed in the accent — the launcher's tiles
            // are chosen the same way.
            let inset = (4.0 * scale).max(3.0);
            let (sx, sy, sw) = (
                (x - inset) as usize,
                (head + gap - inset) as usize,
                (icon + inset * 2.0) as usize,
            );
            canvas.fill_rounded(sx, sy, sw, sw, icon * 0.26, crate::theme::selection());
            canvas.stroke_rounded(
                sx,
                sy,
                sw,
                sw,
                icon * 0.26,
                (1.5 * scale).max(1.5),
                crate::theme::accent(),
            );
        }
        // A magnified icon grows upward from where it would have sat, so the
        // row of icons keeps one baseline and only the lift moves.
        let grown = icon * factor;
        let ix = x + (icon - grown) / 2.0;
        let iy = head + gap + icon - grown;
        if item.is_launcher() {
            // Drawn rather than themed: the launcher is not an installed
            // application and has no `.desktop` file to take an icon from.
            draw_launcher_glyph(&mut canvas, ix, iy, grown, scale);
        } else if item.entry.is_none() {
            // A window of an application with no desktop entry, in the
            // Alt-Tab strip: nothing to take an icon from, so a plain
            // window shape stands in. Its caption says what it is.
            draw_window_glyph(&mut canvas, ix, iy, grown, scale);
        } else if let Some(pixmap) = item
            .entry
            .and_then(|i| apps.get(i))
            .and_then(|e| e.icon.as_deref())
            // Looked up at its logical size for the output's density, which
            // is how icon themes file their 2× artwork; rasterized at the
            // real pixel size either way.
            .and_then(|name| icons.find(name, grown as u32 / density, density))
            .and_then(|path| pixmaps.get(&path, grown as u32))
        {
            canvas.blit(ix as usize, iy as usize, pixmap);
        }

        // Running state: a small mark under the icon. §4 asks for "a subtle
        // indicator, not a separate region" — the same slot, annotated.
        if item.running && prefs.dots {
            let dot = (4.0 * scale).max(3.0);
            canvas.fill_rounded(
                (x + icon / 2.0 - dot / 2.0) as usize,
                (head + bar_h - gap * 0.6) as usize,
                dot as usize,
                dot as usize,
                dot / 2.0,
                crate::theme::accent(),
            );
        }
    }

    // The label last, so it sits over any icon that lifted into its row.
    if prefs.labels
        && let Some(index) = hovered
        && let Some(name) = items.get(index).map(|item| label_for(item, apps))
    {
        let x = side + gap + pitch * index as f32;
        let factor = match at {
            Some(at) => magnification(index as f32 + 0.5 - at, prefs.magnify),
            None => 1.0,
        };
        let top = head + gap + icon - icon * factor;
        draw_label(
            &mut canvas,
            text,
            &name,
            x + icon / 2.0,
            top - LABEL_GAP * scale,
            w as f32,
            scale,
        );
    }
    canvas
}

/// What the hover label says for `item`.
fn label_for(item: &Item, apps: &[Entry]) -> String {
    if item.is_launcher() {
        return "Applications".to_owned();
    }
    item.entry
        .and_then(|i| apps.get(i))
        .map(|entry| entry.name.clone())
        .unwrap_or_default()
}

/// The hover label: a pill of the desktop's material, centred over `centre`
/// with its bottom at `bottom`, kept inside a canvas `width` wide.
fn draw_label(
    canvas: &mut Canvas,
    text: &mut Text,
    label: &str,
    centre: f32,
    bottom: f32,
    width: f32,
    scale: f32,
) {
    if !text.is_usable() || label.is_empty() {
        return;
    }
    let size = LABEL_SIZE * scale;
    let pad = GAP * scale * 0.8;
    let h = label_height() * scale;
    let label = crate::launcher::fit(text, label, size, width - pad * 2.0);
    let (tw, _) = text.measure(&label, size);
    let w = tw + pad * 2.0;
    // Clamped rather than allowed to hang off: the canvas is exactly as wide
    // as the dock, so a name wider than its slot would be cut at the edge.
    let x = (centre - w / 2.0).clamp(0.0, (width - w).max(0.0));
    let y = (bottom - h).max(0.0);
    canvas.material(
        x as usize,
        y as usize,
        w as usize,
        h as usize,
        h / 2.0,
        ALPHA,
    );
    text.draw(
        canvas,
        &label,
        size,
        (x + pad) as i32,
        (y + (h - size * 1.35) / 2.0) as i32,
        crate::theme::TEXT,
    );
}

/// The context menu for one dock item: what the application offers, and then
/// what the desktop offers to do with it.
///
/// Drawn by [`crate::menu`], which is where the pin bar's menu comes from
/// too — the two are the same object opened from different places, and a
/// second copy of the drawing is a second copy to keep in step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn menu(
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    title: (&str, Option<&str>),
    sections: &[&[crate::menu::Row]],
    selected: Option<usize>,
    output: Rect,
    density: u32,
) -> (Panel, Layout) {
    let density = density.max(1);
    let m = Metrics::for_output(output, density);
    let (w, h) = crate::menu::size(&m, sections);
    let mut canvas = Canvas::new((w as usize).max(1), (h as usize).max(1));
    let hits = crate::menu::draw(
        &mut canvas,
        text,
        &m,
        icons,
        pixmaps,
        title,
        sections,
        selected,
        (0.0, 0.0),
    );
    let layout = Layout {
        size: (canvas.stride as i32, canvas.height as i32),
        menu_hits: hits.into_iter().zip(0..).collect(),
        ..Layout::default()
    };
    (Panel::from_canvas(&canvas, density), layout)
}

/// Where that menu goes: above the bar, centred on the item it belongs to,
/// and pushed back onto the screen rather than allowed to hang off it.
///
/// Above the *bar* and not above the icon, because the icon under the
/// pointer is magnified and the menu must not move as it grows.
pub(crate) fn menu_placement(panel: (i32, i32), item: Rect, bar: Rect, output: Rect) -> Rect {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let gap = (GAP * scale) as i32;
    let (w, h) = panel;
    let x = (item.x() + (item.w() - w) / 2).clamp(output.x(), (output.right() - w).max(output.x()));
    let y = (bar.y() - gap - h).max(output.y());
    Rect::from_xywh(x, y, w, h)
}

/// A window's title, as a small pill to sit under its thumbnail, no wider
/// than `max_width` logical pixels.
///
/// A separate panel rather than part of the thumbnail's backing: the backing
/// is sized to the picture, and the renderer scales each panel to its own
/// rectangle, so text drawn into it would stretch with the picture.
///
/// `None` when there is no font to draw with, or nothing to say.
pub(crate) fn caption(
    text: &mut Text,
    title: &str,
    max_width: i32,
    output: Rect,
    density: u32,
) -> Option<Panel> {
    if !text.is_usable() || title.is_empty() {
        return None;
    }
    let density = density.max(1);
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let size = CAPTION_SIZE * scale;
    let pad = GAP * scale;
    // Long titles are cut rather than wrapped: the pill is a label, not a
    // document, and it must not grow wider than what it labels.
    let max_w = (max_width as f32 * density as f32 - pad * 2.0).max(size * 2.0);
    let mut title = title.to_owned();
    let (mut w, h) = text.measure(&title, size);
    while w > max_w && title.chars().count() > 1 {
        title.pop();
        while !title.is_char_boundary(title.len()) {
            title.pop();
        }
        let (tw, _) = text.measure(&format!("{title}…"), size);
        w = tw;
        if w <= max_w {
            title.push('…');
            break;
        }
    }
    let (pw, ph) = ((w + pad * 2.0) as usize, (h + pad) as usize);
    let mut canvas = Canvas::new(pw.max(1), ph.max(1));
    canvas.material(0, 0, pw, ph, ph as f32 * 0.5, ALPHA);
    text.draw(
        &mut canvas,
        &title,
        size,
        pad as i32,
        (pad / 2.0) as i32,
        crate::theme::TEXT,
    );
    Some(Panel::from_canvas(&canvas, density))
}

/// Where a caption goes: centred under `above`, a half margin below it.
pub(crate) fn caption_placement(panel: &Panel, above: Rect, output: Rect) -> Rect {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let (w, h) = panel.size();
    Rect::from_xywh(
        above.x() + (above.w() - w) / 2,
        above.y() + above.h() + (MARGIN * scale * 0.5) as i32,
        w,
        h,
    )
}

/// How much room a caption needs under a thumbnail's backing: its own height
/// plus the half margin [`caption_placement`] leaves.
pub(crate) fn caption_room(panel: &Panel, output: Rect) -> i32 {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    panel.size().1 + (MARGIN * scale * 0.5) as i32
}

/// Where a row of thumbnails goes, for windows of the given sizes.
///
/// Above the strip, the row centred on `anchor_x` — the highlighted tile in the
/// switcher, the hovered one in the dock — and pushed back inside the screen
/// if that would put it off an edge. Each thumbnail keeps its window's aspect
/// and is no larger than [`PREVIEW`] of the screen each way; a row too wide
/// for the screen is shrunk as a whole, so its members stay comparable.
///
/// Bottom-aligned rather than centred: a landscape and a portrait window side
/// by side should stand on one line, like things on a shelf. `reserve` is
/// room left under that line, for captions.
pub(crate) fn preview_row(
    windows: &[(i32, i32)],
    anchor_x: i32,
    dock: Rect,
    output: Rect,
    reserve: i32,
) -> Vec<Rect> {
    if windows.is_empty() {
        return Vec::new();
    }
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let gap = MARGIN * 2.0 * scale;
    let (max_w, max_h) = (output.w() as f32 * PREVIEW, output.h() as f32 * PREVIEW);
    let mut sizes: Vec<(f32, f32)> = windows
        .iter()
        .map(|&(w, h)| {
            let (ww, wh) = (w.max(1) as f32, h.max(1) as f32);
            let fit = (max_w / ww).min(max_h / wh);
            (ww * fit, wh * fit)
        })
        .collect();
    let row_w = |sizes: &[(f32, f32)]| {
        sizes.iter().map(|s| s.0).sum::<f32>() + gap * (sizes.len() - 1) as f32
    };
    let room = output.w() as f32 - gap * 2.0;
    let total = row_w(&sizes);
    if total > room {
        // The gaps stay as they are; only the pictures give way.
        let gaps = gap * (sizes.len() - 1) as f32;
        let shrink = ((room - gaps) / (total - gaps)).max(0.05);
        for size in &mut sizes {
            size.0 *= shrink;
            size.1 *= shrink;
        }
    }
    let total = row_w(&sizes);
    // `min` then `max` rather than `clamp`: after shrinking, the two bounds
    // meet, and rounding can put them a hair the wrong way round, which
    // `clamp` treats as a panic rather than an answer.
    let left = (anchor_x as f32 - total / 2.0)
        .min((output.x() + output.w()) as f32 - gap - total)
        .max(output.x() as f32 + gap);
    let bottom = dock.y() as f32 - gap - reserve.max(0) as f32;
    let mut x = left;
    sizes
        .iter()
        .map(|&(w, h)| {
            let rect = Rect::from_xywh(
                x as i32,
                (bottom - h) as i32,
                (w as i32).max(1),
                (h as i32).max(1),
            );
            x += w + gap;
            rect
        })
        .collect()
}

/// The backing's rectangle: `frame` plus a border on every side.
pub(crate) fn preview_backing(frame: Rect, output: Rect) -> Rect {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let border = (PREVIEW_BORDER * scale) as i32;
    Rect::from_xywh(
        frame.x() - border,
        frame.y() - border,
        frame.w() + border * 2,
        frame.h() + border * 2,
    )
}

/// The backing drawn behind the thumbnail, to the size [`preview_backing`]
/// gives it at `density`.
pub(crate) fn preview_frame(frame: Rect, output: Rect, density: u32) -> Panel {
    let density = density.max(1);
    let backing = preview_backing(frame, output);
    let (w, h) = (
        (backing.w() as u32 * density) as usize,
        (backing.h() as u32 * density) as usize,
    );
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let mut canvas = Canvas::new(w.max(1), h.max(1));
    canvas.material(0, 0, w, h, PREVIEW_BORDER * scale * 1.5, ALPHA);
    Panel::from_canvas(&canvas, density)
}

/// A grid of squares, for the launcher button.
fn draw_launcher_glyph(canvas: &mut Canvas, x: f32, y: f32, size: f32, scale: f32) {
    let cell = size / 3.4;
    let step = size / 2.6;
    let inset = (size - (step + cell)) / 2.0;
    for row in 0..2 {
        for col in 0..2 {
            canvas.fill_rounded(
                (x + inset + step * col as f32) as usize,
                (y + inset + step * row as f32) as usize,
                cell as usize,
                cell as usize,
                (2.0 * scale).max(1.0),
                crate::theme::accent(),
            );
        }
    }
}

/// A window with no icon of its own: a rounded outline with a bar across the
/// top, in the body text colour at half strength — plainly a window, plainly
/// not an application icon.
fn draw_window_glyph(canvas: &mut Canvas, x: f32, y: f32, size: f32, scale: f32) {
    let inset = size * 0.18;
    let (w, h) = (size - inset * 2.0, size - inset * 2.0);
    let line = (2.0 * scale).max(1.5);
    let colour = crate::theme::TEXT.with_alpha(0x80);
    let radius = (3.0 * scale).max(2.0);
    let (left, top) = (x + inset, y + inset);
    // Four edges rather than a filled shape, so it reads as a frame.
    canvas.fill_rounded(
        left as usize,
        top as usize,
        w as usize,
        line as usize,
        radius,
        colour,
    );
    canvas.fill_rounded(
        left as usize,
        (top + h - line) as usize,
        w as usize,
        line as usize,
        radius,
        colour,
    );
    canvas.fill_rounded(
        left as usize,
        top as usize,
        line as usize,
        h as usize,
        radius,
        colour,
    );
    canvas.fill_rounded(
        (left + w - line) as usize,
        top as usize,
        line as usize,
        h as usize,
        radius,
        colour,
    );
    // The title bar, a little way down from the top edge.
    let bar = (line * 1.5).max(2.0);
    canvas.fill_rounded(
        left as usize,
        (top + line * 2.0) as usize,
        w as usize,
        bar as usize,
        0.0,
        colour,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, stem: &str, wm_class: Option<&str>) -> Entry {
        Entry {
            name: name.to_owned(),
            comment: None,
            generic_name: None,
            icon: None,
            exec: "/bin/true".to_owned(),
            categories: Vec::new(),
            keywords: Vec::new(),
            terminal: false,
            startup_wm_class: wm_class.map(str::to_owned),
            path: PathBuf::from(format!("/apps/{stem}.desktop")),
            actions: Vec::new(),
        }
    }

    fn apps() -> Vec<Entry> {
        vec![
            entry("Raven Terminal", "raven-terminal", Some("raven-terminal")),
            entry("Files", "org.gnome.Nautilus", None),
            entry("Firefox", "firefox", Some("Navigator")),
        ]
    }

    /// Two window ids, allocated the only way there is: by a `Space`.
    fn ids() -> (WindowId, WindowId) {
        let mut space = huginn_core::Space::new(SCREEN);
        (space.open_window(), space.open_window())
    }

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);
    const FULL: crate::settings::Motion = crate::settings::Motion::Full;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn startup_wm_class_wins_when_it_is_there() {
        assert!(matches(&apps()[2], "Navigator"), "Firefox's real app_id");
        assert!(
            matches(&apps()[2], "navigator"),
            "case is not authoritative"
        );
    }

    #[test]
    fn the_desktop_file_stem_matches_when_there_is_no_wm_class() {
        assert!(matches(&apps()[1], "org.gnome.Nautilus"));
    }

    #[test]
    fn a_reverse_dns_app_id_matches_its_last_component() {
        // `org.gnome.Nautilus` is a common app_id for `nautilus.desktop`.
        let nautilus = entry("Files", "nautilus", None);
        assert!(matches(&nautilus, "org.gnome.Nautilus"));
    }

    #[test]
    fn an_unrelated_app_id_does_not_match() {
        assert!(!matches(&apps()[0], "chromium"));
        assert!(!matches(&apps()[0], ""));
    }

    #[test]
    fn the_launcher_is_always_first() {
        // §4: "The leftmost dock item."
        let items = items(&apps(), &[]);
        assert!(items[0].is_launcher());
    }

    #[test]
    fn pinned_applications_appear_whether_or_not_they_run() {
        let items = items(&apps(), &[]);
        assert_eq!(items.len(), 2, "the pinned terminal is missing");
        assert_eq!(items[1].entry, Some(0));
        assert!(!items[1].running);
    }

    #[test]
    fn a_pinned_application_that_is_not_running_is_still_launchable() {
        // Clicking it has to start it, so the item must carry an entry whose
        // Exec resolves to something runnable — not just a name to draw.
        let apps = apps();
        let items = items(&apps, &[]);
        let item = items
            .iter()
            .find(|i| !i.is_launcher())
            .expect("pinned item");
        assert!(!item.running);
        let entry = item
            .entry
            .and_then(|i| apps.get(i))
            .expect("resolves to an entry");
        assert!(
            entry.argv(&[]).is_some(),
            "nothing to run for {}",
            entry.name
        );
    }

    #[test]
    fn a_running_application_appears_beside_the_pinned_ones() {
        // The dock is the taskbar: one strip, not two regions.
        let items = items(&apps(), &["Navigator".to_owned()]);
        assert_eq!(items.len(), 3);
        assert!(items.last().expect("firefox").running);
    }

    #[test]
    fn a_pinned_application_that_is_running_is_not_listed_twice() {
        let items = items(&apps(), &["raven-terminal".to_owned()]);
        assert_eq!(items.len(), 2, "the terminal appeared twice");
        assert!(items[1].running, "it is running and was not marked so");
    }

    #[test]
    fn one_running_window_makes_one_dock_item() {
        // Regression, found by drawing it: Brave is installed twice on the
        // development machine — a flatpak and a native package — and both
        // entries match the one running `app_id`, so the dock showed two
        // Braves for a single window.
        let twice = vec![
            entry("Raven Terminal", "raven-terminal", Some("raven-terminal")),
            entry("Brave", "brave-browser", Some("Brave-browser")),
            entry("Brave", "com.brave.Browser", Some("Brave-browser")),
        ];
        let items = items(&twice, &["Brave-browser".to_owned()]);
        let braves = items
            .iter()
            .filter(|i| i.entry.is_some_and(|e| e > 0))
            .count();
        assert_eq!(braves, 1, "one window produced {braves} dock items");
    }

    #[test]
    fn two_windows_of_the_same_application_still_make_one_item() {
        // A taskbar that grows an entry per window is a window list, which
        // §4 explicitly does not want.
        let items = items(&apps(), &["Navigator".to_owned(), "Navigator".to_owned()]);
        let firefoxes = items.iter().filter(|i| i.entry == Some(2)).count();
        assert_eq!(firefoxes, 1);
    }

    #[test]
    fn alt_tab_tiles_keep_the_given_order_and_have_no_launcher_button() {
        let (a, b) = ids();
        let items = alt_tab_items(
            &apps(),
            &[
                (b, Some("Navigator".to_owned())),
                (a, Some("raven-terminal".to_owned())),
            ],
        );
        assert_eq!(items.len(), 2, "one tile per window and nothing else");
        assert!(items.iter().all(|item| !item.is_launcher()));
        assert_eq!(items[0].window, Some(b), "the caller's order is kept");
        assert_eq!(items[0].entry, Some(2), "Firefox's tile carries its icon");
        assert_eq!(items[1].window, Some(a));
        assert_eq!(items[1].entry, Some(0));
        assert!(items.iter().all(|item| item.running));
    }

    #[test]
    fn alt_tab_keeps_a_window_with_no_desktop_entry() {
        let (a, _) = ids();
        let items = alt_tab_items(&apps(), &[(a, Some("mystery".to_owned())), (a, None)]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].entry, None, "no icon to show, but the tile stays");
        assert_eq!(items[0].window, Some(a));
        assert!(
            !items[0].is_launcher(),
            "an icon-less window tile is not the launcher button"
        );
        assert!(Item::launcher().is_launcher());
    }

    #[test]
    fn the_switcher_gives_each_window_of_one_application_its_own_tile() {
        // The one place a window list is wanted: two minimized browser
        // windows are two things to bring back, and a single tile could only
        // ever bring back the first.
        let (a, b) = ids();
        let items = window_items(
            &apps(),
            &[(a, "Navigator".to_owned()), (b, "Navigator".to_owned())],
        );
        let firefoxes: Vec<Option<WindowId>> = items
            .iter()
            .filter(|i| i.entry == Some(2))
            .map(|i| i.window)
            .collect();
        assert_eq!(firefoxes, vec![Some(a), Some(b)]);
        assert!(items[0].is_launcher(), "the launcher is still first");
    }

    #[test]
    fn switcher_tiles_keep_the_dock_order() {
        let (t, f) = ids();
        // Firefox minimized first, but the terminal is pinned and comes first.
        let items = window_items(
            &apps(),
            &[
                (f, "Navigator".to_owned()),
                (t, "raven-terminal".to_owned()),
            ],
        );
        let order: Vec<Option<WindowId>> = items.iter().map(|i| i.window).collect();
        assert_eq!(order, vec![None, Some(t), Some(f)]);
    }

    #[test]
    fn a_window_of_an_unknown_application_makes_no_tile() {
        let items = window_items(&apps(), &[(ids().0, "mystery".to_owned())]);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn the_preview_sits_above_the_switcher_and_keeps_the_aspect() {
        let dock = centred_placement(SCREEN, 4, Prefs::default());
        let anchor = dock.x() + dock.w() / 2;
        let frame = preview_row(&[(1600, 900)], anchor, dock, SCREEN, 0)[0];
        assert!(
            frame.y() + frame.h() < dock.y(),
            "the preview overlaps the strip"
        );
        assert!(frame.w() <= (SCREEN.w() as f32 * PREVIEW) as i32);
        assert!(frame.h() <= (SCREEN.h() as f32 * PREVIEW) as i32);
        let aspect = frame.w() as f32 / frame.h() as f32;
        assert!(
            (aspect - 16.0 / 9.0).abs() < 0.02,
            "aspect drifted to {aspect}"
        );
        let centre = frame.x() + frame.w() / 2;
        assert!((centre - anchor).abs() <= 1, "not centred on the anchor");
    }

    #[test]
    fn a_tall_window_is_limited_by_height_rather_than_width() {
        let dock = centred_placement(SCREEN, 4, Prefs::default());
        let frame = preview_row(&[(600, 1000)], dock.x(), dock, SCREEN, 0)[0];
        assert_eq!(frame.h(), (SCREEN.h() as f32 * PREVIEW) as i32);
        assert!(frame.w() < frame.h());
    }

    #[test]
    fn a_row_stands_on_one_line_and_does_not_overlap() {
        let dock = placement(SCREEN, 6, 1.0, Prefs::default());
        let row = preview_row(
            &[(1600, 900), (600, 1000), (800, 800)],
            960,
            dock,
            SCREEN,
            0,
        );
        assert_eq!(row.len(), 3);
        for pair in row.windows(2) {
            assert!(
                pair[0].x() + pair[0].w() < pair[1].x(),
                "thumbnails overlap"
            );
            assert_eq!(
                pair[0].y() + pair[0].h(),
                pair[1].y() + pair[1].h(),
                "bottoms differ"
            );
        }
        assert!(row.iter().all(|r| r.y() + r.h() < dock.y()));
    }

    #[test]
    fn a_row_anchored_at_the_edge_stays_on_screen() {
        let dock = placement(SCREEN, 6, 1.0, Prefs::default());
        let row = preview_row(&[(1600, 900), (1600, 900)], 10, dock, SCREEN, 0);
        assert!(row[0].x() >= 0, "ran off the left edge");
        let row = preview_row(&[(1600, 900), (1600, 900)], 1910, dock, SCREEN, 0);
        let last = row[1];
        assert!(last.x() + last.w() <= SCREEN.w(), "ran off the right edge");
    }

    #[test]
    fn a_row_too_wide_for_the_screen_shrinks_as_a_whole() {
        let dock = placement(SCREEN, 6, 1.0, Prefs::default());
        let wide: Vec<(i32, i32)> = (0..6).map(|_| (1600, 900)).collect();
        let row = preview_row(&wide, 960, dock, SCREEN, 0);
        let last = row[5];
        assert!(last.x() + last.w() <= SCREEN.w());
        assert!(row[0].x() >= 0);
        let widths: Vec<i32> = row.iter().map(|r| r.w()).collect();
        assert!(
            widths.iter().all(|w| (w - widths[0]).abs() <= 1),
            "unequal shrink: {widths:?}"
        );
    }

    #[test]
    fn reserved_room_lifts_the_row() {
        let dock = placement(SCREEN, 6, 1.0, Prefs::default());
        let plain = preview_row(&[(1600, 900)], 960, dock, SCREEN, 0)[0];
        let lifted = preview_row(&[(1600, 900)], 960, dock, SCREEN, 30)[0];
        assert_eq!(plain.y() - lifted.y(), 30);
        assert_eq!(
            plain.h(),
            lifted.h(),
            "the picture itself should not shrink"
        );
    }

    #[test]
    fn the_backing_surrounds_the_frame() {
        let frame = Rect::from_xywh(100, 100, 300, 200);
        let backing = preview_backing(frame, SCREEN);
        assert!(backing.x() < frame.x() && backing.y() < frame.y());
        assert!(backing.x() + backing.w() > frame.x() + frame.w());
        assert!(backing.y() + backing.h() > frame.y() + frame.h());
        let panel = preview_frame(frame, SCREEN, 1);
        assert_eq!(panel.size(), (backing.w(), backing.h()));
    }

    #[test]
    fn pinned_items_keep_their_place_when_something_starts() {
        // A strip whose contents move as you reach for them is worse than no
        // strip at all.
        let before: Vec<Option<usize>> = items(&apps(), &[]).iter().map(|i| i.entry).collect();
        let after: Vec<Option<usize>> = items(&apps(), &["Navigator".to_owned()])
            .iter()
            .map(|i| i.entry)
            .collect();
        assert_eq!(after[..before.len()], before[..], "the dock reshuffled");
    }

    #[test]
    fn the_dock_stays_hidden_while_the_pointer_only_passes_the_edge() {
        // Without the delay, dragging a scrollbar across the bottom of the
        // screen summons the dock every time.
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        assert!(!dock.is_visible(ms(100)), "it came up during the delay");
        dock.pointer_moved(500, SCREEN, false, ms(100), FULL);
        assert!(
            !dock.is_visible(ms(1_000)),
            "it came up after the pointer left"
        );
    }

    #[test]
    fn resting_at_the_edge_brings_it_up() {
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        dock.pointer_moved(1079, SCREEN, false, ms(300), FULL);
        assert!(dock.is_visible(ms(400)));
        assert!(
            (dock.reveal(ms(1_000)) - 1.0).abs() < 1e-3,
            "it did not finish rising"
        );
    }

    #[test]
    fn the_delay_is_timed_from_arrival_not_from_the_last_movement() {
        // A pointer resting at the edge still produces motion events; timing
        // from the latest one means the dock never appears.
        let mut dock = Dock::default();
        for t in (0..300).step_by(10) {
            dock.pointer_moved(1079, SCREEN, false, ms(t), FULL);
        }
        assert!(dock.is_visible(ms(400)), "the delay never elapsed");
    }

    #[test]
    fn moving_onto_the_dock_keeps_it_up() {
        // The pointer has left the edge band by definition once the dock is
        // up, so without this it would retreat as you reached for it.
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        dock.pointer_moved(1079, SCREEN, false, ms(300), FULL);
        dock.pointer_moved(1000, SCREEN, true, ms(400), FULL);
        assert!(dock.is_visible(ms(800)));
    }

    #[test]
    fn leaving_the_dock_takes_it_away_again() {
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        dock.pointer_moved(1079, SCREEN, false, ms(300), FULL);
        dock.pointer_moved(400, SCREEN, false, ms(600), FULL);
        assert!(!dock.is_visible(ms(2_000)));
    }

    #[test]
    fn reduced_motion_makes_it_appear_without_sliding() {
        let mut dock = Dock::default();
        let reduced = crate::settings::Motion::Reduced;
        dock.pointer_moved(1079, SCREEN, false, ms(0), reduced);
        dock.pointer_moved(1079, SCREEN, false, ms(300), reduced);
        assert_eq!(
            dock.reveal(ms(300)),
            1.0,
            "it animated despite reduced motion"
        );
    }

    #[test]
    fn a_hidden_dock_sits_off_the_bottom_of_the_screen() {
        // Sliding rather than fading: a partly-revealed dock is partly off
        // screen, which reads as an object arriving.
        let hidden = placement(SCREEN, 3, 0.0, Prefs::default());
        assert!(
            hidden.y() >= SCREEN.h(),
            "a hidden dock is on screen at {hidden:?}"
        );
        let shown = placement(SCREEN, 3, 1.0, Prefs::default());
        assert!(
            shown.y() + shown.h() <= SCREEN.h(),
            "a shown dock hangs off the bottom"
        );
    }

    #[test]
    fn it_is_centred_horizontally() {
        let rect = placement(SCREEN, 4, 1.0, Prefs::default());
        let left = rect.x() - SCREEN.x();
        let right = SCREEN.w() - (rect.x() + rect.w());
        assert!((left - right).abs() <= 1, "off centre by {}", left - right);
    }

    #[test]
    fn switcher_placement_is_at_the_centre_of_the_page() {
        let rect = centred_placement(SCREEN, 4, Prefs::default());
        let centre_x = SCREEN.x() + SCREEN.w() / 2;
        let centre_y = SCREEN.y() + SCREEN.h() / 2;
        assert!((rect.x() + rect.w() / 2 - centre_x).abs() <= 1);
        assert!((rect.y() + rect.h() / 2 - centre_y).abs() <= 1);
    }

    #[test]
    fn it_grows_with_the_number_of_items() {
        assert!(
            placement(SCREEN, 6, 1.0, Prefs::default()).w()
                > placement(SCREEN, 2, 1.0, Prefs::default()).w()
        );
    }

    #[test]
    fn clicking_finds_the_item_under_the_pointer() {
        let dock = Dock::default();
        let rect = placement(SCREEN, 4, 1.0, Prefs::default());
        let y = rect.y() + rect.h() / 2;
        assert_eq!(dock.item_at(rect.x() + 1, rect, 4), Some(0));
        assert_eq!(dock.item_at(rect.x() + rect.w() - 2, rect, 4), Some(3));
        let _ = y;
    }

    #[test]
    fn clicking_outside_the_dock_hits_nothing() {
        let dock = Dock::default();
        let rect = placement(SCREEN, 4, 1.0, Prefs::default());
        assert_eq!(dock.item_at(rect.x() - 10, rect, 4), None);
        assert_eq!(dock.item_at(rect.x() + rect.w() + 10, rect, 4), None);
        assert_eq!(
            dock.item_at(rect.x() + 1, rect, 0),
            None,
            "no items, no hit"
        );
    }

    #[test]
    fn going_fullscreen_takes_it_away_at_once() {
        // §4: it must never overlap a fullscreen window, and animating out
        // over one is still overlapping it.
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        dock.pointer_moved(1079, SCREEN, false, ms(300), FULL);
        dock.hide_now();
        assert!(!dock.is_visible(ms(300)));
        assert!(!dock.is_animating(ms(300)));
    }
    /// The icon that grows most and the item a click takes must be the same
    /// item, wherever along the bar the pointer rests. They are worked out
    /// by different code — one a continuous falloff, the other a division
    /// into slots — so this walks the whole bar and holds them together.
    #[test]
    fn the_biggest_icon_is_always_the_one_a_click_would_take() {
        let prefs = Prefs::default();
        let dock = Dock::default();
        let count = 6;
        let rect = placement(SCREEN, count, 1.0, prefs);
        let scale = rect.h() as f32 / (prefs.icon + GAP * 2.0);
        let (icon, gap) = (prefs.icon * scale, GAP * scale);
        let pitch = icon + gap;
        for step in 0..rect.w() {
            let px = step as f32;
            let Some(clicked) = dock.item_at(rect.x() + step, rect, count) else {
                continue;
            };
            let at = (px - gap) / pitch;
            let biggest = (0..count)
                .max_by(|a, b| {
                    let f = |n: &usize| magnification(*n as f32 + 0.5 - at, prefs.magnify);
                    f(a).total_cmp(&f(b))
                })
                .expect("a strip with items in it");
            // Past the last slot's icon `item_at` clamps to the last item,
            // which is the one being aimed at there anyway.
            if at < 0.0 || at >= count as f32 {
                continue;
            }
            assert_eq!(
                biggest, clicked,
                "at {px}: {biggest} grew most but a click takes {clicked}"
            );
        }
    }

    #[test]
    fn magnification_peaks_under_the_pointer_and_fades_to_nothing() {
        let m = 1.5;
        assert!(
            (magnification(0.0, m) - m).abs() < 1e-5,
            "not full under it"
        );
        assert!(
            (magnification(MAG_REACH, m) - 1.0).abs() < 1e-5,
            "still lifted at the reach"
        );
        assert!(
            (magnification(MAG_REACH * 3.0, m) - 1.0).abs() < 1e-5,
            "lifted past it"
        );
        // Falls away, and does so either side alike.
        assert!(magnification(0.5, m) > magnification(1.0, m));
        assert!(magnification(1.0, m) > magnification(1.5, m));
        assert_eq!(magnification(0.8, m), magnification(-0.8, m));
        // Turned off, nothing moves at any distance.
        for slots in [0.0, 0.5, 1.0, 2.0] {
            assert_eq!(magnification(slots, 1.0), 1.0);
        }
    }

    /// The panel keeps room above the bar, and the bar itself does not move
    /// when it does: the headroom is what a magnified icon lifts into.
    #[test]
    fn the_panel_reserves_headroom_without_moving_the_bar() {
        let prefs = Prefs::default();
        let bar = placement(SCREEN, 5, 1.0, prefs);
        let panel = panel_placement(SCREEN, 5, 1.0, prefs);
        assert_eq!(panel.bottom(), bar.bottom(), "the bar moved");
        assert!(panel.h() > bar.h(), "no room to magnify into");
        assert_eq!(panel.y() + (panel.h() - bar.h()), bar.y());
        // And room at each end, equally, for the first and last icons to
        // grow into — so the bar is still in the middle of its own panel.
        assert!(panel.w() > bar.w(), "no room at the ends");
        assert_eq!(bar.x() - panel.x(), panel.right() - bar.right());
        // Nothing to lift and nothing to name is no headroom at all.
        let flat = Prefs {
            magnify: 1.0,
            labels: false,
            ..prefs
        };
        assert_eq!(headroom(flat), 0.0);
        assert_eq!(
            panel_placement(SCREEN, 5, 1.0, flat),
            placement(SCREEN, 5, 1.0, flat)
        );
    }

    #[test]
    fn the_settings_are_held_to_their_bounds() {
        let wild = Prefs::new(4000, 10_000, true, true, true);
        assert_eq!(wild.icon, *ICON_RANGE.end() as f32);
        assert_eq!(wild.magnify, *MAGNIFY_RANGE.end() as f32 / 100.0);
        let tiny = Prefs::new(0, 0, false, false, false);
        assert_eq!(tiny.icon, *ICON_RANGE.start() as f32);
        assert_eq!(tiny.magnify, 1.0, "magnification below 100% would shrink");
        assert!(!tiny.labels && !tiny.dots && !tiny.auto_hide);
    }

    /// A bigger icon setting is a bigger dock, and the hit testing follows
    /// it: the two derive the slot pitch separately.
    #[test]
    fn a_larger_icon_makes_a_larger_dock_that_is_still_hit_correctly() {
        let small = Prefs::new(32, 100, false, true, true);
        let large = Prefs::new(64, 100, false, true, true);
        assert!(placement(SCREEN, 5, 1.0, large).w() > placement(SCREEN, 5, 1.0, small).w());
        assert!(placement(SCREEN, 5, 1.0, large).h() > placement(SCREEN, 5, 1.0, small).h());
        for prefs in [small, large] {
            let mut dock = Dock::default();
            dock.set_prefs(prefs);
            let rect = placement(SCREEN, 5, 1.0, prefs);
            assert_eq!(dock.item_at(rect.x() + 1, rect, 5), Some(0));
            assert_eq!(dock.item_at(rect.x() + rect.w() - 2, rect, 5), Some(4));
            assert_eq!(item_rect(rect, 0, prefs).w(), prefs.icon as i32);
        }
    }

    /// The menu sits above the bar and never off the screen, wherever along
    /// the dock the icon it belongs to is.
    #[test]
    fn the_menu_stays_above_the_bar_and_on_the_screen() {
        let prefs = Prefs::default();
        let panel = (236, 200);
        let bar = placement(SCREEN, 9, 1.0, prefs);
        for index in 0..9 {
            let slot = item_rect(bar, index, prefs);
            let at = menu_placement(panel, slot, bar, SCREEN);
            assert_eq!((at.w(), at.h()), panel, "{index} resized the menu");
            assert!(at.bottom() <= bar.y(), "{index} covered the bar");
            assert!(at.x() >= SCREEN.x(), "{index} hung off the left");
            assert!(at.right() <= SCREEN.right(), "{index} hung off the right");
            assert!(at.y() >= SCREEN.y(), "{index} went off the top");
        }
        // A dock at the left edge of a narrow screen still gets its menu on
        // screen, even when the menu is wider than the dock.
        let narrow = Rect::from_xywh(0, 0, 300, 800);
        let bar = placement(narrow, 2, 1.0, prefs);
        let at = menu_placement(panel, item_rect(bar, 0, prefs), bar, narrow);
        assert!(at.x() >= 0 && at.right() <= 300, "{at:?}");
    }

    /// A menu that is taller than the room above the dock is pushed down to
    /// the top of the screen rather than off it.
    #[test]
    fn a_menu_taller_than_the_screen_starts_at_the_top() {
        let prefs = Prefs::default();
        let bar = placement(SCREEN, 4, 1.0, prefs);
        let at = menu_placement((236, SCREEN.h() * 2), item_rect(bar, 0, prefs), bar, SCREEN);
        assert_eq!(at.y(), SCREEN.y());
    }

    /// The two switches actually take something away.
    #[test]
    fn labels_and_dots_can_be_turned_off() {
        let apps = apps();
        let items = items(&apps, &["term".to_owned()]);
        let paint = |prefs: Prefs, pointer: Option<f32>| {
            let icons = Icons::discover(crate::theme::ICON_THEME);
            let mut pixmaps = Pixmaps::new();
            let mut text = Text::new();
            let canvas = compose(
                &items,
                &apps,
                &icons,
                &mut pixmaps,
                &mut text,
                SCREEN,
                1,
                Strip::Dock { pointer },
                prefs,
            );
            (canvas.stride, canvas.height)
        };
        let on = Prefs::default();
        let no_labels = Prefs {
            labels: false,
            ..on
        };
        // A dock that names nothing needs no room to name it in.
        assert!(paint(no_labels, None).1 < paint(on, None).1);
        // Neither switch changes how wide the strip is.
        assert_eq!(paint(no_labels, None).0, paint(on, None).0);
        assert_eq!(
            paint(Prefs { dots: false, ..on }, None).0,
            paint(on, None).0
        );
    }

    /// A dock told not to hide is up whatever the pointer does.
    #[test]
    fn without_auto_hide_the_dock_stays_up() {
        let mut dock = Dock::default();
        dock.set_prefs(Prefs::new(44, 135, true, true, false));
        assert_eq!(dock.reveal(ms(0)), 1.0);
        assert!(dock.is_visible(ms(0)));
        // The pointer in the middle of the screen changes nothing.
        assert!(!dock.pointer_moved(
            10,
            SCREEN,
            false,
            ms(1000),
            crate::settings::Motion::Reduced
        ));
        assert_eq!(dock.reveal(ms(2000)), 1.0, "it went away");
        // And turning it back on hands the dock back to the pointer.
        dock.set_prefs(Prefs::default());
        assert_eq!(dock.reveal(ms(2000)), 0.0);
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// Dump one dock icon's context menu.
    ///
    /// `DOCK_MENU_DUMP=/tmp/m.ppm DOCK_MENU_APP=Brave cargo test -p
    /// huginn-comp dock_menu_dump -- --nocapture`
    #[test]
    fn dock_menu_dump() {
        let Ok(path) = std::env::var("DOCK_MENU_DUMP") else {
            return;
        };
        let apps = crate::launcher::scan_applications();
        let wanted = std::env::var("DOCK_MENU_APP").unwrap_or_else(|_| "Brave".into());
        let Some(app) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(&wanted)) else {
            println!("no application called {wanted}");
            return;
        };
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let m = Metrics::for_output(output, 1);
        // What `Huginn::draw_dock_menu` builds: the entry's own actions,
        // then what the desktop does with it.
        let offered: Vec<crate::menu::Row> = app
            .actions
            .iter()
            .map(|action| crate::menu::Row::action(&action.name, action.icon.as_deref()))
            .collect();
        let desktop = [
            crate::menu::Row::action(crate::launcher::PIN, Some("bookmark-new")),
            crate::menu::Row::danger(QUIT),
        ];
        let sections: [&[crate::menu::Row]; 2] = [&offered, &desktop];
        let (w, h) = crate::menu::size(&m, &sections);
        let mut canvas = Canvas::new(w as usize, h as usize);
        let mut text = Text::new();
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let selected = std::env::var("DOCK_MENU_ROW")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());
        crate::menu::draw(
            &mut canvas,
            &mut text,
            &m,
            &icons,
            &mut pixmaps,
            (&app.name, app.icon.as_deref()),
            &sections,
            selected,
            (0.0, 0.0),
        );
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.as_chunks::<4>().0.iter() {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }

    /// Dump the dock to a PPM so it can be looked at.
    ///
    /// `DOCK_DUMP=/tmp/d.ppm DOCK_POINTER=200 cargo test -p huginn-comp
    /// dock_dump -- --nocapture`
    ///
    /// `DOCK_POINTER` is where the pointer sits along the bar, in logical
    /// pixels, which is what the icons magnify around; `DOCK_ICON` and
    /// `DOCK_MAG` are the two sliders; `DOCK_LABELS=0` takes the name away;
    /// `DOCK_RUNNING` is a colon-separated list of `app_id`s.
    ///
    /// Through `compose`, which is what the screen gets: a dump drawn by its
    /// own copy of the painting is a dump that stops telling the truth the
    /// first time the painting changes.
    #[test]
    fn dock_dump() {
        let Ok(path) = std::env::var("DOCK_DUMP") else {
            return;
        };
        let number = |key: &str, fallback: f32| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(fallback)
        };
        let apps = crate::launcher::scan_applications();
        // A few real applications marked running, so the indicator shows.
        let running: Vec<String> = std::env::var("DOCK_RUNNING")
            .unwrap_or_else(|_| "raven-terminal:kitty:Brave-browser".into())
            .split(':')
            .map(str::to_owned)
            .collect();
        let items = items(&apps, &running);
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let mut text = Text::new();
        let prefs = Prefs::new(
            number("DOCK_ICON", ICON) as u32,
            number("DOCK_MAG", 135.0) as u32,
            std::env::var("DOCK_LABELS").as_deref() != Ok("0"),
            std::env::var("DOCK_DOTS").as_deref() != Ok("0"),
            true,
        );
        let strip = Strip::Dock {
            pointer: std::env::var("DOCK_POINTER")
                .ok()
                .and_then(|v| v.parse::<f32>().ok()),
        };
        let canvas = compose(
            &items,
            &apps,
            &icons,
            &mut pixmaps,
            &mut text,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
            strip,
            prefs,
        );
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.as_chunks::<4>().0.iter() {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
