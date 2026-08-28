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
use workspace::{Direction, Layout, Workspace, WorkspaceId};

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
    /// The workspace whose strip a touchpad swipe currently has hold of.
    ///
    /// While this names the active workspace the fingers decide where the strip
    /// sits and [`Self::update_carousel_target`] stops deriving it from focus.
    /// Without that, every frame of a drag would pull the strip back to
    /// whichever pane happened to be focused when the fingers went down, and
    /// the gesture would spend its whole length fighting the layout.
    ///
    /// A workspace id rather than a flag, so switching workspace mid-swipe ends
    /// the drag for the one you left instead of freezing the one you arrive on.
    carousel_drag: Option<WorkspaceId>,
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
            carousel_drag: None,
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
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| !w.is_floating() && !w.is_minimized())
            })
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
        // Read before the workspace is borrowed, not because the order matters
        // to the logic but because it does to the borrow checker.
        let drag = self.carousel_drag;
        let ws = &mut self.workspaces[self.active];
        if ws.layout() != Layout::Carousel {
            return None;
        }
        if drag == Some(ws.id()) {
            // The fingers are deciding. Report where they have put it rather
            // than where focus would ask for — a drag is the one time the strip
            // is allowed to leave the focused pane off screen, because the pane
            // it settles on is not chosen until they lift.
            return Some(ws.scroll());
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

    /// Take hold of the active workspace's strip for a touchpad swipe, and
    /// report the offset it is resting at — where the drag starts from.
    ///
    /// A tiled workspace becomes a carousel here, because the gesture is how
    /// you get there. Three fingers on the strip is the same statement
    /// [`Self::toggle_layout`] makes, so it leaves the workspace in the same
    /// place and the layout stays put when the fingers lift.
    ///
    /// `None` when the strip has nowhere to go: no panes at all, or few enough
    /// that they already fill the viewport. Both are the same thing to a swipe.
    /// There is nothing to take hold of, and flipping the layout under a
    /// gesture that can do nothing visible would be a mode change the user
    /// never sees happen — the workspace would sit in [`Layout::Carousel`] with
    /// nothing on screen to say so, and the next `Super`+`Shift`+`C` would read
    /// as inverted because it toggles back to tiling.
    ///
    /// "Nowhere to go" is [`strip::max_offset`] rather than an emptiness test,
    /// because at the default two columns a workspace of one or two panes lays
    /// out identically under both layouts. Refusing on emptiness alone accepted
    /// exactly those cases, which is the invisible flip this rule exists to
    /// prevent.
    pub fn begin_carousel_drag(&mut self) -> Option<i32> {
        // Subsumes the empty case: `max_offset` is zero for a strip with no
        // panes, so there is no separate emptiness test to keep in step.
        let tiled = self.tiled_windows();
        if strip::max_offset(&tiled, self.area, self.gap, self.carousel_columns) == 0 {
            return None;
        }
        let ws = &mut self.workspaces[self.active];
        ws.set_layout(Layout::Carousel);
        let (id, scroll) = (ws.id(), ws.scroll());
        self.carousel_drag = Some(id);
        Some(scroll)
    }

    /// Whether a swipe has hold of the active workspace's strip.
    ///
    /// The compositor asks so it can put the strip exactly where the fingers
    /// are rather than sliding towards them. A drag that animated would sit a
    /// fixed distance behind the hand moving it, which is the one thing direct
    /// manipulation must not do.
    pub fn carousel_dragging(&self) -> bool {
        self.carousel_drag == Some(self.workspaces[self.active].id())
    }

    /// Move the held strip to `offset`, clamped to its own ends, and report
    /// where it actually landed. Call [`Self::arrange`] after.
    ///
    /// The clamp is the whole of the edge behaviour: the strip stops and the
    /// fingers keep going. No rubber band, because there is nothing past the
    /// end to hint at — the strip is the entire workspace, and stretching it
    /// would suggest content that does not exist.
    ///
    /// `None`, and nothing moves, unless [`Self::begin_carousel_drag`] took
    /// hold of *this* workspace first.
    pub fn drag_carousel(&mut self, offset: i32) -> Option<i32> {
        let held = self.carousel_drag?;
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area, self.gap, self.carousel_columns);
        let ws = &mut self.workspaces[self.active];
        if held != ws.id() {
            return None;
        }
        let landed = offset.clamp(0, strip::max_offset(&tiled, area, gap, columns));
        ws.set_scroll(landed);
        Some(landed)
    }

    /// Let go of the strip: settle it onto the pane nearest where the fingers
    /// left it, focus that pane, and report it. Call [`Self::arrange`] after.
    ///
    /// Focusing is what makes the settle stick. Everything else about the
    /// carousel derives the scroll from focus, so a drag that moved the strip
    /// without moving focus would be undone by the very next arrange — and the
    /// pane you swiped to would not be the one the keyboard was talking to,
    /// which is a worse bug than the strip springing back, because it is quiet.
    ///
    /// Ends the drag either way, so a swipe over an emptied workspace releases
    /// its hold rather than leaving the strip pinned.
    pub fn end_carousel_drag(&mut self) -> Option<WindowId> {
        let held = self.carousel_drag.take()?;
        let tiled = self.tiled_windows();
        let (area, gap, columns) = (self.area, self.gap, self.carousel_columns);
        let ws = &mut self.workspaces[self.active];
        if held != ws.id() {
            return None;
        }
        let (pane, offset) = strip::snap(&tiled, area, gap, columns, ws.scroll())?;
        ws.set_scroll(offset);
        ws.focus(pane);
        Some(pane)
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

    /// Move focus through panes that are actually present on screen.
    pub fn cycle_focus(&mut self, dir: Direction) -> Option<WindowId> {
        let order: Vec<_> = self.workspaces[self.active]
            .cycle_order()
            .into_iter()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|window| !window.is_minimized())
            })
            .collect();
        if order.is_empty() {
            self.workspaces[self.active].set_focus(None);
            return None;
        }
        let current = self
            .focused()
            .and_then(|focused| order.iter().position(|id| *id == focused))
            .unwrap_or(0);
        let next = match dir {
            Direction::Forward => (current + 1) % order.len(),
            Direction::Backward => (current + order.len() - 1) % order.len(),
        };
        self.workspaces[self.active].focus(order[next]);
        Some(order[next])
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

    /// Minimize the focused pane while leaving its client alive in this workspace.
    pub fn minimize_focused(&mut self) -> Option<WindowId> {
        let id = self.focused()?;
        self.windows.get_mut(&id)?.minimize();

        let next = self.workspaces[self.active]
            .windows()
            .iter()
            .copied()
            .find(|candidate| {
                *candidate != id
                    && self
                        .windows
                        .get(candidate)
                        .is_some_and(|window| !window.is_minimized())
            });
        self.workspaces[self.active].set_focus(next);
        Some(id)
    }

    /// Move an existing window into the active workspace and focus it.
    pub fn bring_to_active_workspace(&mut self, id: WindowId) -> bool {
        if !self.windows.contains_key(&id) {
            return false;
        }
        for (index, workspace) in self.workspaces.iter_mut().enumerate() {
            if index != self.active {
                workspace.remove(id);
            }
        }
        self.workspaces[self.active].insert(id);
        self.workspaces[self.active].focus(id);
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
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| !w.is_floating() && !w.is_minimized())
            })
            .collect();

        // The tree is a cache over `tiled`, not a second source of truth, so
        // it is brought into line before it is read rather than trusted to
        // have been kept up to date by whoever last changed a window's mode.
        let gap = self.gap;
        let columns = self.carousel_columns;
        let held = self.carousel_offset;
        let drag = self.carousel_drag;
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
                    // A swipe has hold of it, and the compositor has not
                    // handed an offset down this frame. The fingers still win
                    // over focus, or arranging for any other reason mid-drag —
                    // a window opening, a panel resizing the area — would yank
                    // the strip out from under them.
                    None if drag == Some(ws.id()) => ws.scroll(),
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
        debug_assert_eq!(
            laid.len(),
            tiled.len(),
            "the layout and the tiled set disagree"
        );

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
                WindowMode::Minimized => continue,
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
            assert!(
                g.x() >= SCREEN.x() && g.right() <= SCREEN.right(),
                "{g:?} escaped horizontally"
            );
            assert!(
                g.y() >= SCREEN.y() && g.bottom() <= SCREEN.bottom(),
                "{g:?} escaped vertically"
            );
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
        assert!(
            g.right() <= 800 && g.bottom() <= 600,
            "{g:?} left the screen"
        );
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
        assert_eq!(
            s.window(w).expect("still open").geometry.size,
            Size::new(640, 480)
        );
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

        s.window_mut(a)
            .expect("open")
            .unfullscreen(WindowMode::Tiled);
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
            assert!(
                !s.move_focused(dir),
                "{dir:?} found a neighbour that is not there"
            );
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
        assert!(
            !s.move_focused(Dir::Left),
            "a floating window has no slot to swap"
        );

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
        assert!(
            scrolled > 0,
            "the first workspace is scrolled along its strip"
        );

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

    /// A carousel workspace with `n` panes on a 1000x600 screen, focused on the
    /// first, with the strip resting at its start.
    ///
    /// Returns the column stride alongside, read off the geometry rather than
    /// recomputed, so a test that swipes "two panes across" cannot disagree
    /// with the layout about how far that is.
    fn strip_of(n: usize) -> (Space, Vec<WindowId>, i32) {
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        let windows: Vec<WindowId> = (0..n).map(|_| s.open_window()).collect();
        if let Some(first) = windows.first() {
            s.active_workspace_mut().focus(*first);
        }
        s.toggle_layout();
        s.arrange();
        let stride = match windows.get(1) {
            Some(second) => {
                s.window(*second).unwrap().geometry.x() - s.window(windows[0]).unwrap().geometry.x()
            }
            None => 0,
        };
        (s, windows, stride)
    }

    #[test]
    fn a_swipe_turns_a_tiled_workspace_into_the_carousel_and_leaves_it_there() {
        // The gesture is how you get to the carousel, so it makes the same
        // change the keybinding does — including outlasting the fingers.
        //
        // Three panes, not two: at the default two columns a pair already
        // fills the viewport, and a swipe there is refused precisely because
        // it could change nothing visible. See the test below.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        s.open_window();
        s.open_window();
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);

        s.begin_carousel_drag()
            .expect("three panes to take hold of");
        assert_eq!(s.active_workspace().layout(), Layout::Carousel);

        s.end_carousel_drag();
        assert_eq!(
            s.active_workspace().layout(),
            Layout::Carousel,
            "the layout is a decision, not a thing the fingers hold open"
        );
    }

    #[test]
    fn a_swipe_over_a_workspace_that_already_fits_changes_nothing() {
        // The invisible-flip bug: one or two panes at two columns lay out
        // identically tiled and carousel, and the strip cannot scroll at all,
        // so taking the claim would leave the workspace silently in a mode the
        // screen gives no sign of — and the next Super+Shift+C would look
        // inverted, toggling back to tiling instead of into the carousel.
        for panes in [1, 2] {
            let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
            for _ in 0..panes {
                s.open_window();
            }
            assert_eq!(
                strip::max_offset(&s.tiled_windows(), s.area(), s.gap, s.carousel_columns),
                0,
                "{panes} panes must have nothing to scroll"
            );
            assert_eq!(
                s.begin_carousel_drag(),
                None,
                "{panes} panes: a swipe that can do nothing visible must be refused"
            );
            assert_eq!(
                s.active_workspace().layout(),
                Layout::Tiled,
                "{panes} panes: the layout must not flip under a refused swipe"
            );
            assert!(!s.carousel_dragging());
        }
    }

    #[test]
    fn a_swipe_is_taken_as_soon_as_the_strip_can_actually_move() {
        // The other half of the rule: refusing what cannot scroll must not
        // turn into refusing a strip that can. A third pane is what makes the
        // strip longer than the viewport at two columns.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        s.open_window();
        s.open_window();
        assert_eq!(s.begin_carousel_drag(), None, "two panes still fit");

        s.open_window();
        assert!(
            s.begin_carousel_drag().is_some(),
            "a third pane gives the strip somewhere to go"
        );
        assert_eq!(s.active_workspace().layout(), Layout::Carousel);
    }

    #[test]
    fn a_swipe_over_an_empty_workspace_changes_nothing() {
        // Nothing to take hold of. Flipping the layout anyway would be a mode
        // change with no visible cause, discovered later on an empty screen.
        let mut s = Space::new(Rect::from_xywh(0, 0, 1000, 600));
        assert_eq!(s.begin_carousel_drag(), None);
        assert_eq!(s.active_workspace().layout(), Layout::Tiled);
        assert!(!s.carousel_dragging());
    }

    #[test]
    fn the_strip_follows_the_fingers_rather_than_the_focused_pane() {
        // The whole point of the drag flag: focus is still on pane 1, and the
        // strip must be allowed to leave it off screen until the fingers lift.
        let (mut s, windows, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        assert_eq!(s.drag_carousel(700), Some(700));
        assert_eq!(
            s.update_carousel_target(),
            Some(700),
            "focus does not pull it back"
        );

        s.arrange();
        let first = s.window(windows[0]).unwrap().geometry;
        assert!(
            first.right() < 0,
            "pane 1 is off screen while the drag holds it there"
        );
    }

    #[test]
    fn a_drag_stops_at_the_ends_of_the_strip() {
        let (mut s, windows, stride) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");

        assert_eq!(s.drag_carousel(-4000), Some(0), "it stops at the start");
        let end = s
            .drag_carousel(99_999)
            .expect("the drag holds this workspace");
        assert!(
            end > 0 && end < 5 * stride,
            "the end is the last pane flush, not past it"
        );

        // Which is to say: at the end, the final pane sits at the right edge.
        s.arrange();
        assert_eq!(s.window(windows[5]).unwrap().geometry.right(), 992);
    }

    #[test]
    fn letting_go_settles_on_a_pane_and_focuses_it() {
        let (mut s, windows, stride) = strip_of(6);
        assert_eq!(s.focused(), Some(windows[0]));

        s.begin_carousel_drag().expect("panes to take hold of");
        // Two full columns across, so the third pane is what the fingers left
        // at the left of the viewport.
        s.drag_carousel(stride * 2);
        let settled = s.end_carousel_drag().expect("a pane to settle on");

        assert_eq!(settled, windows[2]);
        assert_eq!(
            s.focused(),
            Some(windows[2]),
            "the keyboard went where the fingers did"
        );
        assert!(!s.carousel_dragging(), "the strip was let go of");
    }

    #[test]
    fn a_settled_strip_stays_where_the_swipe_left_it() {
        // The regression this guards: the drag moves the strip, focus stays
        // put, and the next arrange snaps it straight back. Focusing the pane
        // it settled on is what stops that, and this is the proof.
        let (mut s, _, stride) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        // Two columns across and a few pixels short of it, so the settle has
        // somewhere to round to and the test is not asserting on a no-op.
        s.drag_carousel(stride * 2 - 6);
        s.end_carousel_drag().expect("a pane to settle on");

        let after = s.update_carousel_target().expect("a carousel has a target");
        assert_eq!(
            after,
            stride * 2,
            "the arrange after the swipe leaves it alone"
        );
        // And again, because the nudge rule reads the position it last wrote.
        assert_eq!(s.update_carousel_target(), Some(stride * 2));
    }

    #[test]
    fn a_swipe_that_barely_moves_settles_back_where_it_started() {
        let (mut s, windows, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        s.drag_carousel(20);
        assert_eq!(s.end_carousel_drag(), Some(windows[0]));
        assert_eq!(
            s.update_carousel_target(),
            Some(0),
            "back to the start, not stuck at 20"
        );
    }

    #[test]
    fn switching_workspace_mid_swipe_lets_go_rather_than_freezing_the_new_one() {
        // The drag is held by workspace id precisely so this cannot strand the
        // strip you arrive on outside the focus rule that governs it.
        let (mut s, _, _) = strip_of(6);
        s.begin_carousel_drag().expect("panes to take hold of");
        s.drag_carousel(600);

        s.activate_workspace(1);
        assert!(
            !s.carousel_dragging(),
            "the swipe does not follow you across"
        );
        assert_eq!(
            s.drag_carousel(900),
            None,
            "and it cannot move the strip here"
        );
        assert_eq!(s.end_carousel_drag(), None);
    }

    #[test]
    fn a_swipe_never_settles_a_pane_the_strip_does_not_show() {
        // The pairing `strip::snap` promises, exercised through the whole path
        // rather than in the layout alone.
        for stop in [0, 130, 495, 700, 1200, 9_000] {
            let (mut s, _, _) = strip_of(7);
            s.begin_carousel_drag().expect("panes to take hold of");
            s.drag_carousel(stop);
            let pane = s.end_carousel_drag().expect("a pane to settle on");
            s.arrange();
            let geometry = s.window(pane).unwrap().geometry;
            assert!(
                geometry.x() >= 0 && geometry.right() <= 1000,
                "letting go at {stop} focused a pane at {geometry:?}, off screen"
            );
        }
    }

    #[test]
    fn minimizing_removes_only_the_focused_pane_from_layout() {
        let mut s = space();
        let first = s.open_window();
        let second = s.open_window();
        s.arrange();

        assert_eq!(s.minimize_focused(), Some(second));
        let changed = s.arrange();
        assert!(s.window(second).unwrap().is_minimized());
        assert_eq!(s.focused(), Some(first));
        assert_eq!(
            s.window(first).unwrap().geometry,
            Rect::from_xywh(8, 8, 1904, 1064)
        );
        assert!(changed.iter().all(|(id, _)| *id != second));
    }

    #[test]
    fn a_minimized_pane_can_be_brought_to_the_current_workspace() {
        let mut s = space();
        let pane = s.open_window();
        s.minimize_focused();
        s.activate_workspace(2);

        assert!(s.bring_to_active_workspace(pane));
        assert_eq!(s.active_workspace().windows(), &[pane]);
        assert_eq!(s.focused(), Some(pane));
        assert!(!s.workspaces()[0].windows().contains(&pane));
    }
}
