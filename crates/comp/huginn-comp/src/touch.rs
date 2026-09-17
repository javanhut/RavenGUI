//! Fingers on the glass: what a touchscreen's contacts mean before anything
//! acts on them.
//!
//! A touchscreen is the one input device that is both a pointer and a set of
//! fingers. One finger names a place the way a mouse does — it lands on a dock
//! icon, a title bar, a window — and three fingers are the gesture vocabulary
//! the touchpad already has. So this module does two things and keeps them
//! apart: it tracks which contacts are down and where, and it decides at which
//! moment the compositor's own gesture takes the whole hand away from whatever
//! the first fingers had started.
//!
//! # The same gestures, not new ones
//!
//! Three fingers sideways is the carousel, up is the overview, down puts the
//! window away — the same as [`crate::gesture`] and for the same reason
//! [`crate::mouse`] exists: a desktop whose best behaviours are reachable from
//! the touchpad and not from the panel above it is a desktop that feels broken
//! on a convertible. The travel is converted to the touchpad's units here and
//! fed to the touchpad's own [`crate::gesture::Swipe`], so there is one
//! recogniser and one set of thresholds, and a change to how a flick lands
//! changes it for every device at once.
//!
//! # The gesture is taken, not shared
//!
//! Fingers do not land together. By the time a third one arrives the first two
//! have been down for tens of milliseconds and have already gone somewhere — a
//! client is mid-drag, or the dock has been pressed. The gesture cannot wait
//! for a hand that may never become three fingers, and it cannot let a client
//! go on receiving half of one, so the third contact *cancels*: `wl_touch.cancel`
//! goes to whoever held the first two, and from then until the last finger
//! lifts the whole hand is the compositor's. That is what the cancel event is
//! for, and a client that handles it correctly sees exactly what happened —
//! a touch sequence taken over by the compositor.
//!
//! # Why this is a module and not a few fields in the input handler
//!
//! [`crate::gesture`]'s reason. Slot bookkeeping is state, and state that
//! lives in the input handler is state nothing can test; here it is a plain
//! value with no compositor, no seat and no Wayland in sight, so the rules
//! below are unit-tested on any host — including one with no touchscreen.

use smithay::utils::{Logical, Point};

/// Fingers on the glass that mean a compositor gesture rather than a client's
/// touch.
///
/// Three, because that is what the touchpad uses and what the mouse's
/// `Super`+drag stands for. Two is left alone deliberately: it is pinch-to-zoom
/// and two-finger pan in every application that has them, and a compositor that
/// took two fingers would break the map, the photo viewer and the PDF reader in
/// exchange for one more shortcut.
pub(crate) const GESTURE_FINGERS: usize = 3;

/// How many logical screen pixels make one touchpad unit of swipe travel.
///
/// The touchpad's thresholds are in its own units — 180 to a workspace, 140 to
/// a full overview reveal — and a finger on the glass travels in screen pixels,
/// so the two have to be related by a number chosen rather than inherited.
///
/// Three puts one workspace at 540 logical pixels: a comfortable swipe across
/// somewhat less than a third of a 1920-wide panel, so the width of the screen
/// is about three and a half workspaces and no single gesture can fling the
/// carousel further than the row is long. The full overview reveal lands at 420
/// pixels, a little under half the height of a 1080 panel.
///
/// The one knob to turn if a swipe feels long or twitchy. The thresholds
/// themselves stay in [`crate::gesture`] and are the touchpad's, so turning
/// this cannot make a touchscreen and a touchpad disagree about what a flick
/// means — only about how far the hand has to move to say it.
const PIXELS_PER_UNIT: f64 = 3.0;

/// How far a panel's diagonal and a touchscreen's may differ and still be the
/// same piece of hardware, as a fraction.
///
/// A digitizer's active area and the panel's visible area are measured by two
/// different vendors for two different purposes and are never quite the same
/// number; EDID millimetres are rounded to the centimetre and are routinely a
/// little wrong besides. A tenth is wide enough to survive all of that and
/// narrow enough to tell a 13" laptop panel from the 24" monitor beside it,
/// which is the only distinction this has to make.
const PANEL_TOLERANCE: f64 = 0.1;

/// Finger travel as swipe travel, in the units [`crate::gesture`] counts in.
pub(crate) fn travel(dx: f64, dy: f64) -> (f64, f64) {
    (dx / PIXELS_PER_UNIT, dy / PIXELS_PER_UNIT)
}

/// Which of `panels` a touchscreen whose active area is `size` millimetres is
/// the glass on, as an index.
///
/// Physical size is the only evidence there is. libinput knows how big the
/// digitizer is and EDID says how big each panel is, and a touchscreen is
/// glued to a screen of the same size — so the closest match within
/// [`PANEL_TOLERANCE`] is the one, and no match at all is better than a wrong
/// one. `None` sends the caller to its fallback, which is the built-in panel:
/// a touchscreen that could not be matched is almost always the one in the
/// lid, and on the machines where it is the only screen the question does not
/// arise.
///
/// Diagonals rather than width and height, because a touchscreen in a
/// convertible is routinely mounted the other way up from the panel it covers
/// and reports its axes swapped. The diagonal is the same either way.
pub(crate) fn panel_for(size: Option<(f64, f64)>, panels: &[(i32, i32)]) -> Option<usize> {
    let (w, h) = size?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let diagonal = (w * w + h * h).sqrt();
    panels
        .iter()
        .enumerate()
        .filter_map(|(index, (pw, ph))| {
            if *pw <= 0 || *ph <= 0 {
                // A panel that did not report its size cannot be matched
                // against one that did. Skipped rather than guessed at.
                return None;
            }
            let theirs = (f64::from(*pw).powi(2) + f64::from(*ph).powi(2)).sqrt();
            let off = (theirs - diagonal).abs() / diagonal;
            (off <= PANEL_TOLERANCE).then_some((index, off))
        })
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(index, _)| index)
}

/// Who a contact belongs to, decided when it lands and not revisited.
///
/// A finger that went to a client keeps going to that client until it lifts,
/// even if it wanders off the surface — the same rule an implicit pointer grab
/// follows, and for the same reason: a drag that started in a scroll view must
/// not stop being that scroll view's the moment it leaves its edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    /// Something the compositor draws took it: the dock, the launcher, the
    /// overview, a notification card, a title bar. Nothing is forwarded.
    Shell,
    /// A client surface has it. `wl_touch` down, motion and up go there.
    Client,
    /// It is standing in for the pointer, because what it landed on cannot
    /// hear a finger: an X11 window. XWayland speaks the X11 protocol to its
    /// clients and the compositor's `wl_touch` never reaches them, so a finger
    /// on a legacy application would otherwise do nothing whatsoever. One
    /// contact — the first — moves the cursor and presses the primary button
    /// instead, which is what every one of those applications is expecting.
    ///
    /// Only the first: a second finger that also drove the pointer would be
    /// two cursors, and a pinch on an X11 window would read as the pointer
    /// teleporting between the fingers.
    Pointer,
    /// The compositor's own gesture has the whole hand. See the module note on
    /// taking rather than sharing.
    Gesture,
    /// It landed on nothing that wanted it — the bare desktop, say. Tracked so
    /// that the finger count stays right, and forwarded nowhere.
    Nobody,
}

/// One finger, from the moment it lands to the moment it lifts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Contact {
    /// libinput's slot. The identity the protocol carries, so it is what
    /// motion and up are matched on rather than proximity to a last position.
    pub(crate) slot: i32,
    /// Where it is now, in global logical coordinates.
    pub(crate) at: Point<f64, Logical>,
    /// Who took it. See [`Owner`].
    pub(crate) owner: Owner,
}

/// What the caller must do about a finger that has just landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Route it: hit-test the shell, then the scene, and tell
    /// [`Contacts::took`] which of the two took it.
    Route,
    /// This contact completed the hand. Cancel whatever the earlier fingers
    /// had been going to and start the swipe; this finger and every other one
    /// down is now the compositor's.
    Claims,
    /// A gesture already owns the hand, or this slot was somehow already down.
    /// Nothing to do.
    Ignore,
}

/// What the caller must do about a finger that has just lifted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lift {
    /// Send `wl_touch.up` for this slot.
    Client,
    /// Release the primary button: this contact was standing in for the
    /// pointer. See [`Owner::Pointer`].
    Pointer,
    /// It was the shell's, or nobody's. Nothing is forwarded.
    Quiet,
    /// The hand has dropped below [`GESTURE_FINGERS`]: end the swipe. Reported
    /// exactly once per gesture, on the first finger to leave — a swipe ended
    /// twice would fling the carousel and then fling the settled row again.
    EndsGesture,
    /// One of the fingers trailing off a gesture that has already ended.
    /// Nothing is forwarded and nothing is ended.
    Trailing,
}

/// Every finger currently on the glass.
///
/// Small and linear on purpose: a hand has five fingers and the vector is
/// walked a handful of times per event. A map keyed by slot would be more
/// code and no faster at this size.
#[derive(Debug, Default)]
pub(crate) struct Contacts {
    down: Vec<Contact>,
    /// Whether the compositor's gesture has taken this hand. Stays set until
    /// the last finger lifts, so that fingers trailing off a finished swipe
    /// do not start being routed again half way through.
    claimed: bool,
    /// Whether the swipe this hand was driving has already been ended. See
    /// [`Lift::EndsGesture`] — the fingers leave one at a time and only the
    /// first of them is the end of the gesture.
    ended: bool,
}

impl Contacts {
    /// How many fingers are down.
    pub(crate) fn len(&self) -> usize {
        self.down.len()
    }

    /// Whether a compositor gesture owns the hand.
    pub(crate) fn claimed(&self) -> bool {
        self.claimed
    }

    /// Whether the glass is clear. The renderer asks, to know whether to draw
    /// the pointer.
    pub(crate) fn is_empty(&self) -> bool {
        self.down.is_empty()
    }

    /// A finger landed at `at`.
    ///
    /// The count is what decides: the [`GESTURE_FINGERS`]th contact takes the
    /// hand, every earlier one is routed, and anything after the claim is
    /// ignored because the hand already belongs to the gesture.
    pub(crate) fn down(&mut self, slot: i32, at: Point<f64, Logical>) -> Landing {
        if self.down.iter().any(|c| c.slot == slot) {
            // libinput should never report a slot down twice without an up in
            // between. If it does, the old contact is the stale one.
            self.down.retain(|c| c.slot != slot);
        }
        self.down.push(Contact {
            slot,
            at,
            // Provisional. `took` replaces it for a routed contact, and a
            // claim below rewrites every contact's owner at once.
            owner: Owner::Nobody,
        });
        if self.claimed {
            if let Some(contact) = self.down.last_mut() {
                contact.owner = Owner::Gesture;
            }
            return Landing::Ignore;
        }
        if self.down.len() == GESTURE_FINGERS {
            self.claimed = true;
            for contact in &mut self.down {
                contact.owner = Owner::Gesture;
            }
            return Landing::Claims;
        }
        Landing::Route
    }

    /// Record who took the contact the caller was told to [`Landing::Route`].
    pub(crate) fn took(&mut self, slot: i32, owner: Owner) {
        if let Some(contact) = self.down.iter_mut().find(|c| c.slot == slot) {
            contact.owner = owner;
        }
    }

    /// A finger moved to `at`. Returns the contact as it now stands, or `None`
    /// for a slot that is not down — libinput reports motion for a slot it has
    /// already released often enough that this must not be an assertion.
    pub(crate) fn motion(&mut self, slot: i32, at: Point<f64, Logical>) -> Option<Contact> {
        let contact = self.down.iter_mut().find(|c| c.slot == slot)?;
        contact.at = at;
        Some(*contact)
    }

    /// The delta, in [`crate::gesture`]'s units, that a gesture should be fed
    /// for a finger moving to `at`.
    ///
    /// Only the first finger of the hand drives it. Averaging all three sounds
    /// more faithful and is worse: the three contacts are reported in separate
    /// events, so an average recomputed per event moves by a third of the
    /// travel three times, and a hand whose fingers splay slightly as it slides
    /// feeds the recogniser motion nobody made. One finger is the hand's
    /// travel, and the other two are there to say which gesture it is.
    pub(crate) fn drives(&mut self, slot: i32, at: Point<f64, Logical>) -> Option<(f64, f64)> {
        if !self.claimed {
            return None;
        }
        // Copied rather than borrowed: the arms below take `&mut self`, and a
        // `Contact` is two floats and an enum.
        let first = *self.down.first()?;
        if first.slot != slot {
            // Still track it, so the contact's position stays current.
            self.motion(slot, at);
            return None;
        }
        let from = first.at;
        self.motion(slot, at)?;
        Some(travel(at.x - from.x, at.y - from.y))
    }

    /// A finger lifted.
    pub(crate) fn up(&mut self, slot: i32) -> Lift {
        let Some(index) = self.down.iter().position(|c| c.slot == slot) else {
            return Lift::Quiet;
        };
        let contact = self.down.remove(index);
        let lift = match contact.owner {
            // The first finger to leave ends the swipe; the others are the
            // tail of the same lift. See [`Lift::EndsGesture`].
            Owner::Gesture if !self.ended => {
                self.ended = true;
                Lift::EndsGesture
            }
            Owner::Gesture => Lift::Trailing,
            Owner::Client => Lift::Client,
            Owner::Pointer => Lift::Pointer,
            Owner::Shell | Owner::Nobody => Lift::Quiet,
        };
        if self.down.is_empty() {
            // The hand is off the glass; the next finger down starts afresh.
            self.claimed = false;
            self.ended = false;
        }
        lift
    }

    /// Everything is off the glass, or the session took the hand away — a lock,
    /// a VT switch, a device disappearing mid-gesture.
    ///
    /// Returns whether a gesture was in progress and had not already ended,
    /// since that one has to be ended rather than merely forgotten: the
    /// carousel is part way between two workspaces and something has to put it
    /// back.
    pub(crate) fn clear(&mut self) -> bool {
        let running = self.claimed && !self.ended;
        self.down.clear();
        self.claimed = false;
        self.ended = false;
        running
    }

    /// The owner of the contact in `slot`, if it is down.
    pub(crate) fn owner(&self, slot: i32) -> Option<Owner> {
        self.down.iter().find(|c| c.slot == slot).map(|c| c.owner)
    }

    /// Whether any contact is standing in for the pointer, and so is holding
    /// the primary button down on an X11 window. See [`Owner::Pointer`].
    pub(crate) fn any_emulating(&self) -> bool {
        self.down.iter().any(|c| c.owner == Owner::Pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Point<f64, Logical> {
        (x, y).into()
    }

    /// The hand that claims and the swipe it drives have to be the same number
    /// of fingers, or the recogniser is told about a gesture nobody made.
    #[test]
    fn the_gesture_is_the_touchpads_gesture() {
        assert_eq!(
            GESTURE_FINGERS,
            crate::gesture::CAROUSEL_FINGERS as usize,
            "the glass and the pad must agree on what three fingers means"
        );
    }

    #[test]
    fn one_finger_is_routed() {
        let mut contacts = Contacts::default();
        assert_eq!(contacts.down(0, at(100.0, 100.0)), Landing::Route);
        assert_eq!(contacts.len(), 1);
        assert!(!contacts.claimed());
    }

    /// Two fingers are the application's: pinch and pan must keep working.
    #[test]
    fn two_fingers_are_still_the_clients() {
        let mut contacts = Contacts::default();
        assert_eq!(contacts.down(0, at(100.0, 100.0)), Landing::Route);
        assert_eq!(contacts.down(1, at(300.0, 100.0)), Landing::Route);
        assert!(!contacts.claimed());
    }

    #[test]
    fn the_third_finger_takes_the_hand() {
        let mut contacts = Contacts::default();
        contacts.down(0, at(100.0, 100.0));
        contacts.took(0, Owner::Client);
        contacts.down(1, at(300.0, 100.0));
        contacts.took(1, Owner::Client);
        assert_eq!(contacts.down(2, at(500.0, 100.0)), Landing::Claims);
        assert!(contacts.claimed());
        // Every finger, not just the new one: the client's two are gone too.
        for slot in 0..3 {
            assert_eq!(contacts.owner(slot), Some(Owner::Gesture), "slot {slot}");
        }
    }

    /// A fourth finger landing on a claimed hand changes nothing. Four fingers
    /// are not a different gesture; they are three fingers and a thumb.
    #[test]
    fn a_fourth_finger_is_ignored() {
        let mut contacts = Contacts::default();
        for slot in 0..3 {
            contacts.down(slot, at(100.0 * f64::from(slot), 100.0));
        }
        assert_eq!(contacts.down(3, at(700.0, 100.0)), Landing::Ignore);
        assert!(contacts.claimed());
    }

    #[test]
    fn the_first_finger_off_ends_the_gesture_and_the_rest_are_quiet() {
        let mut contacts = Contacts::default();
        for slot in 0..3 {
            contacts.down(slot, at(100.0 * f64::from(slot), 100.0));
        }
        assert_eq!(contacts.up(0), Lift::EndsGesture);
        // The gesture is over. The other two are the tail of the same lift and
        // must not end it a second time.
        assert_eq!(contacts.up(1), Lift::Trailing);
        assert_eq!(contacts.up(2), Lift::Trailing);
        assert!(!contacts.claimed(), "the hand is off the glass");
    }

    /// The claim does not outlive the hand: a gesture, then a tap, and the tap
    /// is routed like any other first finger.
    #[test]
    fn the_claim_is_released_with_the_last_finger() {
        let mut contacts = Contacts::default();
        for slot in 0..3 {
            contacts.down(slot, at(100.0, 100.0));
        }
        for slot in 0..3 {
            contacts.up(slot);
        }
        assert!(!contacts.claimed());
        assert_eq!(contacts.down(0, at(100.0, 100.0)), Landing::Route);
    }

    #[test]
    fn a_clients_finger_lifts_as_the_clients() {
        let mut contacts = Contacts::default();
        contacts.down(7, at(100.0, 100.0));
        contacts.took(7, Owner::Client);
        assert_eq!(contacts.up(7), Lift::Client);
    }

    #[test]
    fn a_shell_press_forwards_nothing_on_the_way_up() {
        let mut contacts = Contacts::default();
        contacts.down(7, at(100.0, 100.0));
        contacts.took(7, Owner::Shell);
        assert_eq!(contacts.up(7), Lift::Quiet);
    }




    /// Only the first finger drives the swipe; see [`Contacts::drives`].
    #[test]
    fn only_the_first_finger_drives_the_gesture() {
        let mut contacts = Contacts::default();
        for slot in 0..3 {
            contacts.down(slot, at(100.0, 100.0));
        }
        assert!(contacts.drives(1, at(400.0, 100.0)).is_none());
        let (dx, dy) = contacts.drives(0, at(400.0, 100.0)).expect("the first drives");
        assert!((dx - travel(300.0, 0.0).0).abs() < f64::EPSILON);
        assert_eq!(dy, 0.0);
    }

    /// An unclaimed hand drives nothing: two fingers panning a map must not
    /// also be moving the workspace row.
    #[test]
    fn an_unclaimed_hand_drives_nothing() {
        let mut contacts = Contacts::default();
        contacts.down(0, at(100.0, 100.0));
        contacts.down(1, at(300.0, 100.0));
        assert!(contacts.drives(0, at(900.0, 100.0)).is_none());
    }

    /// The delta fed to the recogniser is the travel since the last report,
    /// not since the finger landed: the swipe accumulates it itself.
    #[test]
    fn the_gesture_is_fed_deltas_not_totals() {
        let mut contacts = Contacts::default();
        for slot in 0..3 {
            contacts.down(slot, at(100.0, 100.0));
        }
        let (first, _) = contacts.drives(0, at(200.0, 100.0)).unwrap();
        let (second, _) = contacts.drives(0, at(300.0, 100.0)).unwrap();
        assert!(
            (first - second).abs() < f64::EPSILON,
            "two equal steps must report equal travel: {first} then {second}"
        );
    }

    #[test]
    fn clearing_reports_whether_a_gesture_was_running() {
        let mut contacts = Contacts::default();
        contacts.down(0, at(100.0, 100.0));
        assert!(!contacts.clear(), "one finger is no gesture");
        for slot in 0..3 {
            contacts.down(slot, at(100.0, 100.0));
        }
        assert!(contacts.clear(), "a claimed hand was mid-gesture");
        assert!(contacts.is_empty());
    }

    /// The machine this was written for: a 14" HP convertible with the glass
    /// on the lid, and a 24" monitor on the desk beside it.
    #[test]
    fn a_touchscreen_finds_the_panel_it_is_glued_to() {
        // eDP-1, 14" 16:9; DP-1, 24" 16:9.
        let panels = [(309, 174), (527, 296)];
        assert_eq!(panel_for(Some((309.0, 174.0)), &panels), Some(0));
        // A digitizer measured a few millimetres out is still that panel.
        assert_eq!(panel_for(Some((305.0, 171.0)), &panels), Some(0));
        // And the big one is found when that is what is touched.
        assert_eq!(panel_for(Some((527.0, 296.0)), &panels), Some(1));
    }

    /// Mounted the other way up, which convertibles do. The diagonal is what
    /// is compared precisely so that this works.
    #[test]
    fn a_rotated_digitizer_still_matches() {
        let panels = [(309, 174)];
        assert_eq!(panel_for(Some((174.0, 309.0)), &panels), Some(0));
    }

    /// No match is better than a wrong one: the caller's fallback is the
    /// built-in panel, which is the right answer far more often than "the
    /// nearest size, whatever it was".
    #[test]
    fn nothing_near_enough_matches_nothing() {
        let panels = [(527, 296)];
        assert_eq!(panel_for(Some((309.0, 174.0)), &panels), None);
    }

    #[test]
    fn a_device_or_panel_that_gave_no_size_is_not_guessed_at() {
        assert_eq!(panel_for(None, &[(309, 174)]), None);
        assert_eq!(panel_for(Some((0.0, 0.0)), &[(309, 174)]), None);
        assert_eq!(panel_for(Some((309.0, 174.0)), &[(0, 0)]), None);
    }

    /// Two panels of the same size: the closer wins, and it is at least
    /// deterministic rather than whichever was iterated first.
    #[test]
    fn the_closest_panel_wins() {
        let panels = [(320, 180), (309, 174)];
        assert_eq!(panel_for(Some((309.0, 174.0)), &panels), Some(1));
    }

    /// A swipe across the panel has to reach the touchpad's thresholds, the
    /// same proof [`crate::mouse`] carries for a `Super`+drag.
    #[test]
    fn swipe_travel_lands_in_the_gestures_range() {
        let mut swipe = crate::gesture::Swipe::new(crate::gesture::CAROUSEL_FINGERS);
        let (dx, dy) = travel(6.0, -3.0);
        assert!(
            swipe.takes_hold(dx, dy, std::time::Duration::ZERO).is_none(),
            "a finger settling must not commit the axis"
        );

        let mut swipe = crate::gesture::Swipe::new(crate::gesture::CAROUSEL_FINGERS);
        let (dx, dy) = travel(-1920.0, 0.0);
        assert_eq!(
            swipe.takes_hold(dx, dy, std::time::Duration::ZERO),
            Some(crate::gesture::Hold::Horizontal)
        );
        swipe.drives(0.0);
        let position = swipe.position().unwrap();
        assert!(
            (2.0..=5.0).contains(&position),
            "a screen-width swipe moved the row {position} workspaces; a hand \
             that crosses the panel should be a few, not the whole row"
        );
    }
}
