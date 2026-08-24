//! Workspaces: window order, focus, and which layout is in effect.

use crate::layout::{Columns, Layout};
use crate::window::WindowId;

/// Opaque handle to a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(u64);

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

/// An ordered set of windows sharing one layout.
///
/// Order is meaningful: index 0 is the master slot, and layouts consume the
/// list in order. Focus is tracked by id rather than index so that reordering
/// never silently moves focus to a different window.
#[derive(Debug)]
pub struct Workspace {
    id: WorkspaceId,
    windows: Vec<WindowId>,
    focus: Option<WindowId>,
    layout: Box<dyn Layout>,
}

impl Workspace {
    pub(crate) fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            windows: Vec::new(),
            focus: None,
            layout: Box::new(Columns::default()),
        }
    }

    pub const fn id(&self) -> WorkspaceId {
        self.id
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

    pub fn layout(&self) -> &dyn Layout {
        self.layout.as_ref()
    }

    pub fn set_layout(&mut self, layout: Box<dyn Layout>) {
        self.layout = layout;
    }

    /// Insert `window` directly after the focused window and focus it.
    ///
    /// Opening beside the focus rather than appending to the end is what makes
    /// "open a terminal next to this editor" behave the way it reads.
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
    pub fn cycle_focus(&mut self, dir: Direction) -> Option<WindowId> {
        let len = self.windows.len();
        if len == 0 {
            return None;
        }
        let current = self.focus.and_then(|f| self.index_of(f)).unwrap_or(0);
        let next = match dir {
            Direction::Forward => (current + 1) % len,
            Direction::Backward => (current + len - 1) % len,
        };
        self.focus = Some(self.windows[next]);
        self.focus
    }

    /// Swap the focused window with its neighbour in `dir`, keeping it focused.
    pub fn shift_focused(&mut self, dir: Direction) -> bool {
        let len = self.windows.len();
        let Some(current) = self.focus.and_then(|f| self.index_of(f)) else {
            return false;
        };
        if len < 2 {
            return false;
        }
        let target = match dir {
            Direction::Forward => (current + 1) % len,
            Direction::Backward => (current + len - 1) % len,
        };
        self.windows.swap(current, target);
        true
    }

    /// Exchange the positions of two windows in the order.
    ///
    /// Focus is tracked by id, not by index, so whichever of the two held it
    /// keeps it — the window travels and the focus goes with it, which is what
    /// makes a directional move feel like dragging rather than like swapping
    /// two things and being left behind.
    pub fn swap(&mut self, a: WindowId, b: WindowId) -> bool {
        let (Some(i), Some(j)) = (self.index_of(a), self.index_of(b)) else {
            return false;
        };
        if i == j {
            return false;
        }
        self.windows.swap(i, j);
        true
    }

    /// Promote the focused window to the master slot.
    pub fn promote_focused(&mut self) -> bool {
        let Some(current) = self.focus.and_then(|f| self.index_of(f)) else {
            return false;
        };
        if current == 0 {
            return false;
        }
        let w = self.windows.remove(current);
        self.windows.insert(0, w);
        true
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
        let mut w = ws_with(3);
        w.focus(id(1));
        assert!(w.shift_focused(Direction::Forward));
        assert_eq!(w.windows(), &[id(2), id(1), id(3)]);
        assert_eq!(w.focused(), Some(id(1)));
    }

    #[test]
    fn swap_exchanges_positions_and_keeps_focus() {
        let mut w = ws_with(3);
        w.focus(id(1));
        assert!(w.swap(id(1), id(3)));
        assert_eq!(w.windows(), &[id(3), id(2), id(1)]);
        assert_eq!(w.focused(), Some(id(1)), "focus followed the window");
    }

    #[test]
    fn swapping_with_an_absent_or_identical_window_is_a_no_op() {
        let mut w = ws_with(2);
        assert!(!w.swap(id(1), id(99)));
        assert!(!w.swap(id(1), id(1)));
        assert_eq!(w.windows(), &[id(1), id(2)]);
    }

    #[test]
    fn promote_moves_focused_to_master() {
        let mut w = ws_with(3);
        w.focus(id(3));
        assert!(w.promote_focused());
        assert_eq!(w.windows(), &[id(3), id(1), id(2)]);
        assert!(!w.promote_focused(), "already master");
    }
}
