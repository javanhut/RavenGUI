//! Mouse-wheel accumulation: how many whole steps a scroll event is worth.
//!
//! A mouse is the one pointer with no three-finger swipe, so `Super`+wheel is
//! how it reaches the workspace either side. Counting the notches is not quite
//! the one-liner it looks like, for two reasons.
//!
//! The first is free-spinning wheels. libinput reports wheel travel in v120
//! units, where an ordinary detent is 120, but a high-resolution wheel sends
//! fractions of that as the wheel turns — a hundred small events rather than
//! one large one. Acting on every event would fly through the workspaces; only
//! acting on exact multiples of 120 would never act at all. The travel has to
//! be banked until it makes a whole step, which is state, and state in the
//! input handler is state nothing can test — the same argument
//! [`crate::gesture`] makes about swipes.
//!
//! The second is that the bank belongs to a direction. Turning the wheel back
//! the other way is a new intention, not a continuation of the last one, and a
//! bank carried across the reversal would make the first step back arrive
//! early or late depending on where the previous one happened to stop.

use smithay::backend::input::Axis;
use smithay::input::keyboard::ModifiersState;

/// The v120 travel of one detent of an ordinary wheel.
const DETENT: i32 = 120;

/// Which keys are down while the wheel turns, as far as the bindings care.
///
/// Three cases rather than the modifier flags themselves, because that is
/// how many the compositor tells apart: exactly `Super` is the workspace
/// binding wherever the wheel is; nothing at all is the client's wheel,
/// unless a picker is up with nothing under it to scroll; and anything else
/// is left alone — except by the switcher's strip, which takes the wheel
/// whatever is held, since `Alt` is down for the whole of an Alt-Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chord {
    /// `Super` alone.
    Super,
    /// No modifier at all.
    Bare,
    /// Any other combination.
    Other,
}

impl Chord {
    /// Classify the modifiers held while the wheel turned.
    ///
    /// Requiring the other modifiers to be *absent* from [`Chord::Super`]
    /// leaves `Super`+`Shift`+wheel and the rest free for whatever wants
    /// them later, including the client. Lock keys are not modifiers here:
    /// Caps Lock must not disarm a binding.
    pub(crate) fn of(mods: &ModifiersState) -> Self {
        match (mods.logo, mods.ctrl || mods.alt || mods.shift) {
            (true, false) => Self::Super,
            (false, false) => Self::Bare,
            _ => Self::Other,
        }
    }
}

/// Wheel travel banked towards the next whole step.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Notches {
    /// Travel since the last whole step, and the axis it arrived on. `None`
    /// when nothing is part-way — which is also how a fresh accumulator and
    /// one that has just paid out a step look, because they are the same
    /// thing.
    banked: Option<(Axis, i32)>,
}

impl Notches {
    /// Bank `v120` of travel on `axis` and take the whole steps it completes.
    ///
    /// Positive is down and right, matching libinput, so a positive step means
    /// "forwards" on either axis and the caller needs no per-axis sign rule.
    ///
    /// A reversal, or travel on the other axis, discards the bank first: both
    /// are a new gesture, and only the travel since it started should count
    /// towards its first step.
    pub(crate) fn take(&mut self, axis: Axis, v120: i32) -> i32 {
        let carried = match self.banked {
            Some((banked_axis, bank)) if banked_axis == axis && bank.signum() == v120.signum() => {
                bank
            }
            _ => 0,
        };
        // Saturating because a device is free to report nonsense, and a
        // wrapped bank would step the workspace the wrong way.
        let total = carried.saturating_add(v120);
        let steps = total / DETENT;
        let rest = total % DETENT;
        self.banked = (rest != 0).then_some((axis, rest));
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_detent_is_one_step() {
        let mut n = Notches::default();
        assert_eq!(n.take(Axis::Vertical, DETENT), 1);
        assert_eq!(n.take(Axis::Vertical, -DETENT), -1);
    }

    /// The free-spinning case: fractions add up to exactly the steps the
    /// travel is worth, and no more.
    #[test]
    fn fractions_bank_until_they_make_a_step() {
        let mut n = Notches::default();
        let taken: i32 = (0..8).map(|_| n.take(Axis::Vertical, 30)).sum();
        assert_eq!(taken, 2, "240 units of travel is two steps, however split");
    }

    #[test]
    fn a_part_step_alone_does_nothing() {
        let mut n = Notches::default();
        assert_eq!(n.take(Axis::Vertical, DETENT - 1), 0);
    }

    /// Turning back the other way starts over rather than cashing in the bank
    /// that was pointing the opposite direction.
    #[test]
    fn a_reversal_drops_the_bank() {
        let mut n = Notches::default();
        assert_eq!(n.take(Axis::Vertical, 119), 0);
        assert_eq!(
            n.take(Axis::Vertical, -1),
            0,
            "the 119 forwards must not complete a step backwards"
        );
        assert_eq!(n.take(Axis::Vertical, -118), 0, "the bank restarts at -1");
        assert_eq!(
            n.take(Axis::Vertical, -1),
            -1,
            "and -120 in total is a step"
        );
    }

    /// A tilt wheel and a scroll wheel are two gestures, not one.
    #[test]
    fn the_other_axis_drops_the_bank() {
        let mut n = Notches::default();
        assert_eq!(n.take(Axis::Vertical, 119), 0);
        assert_eq!(n.take(Axis::Horizontal, 1), 0);
        assert_eq!(n.take(Axis::Horizontal, 119), 1);
    }

    /// One event worth several detents pays out all of them: a coarse wheel
    /// that reports 240 at once means two workspaces, not one.
    #[test]
    fn several_detents_at_once_are_several_steps() {
        let mut n = Notches::default();
        assert_eq!(n.take(Axis::Vertical, DETENT * 3), 3);
    }

    #[test]
    fn nonsense_travel_does_not_wrap_the_bank() {
        let mut n = Notches::default();
        n.take(Axis::Vertical, DETENT + 7);
        assert_eq!(n.take(Axis::Vertical, i32::MAX), i32::MAX / DETENT);
    }

    #[test]
    fn the_chord_is_exactly_super_bare_or_something_else() {
        let none = ModifiersState::default();
        assert_eq!(Chord::of(&none), Chord::Bare);
        assert_eq!(
            Chord::of(&ModifiersState { logo: true, ..none }),
            Chord::Super
        );
        assert_eq!(
            Chord::of(&ModifiersState {
                logo: true,
                shift: true,
                ..none
            }),
            Chord::Other
        );
        assert_eq!(
            Chord::of(&ModifiersState { alt: true, ..none }),
            Chord::Other
        );
        // Caps Lock is not a modifier as far as the wheel is concerned.
        assert_eq!(
            Chord::of(&ModifiersState {
                logo: true,
                caps_lock: true,
                ..none
            }),
            Chord::Super
        );
    }
}
