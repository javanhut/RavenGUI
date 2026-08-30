//! Quick settings: the small panel of things a desktop needs to change often.
//!
//! Compositor-drawn like the launcher, and for the same reason — it must never
//! be the thing that failed to appear. Deliberately small: §1's non-goals rule
//! out matching GNOME's settings surface area, so this is audio, network,
//! brightness, and the handful of switches that change how the desktop behaves.
//!
//! # Backends
//!
//! Wi-Fi talks to NetworkManager and Bluetooth to BlueZ, both over D-Bus,
//! which is a large dependency surface and hardware to test against. Each
//! control sits behind a trait here with a fake implementation, so the panel
//! can be built and judged before any of that exists — and so the real ones
//! swap in without the UI changing. Bluetooth is wired up, through
//! [`crate::bluetooth`]; Wi-Fi is not yet. What is fake says so on screen
//! rather than pretending.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use huginn_core::geometry::Rect;

use crate::anim::{Animated, Curve};
use crate::canvas::{Canvas, Panel};
use crate::text::Text;

/// Whether the desktop animates.
///
/// Runtime state, not a setting read from disk — there is no configuration
/// file and §11 says there will not be. Reduced motion is here because it is
/// an accessibility need rather than a preference: vestibular disorders make
/// large sliding transitions genuinely unpleasant, and "opinionated" is a
/// statement about taste, not about who gets to use the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Motion {
    #[default]
    Full,
    Reduced,
}

impl Motion {
    /// The duration to actually use for an animation nominally `wanted` long.
    ///
    /// Zero under reduced motion, which every animation already handles: a
    /// zero-length [`Animated`] is simply already finished. That is why there
    /// is no `if reduced { skip }` at any call site — the one place motion is
    /// turned off is here.
    pub(crate) fn duration(self, wanted: Duration) -> Duration {
        match self {
            Self::Full => wanted,
            Self::Reduced => Duration::ZERO,
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Full => Self::Reduced,
            Self::Reduced => Self::Full,
        }
    }
}

/// How long the session waits before locking itself.
///
/// A closed set rather than a number, because this row is stepped with one key
/// and there is nowhere to type into. The values are the ones people actually
/// want: long enough to read a page, short enough to matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IdleAfter {
    /// Never lock on idle. For a machine giving a presentation, or playing a
    /// film — there is no idle-inhibit protocol here, so a video is
    /// indistinguishable from an empty room and this is the only way to say
    /// which it is.
    Off,
    Minutes5,
    #[default]
    Minutes10,
    Minutes15,
    Minutes30,
}

impl IdleAfter {
    /// How long to wait, or `None` for never.
    pub(crate) fn duration(self) -> Option<Duration> {
        let minutes = match self {
            Self::Off => return None,
            Self::Minutes5 => 5,
            Self::Minutes10 => 10,
            Self::Minutes15 => 15,
            Self::Minutes30 => 30,
        };
        Some(Duration::from_secs(minutes * 60))
    }

    /// What the row shows on its right.
    ///
    /// Paired with [`Self::from_value`] and tested to round-trip: the panel
    /// stores a control's state inside the control and reads it back out
    /// through its display string, so a label and its parse that disagree
    /// would be a setting that silently reverts.
    fn value(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Minutes5 => "5 min",
            Self::Minutes10 => "10 min",
            Self::Minutes15 => "15 min",
            Self::Minutes30 => "30 min",
        }
    }

    /// The closest setting to a number of minutes, for `desktop.toml`.
    /// Zero is never; anything else rounds up to the next step, so a request
    /// to lock sooner is never quietly made later.
    pub(crate) fn from_minutes(minutes: u32) -> Self {
        match minutes {
            0 => Self::Off,
            1..=5 => Self::Minutes5,
            6..=10 => Self::Minutes10,
            11..=15 => Self::Minutes15,
            _ => Self::Minutes30,
        }
    }

    fn from_value(value: &str) -> Option<Self> {
        [
            Self::Off,
            Self::Minutes5,
            Self::Minutes10,
            Self::Minutes15,
            Self::Minutes30,
        ]
        .into_iter()
        .find(|candidate| candidate.value() == value)
    }

    /// The next setting, wrapping. Off comes last, so stepping through the
    /// row does not pass through "never lock" on the way to a longer wait.
    fn stepped(self) -> Self {
        match self {
            Self::Minutes5 => Self::Minutes10,
            Self::Minutes10 => Self::Minutes15,
            Self::Minutes15 => Self::Minutes30,
            Self::Minutes30 => Self::Off,
            Self::Off => Self::Minutes5,
        }
    }
}

/// A control's current reading, and whether it is real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    /// What to show on the right of the row.
    pub value: String,
    /// False when this came from a stub rather than from the system.
    ///
    /// Shown, not hidden. A panel that displays invented Wi-Fi networks
    /// indistinguishable from real ones is worse than one that admits it is
    /// not wired up yet — the second is unfinished, the first is wrong.
    pub real: bool,
}

/// Something the panel can read and toggle.
pub(crate) trait Control: std::fmt::Debug {
    /// The row's label.
    fn label(&self) -> &str;
    /// What to show, and whether it is real.
    fn read(&self) -> Reading;
    /// Act on the row. Returns whether anything changed.
    fn activate(&mut self) -> bool;
    /// Nudge the row by `delta` steps, for the arrows. Returns whether
    /// anything changed; most rows have nothing to nudge.
    fn adjust(&mut self, _delta: i32) -> bool {
        false
    }
    /// The row's position along a slider, 0..=1, if it is the kind of row
    /// that has one. Drawn under the label; the reading stays on the right.
    fn slider(&self) -> Option<f32> {
        None
    }
    /// What time it is and whether the desktop animates, for the one row
    /// that pops something up when it is moved. A switch has no use for
    /// either, so the default is to ignore both.
    fn set_clock(&mut self, _now: Duration, _motion: Motion) {}
    /// Stand down anything the row had primed, because the highlight left
    /// it or the panel went away. Returns whether there was anything to
    /// stand down; most rows never arm, so the default is nothing.
    fn disarm(&mut self) -> bool {
        false
    }
    /// Whether the last [`Self::activate`] finished what the row is for, so
    /// the panel should get out of the way rather than stay up waiting for
    /// a next step. A toggle is never finished in that sense.
    fn concluded(&self) -> bool {
        false
    }
    /// A program the row wants started, once. The panel cannot spawn — that
    /// takes the compositor's socket — so it hands the name up and the
    /// compositor launches it after the press is handled.
    fn take_launch(&mut self) -> Option<&'static str> {
        None
    }
}

/// The row that leaves the panel for the settings application, which is
/// where everything this panel is too small for lives.
#[derive(Debug, Default)]
struct AllSettings {
    requested: bool,
}

impl Control for AllSettings {
    fn label(&self) -> &str {
        "All settings"
    }
    fn read(&self) -> Reading {
        Reading {
            value: "Open".to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.requested = true;
        true
    }
    fn concluded(&self) -> bool {
        self.requested
    }
    fn take_launch(&mut self) -> Option<&'static str> {
        self.requested.then_some(crate::theme::SETTINGS_APP)
    }
}

/// The output volume. Real, through whatever [`crate::audio::Volume`] found.
///
/// Shares the compositor's own volume rather than keeping one, so the media
/// keys and this row move the same number and the slider that pops up for
/// the keys shows what this row shows.
#[derive(Debug)]
struct VolumeRow {
    volume: crate::audio::Shared,
    /// The clock, for the slider's reveal. The row is stepped from
    /// [`Settings::press`], which has `now`; the control trait does not.
    now: Duration,
    motion: Motion,
}

impl Control for VolumeRow {
    fn label(&self) -> &str {
        "Volume"
    }
    fn read(&self) -> Reading {
        let volume = self.volume.borrow();
        Reading {
            value: volume.level().caption(),
            real: volume.is_real(),
        }
    }
    fn activate(&mut self) -> bool {
        self.volume.borrow_mut().toggle_mute(self.now, self.motion);
        true
    }
    fn adjust(&mut self, delta: i32) -> bool {
        let before = self.volume.borrow().level();
        self.volume
            .borrow_mut()
            .adjust(delta * crate::audio::STEP as i32, self.now, self.motion);
        self.volume.borrow().level() != before
    }
    fn slider(&self) -> Option<f32> {
        Some(self.volume.borrow().level().fraction())
    }
    fn set_clock(&mut self, now: Duration, motion: Motion) {
        self.now = now;
        self.motion = motion;
    }
}

/// Brightness, as a percentage.
///
/// A stub: the real implementation writes `/sys/class/backlight/*/brightness`
/// or asks logind, and needs the session to own the device.
#[derive(Debug)]
struct Brightness {
    percent: u32,
}

impl Control for Brightness {
    fn label(&self) -> &str {
        "Brightness"
    }
    fn read(&self) -> Reading {
        Reading {
            value: format!("{}%", self.percent),
            real: false,
        }
    }
    fn activate(&mut self) -> bool {
        // Steps rather than a slider until there is pointer input to drag one.
        self.percent = match self.percent {
            0..=24 => 25,
            25..=49 => 50,
            50..=74 => 75,
            75..=99 => 100,
            _ => 0,
        };
        true
    }
}

/// Wi-Fi. A stub until NetworkManager is wired up over D-Bus.
#[derive(Debug)]
struct WiFi {
    on: bool,
}

impl Control for WiFi {
    fn label(&self) -> &str {
        "Wi-Fi"
    }
    fn read(&self) -> Reading {
        Reading {
            value: if self.on { "On" } else { "Off" }.to_owned(),
            real: false,
        }
    }
    fn activate(&mut self) -> bool {
        self.on = !self.on;
        true
    }
}

/// Bluetooth, through [`crate::bluetooth`].
///
/// One row that is a switch, a list and a question, because the panel is
/// rows and nothing else. Return on the row as it stands toggles the radio.
/// The arrows step through what the row could become instead — Off, On,
/// each device BlueZ knows, and Scan — shown with a question mark the way
/// the Power row shows its choice, and Return applies it: a device is
/// connected, or disconnected if it was, or paired first if it never has
/// been. While a scan runs the list grows as devices are found. When
/// pairing needs a number confirmed, the row shows the number and Return
/// is yes; moving off the row is no.
#[derive(Debug)]
struct BluetoothRow {
    backend: Box<dyn crate::bluetooth::Backend>,
    /// Which choice the arrows have stepped to, if any. `None` shows the
    /// reading; `Some` shows a candidate waiting for Return.
    cursor: Option<usize>,
}

/// What the arrows can step the Bluetooth row to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BtChoice {
    Off,
    On,
    Device(crate::bluetooth::Device),
    Scan,
    StopScan,
}

impl BtChoice {
    fn label(&self) -> String {
        match self {
            Self::Off => "Off".to_owned(),
            Self::On => "On".to_owned(),
            Self::Device(d) => d.name.clone(),
            Self::Scan => "Scan".to_owned(),
            Self::StopScan => "Stop scan".to_owned(),
        }
    }
}

/// The most of a device name that fits on the right of a row.
const BT_NAME_CHARS: usize = 22;

fn shorten(name: &str) -> String {
    if name.chars().count() <= BT_NAME_CHARS {
        return name.to_owned();
    }
    let mut short: String = name.chars().take(BT_NAME_CHARS - 1).collect();
    short.push('…');
    short
}

impl BluetoothRow {
    fn new(backend: Box<dyn crate::bluetooth::Backend>) -> Self {
        Self {
            backend,
            cursor: None,
        }
    }

    fn choices(&self, state: &crate::bluetooth::State) -> Vec<BtChoice> {
        if !state.available {
            return Vec::new();
        }
        let mut choices = vec![BtChoice::Off, BtChoice::On];
        // Paired devices are always worth offering; unpaired ones only while
        // a scan is finding them, so the list is not a memory of every
        // phone that ever walked past.
        choices.extend(
            state
                .devices
                .iter()
                .filter(|d| d.paired || state.discovering)
                .cloned()
                .map(BtChoice::Device),
        );
        choices.push(if state.discovering {
            BtChoice::StopScan
        } else {
            BtChoice::Scan
        });
        choices
    }

    /// What the row says with nothing stepped or pending.
    fn reading(state: &crate::bluetooth::State) -> String {
        if !state.powered {
            return "Off".to_owned();
        }
        if let Some(connected) = state.devices.iter().find(|d| d.connected) {
            return shorten(&connected.name);
        }
        if state.discovering {
            return "Scanning…".to_owned();
        }
        "On".to_owned()
    }
}

impl Control for BluetoothRow {
    fn label(&self) -> &str {
        "Bluetooth"
    }
    fn read(&self) -> Reading {
        use crate::bluetooth::Prompt;
        let state = self.backend.state();
        if !state.available {
            return Reading {
                value: "Off".to_owned(),
                real: false,
            };
        }
        let value = match (&state.prompt, &state.busy, &state.error, self.cursor) {
            (Some(Prompt::Confirm { passkey, .. }), ..) => format!("Confirm {passkey:06}?"),
            (Some(Prompt::Display { passkey, device }), ..) => {
                format!("Type {passkey:06} on {}", shorten(device))
            }
            (None, Some(busy), ..) => format!("{busy}…"),
            (None, None, Some(error), _) => shorten(error),
            (None, None, None, Some(cursor)) => match self.choices(&state).get(cursor) {
                Some(choice) => format!("{}?", shorten(&choice.label())),
                None => Self::reading(&state),
            },
            (None, None, None, None) => Self::reading(&state),
        };
        Reading { value, real: true }
    }
    fn activate(&mut self) -> bool {
        use crate::bluetooth::{Command, Prompt};
        let state = self.backend.state();
        if !state.available {
            return false;
        }
        match state.prompt {
            Some(Prompt::Confirm { .. }) => {
                self.backend.answer(true);
                return true;
            }
            // Nothing to say to a number one has to type elsewhere.
            Some(Prompt::Display { .. }) => return false,
            None => {}
        }
        if state.busy.is_some() {
            return false;
        }
        if state.error.is_some() {
            self.backend.send(Command::ClearError);
            self.cursor = None;
            return true;
        }
        let command = match self
            .cursor
            .take()
            .and_then(|c| self.choices(&state).into_iter().nth(c))
        {
            None => Command::Power(!state.powered),
            Some(BtChoice::Off) => Command::Power(false),
            Some(BtChoice::On) => Command::Power(true),
            Some(BtChoice::Scan) => Command::Scan(true),
            Some(BtChoice::StopScan) => Command::Scan(false),
            Some(BtChoice::Device(d)) if d.connected => Command::Disconnect(d.path),
            Some(BtChoice::Device(d)) if d.paired => Command::Connect(d.path),
            Some(BtChoice::Device(d)) => Command::Pair(d.path),
        };
        self.backend.send(command);
        true
    }
    fn adjust(&mut self, delta: i32) -> bool {
        let state = self.backend.state();
        if state.prompt.is_some() || state.busy.is_some() {
            return false;
        }
        if state.error.is_some() {
            self.backend.send(crate::bluetooth::Command::ClearError);
        }
        let choices = self.choices(&state);
        if choices.is_empty() {
            return false;
        }
        let len = choices.len() as i32;
        // From the reading, Right goes to the first choice and Left to the
        // last; after that the arrows wrap, so a long list is never a long
        // way back.
        let next = match self.cursor {
            None if delta > 0 => (delta - 1).rem_euclid(len),
            None => delta.rem_euclid(len),
            Some(c) => (c as i32 + delta).rem_euclid(len),
        };
        self.cursor = Some(next as usize);
        true
    }
    fn disarm(&mut self) -> bool {
        use crate::bluetooth::{Command, Prompt};
        let state = self.backend.state();
        let mut stood_down = self.cursor.take().is_some();
        if let Some(Prompt::Confirm { .. }) = state.prompt {
            self.backend.answer(false);
            stood_down = true;
        }
        if state.error.is_some() {
            self.backend.send(Command::ClearError);
            stood_down = true;
        }
        // A scan only makes sense with the list in front of somebody; leaving
        // the row leaves the list.
        if state.discovering {
            self.backend.send(Command::Scan(false));
            stood_down = true;
        }
        stood_down
    }
}

/// The animations switch. The one control here that is genuinely wired up.
#[derive(Debug)]
struct Animations {
    motion: Motion,
}

impl Control for Animations {
    fn label(&self) -> &str {
        "Animations"
    }
    fn read(&self) -> Reading {
        Reading {
            value: match self.motion {
                Motion::Full => "On",
                Motion::Reduced => "Reduced",
            }
            .to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.motion = self.motion.toggled();
        true
    }
}

/// The idle lock. Wired up, like [`Animations`] and unlike the rest.
#[derive(Debug)]
struct IdleLock {
    after: IdleAfter,
}

impl Control for IdleLock {
    fn label(&self) -> &str {
        "Lock when idle"
    }
    fn read(&self) -> Reading {
        Reading {
            value: self.after.value().to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.after = self.after.stepped();
        true
    }
}

/// Where the pinned panel sits. Wired up: the compositor reads it back
/// through [`Settings::pins_position`] and writes it to [`crate::pins`],
/// which is what the panel and the file take it from.
#[derive(Debug)]
struct PinsPosition {
    position: crate::pins::Position,
}

impl Control for PinsPosition {
    fn label(&self) -> &str {
        PINS_POSITION
    }
    fn read(&self) -> Reading {
        Reading {
            value: self.position.value().to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.position = self.position.stepped(1);
        true
    }
    fn adjust(&mut self, delta: i32) -> bool {
        self.position = self.position.stepped(delta);
        true
    }
}

/// How the pinned panel lays its applications out. As [`PinsPosition`].
#[derive(Debug)]
struct PinsLayout {
    orientation: crate::pins::Orientation,
}

impl Control for PinsLayout {
    fn label(&self) -> &str {
        PINS_LAYOUT
    }
    fn read(&self) -> Reading {
        Reading {
            value: self.orientation.value().to_owned(),
            real: true,
        }
    }
    fn activate(&mut self) -> bool {
        self.orientation = self.orientation.stepped(1);
        true
    }
    fn adjust(&mut self, delta: i32) -> bool {
        self.orientation = self.orientation.stepped(delta);
        true
    }
}

/// The pinned panel rows' labels, which are also how the rows are found.
const PINS_POSITION: &str = "Pinned apps";
const PINS_LAYOUT: &str = "Pinned layout";

/// What the Power row can ask the machine to do.
///
/// A closed set stepped with one key, like [`IdleAfter`]. Suspend comes
/// first and is the default because it is the one that is asked for daily and
/// the one that is cheapest to have asked for by mistake: a suspended laptop
/// is back in a second, a rebooted one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PowerAction {
    #[default]
    Suspend,
    PowerOff,
    Reboot,
}

impl PowerAction {
    /// What the row shows.
    fn label(self) -> &'static str {
        match self {
            Self::Suspend => "Suspend",
            Self::PowerOff => "Power off",
            Self::Reboot => "Reboot",
        }
    }

    /// The word raven-powerd accepts on its control socket. Its vocabulary,
    /// not ours: the daemon is the policy gatekeeper and this is a request.
    fn verb(self) -> &'static str {
        match self {
            Self::Suspend => "suspend",
            Self::PowerOff => "poweroff",
            Self::Reboot => "reboot",
        }
    }

    /// The next action, wrapping, in order of how much is lost by mistake.
    fn stepped(self) -> Self {
        match self {
            Self::Suspend => Self::PowerOff,
            Self::PowerOff => Self::Reboot,
            Self::Reboot => Self::Suspend,
        }
    }
}

/// Where the Power row's verb goes.
///
/// A trait for the reason every other backend here is one: the real target is
/// a socket that only exists on a Raven machine with raven-powerd running, and
/// the thing worth testing — that a first press never sends anything and a
/// second sends exactly one word — must be checkable on a machine that would
/// actually reboot if the test got it wrong.
pub(crate) trait PowerSender: std::fmt::Debug {
    /// Whether there is anything on the other end right now.
    fn is_available(&self) -> bool;
    /// Send one verb and return the daemon's one-line reply.
    fn send(&mut self, verb: &str) -> std::io::Result<String>;
}

/// raven-powerd's control socket for the desktop session.
///
/// Group-owned by `video`, which the session's user is already in for the
/// display, so the compositor can reach it without any privilege it does not
/// already hold. Init's own socket is a different file and stays root-only:
/// the desktop gets a verb into the daemon that already decides whether the
/// machine may sleep, never a line into PID 1.
#[derive(Debug, Default)]
struct PowerSocket;

impl PowerSocket {
    const PATH: &'static str = "/run/raven-power/ctl";
}

impl PowerSender for PowerSocket {
    fn is_available(&self) -> bool {
        // Asked at read time rather than remembered: the daemon may start
        // after the compositor, and a row that decided at startup that it was
        // a stub would stay one for the whole session.
        Path::new(Self::PATH).exists()
    }

    fn send(&mut self, verb: &str) -> std::io::Result<String> {
        let mut stream = UnixStream::connect(Self::PATH)?;
        // The daemon replies before it acts, so a reply that takes long is a
        // daemon that is wedged, and the compositor must not hang with it.
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(format!("{verb}\n").as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        Ok(reply.trim_end().to_owned())
    }
}

/// A sender that records instead of sending, for tests.
#[derive(Debug, Default)]
struct FakePower {
    sent: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl PowerSender for FakePower {
    fn is_available(&self) -> bool {
        true
    }
    fn send(&mut self, verb: &str) -> std::io::Result<String> {
        self.sent.borrow_mut().push(verb.to_owned());
        Ok("ok".to_owned())
    }
}

/// Suspend, power off or reboot, through raven-powerd.
///
/// Two presses, not one. Every other row here is safe to activate by accident
/// — a toggle can be toggled back — but there is no undo for a reboot, and
/// Return on the last row of a panel somebody was stepping through with the
/// arrows is exactly the kind of press that happens by accident. So the first
/// Activate arms the row, shown as a question, and only the second sends;
/// moving off the row or dismissing the panel stands it down again.
#[derive(Debug)]
struct PowerRow {
    action: PowerAction,
    armed: bool,
    /// The daemon's answer when it refused or could not be reached, shown in
    /// place of the label until the highlight moves. Cleared on disarm.
    error: Option<String>,
    /// Set once a verb has gone out, so the panel closes behind it.
    sent: bool,
    sender: Box<dyn PowerSender>,
}

impl PowerRow {
    fn new(sender: Box<dyn PowerSender>) -> Self {
        Self {
            action: PowerAction::default(),
            armed: false,
            error: None,
            sent: false,
            sender,
        }
    }
}

impl Control for PowerRow {
    fn label(&self) -> &str {
        "Power"
    }
    fn read(&self) -> Reading {
        let value = match &self.error {
            Some(error) => error.clone(),
            None if self.armed => format!("{}?", self.action.label()),
            None => self.action.label().to_owned(),
        };
        Reading {
            value,
            real: self.sender.is_available(),
        }
    }
    fn activate(&mut self) -> bool {
        self.sent = false;
        if !self.armed {
            self.armed = true;
            self.error = None;
            return true;
        }
        self.armed = false;
        match self.sender.send(self.action.verb()) {
            Ok(reply) if reply.starts_with("error") => {
                tracing::warn!(verb = self.action.verb(), reply, "raven-powerd refused");
                self.error = Some(reply);
            }
            Ok(reply) => {
                tracing::info!(verb = self.action.verb(), reply, "asked raven-powerd");
                self.sent = true;
            }
            Err(err) => {
                tracing::warn!(verb = self.action.verb(), %err, "raven-powerd unreachable");
                self.error = Some(format!("error: {err}"));
            }
        }
        true
    }
    fn adjust(&mut self, delta: i32) -> bool {
        // Stepping the choice is also a change of mind about the armed one.
        self.armed = false;
        self.error = None;
        let steps = delta.rem_euclid(3);
        for _ in 0..steps {
            self.action = self.action.stepped();
        }
        steps != 0
    }
    fn disarm(&mut self) -> bool {
        let was = self.armed || self.error.is_some();
        self.armed = false;
        self.error = None;
        was
    }
    fn concluded(&self) -> bool {
        self.sent
    }
}

/// What a keystroke means to the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    /// Nudge the highlighted control down or up, if it is a slider.
    Left,
    Right,
    /// Toggle or step the highlighted control.
    Activate,
    Dismiss,
    Ignored,
}

impl Key {
    pub(crate) fn from_keysym(sym: u32) -> Self {
        use smithay::input::keyboard::keysyms;
        match sym {
            keysyms::KEY_Escape => Self::Dismiss,
            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_K => Self::Up,
            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_J => Self::Down,
            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_H | keysyms::KEY_minus => Self::Left,
            keysyms::KEY_Right
            | keysyms::KEY_l
            | keysyms::KEY_L
            | keysyms::KEY_plus
            | keysyms::KEY_equal => Self::Right,
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => Self::Activate,
            _ => Self::Ignored,
        }
    }
}

/// What the compositor should do after a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Unchanged,
    Redraw,
    Dismissed,
}

/// The quick settings panel.
#[derive(Debug)]
pub(crate) struct Settings {
    open: bool,
    selected: usize,
    controls: Vec<Box<dyn Control>>,
    /// 0 closed, 1 open. Drives the reveal.
    reveal: Animated,
}

impl Default for Settings {
    /// A panel with a volume of its own, connected to nothing. For tests;
    /// the compositor builds one with [`Settings::new`] around its volume.
    fn default() -> Self {
        Self::with_power(
            crate::audio::Volume::default().shared(),
            Box::new(FakePower::default()),
        )
    }
}

impl Settings {
    /// The panel, with its volume row driving `volume` and its Power row
    /// talking to raven-powerd.
    pub(crate) fn new(volume: crate::audio::Shared) -> Self {
        Self::with_power(volume, Box::new(PowerSocket))
    }

    /// The panel with a Power row that sends through `power`, so a test can
    /// press Return on it without leaving the machine off.
    fn with_power(volume: crate::audio::Shared, power: Box<dyn PowerSender>) -> Self {
        Self {
            open: false,
            selected: 0,
            controls: vec![
                Box::new(VolumeRow {
                    volume,
                    now: Duration::ZERO,
                    motion: Motion::default(),
                }),
                Box::new(Animations {
                    motion: Motion::default(),
                }),
                Box::new(IdleLock {
                    after: IdleAfter::default(),
                }),
                Box::new(PinsPosition {
                    position: crate::pins::Position::default(),
                }),
                Box::new(PinsLayout {
                    orientation: crate::pins::Orientation::default(),
                }),
                Box::new(Brightness { percent: 75 }),
                Box::new(WiFi { on: true }),
                Box::new(BluetoothRow::new(Box::new(crate::bluetooth::Unavailable))),
                Box::new(AllSettings::default()),
                // Last, so that stepping down through the panel arrives at it
                // deliberately and a stray Return on the first row is a mute,
                // not a shutdown.
                Box::new(PowerRow::new(power)),
            ],
            reveal: Animated::settled(0.0),
        }
    }
}

impl Settings {
    /// Give the Bluetooth row something real to talk to. Called once the
    /// BlueZ thread is up, which is after the panel exists.
    pub(crate) fn set_bluetooth(&mut self, backend: Box<dyn crate::bluetooth::Backend>) {
        if let Some(row) = self.controls.iter_mut().find(|c| c.label() == "Bluetooth") {
            *row = Box::new(BluetoothRow::new(backend));
        }
    }
}

impl Settings {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// How far the panel has slid in, 0..=1.
    pub(crate) fn reveal(&self, now: Duration) -> f32 {
        self.reveal.value(now)
    }

    /// Whether the reveal is still moving, so the frame loop keeps going.
    pub(crate) fn is_animating(&self, now: Duration) -> bool {
        !self.reveal.is_settled(now)
    }

    /// Whether the panel is on screen at all, mid-animation included.
    pub(crate) fn is_visible(&self, now: Duration) -> bool {
        self.open || self.reveal(now) > 0.001
    }

    /// The current motion setting, read from the control that owns it.
    pub(crate) fn motion(&self) -> Motion {
        self.controls
            .iter()
            .find_map(|c| match c.read().value.as_str() {
                _ if c.label() != "Animations" => None,
                "Reduced" => Some(Motion::Reduced),
                _ => Some(Motion::Full),
            })
            .unwrap_or_default()
    }

    /// How long before the session locks itself, read from the control that
    /// owns it.
    ///
    /// Same shape as [`Self::motion`]: the value lives in the control, because
    /// a panel that keeps a second copy of what a row displays is a panel with
    /// two answers to the same question.
    pub(crate) fn idle_after(&self) -> IdleAfter {
        self.controls
            .iter()
            .find(|c| c.label() == "Lock when idle")
            .and_then(|c| IdleAfter::from_value(&c.read().value))
            .unwrap_or_default()
    }

    /// Where the pinned panel sits, read from the control that owns it.
    pub(crate) fn pins_position(&self) -> crate::pins::Position {
        self.controls
            .iter()
            .find(|c| c.label() == PINS_POSITION)
            .and_then(|c| crate::pins::Position::from_value(&c.read().value))
            .unwrap_or_default()
    }

    /// How the pinned panel is laid out, read from the control that owns it.
    pub(crate) fn pins_orientation(&self) -> crate::pins::Orientation {
        self.controls
            .iter()
            .find(|c| c.label() == PINS_LAYOUT)
            .and_then(|c| crate::pins::Orientation::from_value(&c.read().value))
            .unwrap_or_default()
    }

    /// Give the Animations and Lock rows what `desktop.toml` said. Called at
    /// startup and whenever the file changes; between those the rows are the
    /// source, as with [`Self::set_pins_layout`].
    pub(crate) fn apply_desktop_config(&mut self, motion: Motion, after: IdleAfter) {
        for control in &mut self.controls {
            if control.label() == "Animations" {
                *control = Box::new(Animations { motion });
            } else if control.label() == "Lock when idle" {
                *control = Box::new(IdleLock { after });
            }
        }
    }

    /// A program a row asked for on its last activation, if any. Taken once.
    pub(crate) fn take_launch(&mut self) -> Option<&'static str> {
        self.controls.iter_mut().find_map(|c| c.take_launch())
    }

    /// Give the pinned rows what the file said. Called once at startup,
    /// after the pins are loaded; from then on the rows are the source and
    /// the compositor copies them out through the two readers above.
    pub(crate) fn set_pins_layout(
        &mut self,
        position: crate::pins::Position,
        orientation: crate::pins::Orientation,
    ) {
        for control in &mut self.controls {
            if control.label() == PINS_POSITION {
                *control = Box::new(PinsPosition { position });
            } else if control.label() == PINS_LAYOUT {
                *control = Box::new(PinsLayout { orientation });
            }
        }
    }

    pub(crate) fn open(&mut self, now: Duration) {
        self.open = true;
        self.selected = 0;
        // A row left armed when the panel was last closed must not still be
        // armed when it comes back: the person opening it now may not be the
        // one who pressed Return then.
        for control in &mut self.controls {
            control.disarm();
        }
        let motion = self.motion();
        self.reveal.animate_to(
            1.0,
            now,
            motion.duration(crate::anim::LAUNCHER_OPEN),
            Curve::Spring,
        );
    }

    pub(crate) fn close(&mut self, now: Duration) {
        self.open = false;
        let motion = self.motion();
        self.reveal.animate_to(
            0.0,
            now,
            motion.duration(crate::anim::PANEL_CLOSE),
            Curve::EaseOut,
        );
    }

    pub(crate) fn press(&mut self, key: Key, now: Duration) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        match key {
            Key::Dismiss => {
                for control in &mut self.controls {
                    control.disarm();
                }
                self.close(now);
                Outcome::Dismissed
            }
            Key::Up | Key::Down => {
                // Leaving a row stands it down, even when the highlight is
                // already at the end and has nowhere to go: the Power row is
                // last, and Down on it must still be a change of mind.
                let disarmed = self
                    .controls
                    .get_mut(self.selected)
                    .is_some_and(|control| control.disarm());
                let moved = self.move_selection(if key == Key::Up { -1 } else { 1 });
                if disarmed || moved == Outcome::Redraw {
                    Outcome::Redraw
                } else {
                    Outcome::Unchanged
                }
            }
            Key::Left | Key::Right => {
                let delta = if key == Key::Left { -1 } else { 1 };
                self.sync_clock(now);
                let Some(control) = self.controls.get_mut(self.selected) else {
                    return Outcome::Unchanged;
                };
                if control.adjust(delta) {
                    Outcome::Redraw
                } else {
                    Outcome::Unchanged
                }
            }
            Key::Activate => {
                self.sync_clock(now);
                let Some(control) = self.controls.get_mut(self.selected) else {
                    return Outcome::Unchanged;
                };
                if !control.activate() {
                    return Outcome::Unchanged;
                }
                if control.concluded() {
                    // The machine is about to sleep or go down; a panel still
                    // up over it would be the last thing on screen and the
                    // first thing on resume.
                    self.close(now);
                    return Outcome::Dismissed;
                }
                // Turning animations off must take effect on the panel that
                // turned them off, not on the next thing to animate — landing
                // the change one interaction late reads as the switch having
                // done nothing.
                if self.motion() == Motion::Reduced {
                    self.reveal.jump_to(1.0);
                }
                Outcome::Redraw
            }
            Key::Ignored => Outcome::Unchanged,
        }
    }

    /// Tell the rows what time it is and whether the desktop animates, so
    /// the slider the volume row pops up fades on the same clock as
    /// everything else. Done before a row is acted on, not when the panel
    /// opens: the animations switch can be flipped while it is open.
    fn sync_clock(&mut self, now: Duration) {
        let motion = self.motion();
        for control in &mut self.controls {
            control.set_clock(now, motion);
        }
    }

    fn move_selection(&mut self, delta: isize) -> Outcome {
        if self.controls.is_empty() {
            return Outcome::Unchanged;
        }
        let last = self.controls.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        if next == self.selected {
            return Outcome::Unchanged;
        }
        self.selected = next;
        Outcome::Redraw
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

const WIDTH: f32 = 340.0;
const PAD: f32 = 16.0;
const BASE_SIZE: f32 = 15.0;
const ALPHA: u8 = 0xF2;
const TITLE: &str = "Quick settings";
/// Marks a control that is not wired to anything yet.
const STUB: &str = "not connected";

/// Draw the panel for `output` at `density` pixels per logical one.
pub(crate) fn render(
    settings: &Settings,
    text: &mut Text,
    output: Rect,
    now: Duration,
    density: u32,
) -> Panel {
    Panel::from_canvas(&compose(settings, text, output, now, density), density)
}

fn compose(
    settings: &Settings,
    text: &mut Text,
    output: Rect,
    now: Duration,
    density: u32,
) -> Canvas {
    // In the canvas's own pixels, `density` times the logical ones. See
    // `Panel::from_canvas`.
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density.max(1) as f32;
    let size = BASE_SIZE * scale;
    let pad = PAD * scale;
    let width = (WIDTH * scale) as usize;
    let row = size * 2.4;
    let header = size * 2.2;

    let height = (pad * 2.0 + header + row * settings.controls.len() as f32) as usize;
    let mut canvas = Canvas::new(width, height.max(1));
    canvas.fill(
        0,
        0,
        width,
        height,
        crate::theme::BACKGROUND.with_alpha(ALPHA).to_rgba_bytes(),
    );
    canvas.frame(crate::theme::BORDER.to_rgba_bytes());

    text.draw(
        &mut canvas,
        TITLE,
        size,
        pad as i32,
        pad as i32,
        crate::theme::accent(),
    );
    canvas.fill(
        pad as usize,
        (pad + header - size * 0.6) as usize,
        width - (pad * 2.0) as usize,
        1,
        crate::theme::BORDER.to_rgba_bytes(),
    );

    let mut y = pad + header;
    for (index, control) in settings.controls.iter().enumerate() {
        let reading = control.read();
        let highlighted = index == settings.selected;
        if highlighted {
            canvas.tint(
                1,
                y as usize,
                width - 2,
                row as usize,
                crate::theme::accent(),
                0x2E,
            );
            canvas.fill(
                1,
                y as usize,
                (3.0 * scale) as usize,
                row as usize,
                crate::theme::accent().to_rgba_bytes(),
            );
        }

        let text_y = (y + (row - size * 1.35) / 2.0) as i32;
        // The value, right-aligned. A stub says so instead of showing a
        // plausible reading nobody can tell is invented.
        let shown = if reading.real {
            reading.value.clone()
        } else {
            format!("{} · {STUB}", reading.value)
        };
        let value_w = text.measure(&shown, size * 0.95).0;
        // A slider row draws its track between the label and the reading,
        // so what the arrows move is visible as a bar rather than as a
        // number changing.
        if let Some(fraction) = control.slider() {
            let track_h = 4.0 * scale;
            let label_w = text.measure(control.label(), size).0;
            let track_x = pad + 6.0 * scale + label_w + 14.0 * scale;
            let track_end = width as f32 - pad - value_w - 14.0 * scale;
            let track_w = track_end - track_x;
            if track_w > track_h * 2.0 {
                let track_y = y + row / 2.0 - track_h / 2.0;
                canvas.fill_rounded(
                    track_x as usize,
                    track_y as usize,
                    track_w as usize,
                    track_h as usize,
                    track_h / 2.0,
                    crate::theme::BORDER,
                );
                let filled = (track_w * fraction.clamp(0.0, 1.0)).round();
                if filled >= 1.0 {
                    canvas.fill_rounded(
                        track_x as usize,
                        track_y as usize,
                        filled as usize,
                        track_h as usize,
                        track_h / 2.0,
                        crate::theme::accent(),
                    );
                }
                let knob = 10.0 * scale;
                let knob_x = (track_x + filled - knob / 2.0).clamp(track_x, track_end - knob);
                canvas.fill_rounded(
                    knob_x as usize,
                    (track_y + track_h / 2.0 - knob / 2.0) as usize,
                    knob as usize,
                    knob as usize,
                    knob / 2.0,
                    if highlighted {
                        crate::theme::TEXT
                    } else {
                        crate::theme::TEXT_DIM
                    },
                );
            }
        }
        text.draw(
            &mut canvas,
            control.label(),
            size,
            (pad + 6.0 * scale) as i32,
            text_y,
            if highlighted {
                crate::theme::TEXT
            } else {
                crate::theme::TEXT_DIM
            },
        );

        text.draw(
            &mut canvas,
            &shown,
            size * 0.95,
            (width as f32 - pad - value_w) as i32,
            text_y,
            if reading.real {
                crate::theme::accent()
            } else {
                crate::theme::TEXT_DIM
            },
        );
        y += row;
    }

    // The reveal: fade the whole panel. Sliding it as well needs the dock it
    // slides out of, which does not exist yet.
    let reveal = settings.reveal(now).clamp(0.0, 1.0);
    if reveal < 1.0 {
        for pixel in canvas.pixels.chunks_exact_mut(4) {
            pixel[3] = (f32::from(pixel[3]) * reveal) as u8;
        }
    }

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn opened() -> Settings {
        let mut settings = Settings::default();
        settings.open(T0);
        settings
    }

    /// Where a row sits, by name.
    ///
    /// Tests used to index the control list directly, and adding a row in the
    /// middle silently repointed three of them at the wrong control — a test
    /// that still passed while checking something else entirely.
    fn index_of(settings: &Settings, label: &str) -> usize {
        settings
            .controls
            .iter()
            .position(|c| c.label() == label)
            .unwrap_or_else(|| panic!("no control labelled {label:?}"))
    }

    /// Move the highlight onto a row by name.
    fn select(settings: &mut Settings, label: &str) {
        let want = index_of(settings, label);
        for _ in 0..settings.controls.len() {
            if settings.selected == want {
                return;
            }
            settings.press(Key::Down, T0);
        }
        panic!("could not reach {label:?}");
    }

    #[test]
    fn the_pinned_rows_step_and_read_back() {
        use crate::pins::{Orientation, Position};
        let mut settings = opened();
        assert_eq!(settings.pins_position(), Position::Centre);
        assert_eq!(settings.pins_orientation(), Orientation::Grid);
        select(&mut settings, PINS_POSITION);
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Redraw);
        assert_eq!(settings.pins_position(), Position::Top);
        assert_eq!(settings.press(Key::Left, T0), Outcome::Redraw);
        assert_eq!(settings.pins_position(), Position::Centre);
        select(&mut settings, PINS_LAYOUT);
        settings.press(Key::Right, T0);
        assert_eq!(settings.pins_orientation(), Orientation::Row);
        // And what the file said at startup lands in the rows.
        settings.set_pins_layout(Position::Right, Orientation::Column);
        assert_eq!(settings.pins_position(), Position::Right);
        assert_eq!(settings.pins_orientation(), Orientation::Column);
    }

    #[test]
    fn opening_reveals_over_time_rather_than_appearing() {
        let settings = opened();
        assert!(settings.reveal(T0) < 0.5, "it was already there at t=0");
        assert!(settings.is_animating(T0));
        assert!((settings.reveal(ms(200)) - 1.0).abs() < 1e-3);
        assert!(!settings.is_animating(ms(200)));
    }

    #[test]
    fn closing_animates_out_and_then_stops_being_visible() {
        let mut settings = opened();
        assert!(settings.is_visible(ms(200)));
        settings.close(ms(200));
        assert!(
            settings.is_visible(ms(200)),
            "it vanished instead of closing"
        );
        assert!(!settings.is_visible(ms(500)));
    }

    /// The panel keeps a control's state inside the control and reads it back
    /// out through the string it displays, so a value and its parse that
    /// disagree would be a setting that silently reverts to the default.
    #[test]
    fn every_idle_setting_survives_the_round_trip() {
        for after in [
            IdleAfter::Off,
            IdleAfter::Minutes5,
            IdleAfter::Minutes10,
            IdleAfter::Minutes15,
            IdleAfter::Minutes30,
        ] {
            assert_eq!(IdleAfter::from_value(after.value()), Some(after));
        }
    }

    #[test]
    fn off_is_the_only_idle_setting_with_no_duration() {
        assert_eq!(IdleAfter::Off.duration(), None);
        assert_eq!(
            IdleAfter::Minutes10.duration(),
            Some(Duration::from_secs(600))
        );
    }

    /// Stepping must not pass through "never lock" on the way to a longer
    /// wait: somebody lengthening the timeout should not silently disable it
    /// in passing, however briefly.
    #[test]
    fn stepping_reaches_off_only_at_the_end() {
        // Off sits after the longest wait and before the shortest, so somebody
        // stepping the row to lengthen the timeout never passes through
        // "never lock" on the way — the setting is only ever turned off on
        // purpose.
        assert_eq!(IdleAfter::Minutes30.stepped(), IdleAfter::Off);
        assert_eq!(IdleAfter::Off.stepped(), IdleAfter::Minutes5);

        // And every setting is reachable: a row that could not be stepped back
        // to where it started would be one somebody could not undo.
        let mut seen = vec![IdleAfter::default()];
        while seen.len() < 6 {
            seen.push(seen[seen.len() - 1].stepped());
        }
        assert_eq!(seen[5], IdleAfter::default(), "the cycle must close");
        for wanted in [
            IdleAfter::Off,
            IdleAfter::Minutes5,
            IdleAfter::Minutes10,
            IdleAfter::Minutes15,
            IdleAfter::Minutes30,
        ] {
            assert!(
                seen.contains(&wanted),
                "{wanted:?} is unreachable: {seen:?}"
            );
        }
    }

    #[test]
    fn the_idle_row_is_wired_up_and_steps() {
        let mut settings = opened();
        assert_eq!(settings.idle_after(), IdleAfter::default());
        select(&mut settings, "Lock when idle");
        settings.press(Key::Activate, T0);
        assert_ne!(
            settings.idle_after(),
            IdleAfter::default(),
            "activating the row must change what the compositor reads"
        );
    }

    #[test]
    fn every_stub_admits_that_it_is_one() {
        // A panel that shows invented readings indistinguishable from
        // measurements is worse than one that admits it is not wired up, so
        // the thing worth pinning is which rows claim to be real. Asserted as
        // the stub list rather than the real one: a row wired up later should
        // move off this list, and a row *added* as a stub without saying so
        // should fail here.
        let settings = Settings::default();
        let stubs: Vec<&str> = settings
            .controls
            .iter()
            .filter(|c| !c.read().real)
            .map(|c| c.label())
            .collect();
        // Volume and Bluetooth are on the list only because a default panel
        // has a silent mixer and no BlueZ thread behind it; on a machine
        // with PipeWire and bluetoothd those rows are real.
        assert_eq!(stubs, ["Volume", "Brightness", "Wi-Fi", "Bluetooth"]);
    }

    #[test]
    fn turning_animations_off_takes_effect_immediately() {
        // Landing the change on the *next* interaction reads as the switch
        // having done nothing at all.
        let mut settings = opened();
        select(&mut settings, "Animations");
        assert_eq!(settings.motion(), Motion::Full);
        assert_eq!(settings.press(Key::Activate, ms(10)), Outcome::Redraw);
        assert_eq!(settings.motion(), Motion::Reduced);
        assert!(
            (settings.reveal(ms(10)) - 1.0).abs() < 1e-5,
            "the panel was still mid-animation after motion was reduced"
        );
        assert!(!settings.is_animating(ms(10)));
    }

    #[test]
    fn reduced_motion_makes_the_next_reveal_instant() {
        let mut settings = opened();
        select(&mut settings, "Animations");
        settings.press(Key::Activate, ms(10));
        settings.close(ms(20));
        settings.open(ms(30));
        assert_eq!(
            settings.reveal(ms(30)),
            1.0,
            "it animated despite reduced motion"
        );
    }

    #[test]
    fn reduced_motion_turns_a_duration_into_nothing() {
        assert_eq!(Motion::Full.duration(ms(150)), ms(150));
        assert_eq!(Motion::Reduced.duration(ms(150)), Duration::ZERO);
    }

    #[test]
    fn the_selection_moves_and_stops_at_the_ends() {
        let mut settings = opened();
        assert_eq!(settings.press(Key::Up, T0), Outcome::Unchanged);
        assert_eq!(settings.press(Key::Down, T0), Outcome::Redraw);
        for _ in 0..20 {
            settings.press(Key::Down, T0);
        }
        assert_eq!(settings.selected, settings.controls.len() - 1);
        assert_eq!(settings.press(Key::Down, T0), Outcome::Unchanged);
    }

    #[test]
    fn activating_changes_only_the_highlighted_control() {
        let mut settings = opened();
        select(&mut settings, "Brightness");
        let brightness = index_of(&settings, "Brightness");
        let before: Vec<String> = settings.controls.iter().map(|c| c.read().value).collect();
        settings.press(Key::Activate, T0);
        let after: Vec<String> = settings.controls.iter().map(|c| c.read().value).collect();
        let changed: Vec<usize> = (0..before.len())
            .filter(|i| before[*i] != after[*i])
            .collect();
        assert_eq!(changed, [brightness], "activation touched {changed:?}");
    }

    #[test]
    fn brightness_wraps_rather_than_sticking_at_full() {
        let mut settings = opened();
        select(&mut settings, "Brightness");
        let brightness = index_of(&settings, "Brightness");
        let mut seen = Vec::new();
        for _ in 0..6 {
            settings.press(Key::Activate, T0);
            seen.push(settings.controls[brightness].read().value.clone());
        }
        assert!(seen.contains(&"100%".to_owned()));
        assert!(
            seen.contains(&"0%".to_owned()),
            "it stuck at full: {seen:?}"
        );
    }

    #[test]
    fn the_volume_row_is_a_slider_the_arrows_move() {
        let mut settings = opened();
        select(&mut settings, "Volume");
        let row = index_of(&settings, "Volume");
        let before = settings.controls[row]
            .slider()
            .expect("volume has a slider");
        assert_eq!(settings.press(Key::Right, T0), Outcome::Redraw);
        let after = settings.controls[row].slider().unwrap();
        assert!(after > before, "Right did not turn it up");
        assert_eq!(settings.press(Key::Left, T0), Outcome::Redraw);
        assert!((settings.controls[row].slider().unwrap() - before).abs() < 1e-6);
        // Return mutes, and a muted slider sits at nothing.
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Redraw);
        assert_eq!(settings.controls[row].slider(), Some(0.0));
        assert_eq!(settings.controls[row].read().value, "Muted");
    }

    #[test]
    fn the_arrows_do_nothing_on_a_row_without_a_slider() {
        let mut settings = opened();
        select(&mut settings, "Animations");
        assert_eq!(settings.press(Key::Right, T0), Outcome::Unchanged);
        assert_eq!(settings.press(Key::Left, T0), Outcome::Unchanged);
    }

    /// The row and the media keys move one number, not two.
    #[test]
    fn the_volume_row_shares_the_compositors_volume() {
        let volume = crate::audio::Volume::default().shared();
        let mut settings = Settings::new(volume.clone());
        settings.open(T0);
        select(&mut settings, "Volume");
        let row = index_of(&settings, "Volume");
        volume
            .borrow_mut()
            .press(crate::audio::Key::Raise, T0, Motion::Full);
        let level = volume.borrow().level();
        assert_eq!(settings.controls[row].read().value, level.caption());
        settings.press(Key::Right, T0);
        assert_eq!(
            volume.borrow().level().percent,
            level.percent + crate::audio::STEP
        );
    }

    /// A panel whose Power row reports into `sent` instead of at the daemon.
    fn with_recorder() -> (Settings, std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
        let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut settings = Settings::with_power(
            crate::audio::Volume::default().shared(),
            Box::new(FakePower { sent: sent.clone() }),
        );
        settings.open(T0);
        (settings, sent)
    }

    #[test]
    fn the_power_row_is_last() {
        let settings = Settings::default();
        assert_eq!(
            settings.controls.last().map(|c| c.label()),
            Some("Power"),
            "a stray Return at the top of the panel must not be a shutdown"
        );
    }

    #[test]
    fn power_steps_suspend_then_power_off_then_reboot_and_wraps() {
        let mut settings = opened();
        select(&mut settings, "Power");
        let row = index_of(&settings, "Power");
        let mut seen = vec![settings.controls[row].read().value.clone()];
        for _ in 0..3 {
            assert_eq!(settings.press(Key::Right, T0), Outcome::Redraw);
            seen.push(settings.controls[row].read().value.clone());
        }
        assert_eq!(seen, ["Suspend", "Power off", "Reboot", "Suspend"]);
        assert_eq!(PowerAction::Suspend.stepped(), PowerAction::PowerOff);
        assert_eq!(PowerAction::PowerOff.stepped(), PowerAction::Reboot);
        assert_eq!(PowerAction::Reboot.stepped(), PowerAction::Suspend);
    }

    #[test]
    fn a_first_press_on_power_arms_and_sends_nothing() {
        let (mut settings, sent) = with_recorder();
        select(&mut settings, "Power");
        let row = index_of(&settings, "Power");
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Redraw);
        assert_eq!(settings.controls[row].read().value, "Suspend?");
        assert!(settings.is_open(), "arming closed the panel");
        assert!(
            sent.borrow().is_empty(),
            "a first press sent {:?}",
            sent.borrow()
        );
    }

    #[test]
    fn moving_off_an_armed_power_row_disarms_it() {
        let (mut settings, sent) = with_recorder();
        select(&mut settings, "Power");
        let row = index_of(&settings, "Power");
        settings.press(Key::Activate, T0);
        assert_eq!(settings.controls[row].read().value, "Suspend?");
        // Power is the last row, so Down goes nowhere — and must still be a
        // change of mind, because that is the key somebody stepping through
        // the panel is holding.
        assert_eq!(settings.press(Key::Down, T0), Outcome::Redraw);
        assert_eq!(settings.controls[row].read().value, "Suspend");
        settings.press(Key::Activate, T0);
        assert_eq!(settings.press(Key::Up, T0), Outcome::Redraw);
        assert_eq!(settings.controls[row].read().value, "Suspend");
        // Back on the row, one Return is arming again, not confirming what
        // was armed before the highlight left.
        settings.press(Key::Down, T0);
        settings.press(Key::Activate, T0);
        assert!(
            sent.borrow().is_empty(),
            "a disarmed row still sent {:?}",
            sent.borrow()
        );
        assert!(settings.is_open());
    }

    #[test]
    fn dismissing_disarms_the_power_row() {
        let (mut settings, sent) = with_recorder();
        select(&mut settings, "Power");
        let row = index_of(&settings, "Power");
        settings.press(Key::Activate, T0);
        assert_eq!(settings.press(Key::Dismiss, T0), Outcome::Dismissed);
        settings.open(ms(10));
        select(&mut settings, "Power");
        assert_eq!(settings.controls[row].read().value, "Suspend");
        settings.press(Key::Activate, ms(10));
        assert!(sent.borrow().is_empty());
    }

    #[test]
    fn two_presses_on_suspend_send_exactly_suspend_and_close_the_panel() {
        let (mut settings, sent) = with_recorder();
        select(&mut settings, "Power");
        settings.press(Key::Activate, T0);
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Dismissed);
        assert_eq!(*sent.borrow(), ["suspend"]);
        assert!(!settings.is_open(), "the panel stayed up over a suspend");
    }

    #[test]
    fn power_off_and_reboot_are_never_sent_by_a_first_press() {
        for (steps, verb) in [(1, "poweroff"), (2, "reboot")] {
            let (mut settings, sent) = with_recorder();
            select(&mut settings, "Power");
            for _ in 0..steps {
                settings.press(Key::Right, T0);
            }
            settings.press(Key::Activate, T0);
            assert!(sent.borrow().is_empty(), "{verb} went out on a first press");
            settings.press(Key::Activate, T0);
            assert_eq!(*sent.borrow(), [verb]);
        }
    }

    #[test]
    fn stepping_an_armed_power_row_disarms_it() {
        let (mut settings, sent) = with_recorder();
        select(&mut settings, "Power");
        settings.press(Key::Activate, T0);
        settings.press(Key::Right, T0);
        settings.press(Key::Activate, T0);
        assert!(
            sent.borrow().is_empty(),
            "changing the choice confirmed it: {:?}",
            sent.borrow()
        );
    }

    #[test]
    fn a_refused_verb_is_shown_and_does_not_close_the_panel() {
        #[derive(Debug)]
        struct Refusing;
        impl PowerSender for Refusing {
            fn is_available(&self) -> bool {
                true
            }
            fn send(&mut self, _: &str) -> std::io::Result<String> {
                Ok("error: init unreachable".to_owned())
            }
        }
        let mut settings =
            Settings::with_power(crate::audio::Volume::default().shared(), Box::new(Refusing));
        settings.open(T0);
        select(&mut settings, "Power");
        let row = index_of(&settings, "Power");
        settings.press(Key::Activate, T0);
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Redraw);
        assert!(settings.is_open());
        assert_eq!(
            settings.controls[row].read().value,
            "error: init unreachable"
        );
        settings.press(Key::Up, T0);
        assert_eq!(settings.controls[row].read().value, "Suspend");
    }

    #[test]
    fn escape_closes_it() {
        let mut settings = opened();
        assert_eq!(settings.press(Key::Dismiss, T0), Outcome::Dismissed);
        assert!(!settings.is_open());
    }

    #[test]
    fn a_closed_panel_ignores_every_key() {
        let mut settings = Settings::default();
        for key in [Key::Up, Key::Down, Key::Activate, Key::Dismiss] {
            assert_eq!(settings.press(key, T0), Outcome::Unchanged);
        }
    }

    #[test]
    fn every_stub_is_labelled_on_screen() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let settings = opened();
        // Composing at full reveal so the alpha pass does not hide anything.
        let canvas = compose(
            &settings,
            &mut text,
            Rect::from_xywh(0, 0, 1920, 1080),
            ms(500),
            1,
        );
        assert!(canvas.height > 0);
        // The rows that are stubs draw a longer value string than the real one,
        // so the panel must be wide enough for it rather than clipping.
        let widest = settings
            .controls
            .iter()
            .filter(|c| !c.read().real)
            .map(|c| {
                text.measure(&format!("{} · {STUB}", c.read().value), BASE_SIZE * 0.95)
                    .0
            })
            .fold(0.0_f32, f32::max);
        assert!(
            canvas.stride as f32 > widest + PAD * 2.0,
            "the panel is {} wide but a stub label needs {}",
            canvas.stride,
            widest + PAD * 2.0
        );
    }

    #[test]
    fn a_partly_revealed_panel_is_partly_transparent() {
        let mut text = Text::new();
        if !text.is_usable() {
            return;
        }
        let settings = opened();
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let opaque = compose(&settings, &mut text, output, ms(500), 1);
        let faded = compose(&settings, &mut text, output, ms(20), 1);
        let alpha = |c: &Canvas| {
            c.pixels
                .chunks_exact(4)
                .map(|p| u32::from(p[3]))
                .sum::<u32>()
        };
        assert!(
            alpha(&faded) < alpha(&opaque),
            "the reveal did not fade the panel"
        );
    }

    // -----------------------------------------------------------------------
    // Bluetooth
    // -----------------------------------------------------------------------

    /// A BlueZ that remembers what it was asked, for the row's tests.
    #[derive(Debug, Default)]
    struct FakeBluetooth {
        state: std::rc::Rc<std::cell::RefCell<crate::bluetooth::State>>,
        sent: std::rc::Rc<std::cell::RefCell<Vec<crate::bluetooth::Command>>>,
        answers: std::rc::Rc<std::cell::RefCell<Vec<bool>>>,
    }

    impl crate::bluetooth::Backend for FakeBluetooth {
        fn state(&self) -> crate::bluetooth::State {
            self.state.borrow().clone()
        }
        fn send(&self, command: crate::bluetooth::Command) {
            self.sent.borrow_mut().push(command);
        }
        fn answer(&self, yes: bool) {
            self.answers.borrow_mut().push(yes);
        }
    }

    fn bt_device(name: &str, paired: bool, connected: bool) -> crate::bluetooth::Device {
        crate::bluetooth::Device {
            path: zbus::zvariant::OwnedObjectPath::try_from(format!(
                "/org/bluez/hci0/dev_{}",
                name.replace(' ', "_")
            ))
            .unwrap(),
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            name: name.to_owned(),
            paired,
            trusted: paired,
            connected,
        }
    }

    /// A panel whose Bluetooth row talks to `fake`, opened and stepped to it.
    fn bluetooth_panel(fake: &FakeBluetooth) -> Settings {
        let mut settings = opened();
        settings.set_bluetooth(Box::new(FakeBluetooth {
            state: fake.state.clone(),
            sent: fake.sent.clone(),
            answers: fake.answers.clone(),
        }));
        select(&mut settings, "Bluetooth");
        settings
    }

    fn bt_value(settings: &Settings) -> String {
        settings.controls[index_of(settings, "Bluetooth")]
            .read()
            .value
    }

    #[test]
    fn a_bluetooth_row_with_a_backend_is_real() {
        let fake = FakeBluetooth::default();
        fake.state.borrow_mut().available = true;
        let settings = bluetooth_panel(&fake);
        let row = &settings.controls[index_of(&settings, "Bluetooth")];
        assert!(row.read().real);
        assert_eq!(row.read().value, "Off");
    }

    #[test]
    fn a_backend_with_no_bluez_stays_a_stub() {
        let fake = FakeBluetooth::default();
        let settings = bluetooth_panel(&fake);
        assert!(
            !settings.controls[index_of(&settings, "Bluetooth")]
                .read()
                .real
        );
    }

    #[test]
    fn return_on_the_reading_toggles_the_radio() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
        }
        let mut settings = bluetooth_panel(&fake);
        assert_eq!(bt_value(&settings), "On");
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().as_slice(),
            [crate::bluetooth::Command::Power(false)]
        );
    }

    #[test]
    fn the_connected_device_is_the_reading() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.devices = vec![bt_device("Headphones", true, true)];
        }
        let settings = bluetooth_panel(&fake);
        assert_eq!(bt_value(&settings), "Headphones");
    }

    #[test]
    fn the_arrows_step_through_off_on_devices_and_scan_then_wrap() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.devices = vec![
                bt_device("Headphones", true, false),
                // Not paired and no scan running: not offered.
                bt_device("Stranger", false, false),
            ];
        }
        let mut settings = bluetooth_panel(&fake);
        let mut seen = Vec::new();
        for _ in 0..5 {
            settings.press(Key::Right, T0);
            seen.push(bt_value(&settings));
        }
        assert_eq!(seen, ["Off?", "On?", "Headphones?", "Scan?", "Off?"]);
        settings.press(Key::Left, T0);
        assert_eq!(bt_value(&settings), "Scan?");
    }

    #[test]
    fn return_on_a_paired_device_connects_it_and_on_a_connected_one_disconnects() {
        let fake = FakeBluetooth::default();
        let paired = bt_device("Headphones", true, false);
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.devices = vec![paired.clone()];
        }
        let mut settings = bluetooth_panel(&fake);
        for _ in 0..3 {
            settings.press(Key::Right, T0);
        }
        assert_eq!(bt_value(&settings), "Headphones?");
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().last(),
            Some(&crate::bluetooth::Command::Connect(paired.path.clone()))
        );
        // Applied: the row is back to its reading, not still asking.
        assert_eq!(bt_value(&settings), "On");

        fake.state.borrow_mut().devices[0].connected = true;
        for _ in 0..3 {
            settings.press(Key::Right, T0);
        }
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().last(),
            Some(&crate::bluetooth::Command::Disconnect(paired.path))
        );
    }

    #[test]
    fn scanning_offers_what_it_finds_and_return_on_a_new_device_pairs_it() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
        }
        let mut settings = bluetooth_panel(&fake);
        for _ in 0..3 {
            settings.press(Key::Right, T0);
        }
        assert_eq!(bt_value(&settings), "Scan?");
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().last(),
            Some(&crate::bluetooth::Command::Scan(true))
        );

        // The thread reports the scan running and a device found.
        let found = bt_device("New Keyboard", false, false);
        {
            let mut s = fake.state.borrow_mut();
            s.discovering = true;
            s.devices = vec![found.clone()];
        }
        assert_eq!(bt_value(&settings), "Scanning…");
        for _ in 0..3 {
            settings.press(Key::Right, T0);
        }
        assert_eq!(bt_value(&settings), "New Keyboard?");
        settings.press(Key::Right, T0);
        assert_eq!(bt_value(&settings), "Stop scan?");
        settings.press(Key::Left, T0);
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().last(),
            Some(&crate::bluetooth::Command::Pair(found.path))
        );
    }

    #[test]
    fn a_confirmation_is_answered_yes_by_return_and_no_by_leaving() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.prompt = Some(crate::bluetooth::Prompt::Confirm {
                device: "Phone".to_owned(),
                passkey: 4242,
            });
        }
        let mut settings = bluetooth_panel(&fake);
        assert_eq!(bt_value(&settings), "Confirm 004242?");
        // The arrows do nothing while a question is open.
        assert_eq!(settings.press(Key::Right, T0), Outcome::Unchanged);
        settings.press(Key::Activate, T0);
        assert_eq!(fake.answers.borrow().as_slice(), [true]);

        settings.press(Key::Up, T0);
        assert_eq!(fake.answers.borrow().as_slice(), [true, false]);
    }

    #[test]
    fn leaving_the_row_stops_a_scan() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.discovering = true;
        }
        let mut settings = bluetooth_panel(&fake);
        settings.press(Key::Up, T0);
        assert_eq!(
            fake.sent.borrow().as_slice(),
            [crate::bluetooth::Command::Scan(false)]
        );
    }

    #[test]
    fn busy_and_error_show_in_place_of_the_reading() {
        let fake = FakeBluetooth::default();
        {
            let mut s = fake.state.borrow_mut();
            s.available = true;
            s.powered = true;
            s.busy = Some("Pairing Headphones".to_owned());
        }
        let mut settings = bluetooth_panel(&fake);
        assert_eq!(bt_value(&settings), "Pairing Headphones…");
        assert_eq!(settings.press(Key::Activate, T0), Outcome::Unchanged);

        {
            let mut s = fake.state.borrow_mut();
            s.busy = None;
            s.error = Some("Failed: page-timeout".to_owned());
        }
        assert_eq!(bt_value(&settings), "Failed: page-timeout");
        settings.press(Key::Activate, T0);
        assert_eq!(
            fake.sent.borrow().last(),
            Some(&crate::bluetooth::Command::ClearError)
        );
    }

    #[test]
    fn long_device_names_are_shortened_to_fit_the_row() {
        let long = "A Very Long Bluetooth Speaker Name Indeed";
        let short = shorten(long);
        assert!(short.chars().count() <= BT_NAME_CHARS);
        assert!(short.ends_with('…'));
        assert_eq!(shorten("Short"), "Short");
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// `SETTINGS_DUMP=/tmp/s.ppm SETTINGS_DOWN=1 cargo test -p huginn-comp settings_dump`
    #[test]
    fn settings_dump() {
        let Ok(path) = std::env::var("SETTINGS_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let mut settings = Settings::default();
        settings.open(Duration::ZERO);
        for _ in 0..std::env::var("SETTINGS_DOWN")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            settings.press(Key::Down, Duration::ZERO);
        }
        let at = Duration::from_millis(
            std::env::var("SETTINGS_AT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500),
        );
        let canvas = compose(
            &settings,
            &mut text,
            Rect::from_xywh(0, 0, 1920, 1080),
            at,
            1,
        );
        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
