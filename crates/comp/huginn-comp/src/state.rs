//! Compositor state and Wayland protocol handlers.
//!
//! This module is the seam between the two halves of the compositor. The
//! Wayland side lives here — globals, handler traits, surfaces. The policy side
//! lives in `huginn-core` and knows nothing about any of it.
//!
//! The translation is deliberately narrow: [`Huginn::arrange`] asks the core
//! where windows go and turns the answer into `xdg_toplevel::configure` events.
//! Nothing else in this file makes a layout decision.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::OwnedFd;

use crate::window::WindowSurface;
use smithay::{
    delegate_xwayland_shell,
    wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    xwayland::{X11Surface, X11Wm, XWaylandClientData},
};
use huginn_core::{
    Space,
    scale::OutputScale,
    tiles::Axis,
    geometry::{Dir, Rect, Size},
    layer::{
        Anchors, Exclusive, Focusable, Interactivity, KeyboardFocus, Level, Margins,
        keyboard_focus, place, usable_area,
    },
    window::{WindowId, WindowMode},
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBuffer, solid::SolidColorBuffer},
        utils::{on_commit_buffer_handler, with_renderer_surface_state},
    },
    desktop::{PopupKind, PopupManager},
    utils::{Logical, Point},
    delegate_compositor, delegate_data_device, delegate_fractional_scale, delegate_output,
    delegate_seat, delegate_shm, delegate_viewporter,
    delegate_xdg_shell,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::LedState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    delegate_layer_shell,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason, GlobalId},
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale},
        viewporter::ViewporterState,
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::{
            wlr_layer::{
                Layer, LayerSurface, LayerSurfaceCachedState, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            },
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

/// One thing to paint, as [`Huginn::scene`] orders them.
pub(crate) enum SceneItem<'a> {
    /// A client surface: window, panel, wallpaper, popup.
    ///
    /// Owned rather than borrowed because a popup is reached through the
    /// compositor's popup tree, which hands back values rather than references
    /// into anything the compositor holds. A `WlSurface` is a handle, so the
    /// clone costs a refcount.
    Surface(WlSurface, Rect),
    /// One edge of the ring around the focused window. Drawn by the compositor
    /// itself, so unlike a surface there is no client to click on or to send a
    /// frame callback to.
    Ring(&'a SolidColorBuffer, Rect),
    /// The keybinding overlay. Compositor-drawn like the ring, so it too is
    /// invisible to hit testing — clicks fall through it to whatever is
    /// underneath, which is right for something that is a label rather than a
    /// window.
    Overlay(&'a MemoryRenderBuffer, Rect, f32),
}

/// Applications that drive the `Super` layer themselves.
///
/// `Super`+`C` in RavenTerminal is already copy — `Super` is its leader, the
/// way `Cmd` is on macOS. Translating for it would replace a working copy with
/// `Ctrl`+`C`, which a terminal reads as SIGINT and delivers to whatever is
/// running. Getting this wrong kills someone's job instead of copying a line,
/// so an application that names itself and is not on this list gets the
/// translation, and one that gives no name at all does not: a client with no
/// `app_id` cannot be told apart from a terminal with no `app_id`.
const SUPER_IS_THEIRS: &[&str] = &[
    "raven-terminal",
    "Alacritty",
    "com.mitchellh.ghostty",
    "foot",
    "kitty",
    "org.wezfurlong.wezterm",
];


/// Whether `surface` currently holds a buffer.
///
/// The one question that decides whether a window is worth drawing. A surface
/// exists, gets configured, and is laid out well before it has any content, and
/// the interval is long — a Chromium-based application spends hundreds of
/// milliseconds between its first commit and its first real frame. Anything
/// drawn in that window is not the application.
pub(crate) fn has_buffer(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
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
    /// `wp_viewporter`. Held, not read: dropping it withdraws the global.
    ///
    /// Required by the integer-scale contract rather than optional polish. A
    /// client told scale 2 on a panel whose logical size is not a clean half
    /// needs a viewport to say "this 2x buffer covers exactly this logical
    /// rectangle"; without one it can only describe whole buffer scales, and
    /// the odd row at the edge has nowhere to go.
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    /// `wp_fractional_scale_v1`, which this compositor answers with whole
    /// numbers — see [`Huginn::new_fractional_scale`]. Held, not read:
    /// dropping it withdraws the global and clients lose the answer.
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
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
    /// Keyboards whose LEDs the compositor drives, and what they should show.
    ///
    /// In a session the kernel stops driving caps lock — the VT is in raw
    /// mode, key events go to evdev, and the LED follows the compositor's xkb
    /// state or nobody's. Only the udev backend ever adds devices here: nested
    /// under another compositor the host owns the LEDs, exactly as it owns the
    /// keymap.
    ///
    /// This is also the one piece of the input stack observable with a dead
    /// display. A caps lock LED that toggles proves keyboard → libinput →
    /// compositor end to end from the far side of a black screen, which is
    /// worth having on a distribution whose graphics bring-up is younger than
    /// its input stack.
    pub(crate) keyboard_led_devices: Vec<smithay::reexports::input::Device>,
    /// The state those LEDs were last told to show. Kept so a keyboard that
    /// appears mid-session — or reappears after a VT switch — is set to the
    /// session's state rather than left showing whatever the console left on.
    pub(crate) keyboard_led_state: LedState,

    /// Window-management policy. The only thing that decides geometry.
    pub space: Space,
    /// The surface behind each window the core knows about — an `xdg_toplevel`
    /// for a Wayland client, an `X11Surface` for one arriving through XWayland.
    /// The layout core deals only in [`WindowId`] and never learns the
    /// difference; see [`crate::window::WindowSurface`] for where the two
    /// protocols actually diverge.
    windows: HashMap<WindowId, WindowSurface>,

    /// The X11 window manager, once XWayland has signalled ready. `None` when
    /// XWayland is not installed, has not come up yet, or failed — all three of
    /// which are states the compositor runs in perfectly well, just without
    /// X11 clients.
    pub(crate) xwm: Option<X11Wm>,
    /// `xwayland_shell_v1`, the protocol XWayland uses to associate an X11
    /// window with the Wayland surface it created for it. Held, not read:
    /// dropping it withdraws the global and XWayland can no longer associate
    /// anything, which presents as X11 windows that map and never draw.
    pub(crate) xwayland_shell_state: XWaylandShellState,
    /// Override-redirect X11 windows: menus, tooltips, drag icons.
    ///
    /// Outside the layout by definition — override-redirect *means* the window
    /// manager must not position them. Kept in creation order, which is the
    /// order they stack.
    pub(crate) x11_unmanaged: Vec<X11Surface>,
    /// The display number XWayland came up on, once it has.
    ///
    /// Recorded rather than exported to the environment: it is only ever needed
    /// by children, and it reaches them the same way `WAYLAND_DISPLAY` does —
    /// injected in `backend::spawn`. `std::env::set_var` would be a
    /// process-global write, and unsafe in edition 2024.
    pub(crate) x11_display: Option<u32>,

    /// Windows that have committed a buffer, and may therefore be drawn.
    ///
    /// A window is in the layout from the moment it is created — it has to be,
    /// or the first `configure` could not carry the geometry it will actually
    /// occupy — but it is not on screen until it has produced content. Between
    /// those two moments its tile is reserved and empty.
    ///
    /// That gap is the whole point. Drawing a surface that has committed no
    /// buffer, or one whose only buffer was painted at a size it has since been
    /// told to change, is what shows a frame of white or garbage when a
    /// Chromium-based application starts. See the design spec, §5.
    mapped: HashSet<WindowId>,

    /// Menus, dropdowns and tooltips, and the modal grabs that dismiss them.
    ///
    /// Deliberately outside `space`: a popup is not a window, has no place in
    /// a workspace, and is positioned by the client's own rules rather than by
    /// any layout the core runs.
    pub popups: PopupManager,

    /// Panels, docks, wallpapers and overlays, with the geometry last sent to
    /// each. Storing what was sent is what keeps [`Huginn::refresh_layers`]
    /// from configuring on every commit and driving clients into a redraw loop.
    layers: Vec<(LayerSurface, Rect)>,
    /// Where the carousel is scrolled to, while it is getting there.
    ///
    /// The compositor's half of the layout: `huginn-core` works out where the
    /// strip belongs and has no clock to move it with, so the sliding lives
    /// here and the offset is handed back down each frame.
    carousel_scroll: crate::anim::Animated,
    /// Which workspace [`Self::carousel_scroll`] is currently sliding for.
    ///
    /// Scroll position belongs to the workspace, so a change here means the
    /// animation has nothing to continue from and should snap rather than
    /// slide. `None` while no carousel is active.
    carousel_on: Option<huginn_core::workspace::WorkspaceId>,
    /// The layer surface the pointer last clicked into, if it wanted the
    /// keyboard at all.
    ///
    /// Held as the surface rather than an index because [`Self::layers`]
    /// shifts as panels come and go. It is allowed to name something that has
    /// since disappeared: [`Self::resolve_keyboard_focus`] drops a stale click
    /// on the floor rather than checking for one on every commit.
    focused_layer: Option<WlSurface>,
    /// Where the keyboard currently sits. Resolved in one place, in
    /// [`Self::refresh_focus`], and read by the `Super` rule and the ring.
    keyboard_on: KeyboardOn,
    /// The whole output, before exclusive zones are subtracted. `space`'s area
    /// is this minus whatever the panels have claimed.
    output_area: Rect,

    /// The keybinding overlay, drawn once when it is summoned rather than on
    /// every frame. `None` when it is not on screen, which is almost always.
    help: Option<crate::overlay::Overlay>,

    /// What this output renders at, and what clients are told.
    ///
    /// One output for now; this becomes per-output alongside `output_area`.
    pub(crate) scale: OutputScale,

    /// Whether the arrows are currently resizing the focused window.
    ///
    /// A mode rather than a chord because resizing is done by feel: you press
    /// an arrow several times and watch. `Super`+`Shift`+arrows is already
    /// taken by moving a window between tiles, and overloading it would make
    /// the two indistinguishable.
    pub(crate) resizing: bool,

    /// The Wayland socket children are told to connect to.
    ///
    /// Set by the backend once it has bound one — `Huginn` is built before the
    /// socket exists, so this cannot be a constructor argument. Empty until
    /// then, which only matters if something tries to spawn during startup.
    socket: String,

    /// The dock, and its rendered strip.
    pub(crate) dock: crate::dock::Dock,
    dock_panel: Option<crate::canvas::Panel>,
    /// The items the strip currently holds, so a click can be resolved against
    /// the same list that was drawn.
    dock_items: Vec<crate::dock::Item>,

    /// Quick settings, and its rendered panel.
    pub(crate) settings: crate::settings::Settings,
    settings_panel: Option<crate::canvas::Panel>,
    /// When the compositor started, so animations have a monotonic origin.
    started: std::time::Instant,

    /// The application launcher, and the index it searches.
    pub(crate) launcher: crate::launcher::Launcher,
    /// The launcher's pixels, redrawn when its state changes rather than every
    /// frame — a search field repainted at the refresh rate is how one ends up
    /// feeling slower than the typing that drives it.
    launcher_panel: Option<crate::canvas::Panel>,
    /// Installed applications, scanned once at startup.
    pub(crate) apps: Vec<raven_desktop::Entry>,
    /// How often each application has been launched.
    pub(crate) frecency: raven_desktop::Frecency,
    /// The icon theme, indexed once at startup — walking every theme's
    /// `index.theme` costs on the order of a hundred milliseconds.
    icons: raven_desktop::Icons,
    /// Icons already rasterized, kept between draws.
    pixmaps: raven_desktop::Pixmaps,

    /// The font stack. Built once at startup — loading fonts takes on the
    /// order of a hundred milliseconds — and borrowed by anything that draws.
    pub(crate) text: crate::text::Text,

    /// Kept so focus changes can resolve a surface back to the client that
    /// owns it, which is what the clipboard needs and nothing else does.
    display: DisplayHandle,

    /// The four edges of the focus ring, and where they were last put.
    ///
    /// The buffers are kept rather than rebuilt each frame because a buffer
    /// carries the identity the damage tracker recognises it by. Fresh ones
    /// every frame would look like four new elements appearing, so no frame
    /// could ever be reported as unchanged and every redraw would cost a page
    /// flip. `None` means nothing is ringed: no window is focused, or the
    /// focused one is fullscreen.
    focus_ring: [SolidColorBuffer; 4],
    focus_ring_at: Option<[Rect; 4]>,
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
            viewporter_state: ViewporterState::new::<Self>(dh),
            fractional_scale_state: FractionalScaleManagerState::new::<Self>(dh),
            scale: OutputScale::for_output(
                Size::new(area.w(), area.h()),
                Size::new(0, 0),
            ),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            seat,
            pointer_location: (0.0, 0.0).into(),
            // Default until a client sets its own on pointer enter.
            cursor_status: CursorImageStatus::default_named(),
            keyboard_led_devices: Vec::new(),
            keyboard_led_state: LedState::default(),
            space: {
                let mut space = Space::new(area);
                space.set_gap(crate::theme::GAP);
                space.set_carousel_columns(crate::theme::CAROUSEL_COLUMNS);
                space
            },
            windows: HashMap::new(),
            xwm: None,
            xwayland_shell_state: XWaylandShellState::new::<Self>(dh),
            x11_unmanaged: Vec::new(),
            x11_display: None,
            mapped: HashSet::new(),
            popups: PopupManager::default(),
            layers: Vec::new(),
            carousel_scroll: crate::anim::Animated::settled(0.0),
            carousel_on: None,
            focused_layer: None,
            keyboard_on: KeyboardOn::default(),
            output_area: area,
            focus_ring: std::array::from_fn(|_| {
                SolidColorBuffer::new((0, 0), crate::theme::ACCENT.to_rgba_f32())
            }),
            focus_ring_at: None,
            help: None,
            resizing: false,
            socket: String::new(),
            dock: crate::dock::Dock::default(),
            dock_panel: None,
            dock_items: Vec::new(),
            settings: crate::settings::Settings::default(),
            settings_panel: None,
            started: std::time::Instant::now(),
            launcher: crate::launcher::Launcher::default(),
            launcher_panel: None,
            apps: crate::launcher::scan_applications(),
            frecency: raven_desktop::Frecency::default(),
            icons: raven_desktop::Icons::discover(crate::theme::ICON_THEME),
            pixmaps: raven_desktop::Pixmaps::new(),
            text: crate::text::Text::new(),
            display: dh.clone(),
        }
    }

    /// Show or hide the keybinding overlay.
    ///
    /// Rendered on the way in rather than kept around: it is a few hundred
    /// kilobytes that spend the whole session unlooked at, and the table it is
    /// drawn from cannot change while the compositor runs, so there is nothing
    /// to gain by holding it.
    pub(crate) fn toggle_help(&mut self) {
        self.help = match self.help {
            Some(_) => None,
            None => {
                let area = self.output_area;
                Some(crate::overlay::Overlay::render(area, &mut self.text, self.scale.advertised))
            }
        };
        tracing::debug!(visible = self.help.is_some(), "keybinding overlay");
        self.queue_redraw();
    }

    /// Set the full output rectangle and reflow everything beneath it.
    pub(crate) fn set_output_area(&mut self, area: Rect) {
        self.output_area = area;
        // The overlay picks its scale from the output height, so a resize with
        // it open has to redraw it rather than just re-centre it.
        if self.help.is_some() {
            self.help = Some(crate::overlay::Overlay::render(area, &mut self.text, self.scale.advertised));
        }
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
            //
            // And only a surface with something on screen reserves it. A layer
            // surface declares its zone *before* its first commit, and may unmap
            // and stay alive afterwards; honouring either would carve a strip
            // out of the desktop that nothing is drawn in — windows tiled around
            // a gap with no panel in it. The configure below stays
            // unconditional, because a client cannot attach its first buffer
            // until it has been told what size to draw.
            if state.exclusive > 0
                && has_buffer(surface.wl_surface())
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

        // A panel that maps with `exclusive` interactivity takes the keyboard
        // by existing, and one that goes away gives it back, so focus has to be
        // re-resolved wherever the set of layer surfaces can change. Cheap to
        // do here despite running on every commit: smithay's `set_focus`
        // compares against the focus it already holds and sends nothing when
        // they agree, the same property `set_keyboard_focus` already relies on
        // for the clipboard.
        self.refresh_focus();
    }

    /// Every layer surface with the geometry last sent to it, in no particular
    /// order. Used to find a popup's root, which cares which surface it is
    /// rather than which layer it sits on.
    pub(crate) fn all_layers(&self) -> Vec<(&LayerSurface, Rect)> {
        self.layers.iter().map(|(surface, rect)| (surface, *rect)).collect()
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

    /// Everything to paint, front to back.
    ///
    /// This is the single definition of stacking order, shared by rendering and
    /// by pointer hit testing. Keeping them on one function is what guarantees
    /// you can always click the thing you can see: if they were computed
    /// separately they would eventually disagree, and the bug would look like
    /// clicks landing on windows hidden behind a panel.
    pub(crate) fn scene(&self) -> Vec<SceneItem<'_>> {
        let mut out = Vec::new();
        // Front to back, and the order is the whole point:
        //
        //   help overlay   - summoned by someone who has lost track of what is
        //                    on screen, which includes the panels
        //   launcher       - over the dock it grew out of
        //   settings       - likewise
        //   ---- blur boundary ----
        //   dock           - part of the desktop, so it blurs with it
        //   layer surfaces
        //   windows
        //
        // Everything below the boundary is what a panel blurs. See
        // [`Huginn::blur_boundary`], which counts the items above it.
        if let Some(help) = &self.help {
            out.push(SceneItem::Overlay(help.buffer(), help.placement(self.output_area), 1.0));
        }
        if let Some(panel) = &self.launcher_panel {
            let clock = self.uptime();
            let reveal = self.launcher.reveal(clock);
            out.push(SceneItem::Overlay(
                panel.buffer(),
                crate::launcher::placement(
                    self.output_area,
                    panel.size(),
                    self.launcher.origin(),
                    reveal,
                ),
                reveal.clamp(0.0, 1.0),
            ));
        }
        if let Some(panel) = &self.settings_panel {
            out.push(SceneItem::Overlay(
                panel.buffer(),
                panel.centred_on(self.output_area),
                self.settings.reveal(self.uptime()).clamp(0.0, 1.0),
            ));
        }
        // --- blur boundary: everything below here is what a panel blurs ---
        if let Some(panel) = &self.dock_panel
            && let Some(rect) = self.dock_rect()
        {
            out.push(SceneItem::Overlay(panel.buffer(), rect, 1.0));
        }
        // A panel's own menu belongs directly on top of the panel, not on top
        // of everything, so each layer surface carries its popups with it.
        for layer in [Layer::Overlay, Layer::Top] {
            for (surface, rect) in self.layers_on(layer) {
                out.extend(self.popups_of(surface.wl_surface(), rect));
                out.push(SceneItem::Surface(surface.wl_surface().clone(), rect));
            }
        }
        // Override-redirect X11 windows: menus, tooltips, drag icons. They sit
        // with the Wayland popups rather than with the windows, because that is
        // what they are — the X11 spelling of the same thing. Drawn at the
        // coordinates the client chose, since override-redirect means exactly
        // that the window manager does not get to place them.
        //
        // Skips any whose surface XWayland has not associated yet, on the same
        // reasoning as an unmapped window: it exists but has nothing to show.
        for window in &self.x11_unmanaged {
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            let geo = window.geometry();
            let rect = Rect::from_xywh(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
            out.extend(self.popups_of(&surface, rect));
            out.push(SceneItem::Surface(surface, rect));
        }
        // Window popups go above every window and above the ring. A menu is
        // routinely taller than the button that opened it and reaches over the
        // neighbouring tile; drawing it under that tile, or letting the ring
        // draw a line across it, is worse than the alternative of a menu
        // covering a window it does not belong to — which is what a menu is
        // supposed to do.
        for (surface, rect) in self.render_list() {
            out.extend(self.popups_of(&surface, rect));
        }
        // Directly above the windows: over its neighbours, whose pixels it
        // reaches into when the layout leaves no gap, but under a panel or an
        // overlay, which are entitled to cover the desktop.
        out.extend(self.focus_ring().into_iter().map(|(b, r)| SceneItem::Ring(b, r)));
        out.extend(
            self.render_list()
                .into_iter()
                .map(|(surface, r)| SceneItem::Surface(surface, r)),
        );
        for layer in [Layer::Bottom, Layer::Background] {
            for (surface, rect) in self.layers_on(layer) {
                out.extend(self.popups_of(surface.wl_surface(), rect));
                out.push(SceneItem::Surface(surface.wl_surface().clone(), rect));
            }
        }
        out
    }

    /// The surfaces of [`Self::scene`], in the same order.
    ///
    /// Hit testing and frame callbacks both want a client on the other end, and
    /// the focus ring has none.
    pub(crate) fn scene_surfaces(&self) -> impl Iterator<Item = (WlSurface, Rect)> {
        self.scene().into_iter().filter_map(|item| match item {
            SceneItem::Surface(surface, rect) => Some((surface, rect)),
            SceneItem::Ring(..) | SceneItem::Overlay(..) => None,
        })
    }

    /// Every surface that should be told it may draw.
    ///
    /// Deliberately wider than [`Self::scene_surfaces`]: it includes windows
    /// that have not committed a buffer yet. A frame callback is permission to
    /// paint, and the surfaces most in need of that permission are exactly the
    /// ones with nothing on screen. Withholding it from a client that waits on
    /// a callback before its first paint deadlocks it against a compositor
    /// waiting for the buffer that callback would have produced.
    pub(crate) fn frame_surfaces(&self) -> Vec<(WlSurface, Rect)> {
        let mut out: Vec<(WlSurface, Rect)> = self.scene_surfaces().collect();
        out.extend(
            self.space
                .active_workspace()
                .windows()
                .iter()
                .filter(|id| !self.mapped.contains(id))
                .filter_map(|id| {
                    let surface = self.windows.get(id)?.wl_surface()?;
                    let geometry = self.space.window(*id)?.geometry;
                    Some((surface, geometry))
                }),
        );
        out
    }

    /// Whether the focused client handles the `Super` layer itself.
    ///
    /// Decides who answers `Super`+`C`. An unnamed or absent focus counts as
    /// theirs: the safe answer is to leave the keystroke alone, since the harm
    /// of translating for a terminal is worse than the harm of not translating
    /// for an application that would have liked it.
    pub(crate) fn focus_owns_super(&self) -> bool {
        // A layer surface holding the keyboard is not a terminal, and it has no
        // app_id to test against the list — a namespace is not one. Falling
        // through to the window's app_id would let the terminal behind a panel
        // decide what Super+C does while the panel is what receives the keys.
        if self.keyboard_on != KeyboardOn::Window {
            return false;
        }
        let Some(app_id) = self.focused_app_id() else {
            return true;
        };
        SUPER_IS_THEIRS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&app_id))
    }

    /// The `app_id` the focused window advertised, if it advertised one.
    fn focused_app_id(&self) -> Option<String> {
        let id = self.space.focused()?;
        self.windows.get(&id)?.app_id()
    }

    /// The terminal to launch for the spawn binding.
    pub(crate) fn terminal_command(&self) -> &'static str {
        crate::theme::TERMINAL
    }

    /// Adopt the scale a newly-configured output decided on.
    ///
    /// The desktop is laid out in *logical* pixels, so this sets the window
    /// area from `scale.logical` rather than from the panel's real resolution:
    /// on a 4K 27" that is a 2560x1440 desktop over a 3840x2160 panel, which is
    /// what makes windows come out a sensible size instead of tiny.
    pub(crate) fn set_output_scale(&mut self, scale: OutputScale) {
        self.scale = scale;
        self.set_output_area(Rect::from_xywh(0, 0, scale.logical.w, scale.logical.h));
        tracing::info!(
            advertised = scale.advertised,
            logical = %format!("{}x{}", scale.logical.w, scale.logical.h),
            render = %format!("{}x{}", scale.render.w, scale.render.h),
            physical = %format!("{}x{}", scale.physical.w, scale.physical.h),
            resample = scale.needs_resample(),
            "output scale"
        );
    }

    /// Monotonic time since the compositor started.
    ///
    /// Monotonic rather than wall-clock: an NTP step mid-animation would
    /// otherwise jump a panel to its end or run it backwards.
    pub(crate) fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// The pointer, in the core's coordinate type.
    fn pointer_point(&self) -> huginn_core::geometry::Point {
        huginn_core::geometry::Point::new(
            self.pointer_location.x.round() as i32,
            self.pointer_location.y.round() as i32,
        )
    }

    /// The `app_id` of every open window on the active workspace.
    pub(crate) fn running_app_ids(&self) -> Vec<String> {
        self.space
            .active_workspace()
            .windows()
            .iter()
            .filter_map(|id| self.windows.get(id)?.app_id())
            .collect()
    }

    /// Whether a window is covering the whole output.
    ///
    /// §4: the dock must never overlap a fullscreen window. Animating out over
    /// one is still overlapping it, so this suppresses the dock outright
    /// rather than asking it to leave.
    fn has_fullscreen(&self) -> bool {
        self.space
            .active_workspace()
            .windows()
            .iter()
            .filter_map(|id| self.space.window(*id))
            .any(|w| w.mode == WindowMode::Fullscreen)
    }

    /// Where the dock is right now, if it is on screen at all.
    pub(crate) fn dock_rect(&self) -> Option<Rect> {
        let now = self.uptime();
        if self.has_fullscreen() || !self.dock.is_visible(now) {
            return None;
        }
        Some(crate::dock::placement(
            self.output_area,
            self.dock_items.len().max(1),
            self.dock.reveal(now),
        ))
    }

    /// Rebuild the dock strip from what is running.
    pub(crate) fn refresh_dock(&mut self) {
        if self.has_fullscreen() {
            self.dock.hide_now();
            self.dock_panel = None;
            self.queue_redraw();
            return;
        }
        let running = self.running_app_ids();
        self.dock_items = crate::dock::items(&self.apps, &running);
        self.dock_panel = self.dock.is_visible(self.uptime()).then(|| {
            crate::dock::render(
                &self.dock_items,
                &self.apps,
                &self.icons,
                &mut self.pixmaps,
                &mut self.text,
                self.output_area,
                self.scale.advertised,
            )
        });
        self.queue_redraw();
    }

    /// Tell the dock where the pointer went.
    pub(crate) fn dock_pointer_moved(&mut self) {
        let now = self.uptime();
        let over_dock = self
            .dock_rect()
            .is_some_and(|rect| rect.contains(self.pointer_point()));
        let motion = self.settings.motion();
        let y = self.pointer_location.y.round() as i32;
        if self.dock.pointer_moved(y, self.output_area, over_dock, now, motion) {
            self.refresh_dock();
        }
    }

    /// A click landed. Returns the item hit, if any.
    pub(crate) fn dock_click(&self) -> Option<crate::dock::Item> {
        let rect = self.dock_rect()?;
        let point = self.pointer_point();
        if !rect.contains(point) {
            return None;
        }
        let index = self.dock.item_at(point.x, rect, self.dock_items.len())?;
        self.dock_items.get(index).cloned()
    }

    /// Tell the compositor which socket to hand to children.
    pub(crate) fn set_socket(&mut self, socket: String) {
        self.socket = socket;
    }

    /// Run an application, and remember that it was run.
    ///
    /// One place rather than three: the dock, the launcher, and the terminal
    /// binding all start applications, and all three need the same environment
    /// and the same frecency bookkeeping. Recorded before the spawn, because
    /// what the user chose is worth remembering whether or not it starts.
    pub(crate) fn launch(&mut self, entry_path: Option<std::path::PathBuf>, argv: &[String]) {
        if let Some(path) = entry_path {
            let now = self.now();
            self.frecency.record(&path, now);
        }
        crate::backend::spawn(argv, &self.socket, self.x11_display);
    }

    /// Resize the focused window one step in `dir`.
    ///
    /// A step rather than a continuous drag: this is a keyboard, and the unit
    /// of a keyboard is a press.
    pub(crate) fn resize_focused(&mut self, dir: Dir) {
        /// How much of a tile one press moves. Small enough to aim with, large
        /// enough that a deliberate resize does not take twenty presses.
        const STEP: f32 = 0.03;

        let Some(window) = self.space.focused() else {
            return;
        };
        let (axis, delta) = match dir {
            Dir::Left => (Axis::Horizontal, -STEP),
            Dir::Right => (Axis::Horizontal, STEP),
            Dir::Up => (Axis::Vertical, -STEP),
            Dir::Down => (Axis::Vertical, STEP),
        };
        if self
            .space
            .active_workspace_mut()
            .tiles_mut()
            .resize(window, axis, delta)
        {
            self.arrange();
        }
    }

    /// Open the launcher, growing it out of the dock's launcher icon.
    ///
    /// The origin is only taken when the dock is actually on screen — §4 asks
    /// for the motion to start at the icon, and starting it off the bottom of
    /// the screen because the dock happens to be hidden is motion the eye
    /// cannot follow. Without one it grows in place from the centre.
    pub(crate) fn open_launcher(&mut self) {
        let origin = self.dock_rect().map(|dock| crate::dock::item_rect(dock, 0));
        let (now, clock, motion) = (self.now(), self.uptime(), self.settings.motion());
        self.launcher.open(&self.apps, &self.frecency, now, origin, clock, motion);
        self.refresh_launcher();
    }

    /// Apply a keystroke to the launcher, and act on what it asks for.
    pub(crate) fn launcher_key(&mut self, key: crate::launcher::Key) {
        let (now, clock, motion) = (self.now(), self.uptime(), self.settings.motion());
        let outcome = self
            .launcher
            .press(key, &self.apps, &self.frecency, now, clock, motion);
        match outcome {
            crate::launcher::Outcome::Launch(argv) => {
                let path = self
                    .apps
                    .iter()
                    .find(|e| e.argv(&[]).as_deref() == Some(argv.as_slice()))
                    .map(|e| e.path.clone());
                self.launch(path, &argv);
                self.refresh_launcher();
            }
            crate::launcher::Outcome::Dismissed | crate::launcher::Outcome::Redraw => {
                self.refresh_launcher();
            }
            crate::launcher::Outcome::Unchanged => {}
        }
    }

    /// Act on a dock item that was clicked.
    ///
    /// The launcher button opens the launcher — §4 makes it the leftmost item
    /// for exactly this. A running application is focused; one that is not
    /// running is left to the caller to spawn, since only the backend knows
    /// the socket to hand it.
    pub(crate) fn activate_dock_item(&mut self, item: &crate::dock::Item) {
        if item.is_launcher() {
            self.open_launcher();
            return;
        }
        let Some(entry) = item.entry.and_then(|i| self.apps.get(i)) else {
            return;
        };
        if !item.running {
            // Not running: start it. The launcher opens applications rather
            // than documents, so there are no targets to substitute.
            let (path, argv) = (entry.path.clone(), entry.argv(&[]));
            if let Some(argv) = argv {
                self.launch(Some(path), &argv);
            } else {
                tracing::warn!(name = %entry.name, "dock entry has nothing runnable");
            }
            return;
        }
        // Focus the first window belonging to it.
        let running: Vec<(huginn_core::window::WindowId, String)> = self
            .space
            .active_workspace()
            .windows()
            .iter()
            .filter_map(|id| Some((*id, self.windows.get(id)?.app_id()?)))
            .collect();
        if let Some((id, _)) = running
            .iter()
            .find(|(_, app_id)| crate::dock::matches(entry, app_id))
        {
            self.space.active_workspace_mut().focus(*id);
            self.refresh_focus();
        }
    }

    /// How many leading scene items sit *above* the blur.
    ///
    /// The help overlay, the launcher and quick settings, in that order — each
    /// present only when it is. Counted rather than hardcoded so the boundary
    /// cannot drift from the order [`Huginn::scene`] actually pushes in.
    pub(crate) fn blur_boundary(&self) -> usize {
        usize::from(self.help.is_some())
            + usize::from(self.launcher_panel.is_some())
            + usize::from(self.settings_panel.is_some())
    }

    /// How blurred the desktop behind the panels should be, in pixels.
    ///
    /// Zero when no panel is open, which is what lets the renderer take the
    /// ordinary path unchanged for the overwhelming majority of frames.
    pub(crate) fn blur_radius(&self) -> f32 {
        crate::blur::radius_for(self.launcher.reveal(self.uptime()))
    }

    /// Advance anything that is animating, and ask for another frame if it is
    /// still moving.
    ///
    /// Called once per rendered frame. Without it a panel would compose at
    /// whatever its reveal happened to be when a key was pressed and then hold
    /// there, because nothing else asks for a redraw between keystrokes — the
    /// animation would exist and never be seen.
    ///
    /// The converse matters as much: when nothing is animating this asks for
    /// nothing, so an idle desktop still renders no frames.
    pub(crate) fn tick_animations(&mut self) {
        let now = self.uptime();
        if !self.carousel_scroll.is_settled(now) {
            // Re-arranging is what moves the panes; the redraw is what shows it.
            // `arrange` reads the offset back out of `carousel_scroll`, so this
            // is the whole of the animation.
            self.arrange();
            self.queue_redraw();
        }
        if self.settings.is_animating(now) {
            self.refresh_settings();
        }
        if self.dock.is_animating(now) {
            self.refresh_dock();
        }
        if self.launcher.is_animating(now) {
            // Position and alpha come from the reveal at draw time, so the
            // panel itself does not need recomposing — but a frame still has
            // to be asked for, or the motion happens with nothing drawing it.
            self.queue_redraw();
        }
    }

    /// Redraw the quick settings panel, or drop it once it has closed.
    ///
    /// Kept while the close animation is still running: dropping the panel the
    /// moment it is dismissed would make it vanish rather than leave.
    pub(crate) fn refresh_settings(&mut self) {
        let now = self.uptime();
        self.settings_panel = self.settings.is_visible(now).then(|| {
            crate::settings::render(
                &self.settings,
                &mut self.text,
                self.output_area,
                now,
                self.scale.advertised,
            )
        });
        self.queue_redraw();
    }

    /// Re-read the installed applications and rebuild what shows them.
    ///
    /// Driven by [`crate::appwatch`] when an application directory changes.
    /// Cheap enough to be worth doing unconditionally — a few dozen small
    /// files — but the early return still matters: a rescan that found nothing
    /// new would otherwise rebuild the dock and relayout the launcher under
    /// the user's hands every time anything at all touched `/usr/share`.
    pub(crate) fn reload_applications(&mut self) {
        let apps = crate::launcher::scan_applications();
        if apps == self.apps {
            tracing::debug!("application directories changed, list did not");
            return;
        }
        tracing::info!(
            was = self.apps.len(),
            now = apps.len(),
            "application list changed"
        );
        self.apps = apps;

        // Order matters. The launcher holds indices into the list that just
        // moved under it, so it has to be re-ranked before anything renders
        // from it. Only when it is open: re-ranking a closed launcher would
        // discard the selection a reopen is about to reset anyway.
        if self.launcher.is_open() {
            let now = self.now();
            self.launcher.reindex(&self.apps, &self.frecency, now);
        }
        self.refresh_launcher();

        // The dock reads the same list, and a removed application must stop
        // being clickable rather than launch whatever took its index.
        self.refresh_dock();
    }

    /// Redraw the launcher's panel, or drop it when it is closed.
    ///
    /// Called whenever the launcher's state changes rather than on every
    /// frame: composing it walks the result list and shapes a string per row,
    /// which is cheap once per keystroke and wasteful sixty times a second.
    pub(crate) fn refresh_launcher(&mut self) {
        self.launcher_panel = self.launcher.is_visible(self.uptime()).then(|| {
            crate::launcher::render(
                &self.launcher,
                &self.apps,
                &mut self.text,
                &self.icons,
                &mut self.pixmaps,
                self.output_area,
                self.scale.advertised,
            )
        });
        self.queue_redraw();
    }

    /// Seconds since the epoch, for frecency.
    ///
    /// A clock that cannot be read is not a reason to refuse to open the
    /// launcher; zero simply means every application looks equally stale, and
    /// the match quality still orders them.
    pub(crate) fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Ask the backend to draw a frame.
    ///
    /// The focus ring is recomputed here rather than at each of the places that
    /// can move it. Anything that moves the ring — a focus change, a relayout,
    /// a window going fullscreen — has to ask for a redraw or the change would
    /// never reach the screen anyway, which makes this the one hook that cannot
    /// be forgotten.
    pub(crate) fn queue_redraw(&mut self) {
        self.needs_redraw = true;
        self.update_focus_ring();
    }

    /// Point the ring's four buffers at the focused window, or retire them.
    fn update_focus_ring(&mut self) {
        let rects = self.focus_ring_rects();
        for (buffer, rect) in self.focus_ring.iter_mut().zip(rects.unwrap_or_default()) {
            buffer.resize((rect.w(), rect.h()));
        }
        self.focus_ring_at = rects;
    }

    /// Where the focus ring goes, or `None` when there should not be one.
    fn focus_ring_rects(&self) -> Option<[Rect; 4]> {
        // A panel holding the keyboard outright is a modal takeover: every
        // keystroke goes to it, and a ring still drawn round the window would be
        // claiming otherwise. An on-demand panel is deliberately not included —
        // it gives the keyboard back on the next click elsewhere, and blinking
        // the ring off for that would make clicking a bar flicker the desktop.
        if self.keyboard_on == KeyboardOn::ExclusivePanel {
            return None;
        }
        let id = self.space.focused()?;
        // An unmapped window has geometry but nothing on screen; ringing it
        // would draw a rectangle around empty space. Membership in `windows`
        // is not the test — a window is in there from creation, hundreds of
        // milliseconds before it has anything to show.
        if !self.mapped.contains(&id) {
            return None;
        }
        let window = self.space.window(id)?;
        // A fullscreen window covers its output edge to edge. There is nowhere
        // outside it to put a ring, and the point of fullscreen is that nothing
        // else is on screen to tell it apart from.
        if window.mode == WindowMode::Fullscreen {
            return None;
        }
        Some(window.geometry.ring(crate::theme::FOCUS_RING_WIDTH))
    }

    /// The focus ring's edges, each paired with the buffer that paints it.
    /// Empty when nothing is ringed.
    fn focus_ring(&self) -> Vec<(&SolidColorBuffer, Rect)> {
        let Some(rects) = self.focus_ring_at else {
            return Vec::new();
        };
        self.focus_ring.iter().zip(rects).collect()
    }

    /// Consume the redraw request, if there is one.
    pub(crate) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
    }

    /// End-of-cycle housekeeping, run by both backends.
    ///
    /// A dismissed popup leaves a dead entry in the popup tree, and a dead
    /// entry still appears in the scene: the client destroyed the surface, so
    /// there is nothing to paint, but the tree is only pruned when asked. Doing
    /// it once per cycle rather than at each destroy point keeps a nested
    /// menu's teardown — several surfaces going at once — to a single sweep.
    pub(crate) fn refresh(&mut self) {
        self.popups.cleanup();
    }

    /// Take a tile for a newly mapped X11 window.
    ///
    /// Separate from `new_toplevel` only because the XDG path is a protocol
    /// handler and this is not; both take a `WindowId` from the core and record
    /// the surface against it.
    pub(crate) fn open_x11_window(&mut self, window: WindowSurface) -> WindowId {
        let id = self.space.open_window();
        self.windows.insert(id, window);
        id
    }

    /// Drop an X11 window out of the layout and re-run the passes that depend
    /// on it. Mirrors the tail of `toplevel_destroyed`.
    pub(crate) fn close_x11_window(&mut self, id: WindowId) {
        self.windows.remove(&id);
        self.mapped.remove(&id);
        self.space.close_window(id);
        self.arrange();
        self.refresh_focus();
    }

    /// The layout's id for an X11 window, if it is managed.
    pub(crate) fn x11_window_id(&self, window: &X11Surface) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|(_, w)| w.as_x11() == Some(window))
            .map(|(id, _)| *id)
    }

    /// The surface backing `id`, if it is still mapped.
    pub(crate) fn surface(&self, id: WindowId) -> Option<&WindowSurface> {
        self.windows.get(&id)
    }

    /// Windows on the active workspace with the geometry the core assigned,
    /// in the order they should be painted.
    ///
    /// Skips windows that have not yet committed a buffer. Their tile is
    /// already reserved and their `configure` already sent; what they have not
    /// got is anything worth showing, and showing it anyway is a frame of
    /// whatever happened to be in the buffer.
    /// Yields the Wayland surface rather than the window, because every caller
    /// wants exactly that and an X11 window's surface is an `Option` — resolving
    /// it here means the render path, the popup root lookup and the scene
    /// builder do not each have to decide what an X11 window with no surface yet
    /// should mean. It means the same thing as an unmapped window: skip it.
    pub(crate) fn render_list(&self) -> Vec<(WlSurface, Rect)> {
        self.space
            .active_workspace()
            .windows()
            .iter()
            .filter(|id| self.mapped.contains(id))
            .filter_map(|id| {
                let surface = self.windows.get(id)?.wl_surface()?;
                let geometry = self.space.window(*id)?.geometry;
                Some((surface, geometry))
            })
            .collect()
    }

    /// Bring a window's mapped state into line with whether it has a buffer.
    ///
    /// Both directions. A client may unmap a window by attaching a null buffer
    /// and later map it again by attaching a real one; xdg-shell says so
    /// explicitly, and a compositor that only ever latches "mapped" leaves a
    /// focus ring drawn around a window the client has taken away.
    ///
    /// Returns whether the state changed — the moment a window appears or
    /// disappears, and where the open and close animations will hang once
    /// there are any.
    fn sync_mapped(&mut self, surface: &WlSurface) -> bool {
        let Some(id) = self
            .windows
            .iter()
            .find(|(_, w)| w.wl_surface().as_ref() == Some(surface))
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let changed = if has_buffer(surface) {
            self.mapped.insert(id)
        } else {
            self.mapped.remove(&id)
        };
        if changed {
            tracing::debug!(
                window = id.raw(),
                mapped = self.mapped.contains(&id),
                "toplevel map state"
            );
        }
        changed
    }

    /// Ask the core to lay out the active workspace and configure whatever
    /// moved.
    ///
    /// `arrange` returns only windows whose geometry actually changed, so this
    /// sends the minimum number of configures. Sending one per window on every
    /// call would make clients re-render continuously.
    pub(crate) fn arrange(&mut self) {
        self.settle_carousel();
        for (id, rect) in self.space.arrange() {
            let Some(window) = self.windows.get(&id) else {
                continue;
            };
            // A window that has not had its initial configure yet is mid-way
            // through `new_toplevel`, which laid out first precisely so it
            // could send one configure carrying the final geometry. Sending
            // one here would make that two, and the first would be the stale
            // one — reintroducing the resize-after-first-paint this ordering
            // exists to remove.
            // XDG only: an X11 window has no initial-configure handshake, so
            // there is no first configure for a second one to duplicate. The
            // check would skip every X11 window forever.
            if let Some(toplevel) = window.as_xdg()
                && !toplevel.is_initial_configure_sent()
            {
                continue;
            }
            tracing::debug!(window = id.raw(), ?rect, "configure");
            window.configure(rect);
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

        for (id, window) in &self.windows {
            let is_focused = Some(*id) == focused;
            // set_activated reports whether it actually changed anything, and
            // the configure is conditional on that. refresh_focus runs whenever
            // focus *might* have moved, so configuring unconditionally here
            // drives every client on the workspace into a redraw loop.
            if window.set_activated(is_focused) {
                window.send_configure();
            }
        }

        // A layer surface holding the keyboard does not take the window's
        // `activated` state with it: the window is still the one the keyboard
        // returns to when the panel goes away. What an exclusive claim does take
        // is the ring — see [`Self::focus_ring_rects`].
        let (target, keyboard_on) = self.resolve_keyboard_focus();
        self.keyboard_on = keyboard_on;
        self.set_keyboard_focus(target, Serial::from(0));

        // The ring moved. Click-to-focus arrives here without going through
        // arrange, so without this a click would move focus and leave the ring
        // behind on the previous window until something else forced a frame.
        self.queue_redraw();
    }

    /// Record which layer surface the pointer clicked into, and report whether
    /// that changed anything.
    ///
    /// `None` clears the claim, which is what a click anywhere else does: an
    /// on-demand panel holds the keyboard only until the user's attention
    /// visibly goes somewhere else.
    pub(crate) fn set_focused_layer(&mut self, surface: Option<WlSurface>) -> bool {
        if self.focused_layer == surface {
            return false;
        }
        self.focused_layer = surface;
        true
    }

    /// Aim the carousel at wherever focus is, and hand the layout the offset it
    /// should draw at this frame.
    ///
    /// Run at the top of every [`Self::arrange`], because everything that can
    /// move the strip — a focus change, a window opening or closing, the layout
    /// being toggled, the usable area changing under a new panel — already goes
    /// through there. There is no separate "scroll the carousel" path to keep in
    /// step with this one.
    fn settle_carousel(&mut self) {
        let now = self.uptime();
        let Some(target) = self.space.update_carousel_target() else {
            // Not a carousel workspace. Hand the offset back to the layout so a
            // stale slide cannot hold a tiled workspace off its own geometry.
            self.space.set_carousel_offset(None);
            self.carousel_on = None;
            return;
        };

        // Arriving on a different workspace is not a slide. Each one keeps its
        // own scroll position, so there is nothing continuous between where the
        // last strip sat and where this one does — animating across would move
        // this workspace's panes a distance that belongs to the other one.
        let workspace = self.space.active_workspace().id();
        if self.carousel_on != Some(workspace) {
            self.carousel_on = Some(workspace);
            self.carousel_scroll.jump_to(target as f32);
        } else if (self.carousel_scroll.target() - target as f32).abs() >= 1.0 {
            // Only retarget when the destination actually moved.
            // `Animated::animate_to` restarts its clock for a target it has
            // already reached, and this runs on every arrange — including the
            // arranges the animation itself asks for — so calling it
            // unconditionally would keep the strip permanently one frame from
            // settling and the compositor permanently redrawing.
            self.carousel_scroll.animate_to(
                target as f32,
                now,
                self.settings.motion().duration(crate::anim::CAROUSEL_SLIDE),
                crate::anim::Curve::EaseOut,
            );
        }

        self.space
            .set_carousel_offset(Some(self.carousel_scroll.value(now).round() as i32));
    }

    /// Resolve who should hold the keyboard: an interactive layer surface, or
    /// the focused window.
    ///
    /// The decision is [`huginn_core::layer::keyboard_focus`], which is pure and
    /// tested there; this only translates Wayland state into the vocabulary it
    /// works in. Surfaces are keyed by their index in [`Self::layers`], which is
    /// push order and therefore also mapping order — so one number serves as
    /// both the identity and the recency tie-break.
    fn resolve_keyboard_focus(&self) -> (Option<WlSurface>, KeyboardOn) {
        let candidates: Vec<Focusable<usize>> = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(index, (surface, _))| {
                // An unmapped surface is not a candidate. A client may unmap by
                // attaching a null buffer and stay alive to map again later, and
                // one that did so while holding an exclusive claim would go on
                // swallowing every keystroke with nothing on screen to show for
                // it — the keyboard equivalent of the ring drawn around a window
                // the client has taken away.
                //
                // Nothing else is needed to release it: the unmap is a commit,
                // every layer commit reaches `refresh_layers`, and that settles
                // focus. Dropping out of this list is the whole mechanism.
                if !has_buffer(surface.wl_surface()) {
                    return None;
                }
                let state = layer_state(surface)?;
                Some(Focusable {
                    key: index,
                    level: level_of(state.layer),
                    interactivity: state.interactivity,
                    mapped: index as u64,
                })
            })
            .collect();

        let clicked = self
            .focused_layer
            .as_ref()
            .and_then(|want| self.layers.iter().position(|(s, _)| s.wl_surface() == want));

        match keyboard_focus(&candidates, clicked, self.space.focused()) {
            KeyboardFocus::Layer(index) => {
                let Some((surface, _)) = self.layers.get(index) else {
                    return (None, KeyboardOn::Window);
                };
                let exclusive = candidates
                    .iter()
                    .find(|c| c.key == index)
                    .is_some_and(Focusable::holds_exclusive);
                let on = if exclusive {
                    KeyboardOn::ExclusivePanel
                } else {
                    KeyboardOn::Panel
                };
                (Some(surface.wl_surface().clone()), on)
            }
            KeyboardFocus::Window(id) => (
                self.windows.get(&id).and_then(|w| w.wl_surface()),
                KeyboardOn::Window,
            ),
            // Nothing holds the keyboard. Both callers want the ordinary answer:
            // the ring behaves normally (there is no focused window to ring
            // anyway), and `Super` is left to whatever gets focus next.
            KeyboardFocus::Nothing => (None, KeyboardOn::Window),
        }
    }

    /// Give the keyboard — and with it the clipboard — to `target`.
    ///
    /// Always use this rather than `KeyboardHandle::set_focus` directly.
    /// smithay calls [`SeatHandler::focus_changed`] only when focus moves *to*
    /// a surface: the branch that clears it sends the old surface its `leave`
    /// and returns, so nothing tells the data device that nobody holds the
    /// keyboard any more. The clipboard would stay pointed at the window that
    /// just closed, and that client would go on receiving selection offers
    /// while unfocused — which is the same class of bug as never setting the
    /// focus in the first place, just rarer and harder to see.
    ///
    /// The extra call is free when the hook did fire: smithay's
    /// `set_clipboard_focus` compares against the focus it already has and
    /// returns without sending anything if they agree.
    pub(crate) fn set_keyboard_focus(&mut self, target: Option<WlSurface>, serial: Serial) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        keyboard.set_focus(self, target.clone(), serial);
        let client = target.and_then(|surface| self.display.get_client(surface.id()).ok());
        set_data_device_focus(&self.display, &self.seat, client);
    }

    /// Attach an output so clients can discover scale, mode, and position.
    ///
    /// The returned [`GlobalId`] is what withdraws it again. A monitor can be
    /// unplugged, and an output global left behind after that is one clients
    /// still believe in and will happily place surfaces on.
    pub(crate) fn add_output(&self, output: &Output, dh: &DisplayHandle) -> GlobalId {
        output.create_global::<Self>(dh)
    }
}

impl CompositorHandler for Huginn {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    /// The per-client compositor state, for either kind of client.
    ///
    /// XWayland is the exception that has to be named. Ordinary clients arrive
    /// through the listening socket and are inserted with [`ClientState`], but
    /// XWayland's connection is made by smithay inside `XWayland::spawn`, and it
    /// carries smithay's own [`XWaylandClientData`] instead — which holds a
    /// `CompositorClientState` of its own for exactly this reason.
    ///
    /// Looking only for [`ClientState`] and unwrapping is therefore a panic with
    /// a timer on it: the compositor comes up fine, XWayland connects a moment
    /// later, binds `wl_output`, and the session dies during startup with an
    /// expect message that blames client insertion rather than XWayland. It
    /// survives only where the `Xwayland` binary is missing, which is why a
    /// container build and a bare session can disagree about whether the
    /// compositor works at all.
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(xwayland) = client.get_data::<XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState or XWaylandClientData")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.queue_redraw();

        // A buffer arriving is what makes a window visible — not its creation,
        // and not its configure. Until then the window holds a reserved and
        // empty tile. See [`Huginn::sync_mapped`] and the design spec, §5.
        if self.sync_mapped(surface) {
            // Focus was given to this window when it was created, but the ring
            // could not be drawn around something that was not on screen yet.
            self.refresh_focus();
        }

        // Moves a popup from unmapped to mapped once its parent is known, which
        // is what puts it in the tree the scene is built from.
        self.popups.commit(surface);

        // Same rule as a toplevel: the client may not attach a buffer until it
        // has acked a configure, so a popup that never gets one never appears.
        // It cannot be sent from new_popup, because the client sets its
        // positioner between creating the popup and committing it, and
        // configuring early would answer with geometry computed from nothing.
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            self.unconstrain_popup(&popup);
            if let Err(err) = popup.send_configure() {
                tracing::warn!(%err, "initial popup configure refused");
            }
        }

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
        self.windows.insert(id, WindowSurface::Xdg(surface.clone()));
        tracing::debug!(window = id.raw(), "toplevel created");

        // Lay out FIRST, so the geometry this window will actually occupy
        // exists before its one and only initial configure goes out.
        //
        // The order matters more than it looks. Configuring first and arranging
        // second sends the client a size of zero — "pick your own" — it paints
        // at whatever it chose, and the configure that follows immediately
        // resizes it into its tile. That pre-tile frame is visible, and it is
        // one of the two things that makes a Chromium-based application flash
        // when it starts. Arranging first means the first frame the client ever
        // produces is already the right size. See the design spec, §5.
        //
        // `arrange` skips any window whose initial configure has not gone out,
        // so it lays this one out without configuring it and the single
        // configure below is the first the client ever sees.
        self.arrange();

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.size = Some(
                self.space
                    .window(id)
                    .map_or_else(|| (0, 0).into(), |w| {
                        (w.geometry.w(), w.geometry.h()).into()
                    }),
            );
        });
        // xdg-shell requires the client to ack a configure before its first
        // commit, so a window that never gets one simply never appears.
        surface.send_configure();

        self.refresh_focus();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self
            .windows
            .iter()
            .find(|(_, w)| w.as_xdg() == Some(&surface))
            .map(|(id, _)| *id)
        else {
            return;
        };
        self.windows.remove(&id);
        self.mapped.remove(&id);
        self.space.close_window(id);
        tracing::debug!(window = id.raw(), "toplevel destroyed");

        self.arrange();
        self.refresh_focus();
        self.refresh_dock();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // No configure here: see the popup branch of `commit`. Tracking is all
        // that is owed at this point, and a popup whose parent is not set yet
        // is parked as unmapped until it is.
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(%err, "popup died before it could be tracked");
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.take_popup_grab(surface, &seat, serial);
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        // The tree is pruned once per cycle by `refresh`; all that is needed
        // here is a frame without the popup in it.
        self.queue_redraw();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // Reposition is the client saying "same popup, new anchor" — a submenu
        // following the item the pointer moved to. The new positioner has to
        // be stored before unconstraining, since unconstraining is defined in
        // terms of it.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        // The token pairs the client's request with the configure that answers
        // it, so it must go out first.
        surface.send_repositioned(token);
        if let Err(err) = surface.send_configure() {
            tracing::warn!(%err, "reposition configure refused");
        }
        self.queue_redraw();
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

    /// Mirror the xkb lock state onto the physical keyboards.
    ///
    /// smithay recomputes the LED state on every key and calls this only when
    /// it changed, so this is per-caps-lock-press, not per-keystroke.
    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.keyboard_led_state = led_state;
        for device in &mut self.keyboard_led_devices {
            device.led_update(led_state.into());
        }
    }

    /// Hand the clipboard to whoever just took the keyboard.
    ///
    /// Without this the clipboard is inert in a way that looks like the clients
    /// are at fault. `wl_data_device.selection` is only ever sent to the client
    /// smithay believes holds the *data device* focus, which is a separate
    /// thing from keyboard focus and is `None` until someone says otherwise —
    /// and a `None` focus matches no client, so every device is skipped. The
    /// compositor happily stores a `set_selection` from a client and then
    /// offers it to nobody, including back to the client that set it. A
    /// terminal asking to paste gets no offer, reads an empty clipboard, and
    /// silently does nothing.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display.get_client(surface.id()).ok());
        set_data_device_focus(&self.display, seat, client);
    }

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
        // Let go of the click before the surface does, or the keyboard is left
        // pointing at something that no longer exists — the same class of bug
        // the `set_keyboard_focus` comment describes for the clipboard, and the
        // reason `refresh_layers` below now settles focus as well as geometry.
        if self.focused_layer.as_ref() == Some(surface.wl_surface()) {
            self.focused_layer = None;
        }
        self.layers.retain(|(l, _)| l != &surface);
        tracing::debug!(remaining = self.layers.len(), "layer surface destroyed");
        self.refresh_layers();
    }
}

impl OutputHandler for Huginn {}

impl FractionalScaleHandler for Huginn {
    /// Answer a client's fractional-scale request with a whole number.
    ///
    /// The protocol's name is about the wire format — it carries scale in
    /// 120ths so a compositor *can* say 1.5 — not an obligation to send one.
    /// This compositor never does. A client that asks is told 1 or 2, the same
    /// as `wl_output` says, so the two can never disagree and no client has a
    /// reason to render at a fraction. See `huginn_core::scale`.
    ///
    /// Answered on creation rather than waiting for a commit: a client that
    /// binds this and gets no scale back has to guess, and guessing is the
    /// thing this exists to remove.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = f64::from(self.scale.advertised);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

delegate_compositor!(Huginn);
delegate_xdg_shell!(Huginn);
delegate_shm!(Huginn);
delegate_seat!(Huginn);
delegate_data_device!(Huginn);
delegate_output!(Huginn);
delegate_viewporter!(Huginn);
delegate_fractional_scale!(Huginn);
delegate_layer_shell!(Huginn);

/// XWayland associating an X11 window with a Wayland surface.
///
/// The dispatch side lives on `Huginn` because `Display<Huginn>` is what
/// clients are dispatched against. The *handler* side is also needed on each
/// backend's loop data type, which `impl_xwm_handler!` generates -- see
/// `crate::xwayland` for why those are two different types.
impl XWaylandShellHandler for Huginn {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
delegate_xwayland_shell!(Huginn);

/// What a layer surface has asked for, translated out of Wayland vocabulary and
/// into the plain geometry types `huginn-core` works in.
pub(crate) struct LayerRequest {
    anchors: Anchors,
    desired: Size,
    margins: Margins,
    exclusive: i32,
    pub(crate) layer: Layer,
    /// How much of the keyboard this surface is asking for. Read but ignored
    /// before focus resolution existed, which is what made an interactive panel
    /// impossible to write.
    pub(crate) interactivity: Interactivity,
}

/// Where the keyboard sits, in the terms the rest of the compositor asks about.
///
/// Three states rather than a bool because the two questions asked of this have
/// different answers. `Super` belongs to the application only when a *window*
/// holds the keyboard — a panel is never a terminal. The focus ring retires only
/// for an *exclusive* grab: an on-demand panel is one click from handing the
/// keyboard back, and retiring the ring for it would make the ring blink off and
/// on every time someone clicked a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum KeyboardOn {
    /// A window has it, or nothing does. The ordinary case.
    #[default]
    Window,
    /// A layer surface has it because it was clicked.
    Panel,
    /// A layer surface has it because it asked for it and its layer allows it.
    ExclusivePanel,
}

/// Translate the protocol's layer into the core's own vocabulary.
pub(crate) const fn level_of(layer: Layer) -> Level {
    match layer {
        Layer::Background => Level::Background,
        Layer::Bottom => Level::Bottom,
        Layer::Top => Level::Top,
        Layer::Overlay => Level::Overlay,
    }
}

/// Read a layer surface's committed state.
///
/// Returns `None` before the client's first commit, when there is nothing to
/// place yet.
pub(crate) fn layer_state(surface: &LayerSurface) -> Option<LayerRequest> {
    use smithay::wayland::shell::wlr_layer::{Anchor, ExclusiveZone, KeyboardInteractivity};

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
        interactivity: match state.keyboard_interactivity {
            KeyboardInteractivity::Exclusive => Interactivity::Exclusive,
            KeyboardInteractivity::OnDemand => Interactivity::OnDemand,
            // `None`, and anything a newer protocol version adds. Refusing the
            // keyboard is the safe reading of a request we do not understand.
            _ => Interactivity::None,
        },
    })
}

