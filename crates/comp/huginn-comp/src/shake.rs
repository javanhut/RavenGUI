//! Shake to find: a pointer waggled back and forth grows until it is found.
//!
//! A pointer lost on a big screen is found by moving it, and the instinct
//! is to move it fast and back and forth. That motion is the request: the
//! cursor swells to several times its size, holds for long enough to be
//! spotted, and shrinks back once the shaking stops. Nothing reaches a
//! client — the pointer still moves, and clients still see it move — the
//! only thing that changes is how big the compositor draws it.
//!
//! # What a shake is
//!
//! A run of quick reversals. The pointer's travel since it last turned round
//! is the current *leg*; a delta that points against a leg long enough to be
//! deliberate ends it and starts the next. Enough turns inside a short
//! window is a shake. Distance alone is not enough — a slow wave across the
//! screen is a pointer being moved somewhere, not one being looked for — and
//! neither is speed: a flick to the far corner is fast and turns round never.
//!
//! The recogniser is axis-free. A shake is usually sideways, since that is
//! how a wrist moves, but an up-and-down one or a diagonal one reads the
//! same, and a fast scribble turns round twice a circle and counts too. That
//! is the right failure: someone scribbling is looking for the pointer.
//!
//! # Why this is a type and not a few lines in the input handler
//!
//! For the reason [`crate::gesture`] is: a shake is not one event but the
//! shape of the last several, and the accumulation is state that the input
//! handler could not test. Here it is a plain value fed deltas and
//! timestamps, with no compositor or clock in sight.

/// How far a leg must travel, in logical pixels, before turning round counts
/// as a reversal.
///
/// Long enough that the hand's tremor and the sensor's noise — a pixel or two
/// backwards in the middle of a straight move — never register, and short
/// enough that a small, fast shake of the wrist does.
const MIN_LEG: f64 = 24.0;

/// Reversals that make a shake. Four is two full back-and-forths, which is
/// the fewest that nobody does by accident: a single there-and-back is what
/// overshooting a button and correcting looks like.
const REVERSALS: usize = 4;

/// How recent the reversals must all be, in milliseconds. A shake is quick;
/// four turns spread over a second is a pointer being used, not waved.
const WINDOW_MSEC: u32 = 600;

/// The shake recogniser. One per pointer, fed every motion delta.
#[derive(Debug, Default)]
pub(crate) struct Shake {
    /// Travel since the last reversal.
    leg: (f64, f64),
    /// Travel against the leg since it seemed to turn round, while that is
    /// still too short to be sure it did. A turn is only a turn once the
    /// pointer has gone [`MIN_LEG`] back the other way: until then a pixel
    /// or two backwards is the hand's tremor in the middle of a leg, and
    /// counting it would make a straight, slightly wobbly move a shake.
    turn: Option<(f64, f64)>,
    /// When each recent reversal happened, oldest first. Never longer than
    /// [`REVERSALS`]: the count that fires also empties it.
    reversals: Vec<u32>,
}

impl Shake {
    /// The pointer moved by `(dx, dy)` at `time_msec`. `true` when this
    /// motion completed a shake.
    ///
    /// Fires once per shake, then starts counting afresh, so a pointer that
    /// keeps being waved keeps reporting shakes at intervals rather than on
    /// every frame — which is what lets a caller extend the enlarged pointer
    /// for as long as the shaking goes on.
    ///
    /// `time_msec` is the event's own clock, which wraps; differences are
    /// taken modulo so the wrap is not a stall.
    pub(crate) fn moved(&mut self, dx: f64, dy: f64, time_msec: u32) -> bool {
        let (lx, ly) = self.leg;
        let against = |x: f64, y: f64| x * dx + y * dy < 0.0;
        let long = |x: f64, y: f64| (x * x + y * y).sqrt() >= MIN_LEG;
        match self.turn {
            // Back the way the leg was going: the turn was a twitch, and the
            // travel is the leg's.
            Some((tx, ty)) if against(tx, ty) => {
                self.turn = None;
                self.leg = (lx + tx + dx, ly + ty + dy);
            }
            Some((tx, ty)) => self.turn = Some((tx + dx, ty + dy)),
            None if long(lx, ly) && against(lx, ly) => self.turn = Some((dx, dy)),
            None => self.leg = (lx + dx, ly + dy),
        }
        if let Some((tx, ty)) = self.turn
            && long(tx, ty)
        {
            self.reversals.push(time_msec);
            self.leg = (tx, ty);
            self.turn = None;
        }
        self.reversals
            .retain(|&at| time_msec.wrapping_sub(at) <= WINDOW_MSEC);
        if self.reversals.len() >= REVERSALS {
            self.reversals.clear();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `legs` — each a (dx, dy) travelled in one go, split into steps —
    /// `step_msec` apart, and say whether any step fired.
    fn shake(legs: &[(f64, f64)], step_msec: u32) -> bool {
        let mut shake = Shake::default();
        let mut time = 1000;
        let mut fired = false;
        for &(dx, dy) in legs {
            // Three steps per leg, so a leg builds up the way real motion
            // does rather than arriving as one delta.
            for _ in 0..3 {
                time += step_msec;
                fired |= shake.moved(dx / 3.0, dy / 3.0, time);
            }
        }
        fired
    }

    #[test]
    fn a_quick_waggle_is_a_shake() {
        let legs = [
            (60.0, 0.0),
            (-60.0, 0.0),
            (60.0, 0.0),
            (-60.0, 0.0),
            (60.0, 0.0),
        ];
        assert!(shake(&legs, 16));
    }

    #[test]
    fn the_same_waggle_slowly_is_the_pointer_being_used() {
        let legs = [
            (60.0, 0.0),
            (-60.0, 0.0),
            (60.0, 0.0),
            (-60.0, 0.0),
            (60.0, 0.0),
        ];
        // Each leg takes ~450ms: four reversals span far more than the window.
        assert!(!shake(&legs, 150));
    }

    #[test]
    fn travel_in_one_direction_never_counts_however_far_or_fast() {
        let legs = [(400.0, 0.0), (400.0, 0.0), (400.0, 0.0), (400.0, 0.0)];
        assert!(!shake(&legs, 4));
    }

    #[test]
    fn one_there_and_back_is_a_correction_not_a_shake() {
        let legs = [(80.0, 0.0), (-80.0, 0.0), (80.0, 0.0)];
        assert!(!shake(&legs, 8));
    }

    #[test]
    fn a_vertical_or_diagonal_shake_reads_the_same() {
        let legs = [
            (0.0, 50.0),
            (0.0, -50.0),
            (0.0, 50.0),
            (0.0, -50.0),
            (0.0, 50.0),
        ];
        assert!(shake(&legs, 16));
        let legs = [
            (40.0, 40.0),
            (-40.0, -40.0),
            (40.0, 40.0),
            (-40.0, -40.0),
            (40.0, 40.0),
        ];
        assert!(shake(&legs, 16));
    }

    /// A hand is not steady: mid-leg it wobbles a pixel or two the other
    /// way. That must not count as turning round.
    #[test]
    fn jitter_inside_a_leg_is_not_a_reversal() {
        let mut shake = Shake::default();
        let mut time = 1000;
        let mut fired = false;
        for _ in 0..40 {
            time += 8;
            fired |= shake.moved(3.0, 0.0, time);
            time += 8;
            fired |= shake.moved(-1.0, 0.0, time);
        }
        assert!(!fired);
        assert!(shake.reversals.is_empty());
    }

    /// Short backwards twitches before a leg has become deliberate are
    /// absorbed into it rather than counted.
    #[test]
    fn a_leg_shorter_than_the_threshold_cannot_turn_round() {
        let legs = [
            (10.0, 0.0),
            (-10.0, 0.0),
            (10.0, 0.0),
            (-10.0, 0.0),
            (10.0, 0.0),
            (-10.0, 0.0),
        ];
        assert!(!shake(&legs, 8));
    }

    #[test]
    fn fires_once_and_then_needs_a_fresh_shake() {
        let mut shake = Shake::default();
        let mut time = 1000;
        let mut fires = 0;
        // Ten reversals in quick succession: two shakes' worth, not ten.
        for i in 0..11 {
            let dx = if i % 2 == 0 { 60.0 } else { -60.0 };
            for _ in 0..3 {
                time += 16;
                if shake.moved(dx / 3.0, 0.0, time) {
                    fires += 1;
                }
            }
        }
        assert_eq!(fires, 2);
    }

    #[test]
    fn a_wrapping_clock_does_not_stall_it() {
        let mut shake = Shake::default();
        let mut time = u32::MAX - 40;
        let mut fired = false;
        for i in 0..5 {
            let dx = if i % 2 == 0 { 60.0 } else { -60.0 };
            for _ in 0..3 {
                time = time.wrapping_add(16);
                fired |= shake.moved(dx / 3.0, 0.0, time);
            }
        }
        assert!(fired);
    }
}
