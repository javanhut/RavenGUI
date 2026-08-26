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
use raven_desktop::{Entry, Icons, Pixmaps};

use crate::anim::{Animated, Curve};
use crate::canvas::{Canvas, Panel};
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

/// How long the dock takes to spring up.
const REVEAL: Duration = Duration::from_millis(260);

/// One thing in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// Index into the application list, or `None` for the launcher button.
    pub entry: Option<usize>,
    /// Whether a window of this application is open.
    pub running: bool,
}

impl Item {
    /// The launcher button, which is always leftmost. §4.
    pub(crate) const fn launcher() -> Self {
        Self {
            entry: None,
            running: false,
        }
    }

    pub(crate) const fn is_launcher(&self) -> bool {
        self.entry.is_none()
    }
}

/// Applications pinned to the dock, by desktop file stem.
///
/// Compiled in, like everything else — there is no configuration. Whatever
/// RavenLinux ships as its defaults belongs here.
const PINNED: &[&str] = &["raven-terminal"];

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
    same(stem, app_id) || app_id.rsplit('.').next().is_some_and(|tail| same(stem, tail))
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
        });
    }
    items
}

/// The dock's visibility, and what the pointer is doing about it.
#[derive(Debug)]
pub(crate) struct Dock {
    /// 0 hidden, 1 fully up.
    reveal: Animated,
    /// When the pointer arrived at the bottom edge, if it is still there.
    at_edge_since: Option<Duration>,
    /// Whether the pointer is over the dock itself, which keeps it up.
    hovered: bool,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            reveal: Animated::settled(0.0),
            at_edge_since: None,
            hovered: false,
        }
    }
}

impl Dock {
    /// How far up the dock is, 0..=1.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
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

        let target = if should_show { 1.0 } else { 0.0 };
        if (self.reveal.target() - target).abs() < f32::EPSILON {
            return false;
        }
        self.reveal.animate_to(
            target,
            now,
            motion.duration(REVEAL),
            // A spring on the way up, so it arrives like an object; plain
            // easing on the way down, because an overshoot while leaving reads
            // as the dock trying to follow the pointer off the screen.
            if should_show { Curve::Spring } else { Curve::EaseOut },
        );
        true
    }

    /// Hide immediately, without animating. For a window going fullscreen.
    pub(crate) fn hide_now(&mut self) {
        self.reveal.jump_to(0.0);
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
        let scale = rect.h() as f32 / (ICON + GAP * 2.0);
        let pitch = (ICON + GAP) * scale;
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

/// Icon size at a 1080p output.
const ICON: f32 = 44.0;
/// Space around each icon.
const GAP: f32 = 10.0;
/// Corner radius, as a fraction of the dock's height.
const RADIUS: f32 = 0.28;
/// Distance from the bottom of the screen when fully up.
const MARGIN: f32 = 12.0;
const ALPHA: u8 = 0xE6;

/// The dock's rectangle on `output` at the current reveal.
///
/// Slides up from below the bottom edge, so a partly-revealed dock is partly
/// off screen rather than partly transparent — an object arriving, not an
/// image fading in.
pub(crate) fn placement(output: Rect, items: usize, reveal: f32) -> Rect {
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5);
    let (icon, gap, margin) = (ICON * scale, GAP * scale, MARGIN * scale);
    let h = (icon + gap * 2.0) as i32;
    let w = ((icon + gap) * items as f32 + gap) as i32;

    let x = output.x() + (output.w() - w) / 2;
    let resting = output.y() + output.h() - h - margin as i32;
    let hidden = output.y() + output.h();
    let y = hidden + ((resting - hidden) as f32 * reveal.clamp(0.0, 1.0)) as i32;
    Rect::from_xywh(x, y, w, h)
}

/// The rectangle one item occupies, given the dock's own rectangle.
///
/// Where the launcher grows out of. Derived from the dock's height rather than
/// divided out of its width, for the same reason [`Dock::item_at`] is — the
/// width is not a whole number of slots.
pub(crate) fn item_rect(dock: Rect, index: usize) -> Rect {
    let scale = dock.h() as f32 / (ICON + GAP * 2.0);
    let (icon, gap) = (ICON * scale, GAP * scale);
    Rect::from_xywh(
        dock.x() + (gap + (icon + gap) * index as f32) as i32,
        dock.y() + gap as i32,
        icon as i32,
        icon as i32,
    )
}

/// Paint the dock for `output` at `density` pixels per logical one.
///
/// The rectangle it is drawn into comes from [`placement`], in logical pixels;
/// this composes the same shape with `density` times the pixels each way.
pub(crate) fn render(
    items: &[Item],
    apps: &[Entry],
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    _text: &mut Text,
    output: Rect,
    density: u32,
) -> Panel {
    let density = density.max(1);
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let (icon, gap) = (ICON * scale, GAP * scale);
    let h = (icon + gap * 2.0) as usize;
    let w = ((icon + gap) * items.len() as f32 + gap) as usize;

    let mut canvas = Canvas::new(w.max(1), h.max(1));
    canvas.fill_rounded(
        0,
        0,
        w,
        h,
        h as f32 * RADIUS,
        crate::theme::BACKGROUND.with_alpha(ALPHA),
    );

    for (index, item) in items.iter().enumerate() {
        let x = gap + (icon + gap) * index as f32;
        if item.is_launcher() {
            // Drawn rather than themed: the launcher is not an installed
            // application and has no `.desktop` file to take an icon from.
            draw_launcher_glyph(&mut canvas, x, gap, icon, scale);
        } else if let Some(pixmap) = item
            .entry
            .and_then(|i| apps.get(i))
            .and_then(|e| e.icon.as_deref())
            // Looked up at its logical size for the output's density, which
            // is how icon themes file their 2× artwork; rasterized at the
            // real pixel size either way.
            .and_then(|name| icons.find(name, icon as u32 / density, density))
            .and_then(|path| pixmaps.get(&path, icon as u32))
        {
            canvas.blit(x as usize, gap as usize, pixmap);
        }

        // Running state: a small mark under the icon. §4 asks for "a subtle
        // indicator, not a separate region" — the same slot, annotated.
        if item.running {
            let dot = (4.0 * scale).max(3.0);
            canvas.fill_rounded(
                (x + icon / 2.0 - dot / 2.0) as usize,
                (h as f32 - gap * 0.6) as usize,
                dot as usize,
                dot as usize,
                dot / 2.0,
                crate::theme::ACCENT,
            );
        }
    }
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
                crate::theme::ACCENT,
            );
        }
    }
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
        }
    }

    fn apps() -> Vec<Entry> {
        vec![
            entry("Raven Terminal", "raven-terminal", Some("raven-terminal")),
            entry("Files", "org.gnome.Nautilus", None),
            entry("Firefox", "firefox", Some("Navigator")),
        ]
    }

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);
    const FULL: crate::settings::Motion = crate::settings::Motion::Full;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn startup_wm_class_wins_when_it_is_there() {
        assert!(matches(&apps()[2], "Navigator"), "Firefox's real app_id");
        assert!(matches(&apps()[2], "navigator"), "case is not authoritative");
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
        let item = items.iter().find(|i| !i.is_launcher()).expect("pinned item");
        assert!(!item.running);
        let entry = item.entry.and_then(|i| apps.get(i)).expect("resolves to an entry");
        assert!(entry.argv(&[]).is_some(), "nothing to run for {}", entry.name);
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
        let braves = items.iter().filter(|i| i.entry.is_some_and(|e| e > 0)).count();
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
        assert!(!dock.is_visible(ms(1_000)), "it came up after the pointer left");
    }

    #[test]
    fn resting_at_the_edge_brings_it_up() {
        let mut dock = Dock::default();
        dock.pointer_moved(1079, SCREEN, false, ms(0), FULL);
        dock.pointer_moved(1079, SCREEN, false, ms(300), FULL);
        assert!(dock.is_visible(ms(400)));
        assert!((dock.reveal(ms(1_000)) - 1.0).abs() < 1e-3, "it did not finish rising");
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
        assert_eq!(dock.reveal(ms(300)), 1.0, "it animated despite reduced motion");
    }

    #[test]
    fn a_hidden_dock_sits_off_the_bottom_of_the_screen() {
        // Sliding rather than fading: a partly-revealed dock is partly off
        // screen, which reads as an object arriving.
        let hidden = placement(SCREEN, 3, 0.0);
        assert!(hidden.y() >= SCREEN.h(), "a hidden dock is on screen at {hidden:?}");
        let shown = placement(SCREEN, 3, 1.0);
        assert!(shown.y() + shown.h() <= SCREEN.h(), "a shown dock hangs off the bottom");
    }

    #[test]
    fn it_is_centred_horizontally() {
        let rect = placement(SCREEN, 4, 1.0);
        let left = rect.x() - SCREEN.x();
        let right = SCREEN.w() - (rect.x() + rect.w());
        assert!((left - right).abs() <= 1, "off centre by {}", left - right);
    }

    #[test]
    fn it_grows_with_the_number_of_items() {
        assert!(placement(SCREEN, 6, 1.0).w() > placement(SCREEN, 2, 1.0).w());
    }

    #[test]
    fn clicking_finds_the_item_under_the_pointer() {
        let dock = Dock::default();
        let rect = placement(SCREEN, 4, 1.0);
        let y = rect.y() + rect.h() / 2;
        assert_eq!(dock.item_at(rect.x() + 1, rect, 4), Some(0));
        assert_eq!(dock.item_at(rect.x() + rect.w() - 2, rect, 4), Some(3));
        let _ = y;
    }

    #[test]
    fn clicking_outside_the_dock_hits_nothing() {
        let dock = Dock::default();
        let rect = placement(SCREEN, 4, 1.0);
        assert_eq!(dock.item_at(rect.x() - 10, rect, 4), None);
        assert_eq!(dock.item_at(rect.x() + rect.w() + 10, rect, 4), None);
        assert_eq!(dock.item_at(rect.x() + 1, rect, 0), None, "no items, no hit");
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
}

#[cfg(test)]
mod dump {
    use super::*;

    /// `DOCK_DUMP=/tmp/d.ppm cargo test -p huginn-comp dock_dump -- --nocapture`
    #[test]
    fn dock_dump() {
        let Ok(path) = std::env::var("DOCK_DUMP") else {
            return;
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
        let panel = render(
            &items,
            &apps,
            &icons,
            &mut pixmaps,
            &mut text,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        );
        let _ = panel;

        // Re-compose to get at the pixels.
        let scale = 1.0_f32;
        let (icon, gap) = (ICON * scale, GAP * scale);
        let h = (icon + gap * 2.0) as usize;
        let w = ((icon + gap) * items.len() as f32 + gap) as usize;
        let mut canvas = Canvas::new(w, h);
        // Opaque behind it so the PPM (which drops alpha) shows the rounding.
        canvas.fill(0, 0, w, h, [0x0A, 0x0A, 0x10, 0xFF]);
        canvas.fill_rounded(0, 0, w, h, h as f32 * RADIUS,
                            crate::theme::BACKGROUND.with_alpha(ALPHA));
        for (index, item) in items.iter().enumerate() {
            let x = gap + (icon + gap) * index as f32;
            if item.is_launcher() {
                draw_launcher_glyph(&mut canvas, x, gap, icon, scale);
            } else if let Some(pixmap) = item
                .entry
                .and_then(|i| apps.get(i))
                .and_then(|e| e.icon.as_deref())
                .and_then(|name| icons.find(name, icon as u32, 1))
                .and_then(|p| pixmaps.get(&p, icon as u32))
            {
                canvas.blit(x as usize, gap as usize, pixmap);
            }
            if item.running {
                let dot = (4.0 * scale).max(3.0);
                canvas.fill_rounded((x + icon / 2.0 - dot / 2.0) as usize,
                                    (h as f32 - gap * 0.6) as usize,
                                    dot as usize, dot as usize, dot / 2.0,
                                    crate::theme::ACCENT);
            }
        }
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
