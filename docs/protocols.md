# Protocol surface

Every Wayland global Huginn advertises, and — more usefully for anyone porting
software to Raven — everything it does not. Versions follow smithay 0.7's
defaults except where stated.

For how to actually use these, see `docs/integration.md`.

## Implemented

| Global | What it gives a client |
|---|---|
| `wl_compositor`, `wl_subcompositor` | surfaces and subsurfaces |
| `xdg_wm_base` | ordinary application windows |
| `zxdg_decoration_manager_v1` | a title bar drawn by the compositor — the title and a close button, in the desktop's palette — for a toplevel that creates a decoration object and does not ask for `client_side`. A toplevel that never binds this is client-side, as the protocol says; GTK and Firefox draw their own and get no bar. See `docs/integration.md` |
| `zwlr_layer_shell_v1` | panels, docks, bars, wallpapers |
| `raven_shell_manager_v1` | workspace count, active index, occupancy, switching; opening quick settings; the screen layout; capturing a screen, window or region, and the compositor's region picker |
| `wl_shm` | shared-memory buffers |
| `zwp_linux_dmabuf_v1` | hardware buffers; advertised once the backend has a renderer |
| `wp_viewporter` | source cropping and destination scaling |
| `wp_fractional_scale_manager_v1` | fractional scale factors |
| `wl_output`, `xdg_output` | mode, integer scale, logical size, position; `wl_surface.enter`/`leave` per screen |
| `wl_seat` | keyboard and pointer |
| `wp_cursor_shape_manager_v1` | a client names its cursor and the compositor draws it from the system theme, at the screen's density |
| `zwp_pointer_constraints_v1`, `zwp_relative_pointer_manager_v1` | pointer confinement and lock with unbounded relative motion; used by games, XWayland and 3D applications to capture the mouse |
| `wl_data_device_manager` | clipboard and drag-and-drop |
| `xwayland_shell_v1` | XWayland only; associates an X11 window with its surface |
| `ext_session_lock_manager_v1` | locking the session. See below |
| `ext_foreign_toplevel_list_v1` | the window list: every window that has drawn, with its title and app id, kept current as they change and withdrawn when it closes. Read-only — the protocol has no requests that act on a window. Advertised to every client, unfiltered, with the same caveat as `raven_shell_v1` |
| `zwp_idle_inhibit_manager_v1` | holding the idle lock off. Honoured while the inhibiting surface is on screen — a toplevel that has drawn, on a workspace some screen shows, not minimized; a layer surface or popup while mapped. The idle count restarts when the last honoured inhibitor goes; one that goes without a request (a crashed client, a window put away) is noticed by the idle timer's next tick |

X11 clients work: Huginn spawns XWayland and runs a window manager for it,
including override-redirect surfaces — menus, tooltips, drag icons — drawn at
the coordinates the client chose.

## `raven_shell_v1`

The contract between Huginn and the desktop shell, covering only what no
standard protocol provides. Panels, the dock and the wallpaper are layer-shell
surfaces; this file does not duplicate them.

### `raven_shell_manager_v1` — version 4

| | |
|---|---|
| `destroy` | request, destructor. Objects made through the manager are unaffected. |
| `get_workspace_state` | request. Creates a `raven_workspace_state_v1`. |
| `open_quick_settings` | request, since 2. Opens the compositor-drawn quick settings panel as the keybinding would. A no-op if it is already open, and while the session is locked. |
| `get_output_layout` | request, since 3. Creates a `raven_output_layout_v1`. |
| `capture_output(id, output, options)` | request, since 4. Creates a `raven_capture_v1` of the screen with that connector name. A name that matches no screen gives a capture whose only event is `stopped`. |
| `capture_window(id, identifier, options)` | request, since 4. Creates a `raven_capture_v1` of the window with that `ext_foreign_toplevel_handle_v1` identifier: the window alone, with its compositor-drawn bar, nothing overlapping it, at its screen's density. An unknown identifier gives a capture whose only event is `stopped`. |
| `capture_region(id, output, x, y, width, height, options)` | request, since 4. Creates a `raven_capture_v1` of a rectangle of a screen, in logical pixels relative to its top-left corner, clipped to the screen. Empty after clipping, or an unknown screen, gives a capture whose only event is `stopped`. |
| `select_region(id)` | request, since 4. Creates a `raven_region_selection_v1` and puts up the compositor's own region picker — the one `Shift+Print` uses. |

The second version exists for a bar whose battery reading is a natural place
to click: the panel it should lead to is drawn by the compositor, so the bar
cannot open it by showing a surface of its own and has to ask. A client bound
at version 1 sees no difference.

Version 4 exists for Raven Camera, a screen recorder with a live preview: see
below, and the privilege note, which matters more now that this global can read
the screen.

### `raven_capture_v1` — version 1

| | |
|---|---|
| `destroy` | request, destructor. A pending frame is abandoned; its buffer is the client's again. |
| `frame(buffer)` | request. Draw the next frame into this `wl_shm` buffer. Answered by exactly one of `ready` and `failed`. A buffer that is not `wl_shm`, not `argb8888`/`xrgb8888`, not the last `buffer_size`, or with a stride under four bytes a pixel, is answered `failed`. Asking again before the answer is the `already_pending` protocol error. |
| `buffer_size(width, height)` | event. The size in physical pixels a buffer must be. First event of every capture, and again whenever the source changes size — which fails a pending frame. |
| `ready(tv_sec_hi, tv_sec_lo, tv_nsec)` | event. The buffer holds the source as drawn at that `CLOCK_MONOTONIC` time. |
| `failed()` | event. The buffer was not filled: reallocate after a `buffer_size`, otherwise the buffer was unusable. |
| `stopped()` | event. The screen was unplugged, the window closed, or the source never existed. A pending frame is failed first; nothing follows. |

Options, a bitfield on the creating request: `cursor` (1) draws the pointer
into frames; `clicks` (2) draws a fading accent ring where the pointer is
pressed, into frames only, never onto the screen.

How Huginn serves it:

- **Paced by the client.** No frame is drawn that was not asked for, and none
  is drawn until the source has changed since the last one delivered — except
  the first, which is answered at once. A still desktop asked for a frame keeps
  it pending. Frames are drawn no faster than the screen's refresh rate.
- **A frame takes two ticks.** It is rendered offscreen the way a recording
  frame is, and read back from the GPU on the next tick rather than waited on,
  so the desktop never stalls for a capture. Expect one to two refreshes of
  latency.
- **Physical pixels.** Frames are at the screen's own density, as screenshots
  are. A window capture uses the density of the screen the window's centre is
  on.
- **A window capture is the window at rest**: where the layout puts it, not
  where an animation is drawing it this frame, so a tile easing into place is
  not a new size every frame. While the window is minimized, on a workspace no
  screen shows, or off every screen, a pending frame waits.
- **Locked means nothing.** While the session is locked no capture delivers
  anything; a pending frame waits for the unlock, and a frame drawn just before
  the lock is thrown away rather than delivered after it. The lock screen is
  never captured.
- **The recording dot** shows in the top-right corner of every screen a capture
  has delivered a frame of in the last second, as it does for `Super+Print`.
  It is not in the frames, and neither are notification cards.

### `raven_region_selection_v1` — version 1

| | |
|---|---|
| `destroy` | request, destructor. Takes the picker down if it is still up. |
| `selected(output, x, y, width, height)` | event. The rectangle dragged out, in logical pixels relative to the screen the drag started on, clipped to it. |
| `cancelled()` | event. `Escape`, a click that dragged nothing, the session locking, or a picker that could not go up — one was already up (the client's or `Shift+Print`'s), or the session was locked. |

Exactly one of the two events is sent. The result is shaped for
`capture_region`.

### `raven_workspace_state_v1` — version 1

| | |
|---|---|
| `destroy` | request, destructor |
| `activate(index)` | request. Switch to a workspace. An out-of-range index is ignored, not clamped. |
| `state(count, active, occupied)` | event. Sent once on creation and again whenever a field changes, never when nothing did. `occupied` is a bitmask; bit N is set if workspace N holds a window. Caps at 32. |

### `raven_output_layout_v1` — version 1

| | |
|---|---|
| `destroy` | request, destructor |
| `set_position(name, x, y)` | request. Stage where a screen's top-left corner goes, in logical pixels. |
| `set_scale(name, scale)` | request. Stage an effective scale for a screen; 0 returns it to the one derived from its size. |
| `apply()` | request. Apply every staged change at once, save the result, and report the new geometry. A name that matches no connected screen is saved for when it connects. |
| `output(name, x, y, width, height, scale, physical_width, physical_height, mm_width, mm_height, focused)` | event. One per screen, followed by `done`. Sent on creation and again whenever the set of screens or their geometry changes. |
| `done()` | event. The set of `output` events is complete. |

Positions that would overlap are pushed right of what they collide with,
and the arrangement is shifted so its top-left is the origin, so the geometry
reported after `apply` is the truth rather than the request. The arrangement
is kept in `$XDG_STATE_HOME/raven/outputs` by connector name. `raven-output`
(`crates/tools/raven-output`) is the command-line client and the reference for
a settings page. See `docs/outputs.md`.

### Stability

Additive changes only. New requests and events may be added and the interface
version bumped; an existing one is never changed or removed. That discipline is
what keeps moving `protocols/raven-shell-v1.xml` into its own repository a
mechanical step, and it is why the crate is treated as a public API contract
even while it lives in this tree.

### Privilege

The manager global is meant to be advertised only to clients the compositor
considers privileged. **Huginn does not enforce that yet** — every client can
bind it. This is a tracked gap rather than a design decision, and any future
gating will apply to this global, so do not build on being able to bind it from
arbitrary software.

Since version 4 the gap is a real one: **any client on the session can read
the screen** through `raven_capture_v1`, and any window on it by identifier.
The recording dot is the only sign a capture is running, and it cannot stop
one. On a single-user machine every client already runs as the person whose
screen it is, which is why this shipped ungated; it must be gated before
anything untrusted — a sandboxed application, a remote client — runs on
Huginn.

## `ext-session-lock-v1`

Implemented, and used by `raven-lock` from RavenLogin. Three things about this
compositor's implementation are worth knowing before writing another client:

**A locked session is not composited.** `Huginn::scene` returns the lock surface
and nothing else, and `frame_surfaces` stops issuing frame callbacks to anything
underneath. Clients below the lock do not keep painting into buffers nobody
shows.

**The lock is confirmed immediately, not after a vblank.** The session stops
being composited at the instant the lock is taken, so the next frame on the
panel cannot contain the desktop and there is nothing to wait for. Waiting for a
presentation would leave the client believing the machine was unlocked for one
refresh longer than it was.

**The compositor can blank before any client asks.** On resume from suspend
huginn hides the session first and starts the lock screen second, because doing
it the other way round shows the desktop for as long as a process takes to exec.
A client's `lock` request then claims that blank. If nothing claims it within
ten seconds the blank comes down, so a broken lock screen leaves a desktop
rather than a machine that has to be power-cycled.

**No privilege filter.** Any client may lock. The protocol's filter exists to
restrict the global to a privileged client, and on a single-user session there
is nothing to distinguish one client from another: every client here already
runs as the person whose session it is.

## D-Bus: `org.freedesktop.Notifications`

Not a Wayland global, but part of the same contract. Huginn owns
`org.freedesktop.Notifications` on the session bus and serves version 1.2 of
the Desktop Notifications specification; `docs/integration.md` says what it
supports. It asks for the name without replacing an owner that already has it:
a notification daemon started before Huginn keeps the name, and Huginn becomes
the owner when that daemon exits.

### `org.raven.Notifications`

The same object also serves a notification centre, for the desktop's own
software (RoostBar's clock panel) rather than for applications, which should
keep to the standard interface:

| Member | Signature | Meaning |
|---|---|---|
| `List` | `() → a(usssxb)` | Open notifications and the session's history, newest first: id, application name, icon, summary, body without markup, arrival in Unix seconds, still open. Nothing waiting behind the lock screen is listed. |
| `Remove` | `(u) → ()` | Dismiss the notification if it is open (its client gets `NotificationClosed` with reason 2) and forget it from the history. An id that names nothing is not an error. |
| `Clear` | `() → ()` | `Remove` for everything listed. |
| `Changed` | signal, `()` | What `List` returns is different. Not sent when only a card's position changes. |

## Absent

Not stubs — these globals do not exist, and a client asking for them will not
find them in the registry.

| Missing | Consequence |
|---|---|
| `wlr-foreign-toplevel-management-v1` | No window *management* from outside: an external dock or switcher can list windows (see `ext_foreign_toplevel_list_v1` above) but cannot activate, close or minimize one. |
| `wlr-screencopy`, `ext-image-copy-capture-v1` | No *standard* screen capture: no screen sharing through the portal or PipeWire, and third-party recorders (OBS, wf-recorder) find nothing to use. Client capture exists, but through `raven_capture_v1` above, which only Raven's own software speaks. |
| `wp-presentation-time` | Clients cannot get precise presentation feedback. Media players fall back to their own timing. |
| `zwp_primary_selection_v1` | No middle-click paste. The regular clipboard works. |
| `text-input-v3`, `input-method-v2` | No input methods. CJK and other IME input will not work. |
| `wlr-virtual-pointer`, `virtual-keyboard-v1` | No input injection. Remote-desktop and automation tools cannot drive the session. |
| `ext-idle-notify-v1` | A client cannot be told the session went idle. The compositor locks on its own timer, so auto-lock works — what is missing is any way for *other* software to react to idleness. |
| `tablet-v2` | Graphics tablets are not routed. |
| `single-pixel-buffer-v1`, `content-type-v1`, `alpha-modifier-v1` | Minor optimisations unavailable. |
| `drm-lease-v1` | No direct-lease VR headsets. |
| `security-context-v1` | Sandboxes cannot identify themselves, which is also why privilege gating above has no mechanism to build on yet. |

The window list itself is provided — `ext_foreign_toplevel_list_v1` above —
so a switcher or a window-list panel can be written; what it cannot yet do is
act on what it lists.

## Deliberately not planned

Configuration protocols. Raven ships one look, compiled in, and no interface
here will let a client change the compositor's appearance or behaviour. A
protocol that lets a client *read* the appearance so its own surfaces can match
is a different thing and is proposed separately.
