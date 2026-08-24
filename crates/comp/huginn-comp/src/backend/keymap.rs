//! Keybindings, shared by both backends.
//!
//! The winit and udev backends receive input from completely different sources
//! — winit's own event queue and libinput — but the bindings must be identical,
//! or the desktop behaves differently depending on how you started it.

use huginn_core::geometry::Dir;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::{FilterResult, ModifiersState, keysyms};

/// A keybinding resolved to an intent.
///
/// The keyboard filter runs while the keyboard handle is borrowed, so it cannot
/// touch compositor state that the follow-up work needs. Returning an intent
/// and applying it afterwards keeps the borrow checker out of the keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    FocusNext,
    FocusPrev,
    PromoteFocused,
    CloseFocused,
    /// Move the focused window one tile in a direction.
    Move(Dir),
    Workspace(usize),
    SendToWorkspace(usize),
    Spawn,
    Quit,
}

/// One line of help, kept next to the bindings so the two cannot drift.
pub(crate) const HELP: &str = "keys: Super+J/K focus · Super+arrows move window · \
     Super+Return promote · Super+Q/X close · Super+1-9 workspace · Super+Shift+1-9 to workspace · \
     Super+E/T spawn · Super+Esc quit";

/// Map a key event to an action, or forward it to the focused client.
///
/// Runs for releases as well as presses, so both halves of a keystroke need an
/// answer. A bound key is intercepted either way — forwarding a release whose
/// press was swallowed leaves the client believing a key it never saw pressed
/// is still down — but only the press carries an action, or every binding fires
/// twice and one Super+T opens two terminals.
pub(crate) fn resolve(
    key_state: KeyState,
    modifiers: &ModifiersState,
    sym: u32,
) -> FilterResult<Option<Action>> {
    if !modifiers.logo {
        return FilterResult::Forward;
    }
    let action = match sym {
        keysyms::KEY_j | keysyms::KEY_J => Action::FocusNext,
        keysyms::KEY_k | keysyms::KEY_K => Action::FocusPrev,
        keysyms::KEY_Return => Action::PromoteFocused,
        // Two bindings each for close and spawn: Q/E are the originals, X/T are
        // what people arrive expecting from other tiling compositors.
        keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_x | keysyms::KEY_X => Action::CloseFocused,
        keysyms::KEY_e | keysyms::KEY_E | keysyms::KEY_t | keysyms::KEY_T => Action::Spawn,
        keysyms::KEY_Left => Action::Move(Dir::Left),
        keysyms::KEY_Right => Action::Move(Dir::Right),
        keysyms::KEY_Up => Action::Move(Dir::Up),
        keysyms::KEY_Down => Action::Move(Dir::Down),
        keysyms::KEY_Escape => Action::Quit,
        // Shift+digit produces the punctuation keysyms, which is how "move to
        // workspace" is told apart from "switch to workspace" without reading
        // the shift modifier separately.
        keysyms::KEY_1..=keysyms::KEY_9 => Action::Workspace((sym - keysyms::KEY_1) as usize),
        keysyms::KEY_exclam => Action::SendToWorkspace(0),
        keysyms::KEY_at => Action::SendToWorkspace(1),
        keysyms::KEY_numbersign => Action::SendToWorkspace(2),
        keysyms::KEY_dollar => Action::SendToWorkspace(3),
        keysyms::KEY_percent => Action::SendToWorkspace(4),
        _ => return FilterResult::Forward,
    };
    FilterResult::Intercept((key_state == KeyState::Pressed).then_some(action))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The action a press produces, if any.
    fn intercepted(mods: ModifiersState, sym: u32) -> Option<Action> {
        match resolve(KeyState::Pressed, &mods, sym) {
            FilterResult::Intercept(action) => action,
            FilterResult::Forward => None,
        }
    }

    /// Whether the key reaches the focused client instead.
    fn forwarded(key_state: KeyState, mods: ModifiersState, sym: u32) -> bool {
        matches!(resolve(key_state, &mods, sym), FilterResult::Forward)
    }

    fn super_held() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    #[test]
    fn bindings_need_super() {
        // Without Super every key belongs to the focused client. A compositor
        // that swallows a bare 'q' is unusable.
        assert_eq!(intercepted(ModifiersState::default(), keysyms::KEY_q), None);
        assert_eq!(intercepted(super_held(), keysyms::KEY_q), Some(Action::CloseFocused));
    }

    #[test]
    fn digits_map_to_zero_based_workspaces() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_1), Some(Action::Workspace(0)));
        assert_eq!(intercepted(super_held(), keysyms::KEY_9), Some(Action::Workspace(8)));
    }

    #[test]
    fn shifted_digits_move_windows_instead_of_switching() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_exclam), Some(Action::SendToWorkspace(0)));
        assert_eq!(intercepted(super_held(), keysyms::KEY_at), Some(Action::SendToWorkspace(1)));
    }

    #[test]
    fn close_and_spawn_have_two_bindings_each() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_x), Some(Action::CloseFocused));
        assert_eq!(intercepted(super_held(), keysyms::KEY_t), Some(Action::Spawn));
    }

    #[test]
    fn arrows_move_the_focused_window() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_Left), Some(Action::Move(Dir::Left)));
        assert_eq!(intercepted(super_held(), keysyms::KEY_Down), Some(Action::Move(Dir::Down)));
        // Bare arrows are how you move a cursor. They must never be swallowed.
        assert_eq!(intercepted(ModifiersState::default(), keysyms::KEY_Left), None);
    }

    #[test]
    fn only_the_press_fires_a_binding() {
        // The filter sees both halves of a keystroke. Acting on each of them
        // runs every binding twice, which is two terminals per Super+T.
        assert_eq!(intercepted(super_held(), keysyms::KEY_t), Some(Action::Spawn));
        assert!(matches!(
            resolve(KeyState::Released, &super_held(), keysyms::KEY_t),
            FilterResult::Intercept(None)
        ));
    }

    #[test]
    fn a_bound_release_is_swallowed_with_its_press() {
        assert!(!forwarded(KeyState::Released, super_held(), keysyms::KEY_t));
        assert!(forwarded(KeyState::Released, super_held(), keysyms::KEY_z));
    }

    #[test]
    fn unbound_keys_reach_the_client() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_z), None);
    }
}
