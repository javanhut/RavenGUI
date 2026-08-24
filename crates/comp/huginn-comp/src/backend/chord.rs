//! Synthesising a keystroke for the focused client.
//!
//! Copy and paste are not compositor operations. The clipboard belongs to the
//! clients: an application decides what "copy" means for whatever it has
//! selected, and the compositor never learns what that selection is. There is
//! no protocol for asking a client to copy, either — so the only way a
//! compositor binding can reach one is as the keystroke the client already
//! listens for, which for every toolkit is `Ctrl`+`C`.

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{KeyboardHandle, Keycode, Keysym};
use smithay::utils::SERIAL_COUNTER;

use crate::state::Huginn;

/// Send `Ctrl`+`sym` to the focused client as if it had been typed.
///
/// `time` is the timestamp of the keystroke that asked for this. Clients use it
/// to order events and to measure repeat, so inventing one — or reusing a stale
/// one — makes a client's own idea of the keyboard drift from the compositor's.
pub(crate) fn send_ctrl(
    keyboard: &KeyboardHandle<Huginn>,
    state: &mut Huginn,
    sym: Keysym,
    time: u32,
) {
    let (Some(ctrl), Some(key)) = (
        keycode_for(keyboard, state, Keysym::Control_L),
        keycode_for(keyboard, state, sym),
    ) else {
        tracing::warn!(?sym, "no key on this layout produces the chord; nothing sent");
        return;
    };

    // Super is physically down — it is what got us here — and the client would
    // see it alongside the Ctrl being invented. An accelerator that asks for
    // Ctrl+C does not fire on Super+Ctrl+C, so the modifiers have to be
    // corrected between updating the keyboard state and telling the client.
    // That is why this uses the two halves of `input` rather than `input`
    // itself, which does both at once with no seam to reach into.
    let held = keyboard.modifier_state();
    for (code, key_state) in [
        (ctrl, KeyState::Pressed),
        (key, KeyState::Pressed),
        (key, KeyState::Released),
        (ctrl, KeyState::Released),
    ] {
        keyboard.input_intercept::<(), _>(state, code, key_state, |_, _, _| ());
        let mut mods = keyboard.modifier_state();
        mods.logo = false;
        keyboard.set_modifier_state(mods);
        keyboard.input_forward(state, code, key_state, SERIAL_COUNTER.next_serial(), time, true);
    }

    // The Super key is still held, and the compositor's own bindings read this
    // state on the next keystroke, so it has to go back to the truth. The
    // client keeps the corrected view until the next event it is forwarded,
    // which is the release of Super and reports no modifiers either.
    keyboard.set_modifier_state(held);
}

/// The keycode that produces `sym` on the layout in effect.
///
/// Looked up rather than hardcoded: keycode 54 is `c` on a US layout and `j` on
/// Dvorak, so a compositor that hardcodes it copies with the wrong key for
/// anyone who does not type QWERTY.
fn keycode_for(
    keyboard: &KeyboardHandle<Huginn>,
    state: &mut Huginn,
    sym: Keysym,
) -> Option<Keycode> {
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().ok()?;
        let layout = xkb.active_layout();
        // The evdev keycode range. The keymap knows its own bounds, but reading
        // them goes through an unsafe accessor, which this workspace forbids.
        (8u32..=255)
            .map(Keycode::new)
            .find(|code| xkb.raw_syms_for_key_in_layout(*code, layout).contains(&sym))
    })
}
