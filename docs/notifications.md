# Notifications in Huginn — plan

Status: phases 1–4 done. The core logic is in `huginn-core/src/notify.rs`; the
D-Bus server, the cards, their placement and their input are in
`huginn-comp/src/notifications/`. Huginn serves notifications by default: cards
are drawn, expire, pause under the pointer, and can be clicked, acted on and
dismissed, with `Super+Ctrl+N` and `Super+Ctrl+Shift+N` from the keyboard.
Phase 5 is done too: the lock screen, fullscreen windows, idle inhibitors and
time away decide what interrupts; do not disturb is a quick settings row and
`[notifications]` in desktop.toml, written by Raven Settings' Notifications
page; and quick settings brings back what arrived quietly. Phase 6 is done:
mako is uninstalled and nothing starts it, so Huginn owns the name from login.
Still to come: a list of closed notifications (the core keeps it; nothing shows
it yet).

The README says the compositor draws the shell, notifications included, because
"anything that must feel instant and must never fail does not get to be a
separate process". The dock, launcher and quick settings exist; notifications
do not. Until they do, nothing on a Raven session owns
`org.freedesktop.Notifications`, so every application's notifications go
nowhere, and the stopgap is a third-party daemon (mako) that knows nothing
about the desktop: it guesses a layer, sits behind fullscreen windows, cannot
see the lock screen, and does not look like Raven.

This plan puts the whole thing inside Huginn, built from the parts the shell
already uses.

## What it has to be

- **The standard.** Any application that notifies on Linux talks to
  `org.freedesktop.Notifications` (spec 1.2). Huginn serves it in full:
  `Notify`, `CloseNotification`, `GetCapabilities`, `GetServerInformation`,
  and the `ActionInvoked` and `NotificationClosed` signals. No Raven-only API
  for applications — Oracle and Firefox call the same thing.
- **Raven's look.** Cards drawn with `Canvas::material`, the accent, the panel
  radius, cosmic-text and the icon theme, like the dock and quick settings.
- **Aware of the desktop.** The compositor knows what a daemon cannot: the lock
  screen, fullscreen, idle inhibitors, screen recording, which window belongs
  to which application. Every one of those changes what a notification should
  do, and the policy uses them.
- **Never able to stall a frame.** D-Bus lives on its own thread, as
  `bluetooth.rs` already does. The render loop only ever reads a queue.

## Architecture

Three layers, the same split as the rest of Huginn.

```
 application ──D-Bus──▶ notifications/bus.rs   (thread: zbus, owns the name)
                              │  calloop::channel<Incoming>
                              ▼
                        huginn-core/src/notify.rs   (pure: queue, timing, policy)
                              │
                              ▼
                        notifications/card.rs  (canvas → Panel)  ─▶ scene()
                              ▲
                  input.rs clicks, hover ──┘   signals go back over mpsc ─▶ bus thread
```

### 1. `huginn-core/src/notify.rs` — the logic, no I/O

Everything that decides anything, in the crate that has no dependencies and
tests in milliseconds. It never reads the clock; callers pass `now: Duration`.

- `Notification { id, app_name, app_icon, summary, body: Vec<Span>, actions,
  urgency, category, desktop_entry, image: Option<ImageRef>, transient,
  resident, expire: Expire, arrived }`.
- `Expire` from the spec's `expire_timeout`: `-1` → default by urgency
  (low 4 s, normal 6 s, critical never), `0` → never, `n` → n ms.
- `Queue`: insert with `replaces_id` semantics (same card updated in place, no
  new animation), ids never 0 and never reused within a session, at most three
  cards on screen with an "and N more" row, hover pauses every visible card's
  expiry, expiry returns the ids to close with reason 1.
- `CloseReason::{Expired = 1, Dismissed = 2, Closed = 3, Undefined = 4}`.
- Body markup: the spec's subset (`<b> <i> <u> <a href> <img>`) parsed into
  spans; unknown tags stripped, entities decoded. `<img>` becomes its alt text.
- `Policy`: `fn present(n: &Notification, ctx: Context) -> Presentation`,
  where `Context { locked, do_not_disturb, fullscreen, idle_inhibited,
  recording }` and `Presentation::{Show, Silent, Hidden}`:

  | Situation | low / normal | critical |
  |---|---|---|
  | normal desktop | Show | Show |
  | do not disturb | Silent (history only) | Show |
  | fullscreen or idle-inhibited (video, slides) | Silent | Show |
  | screen recording | Show, left out of the capture | Show, left out of the capture |
  | locked | Hidden until unlock, then Silent | Hidden until unlock, then Show |

  "Silent" still records the notification, so nothing is lost; it just does
  not interrupt.

- `History`: the last 50 closed notifications for the session, in memory only.

Tests follow `layer.rs`: inline `#[cfg(test)] mod tests`, names that read as
sentences — `a_replacement_updates_the_card_without_a_second_entry`,
`hovering_holds_every_card_on_screen`, `critical_ignores_do_not_disturb`.

### 2. `huginn-comp/src/notifications/bus.rs` — the D-Bus server

Modelled on `bluetooth.rs`, with one new thing: owning a well-known name on
the session bus, which nothing in Huginn does yet.

- `notifications::start::<D>(handle, state)`, called beside `bluetooth::start`
  in `backend/udev.rs` and `backend/winit.rs`, so both backends get it.
- A dedicated thread builds
  `zbus::blocking::connection::Builder::session()?.name("org.freedesktop.Notifications")?.serve_at("/org/freedesktop/Notifications", Server)?.build()`.
- `#[zbus::interface(name = "org.freedesktop.Notifications")] impl Server`:
  - `Notify` must return the id synchronously, so ids come from an
    `Arc<AtomicU32>` on this thread; `replaces_id` is passed through. The
    notification goes to the loop as `Incoming::Notify` on a
    `calloop::channel` (the `fileindex.rs` pattern).
  - `CloseNotification` → `Incoming::Close(id)`.
  - `GetCapabilities` → `actions`, `body`, `body-markup`, `icon-static`,
    `persistence`. Not `sound` (Huginn plays none) and not
    `body-hyperlinks` until links are clickable.
  - `GetServerInformation` → `("Huginn", "Raven", <crate version>, "1.2")`.
  - Hints read: `urgency` (byte), `category`, `desktop-entry`, `transient`,
    `resident`, `image-data`/`image_data`/`icon_data` (`(iiibiiay)`),
    `image-path`. Anything else is ignored, as the spec allows.
- Signals: the loop sends `Outgoing::{Invoked(id, key), Closed(id, reason)}`
  over a std `mpsc`; the thread emits them through the interface's
  `SignalEmitter` from `object_server().interface()`.
- **Failing soft**, as bluetooth does: no session bus, or the name already
  taken, logs one `tracing::warn!` and retries every few seconds. The desktop
  runs without notifications rather than failing to start. If the name is
  held by another daemon, Huginn does not fight it; the fix is to remove that
  daemon.
- Sender identity: the message header's sender is resolved to a PID via
  `GetConnectionUnixProcessID`, so a notification without `desktop-entry` can
  still be matched to its window (see Activation).

### 3. `huginn-comp/src/notifications/card.rs` — drawing

- One `canvas::Panel` per visible card, composed like `audio::render`: scale
  `(output.h / 1080).clamp(1, 2.5) * density`, every constant multiplied by it.
- Layout of a card, 380 × auto logical px at 1080p:
  - `material(...)` ground at `PANEL_RADIUS`; a 3 px accent edge on the left
    for critical.
  - Icon, 40 px: `image-data` → pixmap; else `image-path`/`app_icon` as a file
    or icon name via `raven_desktop::Icons::find`; else the icon from the
    application's `.desktop` entry; else a generic bell (symbolic, tinted).
  - App name and relative time, `TEXT_DIM`, one line.
  - Summary, weighted, one line, ellipsized.
  - Body, up to three lines, wrapped. `text.rs` draws only unwrapped text, so
    this adds `Text::draw_wrapped(surface, spans, size, width, max_lines)`
    over `layout_runs()`, with the ellipsis done the way `dock.rs` does it.
  - Actions as buttons on the `WELL` / `WELL_RAISED` style, at most three; the
    `default` action is the card itself, not a button.
  - A close control that appears on hover.
- **Placement:** top-right of the focused output, inside its usable area (below
  any bar's exclusive zone), stacked downwards with a gap. Recomputed from the
  same function for drawing and hit testing, as the launcher does.
- **Motion:** each card has an `anim::Reveal` (slide in from the right edge and
  fade); cards below slide up when one closes. `settings::Motion::duration`
  makes it instant when reduced motion is on.

### 4. Into the scene

Following the checklist the other overlays already obey:

1. Push the cards in `Huginn::scene()` above the blur boundary. Order: help,
   volume OSD, **notifications**, launcher, pinned, settings. A notification
   is above the launcher so a click on it is never swallowed by the launcher
   closing, and below the volume OSD, which is shorter-lived.
2. Add the visible card count to `blur_boundary()` (state.rs ~4492). An
   undercount pushes a panel into the blurred group.
3. Recompose in `refresh_output_panels()` when the output or scale changes.
4. Initialise to empty where the other panels are `None` (state.rs ~1107).
5. Leave them out of screenshots and recordings by counting them in
   `capture_hidden_len()`, the way the recording dot is — a notification's
   text should not end up in a screen recording by accident.

Because the cards are compositor items, not layer surfaces, fullscreen
stacking is decided by the policy, not by a protocol rule. That is the bug
mako hit, removed by design.

### 5. Timing

- Animation uses frame ticks, like the volume OSD: `tick_notifications()` in
  `tick_animations()` queues a redraw while any `Reveal` is moving.
- Expiry does **not** keep frames running for six seconds of a still card. One
  calloop `Timer` is armed for the earliest deadline in the queue and re-armed
  (`TimeoutAction::ToDuration`) when the queue changes, the pattern of the
  idle-lock timer in `udev.rs`. The frame loop is idle while a card just sits
  there.

### 6. Input

- `backend/input.rs` `motion()`: `notifications_pointer_moved()` updates hover
  (which pauses expiry and shows the close control) and, while the pointer is
  over a card, clients do not get the motion.
- `button()` on press, beside `launcher_click()`:
  - card body → `default` action if the notification has one, then activate
    the application (below), then close with reason 2 unless `resident`;
  - action button → `ActionInvoked(id, key)`, same closing rule;
  - close control, or a right click anywhere on the card → close, reason 2;
  - the click is swallowed either way.
- `axis()`: scrolling over the stack scrolls the "and N more" overflow.
- Keyboard, through the keymap filter like the other panels:
  `Super+N` dismisses the newest card, `Super+Shift+N` dismisses all. No card
  ever takes keyboard focus from a window.

### 7. Activation — something only the compositor can do

When a notification is clicked, Huginn raises the window it came from: match
`desktop-entry`, else the sender's PID, else `app_name`, against window app ids
and client PIDs, and focus the first match on any workspace (switching to it).
A daemon cannot do this at all; applications currently have to handle
`ActionInvoked` and raise themselves.

The spec's `ActivationToken` signal needs `xdg-activation-v1`, which Huginn
does not implement (`docs/protocols.md`). Raising the window directly covers
the common case; the token is follow-up work once xdg-activation exists.

### 8. Fitting into the rest of Raven

- **Quick settings:** a `Do not disturb` row — a toggle `Control` in the list
  built by `Settings::with_power` (settings.rs ~964), read through
  `Settings::do_not_disturb()`. Its secondary text shows how many arrived
  silently. Activating it while silent notifications are waiting opens the
  history.
- **History:** an "Earlier" section at the foot of quick settings listing
  `History` newest first, each dismissable, with "Clear". Phase 2; the core
  type is built in phase 1 so nothing is thrown away before there is a place
  to show it.
- **desktop.toml:** a new `[notifications]` section in `desktop_config.rs` —
  `do_not_disturb`, `position` (`top-right` | `top-center`), `timeout_seconds`
  — added to `Default`, the module doc, the README key table, a parse test,
  and applied in `new` and `reload_desktop_config`. Raven Settings writes this
  file, so Raven Settings needs the same keys and a Notifications page, or the
  choices can only be made by hand. The quick-settings DND toggle, like the
  Animations row today, lasts until the next reload unless Settings writes it.
- **Lock screen:** while `is_locked()`, nothing is drawn and nothing times
  out; arrivals wait and are presented, per policy, after unlock. The lock
  screen is a separate process and shows nothing from the session.
- **Idle:** time does not count toward expiry while the session is idle, so a
  notification that arrived while you were away is still there when you come
  back.

## Phases

1. **Core** — `huginn-core/src/notify.rs` with the queue, expiry, markup,
   policy and history, fully tested. No behaviour change anywhere.
2. **Server** — `bus.rs` owning the name, wired to the core queue, logging
   what arrives. Verified with `gdbus call ... Notify` and `GetServerInformation`
   against a nested Huginn (`imlazy run`), and with Oracle's own notifications.
   A zbus peer-to-peer test (`connection::Builder::unix_stream` on a socket
   pair) checks signatures, return values and signals without a session bus.
3. **Cards** — `card.rs`, scene integration, `Reveal` motion, the expiry
   timer, capture exclusion. Visual check with the nested compositor and
   `layer-probe`-style scripts that send bursts, replacements and long bodies.
4. **Input and activation** — clicks, actions, hover, dismissal, raising the
   source window, keyboard shortcuts.
5. **Ecosystem** — policy wired to lock, fullscreen, idle inhibit, recording;
   DND row; history in quick settings; `[notifications]` in desktop.toml and
   Raven Settings; docs (`README.md`, `docs/integration.md`,
   `docs/protocols.md` gains a D-Bus section).
6. **Retire mako** (done) — `rvn uninstall mako`, remove `~/.config/mako`.
   Oracle needs no change: it already speaks the standard interface.

Each phase ends with `cargo test --workspace`, `cargo clippy --workspace
--all-targets -- -D warnings` and `./scripts/check-unsafe.sh` clean.

## Decisions to make before phase 3

- **Position:** top-right (conventional, clear of a centred launcher) or
  top-centre (Raven's launcher and settings are centred). Proposed: top-right,
  configurable.
- **Fullscreen:** proposed Silent for low/normal, Show for critical. The
  alternative is holding them and showing them when fullscreen ends.
- **DND persistence:** a quick-settings toggle that lasts only until the next
  config reload, as Animations does, or a written choice. Proposed: written to
  desktop.toml, which means Huginn writes a key in a file Settings owns — a
  first — or a small state file of Huginn's own, as `pins.rs` keeps.
- **History in phase 1 or 2:** proposed 2.
