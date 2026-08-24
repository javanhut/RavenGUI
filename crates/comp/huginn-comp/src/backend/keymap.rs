//! Keybindings, shared by both backends.
//!
//! The winit and udev backends receive input from completely different sources
//! — winit's own event queue and libinput — but the bindings must be identical,
//! or the desktop behaves differently depending on how you started it.
//!
//! # Which layer belongs to whom
//!
//! Window management lives on `Super`+`Shift`. The plain `Super` layer belongs
//! to applications: RavenTerminal uses it as its own leader — `Super`+`T` for a
//! tab, `Super`+`[`/`]` for panes, `Super`+`C` to copy — the way `Cmd` works on
//! macOS. A compositor that takes the whole `Super` layer leaves those chords
//! unreachable, since a key it intercepts never arrives at the client at all.
//!
//! The one exception is copy and paste, which the compositor translates rather
//! than performs. See [`Action::Copy`].

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
    /// Copy in the focused client, by sending it the chord it understands.
    ///
    /// The compositor cannot copy: the clipboard is a client affair, and what
    /// is selected inside a window is something only that window knows. All
    /// this can do is arrive at the client as `Ctrl`+`C`, which is what every
    /// toolkit binds copy to — and what a terminal reads as SIGINT, which is
    /// why a client that handles the `Super` layer itself never sees it.
    Copy,
    /// Paste in the focused client. As [`Action::Copy`], with `Ctrl`+`V`.
    Paste,
}

/// One line of help, kept next to the bindings so the two cannot drift.
pub(crate) const HELP: &str = "keys: Super+Shift+J/K focus · Super+Shift+arrows move window · \
     Super+Shift+Return promote · Super+Shift+Q/X close · Super+Shift+E/T terminal · \
     Super+Shift+1-9 workspace · Super+Ctrl+Shift+1-9 send there · Super+Shift+Esc quit · \
     Super+C/V copy and paste. Plain Super belongs to the focused application.";

/// Map a key event to an action, or forward it to the focused client.
///
/// Runs for releases as well as presses, so both halves of a keystroke need an
/// answer. A bound key is intercepted either way — forwarding a release whose
/// press was swallowed leaves the client believing a key it never saw pressed
/// is still down — but only the press carries an action, or every binding fires
/// twice and one keystroke opens two terminals.
///
/// `focus_owns_super` reports whether the focused client drives the `Super`
/// layer itself, which is the whole of what decides who handles copy and paste.
pub(crate) fn resolve(
    key_state: KeyState,
    modifiers: &ModifiersState,
    sym: u32,
    focus_owns_super: bool,
) -> FilterResult<Option<Action>> {
    if !modifiers.logo {
        return FilterResult::Forward;
    }

    // Plain Super is the application's. Copy and paste are borrowed back from
    // it, and only from clients that have no use of their own for the chord.
    if !modifiers.shift {
        if focus_owns_super {
            return FilterResult::Forward;
        }
        let action = match sym {
            keysyms::KEY_c | keysyms::KEY_C => Action::Copy,
            keysyms::KEY_v | keysyms::KEY_V => Action::Paste,
            _ => return FilterResult::Forward,
        };
        return FilterResult::Intercept(pressed(key_state, action));
    }

    let action = match sym {
        keysyms::KEY_j | keysyms::KEY_J => Action::FocusNext,
        keysyms::KEY_k | keysyms::KEY_K => Action::FocusPrev,
        keysyms::KEY_Return => Action::PromoteFocused,
        keysyms::KEY_q | keysyms::KEY_Q | keysyms::KEY_x | keysyms::KEY_X => Action::CloseFocused,
        keysyms::KEY_e | keysyms::KEY_E | keysyms::KEY_t | keysyms::KEY_T => Action::Spawn,
        keysyms::KEY_Left => Action::Move(Dir::Left),
        keysyms::KEY_Right => Action::Move(Dir::Right),
        keysyms::KEY_Up => Action::Move(Dir::Up),
        keysyms::KEY_Down => Action::Move(Dir::Down),
        keysyms::KEY_Escape => Action::Quit,
        // Adding Ctrl sends the window there instead of going there yourself.
        sym => match workspace_index(sym) {
            Some(i) if modifiers.ctrl => Action::SendToWorkspace(i),
            Some(i) => Action::Workspace(i),
            None => return FilterResult::Forward,
        },
    };
    FilterResult::Intercept(pressed(key_state, action))
}

/// The action, but only on the way down.
fn pressed(key_state: KeyState, action: Action) -> Option<Action> {
    (key_state == KeyState::Pressed).then_some(action)
}

/// The zero-based workspace a digit key names, however the layout shifts it.
///
/// Shift is held for every compositor binding, so on a US layout the digit row
/// arrives as punctuation. Both are accepted: a layout that keeps digits under
/// Shift is no less entitled to nine workspaces.
fn workspace_index(sym: u32) -> Option<usize> {
    const SHIFTED: [u32; 9] = [
        keysyms::KEY_exclam,
        keysyms::KEY_at,
        keysyms::KEY_numbersign,
        keysyms::KEY_dollar,
        keysyms::KEY_percent,
        keysyms::KEY_asciicircum,
        keysyms::KEY_ampersand,
        keysyms::KEY_asterisk,
        keysyms::KEY_parenleft,
    ];
    if (keysyms::KEY_1..=keysyms::KEY_9).contains(&sym) {
        return Some((sym - keysyms::KEY_1) as usize);
    }
    SHIFTED.iter().position(|shifted| *shifted == sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The action a press produces, if any.
    fn intercepted(mods: ModifiersState, sym: u32) -> Option<Action> {
        match resolve(KeyState::Pressed, &mods, sym, false) {
            FilterResult::Intercept(action) => action,
            FilterResult::Forward => None,
        }
    }

    /// Whether the key reaches the focused client instead.
    fn forwarded(key_state: KeyState, mods: ModifiersState, sym: u32) -> bool {
        matches!(
            resolve(key_state, &mods, sym, false),
            FilterResult::Forward
        )
    }

    fn super_held() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    fn super_shift() -> ModifiersState {
        ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        }
    }

    fn super_ctrl_shift() -> ModifiersState {
        ModifiersState {
            logo: true,
            shift: true,
            ctrl: true,
            ..Default::default()
        }
    }

    #[test]
    fn bindings_need_super_and_shift() {
        // Without Super every key belongs to the focused client. A compositor
        // that swallows a bare 'q' is unusable.
        assert_eq!(intercepted(ModifiersState::default(), keysyms::KEY_q), None);
        assert_eq!(intercepted(super_shift(), keysyms::KEY_Q), Some(Action::CloseFocused));
    }

    #[test]
    fn the_plain_super_layer_reaches_the_application() {
        // RavenTerminal's own chords: a new tab, a pane, its help. Binding
        // these in the compositor would make them unreachable everywhere.
        for sym in [keysyms::KEY_t, keysyms::KEY_k, keysyms::KEY_bracketright, keysyms::KEY_1] {
            assert!(forwarded(KeyState::Pressed, super_held(), sym), "Super+{sym:#x} was eaten");
        }
    }

    #[test]
    fn digits_map_to_zero_based_workspaces() {
        // Shift is held, so a US layout delivers the punctuation on the key.
        assert_eq!(intercepted(super_shift(), keysyms::KEY_exclam), Some(Action::Workspace(0)));
        assert_eq!(intercepted(super_shift(), keysyms::KEY_parenleft), Some(Action::Workspace(8)));
        // A layout that keeps the digit itself under Shift works too.
        assert_eq!(intercepted(super_shift(), keysyms::KEY_1), Some(Action::Workspace(0)));
        assert_eq!(intercepted(super_shift(), keysyms::KEY_9), Some(Action::Workspace(8)));
    }

    #[test]
    fn ctrl_sends_the_window_instead_of_following_it() {
        assert_eq!(
            intercepted(super_ctrl_shift(), keysyms::KEY_exclam),
            Some(Action::SendToWorkspace(0))
        );
        assert_eq!(
            intercepted(super_ctrl_shift(), keysyms::KEY_at),
            Some(Action::SendToWorkspace(1))
        );
    }

    #[test]
    fn arrows_move_the_focused_window() {
        assert_eq!(intercepted(super_shift(), keysyms::KEY_Left), Some(Action::Move(Dir::Left)));
        assert_eq!(intercepted(super_shift(), keysyms::KEY_Down), Some(Action::Move(Dir::Down)));
        // Bare arrows are how you move a cursor. They must never be swallowed.
        assert!(forwarded(KeyState::Pressed, ModifiersState::default(), keysyms::KEY_Left));
    }

    #[test]
    fn close_and_spawn_have_two_bindings_each() {
        assert_eq!(intercepted(super_shift(), keysyms::KEY_X), Some(Action::CloseFocused));
        assert_eq!(intercepted(super_shift(), keysyms::KEY_T), Some(Action::Spawn));
    }

    #[test]
    fn copy_and_paste_are_translated_for_a_client_that_needs_it() {
        assert_eq!(intercepted(super_held(), keysyms::KEY_c), Some(Action::Copy));
        assert_eq!(intercepted(super_held(), keysyms::KEY_v), Some(Action::Paste));
    }

    #[test]
    fn a_client_with_its_own_super_layer_keeps_copy_and_paste() {
        // Translating for a terminal would replace a working copy with Ctrl+C,
        // which it reads as SIGINT and sends to whatever is running.
        for sym in [keysyms::KEY_c, keysyms::KEY_v] {
            assert!(matches!(
                resolve(KeyState::Pressed, &super_held(), sym, true),
                FilterResult::Forward
            ));
        }
    }

    #[test]
    fn only_the_press_fires_a_binding() {
        // The filter sees both halves of a keystroke. Acting on each of them
        // runs every binding twice, which is two terminals per keystroke.
        assert_eq!(intercepted(super_shift(), keysyms::KEY_T), Some(Action::Spawn));
        assert!(matches!(
            resolve(KeyState::Released, &super_shift(), keysyms::KEY_T, false),
            FilterResult::Intercept(None)
        ));
    }

    #[test]
    fn a_bound_release_is_swallowed_with_its_press() {
        assert!(!forwarded(KeyState::Released, super_shift(), keysyms::KEY_T));
        assert!(forwarded(KeyState::Released, super_shift(), keysyms::KEY_z));
    }

    #[test]
    fn unbound_keys_reach_the_client() {
        assert_eq!(intercepted(super_shift(), keysyms::KEY_z), None);
    }
}
