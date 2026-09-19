# Outputs

How Huginn treats more than one screen, and what happens at the edges.

## The model

One desktop, in one logical coordinate space. Every screen is a rectangle in
it, laid out left to right with the built-in panel (`eDP-*`, `LVDS-*`,
`DSI-*`) at the origin and the rest in connector-name order after it. A window
has one position in that space and is drawn by whichever screens it overlaps;
the pointer is one point in it and crosses between screens where they touch.

Workspaces belong to screens. Each screen shows one of them, no workspace
shows on two screens, and the *focused* screen is the one whose workspace is
active — where new windows open, where `Super+Ctrl` bindings act, where the
shell's own panels (dock, launcher, quick settings, volume) sit. Focus follows
the pointer between screens and only between screens; within one, clicking
moves it as it always has.

`Super+Ctrl+1..9` on a workspace another screen is showing moves focus to that
screen rather than pulling the workspace across. On a hidden workspace it
shows it on the focused screen. `Super+Ctrl+Tab` focuses the next screen;
`Super+Ctrl+Shift+Tab` sends the focused window there.

This is the model in `huginn-core`: `Space::outputs`, `Workspace::output`,
and `Space::set_outputs`, with tests in `crates/comp/huginn-core/src/lib.rs`
for what happens when screens come and go.

## Scale

Each screen has its own [scale policy](../README.md#scale) from its EDID
millimetres: a 1x laptop panel next to a 2x 4K monitor is the ordinary case,
not a special one. The desktop is laid out in logical pixels; each screen
renders its part of it at its own fractional scale, and clients are told the
integer scale of the screens they are on through `wl_surface.enter`/`leave`,
which the compositor sends once a frame as windows move. A window dragged from
the 1x panel to the 2x monitor is re-rendered by its toolkit at 2x when it
crosses. The cursor is loaded once per density in use.

The mode a screen is driven at is its native resolution — the one the EDID
marks preferred, since anything else goes through the monitor's own scaler —
at the fastest refresh offered at that size. A 144 Hz panel that marks its
60 Hz mode preferred runs at 144.

## Arrangement

Screens are placed by `huginn_core::layout::arrange` from two inputs: what is
connected, and what was saved. A screen with a saved position goes there; one
without goes to the right of everything placed so far, the built-in panel
first. Two saved positions that would overlap keep the one further up and
left and push the other right of it, and the whole arrangement is shifted so
its top-left corner is the origin. Every case is a test in
`crates/comp/huginn-core/src/layout.rs`.

What was saved lives in `$XDG_STATE_HOME/raven/outputs`, one screen per line
by connector name, with an optional scale: `HDMI-A-1 0,-1440` or `eDP-1 0,0
1.5`. State rather than config, by the reasoning that applies to the pins: the
desktop writes it in response to what the person did. It is set through
`raven_output_layout_v1` (`docs/protocols.md`) — `raven-output above HDMI-A-1
eDP-1`, `raven-output scale eDP-1 1.5` — and the compositor applies, saves and
reports the result in one step. A saved entry for a screen that is not
connected waits for it: the monitor placed above the laptop a week ago comes
back above the laptop.

A saved scale overrides the one the panel's size implies, through the same
integer-advertised, fractional-laid-out policy as the automatic one.

## Workspaces across screens

Every screen shows one workspace from a shared row, and no workspace is on
two screens. The rule for moving between them is that navigating on one
screen never changes another:

- `Super`+`Ctrl`+a digit for a workspace another screen is showing moves
  focus to that screen; nothing is rearranged.
- A swipe, the overview and `Super`+wheel step over workspaces another screen
  is showing. The overview draws them dimmed and labels them `on DP-1`.
- The overview belongs to the screen it opened on, and only that screen draws
  it; the others go on showing their own workspaces.

Taking something from another screen is always asked for:

- Holding `Shift` while letting go of a swipe on another screen's workspace
  pulls it (`Space::pull_workspace`). A workspace with windows is merged into
  this one, and the screen that showed it is left on its now empty desktop;
  an empty one is swapped, so this screen gets the empty desktop and that one
  gets what was here.
- `Super`+`Ctrl`+`Shift`+an arrow brings the focused window of the screen that
  way into this workspace (`Space::pull_focused_from_output`); focus stays
  here.

A pulled window is taken out of fullscreen, and a merge is not held to the
tile cap.

## Rotation

A line may end with `rotate=90`, `rotate=180` or `rotate=270` — quarter turns
counter-clockwise, numbered as `wl_output.transform` numbers them — as in
`DP-1 1920,0 rotate=90` or `DP-1 - 1.5 rotate=270`. Set it with
`raven-output rotate DP-1 90`, or from the Display page in Settings; both go
through `raven_output_layout_v1.set_rotation` (since version 5).

A turned screen is measured turned: `relayout` sets the output's transform and
derives the scale from the panel's size with width and height traded, so the
core, the layer shell and clients all see a portrait screen as tall. Only that
screen's workspace is laid out again. The panel's mode is unchanged.

Where the frame is turned depends on how the screen is driven. On the
rendering GPU the DRM compositor follows the output's transform, as it does
for the bridge's copy on a second GPU (the bridge itself is drawn upright, at
the turned size); a display-only device's dumb buffer is drawn turned directly.
Screenshots, recordings and captures are the turned frame
(`OutputInfo::frame_size`), and a touchscreen's reports, which are in the
panel's own orientation, are turned back to the scene in `touch_location`.

## Second GPU and display-only devices

The compositor renders on the primary GPU and drives the connectors of every
DRM device on the seat. For a screen on another device, that screen's view of
the desktop is rendered on the primary exactly as a primary screen is, and
then handed over — see `crates/comp/huginn-comp/src/backend/gpu.rs`:

- **A second GPU** (the discrete card on a hybrid laptop, which owns the HDMI
  and USB-C ports on most of them): the view is rendered into a linear
  dmabuf the primary allocated, the secondary imports that buffer once as a
  texture and draws it on its own scan-out surface each frame. One GPU copy.
- **A device with no GPU** (`udl`, `evdi` — DisplayLink — or a card whose
  driver has no Mesa backend): the view is rendered into a texture, read
  back, and written into a dumb buffer the device page-flips. One CPU copy
  per frame, which is what DisplayLink costs everywhere.

smithay's multi-GPU renderer covers only the first case, since it needs GL on
every device, and its output manager cannot drive dumb buffers; both paths
here share the primary's render path unchanged. Devices come and go with
udev like connectors do: a dock plugged in brings its screens up, unplugged
takes them down and the workspaces on them come home.

## Hotplug

A monitor plugged in gets a workspace of its own — the first one nobody is
looking at — and comes up as a desktop, not a blank. A monitor unplugged sends
its workspaces to the screen that remains with their windows and focus intact;
the pointer, if it was there, is moved to the nearest screen. A monitor
swapped on a live cable (a KVM, a different display on the same port) keeps
its `wl_output` and gets a mode event. With no screens at all the last layout
is kept so nothing is resized to zero and back.

Panels made with `zwlr_layer_shell_v1` stay on the screen they asked for; if
that screen goes, they land on the first. Exclusive zones are per screen: a
dock on the laptop reserves nothing on the monitor.

## Not done

- **A chosen primary.** The focused screen follows the pointer; there is no
  way to say the shell's panels should stay on one screen regardless.
- **Direct scan-out on a second GPU.** A fullscreen client on a screen of the
  discrete GPU still goes through the bridge; its buffer would have to live on
  that GPU to be scanned out directly.
- **Fullscreen and top-layer panels.** A fullscreen window on any screen
  currently drops the `top` layer behind it on the focused screen only.
