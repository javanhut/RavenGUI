//! Pointer input, shared by both backends.
//!
//! winit reports absolute positions inside its window; libinput reports
//! relative deltas from a physical mouse. Both end up here so that focus
//! behaviour, clamping and hit testing cannot drift between the two.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
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
        _ => {}
    }
}

fn motion(state: &mut Huginn, location: Point<f64, Logical>, time: u32) {
    state.pointer_location = location;
    // The dock watches the bottom edge. Told before the event is forwarded, so
    // a reveal and the client's own motion land in the same frame.
    state.dock_pointer_moved();
    let under = state.surface_under(location);
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
    // The dock is compositor-drawn, so it is not under the pointer as far as
    // any client is concerned. It has to be asked first, or a click on it
    // falls through to whatever window is behind it.
    if button_state == ButtonState::Pressed
        && let Some(item) = state.dock_click()
    {
        state.activate_dock_item(&item);
        return;
    }

    if button_state == ButtonState::Pressed
        && !on_popup
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
