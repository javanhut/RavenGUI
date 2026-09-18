//! How many times a finger may be tried, and what happens when it runs out.
//!
//! A lock screen with a fingerprint reader has two ways in and they must not
//! share a budget. The password's attempts are counted by `ravend`, which
//! throttles them because a password is guessable; a finger is not guessable,
//! and the reason to stop offering it is quite different — after three clean
//! readings that did not match, the next one probably will not either, and
//! what the person needs is the password field they have been staring past.
//!
//! So this counts separately, locally, and small.
//!
//! # The rule that matters more than the count
//!
//! **The password is always available.** Nothing here can refuse an unlock,
//! withhold the field, or extend a throttle; the most it can do is stop asking
//! for a finger. A fingerprint reader that could lock somebody out of their own
//! machine would be a downgrade from having no reader at all, and every
//! interesting failure — a dirty sensor, a cut finger, a reader that came
//! unplugged inside the lid — is one where the finger stops working on exactly
//! the day it is needed.
//!
//! # What counts
//!
//! Only [`Scan::NoMatch`]: a clean reading of a finger the sensor does not
//! know. A reading the sensor could not use is not an attempt, because nothing
//! was attempted — see [`Scan::Retry`]. Getting that wrong spends all three
//! tries on a wet thumb and then tells its owner the machine no longer
//! recognises them.

use crate::{Error, Scan};

/// How many clean non-matches before the reader stops being offered.
///
/// Three, which is what every other implementation settled on, and the reason
/// is not security — a fourth reading of the wrong finger is no more dangerous
/// than the third. It is that by the third failure the sensor has said what it
/// thinks, and a prompt that keeps asking is a prompt that is wasting the time
/// of somebody who now has to type anyway.
pub const ATTEMPTS: u8 = 3;

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The finger matched. Ask `ravend` to unlock; it decides, not this.
    Matched,
    /// Ask for the finger again, and say why if there is a reason worth
    /// saying. Nothing was counted.
    Again(Option<crate::Retry>),
    /// That was a finger the sensor does not know, and there are tries left.
    /// The number of them, for a prompt that wants to count down.
    Failed { left: u8 },
    /// Stop offering the reader. The password is the way in, and was all
    /// along.
    Done(Reason),
}

/// Why the reader stopped being offered.
///
/// Carried so the prompt can say something true. "Try your password" after
/// three failures and "no reader" on a machine that has none are different
/// sentences, and a screen that gives the first when it means the second sends
/// somebody looking for a sensor to wipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// [`ATTEMPTS`] clean readings did not match.
    OutOfTries,
    /// There is no reader, or it stopped answering. Not a failure of the
    /// person, and must not be worded as one.
    NoSensor,
    /// The reader is there and nobody has enrolled a finger on it.
    NothingEnrolled,
}

/// The fingerprint half of one lock screen, from the moment it goes up to the
/// moment it comes down.
///
/// Made per lock rather than kept: the count is about this attempt at getting
/// back in, and one that survived an unlock would be a person letting
/// themselves in with a password and finding their finger already spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    left: u8,
    done: Option<Reason>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// A fresh gate with its full budget.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            left: ATTEMPTS,
            done: None,
        }
    }

    /// A gate that was never open: there is nothing to ask a finger of.
    ///
    /// Made at the moment the lock screen finds out, which is before it draws
    /// anything, so that no prompt for a finger appears on a machine with no
    /// reader and none appears on a machine where nobody has enrolled one.
    #[must_use]
    pub const fn shut(reason: Reason) -> Self {
        Self {
            left: 0,
            done: Some(reason),
        }
    }

    /// Whether a finger is still worth asking for.
    #[must_use]
    pub const fn open(&self) -> bool {
        self.done.is_none()
    }

    /// How many clean non-matches are left before it shuts.
    #[must_use]
    pub const fn left(&self) -> u8 {
        self.left
    }

    /// Why it shut, or `None` while it is open.
    #[must_use]
    pub const fn reason(&self) -> Option<Reason> {
        self.done
    }

    /// Account for one reading.
    ///
    /// Returns [`Verdict::Done`] for every reading after the gate has shut, so
    /// that a scan arriving from a reader thread a moment too late cannot
    /// reopen it or unlock anything.
    pub fn scan(&mut self, scan: Scan) -> Verdict {
        if let Some(reason) = self.done {
            return Verdict::Done(reason);
        }
        match scan {
            Scan::Good => Verdict::Matched,
            // Nothing was attempted; see the module note.
            Scan::Retry(why) => Verdict::Again(Some(why)),
            Scan::NoMatch => {
                self.left = self.left.saturating_sub(1);
                if self.left == 0 {
                    self.done = Some(Reason::OutOfTries);
                    Verdict::Done(Reason::OutOfTries)
                } else {
                    Verdict::Failed { left: self.left }
                }
            }
        }
    }

    /// Account for the reader itself having gone wrong.
    ///
    /// Shuts the gate without spending an attempt: whatever happened was not
    /// the person's doing, and a reader that has been unplugged is not three
    /// failures.
    pub fn broke(&mut self, error: &Error) -> Verdict {
        let reason = match error {
            Error::NotEnrolled(_) => Reason::NothingEnrolled,
            Error::NoSensor | Error::Unreachable(_) | Error::Full => Reason::NoSensor,
        };
        self.done = Some(self.done.unwrap_or(reason));
        self.left = 0;
        Verdict::Done(self.done.expect("just set"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Retry;

    #[test]
    fn a_match_is_a_match() {
        let mut gate = Gate::new();
        assert_eq!(gate.scan(Scan::Good), Verdict::Matched);
        // And it does not shut the gate: the unlock is `ravend`'s to refuse,
        // and a refusal must leave the finger usable for the retry.
        assert!(gate.open());
        assert_eq!(gate.left(), ATTEMPTS);
    }

    /// The one that matters. A wet thumb is not a failed attempt.
    #[test]
    fn a_reading_that_could_not_be_used_costs_nothing() {
        let mut gate = Gate::new();
        for why in [
            Retry::TooShort,
            Retry::OffCentre,
            Retry::Unreadable,
            Retry::Unchanged,
        ] {
            assert_eq!(gate.scan(Scan::Retry(why)), Verdict::Again(Some(why)));
        }
        assert_eq!(gate.left(), ATTEMPTS, "four bad readings spent an attempt");
        assert!(gate.open());
    }

    #[test]
    fn three_clean_misses_shut_it() {
        let mut gate = Gate::new();
        assert_eq!(gate.scan(Scan::NoMatch), Verdict::Failed { left: 2 });
        assert_eq!(gate.scan(Scan::NoMatch), Verdict::Failed { left: 1 });
        assert_eq!(
            gate.scan(Scan::NoMatch),
            Verdict::Done(Reason::OutOfTries),
            "the third is the last"
        );
        assert!(!gate.open());
        assert_eq!(gate.reason(), Some(Reason::OutOfTries));
    }

    /// A reading that arrives from the reader's thread after the gate shut
    /// must not unlock anything, however good it is.
    #[test]
    fn a_late_match_after_it_shut_is_not_a_match() {
        let mut gate = Gate::new();
        for _ in 0..ATTEMPTS {
            gate.scan(Scan::NoMatch);
        }
        assert_eq!(gate.scan(Scan::Good), Verdict::Done(Reason::OutOfTries));
    }

    /// A broken reader is not the person's fault and does not read as one.
    #[test]
    fn a_reader_that_broke_spends_nothing_and_says_so() {
        let mut gate = Gate::new();
        assert_eq!(
            gate.broke(&Error::Unreachable("timed out".into())),
            Verdict::Done(Reason::NoSensor)
        );
        assert_eq!(gate.reason(), Some(Reason::NoSensor));
    }

    #[test]
    fn nothing_enrolled_is_its_own_sentence() {
        let mut gate = Gate::new();
        assert_eq!(
            gate.broke(&Error::NotEnrolled(crate::Finger::RightIndex)),
            Verdict::Done(Reason::NothingEnrolled)
        );
    }

    /// The first reason sticks: a reader that goes on failing after it was
    /// already out of tries must not have its story rewritten.
    #[test]
    fn the_first_reason_is_the_one_reported() {
        let mut gate = Gate::new();
        for _ in 0..ATTEMPTS {
            gate.scan(Scan::NoMatch);
        }
        gate.broke(&Error::NoSensor);
        assert_eq!(gate.reason(), Some(Reason::OutOfTries));
    }

    #[test]
    fn a_gate_that_was_never_open_asks_for_nothing() {
        let gate = Gate::shut(Reason::NothingEnrolled);
        assert!(!gate.open());
        assert_eq!(gate.left(), 0);
    }
}
