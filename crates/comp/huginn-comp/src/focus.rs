//! What the keyboard can be focused on.
//!
//! A bare `WlSurface` is not enough once X11 clients exist. Giving XWayland's
//! surface `wl_keyboard.enter` tells the X *server* it has the keyboard; it
//! says nothing about which X11 *window* holds the input focus, and that is a
//! separate piece of state only the window manager — Huginn — can set. Nobody
//! setting it fails in a way that looks like the client's fault: pointer
//! events are routed by position and keep working, while key events are routed
//! by X11 focus and go nowhere. Wine makes it total rather than intermittent,
//! because its windows use the globally-active input model (`WM_HINTS.input`
//! false, `WM_TAKE_FOCUS` in `WM_PROTOCOLS`): they never take the keyboard
//! until the window manager sends `WM_TAKE_FOCUS`. A game under Proton gets a
//! working mouse and a dead keyboard.
//!
//! smithay already knows how to do the X11 half — `SetInputFocus`,
//! `WM_TAKE_FOCUS`, and clearing both on the way out — but only inside
//! `X11Surface`'s own [`KeyboardTarget`] impl. So the fix is to focus the
//! `X11Surface` rather than the `WlSurface` under it, and [`FocusTarget`] is
//! the type that lets the seat hold either.

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::state::Huginn;

/// Whoever holds the keyboard.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FocusTarget {
    /// A Wayland client's surface: a toplevel, a popup, a layer surface, the
    /// lock screen.
    Surface(WlSurface),
    /// An X11 window.
    ///
    /// Carries the `WlSurface` it had when focus was resolved as well as the
    /// window. smithay's popup grabs need `WlSurface: From<FocusTarget>`, and
    /// an `X11Surface` alone cannot promise one — it has none between creation
    /// and XWayland associating it. Resolving it up front keeps that
    /// conversion total, and a window that remaps onto a new surface compares
    /// unequal to its old self, so focus is re-sent rather than assumed.
    ///
    /// Boxed because an `X11Surface` carries the whole atom table and would
    /// otherwise size every Wayland focus to match.
    X11 {
        window: Box<X11Surface>,
        surface: WlSurface,
    },
}

impl FocusTarget {
    /// The Wayland surface the keystrokes end up on, whichever kind this is.
    pub(crate) fn surface(&self) -> &WlSurface {
        match self {
            Self::Surface(surface) | Self::X11 { surface, .. } => surface,
        }
    }
}

impl From<WlSurface> for FocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

impl From<PopupKind> for FocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.into())
    }
}

impl From<FocusTarget> for WlSurface {
    fn from(target: FocusTarget) -> Self {
        match target {
            FocusTarget::Surface(surface) | FocusTarget::X11 { surface, .. } => surface,
        }
    }
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(surface) => surface.alive(),
            Self::X11 { window, .. } => window.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        Some(Cow::Borrowed(self.surface()))
    }
}

// The X11 arm forwards to the `X11Surface`, never to the `WlSurface` beside
// it: the window's impl does the X11 focus work and then forwards to its own
// surface, so going to both would send every event twice.
impl KeyboardTarget<Huginn> for FocusTarget {
    fn enter(
        &self,
        seat: &Seat<Huginn>,
        data: &mut Huginn,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Surface(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
            Self::X11 { window, .. } => KeyboardTarget::enter(&**window, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Huginn>, data: &mut Huginn, serial: Serial) {
        match self {
            Self::Surface(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            Self::X11 { window, .. } => KeyboardTarget::leave(&**window, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<Huginn>,
        data: &mut Huginn,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Surface(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time);
            }
            Self::X11 { window, .. } => {
                KeyboardTarget::key(&**window, seat, data, key, state, serial, time);
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<Huginn>,
        data: &mut Huginn,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Surface(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial);
            }
            Self::X11 { window, .. } => {
                KeyboardTarget::modifiers(&**window, seat, data, modifiers, serial);
            }
        }
    }
}
