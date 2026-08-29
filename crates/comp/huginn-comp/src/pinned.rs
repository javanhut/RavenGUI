//! The pinned panel: the applications the user chose, one key away.
//!
//! The launcher answers "what do I want?" with a search field and a guess.
//! This panel answers a different question — "the things I always want" —
//! and so has no field: what is on it was put there deliberately, through the
//! launcher's actions menu, and stays in the order it was put. `Super`+
//! `Shift`+`A` opens it; Enter opens what is highlighted; the actions menu,
//! `Delete` and `Shift`+arrows take things off it and move them about.
//!
//! It is drawn with the launcher's own tiles, rows, menu and footer (see
//! [`crate::launcher::Metrics`] and the drawing helpers beside it), so the
//! two read as one panel in two moods rather than as two panels. Where it
//! sits and whether it is a grid, a strip or a list is chosen in quick
//! settings and kept in [`crate::pins`].
//!
//! Like the launcher it is compositor-drawn and takes every key while open —
//! see the launcher's module documentation for why, and why `Escape` must
//! always reach it.

use huginn_core::geometry::{Dir, Point, Rect};
use raven_desktop::{Entry, Icons, Pixmaps};

use crate::canvas::{Canvas, Panel};
use crate::launcher::{self, Layout, Metrics};
use crate::pins::{Orientation, Pins, Position};
use crate::text::Text;

/// What a keystroke means to the panel.
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
    /// Take the highlighted application off the panel.
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
    /// Interpret a keysym as a panel key. `shift` turns an arrow into a move.
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
    /// Redraw the panel.
    Redraw,
    /// Close the panel without opening anything.
    Dismissed,
    /// Close it and run `argv` on behalf of `entry`.
    Launch {
        entry: std::path::PathBuf,
        argv: Vec<String>,
    },
    /// The pin list changed — something was unpinned or moved — and should
    /// be saved. The panel stays open, and needs redrawing.
    Changed,
}

/// What an item of the actions menu does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAct {
    Open,
    /// The entry's `n`th desktop action.
    Action(usize),
    Move(Dir),
    Unpin,
}

/// Tiles per row of the grid. One more than the launcher's suggestions: a
/// panel with no field above it has room, and a pinned set is usually
/// larger than six.
pub(crate) const COLUMNS: usize = 4;

/// The pinned panel.
#[derive(Debug)]
pub(crate) struct Pinned {
    open: bool,
    /// Index into [`Self::items`].
    selected: usize,
    /// The pins that resolve to an installed application, as indices into
    /// the application list, in pin order. Navigation order is this order.
    items: Vec<usize>,
    /// The layout the items were laid out for, copied from the pins when
    /// the panel opens or refreshes so the keys and the picture agree even
    /// if quick settings changes it underneath.
    orientation: Orientation,
    position: Position,
    /// The actions menu, if it is up: which item is highlighted.
    menu: Option<usize>,
    /// 0 collapsed, 1 open.
    reveal: crate::anim::Animated,
    /// Where the last redraw put things; see [`Layout`].
    layout: Layout,
}

impl Default for Pinned {
    fn default() -> Self {
        Self {
            open: false,
            selected: 0,
            items: Vec::new(),
            orientation: Orientation::default(),
            position: Position::default(),
            menu: None,
            reveal: crate::anim::Animated::settled(0.0),
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

    pub(crate) fn orientation(&self) -> Orientation {
        self.orientation
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

    /// How many items sit side by side, for the arrows.
    fn columns(&self) -> usize {
        match self.orientation {
            Orientation::Grid => COLUMNS.min(self.items.len()).max(1),
            Orientation::Row => self.items.len().max(1),
            Orientation::Column => 1,
        }
    }

    /// What the actions menu offers for `entry`: open, each of its actions,
    /// a move each way along the panel, and last — where a stray Down cannot
    /// land on it — unpin.
    fn menu_items<'a>(&self, entry: &'a Entry) -> Vec<(&'a str, MenuAct)> {
        let (back, forward) = match self.orientation {
            Orientation::Column => (("Move up", Dir::Up), ("Move down", Dir::Down)),
            Orientation::Grid | Orientation::Row => {
                (("Move left", Dir::Left), ("Move right", Dir::Right))
            }
        };
        std::iter::once(("Open", MenuAct::Open))
            .chain(
                entry
                    .actions
                    .iter()
                    .enumerate()
                    .map(|(n, a)| (a.name.as_str(), MenuAct::Action(n))),
            )
            .chain([
                (back.0, MenuAct::Move(back.1)),
                (forward.0, MenuAct::Move(forward.1)),
                (launcher::UNPIN, MenuAct::Unpin),
            ])
            .collect()
    }

    /// The menu's labels, for drawing.
    pub(crate) fn menu_labels<'a>(&self, entry: &'a Entry) -> Vec<&'a str> {
        self.menu_items(entry)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    /// Open, highlighting the first pin.
    pub(crate) fn open(
        &mut self,
        apps: &[Entry],
        pins: &Pins,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) {
        self.open = true;
        self.selected = 0;
        self.menu = None;
        self.refresh(apps, pins);
        self.reveal.animate_to(
            1.0,
            clock,
            motion.duration(crate::anim::LAUNCHER_OPEN),
            crate::anim::Curve::EaseOut,
        );
    }

    /// Dismiss it, reversing the motion it arrived with.
    pub(crate) fn close(&mut self, clock: std::time::Duration, motion: crate::settings::Motion) {
        self.open = false;
        self.menu = None;
        self.reveal.animate_to(
            0.0,
            clock,
            motion.duration(crate::anim::PANEL_CLOSE),
            crate::anim::Curve::EaseOut,
        );
    }

    /// Re-resolve the pins against the application list and take the
    /// layout from `pins`. Called when either changes under an open panel:
    /// the items are indices into a list that may just have been
    /// reshuffled by an install, and quick settings may have moved the
    /// panel. The highlight stays on the application it was on when that
    /// application is still there, and the menu is put away when it is not.
    pub(crate) fn refresh(&mut self, apps: &[Entry], pins: &Pins) {
        let was = self
            .selection()
            .and_then(|i| apps.get(i))
            .map(|e| e.path.clone());
        self.orientation = pins.orientation();
        self.position = pins.position();
        self.items = pins
            .paths()
            .iter()
            .filter_map(|path| apps.iter().position(|e| e.path == *path))
            .collect();
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
            Key::Unpin => self.unpin(apps, pins),
            Key::Move(dir) => self.shift(dir, apps, pins),
            Key::Ignored => Outcome::Unchanged,
        }
    }

    /// How far along the navigation order one press in `dir` goes, for the
    /// current layout: sideways is one, vertical is a row's worth, and a
    /// direction the layout has no room for is nothing.
    fn stride(&self, dir: Dir) -> Option<isize> {
        let columns = self.columns() as isize;
        let delta = match (self.orientation, dir) {
            (Orientation::Row, Dir::Up | Dir::Down) => return None,
            (Orientation::Column, Dir::Left | Dir::Right) => return None,
            (_, Dir::Left) => -1,
            (_, Dir::Right) => 1,
            (_, Dir::Up) => -columns,
            (_, Dir::Down) => columns,
        };
        Some(delta)
    }

    /// Move the highlight one press in `dir`, stopping at the ends rather
    /// than wrapping, as the launcher does. Down off the last row lands on
    /// the last item rather than nowhere.
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

    /// Take the highlighted application off the panel.
    fn unpin(&mut self, apps: &[Entry], pins: &mut Pins) -> Outcome {
        let Some(entry) = self.selection().and_then(|i| apps.get(i)) else {
            return Outcome::Unchanged;
        };
        if !pins.unpin(&entry.path) {
            return Outcome::Unchanged;
        }
        self.menu = None;
        self.refresh(apps, pins);
        Outcome::Changed
    }

    /// Move the highlighted application one press in `dir`, and the
    /// highlight with it: the thing the user is looking at is the thing
    /// they moved, and it should still be under the highlight afterwards.
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
            MenuAct::Move(dir) => return self.shift(dir, apps, pins),
            MenuAct::Unpin => return self.unpin(apps, pins),
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
            // An Exec that resolves to nothing must not close the panel.
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

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The heading over the items.
const TITLE: &str = "Pinned";
/// What an empty panel says, and how to fill it.
const EMPTY: &str = "Nothing pinned yet";
const EMPTY_HINT: &str = "Tab on an application in the launcher, then Pin";
/// How much of the output a strip may take before its tiles shrink.
const ROW_SHARE: f32 = 0.9;
/// Air between the panel and the edge it is placed against, in logical
/// pixels. Enough that a panel at the bottom sits clear of the dock's edge
/// band and does not look glued to the bezel.
const MARGIN: i32 = 24;

const GRID_HINTS: &[(&str, &str)] = &[
    ("←↑↓→", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Del", "Unpin"),
    ("Esc", "Close"),
];
const ROW_HINTS: &[(&str, &str)] = &[
    ("←→", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Del", "Unpin"),
    ("Esc", "Close"),
];
const COLUMN_HINTS: &[(&str, &str)] = &[
    ("↑↓", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Del", "Unpin"),
    ("Esc", "Close"),
];
const MENU_HINTS: &[(&str, &str)] = &[("↑↓", "Choose"), ("Enter", "Select"), ("Esc", "Back")];
const EMPTY_HINTS: &[(&str, &str)] = &[("Esc", "Close")];

/// Which hints the footer shows.
fn hints_for(
    orientation: Orientation,
    empty: bool,
    menu_open: bool,
) -> &'static [(&'static str, &'static str)] {
    if menu_open {
        return MENU_HINTS;
    }
    if empty {
        return EMPTY_HINTS;
    }
    match orientation {
        Orientation::Grid => GRID_HINTS,
        Orientation::Row => ROW_HINTS,
        Orientation::Column => COLUMN_HINTS,
    }
}

/// Where the panel sits, and how big, at the current reveal.
///
/// Placed by `position` against the output's edge with [`MARGIN`] of air,
/// and grown in place from [`MIN_SCALE`] of its size about its own centre:
/// there is no dock icon to grow from, and a panel against the top edge
/// arriving from the middle of the screen would be motion with no meaning.
pub(crate) fn placement(output: Rect, panel: (i32, i32), position: Position, reveal: f32) -> Rect {
    /// How small the panel gets at the start of the motion.
    const MIN_SCALE: f32 = 0.86;

    let (w, h) = panel;
    let cx = output.x() + (output.w() - w).max(0) / 2;
    let cy = output.y() + (output.h() - h).max(0) / 2;
    let (x, y) = match position {
        Position::Centre => (cx, cy),
        Position::Top => (cx, output.y() + MARGIN),
        Position::Bottom => (cx, (output.bottom() - MARGIN - h).max(output.y())),
        Position::Left => (output.x() + MARGIN, cy),
        Position::Right => ((output.right() - MARGIN - w).max(output.x()), cy),
    };
    let full = Rect::from_xywh(x, y, w, h);
    let t = reveal.clamp(0.0, 1.0);
    if t >= 1.0 {
        return full;
    }
    let scale = MIN_SCALE + (1.0 - MIN_SCALE) * t;
    let (sw, sh) = ((w as f32 * scale) as i32, (h as f32 * scale) as i32);
    let centre = full.center();
    Rect::from_xywh(centre.x - sw / 2, centre.y - sh / 2, sw, sh)
}

/// Draw the panel for `output` at `density` pixels per logical one.
pub(crate) fn render(
    pinned: &Pinned,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Panel, Layout) {
    let (canvas, layout) = compose(pinned, apps, text, icons, pixmaps, output, density);
    (Panel::from_canvas(&canvas, density), layout)
}

/// Lay the panel out and paint it, and say where everything went.
fn compose(
    pinned: &Pinned,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Canvas, Layout) {
    let base = Metrics::for_output(output, density);
    let n = pinned.items().len();
    let orientation = pinned.orientation();
    let tile_h = launcher::TILE * base.scale;
    // The grid's tile: the launcher's inner width shared among the columns.
    let grid_tile_w = (base.inner - base.gap * (COLUMNS - 1) as f32) / COLUMNS as f32;

    // A strip is as wide as its tiles, up to most of the output — past which
    // the tiles shrink rather than the panel running off the edge.
    let (m, tile_w) = match orientation {
        Orientation::Row if n > 0 => {
            let most = output.w() as f32 * base.density as f32 * ROW_SHARE;
            let room = (most - base.pad * 2.0 - base.gap * (n - 1) as f32) / n as f32;
            let tile_w = grid_tile_w.min(room).max(base.size * 3.0);
            let width = base.pad * 2.0 + tile_w * n as f32 + base.gap * (n - 1) as f32;
            (base.with_width(width as usize), tile_w)
        }
        _ => (base, grid_tile_w),
    };
    let Metrics {
        pad,
        width,
        row,
        inner,
        gap,
        heading,
        footer,
        ..
    } = m;

    let tile_rows = n.div_ceil(COLUMNS);
    let body = match orientation {
        _ if n == 0 => row * 2.0 + gap,
        Orientation::Grid => tile_rows as f32 * tile_h + (tile_rows - 1) as f32 * gap + gap,
        Orientation::Row => tile_h + gap,
        Orientation::Column => row * n as f32 + gap,
    };
    let menu_h = pinned
        .menu()
        .and(pinned.selection())
        .and_then(|i| apps.get(i))
        .map(|entry| m.menu_height(pinned.menu_labels(entry).len()));
    let body = body.max(menu_h.unwrap_or(0.0));
    let height = (pad + heading + body + footer + pad) as usize;

    let mut canvas = Canvas::new(width, height.max(1));
    let mut layout = Layout {
        size: (width as i32, height.max(1) as i32),
        hits: Vec::new(),
        menu_hits: Vec::new(),
    };
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    launcher::draw_ground(&mut canvas, &m, width, height);

    let mut y = pad;
    launcher::draw_heading(&mut canvas, text, &m, y, TITLE);
    y += heading;
    let top = y;

    let selected = pinned.selected();
    if n == 0 {
        text.draw(
            &mut canvas,
            EMPTY,
            m.size,
            pad as i32,
            (y + (row - m.size * 1.35) / 2.0) as i32,
            crate::theme::TEXT,
        );
        y += row;
        let hint = launcher::fit(text, EMPTY_HINT, m.size * 0.95, inner);
        text.draw(
            &mut canvas,
            &hint,
            m.size * 0.95,
            pad as i32,
            (y + (row - m.size * 0.95 * 1.35) / 2.0) as i32,
            crate::theme::TEXT_DIM,
        );
        y += row + gap;
    } else {
        match orientation {
            Orientation::Grid | Orientation::Row => {
                let columns = match orientation {
                    Orientation::Row => n.max(1),
                    _ => COLUMNS,
                };
                for (slot, index) in pinned.items().iter().enumerate() {
                    let Some(entry) = apps.get(*index) else {
                        continue;
                    };
                    let (col, row_n) = (slot % columns, slot / columns);
                    let x = pad + (tile_w + gap) * col as f32;
                    let ty = y + (tile_h + gap) * row_n as f32;
                    layout.hits.push((rect(x, ty, tile_w, tile_h), slot));
                    launcher::draw_tile(
                        &mut canvas,
                        text,
                        icons,
                        pixmaps,
                        &m,
                        entry,
                        x,
                        ty,
                        tile_w,
                        tile_h,
                        slot == selected,
                    );
                }
                let rows = n.div_ceil(columns);
                y += rows as f32 * tile_h + (rows - 1) as f32 * gap + gap;
            }
            Orientation::Column => {
                for (slot, index) in pinned.items().iter().enumerate() {
                    let Some(entry) = apps.get(*index) else {
                        continue;
                    };
                    layout.hits.push((rect(pad, y, inner, row), slot));
                    launcher::draw_app_row(
                        &mut canvas,
                        text,
                        icons,
                        pixmaps,
                        &m,
                        entry,
                        y,
                        slot == selected,
                    );
                    y += row;
                }
                y += gap;
            }
        }
    }
    // A short body under a tall menu: the footer goes where the body ends,
    // which is at least where the menu ends.
    y = y.max(top + body);

    if let (Some(item), Some(entry)) = (pinned.menu(), pinned.selection().and_then(|i| apps.get(i)))
    {
        let labels = pinned.menu_labels(entry);
        launcher::draw_menu(
            &mut canvas,
            text,
            &mut layout,
            &m,
            &entry.name,
            &labels,
            item,
            y,
            top,
        );
    }

    launcher::draw_footer(
        &mut canvas,
        text,
        &m,
        y,
        hints_for(orientation, n == 0, pinned.menu().is_some()),
    );

    (canvas, layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const STILL: crate::settings::Motion = crate::settings::Motion::Reduced;
    const CLOCK: std::time::Duration = std::time::Duration::ZERO;

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
            startup_wm_class: None,
            path: PathBuf::from(format!("/apps/{name}.desktop")),
            actions: Vec::new(),
        }
    }

    fn apps() -> Vec<Entry> {
        ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot"]
            .into_iter()
            .map(entry)
            .collect()
    }

    /// A panel over `apps()` with `names` pinned, in that order.
    fn opened(names: &[&str], orientation: Orientation) -> (Pinned, Vec<Entry>, Pins) {
        let apps = apps();
        let mut pins = Pins::new();
        for name in names {
            pins.pin(&PathBuf::from(format!("/apps/{name}.desktop")));
        }
        pins.set_orientation(orientation);
        let mut pinned = Pinned::default();
        pinned.open(&apps, &pins, CLOCK, STILL);
        (pinned, apps, pins)
    }

    fn name_of(pinned: &Pinned, apps: &[Entry]) -> Option<String> {
        pinned.selection().map(|i| apps[i].name.clone())
    }

    fn names(pins: &Pins) -> Vec<String> {
        pins.paths()
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn opening_shows_the_pins_in_order_and_highlights_the_first() {
        let (pinned, apps, _) = opened(&["Charlie", "Alpha"], Orientation::Grid);
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
    fn a_pin_with_no_installed_application_is_skipped_but_kept() {
        let (pinned, apps, pins) = opened(&["Alpha", "Ghost", "Bravo"], Orientation::Grid);
        assert_eq!(pinned.items().len(), 2);
        assert_eq!(names(&pins), ["Alpha", "Ghost", "Bravo"]);
        // And moving past it on screen moves past it in the file too.
        let mut pinned = pinned;
        let mut pins = pins;
        assert_eq!(
            pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Ghost", "Bravo", "Alpha"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
    }

    #[test]
    fn the_grid_walks_a_row_at_a_time_vertically() {
        let (mut pinned, apps, mut pins) = opened(
            &["Alpha", "Bravo", "Charlie", "Delta", "Echo"],
            Orientation::Grid,
        );
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Echo"));
        pinned.press(Key::Up, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
        // Down from the third column, where the second row has nothing:
        // the last item, not nowhere.
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Echo"));
        // And never wraps.
        assert_eq!(
            pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn a_row_has_no_up_and_a_column_has_no_sideways() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Row);
        assert_eq!(
            pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(
            pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
            Outcome::Redraw
        );
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Column);
        assert_eq!(
            pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(
            pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL),
            Outcome::Redraw
        );
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Bravo"));
    }

    #[test]
    fn enter_opens_the_highlight_and_closes_the_panel() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Grid);
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
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
        let (mut pinned, apps, mut pins) =
            opened(&["Alpha", "Bravo", "Charlie"], Orientation::Grid);
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Alpha", "Charlie"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
        assert!(pinned.is_open(), "unpinning closed the panel");
        // Unpinning the last one leaves the highlight on the new last.
        pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
        pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(pinned.selection(), None);
        assert_eq!(
            pinned.press(Key::Unpin, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn shift_arrows_move_the_pin_and_the_highlight_with_it() {
        let (mut pinned, apps, mut pins) =
            opened(&["Alpha", "Bravo", "Charlie"], Orientation::Grid);
        assert_eq!(
            pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Bravo", "Alpha", "Charlie"]);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Alpha"));
        assert_eq!(pinned.selected(), 1);
        // At the end, nothing moves.
        pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Move(Dir::Right), &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(names(&pins), ["Bravo", "Charlie", "Alpha"]);
    }

    #[test]
    fn the_menu_offers_open_moves_and_unpin_and_enter_on_unpin_unpins() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Column);
        assert_eq!(
            pinned.menu_labels(&apps[0]),
            ["Open", "Move up", "Move down", "Unpin"]
        );
        pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(pinned.menu(), Some(0));
        for _ in 0..5 {
            pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        }
        assert_eq!(pinned.menu(), Some(3), "the menu walked off its end");
        assert_eq!(
            pinned.press(Key::Launch, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Bravo"]);
        assert_eq!(pinned.menu(), None);
        assert!(pinned.is_open());
    }

    #[test]
    fn the_menu_moves_a_pin_through_its_move_items() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Grid);
        assert_eq!(
            pinned.menu_labels(&apps[0]),
            ["Open", "Move left", "Move right", "Unpin"]
        );
        pinned.press(Key::Actions, &apps, &mut pins, CLOCK, STILL);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        pinned.press(Key::Down, &apps, &mut pins, CLOCK, STILL);
        assert_eq!(
            pinned.press(Key::Launch, &apps, &mut pins, CLOCK, STILL),
            Outcome::Changed
        );
        assert_eq!(names(&pins), ["Bravo", "Alpha"]);
    }

    #[test]
    fn escape_backs_out_of_the_menu_and_then_closes() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha"], Orientation::Grid);
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
    fn an_empty_panel_opens_and_swallows_keys_but_escape() {
        let (mut pinned, apps, mut pins) = opened(&[], Orientation::Grid);
        assert!(pinned.is_open());
        for key in [Key::Launch, Key::Actions, Key::Down, Key::Unpin] {
            assert_eq!(
                pinned.press(key, &apps, &mut pins, CLOCK, STILL),
                Outcome::Unchanged
            );
        }
        assert_eq!(
            pinned.press(Key::Dismiss, &apps, &mut pins, CLOCK, STILL),
            Outcome::Dismissed
        );
    }

    #[test]
    fn a_refresh_keeps_the_highlight_on_the_same_application() {
        let (mut pinned, apps, mut pins) =
            opened(&["Alpha", "Bravo", "Charlie"], Orientation::Grid);
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
        pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
        // Something pinned in front of it from the launcher.
        pins.pin(&apps[3].path);
        pins.place(&apps[3].path, &apps[0].path, false);
        pinned.refresh(&apps, &pins);
        assert_eq!(name_of(&pinned, &apps).as_deref(), Some("Charlie"));
        assert_eq!(pinned.selected(), 3);
    }

    #[test]
    fn a_refresh_takes_the_layout_from_the_pins() {
        let (mut pinned, apps, mut pins) = opened(&["Alpha", "Bravo"], Orientation::Grid);
        pins.set_orientation(Orientation::Column);
        pins.set_position(Position::Top);
        pinned.refresh(&apps, &pins);
        assert_eq!(pinned.orientation(), Orientation::Column);
        assert_eq!(pinned.position(), Position::Top);
        assert_eq!(
            pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL),
            Outcome::Unchanged,
            "the keys did not follow the layout"
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
    fn placement_puts_the_panel_where_it_was_asked() {
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let panel = (560, 300);
        let at = |position| placement(output, panel, position, 1.0);
        assert_eq!(at(Position::Centre), Rect::from_xywh(680, 390, 560, 300));
        assert_eq!(at(Position::Top).y(), MARGIN);
        assert_eq!(at(Position::Bottom).bottom(), 1080 - MARGIN);
        assert_eq!(at(Position::Left).x(), MARGIN);
        assert_eq!(at(Position::Right).right(), 1920 - MARGIN);
        // Half revealed: smaller, about the same centre.
        let half = placement(output, panel, Position::Top, 0.5);
        assert!(half.w() < 560 && half.h() < 300);
        assert_eq!(half.center(), at(Position::Top).center());
    }

    /// Pointer hits are laid down for every item, in navigation order.
    #[test]
    fn the_layout_records_every_item_in_each_orientation() {
        for orientation in Orientation::ALL {
            let (pinned, apps, _) = opened(&["Alpha", "Bravo", "Charlie"], orientation);
            let mut text = Text::new();
            let icons = Icons::discover(crate::theme::ICON_THEME);
            let mut pixmaps = Pixmaps::new();
            let (canvas, layout) = compose(
                &pinned,
                &apps,
                &mut text,
                &icons,
                &mut pixmaps,
                Rect::from_xywh(0, 0, 1920, 1080),
                1,
            );
            let slots: Vec<usize> = layout.hits.iter().map(|(_, slot)| *slot).collect();
            assert_eq!(slots, [0, 1, 2], "{orientation:?}");
            assert!(canvas.stride > 0 && canvas.height > 0);
            // A strip of three is narrower than the grid panel.
            if orientation == Orientation::Row {
                assert!(canvas.stride < launcher::WIDTH as usize);
            }
            // And every hit lies inside the canvas.
            for (rect, _) in &layout.hits {
                assert!(rect.right() <= canvas.stride as i32);
                assert!(rect.bottom() <= canvas.height as i32);
            }
        }
    }

    /// Dump the panel to a PPM so it can be looked at.
    ///
    /// `PINNED_DUMP=/tmp/p.ppm PINNED_APPS=Firefox,Vim PINNED_LAYOUT=grid
    /// cargo test -p huginn-comp pinned_dump -- --nocapture`
    ///
    /// `PINNED_LAYOUT` is grid, row or column; `PINNED_RIGHT=2` moves the
    /// highlight; `PINNED_TAB=1` opens the menu and steps down once.
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
        if let Some(orientation) = std::env::var("PINNED_LAYOUT")
            .ok()
            .and_then(|v| Orientation::from_value(&v))
        {
            pins.set_orientation(orientation);
        }
        let mut pinned = Pinned::default();
        pinned.open(&apps, &pins, CLOCK, STILL);
        for _ in 0..std::env::var("PINNED_RIGHT")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            pinned.press(Key::Right, &apps, &mut pins, CLOCK, STILL);
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
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let (canvas, layout) = compose(&pinned, &apps, &mut text, &icons, &mut pixmaps, output, 1);
        for (rect, slot) in &layout.hits {
            println!("hit {slot}: {rect:?}");
        }
        for (rect, item) in &layout.menu_hits {
            println!("menu {item}: {rect:?}");
        }
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
