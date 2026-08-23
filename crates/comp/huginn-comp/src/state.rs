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

use huginn_core::{
    Space,
    geometry::{Rect, Size},
    layer::{Anchors, Exclusive, Margins, place, usable_area},
    window::WindowId,
};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    utils::{Logical, Point},
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    delegate_layer_shell,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        dmabuf::{DmabufGlobal, DmabufState},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::{
            wlr_layer::{
                Layer, LayerSurface, LayerSurfaceCachedState, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
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
    pub layer_shell_state: WlrLayerShellState,
    pub raven_shell: crate::shell_protocol::RavenShellState,
    pub dmabuf_state: DmabufState,
    /// `None` until a backend calls `enable_dmabuf`; the winit and udev
    /// backends discover their render node differently.
    pub dmabuf_global: Option<DmabufGlobal>,
    pub(crate) pending_dmabufs: crate::dmabuf::PendingImports,
    /// Set whenever something on screen changed. The backend renders only when
    /// this is set, so an idle desktop does no GPU work.
    needs_redraw: bool,
    pub shm_state: ShmState,
    /// Held, not read: dropping this would withdraw the xdg-output global
    /// and clients would lose their output information mid-session.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,
    /// Pointer position in compositor-global logical coordinates.
    pub pointer_location: Point<f64, Logical>,
    /// What the focused client last asked the cursor to look like.
    pub cursor_status: CursorImageStatus,

    /// Window-management policy. The only thing that decides geometry.
    pub space: Space,
    /// Wayland surface for each window the core knows about.
    toplevels: HashMap<WindowId, ToplevelSurface>,

    /// Panels, docks, wallpapers and overlays, with the geometry last sent to
    /// each. Storing what was sent is what keeps [`Huginn::refresh_layers`]
    /// from configuring on every commit and driving clients into a redraw loop.
    layers: Vec<(LayerSurface, Rect)>,
    /// The whole output, before exclusive zones are subtracted. `space`'s area
    /// is this minus whatever the panels have claimed.
    output_area: Rect,
}

impl Huginn {
    pub(crate) fn new(dh: &DisplayHandle, area: Rect) -> Self {
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "huginn");
        // Advertised for every backend. A seat with no pointer capability makes
        // toolkits that expect a cursor misbehave, and silently breaks anything
        // that relies on clicking — including Muninn's workspace pips.
        seat.add_pointer();

        Self {
            compositor_state: CompositorState::new::<Self>(dh),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            layer_shell_state: WlrLayerShellState::new::<Self>(dh),
            raven_shell: crate::shell_protocol::RavenShellState::new(dh),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            pending_dmabufs: crate::dmabuf::PendingImports::default(),
            // The first frame has to happen unprompted: nothing has committed
            // yet, so nothing would otherwise ask for it.
            needs_redraw: true,
            shm_state: ShmState::new::<Self>(dh, Vec::new()),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            seat,
            pointer_location: (0.0, 0.0).into(),
            // Default until a client sets its own on pointer enter.
            cursor_status: CursorImageStatus::default_named(),
            space: Space::new(area),
            toplevels: HashMap::new(),
            layers: Vec::new(),
            output_area: area,
        }
    }

    /// Set the full output rectangle and reflow everything beneath it.
    pub(crate) fn set_output_area(&mut self, area: Rect) {
        self.output_area = area;
        self.refresh_layers();
    }

    /// Reposition every layer surface, recompute the area left for windows, and
    /// configure whatever changed.
    ///
    /// Called on every commit, so it must be cheap and must not send a
    /// configure unless geometry actually moved — a client that receives a
    /// configure it did not need will redraw, commit, and land right back here.
    pub(crate) fn refresh_layers(&mut self) {
        let output = self.output_area;
        let mut zones = Vec::new();
        let mut updates = Vec::new();

        for (index, (surface, last)) in self.layers.iter().enumerate() {
            let Some(state) = layer_state(surface) else {
                continue;
            };
            let rect = place(output, state.anchors, state.desired, state.margins);

            // A negative exclusive zone means "ignore other panels and use the
            // whole output"; only a positive one reserves space.
            if state.exclusive > 0
                && let Some(edge) = state.anchors.exclusive_edge()
            {
                zones.push(Exclusive { edge, size: state.exclusive });
            }

            if rect != *last {
                updates.push((index, rect));
            }
        }

        for (index, rect) in updates {
            let (surface, last) = &mut self.layers[index];
            *last = rect;
            surface.with_pending_state(|pending| {
                pending.size = Some((rect.w(), rect.h()).into());
            });
            surface.send_configure();
        }

        let usable = usable_area(output, &zones);
        if usable != self.space.area() {
            tracing::debug!(?usable, panels = zones.len(), "usable area changed");
            self.space.set_area(usable);
        }
        self.arrange();
    }

    /// Layer surfaces on `layer`, with their geometry, for rendering.
    pub(crate) fn layers_on(&self, layer: Layer) -> Vec<(&LayerSurface, Rect)> {
        self.layers
            .iter()
            .filter(|(surface, _)| layer_state(surface).is_some_and(|s| s.layer == layer))
            .map(|(surface, rect)| (surface, *rect))
            .collect()
    }

    /// The pointer handle. Always present: the seat is built with one.
    pub(crate) fn pointer(&self) -> PointerHandle<Self> {
        self.seat
            .get_pointer()
            .expect("the seat is constructed with a pointer")
    }

    /// The full output rectangle, before panels reserve any of it.
    pub(crate) fn output_area(&self) -> Rect {
        self.output_area
    }

    /// Every surface to paint, front to back.
    ///
    /// This is the single definition of stacking order, shared by rendering and
    /// by pointer hit testing. Keeping them on one function is what guarantees
    /// you can always click the thing you can see: if they were computed
    /// separately they would eventually disagree, and the bug would look like
    /// clicks landing on windows hidden behind a panel.
    pub(crate) fn scene(&self) -> Vec<(&WlSurface, Rect)> {
        let mut out = Vec::new();
        for layer in [Layer::Overlay, Layer::Top] {
            out.extend(self.layers_on(layer).into_iter().map(|(l, r)| (l.wl_surface(), r)));
        }
        out.extend(self.render_list().into_iter().map(|(w, r)| (w.wl_surface(), r)));
        for layer in [Layer::Bottom, Layer::Background] {
            out.extend(self.layers_on(layer).into_iter().map(|(l, r)| (l.wl_surface(), r)));
        }
        out
    }

    /// The terminal to launch for the spawn binding.
    ///
    /// Config decides; the environment variable exists so a single session can
    /// be started with something else without editing the config file.
    pub(crate) fn terminal_command(&self) -> String {
        std::env::var("HUGINN_TERMINAL")
            .unwrap_or_else(|_| raven_config::Config::default().commands.terminal)
    }

    /// Ask the backend to draw a frame.
    pub(crate) fn queue_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Consume the redraw request, if there is one.
    pub(crate) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
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
            tracing::debug!(window = id.raw(), ?rect, "configure");
            surface.with_pending_state(|state| {
                state.size = Some((rect.w(), rect.h()).into());
            });
            surface.send_configure();
        }

        // Every state change that can alter workspace occupancy or the active
        // index runs through arrange, so this is the one place the shell needs
        // to be told. broadcast_workspaces suppresses no-op events itself.
        self.broadcast_workspaces();
        self.queue_redraw();
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
        self.queue_redraw();

        // A layer surface sends its anchor, size and exclusive zone before its
        // first commit and expects a configure in response. Doing this on every
        // commit covers both that first configure and any later change, without
        // needing to track which is which.
        if self
            .layers
            .iter()
            .any(|(l, _)| l.wl_surface() == surface)
        {
            self.refresh_layers();
        }
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.queue_redraw();
    }
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

impl WlrLayerShellHandler for Huginn {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        tracing::debug!(%namespace, ?layer, "layer surface created");
        // Only record it. Do NOT configure yet: the client sets its anchor,
        // size and exclusive zone *after* creating the surface and before its
        // first commit, so configuring here would compute geometry from empty
        // state and send a bogus size the client has to correct. The commit
        // hook sends the real initial configure a moment later.
        //
        // Rect::ZERO as the "last sent" geometry guarantees that first
        // refresh_layers sees a change and does configure.
        self.layers.push((surface, Rect::ZERO));
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        self.layers.retain(|(l, _)| l != &surface);
        tracing::debug!(remaining = self.layers.len(), "layer surface destroyed");
        self.refresh_layers();
    }
}

impl OutputHandler for Huginn {}

delegate_compositor!(Huginn);
delegate_xdg_shell!(Huginn);
delegate_shm!(Huginn);
delegate_seat!(Huginn);
delegate_data_device!(Huginn);
delegate_output!(Huginn);
delegate_layer_shell!(Huginn);

/// What a layer surface has asked for, translated out of Wayland vocabulary and
/// into the plain geometry types `huginn-core` works in.
struct LayerRequest {
    anchors: Anchors,
    desired: Size,
    margins: Margins,
    exclusive: i32,
    layer: Layer,
}

/// Read a layer surface's committed state.
///
/// Returns `None` before the client's first commit, when there is nothing to
/// place yet.
fn layer_state(surface: &LayerSurface) -> Option<LayerRequest> {
    use smithay::wayland::shell::wlr_layer::{Anchor, ExclusiveZone};

    if !surface.alive() {
        return None;
    }
    let state = with_states(surface.wl_surface(), |states| {
        *states.cached_state.get::<LayerSurfaceCachedState>().current()
    });

    Some(LayerRequest {
        anchors: Anchors {
            top: state.anchor.contains(Anchor::TOP),
            bottom: state.anchor.contains(Anchor::BOTTOM),
            left: state.anchor.contains(Anchor::LEFT),
            right: state.anchor.contains(Anchor::RIGHT),
        },
        desired: Size::new(state.size.w, state.size.h),
        margins: Margins {
            top: state.margin.top,
            right: state.margin.right,
            bottom: state.margin.bottom,
            left: state.margin.left,
        },
        exclusive: match state.exclusive_zone {
            ExclusiveZone::Exclusive(n) => n as i32,
            ExclusiveZone::Neutral | ExclusiveZone::DontCare => 0,
        },
        layer: state.layer,
    })
}
