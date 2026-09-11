//! Animation: values that move over time, and the curves they move along.
//!
//! Deliberately free of Wayland, of the renderer, and of the clock itself —
//! every function takes the current time as an argument. That is what makes a
//! spring's settling behaviour testable at all: driving one from a real clock
//! means observing it at whatever moments the test happens to run, which is not
//! a test of the curve.
//!
//! # How it drives the frame loop
//!
//! Nothing here schedules anything. The compositor asks [`Animated::settled`]
//! whether a value has stopped moving and keeps requesting frames while any has
//! not. An idle desktop animates nothing and therefore renders nothing, which
//! is the property that has to survive adding motion to it.

use std::time::Duration;

/// How a value travels from one number to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Curve {
    /// Constant speed. Almost never right for something a person watches —
    /// real objects do not start and stop instantly — but it is the honest
    /// choice for a progress bar, where the rate *is* the information.
    Linear,
    /// Fast to start, easing into rest. The default for anything appearing:
    /// the motion is mostly over by the time the eye has found it, so the
    /// interface feels like it responded rather than like it played.
    EaseOut,
    /// Slow at both ends. For something moving between two places the user is
    /// watching, where the departure matters as much as the arrival — and for
    /// a window on its way out, which should leave the way it arrived rather
    /// than vanish with a jolt.
    EaseInOut,
}

impl Curve {
    /// Map linear progress `t` in 0..=1 to eased progress.
    ///
    /// Anything that should overshoot is a [`Spring`], not a curve: a curve
    /// has no velocity to carry through a change of mind.
    fn ease(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            // Cubic. Quadratic is too gentle to read as a response at the
            // durations an interface uses; quartic and beyond stops looking
            // like deceleration and starts looking like a stutter.
            Self::EaseOut => 1.0 - (1.0 - t).powi(3),
            Self::EaseInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// A number on its way somewhere.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Animated {
    from: f32,
    to: f32,
    started: Duration,
    duration: Duration,
    curve: Curve,
}

impl Animated {
    /// A value sitting still at `value`.
    pub(crate) fn settled(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            started: Duration::ZERO,
            duration: Duration::ZERO,
            curve: Curve::Linear,
        }
    }

    /// Send it to `target`, starting from wherever it is right now.
    ///
    /// From *now*, not from the previous start: retargeting mid-flight is the
    /// common case — a panel told to close while it is still opening — and
    /// restarting from the original `from` would snap it backwards before it
    /// began moving the other way.
    pub(crate) fn animate_to(
        &mut self,
        target: f32,
        now: Duration,
        duration: Duration,
        curve: Curve,
    ) {
        let current = self.value(now);
        // Already going there from here: leave it alone rather than restarting
        // the clock, which would stall a value that is asked for the same
        // target every frame.
        if (self.to - target).abs() < f32::EPSILON && !self.is_settled(now) {
            return;
        }
        *self = Self {
            from: current,
            to: target,
            started: now,
            duration,
            curve,
        };
    }

    /// Put it at `target` immediately, with no motion.
    pub(crate) fn jump_to(&mut self, target: f32) {
        *self = Self::settled(target);
    }

    /// Where it is at `now`.
    pub(crate) fn value(&self, now: Duration) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        // A clock that went backwards must read as "not started" rather than
        // producing a negative progress and running the curve in reverse.
        let elapsed = now.saturating_sub(self.started);
        let t = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * self.curve.ease(t)
    }

    /// Whether it has stopped moving. While false, the frame loop keeps going.
    pub(crate) fn is_settled(&self, now: Duration) -> bool {
        self.duration.is_zero() || now.saturating_sub(self.started) >= self.duration
    }

    /// Where it is heading.
    ///
    /// Lets a caller ask "is this opening or closing?" without waiting for it
    /// to arrive: the carousel needs it to decide which workspace a fling is
    /// bound for, and the found pointer to know whether it is still growing.
    pub(crate) fn target(&self) -> f32 {
        self.to
    }
}

/// A value pulled towards its target by a spring.
///
/// Where [`Animated`] is a curve played over a fixed duration, this is a
/// physical model: it has a velocity, and when it is retargeted mid-flight
/// it *keeps* that velocity. A tile sent somewhere else while still moving
/// bends towards the new place; a curve restarted from its current position
/// would stop dead and set off again, and the eye catches the kink.
///
/// The same velocity is how a gesture hands over to the animation. Fingers
/// lifting off a row they were dragging leave it moving at their speed, and
/// [`Spring::launch`] starts the settle from that speed rather than from
/// rest, so there is no seam where the hand stopped and the spring began.
///
/// Critically damped by default — no overshoot — because most of what it
/// moves is window rectangles, and a rectangle that overshoots is a window
/// briefly drawn larger than its pane. [`Spring::set_damping`] lowers the
/// damping ratio for the one case where a little overshoot is right: something that
/// was *thrown*, where landing dead would look like it hit a wall. Written
/// in closed form rather than integrated per frame, so the value is a pure
/// function of time and a dropped frame cannot change where it ends up.
///
/// With damping ratio 1 the motion is `target + (x0 + c2·t)·e^(−ω·t)`, where
/// `ω = √stiffness`, `x0` is the starting offset and `c2 = v0 + ω·x0`. Below
/// 1 it is `target + e^(−ζω·t)·(x0·cos(ωd·t) + b·sin(ωd·t))`, with
/// `ωd = ω·√(1−ζ²)` and `b = (v0 + ζω·x0)/ωd`: the same decay, wrapped
/// around one slowing oscillation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spring {
    target: f32,
    /// Offset from the target when the current flight began.
    x0: f32,
    /// Velocity when the current flight began.
    v0: f32,
    omega: f32,
    /// Damping ratio: 1 is critically damped, below 1 overshoots.
    zeta: f32,
    /// How close, in value and in velocity, counts as arrived.
    tolerance: f32,
    started: Duration,
}

impl Spring {
    /// The default tolerance: a tenth of a pixel. Closer than that and the
    /// rounding to a pixel has already stopped changing.
    const EPSILON: f32 = 0.1;

    /// The least damping allowed. Zero would ring forever; this settles in
    /// a handful of swings, and nothing on a desktop should swing longer.
    const LEAST_DAMPING: f32 = 0.2;

    /// At rest at `value`, with a spring of `stiffness` for when it moves.
    pub(crate) fn at_rest(value: f32, stiffness: f32) -> Self {
        Self {
            target: value,
            x0: 0.0,
            v0: 0.0,
            omega: stiffness.max(f32::EPSILON).sqrt(),
            zeta: 1.0,
            tolerance: Self::EPSILON,
            started: Duration::ZERO,
        }
    }

    /// The same spring counting itself arrived within `tolerance` of its
    /// target, for a value not measured in pixels — a row position in whole
    /// stages, say, where a tenth would be a tenth of the screen.
    pub(crate) fn with_tolerance(mut self, tolerance: f32) -> Self {
        self.tolerance = tolerance.max(f32::EPSILON);
        self
    }

    /// Set the damping ratio: 1 for no overshoot, less for some. Apple lands
    /// a thrown drawer at 0.8 — one soft overshoot.
    ///
    /// Safe mid-flight: the motion carries on from where it is and how fast
    /// it is going, on the new curve, with no jump. For a row that was
    /// flicked, and so lands with a bounce, being sent somewhere by a key
    /// before it has landed: a key is not a throw, and the key's motion
    /// should not bounce because the last gesture did.
    pub(crate) fn set_damping(&mut self, zeta: f32, now: Duration) {
        let (value, velocity) = self.state(now);
        self.anchor(value, velocity, self.target, now);
        self.zeta = zeta.clamp(Self::LEAST_DAMPING, 1.0);
    }

    /// Pull towards `target` from wherever it is now, keeping its velocity.
    pub(crate) fn pull_to(&mut self, target: f32, now: Duration) {
        let (value, velocity) = self.state(now);
        self.anchor(value, velocity, target, now);
    }

    /// Pull towards `target`, or be there already when `instant`.
    ///
    /// `instant` is reduced motion. Every spring the desktop drives honours
    /// it through this one method, so no call site needs an `if reduced`.
    pub(crate) fn go_to(&mut self, target: f32, now: Duration, instant: bool) {
        if instant {
            self.jump_to(target);
        } else {
            self.pull_to(target, now);
        }
    }

    /// Set off from where it is at `velocity` per second, towards the
    /// target it already has.
    ///
    /// The handover from a gesture: the fingers lift, the value keeps their
    /// speed, and the pull that follows — [`Self::pull_to`] keeps velocity —
    /// starts from that speed. Called with the speed the fingers had, not
    /// the speed the spring thinks it has, which while the fingers were
    /// driving it was nothing at all.
    pub(crate) fn launch(&mut self, velocity: f32, now: Duration) {
        let value = self.state(now).0;
        self.anchor(value, velocity, self.target, now);
    }

    /// Put it at `target` immediately, at rest. What reduced motion does.
    pub(crate) fn jump_to(&mut self, target: f32) {
        self.target = target;
        self.x0 = 0.0;
        self.v0 = 0.0;
    }

    /// Move the value and its target together, without disturbing the
    /// motion — the window was carried, not sent somewhere.
    pub(crate) fn shift(&mut self, by: f32) {
        self.target += by;
    }

    /// Begin a flight at `now` from `value` moving at `velocity`, bound for
    /// `target`.
    fn anchor(&mut self, value: f32, velocity: f32, target: f32, now: Duration) {
        self.x0 = value - target;
        self.v0 = velocity;
        self.target = target;
        self.started = now;
    }

    fn state(&self, now: Duration) -> (f32, f32) {
        let t = now.saturating_sub(self.started).as_secs_f32();
        let (w, z) = (self.omega, self.zeta);
        if z >= 1.0 {
            let decay = (-w * t).exp();
            let c2 = self.v0 + w * self.x0;
            let value = self.target + (self.x0 + c2 * t) * decay;
            let velocity = (c2 - w * (self.x0 + c2 * t)) * decay;
            (value, velocity)
        } else {
            let wd = w * (1.0 - z * z).sqrt();
            let decay = (-z * w * t).exp();
            let (a, b) = (self.x0, (self.v0 + z * w * self.x0) / wd);
            let (cos, sin) = ((wd * t).cos(), (wd * t).sin());
            let value = self.target + decay * (a * cos + b * sin);
            let velocity = decay * ((wd * b - z * w * a) * cos - (wd * a + z * w * b) * sin);
            (value, velocity)
        }
    }

    /// Where it is at `now`.
    pub(crate) fn value(&self, now: Duration) -> f32 {
        if self.is_settled(now) {
            return self.target;
        }
        self.state(now).0
    }

    /// Where it is heading.
    pub(crate) fn target(&self) -> f32 {
        self.target
    }

    /// Whether it has come to rest. Both the offset and the velocity have to
    /// be negligible: a spring passing through its target at speed is not
    /// there yet. Negligible velocity is measured against the spring's own
    /// pace — slow enough that, left alone, it could not carry the value
    /// more than the tolerance from here — since a velocity per second means
    /// something different to a spring that settles in a tenth of one.
    pub(crate) fn is_settled(&self, now: Duration) -> bool {
        if self.x0 == 0.0 && self.v0 == 0.0 {
            return true;
        }
        let (value, velocity) = self.state(now);
        (value - self.target).abs() < self.tolerance && velocity.abs() < self.tolerance * self.omega
    }
}

/// How far something moving at `velocity` per second would coast before
/// stopping, if it slowed by a factor of `rate` every millisecond.
///
/// The scroll-view sum: each millisecond covers `rate` times what the last
/// one did, and the series adds up to `v·rate/(1−rate)`. It is what turns a
/// flick into a landing place — the row settles on the stage nearest where
/// the fingers were *sending* it, not nearest where they happened to let go,
/// which is the difference between a throw and a drop.
pub(crate) fn project(velocity: f32, rate: f32) -> f32 {
    (velocity / 1000.0) * rate / (1.0 - rate)
}

/// The deceleration for something that snaps to whole stages: quick, so a
/// flick carries about a tenth of its speed in distance, enough to tip the
/// landing to the next stage without skipping the ones after. Apple's
/// "fast" rate; the ordinary scrolling one, 0.998, coasts five times as far.
pub(crate) const PAGED_DECELERATION: f32 = 0.99;

/// How far past a boundary a value dragged `overshoot` beyond it should
/// actually be drawn. Never as far as `give`.
///
/// A hard stop at the end of a row reads as frozen; giving a little, and
/// less the further it is pulled, reads as "responsive, and there is nothing
/// more here". Apple's rubber band: `x·d·c/(d + c·|x|)`, with `c` the
/// stiffness of the band.
pub(crate) fn rubberband(overshoot: f32, give: f32) -> f32 {
    const BAND: f32 = 0.55;
    (overshoot * give * BAND) / (give + BAND * overshoot.abs())
}

/// `value` held within `min..=max` by a rubber band that gives at most
/// `give` beyond either end. Inside the range it is left exactly alone.
pub(crate) fn rubberband_within(value: f32, min: f32, max: f32, give: f32) -> f32 {
    if value < min {
        min + rubberband(value - min, give)
    } else if value > max {
        max + rubberband(value - max, give)
    } else {
        value
    }
}

/// How a panel comes and goes: 0 is hidden, 1 is shown, and a spring is
/// what moves between them.
///
/// One type for the launcher, quick settings, the pinned panel and the dock,
/// so a panel is a panel: they all arrive and leave the same way, and a
/// change to the way is a change here. A spring rather than a curve so that
/// a panel dismissed while still arriving turns round with the speed it
/// has, instead of stopping and setting off again — the eye catches that
/// kink on something as large as a panel — and so that a panel summoned back
/// while still leaving does the same.
///
/// Critically damped. A panel summoned by a key or a pointer at an edge was
/// not thrown, and an overshoot on something that was not thrown reads as a
/// wobble; the dock and quick settings used to bounce on the way up, and no
/// longer do. What makes a panel feel like an object is that it turns round
/// smoothly, not that it jiggles on arrival.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reveal {
    spring: Spring,
}

impl Reveal {
    /// How hard a panel is pulled. Visibly there — within a percent of its
    /// place and size — 150ms after it is asked for, §4's figure for the
    /// launcher, and settled to the last thousandth some 60ms after that.
    const STIFFNESS: f32 = 2000.0;

    /// How close to shown or hidden counts as there: a thousandth, which on
    /// the largest panel there is comes to under a pixel of scale.
    const TOLERANCE: f32 = 0.001;

    /// Hidden, and still.
    pub(crate) fn hidden() -> Self {
        Self {
            spring: Spring::at_rest(0.0, Self::STIFFNESS).with_tolerance(Self::TOLERANCE),
        }
    }

    /// Bring it on, from wherever it is; at once when `instant`.
    pub(crate) fn open(&mut self, now: Duration, instant: bool) {
        self.spring.go_to(1.0, now, instant);
    }

    /// Take it away, from wherever it is; at once when `instant`.
    pub(crate) fn close(&mut self, now: Duration, instant: bool) {
        self.spring.go_to(0.0, now, instant);
    }

    /// Fully shown, now, with no motion.
    pub(crate) fn show_now(&mut self) {
        self.spring.jump_to(1.0);
    }

    /// Fully hidden, now, with no motion.
    pub(crate) fn hide_now(&mut self) {
        self.spring.jump_to(0.0);
    }

    /// How far shown it is at `now`, 0..=1.
    pub(crate) fn value(&self, now: Duration) -> f32 {
        self.spring.value(now).clamp(0.0, 1.0)
    }

    /// Whether it is on its way in rather than out, wherever it is.
    pub(crate) fn is_showing(&self) -> bool {
        self.spring.target() > 0.5
    }

    /// Whether it has stopped moving.
    pub(crate) fn is_settled(&self, now: Duration) -> bool {
        self.spring.is_settled(now)
    }
}

/// How long the carousel takes to slide one focus step.
///
/// Short, because it runs on every focus change rather than on a deliberate
/// open: a slide long enough to admire is one you wait through each time you
/// move between panes. It exists to show *which way* the strip went — a jump
/// leaves you to work out whether the pane you wanted arrived from the left or
/// the right — not to be watched.
pub(crate) const CAROUSEL_SLIDE: Duration = Duration::from_millis(170);

/// How long the volume slider takes to fade once its hold is up. Shorter
/// than its arrival: leaving is not information, and a slider that lingers
/// on its way out is one that is in the way of what the volume was changed
/// for.
pub(crate) const VOLUME_FADE: Duration = Duration::from_millis(110);

/// How long a window takes to appear once it has drawn its first frame: a
/// fade in and a slight growth into its pane. §5: "~150ms, ease-out". Short
/// enough that a window opened by a keystroke is there by the time the eye
/// looks for it, long enough that it arrived from somewhere rather than
/// being switched on.
pub(crate) const WINDOW_OPEN: Duration = Duration::from_millis(150);

/// How long a window takes to leave. Shorter than opening, as with panels:
/// closing is an instruction already given, and the desktop reflows around
/// the gap at the same time, so a long farewell would be a long overlap.
pub(crate) const WINDOW_CLOSE: Duration = Duration::from_millis(120);

/// How long the focus ring stays fully visible after focus moves. Long enough
/// to be seen without looking for it, short enough that it is gone before the
/// eye has settled on the window it marked.
pub(crate) const FOCUS_RING_HOLD: Duration = Duration::from_millis(700);

/// How long the focus ring takes to fade out after its hold.
pub(crate) const FOCUS_RING_FADE: Duration = Duration::from_millis(250);

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn ease_in_out_starts_and_ends_at_rest() {
        assert_eq!(Curve::EaseInOut.ease(0.0), 0.0);
        assert_eq!(Curve::EaseInOut.ease(1.0), 1.0);
        assert!((Curve::EaseInOut.ease(0.5) - 0.5).abs() < 1e-6);
        // Slow at the start: the first tenth covers far less than a tenth.
        assert!(Curve::EaseInOut.ease(0.1) < 0.05);
        // And slow at the end, symmetrically.
        assert!(Curve::EaseInOut.ease(0.9) > 0.95);
    }

    #[test]
    fn every_curve_starts_at_zero_and_ends_at_one() {
        // A curve that does not is a value that jumps at one end or never
        // arrives at the other.
        for curve in [Curve::Linear, Curve::EaseOut, Curve::EaseInOut] {
            assert!(
                curve.ease(0.0).abs() < 1e-5,
                "{curve:?} does not start at 0"
            );
            assert!(
                (curve.ease(1.0) - 1.0).abs() < 1e-5,
                "{curve:?} does not end at 1"
            );
        }
    }

    #[test]
    fn ease_out_is_fast_first() {
        // The property that makes it feel like a response: most of the
        // distance is covered in the first half.
        assert!(
            Curve::EaseOut.ease(0.5) > 0.75,
            "only {} covered by halfway",
            Curve::EaseOut.ease(0.5)
        );
    }

    #[test]
    fn a_settled_value_stays_where_it_is() {
        let value = Animated::settled(0.5);
        assert_eq!(value.value(Duration::ZERO), 0.5);
        assert_eq!(value.value(at(10_000)), 0.5);
        assert!(value.is_settled(Duration::ZERO));
    }

    #[test]
    fn an_animation_travels_from_one_end_to_the_other() {
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::Linear);
        assert!(value.value(Duration::ZERO).abs() < 1e-5);
        assert!((value.value(at(500)) - 0.5).abs() < 1e-3);
        assert!((value.value(SECOND) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn it_reports_settled_only_once_it_has_arrived() {
        // This is what the frame loop asks; getting it wrong either stops the
        // animation early or renders forever.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::EaseOut);
        assert!(!value.is_settled(at(999)));
        assert!(value.is_settled(SECOND));
        assert!(value.is_settled(at(5_000)));
    }

    #[test]
    fn retargeting_midway_continues_from_where_it_is() {
        // A panel told to close while still opening must reverse from its
        // current position. Restarting from the original `from` snaps it back
        // to the start before it begins moving the other way, which reads as a
        // glitch rather than as a change of mind.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::Linear);
        let midway = value.value(at(500));
        assert!((midway - 0.5).abs() < 1e-3);

        value.animate_to(0.0, at(500), SECOND, Curve::Linear);
        assert!(
            (value.value(at(500)) - midway).abs() < 1e-5,
            "it jumped to {} instead of continuing from {midway}",
            value.value(at(500))
        );
        assert!(value.value(at(750)) < midway, "it did not reverse");
    }

    #[test]
    fn asking_for_the_same_target_again_does_not_restart_the_clock() {
        // Called once per frame with the same target, a restart would leave
        // the value permanently at the beginning of its curve.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::Linear);
        for frame in 0..10 {
            value.animate_to(1.0, at(frame * 50), SECOND, Curve::Linear);
        }
        assert!(
            value.value(at(500)) > 0.4,
            "the animation stalled at {}",
            value.value(at(500))
        );
    }

    #[test]
    fn jumping_skips_the_motion_entirely() {
        // What "reduced motion" turns every animation into.
        let mut value = Animated::settled(0.0);
        value.jump_to(1.0);
        assert_eq!(value.value(Duration::ZERO), 1.0);
        assert!(value.is_settled(Duration::ZERO));
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_run_the_curve_in_reverse() {
        // Suspend, an NTP step, a monotonic clock that is not.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, at(1_000), SECOND, Curve::Linear);
        let before = value.value(at(500));
        assert!((0.0..=1.0).contains(&before), "value went to {before}");
    }

    #[test]
    fn a_zero_length_animation_is_already_over() {
        // Otherwise it divides by zero working out progress.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, Duration::ZERO, Curve::EaseOut);
        assert_eq!(value.value(Duration::ZERO), 1.0);
        assert!(value.is_settled(Duration::ZERO));
    }

    #[test]
    fn progress_never_leaves_the_curve_outside_its_domain() {
        // Time past the end must hold at the target rather than extrapolating.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::EaseOut);
        assert!((value.value(at(10_000)) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_spring_arrives_without_overshooting() {
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.pull_to(100.0, Duration::ZERO);
        let mut last = 0.0;
        for ms in (0..1_000).step_by(5) {
            let value = spring.value(at(ms));
            assert!(
                value >= last - 1e-3,
                "went backwards at {ms}ms: {last} -> {value}"
            );
            assert!(value <= 100.0 + 1e-3, "overshot at {ms}ms: {value}");
            last = value;
        }
        assert!(spring.is_settled(SECOND));
        assert_eq!(spring.value(SECOND), 100.0);
    }

    #[test]
    fn a_spring_of_niris_stiffness_settles_in_a_few_hundred_milliseconds() {
        // Stiffness 800 is what niri moves windows with; it should be over
        // by the time an ease-out of the same purpose would be, not linger.
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.pull_to(1_000.0, Duration::ZERO);
        assert!(!spring.is_settled(at(100)));
        assert!(spring.is_settled(at(600)), "still moving at 600ms");
    }

    #[test]
    fn retargeting_a_spring_keeps_its_velocity() {
        // The reason for a spring at all. Sent back the way it came while
        // moving fast, it must carry on past the retarget point before
        // turning, rather than stopping dead.
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.pull_to(100.0, Duration::ZERO);
        let midway = spring.value(at(20));
        spring.pull_to(0.0, at(20));
        assert!((spring.value(at(20)) - midway).abs() < 1e-3, "it jumped");
        assert!(
            spring.value(at(25)) > midway,
            "it stopped dead instead of carrying its momentum"
        );
        assert!(spring.value(at(300)) < midway, "it never turned back");
    }

    #[test]
    fn shifting_a_spring_moves_it_without_restarting() {
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.pull_to(100.0, Duration::ZERO);
        let before = spring.value(at(30));
        spring.shift(-40.0);
        assert!((spring.value(at(30)) - (before - 40.0)).abs() < 1e-3);
        assert_eq!(spring.target(), 60.0);
    }

    #[test]
    fn a_spring_that_jumps_is_at_rest() {
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.jump_to(50.0);
        assert!(spring.is_settled(Duration::ZERO));
        assert_eq!(spring.value(Duration::ZERO), 50.0);
    }

    #[test]
    fn the_target_is_readable_before_it_arrives() {
        // The compositor asks "is this opening or closing?" without waiting.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::EaseOut);
        assert_eq!(value.target(), 1.0);
    }

    #[test]
    fn a_panel_is_there_within_a_percent_in_a_hundred_and_fifty_milliseconds() {
        // §4's figure for the launcher, kept as the spring's stiffness.
        let mut reveal = Reveal::hidden();
        reveal.open(Duration::ZERO, false);
        assert!(reveal.value(Duration::ZERO) < 0.01, "it was already there");
        assert!(reveal.is_showing());
        assert!(
            reveal.value(at(150)) > 0.99,
            "{} at 150ms",
            reveal.value(at(150))
        );
        assert!(
            !reveal.is_settled(at(150)),
            "there is a last thousandth to go"
        );
        assert!(reveal.is_settled(at(300)));
        assert_eq!(reveal.value(at(300)), 1.0);
    }

    #[test]
    fn a_panel_dismissed_while_arriving_turns_round_without_a_kink() {
        // The reason for a spring at all: value and velocity are continuous
        // through the change of mind.
        let mut reveal = Reveal::hidden();
        reveal.open(Duration::ZERO, false);
        let midway = reveal.value(at(30));
        reveal.close(at(30), false);
        assert!(!reveal.is_showing());
        assert!((reveal.value(at(30)) - midway).abs() < 1e-4, "it jumped");
        assert!(
            reveal.value(at(32)) > midway,
            "it stopped dead instead of carrying its momentum"
        );
        assert!(reveal.value(at(300)) < midway, "it never turned back");
        assert!(reveal.is_settled(SECOND));
        assert_eq!(reveal.value(SECOND), 0.0);
    }

    #[test]
    fn a_panel_never_overshoots_its_place() {
        // Not thrown, so no bounce: a panel drawn larger than itself for a
        // frame is a panel that wobbled.
        let mut reveal = Reveal::hidden();
        reveal.open(Duration::ZERO, false);
        for ms in (0..500).step_by(2) {
            assert!(reveal.value(at(ms)) <= 1.0, "overshot at {ms}ms");
        }
    }

    #[test]
    fn a_panel_under_reduced_motion_is_simply_there() {
        let mut reveal = Reveal::hidden();
        reveal.open(at(10), true);
        assert_eq!(reveal.value(at(10)), 1.0);
        assert!(reveal.is_settled(at(10)));
        reveal.close(at(20), true);
        assert_eq!(reveal.value(at(20)), 0.0);
        reveal.show_now();
        assert_eq!(reveal.value(at(20)), 1.0);
        reveal.hide_now();
        assert_eq!(reveal.value(at(20)), 0.0);
        assert!(!reveal.is_showing());
    }

    #[test]
    fn a_damped_spring_overshoots_once_and_then_settles() {
        // What a thrown thing does: past the mark, back, and still.
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.set_damping(0.8, Duration::ZERO);
        spring.pull_to(100.0, Duration::ZERO);
        let peak = (0..1_000)
            .step_by(2)
            .map(|ms| spring.value(at(ms)))
            .fold(0.0_f32, f32::max);
        assert!(peak > 100.5, "it never overshot; peak was {peak}");
        assert!(peak < 110.0, "it overshot too far: {peak}");
        assert!(spring.is_settled(SECOND));
        assert_eq!(spring.value(SECOND), 100.0);
    }

    #[test]
    fn a_launched_spring_sets_off_at_the_speed_it_was_given() {
        // The handover from fingers: the row keeps moving the way they were
        // moving, at their speed, before the pull turns it.
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.launch(-500.0, Duration::ZERO);
        assert!(spring.value(at(10)) < -3.0, "it did not carry the speed");
        assert!(spring.value(SECOND).abs() < 0.1, "it never came back");
        // And a pull straight after keeps that speed: the throw and the
        // destination are decided at the same instant, in either order.
        let mut thrown = Spring::at_rest(0.0, 800.0);
        thrown.launch(-500.0, Duration::ZERO);
        thrown.pull_to(100.0, Duration::ZERO);
        assert!(thrown.value(at(10)) < 0.0, "the pull threw the speed away");
        assert!((thrown.value(SECOND) - 100.0).abs() < 0.1);
    }

    #[test]
    fn changing_the_damping_mid_flight_does_not_jump() {
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.set_damping(0.6, Duration::ZERO);
        spring.pull_to(100.0, Duration::ZERO);
        let before = spring.state(at(30));
        spring.set_damping(1.0, at(30));
        let after = spring.state(at(30));
        assert!((after.0 - before.0).abs() < 1e-3, "the value jumped");
        assert!((after.1 - before.1).abs() < 1e-2, "the velocity jumped");
        // From here it is critically damped: no overshoot past 100.
        let peak = (30..1_000)
            .step_by(2)
            .map(|ms| spring.value(at(ms)))
            .fold(0.0_f32, f32::max);
        assert!(peak <= 100.0 + 1e-2, "it still overshot: {peak}");
    }

    #[test]
    fn the_damping_is_kept_off_the_floor() {
        // Damping of zero would ring forever, and the frame loop with it.
        let mut spring = Spring::at_rest(0.0, 800.0);
        spring.set_damping(0.0, Duration::ZERO);
        spring.pull_to(100.0, Duration::ZERO);
        assert!(spring.is_settled(at(5_000)), "it rang for five seconds");
    }

    #[test]
    fn the_tolerance_is_in_the_value_s_own_units() {
        // A row position in stages: a tenth of a stage is a tenth of the
        // screen, so the pixel tolerance would call it settled while it was
        // visibly still moving.
        let mut coarse = Spring::at_rest(0.0, 700.0);
        let mut fine = Spring::at_rest(0.0, 700.0).with_tolerance(0.001);
        coarse.pull_to(1.0, Duration::ZERO);
        fine.pull_to(1.0, Duration::ZERO);
        let coarse_done = (0..2_000).find(|&ms| coarse.is_settled(at(ms))).unwrap();
        let fine_done = (0..2_000).find(|&ms| fine.is_settled(at(ms))).unwrap();
        assert!(
            fine_done > coarse_done,
            "{fine_done}ms is not later than {coarse_done}ms"
        );
        assert!(fine_done < 600, "still moving at {fine_done}ms");
    }

    #[test]
    fn projection_carries_a_flick_a_short_way_and_a_drop_nowhere() {
        assert_eq!(project(0.0, PAGED_DECELERATION), 0.0);
        let flick = project(3.0, PAGED_DECELERATION);
        assert!((flick - 0.297).abs() < 1e-3, "{flick}");
        assert_eq!(project(-3.0, PAGED_DECELERATION), -flick);
        // The ordinary scrolling rate coasts far further from the same speed.
        assert!(project(3.0, 0.998) > flick * 4.0);
    }

    #[test]
    fn the_rubber_band_gives_less_the_further_it_is_pulled_and_never_past_its_give() {
        assert_eq!(rubberband(0.0, 0.3), 0.0);
        let mut last = 0.0;
        for step in 1..200 {
            let pulled = rubberband(step as f32 * 0.05, 0.3);
            assert!(pulled > last, "it went backwards at {step}");
            assert!(pulled < 0.3, "it gave more than its give: {pulled}");
            assert!(pulled < step as f32 * 0.05, "it gave more than the pull");
            last = pulled;
        }
        assert!(
            (rubberband(-1.0, 0.3) + rubberband(1.0, 0.3)).abs() < 1e-6,
            "not symmetric"
        );
    }

    #[test]
    fn the_band_holds_a_row_within_its_ends_and_leaves_the_middle_alone() {
        assert_eq!(rubberband_within(1.3, 0.0, 2.0, 0.3), 1.3);
        assert_eq!(rubberband_within(0.0, 0.0, 2.0, 0.3), 0.0);
        assert_eq!(rubberband_within(2.0, 0.0, 2.0, 0.3), 2.0);
        let past = rubberband_within(3.0, 0.0, 2.0, 0.3);
        assert!(past > 2.0 && past < 2.3, "{past}");
        let before = rubberband_within(-1.0, 0.0, 2.0, 0.3);
        assert!(before < 0.0 && before > -0.3, "{before}");
    }
}
