//! The pin bar: the applications the user chose, one key away.
//!
//! The launcher answers "what do I want?" with a search field and a guess.
//! This bar answers a different question — "the things I always want" — and
//! so has no field, no heading and no labels: what is on it was put there
//! deliberately, through the launcher's actions menu, and it stays in the
//! order it was put. `Super`+`Ctrl`+`A` opens it; Enter opens what is
//! highlighted; the actions menu, `Delete` and `Shift`+arrows take things
//! off it and move them about.
//!
//! # A rail, not a panel
//!
//! It is one slot wide and as long as it has pins: a rail riding the edge
//! [`crate::pins::Position`] names, in Raven Glass's one material, with each
//! icon in a well of its own. It carries nothing that is not a pin — a
//! heading over four icons is a label on a thing that is already obvious,
//! and a footer of key hints on something the pointer can drive is a
//! reference card nailed to the wall.
//!
//! Which way the rail runs is not a separate setting: an edge says it. See
//! [`crate::pins`], which is also why there is no floating centre any more.
//!
//! # Nothing pinned is nothing drawn
//!
//! With an empty pin list the bar does not open at all, rather than opening
//! onto an empty rail with an explanation in it: the place to put something
//! on the bar is the launcher's Pin action, which is where somebody with an
//! empty bar has to go regardless. Take the last pin off an open bar and it
//! closes, fading out with that last icon still in it — see [`Pinned::unpin`].
//!
//! # The states a slot has
//!
//! Idle is a well. Under the pointer — or under the keyboard's highlight,
//! which is the same thing here — it is a raised well, ringed in the accent,
//! with the accent spilling a few pixels past the ring. A running
//! application has a dot under its icon, the dock's mark for the same fact
//! (§4: a subtle indicator, not a separate region). While a slot's menu is
//! up its ring stays but at half strength, because the menu beside it is
//! now the thing with the focus.
//!
//! The dot means running and only running, and the ring means "this one" and
//! only that. One mark, one meaning: a bar where the accent meant two things
//! at once would be a bar you have to read twice.
//!
//! Like the launcher it is compositor-drawn and takes every key while open —
//! see the launcher's module documentation for why, and why `Escape` must
//! always reach it.

use huginn_core::geometry::{Dir, Point, Rect};
use raven_desktop::{Entry, Icons, Pixmaps};

use crate::canvas::{Canvas, Panel};
use crate::launcher::{self, Layout, Metrics};
use crate::pins::{Pins, Position};
use crate::text::Text;

/// What a keystroke means to the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Left,
    Right,
    /// Open the highlighted application.
    Launch,
    /// Show, or hide, the highlighted application's menu.
    Actions,
    /// Take the highlighted application off the bar.
    Unpin,
    /// Move the highlighted application one place in a direction.
    Move(Dir),
    /// Close without opening anything.
    Dismiss,
    /// Recognised, deliberately does nothing — swallowed rather than
    /// forwarded, for the reason the launcher gives.
    Ignored,
}

impl Key {
    /// Interpret a keysym as a bar key. `shift` turns an arrow into a move.
    pub(crate) fn from_keysym(sym: u32, shift: bool) -> Self {
        use smithay::input::keyboard::keysyms;
        let arrow = |dir: Dir, plain: Self| if shift { Self::Move(dir) } else { plain };
        match sym {
            keysyms::KEY_Escape => Self::Dismiss,
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => Self::Launch,
            keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => Self::Actions,
            keysyms::KEY_Delete | keysyms::KEY_BackSpace => Self::Unpin,
            // There is no field to type into, so the vi keys are free to be
            // arrows, as they are in quick settings.
            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_K => arrow(Dir::Up, Self::Up),
            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_J => arrow(Dir::Down, Self::Down),
            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_H => arrow(Dir::Left, Self::Left),
            keysyms::KEY_Right | keysyms::KEY_l | keysyms::KEY_L => arrow(Dir::Right, Self::Right),
            _ => Self::Ignored,
        }
    }
}

/// What the compositor should do after a keystroke or a click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing visible changed; do not redraw.
    Unchanged,
    /// Redraw the bar.
    Redraw,
    /// Close the bar without opening anything.
    Dismissed,
    /// Close it and run `argv` on behalf of `entry`.
    Launch {
        entry: std::path::PathBuf,
        argv: Vec<String>,
    },
    /// The pin list changed — something was unpinned or moved — and should
    /// be saved. The bar needs redrawing, and may have closed itself if
    /// that was the last pin.
    Changed,
}

/// What an item of the actions menu does.
///
/// No `Open`: the menu's rows are the entry's own desktop actions, and
/// opening the application is what clicking its icon or pressing Enter on it
/// already does — a row that repeats the thing the menu was opened from is a
/// row nobody reads. No `Move` either; `Shift`+arrows move a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAct {
    /// Plain open, which is what Enter on a slot means. Never a menu row.
    Open,
    /// The entry's `n`th desktop action.
    Action(usize),
    Unpin,
}

/// The pin bar.
#[derive(Debug)]
pub(crate) struct Pinned {
    open: bool,
    /// Index into [`Self::items`].
    selected: usize,
    /// The pins that resolve to an installed application, as indices into
    /// the application list, in pin order. Navigation order is this order.
    items: Vec<usize>,
    /// The edge the items were laid out against, copied from the pins when
    /// the bar opens or refreshes so the keys and the picture agree even if
    /// the setting changes underneath.
    position: Position,
    /// The actions menu, if it is up: which item is highlighted.
    menu: Option<usize>,
    /// 0 collapsed, 1 open.
    reveal: crate::anim::Reveal,
    /// Where the last redraw put things; see [`Layout`].
    layout: Layout,
}

impl Default for Pinned {
    fn default() -> Self {
        Self {
            open: false,
            selected: 0,
            items: Vec::new(),
            position: Position::default(),
            menu: None,
            reveal: crate::anim::Reveal::hidden(),
            layout: Layout::default(),
        }
    }
}

impl Pinned {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// The applications shown, as indices into the application list.
    pub(crate) fn items(&self) -> &[usize] {
        &self.items
    }

    /// Which item is highlighted, as an index into the application list.
    pub(crate) fn selection(&self) -> Option<usize> {
        self.items.get(self.selected).copied()
    }

    /// The highlighted item's position in navigation order.
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn position(&self) -> Position {
        self.position
    }

    pub(crate) fn menu(&self) -> Option<usize> {
        self.menu
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(crate) fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    pub(crate) fn reveal(&self, clock: std::time::Duration) -> f32 {
        self.reveal.value(clock)
    }

    pub(crate) fn is_visible(&self, clock: std::time::Duration) -> bool {
        self.open || self.reveal(clock) > 0.001
    }

    pub(crate) fn is_animating(&self, clock: std::time::Duration) -> bool {
        !self.reveal.is_settled(clock)
    }

    /// The patch of desktop to blur, as a rectangle of the output.
    ///
    /// Not the whole placement, the way a rectangular panel's is: the canvas
    /// holds the rail, the menu beside it and the transparent air between
    /// them, and blurring all of that would put a blurred band across the
    /// desktop where nothing is drawn. The composition names the rail, which
    /// is the piece that is always there. See [`Layout::blur`].
    pub(crate) fn blur_region(&self, placement: Rect) -> Option<Rect> {
        let region = self.layout.to_output(placement, self.layout.blur?);
        (!region.is_empty()).then_some(region)
    }

    /// What the actions menu offers for `entry`: each of its desktop actions
    /// and, last — where a stray Down cannot land on it — unpin.
    fn menu_items<'a>(&self, entry: &'a Entry) -> Vec<(&'a str, MenuAct)> {
        entry
            .actions
            .iter()
            .enumerate()
            .map(|(n, a)| (a.name.as_str(), MenuAct::Action(n)))
            .chain(std::iter::once((launcher::UNPIN, MenuAct::Unpin)))
            .collect()
    }

    /// Open, highlighting the first pin — unless there are none, in which
    /// case the bar stays out of sight. See the module documentation.
    pub(crate) fn open(
        &mut self,
        apps: &[Entry],
        pins: &Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) {
        self.selected = 0;
        self.menu = None;
        // Open first, because [`Self::refresh`] leaves a closed bar's
        // picture alone, and then only stay open if there was something to
        // show.
        self.open = true;
        self.refresh(apps, pins);
        self.open = !self.items.is_empty();
        if self.open {
            self.reveal.open(clock, motion.is_reduced());
        }
    }

    /// Dismiss it, reversing the motion it arrived with.
    pub(crate) fn close(&mut self, clock: std::time::Duration, motion: crate::settings::Motion) {
        self.open = false;
        self.menu = None;
        self.reveal.close(clock, motion.is_reduced());
    }

    /// Re-resolve the pins against the application list and take the edge
    /// from `pins`. Called when either changes under an open bar: the items
    /// are indices into a list that may just have been reshuffled by an
    /// install, and the setting may have moved the bar. The highlight stays
    /// on the application it was on when that application is still there,
    /// and the menu is put away when it is not.
    ///
    /// A closed bar keeps the picture it had. It is either invisible, in
    /// which case nothing here matters, or it is fading out — and a bar that
    /// emptied itself on the way out would vanish rather than fade, which is
    /// the one thing the last state asks it not to do.
    pub(crate) fn refresh(&mut self, apps: &[Entry], pins: &Pins) {
        if !self.open {
            return;
        }
        let was = self
            .selection()
            .and_then(|i| apps.get(i))
            .map(|e| e.path.clone());
        self.position = pins.position();
        self.items = resolve(apps, pins);
        let found = was
            .as_ref()
            .and_then(|path| self.items.iter().position(|i| apps[*i].path == *path));
        self.selected = found
            .unwrap_or(self.selected)
            .min(self.items.len().saturating_sub(1));
        if found.is_none() {
            self.menu = None;
        }
    }

    /// Apply a keystroke.
    pub(crate) fn press(
        &mut self,
        key: Key,
        apps: &[Entry],
        pins: &mut Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if let Some(item) = self.menu {
            match key {
                Key::Dismiss | Key::Actions => {
                    self.menu = None;
                    return Outcome::Redraw;
                }
                // The menu is a column of rows whichever edge the rail
                // rides, so it is the vertical arrows that walk it.
                Key::Up | Key::Down => {
                    let count = self
                        .selection()
                        .and_then(|i| apps.get(i))
                        .map_or(1, |e| self.menu_items(e).len());
                    let next = if key == Key::Up {
                        item.saturating_sub(1)
                    } else {
                        (item + 1).min(count - 1)
                    };
                    if next == item {
                        return Outcome::Unchanged;
                    }
                    self.menu = Some(next);
                    return Outcome::Redraw;
                }
                Key::Launch => return self.launch(apps, pins, clock, motion),
                // The menu is about the highlighted item; anything that
                // would move or remove it is answered with the menu down.
                Key::Left | Key::Right | Key::Ignored => return Outcome::Unchanged,
                Key::Unpin | Key::Move(_) => {
                    self.menu = None;
                }
            }
        }
        match key {
            Key::Dismiss => {
                self.close(clock, motion);
                Outcome::Dismissed
            }
            Key::Launch => self.launch(apps, pins, clock, motion),
            Key::Actions => {
                if self.selection().is_none() {
                    return Outcome::Unchanged;
                }
                self.menu = Some(0);
                Outcome::Redraw
            }
            Key::Up => self.step(Dir::Up),
            Key::Down => self.step(Dir::Down),
            Key::Left => self.step(Dir::Left),
            Key::Right => self.step(Dir::Right),
            Key::Unpin => self.unpin(apps, pins, clock, motion),
            Key::Move(dir) => self.shift(dir, apps, pins),
            Key::Ignored => Outcome::Unchanged,
        }
    }

    /// How far along the rail one press in `dir` goes: one slot along the
    /// way the rail runs, and nothing across it. A rail down the right has
    /// no left and no right.
    fn stride(&self, dir: Dir) -> Option<isize> {
        match (self.position.is_vertical(), dir) {
            (true, Dir::Up) | (false, Dir::Left) => Some(-1),
            (true, Dir::Down) | (false, Dir::Right) => Some(1),
            _ => None,
        }
    }

    /// Move the highlight one slot in `dir`, stopping at the ends rather
    /// than wrapping, as the launcher does.
    fn step(&mut self, dir: Dir) -> Outcome {
        let Some(delta) = self.stride(dir) else {
            return Outcome::Unchanged;
        };
        if self.items.is_empty() {
            return Outcome::Unchanged;
        }
        let last = self.items.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, last) as usize;
        if next == self.selected {
            return Outcome::Unchanged;
        }
        self.selected = next;
        Outcome::Redraw
    }

    /// Take the highlighted application off the bar.
    ///
    /// The last one closes it. The picture is deliberately left as it is in
    /// that case: the reveal fades the rail out with its final icon still in
    /// it, where refreshing first would empty the rail and leave nothing to
    /// fade.
    fn unpin(
        &mut self,
        apps: &[Entry],
        pins: &mut Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        let Some(entry) = self.selection().and_then(|i| apps.get(i)) else {
            return Outcome::Unchanged;
        };
        if !pins.unpin(&entry.path) {
            return Outcome::Unchanged;
        }
        self.menu = None;
        if resolve(apps, pins).is_empty() {
            self.close(clock, motion);
        } else {
            self.refresh(apps, pins);
        }
        Outcome::Changed
    }

    /// Move the highlighted application one slot in `dir`, and the highlight
    /// with it: the thing the user is looking at is the thing they moved, and
    /// it should still be under the highlight afterwards.
    fn shift(&mut self, dir: Dir, apps: &[Entry], pins: &mut Pins) -> Outcome {
        let Some(delta) = self.stride(dir) else {
            return Outcome::Unchanged;
        };
        let Some(from) = self.selection().and_then(|i| apps.get(i)) else {
            return Outcome::Unchanged;
        };
        let last = self.items.len() as isize - 1;
        let target = (self.selected as isize + delta).clamp(0, last) as usize;
        if target == self.selected {
            return Outcome::Unchanged;
        }
        let Some(other) = self.items.get(target).and_then(|i| apps.get(*i)) else {
            return Outcome::Unchanged;
        };
        if !pins.place(&from.path, &other.path, delta > 0) {
            return Outcome::Unchanged;
        }
        self.menu = None;
        self.refresh(apps, pins);
        Outcome::Changed
    }

    /// Run the highlight — or, with the menu up, do what its item says.
    fn launch(
        &mut self,
        apps: &[Entry],
        pins: &mut Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        let Some(entry) = self.selection().and_then(|i| apps.get(i)) else {
            return Outcome::Unchanged;
        };
        let act = match self.menu {
            None => MenuAct::Open,
            Some(n) => match self.menu_items(entry).get(n) {
                Some((_, act)) => *act,
                None => return Outcome::Unchanged,
            },
        };
        let argv = match act {
            MenuAct::Open => entry.argv(&[]),
            MenuAct::Action(n) => entry
                .actions
                .get(n)
                .and_then(|action| entry.action_argv(action, &[])),
            MenuAct::Unpin => return self.unpin(apps, pins, clock, motion),
        };
        match argv {
            Some(argv) => {
                let argv = if entry.terminal {
                    launcher::in_terminal(argv, apps)
                } else {
                    argv
                };
                let entry = entry.path.clone();
                self.close(clock, motion);
                Outcome::Launch { entry, argv }
            }
            // An Exec that resolves to nothing must not close the bar.
            None => Outcome::Unchanged,
        }
    }

    /// The pointer moved to `point`, in canvas pixels. The highlight follows
    /// it, as the launcher's does; with the menu up, the menu's own does.
    pub(crate) fn hover(&mut self, point: Point) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if self.menu.is_some() {
            return match self.layout.menu_hit(point) {
                Some(item) if self.menu != Some(item) => {
                    self.menu = Some(item);
                    Outcome::Redraw
                }
                _ => Outcome::Unchanged,
            };
        }
        match self.layout.hit(point) {
            Some(index) if index < self.items.len() && index != self.selected => {
                self.selected = index;
                Outcome::Redraw
            }
            _ => Outcome::Unchanged,
        }
    }

    /// A click at `point`, in canvas pixels: a hover and then Enter, or a
    /// menu item, or — beside the menu — the menu put away.
    pub(crate) fn click(
        &mut self,
        point: Point,
        apps: &[Entry],
        pins: &mut Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if self.menu.is_some() {
            return match self.layout.menu_hit(point) {
                Some(item) => {
                    self.menu = Some(item);
                    self.launch(apps, pins, clock, motion)
                }
                None => {
                    self.menu = None;
                    Outcome::Redraw
                }
            };
        }
        let moved = self.hover(point);
        if self.layout.hit(point).is_none() {
            return moved;
        }
        match self.launch(apps, pins, clock, motion) {
            Outcome::Unchanged => moved,
            outcome => outcome,
        }
    }
}

/// The menu's rows for `entry`: its desktop actions, then Unpin.
///
/// The same order and the same count as [`Pinned::menu_items`], because the
/// keyboard walks that and the pointer clicks this; a test holds them to it.
fn menu_sections(entry: &Entry) -> [Vec<crate::menu::Row<'_>>; 2] {
    [
        entry
            .actions
            .iter()
            .map(|action| crate::menu::Row::action(&action.name, action.icon.as_deref()))
            .collect(),
        vec![crate::menu::Row::danger(launcher::UNPIN)],
    ]
}

/// The pins that resolve to an installed application, as indices into the
/// application list, in pin order.
fn resolve(apps: &[Entry], pins: &Pins) -> Vec<usize> {
    pins.paths()
        .iter()
        .filter_map(|path| apps.iter().position(|e| e.path == *path))
        .collect()
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Icon size at a 1080p output, in logical pixels.
///
/// Smaller than the dock's 44: the dock is a strip you aim at along the
/// bottom of the screen, and the pin bar is a rail at the side that should
/// read as trim rather than as a second dock.
const ICON: f32 = 40.0;
/// A slot: the well one icon sits in, the icon with room around it.
const SLOT: f32 = 52.0;
/// Between one slot and the next.
const SLOT_GAP: f32 = 6.0;
/// Between the slots and the rail's edge.
const PAD: f32 = 8.0;
/// The rail's corner radius: the hero of Raven Glass's radius scale, which
/// is what something one slot wide needs to read as a rail and not a box.
const RAIL_RADIUS: f32 = 20.0;
/// A slot's corner radius: a card, one step down the scale from the rail it
/// sits in.
const SLOT_RADIUS: f32 = 14.0;
/// Air between the rail and the edge it rides, in logical pixels. Enough
/// that a rail along the bottom sits clear of the dock's edge band and does
/// not look glued to the bezel.
const MARGIN: i32 = 24;
/// Between the rail and an open menu.
const MENU_GAP: f32 = 8.0;
/// The running mark: a dot under the icon, as the dock draws one.
const DOT: f32 = 4.0;
/// How far the hover glow reaches past a slot's ring.
const GLOW: f32 = 3.0;
/// The ground's opacity: the desktop's one material, at the one alpha.
const ALPHA: u8 = crate::theme::PANEL_ALPHA;

/// Where the rail sits, and how big, at the current reveal.
///
/// `panel` is the whole canvas, which is the rail plus whatever room an open
/// menu needs beside it. The menu is always composed on the side *away* from
/// the edge, and the canvas is padded equally at both ends of the rail, so
/// putting the canvas against the edge puts the rail against the edge — see
/// [`compose`].
///
/// A closing rail slides back into the edge it rides rather than shrinking
/// about its own centre: it is an object leaving the way it came, which is
/// how the dock goes too. The reveal drives its opacity as well, so a rail
/// half in is half there.
pub(crate) fn placement(output: Rect, panel: (i32, i32), position: Position, reveal: f32) -> Rect {
    let (w, h) = panel;
    let cx = output.x() + (output.w() - w).max(0) / 2;
    let cy = output.y() + (output.h() - h).max(0) / 2;
    let (x, y) = match position {
        Position::Top => (cx, output.y() + MARGIN),
        Position::Bottom => (cx, (output.bottom() - MARGIN - h).max(output.y())),
        Position::Left => (output.x() + MARGIN, cy),
        Position::Right => ((output.right() - MARGIN - w).max(output.x()), cy),
    };
    let t = reveal.clamp(0.0, 1.0);
    if t >= 1.0 {
        return Rect::from_xywh(x, y, w, h);
    }
    // How far out of sight it is: its own thickness plus the air it would
    // have had, so a rail at nothing is wholly off the screen.
    let thickness = if position.is_vertical() { w } else { h };
    let out = ((1.0 - t) * (thickness + MARGIN) as f32) as i32;
    let (dx, dy) = match position {
        Position::Right => (out, 0),
        Position::Left => (-out, 0),
        Position::Top => (0, -out),
        Position::Bottom => (0, out),
    };
    Rect::from_xywh(x + dx, y + dy, w, h)
}

/// Draw the bar for `output` at `density` pixels per logical one.
///
/// `running` is the `app_id` of every open window, so a pinned application
/// that is running can say so. The same list the dock is built from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    pinned: &Pinned,
    apps: &[Entry],
    running: &[String],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Panel, Layout) {
    let (canvas, layout) = compose(pinned, apps, running, text, icons, pixmaps, output, density);
    (Panel::from_canvas(&canvas, density), layout)
}

/// Lay the bar out and paint it, and say where everything went.
///
/// # The canvas
///
/// The rail is composed against the canvas edge that touches the screen
/// edge, and the menu — when one is up — on the far side of it. Across the
/// rail that is all there is to it. Along the rail the canvas is padded
/// *equally at both ends* by however far the menu hangs off either one,
/// which is what keeps the rail in the middle of its own canvas: [`placement`]
/// centres the canvas on the output, and a canvas that grew at one end only
/// would drag the rail off centre with it as the menu opened.
///
/// The air between the rail and the menu is transparent and belongs to the
/// desktop, not to the bar. [`Layout::surfaces`] says so, which is what
/// makes a click there a click on the desktop.
#[allow(clippy::too_many_arguments)]
fn compose(
    pinned: &Pinned,
    apps: &[Entry],
    running: &[String],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Canvas, Layout) {
    let m = Metrics::for_output(output, density);
    let scale = m.scale;
    let n = pinned.items().len();
    let vertical = pinned.position().is_vertical();
    // Nothing pinned is nothing drawn, answered before anything is measured.
    // One transparent pixel rather than an empty canvas, because an empty
    // `Layout::surfaces` means "all of it is panel" and a rail with no slots
    // must not take a click.
    if n == 0 {
        let canvas = Canvas::new(1, 1);
        let layout = Layout {
            size: (1, 1),
            surfaces: vec![Rect::ZERO],
            ..Layout::default()
        };
        return (canvas, layout);
    }
    let (slot, slot_gap, pad) = (SLOT * scale, SLOT_GAP * scale, PAD * scale);

    // The rail: one slot thick, as long as the slots it holds.
    let thick = pad * 2.0 + slot;
    let long = pad * 2.0 + slot * n as f32 + slot_gap * n.saturating_sub(1) as f32;
    let along_of = |index: usize| pad + (slot + slot_gap) * index as f32;

    // The menu, when it is up: how big, and where along the rail it wants to
    // be. Beside a vertical rail its header sits level with the slot it
    // belongs to; under a horizontal one it is centred on that slot.
    let menu = pinned
        .menu()
        .and(pinned.selection())
        .and_then(|i| apps.get(i))
        .map(|entry| {
            let sections = menu_sections(entry);
            let (menu_w, menu_h) = crate::menu::size(&m, &[&sections[0], &sections[1]]);
            let (menu_long, menu_thick) = if vertical {
                (menu_h, menu_w)
            } else {
                (menu_w, menu_h)
            };
            let centre = along_of(pinned.selected()) + slot / 2.0;
            let at = if vertical {
                centre - (pad + m.row / 2.0)
            } else {
                centre - menu_long / 2.0
            };
            (entry, menu_w, menu_h, menu_long, menu_thick, at)
        });

    // Padded equally at both ends, so the rail stays centred in the canvas.
    let overhang = menu
        .map(|(_, _, _, menu_long, _, at)| (-at).max(at + menu_long - long).max(0.0))
        .unwrap_or(0.0);
    let canvas_long = long + overhang * 2.0;
    let rail_along = overhang;
    let menu_span = menu
        .map(|(_, _, _, _, menu_thick, _)| menu_thick + MENU_GAP * scale)
        .unwrap_or(0.0);
    let canvas_thick = thick + menu_span;
    // The menu lies on the side away from the edge: left of a rail on the
    // right, above one along the bottom. Which is to say the rail is always
    // at the canvas edge that touches the screen's.
    let menu_first = matches!(pinned.position(), Position::Right | Position::Bottom);
    let rail_across = if menu_first { menu_span } else { 0.0 };
    let menu_across = if menu_first {
        0.0
    } else {
        thick + MENU_GAP * scale
    };

    // (along, across) is the rail's own frame; this is the only place that
    // knows which way round it is on the screen.
    let frame = |along: f32, across: f32, length: f32, depth: f32| {
        if vertical {
            (across, along, depth, length)
        } else {
            (along, across, length, depth)
        }
    };
    let rect = |(x, y, w, h): (f32, f32, f32, f32)| {
        Rect::from_xywh(x as i32, y as i32, w as i32, h as i32)
    };

    let (cw, ch) = if vertical {
        (canvas_thick as usize, canvas_long as usize)
    } else {
        (canvas_long as usize, canvas_thick as usize)
    };
    let mut canvas = Canvas::new(cw.max(1), ch.max(1));
    let mut layout = Layout {
        size: (cw.max(1) as i32, ch.max(1) as i32),
        ..Layout::default()
    };

    let (rx, ry, rw, rh) = frame(rail_along, rail_across, long, thick);
    canvas.material(
        rx as usize,
        ry as usize,
        rw as usize,
        rh as usize,
        RAIL_RADIUS * scale,
        ALPHA,
    );
    let rail = rect((rx, ry, rw, rh));
    layout.surfaces.push(rail);
    // The blur is the rail's own glass, inset by its corners so the blurred
    // patch does not show past them.
    layout.blur = Some(rail.inset((RAIL_RADIUS * scale) as i32)).filter(|r| !r.is_empty());

    let selected = pinned.selected();
    let radius = SLOT_RADIUS * scale;
    for (index, app) in pinned.items().iter().enumerate() {
        let Some(entry) = apps.get(*app) else {
            continue;
        };
        let (sx, sy, sw, sh) = frame(along_of(index) + rail_along, rail_across + pad, slot, slot);
        layout.hits.push((rect((sx, sy, sw, sh)), index));
        let chosen = index == selected;
        let context = chosen && pinned.menu().is_some();
        if chosen && !context {
            draw_glow(&mut canvas, sx, sy, sw, radius, scale);
        }
        canvas.fill_rounded(
            sx as usize,
            sy as usize,
            sw as usize,
            sh as usize,
            radius,
            if chosen {
                crate::theme::WELL_RAISED
            } else {
                crate::theme::WELL
            },
        );
        // Ringed in the accent when it is the one, and at half strength
        // while its menu holds the focus instead.
        let (ring, ink) = if context {
            (
                (1.5 * scale).max(1.5),
                crate::theme::accent().with_alpha(0x80),
            )
        } else if chosen {
            ((1.5 * scale).max(1.5), crate::theme::accent())
        } else {
            (1.0, crate::theme::HAIRLINE)
        };
        canvas.stroke_rounded(
            sx as usize,
            sy as usize,
            sw as usize,
            sh as usize,
            radius,
            ring,
            ink,
        );

        let icon = ICON * scale;
        if let Some(pixmap) = entry
            .icon
            .as_deref()
            // At its logical size for the output's density, which is how
            // icon themes file their 2× artwork, and in its own colours:
            // the mark on a pin is the application's, not the theme's.
            .and_then(|name| icons.find(name, icon as u32 / m.density, m.density))
            .and_then(|path| pixmaps.get(&path, icon as u32))
        {
            canvas.blit(
                (sx + (sw - icon) / 2.0) as usize,
                (sy + (sh - icon) / 2.0) as usize,
                pixmap,
            );
        }

        // Running: the dock's mark, in the band the slot leaves under the
        // icon, so it costs the rail no room of its own.
        if running.iter().any(|id| crate::dock::matches(entry, id)) {
            let dot = (DOT * scale).max(3.0);
            // The band between the icon's bottom edge and the slot's.
            let band = (sh - icon) / 2.0;
            let top = sy + (sh + icon) / 2.0 + (band - dot) / 2.0;
            canvas.fill_rounded(
                (sx + (sw - dot) / 2.0) as usize,
                top as usize,
                dot as usize,
                dot as usize,
                dot / 2.0,
                crate::theme::accent(),
            );
        }
    }

    if let (Some(item), Some((entry, menu_w, menu_h, _, _, at))) = (pinned.menu(), menu) {
        // The menu is a card the same way round whichever edge the rail
        // rides, so its own width and height go in unswapped; only where it
        // sits comes out of the rail's frame.
        let (mx, my, ..) = frame(at + rail_along, menu_across, menu_w, menu_h);
        let sections = menu_sections(entry);
        let hits = crate::menu::draw(
            &mut canvas,
            text,
            &m,
            icons,
            pixmaps,
            (&entry.name, entry.icon.as_deref()),
            &[&sections[0], &sections[1]],
            Some(item),
            (mx, my),
        );
        layout.surfaces.push(Rect::from_xywh(
            mx as i32,
            my as i32,
            menu_w as i32,
            menu_h as i32,
        ));
        layout
            .menu_hits
            .extend(hits.into_iter().zip(0..));
    }

    (canvas, layout)
}

/// The hover glow: the accent spilling off a slot's ring.
///
/// Three rings of halving alpha rather than one blurred pass, because the
/// canvas has no blur of its own and a byte canvas cannot afford one per
/// frame. At the few pixels this reaches, a stepped falloff and a smooth one
/// are the same picture.
fn draw_glow(canvas: &mut Canvas, x: f32, y: f32, size: f32, radius: f32, scale: f32) {
    let step = (GLOW * scale / 3.0).max(1.0);
    for ring in 0..3u8 {
        let out = step * (ring + 1) as f32;
        canvas.stroke_rounded(
            (x - out) as usize,
            (y - out) as usize,
            (size + out * 2.0) as usize,
            (size + out * 2.0) as usize,
            radius + out,
            step,
            crate::theme::accent().with_alpha(0x30 >> ring),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const STILL: crate::settings::Motion = crate::settings::Motion::Reduced;
    const CLOCK: std::time::Duration = std::time::Duration::ZERO;
    const OUTPUT: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            comment: None,
            generic_name: None,
            icon: None,
            exec: format!("/bin/{}", name.to_lowercase()),
            categories: Vec::new(),
            keywords: Vec::new(),
            terminal: false,
            mime_types: Vec::new(),
            startup_wm_class: None,
            path: PathBuf::from(format!("/apps/{name}.desktop")),
            actions: Vec::new(),
        }
    }

    /// An entry with desktop actions, as Firefox has.
    fn with_actions(name: &str, actions: &[&str]) -> Entry {
        let mut e = entry(name);
        e.actions = actions
            .iter()
            .map(|action| raven_desktop::entry::Action {
                id: action.to_lowercase().replace(' ', "-"),
                name: (*action).to_owned(),
                exec: format!("/bin/{} --{}", name.to_lowercase(), action.len()),
                icon: None,
            })
            .collect();
        e
    }

    fn apps() -> Vec<Entry> {
        ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot"]
            .into_iter()
            .map(entry)
            .collect()
    }

    /// A bar over `apps()` with `names` pinned, in that order, on `position`.
    fn opened(names: &[&str], position: Position) -> (Pinned, Vec<Entry>, Pins) {
        opened_over(apps(), names, position)
    }

    fn opened_over(
        apps: Vec<Entry>,
        names: &[&str],
        position: Position,
    ) -> (Pinned, Vec<Entry>, Pins) {
        let mut pins = Pins::new();
        for name in names {
            pins.pin(&PathBuf::from(format!("/apps/{name}.desktop")));
        }
        pins.set_position(position);
        let mut pinned = Pinned::default();
        pinned.open(&apps, &pins, CLOCK, STILL);
        (pinned, apps, pins)
    }

    fn name_of(pinned: &Pinned, apps: &[Entry]) -> Option<String> {
        pinned.selection().map(|i| apps[i].name.clone())
    }

    /// What the menu's rows say, in the order the keyboard walks them.
    fn labels<'a>(pinned: &Pinned, entry: &'a Entry) -> Vec<&'a str> {
        pinned
            .menu_items(entry)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    fn names(pins: &Pins) -> Vec<String> {
        pins.paths()
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Compose with no icons and no windows open, which is what most of
    /// these care about.
    fn laid_out(pinned: &Pinned, apps: &[Entry]) -> (Canvas, Layout) {
        let mut text = Text::new();
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        compose(
            pinned,
            apps,
            &[],
            &mut text,
            &icons,
            &mut pixmaps,
            OUTPUT,
            1,
        )
    }

    #[test]
    fn opening_shows_the_pins_in_order_and_highlights_the_first() {
        let (pinned, apps, _) = opened(&["Charlie", "Alpha"], Position::Right);
        assert!(pinned.is_open());
        let shown: Vec<_> = pinned
            .items()
            .iter()
            .map(|i| apps[*i].name.as_str())
            .collect();
        assert_eq!(shown, ["Charlie", "Alpha"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
    }

    #[test]
    fn with_nothing_pinned_the_bar_does_not_open_at_all() {
        let (mut pinned, apps, mut pins) = opened(&[], Position::Right);
        assert!(!pinned.is_open(), "an empty bar opened");
        assert!(!pinned.is_visible(CLOCK), "an empty bar was drawn");
        // And it takes no keys, so Super+Ctrl+A on an empty bar leaves the
        // keyboard where it was rather than swallowing it.
        for key in [Key::Launch, Key::Actions, Key::Down, Key::Dismiss] {
            assert_eq!(
                pinned.press(key, &apps, &mut pins, CLOCK, STILL),
                Outcome::Unchanged
            );
        }
        // A pin that resolves to nothing installed is no pin at all.
        let (pinned, ..) = opened(&["Ghost"], Position::Right);
        assert!(!pinned.is_open());
    }

    #[test]
    fn a_pin_with_no_installed_application_is_skipped_but_kept() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Ghost", "Bravo"], Position::Right);
        assert_eq!(pinned.items().len(), 2);
        assert_eq!(names(&pins), ["Alpha", "Ghost", "Bravo"]);
        // And moving past it on screen moves past it in the file too.
        assert_eq!(
            pinned.press(Key::Move(Dir::Down), &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Ghost", "Bravo", "Alpha"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
    }

    #[test]
    fn the_edge_decides_which_arrows_walk_the_rail() {
        for position in [Position::Right, Position::Left] {
            let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], position);
            assert_eq!(
                pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
                Outcome::Unchanged,
                "{position:?} answered a sideways arrow"
            );
            assert_eq!(
                pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
                Outcome::Redraw
            );
            assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Bravo"));
            // Never wraps.
            assert_eq!(
                pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
                Outcome::Unchanged
            );
        }
        for position in [Position::Top, Position::Bottom] {
            let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], position);
            assert_eq!(
                pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
                Outcome::Unchanged,
                "{position:?} answered a vertical arrow"
            );
            assert_eq!(
                pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
                Outcome::Redraw
            );
            assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Bravo"));
        }
    }

    #[test]
    fn enter_opens_the_highlight_and_closes_the_bar() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Position::Right);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        match pinned.press(Key::Launch, &apps, &mut pins, CLOCK, STILL) {
            Outcome::Launch { entry, argv } => {
                assert_eq!(entry, PathBuf::from("/apps/Bravo.desktop"));
                assert_eq!(argv, ["/bin/bravo"]);
            }
            other => panic!("did not launch: {other:?}"),
        }
        assert!(!pinned.is_open());
    }

    #[test]
    fn delete_unpins_and_the_highlight_stays_put() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo", "Charlie"], Position::Right);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Alpha", "Charlie"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
        assert!(pinned.is_open(), "unpinning closed the bar early");
    }

    #[test]
    fn taking_the_last_pin_off_closes_the_bar_with_its_picture_intact() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha"], Position::Right);
        assert_eq!(
            pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert!(!pinned.is_open(), "the last pin did not close the bar");
        assert!(names(&pins).is_empty());
        // Still one item, so the rail has something to fade out with, and a
        // refresh on the way out does not take it away.
        assert_eq!(pinned.items().len(), 1);
        pinned.refresh(&apps, &pins);
        assert_eq!(pinned.items().len(), 1, "the fade was left with nothing");
        let (canvas, layout) = laid_out(&pinned, &apps);
        assert_eq!(layout.hits.len(), 1);
        assert!(canvas.stride > 0 && canvas.height > 0);
    }

    #[test]
    fn shift_arrows_move_the_pin_and_the_highlight_with_it() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo", "Charlie"], Position::Top);
        assert_eq!(
            pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Bravo", "Alpha", "Charlie"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
        assert_eq!(pinned.selected(), 1);
        // At the end, nothing moves. And a move across the rail is nothing.
        pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(
            pinned.press(Key::Move(Dir::Down), &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(names(&pins), ["Bravo", "Charlie", "Alpha"]);
    }

    #[test]
    fn the_menu_is_the_entrys_actions_and_then_unpin() {
        let apps = vec![
            with_actions("Firefox", &["New Window", "New Private Window"]),
            entry("Bravo"),
        ];
        let (mut pinned, apps, mut pins) =
            opened_over(apps, &["Firefox", "Bravo"], Position::Right);
        assert_eq!(
            labels(&pinned, &apps[0]),
            ["New Window", "New Private Window", "Unpin"]
        );
        // An entry with no actions of its own still offers Unpin.
        assert_eq!(labels(&pinned, &apps[1]), ["Unpin"]);
        pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(pinned.menu(), Some(0));
        match pinned.press(Key::Launch, &apps, &mut pins, CLOCK, STILL) {
            Outcome::Launch { argv, .. } => assert_eq!(argv, ["/bin/firefox", "--10"]),
            other => panic!("an action did not run: {other:?}"),
        }
    }

    #[test]
    fn enter_on_unpin_unpins_and_the_menu_cannot_walk_off_its_end() {
        let apps = vec![with_actions("Firefox", &["New Window"]), entry("Bravo")];
        let (mut pinned, apps, mut pins) =
            opened_over(apps, &["Firefox", "Bravo"], Position::Right);
        pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
        for _ in 0..5 {
            pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        }
        assert_eq!(pinned.menu(), Some(1), "the menu walked off its end");
        assert_eq!(
            pinned.press(Key::Launch, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Bravo"]);
        assert_eq!(pinned.menu(), None);
        assert!(pinned.is_open());
    }

    /// The keyboard walks [`Pinned::menu_items`] and the pointer clicks the
    /// rows [`menu_sections`] draws. They are built separately, so an action
    /// added to one and not the other would make Enter and a click mean
    /// different things.
    #[test]
    fn the_rows_drawn_are_the_rows_the_keyboard_walks() {
        let entries = vec![
            with_actions("Firefox", &["New Window", "New Private Window"]),
            entry("Bravo"),
        ];
        let (pinned, apps, _) = opened_over(entries, &["Firefox", "Bravo"], Position::Right);
        for entry in &apps {
            let drawn: Vec<&str> = menu_sections(entry)
                .iter()
                .flatten()
                .map(|row| row.label)
                .collect();
            assert_eq!(drawn, labels(&pinned, entry), "{}", entry.name);
        }
    }

    #[test]
    fn escape_backs_out_of_the_menu_and_then_closes() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha"], Position::Right);
        pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Dismiss, &apps, &mut pins, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(pinned.is_open());
        assert_eq!(
            pinned.press(Key::Dismiss, &apps, &mut pins, CLOCK, STILL),
            Outcome::Dismissed
        );
        assert!(!pinned.is_open());
    }

    #[test]
    fn a_refresh_keeps_the_highlight_on_the_same_application() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo", "Charlie"], Position::Right);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        // Something pinned in front of it from the launcher.
        pins.pin(&apps[3].path);
        pins.place(&apps[3].path, &apps[0].path, false);
        pinned.refresh(&apps, &pins);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
        assert_eq!(pinned.selected(), 3);
    }

    #[test]
    fn a_refresh_takes_the_edge_from_the_pins() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Position::Right);
        pins.set_position(Position::Top);
        pinned.refresh(&apps, &pins);
        assert_eq!(pinned.position(), Position::Top);
        assert_eq!(
            pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged,
            "the keys did not follow the edge"
        );
    }

    #[test]
    fn keys_resolve_and_shift_turns_arrows_into_moves() {
        use smithay::input::keyboard::keysyms;
        assert_eq!(Key::from_keysym(keysyms::KEY_Escape, false), Key::Dismiss);
        assert_eq!(Key::from_keysym(keysyms::KEY_Return, false), Key::Launch);
        assert_eq!(Key::from_keysym(keysyms::KEY_Tab, false), Key::Actions);
        assert_eq!(Key::from_keysym(keysyms::KEY_Delete, false), Key::Unpin);
        assert_eq!(Key::from_keysym(keysyms::KEY_Left, false), Key::Left);
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Left, true),
            Key::Move(Dir::Left)
        );
        assert_eq!(Key::from_keysym(keysyms::KEY_J, true), Key::Move(Dir::Down));
        assert_eq!(Key::from_keysym(keysyms::KEY_a, false), Key::Ignored);
    }

    #[test]
    fn placement_puts_the_rail_against_its_edge() {
        let panel = (68, 300);
        let at = |position| placement(OUTPUT, panel, position, 1.0);
        assert_eq!(at(Position::Right).right(), 1920 - MARGIN);
        assert_eq!(at(Position::Left).x(), MARGIN);
        assert_eq!(at(Position::Top).y(), MARGIN);
        assert_eq!(at(Position::Bottom).bottom(), 1080 - MARGIN);
        // Centred on the other axis.
        assert_eq!(at(Position::Right).y(), (1080 - 300) / 2);
        assert_eq!(at(Position::Top).x(), (1920 - 68) / 2);
    }

    #[test]
    fn a_rail_arriving_slides_out_of_its_own_edge() {
        let panel = (68, 300);
        for position in Position::ALL {
            let full = placement(OUTPUT, panel, position, 1.0);
            let half = placement(OUTPUT, panel, position, 0.5);
            let gone = placement(OUTPUT, panel, position, 0.0);
            assert_eq!(
                (half.w(), half.h()),
                (full.w(), full.h()),
                "{position:?} changed size instead of moving"
            );
            // Further out at half than at full, and wholly out of sight at
            // nothing: past the edge it rides by its own thickness.
            let (axis, sign): (fn(Rect) -> i32, i32) = match position {
                Position::Right => (|r| r.x(), 1),
                Position::Left => (|r| r.x(), -1),
                Position::Top => (|r| r.y(), -1),
                Position::Bottom => (|r| r.y(), 1),
            };
            assert!(
                (axis(half) - axis(full)) * sign > 0,
                "{position:?} did not slide outward"
            );
            assert!(
                (axis(gone) - axis(full)) * sign >= MARGIN,
                "{position:?} was still on screen at nothing"
            );
        }
    }

    /// Pointer hits are laid down for every slot, on every edge, and the
    /// rail is the canvas.
    #[test]
    fn the_layout_records_every_slot_on_each_edge() {
        for position in Position::ALL {
            let (pinned, apps, _) = opened(&["Alpha", "Bravo", "Charlie"], position);
            let (canvas, layout) = laid_out(&pinned, &apps);
            let slots: Vec<usize> = layout.hits.iter().map(|(_, slot)| *slot).collect();
            assert_eq!(slots, [0, 1, 2], "{position:?}");
            // One slot thick and three slots long, whichever way round.
            let (thin, long) = if position.is_vertical() {
                (canvas.stride, canvas.height)
            } else {
                (canvas.height, canvas.stride)
            };
            assert!(thin < long, "{position:?} is not a rail: {thin}x{long}");
            assert!(thin < launcher::WIDTH as usize / 2, "{position:?} is wide");
            // The whole canvas is rail, and it is what gets blurred.
            assert_eq!(layout.surfaces.len(), 1, "{position:?}");
            assert_eq!(
                layout.surfaces[0],
                Rect::from_xywh(0, 0, canvas.stride as i32, canvas.height as i32),
                "{position:?}"
            );
            assert!(layout.blur.is_some(), "{position:?} does not blur");
            for (rect, _) in &layout.hits {
                assert!(rect.right() <= canvas.stride as i32, "{position:?}");
                assert!(rect.bottom() <= canvas.height as i32, "{position:?}");
            }
        }
    }

    /// The menu is composed away from the edge, and the rail stays where it
    /// was: the whole reason the canvas is padded at both ends.
    #[test]
    fn the_menu_opens_away_from_the_edge_and_leaves_the_rail_in_place() {
        let entries = vec![
            with_actions("Firefox", &["New Window", "New Private Window"]),
            entry("Bravo"),
            entry("Charlie"),
        ];
        for position in Position::ALL {
            let (mut pinned, apps, mut pins) =
                opened_over(entries.clone(), &["Firefox", "Bravo", "Charlie"], position);
            let (shut, closed) = laid_out(&pinned, &apps);
            pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
            let (open, layout) = laid_out(&pinned, &apps);
            assert_eq!(layout.surfaces.len(), 2, "{position:?} drew no menu");
            let (rail, menu) = (layout.surfaces[0], layout.surfaces[1]);
            assert_eq!(
                (rail.w(), rail.h()),
                (shut.stride as i32, shut.height as i32),
                "{position:?} resized the rail"
            );
            assert_eq!(closed.hits.len(), 3, "{position:?}");
            // The rail still touches the canvas edge that touches the
            // screen's, and the menu is on the far side of it.
            match position {
                Position::Right => {
                    assert_eq!(rail.right(), open.stride as i32);
                    assert!(menu.right() <= rail.x());
                }
                Position::Left => {
                    assert_eq!(rail.x(), 0);
                    assert!(menu.x() >= rail.right());
                }
                Position::Top => {
                    assert_eq!(rail.y(), 0);
                    assert!(menu.y() >= rail.bottom());
                }
                Position::Bottom => {
                    assert_eq!(rail.bottom(), open.height as i32);
                    assert!(menu.bottom() <= rail.y());
                }
            }
            // The rail is still in the middle of its canvas, so placing the
            // canvas still places the rail.
            let (before, after) = if position.is_vertical() {
                (rail.y(), open.height as i32 - rail.bottom())
            } else {
                (rail.x(), open.stride as i32 - rail.right())
            };
            assert_eq!(before, after, "{position:?} pushed the rail off centre");
            // Every row of the menu is reachable, and inside the canvas.
            let rows: Vec<usize> = layout.menu_hits.iter().map(|(_, n)| *n).collect();
            assert_eq!(rows, [0, 1, 2], "{position:?}");
            for (rect, n) in &layout.menu_hits {
                assert!(rect.x() >= 0 && rect.y() >= 0, "{position:?} row {n}");
                assert!(rect.right() <= open.stride as i32, "{position:?} row {n}");
                assert!(rect.bottom() <= open.height as i32, "{position:?} row {n}");
            }
        }
    }

    /// A running application is marked, and nothing else is.
    #[test]
    fn a_running_pin_is_marked() {
        let (pinned, apps, _) = opened(&["Alpha", "Bravo"], Position::Right);
        let mut text = Text::new();
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let paint = |running: &[String], text: &mut Text, pixmaps: &mut Pixmaps| {
            let (canvas, _) = compose(&pinned, &apps, running, text, &icons, pixmaps, OUTPUT, 1);
            canvas
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| **p == crate::theme::accent().to_rgba_bytes())
                .count()
        };
        let none = paint(&[], &mut text, &mut pixmaps);
        let one = paint(&["Bravo".to_owned()], &mut text, &mut pixmaps);
        assert!(
            one > none,
            "a running pin drew no dot: {one} accent pixels against {none}"
        );
    }

    /// Dump the bar to a PPM so it can be looked at.
    ///
    /// `PINNED_DUMP=/tmp/p.ppm PINNED_APPS=Firefox,Vim PINNED_EDGE=right
    /// cargo test -p huginn-comp pinned_dump -- --nocapture`
    ///
    /// `PINNED_EDGE` is right, left, top or bottom; `PINNED_STEP=2` moves the
    /// highlight along the rail; `PINNED_TAB=1` opens the menu and steps down
    /// once; `PINNED_RUNNING=firefox` marks what is running.
    #[test]
    fn pinned_dump() {
        let Ok(path) = std::env::var("PINNED_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let apps = launcher::scan_applications();
        let mut pins = Pins::new();
        for name in std::env::var("PINNED_APPS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
        {
            if let Some(app) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(name)) {
                pins.pin(&app.path);
            }
        }
        if let Some(position) = std::env::var("PINNED_EDGE")
            .ok()
            .and_then(|v| Position::from_value(&v))
        {
            pins.set_position(position);
        }
        let mut pinned = Pinned::default();
        pinned.open(&apps, &pins, CLOCK, STILL);
        let along = if pins.position().is_vertical() {
            Key::Down
        } else {
            Key::Right
        };
        for _ in 0..std::env::var("PINNED_STEP")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            pinned.press(along, &apps, &mut pins, CLOCK, STILL);
        }
        if let Some(steps) = std::env::var("PINNED_TAB")
            .ok()
            .and_then(|d| d.parse::<usize>().ok())
        {
            pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
            for _ in 0..steps {
                pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
            }
        }
        let running: Vec<String> = std::env::var("PINNED_RUNNING")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let (canvas, layout) = compose(
            &pinned,
            &apps,
            &running,
            &mut text,
            &icons,
            &mut pixmaps,
            OUTPUT,
            1,
        );
        for (rect, slot) in &layout.hits {
            println!("slot {slot}: {rect:?}");
        }
        for (rect, item) in &layout.menu_hits {
            println!("menu {item}: {rect:?}");
        }
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.as_chunks::<4>().0.iter() {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
