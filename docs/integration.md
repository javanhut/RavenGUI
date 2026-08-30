# Integrating with Huginn

How to write desktop software that runs on the Raven desktop — a panel, a bar, a
wallpaper, a launcher, a status readout — without building it into the
compositor.

Huginn draws its own dock, launcher, quick settings and notifications inside the
render loop, because the design spec says the shell is not a client: anything
that must feel instant and must never fail does not get to be a separate process
that can miss a frame or die. That rule governs the shell Raven *ships*. It does
not stop you adding a surface of your own, and this document is how.

There is no configuration file and there will not be one. Everything below is a
protocol a client speaks, not a file it edits.

## What you can build today

| You want | Use | Status |
|---|---|---|
| A panel, bar, dock or wallpaper | `wlr-layer-shell` | Works |
| A panel that takes typing | `keyboard_interactivity` | Works |
| Workspace indicators | `raven_shell_v1` | Works |
| An ordinary application window | `xdg-shell` | Works |
| A task switcher or window list | foreign-toplevel | **Not implemented** |
| Replacing the launcher or dock | role claiming | **Not designed in** |
| Reading the desktop's accent colour | appearance protocol | **Not implemented** |

The last three are the subject of a separate proposal. Nothing in this document
depends on them.

## Panels, bars and wallpapers

Connect to `$WAYLAND_DISPLAY` like any Wayland client and bind
`zwlr_layer_shell_v1`. Huginn implements the whole of it: four layers, anchors,
per-edge margins, exclusive zones, keyboard interactivity, and popups belonging
to a layer surface.

### Reserving space

A surface that anchors to an edge and sets a positive `exclusive_zone` has that
space taken out of the tiling area — Huginn shrinks the space windows are laid
out in and rearranges. Zones stack, so a panel and a dock on the same edge each
get their share.

Space is reserved only while you have something on screen. A layer surface
declares its zone before its first commit and may unmap and stay alive later; in
both cases it reserves nothing, so the desktop never tiles around a gap with no
panel in it. Your `configure` still arrives before you have a buffer — you need
it to know what size to draw — so the sequence is: declare the zone, receive a
configure, attach a buffer, and the desktop makes room at that point.

The protocol only gives an exclusive zone a meaning when the surface is anchored
to exactly one edge, or to one edge plus the two edges perpendicular to it.
Anchored to a corner or to all four edges there is no unambiguous edge to
reserve from, and Huginn reserves nothing rather than guessing.

A zone larger than the output starves the tiling area to nothing rather than
producing a negative one. If your panel asks for 99999 pixels the desktop will
have no room for windows, which is your bug and will look like it.

### Sizing

A zero in either dimension of `set_size` means "you decide", which is how a
panel asks to span an edge. Anchoring to both edges of an axis stretches the
surface across it; anchoring to neither centres it on that axis. Huginn sends a
`configure` with the size it settled on, and only when that size actually
changed — a configure you did not need would have you redraw, commit, and land
straight back here.

### Taking the keyboard

`keyboard_interactivity` is honoured. Three modes, and Huginn resolves between
competing claims in a fixed order:

1. **`exclusive`** on the `top` or `overlay` layer takes the keyboard for as
   long as the surface is mapped. Topmost layer wins; among surfaces on the same
   layer the most recently mapped wins, so a prompt opening over an already
   exclusive panel is the thing that gets answered.
2. **`on_demand`** takes the keyboard when the surface is clicked, and gives it
   back when the user clicks anywhere else.
3. **`none`** never takes it. This is the protocol default and the right answer
   for a wallpaper or a status readout — a surface that spans the output and
   accepts focus would take the keyboard away from the user's window every time
   they clicked the desktop.

`exclusive` requested on the `bottom` or `background` layer is **demoted to
`on_demand`** rather than refused. A wallpaper that could take the keyboard
outright would hold it against every window on the desktop for as long as it
stayed mapped, which from the user's side is indistinguishable from a hung
session. You can still click into such a surface.

Holding `exclusive` **retires the focus ring**. Every keystroke is going to your
surface, and a ring still drawn around the window behind you would be claiming
otherwise. The window keeps its `activated` state — it is still where the
keyboard returns when you go away — so a toolkit that styles itself on activation
will still look focused; only the compositor's own ring goes. An `on_demand`
surface does *not* retire the ring, because it hands the keyboard back on the
next click elsewhere and blinking the ring off for that would make clicking a bar
flicker the desktop.

Two things this does not change. The compositor's own keybindings are tested
before anything is forwarded, so an `exclusive` surface cannot swallow the chord
that closes it. And a focused layer surface never counts as owning the `Super`
layer — see below.

Both destroying a surface and unmapping it release the keyboard. Unmapping is the
one worth saying out loud: a surface that attaches a null buffer stays alive and
keeps its place in the layout, but it stops being a candidate for focus, so a
panel that hides itself while holding `exclusive` hands the keyboard back rather
than swallowing keystrokes with nothing on screen. Attaching a buffer again makes
it a candidate again.

### Where you land in the stack

Front to back:

1. Huginn's own compositor-drawn surfaces — help overlay, launcher, quick
   settings, dock
2. `overlay` layer, then `top` layer, each with their popups
3. override-redirect X11 windows
4. window popups
5. the focus ring
6. windows
7. `bottom` layer, then `background` layer

Note the first entry. **The compositor's own surfaces draw above every layer
surface, including `overlay`.** You cannot draw over the dock or the launcher.
This is deliberate — those are the surfaces that must never be obscured by
something that failed — and it is the one place where the shell being in-process
is visible to you.

### Popups

A layer surface's popups are drawn directly above that surface rather than above
everything, so a panel's menu does not land on top of an unrelated overlay.
Create them the ordinary way, with the layer surface as the parent.

## Workspace state

`raven_shell_v1` covers what no standard protocol provides. Bind
`raven_shell_manager_v1` (version 1) and call `get_workspace_state`.

You get one `state` event immediately on creation, so there is never a moment
where you have to render a placeholder while you wait, and another whenever any
field changes. Fields are the workspace count, the zero-based active index, and
an occupancy bitmask where bit N is set if workspace N holds at least one
window. The mask caps at 32 workspaces; there are nine.

`activate(index)` switches workspace. An out-of-range index is **ignored rather
than clamped** — clamping would turn a bug in your client into a silent jump to
an unrelated workspace, which is harder to notice than nothing happening.

One caveat worth knowing: this global is advertised to every client. It is meant
to be privileged and is not yet gated, so treat the fact that you *can* bind it
as a current implementation state rather than a promise.

## Keys you may not take

Huginn owns these chords. They are tested before any key is forwarded, so a
client cannot receive or override them:

| Chord | Does |
|---|---|
| `Super+Ctrl+E` / `T` | open a terminal |
| `Super+Ctrl+Q` / `X` | close the focused window |
| `Super+Ctrl+J` / `K` | focus the next / previous window |
| `Super+Ctrl+arrows` | move the focused window between tiles |
| `Super+Ctrl+Return` | swap the focused window into the first tile |
| `Super+Ctrl+C` | open or accept the workspace carousel |
| `Super+Ctrl+P` | open the settings application |
| `Super+Ctrl+R` | resize the focused window with the arrows |
| `Super+Ctrl+1..9` | go to a workspace |
| `Super+Ctrl+Shift+1..9` | send the focused window to a workspace |
| `Super+C` / `Super+V` | copy / paste in the focused client |
| `Super+L` | lock the session |
| `Super+Ctrl+Space` | open the application launcher |
| `Super+Ctrl+A` | open the pinned applications |
| `Super+Ctrl+S` | open quick settings |
| `Super+Ctrl+H` | show or hide the keybinding list |
| `Super+Ctrl+Esc` | quit the compositor |
| volume keys | raise, lower or mute the output volume |

`Super+L` is the one chord on the plain `Super` layer that is never handed back.
`Super+C` and `Super+V` are given to a client that drives `Super` itself, since
a terminal has its own use for them; `Super+L` is not, because a lock chord that
does nothing whenever a terminal happens to be focused fails at exactly the
moment somebody walks away from a machine believing they locked it.

While the session is locked no chord in this table resolves at all — every key
goes to the lock screen, including `Super+Ctrl+Esc`. The volume keys are the
one exception: they act on the speakers rather than on the session, and they
work whatever is open — the launcher, quick settings, or the lock screen. Each
press shows a slider at the bottom of the screen for a moment; the level is set
through `wpctl`, so it needs PipeWire, and the slider says "not connected" when
there is none. The same level is a row in quick settings (`Super+Ctrl+S`),
where the left and right arrows step it and `Return` mutes.

The pinned panel (`Super+Ctrl+A`) shows the applications the user has pinned,
drawn the way the launcher draws its suggestions. Things get onto it through
the launcher: `Tab` on an application opens its actions menu, whose last item
is "Pin" (or "Unpin"). On the panel, `Return` opens the highlighted
application, `Delete` unpins it, `Shift`+arrows move it, and `Tab` opens a
menu with the same choices. Two rows in quick settings decide where the panel
sits ("Pinned apps": centre, top, bottom, left or right) and how it is laid
out ("Pinned layout": a grid, a single row, or a column). The pins, their
order and that layout are kept in `$XDG_STATE_HOME/raven/pins`, beside the
launch history and for the same reason — it is state the desktop writes, not
configuration the user is expected to.

The Bluetooth row in the same panel talks to BlueZ over D-Bus (it needs
`bluetoothd` running; without it the row says "not connected"). `Return` on
the row toggles the radio. The arrows step through what the row could be
instead — Off, On, each paired device, and Scan — shown with a question mark,
and `Return` applies it: a paired device connects, a connected one
disconnects, Scan lists what is in range as it is found and `Return` on a
new device pairs it, marks it trusted and connects. When pairing wants a
six-digit number confirmed the row shows it; `Return` is yes, moving off the
row is no. The row cannot type a PIN, so a device that insists on one needs
`roostbar bt pair` or `bluetoothctl`. Leaving the row stops a scan.

The session also locks itself after a period with no input — ten minutes by
default, changed or turned off from the "Lock when idle" row in quick settings
(`Super+Ctrl+S`). Every input event counts as presence, including one that
resolves to no binding. There is no `idle-inhibit-unstable-v1`, so a client
cannot hold this off; a video player that needs to has to ask the person
watching to turn the row off.

There is no way for a client to register a global chord of its own. If your
software needs one, it currently has to be reached from a dock icon or by being
spawned.

## Touchpad gestures huginn takes

| Gesture | Does |
|---|---|
| three fingers sideways | preview and switch between workspaces |
| three fingers down | minimize the focused pane without closing it |
| three-finger double tap | temporarily show minimized applications in a centered dock |
| sideways while the centered dock is open | highlight a minimized application |
| three fingers up while the centered dock is open | restore the highlighted application into the current workspace |

The current workspace shrinks into a centred card while the workspaces beside
it appear as narrow, dimmed side cards. The row follows the fingers and settles
onto the workspace nearest where they lift; that workspace then expands to fill
the output. Each card is a complete workspace, not a window from the current
workspace. `Super+Ctrl+C` opens the same view from the keyboard and a second
press accepts the centred workspace.

**A three-finger swipe never reaches a client.** Huginn advertises no
`pointer-gestures-unstable-v1` global, so no client can see swipe, pinch or hold
gestures at all today — they are not being taken away from you by this binding,
they were never delivered. Two-finger scrolling is unaffected: libinput reports
it as an axis event rather than a gesture, and it is forwarded as normal.

The centered dock dismisses itself after four seconds without a choice; an
`Escape` press dismisses it immediately. A vertical swipe stays vertical for
its whole length even if it later drifts sideways, so minimizing never jerks
the workspace carousel partway across the screen.

### The `Super` rule

**Plain `Super` belongs to the focused application.** `Super+C` and `Super+V`
above are translated: the compositor synthesises `Ctrl+C` / `Ctrl+V` for the
focused client, because copy is not a compositor operation — the clipboard
belongs to the client, and there is no protocol for asking one to copy.

Applications that use `Super` as their own leader are exempt from that
translation, by `app_id`, and the list is compiled in: `raven-terminal`,
`Alacritty`, `com.mitchellh.ghostty`, `foot`, `kitty`,
`org.wezfurlong.wezterm`. A terminal not on that list would get `Ctrl+C`
delivered where it expected copy, which a shell reads as SIGINT and hands to
whatever is running — so if you are shipping a terminal, that list is the thing
to be added to.

A client that advertises **no `app_id` at all** gets the translation, because it
cannot be told apart from a terminal that advertises none. Set an `app_id`.

A focused layer surface never owns `Super`: a namespace is not an `app_id`, and
a panel is not a terminal.

## Matching the desktop

Raven ships one look and it is compiled into the compositor. There is no theming
engine and no way to query these at runtime yet, so a client that wants to match
has to carry the same values:

| Role | Value |
|---|---|
| Accent — focus ring, headings, running-app indicator | `#7AA2F7` |
| Panel and overlay background | `#16161F` |
| Hairline borders | `#2A2A3A` |
| Body text | `#D0D0E0` |
| Secondary text | `#8A8AA0` |
| Gutter between tiles and at the screen edge | 8 px |
| Focus ring thickness | 2 px |
| Icon theme | `breeze-dark` |

Copying these into your binary means they can drift from the compositor's, which
is exactly the problem the compositor solved internally by defining each colour
once. Making them readable over a protocol is proposed but not implemented;
until then, this table is the source and `crates/comp/huginn-comp/src/theme.rs`
is the definition.

Resolve `Icon=` names against `breeze-dark` rather than bare `hicolor`.
Measured on the development machine, 10 of 36 installed applications had no icon
under `hicolor` and 1 under `breeze-dark` — a launcher that uses the spec's
universal fallback draws blanks for a third of the menu.

## Scale and outputs

Huginn advertises an integer scale on `wl_output` and lays the desktop out at a
fractional one, so render at the integer scale you are given and let the
compositor resample. `wp_viewporter` and `wp_fractional_scale_v1` are both
available. `xdg_output` reports the logical size, which agrees with the desktop
the compositor actually laid out.

Several outputs share one logical coordinate space; `xdg_output` gives each
its position. `wl_surface.enter`/`leave` are sent as a surface crosses screens,
so take your scale from the outputs you have entered, as the toolkits do.
`zwlr_layer_shell_v1.get_layer_surface` honours its output argument: pass the
`wl_output` you want a panel on, or `None` for the focused screen, and make one
surface per output for a bar that should be on every screen. Exclusive zones
are per screen. Lock screens are asked for one surface per output and every
screen without one stays blank. See `docs/outputs.md`.

## Developing against it

You do not need a spare machine or a TTY. Huginn picks its nested backend
whenever it inherits a `WAYLAND_DISPLAY`, so it runs inside your existing
session as an ordinary window:

```sh
cargo run -p huginn-comp        # a compositor in a window
```

It prints the socket it created — `huginn is up socket=wayland-2` — and your
client connects to that instead of the outer session:

```sh
WAYLAND_DISPLAY=wayland-2 ./my-panel
```

The nested backend needs no seat, no DRM master and no TTY, so it also works
over ssh. `Super+Ctrl+Esc` quits it.

### A client to check the compositor against

`layer-probe` is a layer-shell client that exists to make the behaviours above
visible while you develop against them. It draws a flat band, prints every
configure, focus change and click, and turns from the panel background to the
accent while it holds the keyboard.

```sh
cargo run -p layer-probe -- --interactivity exclusive   # takes the keyboard on map
cargo run -p layer-probe -- --interactivity on-demand   # takes it on a click
cargo run -p layer-probe -- --interactivity none        # never takes it
cargo run -p layer-probe -- --cycle 2                   # unmap and remap every 2s
cargo run -p layer-probe -- --anchor left --size 200 --exclusive 0
```

`--cycle` is the one worth running against a compositor change: an unmapped
surface must give the keyboard back and stop reserving its zone, and take both
again when it maps. The tiled windows behind it grow and shrink as it does.

`Esc` exits.

## See also

- `docs/protocols.md` — every global Huginn advertises, and what is absent
- `protocols/raven-shell-v1.xml` — the `raven_shell_v1` contract
- `README.md` — architecture, and why the shell is drawn in-process
