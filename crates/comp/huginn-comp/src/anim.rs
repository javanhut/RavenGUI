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
    /// watching, where the departure matters as much as the arrival.
    ///
    /// Unused so far — it is what the carousel's workspace-to-workspace motion
    /// wants, and that does not exist yet.
    #[allow(dead_code)]
    EaseInOut,
    /// Overshoots slightly and settles back. What makes a panel feel like an
    /// object with mass instead of a value being assigned.
    Spring,
}

impl Curve {
    /// Map linear progress `t` in 0..=1 to eased progress.
    ///
    /// May return values outside 0..=1 — [`Curve::Spring`] overshoots on
    /// purpose, and clamping that away is exactly what makes a spring look
    /// like a tween.
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
            // A decaying cosine: one visible overshoot, then rest. Written out
            // rather than integrated per frame so the value is a pure function
            // of time and a dropped frame cannot change where it ends up.
            Self::Spring => {
                if t >= 1.0 {
                    return 1.0;
                }
                const DECAY: f32 = 6.0;
                const FREQUENCY: f32 = 9.0;
                1.0 - (-DECAY * t).exp() * (FREQUENCY * t).cos()
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
    /// to arrive. Read by tests today; the carousel needs it to decide which
    /// workspace a fling is bound for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn target(&self) -> f32 {
        self.to
    }
}

/// How long the launcher takes to appear. §4: "~150ms, ease-out".
pub(crate) const LAUNCHER_OPEN: Duration = Duration::from_millis(150);

/// How long the carousel takes to slide one focus step.
///
/// Short, because it runs on every focus change rather than on a deliberate
/// open: a slide long enough to admire is one you wait through each time you
/// move between panes. It exists to show *which way* the strip went — a jump
/// leaves you to work out whether the pane you wanted arrived from the left or
/// the right — not to be watched.
pub(crate) const CAROUSEL_SLIDE: Duration = Duration::from_millis(170);

/// Shrink the desktop into the workspace switcher quickly enough to stay
/// attached to the gesture that summoned it.
pub(crate) const WORKSPACE_CAROUSEL_OPEN: Duration = Duration::from_millis(150);

/// Snap to the selected workspace and expand it back to the output.
pub(crate) const WORKSPACE_CAROUSEL_CLOSE: Duration = Duration::from_millis(220);

/// How long a panel takes to slide away. Shorter than opening: dismissing is
/// an instruction that has already been given, and waiting for it to finish is
/// waiting for nothing.
pub(crate) const PANEL_CLOSE: Duration = Duration::from_millis(110);

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
    fn every_curve_starts_at_zero_and_ends_at_one() {
        // A curve that does not is a value that jumps at one end or never
        // arrives at the other.
        for curve in [
            Curve::Linear,
            Curve::EaseOut,
            Curve::EaseInOut,
            Curve::Spring,
        ] {
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
    fn the_spring_actually_overshoots() {
        // A spring that never passes its target is a tween with extra
        // arithmetic. Clamping the curve to 0..=1 is the usual way to lose it.
        let peak = (0..100)
            .map(|i| Curve::Spring.ease(i as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.0, "the spring never overshot; peak was {peak}");
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
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::Spring);
        assert!((value.value(at(10_000)) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn the_target_is_readable_before_it_arrives() {
        // The compositor asks "is this opening or closing?" without waiting.
        let mut value = Animated::settled(0.0);
        value.animate_to(1.0, Duration::ZERO, SECOND, Curve::EaseOut);
        assert_eq!(value.target(), 1.0);
    }
}
