//! Touchpad gestures: what the fingers mean before anything acts on them.
//!
//! Three fingers sliding sideways drive the workspace switcher. The current
//! workspace shrinks into the centre, its neighbours appear at the sides, and
//! the row follows the fingers until they choose a workspace.
//!
//! Vertical motion depends on what is on screen. Bare, up shrinks the
//! workspaces into the overview — the reveal follows the fingers, and lifting
//! them commits or snaps back — and down puts the focused pane away to the
//! dock. With the overview open, down expands the centred workspace back out
//! the same way. With the application switcher up, up accepts the highlighted
//! minimized application. A three-finger double tap opens that temporary
//! switcher; horizontal motion then moves through its applications instead of
//! through workspaces.
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
//! from the first few of a horizontal one, so the row is not touched until the
//! travel has committed to an axis — see [`Swipe::takes_hold`]. One consequence
//! is deliberate:
//!
//!   * A three-finger swipe that turns out to be vertical never becomes a
//!     carousel drag, even if it later drifts sideways. Deciding once and
//!     staying decided is what keeps the workspace row from lurching part way
//!     through a motion the user meant for something else, and is what lets
//!     the overview reveal ride the vertical travel without ever fighting the
//!     row for the same fingers.
//!
//! # The lift carries the speed of the fingers
//!
//! A swipe ends with the fingers still moving, and what they were doing at
//! that instant is the second half of the gesture: a row let go of at speed
//! was thrown, one let go of at rest was placed. So the swipe keeps a running
//! estimate of its speed, weighted to its last few reports, and at the lift
//! reports a velocity
//! ([`Swipe::velocity`]) for the animation to set off at, and a landing stage
//! ([`Swipe::landing`]) chosen from where that speed would have *carried* the
//! row rather than from where the fingers happened to be — the same
//! projection a scroll view uses to decide where a fling stops. A flick that
//! moved the row a quarter of a stage still lands on the next one, which is
//! what makes it feel thrown rather than dropped and snatched back.

use std::time::Duration;

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
/// Small enough that the row starts moving as soon as the gesture reads as
/// deliberate, and large enough that the jitter of three fingers settling onto
/// the pad does not decide which way it went.
const COMMIT: f64 = 16.0;

/// Touchpad travel that moves the workspace row by one complete card.
const UNITS_PER_WORKSPACE: f64 = 180.0;

/// Touchpad travel that takes the overview from fully expanded to fully
/// revealed.
///
/// Short enough that one comfortable flick spans the whole reveal, and long
/// enough that the shrink reads as following the fingers rather than jumping
/// at them.
const UNITS_PER_REVEAL: f64 = 140.0;

/// How close to where it started a reveal swipe must come back for the lift
/// to read as changed-my-mind rather than as the command it set out to be.
const REVEAL_RETURN: f32 = 0.1;

/// Longest gap between taps that still reads as a double tap.
const DOUBLE_TAP_MSEC: u32 = 500;

/// How far back the velocity estimate remembers, as the time constant of
/// its weighting: a report this old counts about a third as much as the
/// newest.
///
/// Long enough that the jitter of one report does not decide the speed,
/// short enough that a swipe reversed in its last fifty milliseconds — the
/// hand saying "no" at the end — reports the reversal and not the run-up.
const VELOCITY_MEMORY: Duration = Duration::from_millis(24);

/// Fingers that have not moved for this long before lifting were resting,
/// and a rest has no velocity: a row held still and let go must not fling.
/// libinput reports nothing for fingers that are still, so this is how a
/// rest is seen at all.
const VELOCITY_STALE: Duration = Duration::from_millis(60);

/// Faster than this, in reveals per second, a lift decides the reveal by the
/// way it was going rather than by where it was: an upward drag that ends
/// with a flick down was changed to a close on the way, whatever it opened.
const REVEAL_FLICK: f32 = 2.0;

/// The most a flick can carry the row past the fingers, in stages. One: a
/// throw goes to the next stage, not across the row, as a page turns to
/// the next page however hard it is flicked.
const CARRY: f32 = 1.0;

/// Recognition state for the three-finger double-tap shortcut.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DoubleTap {
    last: Option<u32>,
}

impl DoubleTap {
    /// Record one tap, returning true only for the second tap of a close pair.
    pub(crate) fn tap(&mut self, fingers: u32, time_msec: u32) -> bool {
        if fingers != CAROUSEL_FINGERS {
            self.last = None;
            return false;
        }
        let doubled = self
            .last
            .is_some_and(|last| time_msec.wrapping_sub(last) <= DOUBLE_TAP_MSEC);
        self.last = (!doubled).then_some(time_msec);
        doubled
    }
}

/// What a swipe in progress has been decided to be.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Claim {
    /// Still settling. Not enough travel to say which way this is going.
    Undecided,
    /// Driving the workspace row from the workspace at `origin`.
    Carousel { origin: f32 },
    /// A vertical application-switcher command.
    Vertical(Vertical),
    /// Driving the overview reveal, which sat at `origin` when the axis
    /// committed, having set out in `direction`.
    Reveal { origin: f32, direction: Vertical },
    /// The wrong number of fingers. Nothing here wants it.
    Ignored,
}

/// A completed vertical three-finger command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vertical {
    Up,
    Down,
}

/// The axis a swipe has just committed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    Horizontal,
    Vertical,
}

/// A running estimate of how fast the fingers are moving.
///
/// Each report's speed is folded into the estimate with a weight that
/// depends on how long it covers, so the estimate follows the last
/// [`VELOCITY_MEMORY`] or so of the swipe and forgets the rest. Two values
/// rather than a history, so the swipe stays a plain copyable value.
#[derive(Debug, Clone, Copy)]
struct Pace {
    /// Touchpad units per second, along each axis.
    velocity: (f64, f64),
    /// When the last report arrived.
    last: Option<Duration>,
}

impl Pace {
    fn new() -> Self {
        Self {
            velocity: (0.0, 0.0),
            last: None,
        }
    }

    /// Fold in a report of `(dx, dy)` at `now`.
    fn report(&mut self, dx: f64, dy: f64, now: Duration) {
        let Some(last) = self.last.replace(now) else {
            return;
        };
        let dt = now.saturating_sub(last).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        // A report covering longer than the memory replaces the estimate
        // outright; a short one nudges it. Continuous in `dt`, so the rate
        // libinput happens to report at does not change what is measured.
        let weight = 1.0 - (-dt / VELOCITY_MEMORY.as_secs_f64()).exp();
        let (vx, vy) = (dx / dt, dy / dt);
        self.velocity.0 += weight * (vx - self.velocity.0);
        self.velocity.1 += weight * (vy - self.velocity.1);
    }

    /// The speed at `now`, or none if the fingers had stopped by then.
    fn velocity(&self, now: Duration) -> (f64, f64) {
        match self.last {
            Some(last) if now.saturating_sub(last) <= VELOCITY_STALE => self.velocity,
            _ => (0.0, 0.0),
        }
    }
}

/// A touchpad swipe from the moment the fingers land to the moment they lift.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Swipe {
    /// Everything the fingers have travelled since they went down, in touchpad
    /// units. Accumulated rather than used per-frame because the axis decision
    /// is about the shape of the whole motion, not the last few pixels of it.
    travel: (f64, f64),
    /// How fast the fingers are going, for the lift.
    pace: Pace,
    claim: Claim,
}

impl Swipe {
    /// Begin a swipe of `fingers` fingers.
    ///
    /// Any other count is dismissed here rather than tested on every update, so
    /// a four-finger swipe accumulates nothing and can never take the row.
    pub(crate) fn new(fingers: u32) -> Self {
        Self {
            travel: (0.0, 0.0),
            pace: Pace::new(),
            claim: if fingers == CAROUSEL_FINGERS {
                Claim::Undecided
            } else {
                Claim::Ignored
            },
        }
    }

    /// Add a frame of finger travel at `now`, and report whether this is the
    /// update that proved the swipe horizontal — the moment to take hold of
    /// the row.
    ///
    /// True at most once per swipe: the caller answers it with [`Self::drives`]
    /// and the swipe is decided from then on.
    pub(crate) fn takes_hold(&mut self, dx: f64, dy: f64, now: Duration) -> Option<Hold> {
        self.travel.0 += dx;
        self.travel.1 += dy;
        self.pace.report(dx, dy, now);

        if self.claim != Claim::Undecided {
            return None;
        }
        let (x, y) = (self.travel.0.abs(), self.travel.1.abs());
        if x >= COMMIT && x > y {
            return Some(Hold::Horizontal);
        }
        if y >= COMMIT {
            self.claim = Claim::Vertical(if self.travel.1 < 0.0 {
                Vertical::Up
            } else {
                Vertical::Down
            });
            return Some(Hold::Vertical);
        }
        None
    }

    /// Take the swipe, recording which workspace was centred when it started.
    pub(crate) fn drives(&mut self, origin: f32) {
        self.claim = Claim::Carousel { origin };
    }

    /// Which workspace position the row should sit at for the accumulated
    /// travel, or
    /// `None` while the swipe belongs to nothing.
    ///
    /// Computed from the total travel and the origin rather than accumulated a
    /// delta at a time, so rounding to whole pixels cannot drift over the
    /// length of a long swipe — and so a swipe that goes out and comes back
    /// lands exactly where it started.
    ///
    /// Sliding left advances to the workspace on the right, as though the row
    /// of workspace cards were directly under the fingers.
    pub(crate) fn position(&self) -> Option<f32> {
        let Claim::Carousel { origin } = self.claim else {
            return None;
        };
        Some(origin - (self.travel.0 / UNITS_PER_WORKSPACE) as f32)
    }

    /// How fast what the swipe is driving was moving at `now`, in its own
    /// units per second — stages for the row, reveals for the overview — or
    /// `None` while the swipe drives nothing. Positive the way the value
    /// grows, so it can be handed straight to the value's spring.
    pub(crate) fn velocity(&self, now: Duration) -> Option<f32> {
        let (vx, vy) = self.pace.velocity(now);
        match self.claim {
            Claim::Carousel { .. } => Some(-(vx / UNITS_PER_WORKSPACE) as f32),
            Claim::Reveal { .. } => Some(-(vy / UNITS_PER_REVEAL) as f32),
            _ => None,
        }
    }

    /// The stage the row should settle on if the fingers lift at `now`, or
    /// `None` while the swipe is not driving the row.
    ///
    /// Not the nearest stage to the fingers: the nearest to where their speed
    /// would have carried the row, so a flick lands a stage over though the
    /// fingers only moved it a little, and a drag past halfway that is
    /// flicked back goes back. Never more than [`CARRY`] past the fingers,
    /// however hard the throw. The row's ends are the caller's to apply, as
    /// they are for [`Self::position`].
    pub(crate) fn landing(&self, now: Duration) -> Option<f32> {
        let position = self.position()?;
        let velocity = self.velocity(now)?;
        let carried = crate::anim::project(velocity, crate::anim::PAGED_DECELERATION);
        Some((position + carried.clamp(-CARRY, CARRY)).round())
    }

    /// The vertical command this swipe committed to, if any.
    pub(crate) fn vertical(&self) -> Option<Vertical> {
        let Claim::Vertical(direction) = self.claim else {
            return None;
        };
        Some(direction)
    }

    /// Take the vertical swipe as the overview's reveal, which sat at `origin`
    /// when the axis committed.
    ///
    /// Only a swipe already proved vertical can be taken. From here the swipe
    /// is a drag, not a command: [`Self::vertical`] goes quiet, so the lift
    /// cannot also minimize or accept anything.
    pub(crate) fn drives_reveal(&mut self, origin: f32) {
        if let Claim::Vertical(direction) = self.claim {
            self.claim = Claim::Reveal { origin, direction };
        }
    }

    /// Where the overview reveal should sit for the accumulated travel, or
    /// `None` while the swipe is not driving it.
    ///
    /// Computed from the total travel for the same reason as
    /// [`Self::position`]: a swipe that goes out and comes back lands exactly
    /// where it started. Sliding up reveals, as though the workspace were
    /// pushed away under the fingers.
    pub(crate) fn reveal(&self) -> Option<f32> {
        let Claim::Reveal { origin, .. } = self.claim else {
            return None;
        };
        Some((origin - (self.travel.1 / UNITS_PER_REVEAL) as f32).clamp(0.0, 1.0))
    }

    /// Whether a reveal-driving swipe ending at `now` leaves the overview
    /// open.
    ///
    /// Fingers still moving at the lift decide by the way they were going:
    /// a flick is the clearest statement a gesture makes, and one that
    /// reverses the drag it ends is a change of mind mid-motion. Fingers at
    /// rest decide by the direction the swipe set out in — a flick does not
    /// have to drag the reveal past halfway to mean it — unless they came
    /// back to within [`REVEAL_RETURN`] of where they started, which is the
    /// slower change of mind. The threshold is clamped into the reveal's
    /// range so a drag that began near either end still has a line it can
    /// cross.
    pub(crate) fn reveal_commits(&self, now: Duration) -> Option<bool> {
        let Claim::Reveal { origin, direction } = self.claim else {
            return None;
        };
        let at = self.reveal()?;
        if let Some(velocity) = self.velocity(now)
            && velocity.abs() >= REVEAL_FLICK
        {
            return Some(velocity > 0.0);
        }
        Some(match direction {
            Vertical::Up => at > (origin + REVEAL_RETURN).min(1.0 - REVEAL_RETURN),
            Vertical::Down => at > (origin - REVEAL_RETURN).max(REVEAL_RETURN),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far apart the reports of a swipe are: libinput at a touchpad's
    /// usual rate.
    const STEP: Duration = Duration::from_millis(8);

    /// The clock at the `n`th report.
    fn tick(n: usize) -> Duration {
        STEP * n as u32
    }

    /// When a swipe of `n` reports lifts, with the fingers still moving.
    fn lift(n: usize) -> Duration {
        tick(n.saturating_sub(1))
    }

    /// Feed `travel` to a fresh three-finger swipe one step at a time, taking
    /// hold at `origin` if it asks to, and report where it ends up.
    fn swipe(origin: f32, travel: &[(f64, f64)]) -> Swipe {
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        for (n, (dx, dy)) in travel.iter().enumerate() {
            if s.takes_hold(*dx, *dy, tick(n)) == Some(Hold::Horizontal) {
                s.drives(origin);
            }
        }
        s
    }

    #[test]
    fn a_swipe_does_not_touch_the_row_before_it_has_committed_to_an_axis() {
        // Three fingers landing unevenly. Nothing should move yet: the gesture
        // has not said what it is.
        let s = swipe(0.0, &[(1.0, -2.0), (-3.0, 1.0), (2.0, 2.0)]);
        assert_eq!(s.position(), None);
    }

    #[test]
    fn a_sideways_swipe_takes_the_row_and_moves_it_with_the_fingers() {
        let s = swipe(2.0, &[(-60.0, 1.0), (-60.0, 0.0), (-60.0, -1.0)]);
        assert_eq!(s.position(), Some(3.0));
    }

    #[test]
    fn a_swipe_the_other_way_moves_the_row_the_other_way() {
        let s = swipe(2.0, &[(180.0, 0.0)]);
        assert_eq!(s.position(), Some(1.0));
    }

    #[test]
    fn a_vertical_swipe_never_becomes_a_carousel_drag() {
        // Straight down, then wandering well past the commit distance
        // sideways. The axis was decided on the way down and stays decided.
        let s = swipe(0.0, &[(0.0, -20.0), (-40.0, -5.0), (-40.0, 0.0)]);
        assert_eq!(s.position(), None, "the row must not lurch mid-gesture");
        assert_eq!(s.vertical(), Some(Vertical::Up));
    }

    #[test]
    fn the_wrong_number_of_fingers_is_not_this_gesture() {
        for fingers in [1, 2, 4, 5] {
            let mut s = Swipe::new(fingers);
            assert!(
                s.takes_hold(-200.0, 0.0, Duration::ZERO).is_none(),
                "{fingers} fingers must not take the row"
            );
            assert_eq!(s.position(), None);
        }
    }

    #[test]
    fn the_row_is_taken_hold_of_exactly_once() {
        // `drives` records the origin, and asking twice would record a second
        // one part way along — the row would jump by however far the fingers
        // had already travelled.
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        let mut claims = 0;
        for n in 0..20 {
            if s.takes_hold(-5.0, 0.0, tick(n)) == Some(Hold::Horizontal) {
                claims += 1;
                s.drives(2.0);
            }
        }
        assert_eq!(claims, 1);
        assert!((s.position().unwrap() - (2.0 + 100.0 / 180.0)).abs() < 0.001);
    }

    #[test]
    fn a_swipe_out_and_back_lands_exactly_where_it_started() {
        // The property that comes of computing from total travel rather than
        // accumulating rounded steps: no drift over a long gesture.
        let mut out: Vec<(f64, f64)> = (0..40).map(|_| (-7.3, 0.0)).collect();
        out.extend((0..40).map(|_| (7.3, 0.0)));
        assert_eq!(swipe(3.0, &out).position(), Some(3.0));
    }

    #[test]
    fn vertical_direction_is_decided_from_the_fingers_motion() {
        assert_eq!(swipe(0.0, &[(0.0, 20.0)]).vertical(), Some(Vertical::Down));
        assert_eq!(swipe(0.0, &[(0.0, -20.0)]).vertical(), Some(Vertical::Up));
    }

    /// Feed `travel` to a fresh three-finger swipe one step at a time, taking
    /// it as the reveal drag from `origin` the moment it proves vertical.
    fn reveal_swipe(origin: f32, travel: &[(f64, f64)]) -> Swipe {
        let mut s = Swipe::new(CAROUSEL_FINGERS);
        for (n, (dx, dy)) in travel.iter().enumerate() {
            if s.takes_hold(*dx, *dy, tick(n)) == Some(Hold::Vertical) {
                s.drives_reveal(origin);
            }
        }
        s
    }

    /// Whether `s`, of `n` reports, commits when lifted with the fingers at
    /// rest — well after the last report, so no velocity is read.
    fn commits_at_rest(s: &Swipe) -> Option<bool> {
        s.reveal_commits(Duration::from_secs(10))
    }

    #[test]
    fn an_upward_swipe_drives_the_reveal_with_the_fingers() {
        let s = reveal_swipe(0.0, &[(0.0, -35.0), (1.0, -35.0)]);
        assert!((s.reveal().unwrap() - 0.5).abs() < 0.001);
        assert_eq!(commits_at_rest(&s), Some(true));
    }

    #[test]
    fn a_short_flick_up_still_opens() {
        // Committing must not require dragging past halfway: the direction
        // the swipe set out in is the intent.
        let s = reveal_swipe(0.0, &[(0.0, -30.0)]);
        assert_eq!(commits_at_rest(&s), Some(true));
    }

    #[test]
    fn an_up_swipe_that_comes_back_down_is_a_change_of_mind() {
        let s = reveal_swipe(0.0, &[(0.0, -80.0), (0.0, 75.0)]);
        assert_eq!(commits_at_rest(&s), Some(false));
    }

    #[test]
    fn a_downward_swipe_from_open_closes_the_overview() {
        let s = reveal_swipe(1.0, &[(0.0, 40.0)]);
        assert!(s.reveal().unwrap() < 1.0, "the reveal must follow the drag");
        assert_eq!(commits_at_rest(&s), Some(false));
    }

    #[test]
    fn a_down_swipe_that_comes_back_up_leaves_the_overview_open() {
        let s = reveal_swipe(1.0, &[(0.0, 60.0), (0.0, -55.0)]);
        assert_eq!(commits_at_rest(&s), Some(true));
    }

    #[test]
    fn a_down_swipe_on_a_nearly_closed_overview_still_closes_it() {
        // The origin sits below the return margin, so the naive threshold
        // would be negative and the clamped reveal could never land under it.
        let s = reveal_swipe(0.05, &[(0.0, 40.0)]);
        assert_eq!(commits_at_rest(&s), Some(false));
    }

    #[test]
    fn the_reveal_is_clamped_to_its_range() {
        assert_eq!(reveal_swipe(0.0, &[(0.0, -500.0)]).reveal(), Some(1.0));
        assert_eq!(reveal_swipe(1.0, &[(0.0, 500.0)]).reveal(), Some(0.0));
    }

    #[test]
    fn a_command_swipe_never_reports_a_reveal() {
        // A vertical swipe nothing took as the reveal stays a command — it
        // minimizes or accepts on the lift, and drives nothing meanwhile.
        let s = swipe(0.0, &[(0.0, 20.0)]);
        assert_eq!(s.reveal(), None);
        assert_eq!(commits_at_rest(&s), None);
        assert_eq!(s.vertical(), Some(Vertical::Down));
    }

    #[test]
    fn driving_the_reveal_silences_the_vertical_command() {
        let s = reveal_swipe(0.0, &[(0.0, -40.0)]);
        assert_eq!(s.vertical(), None, "the lift must not also minimize");
    }

    #[test]
    fn a_horizontal_swipe_cannot_be_taken_as_the_reveal() {
        let mut s = swipe(2.0, &[(-60.0, 0.0)]);
        s.drives_reveal(0.0);
        assert_eq!(s.reveal(), None);
        assert_eq!(s.position(), Some(2.0 + 60.0 / 180.0));
    }

    #[test]
    fn three_finger_double_tap_is_bounded_and_resets() {
        let mut taps = DoubleTap::default();
        assert!(!taps.tap(3, 100));
        assert!(taps.tap(3, 550));
        assert!(!taps.tap(3, 600), "the accepted pair must reset");
        assert!(!taps.tap(3, 1_200), "a late second tap starts a new pair");
        assert!(taps.tap(3, 1_400));
    }

    #[test]
    fn a_swipe_at_rest_has_no_velocity_and_lands_on_the_nearest_stage() {
        // Dragged four tenths of a stage and held still before lifting: a
        // placement, and the row goes back to where it was.
        let mut s = swipe(1.0, &[(-36.0, 0.0), (-36.0, 0.0)]);
        for n in 2..30 {
            s.takes_hold(0.0, 0.0, tick(n));
        }
        let v = s.velocity(lift(30)).unwrap();
        assert!(v.abs() < 0.01, "{v}");
        assert_eq!(s.landing(lift(30)), Some(1.0));
    }

    #[test]
    fn velocity_is_in_stages_per_second_the_way_the_row_moves() {
        // 20 units per 8ms leftwards is 2500 units/s, which advances the row
        // at 2500/180 stages per second.
        let travel: Vec<(f64, f64)> = (0..20).map(|_| (-20.0, 0.0)).collect();
        let s = swipe(0.0, &travel);
        let v = s.velocity(lift(travel.len())).unwrap();
        assert!((v - 2500.0 / 180.0).abs() < 0.2, "{v}");
        let back = swipe(
            2.0,
            &travel.iter().map(|(x, y)| (-x, *y)).collect::<Vec<_>>(),
        );
        assert!(back.velocity(lift(travel.len())).unwrap() < 0.0);
    }

    #[test]
    fn a_flick_lands_a_stage_over_though_the_fingers_barely_moved_it() {
        // A quarter of a stage of travel, fast: thrown, not dropped.
        let travel: Vec<(f64, f64)> = (0..5).map(|_| (-9.0, 0.0)).collect();
        let s = swipe(1.0, &travel);
        let at = s.position().unwrap();
        assert!(at < 1.3, "the fingers moved it {at}");
        assert_eq!(s.landing(lift(travel.len())), Some(2.0));
    }

    #[test]
    fn a_drag_past_halfway_flicked_back_goes_back() {
        // Velocity decides over position: the hand said "no" at the end.
        let mut travel: Vec<(f64, f64)> = (0..7).map(|_| (-20.0, 0.0)).collect();
        travel.extend((0..6).map(|_| (10.0, 0.0)));
        let s = swipe(0.0, &travel);
        assert!(s.position().unwrap() > 0.4, "{}", s.position().unwrap());
        assert_eq!(s.landing(lift(travel.len())), Some(0.0));
    }

    #[test]
    fn a_throw_carries_at_most_one_stage_past_the_fingers() {
        // However hard: the row is pages, not a scroll.
        let travel: Vec<(f64, f64)> = (0..6).map(|_| (-120.0, 0.0)).collect();
        let s = swipe(0.0, &travel);
        let at = s.position().unwrap();
        let landing = s.landing(lift(travel.len())).unwrap();
        assert!(landing <= (at + CARRY).round(), "{landing} from {at}");
        assert!(landing > at, "a hard throw must still go forwards");
    }

    #[test]
    fn a_pause_before_the_lift_forgets_the_speed() {
        // Fast, then held for a moment, then lifted: the hold is the intent.
        let travel: Vec<(f64, f64)> = (0..5).map(|_| (-9.0, 0.0)).collect();
        let s = swipe(1.0, &travel);
        let held = lift(travel.len()) + VELOCITY_STALE * 2;
        assert_eq!(s.velocity(held), Some(0.0));
        assert_eq!(s.landing(held), Some(1.0));
    }

    #[test]
    fn the_velocity_reads_the_end_of_the_swipe_not_its_average() {
        // Slow for a long while, then fast: the lift happens at the fast.
        let mut travel: Vec<(f64, f64)> = (0..40).map(|_| (-1.0, 0.0)).collect();
        travel.extend((0..6).map(|_| (-30.0, 0.0)));
        let s = swipe(0.0, &travel);
        let v = s.velocity(lift(travel.len())).unwrap();
        assert!(v > 15.0, "{v} is the average, not the end");
    }

    #[test]
    fn a_swipe_driving_nothing_has_no_velocity_and_no_landing() {
        let s = swipe(0.0, &[(1.0, 1.0)]);
        assert_eq!(s.velocity(lift(1)), None);
        assert_eq!(s.landing(lift(1)), None);
        let reveal = reveal_swipe(0.0, &[(0.0, -30.0)]);
        assert_eq!(reveal.landing(lift(1)), None, "the reveal has no stages");
    }

    #[test]
    fn the_reveal_reports_its_speed_upwards_as_opening() {
        let travel: Vec<(f64, f64)> = (0..20).map(|_| (0.0, -14.0)).collect();
        let s = reveal_swipe(0.0, &travel);
        let v = s.velocity(lift(travel.len())).unwrap();
        assert!((v - 14.0 / 0.008 / 140.0).abs() < 0.2, "{v}");
    }

    #[test]
    fn an_upward_drag_that_flicks_back_down_closes() {
        // Opened most of the way, then thrown shut at the end: the throw wins
        // over both the position and the direction it set out in.
        let mut travel: Vec<(f64, f64)> = (0..10).map(|_| (0.0, -14.0)).collect();
        travel.extend((0..6).map(|_| (0.0, 8.0)));
        let s = reveal_swipe(0.0, &travel);
        assert!(s.reveal().unwrap() > 0.5);
        assert_eq!(s.reveal_commits(lift(travel.len())), Some(false));
        // Held still before the lift, the same swipe is read the old way.
        assert_eq!(commits_at_rest(&s), Some(true));
    }

    #[test]
    fn a_creeping_lift_is_decided_by_direction_not_by_a_tiny_velocity() {
        // Slower than a flick: the speed is noise, and the direction rule
        // still holds.
        let travel: Vec<(f64, f64)> = (0..24).map(|_| (0.0, -1.0)).collect();
        let s = reveal_swipe(0.0, &travel);
        let v = s.velocity(lift(travel.len())).unwrap();
        assert!(v.abs() < REVEAL_FLICK, "{v}");
        assert_eq!(s.reveal_commits(lift(travel.len())), Some(true));
    }
}
