//! Window-management model for the Huginn compositor.
//!
//! This crate is the design surface of the compositor: what a window is, which
//! workspace it lives on, where focus goes when one closes, and how tiles are
//! laid out. It knows nothing about Wayland, DRM, or the GPU, which is exactly
//! the point — it compiles and tests on any host, so window-management
//! behaviour can be iterated on without a display attached.
//!
//! `huginn-comp` owns the other half: it maps [`WindowId`] to a real
//! `WlSurface`, drives the event loop, and turns the output of [`Space::arrange`]
//! into `xdg_toplevel::configure` events.
//!
//! ```
//! use huginn_core::{Space, geometry::Rect};
//!
//! let mut space = Space::new(Rect::from_xywh(0, 0, 1920, 1080));
//! let editor = space.open_window();
//! let terminal = space.open_window();
//!
//! // Two tiled windows share the screen; both get fresh geometry.
//! assert_eq!(space.arrange().len(), 2);
//! assert_eq!(space.focused(), Some(terminal));
//!
//! // Closing the focused window hands focus back rather than dropping it.
//! space.close_window(terminal);
//! assert_eq!(space.focused(), Some(editor));
//! ```

pub mod geometry;
pub mod layer;
pub mod scale;
pub mod strip;
pub mod tiles;
pub mod window;
pub mod workspace;

use std::collections::BTreeMap;

use geometry::{Dir, Rect};
use window::{Window, WindowId, WindowMode};
use workspace::{Layout, Workspace, WorkspaceId};

/// How many workspaces exist at startup.
///
/// Fixed rather than dynamic for now: static workspaces let a panel render a
/// stable row of indicators, which is the behaviour `muninn` is built around.
const DEFAULT_WORKSPACES: u64 = 9;

/// Gutter used until the compositor supplies its own. See [`Space::set_gap`].
const DEFAULT_GAP: i32 = 8;

/// The complete window-management state of one seat.
#[derive(Debug)]
pub struct Space {
    windows: BTreeMap<WindowId, Window>,
    workspaces: Vec<Workspace>,
    active: usize,
    /// Usable area after layer-shell exclusive zones (panels, docks) are
    /// subtracted. Single-output for now; multi-output arrives with the udev
    /// backend, at which point this becomes a per-output field.
    area: Rect,
    /// Space between tiled windows and around the edge of the pane.
    ///
    /// Held here rather than read from a constant so `huginn-core` keeps
    /// knowing nothing about the desktop's appearance; the compositor sets it
    /// once from its theme.
    gap: i32,
    /// How many panes the carousel shows at once.
    ///
    /// Held beside [`Self::gap`] and for the same reason: how wide a pane should
    /// be is an appearance decision with geometric consequences, and this crate
    /// does not get to make appearance decisions. The compositor sets it once
    /// from its theme.
    carousel_columns: u32,
    /// Where the carousel is scrolled to right now, when that is not simply
    /// wherever focus is.
    ///
    /// `None` means "wherever focus is" — the layout resolves the offset itself
    /// and the strip is wherever it belongs. `Some` is the compositor sliding
    /// the strip towards that place over several frames, which is the one thing
    /// this crate cannot work out for itself because it has no clock.
    carousel_offset: Option<i32>,
    next_window: u64,
}

impl Space {
    /// Create a space with [`DEFAULT_WORKSPACES`] empty workspaces.
    pub fn new(area: Rect) -> Self {
        Self {
            windows: BTreeMap::new(),
            workspaces: (1..=DEFAULT_WORKSPACES)
                .map(|n| Workspace::new(WorkspaceId::from_raw(n)))
                .collect(),
            active: 0,
            area,
            gap: DEFAULT_GAP,
            carousel_columns: strip::DEFAULT_COLUMNS,
            carousel_offset: None,
            next_window: 1,
        }
    }

    /// Set the gutter between tiled windows. Call [`Self::arrange`] after.
    pub fn set_gap(&mut self, gap: i32) {
        self.gap = gap.max(0);
    }

    /// Set how many panes the carousel shows at once. Call [`Self::arrange`]
    /// after. Clamped to at least one, since zero columns has no layout.
    pub fn set_carousel_columns(&mut self, columns: u32) {
        self.carousel_columns = columns.max(1);
    }

    /// Flip the active workspace between tiling and the carousel, and report
    /// the layout now in force. Call [`Self::arrange`] after.
    pub fn toggle_layout(&mut self) -> Layout {
        self.workspaces[self.active].toggle_layout()
    }

    /// The tiled windows of the active workspace, in order.
    fn tiled_windows(&self) -> Vec<WindowId> {
        self.workspaces[self.active]
            .windows()
            .iter()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(|w| !w.is_floating()))
            .collect()
    }

    /// Work out where the active workspace's strip should now be settled,
    /// record it, and return it. `None` when that workspace is not a carousel.
    ///
    /// The compositor animates towards this. Nothing here moves on its own.
    ///
    /// This commits rather than merely reporting, because the answer depends on
    /// where the strip already is: a pane that is on screen leaves the offset
    /// alone, so the previous position has to be the one that is read next time.
    /// Reporting without committing would compute every step from the same
    /// stale origin and undo the nudge.
    pub fn update_carousel_target(&mut self) -> Option<i32> {
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area, self.gap, self.carousel_columns);
        let ws = &mut self.workspaces[self.active];
        if ws.layout() != Layout::Carousel {
            return None;
        }
        let settled = strip::target_offset(&tiled, ws.focused(), area, gap, columns, ws.scroll());
        ws.set_scroll(settled);
        Some(settled)
    }

    /// Hold the carousel at `offset` while it is sliding, or hand it back to
    /// focus with `None`. Call [`Self::arrange`] after.
    pub fn set_carousel_offset(&mut self, offset: Option<i32>) {
        self.carousel_offset = offset;
    }

    /// The area available to windows.
    pub const fn area(&self) -> Rect {
        self.area
    }

    /// Update the usable area, e.g. when a panel claims an exclusive zone or an
    /// output is resized. Call [`Self::arrange`] afterwards to apply it.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// Register a new window on the active workspace and focus it.
    pub fn open_window(&mut self) -> WindowId {
        let id = WindowId::from_raw(self.next_window);
        self.next_window += 1;
        self.windows.insert(id, Window::new(id));
        self.workspaces[self.active].insert(id);
        id
    }

    /// Forget a window, removing it from whichever workspace holds it.
    ///
    /// Returns whether the window existed. Safe to call for a window on an
    /// inactive workspace, which is the common case for a client that exits on
    /// its own.
    pub fn close_window(&mut self, id: WindowId) -> bool {
        if self.windows.remove(&id).is_none() {
            return false;
        }
        for ws in &mut self.workspaces {
            if ws.remove(id) {
                break;
            }
        }
        true
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn active_workspace(&self) -> &Workspace {
        &self.workspaces[self.active]
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }

    /// Index of the active workspace, zero-based.
    pub const fn active_index(&self) -> usize {
        self.active
    }

    /// The focused window on the active workspace, if any.
    pub fn focused(&self) -> Option<WindowId> {
        self.active_workspace().focused()
    }

    /// Switch to workspace `index`. Out-of-range indices are ignored rather
    /// than clamped, so a stray keybinding cannot silently jump to workspace 9.
    pub fn activate_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// Send the focused window to workspace `index`, keeping the current
    /// workspace active.
    pub fn send_focused_to_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() || index == self.active {
            return false;
        }
        let Some(id) = self.focused() else {
            return false;
        };
        self.workspaces[self.active].remove(id);
        self.workspaces[index].insert(id);
        true
    }

    /// Swap the focused window with its nearest neighbour in `dir`.
    ///
    /// Neighbours are found by geometry rather than by list order, because the
    /// two disagree the moment a layout is anything but a single row: with a
    /// master column beside a stack, "right" has to mean the window drawn to
    /// the right, whatever index it holds.
    ///
    /// Only tiled windows take part. A floating window has no slot in the order
    /// to swap, and a fullscreen one has no neighbours on screen to swap with.
    /// Returns whether anything moved; call [`Self::arrange`] afterwards to turn
    /// the new order into geometry.
    pub fn move_focused(&mut self, dir: Dir) -> bool {
        let Some(id) = self.focused() else {
            return false;
        };
        if !self.windows.get(&id).is_some_and(Window::is_tiled) {
            return false;
        }
        let Some(target) = self.neighbour(id, dir) else {
            return false;
        };
        self.workspaces[self.active].swap(id, target)
    }

    /// The tiled window nearest `from` in `dir` on the active workspace.
    fn neighbour(&self, from: WindowId, dir: Dir) -> Option<WindowId> {
        let origin = self.windows.get(&from)?.geometry;
        self.workspaces[self.active]
            .windows()
            .iter()
            .filter(|id| **id != from)
            .filter_map(|id| self.windows.get(id))
            .filter(|win| win.is_tiled())
            .filter(|win| dir.advances(origin, win.geometry) && dir.aligned(origin, win.geometry))
            // min_by_key keeps the first of equal keys, so a tie — every window
            // of a stack is equally far right of a full-height master — falls to
            // the one earliest in workspace order.
            .min_by_key(|win| dir.distance(origin, win.geometry))
            .map(Window::id)
    }

    /// Recompute geometry for every window on the active workspace.
    ///
    /// Returns only the windows whose geometry actually changed. The compositor
    /// sends one `configure` per entry, so returning the full set instead would
    /// mean a configure storm on every unrelated state change — clients treat
    /// that as a resize and re-render.
    pub fn arrange(&mut self) -> Vec<(WindowId, Rect)> {
        let area = self.area;
        let ws = &self.workspaces[self.active];

        // Everything that owns a tile — which is everything except floating
        // windows. A fullscreen window stays in the tree so that leaving
        // fullscreen puts it back exactly where it was.
        let tiled: Vec<WindowId> = ws
            .windows()
            .iter()
            .copied()
            .filter(|id| self.windows.get(id).is_some_and(|w| !w.is_floating()))
            .collect();

        // The tree is a cache over `tiled`, not a second source of truth, so
        // it is brought into line before it is read rather than trusted to
        // have been kept up to date by whoever last changed a window's mode.
        let gap = self.gap;
        let columns = self.carousel_columns;
        let held = self.carousel_offset;
        let ws = &mut self.workspaces[self.active];
        let laid = match ws.layout() {
            Layout::Tiled => {
                ws.reconcile_tiles(&tiled, area, gap);
                ws.tiles().arrange(area, gap)
            }
            // The strip needs no reconcile: it lays out `tiled` directly rather
            // than keeping a tree that has to be brought back into line with it.
            // Every window in gets a rect out, including the ones scrolled off
            // screen, which is what keeps the assertion below true for both.
            Layout::Carousel => {
                // A held offset is the compositor part way through a slide. With
                // nothing held, settle the strip here so that arranging on its
                // own — which is what the tests and any non-animating caller
                // do — still lands where focus asks for.
                let offset = match held {
                    Some(offset) => offset,
                    None => {
                        let settled = strip::target_offset(
                            &tiled,
                            ws.focused(),
                            area,
                            gap,
                            columns,
                            ws.scroll(),
                        );
                        ws.set_scroll(settled);
                        settled
                    }
                };
                strip::arrange_at(&tiled, area, gap, columns, offset)
            }
        };
        debug_assert_eq!(laid.len(), tiled.len(), "the layout and the tiled set disagree");

        let mut changed = Vec::new();
        for (id, rect) in laid {
            let Some(win) = self.windows.get_mut(&id) else {
                continue;
            };
            // Holds a tile but is not currently in it; the fullscreen pass
            // below gives it the whole area.
            if !win.is_tiled() {
                continue;
            }
            let sized = Rect::new(rect.origin, win.hints.clamp(rect.size));
            if win.geometry != sized {
                win.geometry = sized;
                changed.push((id, sized));
            }
        }

        // Floating and fullscreen windows sit outside the layout, but still
        // have to follow the area when an output resizes or a panel appears.
        for id in ws.windows() {
            let Some(win) = self.windows.get_mut(id) else {
                continue;
            };
            let wanted = match win.mode {
                WindowMode::Tiled => continue,
                WindowMode::Fullscreen => area,
                WindowMode::Floating => win.geometry.constrain_to(area),
            };
            if win.geometry != wanted {
                win.geometry = wanted;
                changed.push((*id, wanted));
            }
        }

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Dir, Point};
    use crate::window::SizeHints;

    use geometry::Size;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    fn space() -> Space {
        Space::new(SCREEN)
    }

    #[test]
    fn arrange_is_idempotent() {
        let mut s = space();
        s.open_window();
        s.open_window();
        assert_eq!(s.arrange().len(), 2, "first arrange assigns geometry");
        assert!(
            s.arrange().is_empty(),
            "a second arrange with no state change must configure nothing"
        );
    }

    #[test]
    fn tiled_windows_stay_inside_the_area() {
        let mut s = space();
        for _ in 0..5 {
            s.open_window();
        }
        s.arrange();
        for id in s.active_workspace().windows() {
            let g = s.window(*id).expect("window is registered").geometry;
            assert!(g.x() >= SCREEN.x() && g.right() <= SCREEN.right(), "{g:?} escaped horizontally");
            assert!(g.y() >= SCREEN.y() && g.bottom() <= SCREEN.bottom(), "{g:?} escaped vertically");
        }
    }

    #[test]
    fn shrinking_the_area_pulls_floating_windows_back_on_screen() {
        let mut s = space();
        let w = s.open_window();
        let win = s.window_mut(w).expect("just opened");
        win.mode = WindowMode::Floating;
        win.geometry = Rect::from_xywh(1600, 900, 400, 300);
        s.arrange();

        // Unplugging the big monitor.
        s.set_area(Rect::from_xywh(0, 0, 800, 600));
        s.arrange();

        let g = s.window(w).expect("still open").geometry;
        assert!(g.right() <= 800 && g.bottom() <= 600, "{g:?} left the screen");
    }

    #[test]
    fn a_panel_claiming_space_reflows_tiled_windows() {
        let mut s = space();
        s.open_window();
        s.arrange();

        // A 40px top panel takes an exclusive zone.
        s.set_area(Rect::new(Point::new(0, 40), Size::new(1920, 1040)));
        let changed = s.arrange();
        assert_eq!(changed.len(), 1);
        assert!(changed[0].1.y() >= 40, "window overlaps the panel");
    }

    #[test]
    fn size_hints_are_honoured_over_layout_geometry() {
        let mut s = space();
        let w = s.open_window();
        s.window_mut(w).expect("just opened").hints = SizeHints {
            min: None,
            max: Some(Size::new(640, 480)),
        };
        s.arrange();
        assert_eq!(s.window(w).expect("still open").geometry.size, Size::new(640, 480));
    }

    #[test]
    fn fullscreen_covers_the_whole_area_and_restores() {
        let mut s = space();
        let a = s.open_window();
        s.open_window();
        s.arrange();
        let tiled_geom = s.window(a).expect("open").geometry;

        s.window_mut(a).expect("open").fullscreen(SCREEN);
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, SCREEN);

        s.window_mut(a).expect("open").unfullscreen(WindowMode::Tiled);
        s.arrange();
        assert_eq!(s.window(a).expect("open").geometry, tiled_geom);
    }

    #[test]
    fn windows_on_inactive_workspaces_are_not_arranged() {
        let mut s = space();
        let a = s.open_window();
        s.arrange();
        let before = s.window(a).expect("open").geometry;

        s.activate_workspace(1);
        s.open_window();
        s.set_area(Rect::from_xywh(0, 0, 640, 480));
        s.arrange();

        assert_eq!(
            s.window(a).expect("open").geometry,
            before,
            "a hidden window must not be reconfigured"
        );
    }

    #[test]
    fn closing_a_window_on_an_inactive_workspace_works() {
        let mut s = space();
        let a = s.open_window();
        s.activate_workspace(3);
        assert!(s.close_window(a));
        assert!(s.workspaces().iter().all(|w| w.is_empty()));
    }

    #[test]
    fn sending_a_window_away_moves_focus_to_a_neighbour() {
        let mut s = space();
        let a = s.open_window();
        let b = s.open_window();
        assert_eq!(s.focused(), Some(b));

        assert!(s.send_focused_to_workspace(2));
        assert_eq!(s.focused(), Some(a), "focus stays on this workspace");

        s.activate_workspace(2);
        assert_eq!(s.focused(), Some(b), "and the window arrived focused");
    }

    /// Three windows: `a` takes the left half, `b` and `c` split the right,
    /// top to bottom. Each new window splits the tile of the one before it,
    /// and the split follows the longer edge — so a wide pane divides in two
    /// vertically first, then the tall right-hand tile divides horizontally.
    fn master_and_stack() -> (Space, WindowId, WindowId, WindowId) {
        let mut s = space();
        let a = s.open_window();
        let b = s.open_window();
        let c = s.open_window();
        s.arrange();
        (s, a, b, c)
    }

    #[test]
    fn moving_right_out_of_the_master_takes_the_top_of_the_stack() {
        let (mut s, a, b, c) = master_and_stack();
        s.active_workspace_mut().focus(a);
        assert!(s.move_focused(Dir::Right));
        // Tile order, not membership order: membership records who lives here,
        // the tree records where they sit, and only the latter moves.
        assert_eq!(s.active_workspace().tiles().windows(), [b, a, c]);
        assert_eq!(s.focused(), Some(a), "the window moved, not the focus");
    }

    #[test]
    fn moving_within_the_stack_walks_it_one_window_at_a_time() {
        let (mut s, a, b, c) = master_and_stack();
        s.active_workspace_mut().focus(b);
        assert!(s.move_focused(Dir::Down));
        assert_eq!(s.active_workspace().tiles().windows(), [a, c, b]);

        // And back, which must land exactly where it started.
        s.arrange();
        assert!(s.move_focused(Dir::Up));
        assert_eq!(s.active_workspace().tiles().windows(), [a, b, c]);
    }

    #[test]
    fn moving_into_the_wall_does_nothing() {
        let (mut s, a, _b, _c) = master_and_stack();
        s.active_workspace_mut().focus(a);
        // The master column is full height at the left edge: nothing is above,
        // below, or left of it.
        for dir in [Dir::Left, Dir::Up, Dir::Down] {
            assert!(!s.move_focused(dir), "{dir:?} found a neighbour that is not there");
        }
        assert_eq!(s.active_workspace().windows()[0], a);
    }

    #[test]
    fn a_lone_window_has_nowhere_to_move() {
        let mut s = space();
        s.open_window();
        s.arrange();
        assert!(!s.move_focused(Dir::Right));
    }

    #[test]
    fn moving_with_nothing_focused_is_a_no_op() {
        let mut s = space();
        assert!(!s.move_focused(Dir::Left));
    }

    #[test]
    fn floating_windows_do_not_join_the_tiling_order() {
        let (mut s, a, b, _c) = master_and_stack();
        s.window_mut(b).expect("open").mode = WindowMode::Floating;
        s.arrange();
        s.active_workspace_mut().focus(b);
        assert!(!s.move_focused(Dir::Left), "a floating window has no slot to swap");

        // And it is not a target either: from the master, right must skip it.
        s.active_workspace_mut().focus(a);
        assert!(s.move_focused(Dir::Right));
        assert_ne!(s.active_workspace().windows()[0], b);
    }

    #[test]
    fn a_move_is_its_own_inverse() {
        let (mut s, a, b, c) = master_and_stack();
        s.active_workspace_mut().focus(c);
        assert!(s.move_focused(Dir::Up));
        s.arrange();
        assert!(s.move_focused(Dir::Down));
        assert_eq!(s.active_workspace().windows(), &[a, b, c]);
    }

    #[test]
    fn out_of_range_workspace_switches_are_rejected() {
        let mut s = space();
        assert!(!s.activate_workspace(99));
        assert_eq!(s.active_index(), 0);
    }

    #[test]
    fn closing_an_unknown_window_is_a_no_op() {
        let mut s = space();
        assert!(!s.close_window(WindowId::from_raw(999)));
    }

    #[test]
    fn a_workspace_starts_tiled_and_toggles_to_the_carousel() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);
        assert_eq!(s.toggle_layout(), Layout::Carousel);
        assert_eq!(s.toggle_layout(), Layout::Tiled);
    }

    #[test]
    fn the_carousel_keeps_pane_size_as_windows_arrive_and_tiling_does_not() {
        // The difference the mode exists for, stated as the property rather
        // than as one arithmetic coincidence: adding windows to a tiled
        // workspace makes the existing ones smaller, and adding them to a
        // carousel does not — the strip grows past the edge instead.
        // The newest window is the probe: each `insert` splits the tile the
        // *focused* window holds, and opening focuses what you opened, so the
        // first window is split once and then left alone while the newest keeps
        // being halved. Measuring the first would show tiling holding steady.
        let size_after = |layout: Layout, count: usize| {
            let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
            s.active_workspace_mut().set_layout(layout);
            let mut newest = s.open_window();
            for _ in 1..count {
                newest = s.open_window();
            }
            s.arrange();
            let g = s.window(newest).unwrap().geometry;
            (g.w(), g.h())
        };

        assert_eq!(
            size_after(Layout::Carousel, 2),
            size_after(Layout::Carousel, 8),
            "a carousel pane is the same size whether two windows are open or eight"
        );
        assert_ne!(
            size_after(Layout::Tiled, 2),
            size_after(Layout::Tiled, 8),
            "tiling divides the same area further as windows arrive"
        );
    }

    #[test]
    fn a_tiled_workspace_has_no_carousel_offset_to_animate() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        assert_eq!(s.update_carousel_target(), None);
    }

    #[test]
    fn each_workspace_keeps_its_own_scroll_position() {
        // Scroll is per workspace, like the layout it belongs to. Held globally,
        // arriving on a carousel would inherit wherever the last one happened to
        // be sitting.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut first = Vec::new();
        for _ in 0..6 {
            first.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(first[5]);
        let scrolled = s.update_carousel_target().expect("a carousel has a target");
        assert!(scrolled > 0, "the first workspace is scrolled along its strip");

        // A second carousel workspace, with its own short strip.
        assert!(s.activate_workspace(1));
        s.toggle_layout();
        s.open_window();
        assert_eq!(
            s.update_carousel_target(),
            Some(0),
            "a fresh workspace starts at its own beginning, not the last one's"
        );

        // And going back finds the first one where it was left.
        assert!(s.activate_workspace(0));
        assert_eq!(s.update_carousel_target(), Some(scrolled));
    }

    #[test]
    fn toggling_out_to_tiling_and_back_keeps_the_scroll() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut opened = Vec::new();
        for _ in 0..6 {
            opened.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(opened[5]);
        let scrolled = s.update_carousel_target().expect("a carousel has a target");

        s.toggle_layout();
        assert_eq!(s.update_carousel_target(), None, "tiling has no strip");
        s.toggle_layout();
        assert_eq!(
            s.update_carousel_target(),
            Some(scrolled),
            "the strip is where it was, not back at its start"
        );
    }

    #[test]
    fn the_held_offset_slides_the_strip_without_moving_the_target() {
        // The animation contract. The compositor holds the strip part way while
        // it slides; the target stays where focus is, so releasing the hold puts
        // it exactly where it was always heading.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let mut opened = Vec::new();
        for _ in 0..6 {
            opened.push(s.open_window());
        }
        s.toggle_layout();
        s.active_workspace_mut().focus(opened[5]);

        let target = s.update_carousel_target().expect("a carousel has a target");
        assert!(target > 0, "focus on the last pane must scroll the strip");

        s.set_carousel_offset(Some(target / 2));
        s.arrange();
        let midway = s.window(opened[0]).unwrap().geometry.x();

        assert_eq!(
            s.update_carousel_target(),
            Some(target),
            "holding the strip part way must not move where it is going"
        );

        s.set_carousel_offset(None);
        s.arrange();
        let arrived = s.window(opened[0]).unwrap().geometry.x();

        assert!(arrived < midway, "releasing the hold completes the scroll");
        assert_eq!(arrived, midway - (target - target / 2));
    }

    #[test]
    fn a_held_offset_does_not_leak_into_a_tiled_workspace() {
        // `set_carousel_offset` is unconditional, so the compositor clearing it
        // when the layout is not a carousel is what keeps a stale slide from
        // displacing tiled windows. Guard the core side of that too.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let a = s.open_window();
        s.arrange();
        let tiled = s.window(a).unwrap().geometry;

        s.set_carousel_offset(Some(400));
        s.arrange();
        assert_eq!(
            s.window(a).unwrap().geometry,
            tiled,
            "a tiled workspace ignores the carousel's offset"
        );
    }

    #[test]
    fn going_to_the_carousel_and_back_restores_the_tiling() {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let a = s.open_window();
        let b = s.open_window();
        s.arrange();
        let before = (s.window(a).unwrap().geometry, s.window(b).unwrap().geometry);

        s.toggle_layout();
        s.arrange();
        s.toggle_layout();
        s.arrange();

        let after = (s.window(a).unwrap().geometry, s.window(b).unwrap().geometry);
        assert_eq!(before, after, "the tile tree survived the round trip");
    }
}
