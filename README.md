# Huginn & Muninn

The Wayland compositor and desktop shell for [RavenLinux](../RavenLinux),
written in Rust on [Smithay](https://github.com/Smithay/smithay).

Odin kept two ravens. **Huginn** — *thought* — flew out over the world each day
and reported what he saw. **Muninn** — *memory* — is the half you actually look
at.

| Binary | What it is |
|---|---|
| `huginn` | the compositor |
| `muninn` | the desktop shell — panel, launcher, notifications |
| `muninn-lock` | the lock screen |

## Architecture

**The shell is a separate process.** `muninn` is an ordinary Wayland client that
draws through `wlr-layer-shell` and talks to the compositor over the private
`raven_shell_v1` protocol. GNOME takes the opposite approach — the shell lives
inside Mutter, which buys synchronous access to window state at the price of a
single failure domain, where a panel bug kills every open application. Here a
crashed `muninn` costs you the panel and nothing else, and it can be rebuilt and
restarted inside a live session.

COSMIC is the reference design, and conveniently it is what the dev box runs, so
a working example of this exact architecture is always a `ps` away.

`muninn-lock` is a third binary for the same reason, more sharply:
`ext-session-lock-v1` guarantees the compositor keeps the screen locked if the
locking client dies, and that guarantee only means something if a shell bug
cannot take the locker down with it.

**`huginn-core` holds no I/O.** Layout, focus, and workspace behaviour live in a
crate with no Wayland, DRM, or GPU dependency, so all of it is unit-tested in
milliseconds on any host — including a Mac that cannot build the compositor at
all. `huginn-comp` maps `WindowId` to a real `WlSurface` and turns
`Space::arrange` output into `xdg_toplevel::configure` events.

**One repository, split at protocol v1.** The custom protocol is co-designed by
both halves, so a change to it touches the XML, the compositor, and the shell in
one commit that either builds or doesn't. `raven-protocol` is kept standalone
and additive-only so that the eventual `git subtree split` stays mechanical.

Shared crates take the `raven-` prefix, since they belong to neither bird.

```
crates/
├── raven-protocol/        raven_shell_v1 — shared, versioned, split-ready
├── raven-config/          config schema — shared
├── comp/
│   ├── huginn-core/       ★ layout, focus, workspaces. Pure. Tests anywhere.
│   ├── huginn-comp/       smithay glue, event loop, protocol handlers
│   └── huginn-egl/        ★ the only crate permitted `unsafe`
└── shell/
    ├── muninn/            panel, launcher, notifications
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

The udev/TTY backend drives real hardware: a libseat session, DRM/KMS scan-out
through GBM, libinput, VT switching, and monitor hotplug. udev has no notion of
a connector — plugging a monitor in shows up as a `Changed` event on the GPU's
device node — so a hotplug re-probes the connector list and reconciles against
it, and the initial scan at startup is that same code path. A connector that
goes away loses its `wl_output` global and gives its CRTC back; a new one is
placed to the right of the screens already up.

```sh
./scripts/remote.sh run -p huginn-comp                       # compositor
WAYLAND_DISPLAY=huginn-1 cargo run -p muninn                 # panel
WAYLAND_DISPLAY=huginn-1 cargo run -p muninn \
    --example raven-shell-probe 3                            # protocol probe
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
