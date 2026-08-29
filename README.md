# Huginn & Muninn

The Wayland compositor and desktop shell for [RavenLinux](../RavenLinux),
written in Rust on [Smithay](https://github.com/Smithay/smithay).

Odin kept two ravens. **Huginn** — *thought* — flew out over the world each day
and reported what he saw. **Muninn** — *memory* — is the half you actually look
at.

| Binary | What it is |
|---|---|
| `huginn` | the compositor, and the shell it draws — dock, launcher, notifications |

The lock screen is not here. It is `raven-lock`, from
[RavenLogin](../RavenLogin) — the login screen's twin, drawn by the same code
and authenticating against the same daemon, because a lock screen that merely
*resembles* the login screen teaches its owner to type their password into
things that look about right. What lives here is the compositor half:
`ext-session-lock-v1`, and the rule that a locked session is not composited.

## Architecture

**`huginn-core` holds no I/O.** Layout, focus, and workspace behaviour live in a
crate with no Wayland, DRM, or GPU dependency, so all of it is unit-tested in
milliseconds on any host — including a Mac that cannot build the compositor at
all. `huginn-comp` maps `WindowId` to a real `WlSurface` and turns
`Space::arrange` output into `xdg_toplevel::configure` events.

**The shell is not a client.** The dock, launcher, overview and notifications
are drawn by the compositor itself, inside the render loop. Anything that must
feel instant and must never fail does not get to be a separate process that can
miss a frame or die.

The lock screen is the one thing that goes the other way, and for the opposite
reason. `ext-session-lock-v1` guarantees that if the locking client dies the
compositor keeps the screen locked rather than revealing the session — a
guarantee that is worth exactly nothing if the locking client is the same
process as the launcher. So it is not merely a separate process but a separate
*repository*: `raven-lock` ships with the login screen it is a copy of.

**A locked session is not composited.** Not "the lock is drawn on top": while
the session is locked, `Huginn::scene` returns the lock surface and nothing
else, and `frame_surfaces` stops handing out frame callbacks to anything below.
There is no ordering rule that could be got wrong, no surface that could be
raised above the lock, and nothing behind it still painting.

**One repository, split at protocol v1.** The custom protocol is co-designed by
both halves, so a change to it touches the XML, the compositor, and the shell in
one commit that either builds or doesn't. `raven-protocol` is kept standalone
and additive-only so that the eventual `git subtree split` stays mechanical.

Shared crates take the `raven-` prefix, since they belong to neither bird.

```
crates/
├── raven-protocol/        raven_shell_v1 — shared, versioned, split-ready
├── raven-desktop/         ★ .desktop entries + icon lookup. Pure.
├── comp/
│   ├── huginn-core/       ★ layout, focus, workspaces. Pure. Tests anywhere.
│   ├── huginn-comp/       smithay glue, event loop, protocol handlers,
│   │                        and the shell — dock, launcher, notifications
│   └── huginn-egl/        ★ the only crate permitted `unsafe`
└── tools/
    └── layer-probe/       a layer-shell client that reports what it was told
```

## No unsafe

`[workspace.lints.rust] unsafe_code = "forbid"` in the root manifest, inherited
by every crate via `[lints] workspace = true`. `forbid` cannot be overridden by
an `#[allow]` anywhere inside the crate — attempting it is `error[E0453]`.

A compositor cannot reach zero: smithay exposes 28 `pub unsafe fn`, of which a
DRM/GLES compositor must call about six, all in EGL/GLES bring-up. Those are
quarantined in `huginn-egl`, the single crate that declares its own `[lints]`
table. It also sets `clippy::undocumented_unsafe_blocks = "deny"`, so an
`unsafe` block there without a `// SAFETY:` comment fails the build.

The only way to smuggle unsafe into another crate is to drop the `[lints]`
opt-in from its manifest, which is what `scripts/check-unsafe.sh` exists to
catch. Run it in CI and from a pre-push hook.

## Window mapping

A window is in the layout from the moment it is created and on screen only once
it has committed a buffer. Between those two moments its tile is reserved and
empty.

Two orderings do the work, and both are load-bearing:

- **Lay out, then configure.** `new_toplevel` arranges before it sends the
  initial `configure`, so that configure carries the geometry the window will
  actually occupy. The other way round sends a size of zero, the client paints
  at whatever it picked, and the next configure resizes it into its tile — and
  that pre-tile frame is visible. `arrange` skips any window whose initial
  configure has not gone out, so exactly one is ever sent.
- **Buffer, then draw.** `render_list` skips unmapped windows. A surface exists,
  is configured, and is laid out well before it has content; a Chromium-based
  application spends hundreds of milliseconds getting there, and anything drawn
  in that interval is not the application.

Frame callbacks deliberately go to a *wider* set than rendering does
(`frame_surfaces`), including windows with no buffer. A frame callback is
permission to paint, and the surfaces most needing it are the ones with nothing
on screen; withholding it deadlocks a client against a compositor waiting for
the buffer that callback would have produced.

Mapping tracks both directions, since a client may unmap by attaching a null
buffer and map again later.

**Not yet implemented:** §5's animation clauses. There are no open or close
animations, so there is nothing yet to hold until the first buffer or to run out
from the last one. `sync_mapped` is where both will hang.

## Tiling

Opening a window splits the focused tile in two; closing one gives its space
back to whatever it was split from. The split follows the tile's longer edge and
is then **kept**, so tiles tend toward square instead of degenerating into
slivers, and a resize or an output change never rearranges a pane behind your
back.

`huginn_core::tiles` is the tree. `Workspace` holds it alongside a plain
membership list, because the two answer different questions — who lives on this
workspace, versus how the tiled ones divide the screen. A floating window is in
the first and not the second. A **fullscreen window is in both**: it keeps its
tile so that leaving fullscreen puts it back exactly where it was, which is what
§2's "fullscreen is a layout state inside the current pane" requires.

The tree is a cache, never an authority. `Space::arrange` reconciles it against
the membership list and each window's mode before reading it, so a missed update
costs a frame of staleness rather than a window tiled into a slot it no longer
occupies.

There is one tiling model and no way to choose another.

## Scaling

**No client is ever handed a fractional scale.** `wl_output` advertises 1 or 2,
and `wp_fractional_scale_v1` — whose name is about the wire format, not an
obligation — answers with the same whole number. A client told 1.5× either
renders at 1× and is upscaled or renders at 2× and is downscaled by a toolkit
that does not know what the panel is; either way the glyphs pass through a
resample the text rasterizer did not know about, which is why the same terminal
looks sharp on macOS and mushy elsewhere.

`huginn_core::scale` decides, from the panel's resolution and its reported
millimetres. The DPI ratio snaps **down** to a quarter step, so a step is a
threshold to be reached: an ordinary ~110 DPI monitor (27" 1440p, 34"
ultrawide) stays at 1× rather than being promoted to a 2× desktop and
downsampled every frame.

| panel | dpi | advertised | logical desktop |
|---|---|---|---|
| 1920×1080 24" | 92 | 1 | 1920×1080 |
| 2560×1440 27" | 109 | 1 | 2560×1440 |
| 3840×2160 27" | 163 | 2 | 1920×1080 |
| 2880×1800 15.4" | 221 | 2 | 1440×900 |

The fraction itself is applied by the compositor. The output carries two scales
(`smithay::output::Scale::Custom`): the integer for `wl_output`, and
`OutputScale::fractional` — 1.5 on that 4K 27" — for laying the desktop out,
for `xdg_output`'s logical size, and for composing. Each 2× client buffer is
sampled down by 0.75 as it is drawn, once, by the compositor; the client never
knows. That is macOS's "looks like 2560×1440" done per surface rather than to
the finished frame, which costs nothing in code because `DrmCompositor` already
composes at whatever fraction the output reports.

## No configuration

There is no config file. The desktop ships one look and one set of behaviours,
compiled in — see `huginn-comp/src/theme.rs`, which is the whole visual
language, and `backend/keymap.rs`, which is the whole keymap.

This is a deliberate constraint rather than an unfinished feature. A format a
user can write is a format that must not change between releases, and a config
schema drifting under someone is the commonest way a compositor breaks a
desktop that was working. A constant cannot drift, because nothing outside the
binary ever names it.

There is no `HUGINN_TERMINAL` override either: an environment variable is a
user-facing configuration surface with extra steps.

What this constrains is configuration, not extension. Software written outside
this repository still gets a surface on the desktop — as a layer-shell client,
which is a protocol it speaks rather than a file it edits.
[`docs/integration.md`](docs/integration.md) is how to write one, and
[`docs/protocols.md`](docs/protocols.md) lists every global the compositor
advertises and the ones it does not.

## What the system has to provide

Compiling the look in means the distribution underneath has to supply things by
name, and every one of these fails silently — the compositor starts, and part of
the desktop is simply not there. RavenLinux's `stage-gui.sh` installs all four
and its stage summary reports on each.

| Named in | Expects | Missing means |
|---|---|---|
| `theme::TERMINAL`, `dock::PINNED` | `raven-terminal` on `$PATH` | `Super`+`Shift`+`T` does nothing and the dock's one pinned item is dead — the desktop can launch no process at all |
| `launcher::scan_applications` | `.desktop` files in `$XDG_DATA_DIRS/applications` | the launcher opens and enumerates nothing |
| `pointer::Cursor::load` | an xcursor theme at `$XCURSOR_THEME`, default `default` | no pointer is drawn over the dock, launcher or background — clients that set their own still show one, so it reads as a rendering bug |
| `theme::ICON_THEME` | the `breeze-dark` icon theme | every dock and launcher icon draws blank |
| `wallpaper::SET_DIR` | an image at `/usr/share/wallpaper/set/wallpaper.<ext>` | the desktop is the backends' flat clear colour, which is what an image with no wallpaper set looks like and not a fault |
| `sleep::MARKER_DIR` | `raven-init` publishing `/run/raven-power/state` | the screen may stay black after a resume until something else forces a repaint |

The sleep marker is the one entry here that is not about how the desktop looks.
Huginn holds DRM master straight through a suspend, and seatd -- unlike logind
-- has nothing to say about sleep, so the compositor learns that the machine
went away by watching a file `raven-init` writes either side of the suspend.
Without it nothing tells the compositor to re-take the display and repaint, and
what that costs is the panel staying dark after the lid opens. Under any other
init the watch simply never arms, which is why it is a warning at startup and
not a refusal to run.

Three of them have no in-band failure at all: a missing cursor theme, a missing
icon theme and a missing wallpaper all take the `None` branch by design, because
refusing to start over a cosmetic asset would be worse. That makes them the
distribution's problem to check for, not the compositor's to report.

The wallpaper's path is compiled in rather than configured, and that is
deliberate: RavenLogin's greeter reads the same file, so it is a contract
between the login screen and the session behind it — a machine where the
password prompt and the desktop it hands over to show the same picture. The
extension is a label; PNG and JPEG are told apart by their first bytes, and a
symlink into `/usr/share/wallpaper` counts. The greeter dims its copy so a
password field stays readable on it; huginn does not, having no text of its
own to keep legible.

`Terminal=true` entries — and their desktop actions — are launched inside
`theme::TERMINAL` as `raven-terminal -e <command...>`, resolved through the
terminal's own desktop entry when one is installed and the bare binary on
`$PATH` when not. The `-e` flag is an assumption (`launcher::in_terminal`
documents it); a terminal that spells it differently silently breaks every TUI
entry.

## Building

**The compositor only builds on Linux.** Smithay needs `libudev`, `libdrm`,
`libseat`, and `libinput` — Linux kernel-interface libraries with no macOS
equivalent. On a Mac, `cargo check --workspace` still passes: the Linux-only
dependencies sit behind `[target.'cfg(target_os = "linux")'.dependencies]`, and
each Linux binary has a single top-level `cfg` gate.

```sh
# Anywhere, including macOS — this is where window-management work happens.
cargo test -p huginn-core
cargo check --workspace --all-targets

# Edit here, build on the Linux box.
./scripts/remote.sh test -p huginn-core
./scripts/remote.sh run -p huginn-comp    # nested window in the host's session
./scripts/check-unsafe.sh
```

`--backend` is autodetected: an inherited `WAYLAND_DISPLAY` means you are inside
a session, which means development, which means the nested `winit` backend. It
is the difference between a five-second edit loop and a reboot.

**The udev/TTY backend cannot be driven over SSH.** logind grants DRM master
only to the active session on a seat, and an ssh login has no seat at all — so
`--backend udev` has to be run at the machine on a TTY, or in a VM with
virtio-gpu.

## Running it on the machine you are using

`lazy.toml` is the build interface, the same way it is for RavenLinux:

```sh
imlazy run          # a whole desktop in a window, inside the session you are in
imlazy probe        # a layer-shell client that says what the compositor did to it
imlazy lint         # clippy, and the unsafe quarantine check
imlazy install      # replace the installed huginn with this tree's
imlazy installed    # what is installed, and where it came from
imlazy restore      # put the originals back
```

`imlazy install` builds optimised and renames the new binaries over the old ones
rather than writing through them, because the kernel refuses to truncate a
running executable and huginn is running whenever you are looking at the
desktop. The live compositor keeps the inode it started with and carries on; the
new one takes over at the next session. `imlazy restore` returns the binary the
image shipped with, which the first install sets aside as `huginn.orig` and no
later install overwrites.

Nothing tracks these two files — `rvn owns /usr/bin/huginn` reports no owner,
because the `gui` stage installs them directly. Replacing them corrupts no
package database, and equally nothing but `restore` will put them back.

## Status

Working: Huginn runs nested via the winit backend, hosts xdg-shell clients and
wlr-layer-shell surfaces, and lays both out from `huginn-core`. Muninn draws a
top panel with workspace pips, receives workspace state over `raven_shell_v1`,
and switches workspaces when a pip is clicked.

The focused window wears a ring in the same accent the panel uses, and
`Super`+`Shift`+arrows move it between tiles. Neighbours are found by position
rather than by tile order — with one window beside a split stack the two disagree
— and a direction with nothing squarely that way does nothing rather than
sending the window off diagonally.

`Super`+`Shift`+`H` draws the keybinding list over everything, and pressing it
again takes it away. The compositor renders it itself, with a bitmap font it
carries in the binary — the shell is a separate process precisely so it can
crash, and a help screen that vanishes along with the panel would be missing
exactly when it is wanted. Both the overlay and the line the compositor logs at
startup are built from one table next to the keymap, and a test walks the
keysym space to prove no binding has been added without a row in it.

Window management lives on the `Super`+`Shift` layer. Plain `Super` belongs to
applications: RavenTerminal uses it as its own leader, and a chord the
compositor intercepts never reaches a client at all. The exception is
`Super`+`C` and `Super`+`V`, which the compositor translates rather than
performs — it cannot copy, having no idea what a client has selected, so it
synthesises the `Ctrl`+`C` that toolkits listen for. Clients that drive the
`Super` layer themselves are exempt by `app_id`, because a terminal reads
`Ctrl`+`C` as SIGINT and would kill the job instead of copying. A client that
advertises no `app_id` is treated as one of them: it cannot be told apart from
a terminal that advertises none either.

`Super`+`L` is the second exception, and it works the other way: it locks the
session and it is never handed back to anybody. The rule that plain `Super`
belongs to the application is a good one, and a lock chord that a focused
terminal can swallow is worse than not having a lock chord — it fails precisely
when somebody is walking away from a machine they believe they just locked. So
that chord is reserved, and RavenTerminal may not use it.

While the session is locked, no binding resolves at all: every key is the lock
screen's, checked before the launcher and quick settings, which would otherwise
swallow the keystrokes and leave no way to type a password.

The session locks itself after ten minutes with no input, on a timer that
reschedules itself for however long is actually left rather than polling — an
idle desktop draws no frames, so a check that ran per frame would stop running
at exactly the point it is meant to fire. The wait is counted on the monotonic
clock, which does not advance across a suspend: a laptop shut for the night has
been idle for as long as it was *awake*, so the idle lock does not also fire on
every resume and race the one the resume already does.

"Lock when idle" in quick settings steps that between 5, 10, 15 and 30 minutes
and off, and it is the only way to hold it off — there is no
`idle-inhibit-unstable-v1` here, so a full-screen film is indistinguishable
from an empty room. Off is placed after the longest wait and before the
shortest, so stepping the row to lengthen the timeout never passes through
"never lock" on the way.

The last row is "Power", which suspends, powers off or reboots the machine by
sending one word to raven-powerd's control socket — the desktop gets a verb
into the daemon that already decides whether the machine may sleep, never a
line into init. It is the one row that takes two presses. Every other row can
be undone by pressing it again, but there is no undo for a reboot, and Return
on the last row of a panel somebody was stepping through with the arrows is
exactly the press that happens by accident. So the first Return arms the row,
which shows as a question, and only the second sends; moving the highlight off
the row, stepping the choice, or closing the panel stands it down. Suspend
comes first and is the default because it is the one asked for daily and the
cheapest to have asked for by mistake. A bar can open the panel too, over
`raven_shell_v1`, so a click on a battery reading leads here.

The udev/TTY backend drives real hardware: a libseat session, DRM/KMS scan-out
through GBM, libinput, VT switching, and monitor hotplug. udev has no notion of
a connector — plugging a monitor in shows up as a `Changed` event on the GPU's
device node — so a hotplug re-probes the connector list and reconciles against
it, and the initial scan at startup is that same code path. A connector that
goes away loses its `wl_output` global and gives its CRTC back; a new one is
placed to the right of the screens already up.

```sh
./scripts/remote.sh run -p huginn-comp                       # compositor
cargo run -p raven-desktop --example inventory                # app/icon index
```

Clients get hardware buffers via `zwp_linux_dmabuf_v1`, with the render node
advertised as the main device so they allocate on the GPU the compositor
actually renders on.

The compositor runs on a calloop event loop — listening socket, display poll
fd, and winit all as event sources — and renders only on damage, so an idle
session costs ~0.2% of one core.

Not done yet, roughly in the order they matter:

- **SIGTERM handling** — calloop's signal source needs its `signals` feature,
  which smithay does not enable. Ctrl-C is enough for the nested backend; a
  compositor killed on a TTY has to hand the session back.
- **Per-output areas in `huginn-core`** — the core still models one usable
  area, so with several monitors up the leftmost one defines it and windows
  only ever tile there. Everything below it in the stack — hotplug, output
  globals, per-CRTC scan-out — already handles the plural case.
- **Multi-GPU** — only the primary GPU is driven. A second card is detected and
  logged, but lighting up its connectors needs a `DrmOutputManager` per device
  and a way to move buffers between them.
- **Mode changes on a live connector** — a monitor that is already up keeps the
  mode it came up with. Only connect and disconnect are reconciled.
- **Privilege gating** — `raven_shell_v1` is advertised to every client, so any
  client can currently read workspace state and switch workspaces. Harmless on
  a single-user session, but it must be gated before anything untrusted runs.
- **iced** — the panel is software-rendered through `wl_shm`. Deliberate for
  now; the protocol plumbing does not change when the renderer does.
