//! Keybindings, shared by both backends.
//!
//! The winit and udev backends receive input from completely different sources
//! — winit's own event queue and libinput — but the bindings must be identical,
//! or the desktop behaves differently depending on how you started it.

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
    Workspace(usize),
    SendToWorkspace(usize),
    Spawn,
    Quit,
}

/// One line of help, kept next to the bindings so the two cannot drift.
pub(crate) const HELP: &str = "keys: Super+J/K focus · Super+Return promote · Super+Q close · \
     Super+1-9 workspace · Super+Shift+1-9 move · Super+E spawn · Super+Esc quit";

/// Map a key press to an action, or forward it to the focused client.
pub(crate) fn resolve(modifiers: &ModifiersState, sym: u32) -> FilterResult<Action> {
    if !modifiers.logo {
        return FilterResult::Forward;
    }
    let action = match sym {
        keysyms::KEY_j | keysyms::KEY_J => Action::FocusNext,
        keysyms::KEY_k | keysyms::KEY_K => Action::FocusPrev,
        keysyms::KEY_Return => Action::PromoteFocused,
        keysyms::KEY_q | keysyms::KEY_Q => Action::CloseFocused,
        keysyms::KEY_e | keysyms::KEY_E => Action::Spawn,
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
    FilterResult::Intercept(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intercepted(mods: ModifiersState, sym: u32) -> Option<Action> {
        match resolve(&mods, sym) {
            FilterResult::Intercept(a) => Some(a),
            FilterResult::Forward => None,
        }
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
    fn unbound_keys_reach_the_client() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_z), None);
    }
}
