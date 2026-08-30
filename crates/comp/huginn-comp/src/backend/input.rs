//! Pointer and touchpad input, shared by both backends.
//!
//! winit reports absolute positions inside its window; libinput reports
//! relative deltas from a physical mouse. Both end up here so that focus
//! behaviour, clamping and hit testing cannot drift between the two.
//!
//! Touchpad gestures only ever arrive from libinput — winit's backend types
//! them as the uninhabited `UnusedEvent`, so the arms below compile there and
//! can never run. What the gesture *means* still lives in [`crate::gesture`]
//! rather than here, which is what keeps it testable without a touchpad.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent,
        GestureEndEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionEvent,
    },
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent},
    utils::{Logical, Point, SERIAL_COUNTER, Size},
};

use crate::state::Huginn;

/// Feed a pointer event to the compositor. Non-pointer events are ignored.
pub(crate) fn handle<B: InputBackend>(state: &mut Huginn, event: InputEvent<B>) {
    match event {
        InputEvent::PointerMotion { event } => {
            let location = state.clamp_pointer(state.pointer_location + event.delta());
            motion(state, location, event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            // Absolute devices report a fraction of the surface they are bound
            // to, so the position has to be scaled by the output and then
            // offset by where that output sits in global space.
            let area = state.output_area();
            let extent: Size<i32, Logical> = (area.w(), area.h()).into();
            let origin: Point<f64, Logical> = (f64::from(area.x()), f64::from(area.y())).into();
            let location = state.clamp_pointer(event.position_transformed(extent) + origin);
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
        _ => {}
    }
}

fn motion(state: &mut Huginn, location: Point<f64, Logical>, time: u32) {
    state.pointer_location = location;
    state.pointer_crossed_outputs();
    // The dock watches the bottom edge. Told before the event is forwarded, so
    // a reveal and the client's own motion land in the same frame -- unless the
    // session is locked, in which case the dock is not on screen and a pointer
    // at the bottom edge must not reveal it. `surface_under` is already empty
    // of everything but the lock, so the motion itself is harmless; this is
    // about the compositor's own drawing, which does not go through the scene.
    if !state.is_locked() {
        state.dock_pointer_moved();
        state.launcher_pointer_moved();
        state.pinned_pointer_moved();
    }
    // The launcher is compositor-drawn, so no client is under the pointer
    // while it is there as far as the scene knows — but a window behind the
    // panel is, and it must not be told about a pointer the user sees as
    // being on the panel. Leaving the client is what stops its hover
    // effects tracking a pointer that is not on it.
    let under = if state.launcher_covers_pointer() || state.pinned_covers_pointer() {
        None
    } else {
        state.surface_under(location)
    };
    let pointer = state.pointer();
    pointer.motion(
        state,
        under.map(|(surface, position)| (surface, position.to_f64())),
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
    // The cursor moved, so the frame on screen is stale even if no client
    // changed anything.
    state.queue_redraw();
}

fn button<B: InputBackend>(state: &mut Huginn, event: &B::PointerButtonEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let button_state = event.state();

    // Tap-to-click touchpads encode three fingers as a middle-button press.
    const BTN_MIDDLE: u32 = 0x112;
    if button_state == ButtonState::Pressed && event.button_code() == BTN_MIDDLE {
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
            state.set_keyboard_focus(Some(surface.clone()), serial);
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
    const BTN_LEFT: u32 = 0x110;
    if button_state == ButtonState::Pressed
        && event.button_code() == BTN_LEFT
        && (state.launcher_click() || state.pinned_click())
    {
        return;
    }

    // The dock is compositor-drawn, so it is not under the pointer as far as
    // any client is concerned. It has to be asked first, or a click on it
    // falls through to whatever window is behind it.
    if button_state == ButtonState::Pressed
        && let Some(item) = state.dock_click()
    {
        state.activate_dock_item(&item);
        return;
    }

    // A click on a layer surface that asked for the keyboard is how it takes
    // focus, and a click anywhere else is how it gives it back. Settled before
    // click-to-focus, because a panel overlapping a tile must not also raise
    // the window behind it — the click belongs to whatever is drawn on top.
    let clicked_layer =
        if button_state == ButtonState::Pressed && !on_popup && !state.pointer().is_grabbed() {
            let hit = state.layer_under(state.pointer_location);
            let landed = hit.is_some();
            if state.set_focused_layer(hit) {
                state.refresh_focus();
            }
            landed
        } else {
            false
        };

    if button_state == ButtonState::Pressed
        && !on_popup
        && !clicked_layer
        && !state.pointer().is_grabbed()
        && let Some(window) = state.window_under(state.pointer_location)
    {
        state.space.active_workspace_mut().focus(window);
        state.refresh_focus();
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

fn axis<B: InputBackend>(state: &mut Huginn, event: &B::PointerAxisEvent) {
    let source = event.source();
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
