//! Touchpad gestures: what the fingers mean before anything acts on them.
//!
//! Only one gesture exists so far. Three fingers sliding sideways drive the
//! carousel — the strip goes where the fingers go, and settles onto a pane when
//! they lift. It is the touch counterpart of `Super`+`Shift`+`C` followed by a
//! walk along the strip, done in one motion.
//!
//! # Why this is a type and not four lines in the input handler
//!
//! A swipe is not a single event. libinput reports a begin, a run of deltas,
//! and an end, and the decision about what the gesture *is* cannot be made from
//! any one of them — it needs the travel so far. That accumulation is state,
//! and state that lives in the input handler is state nothing can test. Here it
//! is a plain value with no compositor in sight.
//!
//! # The gesture is claimed, not assumed
//!
//! Three fingers going down is not yet a carousel drag. Fingers land unevenly
//! and the first few deltas of a deliberate vertical swipe are indistinguishable
//! from the first few of a horizontal one, so the strip is not touched until the
//! travel has committed to an axis — see [`Swipe::takes_hold`]. Two consequences
//! are deliberate:
//!
//!   * A three-finger swipe that turns out to be vertical never becomes a
//!     carousel drag, even if it later drifts sideways. Deciding once and
//!     staying decided is what keeps the strip from lurching part way through a
//!     motion the user meant for something else, and it leaves the vertical
//!     axis free for a gesture that does not exist yet.
//!   * A workspace with nothing to scroll is never flipped into the carousel,
//!     because the claim can be refused — see [`Swipe::ignore`].

/// Fingers that mean the carousel.
///
/// Three rather than four because four is conventionally the window-manager
/// gesture on the desktops people arrive from, and three is the one that is
/// still free. Two is already scrolling, and libinput reports it as an axis
/// event rather than a swipe, so there is no collision to avoid there.
pub(crate) const CAROUSEL_FINGERS: u32 = 3;

/// How far the fingers must travel before the swipe commits to an axis, in the
/// touchpad units libinput reports.
///
/// Small enough that the strip starts moving as soon as the gesture reads as
/// deliberate, and large enough that the jitter of three fingers settling onto
/// the pad does not decide which way it went.
const COMMIT: f64 = 16.0;

/// Pixels of strip per unit of finger travel.
///
/// One-to-one is the honest mapping and the wrong one: a touchpad is a few
/// centimetres wide and a pane is half the screen, so an unamplified swipe
/// would take several strokes to reach the pane beside the one you are on.
/// Two puts a comfortable single stroke a little over one pane, which is the
/// distance the gesture is usually asking for.
const GAIN: f64 = 2.0;

/// What a swipe in progress has been decided to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// Still settling. Not enough travel to say which way this is going.
    Undecided,
    /// Driving the strip, which was resting at `origin` when it was claimed.
    Carousel { origin: i32 },
    /// Vertical, or the wrong number of fingers, or refused by a workspace with
    /// nothing to scroll. Nothing here wants it, and nothing will.
    Ignored,
}

/// A touchpad swipe from the moment the fingers land to the moment they lift.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Swipe {
    /// Everything the fingers have travelled since they went down, in touchpad
    /// units. Accumulated rather than used per-frame because the axis decision
    /// is about the shape of the whole motion, not the last few pixels of it.
    travel: (f64, f64),
    claim: Claim,
}

impl Swipe {
    /// Begin a swipe of `fingers` fingers.
    ///
    /// Any other count is dismissed here rather than tested on every update, so
    /// a four-finger swipe accumulates nothing and can never take the strip.
    pub(crate) fn new(fingers: u32) -> Self {
        Self {
            travel: (0.0, 0.0),
            claim: if fingers == CAROUSEL_FINGERS {
                Claim::Undecided
            } else {
                Claim::Ignored
            },
        }
    }

    /// Add a frame of finger travel, and report whether this is the update that
    /// proved the swipe horizontal — the moment to take hold of the strip.
    ///
    /// True at most once per swipe: the caller answers it with [`Self::drives`]
    /// or [`Self::ignore`], and either way the swipe is decided from then on.
    pub(crate) fn takes_hold(&mut self, dx: f64, dy: f64) -> bool {
        self.travel.0 += dx;
        self.travel.1 += dy;

        if self.claim != Claim::Undecided {
            return false;
        }
        let (x, y) = (self.travel.0.abs(), self.travel.1.abs());
        if x >= COMMIT && x > y {
            return true;
        }
        if y >= COMMIT {
            // Committed the other way. Marked rather than left undecided, so a
            // vertical swipe that wanders cannot grab the strip half way down.
            self.claim = Claim::Ignored;
        }
        false
    }

    /// Take the swipe, recording where the strip was resting when it started.
    pub(crate) fn drives(&mut self, origin: i32) {
        self.claim = Claim::Carousel { origin };
    }

    /// Refuse the swipe. Nothing it does from here on moves anything.
    pub(crate) fn ignore(&mut self) {
        self.claim = Claim::Ignored;
    }

    /// Where the strip should sit for everything the fingers have travelled, or
    /// `None` while the swipe belongs to nothing.
    ///
    /// Computed from the total travel and the origin rather than accumulated a
    /// delta at a time, so rounding to whole pixels cannot drift over the
    /// length of a long swipe — and so a swipe that goes out and comes back
    /// lands exactly where it started.
    ///
    /// Content follows the fingers: sliding left moves the strip left, which
    /// brings the panes to its right into view. That is the direction the
    /// gesture *is* — the strip is a thing under your hand, not a scrollbar
    /// beside one.
    pub(crate) fn offset(&self) -> Option<i32> {
        let Claim::Carousel { origin } = self.claim else {
            return None;
        };
        Some(origin - (self.travel.0 * GAIN).round() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `travel` to a fresh three-finger swipe one step at a time, taking
    /// hold at `origin` if it asks to, and report where it ends up.
    fn swipe(origin: i32, travel: &[(f64, f64)]) -> Swipe {
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        for (dx, dy) in travel {
            if s.takes_hold(*dx, *dy) {
                s.drives(origin);
            }
        }
        s
    }

    #[test]
    fn a_swipe_does_not_touch_the_strip_before_it_has_committed_to_an_axis() {
        // Three fingers landing unevenly. Nothing should move yet: the gesture
        // has not said what it is.
        let s = swipe(0, &[(1.0, -2.0), (-3.0, 1.0), (2.0, 2.0)]);
        assert_eq!(s.offset(), None);
    }

    #[test]
    fn a_sideways_swipe_takes_the_strip_and_moves_it_with_the_fingers() {
        let s = swipe(1000, &[(-10.0, 1.0), (-10.0, 0.0), (-10.0, -1.0)]);
        // Fingers left, so the strip goes left and the panes to the right of
        // the viewport come into it: the offset grows.
        assert_eq!(
            s.offset(),
            Some(1000 + 60),
            "30 units of travel at a gain of two"
        );
    }

    #[test]
    fn a_swipe_the_other_way_moves_the_strip_the_other_way() {
        let s = swipe(1000, &[(20.0, 0.0)]);
        assert_eq!(s.offset(), Some(1000 - 40));
    }

    #[test]
    fn a_vertical_swipe_never_becomes_a_carousel_drag() {
        // Straight down, then wandering well past the commit distance
        // sideways. The axis was decided on the way down and stays decided.
        let s = swipe(0, &[(0.0, -20.0), (-40.0, -5.0), (-40.0, 0.0)]);
        assert_eq!(s.offset(), None, "the strip must not lurch mid-gesture");
    }

    #[test]
    fn the_wrong_number_of_fingers_is_not_this_gesture() {
        for fingers in [1, 2, 4, 5] {
            let mut s = Swipe::new(fingers);
            assert!(
                !s.takes_hold(-200.0, 0.0),
                "{fingers} fingers must not take the strip"
            );
            assert_eq!(s.offset(), None);
        }
    }

    #[test]
    fn a_refused_claim_leaves_the_swipe_inert_for_the_rest_of_its_life() {
        // What an empty workspace does: there is nothing to scroll, so the
        // gesture is turned away and must not keep asking on every frame.
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        assert!(s.takes_hold(-20.0, 0.0));
        s.ignore();
        for _ in 0..10 {
            assert!(
                !s.takes_hold(-20.0, 0.0),
                "a refused swipe must not ask again"
            );
        }
        assert_eq!(s.offset(), None);
    }

    #[test]
    fn the_strip_is_taken_hold_of_exactly_once() {
        // `drives` records the origin, and asking twice would record a second
        // one part way along — the strip would jump by however far the fingers
        // had already travelled.
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        let mut claims = 0;
        for _ in 0..20 {
            if s.takes_hold(-5.0, 0.0) {
                claims += 1;
                s.drives(500);
            }
        }
        assert_eq!(claims, 1);
        assert_eq!(
            s.offset(),
            Some(500 + 200),
            "100 units of travel, all of it counted"
        );
    }

    #[test]
    fn a_swipe_out_and_back_lands_exactly_where_it_started() {
        // The property that comes of computing from total travel rather than
        // accumulating rounded steps: no drift over a long gesture.
        let mut out: Vec<(f64, f64)> = (0..40).map(|_| (-7.3, 0.0)).collect();
        out.extend((0..40).map(|_| (7.3, 0.0)));
        assert_eq!(swipe(1234, &out).offset(), Some(1234));
    }
}
