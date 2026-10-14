//! Drag and drop between clients: the icon under the pointer, and the action
//! the held modifiers choose.
//!
//! smithay runs the protocol -- offers, enter and leave, the drop itself --
//! but leaves two things to the compositor, and without them a drag looks
//! like nothing is happening at all.
//!
//! **The icon.** `wl_data_device.start_drag` hands over a surface for the
//! pointer to carry. Nobody draws it unless the compositor does, and a client
//! waits on frame callbacks for it like any other surface, so an icon that is
//! never drawn and never called back sits frozen on its first frame. Huginn
//! draws it under the pointer (see `render::push_dnd_icon`), counts it among
//! the surfaces that are told they may paint, and follows the offset a client
//! attaches with each buffer, which is how the grab point stays put.
//!
//! **The action.** Copy or move is negotiated between source and target, and
//! the target decides from the modifiers it sees. A window the drag merely
//! passes over is not focused, is sent no `wl_keyboard.modifiers`, and so
//! cannot tell Ctrl from Shift. The compositor can: Ctrl copies and Shift
//! moves, the convention of every file manager, whichever window is under the
//! pointer. Holding neither leaves the choice to the client.

use smithay::reexports::wayland_server::protocol::wl_data_device_manager::DndAction;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point};

/// The surface a client asked to have carried under the pointer while it
/// drags.
#[derive(Debug, Clone)]
pub(crate) struct DndIcon {
    pub(crate) surface: WlSurface,
    /// Where the icon's top-left sits relative to the pointer: the sum of
    /// every buffer offset the client has attached to it. Starts at the
    /// pointer itself, as the protocol says.
    pub(crate) offset: Point<i32, Logical>,
}

impl DndIcon {
    pub(crate) fn new(surface: WlSurface) -> Self {
        Self {
            surface,
            offset: (0, 0).into(),
        }
    }

    /// Whether the client still has the surface. A client that destroys its
    /// icon mid-drag keeps dragging, with nothing to carry.
    pub(crate) fn alive(&self) -> bool {
        self.surface.alive()
    }

    /// Where the icon's top-left is drawn for a pointer at `pointer`.
    pub(crate) fn origin(&self, pointer: Point<f64, Logical>) -> Point<i32, Logical> {
        pointer.to_i32_round() + self.offset
    }
}

/// The action the held modifiers ask for, when the drag offers it: Ctrl
/// copies, Shift moves, and Ctrl wins when both are held. `None` leaves the
/// choice to what the target preferred.
pub(crate) fn modifier_action(ctrl: bool, shift: bool, available: DndAction) -> Option<DndAction> {
    let wanted = if ctrl {
        DndAction::Copy
    } else if shift {
        DndAction::Move
    } else {
        return None;
    };
    available.contains(wanted).then_some(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: DndAction = DndAction::Copy.union(DndAction::Move);

    #[test]
    fn ctrl_copies_and_shift_moves() {
        assert_eq!(modifier_action(true, false, BOTH), Some(DndAction::Copy));
        assert_eq!(modifier_action(false, true, BOTH), Some(DndAction::Move));
        assert_eq!(modifier_action(true, true, BOTH), Some(DndAction::Copy));
    }

    #[test]
    fn no_modifier_leaves_the_choice_to_the_client() {
        assert_eq!(modifier_action(false, false, BOTH), None);
    }

    #[test]
    fn an_action_the_source_does_not_offer_is_never_forced() {
        assert_eq!(modifier_action(true, false, DndAction::Move), None);
        assert_eq!(modifier_action(false, true, DndAction::Copy), None);
    }
}
