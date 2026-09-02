//! Logical-pixel geometry.
//!
//! All coordinates here are *logical* pixels in the compositor's global space.
//! Scale factors belong to the output layer, not to this module — keeping the
//! two separate is what lets mixed-DPI setups be reasoned about at all.

/// A point in the global logical coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const ORIGIN: Self = Self { x: 0, y: 0 };

    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A cardinal direction, for finding a tile by where it is rather than by
/// where it sits in a list.
///
/// List order and screen position are not the same thing: in a master-and-stack
/// layout the window to the right of the master is the second in the list, but
/// the window below *that* one is the third — so "move right" and "move down"
/// cannot both be list arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// True if `to` lies on this side of `from`, judged by their centres.
    ///
    /// Centres rather than edges, because tiles share edges: two windows in a
    /// column touch, and an edge test would call each of them both above and
    /// below the other.
    pub const fn advances(self, from: Rect, to: Rect) -> bool {
        self.distance(from, to) > 0
    }

    /// True if the two rects share any extent across the axis of travel.
    ///
    /// A neighbour must be aligned as well as [`advances`](Self::advances), or a
    /// direction picks up diagonals: the top window of a stack sits higher than
    /// a full-height master column beside it, so by centres alone it is "up"
    /// from the master. Pressing up there and watching a window fly off to the
    /// right is not what the key says it does. Requiring alignment means a
    /// direction with nothing squarely that way does nothing at all.
    pub const fn aligned(self, from: Rect, to: Rect) -> bool {
        match self {
            Self::Left | Self::Right => from.origin.y < to.bottom() && to.origin.y < from.bottom(),
            Self::Up | Self::Down => from.origin.x < to.right() && to.origin.x < from.right(),
        }
    }

    /// How far `to` lies along this direction, centre to centre. Only
    /// meaningful for a rect that [`advances`](Self::advances).
    pub const fn distance(self, from: Rect, to: Rect) -> i32 {
        let a = from.center();
        let b = to.center();
        match self {
            Self::Left => a.x - b.x,
            Self::Right => b.x - a.x,
            Self::Up => a.y - b.y,
            Self::Down => b.y - a.y,
        }
    }
}

/// A width/height pair. Both dimensions are clamped to be non-negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl Size {
    pub const ZERO: Self = Self { w: 0, h: 0 };

    pub const fn new(w: i32, h: i32) -> Self {
        Self {
            w: if w < 0 { 0 } else { w },
            h: if h < 0 { 0 } else { h },
        }
    }

    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// An axis-aligned rectangle: an origin plus a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const ZERO: Self = Self {
        origin: Point::ORIGIN,
        size: Size::ZERO,
    };

    pub const fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self::new(Point::new(x, y), Size::new(w, h))
    }

    pub const fn x(self) -> i32 {
        self.origin.x
    }
    pub const fn y(self) -> i32 {
        self.origin.y
    }
    pub const fn w(self) -> i32 {
        self.size.w
    }
    pub const fn h(self) -> i32 {
        self.size.h
    }

    /// Exclusive right edge.
    pub const fn right(self) -> i32 {
        self.origin.x + self.size.w
    }
    /// Exclusive bottom edge.
    pub const fn bottom(self) -> i32 {
        self.origin.y + self.size.h
    }

    pub const fn is_empty(self) -> bool {
        self.size.is_empty()
    }

    pub const fn center(self) -> Point {
        Point::new(
            self.origin.x + self.size.w / 2,
            self.origin.y + self.size.h / 2,
        )
    }

    pub const fn contains(self, p: Point) -> bool {
        p.x >= self.origin.x && p.x < self.right() && p.y >= self.origin.y && p.y < self.bottom()
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.origin.x < other.right()
            && other.origin.x < self.right()
            && self.origin.y < other.bottom()
            && other.origin.y < self.bottom()
    }

    /// The rectangle where `self` and `other` overlap, or `None` when they are
    /// disjoint or merely touch at an edge.
    ///
    /// Distinct from [`Self::constrain_to`], which slides a rect until it fits
    /// inside another keeping its size; this keeps only the shared area. It is
    /// what clips a screenshot selection to the screen it was drawn on, so a
    /// drag that ran off the edge captures what is on screen and not a region
    /// nudged back inside it.
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.origin.x.max(other.origin.x);
        let y = self.origin.y.max(other.origin.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right > x && bottom > y {
            Some(Self::from_xywh(x, y, right - x, bottom - y))
        } else {
            None
        }
    }

    /// Shrink by `by` on every side. Collapses to empty rather than inverting.
    pub const fn inset(self, by: i32) -> Self {
        Self::from_xywh(
            self.origin.x + by,
            self.origin.y + by,
            self.size.w - by * 2,
            self.size.h - by * 2,
        )
    }

    /// The four edges of a ring `width` thick drawn just outside this rect, in
    /// the order top, bottom, left, right.
    ///
    /// Outside rather than inside: the pixels within the rect belong to the
    /// client, and a ring painted over them hides content the client drew. That
    /// puts the ring in the layout's gap, so a layout configured with no gap at
    /// all lets it reach a pixel or two into the neighbour.
    pub const fn ring(self, width: i32) -> [Self; 4] {
        let span = self.size.w + width * 2;
        [
            Self::from_xywh(self.origin.x - width, self.origin.y - width, span, width),
            Self::from_xywh(self.origin.x - width, self.bottom(), span, width),
            Self::from_xywh(self.origin.x - width, self.origin.y, width, self.size.h),
            Self::from_xywh(self.right(), self.origin.y, width, self.size.h),
        ]
    }

    /// Clamp this rect to lie inside `bounds`, preferring to move over resize.
    ///
    /// Used to keep a floating window on screen when an output is unplugged or
    /// resized. A window larger than `bounds` is shrunk to fit.
    pub fn constrain_to(self, bounds: Self) -> Self {
        let w = self.size.w.min(bounds.size.w);
        let h = self.size.h.min(bounds.size.h);
        let x = self.origin.x.clamp(bounds.origin.x, bounds.right() - w);
        let y = self.origin.y.clamp(bounds.origin.y, bounds.bottom() - h);
        Self::from_xywh(x, y, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_sizes_clamp_to_zero() {
        assert_eq!(Size::new(-5, -5), Size::ZERO);
        assert!(Rect::from_xywh(0, 0, 10, -1).is_empty());
    }

    #[test]
    fn intersection_keeps_only_the_shared_area() {
        let a = Rect::from_xywh(0, 0, 100, 100);
        // Overlapping in a corner.
        let b = Rect::from_xywh(80, 80, 100, 100);
        assert_eq!(a.intersection(b), Some(Rect::from_xywh(80, 80, 20, 20)));
        // A selection hanging off the right edge keeps its true position, not a
        // shifted one — the difference from constrain_to.
        let overhang = Rect::from_xywh(60, 10, 80, 20);
        assert_eq!(
            a.intersection(overhang),
            Some(Rect::from_xywh(60, 10, 40, 20))
        );
        // Disjoint, and merely touching, are both None.
        assert_eq!(a.intersection(Rect::from_xywh(200, 0, 10, 10)), None);
        assert_eq!(a.intersection(Rect::from_xywh(100, 0, 10, 10)), None);
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::from_xywh(0, 0, 10, 10);
        assert!(r.contains(Point::new(0, 0)));
        assert!(r.contains(Point::new(9, 9)));
        // The bottom-right edge is exclusive, so adjacent rects never both
        // claim the same pixel — which is what makes pointer hit-testing
        // unambiguous at a tiling seam.
        assert!(!r.contains(Point::new(10, 10)));
    }

    #[test]
    fn adjacent_rects_do_not_overlap() {
        let a = Rect::from_xywh(0, 0, 10, 10);
        let b = Rect::from_xywh(10, 0, 10, 10);
        assert!(!a.overlaps(b));
        assert!(a.overlaps(Rect::from_xywh(9, 0, 10, 10)));
    }

    #[test]
    fn inset_collapses_instead_of_inverting() {
        assert!(Rect::from_xywh(0, 0, 10, 10).inset(20).is_empty());
    }

    #[test]
    fn a_ring_surrounds_without_covering() {
        let r = Rect::from_xywh(100, 100, 40, 20);
        let ring = r.ring(2);
        for edge in ring {
            assert!(!edge.overlaps(r), "{edge:?} covers client pixels");
        }
        // Corners included: the top and bottom bars run the full span, so the
        // ring has no gaps where the sides meet them.
        assert_eq!(ring[0], Rect::from_xywh(98, 98, 44, 2));
        assert_eq!(ring[1], Rect::from_xywh(98, 120, 44, 2));
        assert_eq!(ring[2], Rect::from_xywh(98, 100, 2, 20));
        assert_eq!(ring[3], Rect::from_xywh(140, 100, 2, 20));
    }

    #[test]
    fn a_zero_width_ring_is_four_empty_rects() {
        assert!(
            Rect::from_xywh(0, 0, 10, 10)
                .ring(0)
                .iter()
                .all(|e| e.is_empty())
        );
    }

    #[test]
    fn direction_reads_centres_not_edges() {
        let top = Rect::from_xywh(0, 0, 10, 10);
        let bottom = Rect::from_xywh(0, 10, 10, 10);
        // The two touch. Only one of the two answers may be true, both ways.
        assert!(Dir::Down.advances(top, bottom));
        assert!(!Dir::Up.advances(top, bottom));
        assert!(Dir::Up.advances(bottom, top));
        assert!(!Dir::Down.advances(bottom, top));
    }

    #[test]
    fn a_window_is_not_its_own_neighbour() {
        let r = Rect::from_xywh(0, 0, 10, 10);
        for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
            assert!(!dir.advances(r, r));
        }
    }

    #[test]
    fn alignment_rejects_the_diagonal() {
        let master = Rect::from_xywh(0, 0, 100, 200);
        let beside = Rect::from_xywh(110, 0, 100, 100);
        // Squarely to the right, so right accepts it...
        assert!(Dir::Right.advances(master, beside) && Dir::Right.aligned(master, beside));
        // ...and its centre is higher, so up would too if centres were all
        // that counted. They are not: nothing in that column is above.
        assert!(Dir::Up.advances(master, beside));
        assert!(!Dir::Up.aligned(master, beside));
    }

    #[test]
    fn distance_grows_with_the_gap() {
        let from = Rect::from_xywh(0, 0, 10, 10);
        let near = Rect::from_xywh(20, 0, 10, 10);
        let far = Rect::from_xywh(200, 0, 10, 10);
        assert!(Dir::Right.distance(from, near) < Dir::Right.distance(from, far));
    }

    #[test]
    fn constrain_moves_then_shrinks() {
        let bounds = Rect::from_xywh(0, 0, 100, 100);
        // Fully outside: pulled back in, size preserved.
        assert_eq!(
            Rect::from_xywh(200, 200, 40, 40).constrain_to(bounds),
            Rect::from_xywh(60, 60, 40, 40)
        );
        // Bigger than bounds: shrunk to fit.
        assert_eq!(
            Rect::from_xywh(-10, -10, 400, 400).constrain_to(bounds),
            bounds
        );
    }
}
