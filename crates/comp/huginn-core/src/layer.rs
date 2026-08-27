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

/// Which layer a surface sits on, ordered bottom to top.
///
/// Mirrors `wlr-layer-shell`'s own four layers, defined locally for the same
/// reason [`Anchors`] is: this crate stays independent of Wayland, and the
/// compositor translates at the boundary.
///
/// The derived `Ord` follows stacking order, which is what lets
/// [`keyboard_focus`] pick the topmost claimant by comparison rather than from
/// a table it would have to keep in step by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Background,
    Bottom,
    Top,
    Overlay,
}

impl Level {
    /// Whether a surface on this layer may take the keyboard outright.
    ///
    /// The protocol grants `exclusive` only above the ordinary window layer. A
    /// wallpaper that asked for it would hold the keyboard against every window
    /// on the desktop for as long as it stayed mapped, which from the user's
    /// side is indistinguishable from a hung session.
    ///
    /// Below `Top` the request degrades to [`Interactivity::OnDemand`] rather
    /// than being refused outright, which is what wlroots does and therefore
    /// what existing clients are written against.
    pub const fn allows_exclusive(self) -> bool {
        matches!(self, Self::Top | Self::Overlay)
    }
}

/// How much of the keyboard a layer surface is asking for.
///
/// This is the protocol's `keyboard_interactivity`, which the compositor did
/// not read at all until this existed: every layer surface was treated as
/// [`Interactivity::None`], so a panel could draw a search field it could never
/// fill and an Escape-to-close that never saw Escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interactivity {
    /// Draws only, and never takes the keyboard. The protocol default, and the
    /// right answer for a wallpaper or a status readout.
    #[default]
    None,
    /// Takes the keyboard when clicked and holds it until something else takes
    /// it. For a panel with a field in it.
    OnDemand,
    /// Takes the keyboard for as long as it is mapped. For a launcher or a
    /// session prompt.
    Exclusive,
}

/// A layer surface competing for the keyboard.
///
/// Generic over whatever the compositor uses to name a surface, so this crate
/// never has to hold a `WlSurface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focusable<K> {
    pub key: K,
    pub level: Level,
    pub interactivity: Interactivity,
    /// Mapping order; higher is more recent. Only the ordering is ever read, so
    /// an index into the compositor's own list does the job.
    pub mapped: u64,
}

/// Who should hold the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardFocus<K, W> {
    /// A layer surface asked for it and won.
    Layer(K),
    /// The ordinary focused window, which is the usual answer.
    Window(W),
    /// Nobody: an empty workspace with no interactive panel on it.
    Nothing,
}

/// Decide who holds the keyboard.
///
/// Resolution order, highest priority first:
///
/// 1. An [`Interactivity::Exclusive`] surface on a layer that
///    [`Level::allows_exclusive`] — topmost first, and most recently mapped
///    among equals. Recency wins because a session prompt opening over an
///    already-exclusive panel is the thing the user is being asked to answer.
/// 2. `clicked`, when it names a surface that wants the keyboard at all. This
///    is where an `on_demand` panel gets focus, and where an `exclusive`
///    request demoted by its layer still lands.
/// 3. The focused window, exactly as before any of this existed.
///
/// A surface named by `clicked` that has since unmapped or been destroyed is
/// simply absent from `layers` and falls through to the window. That is what
/// stops a dead panel from holding the keyboard, and it is why the compositor
/// may pass a stale `clicked` without checking it first.
pub fn keyboard_focus<K, W>(
    layers: &[Focusable<K>],
    clicked: Option<K>,
    window: Option<W>,
) -> KeyboardFocus<K, W>
where
    K: Copy + PartialEq,
    W: Copy,
{
    let exclusive = layers
        .iter()
        .filter(|l| l.interactivity == Interactivity::Exclusive && l.level.allows_exclusive())
        .max_by_key(|l| (l.level, l.mapped));
    if let Some(claimant) = exclusive {
        return KeyboardFocus::Layer(claimant.key);
    }

    if let Some(key) = clicked
        && let Some(claimant) = layers.iter().find(|l| l.key == key)
        && claimant.interactivity != Interactivity::None
    {
        return KeyboardFocus::Layer(claimant.key);
    }

    match window {
        Some(w) => KeyboardFocus::Window(w),
        None => KeyboardFocus::Nothing,
    }
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

#[cfg(test)]
mod focus {
    use super::*;

    /// A layer surface asking for `interactivity` on `level`.
    const fn claimant(key: u8, level: Level, interactivity: Interactivity) -> Focusable<u8> {
        Focusable { key, level, interactivity, mapped: key as u64 }
    }

    /// Shorthand for the common call: no click, one focused window.
    fn resolve(layers: &[Focusable<u8>]) -> KeyboardFocus<u8, u32> {
        keyboard_focus(layers, None, Some(7))
    }

    #[test]
    fn with_no_layers_at_all_the_window_keeps_the_keyboard() {
        assert_eq!(resolve(&[]), KeyboardFocus::Window(7));
    }

    #[test]
    fn with_nothing_focusable_nobody_holds_it() {
        let empty: [Focusable<u8>; 0] = [];
        assert_eq!(
            keyboard_focus::<u8, u32>(&empty, None, None),
            KeyboardFocus::Nothing
        );
    }

    #[test]
    fn a_wallpaper_never_takes_the_keyboard() {
        // The protocol default, and the case that must stay free: a background
        // surface under the pointer must not be able to take focus from a
        // window just by existing.
        let layers = [claimant(1, Level::Background, Interactivity::None)];
        assert_eq!(resolve(&layers), KeyboardFocus::Window(7));
    }

    #[test]
    fn an_exclusive_overlay_takes_it_from_the_window() {
        let layers = [claimant(1, Level::Overlay, Interactivity::Exclusive)];
        assert_eq!(resolve(&layers), KeyboardFocus::Layer(1));
    }

    #[test]
    fn exclusive_below_top_is_demoted_rather_than_honoured() {
        // A wallpaper asking for the keyboard outright would hold it against
        // every window for as long as it stayed mapped. It gets on-demand
        // instead, so it can still be clicked into.
        let layers = [claimant(1, Level::Background, Interactivity::Exclusive)];
        assert_eq!(resolve(&layers), KeyboardFocus::Window(7));

        let clicked = keyboard_focus(&layers, Some(1), Some(7));
        assert_eq!(clicked, KeyboardFocus::Layer(1), "a click still reaches it");
    }

    #[test]
    fn the_topmost_exclusive_surface_wins() {
        let layers = [
            claimant(1, Level::Overlay, Interactivity::Exclusive),
            claimant(2, Level::Top, Interactivity::Exclusive),
        ];
        // Ordering is by layer, not by position in the list: 2 is later but
        // sits lower, and lower loses.
        assert_eq!(resolve(&layers), KeyboardFocus::Layer(1));
    }

    #[test]
    fn among_equals_the_most_recently_mapped_wins() {
        // A lock-style prompt opening over an already-exclusive panel is the
        // thing being asked about, so it gets the keys.
        let layers = [
            claimant(1, Level::Overlay, Interactivity::Exclusive),
            claimant(2, Level::Overlay, Interactivity::Exclusive),
        ];
        assert_eq!(resolve(&layers), KeyboardFocus::Layer(2));
    }

    #[test]
    fn a_click_focuses_an_on_demand_panel() {
        let layers = [claimant(1, Level::Top, Interactivity::OnDemand)];
        assert_eq!(resolve(&layers), KeyboardFocus::Window(7), "not until clicked");
        assert_eq!(
            keyboard_focus(&layers, Some(1), Some(7)),
            KeyboardFocus::Layer(1)
        );
    }

    #[test]
    fn clicking_a_surface_that_wants_nothing_changes_nothing() {
        let layers = [claimant(1, Level::Top, Interactivity::None)];
        assert_eq!(
            keyboard_focus(&layers, Some(1), Some(7)),
            KeyboardFocus::Window(7)
        );
    }

    #[test]
    fn exclusive_outranks_a_clicked_panel() {
        let layers = [
            claimant(1, Level::Top, Interactivity::OnDemand),
            claimant(2, Level::Overlay, Interactivity::Exclusive),
        ];
        assert_eq!(
            keyboard_focus(&layers, Some(1), Some(7)),
            KeyboardFocus::Layer(2),
            "clicking a panel must not dismiss an exclusive grab"
        );
    }

    #[test]
    fn a_clicked_surface_that_went_away_falls_back_to_the_window() {
        // The compositor is allowed to hold a stale click: a panel that
        // unmapped or died is absent from `layers`, and focus returns to the
        // window rather than pointing at nothing. This is the case that
        // otherwise strands the keyboard on a dead surface.
        let layers = [claimant(2, Level::Top, Interactivity::OnDemand)];
        assert_eq!(
            keyboard_focus(&layers, Some(1), Some(7)),
            KeyboardFocus::Window(7)
        );
    }

    #[test]
    fn an_exclusive_surface_holds_it_even_with_no_window() {
        let layers = [claimant(1, Level::Top, Interactivity::Exclusive)];
        assert_eq!(
            keyboard_focus::<u8, u32>(&layers, None, None),
            KeyboardFocus::Layer(1)
        );
    }
}
