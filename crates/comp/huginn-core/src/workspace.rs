//! Workspaces: membership, focus, and the tile tree.
//!
//! Two structures, deliberately, because they answer different questions.
//! `windows` is *who lives here* — every window on this workspace, tiled or
//! floating or fullscreen. [`Tiles`] is *how the tiled ones divide the screen*,
//! and it holds a strict subset: a window that goes fullscreen leaves the tree
//! and comes back to it on the way out.
//!
//! Keeping both risks them disagreeing, so the tree is never treated as
//! authoritative. [`Workspace::reconcile_tiles`] rebuilds it from the
//! membership list and each window's mode every time the layout runs, which
//! turns a missed update from a wrong layout into nothing at all.

use crate::tiles::Tiles;
use crate::window::WindowId;
use crate::geometry::Rect;

/// Opaque handle to a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(u64);

/// How a workspace arranges its tiled windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// The split tree in [`crate::tiles`]: every window on screen, each new one
    /// making the others smaller.
    #[default]
    Tiled,
    /// The strip in [`crate::strip`]: panes keep their width and the workspace
    /// scrolls past the edge of the output.
    Carousel,
}

impl WorkspaceId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Which way [`Workspace::cycle_focus`] moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// The windows of one pane, and how the tiled ones are divided.
///
/// Focus is tracked by id rather than by index so that reordering never
/// silently moves focus to a different window.
#[derive(Debug)]
pub struct Workspace {
    id: WorkspaceId,
    /// How this workspace lays its tiled windows out.
    ///
    /// Per workspace rather than global: the point of having a second layout is
    /// to use it where it suits the work, and a setting that changed every
    /// workspace at once would make it a mode you switch into rather than a
    /// property of the space you are in.
    layout: Layout,
    /// Everyone here, in the order they were opened relative to one another.
    /// Includes floating and fullscreen windows, which the tree does not.
    windows: Vec<WindowId>,
    focus: Option<WindowId>,
    /// The split tree over the *tiled* subset of `windows`.
    tiles: Tiles,
}

impl Workspace {
    pub(crate) fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            layout: Layout::default(),
            windows: Vec::new(),
            focus: None,
            tiles: Tiles::new(),
        }
    }

    pub const fn id(&self) -> WorkspaceId {
        self.id
    }

    /// How this workspace lays out.
    pub const fn layout(&self) -> Layout {
        self.layout
    }

    /// Switch between tiling and the carousel.
    ///
    /// The tile tree is left alone. It is a cache over the tiled set that
    /// `reconcile_tiles` rebuilds anyway, so a workspace that goes to the
    /// carousel and back returns to the splits it had rather than to a fresh
    /// row of equal columns.
    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    /// Flip between the two layouts, and report the one now in force.
    pub fn toggle_layout(&mut self) -> Layout {
        self.layout = match self.layout {
            Layout::Tiled => Layout::Carousel,
            Layout::Carousel => Layout::Tiled,
        };
        self.layout
    }

    pub fn windows(&self) -> &[WindowId] {
        &self.windows
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub const fn focused(&self) -> Option<WindowId> {
        self.focus
    }

    /// The split tree over this workspace's tiled windows.
    pub fn tiles(&self) -> &Tiles {
        &self.tiles
    }

    /// The split tree, mutably. For resizing, which changes a split's ratio
    /// rather than the set of windows — the one edit that does not go through
    /// [`Self::reconcile_tiles`].
    pub fn tiles_mut(&mut self) -> &mut Tiles {
        &mut self.tiles
    }

    /// Bring the tree into line with `tiled`, the windows that belong in it.
    ///
    /// Anything in the tree but not in `tiled` is dropped and its space
    /// collapses; anything in `tiled` but not in the tree is inserted beside
    /// the focused window, so a window that stops being fullscreen returns to
    /// the tile next to whatever you were looking at.
    pub fn reconcile_tiles(&mut self, tiled: &[WindowId], area: Rect, gap: i32) {
        self.tiles.retain(|w| tiled.contains(&w));
        for window in tiled {
            if !self.tiles.contains(*window) {
                self.tiles.insert(*window, self.focus, area, gap);
            }
        }
    }

    /// Insert `window` directly after the focused window and focus it.
    ///
    /// Opening beside the focus rather than appending to the end is what makes
    /// "open a terminal next to this editor" behave the way it reads. The tile
    /// tree is not touched here — it has no way to know whether the new window
    /// is even tiled yet — and picks the window up at the next reconcile.
    pub fn insert(&mut self, window: WindowId) {
        if self.windows.contains(&window) {
            return;
        }
        let at = match self.focus.and_then(|f| self.index_of(f)) {
            Some(i) => i + 1,
            None => self.windows.len(),
        };
        self.windows.insert(at, window);
        self.focus = Some(window);
    }

    /// Remove `window`, moving focus to a neighbour if it held focus.
    ///
    /// Returns whether the window was present.
    pub fn remove(&mut self, window: WindowId) -> bool {
        let Some(i) = self.index_of(window) else {
            return false;
        };
        self.windows.remove(i);
        self.tiles.remove(window);

        if self.focus == Some(window) {
            // Prefer the window that slid into the vacated slot; fall back to
            // the previous one when the last window was closed, so focus never
            // lands on nothing while windows remain.
            self.focus = self.windows.get(i).or_else(|| self.windows.last()).copied();
        }
        true
    }

    /// Focus `window` if it lives here. Returns whether it did.
    pub fn focus(&mut self, window: WindowId) -> bool {
        if self.windows.contains(&window) {
            self.focus = Some(window);
            true
        } else {
            false
        }
    }

    /// Move focus one window in `dir`, wrapping around.
    ///
    /// Walks [`Self::cycle_order`], which is screen order rather than the order
    /// things were opened in — "focus the next window" has to mean the one you
    /// can see beside this one, or the key is unusable the moment anything has
    /// been swapped or promoted.
    pub fn cycle_focus(&mut self, dir: Direction) -> Option<WindowId> {
        let order = self.cycle_order();
        let len = order.len();
        if len == 0 {
            return None;
        }
        let current = self
            .focus
            .and_then(|f| order.iter().position(|w| *w == f))
            .unwrap_or(0);
        let next = match dir {
            Direction::Forward => (current + 1) % len,
            Direction::Backward => (current + len - 1) % len,
        };
        self.focus = Some(order[next]);
        self.focus
    }

    /// The order focus travels in: tiles as they read on screen, then the
    /// floating windows.
    ///
    /// Floating windows are appended rather than dropped. Cycling over the tree
    /// alone would strand them — a floating window would be one no key could
    /// ever reach, which is a worse failure than visiting it out of position.
    fn cycle_order(&self) -> Vec<WindowId> {
        let mut order = self.tiles.windows();
        order.extend(
            self.windows
                .iter()
                .copied()
                .filter(|w| !self.tiles.contains(*w)),
        );
        order
    }

    /// Swap the focused window with its neighbour in `dir`, keeping it focused.
    ///
    /// Neighbour in *tile* order, not membership order, because this is a
    /// request to move a window on screen and a floating window is not on the
    /// grid to move past.
    pub fn shift_focused(&mut self, dir: Direction) -> bool {
        let order = self.tiles.windows();
        let Some(current) = self.focus.and_then(|f| order.iter().position(|w| *w == f)) else {
            return false;
        };
        if order.len() < 2 {
            return false;
        }
        let target = match dir {
            Direction::Forward => (current + 1) % order.len(),
            Direction::Backward => (current + order.len() - 1) % order.len(),
        };
        self.tiles.swap(order[current], order[target])
    }

    /// Exchange the positions of two windows in the order.
    ///
    /// Focus is tracked by id, not by index, so whichever of the two held it
    /// keeps it — the window travels and the focus goes with it, which is what
    /// makes a directional move feel like dragging rather than like swapping
    /// two things and being left behind.
    pub fn swap(&mut self, a: WindowId, b: WindowId) -> bool {
        self.tiles.swap(a, b)
    }

    /// Trade the focused window into the first tile.
    ///
    /// A split tree has no master slot, so "promote" means the largest,
    /// left-most tile — the one a window would have had to itself. Swapping
    /// rather than reordering keeps the pane's shape, so the promoted window
    /// lands in a tile that already existed instead of the layout reflowing.
    pub fn promote_focused(&mut self) -> bool {
        let order = self.tiles.windows();
        let (Some(focus), Some(first)) = (self.focus, order.first().copied()) else {
            return false;
        };
        self.tiles.swap(focus, first)
    }

    fn index_of(&self, window: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| *w == window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        Workspace::new(WorkspaceId::from_raw(1))
    }

    fn id(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    /// Insert 1,2,3 in sequence. Each opens after the focus, so order is 1,2,3.
    fn ws_with(n: u64) -> Workspace {
        let mut w = ws();
        for i in 1..=n {
            w.insert(id(i));
        }
        w
    }

    /// The same, with every window tiled — which the tree needs, since
    /// `insert` only records membership and leaves the tree to `arrange`.
    fn tiled_ws(n: u64) -> Workspace {
        let mut w = ws_with(n);
        let all: Vec<WindowId> = w.windows().to_vec();
        w.reconcile_tiles(&all, AREA, 0);
        w
    }

    const AREA: Rect = Rect::from_xywh(0, 0, 1000, 600);

    #[test]
    fn insert_places_after_focus_and_takes_focus() {
        let w = ws_with(3);
        assert_eq!(w.windows(), &[id(1), id(2), id(3)]);
        assert_eq!(w.focused(), Some(id(3)));
    }

    #[test]
    fn insert_after_refocusing_splits_the_list() {
        let mut w = ws_with(3);
        w.focus(id(1));
        w.insert(id(9));
        assert_eq!(w.windows(), &[id(1), id(9), id(2), id(3)]);
    }

    #[test]
    fn insert_is_idempotent() {
        let mut w = ws_with(2);
        w.insert(id(1));
        assert_eq!(w.windows(), &[id(1), id(2)]);
    }

    #[test]
    fn closing_focused_window_focuses_its_successor() {
        let mut w = ws_with(3);
        w.focus(id(2));
        assert!(w.remove(id(2)));
        assert_eq!(w.focused(), Some(id(3)));
    }

    #[test]
    fn closing_the_last_window_falls_back_to_the_previous() {
        let mut w = ws_with(3);
        assert_eq!(w.focused(), Some(id(3)));
        w.remove(id(3));
        assert_eq!(w.focused(), Some(id(2)));
    }

    #[test]
    fn closing_the_only_window_clears_focus() {
        let mut w = ws_with(1);
        w.remove(id(1));
        assert_eq!(w.focused(), None);
        assert!(w.is_empty());
    }

    #[test]
    fn closing_an_unfocused_window_leaves_focus_alone() {
        let mut w = ws_with(3);
        w.focus(id(3));
        w.remove(id(1));
        assert_eq!(w.focused(), Some(id(3)));
    }

    #[test]
    fn removing_an_absent_window_is_a_no_op() {
        let mut w = ws_with(2);
        assert!(!w.remove(id(42)));
        assert_eq!(w.windows().len(), 2);
    }

    #[test]
    fn cycle_focus_wraps_both_ways() {
        let mut w = ws_with(3);
        w.focus(id(3));
        assert_eq!(w.cycle_focus(Direction::Forward), Some(id(1)));
        assert_eq!(w.cycle_focus(Direction::Backward), Some(id(3)));
    }

    #[test]
    fn cycle_focus_on_empty_workspace_is_none() {
        assert_eq!(ws().cycle_focus(Direction::Forward), None);
    }

    #[test]
    fn shift_moves_the_window_not_the_focus() {
        // Membership order is unchanged — it records who lives here, not where
        // they sit. The tile tree is what moves.
        let mut w = tiled_ws(3);
        w.focus(id(1));
        let before = w.windows().to_vec();
        assert!(w.shift_focused(Direction::Forward));
        assert_eq!(w.tiles().windows(), [id(2), id(1), id(3)]);
        assert_eq!(w.windows(), before, "membership order should not move");
        assert_eq!(w.focused(), Some(id(1)), "focus travelled with the window");
    }

    #[test]
    fn shifting_needs_two_tiled_windows_to_mean_anything() {
        let mut w = tiled_ws(1);
        w.focus(id(1));
        assert!(!w.shift_focused(Direction::Forward));
    }

    #[test]
    fn swap_exchanges_positions_and_keeps_focus() {
        let mut w = tiled_ws(3);
        w.focus(id(1));
        assert!(w.swap(id(1), id(3)));
        assert_eq!(w.tiles().windows(), [id(3), id(2), id(1)]);
        assert_eq!(w.focused(), Some(id(1)), "focus followed the window");
    }

    #[test]
    fn swapping_with_an_absent_or_identical_window_is_a_no_op() {
        let mut w = tiled_ws(2);
        assert!(!w.swap(id(1), id(99)));
        assert!(!w.swap(id(1), id(1)));
        assert_eq!(w.tiles().windows(), [id(1), id(2)]);
    }

    #[test]
    fn promote_trades_the_focused_window_into_the_first_tile() {
        // A split tree has no master slot, so promote is a swap into the
        // first tile rather than a reordering of the whole pane.
        let mut w = tiled_ws(3);
        w.focus(id(3));
        assert!(w.promote_focused());
        assert_eq!(w.tiles().windows(), [id(3), id(2), id(1)]);
        assert!(!w.promote_focused(), "already in the first tile");
    }

    #[test]
    fn focus_cycling_follows_what_is_on_screen_not_what_was_opened_first() {
        // Regression. `shift_focused` and friends reorder the TREE and leave
        // membership alone, so cycling over membership order sends focus to
        // whichever window happened to be opened next — which after any swap
        // is not the one sitting beside you.
        let mut w = tiled_ws(3);
        w.focus(id(3));
        assert!(w.promote_focused());
        assert_eq!(w.tiles().windows(), [id(3), id(2), id(1)]);

        // Focus is on id(3), now in the first tile. Next on screen is id(2).
        assert_eq!(w.cycle_focus(Direction::Forward), Some(id(2)));
        assert_eq!(w.cycle_focus(Direction::Forward), Some(id(1)));
        assert_eq!(w.cycle_focus(Direction::Forward), Some(id(3)), "should wrap");
    }

    #[test]
    fn a_floating_window_can_still_be_reached_by_cycling() {
        // Cycling over tile order alone would strand every floating window:
        // there would be no key that reaches it.
        let mut w = ws_with(3);
        w.reconcile_tiles(&[id(1), id(2)], AREA, 0);
        w.focus(id(1));
        let seen: Vec<Option<WindowId>> =
            (0..3).map(|_| w.cycle_focus(Direction::Forward)).collect();
        assert!(seen.contains(&Some(id(3))), "the floating window was unreachable: {seen:?}");
    }

    #[test]
    fn a_floating_window_is_left_out_of_the_tree_entirely() {
        // Only floating windows have no tile. Membership still holds them.
        let mut w = ws_with(3);
        w.reconcile_tiles(&[id(1), id(3)], AREA, 0);
        assert_eq!(w.tiles().windows(), [id(1), id(3)]);
        assert_eq!(w.windows().len(), 3, "membership lost a window");
    }

    #[test]
    fn closing_a_window_takes_it_out_of_the_tree_as_well() {
        // Otherwise its tile would be held open by a window that is gone.
        let mut w = tiled_ws(3);
        assert!(w.remove(id(2)));
        assert!(!w.tiles().contains(id(2)));
        assert_eq!(w.tiles().windows(), [id(1), id(3)]);
    }
}
