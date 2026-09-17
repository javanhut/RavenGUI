//! What the seat's keyboard focus points at.
//!
//! Before XWayland there was one answer — a `WlSurface` — and the seat was
//! declared with it directly. An X11 window is backed by a `wl_surface` too,
//! and focusing that surface does deliver key events, which is exactly why
//! focusing it looks like it works. It is not enough.
//!
//! smithay sets real X input focus in one place only: `KeyboardTarget for
//! X11Surface`'s `enter`, which calls `SetInputFocus` and sends
//! `WM_TAKE_FOCUS`. Reach an X11 window through its `wl_surface` and that impl
//! never runs, so no X11 client is ever focused as far as the X server is
//! concerned. No `FocusIn` follows; the X11 window manager updates
//! `_NET_ACTIVE_WINDOW` from `FocusIn`/`FocusOut`, so that root property stays
//! pointed at the root window and every client reading it concludes nobody
//! holds the keyboard.
//!
//! Wine is the client that makes this visible. It takes focus and fullscreen
//! confirmation from EWMH rather than from the events it is handed, so a game
//! that is plainly receiving keystrokes still believes it is unfocused, and
//! never grabs the pointer or the keyboard. The window cannot be typed into
//! and the cursor will not stay inside it.
//!
//! So the seat holds this instead, and an X11 window is focused *as* an
//! [`X11Surface`].
//!
//! ## Why the X11 variant carries its `wl_surface`
//!
//! [`PopupGrab`](smithay::desktop::PopupGrab) requires
//! `PointerFocus: From<KeyboardFocus>`, and the pointer still focuses a plain
//! `WlSurface`. An `X11Surface` is associated with its `wl_surface` a round
//! trip after the X11 window appears, so asking one for its surface is
//! fallible and that conversion could not be written. Holding the surface in
//! the variant makes it total, and costs nothing: a window with no surface has
//! nothing on screen and is not somewhere the keyboard can go, so the only
//! constructor ([`WindowSurface::keyboard_target`](crate::window::WindowSurface::keyboard_target))
//! already declines to build one.

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

/// Whatever currently holds the keyboard.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KeyboardFocusTarget {
    /// A native Wayland surface: an `xdg_toplevel`, a layer surface, a popup,
    /// or the lock screen.
    Wayland(WlSurface),
    /// An X11 window, focused through the X11 window manager.
    ///
    /// `wl` is the surface XWayland associated with `surface`; see the module
    /// documentation for why it is stored rather than asked for.
    ///
    /// Boxed because an `X11Surface` is several times the size of a
    /// `WlSurface`, and this enum is copied around on every focus change.
    X11 {
        surface: Box<X11Surface>,
        wl: WlSurface,
    },
}

impl KeyboardFocusTarget {
    /// The Wayland surface behind this focus.
    ///
    /// The clipboard is handed out per client and a client is found from a
    /// surface, so the data device needs this for X11 windows as much as for
    /// native ones.
    pub(crate) fn surface(&self) -> &WlSurface {
        match self {
            Self::Wayland(s) => s,
            Self::X11 { wl, .. } => wl,
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

/// What a popup grab hands back when the grab ends and focus returns to the
/// surface underneath.
impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

/// Total by construction — see the module documentation.
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        match target {
            KeyboardFocusTarget::Wayland(s) => s,
            KeyboardFocusTarget::X11 { wl, .. } => wl,
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        Some(Cow::Borrowed(self.surface()))
    }
}

impl IsAlive for KeyboardFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            // The X11 window is the thing that is focused, so it is the thing
            // whose death ends the focus. Its surface can outlive the X11
            // window it was associated with.
            Self::X11 { surface, .. } => surface.alive(),
        }
    }
}

// Every method forwards to the variant's own `KeyboardTarget`. The X11 arm is
// the entire point of this enum: that impl is what sets X input focus, and it
// forwards to the `wl_surface` itself once it has.
impl KeyboardTarget<Huginn> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<Huginn>,
        data: &mut Huginn,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11 { surface, .. } => {
                KeyboardTarget::enter(&**surface, seat, data, keys, serial)
            }
        }
    }

    fn leave(&self, seat: &Seat<Huginn>, data: &mut Huginn, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11 { surface, .. } => KeyboardTarget::leave(&**surface, seat, data, serial),
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
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11 { surface, .. } => {
                KeyboardTarget::key(&**surface, seat, data, key, state, serial, time)
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
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11 { surface, .. } => {
                KeyboardTarget::modifiers(&**surface, seat, data, modifiers, serial)
            }
        }
    }
}
