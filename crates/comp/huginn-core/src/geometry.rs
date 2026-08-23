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

    /// Shrink by `by` on every side. Collapses to empty rather than inverting.
    pub const fn inset(self, by: i32) -> Self {
        Self::from_xywh(
            self.origin.x + by,
            self.origin.y + by,
            self.size.w - by * 2,
            self.size.h - by * 2,
        )
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
