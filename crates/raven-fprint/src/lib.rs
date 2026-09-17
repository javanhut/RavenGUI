//! Fingerprints: enrolling one, checking one, and the rules around both.
//!
//! # What this crate is
//!
//! RavenLinux has no fingerprint stack. There is no `libfprint` on the image
//! and no `fprintd` on the bus, and adopting either would put a GLib service
//! and a C USB library on the path to unlocking a machine whose whole userland
//! is static musl binaries — the same trade [CAW] declined when it wrote the
//! wireless stack in-process rather than shipping `wpa_supplicant`. So the
//! stack is Raven's, and this crate is the part of it that decides things.
//!
//! [CAW]: https://github.com/javanhut/CAW
//!
//! **Everything here is pure.** No USB, no D-Bus, no sockets, no clock it did
//! not have handed to it. What a sensor *is* lives behind [`Sensor`], which is
//! one trait with one implementation in this crate — [`fake::Fake`], which
//! scans nothing and says so. The real transport is a separate crate, for the
//! reason `huginn-core` has no Wayland in it: the rules below decide whether
//! somebody gets into a machine, and rules that can only be exercised by
//! holding a finger against hardware are rules nobody will exercise.
//!
//! # What a fingerprint is allowed to do
//!
//! Unlock a session that is already somebody's. That is the whole of it.
//!
//! It cannot log in — the greeter asks for a password and always will, because
//! at the login screen the machine does not yet know whose session it would be
//! and a finger is an answer to "is this you?", never to "who are you?" on a
//! sensor that holds a handful of templates. It cannot authorise anything. And
//! it is never the only way past a lock screen: the password field stays on
//! screen, focused, the entire time. A reader that has broken, got dirty, or
//! decided today that this is not the finger it enrolled must be an
//! inconvenience and never a locked-out machine, which is exactly what
//! [`Gate`] is for.
//!
//! # Where the answer comes from
//!
//! Not from here. This crate says whether the sensor matched; `ravend` says
//! whether that is enough to unlock, because `ravend` is the only process that
//! may say so and the lock screen is not to be trusted with its own answer.
//! See `docs/fingerprint.md` for the protocol that carries it.
//!
//! # The two stacks, and finding out which one is here
//!
//! RavenLinux's own driver is `raven-fprintd`, built from the RavenLinux tree
//! and talking to the reader over usbfs with no `libfprint` and no `libusb`.
//! Where a machine does not have it — an image built before it existed, or a
//! reader it does not drive — `rvn` installs the conventional `fprintd`, since
//! RavenLinux shares Arch's package names. [`Stack`] says which is here,
//! prefers Raven's own, and hands back the command that would install the other
//! rather than installing anything itself.
//!
//! # Status
//!
//! The rules below are done and tested. The client that talks to a running
//! daemon is not written yet: the sensor is root's, so a session reaches it
//! through `ravend`, and that is RavenLogin's half. [`fake::Fake`] is the
//! [`Sensor`] this crate ships, and it is honest about being a fake rather than
//! pretending to scan. See `docs/fingerprint.md`.

#![forbid(unsafe_code)]

pub mod enrol;
pub mod fake;
pub mod gate;
pub mod stack;

pub use enrol::{Enrolment, Progress};
pub use gate::{Gate, Reason, Verdict};
pub use stack::Stack;

/// Which finger a template was taken from.
///
/// Stored so that a person with two fingers enrolled can be told which one did
/// not match, and so that re-enrolling a finger replaces it rather than filling
/// the sensor with duplicates. The order is the one every other fingerprint
/// stack uses, which matters only because it is the order the hardware's own
/// tooling prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Finger {
    LeftThumb,
    LeftIndex,
    LeftMiddle,
    LeftRing,
    LeftLittle,
    RightThumb,
    RightIndex,
    RightMiddle,
    RightRing,
    RightLittle,
}

impl Finger {
    /// Every finger, in a fixed order, for a picker to list.
    pub const ALL: [Self; 10] = [
        Self::LeftThumb,
        Self::LeftIndex,
        Self::LeftMiddle,
        Self::LeftRing,
        Self::LeftLittle,
        Self::RightThumb,
        Self::RightIndex,
        Self::RightMiddle,
        Self::RightRing,
        Self::RightLittle,
    ];

    /// What to call it on screen.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LeftThumb => "left thumb",
            Self::LeftIndex => "left index",
            Self::LeftMiddle => "left middle",
            Self::LeftRing => "left ring",
            Self::LeftLittle => "left little",
            Self::RightThumb => "right thumb",
            Self::RightIndex => "right index",
            Self::RightMiddle => "right middle",
            Self::RightRing => "right ring",
            Self::RightLittle => "right little",
        }
    }
}

/// What came of one presentation of a finger.
///
/// The distinction that matters is between *this reading was no good* and *this
/// reading was good and was not you*. Everything in [`Self::Retry`] is the
/// first: the sensor saw something and could not use it, so the right response
/// is to ask again and not to count an attempt. Only [`Self::NoMatch`] is the
/// second.
///
/// Getting this wrong in the obvious direction — treating a smudged read as a
/// failed match — is what makes a fingerprint reader feel like it is accusing
/// its owner of being someone else, and burns the three tries in [`Gate`] on
/// readings nobody could have matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// A clean reading. During enrolment it advances a stage; during
    /// verification it matched.
    Good,
    /// A reading the sensor could not use. Ask for the finger again; nothing
    /// is counted.
    Retry(Retry),
    /// A clean reading of a finger the sensor does not know. This one counts.
    NoMatch,
}

/// Why a reading was no good, in the sensor's own words.
///
/// Surfaced to the person because the corrections are different and each one
/// is actionable: "hold it there a moment" is not "move it a little".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Lifted before the sensor had finished reading.
    TooShort,
    /// Not enough of the finger on the sensor — the edge of a fingertip, or a
    /// finger laid across a corner of it.
    OffCentre,
    /// Wet, dry, dirty, or a reader that wants wiping. The sensor cannot tell
    /// which of those it is, so neither can this.
    Unreadable,
    /// The same reading as the last one: the finger did not move between two
    /// presentations, and a template built from ten copies of one patch of
    /// skin matches almost nothing.
    Unchanged,
}

/// Something wrong with the sensor or the machine, rather than with a finger.
///
/// Kept apart from [`Scan`] for the reason `raven-auth` keeps "cannot read
/// /etc/shadow" apart from "wrong password": a reader that has been unplugged
/// is a broken machine and a finger that did not match is an answer, and a
/// screen that reports them the same way teaches its owner to ignore both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No reader is attached, or the one that was has gone.
    NoSensor,
    /// The reader is there and will not talk — a transport error, a timeout, a
    /// device that answered something this does not understand.
    Unreachable(String),
    /// The sensor's template store is full. Match-on-chip readers hold very
    /// few: a handful, not a database.
    Full,
    /// There is no template for the finger asked about.
    NotEnrolled(Finger),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSensor => f.write_str("no fingerprint reader"),
            Self::Unreachable(why) => write!(f, "the fingerprint reader is not answering: {why}"),
            Self::Full => f.write_str("the fingerprint reader has no room for another finger"),
            Self::NotEnrolled(finger) => write!(f, "no {} is enrolled", finger.label()),
        }
    }
}

impl std::error::Error for Error {}

/// A fingerprint reader.
///
/// One trait, and the only thing in this crate that touches hardware — which
/// it does in some other crate, because nothing here does. Every method
/// blocks: a finger takes as long as it takes, and a reader is spoken to from
/// a thread of its own for the same reason `huginn-comp` talks to BlueZ from
/// one. Nothing on a frame loop calls any of this.
///
/// # Match on chip
///
/// The readers this was written for — the Elan sensor in the machine it was
/// written on among them — match on the chip. The template never leaves the
/// device, the host never sees an image of a fingerprint, and [`Self::verify`]
/// asks the sensor a question rather than doing any comparing itself. That is
/// worth keeping: a host that cannot read the template cannot leak it, and the
/// trait is shaped so that a sensor which works the other way has to bring its
/// own storage rather than handing biometric data up to a caller that has
/// nowhere safe to put it.
pub trait Sensor {
    /// How many times a finger must be presented to enrol it.
    ///
    /// The sensor's own number. It varies by an order of magnitude between
    /// readers, it is the difference between an enrolment that feels brisk and
    /// one that feels broken, and a progress bar that made it up would be a
    /// progress bar that lies.
    fn stages(&self) -> u8;

    /// Take one reading towards enrolling `finger`.
    ///
    /// Called [`Self::stages`] times, or more when readings come back
    /// [`Scan::Retry`]. The sensor accumulates; [`Enrolment`] counts.
    fn enrol_step(&mut self, finger: Finger) -> Result<Scan, Error>;

    /// Abandon an enrolment in progress, discarding whatever the sensor has
    /// accumulated so far.
    ///
    /// Must be safe to call when no enrolment is in progress: it is what a
    /// dropped [`Enrolment`] and a cancelled dialog both do, and neither is in
    /// a position to know.
    fn enrol_abandon(&mut self);

    /// Take one reading and ask the sensor whether it is a finger it knows.
    ///
    /// [`Scan::Good`] means it matched something enrolled. Which finger it was
    /// is deliberately not returned: nothing needs to know, and a sensor that
    /// reported it would be a sensor that could be asked which fingers a person
    /// has by presenting a stranger's hand to it.
    fn verify(&mut self) -> Result<Scan, Error>;

    /// Which fingers have templates on this sensor.
    fn enrolled(&self) -> Result<Vec<Finger>, Error>;

    /// Forget `finger`'s template.
    fn forget(&mut self, finger: Finger) -> Result<(), Error>;

    /// Forget every template. What a person leaving a machine does, and what
    /// an installer does to one that arrived with somebody else's finger on it.
    fn forget_all(&mut self) -> Result<(), Error>;
}
