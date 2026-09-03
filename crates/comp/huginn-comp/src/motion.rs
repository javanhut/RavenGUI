//! Windows on the move: a tile changing size, a pane leaving for the dock, a
//! pane coming back from it.
//!
//! # Why the *drawn* rectangle is animated, not the client
//!
//! A Wayland resize is a negotiation the compositor does not control the
//! timing of: it proposes a size, and the client commits a buffer at that
//! size whenever it gets round to it — a frame later, or five. Every window
//! touched by a relayout answers on its own schedule, so a minimize that
//! widens two neighbours shows up as a hole, then one neighbour jumping, then
//! the other. That is the flicker.
//!
//! So the compositor keeps its own idea of where each affected window is on
//! screen and eases that towards the layout's answer. The outline moves
//! continuously no matter when the commit arrives; the commit only changes
//! what is painted inside it.
//!
//! # Crop, don't stretch
//!
//! What is painted inside matters too. A tile being resized is drawn at its
//! natural size and *cropped* to the moving rectangle — the way niri does it
//! — rather than scaled into it. Scaling would mean the content squashes and
//! stretches with the animation and then pops when the client's new buffer
//! arrives at a different size, which is Hyprland's well-known resize
//! artefact and the last visible jump. Cropped, the content stays 1:1, the
//! edge slides, and a commit mid-flight changes nothing about the outline.
//!
//! A pane flying into the dock really is shrinking, so that one is scaled,
//! with its aspect kept; see [`fit`] and [`fit_aspect`].
//!
//! # Springs, not curves
//!
//! Each side of the rectangle is a critically damped [`Spring`] rather than a
//! curve over a fixed duration: a second relayout landing mid-flight bends
//! the tile towards its new place with the momentum it already has, instead
//! of stopping it and setting off again. That is the property Apple's fluid
//! interfaces rest on — interruptible, redirectable — and it is also why the
//! constants here are stiffnesses rather than durations. Stiffness 800 is
//! niri's default for moving windows, and settles in roughly 200ms.
//!
//! Like [`crate::anim`], nothing here reads a clock: every question takes
//! `now`, which is what lets the arithmetic be tested at chosen instants.

use std::time::Duration;

use huginn_core::geometry::Rect;

use crate::anim::Spring;
use crate::state::WorkspacePreview;

/// The spring that carries a tile to a new size or place.
pub(crate) const RESIZE_STIFFNESS: f32 = 800.0;

/// The spring that carries a pane down into the dock, or back up out of it.
///
/// Softer than a resize because the distance is longer, and because this one
/// *is* meant to be seen: it is what tells the user where the window went and
/// therefore where to find it again.
pub(crate) const MINIMIZE_STIFFNESS: f32 = 450.0;

/// What a window in motion is doing, which decides how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A tile going from one place in the layout to another. Drawn 1:1 and
    /// cropped to the moving rectangle.
    Resize,
    /// A pane shrinking and fading into the dock. The window is already
    /// minimized as far as the layout is concerned; this is its ghost.
    Minimize,
    /// A pane growing out of the dock into its tile.
    Restore,
}

/// One window's journey from where it was drawn to where it belongs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Motion {
    kind: Kind,
    x: Spring,
    y: Spring,
    w: Spring,
    h: Spring,
    alpha: Spring,
}

impl Motion {
    fn new(
        kind: Kind,
        from: Rect,
        to: Rect,
        alpha: (f32, f32),
        now: Duration,
        stiffness: f32,
        instant: bool,
    ) -> Self {
        let spring = |a: i32| Spring::at_rest(a as f32, stiffness);
        let mut motion = Self {
            kind,
            x: spring(from.x()),
            y: spring(from.y()),
            w: spring(from.w()),
            h: spring(from.h()),
            alpha: Spring::at_rest(alpha.0, stiffness),
        };
        motion.send(to, alpha.1, now, instant);
        motion
    }

    fn send(&mut self, to: Rect, alpha: f32, now: Duration, instant: bool) {
        let targets = [
            (&mut self.x, to.x() as f32),
            (&mut self.y, to.y() as f32),
            (&mut self.w, to.w() as f32),
            (&mut self.h, to.h() as f32),
            (&mut self.alpha, alpha),
        ];
        for (spring, target) in targets {
            if instant {
                spring.jump_to(target);
            } else {
                spring.pull_to(target, now);
            }
        }
    }

    /// A tile moving from `from` to `to`. `instant` is reduced motion: it
    /// is simply already there.
    pub(crate) fn resize(from: Rect, to: Rect, now: Duration, instant: bool) -> Self {
        Self::new(
            Kind::Resize,
            from,
            to,
            (1.0, 1.0),
            now,
            RESIZE_STIFFNESS,
            instant,
        )
    }

    /// A pane leaving `from` for the dock tile at `to`, fading as it goes.
    pub(crate) fn minimize(from: Rect, to: Rect, now: Duration, instant: bool) -> Self {
        Self::new(
            Kind::Minimize,
            from,
            to,
            (1.0, 0.0),
            now,
            MINIMIZE_STIFFNESS,
            instant,
        )
    }

    /// A pane arriving from the dock tile at `from` into its tile at `to`.
    pub(crate) fn restore(from: Rect, to: Rect, now: Duration, instant: bool) -> Self {
        Self::new(
            Kind::Restore,
            from,
            to,
            (0.0, 1.0),
            now,
            MINIMIZE_STIFFNESS,
            instant,
        )
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// Where it is heading.
    pub(crate) fn to(&self) -> Rect {
        Rect::from_xywh(
            self.x.target().round() as i32,
            self.y.target().round() as i32,
            self.w.target().round() as i32,
            self.h.target().round() as i32,
        )
    }

    /// Aim somewhere else, keeping whatever momentum it has.
    ///
    /// A relayout can land while a previous one is still being drawn — a
    /// second window closing before the first has finished going. The
    /// springs carry their velocity across, so the tile bends towards the new
    /// place rather than stopping and restarting.
    pub(crate) fn retarget(&mut self, to: Rect, now: Duration, instant: bool) {
        self.send(to, self.alpha.target(), now, instant);
    }

    /// Move both ends by the same amount, without restarting.
    ///
    /// For a window carried along by something that is not a resize — the
    /// carousel sliding the whole strip under it — so the motion follows the
    /// strip rather than finishing at a place the strip has since left.
    pub(crate) fn shift(&mut self, dx: i32, dy: i32) {
        self.x.shift(dx as f32);
        self.y.shift(dy as f32);
    }

    /// The rectangle to draw the window into at `now`.
    pub(crate) fn rect_at(&self, now: Duration) -> Rect {
        Rect::from_xywh(
            self.x.value(now).round() as i32,
            self.y.value(now).round() as i32,
            (self.w.value(now).round() as i32).max(1),
            (self.h.value(now).round() as i32).max(1),
        )
    }

    /// How opaque to draw it at `now`.
    pub(crate) fn alpha_at(&self, now: Duration) -> f32 {
        self.alpha.value(now).clamp(0.0, 1.0)
    }

    /// Whether it has arrived. A settled motion should be dropped: from then
    /// on the window is drawn the ordinary way, at the layout's rectangle.
    pub(crate) fn is_settled(&self, now: Duration) -> bool {
        [&self.x, &self.y, &self.w, &self.h, &self.alpha]
            .iter()
            .all(|spring| spring.is_settled(now))
    }
}

/// The transform that draws a buffer sitting at `placed` so that it fills
/// `drawn` instead.
///
/// For a pane on its way to or from the dock, which really is changing
/// size. The renderer scales about the origin and then shifts, so the shift
/// is where the box is minus where the scaled buffer would have landed on
/// its own — the same arithmetic as the dock's thumbnails.
pub(crate) fn fit(placed: Rect, drawn: Rect, alpha: f32) -> WorkspacePreview {
    let scale_x = f64::from(drawn.w()) / f64::from(placed.w().max(1));
    let scale_y = f64::from(drawn.h()) / f64::from(placed.h().max(1));
    WorkspacePreview {
        scale_x,
        scale_y,
        offset_x: f64::from(drawn.x()) - f64::from(placed.x()) * scale_x,
        offset_y: f64::from(drawn.y()) - f64::from(placed.y()) * scale_y,
        alpha,
    }
}

/// How small a window is at the start of its opening, and the end of its
/// closing, as a fraction of its pane. Slight on purpose: enough that the
/// window visibly arrives, not so much that it reads as being thrown at the
/// screen.
pub(crate) const APPEAR_SCALE: f64 = 0.94;

/// `rect` scaled by `factor` about its own centre.
pub(crate) fn scale_about_centre(rect: Rect, factor: f64) -> Rect {
    let (w, h) = (
        (f64::from(rect.w()) * factor).round() as i32,
        (f64::from(rect.h()) * factor).round() as i32,
    );
    Rect::from_xywh(
        rect.x() + (rect.w() - w) / 2,
        rect.y() + (rect.h() - h) / 2,
        w.max(1),
        h.max(1),
    )
}

/// The rectangle a window `t` of the way through appearing is drawn in:
/// [`APPEAR_SCALE`] of `placed` at `t == 0`, exactly `placed` at `t == 1`.
pub(crate) fn appear_rect(placed: Rect, t: f32) -> Rect {
    let t = f64::from(t.clamp(0.0, 1.0));
    scale_about_centre(placed, APPEAR_SCALE + (1.0 - APPEAR_SCALE) * t)
}

/// A window `t` of the way in: small and invisible at 0, exactly `placed` and
/// opaque at 1. `t` is the eased progress, so the caller picks the curve.
pub(crate) fn appear(placed: Rect, t: f32) -> WorkspacePreview {
    let t = t.clamp(0.0, 1.0);
    fit(placed, appear_rect(placed, t), t)
}

/// The reverse: a window `t` of the way out, exactly `placed` at 0 and small
/// and gone at 1.
pub(crate) fn vanish(placed: Rect, t: f32) -> WorkspacePreview {
    appear(placed, 1.0 - t.clamp(0.0, 1.0))
}

/// The largest rectangle with `shape`'s aspect ratio that fits centred in
/// `within`.
///
/// Where a pane lands in a dock tile. A window squashed square into an icon
/// slot reads as a distortion; one shrunk in proportion reads as the window,
/// smaller. And because both ends of the journey then share an aspect, every
/// rectangle in between does too.
pub(crate) fn fit_aspect(shape: Rect, within: Rect) -> Rect {
    let (sw, sh) = (f64::from(shape.w().max(1)), f64::from(shape.h().max(1)));
    let scale = (f64::from(within.w()) / sw).min(f64::from(within.h()) / sh);
    let (w, h) = ((sw * scale).round() as i32, (sh * scale).round() as i32);
    Rect::from_xywh(
        within.x() + (within.w() - w) / 2,
        within.y() + (within.h() - h) / 2,
        w.max(1),
        h.max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn scaling_about_the_centre_keeps_the_centre() {
        let rect = Rect::from_xywh(100, 200, 400, 300);
        let small = scale_about_centre(rect, 0.5);
        assert_eq!(small, Rect::from_xywh(200, 275, 200, 150));
        assert_eq!(small.center(), rect.center());
        assert_eq!(scale_about_centre(rect, 1.0), rect);
        assert_eq!(scale_about_centre(rect, 0.0).w(), 1, "never collapses to nothing");
    }

    #[test]
    fn appearing_starts_small_and_faint_and_ends_exact() {
        let placed = Rect::from_xywh(100, 100, 1000, 500);
        let start = appear(placed, 0.0);
        assert!((start.scale_x - APPEAR_SCALE).abs() < 0.01, "{}", start.scale_x);
        assert!((start.scale_y - APPEAR_SCALE).abs() < 0.01, "{}", start.scale_y);
        assert_eq!(start.alpha, 0.0);
        assert_eq!(appear_rect(placed, 0.0).center(), placed.center());

        let end = appear(placed, 1.0);
        assert_eq!(end.scale_x, 1.0);
        assert_eq!(end.scale_y, 1.0);
        assert_eq!(end.offset_x, 0.0);
        assert_eq!(end.offset_y, 0.0);
        assert_eq!(end.alpha, 1.0);
        assert_eq!(appear_rect(placed, 1.0), placed);
    }

    #[test]
    fn vanishing_is_the_mirror_of_appearing() {
        let placed = Rect::from_xywh(0, 0, 640, 480);
        for t in [0.0_f32, 0.25, 0.5, 0.9, 1.0] {
            let out = vanish(placed, t);
            let back = appear(placed, 1.0 - t);
            assert_eq!(out.scale_x, back.scale_x);
            assert_eq!(out.offset_y, back.offset_y);
            assert_eq!(out.alpha, back.alpha);
        }
        assert_eq!(vanish(placed, 0.0).alpha, 1.0);
        assert_eq!(vanish(placed, 1.0).alpha, 0.0);
    }

    #[test]
    fn a_motion_starts_where_it_was_and_ends_where_it_is_going() {
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(200, 50, 400, 300);
        let motion = Motion::resize(from, to, Duration::ZERO, false);
        assert_eq!(motion.rect_at(Duration::ZERO), from);
        assert_eq!(motion.rect_at(SECOND), to);
        assert!(!motion.is_settled(at(50)));
        assert!(motion.is_settled(SECOND));
        assert_eq!(motion.to(), to);
    }

    #[test]
    fn midway_it_is_somewhere_between() {
        // The whole point: no frame shows the old rectangle or the new one
        // until the motion has actually got there.
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(0, 0, 500, 100);
        let motion = Motion::resize(from, to, Duration::ZERO, false);
        let mid = motion.rect_at(at(60)).w();
        assert!(mid > 100 && mid < 500, "width jumped to {mid}");
    }

    #[test]
    fn it_never_draws_larger_than_where_it_is_going() {
        // Critically damped: a tile that overshot its pane would be drawn
        // over its neighbour for a frame.
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(0, 0, 500, 100);
        let motion = Motion::resize(from, to, Duration::ZERO, false);
        for ms in (0..1_000).step_by(4) {
            assert!(motion.rect_at(at(ms)).w() <= 500, "overshot at {ms}ms");
        }
    }

    #[test]
    fn minimizing_fades_out_and_restoring_fades_in() {
        let a = Rect::from_xywh(0, 0, 100, 100);
        let b = Rect::from_xywh(0, 0, 10, 10);
        let away = Motion::minimize(a, b, Duration::ZERO, false);
        assert!((away.alpha_at(Duration::ZERO) - 1.0).abs() < 1e-5);
        assert!(away.alpha_at(SECOND).abs() < 1e-5);
        let back = Motion::restore(b, a, Duration::ZERO, false);
        assert!(back.alpha_at(Duration::ZERO).abs() < 1e-5);
        assert!((back.alpha_at(SECOND) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn retargeting_continues_from_where_it_is_with_its_momentum() {
        // A second relayout mid-flight must not snap the tile back to where
        // the first one started, nor stop it dead: it was moving, so for a
        // moment it keeps moving that way before bending back.
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(0, 0, 500, 100);
        let mut motion = Motion::resize(from, to, Duration::ZERO, false);
        let midway = motion.rect_at(at(20));
        motion.retarget(Rect::from_xywh(0, 0, 50, 100), at(20), false);
        assert_eq!(motion.rect_at(at(20)), midway);
        assert!(motion.rect_at(at(24)).w() >= midway.w(), "it stopped dead");
        assert!(
            motion.rect_at(at(300)).w() < midway.w(),
            "it did not reverse"
        );
        assert_eq!(motion.rect_at(SECOND).w(), 50);
    }

    #[test]
    fn shifting_moves_both_ends_without_restarting() {
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(0, 0, 300, 100);
        let mut motion = Motion::resize(from, to, Duration::ZERO, false);
        let before = motion.rect_at(at(30));
        motion.shift(-40, 0);
        let after = motion.rect_at(at(30));
        assert_eq!(after.x(), before.x() - 40);
        assert_eq!(after.w(), before.w());
        assert_eq!(motion.to(), Rect::from_xywh(-40, 0, 300, 100));
    }

    #[test]
    fn an_instant_motion_is_already_there() {
        // What reduced motion turns every one of these into.
        let from = Rect::from_xywh(0, 0, 100, 100);
        let to = Rect::from_xywh(9, 9, 9, 9);
        let motion = Motion::resize(from, to, at(10), true);
        assert_eq!(motion.rect_at(at(10)), to);
        assert!(motion.is_settled(at(10)));
    }

    #[test]
    fn fit_puts_the_buffer_exactly_in_the_drawn_box() {
        let placed = Rect::from_xywh(100, 200, 400, 300);
        let drawn = Rect::from_xywh(10, 20, 200, 150);
        let t = fit(placed, drawn, 1.0);
        // Scaled origin plus the offset lands on the box's corner.
        let x = f64::from(placed.x()) * t.scale_x + t.offset_x;
        let y = f64::from(placed.y()) * t.scale_y + t.offset_y;
        assert!(
            (x - 10.0).abs() < 1e-9 && (y - 20.0).abs() < 1e-9,
            "landed at {x},{y}"
        );
        assert!((f64::from(placed.w()) * t.scale_x - 200.0).abs() < 1e-9);
        assert!((f64::from(placed.h()) * t.scale_y - 150.0).abs() < 1e-9);
    }

    #[test]
    fn fit_aspect_keeps_the_shape_and_centres_it() {
        let shape = Rect::from_xywh(0, 0, 1600, 900);
        let tile = Rect::from_xywh(100, 100, 64, 64);
        let r = fit_aspect(shape, tile);
        assert_eq!(r.w(), 64);
        assert_eq!(r.h(), 36);
        assert_eq!(r.x(), 100);
        assert_eq!(r.y(), 100 + (64 - 36) / 2);
    }
}
