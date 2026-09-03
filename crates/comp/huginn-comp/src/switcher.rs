//! The window switcher's arithmetic: who was focused most recently, and how
//! a highlight steps through a list.
//!
//! Alt-Tab lists every window, most recently focused first, across every
//! workspace, so the first press swaps to the window you were just in and
//! each further press goes one further back. That order is not one the core
//! keeps — `huginn-core` tracks focus per workspace and knows nothing about
//! time — so the compositor keeps it here, fed from the one place every focus
//! change passes through. Pure functions over ids, so the rules are tested
//! without a display.

use huginn_core::window::WindowId;
use huginn_core::workspace::Direction;

/// `focused` has focus now: move it to the front. A window is in the list
/// once, however often it has been focused.
pub(crate) fn note_focus(history: &mut Vec<WindowId>, focused: WindowId) {
    history.retain(|id| *id != focused);
    history.insert(0, focused);
}

/// Forget windows that no longer exist.
pub(crate) fn prune(history: &mut Vec<WindowId>, alive: impl Fn(WindowId) -> bool) {
    history.retain(|id| alive(*id));
}

/// `all` in most-recently-focused order: the ones in `history` first, in its
/// order, then the ones never focused, in the order given. Anything in the
/// history that is not in `all` is left out.
pub(crate) fn order(history: &[WindowId], all: &[WindowId]) -> Vec<WindowId> {
    let mut out: Vec<WindowId> = history
        .iter()
        .copied()
        .filter(|id| all.contains(id))
        .collect();
    out.extend(all.iter().copied().filter(|id| !history.contains(id)));
    out
}

/// One step from `current` through a list `len` long, wrapping at both ends.
/// An empty list has nowhere to go.
pub(crate) fn step(current: usize, dir: Direction, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = current.min(len - 1);
    match dir {
        Direction::Forward => (current + 1) % len,
        Direction::Backward => (current + len - 1) % len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are allocated by a `Space` and nothing else, so tests borrow one.
    fn ids(count: usize) -> Vec<WindowId> {
        let mut space =
            huginn_core::Space::new(huginn_core::geometry::Rect::from_xywh(0, 0, 1920, 1080));
        (0..count).map(|_| space.open_window()).collect()
    }

    #[test]
    fn focusing_moves_a_window_to_the_front_and_never_duplicates_it() {
        let w = ids(3);
        let mut history = Vec::new();
        note_focus(&mut history, w[0]);
        note_focus(&mut history, w[1]);
        note_focus(&mut history, w[2]);
        assert_eq!(history, vec![w[2], w[1], w[0]]);
        note_focus(&mut history, w[0]);
        assert_eq!(history, vec![w[0], w[2], w[1]]);
        note_focus(&mut history, w[0]);
        assert_eq!(
            history,
            vec![w[0], w[2], w[1]],
            "refocusing changes nothing"
        );
    }

    #[test]
    fn pruning_forgets_the_dead() {
        let w = ids(3);
        let mut history = vec![w[2], w[1], w[0]];
        prune(&mut history, |id| id != w[1]);
        assert_eq!(history, vec![w[2], w[0]]);
    }

    #[test]
    fn ordering_puts_the_history_first_and_the_never_focused_after() {
        let w = ids(5);
        // w[4] was focused once but has since closed: it is in the history
        // and not in `all`.
        let history = vec![w[3], w[1], w[4]];
        let all = vec![w[0], w[1], w[2], w[3]];
        assert_eq!(order(&history, &all), vec![w[3], w[1], w[0], w[2]]);
        assert!(order(&history, &[]).is_empty());
        assert_eq!(order(&[], &all), all, "no history is the given order");
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        assert_eq!(step(0, Direction::Forward, 3), 1);
        assert_eq!(step(2, Direction::Forward, 3), 0);
        assert_eq!(step(0, Direction::Backward, 3), 2);
        assert_eq!(step(1, Direction::Backward, 3), 0);
    }

    #[test]
    fn a_list_of_one_stays_put_and_an_empty_one_has_nowhere_to_go() {
        assert_eq!(step(0, Direction::Forward, 1), 0);
        assert_eq!(step(0, Direction::Backward, 1), 0);
        assert_eq!(step(4, Direction::Forward, 0), 0);
        assert_eq!(
            step(7, Direction::Forward, 3),
            0,
            "an index past the end is clamped first"
        );
    }
}
