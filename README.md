# Huginn & Muninn

The Wayland compositor and desktop shell for [RavenLinux](../RavenLinux),
written in Rust on [Smithay](https://github.com/Smithay/smithay).

Odin kept two ravens. **Huginn** — *thought* — flew out over the world each day
and reported what he saw. **Muninn** — *memory* — is the half you actually look
at.

| Binary | What it is |
|---|---|
| `huginn` | the compositor, and the shell it draws — dock, launcher, notifications |
| `muninn-lock` | the lock screen |

## Architecture

**`huginn-core` holds no I/O.** Layout, focus, and workspace behaviour live in a
crate with no Wayland, DRM, or GPU dependency, so all of it is unit-tested in
milliseconds on any host — including a Mac that cannot build the compositor at
all. `huginn-comp` maps `WindowId` to a real `WlSurface` and turns
`Space::arrange` output into `xdg_toplevel::configure` events.

**The shell is not a client.** The dock, launcher, overview and notifications
are drawn by the compositor itself, inside the render loop. Anything that must
feel instant and must never fail does not get to be a separate process that can
miss a frame or die. `muninn-lock` is the one exception, and it has an
independent reason: `ext-session-lock-v1` keeps the screen locked if the locking
client dies, which is only worth anything if that client shares no address space
with the rest of the shell.

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
└── shell/
    └── muninn-lock/       session-lock client
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

**Not yet implemented:** the offscreen render-at-2×-and-downsample pass for
panels whose ideal scale is fractional. `DrmCompositor::render_frame` draws
straight into the scanout buffer, so there is nowhere for a larger render target
to live. Until then the backends call `OutputScale::integer_only`, which divides
by the advertised integer instead — a 4K 27" gets a crisp 1920×1080-at-2×
desktop rather than the 2560×1440-at-1.5× it would prefer.

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
