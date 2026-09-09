//! Mouse buttons with `Super` held: the three-finger gestures for a pointer
//! that has no fingers.
//!
//! A touchpad puts a window away with three fingers down, opens the overview
//! with three fingers up, and summons the put-away windows with a three-finger
//! double tap. A mouse can do none of that, and a desktop where the best
//! behaviours are reachable from one kind of pointer and not the other is a
//! desktop that feels broken on the machine that happens to have a mouse. So
//! each vertical gesture has a button here, and [`crate::wheel`] gives the
//! sideways one to the wheel.
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

/// What a `Super`+button press means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Click {
    /// Put the window under the pointer away to the dock: three fingers down.
    ///
    /// The window under the pointer rather than the focused one, because a
    /// pointer names a window in a way three fingers on a pad cannot, and a
    /// click that put away a window other than the one it landed on would
    /// read as a misfire.
    PutAway,
    /// Open the overview, or close it if it is up: three fingers up, and
    /// three fingers down over an open overview.
    Overview,
    /// Show the put-away windows in the centred strip, or dismiss the strip if
    /// it is up: the three-finger double tap.
    PutAwayList,
}

/// The binding a button press with `mods` held stands for, or `None` for a
/// press that is the application's.
pub(crate) fn binding(button: u32, mods: &ModifiersState) -> Option<Click> {
    if !mods.logo || mods.ctrl || mods.alt || mods.shift {
        return None;
    }
    match button {
        BTN_LEFT => Some(Click::PutAway),
        BTN_RIGHT => Some(Click::Overview),
        BTN_MIDDLE => Some(Click::PutAwayList),
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
        assert_eq!(binding(BTN_LEFT, &super_held()), Some(Click::PutAway));
        assert_eq!(binding(BTN_RIGHT, &super_held()), Some(Click::Overview));
        assert_eq!(binding(BTN_MIDDLE, &super_held()), Some(Click::PutAwayList));
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
}
