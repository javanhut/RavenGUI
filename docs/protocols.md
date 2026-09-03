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
| `raven_shell_manager_v1` | workspace count, active index, occupancy, switching; opening quick settings |
| `wl_shm` | shared-memory buffers |
| `zwp_linux_dmabuf_v1` | hardware buffers; advertised once the backend has a renderer |
| `wp_viewporter` | source cropping and destination scaling |
| `wp_fractional_scale_manager_v1` | fractional scale factors |
| `wl_output`, `xdg_output` | mode, integer scale, logical size, position; `wl_surface.enter`/`leave` per screen |
| `wl_seat` | keyboard and pointer |
| `wp_cursor_shape_manager_v1` | a client names its cursor and the compositor draws it from the system theme, at the screen's density |
| `wl_data_device_manager` | clipboard and drag-and-drop |
| `xwayland_shell_v1` | XWayland only; associates an X11 window with its surface |
| `ext_session_lock_manager_v1` | locking the session. See below |

X11 clients work: Huginn spawns XWayland and runs a window manager for it,
including override-redirect surfaces — menus, tooltips, drag icons — drawn at
the coordinates the client chose.

## `raven_shell_v1`

The contract between Huginn and the desktop shell, covering only what no
standard protocol provides. Panels, the dock and the wallpaper are layer-shell
surfaces; this file does not duplicate them.

### `raven_shell_manager_v1` — version 3

| | |
|---|---|
| `destroy` | request, destructor. Objects made through the manager are unaffected. |
| `get_workspace_state` | request. Creates a `raven_workspace_state_v1`. |
| `open_quick_settings` | request, since 2. Opens the compositor-drawn quick settings panel as the keybinding would. A no-op if it is already open, and while the session is locked. |
| `get_output_layout` | request, since 3. Creates a `raven_output_layout_v1`. |

The second version exists for a bar whose battery reading is a natural place
to click: the panel it should lead to is drawn by the compositor, so the bar
cannot open it by showing a surface of its own and has to ask. A client bound
at version 1 sees no difference.

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

## Absent

Not stubs — these globals do not exist, and a client asking for them will not
find them in the registry.

| Missing | Consequence |
|---|---|
| `ext-foreign-toplevel-list-v1`, `wlr-foreign-toplevel-management-v1` | No window list. Task switchers, external docks and window-list panels — waybar's taskbar, wlrctl, rofi's window mode — cannot work. |
| `wlr-screencopy`, `ext-image-copy-capture-v1` | No *client* screen capture: no screen sharing, and no third-party screen recorder. The compositor takes its own screenshots on `Print` (see `docs/integration.md`), so there is no protocol here to do it through. |
| `wp-presentation-time` | Clients cannot get precise presentation feedback. Media players fall back to their own timing. |
| `zwp_primary_selection_v1` | No middle-click paste. The regular clipboard works. |
| `text-input-v3`, `input-method-v2` | No input methods. CJK and other IME input will not work. |
| `pointer-constraints`, `relative-pointer` | No pointer lock or warping. Games and 3D applications cannot capture the cursor. |
| `wlr-virtual-pointer`, `virtual-keyboard-v1` | No input injection. Remote-desktop and automation tools cannot drive the session. |
| `ext-idle-notify-v1` | A client cannot be told the session went idle. The compositor locks on its own timer, so auto-lock works — what is missing is any way for *other* software to react to idleness. |
| `idle-inhibit-unstable-v1` | A client cannot hold the idle lock off. A full-screen video is indistinguishable from an empty room, so a film longer than the timeout locks the screen mid-play. The way out is the "Lock when idle" row in quick settings, which is a person saying what a protocol would otherwise have said for them. |
| `tablet-v2` | Graphics tablets are not routed. |
| `single-pixel-buffer-v1`, `content-type-v1`, `alpha-modifier-v1` | Minor optimisations unavailable. |
| `drm-lease-v1` | No direct-lease VR headsets. |
| `security-context-v1` | Sandboxes cannot identify themselves, which is also why privilege gating above has no mechanism to build on yet. |

One of these is load-bearing for the desktop rather than for third-party
software: without foreign-toplevel there is no way to write a switcher at all.

## Deliberately not planned

Configuration protocols. Raven ships one look, compiled in, and no interface
here will let a client change the compositor's appearance or behaviour. A
protocol that lets a client *read* the appearance so its own surfaces can match
is a different thing and is proposed separately.
