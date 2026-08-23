//! Tiling layouts.
//!
//! A layout answers exactly one question: given a rectangle and a number of
//! tiled windows, where does each one go? It sees no window state, no focus,
//! and no Wayland — which keeps every layout a pure function that can be
//! property-tested in microseconds.
//!
//! Floating and fullscreen windows never reach a layout; the workspace filters
//! them out first.

use crate::geometry::Rect;
use core::fmt;

/// Computes geometry for the tiled windows of a workspace.
pub trait Layout: fmt::Debug {
    /// Stable identifier, used in config and in the IPC protocol.
    fn name(&self) -> &'static str;

    /// Return exactly `count` rectangles, in window order, inside `area`.
    ///
    /// Implementations must return `count` rects even when `area` is too small
    /// to divide sensibly — an empty rect is a valid answer, a short vector is
    /// not, because callers zip the result against the window list.
    fn arrange(&self, area: Rect, count: usize) -> Vec<Rect>;
}

/// One master column beside a vertical stack of the remaining windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Columns {
    /// Fraction of the width given to the master window, clamped to 0.1..=0.9.
    pub master_ratio: f32,
    /// Space between windows, and between windows and the screen edge.
    pub gap: i32,
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            master_ratio: 0.5,
            gap: 8,
        }
    }
}

impl Layout for Columns {
    fn name(&self) -> &'static str {
        "columns"
    }

    fn arrange(&self, area: Rect, count: usize) -> Vec<Rect> {
        if count == 0 {
            return Vec::new();
        }
        let area = area.inset(self.gap);
        if area.is_empty() {
            return vec![Rect::ZERO; count];
        }
        if count == 1 {
            return vec![area];
        }

        let gap = self.gap;
        let ratio = self.master_ratio.clamp(0.1, 0.9);
        let master_w = ((area.w() - gap) as f32 * ratio).round() as i32;

        let mut out = Vec::with_capacity(count);
        out.push(Rect::from_xywh(area.x(), area.y(), master_w, area.h()));

        let stack_x = area.x() + master_w + gap;
        let stack_w = area.w() - master_w - gap;
        let n = (count - 1) as i32;

        // Integer division leaves a remainder of up to n-1 pixels. Spreading it
        // one pixel at a time across the leading windows makes the stack fill
        // the column exactly, instead of leaving a ragged gap at the bottom.
        let avail = area.h() - gap * (n - 1);
        let each = avail.div_euclid(n);
        let mut extra = avail.rem_euclid(n);

        let mut y = area.y();
        for _ in 0..n {
            let h = each + if extra > 0 { 1 } else { 0 };
            extra = (extra - 1).max(0);
            out.push(Rect::from_xywh(stack_x, y, stack_w, h));
            y += h + gap;
        }
        out
    }
}

/// Every window fills the whole area; only the focused one is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Monocle {
    pub gap: i32,
}

impl Layout for Monocle {
    fn name(&self) -> &'static str {
        "monocle"
    }

    fn arrange(&self, area: Rect, count: usize) -> Vec<Rect> {
        vec![area.inset(self.gap); count]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    /// No two tiles may claim the same pixel.
    fn assert_no_overlap(rects: &[Rect]) {
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.overlaps(*b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn always_returns_exactly_count_rects() {
        for n in 0..12 {
            assert_eq!(Columns::default().arrange(SCREEN, n).len(), n);
            assert_eq!(Monocle::default().arrange(SCREEN, n).len(), n);
        }
    }

    #[test]
    fn columns_never_overlap() {
        for n in 1..12 {
            assert_no_overlap(&Columns::default().arrange(SCREEN, n));
        }
    }

    #[test]
    fn columns_stack_fills_height_exactly() {
        let layout = Columns::default();
        // 7 windows over 1080px is the awkward case: the stack height does not
        // divide evenly, so a naive implementation leaves dead pixels.
        for n in 2..12 {
            let rects = layout.arrange(SCREEN, n);
            let stack = &rects[1..];
            let top = stack.first().expect("stack is non-empty for n >= 2").y();
            let bottom = stack.last().expect("stack is non-empty for n >= 2").bottom();
            assert_eq!(
                bottom - top,
                SCREEN.inset(layout.gap).h(),
                "stack for n={n} does not fill the column"
            );
        }
    }

    #[test]
    fn columns_respects_gaps_at_the_edges() {
        let layout = Columns { gap: 10, ..Default::default() };
        let r = layout.arrange(SCREEN, 1)[0];
        assert_eq!(r, Rect::from_xywh(10, 10, 1900, 1060));
    }

    #[test]
    fn absurd_master_ratios_are_clamped() {
        for ratio in [-5.0, 0.0, 1.0, 42.0] {
            let layout = Columns { master_ratio: ratio, gap: 0 };
            let rects = layout.arrange(SCREEN, 3);
            assert_no_overlap(&rects);
            assert!(rects.iter().all(|r| r.w() > 0), "ratio {ratio} produced a zero-width tile");
        }
    }

    #[test]
    fn tiny_area_degrades_without_panicking() {
        let tiny = Rect::from_xywh(0, 0, 4, 4);
        for n in 0..6 {
            let rects = Columns::default().arrange(tiny, n);
            assert_eq!(rects.len(), n);
        }
    }
}
