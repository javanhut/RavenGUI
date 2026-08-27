//! What a window is, once X11 clients exist.
//!
//! Before XWayland there was exactly one kind of window and the compositor
//! stored it directly: `HashMap<WindowId, ToplevelSurface>`. An X11 client is
//! not a `ToplevelSurface` and never becomes one — it is an [`X11Surface`],
//! managed over the X11 protocol by [`X11Wm`](smithay::xwayland::X11Wm) and
//! only incidentally backed by a Wayland surface that XWayland creates on its
//! behalf.
//!
//! So the layout core keeps its [`WindowId`](huginn_core::WindowId) keys and
//! this enum becomes what those keys point at. Everything the compositor asks
//! of a window — where is your surface, what are you called, take focus, be
//! this size, go away — is asked through here, and the two answers differ more
//! than they look:
//!
//! * **Geometry is a request to Wayland and a command to X11.** `xdg_toplevel`
//!   configure is a negotiation: the compositor proposes, the client commits a
//!   buffer at whatever size it accepted. X11 `ConfigureWindow` just moves the
//!   window. That is why [`WindowSurface::configure`] returns nothing for the
//!   XDG case (the answer arrives later, as a commit) and applies immediately
//!   for X11.
//!
//! * **An X11 surface can have no `wl_surface` yet.** XWayland creates the X11
//!   window first and associates a Wayland surface a round trip later, so
//!   `wl_surface()` returns `Option`. Everything that renders or hit-tests has
//!   to tolerate the gap rather than unwrap it — during that window the window
//!   exists to the layout and has nothing to draw, which is the same state an
//!   XDG window is in before its first commit and is handled the same way.
//!
//! * **X11 calls go over a socket and can fail.** Every setter returns a
//!   `Result` carrying a `ConnectionError`. A dead X connection is not
//!   something the compositor can do anything about mid-frame, so these are
//!   logged at debug and swallowed; the alternative is propagating an error
//!   through the layout for a client that is already gone.

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Rectangle},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
    xwayland::X11Surface,
};

use smithay::wayland::shell::xdg::ToplevelSurface;

use huginn_core::geometry::Rect;

/// A window the layout knows about, whichever protocol it speaks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WindowSurface {
    /// A native Wayland window, via `xdg_shell`.
    Xdg(ToplevelSurface),
    /// An X11 window, via XWayland and the compositor's X11 window manager.
    X11(X11Surface),
}

impl WindowSurface {
    /// The Wayland surface backing this window, if it has one yet.
    ///
    /// `None` for an X11 window between its creation and XWayland associating a
    /// surface with it. See the module docs.
    pub(crate) fn wl_surface(&self) -> Option<WlSurface> {
        match self {
            Self::Xdg(t) => Some(t.wl_surface().clone()),
            Self::X11(x) => x.wl_surface(),
        }
    }

    /// What the window calls itself, for the dock and the launcher.
    ///
    /// `app_id` on Wayland; the X11 equivalent is `WM_CLASS`, whose *class*
    /// field is the one that identifies the application. It is a plain `String`
    /// there rather than an `Option`, and empty when unset — normalised to
    /// `None` here so callers have one shape to handle.
    pub(crate) fn app_id(&self) -> Option<String> {
        match self {
            Self::Xdg(t) => with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()?
                    .lock()
                    .ok()?
                    .app_id
                    .clone()
            }),
            Self::X11(x) => {
                let class = x.class();
                (!class.is_empty()).then_some(class)
            }
        }
    }

    /// Ask the client to close. Politely: both of these are requests the client
    /// is free to answer with a save dialog, and neither kills anything.
    pub(crate) fn close(&self) {
        match self {
            Self::Xdg(t) => t.send_close(),
            Self::X11(x) => {
                if let Err(e) = x.close() {
                    tracing::debug!("x11 close failed: {e}");
                }
            }
        }
    }

    /// Mark the window focused or not, and report whether that changed.
    ///
    /// The return value is what keeps the compositor from configuring on every
    /// pass: `refresh_focus` runs whenever focus *might* have moved, and an
    /// unconditional configure there drives clients into a redraw loop.
    pub(crate) fn set_activated(&self, activated: bool) -> bool {
        match self {
            Self::Xdg(t) => t.with_pending_state(|state| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
                let was = state.states.contains(xdg_toplevel::State::Activated);
                if activated {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
                was != activated
            }),
            Self::X11(x) => {
                if x.is_activated() == activated {
                    return false;
                }
                if let Err(e) = x.set_activated(activated) {
                    tracing::debug!("x11 set_activated failed: {e}");
                    return false;
                }
                true
            }
        }
    }

    /// Send whatever the protocol needs to make a pending state change take
    /// effect. Only XDG has a separate commit step; X11 applied it already.
    pub(crate) fn send_configure(&self) {
        if let Self::Xdg(t) = self {
            t.send_configure();
        }
    }

    /// Put the window at `rect`.
    ///
    /// XDG: stage the size and send one configure. X11: move and resize now.
    pub(crate) fn configure(&self, rect: Rect) {
        match self {
            Self::Xdg(t) => {
                t.with_pending_state(|state| {
                    state.size = Some((rect.w(), rect.h()).into());
                });
                // Only when something actually changed. An `xdg_toplevel`
                // configure carries a size and never a position — where a window
                // sits is the compositor's business — so a window that moved
                // without resizing has nothing to be told, and `send_configure`
                // would tell it anyway and wait for an ack.
                //
                // Tiling rarely moves a window without also resizing it, which
                // is why this went unnoticed. The carousel does nothing else:
                // scrolling the strip slides every pane at a constant width, so
                // with `send_configure` here a single focus change would
                // configure every window on the workspace and have each redraw
                // to arrive at the size it already had.
                t.send_pending_configure();
            }
            Self::X11(x) => {
                let geo = Rectangle::<i32, Logical>::new(
                    (rect.x(), rect.y()).into(),
                    (rect.w(), rect.h()).into(),
                );
                if let Err(e) = x.configure(Some(geo)) {
                    tracing::debug!("x11 configure failed: {e}");
                }
            }
        }
    }

    /// The XDG toplevel, for the paths that are genuinely XDG-only — the
    /// initial-configure check in `arrange`, which has no X11 counterpart
    /// because X11 windows are not configured before they are mapped.
    pub(crate) fn as_xdg(&self) -> Option<&ToplevelSurface> {
        match self {
            Self::Xdg(t) => Some(t),
            Self::X11(_) => None,
        }
    }

    /// The X11 surface, for stacking and map/unmap, which have no XDG
    /// counterpart because Wayland has no notion of either.
    pub(crate) fn as_x11(&self) -> Option<&X11Surface> {
        match self {
            Self::X11(x) => Some(x),
            Self::Xdg(_) => None,
        }
    }
}

impl From<ToplevelSurface> for WindowSurface {
    fn from(t: ToplevelSurface) -> Self {
        Self::Xdg(t)
    }
}

impl From<X11Surface> for WindowSurface {
    fn from(x: X11Surface) -> Self {
        Self::X11(x)
    }
}
