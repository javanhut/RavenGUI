//! Compositor state and Wayland protocol handlers.
//!
//! This module is the seam between the two halves of the compositor. The
//! Wayland side lives here — globals, handler traits, surfaces. The policy side
//! lives in `huginn-core` and knows nothing about any of it.
//!
//! The translation is deliberately narrow: [`Huginn::arrange`] asks the core
//! where windows go and turns the answer into `xdg_toplevel::configure` events.
//! Nothing else in this file makes a layout decision.

use std::collections::HashMap;
use std::os::unix::io::OwnedFd;

use huginn_core::{Space, geometry::Rect, window::WindowId};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};

/// Per-client state the compositor attaches to every connection.
#[derive(Default, Debug)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, id: ClientId) {
        tracing::debug!(?id, "client connected");
    }

    fn disconnected(&self, id: ClientId, reason: DisconnectReason) {
        tracing::debug!(?id, ?reason, "client disconnected");
    }
}

/// The compositor.
pub(crate) struct Huginn {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    /// Held, not read: dropping this would withdraw the xdg-output global
    /// and clients would lose their output information mid-session.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,

    /// Window-management policy. The only thing that decides geometry.
    pub space: Space,
    /// Wayland surface for each window the core knows about.
    toplevels: HashMap<WindowId, ToplevelSurface>,
}

impl Huginn {
    pub(crate) fn new(dh: &DisplayHandle, area: Rect) -> Self {
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(dh, "huginn");

        Self {
            compositor_state: CompositorState::new::<Self>(dh),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            shm_state: ShmState::new::<Self>(dh, Vec::new()),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            seat,
            space: Space::new(area),
            toplevels: HashMap::new(),
        }
    }

    /// The surface backing `id`, if it is still mapped.
    pub(crate) fn surface(&self, id: WindowId) -> Option<&ToplevelSurface> {
        self.toplevels.get(&id)
    }

    /// Windows on the active workspace with the geometry the core assigned,
    /// in the order they should be painted.
    pub(crate) fn render_list(&self) -> Vec<(&ToplevelSurface, Rect)> {
        self.space
            .active_workspace()
            .windows()
            .iter()
            .filter_map(|id| {
                let surface = self.toplevels.get(id)?;
                let geometry = self.space.window(*id)?.geometry;
                Some((surface, geometry))
            })
            .collect()
    }

    /// Ask the core to lay out the active workspace and configure whatever
    /// moved.
    ///
    /// `arrange` returns only windows whose geometry actually changed, so this
    /// sends the minimum number of configures. Sending one per window on every
    /// call would make clients re-render continuously.
    pub(crate) fn arrange(&mut self) {
        for (id, rect) in self.space.arrange() {
            let Some(surface) = self.toplevels.get(&id) else {
                continue;
            };
            surface.with_pending_state(|state| {
                state.size = Some((rect.w(), rect.h()).into());
            });
            surface.send_configure();
        }
    }

    /// Give keyboard focus to the core's focused window, and mark it activated
    /// so clients draw themselves as focused.
    pub(crate) fn refresh_focus(&mut self) {
        let focused = self.space.focused();

        for (id, surface) in &self.toplevels {
            let is_focused = Some(*id) == focused;
            let changed = surface.with_pending_state(|state| {
                let was = state.states.contains(xdg_toplevel::State::Activated);
                if is_focused {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
                was != is_focused
            });
            if changed {
                surface.send_configure();
            }
        }

        let target = focused
            .and_then(|id| self.toplevels.get(&id))
            .map(|s| s.wl_surface().clone());
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, target, Serial::from(0));
        }
    }

    /// Attach an output so clients can discover scale, mode, and position.
    pub(crate) fn add_output(&self, output: &Output, dh: &DisplayHandle) {
        output.create_global::<Self>(dh);
    }
}

impl CompositorHandler for Huginn {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
    }
}

impl XdgShellHandler for Huginn {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.space.open_window();
        self.toplevels.insert(id, surface.clone());
        tracing::debug!(window = id.raw(), "toplevel mapped");

        // The first configure must go out before the client attaches a buffer;
        // xdg-shell requires the client to ack a configure before its first
        // commit, so a window that never gets one simply never appears.
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();

        self.arrange();
        self.refresh_focus();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self
            .toplevels
            .iter()
            .find(|(_, s)| *s == &surface)
            .map(|(id, _)| *id)
        else {
            return;
        };
        self.toplevels.remove(&id);
        self.space.close_window(id);
        tracing::debug!(window = id.raw(), "toplevel unmapped");

        self.arrange();
        self.refresh_focus();
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl ShmHandler for Huginn {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for Huginn {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl SeatHandler for Huginn {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for Huginn {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Huginn {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Huginn {}
impl ServerDndGrabHandler for Huginn {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl OutputHandler for Huginn {}

delegate_compositor!(Huginn);
delegate_xdg_shell!(Huginn);
delegate_shm!(Huginn);
delegate_seat!(Huginn);
delegate_data_device!(Huginn);
delegate_output!(Huginn);
