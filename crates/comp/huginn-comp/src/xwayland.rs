//! XWayland: running X11 clients under Huginn.
//!
//! Smithay does not manage X11 windows for you. It runs the XWayland server and
//! hands back an X11 connection; the compositor has to *be* the X11 window
//! manager on the other end of it — accept `MapRequest`, answer
//! `ConfigureRequest`, and drive focus. That is what `XwmHandler` is, and this
//! module is Huginn's implementation of it.
//!
//! # Two moments, not one
//!
//! [`start`] spawns XWayland and inserts it into the event loop. Nothing is
//! managed at that point: XWayland comes up, and *later* emits
//! [`XWaylandEvent::Ready`] carrying a privileged X11 socket. Only then can
//! `X11Wm::start_wm` run, and only then is `DISPLAY` published — setting it
//! earlier means a client starting in the gap finds an X server with no window
//! manager, and its windows never map.
//!
//! # Why the handler is implemented twice
//!
//! `XwmHandler` is needed on two different types, for two different reasons.
//! `delegate_xwayland_shell!` requires `XWaylandShellHandler + XwmHandler` on
//! the type clients dispatch against, which is [`Huginn`] — that is where
//! `Display<Huginn>` sends them. And `X11Wm::start_wm::<D>` requires it on the
//! event loop's data type, which is `Nested` for the winit backend and `Udev`
//! for the real one, because `D` comes from `LoopHandle<'static, D>`.
//!
//! So the policy is written once, on `Huginn`, and [`impl_xwm_handler!`]
//! generates the backend impls as straight forwards into it. Hand-writing the
//! handler three times is the alternative, and the copies would drift.
//!
//! # Override-redirect
//!
//! Menus, tooltips and drag icons set `override_redirect`, which in X11 means
//! "the window manager must not touch this". They are deliberately kept out of
//! the layout — they choose their own coordinates and are tracked in
//! [`Huginn::x11_unmanaged`] instead. Handing one to the tiler would tile a
//! dropdown menu.

use smithay::{
    reexports::{calloop::LoopHandle, wayland_server::DisplayHandle},
    utils::{Logical, Rectangle},
    xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler, xwm::Reorder},
};

use smithay::wayland::xwayland_shell::XWaylandShellHandler;

use crate::{state::Huginn, window::WindowSurface};

/// What [`start`] needs from whichever type owns the event loop.
///
/// Both backend wrappers hold a `Huginn`; this is the one thing the shared
/// XWayland plumbing needs from them.
pub(crate) trait AsHuginn {
    fn as_huginn(&mut self) -> &mut Huginn;
}

/// Spawn XWayland and start the window manager when it signals ready.
///
/// Fail-soft throughout, deliberately: a compositor that refuses to start
/// because XWayland is missing is worse than one that starts without X11
/// support. The common cause is the `Xwayland` binary simply not being
/// installed, which is a reasonable state for a Wayland-only image.
pub(crate) fn start<D>(dh: &DisplayHandle, handle: &LoopHandle<'static, D>)
where
    D: XwmHandler + XWaylandShellHandler + AsHuginn + 'static,
{
    let (xwayland, client) = match XWayland::spawn(
        dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| {},
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("XWayland unavailable, X11 clients will not run: {e}");
            return;
        }
    };

    // Cloned into the callback because `start_wm` wants an owned handle, and
    // the callback may fire long after `start` has returned.
    let lh = handle.clone();

    let inserted = handle.insert_source(xwayland, move |event, _, data: &mut D| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            let wm = match X11Wm::start_wm(lh.clone(), x11_socket, client.clone()) {
                Ok(wm) => wm,
                Err(e) => {
                    tracing::error!("failed to start the X11 window manager: {e}");
                    return;
                }
            };
            let huginn = data.as_huginn();
            huginn.xwm = Some(wm);
            // Recorded rather than exported. `std::env::set_var` is unsafe in
            // edition 2024 and process-global for a value only child processes
            // need, so the display travels the same route WAYLAND_DISPLAY
            // already does: injected per-child in `backend::spawn`. Set only
            // now, never at spawn time — a client that connects before the
            // window manager exists maps windows nobody will manage.
            huginn.x11_display = Some(display_number);
            tracing::info!(display = display_number, "XWayland ready");
        }
        XWaylandEvent::Error => {
            tracing::warn!("XWayland failed to start; X11 clients will not run");
            let huginn = data.as_huginn();
            huginn.xwm = None;
            huginn.x11_display = None;
        }
    });

    if let Err(e) = inserted {
        tracing::warn!("could not insert the XWayland event source: {e}");
    }
}

/// The window-management policy.
///
/// `XwmHandler` is required on `Huginn` itself, because
/// `delegate_xwayland_shell!` needs `XWaylandShellHandler + XwmHandler` on the
/// type clients dispatch against — and separately on each backend's loop data
/// type, because that is `start_wm`'s `D`. The policy lives here; the backend
/// impls that `impl_xwm_handler!` generates forward straight into it.
impl XwmHandler for Huginn {
    fn xwm_state(&mut self, _xwm: smithay::xwayland::xwm::XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("x11 event dispatched with no window manager")
    }

    fn new_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_new_window(w);
    }

    fn new_override_redirect_window(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        _w: X11Surface,
    ) {
    }

    fn map_window_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_map_request(w);
    }

    fn mapped_override_redirect_window(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        w: X11Surface,
    ) {
        self.x11_mapped_override_redirect(w);
    }

    fn unmapped_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_unmapped_window(w);
    }

    fn destroyed_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_destroyed_window(w);
    }

    #[allow(clippy::too_many_arguments)]
    fn configure_request(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        w: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.x11_configure_request(w, x, y, width, height, reorder);
    }

    fn configure_notify(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        _w: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        self.x11_configure_notify();
    }

    fn resize_request(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        _w: X11Surface,
        _button: u32,
        _edges: smithay::xwayland::xwm::ResizeEdge,
    ) {
        self.x11_resize_request();
    }

    fn fullscreen_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_fullscreen_request(&w, true);
    }

    fn unfullscreen_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, w: X11Surface) {
        self.x11_fullscreen_request(&w, false);
    }

    fn move_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, _w: X11Surface, _button: u32) {
        self.x11_move_request();
    }

    fn property_notify(
        &mut self,
        _xwm: smithay::xwayland::xwm::XwmId,
        w: X11Surface,
        property: smithay::xwayland::xwm::WmWindowProperty,
    ) {
        self.x11_property_notify(&w, property);
    }
}

impl Huginn {
    /// A window property changed. The title is on the bar and the class picks
    /// the dock icon, so either is a redraw; the rest are nothing to us.
    pub(crate) fn x11_property_notify(
        &mut self,
        window: &X11Surface,
        property: smithay::xwayland::xwm::WmWindowProperty,
    ) {
        use smithay::xwayland::xwm::WmWindowProperty;
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class)
            && let Some(id) = self.x11_window_id(window)
        {
            self.sync_foreign_toplevel(id);
        }
    }

    /// A window exists but has not asked to be shown. Nothing to do: X11
    /// creates windows eagerly and most are never mapped.
    pub(crate) fn x11_new_window(&mut self, _window: X11Surface) {}

    /// The client wants the window on screen: this is where it enters the
    /// layout.
    ///
    /// Ordering mirrors `new_toplevel` — take a tile first, then tell the
    /// client where it landed — for the same reason. A window that paints at a
    /// size it chose and is resized immediately afterwards shows one wrong
    /// frame, and that frame is what users read as the desktop being janky.
    pub(crate) fn x11_map_request(&mut self, window: X11Surface) {
        if let Err(e) = window.set_mapped(true) {
            tracing::warn!("x11 set_mapped failed: {e}");
            return;
        }

        let id = self.open_x11_window(WindowSurface::X11(window.clone()));
        tracing::debug!(window = id.raw(), class = %window.class(), "x11 window mapped");

        // An X11 window has no way to ask for a frame, so it gets one unless
        // its Motif hints say it draws its own. Before `arrange`, so the one
        // configure that follows already carries the content size.
        self.set_decor_mode(
            id,
            if window.is_decorated() {
                crate::decor::DecorMode::Client
            } else {
                crate::decor::DecorMode::Server
            },
        );

        self.arrange();
        self.refresh_focus();
        self.queue_redraw();
    }

    /// An override-redirect window became visible. Not laid out — see the
    /// module docs — only recorded so it can be drawn where the client put it.
    pub(crate) fn x11_mapped_override_redirect(&mut self, window: X11Surface) {
        if !self.x11_unmanaged.contains(&window) {
            self.x11_unmanaged.push(window);
        }
        self.queue_redraw();
    }

    /// Gone from the screen, managed or not.
    ///
    /// Unmapped is not destroyed: the client may map the same window again, and
    /// it must not still be marked mapped when it does.
    pub(crate) fn x11_unmapped_window(&mut self, window: X11Surface) {
        self.x11_unmanaged.retain(|w| w != &window);

        if let Some(id) = self.x11_window_id(&window) {
            self.close_x11_window(id);
            tracing::debug!(window = id.raw(), "x11 window unmapped");
        }

        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        self.queue_redraw();
    }

    /// Destroyed. `unmapped_window` has normally run already, so this is
    /// usually just the unmanaged list.
    pub(crate) fn x11_destroyed_window(&mut self, window: X11Surface) {
        self.x11_unmanaged.retain(|w| w != &window);
        if let Some(id) = self.x11_window_id(&window) {
            self.close_x11_window(id);
        }
        self.queue_redraw();
    }

    /// The client asked to be moved or resized.
    ///
    /// A managed window does not get to choose — the layout owns its geometry.
    /// But the request cannot simply be dropped: X11 requires a reply, and a
    /// client that never receives one can wait forever. So it is answered with
    /// the tile it already has.
    pub(crate) fn x11_configure_request(
        &mut self,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        if let Some(id) = self.x11_window_id(&window)
            && let Some(rect) = self.space.window(id).map(|w| w.content())
        {
            let geo = Rectangle::<i32, Logical>::new(
                (rect.x(), rect.y()).into(),
                (rect.w(), rect.h()).into(),
            );
            let _ = window.configure(Some(geo));
            return;
        }

        // Unmanaged, or not in the layout: honour the request, since nothing
        // else is deciding for it.
        let current = window.geometry();
        let geo = Rectangle::<i32, Logical>::new(
            (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into(),
            (
                w.map_or(current.size.w, |v| v as i32),
                h.map_or(current.size.h, |v| v as i32),
            )
                .into(),
        );
        let _ = window.configure(Some(geo));
    }

    /// XWayland reporting where a window ended up. Only override-redirect
    /// windows place themselves, so this is a redraw and nothing more.
    pub(crate) fn x11_configure_notify(&mut self) {
        self.queue_redraw();
    }

    /// Interactive move and resize, which this compositor does not offer: the
    /// layout owns geometry and there is no drag-to-resize. Doing nothing is
    /// the correct answer here, not a missing one.
    pub(crate) fn x11_resize_request(&mut self) {}

    /// An X11 client asking for `_NET_WM_STATE_FULLSCREEN`, or asking out of
    /// it. Same path as an XDG client: see [`Huginn::set_fullscreen`].
    pub(crate) fn x11_fullscreen_request(&mut self, window: &X11Surface, on: bool) {
        if let Some(id) = self.x11_window_id(window) {
            self.set_fullscreen(id, on);
        }
    }
    pub(crate) fn x11_move_request(&mut self) {}
}

/// Generate the two XWayland trait impls for a backend's loop data type.
///
/// See the module docs for why these cannot live on `Huginn`.
#[macro_export]
macro_rules! impl_xwm_handler {
    ($ty:ty) => {
        impl $crate::xwayland::AsHuginn for $ty {
            fn as_huginn(&mut self) -> &mut $crate::state::Huginn {
                &mut self.state
            }
        }

        impl smithay::wayland::xwayland_shell::XWaylandShellHandler for $ty {
            fn xwayland_shell_state(
                &mut self,
            ) -> &mut smithay::wayland::xwayland_shell::XWaylandShellState {
                &mut self.state.xwayland_shell_state
            }
        }

        // Every method forwards into the `XwmHandler for Huginn` impl above,
        // which is where the actual policy lives.
        impl smithay::xwayland::XwmHandler for $ty {
            fn xwm_state(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
            ) -> &mut smithay::xwayland::X11Wm {
                smithay::xwayland::XwmHandler::xwm_state(&mut self.state, xwm)
            }

            fn new_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::new_window(&mut self.state, xwm, w);
            }

            fn new_override_redirect_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::new_override_redirect_window(
                    &mut self.state,
                    xwm,
                    w,
                );
            }

            fn map_window_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::map_window_request(&mut self.state, xwm, w);
            }

            fn mapped_override_redirect_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::mapped_override_redirect_window(
                    &mut self.state,
                    xwm,
                    w,
                );
            }

            fn unmapped_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::unmapped_window(&mut self.state, xwm, w);
            }

            fn destroyed_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::destroyed_window(&mut self.state, xwm, w);
            }

            #[allow(clippy::too_many_arguments)]
            fn configure_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
                x: Option<i32>,
                y: Option<i32>,
                width: Option<u32>,
                height: Option<u32>,
                reorder: Option<smithay::xwayland::xwm::Reorder>,
            ) {
                smithay::xwayland::XwmHandler::configure_request(
                    &mut self.state,
                    xwm,
                    w,
                    x,
                    y,
                    width,
                    height,
                    reorder,
                );
            }

            fn configure_notify(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
                geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
                above: Option<u32>,
            ) {
                smithay::xwayland::XwmHandler::configure_notify(
                    &mut self.state,
                    xwm,
                    w,
                    geometry,
                    above,
                );
            }

            fn resize_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
                button: u32,
                edges: smithay::xwayland::xwm::ResizeEdge,
            ) {
                smithay::xwayland::XwmHandler::resize_request(
                    &mut self.state,
                    xwm,
                    w,
                    button,
                    edges,
                );
            }

            fn fullscreen_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::fullscreen_request(&mut self.state, xwm, w);
            }

            fn unfullscreen_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
            ) {
                smithay::xwayland::XwmHandler::unfullscreen_request(&mut self.state, xwm, w);
            }

            fn property_notify(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
                property: smithay::xwayland::xwm::WmWindowProperty,
            ) {
                smithay::xwayland::XwmHandler::property_notify(&mut self.state, xwm, w, property);
            }

            fn move_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                w: smithay::xwayland::X11Surface,
                button: u32,
            ) {
                smithay::xwayland::XwmHandler::move_request(&mut self.state, xwm, w, button);
            }
        }
    };
}
