//! Placement of layer-shell surfaces: panels, docks, wallpapers, overlays.
//!
//! `wlr-layer-shell` lets a privileged client anchor a surface to screen edges
//! and optionally reserve space that normal windows must not cover. Both of
//! those are pure geometry, so both live here rather than in the compositor —
//! a panel that lands two pixels off is a bug you want to catch in a unit test,
//! not by squinting at a screenshot.
//!
//! The types mirror the protocol's own vocabulary but are defined locally, so
//! this crate keeps its independence from Wayland.

use crate::geometry::{Point, Rect, Size};

/// A screen edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Which edges a surface is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Anchors {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl Anchors {
    pub const fn count(self) -> u8 {
        self.top as u8 + self.bottom as u8 + self.left as u8 + self.right as u8
    }

    /// The edge this surface may reserve space against, if any.
    ///
    /// The protocol only gives an exclusive zone meaning when the surface is
    /// anchored to exactly one edge, or to one edge plus both edges
    /// perpendicular to it. A surface pinned to a corner, or to all four edges,
    /// has no unambiguous edge to reserve from, so it reserves nothing.
    pub const fn exclusive_edge(self) -> Option<Edge> {
        match self.count() {
            1 => {
                if self.top {
                    Some(Edge::Top)
                } else if self.bottom {
                    Some(Edge::Bottom)
                } else if self.left {
                    Some(Edge::Left)
                } else {
                    Some(Edge::Right)
                }
            }
            3 => {
                if self.left && self.right && self.top {
                    Some(Edge::Top)
                } else if self.left && self.right && self.bottom {
                    Some(Edge::Bottom)
                } else if self.top && self.bottom && self.left {
                    Some(Edge::Left)
                } else if self.top && self.bottom && self.right {
                    Some(Edge::Right)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Per-edge insets requested by a layer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

/// Space a layer surface has reserved along one edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exclusive {
    pub edge: Edge,
    pub size: i32,
}

/// Shrink `output` by every reserved zone, leaving the area windows may use.
///
/// Zones stack: a panel and a dock on the same edge each take their share. The
/// result is clamped at empty rather than inverting, so a client asking for an
/// absurd zone starves the tiling area instead of corrupting it.
pub fn usable_area(output: Rect, zones: &[Exclusive]) -> Rect {
    let (mut top, mut bottom, mut left, mut right) = (0, 0, 0, 0);
    for zone in zones {
        let size = zone.size.max(0);
        match zone.edge {
            Edge::Top => top += size,
            Edge::Bottom => bottom += size,
            Edge::Left => left += size,
            Edge::Right => right += size,
        }
    }

    let w = (output.w() - left - right).max(0);
    let h = (output.h() - top - bottom).max(0);
    Rect::new(
        Point::new(output.x() + left, output.y() + top),
        Size::new(w, h),
    )
}

/// Place a layer surface within `output`.
///
/// A zero in `desired` means "you decide", which is how a panel asks to span an
/// edge. Anchoring to both edges of an axis stretches the surface across it;
/// anchoring to neither centres it.
pub fn place(output: Rect, anchors: Anchors, desired: Size, margins: Margins) -> Rect {
    let stretch_h = anchors.left && anchors.right;
    let stretch_v = anchors.top && anchors.bottom;

    let w = if desired.w > 0 && !stretch_h {
        desired.w
    } else {
        output.w() - margins.left - margins.right
    };
    let h = if desired.h > 0 && !stretch_v {
        desired.h
    } else {
        output.h() - margins.top - margins.bottom
    };

    let x = if anchors.left && !anchors.right {
        output.x() + margins.left
    } else if anchors.right && !anchors.left {
        output.right() - w - margins.right
    } else {
        output.x() + (output.w() - w) / 2
    };

    let y = if anchors.top && !anchors.bottom {
        output.y() + margins.top
    } else if anchors.bottom && !anchors.top {
        output.bottom() - h - margins.bottom
    } else {
        output.y() + (output.h() - h) / 2
    };

    Rect::from_xywh(x, y, w.max(0), h.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1920, 1080);

    const fn anchors(top: bool, bottom: bool, left: bool, right: bool) -> Anchors {
        Anchors {
            top,
            bottom,
            left,
            right,
        }
    }

    #[test]
    fn a_top_panel_reserves_from_the_top() {
        let area = usable_area(
            SCREEN,
            &[Exclusive {
                edge: Edge::Top,
                size: 40,
            }],
        );
        assert_eq!(area, Rect::from_xywh(0, 40, 1920, 1040));
    }

    #[test]
    fn zones_on_opposite_edges_both_apply() {
        let area = usable_area(
            SCREEN,
            &[
                Exclusive { edge: Edge::Top, size: 32 },
                Exclusive { edge: Edge::Bottom, size: 64 },
            ],
        );
        assert_eq!(area, Rect::from_xywh(0, 32, 1920, 984));
    }

    #[test]
    fn two_zones_on_the_same_edge_stack() {
        let area = usable_area(
            SCREEN,
            &[
                Exclusive { edge: Edge::Left, size: 50 },
                Exclusive { edge: Edge::Left, size: 30 },
            ],
        );
        assert_eq!(area, Rect::from_xywh(80, 0, 1840, 1080));
    }

    #[test]
    fn an_absurd_zone_starves_the_area_without_inverting_it() {
        let area = usable_area(
            SCREEN,
            &[Exclusive { edge: Edge::Top, size: 99_999 }],
        );
        assert!(area.is_empty());
        assert_eq!(area.h(), 0, "height must clamp at zero, not go negative");
    }

    #[test]
    fn exclusive_edge_follows_the_protocol_rules() {
        // One edge: unambiguous.
        assert_eq!(anchors(true, false, false, false).exclusive_edge(), Some(Edge::Top));
        assert_eq!(anchors(false, true, false, false).exclusive_edge(), Some(Edge::Bottom));
        // One edge plus both perpendicular edges: still unambiguous. This is
        // how every real panel anchors itself.
        assert_eq!(anchors(true, false, true, true).exclusive_edge(), Some(Edge::Top));
        assert_eq!(anchors(false, true, true, true).exclusive_edge(), Some(Edge::Bottom));
        assert_eq!(anchors(true, true, true, false).exclusive_edge(), Some(Edge::Left));
        // A corner has no single edge to reserve from.
        assert_eq!(anchors(true, false, true, false).exclusive_edge(), None);
        // All four edges, or none, likewise.
        assert_eq!(anchors(true, true, true, true).exclusive_edge(), None);
        assert_eq!(Anchors::default().exclusive_edge(), None);
    }

    #[test]
    fn a_panel_anchored_across_the_top_spans_the_width() {
        let rect = place(
            SCREEN,
            anchors(true, false, true, true),
            Size::new(0, 40),
            Margins::default(),
        );
        assert_eq!(rect, Rect::from_xywh(0, 0, 1920, 40));
    }

    #[test]
    fn margins_inset_the_surface() {
        let rect = place(
            SCREEN,
            anchors(true, false, true, true),
            Size::new(0, 40),
            Margins { top: 8, right: 12, bottom: 0, left: 12 },
        );
        assert_eq!(rect, Rect::from_xywh(12, 8, 1896, 40));
    }

    #[test]
    fn an_unanchored_surface_is_centred() {
        let rect = place(SCREEN, Anchors::default(), Size::new(400, 300), Margins::default());
        assert_eq!(rect, Rect::from_xywh(760, 390, 400, 300));
        assert_eq!(rect.center(), SCREEN.center());
    }

    #[test]
    fn a_right_anchored_bar_sits_against_the_right_edge() {
        let rect = place(
            SCREEN,
            anchors(true, true, false, true),
            Size::new(64, 0),
            Margins::default(),
        );
        assert_eq!(rect, Rect::from_xywh(1856, 0, 64, 1080));
        assert_eq!(rect.right(), SCREEN.right());
    }

    #[test]
    fn a_fullscreen_overlay_anchors_to_every_edge() {
        // This is how a lock screen covers the output.
        let rect = place(SCREEN, anchors(true, true, true, true), Size::ZERO, Margins::default());
        assert_eq!(rect, SCREEN);
    }
}
