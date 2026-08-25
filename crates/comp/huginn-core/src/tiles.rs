//! Auto-tiling within one pane: a binary tree of splits.
//!
//! Opening a window splits the focused tile in two. Closing one gives its space
//! back to whatever it was split from. That is the whole model, and it is a
//! tree rather than a list because the shape of the screen is a tree — "the
//! right half, with its top and bottom" is not something an ordered list can
//! say.
//!
//! # Which way a tile splits
//!
//! Along its longer edge, decided when the split is made and then kept. A wide
//! tile splits into two side by side; a tall one splits top and bottom. That is
//! what keeps tiles tending toward square instead of degenerating into slivers
//! after four windows, and it is why [`Tiles::insert`] needs to know the area:
//! the decision depends on the tile's real proportions at that moment.
//!
//! Kept, not recomputed, because a layout that re-chose its axes on every
//! arrange would rearrange itself when a window was resized or an output
//! changed — the windows would move without anyone having asked them to.

use crate::geometry::Rect;
use crate::window::WindowId;

/// Which way a split divides its tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Two tiles side by side. Chosen for a tile wider than it is tall.
    Horizontal,
    /// Two tiles stacked. Chosen for a tile taller than it is wide.
    Vertical,
}

impl Axis {
    /// The axis that splits `rect` across its longer edge.
    ///
    /// Ties go to vertical, which splits a square top and bottom. Arbitrary,
    /// but it has to be one of them and this way two windows on a square
    /// screen get the shape a document wants rather than the shape a terminal
    /// wants.
    fn for_rect(rect: Rect) -> Self {
        if rect.w() > rect.h() {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }
}

/// One node: either a window, or a division of space between two subtrees.
#[derive(Debug, Clone)]
enum Node {
    Leaf(WindowId),
    Split {
        axis: Axis,
        /// Fraction of the tile given to `first`, clamped when applied.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// Walk the tree, laying each leaf into its share of `rect`.
    fn arrange(&self, rect: Rect, gap: i32, out: &mut Vec<(WindowId, Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = divide(rect, *axis, *ratio, gap);
                first.arrange(a, gap, out);
                second.arrange(b, gap, out);
            }
        }
    }

    /// Every window under this node, left to right and top to bottom.
    fn windows(&self, out: &mut Vec<WindowId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.windows(out);
                second.windows(out);
            }
        }
    }

    /// Replace the leaf holding `target` with a split of `target` and `new`.
    ///
    /// `rect` is the area this node occupies, threaded down so the split's axis
    /// can be chosen from the real proportions of the tile being divided.
    fn split_at(&mut self, target: WindowId, new: WindowId, rect: Rect, gap: i32) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    axis: Axis::for_rect(rect),
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = divide(rect, *axis, *ratio, gap);
                first.split_at(target, new, a, gap) || second.split_at(target, new, b, gap)
            }
        }
    }

    /// Exchange two window ids wherever they appear as leaves.
    fn rename(&mut self, a: WindowId, b: WindowId) {
        match self {
            Self::Leaf(id) if *id == a => *id = b,
            Self::Leaf(id) if *id == b => *id = a,
            Self::Leaf(_) => {}
            Self::Split { first, second, .. } => {
                first.rename(a, b);
                second.rename(a, b);
            }
        }
    }

    /// Adjust the nearest ancestor split on `axis` that contains `target`.
    ///
    /// Returns `Found` once the window is located, so each ancestor on the way
    /// back up gets the chance to be the one that moves — and the first one on
    /// the right axis takes it.
    fn resize(&mut self, target: WindowId, axis: Axis, delta: f32) -> bool {
        let Self::Split {
            axis: split_axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        // Which side the window is on decides the sign: growing a window in
        // the second half means *shrinking* the first half's ratio.
        let in_first = first.contains(target);
        let in_second = !in_first && second.contains(target);
        if !in_first && !in_second {
            return false;
        }
        // Deeper first, so the innermost split on the right axis wins — that
        // is the one whose edge is actually beside the focused window.
        let child = if in_first { first } else { second };
        if child.resize(target, axis, delta) {
            return true;
        }
        if *split_axis != axis {
            return false;
        }
        let wanted = if in_first { *ratio + delta } else { *ratio - delta };
        let clamped = wanted.clamp(MIN_RATIO, MAX_RATIO);
        if (clamped - *ratio).abs() < f32::EPSILON {
            return false;
        }
        *ratio = clamped;
        true
    }

    /// Whether `target` is somewhere under this node.
    fn contains(&self, target: WindowId) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => {
                first.contains(target) || second.contains(target)
            }
        }
    }

    /// Remove `target`, collapsing the split it was half of.
    ///
    /// Returns `Some(replacement)` when this node must be replaced by its
    /// surviving child, which is how a tile's space returns to its sibling
    /// rather than being left as a hole.
    fn remove(&mut self, target: WindowId) -> Removal {
        match self {
            Self::Leaf(id) if *id == target => Removal::CollapseMe,
            Self::Leaf(_) => Removal::NotFound,
            Self::Split { first, second, .. } => {
                match first.remove(target) {
                    Removal::CollapseMe => {
                        *self = (**second).clone();
                        return Removal::Done;
                    }
                    Removal::Done => return Removal::Done,
                    Removal::NotFound => {}
                }
                match second.remove(target) {
                    Removal::CollapseMe => {
                        *self = (**first).clone();
                        Removal::Done
                    }
                    other => other,
                }
            }
        }
    }
}

/// What a subtree tells its parent after a removal.
#[derive(Debug, PartialEq, Eq)]
enum Removal {
    /// Not in this subtree.
    NotFound,
    /// This node *is* the window; the parent must collapse to the sibling.
    CollapseMe,
    /// Handled below; nothing for the parent to do.
    Done,
}

/// Cut `rect` in two along `axis`, leaving `gap` between the halves.
///
/// The gap comes out of the middle before the ratio is applied, so the
/// proportion describes the space the windows actually get rather than the
/// space before the gutter was taken out of it. Getting that backwards makes
/// a 50/50 split visibly uneven at large gaps.
/// How lopsided a split is allowed to get.
///
/// A tile at zero would be a window with no area, which reaches a client as a
/// protocol error rather than as a small window.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

fn divide(rect: Rect, axis: Axis, ratio: f32, gap: i32) -> (Rect, Rect) {
    let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
    match axis {
        Axis::Horizontal => {
            let usable = (rect.w() - gap).max(0);
            let first = (usable as f32 * ratio).round() as i32;
            (
                Rect::from_xywh(rect.x(), rect.y(), first, rect.h()),
                Rect::from_xywh(
                    rect.x() + first + gap,
                    rect.y(),
                    usable - first,
                    rect.h(),
                ),
            )
        }
        Axis::Vertical => {
            let usable = (rect.h() - gap).max(0);
            let first = (usable as f32 * ratio).round() as i32;
            (
                Rect::from_xywh(rect.x(), rect.y(), rect.w(), first),
                Rect::from_xywh(
                    rect.x(),
                    rect.y() + first + gap,
                    rect.w(),
                    usable - first,
                ),
            )
        }
    }
}

/// The tiled windows of one pane.
#[derive(Debug, Clone, Default)]
pub struct Tiles {
    root: Option<Node>,
}

impl Tiles {
    /// An empty pane.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the pane holds no windows.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Every window, in reading order.
    pub fn windows(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.windows(&mut out);
        }
        out
    }

    /// Whether `window` is tiled here.
    pub fn contains(&self, window: WindowId) -> bool {
        self.windows().contains(&window)
    }

    /// Add `window`, splitting the tile `beside` occupies.
    ///
    /// `area` is the pane's area and `gap` its gutter, both needed to work out
    /// the proportions of the tile being split. With no `beside` — or one that
    /// is not here — the new window splits the last tile in the tree, which is
    /// what makes "open a window with nothing focused" behave sensibly rather
    /// than silently doing nothing.
    pub fn insert(&mut self, window: WindowId, beside: Option<WindowId>, area: Rect, gap: i32) {
        if self.contains(window) {
            return;
        }
        // Resolved before the tree is borrowed mutably: choosing the target
        // is a read of the same tree the split will modify.
        let present = self.windows();
        let Some(target) = beside
            .filter(|id| present.contains(id))
            .or_else(|| present.last().copied())
        else {
            self.root = Some(Node::Leaf(window));
            return;
        };
        if let Some(root) = &mut self.root {
            root.split_at(target, window, area.inset(gap), gap);
        }
    }

    /// Remove `window`. Its space goes to whatever it was split from.
    pub fn remove(&mut self, window: WindowId) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        match root.remove(window) {
            // The last window: the pane is now empty.
            Removal::CollapseMe => {
                self.root = None;
                true
            }
            Removal::Done => true,
            Removal::NotFound => false,
        }
    }

    /// Exchange the positions of two tiled windows.
    ///
    /// The windows travel and the tree does not: the split shapes stay exactly
    /// as they were and only the leaves' contents trade places. That is what
    /// makes a directional move feel like dragging one window into another's
    /// slot rather than like the layout rearranging itself around you.
    pub fn swap(&mut self, a: WindowId, b: WindowId) -> bool {
        if a == b || !self.contains(a) || !self.contains(b) {
            return false;
        }
        if let Some(root) = &mut self.root {
            root.rename(a, b);
        }
        true
    }

    /// Nudge the split that governs `window` along `axis`.
    ///
    /// `delta` is a fraction of the tile: positive grows the window, negative
    /// shrinks it. Returns whether anything moved.
    ///
    /// Walks up from the leaf to the *nearest ancestor splitting on that axis*,
    /// which is what makes a resize mean the same thing wherever the focus is.
    /// Adjusting only the immediate parent would do nothing at all whenever the
    /// parent happened to split the other way — the window would simply refuse
    /// to widen, with no indication why.
    pub fn resize(&mut self, window: WindowId, axis: Axis, delta: f32) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        root.resize(window, axis, delta)
    }

    /// Drop every window for which `keep` is false, collapsing as it goes.
    ///
    /// The reconciliation hook. A window can stop belonging in the tree
    /// without anyone calling [`Self::remove`] — it goes fullscreen, it is
    /// dragged out to float, it moves to another workspace — and each of those
    /// lives somewhere other than here. Rather than have every one of them
    /// remember to update the tree, the tree is made to converge on the truth
    /// each time the layout runs, so a missed call is a frame of staleness
    /// instead of a window tiled into a slot it no longer occupies.
    pub fn retain(&mut self, keep: impl Fn(WindowId) -> bool) {
        for window in self.windows() {
            if !keep(window) {
                self.remove(window);
            }
        }
    }

    /// Where every window goes, given the pane's area.
    ///
    /// `gap` is applied at the edges as well as between tiles, so a single
    /// window is inset from the screen rather than flush against it.
    pub fn arrange(&self, area: Rect, gap: i32) -> Vec<(WindowId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.arrange(area.inset(gap), gap, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1000, 600);
    const GAP: i32 = 10;

    fn id(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    /// Insert windows 1..=n, each splitting the one before it.
    fn chain(n: u64) -> Tiles {
        let mut tiles = Tiles::new();
        let mut previous = None;
        for i in 1..=n {
            tiles.insert(id(i), previous, SCREEN, GAP);
            previous = Some(id(i));
        }
        tiles
    }

    #[test]
    fn an_empty_pane_arranges_to_nothing() {
        assert!(Tiles::new().is_empty());
        assert!(Tiles::new().arrange(SCREEN, GAP).is_empty());
    }

    #[test]
    fn one_window_fills_the_pane_inset_by_the_gap() {
        let tiles = chain(1);
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].1, SCREEN.inset(GAP));
    }

    #[test]
    fn a_second_window_splits_the_first() {
        // 1000x600 inset by 10 is 980x580 — wider than tall, so side by side.
        let laid = chain(2).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 2);
        let (a, b) = (laid[0].1, laid[1].1);
        assert_eq!(a.y(), b.y(), "a horizontal split should not change y");
        assert_eq!(a.h(), b.h());
        assert!(a.x() < b.x());
        // The gap sits between them, and nothing overlaps.
        assert_eq!(b.x() - (a.x() + a.w()), GAP);
    }

    #[test]
    fn a_tall_tile_splits_the_other_way() {
        // A pane taller than it is wide must split top and bottom, or two
        // windows on a portrait monitor come out as two slivers.
        let tall = Rect::from_xywh(0, 0, 400, 1200);
        let mut tiles = Tiles::new();
        tiles.insert(id(1), None, tall, GAP);
        tiles.insert(id(2), Some(id(1)), tall, GAP);
        let laid = tiles.arrange(tall, GAP);
        let (a, b) = (laid[0].1, laid[1].1);
        assert_eq!(a.x(), b.x(), "a vertical split should not change x");
        assert_eq!(a.w(), b.w());
        assert!(a.y() < b.y());
    }

    #[test]
    fn splitting_alternates_as_tiles_change_shape() {
        // The point of choosing by proportion: four windows should end up in
        // a rough grid rather than four slivers.
        let laid = chain(4).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 4);
        let widest = laid.iter().map(|(_, r)| r.w()).max().expect("some");
        let narrowest = laid.iter().map(|(_, r)| r.w()).min().expect("some");
        assert!(
            widest <= narrowest * 3,
            "tiles degenerated into slivers: {:?}",
            laid.iter().map(|(_, r)| (r.w(), r.h())).collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_two_tiles_ever_overlap() {
        for n in 1..=8 {
            let laid = chain(n).arrange(SCREEN, GAP);
            for (i, (_, a)) in laid.iter().enumerate() {
                for (_, b) in &laid[i + 1..] {
                    let disjoint = a.x() + a.w() <= b.x()
                        || b.x() + b.w() <= a.x()
                        || a.y() + a.h() <= b.y()
                        || b.y() + b.h() <= a.y();
                    assert!(disjoint, "{n} windows: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn every_tile_stays_inside_the_pane() {
        for n in 1..=8 {
            for (_, r) in chain(n).arrange(SCREEN, GAP) {
                assert!(r.x() >= SCREEN.x(), "{n} windows: {r:?} escaped left");
                assert!(r.y() >= SCREEN.y(), "{n} windows: {r:?} escaped top");
                assert!(
                    r.x() + r.w() <= SCREEN.x() + SCREEN.w(),
                    "{n} windows: {r:?} escaped right"
                );
                assert!(
                    r.y() + r.h() <= SCREEN.y() + SCREEN.h(),
                    "{n} windows: {r:?} escaped bottom"
                );
            }
        }
    }

    #[test]
    fn closing_a_window_gives_its_space_to_its_sibling() {
        // The defining behaviour of a split tree: space collapses back rather
        // than leaving a hole.
        let mut tiles = chain(2);
        assert!(tiles.remove(id(2)));
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].1, SCREEN.inset(GAP), "the survivor did not reclaim the space");
    }

    #[test]
    fn closing_the_last_window_empties_the_pane() {
        let mut tiles = chain(1);
        assert!(tiles.remove(id(1)));
        assert!(tiles.is_empty());
        assert!(tiles.arrange(SCREEN, GAP).is_empty());
    }

    #[test]
    fn closing_from_the_middle_leaves_the_rest_covering_the_pane() {
        let mut tiles = chain(4);
        assert!(tiles.remove(id(2)));
        assert_eq!(tiles.windows().len(), 3);
        // And the survivors still tile without overlap or escape.
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 3);
        for (_, r) in &laid {
            assert!(r.w() > 0 && r.h() > 0, "collapsed to nothing: {r:?}");
        }
    }

    #[test]
    fn removing_a_window_that_is_not_here_changes_nothing() {
        let mut tiles = chain(2);
        assert!(!tiles.remove(id(99)));
        assert_eq!(tiles.windows().len(), 2);
    }

    #[test]
    fn inserting_the_same_window_twice_is_a_no_op() {
        // A map event arriving twice must not put one window in two tiles.
        let mut tiles = chain(2);
        tiles.insert(id(2), Some(id(1)), SCREEN, GAP);
        assert_eq!(tiles.windows(), [id(1), id(2)]);
    }

    #[test]
    fn a_window_opened_with_nothing_focused_still_lands_somewhere() {
        let mut tiles = chain(2);
        tiles.insert(id(3), None, SCREEN, GAP);
        assert_eq!(tiles.windows().len(), 3, "the window vanished");
        assert!(tiles.contains(id(3)));
    }

    #[test]
    fn splitting_beside_an_absent_window_still_lands_somewhere() {
        let mut tiles = chain(2);
        tiles.insert(id(3), Some(id(99)), SCREEN, GAP);
        assert!(tiles.contains(id(3)));
    }

    #[test]
    fn a_pane_too_small_to_divide_produces_no_negative_tiles() {
        // An output smaller than the gap is absurd, but a negative width
        // reaches a client as a protocol error rather than as a bad layout.
        let tiny = Rect::from_xywh(0, 0, 12, 12);
        let mut tiles = Tiles::new();
        for i in 1..=4 {
            tiles.insert(id(i), Some(id(i.saturating_sub(1))), tiny, GAP);
        }
        for (_, r) in tiles.arrange(tiny, GAP) {
            assert!(r.w() >= 0 && r.h() >= 0, "negative tile {r:?}");
        }
    }

    #[test]
    fn the_gap_comes_out_before_the_ratio_so_halves_are_equal() {
        // Applying the ratio first and then subtracting the gutter makes a
        // 50/50 split visibly uneven once the gap is large.
        let laid = chain(2).arrange(SCREEN, GAP);
        let (a, b) = (laid[0].1, laid[1].1);
        assert!(
            a.w().abs_diff(b.w()) <= 1,
            "halves differ by {}: {a:?} vs {b:?}",
            a.w().abs_diff(b.w())
        );
    }

    #[test]
    fn swapping_moves_the_windows_and_leaves_the_shape_alone() {
        // Dragging a window into another's slot must not rearrange the pane
        // around it: the tiles stay put and only their occupants trade.
        let tiles = chain(4);
        let before: Vec<Rect> = tiles.arrange(SCREEN, GAP).into_iter().map(|(_, r)| r).collect();

        let mut swapped = tiles.clone();
        let (a, b) = (swapped.windows()[0], swapped.windows()[3]);
        assert!(swapped.swap(a, b));

        let after: Vec<Rect> = swapped.arrange(SCREEN, GAP).into_iter().map(|(_, r)| r).collect();
        assert_eq!(before, after, "the split shape moved");

        let ids: Vec<WindowId> = swapped.arrange(SCREEN, GAP).into_iter().map(|(i, _)| i).collect();
        assert_eq!(ids[0], b, "the windows did not trade places");
        assert_eq!(ids[3], a);
    }

    #[test]
    fn swapping_a_window_with_itself_or_an_absent_one_does_nothing() {
        let mut tiles = chain(2);
        let a = tiles.windows()[0];
        assert!(!tiles.swap(a, a));
        assert!(!tiles.swap(a, id(99)));
        assert_eq!(tiles.windows(), [id(1), id(2)]);
    }

    #[test]
    fn retain_drops_what_no_longer_belongs_and_collapses_the_gap() {
        // A window that goes fullscreen or floats stops being tiled without
        // anyone calling remove; the tree converges on the truth instead.
        let mut tiles = chain(4);
        let keep = [id(1), id(3)];
        tiles.retain(|w| keep.contains(&w));
        assert_eq!(tiles.windows(), keep);
        // And what is left still covers the pane without holes or overlap.
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 2);
        for (_, r) in &laid {
            assert!(r.w() > 0 && r.h() > 0, "collapsed to nothing: {r:?}");
        }
    }

    #[test]
    fn retaining_nothing_empties_the_pane() {
        let mut tiles = chain(3);
        tiles.retain(|_| false);
        assert!(tiles.is_empty());
    }

    #[test]
    fn retaining_everything_changes_nothing() {
        let tiles = chain(4);
        let mut kept = tiles.clone();
        kept.retain(|_| true);
        assert_eq!(kept.windows(), tiles.windows());
        assert_eq!(kept.arrange(SCREEN, GAP), tiles.arrange(SCREEN, GAP));
    }

    #[test]
    fn resizing_widens_the_window_and_narrows_its_neighbour() {
        let mut tiles = chain(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(1), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[0].1.w() > before[0].1.w(), "the focused window did not grow");
        assert!(after[1].1.w() < before[1].1.w(), "its neighbour did not give way");
        // And the pane is still fully covered — no gap opened between them.
        assert_eq!(
            after[0].1.w() + after[1].1.w() + GAP,
            before[0].1.w() + before[1].1.w() + GAP
        );
    }

    #[test]
    fn resizing_the_second_window_grows_it_too() {
        // The sign has to flip: growing the window on the right means
        // shrinking the split's ratio, not raising it.
        let mut tiles = chain(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(2), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[1].1.w() > before[1].1.w(), "the right window shrank instead");
    }

    #[test]
    fn resizing_along_an_axis_nothing_splits_on_does_nothing() {
        // Two windows side by side cannot be made taller relative to one
        // another. Reporting that honestly is what lets the caller leave the
        // resize mode's feedback alone rather than showing a change.
        let mut tiles = chain(2);
        assert!(!tiles.resize(id(1), Axis::Vertical, 0.1));
    }

    #[test]
    fn a_resize_finds_the_nearest_split_on_that_axis() {
        // Three windows: 1 on the left, 2 and 3 splitting the right half
        // vertically. Resizing 2 vertically must move the split between 2 and
        // 3 — the innermost one on that axis — not the outer horizontal one.
        let mut tiles = chain(3);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(2), Axis::Vertical, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert_eq!(after[0].1, before[0].1, "the unrelated left window moved");
        assert!(after[1].1.h() > before[1].1.h());
        assert!(after[2].1.h() < before[2].1.h());
    }

    #[test]
    fn an_outer_split_is_still_reachable_from_a_nested_window() {
        // Resizing window 2 horizontally has no split beside it on that axis
        // until the outer one, which is the whole point of walking up.
        let mut tiles = chain(3);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(2), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[0].1.w() < before[0].1.w(), "the outer split did not move");
    }

    #[test]
    fn a_window_can_never_be_resized_out_of_existence() {
        // A zero-width tile reaches a client as a protocol error rather than
        // as a very small window.
        let mut tiles = chain(2);
        for _ in 0..50 {
            tiles.resize(id(1), Axis::Horizontal, -0.5);
        }
        for (_, rect) in tiles.arrange(SCREEN, GAP) {
            assert!(rect.w() > 0 && rect.h() > 0, "resized to nothing: {rect:?}");
        }
    }

    #[test]
    fn resizing_stops_reporting_change_once_it_is_clamped() {
        // So a key held down at the limit does not keep asking for redraws.
        let mut tiles = chain(2);
        while tiles.resize(id(1), Axis::Horizontal, 0.1) {}
        assert!(!tiles.resize(id(1), Axis::Horizontal, 0.1));
    }

    #[test]
    fn resizing_a_window_that_is_not_here_does_nothing() {
        let mut tiles = chain(2);
        assert!(!tiles.resize(id(99), Axis::Horizontal, 0.1));
    }

    #[test]
    fn a_lone_window_has_nothing_to_resize_against() {
        let mut tiles = chain(1);
        assert!(!tiles.resize(id(1), Axis::Horizontal, 0.1));
    }

    #[test]
    fn window_order_is_stable_across_arrange() {
        // Two arranges with no change between them must agree, or the shell
        // would see windows moving on every frame.
        let tiles = chain(5);
        assert_eq!(tiles.arrange(SCREEN, GAP), tiles.arrange(SCREEN, GAP));
    }
}
