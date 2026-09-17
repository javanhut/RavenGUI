# Fingerprint

The machine this was written on has an Elan `04f3:0c00` reader — a match-on-chip
sensor, the one `libfprint` drives with `elanmoc`. RavenLinux had nothing that
could talk to it: no `libfprint`, no `fprintd`, no `libusb`. It has its own
driver now — `raven-fprintd`, in the RavenLinux tree — and this is what the
stack is, how a machine gets one, and what is still missing above it.

## Why not fprintd

Because of what it would drag in. `fprintd` is a GLib D-Bus service over
`libfprint` over `libusb`, and RavenLinux is a musl userland of static Rust and
Go binaries whose wireless stack — [CAW] — is netlink and WPA *in process*,
with no `wpa_supplicant` anywhere. Putting a C USB library and a GLib daemon on
the path between a finger and an unlocked screen would be the largest runtime
dependency on the image, and it would sit under the one operation that has to
work when everything else has gone wrong.

So the stack is Raven's own, in Rust, and the shape below follows the one the
rest of the system already uses: the rules are pure and tested, and the thing
that touches hardware is small, isolated, and owned by the process that is
allowed to own it.

That is a principle and not a wall. A machine whose reader works is better than
a machine whose reader is an architectural position, and there are readers
`libfprint` drives that Raven's driver does not — so where Raven's own is absent,
`rvn` installs the conventional stack and the desktop uses it. See
[Installing it](#installing-it).

[CAW]: https://github.com/javanhut/CAW

## Who owns the sensor

**A root daemon.** Not the lock screen, and this is the decision the whole
design turns on.

`raven-lock` runs as the person logged in. If it read the sensor itself and then
told `ravend` "a finger matched, let me back in", that request would carry no
secret, and *any* process running as that person could send it. A lock screen
exists to stop somebody standing at the keyboard; a process already running in
the session is not that somebody, but it is trivially able to *become* them by
unlocking the screen for one. Today's `Request::Verify { secret }` is safe on a
world-connectable socket precisely because it requires knowing the password —
an unauthenticated "let me in" alongside it would undo that in one line.

So the reader belongs to root, and `ravend` — which already owns `/etc/shadow`
and already decides every question of this kind — is what the lock screen asks.
It asks about a finger the same way it asks about a password.

`ravend` does not drive the device itself, though. `raven-fprintd` does, and it
is a separate process for the reason the USB code is not in PID 1: driving a
sensor means claiming a USB interface and coping with whatever the hardware does
when it is confused, and that is a surface the process holding the shadow file
should not grow. The split is narrow — `raven-fprintd` has no account database,
cannot start a session and cannot unlock anything. It answers one question,
which stored finger is on the sensor right now, on a root-only socket, and
`ravend` decides what the answer is worth.

Enrolment goes the same way, for a second reason: writing a template to the chip
needs the device, and the device is the daemon's.

## The pieces

| Piece | Where | State |
|---|---|---|
| Enrolment and verification rules, `Sensor` trait, fake sensor | `raven-fprint`, this repo | **Done** |
| Which stack a machine has, and the `rvn` fallback | `raven-fprint::stack`, this repo | **Done** |
| The `elanmoc` driver — usbfs transport and protocol | `init/src/fprint.rs`, **RavenLinux** | **Done** |
| `raven-fprintd` — the daemon that owns the device | `init/src/fprintd.rs`, **RavenLinux** | **Done** |
| Build and install wiring — `imlazy dev`, the ISO stage, the service | RavenLinux | **Done** |
| `ravend` asking the daemon, protocol extension | RavenLogin | Not written |
| `raven-lock` offering the finger beside the password | RavenLogin | Not written |
| Enrolment UI | `raven-settings` | Not written |

`raven-fprint` is deliberately in RavenGUI rather than RavenLogin, alongside
`raven-protocol`, and for the same reason: both repositories consume it, it is
additive and standalone, and a `git subtree split` out of here stays mechanical
if it should ever want its own home. The driver is in RavenLinux because that is
where hardware daemons live — `raven-powerd` owns the lid, `raven-mount` owns
removable storage, and this owns the reader.

## Installing it

Two ways, and RavenGUI prefers the first.

**RavenLinux built it.** `raven-fprintd` comes out of the `init` crate with
`raven-init`, `raven-rc`, `raven-powerd`, `raven-ports`, `raven-timed` and
`raven-mount` — one cargo build, seven binaries. On a running machine:

```
cd ~/Development/RavenLinux
imlazy dev            # builds init/ natively and installs all seven to /usr/bin
imlazy dev-diff       # what that would change, writing nothing
```

For an image, it is in `RAVEN_INIT_BINARIES` and `stage-raven.sh` installs it
beside the others, so `imlazy raven` or a full `imlazy build` carries it. It
runs as the `fprintd` service in `/etc/raven/init.toml`, after `udev`, on every
machine — including those with no reader, because a daemon that refused to start
without hardware could not answer "there is no reader", and a settings panel
would find a missing socket and have to guess. It opens the device lazily, so on
a machine with none it costs a socket.

**Otherwise, `rvn`.** RavenLinux shares Arch's package names, so the
conventional stack is one command away, and `raven-fprint::stack` is what
decides whether to offer it:

```rust
match Stack::detect() {
    Stack::Native  => { /* raven-fprintd: nothing to install */ }
    Stack::Fprintd => { /* fprintd from rvn: nothing to install */ }
    Stack::Missing => {
        // sudo rvn install --yes fprintd libfprint
        let argv = Stack::Missing.install_command().unwrap();
    }
}
```

`Stack::detect` reads four paths and starts nothing, because it is asked while a
panel is being drawn. It reports what is *installed* rather than what is
working, which is the question something offering to install has. And it never
installs on its own: `install_command` hands back the argv and stops. Fetching
and installing packages is a privileged change to somebody's machine, and a
desktop that did it because a panel was opened is a desktop that installs things
nobody asked for. `install_prompt` is the sentence to put in front of them
first.

Native wins when both are present. A machine that has Raven's own driver should
not be driving the reader through a GLib service.

## What `raven-fprint` decides

Three things, all pure, all tested on a machine with no reader attached:

**A bad reading is not a failed attempt.** `Scan::Retry` — too short, off
centre, not enough finger, unreadable, unchanged — means the sensor saw
something it could not use, and each carries the correction to show for it.
`Retry::from_wire` parses the daemon's word and `Retry::advice` turns it into
a sentence, which is why the copy lives here and not in a driver: off-centre
says *move your finger* and not-enough says *cover more of the sensor*, and a
stack that collapsed the two would tell somebody whose finger is already
centred to move it.
`Scan::NoMatch` means it saw a finger cleanly and did not know it. Only the
second counts against anything. Conflating them is what makes a reader feel like
it is accusing its owner of being a stranger, and spends all three tries on a
wet thumb.

**Three clean misses, then the password.** `Gate` counts locally and separately
from `ravend`'s password throttle, because a password is guessable and a finger
is not, and the two have nothing to say about each other. The gate can only ever
stop *offering* the reader: it cannot refuse an unlock, withhold the password
field or extend a throttle. A reader that could lock somebody out of their own
machine is worse than no reader, and every interesting failure — a cut finger, a
dirty sensor, a cable knocked loose inside the lid — happens on exactly the day
it is needed.

**An enrolment that is going nowhere stops.** The sensor says how many good
readings make a template, because that is hardware and a number invented here
would be a progress bar that lies. `PATIENCE` unusable readings *in a row* end
it, and a good one restores the budget — so a finger that keeps working enrols
however many poor readings it took on the way, and a reader with something wrong
with it says so in seconds rather than asking for a fingertip forever.

## The driver

`init/src/fprint.rs` in RavenLinux, ~700 lines, `libc` and nothing else.

It finds the reader by walking `/sys/bus/usb/devices` for Elan's vendor id
rather than opening every node under `/dev/bus/usb` to read its descriptors,
claims interface 0, and talks to it with `USBDEVFS_SUBMITURB` and
`USBDEVFS_REAPURBNDELAY` — the ioctls `libusb` itself uses underneath.

Waiting for a finger has no deadline, so it cannot be a blocking ioctl or the
daemon could never be told to stop. The URB is submitted and the device's file
descriptor is polled alongside the client's own socket; anything at all on that
socket — a byte, a close, a lock screen that died — discards the URB and ends
the wait. That is also why there is no cancel verb: a cancel that had to be
delivered is a sensor left running for a client that is gone.

**The protocol came from `libfprint`'s `elanmoc`, which is LGPL-2.1-or-later and
copyright Elan Microelectronics.** What was taken are interface facts — endpoint
numbers, command bytes, response codes, frames per template — and the Rust is
written fresh against them rather than translated. That is the ordinary basis
for reimplementing a driver and it is not a lawyer's opinion; if RavenLinux
wants certainty the alternative is to ship that one file under LGPL-2.1+ with
attribution instead of the repository's MIT. The file says so at the top, and
it is a decision for whoever owns the licensing.

Nothing reads a fingerprint image. These are match-on-chip sensors: the template
stays on the device, enrolment feeds it frames until it says it has enough, and
verification asks it which stored finger matched. A host that cannot read a
template cannot leak one.

## What is left

`ravend` asking the daemon, and the lock screen offering the finger. The driver
answers on `/run/raven-fprint/sensor.sock` today — `status`, `list`, `verify`,
`enrol`, `forget`, `forget-all`, one line each — and can be driven by hand with
`socat` as root. What it cannot yet do is unlock anything, because nothing asks
it to.

## The protocol extension

`raven-greet-proto`, on `VERIFY_SOCKET_PATH` only, never on the greet socket:
there is no fingerprint login, because at the login screen the machine does not
know whose session it would be, and a finger answers "is this you?" and not "who
are you?".

Unlike `Verify`, this cannot be one request and one response. A finger takes
seconds and reports retries along the way, and a lock screen that said "touch
the sensor" and then went silent until it succeeded would be one nobody could
tell from a crashed one. So it streams: one request, then a response per
reading until the last.

```rust
pub enum Request {
    // ...
    /// Watch the reader for the account that owns this connection.
    ///
    /// Carries no username, like `Verify`, and for the same reason. Only valid
    /// on `VERIFY_SOCKET_PATH`. Answered with a run of `Finger` responses and
    /// then exactly one of `Verified`, `Denied` or `Failed`.
    ///
    /// The connection is the session: if it closes, the watch stops and the
    /// daemon puts the reader down. There is no cancel request, because a
    /// cancel that had to be delivered is a watch that outlives a lock screen
    /// that has died.
    WatchFinger,
    /// Which fingers this account has enrolled. For the settings panel.
    EnrolledFingers,
    /// Begin enrolling one, replacing any template it already has.
    EnrolFinger { finger: Finger },
    /// Forget one, or all of them.
    ForgetFinger { finger: Option<Finger> },
}

pub enum Response {
    // ...
    /// One reading, mid-watch or mid-enrolment. Not a verdict.
    Finger {
        /// Already filtered for display, as `Denied`'s message is.
        message: String,
        /// Enrolment only: readings in, and readings wanted.
        progress: Option<(u8, u8)>,
    },
}
```

`Verified` and `Denied` already mean the right things and are reused unchanged,
which is the point of streaming into them: the lock screen's existing "let go"
and "that was wrong" paths need nothing new, and there is no second way for a
session to become unlocked.

Three rules for the daemon:

- **The watch is per connection and dies with it.** No cancel request; a lock
  screen that has crashed must not leave the reader running.
- **A `Denied` from a finger is a `Denied`.** It does not say which finger it
  was and it does not say how close it got.
- **`ravend` counts too.** `Gate` is the lock screen's own budget for how long
  to keep asking; the daemon must not trust a client's count, and applies the
  same three of its own before it stops watching.

## The machine

For anyone picking this up on the same hardware:

```
Bus 001 Device 002: ID 04f3:0c00 Elan Microelectronics Corp. ELAN:ARM-M4
```

Not an input device — it does not appear in `/proc/bus/input/devices`, and
nothing in the compositor sees it. It is a USB device, and the only way to it
is a USB transport.

To drive it by hand, as root, on a machine running the daemon:

```
$ socat - UNIX-CONNECT:/run/raven-fprint/sensor.sock
status
ok present 9 0 0104
enrol javanstorm right-index
frame 1 9
retry centre 1 9
frame 2 9
...
ok
verify
match javanstorm:right-index
```
