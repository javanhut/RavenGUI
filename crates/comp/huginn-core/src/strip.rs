//! The carousel: one ordered strip of panes, scrolled to follow focus.
//!
//! The tiling model in [`crate::tiles`] divides a fixed area, so every window
//! has to fit on screen and each new one makes the others smaller. A strip does
//! the opposite: panes keep a readable width and the strip grows past the edge
//! of the output, with the screen acting as a window onto it.
//!
//! # Scrolling is focus
//!
//! There is no scroll offset stored anywhere. The viewport is derived from which
//! window is focused, every time, which is what keeps this a pure function of
//! the workspace rather than a second piece of state that can disagree with it.
//!
//! It also means the existing focus bindings already drive the carousel:
//! `Super`+`Shift`+`J` moves to the next pane and brings it into view, because
//! bringing it into view is all that focusing it means here. A separate "scroll"
//! that could leave the focused pane off screen would be a second way to say the
//! same thing, and the two would drift.
//!
//! # Sizes do not change while scrolling
//!
//! Every pane in the strip is the same width, so moving the viewport changes
//! only positions. That matters beyond tidiness: an `xdg_toplevel` configure
//! carries a size and never a position, so a scroll that keeps widths constant
//! has nothing to tell clients about and costs no round-trips.

use crate::geometry::Rect;
use crate::window::WindowId;

/// How many panes to show at once, before the compositor says otherwise.
pub const DEFAULT_COLUMNS: u32 = 2;

/// The column geometry of a strip: where panes sit before scrolling.
///
/// Worked out once and shared, so [`target_offset`] and [`arrange_at`] cannot
/// disagree about how wide a pane is — which they would show up as the strip
/// scrolling to a position that does not quite reveal what it was aiming at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Metrics {
    inner: Rect,
    col_w: i32,
    stride: i32,
}

fn metrics(area: Rect, gap: i32, columns: u32, count: usize) -> Metrics {
    let inner = area.inset(gap);
    let gap = gap.max(0);
    let visible = columns.max(1).min(count.max(1) as u32) as i32;

    // Integer division leaves up to `visible - 1` pixels unused at the right
    // edge rather than making one column wider than its neighbours. A column a
    // pixel off from the one beside it is more visible than a strip that stops
    // a pixel short of the edge.
    let col_w = ((inner.w() - gap * (visible - 1)) / visible).max(1);
    Metrics {
        inner,
        col_w,
        stride: col_w + gap,
    }
}

/// The furthest a strip of `count` panes can be scrolled: the offset at which
/// its last pane ends flush with the right of the viewport.
///
/// Zero when the whole strip already fits, so a short strip has exactly one
/// legal position rather than a range it can drift within.
fn furthest(m: Metrics, count: usize) -> i32 {
    let span = (count as i32 - 1) * m.stride + m.col_w;
    (span - m.inner.w()).max(0)
}

/// How far the strip should be scrolled to show `focus`, in pixels.
///
/// Split out from [`arrange_at`] so the compositor can animate towards this
/// rather than jumping to it. The layout itself stays a pure function of an
/// offset it is handed; nothing here knows that time exists.
pub fn target_offset(
    windows: &[WindowId],
    focus: Option<WindowId>,
    area: Rect,
    gap: i32,
    columns: u32,
    current: i32,
) -> i32 {
    if windows.is_empty() {
        return 0;
    }
    let m = metrics(area, gap, columns, windows.len());
    scroll_offset(windows, focus, m, current)
}

/// The furthest the strip can be scrolled, in pixels.
///
/// What a touchpad drag clamps to. It has to be the same number
/// [`target_offset`] clamps to, or a swipe could park the strip a pixel past
/// where focus is ever allowed to take it and the next arrange would spring it
/// back under a hand that had not moved.
pub fn max_offset(windows: &[WindowId], area: Rect, gap: i32, columns: u32) -> i32 {
    if windows.is_empty() {
        return 0;
    }
    furthest(metrics(area, gap, columns, windows.len()), windows.len())
}

/// Where a strip let go of at `offset` should come to rest, and which pane ends
/// up at the left of the viewport there.
///
/// The two are returned together because they have to agree, and the caller
/// needs both: the compositor focuses the pane and slides to the offset, and
/// [`target_offset`] is asked to confirm the pair on the very next arrange. It
/// returns this same offset only because the pane named beside it is fully on
/// screen at it — which is what stops the settle from being undone by the focus
/// rule a frame later.
///
/// Nearest column, rather than the pane the drag started on: a swipe that
/// travelled more than half a column has moved on, and one that travelled less
/// has not. `None` when the strip has no panes to rest on.
pub fn snap(
    windows: &[WindowId],
    area: Rect,
    gap: i32,
    columns: u32,
    offset: i32,
) -> Option<(WindowId, i32)> {
    if windows.is_empty() {
        return None;
    }
    let m = metrics(area, gap, columns, windows.len());
    let furthest = furthest(m, windows.len());
    let settled = offset.clamp(0, furthest);

    let last = windows.len() as i32 - 1;
    let index = (settled as f32 / m.stride as f32)
        .round()
        .clamp(0.0, last as f32) as i32;
    // Clamped again on the way out: at the end of a strip whose length is not a
    // whole number of columns, the nearest column boundary lies past the last
    // legal offset. Stopping at the end still leaves that pane fully on screen,
    // because the end is defined as the place the final pane is flush with.
    Some((
        windows[index as usize],
        (index * m.stride).clamp(0, furthest),
    ))
}

/// Lay `windows` out left to right, scrolled by `offset` pixels.
///
/// Returns a rect for *every* window, including those scrolled off screen —
/// they get geometry outside the viewport rather than being left out. Omitting
/// them would be the cheaper-looking answer and the wrong one: the compositor
/// hands frame callbacks to a wider set than it renders, because a client that
/// stops being asked to paint deadlocks against a compositor waiting for the
/// buffer that callback would have produced. A pane that scrolls out of view
/// must still be able to keep drawing.
///
/// `columns` is clamped to at least one, and to at most the number of windows,
/// so a lone pane fills the area instead of sitting in a half-width column with
/// nothing beside it.
pub fn arrange_at(
    windows: &[WindowId],
    area: Rect,
    gap: i32,
    columns: u32,
    offset: i32,
) -> Vec<(WindowId, Rect)> {
    if windows.is_empty() {
        return Vec::new();
    }
    let m = metrics(area, gap, columns, windows.len());

    windows
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let x = m.inner.x() + index as i32 * m.stride - offset;
            (
                *id,
                Rect::from_xywh(x, m.inner.y(), m.col_w, m.inner.h().max(0)),
            )
        })
        .collect()
}

/// Lay `windows` out with the strip scrolled straight to `focus`.
///
/// The un-animated path, and what the layout means when nothing is in flight.
pub fn arrange(
    windows: &[WindowId],
    focus: Option<WindowId>,
    area: Rect,
    gap: i32,
    columns: u32,
    current: i32,
) -> Vec<(WindowId, Rect)> {
    let offset = target_offset(windows, focus, area, gap, columns, current);
    arrange_at(windows, area, gap, columns, offset)
}

/// How far the strip is scrolled, in pixels, so that `focus` is fully on screen.
///
/// Nudges rather than centres: a pane already in view does not move, so
/// focusing the pane beside the one you are on shifts the strip by one column
/// instead of recentring the whole thing under you. Centring reads well in a
/// demo and badly in use, because it moves panes that were already where you
/// were looking.
fn scroll_offset(windows: &[WindowId], focus: Option<WindowId>, m: Metrics, current: i32) -> i32 {
    let viewport = m.inner.w();
    let furthest = furthest(m, windows.len());

    // Where the strip is now, made safe to reason from: a held offset can be
    // stale after windows closed and the strip got shorter.
    let settled = current.clamp(0, furthest);

    let Some(index) = focus.and_then(|id| windows.iter().position(|w| *w == id)) else {
        // Nothing to reveal, so stay put rather than snapping home. A strip
        // that jumped back to the start whenever focus was briefly lost would
        // move under a user who did nothing.
        return settled;
    };

    let left = index as i32 * m.stride;
    let right = left + m.col_w;

    // Start from where the strip already is. A pane that is fully on screen
    // satisfies neither test below and the offset comes back unchanged, which
    // is what makes this a nudge rather than a recentre — and it is the whole
    // reason the current position has to be an argument. Derived from the
    // focused index alone this could only ever pin the pane to one edge, so
    // every step back towards the start scrolled the strip a full column even
    // though the pane was already in view.
    //
    // Two clamps in sequence, and the order is deliberate. Revealing the right
    // edge can push the offset past the left edge of the same pane when a pane
    // is wider than the viewport; applying the left rule second means such a
    // pane is shown from its start, which is where its content is.
    let mut offset = settled;
    if right > offset + viewport {
        offset = right - viewport;
    }
    if left < offset {
        offset = left;
    }
    offset.clamp(0, furthest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::from_xywh(0, 0, 1000, 600);
    const GAP: i32 = 10;

    fn id(n: u64) -> WindowId {
        WindowId::from_raw(n)
    }

    fn ids(n: u64) -> Vec<WindowId> {
        (1..=n).map(id).collect()
    }

    /// The x of each pane, in order.
    fn xs(laid: &[(WindowId, Rect)]) -> Vec<i32> {
        laid.iter().map(|(_, r)| r.x()).collect()
    }

    #[test]
    fn an_empty_strip_arranges_to_nothing() {
        assert!(arrange(&[], None, SCREEN, GAP, 2, 0).is_empty());
    }

    #[test]
    fn a_lone_pane_fills_the_area_rather_than_taking_one_column() {
        let laid = arrange(&ids(1), Some(id(1)), SCREEN, GAP, 2, 0);
        assert_eq!(laid.len(), 1);
        // The area inset by the gutter, and no narrower: `columns` is capped at
        // the number of windows so one pane is not left beside a hole.
        assert_eq!(laid[0].1, Rect::from_xywh(10, 10, 980, 580));
    }

    #[test]
    fn two_panes_split_the_area_with_a_gap_between_them() {
        let laid = arrange(&ids(2), Some(id(1)), SCREEN, GAP, 2, 0);
        let (w, gap_between) = (laid[0].1.w(), laid[1].1.x() - laid[0].1.right());
        assert_eq!(w, 485, "980 minus one gap, halved");
        assert_eq!(laid[1].1.w(), 485, "both columns are the same width");
        assert_eq!(gap_between, GAP);
        assert_eq!(laid[1].1.right(), 990, "the strip reaches the far gutter");
    }

    #[test]
    fn every_pane_gets_geometry_including_the_ones_off_screen() {
        // The count is the contract: `Space::arrange` asserts that the layout
        // returns exactly as many rects as there are tiled windows, and a pane
        // with no frame callback deadlocks its client.
        let laid = arrange(&ids(6), Some(id(1)), SCREEN, GAP, 2, 0);
        assert_eq!(laid.len(), 6);
        assert!(
            laid.iter().any(|(_, r)| r.x() > SCREEN.right()),
            "with six panes and two columns, some must sit past the edge"
        );
    }

    #[test]
    fn the_strip_starts_unscrolled_when_the_first_pane_has_focus() {
        let laid = arrange(&ids(6), Some(id(1)), SCREEN, GAP, 2, 0);
        assert_eq!(xs(&laid)[0], 10, "the first pane sits at the left gutter");
    }

    #[test]
    fn focusing_a_pane_to_the_right_scrolls_it_into_view() {
        let windows = ids(6);
        let at_first = arrange(&windows, Some(id(1)), SCREEN, GAP, 2, 0);
        let at_third = arrange(&windows, Some(id(3)), SCREEN, GAP, 2, 0);

        let third = at_third.iter().find(|(w, _)| *w == id(3)).unwrap().1;
        assert!(
            third.x() >= 0 && third.right() <= SCREEN.right(),
            "fully visible"
        );
        assert!(xs(&at_third)[0] < xs(&at_first)[0], "the strip moved left");
    }

    #[test]
    fn a_pane_already_in_view_does_not_move_the_strip() {
        // Nudge, not centre. This has to walk: focus out to pane 3, which
        // scrolls, then back to pane 2, which is still fully on screen.
        //
        // Comparing panes 1 and 2 from rest — which is what this test used to
        // do — proves nothing, because the offset is zero either way. The rule
        // only has teeth once the strip has actually moved.
        let windows = ids(6);
        let out = target_offset(&windows, Some(id(3)), SCREEN, GAP, 2, 0);
        assert!(out > 0, "focusing pane 3 from rest scrolls the strip");

        let laid = arrange_at(&windows, SCREEN, GAP, 2, out);
        let two = laid.iter().find(|(w, _)| *w == id(2)).expect("pane 2").1;
        assert!(
            two.x() >= 10 && two.right() <= 990,
            "pane 2 is fully on screen"
        );

        let back = target_offset(&windows, Some(id(2)), SCREEN, GAP, 2, out);
        assert_eq!(back, out, "a pane already in view must not move the strip");
    }

    #[test]
    fn focusing_a_pane_off_to_the_left_scrolls_back_to_it() {
        // The other half: not moving for a visible pane must not turn into
        // never moving backwards at all.
        let windows = ids(6);
        let out = target_offset(&windows, Some(id(5)), SCREEN, GAP, 2, 0);
        let back = target_offset(&windows, Some(id(1)), SCREEN, GAP, 2, out);
        assert!(
            back < out,
            "the strip scrolls back for a pane off to the left"
        );
        let laid = arrange_at(&windows, SCREEN, GAP, 2, back);
        assert_eq!(laid[0].1.x(), 10, "pane 1 ends up at the left gutter");
    }

    #[test]
    fn walking_focus_out_and_back_returns_the_strip_to_where_it_started() {
        // The property the nudge rule buys: moving focus along the strip and
        // back leaves it where it was, instead of ratcheting a column each way.
        let windows = ids(6);
        let start = 0;
        let out = target_offset(&windows, Some(id(3)), SCREEN, GAP, 2, start);
        let back_two = target_offset(&windows, Some(id(2)), SCREEN, GAP, 2, out);
        let back_one = target_offset(&windows, Some(id(1)), SCREEN, GAP, 2, back_two);
        assert_eq!(
            back_one, start,
            "back at the start, not a column short of it"
        );
    }

    #[test]
    fn an_offset_left_stale_by_a_shorter_strip_is_pulled_back() {
        // Windows closing shortens the strip under a held offset. The layout
        // must not keep honouring a position the strip no longer reaches.
        let windows = ids(3);
        let target = target_offset(&windows, Some(id(1)), SCREEN, GAP, 2, 99_999);
        let laid = arrange_at(&windows, SCREEN, GAP, 2, target);
        assert_eq!(laid[0].1.x(), 10, "pane 1 is reachable again");
    }

    #[test]
    fn the_strip_stops_at_its_own_end() {
        // Focusing the last pane must not scroll past it and leave a gap on the
        // right where nothing is.
        let windows = ids(6);
        let laid = arrange(&windows, Some(id(6)), SCREEN, GAP, 2, 0);
        let last = laid.last().unwrap().1;
        assert_eq!(last.right(), 990, "the final pane ends at the far gutter");
        // Two columns, so the pane before it is the other visible one and sits
        // at the left gutter. Everything earlier is off screen to the left.
        assert_eq!(xs(&laid)[4], 10);
        assert!(
            xs(&laid)[..4].iter().all(|x| *x < 0),
            "the rest scrolled off"
        );
    }

    #[test]
    fn scrolling_never_changes_a_pane_width() {
        // The property the whole design leans on: an xdg configure carries a
        // size and never a position, so if widths hold, scrolling tells clients
        // nothing and costs no round-trips.
        let windows = ids(6);
        let first = arrange(&windows, Some(id(1)), SCREEN, GAP, 2, 0);
        for focus in 1..=6 {
            let laid = arrange(&windows, Some(id(focus)), SCREEN, GAP, 2, 0);
            for (a, b) in first.iter().zip(laid.iter()) {
                assert_eq!(a.1.w(), b.1.w(), "width changed when focus moved");
                assert_eq!(a.1.h(), b.1.h(), "height changed when focus moved");
            }
        }
    }

    #[test]
    fn an_unfocused_strip_stays_where_it_is() {
        let at_rest = arrange(&ids(6), None, SCREEN, GAP, 2, 0);
        assert_eq!(xs(&at_rest)[0], 10, "at rest it shows its start");
        // And scrolled, it stays scrolled: losing focus is not a reason to move.
        let scrolled = arrange(&ids(6), None, SCREEN, GAP, 2, 495);
        assert_eq!(xs(&scrolled)[0], 10 - 495);
    }

    #[test]
    fn a_focus_that_is_not_in_the_strip_is_ignored() {
        let laid = arrange(&ids(3), Some(id(99)), SCREEN, GAP, 2, 0);
        assert_eq!(laid.len(), 3);
        assert_eq!(
            xs(&laid)[0],
            10,
            "no scroll, rather than a panic or an empty layout"
        );
    }

    #[test]
    fn one_column_gives_a_pane_the_whole_area() {
        let laid = arrange(&ids(4), Some(id(2)), SCREEN, GAP, 1, 0);
        assert_eq!(laid[0].1.w(), 980);
        let second = laid[1].1;
        assert_eq!(second.x(), 10, "the focused pane fills the viewport");
    }

    #[test]
    fn zero_columns_is_treated_as_one_rather_than_dividing_by_it() {
        let laid = arrange(&ids(3), Some(id(1)), SCREEN, GAP, 0, 0);
        assert_eq!(laid.len(), 3);
        assert_eq!(laid[0].1.w(), 980);
    }

    #[test]
    fn arranging_at_the_target_offset_is_the_same_as_arranging_to_focus() {
        // The split must not change what the layout means: `arrange` is only
        // `arrange_at` aimed at the target, and the compositor animating
        // between offsets must pass through exactly the same geometry.
        let windows = ids(6);
        for focus in 1..=6 {
            let target = target_offset(&windows, Some(id(focus)), SCREEN, GAP, 2, 0);
            assert_eq!(
                arrange(&windows, Some(id(focus)), SCREEN, GAP, 2, 0),
                arrange_at(&windows, SCREEN, GAP, 2, target),
            );
        }
    }

    #[test]
    fn an_offset_part_way_slides_the_panes_without_resizing_them() {
        // What an in-flight animation frame looks like. Positions move, widths
        // do not, which is the property that keeps a scroll off the wire.
        let windows = ids(6);
        let at_rest = arrange_at(&windows, SCREEN, GAP, 2, 0);
        let mid = arrange_at(&windows, SCREEN, GAP, 2, 247);
        for (a, b) in at_rest.iter().zip(mid.iter()) {
            assert_eq!(a.1.w(), b.1.w());
            assert_eq!(b.1.x(), a.1.x() - 247);
        }
    }

    #[test]
    fn the_furthest_offset_is_where_the_last_pane_ends_at_the_edge() {
        let windows = ids(6);
        let end = max_offset(&windows, SCREEN, GAP, 2);
        let laid = arrange_at(&windows, SCREEN, GAP, 2, end);
        assert_eq!(
            laid.last().unwrap().1.right(),
            990,
            "flush with the far gutter"
        );
        // And it agrees with what focusing the last pane asks for, which is the
        // property that keeps a drag and a focus change from disagreeing about
        // where the end of the strip is.
        assert_eq!(end, target_offset(&windows, Some(id(6)), SCREEN, GAP, 2, 0));
    }

    #[test]
    fn a_strip_that_already_fits_cannot_be_scrolled_at_all() {
        // Two panes in two columns fill the viewport exactly, so there is one
        // legal position rather than a range a swipe could drift within.
        assert_eq!(max_offset(&ids(2), SCREEN, GAP, 2), 0);
        assert_eq!(max_offset(&ids(1), SCREEN, GAP, 2), 0);
        assert_eq!(max_offset(&[], SCREEN, GAP, 2), 0);
    }

    #[test]
    fn letting_go_part_way_settles_on_the_nearer_column() {
        let windows = ids(6);
        // One column plus the gap after it, read off the layout rather than
        // recomputed here, so the test cannot disagree with the geometry.
        let laid = arrange_at(&windows, SCREEN, GAP, 2, 0);
        let stride = laid[1].1.x() - laid[0].1.x();
        assert_eq!(stride, 495);

        // A third of the way across: still nearer where it started.
        let (pane, offset) = snap(&windows, SCREEN, GAP, 2, stride / 3).expect("panes to rest on");
        assert_eq!(
            (pane, offset),
            (id(1), 0),
            "short of half a column stays put"
        );

        // Two thirds: it has moved on.
        let (pane, offset) =
            snap(&windows, SCREEN, GAP, 2, stride * 2 / 3).expect("panes to rest on");
        assert_eq!(
            (pane, offset),
            (id(2), stride),
            "past half a column moves on"
        );
    }

    #[test]
    fn the_pane_a_snap_names_is_the_one_on_screen_at_the_offset_it_names() {
        // The contract the compositor leans on: it focuses the pane and slides
        // to the offset, and `target_offset` must then agree rather than pull
        // the strip somewhere else on the next arrange.
        let windows = ids(7);
        let end = max_offset(&windows, SCREEN, GAP, 2);
        for raw in [-500, 0, 37, 260, 900, end - 1, end, end + 500] {
            let (pane, offset) = snap(&windows, SCREEN, GAP, 2, raw).expect("panes to rest on");
            assert_eq!(
                target_offset(&windows, Some(pane), SCREEN, GAP, 2, offset),
                offset,
                "settling at {raw} named a pane the offset does not show"
            );
        }
    }

    #[test]
    fn a_snap_never_lands_past_the_end_of_the_strip() {
        // Five panes in two columns: the strip's length is not a whole number
        // of columns, so the column boundary nearest the end lies past it.
        let windows = ids(5);
        let end = max_offset(&windows, SCREEN, GAP, 2);
        let (_, offset) = snap(&windows, SCREEN, GAP, 2, end).expect("panes to rest on");
        assert_eq!(
            offset, end,
            "stops at the end rather than at the boundary past it"
        );
    }

    #[test]
    fn an_empty_strip_has_nothing_to_settle_onto() {
        assert_eq!(snap(&[], SCREEN, GAP, 2, 400), None);
    }

    #[test]
    fn a_degenerate_area_produces_no_negative_sizes() {
        let tiny = Rect::from_xywh(0, 0, 4, 4);
        let laid = arrange(&ids(3), Some(id(2)), tiny, GAP, 2, 0);
        assert!(laid.iter().all(|(_, r)| r.w() >= 1 && r.h() >= 0));
    }
}
