//! Auto-tiling within one pane: a fixed family of grid layouts.
//!
//! How many tiled windows there are decides the shape. One window fills the
//! pane. Two sit side by side. Three put two equal tiles on top and give the
//! third the whole width beneath them, as tall as the two above. Four are the
//! four quadrants, and past four the later quadrants subdivide in the same
//! way, so eight windows are the quadrants each split in half.
//!
//! The whole family has a transpose, where two windows stack instead of
//! sitting side by side and the third goes down the right-hand side instead of
//! across the bottom. Which one a pane uses is its [`Orientation`]: the
//! screen's own shape by default — a portrait screen transposes, or two
//! windows there would be two slivers — or named outright, because which shape
//! suits the work is not something the aspect ratio knows.
//!
//! The shapes are held as a binary tree of splits rather than as a list of
//! rectangles, because a divider the user can move is a *shared edge*: the
//! line between the top two tiles and the wide one below is a single split
//! ratio, so dragging the bottom of tile A drags the bottom of tile B and the
//! top of tile C with it, by construction rather than by bookkeeping.
//!
//! The tree is rebuilt to the canonical shape whenever the set of windows
//! changes, and left alone otherwise. Ratios are carried across the rebuild
//! wherever the old and new shapes agree, so a divider you moved stays moved
//! for as long as that divider exists — and a layout that loses a window
//! renormalises to the shape its new count calls for instead of keeping the
//! hole-shaped tree a collapse would leave.

use crate::edge_gap;
use crate::geometry::Rect;
use crate::window::WindowId;

/// Which way a split divides its tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Two tiles side by side.
    Horizontal,
    /// Two tiles stacked.
    Vertical,
}

impl Axis {
    /// The other axis.
    const fn crossed(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

/// Which way round the whole family of shapes is turned.
///
/// Every shape in [`grid`] is built from one axis — the one that puts two
/// windows *next to* each other — so naming that axis turns the family as a
/// whole: two windows side by side with the third across the bottom, or two
/// windows stacked with the third down the right-hand side. There is no third
/// possibility, because there is no third way to cut a rectangle in two.
///
/// Held per pane rather than derived from the screen alone, because which one
/// suits the work is not something the aspect ratio knows: an editor over a
/// terminal is the right shape on the same wide monitor where two browsers
/// side by side are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// Follow the screen: side by side on a landscape one, stacked on a
    /// portrait one, where side by side would be two slivers.
    #[default]
    Auto,
    /// Two windows side by side; the third across the bottom.
    Columns,
    /// Two windows stacked; the third down the right-hand side.
    Rows,
}

impl Orientation {
    /// The axis that puts two tiles next to each other, in a pane of `area`.
    const fn beside(self, area: Rect) -> Axis {
        match self {
            Self::Columns => Axis::Horizontal,
            Self::Rows => Axis::Vertical,
            Self::Auto if area.h() > area.w() => Axis::Vertical,
            Self::Auto => Axis::Horizontal,
        }
    }

    /// Which of the two named orientations this one is, in a pane of `area`.
    /// [`Self::Auto`] is whichever the screen's shape asks for.
    pub const fn settled(self, area: Rect) -> Self {
        match self.beside(area) {
            Axis::Horizontal => Self::Columns,
            Axis::Vertical => Self::Rows,
        }
    }

    /// The other one.
    ///
    /// [`Self::Auto`] is settled against `area` first, so the first flip goes
    /// *away* from the shape that is on screen. Flipping to an explicit value
    /// that happened to match what `Auto` was already doing would look like
    /// the key had failed.
    const fn flipped(self, area: Rect) -> Self {
        match self.settled(area) {
            Self::Columns => Self::Rows,
            _ => Self::Columns,
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
        let wanted = if in_first {
            *ratio + delta
        } else {
            *ratio - delta
        };
        let clamped = wanted.clamp(MIN_RATIO, MAX_RATIO);
        if (clamped - *ratio).abs() < f32::EPSILON {
            return false;
        }
        *ratio = clamped;
        true
    }

    /// Adjust the innermost split containing `target`, whichever way it cuts.
    ///
    /// The one-dimensional counterpart to [`Self::resize`], for a wheel: there
    /// is no axis to name, so the divider that moves is simply the one nearest
    /// the window — the edge of its own tile. In the two-window shapes that is
    /// the only divider there is; in the quadrants it is the one between the
    /// window and the tile it was split from, so a notch grows the tile the
    /// window is actually sitting in.
    ///
    /// Returns `Some(moved)` once that divider has been found, `None` when
    /// `target` is not under this node. `Some(false)` — found but already at
    /// its limit — deliberately does *not* fall through to the next divider
    /// out: a tile pinned at [`MAX_RATIO`] would otherwise start growing along
    /// the other axis instead, and a wheel that changes what it means halfway
    /// through a turn is a wheel nobody can aim.
    fn grow(&mut self, target: WindowId, delta: f32) -> Option<bool> {
        let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return None;
        };
        let in_first = first.contains(target);
        if !in_first && !second.contains(target) {
            return None;
        }
        // Deeper first: the innermost split is the one whose edge the window
        // touches, and it is the parent of every other candidate.
        let child = if in_first { first } else { second };
        if let Some(moved) = child.grow(target, delta) {
            return Some(moved);
        }
        // As in `resize`: growing a window in the second half means shrinking
        // the first half's share.
        let wanted = if in_first {
            *ratio + delta
        } else {
            *ratio - delta
        };
        let clamped = wanted.clamp(MIN_RATIO, MAX_RATIO);
        let moved = (clamped - *ratio).abs() >= f32::EPSILON;
        *ratio = clamped;
        Some(moved)
    }

    /// Whether `target` is somewhere under this node.
    fn contains(&self, target: WindowId) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    /// Remove `target`, collapsing the split it was half of.
    ///
    /// Returns `Some(replacement)` when this node must be replaced by its
    /// surviving child, which is how a tile's space returns to its sibling
    /// rather than being left as a hole. The shape this leaves is not the
    /// canonical one for the new count; the next [`Tiles::reconcile`]
    /// renormalises it, so the collapse is only ever what a frame between the
    /// two shows.
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

/// The canonical tree for `windows`, in order, shaped by how many there are.
///
/// `beside` is the axis that puts two tiles side by side — horizontal on a
/// landscape screen, transposed on a portrait one so the same shapes read
/// down the long edge instead of across it.
fn grid(windows: &[WindowId], beside: Axis) -> Node {
    let split = |axis: Axis, first: Node, second: Node| Node::Split {
        axis,
        ratio: 0.5,
        first: Box::new(first),
        second: Box::new(second),
    };
    match windows {
        [] => unreachable!("the canonical grid is never asked for zero windows"),
        [only] => Node::Leaf(*only),
        [a, b] => split(beside, Node::Leaf(*a), Node::Leaf(*b)),
        // Two equal tiles on top, the third across the bottom at the same
        // height: the top pair and the wide tile share one split, which is
        // what makes resizing either top tile's bottom edge move the wide
        // tile's top edge with it.
        [a, b, c] => split(
            beside.crossed(),
            split(beside, Node::Leaf(*a), Node::Leaf(*b)),
            Node::Leaf(*c),
        ),
        _ => {
            // Quadrants. Windows past a multiple of four subdivide the later
            // quadrants first, so the panes already on screen keep their
            // places while the newest corner splits.
            let n = windows.len();
            let (base, extra) = (n / 4, n % 4);
            let mut bounds = [0usize; 5];
            for q in 0..4 {
                bounds[q + 1] = bounds[q] + base + usize::from(q >= 4 - extra);
            }
            let quadrant = |q: usize| grid(&windows[bounds[q]..bounds[q + 1]], beside);
            split(
                beside.crossed(),
                split(beside, quadrant(0), quadrant(1)),
                split(beside, quadrant(2), quadrant(3)),
            )
        }
    }
}

/// Carry split ratios from `old` onto `new` wherever the two shapes agree.
///
/// Positional, from the root down, stopping at the first disagreement on each
/// path: a ratio names an edge of the layout, not a window, so the edge
/// between the top row and the bottom keeps its place when a fourth window
/// splits the bottom tile — that edge is still the same edge.
fn adopt_ratios(new: &mut Node, old: &Node) {
    if let (
        Node::Split {
            axis: new_axis,
            ratio: new_ratio,
            first: new_first,
            second: new_second,
        },
        Node::Split {
            axis: old_axis,
            ratio: old_ratio,
            first: old_first,
            second: old_second,
        },
    ) = (new, old)
        && new_axis == old_axis
    {
        *new_ratio = *old_ratio;
        adopt_ratios(new_first, old_first);
        adopt_ratios(new_second, old_second);
    }
}

/// How lopsided a split is allowed to get.
///
/// A tile at zero would be a window with no area, which reaches a client as a
/// protocol error rather than as a small window.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

/// Cut `rect` in two along `axis`, leaving `gap` between the halves.
///
/// The gap comes out of the middle before the ratio is applied, so the
/// proportion describes the space the windows actually get rather than the
/// space before the gutter was taken out of it. Getting that backwards makes
/// a 50/50 split visibly uneven at large gaps.
fn divide(rect: Rect, axis: Axis, ratio: f32, gap: i32) -> (Rect, Rect) {
    let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
    match axis {
        Axis::Horizontal => {
            let usable = (rect.w() - gap).max(0);
            let first = (usable as f32 * ratio).round() as i32;
            (
                Rect::from_xywh(rect.x(), rect.y(), first, rect.h()),
                Rect::from_xywh(rect.x() + first + gap, rect.y(), usable - first, rect.h()),
            )
        }
        Axis::Vertical => {
            let usable = (rect.h() - gap).max(0);
            let first = (usable as f32 * ratio).round() as i32;
            (
                Rect::from_xywh(rect.x(), rect.y(), rect.w(), first),
                Rect::from_xywh(rect.x(), rect.y() + first + gap, rect.w(), usable - first),
            )
        }
    }
}

/// The tiled windows of one pane.
#[derive(Debug, Clone, Default)]
pub struct Tiles {
    root: Option<Node>,
    /// Which way the shapes are turned. See [`Orientation`]; applied by
    /// [`Self::reconcile`], which is the only thing that builds the tree.
    orientation: Orientation,
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

    /// Bring the tree into line with `tiled`, the windows that belong in it.
    ///
    /// The tree is rebuilt to the canonical shape for the new count. Windows
    /// already here keep their on-screen order and newcomers join at the end,
    /// which is what puts a third window in the wide bottom tile rather than
    /// wherever focus happened to be sitting. Ratios carry over through
    /// [`adopt_ratios`], so with nothing added or removed this is exactly a
    /// no-op and a moved divider survives the windows around it changing.
    ///
    /// `area` is the pane's area — not to size anything here, but to settle
    /// the [`Orientation`]: which way the family is turned, and for
    /// [`Orientation::Auto`] that is the screen's shape.
    pub fn reconcile(&mut self, tiled: &[WindowId], area: Rect) {
        let mut order: Vec<WindowId> = self
            .windows()
            .into_iter()
            .filter(|w| tiled.contains(w))
            .collect();
        let arrivals: Vec<WindowId> = tiled
            .iter()
            .copied()
            .filter(|w| !order.contains(w))
            .collect();
        order.extend(arrivals);
        if order.is_empty() {
            self.root = None;
            return;
        }
        let mut fresh = grid(&order, self.orientation.beside(area));
        if let Some(old) = &self.root {
            adopt_ratios(&mut fresh, old);
        }
        self.root = Some(fresh);
    }

    /// Remove `window`. Its space goes to whatever it was split from, until
    /// the next [`Self::reconcile`] renormalises the shape.
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
    /// to widen, with no indication why. It is also what links the edges the
    /// shapes share: in the three-window layout the top pair and the wide tile
    /// meet at one split, so resizing any of the three vertically moves that
    /// shared edge and all three tiles follow.
    pub fn resize(&mut self, window: WindowId, axis: Axis, delta: f32) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        root.resize(window, axis, delta)
    }

    /// Which way this pane's shapes are turned.
    pub const fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Turn the shapes. Takes effect at the next [`Self::reconcile`], which is
    /// the only place the tree is built — so the caller relayouts afterwards
    /// and nothing here has to know the pane's size.
    pub fn set_orientation(&mut self, orientation: Orientation) {
        self.orientation = orientation;
    }

    /// Flip between the two orientations, and report the one now in force.
    ///
    /// `area` settles [`Orientation::Auto`] — see [`Orientation::flipped`].
    ///
    /// Moved dividers do not survive the flip, and should not: a ratio names
    /// an edge, [`adopt_ratios`] carries one only where the old and new trees
    /// split the same way, and after a turn they agree nowhere. The 70/30 you
    /// set between two columns is not a wish about the two rows they become.
    pub fn toggle_orientation(&mut self, area: Rect) -> Orientation {
        self.orientation = self.orientation.flipped(area);
        self.orientation
    }

    /// Grow or shrink `window` against the divider nearest it.
    ///
    /// `delta` is a fraction of the tile that divider splits: positive grows
    /// the window, negative shrinks it. Returns whether anything moved.
    ///
    /// The axis-free form of [`Self::resize`], for a wheel or any other
    /// one-dimensional input. Which edge moves is decided by where the window
    /// sits rather than by a direction the caller names, so one gesture means
    /// "more room" whichever way the pane happens to be turned — which is the
    /// whole point of it when [`Self::toggle_orientation`] can turn the pane
    /// under it. See [`Node::grow`] for why a divider already at its limit
    /// stops there rather than handing the gesture to the next one out.
    pub fn grow(&mut self, window: WindowId, delta: f32) -> bool {
        self.root
            .as_mut()
            .and_then(|root| root.grow(window, delta))
            .unwrap_or(false)
    }

    /// Where every window goes, given the pane's area.
    ///
    /// The edges are inset too, so a single window floats off the screen edge
    /// rather than sitting flush against it — but by [`edge_gap`] rather than
    /// by the whole `gap` that falls between two tiles.
    pub fn arrange(&self, area: Rect, gap: i32) -> Vec<(WindowId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.arrange(area.inset(edge_gap(gap)), gap, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1000, 600);
    const GAP: i32 = 10;
    /// What the grid leaves between a tile and the screen edge: half of what
    /// it leaves between two tiles. See [`crate::edge_gap`].
    const EDGE: i32 = crate::edge_gap(GAP);

    fn id(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    fn ids(n: u64) -> Vec<WindowId> {
        (1..=n).map(id).collect()
    }

    /// Windows 1..=n reconciled into the canonical shape for the screen.
    fn tiled(n: u64) -> Tiles {
        let mut tiles = Tiles::new();
        tiles.reconcile(&ids(n), SCREEN);
        tiles
    }

    #[test]
    fn an_empty_pane_arranges_to_nothing() {
        assert!(Tiles::new().is_empty());
        assert!(Tiles::new().arrange(SCREEN, GAP).is_empty());
    }

    #[test]
    fn one_window_fills_the_pane_inset_by_the_edge_share_of_the_gap() {
        let tiles = tiled(1);
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].1, SCREEN.inset(EDGE));
    }

    #[test]
    fn two_windows_sit_side_by_side_in_equal_halves() {
        let laid = tiled(2).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 2);
        let (a, b) = (laid[0].1, laid[1].1);
        assert_eq!(a.y(), b.y(), "a side-by-side split should not change y");
        assert_eq!(a.h(), b.h());
        assert!(a.x() < b.x());
        assert!(a.w().abs_diff(b.w()) <= 1, "halves are equal");
        // The gap sits between them, and nothing overlaps.
        assert_eq!(b.x() - (a.x() + a.w()), GAP);
    }

    #[test]
    fn a_portrait_screen_transposes_the_shapes() {
        // Side by side on a portrait monitor would be two slivers, so the
        // whole family turns: two windows stack top and bottom there.
        let tall = Rect::from_xywh(0, 0, 400, 1200);
        let mut tiles = Tiles::new();
        tiles.reconcile(&ids(2), tall);
        let laid = tiles.arrange(tall, GAP);
        let (a, b) = (laid[0].1, laid[1].1);
        assert_eq!(a.x(), b.x(), "a stacked split should not change x");
        assert_eq!(a.w(), b.w());
        assert!(a.y() < b.y());
    }

    #[test]
    fn three_windows_are_two_tiles_over_one_wide_one() {
        // The A/B/C shape: two equal tiles on top, the third across the
        // bottom — as wide as both together and exactly as tall as they are.
        let laid = tiled(3).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 3);
        let (a, b, c) = (laid[0].1, laid[1].1, laid[2].1);
        assert_eq!(a.y(), b.y());
        assert!(a.x() < b.x());
        assert!(a.w().abs_diff(b.w()) <= 1, "a and b are the same width");
        assert_eq!(a.h(), b.h());
        assert!(c.y() > a.y() + a.h(), "c sits beneath the pair");
        assert_eq!(c.w(), a.w() + GAP + b.w(), "c spans both columns");
        assert!(a.h().abs_diff(c.h()) <= 1, "the two rows are equal heights");
    }

    #[test]
    fn four_windows_are_the_four_quadrants() {
        let laid = tiled(4).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 4);
        let (a, b, c, d) = (laid[0].1, laid[1].1, laid[2].1, laid[3].1);
        for (top, bottom) in [(a, c), (b, d)] {
            assert_eq!(top.x(), bottom.x(), "the columns line up");
            assert_eq!(top.w(), bottom.w());
        }
        for (left, right) in [(a, b), (c, d)] {
            assert_eq!(left.y(), right.y(), "the rows line up");
            assert_eq!(left.h(), right.h());
        }
        let sizes: Vec<_> = laid.iter().map(|(_, r)| (r.w(), r.h())).collect();
        assert!(
            sizes.iter().all(|s| *s == sizes[0]),
            "quadrants are equal: {sizes:?}"
        );
    }

    #[test]
    fn a_fifth_window_splits_the_last_quadrant() {
        // Nesting continues corner by corner from the end, so the panes
        // already on screen keep their places.
        let laid = tiled(5).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 5);
        let whole: Vec<Rect> = laid.iter().map(|(_, r)| *r).collect();
        let quadrant = tiled(4).arrange(SCREEN, GAP)[0].1;
        let full: Vec<&Rect> = whole
            .iter()
            .filter(|r| (r.w(), r.h()) == (quadrant.w(), quadrant.h()))
            .collect();
        assert_eq!(full.len(), 3, "three quadrants are untouched: {whole:?}");
        // And the first three windows have not moved.
        let four = tiled(4).arrange(SCREEN, GAP);
        for i in 0..3 {
            assert_eq!(laid[i], four[i], "window {i} moved to make room");
        }
    }

    #[test]
    fn eight_windows_split_every_quadrant() {
        let laid = tiled(8).arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 8);
        let heights: Vec<i32> = laid.iter().map(|(_, r)| r.h()).collect();
        assert!(
            heights.iter().all(|h| *h == heights[0]),
            "two even rows: {heights:?}"
        );
        let widest = laid.iter().map(|(_, r)| r.w()).max().expect("some");
        let narrowest = laid.iter().map(|(_, r)| r.w()).min().expect("some");
        assert!(widest - narrowest <= 1, "eight equal tiles");
    }

    #[test]
    fn no_two_tiles_ever_overlap() {
        for n in 1..=9 {
            let laid = tiled(n).arrange(SCREEN, GAP);
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
        for n in 1..=9 {
            for (_, r) in tiled(n).arrange(SCREEN, GAP) {
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
    fn losing_a_window_renormalises_to_the_smaller_shape() {
        // Not a collapse to whatever the tree happened to look like: three
        // windows are the A/B/C shape whether they got there by opening or by
        // a fourth one closing.
        let mut tiles = tiled(4);
        tiles.reconcile(&ids(3), SCREEN);
        assert_eq!(tiles.windows(), ids(3));
        assert_eq!(tiles.arrange(SCREEN, GAP), tiled(3).arrange(SCREEN, GAP));
    }

    #[test]
    fn closing_the_last_window_empties_the_pane() {
        let mut tiles = tiled(1);
        assert!(tiles.remove(id(1)));
        assert!(tiles.is_empty());
        assert!(tiles.arrange(SCREEN, GAP).is_empty());
    }

    #[test]
    fn removing_a_window_that_is_not_here_changes_nothing() {
        let mut tiles = tiled(2);
        assert!(!tiles.remove(id(99)));
        assert_eq!(tiles.windows().len(), 2);
    }

    #[test]
    fn remove_gives_the_space_back_until_the_next_reconcile() {
        let mut tiles = tiled(2);
        assert!(tiles.remove(id(2)));
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(laid.len(), 1);
        assert_eq!(laid[0].1, SCREEN.inset(EDGE), "the survivor reclaims it");
    }

    #[test]
    fn reconciling_the_same_set_changes_nothing() {
        // Including a moved divider: a rebuild that reset ratios on every
        // arrange would snap a resize back the moment anything else changed.
        let mut tiles = tiled(3);
        assert!(tiles.resize(id(1), Axis::Vertical, 0.15));
        let before = tiles.arrange(SCREEN, GAP);
        tiles.reconcile(&ids(3), SCREEN);
        assert_eq!(tiles.arrange(SCREEN, GAP), before);
    }

    #[test]
    fn a_survivors_swap_outlives_a_newcomer() {
        // Order in the rebuilt tree is the on-screen order, not the order the
        // windows were opened in, so a pane you moved stays where you put it.
        let mut tiles = tiled(3);
        assert!(tiles.swap(id(1), id(3)));
        tiles.reconcile(&ids(4), SCREEN);
        assert_eq!(tiles.windows(), [id(3), id(2), id(1), id(4)]);
    }

    #[test]
    fn a_moved_edge_survives_the_window_count_changing() {
        // The edge between the rows exists in both the three- and the
        // four-window shape, so the ratio you dragged it to carries across.
        let mut tiles = tiled(3);
        assert!(tiles.resize(id(3), Axis::Vertical, 0.2));
        let c_height = tiles.arrange(SCREEN, GAP)[2].1.h();

        tiles.reconcile(&ids(4), SCREEN);
        let laid = tiles.arrange(SCREEN, GAP);
        assert_eq!(
            laid[2].1.h(),
            c_height,
            "the bottom row kept its dragged height"
        );
    }

    #[test]
    fn swapping_moves_the_windows_and_leaves_the_shape_alone() {
        // Dragging a window into another's slot must not rearrange the pane
        // around it: the tiles stay put and only their occupants trade.
        let tiles = tiled(4);
        let before: Vec<Rect> = tiles
            .arrange(SCREEN, GAP)
            .into_iter()
            .map(|(_, r)| r)
            .collect();

        let mut swapped = tiles.clone();
        let (a, b) = (swapped.windows()[0], swapped.windows()[3]);
        assert!(swapped.swap(a, b));

        let after: Vec<Rect> = swapped
            .arrange(SCREEN, GAP)
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        assert_eq!(before, after, "the split shape moved");

        let ids: Vec<WindowId> = swapped
            .arrange(SCREEN, GAP)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        assert_eq!(ids[0], b, "the windows did not trade places");
        assert_eq!(ids[3], a);
    }

    #[test]
    fn swapping_a_window_with_itself_or_an_absent_one_does_nothing() {
        let mut tiles = tiled(2);
        let a = tiles.windows()[0];
        assert!(!tiles.swap(a, a));
        assert!(!tiles.swap(a, id(99)));
        assert_eq!(tiles.windows(), [id(1), id(2)]);
    }

    #[test]
    fn a_pane_too_small_to_divide_produces_no_negative_tiles() {
        // An output smaller than the gap is absurd, but a negative width
        // reaches a client as a protocol error rather than as a bad layout.
        let tiny = Rect::from_xywh(0, 0, 12, 12);
        let mut tiles = Tiles::new();
        tiles.reconcile(&ids(4), tiny);
        for (_, r) in tiles.arrange(tiny, GAP) {
            assert!(r.w() >= 0 && r.h() >= 0, "negative tile {r:?}");
        }
    }

    #[test]
    fn the_gap_comes_out_before_the_ratio_so_halves_are_equal() {
        // Applying the ratio first and then subtracting the gutter makes a
        // 50/50 split visibly uneven once the gap is large.
        let laid = tiled(2).arrange(SCREEN, GAP);
        let (a, b) = (laid[0].1, laid[1].1);
        assert!(
            a.w().abs_diff(b.w()) <= 1,
            "halves differ by {}: {a:?} vs {b:?}",
            a.w().abs_diff(b.w())
        );
    }

    #[test]
    fn resizing_widens_the_window_and_narrows_its_neighbour() {
        let mut tiles = tiled(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(1), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(
            after[0].1.w() > before[0].1.w(),
            "the focused window did not grow"
        );
        assert!(
            after[1].1.w() < before[1].1.w(),
            "its neighbour did not give way"
        );
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
        let mut tiles = tiled(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(2), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(
            after[1].1.w() > before[1].1.w(),
            "the right window shrank instead"
        );
    }

    #[test]
    fn resizing_along_an_axis_nothing_splits_on_does_nothing() {
        // Two windows side by side cannot be made taller relative to one
        // another. Reporting that honestly is what lets the caller leave the
        // resize mode's feedback alone rather than showing a change.
        let mut tiles = tiled(2);
        assert!(!tiles.resize(id(1), Axis::Vertical, 0.1));
    }

    #[test]
    fn the_top_pair_resize_against_each_other_and_c_stays_put() {
        // Position decides what a resize means: widening A moves only the
        // divider between A and B, and the wide tile below is untouched.
        let mut tiles = tiled(3);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(1), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[0].1.w() > before[0].1.w());
        assert!(after[1].1.w() < before[1].1.w());
        assert_eq!(after[2].1, before[2].1, "c does not follow the divider");
    }

    #[test]
    fn the_rows_shared_edge_moves_all_three_tiles() {
        // A and B are the same height by construction, so making them taller
        // has to come out of C — one split governs the whole edge.
        let mut tiles = tiled(3);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(1), Axis::Vertical, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[0].1.h() > before[0].1.h(), "a grew");
        assert_eq!(after[0].1.h(), after[1].1.h(), "b grew with it");
        assert!(after[2].1.h() < before[2].1.h(), "c gave the space up");
    }

    #[test]
    fn resizing_c_takes_the_space_back_from_the_pair() {
        let mut tiles = tiled(3);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(3), Axis::Vertical, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[2].1.h() > before[2].1.h(), "c grew");
        assert!(after[0].1.h() < before[0].1.h());
        assert!(after[1].1.h() < before[1].1.h());
    }

    #[test]
    fn a_nested_resize_finds_the_innermost_split_on_that_axis() {
        // Five windows: the last quadrant is split in half. Resizing one of
        // that pair horizontally moves the divider between them, not the
        // divider between the columns.
        let mut tiles = tiled(5);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(4), Axis::Horizontal, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert_eq!(after[0].1, before[0].1, "the far quadrant moved");
        assert_eq!(after[2].1, before[2].1, "its neighbour column moved");
        assert!(after[3].1.w() > before[3].1.w());
        assert!(after[4].1.w() < before[4].1.w());
    }

    #[test]
    fn an_outer_split_is_still_reachable_from_a_nested_window() {
        // From inside the split quadrant, a vertical resize has no split on
        // that axis until the rows — walking up is what reaches it.
        let mut tiles = tiled(5);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.resize(id(4), Axis::Vertical, 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(
            after[3].1.h() > before[3].1.h(),
            "the row boundary did not move"
        );
        assert!(after[0].1.h() < before[0].1.h(), "the top row gave way");
    }

    #[test]
    fn a_window_can_never_be_resized_out_of_existence() {
        // A zero-width tile reaches a client as a protocol error rather than
        // as a very small window.
        let mut tiles = tiled(2);
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
        let mut tiles = tiled(2);
        while tiles.resize(id(1), Axis::Horizontal, 0.1) {}
        assert!(!tiles.resize(id(1), Axis::Horizontal, 0.1));
    }

    #[test]
    fn resizing_a_window_that_is_not_here_does_nothing() {
        let mut tiles = tiled(2);
        assert!(!tiles.resize(id(99), Axis::Horizontal, 0.1));
    }

    #[test]
    fn a_lone_window_has_nothing_to_resize_against() {
        let mut tiles = tiled(1);
        assert!(!tiles.resize(id(1), Axis::Horizontal, 0.1));
    }

    /// Windows 1..=n reconciled into `orientation` on the landscape screen.
    fn turned(n: u64, orientation: Orientation) -> Tiles {
        let mut tiles = Tiles::new();
        tiles.set_orientation(orientation);
        tiles.reconcile(&ids(n), SCREEN);
        tiles
    }

    #[test]
    fn the_stacked_orientation_puts_two_windows_one_above_the_other() {
        // The same screen the default lays out side by side: the orientation
        // decides the shape, not the aspect ratio, once it has been named.
        let laid = turned(2, Orientation::Rows).arrange(SCREEN, GAP);
        let (a, b) = (laid[0].1, laid[1].1);
        assert_eq!(a.x(), b.x(), "a stacked split should not change x");
        assert_eq!(a.w(), b.w());
        assert!(a.y() < b.y());
        assert!(a.h().abs_diff(b.h()) <= 1, "halves are equal");
        assert_eq!(b.y() - (a.y() + a.h()), GAP);
    }

    #[test]
    fn the_third_window_goes_down_the_side_when_stacked() {
        // The transpose of the A/B/C shape: two tiles in a column on the
        // left, the third down the whole right-hand side.
        let laid = turned(3, Orientation::Rows).arrange(SCREEN, GAP);
        let (a, b, c) = (laid[0].1, laid[1].1, laid[2].1);
        assert_eq!(a.x(), b.x(), "a and b share a column");
        assert!(a.y() < b.y());
        assert!(a.h().abs_diff(b.h()) <= 1);
        assert!(c.x() > a.x() + a.w(), "c sits to the right of the pair");
        assert_eq!(c.h(), a.h() + GAP + b.h(), "c spans both rows");
        assert!(a.w().abs_diff(c.w()) <= 1, "the two columns are equal");
    }

    #[test]
    fn toggling_turns_the_tiling_the_other_way_and_back() {
        let mut tiles = tiled(2);
        assert_eq!(tiles.orientation(), Orientation::Auto);
        assert_eq!(tiles.toggle_orientation(SCREEN), Orientation::Rows);
        tiles.reconcile(&ids(2), SCREEN);
        let stacked = tiles.arrange(SCREEN, GAP);
        assert_eq!(stacked[0].1.x(), stacked[1].1.x(), "stacked now");

        assert_eq!(tiles.toggle_orientation(SCREEN), Orientation::Columns);
        tiles.reconcile(&ids(2), SCREEN);
        let side_by_side = tiles.arrange(SCREEN, GAP);
        assert_eq!(
            side_by_side,
            tiled(2).arrange(SCREEN, GAP),
            "back as it was"
        );
    }

    #[test]
    fn the_first_toggle_flips_away_from_what_is_on_screen() {
        // `Auto` on a portrait screen is already stacked, so flipping it to
        // `Rows` would look like the key had done nothing at all.
        let tall = Rect::from_xywh(0, 0, 400, 1200);
        let mut tiles = Tiles::new();
        tiles.reconcile(&ids(2), tall);
        assert_eq!(tiles.toggle_orientation(tall), Orientation::Columns);
        tiles.reconcile(&ids(2), tall);
        let laid = tiles.arrange(tall, GAP);
        assert!(
            laid[0].1.x() < laid[1].1.x(),
            "side by side on the tall one"
        );
    }

    #[test]
    fn the_orientation_outlives_the_windows_it_was_set_for() {
        // It is a property of the pane, not of the shape that happened to be
        // on screen when it was chosen: closing down to one window and opening
        // back up to three must not quietly put the default back.
        let mut tiles = turned(3, Orientation::Rows);
        tiles.reconcile(&[id(1)], SCREEN);
        tiles.reconcile(&ids(3), SCREEN);
        assert_eq!(tiles.orientation(), Orientation::Rows);
        let laid = tiles.arrange(SCREEN, GAP);
        assert!(
            laid[2].1.x() > laid[0].1.x() + laid[0].1.w(),
            "still stacked"
        );
    }

    #[test]
    fn turning_the_pane_gives_up_the_dividers_that_were_moved() {
        // A ratio names an edge, and after a turn the old edges are gone: a
        // 70/30 between two columns is not a wish about the rows they become.
        let mut tiles = tiled(2);
        assert!(tiles.resize(id(1), Axis::Horizontal, 0.2));
        tiles.toggle_orientation(SCREEN);
        tiles.reconcile(&ids(2), SCREEN);
        let laid = tiles.arrange(SCREEN, GAP);
        assert!(
            laid[0].1.h().abs_diff(laid[1].1.h()) <= 1,
            "the rows started lopsided: {laid:?}"
        );
    }

    #[test]
    fn growing_widens_a_column_and_shrinking_narrows_it() {
        let mut tiles = tiled(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.grow(id(1), 0.1));
        let wider = tiles.arrange(SCREEN, GAP);
        assert!(wider[0].1.w() > before[0].1.w(), "it did not grow");
        assert!(wider[1].1.w() < before[1].1.w(), "its neighbour kept its w");
        assert!(tiles.grow(id(1), -0.1));
        assert_eq!(tiles.arrange(SCREEN, GAP), before, "and back again");
    }

    #[test]
    fn growing_a_row_makes_it_taller_rather_than_wider() {
        // The whole point of the axis-free form: one gesture means "more
        // room" whichever way the pane is turned.
        let mut tiles = turned(2, Orientation::Rows);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.grow(id(1), 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[0].1.h() > before[0].1.h(), "it did not grow");
        assert_eq!(after[0].1.w(), before[0].1.w(), "it grew sideways instead");
    }

    #[test]
    fn growing_the_second_window_grows_it_too() {
        // As in `resize`, the sign flips for the window on the far side of
        // the divider.
        let mut tiles = tiled(2);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.grow(id(2), 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[1].1.w() > before[1].1.w(), "the right window shrank");
    }

    #[test]
    fn growing_moves_the_divider_nearest_the_window() {
        // Five windows: the last quadrant is split in half, and that inner
        // divider is the edge of window four's own tile.
        let mut tiles = tiled(5);
        let before = tiles.arrange(SCREEN, GAP);
        assert!(tiles.grow(id(4), 0.1));
        let after = tiles.arrange(SCREEN, GAP);
        assert!(after[3].1.w() > before[3].1.w(), "four did not grow");
        assert!(after[4].1.w() < before[4].1.w(), "five did not give way");
        for i in 0..3 {
            assert_eq!(after[i], before[i], "window {i} moved with it");
        }
    }

    #[test]
    fn a_tile_at_its_limit_stops_rather_than_growing_the_other_way() {
        // A wheel that quietly changed which divider it was moving — and so
        // which way the tile grew — halfway through a turn would be a wheel
        // nobody can aim.
        let mut tiles = tiled(5);
        while tiles.grow(id(4), 0.1) {}
        let pinned = tiles.arrange(SCREEN, GAP);
        assert!(!tiles.grow(id(4), 0.1), "it kept reporting movement");
        assert_eq!(tiles.arrange(SCREEN, GAP), pinned, "an outer divider moved");
    }

    #[test]
    fn growing_can_never_take_a_window_out_of_existence() {
        let mut tiles = tiled(5);
        for _ in 0..50 {
            tiles.grow(id(4), -0.5);
        }
        for (_, rect) in tiles.arrange(SCREEN, GAP) {
            assert!(rect.w() > 0 && rect.h() > 0, "grown to nothing: {rect:?}");
        }
    }

    #[test]
    fn a_lone_window_has_nothing_to_grow_against() {
        assert!(!tiled(1).grow(id(1), 0.1));
        assert!(!tiled(2).grow(id(99), 0.1));
        assert!(!Tiles::new().grow(id(1), 0.1));
    }

    #[test]
    fn window_order_is_stable_across_arrange() {
        // Two arranges with no change between them must agree, or the shell
        // would see windows moving on every frame.
        let tiles = tiled(5);
        assert_eq!(tiles.arrange(SCREEN, GAP), tiles.arrange(SCREEN, GAP));
    }
}
