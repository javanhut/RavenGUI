//! Pointer, touchpad and touchscreen input, shared by both backends.
//!
//! winit reports absolute positions inside its window; libinput reports
//! relative deltas from a physical mouse. Both end up here so that focus
//! behaviour, clamping and hit testing cannot drift between the two.
//!
//! Touchpad gestures and touchscreen contacts only ever arrive from libinput —
//! winit's backend types them as the uninhabited `UnusedEvent`, so the arms
//! below compile there and can never run. What a gesture *means* still lives in
//! [`crate::gesture`], and what a set of fingers means in [`crate::touch`],
//! rather than here: that is what keeps both testable on a machine with
//! neither.
//!
//! # One press, two devices
//!
//! A finger and a mouse button both press things the compositor draws, and the
//! rules for which of those things gets the press — the card in front of
//! everything, then the launcher, then the overview, then the dock, then a
//! title bar, then the window — are subtle, ordered, and exactly the same for
//! both. So they are written once, in [`shell_press`], and the two devices
//! differ only in what they hand it. A second copy for the touchscreen would
//! be a second copy to keep in step, and the first time it fell behind the
//! answer would be that the dock works with a mouse and not with a finger.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        GestureBeginEvent, GestureEndEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent, TouchSlot,
    },
    input::{
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
        touch::{DownEvent, MotionEvent as TouchMoveEvent, UpEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER, Size},
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
    },
};

use crate::{
    state::Huginn,
    touch::{Landing, Lift, Owner},
};

/// Feed one input event to the compositor.
///
/// Pointer motion and buttons, wheel and touchpad gestures, and touchscreen
/// contacts. Keyboard events do not come here — each backend resolves those
/// against its own keymap. Anything else is ignored.
pub(crate) fn handle<B: InputBackend>(state: &mut Huginn, event: InputEvent<B>) {
    match event {
        InputEvent::PointerMotion { event } => {
            // A `Super`+left drag is fed the raw delta, not the clamped
            // one: the swipe should keep travelling when the pointer has
            // run into the edge of the screen, as fingers keep travelling
            // when the cursor they are not drawing has.
            let delta = event.delta();
            relative_motion(state, delta, event.delta_unaccel(), event.time());
            if state.pointer_locked() {
                state.pointer().frame(state);
                return;
            }
            state.drag_moved(delta.x, delta.y);
            // Raw as well, for the same reason: a pointer shaken against the
            // edge of the screen is still being shaken.
            state.pointer_moved_by(delta.x, delta.y, event.time_msec());
            let location =
                constrained_location(state, state.clamp_pointer(state.pointer_location + delta));
            motion(state, location, event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            // Absolute devices report a fraction of the surface they are bound
            // to, so the position has to be scaled by the output and then
            // offset by where that output sits in global space.
            let area = state.output_area();
            let extent: Size<i32, Logical> = (area.w(), area.h()).into();
            let origin: Point<f64, Logical> = (f64::from(area.x()), f64::from(area.y())).into();
            if state.pointer_locked() {
                return;
            }
            let location = constrained_location(
                state,
                state.clamp_pointer(event.position_transformed(extent) + origin),
            );
            // No raw delta here; the difference in position is the best
            // there is, and a nested window's edge is where it ends.
            let delta = location - state.pointer_location;
            state.drag_moved(delta.x, delta.y);
            state.pointer_moved_by(delta.x, delta.y, event.time_msec());
            motion(state, location, event.time_msec());
        }
        InputEvent::PointerButton { event } => button::<B>(state, &event),
        InputEvent::PointerAxis { event } => axis::<B>(state, &event),
        // Three fingers sliding sideways drive the carousel. Not forwarded to
        // any client: huginn advertises no pointer-gestures protocol, so there
        // is nothing downstream this could be taken away from.
        InputEvent::GestureSwipeBegin { event } => state.swipe_begin(event.fingers()),
        InputEvent::GestureSwipeUpdate { event } => {
            state.swipe_update(event.delta_x(), event.delta_y());
        }
        // The end event's `cancelled` is deliberately not read; see
        // `Huginn::swipe_end`.
        InputEvent::GestureSwipeEnd { .. } => state.swipe_end(),
        InputEvent::GestureHoldBegin { event } => state.hold_begin(event.fingers()),
        InputEvent::GestureHoldEnd { event } => {
            state.hold_end(event.cancelled(), event.time_msec());
        }
        // Fingers on the glass. What a set of them means is [`crate::touch`]'s;
        // what happens to one is below.
        //
        // `touch.enabled` is checked on each of them rather than once at
        // startup, because it is turned off in the middle of a session by
        // somebody whose digitizer has started reporting touches nobody made
        // -- see [`crate::desktop_config::Touch`]. A hand that was down when
        // it was turned off is released by `reload_desktop_config`, so there
        // is nothing left here for these to close.
        InputEvent::TouchDown { event } if state.touch_enabled() => {
            touch_down::<B>(state, &event);
        }
        InputEvent::TouchMotion { event } if state.touch_enabled() => {
            touch_motion::<B>(state, &event);
        }
        InputEvent::TouchUp { event } if state.touch_enabled() => touch_up::<B>(state, &event),
        InputEvent::TouchCancel { event } if state.touch_enabled() => {
            touch_cancel::<B>(state, &event);
        }
        // The end of a set of touch events that belong together. Passed
        // straight through: the compositor groups its own sends around each
        // event it handles, and a client that batches on frames needs the
        // device's own boundaries as well as those.
        InputEvent::TouchFrame { .. } if state.touch_enabled() => {
            let touch = state.touch();
            touch.frame(state);
        }
        _ => {}
    }
}

/// Send the unbounded device delta before absolute cursor handling. Relative
/// pointer clients receive these events even without a lock; while locked this
/// is the only motion they receive, so mouse-look never runs into an edge.
fn relative_motion(
    state: &mut Huginn,
    delta: Point<f64, Logical>,
    delta_unaccel: Point<f64, Logical>,
    utime: u64,
) {
    let pointer = state.pointer();
    pointer.relative_motion(
        state,
        None,
        &RelativeMotionEvent {
            delta,
            delta_unaccel,
            utime,
        },
    );
}

/// Keep an active confinement inside its surface (and optional region). A
/// binary search preserves as much of a large physical delta as possible
/// instead of making the pointer stick one whole event before the edge.
fn constrained_location(state: &Huginn, proposed: Point<f64, Logical>) -> Point<f64, Logical> {
    let pointer = state.pointer();
    let Some(surface) = pointer.current_focus() else {
        return proposed;
    };
    let region = with_pointer_constraint(&surface, &pointer, |constraint| {
        constraint.and_then(|constraint| {
            (constraint.is_active() && matches!(*constraint, PointerConstraint::Confined(_)))
                .then(|| constraint.region().cloned())
        })
    });
    let Some(region) = region else {
        return proposed;
    };

    let allowed = |location: Point<f64, Logical>| {
        state
            .surface_under(location)
            .is_some_and(|(under, origin)| {
                under == surface
                    && region.as_ref().is_none_or(|region: &RegionAttributes| {
                        region.contains((location - origin.to_f64()).to_i32_round())
                    })
            })
    };
    if allowed(proposed) {
        return proposed;
    }

    let start = state.pointer_location;
    let delta = proposed - start;
    let mut inside = 0.0;
    let mut outside = 1.0;
    for _ in 0..16 {
        let middle = (inside + outside) / 2.0;
        let candidate = Point::from((start.x + delta.x * middle, start.y + delta.y * middle));
        if allowed(candidate) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    Point::from((start.x + delta.x * inside, start.y + delta.y * inside))
}

/// Tell whatever is under the pointer that it is there, though the pointer
/// has not moved.
///
/// A window that opens under a resting pointer, a tile that reflows beneath
/// it, a launcher that closes off it: the pointer is now over a different
/// surface, and until it moves nobody has said so. The client gets no
/// `enter`, draws no hover, sets no cursor, and the first scroll goes to the
/// surface that used to be there. A motion to where the pointer already is
/// settles all of it.
pub(crate) fn rehover(state: &mut Huginn) {
    let location = state.pointer_location;
    let time = state.uptime().as_millis() as u32;
    motion(state, location, time);
}

fn motion(state: &mut Huginn, location: Point<f64, Logical>, time: u32) {
    state.pointer_location = location;
    // A region screenshot is being framed: the pointer draws the rectangle and
    // nothing else. It does not cross outputs (the shot is fixed to the screen
    // it began on) and it does not reach clients.
    if state.region_active() {
        state.region_pointer_moved();
        return;
    }
    state.pointer_crossed_outputs();
    // The dock watches the bottom edge. Told before the event is forwarded, so
    // a reveal and the client's own motion land in the same frame -- unless the
    // session is locked, in which case the dock is not on screen and a pointer
    // at the bottom edge must not reveal it. `surface_under` is already empty
    // of everything but the lock, so the motion itself is harmless; this is
    // about the compositor's own drawing, which does not go through the scene.
    if !state.is_locked() {
        state.notifications_pointer_moved();
        state.dock_pointer_moved();
        state.launcher_pointer_moved();
        state.pinned_pointer_moved();
        state.overview_pointer_moved();
    }
    // The launcher is compositor-drawn, so no client is under the pointer
    // while it is there as far as the scene knows — but a window behind the
    // panel is, and it must not be told about a pointer the user sees as
    // being on the panel. Leaving the client is what stops its hover
    // effects tracking a pointer that is not on it.
    // A title bar is the same: compositor-drawn, above the client it frames,
    // and the client must not see a pointer that is on the bar. The cursor
    // goes back to the arrow too, since whatever shape the client last asked
    // for was for its own content.
    // A notification card is compositor-drawn in the same way, in front of
    // everything the pointer can reach.
    let under = if state.notifications_cover_pointer()
        || state.launcher_covers_pointer()
        || state.pinned_covers_pointer()
    {
        None
    } else if !state.is_locked() && state.decor_covers_pointer() {
        state.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
        None
    } else {
        state.surface_under(location)
    };
    let pointer = state.pointer();
    pointer.motion(
        state,
        under
            .as_ref()
            .map(|(surface, position)| (surface.clone(), position.to_f64())),
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    if let Some((surface, origin)) = under {
        let local = location - origin.to_f64();
        with_pointer_constraint(&surface, &pointer, |constraint| {
            if let Some(constraint) = constraint
                && !constraint.is_active()
                && constraint
                    .region()
                    .is_none_or(|region| region.contains(local.to_i32_round()))
            {
                constraint.activate();
            }
        });
    }
    pointer.frame(state);
    // The cursor moved, so the frame on screen is stale even if no client
    // changed anything.
    state.queue_redraw();
}

fn button<B: InputBackend>(state: &mut Huginn, event: &B::PointerButtonEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let button_state = event.state();

    // A capture drawing clicks rings every press, whoever it goes to. Only
    // into the capture: the screen itself never shows it.
    if button_state == ButtonState::Pressed {
        state.note_capture_click();
    }

    // The button that began a `Super`+left drag ends it, however `Super`
    // stands by then and whatever has happened since — a lock included,
    // which is why this is first. The release is the compositor's, as the
    // press was: a client must not see the up of a button it never saw go
    // down.
    if button_state == ButtonState::Released
        && event.button_code() == crate::mouse::BTN_LEFT
        && state.drag_active()
    {
        state.drag_end();
        state.queue_redraw();
        return;
    }

    // Tap-to-click touchpads encode three fingers as a middle-button press.
    // Touchpads only — the devices that report gestures — because on a mouse
    // a double middle click is two pastes of the primary selection, and a
    // strip opening over them would be the touchpad's shortcut leaking onto
    // a device that has a binding of its own for it: `Super`+click, below.
    if button_state == ButtonState::Pressed
        && event.button_code() == crate::mouse::BTN_MIDDLE
        && event.device().has_capability(DeviceCapability::Gesture)
    {
        state.middle_tap(event.time_msec());
    }

    // Locked: the click reaches the lock screen, through the ordinary pointer
    // path at the bottom of this function, and nothing else. Everything skipped
    // between here and there reads the desktop directly rather than through the
    // scene -- the dock is compositor-drawn, and click-to-focus and the layer
    // claim walk the window list and the layer list -- so an empty scene does
    // not stop them on its own. A click that raised a window or activated a
    // dock item behind a lock screen would be a click that acted on a session
    // whose whole point, right now, is that nobody is acting on it.
    if state.is_locked() {
        let pointer = state.pointer();
        let under = state.surface_under(state.pointer_location);
        if let Some((surface, _)) = under.as_ref() {
            // Focus follows the click into the lock surface, so a compositor
            // that had the keyboard elsewhere hands it over on the first press.
            state.set_keyboard_focus(Some(surface.clone().into()), serial);
        }
        pointer.button(
            state,
            &ButtonEvent {
                button: event.button_code(),
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(state);
        state.queue_redraw();
        return;
    }

    // A region screenshot owns the pointer while it is up: the primary button
    // drags the rectangle out, releasing takes it, and nothing reaches a client
    // underneath. Checked after the lock, which owns the pointer more strongly
    // still, and before click-to-focus, which would otherwise raise a window
    // the drag merely passed over.
    if state.region_active() {
        const BTN_LEFT: u32 = 0x110;
        if event.button_code() == BTN_LEFT {
            match button_state {
                ButtonState::Pressed => state.region_press(),
                ButtonState::Released => state.region_release(),
            }
        }
        return;
    }

    // Everything the compositor itself draws gets the press before any client
    // does, in the order it is drawn. Shared with the touchscreen; see
    // [`shell_press`] and the module note above.
    //
    // Presses only. A release falls through, so that a swallowed press's
    // release still goes out and no client sees the up of a button it never
    // saw go down.
    if button_state == ButtonState::Pressed {
        let gesture_device = event.device().has_capability(DeviceCapability::Gesture);
        let source = Source::Pointer { gesture_device };
        if shell_press(state, event.button_code(), source) == Taken::Shell {
            return;
        }
    }

    let pointer = state.pointer();
    pointer.button(
        state,
        &ButtonEvent {
            button: event.button_code(),
            state: button_state,
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
}

/// Where a press came from, for the few steps of [`shell_press`] that differ
/// between a button under a finger and a finger itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A pointer button. Whether the device also reports gestures decides what
    /// a middle press means: a touchpad's three-finger tap arrives as one, and
    /// it is the strip's rather than the dock's.
    Pointer { gesture_device: bool },
    /// A finger on the glass. It carries no modifiers and has no second or
    /// third button, so the chords are not consulted at all — see the arms
    /// below that ask.
    Touch,
}

/// Whether the compositor's own drawing took a press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taken {
    /// Something the compositor draws acted on it, and nothing is forwarded:
    /// the press was aimed at a card, the launcher, the overview, the dock or
    /// a title bar, and a client underneath must not also see it.
    Shell,
    /// Nothing there wanted it. It belongs to whatever is under it — which for
    /// the last two steps below is a window that has just been focused by it.
    No,
}

/// The shell's claim on a press, asked in the order the things that make it
/// are drawn: front to back, so what you press is what you can see.
///
/// Every step here reads the desktop directly rather than through the scene —
/// the dock and the launcher are compositor-drawn, and click-to-focus walks
/// the window list — so an empty scene does not stop any of them on its own.
///
/// Called for presses only; see the caller.
fn shell_press(state: &mut Huginn, button: u32, source: Source) -> Taken {
    // A notification card is drawn in front of everything but the recording
    // dot, so it is asked first. A press on a card acts on the card — a left
    // click takes it or the control it landed on, a right click dismisses it —
    // and never reaches what is behind. See `Huginn::notifications_click`.
    if state.notifications_click(button) {
        return Taken::Shell;
    }

    // The keybinding overlay takes any press while it is up: off the panel it
    // closes, on it nothing happens. Swallowed either way, as the launcher's
    // are — it is drawn over everything, so a click is aimed at the list or at
    // getting rid of it, not at a window the user can barely see behind it.
    if state.help_click() {
        return Taken::Shell;
    }

    // `Super`+a button is the mouse's three-finger gesture — see
    // [`crate::mouse`]. After the lock and the region selection, which own
    // the pointer outright, and before everything that reads the desktop: a
    // press that put a window away must not also focus what was under it.
    // Releases are not bound, so a swallowed press's release still goes
    // through, exactly as a dock click's does.
    //
    // A finger never reaches this. `Super`+tap is not a gesture anybody makes
    // on a panel, and a touchscreen has the real three fingers to do it with.
    if matches!(source, Source::Pointer { .. })
        && let Some(keyboard) = state.seat.get_keyboard()
        && let Some(click) = crate::mouse::binding(button, &keyboard.modifier_state())
        && state.mouse_click(click)
    {
        return Taken::Shell;
    }

    // Click to focus. Done on press rather than release so that a click-drag
    // starting in an unfocused window focuses it before the drag begins.
    //
    // Two things are exempt. A click on a popup belongs to the window that
    // opened the popup, and a menu routinely hangs over the tile next door —
    // focusing whatever lies under it would dismiss the menu and focus the
    // wrong window in the same gesture. And while any grab is active the grab
    // decides where input goes, so moving focus underneath it would leave the
    // grab holding a seat that is pointing somewhere else.
    let on_popup = state
        .surface_under(state.pointer_location)
        .is_some_and(|(surface, _)| state.is_popup(&surface));
    // The launcher, while it is open, takes the primary click the way it
    // takes every key: on the panel it launches, off the panel it dismisses.
    // Asked before the dock, which the panel is drawn over. Other buttons
    // fall through unchanged — a right click has no meaning here, and
    // swallowing it would make the pointer feel dead.
    if button == crate::mouse::BTN_LEFT && (state.launcher_click() || state.pinned_click()) {
        return Taken::Shell;
    }

    // The overview owns the primary click while it is up: on a window's
    // patch it takes that window, anywhere else it dismisses and the tiling
    // goes back. Before the dock and the scene both — everything under the
    // overview is scenery while it is showing, and a click that fell through
    // to a window's stale rectangle would act on a desktop nobody can see.
    if button == crate::mouse::BTN_LEFT && state.overview_click() {
        return Taken::Shell;
    }

    // The dock's context menu owns the press while it is up: on a row it does
    // what the row says, anywhere else it puts the menu away. Either way the
    // press is the menu's and reaches nothing behind it — which is what a
    // menu being open means.
    if state.dock_menu_is_open() {
        state.dock_menu_click();
        return Taken::Shell;
    }

    // The right button on a dock icon opens that icon's menu. Swallowed even
    // when nothing opens — on the launcher button, which is not an
    // application — so that a right click on the dock never starts anything:
    // a button that launches a browser because there was no menu to show is
    // a button nobody can trust.
    if button == crate::mouse::BTN_RIGHT && state.dock_click().is_some() {
        state.open_dock_menu();
        return Taken::Shell;
    }

    // The dock is compositor-drawn, so it is not under the pointer as far as
    // any client is concerned. It has to be asked first, or a click on it
    // falls through to whatever window is behind it.
    if let Some(item) = state.dock_click() {
        // A middle click, or `Ctrl`+click, on an icon opens another window
        // of that application; any other press is the ordinary click that
        // starts or raises it. A touchpad's three-finger tap arrives as a
        // middle press too and is *not* this: that tap is the strip's, and
        // a tap on the dock that started a second copy of something would
        // be the touchpad shortcut misfiring on the wrong device.
        //
        // A finger has neither of those, so a tap on the dock always starts
        // or raises. Opening a second window stays something you do with a
        // keyboard in reach, rather than a gesture invented for the glass
        // that nothing on screen could tell you about.
        let anew = match source {
            Source::Pointer { gesture_device } => {
                let ctrl_click = button == crate::mouse::BTN_LEFT
                    && state.seat.get_keyboard().is_some_and(|keyboard| {
                        let mods = keyboard.modifier_state();
                        mods.ctrl && !mods.logo && !mods.alt && !mods.shift
                    });
                let middle_click = button == crate::mouse::BTN_MIDDLE && !gesture_device;
                ctrl_click || middle_click
            }
            Source::Touch => false,
        };
        if anew {
            state.launch_dock_item_anew(&item);
        } else {
            state.activate_dock_item(&item);
        }
        return Taken::Shell;
    }

    // A click on a layer surface that asked for the keyboard is how it takes
    // focus, and a click anywhere else is how it gives it back. Settled before
    // click-to-focus, because a panel overlapping a tile must not also raise
    // the window behind it — the click belongs to whatever is drawn on top.
    let clicked_layer = if !on_popup && !state.pointer().is_grabbed() {
        let hit = state.layer_under(state.pointer_location);
        let landed = hit.is_some();
        if state.set_focused_layer(hit) {
            state.refresh_focus();
        }
        landed
    } else {
        false
    };

    // A title bar the compositor drew. A press anywhere on it focuses its
    // window; the primary button on the close button asks the window to
    // close. Nothing reaches a client: the bar is not the client's, and a
    // press that started on chrome must not become a drag inside the window
    // under it. Any button is swallowed, since the bar has no other use for
    // one and a right click falling through to the content would be a click
    // on something the user cannot see there.
    if !on_popup
        && !clicked_layer
        && !state.pointer().is_grabbed()
        && let Some((window, hit)) = state.decor_hit()
    {
        state.space.focus_window(window);
        state.refresh_focus();
        if hit == crate::decor::Hit::Close
            && button == crate::mouse::BTN_LEFT
            && let Some(surface) = state.surface(window)
        {
            surface.close();
        }
        return Taken::Shell;
    }

    if !on_popup
        && !clicked_layer
        && !state.pointer().is_grabbed()
        && let Some(window) = state.window_under(state.pointer_location)
    {
        // `focus_window` rather than the active workspace's own focus: the
        // click may have landed on the other screen, and a window that is not
        // on the active workspace would otherwise be highlighted by the ring
        // and never given the keyboard.
        state.space.focus_window(window);
        state.refresh_focus();
    }

    // Focus may have moved above, but the press itself is the client's.
    Taken::No
}

fn axis<B: InputBackend>(state: &mut Huginn, event: &B::PointerAxisEvent) {
    let source = event.source();
    if wheel::<B>(state, event, source) {
        return;
    }
    let mut frame = AxisFrame::new(event.time_msec()).source(source);

    for axis in [Axis::Horizontal, Axis::Vertical] {
        if let Some(value) = event.amount(axis) {
            // A finger lifting off a touchpad reports 0.0 to mark the end of a
            // scroll gesture. Clients rely on that stop event to end kinetic
            // scrolling, so it must be forwarded rather than filtered out as
            // "no movement".
            if value == 0.0 && source == AxisSource::Finger {
                frame = frame.stop(axis);
            } else {
                frame = frame.value(axis, value);
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
        }
    }

    let pointer = state.pointer();
    pointer.axis(state, frame);
    pointer.frame(state);
}

/// The compositor's own wheel bindings, taken before the client's.
///
/// `Super`+wheel steps through the workspaces; `Super`+`Ctrl`+wheel grows or
/// shrinks the focused tile.
///
/// This is the mouse's three-finger swipe. A trackpad has the gesture and a
/// mouse does not, so without this the only way to the workspace next door
/// with a mouse in hand is a key. And as the fingers do, the wheel drives
/// whichever picker is up: it slides the overview's row, and a notch up
/// brings the strip's highlighted window back.
///
/// Wheels only. A touchpad's two-finger scroll arrives here as
/// [`AxisSource::Finger`] with no v120 to count, and the device it comes from
/// already has the swipe — taking it would add a second, worse way to do the
/// same thing on the one pointer that needs it least.
///
/// The two chords are the way round they are because of what they act on.
/// Stepping the workspaces is about the desktop, and the desktop's gesture is
/// the plainest chord there is: exactly `Super`, which needs no keysym to
/// disambiguate it the way `Super`+`Ctrl` disambiguates the keyboard's window
/// management from the plain `Super` layer the client owns. Resizing a tile
/// *is* window management, so it takes the modifier the window-management keys
/// already use.
///
/// The resize is offered the event first, and takes it only if it has a
/// divider to move: a `Super`+`Ctrl` turn that resizes nothing — nothing tiled
/// focused, a picker up — falls through to whatever the plain classification
/// would have done with it. What the other chords mean while a picker is up is
/// [`Huginn::wheel_workspace`]'s to decide, which is why the classification is
/// passed along rather than settled here.
///
/// Returns whether the event was taken.
fn wheel<B: InputBackend>(
    state: &mut Huginn,
    event: &B::PointerAxisEvent,
    source: AxisSource,
) -> bool {
    if !matches!(source, AxisSource::Wheel | AxisSource::WheelTilt) {
        return false;
    }
    let Some(keyboard) = state.seat.get_keyboard() else {
        return false;
    };
    let chord = crate::wheel::Chord::of(&keyboard.modifier_state());
    // Vertical first: it is the wheel every mouse has, and a device that
    // reports both at once is tilting while scrolling, which is one gesture
    // too many to guess at. Zero is skipped rather than banked — libinput
    // names both axes on an event that moved one of them.
    let Some((axis, v120)) = [Axis::Vertical, Axis::Horizontal]
        .into_iter()
        .find_map(|axis| {
            let amount = event.amount_v120(axis)?;
            (amount != 0.0).then_some((axis, amount as i32))
        })
    else {
        // A wheel with no discrete travel at all is one this cannot count in
        // notches, so it goes to the client rather than being swallowed.
        return false;
    };
    if chord == crate::wheel::Chord::SuperCtrl && state.wheel_resize(axis, v120) {
        return true;
    }
    state.wheel_workspace(axis, v120, chord)
}

/// Where on the desktop a contact belonging to `device` actually is.
///
/// A touchscreen reports a fraction of the panel it is stuck to, never of the
/// desktop, which is the whole difference between this and the absolute-pointer
/// arm in [`handle`]. See [`Huginn::touch_output`] for how the panel is found.
///
/// Clamped to that screen. A fraction is inside its own panel by construction,
/// so this only ever catches a miscalibrated or lying device — and the failure
/// it prevents is the interesting one: an out-of-range fraction on a laptop
/// with a monitor plugged in puts the finger on the *other* screen, where it
/// presses something the user is not looking at.
fn touch_location<B: InputBackend>(
    state: &Huginn,
    device: &str,
    event: &impl AbsolutePositionEvent<B>,
) -> Point<f64, Logical> {
    let output = state.touch_output(device);
    let area = output.rect;
    let origin: Point<f64, Logical> = (f64::from(area.x()), f64::from(area.y())).into();
    // The digitiser is glued to the panel and reports in the panel's own
    // orientation. On a turned screen the finger is found on the upright
    // panel and then turned the way the scene was, backwards.
    let transform = output
        .output
        .as_ref()
        .map_or(smithay::utils::Transform::Normal, |o| o.current_transform());
    let turned: Size<f64, Logical> = (f64::from(area.w()), f64::from(area.h())).into();
    let panel = transform.transform_size(turned);
    let raw = event.position_transformed((panel.w as i32, panel.h as i32).into());
    let at = transform.invert().transform_point_in(raw, &panel) + origin;
    let max_x = f64::from(area.right() - 1).max(f64::from(area.x()));
    let max_y = f64::from(area.bottom() - 1).max(f64::from(area.y()));
    (
        at.x.clamp(f64::from(area.x()), max_x),
        at.y.clamp(f64::from(area.y()), max_y),
    )
        .into()
}

/// A finger landed.
fn touch_down<B: InputBackend>(state: &mut Huginn, event: &B::TouchDownEvent) {
    let slot = event.slot();
    let id = i32::from(slot);
    let device = event.device().name();
    let location = touch_location::<B>(state, &device, event);
    let time = event.time_msec();
    // Read before the contact is recorded: a claim below rewrites every
    // finger's owner to the gesture's, and after that there is no way left to
    // tell that one of them had been standing in for the pointer.
    let was_emulating = state.contacts.any_emulating();

    match state.contacts.down(id, location) {
        // A gesture already has the hand. Counted, and nothing else.
        Landing::Ignore => {}
        // This contact completed the hand. Whoever had the earlier fingers is
        // told the sequence was taken rather than left half finished -- that
        // is what `wl_touch.cancel` is for -- and from here until the last
        // finger lifts the hand drives the same recogniser the touchpad does.
        Landing::Claims => {
            let touch = state.touch();
            touch.cancel(state);
            // The pointer may have been emulating for an X11 window; that
            // contact is the gesture's now, so the button it pressed has to be
            // let go of or the window is left with it held down forever.
            if was_emulating {
                release_primary_button(state, time);
            }
            state.swipe_begin(crate::gesture::CAROUSEL_FINGERS);
            state.queue_redraw();
        }
        Landing::Route => {
            let owner = route_touch(state, location, slot, time);
            state.contacts.took(id, owner);
        }
    }
}

/// Send the first contact to whatever should have it, and say who that was.
fn route_touch(
    state: &mut Huginn,
    location: Point<f64, Logical>,
    slot: TouchSlot,
    time: u32,
) -> Owner {
    let serial = SERIAL_COUNTER.next_serial();
    // Everything the compositor draws asks where the *pointer* is, because
    // until now the pointer was the only thing that could be anywhere. Rather
    // than teach the dock, the launcher, the overview, the title bars and the
    // hit tests to take a position, the finger becomes the pointer's position
    // for as long as it is down. The cursor is not drawn while it is -- see
    // `Huginn::pointer_visible` -- so nothing appears to jump.
    state.pointer_location = location;

    // Locked: the touch reaches the lock screen and nothing else, for exactly
    // the reasons the pointer's button handler gives. Everything below this
    // reads the desktop directly, so an empty scene does not stop it.
    if state.is_locked() {
        let under = state.surface_under(location);
        if let Some((surface, _)) = under.as_ref() {
            state.set_keyboard_focus(Some(surface.clone().into()), serial);
        }
        let touch = state.touch();
        touch.down(
            state,
            under.map(|(surface, position)| (surface, position.to_f64())),
            &DownEvent {
                slot,
                location,
                serial,
                time,
            },
        );
        touch.frame(state);
        state.queue_redraw();
        return Owner::Client;
    }

    // A region screenshot owns the glass while it is up, as it owns the
    // pointer. One event does what two do for a pointer: the finger names the
    // corner and presses it in the same instant.
    if state.region_active() {
        state.region_pointer_moved();
        state.region_press();
        state.queue_redraw();
        return Owner::Shell;
    }

    // The same reveals a pointer gets on its way to a press, so that a finger
    // brought to the bottom of the screen raises the dock and a card under it
    // knows it is being touched.
    state.notifications_pointer_moved();
    state.dock_pointer_moved();
    state.launcher_pointer_moved();
    state.pinned_pointer_moved();
    state.overview_pointer_moved();

    // A tap is the primary press, and only ever that.
    if shell_press(state, crate::mouse::BTN_LEFT, Source::Touch) == Taken::Shell {
        state.queue_redraw();
        return Owner::Shell;
    }

    // An X11 window cannot hear a finger. See [`Owner::Pointer`]: the first
    // contact on one becomes the cursor instead, and any later one is left
    // alone rather than being made into a second.
    if state.contacts.len() == 1 && touches_x11(state, location) {
        motion(state, location, time);
        let pointer = state.pointer();
        pointer.button(
            state,
            &ButtonEvent {
                button: crate::mouse::BTN_LEFT,
                state: ButtonState::Pressed,
                serial,
                time,
            },
        );
        pointer.frame(state);
        state.queue_redraw();
        return Owner::Pointer;
    }

    let Some((surface, position)) = state.surface_under(location) else {
        // The bare desktop. Counted so the finger tally stays right, and
        // forwarded nowhere.
        state.queue_redraw();
        return Owner::Nobody;
    };
    let touch = state.touch();
    touch.down(
        state,
        Some((surface, position.to_f64())),
        &DownEvent {
            slot,
            location,
            serial,
            time,
        },
    );
    touch.frame(state);
    state.queue_redraw();
    Owner::Client
}

/// Whether `location` is over a window that arrived through XWayland.
///
/// Asked of the window rather than of the surface: an X11 client's content and
/// its subsurfaces are all equally deaf to `wl_touch`, and the window is what
/// the compositor knows the provenance of.
fn touches_x11(state: &Huginn, location: Point<f64, Logical>) -> bool {
    state
        .window_under(location)
        .and_then(|window| state.surface(window))
        .is_some_and(|surface| surface.as_x11().is_some())
}

/// A finger moved.
fn touch_motion<B: InputBackend>(state: &mut Huginn, event: &B::TouchMotionEvent) {
    let slot = event.slot();
    let id = i32::from(slot);
    let device = event.device().name();
    let location = touch_location::<B>(state, &device, event);
    let time = event.time_msec();

    // A claimed hand drives the swipe and nothing else. Only the first finger
    // of it counts; see `Contacts::drives`.
    if state.contacts.claimed() {
        if let Some((dx, dy)) = state.contacts.drives(id, location) {
            state.swipe_update(dx, dy);
        }
        return;
    }

    let Some(owner) = state.contacts.owner(id) else {
        return;
    };
    state.contacts.motion(id, location);

    match owner {
        Owner::Client => {
            state.pointer_location = location;
            let under = state.surface_under(location);
            let touch = state.touch();
            touch.motion(
                state,
                under.map(|(surface, position)| (surface, position.to_f64())),
                &TouchMoveEvent {
                    slot,
                    location,
                    time,
                },
            );
            touch.frame(state);
            state.queue_redraw();
        }
        // Standing in for the pointer, so it moves the pointer -- through the
        // same path a mouse takes, hover, output crossing and all.
        Owner::Pointer => motion(state, location, time),
        // The shell acted when the finger landed, but a finger sliding along
        // the dock should still light up what it passes over, and one framing
        // a region screenshot is drawing the rectangle.
        Owner::Shell => {
            state.pointer_location = location;
            if state.region_active() {
                state.region_pointer_moved();
            } else {
                state.notifications_pointer_moved();
                state.dock_pointer_moved();
                state.launcher_pointer_moved();
                state.pinned_pointer_moved();
                state.overview_pointer_moved();
            }
            state.queue_redraw();
        }
        Owner::Nobody | Owner::Gesture => {}
    }
}

/// A finger lifted.
fn touch_up<B: InputBackend>(state: &mut Huginn, event: &B::TouchUpEvent) {
    let slot = event.slot();
    let id = i32::from(slot);
    let time = event.time_msec();
    let on_shell = state.contacts.owner(id) == Some(Owner::Shell);

    match state.contacts.up(id) {
        Lift::Client => {
            let serial = SERIAL_COUNTER.next_serial();
            let touch = state.touch();
            touch.up(state, &UpEvent { slot, serial, time });
            touch.frame(state);
            state.queue_redraw();
        }
        Lift::Pointer => {
            let serial = SERIAL_COUNTER.next_serial();
            let pointer = state.pointer();
            pointer.button(
                state,
                &ButtonEvent {
                    button: crate::mouse::BTN_LEFT,
                    state: ButtonState::Released,
                    serial,
                    time,
                },
            );
            pointer.frame(state);
            state.queue_redraw();
        }
        Lift::EndsGesture => {
            state.swipe_end();
            state.queue_redraw();
        }
        Lift::Trailing => {}
        Lift::Quiet => {
            // A region screenshot is taken by letting go, as it is with a
            // mouse. The shell's other presses did their work on the way down.
            if on_shell && state.region_active() {
                state.region_release();
                state.queue_redraw();
            }
        }
    }
}

/// The touch sequence was taken away from us — a device unplugged mid-gesture,
/// a session switched away, libinput giving up on a contact it lost.
///
/// Everything is dropped and anything in flight is ended, because the state
/// this leaves behind is state no further event will ever arrive to close: a
/// carousel stopped between two workspaces, a button held down on an X11
/// window, a client waiting for the up of a touch that will not come.
fn touch_cancel<B: InputBackend>(state: &mut Huginn, event: &B::TouchCancelEvent) {
    let time = event.time_msec();
    if state.contacts.any_emulating() {
        release_primary_button(state, time);
    }
    let touch = state.touch();
    touch.cancel(state);
    if state.contacts.clear() {
        state.swipe_end();
    }
    state.queue_redraw();
}

/// Let go of the primary button a contact was holding down for an X11 window.
/// See [`Owner::Pointer`].
fn release_primary_button(state: &mut Huginn, time: u32) {
    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.pointer();
    pointer.button(
        state,
        &ButtonEvent {
            button: crate::mouse::BTN_LEFT,
            state: ButtonState::Released,
            serial,
            time,
        },
    );
    pointer.frame(state);
}
