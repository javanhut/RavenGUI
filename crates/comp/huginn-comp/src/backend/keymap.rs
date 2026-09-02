//! Keybindings, shared by both backends.
//!
//! The winit and udev backends receive input from completely different sources
//! — winit's own event queue and libinput — but the bindings must be identical,
//! or the desktop behaves differently depending on how you started it.
//!
//! # Which layer belongs to whom
//!
//! Window management lives on `Super`+`Ctrl`. The plain `Super` layer belongs
//! to applications: RavenTerminal uses it as its own leader — `Super`+`T` for a
//! tab, `Super`+`[`/`]` for panes, `Super`+`C` to copy — the way `Cmd` works on
//! macOS. A compositor that takes the whole `Super` layer leaves those chords
//! unreachable, since a key it intercepts never arrives at the client at all.
//!
//! There are two exceptions. Copy and paste, which the compositor translates
//! rather than performs — see [`Action::Copy`] — and which it hands back to any
//! client that has its own use for the chord. And [`Action::Lock`], which it
//! does *not* hand back to anybody, for the reason given there.

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
    /// Open or accept the workspace carousel.
    ToggleCarousel,
    CloseFocused,
    /// Move the focused window one tile in a direction.
    Move(Dir),
    Workspace(usize),
    SendToWorkspace(usize),
    /// Move focus to the next screen, onto whatever workspace it shows.
    FocusNextOutput,
    /// Send the focused window to the next screen, keeping focus here.
    SendToNextOutput,
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
    /// Lock the session.
    ///
    /// On plain `Super` rather than `Super`+`Ctrl`, breaking this module's own
    /// rule, because `Super`+`L` is what a person coming from any other desktop
    /// will press and a lock chord that is nearly right is a lock chord that
    /// leaves machines unlocked.
    ///
    /// Unlike [`Action::Copy`] it is not given back to a client that owns the
    /// `Super` layer. A chord that locks the machine everywhere except when a
    /// terminal happens to be focused is worse than not having it: it fails
    /// exactly when somebody is walking away from a machine they believe they
    /// just locked. So `Super`+`L` is reserved, and RavenTerminal may not use
    /// it.
    Lock,
    /// Show or hide the keybinding overlay.
    ToggleHelp,
    /// Open the application launcher.
    OpenLauncher,
    /// Open the settings application — the full one, not the panel.
    OpenFullSettings,
    /// Open the software store.
    OpenStore,
    /// Enter resize mode: arrows then resize the focused window.
    EnterResize,
    /// Resize the focused window while in resize mode.
    Resize(Dir),
    /// Leave resize mode.
    LeaveResize,
    /// Move the overview's highlight one window over.
    OverviewMove(Dir),
    /// Take the overview's highlighted window: it gets the screen to itself
    /// and the rest of its workspace is put away. With nothing highlighted,
    /// the overview closes and the tiling is put back instead.
    OverviewConfirm,
    /// Leave the overview without taking anything, putting the tiling back.
    OverviewCancel,
    /// Open the quick settings panel.
    OpenSettings,
    /// A key while quick settings is open.
    Settings(crate::settings::Key),
    /// A key while the launcher is open. Every key goes to it.
    ///
    /// The launcher is drawn by the compositor rather than by a client, so
    /// there is no surface to focus and nothing to forward to. While it is
    /// open the keymap stops resolving chords entirely.
    Launcher(crate::launcher::Key),
    /// Open the pinned panel.
    OpenPinned,
    /// A key while the pinned panel is open. Every key goes to it, for the
    /// reason every key goes to the launcher.
    Pinned(crate::pinned::Key),
    /// Dismiss the temporary minimized-application switcher.
    DismissSwitcher,
    /// A media key: raise, lower or mute the output volume.
    ///
    /// Resolved before every mode but the lock — and, unlike everything else
    /// in this table, while locked too. The keys act on the speakers, not on
    /// the session: the one thing somebody wants from a locked laptop that
    /// has started playing something is for it to stop, and a mute key that
    /// works only after the password is a mute key that arrives late.
    Volume(crate::audio::Key),
    /// Take a screenshot: the whole screen, the focused window, or — for
    /// [`Shot::Region`](crate::screenshot::Shot::Region) — arm the interactive
    /// selection the pointer finishes. On `Print`, before the `Super` layer, so
    /// it needs no modifier; Shift and Ctrl pick region and window.
    Screenshot(crate::screenshot::Shot),
    /// Abandon an in-progress region selection. Escape, and only reachable while
    /// the selection is up, which is why it has no row in [`BINDINGS`].
    CancelRegion,
}

/// What the compositor is already doing, which decides what a key means.
///
/// Gathered into one value rather than passed as five: the list grew a flag
/// per mode until the call sites were a row of bare booleans that only their
/// position told apart, and `resolve(.., false, None, false, false, true)` is
/// a call nobody can read or check. Named fields also mean adding the next
/// mode cannot silently transpose two arguments at a call site that still
/// compiles.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Modes {
    /// The focused client drives the `Super` layer itself, so most of it is
    /// theirs. See [`Action::Copy`], and [`Action::Lock`] for the exception.
    pub focus_owns_super: bool,
    /// The launcher is open, and the character this key produces. It takes
    /// every key.
    pub launcher: Option<Option<char>>,
    /// Quick settings is open, and takes every key.
    pub settings_open: bool,
    /// The pinned panel is open, and takes every key.
    pub pinned_open: bool,
    /// Resize mode is active, and owns the arrows.
    pub resizing: bool,
    /// The workspace overview is up, and owns the arrows, Return and Escape.
    pub overview: bool,
    /// The session is locked. Nothing resolves; every key is the lock
    /// screen's.
    pub locked: bool,
    /// The centered minimized-application dock owns input until dismissed.
    pub switcher_open: bool,
    /// A region screenshot is being dragged out. Every key but Escape is
    /// swallowed so a keystroke cannot act on a window under the selection.
    pub selecting_region: bool,
}

/// One row of the keybinding overlay, and one clause of the startup log line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// The action this row stands for. Present so a test can prove that every
    /// action [`resolve`] can return has a row here — the whole point of the
    /// table being the only place bindings are written down. Nothing outside
    /// that test reads it, and it earns its keep anyway: it is the only thing
    /// making the link between a binding and its description checkable.
    #[cfg_attr(not(test), allow(dead_code))]
    pub action: Action,
    /// The chord as a user reads it, not as xkb spells it.
    pub chord: &'static str,
    /// What it does, in the imperative and lowercase, so the column reads as a
    /// list rather than as a series of headings.
    pub description: &'static str,
}

/// Every binding the compositor answers, in the order they are worth learning.
///
/// This is the single source of truth. The overlay renders it, the startup log
/// line is built from it, and `bindings_cover_every_action` fails the build if
/// [`resolve`] grows an action that never made it into this table. A binding
/// that exists but is written down nowhere is a binding nobody will find.
pub(crate) const BINDINGS: &[Binding] = &[
    Binding {
        action: Action::Spawn,
        chord: "Super+Ctrl+E / T",
        description: "open a terminal",
    },
    Binding {
        action: Action::Lock,
        chord: "Super+L",
        description: "lock the session",
    },
    Binding {
        action: Action::CloseFocused,
        chord: "Super+Ctrl+Q / X",
        description: "close the focused window",
    },
    Binding {
        action: Action::FocusNext,
        chord: "Super+Ctrl+J",
        description: "focus the next window",
    },
    Binding {
        action: Action::FocusPrev,
        chord: "Super+Ctrl+K",
        description: "focus the previous window",
    },
    Binding {
        action: Action::Move(Dir::Left),
        chord: "Super+Ctrl+arrows",
        description: "move the focused window between tiles",
    },
    Binding {
        action: Action::PromoteFocused,
        chord: "Super+Ctrl+Return",
        description: "swap the focused window into the first tile",
    },
    Binding {
        action: Action::ToggleCarousel,
        chord: "Super+Ctrl+C",
        description: "open or accept the workspace carousel",
    },
    Binding {
        action: Action::EnterResize,
        chord: "Super+Ctrl+R",
        description: "resize the focused window with the arrows",
    },
    Binding {
        action: Action::Workspace(0),
        chord: "Super+Ctrl+1..9",
        description: "go to a workspace",
    },
    Binding {
        action: Action::SendToWorkspace(0),
        chord: "Super+Ctrl+Shift+1..9",
        description: "send the focused window to a workspace",
    },
    Binding {
        action: Action::FocusNextOutput,
        chord: "Super+Ctrl+Tab",
        description: "focus the next screen",
    },
    Binding {
        action: Action::SendToNextOutput,
        chord: "Super+Ctrl+Shift+Tab",
        description: "send the focused window to the next screen",
    },
    Binding {
        action: Action::Copy,
        chord: "Super+C",
        description: "copy in the focused client",
    },
    Binding {
        action: Action::Paste,
        chord: "Super+V",
        description: "paste in the focused client",
    },
    Binding {
        action: Action::OpenLauncher,
        chord: "Super+Ctrl+Space",
        description: "open the application launcher",
    },
    Binding {
        action: Action::OpenPinned,
        chord: "Super+Ctrl+A",
        description: "open the pinned applications",
    },
    Binding {
        action: Action::OpenSettings,
        chord: "Super+Ctrl+S",
        description: "open quick settings",
    },
    Binding {
        action: Action::OpenFullSettings,
        chord: "Super+Ctrl+P",
        description: "open the settings application",
    },
    Binding {
        action: Action::OpenStore,
        chord: "Super+Ctrl+I",
        description: "open the software store",
    },
    Binding {
        action: Action::ToggleHelp,
        chord: "Super+Ctrl+H",
        description: "show or hide this list",
    },
    Binding {
        action: Action::Quit,
        chord: "Super+Ctrl+Esc",
        description: "quit the compositor",
    },
    Binding {
        action: Action::Screenshot(crate::screenshot::Shot::Screen),
        chord: "Print",
        description: "screenshot the screen (Shift: region, Ctrl: window)",
    },
    Binding {
        action: Action::Volume(crate::audio::Key::Raise),
        chord: "Volume keys",
        description: "raise, lower or mute the volume",
    },
];

/// The startup log line, built from [`BINDINGS`] rather than written out again.
///
/// Someone reading a session log is usually not looking at the screen, which is
/// exactly when an overlay is no use — so the same table has to reach both.
pub(crate) fn help_line() -> String {
    let keys = BINDINGS
        .iter()
        .map(|b| format!("{} {}", b.chord, b.description))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("keys: {keys}. Plain Super belongs to the focused application.")
}

/// Map a key event to an action, or forward it to the focused client.
///
/// Runs for releases as well as presses, so both halves of a keystroke need an
/// answer. A bound key is intercepted either way — forwarding a release whose
/// press was swallowed leaves the client believing a key it never saw pressed
/// is still down — but only the press carries an action, or every binding fires
/// twice and one keystroke opens two terminals.
///
/// [`Modes`] carries everything about the compositor's current state that
/// changes the answer — including whether the session is locked, which is
/// checked before anything else.
pub(crate) fn resolve(
    key_state: KeyState,
    modifiers: &ModifiersState,
    sym: u32,
    mode: Modes,
) -> FilterResult<Option<Action>> {
    // Locked, so there are no bindings at all: every key belongs to the lock
    // screen. First, before the modes below, and that ordering is load-bearing
    // twice over. A window-management chord resolved here would act on the
    // session the lock exists to hold — `Super`+`Ctrl`+`Q` would close a
    // window nobody can see. And the launcher branch further down swallows
    // *everything* when it is open, so a session locked with the launcher up
    // would forward not one keystroke to the lock screen: no password could be
    // typed, and the way out would be the power button.
    // The media keys, whatever else is going on. Before the lock check, for
    // the reason given on [`Action::Volume`], and before every panel: a
    // launcher that swallowed the mute key would be a launcher you could not
    // silence the machine through.
    if let Some(key) = volume_key(sym) {
        return FilterResult::Intercept(pressed(key_state, Action::Volume(key)));
    }

    if mode.locked {
        return FilterResult::Forward;
    }

    // A region selection owns the keyboard while it is up: Escape abandons it,
    // and every other key is swallowed rather than forwarded, so a keystroke
    // cannot reach and act on a window under the rectangle being dragged. Before
    // the screenshot keys below, so `Print` mid-selection does not start a
    // second capture over the first.
    if mode.selecting_region {
        let action = (sym == keysyms::KEY_Escape).then_some(Action::CancelRegion);
        return FilterResult::Intercept(action.and_then(|action| pressed(key_state, action)));
    }

    // Screenshots resolve here, before the `Super`-layer gate below, so `Print`
    // needs no modifier. It is available over any open panel — a capture is of
    // whatever is on the screen — but not while locked, which returned above.
    if let Some(shot) = screenshot_key(sym, modifiers) {
        return FilterResult::Intercept(pressed(key_state, Action::Screenshot(shot)));
    }

    if mode.switcher_open {
        let action = (sym == keysyms::KEY_Escape).then_some(Action::DismissSwitcher);
        return FilterResult::Intercept(action.and_then(|action| pressed(key_state, action)));
    }

    // Quick settings takes everything, for the same reason the launcher does:
    // it is compositor-drawn, so there is no surface to focus and nothing to
    // forward to. Checked first because a panel that is open owns the keyboard.
    // Resize mode owns the arrows and nothing else, so every other key both
    // leaves the mode and does what it would have done. A mode that swallowed
    // everything until it was dismissed would be a mode you get stuck in.
    if mode.resizing {
        let action = match sym {
            keysyms::KEY_Left => Some(Action::Resize(Dir::Left)),
            keysyms::KEY_Right => Some(Action::Resize(Dir::Right)),
            keysyms::KEY_Up => Some(Action::Resize(Dir::Up)),
            keysyms::KEY_Down => Some(Action::Resize(Dir::Down)),
            keysyms::KEY_Escape | keysyms::KEY_Return => Some(Action::LeaveResize),
            _ => None,
        };
        if let Some(action) = action {
            return FilterResult::Intercept(pressed(key_state, action));
        }
        // Not a resize key: fall through, and the caller leaves the mode.
    }

    // The overview owns only the keys that steer it — highlight with the
    // arrows, Return to take the highlighted window, Escape to put the
    // tiling back. Everything else falls through, so the chords still work:
    // `Super`+`Ctrl`+`C` still accepts, the digits still jump workspaces.
    if mode.overview {
        let action = match sym {
            keysyms::KEY_Left => Some(Action::OverviewMove(Dir::Left)),
            keysyms::KEY_Right => Some(Action::OverviewMove(Dir::Right)),
            keysyms::KEY_Up => Some(Action::OverviewMove(Dir::Up)),
            keysyms::KEY_Down => Some(Action::OverviewMove(Dir::Down)),
            keysyms::KEY_Return => Some(Action::OverviewConfirm),
            keysyms::KEY_Escape => Some(Action::OverviewCancel),
            _ => None,
        };
        if let Some(action) = action {
            return FilterResult::Intercept(pressed(key_state, action));
        }
    }

    if mode.settings_open {
        let key = crate::settings::Key::from_keysym(sym);
        return FilterResult::Intercept(pressed(key_state, Action::Settings(key)));
    }

    // The pinned panel, likewise: compositor-drawn, and every key is its
    // own — Shift turns the arrows into moves, so the modifier is passed
    // along rather than stripped.
    if mode.pinned_open {
        let key = crate::pinned::Key::from_keysym(sym, modifiers.shift);
        return FilterResult::Intercept(pressed(key_state, Action::Pinned(key)));
    }

    // The launcher takes everything, chords included, and forwards nothing.
    // A key reaching the focused client while a search field is open would let
    // a window act on what the user was typing at the launcher — and `Escape`
    // is the only way out, so it must never be shadowed by a binding.
    if let Some(character) = mode.launcher {
        let key = crate::launcher::Key::from_keysym(sym, modifiers.ctrl, character);
        return FilterResult::Intercept(pressed(key_state, Action::Launcher(key)));
    }

    if !modifiers.logo {
        return FilterResult::Forward;
    }

    // Plain Super is the application's, with two things taken out of it.
    // Ctrl is the compositor's modifier rather than Shift because Ctrl
    // leaves the keysym alone: `Super`+`Shift`+`1` arrives as `!` on a US
    // layout and as `1` on others, and every letter arrives capitalised,
    // whereas `Super`+`Ctrl`+`1` is `1` everywhere.
    if !modifiers.ctrl {
        // Locking is checked before the client's claim, not after. See
        // [`Action::Lock`]: this is the one chord that must mean the same thing
        // whatever is focused.
        if matches!(sym, keysyms::KEY_l | keysyms::KEY_L) {
            return FilterResult::Intercept(pressed(key_state, Action::Lock));
        }
        // Copy and paste are borrowed back, and only from clients that have no
        // use of their own for the chord.
        if mode.focus_owns_super {
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
        keysyms::KEY_space => Action::OpenLauncher,
        keysyms::KEY_a | keysyms::KEY_A => Action::OpenPinned,
        keysyms::KEY_s | keysyms::KEY_S => Action::OpenSettings,
        // P for preferences: S is the panel, and the two sit together.
        keysyms::KEY_p | keysyms::KEY_P => Action::OpenFullSettings,
        // I for install: the store is the other application the desktop
        // opens by name rather than through the launcher.
        keysyms::KEY_i | keysyms::KEY_I => Action::OpenStore,
        keysyms::KEY_r | keysyms::KEY_R => Action::EnterResize,
        // Ctrl is what separates this from `Super`+`C`, which is copy: the
        // branch above returns before this one whenever Ctrl is not held.
        keysyms::KEY_c | keysyms::KEY_C => Action::ToggleCarousel,
        keysyms::KEY_h | keysyms::KEY_H => Action::ToggleHelp,
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
        // Shift+Tab arrives as ISO_Left_Tab on most layouts and as Tab with
        // Shift held on the rest; both mean the same thing here.
        keysyms::KEY_ISO_Left_Tab => Action::SendToNextOutput,
        keysyms::KEY_Tab if modifiers.shift => Action::SendToNextOutput,
        keysyms::KEY_Tab => Action::FocusNextOutput,
        // Adding Shift sends the window there instead of going there yourself.
        sym => match workspace_index(sym) {
            Some(i) if modifiers.shift => Action::SendToWorkspace(i),
            Some(i) => Action::Workspace(i),
            None => return FilterResult::Forward,
        },
    };
    FilterResult::Intercept(pressed(key_state, action))
}

/// The media key a keysym names, if it is one.
///
/// These arrive without modifiers: on a laptop the volume keys are `Fn`
/// chords that the firmware has already turned into the XF86 keysyms, and on
/// a desktop keyboard they are keys of their own. Either way there is no
/// `Super` to check for.
fn volume_key(sym: u32) -> Option<crate::audio::Key> {
    use crate::audio::Key;
    match sym {
        keysyms::KEY_XF86AudioRaiseVolume => Some(Key::Raise),
        keysyms::KEY_XF86AudioLowerVolume => Some(Key::Lower),
        keysyms::KEY_XF86AudioMute => Some(Key::ToggleMute),
        _ => None,
    }
}

/// The screenshot a `Print` press asks for, from its modifiers, or `None` for
/// any other key.
///
/// `Print` is the whole screen, `Shift` the interactive region, `Ctrl` the
/// focused window — the three every desktop spells roughly this way.
fn screenshot_key(sym: u32, modifiers: &ModifiersState) -> Option<crate::screenshot::Shot> {
    use crate::screenshot::Shot;
    if sym != keysyms::KEY_Print {
        return None;
    }
    Some(if modifiers.shift {
        Shot::Region
    } else if modifiers.ctrl {
        Shot::Window
    } else {
        Shot::Screen
    })
}

/// The action, but only on the way down.
fn pressed(key_state: KeyState, action: Action) -> Option<Action> {
    (key_state == KeyState::Pressed).then_some(action)
}

/// The zero-based workspace a digit key names, however the layout shifts it.
///
/// Shift is held to send a window to a workspace, so on a US layout the digit
/// row then arrives as punctuation. Both are accepted: a layout that keeps
/// digits under Shift is no less entitled to nine workspaces.
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
        match resolve(KeyState::Pressed, &mods, sym, Modes::default()) {
            FilterResult::Intercept(action) => action,
            FilterResult::Forward => None,
        }
    }

    /// Whether the key reaches the focused client instead.
    fn forwarded(key_state: KeyState, mods: ModifiersState, sym: u32) -> bool {
        matches!(
            resolve(key_state, &mods, sym, Modes::default()),
            FilterResult::Forward
        )
    }

    fn super_held() -> ModifiersState {
        ModifiersState {
            logo: true,
            ..Default::default()
        }
    }

    fn super_ctrl() -> ModifiersState {
        ModifiersState {
            logo: true,
            ctrl: true,
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

    /// The action a press produces with the launcher open.
    fn to_launcher(
        mods: ModifiersState,
        sym: u32,
        ch: Option<char>,
    ) -> FilterResult<Option<Action>> {
        let mode = Modes {
            launcher: Some(ch),
            ..Modes::default()
        };
        resolve(KeyState::Pressed, &mods, sym, mode)
    }

    /// Resolve with resize mode active.
    fn while_resizing(sym: u32) -> FilterResult<Option<Action>> {
        let mode = Modes {
            resizing: true,
            ..Modes::default()
        };
        resolve(KeyState::Pressed, &ModifiersState::default(), sym, mode)
    }

    /// Resolve as though a client owns the `Super` layer.
    fn with_super_owned(mods: ModifiersState, sym: u32) -> FilterResult<Option<Action>> {
        let mode = Modes {
            focus_owns_super: true,
            ..Modes::default()
        };
        resolve(KeyState::Pressed, &mods, sym, mode)
    }

    /// Resolve as though the session is locked.
    fn while_locked(
        mods: ModifiersState,
        sym: u32,
        launcher: Option<Option<char>>,
    ) -> FilterResult<Option<Action>> {
        let mode = Modes {
            launcher,
            locked: true,
            ..Modes::default()
        };
        resolve(KeyState::Pressed, &mods, sym, mode)
    }

    #[test]
    fn super_l_locks_the_session() {
        assert_eq!(
            intercepted(super_held(), keysyms::KEY_l),
            Some(Action::Lock)
        );
        // Whatever the layout does with shift-lock or a shifted layer.
        assert_eq!(
            intercepted(super_held(), keysyms::KEY_L),
            Some(Action::Lock)
        );
    }

    /// The exception this binding exists on. A lock chord that a focused
    /// terminal can swallow fails at exactly the moment somebody walks away
    /// from a machine believing they locked it.
    #[test]
    fn super_l_is_not_given_back_to_a_client_that_owns_super() {
        assert!(matches!(
            with_super_owned(super_held(), keysyms::KEY_l),
            FilterResult::Intercept(Some(Action::Lock))
        ));
        // ...unlike copy, which is handed straight back to such a client.
        assert!(matches!(
            with_super_owned(super_held(), keysyms::KEY_c),
            FilterResult::Forward
        ));
    }

    #[test]
    fn l_on_its_own_is_just_a_letter() {
        assert_eq!(intercepted(ModifiersState::default(), keysyms::KEY_l), None);
    }

    /// Every key belongs to the lock screen, and no binding may act on the
    /// session behind it.
    #[test]
    fn a_locked_session_resolves_no_bindings() {
        for (mods, sym) in [
            (super_ctrl(), keysyms::KEY_q),      // would close a window
            (super_ctrl(), keysyms::KEY_space),  // would open the launcher
            (super_held(), keysyms::KEY_l),      // would lock again
            (super_held(), keysyms::KEY_c),      // would send the client Ctrl+C
            (super_ctrl(), keysyms::KEY_Escape), // would quit the compositor
        ] {
            assert!(
                matches!(while_locked(mods, sym, None), FilterResult::Forward),
                "a locked session must forward {sym:#x} rather than act on it"
            );
        }
    }

    /// The one that would be unrecoverable. The launcher swallows every key
    /// when it is open, so a session locked with it up would forward nothing
    /// to the lock screen -- no password could be typed at all.
    #[test]
    fn locking_over_an_open_launcher_still_reaches_the_lock_screen() {
        assert!(matches!(
            while_locked(ModifiersState::default(), keysyms::KEY_a, Some(Some('a'))),
            FilterResult::Forward
        ));
    }

    #[test]
    fn resize_mode_takes_the_bare_arrows() {
        // Bare, not Super+Ctrl+arrows — that chord already moves a window
        // between tiles, and overloading it would make the two
        // indistinguishable.
        for (sym, dir) in [
            (keysyms::KEY_Left, Dir::Left),
            (keysyms::KEY_Right, Dir::Right),
            (keysyms::KEY_Up, Dir::Up),
            (keysyms::KEY_Down, Dir::Down),
        ] {
            assert!(
                matches!(while_resizing(sym), FilterResult::Intercept(Some(Action::Resize(d))) if d == dir),
                "{sym:#x} did not resize"
            );
        }
    }

    #[test]
    fn escape_and_return_both_leave_resize_mode() {
        for sym in [keysyms::KEY_Escape, keysyms::KEY_Return] {
            assert!(matches!(
                while_resizing(sym),
                FilterResult::Intercept(Some(Action::LeaveResize))
            ));
        }
    }

    #[test]
    fn resize_mode_does_not_swallow_everything_else() {
        // A mode that held every key until dismissed is a mode you get stuck
        // in. Anything that is not an arrow falls through to what it would
        // have done, and the caller leaves the mode.
        assert!(matches!(
            while_resizing(keysyms::KEY_a),
            FilterResult::Forward
        ));
        assert!(matches!(
            while_resizing(keysyms::KEY_space),
            FilterResult::Forward
        ));
    }

    #[test]
    fn the_arrows_are_untouched_when_not_resizing() {
        // Or a text editor could never move its cursor.
        assert!(forwarded(
            KeyState::Pressed,
            ModifiersState::default(),
            keysyms::KEY_Left
        ));
    }

    #[test]
    fn the_resize_binding_is_reachable() {
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_R),
            Some(Action::EnterResize)
        );
    }

    /// Resolve with the overview up.
    fn while_overviewing(mods: ModifiersState, sym: u32) -> FilterResult<Option<Action>> {
        let mode = Modes {
            overview: true,
            ..Modes::default()
        };
        resolve(KeyState::Pressed, &mods, sym, mode)
    }

    #[test]
    fn the_overview_takes_the_arrows_return_and_escape() {
        let none = ModifiersState::default();
        assert!(matches!(
            while_overviewing(none, keysyms::KEY_Right),
            FilterResult::Intercept(Some(Action::OverviewMove(Dir::Right)))
        ));
        assert!(matches!(
            while_overviewing(none, keysyms::KEY_Return),
            FilterResult::Intercept(Some(Action::OverviewConfirm))
        ));
        assert!(matches!(
            while_overviewing(none, keysyms::KEY_Escape),
            FilterResult::Intercept(Some(Action::OverviewCancel))
        ));
    }

    #[test]
    fn the_overview_leaves_the_chords_alone() {
        // Super+Ctrl+C must still accept the overview, and the digits must
        // still jump workspaces — the mode owns three keys, not the keyboard.
        assert!(matches!(
            while_overviewing(super_ctrl(), keysyms::KEY_c),
            FilterResult::Intercept(Some(Action::ToggleCarousel))
        ));
        assert!(matches!(
            while_overviewing(super_ctrl(), keysyms::KEY_1),
            FilterResult::Intercept(Some(Action::Workspace(0)))
        ));
        assert!(matches!(
            while_overviewing(ModifiersState::default(), keysyms::KEY_a),
            FilterResult::Forward
        ));
    }

    #[test]
    fn an_open_launcher_takes_every_key_including_plain_letters() {
        // Nothing may reach the focused client while a search field is open,
        // or a window acts on what the user was typing at the launcher.
        for (mods, sym, ch) in [
            (ModifiersState::default(), keysyms::KEY_a, Some('a')),
            (ModifiersState::default(), keysyms::KEY_1, Some('1')),
            (super_held(), keysyms::KEY_c, Some('c')),
            (super_ctrl(), keysyms::KEY_q, Some('q')),
        ] {
            assert!(
                matches!(to_launcher(mods, sym, ch), FilterResult::Intercept(_)),
                "{sym:#x} escaped to the client"
            );
        }
    }

    #[test]
    fn an_open_launcher_shadows_every_compositor_binding() {
        // Including the ones that would otherwise close a window or quit the
        // session out from under the launcher.
        for sym in [
            keysyms::KEY_q,
            keysyms::KEY_T,
            keysyms::KEY_Escape,
            keysyms::KEY_H,
        ] {
            let FilterResult::Intercept(Some(action)) = to_launcher(super_ctrl(), sym, None) else {
                panic!("{sym:#x} was not intercepted for the launcher");
            };
            assert!(
                matches!(action, Action::Launcher(_)),
                "{sym:#x} resolved to {action:?} instead of going to the launcher"
            );
        }
    }

    #[test]
    fn escape_always_reaches_the_launcher_as_dismiss() {
        // The only way out. If a binding ever shadowed this the launcher would
        // have the keyboard and no exit.
        for mods in [ModifiersState::default(), super_held(), super_ctrl()] {
            assert!(
                matches!(
                    to_launcher(mods, keysyms::KEY_Escape, None),
                    FilterResult::Intercept(Some(Action::Launcher(crate::launcher::Key::Dismiss)))
                ),
                "Escape did not reach the launcher with {mods:?}"
            );
        }
    }

    #[test]
    fn super_ctrl_a_opens_the_pinned_panel_and_the_panel_takes_every_key() {
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_a),
            Some(Action::OpenPinned)
        );
        // Plain Super+A stays the application's.
        assert!(forwarded(KeyState::Pressed, super_held(), keysyms::KEY_a));
        let mode = Modes {
            pinned_open: true,
            ..Modes::default()
        };
        // Escape is the way out, and a compositor chord does not shadow it.
        for mods in [ModifiersState::default(), super_held(), super_ctrl()] {
            assert!(matches!(
                resolve(KeyState::Pressed, &mods, keysyms::KEY_Escape, mode),
                FilterResult::Intercept(Some(Action::Pinned(crate::pinned::Key::Dismiss)))
            ));
        }
        // Shift+arrow arrives as a move, and Super+Ctrl+arrow — the
        // window-management chord — reaches the panel too rather than moving
        // a window under it.
        let shift = ModifiersState {
            shift: true,
            ..Default::default()
        };
        assert!(matches!(
            resolve(KeyState::Pressed, &shift, keysyms::KEY_Left, mode),
            FilterResult::Intercept(Some(Action::Pinned(crate::pinned::Key::Move(Dir::Left))))
        ));
        assert!(matches!(
            resolve(KeyState::Pressed, &super_ctrl(), keysyms::KEY_Left, mode),
            FilterResult::Intercept(Some(Action::Pinned(crate::pinned::Key::Left)))
        ));
        // A letter it has no use for is swallowed rather than forwarded.
        assert!(matches!(
            resolve(
                KeyState::Pressed,
                &ModifiersState::default(),
                keysyms::KEY_z,
                mode
            ),
            FilterResult::Intercept(Some(Action::Pinned(crate::pinned::Key::Ignored)))
        ));
    }

    #[test]
    fn a_release_while_the_launcher_is_open_is_swallowed_but_does_nothing() {
        // Same rule as every other binding: both halves are intercepted so the
        // client never sees an orphaned release, but only the press acts.
        assert!(matches!(
            resolve(
                KeyState::Released,
                &ModifiersState::default(),
                keysyms::KEY_a,
                Modes {
                    launcher: Some(Some('a')),
                    ..Modes::default()
                },
            ),
            FilterResult::Intercept(None)
        ));
    }

    #[test]
    fn escape_dismisses_the_application_switcher_and_other_keys_are_swallowed() {
        let mode = Modes {
            switcher_open: true,
            ..Modes::default()
        };
        assert!(matches!(
            resolve(
                KeyState::Pressed,
                &ModifiersState::default(),
                keysyms::KEY_Escape,
                mode,
            ),
            FilterResult::Intercept(Some(Action::DismissSwitcher))
        ));
        assert!(matches!(
            resolve(
                KeyState::Pressed,
                &ModifiersState::default(),
                keysyms::KEY_a,
                mode,
            ),
            FilterResult::Intercept(None)
        ));
    }

    #[test]
    fn the_launcher_binding_is_reachable_when_it_is_closed() {
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_space),
            Some(Action::OpenLauncher)
        );
    }

    #[test]
    fn bindings_need_super_and_ctrl() {
        // Without Super every key belongs to the focused client. A compositor
        // that swallows a bare 'q' is unusable.
        assert_eq!(intercepted(ModifiersState::default(), keysyms::KEY_q), None);
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_Q),
            Some(Action::CloseFocused)
        );
    }

    #[test]
    fn the_plain_super_layer_reaches_the_application() {
        // RavenTerminal's own chords: a new tab, a pane, its help. Binding
        // these in the compositor would make them unreachable everywhere.
        for sym in [
            keysyms::KEY_t,
            keysyms::KEY_k,
            keysyms::KEY_bracketright,
            keysyms::KEY_1,
        ] {
            assert!(
                forwarded(KeyState::Pressed, super_held(), sym),
                "Super+{sym:#x} was eaten"
            );
        }
    }

    #[test]
    fn digits_map_to_zero_based_workspaces() {
        // Ctrl leaves the digit a digit, on every layout.
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_1),
            Some(Action::Workspace(0))
        );
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_9),
            Some(Action::Workspace(8))
        );
    }

    #[test]
    fn shift_sends_the_window_instead_of_following_it() {
        // With Shift held a US layout delivers the punctuation on the key...
        assert_eq!(
            intercepted(super_ctrl_shift(), keysyms::KEY_exclam),
            Some(Action::SendToWorkspace(0))
        );
        assert_eq!(
            intercepted(super_ctrl_shift(), keysyms::KEY_at),
            Some(Action::SendToWorkspace(1))
        );
        // ...and a layout that keeps the digit under Shift works too.
        assert_eq!(
            intercepted(super_ctrl_shift(), keysyms::KEY_9),
            Some(Action::SendToWorkspace(8))
        );
    }

    #[test]
    fn super_shift_alone_is_the_applications() {
        // The layer the compositor used to live on. Everything on it now
        // reaches the client, so a terminal may bind Super+Shift+T itself.
        let super_shift = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        for sym in [
            keysyms::KEY_T,
            keysyms::KEY_Q,
            keysyms::KEY_space,
            keysyms::KEY_exclam,
        ] {
            assert!(
                forwarded(KeyState::Pressed, super_shift, sym),
                "Super+Shift+{sym:#x} was eaten"
            );
        }
    }

    #[test]
    fn arrows_move_the_focused_window() {
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_Left),
            Some(Action::Move(Dir::Left))
        );
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_Down),
            Some(Action::Move(Dir::Down))
        );
        // Bare arrows are how you move a cursor. They must never be swallowed.
        assert!(forwarded(
            KeyState::Pressed,
            ModifiersState::default(),
            keysyms::KEY_Left
        ));
    }

    #[test]
    fn close_and_spawn_have_two_bindings_each() {
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_X),
            Some(Action::CloseFocused)
        );
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_T),
            Some(Action::Spawn)
        );
    }

    #[test]
    fn the_carousel_chord_is_not_swallowed_by_copy() {
        // Super+Ctrl+C must reach the carousel in both sym forms, and must do
        // so even when the focused client owns the Super layer -- the copy
        // branch consults `focus_owns_super`, the Ctrl layer must not.
        for sym in [keysyms::KEY_c, keysyms::KEY_C] {
            assert_eq!(
                intercepted(super_ctrl(), sym),
                Some(Action::ToggleCarousel),
                "Super+Ctrl+C resolved to something other than the carousel"
            );
            assert!(
                matches!(
                    resolve(
                        KeyState::Pressed,
                        &super_ctrl(),
                        sym,
                        Modes {
                            focus_owns_super: true,
                            ..Modes::default()
                        },
                    ),
                    FilterResult::Intercept(Some(Action::ToggleCarousel))
                ),
                "a terminal must not keep Super+Ctrl+C"
            );
        }
    }

    #[test]
    fn copy_and_paste_are_translated_for_a_client_that_needs_it() {
        assert_eq!(
            intercepted(super_held(), keysyms::KEY_c),
            Some(Action::Copy)
        );
        assert_eq!(
            intercepted(super_held(), keysyms::KEY_v),
            Some(Action::Paste)
        );
    }

    #[test]
    fn a_client_with_its_own_super_layer_keeps_copy_and_paste() {
        // Translating for a terminal would replace a working copy with Ctrl+C,
        // which it reads as SIGINT and sends to whatever is running.
        for sym in [keysyms::KEY_c, keysyms::KEY_v] {
            assert!(matches!(
                resolve(
                    KeyState::Pressed,
                    &super_held(),
                    sym,
                    Modes {
                        focus_owns_super: true,
                        ..Modes::default()
                    },
                ),
                FilterResult::Forward
            ));
        }
    }

    #[test]
    fn only_the_press_fires_a_binding() {
        // The filter sees both halves of a keystroke. Acting on each of them
        // runs every binding twice, which is two terminals per keystroke.
        assert_eq!(
            intercepted(super_ctrl(), keysyms::KEY_T),
            Some(Action::Spawn)
        );
        assert!(matches!(
            resolve(
                KeyState::Released,
                &super_ctrl(),
                keysyms::KEY_T,
                Modes::default(),
            ),
            FilterResult::Intercept(None)
        ));
    }

    #[test]
    fn a_bound_release_is_swallowed_with_its_press() {
        assert!(!forwarded(KeyState::Released, super_ctrl(), keysyms::KEY_T));
        assert!(forwarded(KeyState::Released, super_ctrl(), keysyms::KEY_z));
    }

    #[test]
    fn unbound_keys_reach_the_client() {
        assert_eq!(intercepted(super_ctrl(), keysyms::KEY_z), None);
    }

    /// Every action the keymap can produce is written down in `BINDINGS`, and
    /// nothing in `BINDINGS` is written down for an action the keymap cannot
    /// produce.
    ///
    /// This is what makes the table a source of truth rather than a comment
    /// that happens to be code. Sweeping the keysym space is cheap and needs no
    /// maintenance — a new binding added to `resolve` shows up here on its own,
    /// which a hand-written list of actions would not.
    #[test]
    fn bindings_cover_every_action() {
        use std::collections::HashSet;
        use std::mem::discriminant;

        let mut reachable = HashSet::new();
        for mods in [super_held(), super_ctrl(), super_ctrl_shift()] {
            for sym in 0..=u32::from(u16::MAX) {
                if let Some(action) = intercepted(mods, sym) {
                    reachable.insert(discriminant(&action));
                }
            }
        }
        // The XF86 block, where the media keys live, sits well above the
        // 16-bit keysyms and needs no modifier at all.
        for sym in 0x1008_FF00..=0x1008_FFFF {
            if let Some(action) = intercepted(ModifiersState::default(), sym) {
                reachable.insert(discriminant(&action));
            }
        }

        let documented: HashSet<_> = BINDINGS.iter().map(|b| discriminant(&b.action)).collect();

        let orphan = BINDINGS
            .iter()
            .find(|b| !reachable.contains(&discriminant(&b.action)));
        assert!(
            orphan.is_none(),
            "BINDINGS lists {:?}, which no key produces",
            orphan
        );

        assert_eq!(
            reachable.len(),
            documented.len(),
            "an action is reachable by a key but missing from BINDINGS, so the \
             overlay and the log line will not mention it"
        );
    }

    #[test]
    fn the_media_keys_need_no_modifier() {
        use crate::audio::Key;
        let none = ModifiersState::default();
        assert_eq!(
            intercepted(none, keysyms::KEY_XF86AudioRaiseVolume),
            Some(Action::Volume(Key::Raise))
        );
        assert_eq!(
            intercepted(none, keysyms::KEY_XF86AudioLowerVolume),
            Some(Action::Volume(Key::Lower))
        );
        assert_eq!(
            intercepted(none, keysyms::KEY_XF86AudioMute),
            Some(Action::Volume(Key::ToggleMute))
        );
    }

    /// Every panel swallows every key while it is open, and the lock forwards
    /// every key to the lock screen. The mute key is the one thing that has
    /// to get through all of them.
    #[test]
    fn the_media_keys_work_whatever_is_open() {
        use crate::audio::Key;
        let sym = keysyms::KEY_XF86AudioMute;
        let modes = [
            Modes {
                locked: true,
                ..Modes::default()
            },
            Modes {
                launcher: Some(None),
                ..Modes::default()
            },
            Modes {
                settings_open: true,
                ..Modes::default()
            },
            Modes {
                switcher_open: true,
                ..Modes::default()
            },
            Modes {
                resizing: true,
                ..Modes::default()
            },
            Modes {
                focus_owns_super: true,
                ..Modes::default()
            },
        ];
        for mode in modes {
            assert!(
                matches!(
                    resolve(KeyState::Pressed, &ModifiersState::default(), sym, mode),
                    FilterResult::Intercept(Some(Action::Volume(Key::ToggleMute)))
                ),
                "mute did not resolve under {mode:?}"
            );
        }
        // And the release is swallowed with its press, like every binding.
        assert!(matches!(
            resolve(
                KeyState::Released,
                &ModifiersState::default(),
                sym,
                Modes::default()
            ),
            FilterResult::Intercept(None)
        ));
    }

    fn shift() -> ModifiersState {
        ModifiersState {
            shift: true,
            ..Default::default()
        }
    }

    fn ctrl() -> ModifiersState {
        ModifiersState {
            ctrl: true,
            ..Default::default()
        }
    }

    #[test]
    fn print_screenshots_the_screen_with_no_modifier() {
        use crate::screenshot::Shot;
        // No Super, no anything: Print is one of the two keys (with the volume
        // keys) that resolve without the Super layer.
        assert_eq!(
            intercepted(ModifiersState::default(), keysyms::KEY_Print),
            Some(Action::Screenshot(Shot::Screen))
        );
        // Shift is a region, Ctrl is the focused window.
        assert_eq!(
            intercepted(shift(), keysyms::KEY_Print),
            Some(Action::Screenshot(Shot::Region))
        );
        assert_eq!(
            intercepted(ctrl(), keysyms::KEY_Print),
            Some(Action::Screenshot(Shot::Window))
        );
    }

    #[test]
    fn a_locked_session_does_not_screenshot() {
        // The lock check comes first, so Print over the lock screen reaches it
        // like any other key rather than capturing the session behind it.
        assert!(matches!(
            while_locked(ModifiersState::default(), keysyms::KEY_Print, None),
            FilterResult::Forward
        ));
    }

    #[test]
    fn a_region_selection_swallows_keys_but_escape_cancels() {
        let mode = Modes {
            selecting_region: true,
            ..Modes::default()
        };
        assert!(matches!(
            resolve(KeyState::Pressed, &ModifiersState::default(), keysyms::KEY_Escape, mode),
            FilterResult::Intercept(Some(Action::CancelRegion))
        ));
        // Any other key is intercepted-but-ignored, so nothing under the
        // rectangle being dragged sees it.
        assert!(matches!(
            resolve(KeyState::Pressed, &ModifiersState::default(), keysyms::KEY_a, mode),
            FilterResult::Intercept(None)
        ));
        // Including a second Print: one selection at a time.
        assert!(matches!(
            resolve(KeyState::Pressed, &ModifiersState::default(), keysyms::KEY_Print, mode),
            FilterResult::Intercept(None)
        ));
    }

    /// The overlay draws with an ASCII bitmap font, so a chord or description
    /// that strays outside it comes out as blanks.
    #[test]
    fn binding_text_is_ascii() {
        for binding in BINDINGS {
            for text in [binding.chord, binding.description] {
                assert!(
                    text.chars().all(|c| (' '..='~').contains(&c)),
                    "{text:?} is not renderable by the overlay font"
                );
            }
        }
    }
}
