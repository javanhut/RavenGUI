//! Mouse buttons with `Super` held: the three-finger gestures for a pointer
//! that has no fingers.
//!
//! A touchpad puts a window away with three fingers down, summons the
//! put-away windows with a three-finger double tap and brings one back with
//! three fingers up, and opens the overview with three fingers up on a bare
//! desktop. A mouse can do none of that, and a desktop where the best
//! behaviours are reachable from one kind of pointer and not the other is a
//! desktop that feels broken on the machine that happens to have a mouse.
//!
//! The mapping reads like the gestures rather than like a list of features.
//! `Super`+left is the fingers. Held and moved, it *is* the three-finger
//! swipe — the pointer's travel goes to the same [`crate::gesture::Swipe`]
//! the touchpad feeds, so the carousel follows the mouse sideways, the
//! overview reveal follows it up, down puts the window away on release, and
//! inside the strip sideways moves the highlight and up accepts. Pressed and
//! released without travelling, it is the *tap*: it brings up the strip of
//! put-away windows, and a second tap on a tile brings that window back.
//! `Super`+right click is *down* on its own: the window under the pointer
//! goes away, or whatever picker is up goes away. The wheel is sideways
//! travel through the workspaces, and with the strip up a notch up brings
//! the highlighted window back. That leaves the middle button for the
//! overview, the one gesture that has a key already. See [`crate::wheel`]
//! for the wheel's half.
//!
//! # Exactly `Super`
//!
//! The same rule as `Super`+wheel: `Super` alone, with `Ctrl`, `Alt` and
//! `Shift` all up. Plain buttons are the application's and stay untouched,
//! and the other chords are left free for whatever wants them later,
//! including the client. `Super` rather than `Alt` because `Alt`+click is what
//! editors and browsers already use for their own purposes, and because
//! `Super`+button is where every other desktop puts window management for a
//! mouse.
//!
//! # Why this is a module and not three lines in the input handler
//!
//! For the reason [`crate::wheel`] is: a rule that lives in the input handler
//! is a rule nothing can test. This one has no compositor in sight, so it is
//! tested on any host, and the handler is left with the one line that asks.

use smithay::input::keyboard::ModifiersState;

/// Linux evdev codes for the three buttons every mouse has.
pub(crate) const BTN_LEFT: u32 = 0x110;
pub(crate) const BTN_RIGHT: u32 = 0x111;
pub(crate) const BTN_MIDDLE: u32 = 0x112;

/// How many pointer pixels make one touchpad unit of swipe travel.
///
/// libinput reports swipe deltas in the same accelerated units as pointer
/// motion, so one is the honest starting point: a mouse moved an inch drives
/// the carousel about as far as three fingers moved an inch. The one knob to
/// turn if a drag feels long or twitchy — the thresholds themselves live in
/// [`crate::gesture`] and are the touchpad's.
const PIXELS_PER_UNIT: f64 = 1.0;

/// Pointer travel as swipe travel.
pub(crate) fn travel(dx: f64, dy: f64) -> (f64, f64) {
    (dx / PIXELS_PER_UNIT, dy / PIXELS_PER_UNIT)
}

/// What a `Super`+button press means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Click {
    /// The fingers landing. Moved before release it is the three-finger
    /// swipe; released where it landed it is the tap: show the put-away
    /// windows in the centred strip, and — with the strip already up — take
    /// the tile under the pointer, or dismiss the strip if it landed on none.
    Fingers,
    /// Down: put the window under the pointer away to the dock. Over an open
    /// picker it closes the picker instead, as three fingers down close the
    /// overview.
    ///
    /// The window under the pointer rather than the focused one, because a
    /// pointer names a window in a way three fingers on a pad cannot, and a
    /// click that put away a window other than the one it landed on would
    /// read as a misfire.
    Down,
    /// Open the overview, or close it if it is up.
    Overview,
}

/// The binding a button press with `mods` held stands for, or `None` for a
/// press that is the application's.
pub(crate) fn binding(button: u32, mods: &ModifiersState) -> Option<Click> {
    if !mods.logo || mods.ctrl || mods.alt || mods.shift {
        return None;
    }
    match button {
        BTN_LEFT => Some(Click::Fingers),
        BTN_RIGHT => Some(Click::Down),
        BTN_MIDDLE => Some(Click::Overview),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn super_held() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..ModifiersState::default()
        }
    }

    #[test]
    fn super_and_a_button_is_a_gesture() {
        assert_eq!(binding(BTN_LEFT, &super_held()), Some(Click::Fingers));
        assert_eq!(binding(BTN_RIGHT, &super_held()), Some(Click::Down));
        assert_eq!(binding(BTN_MIDDLE, &super_held()), Some(Click::Overview));
    }

    #[test]
    fn a_plain_button_is_the_applications() {
        let none = ModifiersState::default();
        for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
            assert_eq!(binding(button, &none), None);
        }
    }

    /// The other chords stay free: `Super`+`Shift`+click is not a weaker
    /// `Super`+click, it is nothing, until something claims it.
    #[test]
    fn any_other_modifier_disarms_it() {
        for extra in [
            ModifiersState {
                ctrl: true,
                ..super_held()
            },
            ModifiersState {
                alt: true,
                ..super_held()
            },
            ModifiersState {
                shift: true,
                ..super_held()
            },
            ModifiersState {
                alt: true,
                ..ModifiersState::default()
            },
        ] {
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                assert_eq!(binding(button, &extra), None, "{extra:?}");
            }
        }
    }

    #[test]
    fn buttons_beyond_the_first_three_mean_nothing() {
        // BTN_SIDE, BTN_EXTRA: the thumb buttons a browser already binds.
        assert_eq!(binding(0x113, &super_held()), None);
        assert_eq!(binding(0x114, &super_held()), None);
    }

    /// A drag drives the touchpad's recogniser, so its travel has to reach
    /// the touchpad's thresholds: a screen-width drag is several workspaces,
    /// and the pointer settling under a pressed button is none.
    #[test]
    fn drag_travel_lands_in_the_swipes_range() {
        let mut swipe = crate::gesture::Swipe::new(crate::gesture::CAROUSEL_FINGERS);
        let (dx, dy) = travel(2.0, -1.0);
        assert!(swipe.takes_hold(dx, dy).is_none(), "jitter must not commit");
        let mut swipe = crate::gesture::Swipe::new(crate::gesture::CAROUSEL_FINGERS);
        let (dx, dy) = travel(-1920.0, 0.0);
        assert_eq!(
            swipe.takes_hold(dx, dy),
            Some(crate::gesture::Hold::Horizontal)
        );
        swipe.drives(0.0);
        let position = swipe.position().unwrap();
        assert!(
            (2.0..=12.0).contains(&position),
            "a screen-width drag moved the row {position} workspaces"
        );
    }
}
