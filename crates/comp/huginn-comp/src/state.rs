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
use std::time::Instant;

use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;

use crate::window::WindowSurface;
use huginn_core::{
    Space,
    geometry::{Dir, Rect, Size},
    layer::{
        Anchors, Exclusive, Focusable, Interactivity, KeyboardFocus, Level, Margins,
        keyboard_focus, place, usable_area,
    },
    scale::OutputScale,
    tiles::Axis,
    window::{WindowId, WindowMode},
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBuffer, solid::SolidColorBuffer},
        utils::{on_commit_buffer_handler, with_renderer_surface_state},
    },
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_fractional_scale,
    delegate_layer_shell, delegate_output, delegate_seat, delegate_session_lock, delegate_shm,
    delegate_viewporter, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::LedState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason, GlobalId},
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    utils::{Logical, Point},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::{
            FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
        shell::{
            wlr_layer::{
                Layer, LayerSurface, LayerSurfaceCachedState, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
        shm::{ShmHandler, ShmState},
        viewporter::ViewporterState,
    },
};
use smithay::{
    delegate_xwayland_shell,
    wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    xwayland::{X11Surface, X11Wm, XWaylandClientData},
};

/// Per-client state the compositor attaches to every connection.
#[derive(Default, Debug)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Rung when this client goes away, so the event loop can ask
    /// [`Huginn::recover_lost_lock`] whether the client that just left was
    /// the lock screen. The lock protocol has no message for that case -- a
    /// client that dies simply stops talking -- and this runs on the
    /// display's thread with no access to the compositor's state, so a ping
    /// is the whole of what it can do.
    pub on_disconnect: Option<calloop::ping::Ping>,
}

impl ClientState {
    pub(crate) fn new(on_disconnect: calloop::ping::Ping) -> Self {
        Self {
            compositor_state: CompositorClientState::default(),
            on_disconnect: Some(on_disconnect),
        }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, id: ClientId) {
        tracing::debug!(?id, "client connected");
    }

    fn disconnected(&self, id: ClientId, reason: DisconnectReason) {
        tracing::debug!(?id, ?reason, "client disconnected");
        if let Some(ping) = &self.on_disconnect {
            ping.ping();
        }
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
    /// A window belonging to one of the workspace cards in the touchpad
    /// switcher. The transform is applied to the whole workspace coordinate
    /// system, so every window keeps its place inside the card.
    WorkspaceSurface(WlSurface, Rect, WorkspacePreview),
    /// A minimized window shown small above the application switcher, so the
    /// tiles of one application can be told apart by what is in them. Drawn
    /// like a workspace card's window, but invisible to hit testing: it is a
    /// picture of a window, not the window.
    Preview(WlSurface, Rect, WorkspacePreview),
    /// A tiled window: the surface at its natural size with its origin at the
    /// first rectangle, cropped to its pane in the second, at an alpha.
    ///
    /// The crop remains after a resize motion finishes. Wayland clients answer
    /// a configure asynchronously, so for a few frames their current buffer
    /// can still have the old size. Letting it render without this crop would
    /// spill into a newly nested sibling until the new buffer arrived.
    Clipped(WlSurface, Rect, Rect, f32),
    /// Opaque backing for a workspace card, so empty space and empty
    /// workspaces still read as physical cards rather than holes in the row.
    WorkspaceCard(&'a SolidColorBuffer, WorkspacePreview),
    /// One edge of the ring around the focused window. Drawn by the compositor
    /// itself, so unlike a surface there is no client to click on or to send a
    /// frame callback to.
    Ring(&'a SolidColorBuffer, Rect, f32),
    /// The keybinding overlay. Compositor-drawn like the ring, so it too is
    /// invisible to hit testing — clicks fall through it to whatever is
    /// underneath, which is right for something that is a label rather than a
    /// window.
    Overlay(&'a MemoryRenderBuffer, Rect, f32),
}

/// The compositor transform for one workspace card.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkspacePreview {
    pub scale_x: f64,
    pub scale_y: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub alpha: f32,
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceCarousel {
    position: crate::anim::Animated,
    reveal: crate::anim::Animated,
    closing: bool,
}

/// One thumbnail above the dock: the switcher's highlighted window, or a
/// window of the application under the pointer.
#[derive(Debug)]
struct DockPreview {
    window: huginn_core::window::WindowId,
    /// Where the window is drawn, scaled to fit; logical pixels.
    frame: Rect,
    /// A backing drawn a little larger than `frame`.
    panel: crate::canvas::Panel,
    /// The window's title, to sit under the backing.
    caption: Option<crate::canvas::Panel>,
}

/// The dock promoted to the centre of the screen for gesture navigation.
#[derive(Debug, Clone, Copy)]
struct AppSwitcher {
    /// Index into `dock_items`, always naming a running application.
    selected: usize,
    /// The switcher is deliberately temporary; input extends this deadline.
    dismiss_at: std::time::Duration,
}

const APP_SWITCHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

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
    /// `wp_cursor_shape_manager_v1`: a client names a cursor and the
    /// compositor draws it from the system theme, at the screen's density.
    /// Without this GTK 4.22 falls back to its built-in 32-pixel bitmaps and
    /// asks for them at the wrong size through a viewport, so the pointer
    /// grows the moment it crosses into a GTK window. Held, not read.
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    /// Held, not read: dropping this would withdraw the xdg-output global
    /// and clients would lose their output information mid-session.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    /// `ext_session_lock_manager_v1`. What `raven-lock` binds to hold the
    /// session.
    pub session_lock_state: SessionLockManagerState,
    /// The lock, if the session is being held. See [`Lock`].
    pub(crate) lock: Option<Lock>,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,
    /// Pointer position in compositor-global logical coordinates.
    pub pointer_location: Point<f64, Logical>,
    /// When input last arrived, for the idle lock.
    ///
    /// `Instant` is `CLOCK_MONOTONIC`, which on Linux does not advance while
    /// the machine is suspended — so a laptop shut for the night and opened in
    /// the morning has been idle for however long it was *awake*, not for
    /// eight hours. That is the behaviour you want here and it is not the one
    /// a wall clock gives: the resume already locks the session on its own,
    /// and counting the sleep as idle time would mean the idle lock also fired
    /// on every wake, racing it for no reason.
    last_input: Instant,
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
    pub(crate) mapped: HashSet<WindowId>,

    /// Menus, dropdowns and tooltips, and the modal grabs that dismiss them.
    ///
    /// Deliberately outside `space`: a popup is not a window, has no place in
    /// a workspace, and is positioned by the client's own rules rather than by
    /// any layout the core runs.
    pub popups: PopupManager,

    /// Panels, docks, wallpapers and overlays, with the geometry last sent to
    /// each. Storing what was sent is what keeps [`Huginn::refresh_layers`]
    /// from configuring on every commit and driving clients into a redraw loop.
    layers: Vec<(LayerSurface, Rect, usize)>,
    /// Where the carousel is scrolled to, while it is getting there.
    ///
    /// The compositor's half of the layout: `huginn-core` works out where the
    /// strip belongs and has no clock to move it with, so the sliding lives
    /// here and the offset is handed back down each frame.
    carousel_scroll: crate::anim::Animated,
    /// Workspace-level Cover Flow shown while a three-finger swipe is active.
    workspace_carousel: Option<WorkspaceCarousel>,
    /// The touchpad swipe in progress, if any.
    ///
    /// Held on the compositor rather than in the core because a gesture is
    /// input, not window management: `huginn-core` is told where the strip
    /// should be, and has no interest in how many fingers said so.
    swipe: Option<crate::gesture::Swipe>,
    /// Recognises the gesture that temporarily summons the application dock.
    double_tap: crate::gesture::DoubleTap,
    /// Finger count retained between a hold begin/end pair.
    hold_fingers: Option<u32>,
    /// Present while the temporary application switcher is visible.
    app_switcher: Option<AppSwitcher>,
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
    /// Every screen, in the one logical coordinate space the desktop is laid
    /// out in, parallel to `space.outputs()`. Never empty: with nothing
    /// connected the last known screen is kept so the desktop has somewhere
    /// to be.
    outputs: Vec<OutputInfo>,
    /// Where the screens were asked to go, by connector name, kept across
    /// sessions. See `huginn_core::layout`.
    layout: Vec<huginn_core::layout::Saved>,
    /// Changes a client has staged and not yet applied.
    staged_layout: Vec<huginn_core::layout::Saved>,
    /// `apply` happened; the backend owes a re-arrange.
    layout_changed: bool,

    /// The keybinding overlay, drawn once when it is summoned rather than on
    /// every frame. `None` when it is not on screen, which is almost always.
    help: Option<crate::overlay::Overlay>,

    /// The wallpaper at its own size, read from disk once at startup, and the
    /// copies composed for each output, parallel to `outputs`.
    ///
    /// Two fields because the halves have different costs: decoding a
    /// photograph is tens of milliseconds and its result never changes, while
    /// scaling it is one pass over the output and has to happen again whenever
    /// the output does. A mode change re-scales; it does not re-read the disk.
    /// `None` on either is the ordinary state of a machine with no wallpaper
    /// set, and leaves the backends' clear colour showing.
    wallpaper: Option<crate::wallpaper::Wallpaper>,
    wallpaper_panels: Vec<Option<crate::canvas::Panel>>,
    /// What `~/.config/raven/desktop.toml` said when last read.
    desktop_config: crate::desktop_config::DesktopConfig,

    /// Whether the arrows are currently resizing the focused window.
    ///
    /// A mode rather than a chord because resizing is done by feel: you press
    /// an arrow several times and watch. `Super`+`Ctrl`+arrows is already
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
    /// Thumbnails above the strip: the switcher's highlighted window, or every
    /// window of the application the pointer is over.
    dock_previews: Vec<DockPreview>,
    /// Which dock item the pointer is over, so a change of item — and only
    /// that — rebuilds the thumbnails.
    dock_hover: Option<usize>,
    /// When the pointer arrived on [`Self::dock_hover`], while its pictures
    /// are still waiting out [`crate::dock::PREVIEW_DELAY`].
    dock_hover_since: Option<std::time::Duration>,
    /// The items the strip currently holds, so a click can be resolved against
    /// the same list that was drawn.
    dock_items: Vec<crate::dock::Item>,

    /// Quick settings, and its rendered panel.
    pub(crate) settings: crate::settings::Settings,
    settings_panel: Option<crate::canvas::Panel>,
    /// The output volume, shared with the settings row that steps it, and
    /// the slider drawn while it is being changed.
    volume: crate::audio::Shared,
    volume_panel: Option<crate::canvas::Panel>,
    /// When the compositor started, so animations have a monotonic origin.
    started: std::time::Instant,

    /// The application launcher, and the index it searches.
    pub(crate) launcher: crate::launcher::Launcher,
    /// A way to ask [`crate::fileindex`] for a walk ahead of schedule, and
    /// when the last index arrived, so the asking is only done for a stale
    /// one. Both absent when the indexer never started.
    file_index_requests: Option<crate::fileindex::Requests>,
    file_index_built: Option<Instant>,
    /// The launcher's pixels, redrawn when its state changes rather than every
    /// frame — a search field repainted at the refresh rate is how one ends up
    /// feeling slower than the typing that drives it.
    launcher_panel: Option<crate::canvas::Panel>,
    /// The pinned panel, the pins it shows, and its pixels. The pins are
    /// held here rather than in the panel because three things write them:
    /// the panel, the launcher's menu, and quick settings.
    pub(crate) pinned: crate::pinned::Pinned,
    pub(crate) pins: crate::pins::Pins,
    pinned_panel: Option<crate::canvas::Panel>,
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
    workspace_card: SolidColorBuffer,
    focus_ring_at: Option<[Rect; 4]>,
    /// Windows whose drawn rectangle is still on its way to the layout's.
    ///
    /// A relayout moves the layout's rectangles at once; these are what the
    /// screen shows in the meantime, easing after them. See [`crate::motion`]
    /// for why this is the compositor's job and not the client's. Emptied as
    /// each arrives, so an idle desktop holds none.
    motions: HashMap<WindowId, crate::motion::Motion>,
    /// Which window the ring was last shown on, and when it appeared. The
    /// ring is a cue that focus *moved*, not a permanent frame: it is shown
    /// when the focused window changes and fades out shortly after. Tracked
    /// by window rather than by rect so a relayout that shifts the focused
    /// pane does not re-announce a focus that never changed.
    focus_ring_shown: Option<(huginn_core::window::WindowId, std::time::Duration)>,
}

/// Where a window's *surface* goes so that its visible frame sits in the
/// middle of `pane`.
///
/// The layout hands every window a pane and asks it to fill it, but what the
/// client commits is its own business: an application with a fixed or
/// clamped size comes back smaller than it was asked for, and one drawing its
/// own shadows comes back with a margin around the frame. Painting the buffer
/// with its corner at the pane's corner leaves both huddled top-left. So the
/// frame — the xdg window geometry, or the whole buffer when the client set
/// none — is centred in the pane, and the surface origin is wherever that
/// puts it. Rendering, popups and hit-testing all read this one rect, so they
/// cannot disagree about where the window is.
///
/// A frame larger than the pane is pinned to the pane's corner rather than
/// centred: overflowing evenly on both sides would hide the title bar.
pub(crate) fn place_in_pane(surface: &WlSurface, pane: Rect) -> Rect {
    let mut frame = crate::popup::window_geometry(surface);
    if frame.size.w <= 0 || frame.size.h <= 0 {
        frame = smithay::desktop::utils::bbox_from_surface_tree(surface, (0, 0));
    }
    if frame.size.w <= 0 || frame.size.h <= 0 {
        return pane;
    }
    let slack_x = (pane.w() - frame.size.w).max(0) / 2;
    let slack_y = (pane.h() - frame.size.h).max(0) / 2;
    Rect::from_xywh(
        pane.x() + slack_x - frame.loc.x,
        pane.y() + slack_y - frame.loc.y,
        frame.size.w,
        frame.size.h,
    )
}

impl Huginn {
    pub(crate) fn new(dh: &DisplayHandle, area: Rect) -> Self {
        let desktop_config = crate::desktop_config::DesktopConfig::load();
        crate::theme::set_accent(desktop_config.accent());
        let volume = crate::audio::Volume::detect().shared();
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "huginn");
        // Advertised for every backend. A seat with no pointer capability makes
        // toolkits that expect a cursor misbehave, and silently breaks anything
        // that relies on clicking — including Muninn's workspace pips.
        seat.add_pointer();

        let mut huginn = Self {
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
            cursor_shape_state: CursorShapeManagerState::new::<Self>(dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            // Any client may lock. The protocol's filter exists to restrict the
            // global to a privileged client, which needs a way to tell one
            // client from another -- and on a single-user session there is
            // none: every client here runs as the person whose session it is
            // and can already read their files, take their keystrokes and open
            // their windows. A filter that cannot distinguish them would be a
            // check that looks like security and is not.
            session_lock_state: SessionLockManagerState::new::<Self, _>(dh, |_| true),
            lock: None,
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            seat,
            pointer_location: (0.0, 0.0).into(),
            last_input: Instant::now(),
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
            workspace_carousel: None,
            swipe: None,
            double_tap: crate::gesture::DoubleTap::default(),
            hold_fingers: None,
            app_switcher: None,
            carousel_on: None,
            focused_layer: None,
            keyboard_on: KeyboardOn::default(),
            outputs: vec![OutputInfo::bare(area)],
            layout: load_layout(),
            staged_layout: Vec::new(),
            layout_changed: false,
            focus_ring: std::array::from_fn(|_| {
                SolidColorBuffer::new((0, 0), crate::theme::accent().to_rgba_f32())
            }),
            workspace_card: SolidColorBuffer::new(
                (area.w(), area.h()),
                crate::theme::BACKGROUND.to_rgba_f32(),
            ),
            focus_ring_at: None,
            motions: HashMap::new(),
            focus_ring_shown: None,
            help: None,
            wallpaper: crate::wallpaper::Wallpaper::chosen_or_installed(desktop_config.wallpaper()),
            desktop_config,
            wallpaper_panels: Vec::new(),
            resizing: false,
            socket: String::new(),
            dock: crate::dock::Dock::default(),
            dock_panel: None,
            dock_previews: Vec::new(),
            dock_hover: None,
            dock_hover_since: None,
            dock_items: Vec::new(),
            settings: crate::settings::Settings::new(volume.clone()),
            settings_panel: None,
            volume,
            volume_panel: None,
            started: std::time::Instant::now(),
            launcher: crate::launcher::Launcher::default(),
            file_index_requests: None,
            file_index_built: None,
            launcher_panel: None,
            pinned: crate::pinned::Pinned::default(),
            pins: load_pins(),
            pinned_panel: None,
            apps: crate::launcher::scan_applications(),
            frecency: load_frecency(),
            icons: raven_desktop::Icons::discover(crate::theme::ICON_THEME),
            pixmaps: raven_desktop::Pixmaps::new(),
            text: crate::text::Text::new(),
            display: dh.clone(),
        };
        // The rows in quick settings are where the pinned panel's layout is
        // changed, so they start from what the file said.
        huginn
            .settings
            .set_pins_layout(huginn.pins.position(), huginn.pins.orientation());
        huginn
            .settings
            .apply_desktop_config(huginn.desktop_config.motion(), huginn.desktop_config.idle_after());
        huginn
    }

    /// Re-read `desktop.toml` and apply what changed. Driven by
    /// [`crate::configwatch`] when the settings application saves.
    pub(crate) fn reload_desktop_config(&mut self) {
        let cfg = crate::desktop_config::DesktopConfig::load();
        tracing::info!("desktop settings changed");

        crate::theme::set_accent(cfg.accent());
        let accent = crate::theme::accent().to_rgba_f32();
        for ring in &mut self.focus_ring {
            ring.set_color(accent);
        }

        self.settings.apply_desktop_config(cfg.motion(), cfg.idle_after());

        if cfg.wallpaper() != self.desktop_config.wallpaper() {
            self.wallpaper = crate::wallpaper::Wallpaper::chosen_or_installed(cfg.wallpaper());
            self.wallpaper_panels = self
                .outputs
                .iter()
                .map(|output| {
                    self.wallpaper
                        .as_ref()
                        .map(|w| w.panel(output.scale.render, output.scale.advertised))
                })
                .collect();
        }
        self.desktop_config = cfg;

        // Everything compositor-drawn reads the accent when it renders, so
        // rebuild what is on screen.
        self.refresh_dock();
        self.refresh_settings();
        self.refresh_launcher();
        self.refresh_pinned();
        self.queue_redraw();
    }

    /// A key for the quick settings panel, and whatever a row asked for.
    pub(crate) fn settings_key(&mut self, key: crate::settings::Key) {
        let now = self.uptime();
        match self.settings.press(key, now) {
            crate::settings::Outcome::Dismissed | crate::settings::Outcome::Redraw => {
                self.refresh_settings()
            }
            crate::settings::Outcome::Unchanged => {}
        }
        if let Some(program) = self.settings.take_launch() {
            self.launch(None, &[program.to_owned()]);
        }
    }

    /// Open the settings application.
    pub(crate) fn open_full_settings(&mut self) {
        self.launch(None, &[crate::theme::SETTINGS_APP.to_owned()]);
    }

    /// Open the software store.
    pub(crate) fn open_store(&mut self) {
        self.launch(None, &[crate::theme::STORE_APP.to_owned()]);
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
                let area = self.output_area();
                let advertised = self.scale().advertised;
                Some(crate::overlay::Overlay::render(
                    area,
                    &mut self.text,
                    advertised,
                ))
            }
        };
        tracing::debug!(visible = self.help.is_some(), "keybinding overlay");
        self.queue_redraw();
    }

    /// Set the full output rectangle and reflow everything beneath it.
    fn apply_output_geometry(&mut self) {
        let area = self.output_area();
        self.workspace_card.resize((area.w(), area.h()));
        // The overlay picks its scale from the output height, so a resize with
        // it open has to redraw it rather than just re-centre it.
        if self.help.is_some() {
            let advertised = self.scale().advertised;
            self.help = Some(crate::overlay::Overlay::render(
                area,
                &mut self.text,
                advertised,
            ));
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
        // One set of exclusive zones per screen: a dock on the laptop panel
        // takes nothing from the monitor beside it.
        let mut zones: Vec<Vec<Exclusive>> = vec![Vec::new(); self.outputs.len()];
        let mut updates = Vec::new();

        for (index, (surface, last, on)) in self.layers.iter().enumerate() {
            let Some(state) = layer_state(surface) else {
                continue;
            };
            // A surface whose screen was unplugged lands on the primary
            // rather than vanishing: a panel is more use on the wrong screen
            // than on none.
            let on = (*on).min(self.outputs.len() - 1);
            let output = self.outputs[on].rect;
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
                zones[on].push(Exclusive {
                    edge,
                    size: state.exclusive,
                });
            }

            if rect != *last {
                updates.push((index, rect));
            }
        }

        for (index, rect) in updates {
            let (surface, last, _) = &mut self.layers[index];
            *last = rect;
            surface.with_pending_state(|pending| {
                pending.size = Some((rect.w(), rect.h()).into());
            });
            surface.send_configure();
        }

        for (on, zones) in zones.iter().enumerate() {
            let usable = usable_area(self.outputs[on].rect, zones);
            if self.space.set_output_area(on, usable) {
                tracing::debug!(output = %self.outputs[on].name, ?usable, panels = zones.len(), "usable area changed");
            }
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
        self.layers
            .iter()
            .map(|(surface, rect, _)| (surface, *rect))
            .collect()
    }

    /// Layer surfaces on `layer`, with their geometry, for rendering.
    pub(crate) fn layers_on(&self, layer: Layer) -> Vec<(&LayerSurface, Rect)> {
        self.layers
            .iter()
            .filter(|(surface, _, _)| layer_state(surface).is_some_and(|s| s.layer == layer))
            .map(|(surface, rect, _)| (surface, *rect))
            .collect()
    }

    /// The pointer handle. Always present: the seat is built with one.
    pub(crate) fn pointer(&self) -> PointerHandle<Self> {
        self.seat
            .get_pointer()
            .expect("the seat is constructed with a pointer")
    }

    /// The focused screen's rectangle, before panels reserve any of it.
    ///
    /// The focused screen is the one the shell's own panels sit on and the
    /// one keybindings act on; it follows the pointer between screens.
    pub(crate) fn output_area(&self) -> Rect {
        self.outputs[self.space.focused_output().min(self.outputs.len() - 1)].rect
    }

    /// The focused screen's scale: what the shell composes its panels at.
    pub(crate) fn scale(&self) -> OutputScale {
        self.outputs[self.space.focused_output().min(self.outputs.len() - 1)].scale
    }

    /// Every screen, in output order.
    pub(crate) fn outputs(&self) -> &[OutputInfo] {
        &self.outputs
    }

    /// The screen named `name`, as an index into [`Self::outputs`].
    pub(crate) fn output_index(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|o| o.name == name)
    }

    /// Replace the set of screens.
    ///
    /// The backend calls this whenever a connector comes or goes or changes
    /// mode, with every screen's position already decided. The core
    /// reconciles the workspaces (see [`huginn_core::Space::set_outputs`]);
    /// this composes what the compositor itself draws per screen -- the
    /// wallpaper at each panel's density -- and reflows the panels.
    pub(crate) fn set_outputs(&mut self, outputs: Vec<OutputInfo>) {
        if outputs.is_empty() {
            return;
        }
        for output in &outputs {
            let scale = output.scale;
            tracing::info!(
                name = %output.name,
                at = %format!("{},{}", output.rect.x(), output.rect.y()),
                advertised = scale.advertised,
                logical = %format!("{}x{}", scale.logical.w, scale.logical.h),
                render = %format!("{}x{}", scale.render.w, scale.render.h),
                physical = %format!("{}x{}", scale.physical.w, scale.physical.h),
                resample = scale.needs_resample(),
                "output"
            );
        }
        // Composed here rather than on demand: this is the one place the size
        // it has to match can change, and scaling a photograph in the middle
        // of assembling a frame would drop that frame.
        self.wallpaper_panels = outputs
            .iter()
            .map(|output| {
                self.wallpaper
                    .as_ref()
                    .map(|wallpaper| wallpaper.panel(output.scale.render, output.scale.advertised))
            })
            .collect();
        self.space.set_outputs(
            outputs
                .iter()
                .map(|output| huginn_core::OutputArea::new(output.rect))
                .collect(),
        );
        self.outputs = outputs;
        // Somewhere on a screen, always: a pointer left on a monitor that was
        // just unplugged is invisible and can reach nothing.
        self.pointer_location = self.clamp_pointer(self.pointer_location);
        self.apply_output_geometry();
        self.broadcast_outputs();
    }

    /// Move focus to the screen after the focused one, wrapping round.
    pub(crate) fn focus_next_output(&mut self) {
        let count = self.space.outputs().len();
        let next = (self.space.focused_output() + 1) % count;
        if self.space.focus_output(next) {
            self.broadcast_workspaces();
            self.queue_redraw();
        }
    }

    /// Send the focused window to the screen after the focused one.
    pub(crate) fn send_focused_to_next_output(&mut self) {
        let count = self.space.outputs().len();
        let next = (self.space.focused_output() + 1) % count;
        if next != self.space.focused_output() {
            self.space.send_focused_to_output(next);
        }
    }

    /// The pointer moved; if it crossed onto another screen, focus follows.
    ///
    /// Called from the shared motion handler. Focus follows the pointer
    /// between screens and only between screens: within one, clicking is
    /// what moves it, as it always has.
    pub(crate) fn pointer_crossed_outputs(&mut self) {
        let Some(on) = self.space.output_at(self.pointer_point()) else {
            return;
        };
        if on == self.space.focused_output() {
            return;
        }
        if self.space.focus_output(on) {
            tracing::debug!(output = %self.outputs[on].name, "focus followed the pointer");
            self.refresh_focus();
            self.broadcast_workspaces();
            self.queue_redraw();
        }
    }

    /// Tell every surface which screens it is on.
    ///
    /// `wl_surface.enter`/`leave` is how a toolkit learns the scale to draw
    /// at: it picks the largest of the outputs it has entered. Never sending
    /// them leaves every client at 1x, which on a 4K panel is a desktop of
    /// tiny blurry windows. Sent from the render path, once per frame, so a
    /// window dragged across a bezel is told as it crosses.
    pub(crate) fn update_surface_outputs(&self) {
        let surfaces: Vec<(WlSurface, Rect)> = self.frame_surfaces();
        for output in &self.outputs {
            let Some(wl) = &output.output else {
                continue;
            };
            let screen = smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
                (output.rect.x(), output.rect.y()).into(),
                (output.rect.w(), output.rect.h()).into(),
            );
            for (surface, rect) in &surfaces {
                let placed = smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
                    (rect.x(), rect.y()).into(),
                    (rect.w(), rect.h()).into(),
                );
                // `output_update` walks the tree with the root at `placed`'s
                // origin, so the overlap is expressed relative to that root.
                let overlap = screen.intersection(placed).map(|mut overlap| {
                    overlap.loc -= placed.loc;
                    overlap
                });
                smithay::desktop::utils::output_update(wl, overlap, surface);
            }
        }
    }

    /// Everything to paint, front to back.
    ///
    /// This is the single definition of stacking order, shared by rendering and
    /// by pointer hit testing. Keeping them on one function is what guarantees
    /// you can always click the thing you can see: if they were computed
    /// separately they would eventually disagree, and the bug would look like
    /// clicks landing on windows hidden behind a panel.
    pub(crate) fn scene(&self) -> Vec<SceneItem<'_>> {
        // Locked, and that is the whole scene. Not "the lock drawn on top of
        // the desktop": the desktop is not in the list at all, so there is no
        // ordering bug, no surface that could be raised above it and no
        // animation that could slide it aside. What is behind a lock screen
        // should be nothing, and the cheapest way to guarantee that is for it
        // to *be* nothing.
        //
        // An empty list is the correct answer while the lock is still a blank:
        // the backend clears the framebuffer every frame, so no elements means
        // a cleared screen rather than a stale one.
        if let Some(lock) = &self.lock {
            let mut out = Vec::new();
            for (name, surface) in lock.surfaces() {
                let Some(rect) = self.output_index(name).map(|i| self.outputs[i].rect) else {
                    continue;
                };
                out.push(SceneItem::Surface(surface.wl_surface().clone(), rect));
            }
            return out;
        }

        let mut out = Vec::new();
        // Front to back, and the order is the whole point:
        //
        //   help overlay   - summoned by someone who has lost track of what is
        //                    on screen, which includes the panels
        //   volume slider  - a key was just pressed; the answer goes on top
        //   launcher       - over the dock it grew out of
        //   pinned         - likewise
        //   settings       - likewise
        //   ---- blur boundary ----
        //   dock           - part of the desktop, so it blurs with it
        //   layer surfaces
        //   windows
        //
        // Everything below the boundary is what a panel blurs. See
        // [`Huginn::blur_boundary`], which counts the items above it.
        if let Some(help) = &self.help {
            out.push(SceneItem::Overlay(
                help.buffer(),
                help.placement(self.output_area()),
                1.0,
            ));
        }
        if let Some(panel) = &self.volume_panel {
            out.push(SceneItem::Overlay(
                panel.buffer(),
                crate::audio::placement(self.output_area(), panel.size()),
                self.volume.borrow().reveal(self.uptime()),
            ));
        }
        if let Some(panel) = &self.launcher_panel {
            let clock = self.uptime();
            let reveal = self.launcher.reveal(clock);
            out.push(SceneItem::Overlay(
                panel.buffer(),
                crate::launcher::placement(
                    self.output_area(),
                    panel.size(),
                    self.launcher.origin(),
                    reveal,
                ),
                reveal.clamp(0.0, 1.0),
            ));
        }
        if let Some(panel) = &self.pinned_panel {
            let reveal = self.pinned.reveal(self.uptime());
            out.push(SceneItem::Overlay(
                panel.buffer(),
                crate::pinned::placement(
                    self.output_area(),
                    panel.size(),
                    self.pinned.position(),
                    reveal,
                ),
                reveal.clamp(0.0, 1.0),
            ));
        }
        if let Some(panel) = &self.settings_panel {
            out.push(SceneItem::Overlay(
                panel.buffer(),
                panel.centred_on(self.output_area()),
                self.settings.reveal(self.uptime()).clamp(0.0, 1.0),
            ));
        }
        // --- blur boundary: everything below here is what a panel blurs ---
        if let Some(panel) = &self.dock_panel
            && let Some(rect) = self.dock_rect()
        {
            out.push(SceneItem::Overlay(panel.buffer(), rect, 1.0));
            out.extend(self.dock_preview_items());
        }
        // A panel's own menu belongs directly on top of the panel, not on top
        // of everything, so each layer surface carries its popups with it.
        //
        // The `top` layer is where bars and docks live, and a fullscreen window
        // goes over them: the layer-shell protocol says as much, and a video
        // with a status bar across it is not full screen. The `overlay` layer
        // stays above everything regardless — that is what it is for.
        let fullscreen = self.has_fullscreen();
        let mut top_layers = Vec::new();
        for layer in [Layer::Overlay, Layer::Top] {
            for (surface, rect) in self.layers_on(layer) {
                let mut items = self.popups_of(surface.wl_surface(), rect);
                items.push(SceneItem::Surface(surface.wl_surface().clone(), rect));
                if layer == Layer::Top && fullscreen {
                    top_layers.extend(items);
                } else {
                    out.extend(items);
                }
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
        if let Some(previews) = self.workspace_previews() {
            // The centre card is first because scene order is front-to-back.
            // Popups and the focus ring belong to the interactive full-size
            // desktop, not to passive workspace previews.
            for (workspace, transform) in previews {
                out.extend(
                    self.render_list_for(workspace)
                        .into_iter()
                        .map(|(surface, rect)| {
                            SceneItem::WorkspaceSurface(surface, rect, transform)
                        }),
                );
                out.push(SceneItem::WorkspaceCard(&self.workspace_card, transform));
            }
        } else {
            for (surface, rect) in self.render_list() {
                out.extend(self.popups_of(&surface, rect));
            }
            out.extend(
                self.focus_ring()
                    .into_iter()
                    .map(|(b, r, a)| SceneItem::Ring(b, r, a)),
            );
            out.extend(self.window_items());
        }
        // Top-layer panels displaced by a fullscreen window: behind it, but
        // still above everything else, so the rest of the desktop is unchanged.
        out.extend(top_layers);
        for layer in [Layer::Bottom, Layer::Background] {
            for (surface, rect) in self.layers_on(layer) {
                out.extend(self.popups_of(surface.wl_surface(), rect));
                out.push(SceneItem::Surface(surface.wl_surface().clone(), rect));
            }
        }
        // Last, so it is behind everything: the scene is front to back, and a
        // wallpaper is by definition what the rest of the desktop is on top
        // of. Below the background layer too -- a client that asked for
        // `Layer::Background` wants to be over the wallpaper, which is the
        // only thing that layer is for.
        for (output, panel) in self.outputs.iter().zip(&self.wallpaper_panels) {
            if let Some(panel) = panel {
                out.push(SceneItem::Overlay(panel.buffer(), output.rect, 1.0));
            }
        }
        out
    }

    /// The surfaces of [`Self::scene`], in the same order.
    ///
    /// Hit testing and frame callbacks both want a client on the other end, and
    /// the focus ring has none.
    pub(crate) fn scene_surfaces(&self) -> impl Iterator<Item = (WlSurface, Rect, Option<Rect>)> {
        self.scene().into_iter().filter_map(|item| match item {
            SceneItem::Surface(surface, rect) | SceneItem::WorkspaceSurface(surface, rect, _) => {
                Some((surface, rect, None))
            }
            SceneItem::Clipped(surface, rect, clip, _) => Some((surface, rect, Some(clip))),
            SceneItem::Preview(..)
            | SceneItem::Ring(..)
            | SceneItem::Overlay(..)
            | SceneItem::WorkspaceCard(..) => None,
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
        let mut out: Vec<(WlSurface, Rect)> = self
            .scene_surfaces()
            .map(|(surface, rect, _)| (surface, rect))
            .collect();
        // The switcher's thumbnail is a picture, not a surface to click, so
        // it is not in the scene list -- but it is on screen, and a window
        // that is on screen should be allowed to keep painting itself.
        out.extend(self.scene().into_iter().filter_map(|item| match item {
            SceneItem::Preview(surface, rect, _) => Some((surface, rect)),
            _ => None,
        }));

        // While locked that is the entire list. The windows below would
        // otherwise go on receiving frame callbacks and go on painting -- into
        // buffers nobody composites, which wastes a session's worth of GPU
        // work, and, worse, keeps a video call or a terminal live behind a
        // screen whose whole purpose is that nothing behind it is running
        // where anybody can see it.
        if self.lock.is_some() {
            return out;
        }

        out.extend(
            self.visible_window_ids()
                .into_iter()
                .filter(|id| !self.mapped.contains(id))
                .filter_map(|id| {
                    let surface = self.windows.get(&id)?.wl_surface()?;
                    let geometry = self.space.window(id)?.geometry;
                    Some((surface, geometry))
                }),
        );
        out
    }

    /// Every window on a workspace some screen is showing, screen by screen.
    pub(crate) fn visible_window_ids(&self) -> Vec<huginn_core::window::WindowId> {
        self.space
            .visible_workspaces()
            .filter_map(|(_, ws)| self.space.workspaces().get(ws))
            .flat_map(|ws| ws.windows().iter().copied())
            .collect()
    }

    /// Put the lock screen up.
    ///
    /// Order matters and is the opposite of the obvious one: the screen is
    /// blanked *first*, then the lock screen is started. Starting it first
    /// would leave the desktop on the panel for as long as a process takes to
    /// exec, connect to Wayland and ask -- which is exactly the window somebody
    /// walking up to a just-woken laptop would be looking at.
    ///
    /// If the spawn fails the blank comes straight back down. A machine whose
    /// lock screen is not installed should show its desktop, not a screen
    /// nobody can ever get past.
    pub(crate) fn lock_and_launch(&mut self) -> bool {
        if !self.begin_lock() {
            return false;
        }

        let argv = [crate::theme::LOCK_SCREEN.to_string()];
        if crate::backend::spawn(&argv, &self.socket, self.x11_display) {
            return true;
        }

        tracing::error!(
            program = crate::theme::LOCK_SCREEN,
            "the lock screen would not start; leaving the session unlocked"
        );
        self.lock = None;
        self.refresh_focus();
        self.queue_redraw();
        false
    }

    /// Input arrived; the session is not idle.
    ///
    /// Called once per input event by each backend, before the event is
    /// interpreted — a keystroke that resolves to no binding at all is still
    /// somebody at the keyboard.
    pub(crate) fn note_activity(&mut self) {
        self.last_input = Instant::now();
    }

    /// Whether the session has been idle long enough to lock itself.
    ///
    /// Answered here rather than in the backend's timer so that the rule is
    /// testable and lives next to the state it reads. A session already locked
    /// is never "due": it cannot be locked twice, and asking would restart the
    /// lock screen on top of itself.
    pub(crate) fn idle_lock_due(&self, now: Instant) -> bool {
        idle_due(
            self.is_locked(),
            self.settings.idle_after().duration(),
            now.saturating_duration_since(self.last_input),
        )
    }

    /// How long until [`Self::idle_lock_due`] is worth asking again.
    ///
    /// The idle timer reschedules itself by this rather than polling on a fixed
    /// tick, so a ten-minute timeout costs one wakeup ten minutes from now
    /// instead of twenty wakeups that find nothing. The coarse fallback covers
    /// the two cases with nothing to count down to — the setting is off, or the
    /// session is already locked — where the only thing being waited for is
    /// somebody changing the setting.
    pub(crate) fn idle_check_in(&self, now: Instant) -> std::time::Duration {
        idle_wait(
            self.is_locked(),
            self.settings.idle_after().duration(),
            now.saturating_duration_since(self.last_input),
        )
    }

    /// Whether the session is being held by a lock screen.
    pub(crate) fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    /// Stop drawing the session, before any client has asked.
    ///
    /// This is the compositor's own half of the lock; see [`Lock`] for why it
    /// exists separately from the protocol's. Returns whether it did anything,
    /// so a caller that spawned a lock screen can tell a fresh blank from one
    /// that was already up.
    pub(crate) fn begin_lock(&mut self) -> bool {
        if self.lock.is_some() {
            return false;
        }
        tracing::info!("locking the session");
        // The quick settings panel goes down with the session, disarmed. Its
        // Power row stays armed until something stands it down, and the
        // keymap forwards nothing to the panel while locked, so left open it
        // would come back after the unlock exactly as it was: highlighted on
        // Power, armed, one Return from a reboot. The person unlocking may
        // not be the one who pressed Return before the idle timer fired, and
        // Return is the key most people reach for to get rid of a panel they
        // did not expect. Dismiss is the same path the panel's own key takes,
        // so it disarms every row and leaves through the close animation.
        let now = self.uptime();
        let dismissed = self.settings.press(crate::settings::Key::Dismiss, now);
        if dismissed == crate::settings::Outcome::Dismissed {
            self.refresh_settings();
        }
        // The pinned panel too: it takes every key, and a locked session
        // must not come back with a panel that swallows the first thing
        // typed at it.
        if self.pinned.is_open() {
            let motion = self.settings.motion();
            self.pinned.close(now, motion);
            self.refresh_pinned();
        }
        self.lock = Some(Lock::default());
        self.refresh_focus();
        self.queue_redraw();
        true
    }

    /// Take the blank back down if no client ever claimed it.
    ///
    /// The escape hatch described on [`Lock`]. Does nothing once a client has
    /// asked for the lock -- from that point the session stays hidden until it
    /// says otherwise, which is the guarantee the protocol is for.
    pub(crate) fn abandon_lock_if_unclaimed(&mut self) {
        let unclaimed = self.lock.as_ref().is_some_and(|lock| lock.client.is_none());
        if !unclaimed {
            return;
        }

        tracing::error!(
            "no lock screen claimed the session; revealing the desktop again. \
             Is raven-lock installed?"
        );
        self.lock = None;
        self.refresh_focus();
        self.queue_redraw();
    }

    /// How many times a lock screen that died is started again before the
    /// session is revealed instead. One crash is a bad frame; the same crash
    /// three times running is a lock screen that cannot run on this machine,
    /// and restarting it forever is a black screen with a heartbeat.
    const LOCK_RELAUNCHES: u32 = 3;

    /// Notice a lock screen that died while holding the session, and start
    /// another.
    ///
    /// `ext-session-lock-v1` has no message for this. A client that crashes
    /// -- a protocol error, a signal, an OOM -- simply disconnects, its lock
    /// object dies with the connection, and the compositor is left exactly
    /// where the protocol told it to be: session hidden, every output a
    /// cleared frame, waiting for an unlock that can never come. Every
    /// lock-screen crash was a machine that had to be power-cycled.
    ///
    /// So this runs whenever any client disconnects (see
    /// [`ClientState::on_disconnect`]). If the client that held the lock is
    /// gone, the blank stays up -- the session is still hidden, and that is
    /// not negotiable -- and a fresh lock screen is started to claim it,
    /// with the same claim timeout as the first. After
    /// [`Self::LOCK_RELAUNCHES`] of those the session is revealed instead,
    /// for the reason on [`Lock`]: somebody at a wedged blank screen can
    /// already hold the power button, so the blank protects nothing.
    ///
    /// Returns whether a new lock screen was started, so the caller can arm
    /// the claim timeout for it.
    pub(crate) fn recover_lost_lock(&mut self) -> bool {
        let lost = self
            .lock
            .as_ref()
            .and_then(|lock| lock.client.as_ref())
            .is_some_and(|client| !client.handle.is_alive());
        if !lost {
            return false;
        }
        let relaunches = match self.lock.as_mut() {
            Some(lock) => {
                lock.client = None;
                lock.relaunches += 1;
                lock.relaunches
            }
            None => return false,
        };
        // The surfaces went with the client; nothing is drawn until the next
        // one makes its own, and the cleared frame in the meantime is the
        // point.
        self.refresh_focus();
        self.queue_redraw();

        if relaunches > Self::LOCK_RELAUNCHES {
            tracing::error!(
                program = crate::theme::LOCK_SCREEN,
                attempts = Self::LOCK_RELAUNCHES,
                "the lock screen keeps dying; revealing the desktop rather than \
                 leaving a screen nobody can get past"
            );
            self.lock = None;
            self.refresh_focus();
            self.queue_redraw();
            return false;
        }

        tracing::error!(
            program = crate::theme::LOCK_SCREEN,
            attempt = relaunches,
            "the lock screen died while the session was locked; starting it again"
        );
        let argv = [crate::theme::LOCK_SCREEN.to_string()];
        if crate::backend::spawn(&argv, &self.socket, self.x11_display) {
            return true;
        }

        tracing::error!(
            program = crate::theme::LOCK_SCREEN,
            "the lock screen would not start again; revealing the desktop"
        );
        self.lock = None;
        self.refresh_focus();
        self.queue_redraw();
        false
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

    /// The terminal to launch for the spawn binding: `desktop.toml`'s, or
    /// the compiled-in one.
    pub(crate) fn terminal_command(&self) -> &str {
        self.desktop_config.terminal()
    }

    /// Adopt the scale a newly-configured output decided on.
    ///
    /// The desktop is laid out in *logical* pixels, so this sets the window
    /// area from `scale.logical` rather than from the panel's real resolution:
    /// on a 4K 27" that is a 2560x1440 desktop over a 3840x2160 panel, which is
    /// what makes windows come out a sensible size instead of tiny.
    pub(crate) fn set_output_scale(
        &mut self,
        name: &str,
        output: Option<Output>,
        scale: OutputScale,
    ) {
        self.set_outputs(vec![OutputInfo {
            name: name.to_owned(),
            rect: Rect::from_xywh(0, 0, scale.logical.w, scale.logical.h),
            scale,
            mm: Size::ZERO,
            output,
        }]);
    }

    /// What was saved about where the screens go. See `huginn_core::layout`.
    pub(crate) fn output_layout(&self) -> &[huginn_core::layout::Saved] {
        &self.layout
    }

    /// Stage a position for a named screen; nothing moves until
    /// [`Self::apply_output_layout`].
    pub(crate) fn stage_output_position(&mut self, name: &str, x: i32, y: i32) {
        huginn_core::layout::set_position(
            &mut self.staged_layout,
            name,
            huginn_core::geometry::Point::new(x, y),
        );
    }

    /// Stage a scale override, or `None` to go back to the derived one.
    pub(crate) fn stage_output_scale(&mut self, name: &str, scale: Option<f64>) {
        if !self.staged_layout.iter().any(|s| s.name == name)
            && let Some(saved) = self.layout.iter().find(|s| s.name == name)
        {
            // Start from what is saved so a scale change keeps the position.
            self.staged_layout.push(saved.clone());
        }
        huginn_core::layout::set_scale(&mut self.staged_layout, name, scale);
    }

    /// Make the staged layout the saved one and ask the backend to re-arrange.
    ///
    /// Saved first, so a compositor that dies mid-reflow still comes back with
    /// the arrangement that was asked for.
    pub(crate) fn apply_output_layout(&mut self) {
        let staged = std::mem::take(&mut self.staged_layout);
        for entry in staged {
            if let Some(at) = entry.position {
                huginn_core::layout::set_position(&mut self.layout, &entry.name, at);
            }
            // A staged entry carries the scale it wants, including `None`
            // for "back to automatic".
            huginn_core::layout::set_scale(&mut self.layout, &entry.name, entry.scale);
        }
        self.save_layout();
        self.layout_changed = true;
    }

    /// Whether the layout was applied since the backend last arranged. Clears
    /// the flag: the backend calls this once per turn.
    pub(crate) fn take_layout_change(&mut self) -> bool {
        std::mem::take(&mut self.layout_changed)
    }

    fn save_layout(&self) {
        if let Some(file) = layout_path()
            && let Err(e) = save_text(&file, &huginn_core::layout::to_text(&self.layout))
        {
            tracing::warn!(
                "could not save the output layout to {}: {e}",
                file.display()
            );
        }
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

    /// Panes explicitly put in the background, on any workspace, with the
    /// application each belongs to.
    fn minimized_windows(&self) -> Vec<(huginn_core::window::WindowId, String)> {
        self.space
            .workspaces()
            .iter()
            .flat_map(|workspace| workspace.windows())
            .filter(|id| {
                self.space
                    .window(**id)
                    .is_some_and(huginn_core::window::Window::is_minimized)
            })
            .filter_map(|id| Some((*id, self.windows.get(id)?.app_id()?)))
            .collect()
    }

    pub(crate) fn app_switcher_open(&self) -> bool {
        self.app_switcher.is_some()
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
        if self.app_switcher.is_some() {
            return Some(crate::dock::centred_placement(
                self.output_area(),
                self.dock_items.len().max(1),
            ));
        }
        let now = self.uptime();
        if self.has_fullscreen() || !self.dock.is_visible(now) {
            return None;
        }
        Some(crate::dock::placement(
            self.output_area(),
            self.dock_items.len().max(1),
            self.dock.reveal(now),
        ))
    }

    /// Rebuild the dock strip from what is running.
    pub(crate) fn refresh_dock(&mut self) {
        if self.has_fullscreen() && self.app_switcher.is_none() {
            self.dock.hide_now();
            self.dock_panel = None;
            self.dock_previews.clear();
            self.dock_hover_since = None;
            self.queue_redraw();
            return;
        }
        self.dock_items = if self.app_switcher.is_some() {
            crate::dock::window_items(&self.apps, &self.minimized_windows())
        } else {
            crate::dock::items(&self.apps, &self.running_app_ids())
        };
        let selected = self.app_switcher.map(|switcher| switcher.selected);
        self.rebuild_dock_previews();
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        self.dock_panel = (self.app_switcher.is_some() || self.dock.is_visible(self.uptime()))
            .then(|| {
                crate::dock::render(
                    &self.dock_items,
                    &self.apps,
                    &self.icons,
                    &mut self.pixmaps,
                    &mut self.text,
                    area,
                    advertised,
                    selected,
                )
            });
        self.queue_redraw();
    }

    /// Which windows the strip should show pictures of, and where the row of
    /// them is centred: the switcher's highlighted window over the strip's
    /// middle, or every window of the hovered application over its tile.
    fn previewed_windows(&self) -> (Vec<huginn_core::window::WindowId>, Option<i32>) {
        let Some(dock) = self.dock_rect() else {
            return (Vec::new(), None);
        };
        if let Some(switcher) = self.app_switcher {
            let window = self
                .dock_items
                .get(switcher.selected)
                .and_then(|item| item.window);
            return (window.into_iter().collect(), Some(dock.x() + dock.w() / 2));
        }
        let Some(index) = self.dock_hover else {
            return (Vec::new(), None);
        };
        let Some(entry) = self
            .dock_items
            .get(index)
            .and_then(|item| item.entry)
            .and_then(|i| self.apps.get(i))
        else {
            return (Vec::new(), None);
        };
        // Every workspace, not just this one: the point of a picture is to
        // find a window you cannot see, and one put away elsewhere is
        // exactly that.
        let windows = self
            .space
            .workspaces()
            .iter()
            .flat_map(|workspace| workspace.windows().iter().copied())
            .filter(|id| self.mapped.contains(id))
            .filter(|id| {
                self.windows
                    .get(id)
                    .and_then(WindowSurface::app_id)
                    .is_some_and(|app_id| crate::dock::matches(entry, &app_id))
            })
            .collect();
        let tile = crate::dock::item_rect(dock, index);
        (windows, Some(tile.x() + tile.w() / 2))
    }

    /// Lay out and frame the thumbnails for [`Self::previewed_windows`].
    fn rebuild_dock_previews(&mut self) {
        self.dock_previews.clear();
        // Still waiting out the hover delay: `refresh_dock` runs on every
        // frame of the strip's own animation, and must not jump the queue.
        if self.app_switcher.is_none() && self.dock_hover_since.is_some() {
            return;
        }
        let (windows, anchor) = self.previewed_windows();
        let (Some(anchor), Some(dock)) = (anchor, self.dock_rect()) else {
            return;
        };
        let sized: Vec<(huginn_core::window::WindowId, (i32, i32))> = windows
            .into_iter()
            .filter_map(|id| {
                let placed = self.placed_rect(id)?;
                Some((id, (placed.w(), placed.h())))
            })
            .collect();
        let sizes: Vec<(i32, i32)> = sized.iter().map(|(_, size)| *size).collect();
        // Laid out twice: the captions need the frames' widths to fit under,
        // and the frames need the captions' height to stand above. The second
        // pass changes only where the row sits, not how wide anything is.
        let frames = crate::dock::preview_row(&sizes, anchor, dock, self.output_area(), 0);
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        let captions: Vec<Option<crate::canvas::Panel>> = sized
            .iter()
            .zip(&frames)
            .map(|((id, _), frame)| {
                let title = self.windows.get(id)?.title()?;
                let backing = crate::dock::preview_backing(*frame, area);
                crate::dock::caption(&mut self.text, &title, backing.w(), area, advertised)
            })
            .collect();
        let reserve = captions
            .iter()
            .flatten()
            .map(|caption| crate::dock::caption_room(caption, self.output_area()))
            .max()
            .unwrap_or(0);
        let frames = crate::dock::preview_row(&sizes, anchor, dock, self.output_area(), reserve);
        self.dock_previews = sized
            .into_iter()
            .zip(frames)
            .zip(captions)
            .map(|(((window, _), frame), caption)| DockPreview {
                window,
                frame,
                panel: crate::dock::preview_frame(
                    frame,
                    self.output_area(),
                    self.scale().advertised,
                ),
                caption,
            })
            .collect();
    }

    /// Where `id`'s surface would be drawn at full size, if it has one.
    fn placed_rect(&self, id: huginn_core::window::WindowId) -> Option<Rect> {
        let surface = self.windows.get(&id)?.wl_surface()?;
        let placed = place_in_pane(&surface, self.space.window(id)?.geometry);
        (placed.w() > 0 && placed.h() > 0).then_some(placed)
    }

    /// Where the window is on screen at `now`: part way through a motion if
    /// it has one, otherwise wherever its buffer sits in its pane.
    fn drawn_rect(&self, id: WindowId, now: std::time::Duration) -> Option<Rect> {
        match self.motions.get(&id) {
            Some(motion) => Some(motion.rect_at(now)),
            None => self.placed_rect(id),
        }
    }

    /// The dock tile a minimized window goes to and comes back from.
    ///
    /// Its application's tile when the dock shows one; otherwise — the dock
    /// hidden, or the last window of an application that is not pinned — the
    /// middle of the bottom edge, which is where the dock lives even when it
    /// is not showing.
    fn minimize_target(&self, id: WindowId) -> Rect {
        let tile = self.dock_rect().and_then(|dock| {
            let app_id = self.windows.get(&id)?.app_id()?;
            let index = self.dock_items.iter().position(|item| {
                item.entry
                    .and_then(|entry| self.apps.get(entry))
                    .is_some_and(|entry| crate::dock::matches(entry, &app_id))
            })?;
            Some(crate::dock::item_rect(dock, index))
        });
        tile.unwrap_or_else(|| {
            let area = self.output_area();
            let side = crate::dock::placement(area, 1, 1.0).h();
            Rect::from_xywh(
                area.x() + (area.w() - side) / 2,
                area.bottom() - side,
                side,
                side,
            )
        })
    }

    /// Start or redirect a motion for every window `arrange` moved.
    ///
    /// `before` is where each visible window was drawn going in; `changed` is
    /// what the core reports coming out. Only a change of *size* starts a
    /// motion — a pane carried sideways at the same size is the carousel
    /// scrolling, which is already an animation, and a motion in flight is
    /// carried along with it rather than left behind to finish at a place the
    /// strip has since left.
    fn start_motions(
        &mut self,
        before: &[(WindowId, Rect)],
        changed: &[(WindowId, Rect)],
        now: std::time::Duration,
    ) {
        let instant = self.reduced_motion();
        for (id, to) in changed {
            let Some((_, from)) = before.iter().find(|(other, _)| other == id) else {
                continue;
            };
            let resized = from.w() != to.w() || from.h() != to.h();
            match self.motions.get_mut(id) {
                Some(motion) if !resized => {
                    let was = motion.to();
                    motion.shift(to.x() - was.x(), to.y() - was.y());
                }
                Some(motion) => motion.retarget(*to, now, instant),
                None if resized => {
                    self.motions
                        .insert(*id, crate::motion::Motion::resize(*from, *to, now, instant));
                }
                None => {}
            }
        }
    }

    /// Whether window motion should skip to the end. See
    /// [`crate::settings::Motion`].
    fn reduced_motion(&self) -> bool {
        self.settings.motion() == crate::settings::Motion::Reduced
    }

    /// The active workspace's windows as scene items, frontmost first: any
    /// pane still flying into the dock on top, then the tiles, each drawn at
    /// its motion's rectangle if it has one and at its pane if not.
    fn window_items(&self) -> Vec<SceneItem<'_>> {
        let now = self.uptime();
        let mut out = Vec::new();
        // A ghost: the layout no longer has the window, but the screen still
        // does until it reaches the dock. A picture rather than a surface,
        // since there is nothing there to click on any more.
        for (id, motion) in &self.motions {
            if motion.kind() != crate::motion::Kind::Minimize {
                continue;
            }
            let Some(surface) = self.windows.get(id).and_then(WindowSurface::wl_surface) else {
                continue;
            };
            let Some(placed) = self.placed_rect(*id) else {
                continue;
            };
            let transform = crate::motion::fit(placed, motion.rect_at(now), motion.alpha_at(now));
            out.push(SceneItem::Preview(surface, placed, transform));
        }
        for id in &self.visible_window_ids() {
            if !self.mapped.contains(id)
                || self
                    .space
                    .window(*id)
                    .is_none_or(huginn_core::window::Window::is_minimized)
            {
                continue;
            }
            let Some(surface) = self.windows.get(id).and_then(WindowSurface::wl_surface) else {
                continue;
            };
            let Some(placed) = self.placed_rect(*id) else {
                continue;
            };
            match self.motions.get(id) {
                // A resize: the buffer at its natural size, its frame's
                // corner pinned to the moving rectangle's corner, cropped
                // to it. The content never stretches; the edge slides.
                Some(motion) if motion.kind() == crate::motion::Kind::Resize => {
                    let drawn = motion.rect_at(now);
                    let frame = crate::popup::window_geometry(&surface);
                    let origin = Rect::from_xywh(
                        drawn.x() - frame.loc.x,
                        drawn.y() - frame.loc.y,
                        placed.w(),
                        placed.h(),
                    );
                    out.push(SceneItem::Clipped(
                        surface,
                        origin,
                        drawn,
                        motion.alpha_at(now),
                    ));
                }
                // Coming back from the dock: genuinely growing, so scaled,
                // and hit-tested at its real rectangle, which is where it
                // is about to be.
                Some(motion) => {
                    let transform =
                        crate::motion::fit(placed, motion.rect_at(now), motion.alpha_at(now));
                    out.push(SceneItem::WorkspaceSurface(surface, placed, transform));
                }
                // Keep ordinary tiled panes cropped too. The resize spring can
                // settle before a slow client has committed the configured
                // size; drawing the old buffer unrestricted for that interval
                // is the one-frame flash into a sibling pane. Floating and
                // fullscreen windows are not nested and keep their natural
                // surface bounds.
                None if self
                    .space
                    .window(*id)
                    .is_some_and(|window| window.is_tiled()) =>
                {
                    let pane = self
                        .space
                        .window(*id)
                        .expect("window checked above")
                        .geometry;
                    out.push(SceneItem::Clipped(surface, placed, pane, 1.0));
                }
                None => out.push(SceneItem::Surface(surface, placed)),
            }
        }
        out
    }

    /// The scene items for the thumbnails, frontmost first.
    fn dock_preview_items(&self) -> Vec<SceneItem<'_>> {
        let mut out = Vec::new();
        for preview in &self.dock_previews {
            let Some(surface) = self
                .windows
                .get(&preview.window)
                .and_then(WindowSurface::wl_surface)
            else {
                continue;
            };
            // Re-placed each frame rather than remembered: the client may
            // commit a new size while the pictures are up, and they should
            // follow.
            let Some(placed) = self.placed_rect(preview.window) else {
                continue;
            };
            let frame = preview.frame;
            let scale = (f64::from(frame.w()) / f64::from(placed.w()))
                .min(f64::from(frame.h()) / f64::from(placed.h()));
            // Centred inside the frame: the frame is sized to the window's
            // aspect, but rounding leaves a pixel or two either way.
            let (w, h) = (f64::from(placed.w()) * scale, f64::from(placed.h()) * scale);
            let x = f64::from(frame.x()) + (f64::from(frame.w()) - w) / 2.0;
            let y = f64::from(frame.y()) + (f64::from(frame.h()) - h) / 2.0;
            // The renderer scales about the origin and then shifts, so the
            // shift is where the box is minus where the scaled window would
            // have landed.
            let transform = WorkspacePreview {
                scale_x: scale,
                scale_y: scale,
                offset_x: x - f64::from(placed.x()) * scale,
                offset_y: y - f64::from(placed.y()) * scale,
                alpha: 1.0,
            };
            let backing = crate::dock::preview_backing(frame, self.output_area());
            out.push(SceneItem::Preview(surface, placed, transform));
            out.push(SceneItem::Overlay(preview.panel.buffer(), backing, 1.0));
            if let Some(caption) = &preview.caption {
                let below = crate::dock::caption_placement(caption, backing, self.output_area());
                out.push(SceneItem::Overlay(caption.buffer(), below, 1.0));
            }
        }
        out
    }

    /// Tell the dock where the pointer went.
    pub(crate) fn dock_pointer_moved(&mut self) {
        let now = self.uptime();
        let pointer = self.pointer_point();
        let over_dock = self.dock_rect().is_some_and(|rect| rect.contains(pointer));
        // While the strip is travelling, its destination is an approach area.
        // Without this, moving upward from the reveal edge can leave both the
        // narrow edge band and the dock's still-moving rectangle, which
        // immediately reverses the reveal before the pointer can reach an
        // icon. Actual item hover below still uses `over_dock`, so previews do
        // not appear ahead of the visible strip.
        let approaching_dock = self.app_switcher.is_none()
            && self.dock.is_animating(now)
            && crate::dock::placement(self.output_area(), self.dock_items.len().max(1), 1.0)
                .contains(pointer);
        let motion = self.settings.motion();
        let y = self.pointer_location.y.round() as i32;
        if self.dock.pointer_moved(
            y,
            self.output_area(),
            over_dock || approaching_dock,
            now,
            motion,
        ) {
            self.refresh_dock();
        }
        // Pictures of the hovered application's windows. Only the ordinary
        // dock: the switcher chooses its own picture, by selection.
        let hover = if self.app_switcher.is_none() && over_dock {
            self.dock_rect().and_then(|rect| {
                self.dock
                    .item_at(self.pointer_point().x, rect, self.dock_items.len())
            })
        } else {
            None
        };
        if hover != self.dock_hover {
            // Pictures already up follow the pointer along the strip at
            // once; arriving from elsewhere waits out the delay.
            let browsing = !self.dock_previews.is_empty();
            self.dock_hover = hover;
            self.dock_previews.clear();
            self.dock_hover_since = None;
            if hover.is_some() {
                if browsing {
                    self.rebuild_dock_previews();
                } else {
                    self.dock_hover_since = Some(now);
                }
            }
            self.queue_redraw();
        }
    }

    /// Fingers landed on the touchpad.
    ///
    /// Nothing is claimed yet — see [`crate::gesture`].
    ///
    /// A swipe still in flight is *ended* rather than dropped. libinput sends
    /// an end for every begin, so one arriving here means the last gesture's
    /// end went missing. Ending it first keeps an interrupted gesture from
    /// leaving the workspace switcher open with nothing controlling it.
    pub(crate) fn swipe_begin(&mut self, fingers: u32) {
        if self.swipe.is_some() {
            tracing::debug!("a touchpad swipe began before the last one ended");
            self.swipe_end();
        }
        if let Some(switcher) = &mut self.app_switcher {
            switcher.dismiss_at = self.started.elapsed() + APP_SWITCHER_TIMEOUT;
        }
        self.swipe = Some(crate::gesture::Swipe::new(fingers));
    }

    /// The fingers moved. Opens and drives the workspace row once the swipe has
    /// proved itself a horizontal three-finger one.
    pub(crate) fn swipe_update(&mut self, dx: f64, dy: f64) {
        let Some(mut swipe) = self.swipe.take() else {
            return;
        };
        if swipe.takes_hold(dx, dy) == Some(crate::gesture::Hold::Horizontal) {
            if let Some(switcher) = self.app_switcher {
                let selectable = self.switcher_items();
                let origin = selectable
                    .iter()
                    .position(|index| *index == switcher.selected)
                    .unwrap_or(0) as f32;
                swipe.drives(origin);
            } else {
                let origin = self.space.active_index() as f32;
                self.open_workspace_carousel();
                swipe.drives(origin);
            }
        }
        if let Some(position) = swipe.position() {
            if self.app_switcher.is_some() {
                let selectable = self.switcher_items();
                let last = selectable.len().saturating_sub(1) as f32;
                let ordinal = position.round().clamp(0.0, last) as usize;
                if let Some(&selected) = selectable.get(ordinal)
                    && self.app_switcher.is_some_and(|s| s.selected != selected)
                {
                    self.app_switcher = Some(AppSwitcher {
                        selected,
                        dismiss_at: self.uptime() + APP_SWITCHER_TIMEOUT,
                    });
                    self.refresh_dock();
                }
            } else {
                let last = self.space.workspaces().len().saturating_sub(1) as f32;
                if let Some(carousel) = &mut self.workspace_carousel {
                    carousel.position.jump_to(position.clamp(0.0, last));
                    self.queue_redraw();
                }
            }
        }
        self.swipe = Some(swipe);
    }

    /// The fingers lifted, or libinput gave up on the gesture.
    ///
    /// A cancelled swipe settles exactly like a completed one. The fingers are
    /// off the pad either way, and leaving the workspace row between cards
    /// because the touchpad lost track of a finger would look broken.
    pub(crate) fn swipe_end(&mut self) {
        let Some(swipe) = self.swipe.take() else {
            return;
        };
        if let Some(direction) = swipe.vertical() {
            match direction {
                crate::gesture::Vertical::Down if self.app_switcher.is_none() => {
                    self.minimize_focused();
                }
                crate::gesture::Vertical::Up if self.app_switcher.is_some() => {
                    self.accept_app_switcher();
                }
                _ => {}
            }
            return;
        }
        if swipe.position().is_none() {
            return;
        }
        if self.app_switcher.is_none() {
            self.close_workspace_carousel();
        }
    }

    /// Dock indices that can be restored by the application switcher.
    fn switcher_items(&self) -> Vec<usize> {
        self.dock_items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.running.then_some(index))
            .collect()
    }

    /// A stationary three-finger contact can represent each half of a double tap.
    pub(crate) fn hold_begin(&mut self, fingers: u32) {
        self.hold_fingers = Some(fingers);
    }

    pub(crate) fn hold_end(&mut self, cancelled: bool, time_msec: u32) {
        let fingers = self.hold_fingers.take().unwrap_or_default();
        if !cancelled && self.double_tap.tap(fingers, time_msec) {
            self.open_app_switcher();
        }
    }

    /// Tap-to-click touchpads commonly encode a three-finger tap as middle click.
    pub(crate) fn middle_tap(&mut self, time_msec: u32) {
        if self
            .double_tap
            .tap(crate::gesture::CAROUSEL_FINGERS, time_msec)
        {
            self.open_app_switcher();
        }
    }

    /// Put `id` into or out of fullscreen: the core's geometry, the client's
    /// `fullscreen` state, and the configure that carries both.
    ///
    /// The one path for every way a window can go fullscreen — a client asking
    /// for itself, an X11 window setting `_NET_WM_STATE`, the app switcher
    /// restoring one — so they cannot disagree about what fullscreen is: the
    /// whole output, at the output's logical size, above the panels.
    ///
    /// The configure goes out unconditionally rather than through `arrange`'s
    /// changed-only filter. A lone window with no panels already sits at the
    /// output's size, so its geometry does not change on the way in; the state
    /// still has to reach it or it never learns it is fullscreen.
    pub(crate) fn set_fullscreen(&mut self, id: WindowId, on: bool) {
        if !self.space.set_fullscreen(id, on) {
            return;
        }
        tracing::debug!(window = id.raw(), on, "fullscreen");
        if let Some(surface) = self.windows.get(&id) {
            surface.set_fullscreen(on);
        }
        self.arrange();
        if let Some(surface) = self.windows.get(&id) {
            surface.send_configure();
        }
        self.refresh_focus();
        self.refresh_dock();
    }

    /// The window an XDG toplevel belongs to.
    fn xdg_window_id(&self, surface: &ToplevelSurface) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|(_, w)| w.as_xdg() == Some(surface))
            .map(|(id, _)| *id)
    }

    /// Put only the highlighted pane in the background, leaving the workspace usable.
    fn minimize_focused(&mut self) {
        if let Some(id) = self.space.focused() {
            self.minimize_window(id);
        }
    }

    /// Put one window away to the dock, with the flight that shows where it
    /// went. The gesture and the chord minimize the focused one; a client's
    /// own minimize button — `xdg_toplevel.set_minimized` — names its window.
    pub(crate) fn minimize_window(&mut self, id: WindowId) {
        let now = self.uptime();
        // Where it is on screen right now, before the core forgets it is
        // there: the flight to the dock starts from here.
        let from = self.drawn_rect(id, now);
        if !self.space.minimize(id) {
            return;
        }
        if let Some(surface) = self.windows.get(&id) {
            surface.set_fullscreen(false);
            surface.send_configure();
        }
        self.arrange();
        self.refresh_dock();
        self.refresh_focus();
        // After `refresh_dock`, so the tile it flies to is the one the dock
        // now shows — and after `arrange`, which would otherwise read this as
        // a resize (it is not: the window is leaving, not moving).
        if let Some(from) = from {
            let to = crate::motion::fit_aspect(from, self.minimize_target(id));
            let instant = self.reduced_motion();
            self.motions
                .insert(id, crate::motion::Motion::minimize(from, to, now, instant));
            self.queue_redraw();
        }
    }

    /// Temporarily promote the minimized-application dock over the workspace.
    fn open_app_switcher(&mut self) {
        self.dock_items = crate::dock::window_items(&self.apps, &self.minimized_windows());
        let selected = self.switcher_items().into_iter().next();
        let Some(selected) = selected else {
            return;
        };
        self.workspace_carousel = None;
        self.app_switcher = Some(AppSwitcher {
            selected,
            dismiss_at: self.uptime() + APP_SWITCHER_TIMEOUT,
        });
        self.refresh_dock();
        self.refresh_focus();
    }

    /// Close the temporary dock without restoring anything.
    pub(crate) fn dismiss_app_switcher(&mut self) {
        if self.app_switcher.take().is_none() {
            return;
        }
        self.refresh_dock();
        self.refresh_focus();
    }

    /// Restore the highlighted application into the current workspace.
    ///
    /// The window comes back as an ordinary tile filling the layout — not
    /// fullscreen. Fullscreen tells the client it owns the whole screen, and a
    /// browser answers by hiding its tab strip and chrome, which is not what
    /// sliding an application back up from the bar means.
    fn accept_app_switcher(&mut self) {
        let Some(switcher) = self.app_switcher.take() else {
            return;
        };
        // The tile names its window outright; there is nothing to search for.
        // Checked that it is still minimized, since the window may have been
        // closed or restored some other way while the switcher was up.
        let minimized = self
            .dock_items
            .get(switcher.selected)
            .and_then(|item| item.window)
            .filter(|id| {
                self.space
                    .window(*id)
                    .is_some_and(|window| window.is_minimized())
            });
        if let Some(id) = minimized {
            self.restore_window(id);
        }
        self.refresh_dock();
    }

    /// Bring a minimized window back onto the active workspace as an ordinary
    /// tile, and focus it. See [`Self::accept_app_switcher`] for why not
    /// fullscreen.
    fn restore_window(&mut self, id: huginn_core::window::WindowId) {
        self.space.bring_to_active_workspace(id);
        self.space.unminimize(id);
        if let Some(surface) = self.windows.get(&id) {
            surface.set_fullscreen(false);
        }
        self.arrange();
        if let Some(surface) = self.windows.get(&id) {
            surface.send_configure();
        }
        // Grow out of the dock tile it was put away into, into the tile the
        // layout just gave it. This replaces whatever `arrange` started for
        // it: from the layout's point of view the window merely changed size,
        // but it was not on screen to change size from.
        if self.mapped.contains(&id)
            && let Some(window) = self.space.window(id)
        {
            let now = self.uptime();
            let to = window.geometry;
            let from = crate::motion::fit_aspect(to, self.minimize_target(id));
            let instant = self.reduced_motion();
            self.motions
                .insert(id, crate::motion::Motion::restore(from, to, now, instant));
        }
        self.refresh_focus();
    }

    /// Open the workspace Cover Flow at the active workspace.
    pub(crate) fn open_workspace_carousel(&mut self) {
        let now = self.uptime();
        let duration = self
            .settings
            .motion()
            .duration(crate::anim::WORKSPACE_CAROUSEL_OPEN);
        if let Some(carousel) = &mut self.workspace_carousel {
            // A fresh gesture may arrive while the previous selection is still
            // expanding. Reverse that motion from its current value instead of
            // letting the old close finish underneath the new fingers.
            carousel
                .reveal
                .animate_to(1.0, now, duration, crate::anim::Curve::EaseOut);
            carousel.closing = false;
            self.queue_redraw();
            return;
        }
        let mut reveal = crate::anim::Animated::settled(0.0);
        reveal.animate_to(1.0, now, duration, crate::anim::Curve::EaseOut);
        self.workspace_carousel = Some(WorkspaceCarousel {
            position: crate::anim::Animated::settled(self.space.active_index() as f32),
            reveal,
            closing: false,
        });
        self.queue_redraw();
    }

    /// Select the nearest card and expand it back to a normal workspace.
    pub(crate) fn close_workspace_carousel(&mut self) {
        let now = self.uptime();
        let last = self.space.workspaces().len().saturating_sub(1);
        let duration = self
            .settings
            .motion()
            .duration(crate::anim::WORKSPACE_CAROUSEL_CLOSE);
        let Some(carousel) = &mut self.workspace_carousel else {
            return;
        };
        let target = carousel.position.value(now).round().clamp(0.0, last as f32) as usize;
        carousel
            .position
            .animate_to(target as f32, now, duration, crate::anim::Curve::EaseInOut);
        carousel
            .reveal
            .animate_to(0.0, now, duration, crate::anim::Curve::EaseInOut);
        carousel.closing = true;
        self.space.activate_workspace(target);
        self.arrange();
        self.refresh_focus();
    }

    /// Keyboard counterpart: first press opens, second accepts the centre card.
    pub(crate) fn toggle_workspace_carousel(&mut self) {
        if self.workspace_carousel.is_some() {
            self.close_workspace_carousel();
        } else {
            self.open_workspace_carousel();
        }
    }

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
            // Saved on every launch rather than at exit: a compositor does not
            // reliably get an exit, and the file is a few hundred bytes. The
            // prune is here too, since this is the one time the file is
            // rewritten and a score cannot fall below the threshold between
            // two launches without being written here.
            self.frecency.prune(now);
            if let Some(file) = frecency_path()
                && let Err(e) = self.frecency.save(&file)
            {
                tracing::warn!("could not save launch history to {}: {e}", file.display());
            }
        }
        // The answer is not used here: a desktop entry that will not run is a
        // line in the log, not something the launcher can do anything about.
        let _ = crate::backend::spawn(argv, &self.socket, self.x11_display);
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

    /// Enter or leave keyboard resize mode and mirror it to the focused
    /// xdg-toplevel.
    ///
    /// Keeping this transition beside the layout operation matters for
    /// third-party clients: GTK, Qt and Chromium all understand the standard
    /// `resizing` state, while none can know about Raven's private key mode.
    /// The state-only configure on entry and exit brackets the size configures
    /// produced by [`Self::resize_focused`].
    pub(crate) fn set_resize_mode(&mut self, resizing: bool) {
        if self.resizing == resizing {
            return;
        }
        self.resizing = resizing;
        if let Some(surface) = self.space.focused().and_then(|id| self.windows.get(&id)) {
            surface.set_resizing(resizing);
            surface.send_configure();
        }
    }

    /// Open the launcher, growing it out of the dock's launcher icon.
    ///
    /// The origin is only taken when the dock is actually on screen — §4 asks
    /// for the motion to start at the icon, and starting it off the bottom of
    /// the screen because the dock happens to be hidden is motion the eye
    /// cannot follow. Without one it grows in place from the centre.
    pub(crate) fn open_launcher(&mut self) {
        self.request_file_index_if_stale();
        let origin = self.dock_rect().map(|dock| crate::dock::item_rect(dock, 0));
        let (now, clock, motion) = (self.now(), self.uptime(), self.settings.motion());
        self.launcher.set_pinned(self.pins.paths().to_vec());
        self.launcher
            .open(&self.apps, &self.frecency, now, origin, clock, motion);
        self.refresh_launcher();
    }

    /// Apply a keystroke to the launcher, and act on what it asks for.
    pub(crate) fn launcher_key(&mut self, key: crate::launcher::Key) {
        let (now, clock, motion) = (self.now(), self.uptime(), self.settings.motion());
        let outcome = self
            .launcher
            .press(key, &self.apps, &self.frecency, now, clock, motion);
        self.act_on_launcher(outcome);
    }

    /// The pointer, as a pixel of the launcher's canvas, when it is over the
    /// open launcher. `None` when the launcher is closed, closing, or the
    /// pointer is somewhere else.
    ///
    /// Goes through the same [`crate::launcher::placement`] the scene draws
    /// the panel with, at the same reveal, so what the pointer is over is
    /// what is on screen — including during the opening motion, when the
    /// panel is smaller than its canvas and off to one side of centre.
    fn launcher_canvas_point(&self) -> Option<huginn_core::geometry::Point> {
        if !self.launcher.is_open() {
            return None;
        }
        let panel = self.launcher_panel.as_ref()?;
        let placed = crate::launcher::placement(
            self.output_area(),
            panel.size(),
            self.launcher.origin(),
            self.launcher.reveal(self.uptime()),
        );
        self.launcher
            .layout()
            .canvas_point(placed, self.pointer_point())
    }

    /// Whether the open launcher is under the pointer.
    ///
    /// The launcher is compositor-drawn, so as far as any client knows the
    /// pointer is over whatever window is behind it. The input backend asks
    /// this before forwarding motion, so a window under the panel is not told
    /// about a pointer that is, to the user's eye, on the panel.
    pub(crate) fn launcher_covers_pointer(&self) -> bool {
        self.launcher_canvas_point().is_some()
    }

    /// Tell the launcher where the pointer went: the highlight follows it.
    pub(crate) fn launcher_pointer_moved(&mut self) {
        let Some(point) = self.launcher_canvas_point() else {
            return;
        };
        let outcome = self.launcher.hover(point);
        self.act_on_launcher(outcome);
    }

    /// A press while the launcher is open. Returns whether it was taken.
    ///
    /// On the panel, the click goes to the launcher, which launches what it
    /// landed on. Anywhere else it dismisses the launcher, as Escape would:
    /// clicking away is the universal gesture for "not this", and a panel
    /// that stayed up over a window the user had just clicked into would be
    /// in the way of exactly the thing they turned to. Either way the click
    /// is swallowed rather than forwarded — it was aimed at the launcher, or
    /// at getting rid of it, and not at whatever happens to be behind.
    pub(crate) fn launcher_click(&mut self) -> bool {
        if !self.launcher.is_open() {
            return false;
        }
        match self.launcher_canvas_point() {
            Some(point) => {
                let (clock, motion) = (self.uptime(), self.settings.motion());
                let outcome = self.launcher.click(point, &self.apps, clock, motion);
                self.act_on_launcher(outcome);
            }
            None => self.launcher_key(crate::launcher::Key::Dismiss),
        }
        true
    }

    /// Do what the launcher asked for after a key or a click: run what it
    /// chose, and redraw it when it changed. One place for both, so a mouse
    /// launch and a keyboard launch cannot be credited differently.
    fn act_on_launcher(&mut self, outcome: crate::launcher::Outcome) {
        match outcome {
            crate::launcher::Outcome::Launch { entry, argv } => {
                self.launch(entry, &argv);
                self.refresh_launcher();
            }
            crate::launcher::Outcome::Dismissed | crate::launcher::Outcome::Redraw => {
                self.refresh_launcher();
            }
            crate::launcher::Outcome::TogglePin { entry } => {
                self.toggle_pin(&entry);
                self.refresh_launcher();
            }
            crate::launcher::Outcome::Unchanged => {}
        }
    }

    /// Pin `entry`, or unpin it if it is, and tell everyone who shows pins.
    fn toggle_pin(&mut self, entry: &std::path::Path) {
        let pinned = self.pins.toggle(entry);
        tracing::info!(entry = %entry.display(), pinned, "pin toggled");
        self.pins_changed();
    }

    /// The pin list or its layout changed: save it, and bring every view of
    /// it up to date — the launcher's menu label, the pinned panel's items.
    fn pins_changed(&mut self) {
        self.save_pins();
        self.launcher.set_pinned(self.pins.paths().to_vec());
        let clock = self.uptime();
        if self.pinned.is_visible(clock) {
            self.pinned.refresh(&self.apps, &self.pins);
            self.refresh_pinned();
        }
    }

    /// Write the pins beside the launch history. Saved on every change
    /// rather than at exit, for the reason the history is.
    fn save_pins(&self) {
        if let Some(file) = pins_path()
            && let Err(e) = self.pins.save(&file)
        {
            tracing::warn!("could not save pins to {}: {e}", file.display());
        }
    }

    /// Open the pinned panel where quick settings put it.
    pub(crate) fn open_pinned(&mut self) {
        let (clock, motion) = (self.uptime(), self.settings.motion());
        self.pinned.open(&self.apps, &self.pins, clock, motion);
        self.refresh_pinned();
    }

    /// Apply a keystroke to the pinned panel, and act on what it asks for.
    pub(crate) fn pinned_key(&mut self, key: crate::pinned::Key) {
        let (clock, motion) = (self.uptime(), self.settings.motion());
        let outcome = self
            .pinned
            .press(key, &self.apps, &mut self.pins, clock, motion);
        self.act_on_pinned(outcome);
    }

    /// The pointer, as a pixel of the pinned panel's canvas, when it is over
    /// the open panel. See [`Huginn::launcher_canvas_point`].
    fn pinned_canvas_point(&self) -> Option<huginn_core::geometry::Point> {
        if !self.pinned.is_open() {
            return None;
        }
        let panel = self.pinned_panel.as_ref()?;
        let placed = crate::pinned::placement(
            self.output_area(),
            panel.size(),
            self.pinned.position(),
            self.pinned.reveal(self.uptime()),
        );
        self.pinned
            .layout()
            .canvas_point(placed, self.pointer_point())
    }

    /// Whether the open pinned panel is under the pointer.
    pub(crate) fn pinned_covers_pointer(&self) -> bool {
        self.pinned_canvas_point().is_some()
    }

    /// Tell the pinned panel where the pointer went.
    pub(crate) fn pinned_pointer_moved(&mut self) {
        let Some(point) = self.pinned_canvas_point() else {
            return;
        };
        let outcome = self.pinned.hover(point);
        self.act_on_pinned(outcome);
    }

    /// A press while the pinned panel is open. Returns whether it was
    /// taken: on the panel it opens what it landed on, anywhere else it
    /// dismisses the panel, as Escape would.
    pub(crate) fn pinned_click(&mut self) -> bool {
        if !self.pinned.is_open() {
            return false;
        }
        match self.pinned_canvas_point() {
            Some(point) => {
                let (clock, motion) = (self.uptime(), self.settings.motion());
                let outcome = self
                    .pinned
                    .click(point, &self.apps, &mut self.pins, clock, motion);
                self.act_on_pinned(outcome);
            }
            None => self.pinned_key(crate::pinned::Key::Dismiss),
        }
        true
    }

    /// Do what the pinned panel asked for after a key or a click.
    fn act_on_pinned(&mut self, outcome: crate::pinned::Outcome) {
        match outcome {
            crate::pinned::Outcome::Launch { entry, argv } => {
                self.launch(Some(entry), &argv);
                self.refresh_pinned();
            }
            crate::pinned::Outcome::Changed => self.pins_changed(),
            crate::pinned::Outcome::Dismissed | crate::pinned::Outcome::Redraw => {
                self.refresh_pinned();
            }
            crate::pinned::Outcome::Unchanged => {}
        }
    }

    /// Redraw the pinned panel, or drop it when it is closed.
    pub(crate) fn refresh_pinned(&mut self) {
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        self.pinned_panel = self.pinned.is_visible(self.uptime()).then(|| {
            let (panel, layout) = crate::pinned::render(
                &self.pinned,
                &self.apps,
                &mut self.text,
                &self.icons,
                &mut self.pixmaps,
                area,
                advertised,
            );
            self.pinned.set_layout(layout);
            panel
        });
        self.queue_redraw();
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
        // Focus the first visible window belonging to it on this workspace.
        // Failing that, bring back one it has put away: a running indicator
        // under an icon whose click does nothing reads as broken, and the
        // one thing a minimized window wants from its icon is to come back.
        let visible = self
            .space
            .active_workspace()
            .windows()
            .iter()
            .copied()
            .filter(|id| {
                self.space
                    .window(*id)
                    .is_some_and(|window| !window.is_minimized())
            })
            .find(|id| {
                self.windows
                    .get(id)
                    .and_then(WindowSurface::app_id)
                    .is_some_and(|app_id| crate::dock::matches(entry, &app_id))
            });
        if let Some(id) = visible {
            self.space.active_workspace_mut().focus(id);
            self.refresh_focus();
            return;
        }
        let minimized = self
            .minimized_windows()
            .into_iter()
            .find(|(_, app_id)| crate::dock::matches(entry, app_id))
            .map(|(id, _)| id);
        if let Some(id) = minimized {
            self.restore_window(id);
            self.refresh_dock();
        }
    }

    /// How many leading scene items sit *above* the blur.
    ///
    /// The help overlay, the launcher and quick settings, in that order — each
    /// present only when it is. Counted rather than hardcoded so the boundary
    /// cannot drift from the order [`Huginn::scene`] actually pushes in.
    ///
    /// With no panel open and a glass window on screen the renderer ignores
    /// this and splits at the window's own elements — see [`Self::glass_surface`].
    pub(crate) fn blur_boundary(&self) -> usize {
        usize::from(self.help.is_some())
            + usize::from(self.volume_panel.is_some())
            + usize::from(self.launcher_panel.is_some())
            + usize::from(self.pinned_panel.is_some())
            + usize::from(self.settings_panel.is_some())
    }

    /// The glass window's surface, when it — not a panel — is what the blur
    /// is for. The renderer splits the scene at its elements; see
    /// `render::elements_split`.
    pub(crate) fn glass_surface(&self) -> Option<WlSurface> {
        if self.panel_blur_open() {
            return None;
        }
        let (id, _) = self.glass_window()?;
        self.windows.get(&id)?.wl_surface()
    }

    /// Whether a panel that blurs is on screen (mid-animation included).
    fn panel_blur_open(&self) -> bool {
        self.launcher_panel.is_some() || self.pinned_panel.is_some()
    }

    /// Windows that draw themselves translucent and ask for the desktop
    /// blurred behind them, by `app_id`. Raven Settings is the one today:
    /// its glass look is only glass with something soft behind it.
    const GLASS_APP_IDS: &'static [&'static str] = &["raven-settings", "com.ravensettings.Raven"];

    /// The glass window on screen, if `desktop.toml` allows blur and one is
    /// mapped and not put away: the focused one first, else any.
    pub(crate) fn glass_window(&self) -> Option<(WindowId, Rect)> {
        if !self.desktop_config.appearance.blur {
            return None;
        }
        let now = self.uptime();
        let is_glass = |id: &WindowId| {
            self.mapped.contains(id)
                && self
                    .space
                    .window(*id)
                    .is_some_and(|w| !w.is_minimized())
                && self
                    .windows
                    .get(id)
                    .and_then(|w| w.app_id())
                    .is_some_and(|app| Self::GLASS_APP_IDS.iter().any(|g| g.eq_ignore_ascii_case(&app)))
        };
        let id = self
            .space
            .focused()
            .filter(is_glass)
            .or_else(|| self.space.active_workspace().windows().iter().copied().find(is_glass))?;
        let rect = self.drawn_rect(id, now)?;
        Some((id, rect))
    }

    /// How blurred the desktop behind the panels should be, in pixels.
    ///
    /// Zero when no panel is open, which is what lets the renderer take the
    /// ordinary path unchanged for the overwhelming majority of frames. A
    /// glass window asks for the full radius for as long as it is there.
    pub(crate) fn blur_radius(&self) -> f32 {
        let clock = self.uptime();
        let panels = crate::blur::radius_for(self.launcher.reveal(clock).max(self.pinned.reveal(clock)));
        if panels > 0.0 || self.panel_blur_open() {
            return panels;
        }
        if self.glass_window().is_some() {
            crate::blur::MAX_RADIUS
        } else {
            0.0
        }
    }

    /// Where on the output the blurred desktop shows through: the launcher
    /// panel's placement, inset by its corner radius. See
    /// [`crate::launcher::blur_rect`] for why the inset.
    ///
    /// The whole desktop is blurred into the texture regardless — the blur
    /// kernel needs the pixels past the panel's edge to soften the ones just
    /// inside it — but only this much of the texture is drawn. `None` when
    /// there is no panel, or it is still too small to blur, and the renderer
    /// draws the desktop sharp.
    ///
    /// Computed from the same placement [`Huginn::scene`] pushes, so the blur
    /// cannot drift from the panel as it animates.
    pub(crate) fn blur_rect(&self) -> Option<Rect> {
        let clock = self.uptime();
        if let Some(panel) = self.launcher_panel.as_ref() {
            return crate::launcher::blur_rect(crate::launcher::placement(
                self.output_area(),
                panel.size(),
                self.launcher.origin(),
                self.launcher.reveal(clock),
            ));
        }
        // The pinned panel is drawn with the launcher's corners, so its
        // blur is inset the same way.
        if let Some(panel) = self.pinned_panel.as_ref() {
            return crate::launcher::blur_rect(crate::pinned::placement(
                self.output_area(),
                panel.size(),
                self.pinned.position(),
                self.pinned.reveal(clock),
            ));
        }
        // A glass window: its own rectangle, inset by the corner radius a
        // libadwaita window draws, so the blurred patch does not show past
        // the rounded corners.
        let (_, rect) = self.glass_window()?;
        const CORNER: i32 = 12;
        let inset = Rect::from_xywh(
            rect.x() + CORNER,
            rect.y() + CORNER,
            (rect.w() - 2 * CORNER).max(0),
            (rect.h() - 2 * CORNER).max(0),
        );
        (inset.w() > 0 && inset.h() > 0).then_some(inset)
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
        if self
            .app_switcher
            .is_some_and(|switcher| now >= switcher.dismiss_at)
        {
            self.dismiss_app_switcher();
        } else if self.app_switcher.is_some() {
            // Keep frames flowing only for the switcher's short timeout window.
            self.queue_redraw();
        }
        let mut finish_workspace_carousel = false;
        if let Some(carousel) = &self.workspace_carousel {
            let moving = !carousel.position.is_settled(now) || !carousel.reveal.is_settled(now);
            if moving {
                self.queue_redraw();
            } else if carousel.closing {
                finish_workspace_carousel = true;
            }
        }
        if finish_workspace_carousel {
            self.workspace_carousel = None;
            self.arrange();
            self.refresh_focus();
        }
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
        self.tick_volume(now);
        if let Some(since) = self.dock_hover_since {
            if now.saturating_sub(since) >= crate::dock::PREVIEW_DELAY {
                self.dock_hover_since = None;
                self.rebuild_dock_previews();
            }
            // Keep frames flowing until the delay is up, so a pointer that
            // has stopped moving still gets its pictures on time.
            self.queue_redraw();
        }
        if self.dock.is_animating(now) {
            // The dock's rectangle moves while it reveals and hides. Pointer
            // input may be completely still during that motion, so relying on
            // the input backend to call `dock_pointer_moved` leaves
            // `dock_hover` describing the dock's old position. Re-hit-test the
            // stationary pointer on every animation frame so a tile that moves
            // underneath it can start its preview delay.
            self.dock_pointer_moved();
            self.refresh_dock();
        }
        if self.launcher.is_animating(now) {
            // Position and alpha come from the reveal at draw time, so the
            // panel itself does not need recomposing — but a frame still has
            // to be asked for, or the motion happens with nothing drawing it.
            self.queue_redraw();
        } else if self.launcher_panel.is_some() && !self.launcher.is_visible(now) {
            // The close animation has ended; the pixels can go. Left in place,
            // the panel would keep steering `blur_rect` under whatever opens
            // next.
            self.refresh_launcher();
        }
        if self.pinned.is_animating(now) {
            self.queue_redraw();
        } else if self.pinned_panel.is_some() && !self.pinned.is_visible(now) {
            // The close animation has ended; the pixels can go.
            self.refresh_pinned();
        }
        if self.focus_ring_is_animating(now) {
            // Same shape as the launcher: the alpha is read at draw time, so
            // all the fade needs is frames. The last one paints it at zero.
            self.queue_redraw();
        }
        // Windows still travelling need frames; ones that have arrived are
        // dropped, and the frame after that draws them the ordinary way at
        // the rectangle they arrived at. Either way a redraw is owed.
        let travelling = self.motions.len();
        self.motions.retain(|_, motion| !motion.is_settled(now));
        if travelling > 0 {
            self.queue_redraw();
        }
    }

    /// A media key. Moves the level and shows the slider.
    pub(crate) fn volume_key(&mut self, key: crate::audio::Key) {
        let (now, motion) = (self.uptime(), self.settings.motion());
        self.volume.borrow_mut().press(key, now, motion);
        self.refresh_volume();
        // The settings row shows the same number, so if the panel is up it
        // has to be told.
        if self.settings_panel.is_some() {
            self.refresh_settings();
        }
    }

    /// Advance the slider's hold and fade, and drop it once it has gone.
    ///
    /// The alpha is read at draw time from the reveal, so the panel is not
    /// recomposed here — the level has not changed, only the fade. All the
    /// fade needs is frames.
    fn tick_volume(&mut self, now: std::time::Duration) {
        let motion = self.settings.motion();
        let mut volume = self.volume.borrow_mut();
        volume.tick(now, motion);
        let animating = volume.is_animating(now);
        drop(volume);
        if animating {
            self.queue_redraw();
        } else if self.volume_panel.is_some() {
            self.volume_panel = None;
            self.queue_redraw();
        }
    }

    /// Recompose the volume slider for the level it now shows.
    ///
    /// Called when the level changes, from a media key or from the settings
    /// row — which is why the settings row's path also lands here, through
    /// [`Huginn::refresh_settings`].
    pub(crate) fn refresh_volume(&mut self) {
        let now = self.uptime();
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        let volume = self.volume.borrow();
        self.volume_panel = volume
            .is_visible(now)
            .then(|| crate::audio::render(&volume, &mut self.text, area, advertised));
        drop(volume);
        self.queue_redraw();
    }

    /// Open the quick settings panel.
    ///
    /// One entry point for the keybinding and for the shell's
    /// `open_quick_settings` request, so that the two cannot drift: whatever
    /// opening the panel comes to involve happens here, whichever side asked.
    pub(crate) fn open_settings(&mut self) {
        let now = self.uptime();
        self.settings.open(now);
        self.refresh_settings();
    }

    /// Redraw the quick settings panel, or drop it once it has closed.
    ///
    /// Kept while the close animation is still running: dropping the panel the
    /// moment it is dismissed would make it vanish rather than leave.
    pub(crate) fn refresh_settings(&mut self) {
        let now = self.uptime();
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        self.settings_panel = self.settings.is_visible(now).then(|| {
            crate::settings::render(&self.settings, &mut self.text, area, now, advertised)
        });
        // The volume row may just have moved the level, and the slider that
        // shows it is drawn whether the change came from a key or a row.
        if self.volume.borrow().is_visible(now) {
            self.refresh_volume();
        }
        // The pinned rows may just have been stepped. The rows own the
        // value; the pins take a copy, and the file and the panel follow.
        let position = self.settings.pins_position();
        let orientation = self.settings.pins_orientation();
        if self.pins.set_position(position) | self.pins.set_orientation(orientation) {
            self.pins_changed();
        }
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
        // The pinned panel holds indices into the same list.
        if self.pinned.is_visible(self.uptime()) {
            self.pinned.refresh(&self.apps, &self.pins);
            self.refresh_pinned();
        }

        // The dock reads the same list, and a removed application must stop
        // being clickable rather than launch whatever took its index.
        self.refresh_dock();
    }

    /// Keep the handle that asks the file indexer for an early walk.
    /// Hand the quick settings row its BlueZ client. From [`crate::bluetooth::start`].
    pub(crate) fn set_bluetooth(&mut self, backend: Box<dyn crate::bluetooth::Backend>) {
        self.settings.set_bluetooth(backend);
    }

    /// The BlueZ thread changed something. Only the panel shows it, so
    /// there is nothing to do unless the panel is up.
    pub(crate) fn bluetooth_changed(&mut self) {
        if self.settings_panel.is_some() {
            self.refresh_settings();
        }
    }

    pub(crate) fn set_file_index_requests(&mut self, requests: crate::fileindex::Requests) {
        self.file_index_requests = Some(requests);
    }

    /// Ask for a new file index if the one in hand is old enough to be
    /// missing something. Never waits: the worker walks when it gets to it,
    /// and a worker that is gone is a launcher that searches what it has.
    fn request_file_index_if_stale(&mut self) {
        if !crate::fileindex::should_refresh(self.file_index_built, Instant::now()) {
            return;
        }
        if let Some(requests) = &self.file_index_requests {
            let _ = requests.send(crate::fileindex::WalkNow);
        }
    }

    /// Take a freshly built file index from [`crate::fileindex`].
    pub(crate) fn set_file_index(&mut self, index: raven_desktop::FileIndex) {
        self.file_index_built = Some(Instant::now());
        self.launcher.set_files(std::sync::Arc::new(index));
        // The launcher may be open on a query whose file results just
        // changed; a closed one picks the index up when it opens.
        if self.launcher.is_open() {
            let now = self.now();
            self.launcher.reindex(&self.apps, &self.frecency, now);
            self.refresh_launcher();
        }
    }

    /// Redraw the launcher's panel, or drop it when it is closed.
    ///
    /// Called whenever the launcher's state changes rather than on every
    /// frame: composing it walks the result list and shapes a string per row,
    /// which is cheap once per keystroke and wasteful sixty times a second.
    pub(crate) fn refresh_launcher(&mut self) {
        let (area, advertised) = (self.output_area(), self.scale().advertised);
        self.launcher_panel = self.launcher.is_visible(self.uptime()).then(|| {
            let (panel, layout) = crate::launcher::render(
                &self.launcher,
                &self.apps,
                &mut self.text,
                &self.icons,
                &mut self.pixmaps,
                area,
                advertised,
            );
            // The pointer hit-tests against where this redraw put things,
            // so the layout and the pixels change together.
            self.launcher.set_layout(layout);
            panel
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
        // Start the ring's clock only when it lands on a different window.
        // A ring that has already faded on this window stays faded.
        let on = rects.and(self.space.focused());
        match (on, self.focus_ring_shown) {
            (None, _) => self.focus_ring_shown = None,
            (Some(id), Some((shown_on, _))) if shown_on == id => {}
            (Some(id), _) => self.focus_ring_shown = Some((id, self.uptime())),
        }
    }

    /// How opaque the focus ring is right now: fully, for a moment after focus
    /// moves, then fading to nothing.
    fn focus_ring_alpha(&self, now: std::time::Duration) -> f32 {
        let Some((_, shown_at)) = self.focus_ring_shown else {
            return 0.0;
        };
        let since = now.saturating_sub(shown_at);
        if since <= crate::anim::FOCUS_RING_HOLD {
            return 1.0;
        }
        let fading = since - crate::anim::FOCUS_RING_HOLD;
        let t = fading.as_secs_f32() / crate::anim::FOCUS_RING_FADE.as_secs_f32();
        (1.0 - t).clamp(0.0, 1.0)
    }

    /// Whether the ring is still on its way out and needs frames to get there.
    fn focus_ring_is_animating(&self, now: std::time::Duration) -> bool {
        self.focus_ring_at.is_some()
            && self.focus_ring_shown.is_some_and(|(_, shown_at)| {
                now.saturating_sub(shown_at)
                    < crate::anim::FOCUS_RING_HOLD + crate::anim::FOCUS_RING_FADE
            })
    }

    /// Where the focus ring goes, or `None` when there should not be one.
    fn focus_ring_rects(&self) -> Option<[Rect; 4]> {
        // A panel holding the keyboard outright is a modal takeover: every
        // keystroke goes to it, and a ring still drawn round the window would be
        // claiming otherwise. An on-demand panel is deliberately not included —
        // it gives the keyboard back on the next click elsewhere, and blinking
        // the ring off for that would make clicking a bar flicker the desktop.
        if matches!(
            self.keyboard_on,
            KeyboardOn::ExclusivePanel | KeyboardOn::Switcher
        ) {
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
        // A minimized window is in the dock, not on the desktop, and its
        // geometry is wherever its pane was before it left. Ringing that
        // would outline empty space inside whichever pane grew into it.
        if window.is_minimized() {
            return None;
        }
        // Around where the window is *drawn*, so the ring travels with a
        // tile still on its way rather than waiting for it at the far end.
        let rect = self
            .motions
            .get(&id)
            .map_or(window.geometry, |motion| motion.rect_at(self.uptime()));
        Some(rect.ring(crate::theme::FOCUS_RING_WIDTH))
    }

    /// The focus ring's edges, each paired with the buffer that paints it.
    /// Empty when nothing is ringed.
    fn focus_ring(&self) -> Vec<(&SolidColorBuffer, Rect, f32)> {
        let Some(rects) = self.focus_ring_at else {
            return Vec::new();
        };
        let alpha = self.focus_ring_alpha(self.uptime());
        if alpha <= 0.0 {
            return Vec::new();
        }
        self.focus_ring
            .iter()
            .zip(rects)
            .map(|(b, r)| (b, r, alpha))
            .collect()
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
            .visible_workspaces()
            .flat_map(|(_, ws)| self.render_list_for(ws))
            .collect()
    }

    fn render_list_for(&self, workspace: usize) -> Vec<(WlSurface, Rect)> {
        let Some(workspace) = self.space.workspaces().get(workspace) else {
            return Vec::new();
        };
        workspace
            .windows()
            .iter()
            .filter(|id| self.mapped.contains(id))
            .filter(|id| {
                self.space
                    .window(**id)
                    .is_some_and(|window| !window.is_minimized())
            })
            .filter_map(|id| {
                let surface = self.windows.get(id)?.wl_surface()?;
                let pane = self.space.window(*id)?.geometry;
                let placed = place_in_pane(&surface, pane);
                Some((surface, placed))
            })
            .collect()
    }

    /// The visible workspace cards, centre first, with a flattened side-card
    /// transform that reads like the edge of a record sleeve.
    fn workspace_previews(&self) -> Option<Vec<(usize, WorkspacePreview)>> {
        let carousel = self.workspace_carousel?;
        let now = self.uptime();
        let position = carousel.position.value(now);
        let reveal = carousel.reveal.value(now).clamp(0.0, 1.0) as f64;
        let area = self.output_area();
        let mut cards: Vec<(usize, f32, WorkspacePreview)> = self
            .space
            .workspaces()
            .iter()
            .enumerate()
            .filter_map(|(index, _)| {
                let distance = index as f32 - position;
                (distance.abs() <= 1.6).then(|| {
                    let front = (1.0 - f64::from(distance.abs())).clamp(0.0, 1.0);
                    let overview_x = 0.30 + 0.46 * front;
                    let overview_y = 0.58 + 0.18 * front;
                    let scale_x = 1.0 + (overview_x - 1.0) * reveal;
                    let scale_y = 1.0 + (overview_y - 1.0) * reveal;
                    let slot = f64::from(distance) * f64::from(area.w()) * 0.48 * reveal;
                    let offset_x = (1.0 - scale_x) * f64::from(area.w()) * 0.5 + slot;
                    let offset_y = (1.0 - scale_y) * f64::from(area.h()) * 0.5;
                    let side = (f64::from(distance.abs()) - 0.15).clamp(0.0, 1.0);
                    let alpha = (1.0 - side * 0.48 * reveal) as f32;
                    (
                        index,
                        distance.abs(),
                        WorkspacePreview {
                            scale_x,
                            scale_y,
                            offset_x,
                            offset_y,
                            alpha,
                        },
                    )
                })
            })
            .collect();
        cards.sort_by(|a, b| a.1.total_cmp(&b.1));
        Some(
            cards
                .into_iter()
                .map(|(index, _, transform)| (index, transform))
                .collect(),
        )
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
        // Where each visible window is drawn *before* the layout moves it,
        // which is where its motion has to start from. Taken now rather than
        // remembered, because "where it is drawn" already accounts for a
        // motion still in flight from the last relayout.
        let now = self.uptime();
        let before: Vec<(WindowId, Rect)> = self
            .space
            .active_workspace()
            .windows()
            .iter()
            .filter(|id| self.mapped.contains(id))
            .filter(|id| {
                self.space
                    .window(**id)
                    .is_some_and(|window| !window.is_minimized())
            })
            .filter_map(|id| Some((*id, self.drawn_rect(*id, now)?)))
            .collect();
        let changed = self.space.arrange();
        self.start_motions(&before, &changed, now);
        for (id, rect) in changed {
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
        let focused = self
            .app_switcher
            .is_none()
            .then(|| self.space.focused())
            .flatten();

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
        // A swipe in progress is not a slide either, for the opposite reason:
        // the fingers are the animation. Easing towards them would leave the
        // strip a fixed distance behind the hand moving it, which is the one
        // thing direct manipulation must never do — and the ease would then
        // have to unwind after the fingers stopped, so the strip would keep
        // drifting for a sixth of a second after the gesture ended.
        if self.carousel_on != Some(workspace) || self.space.carousel_dragging() {
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
        // Locked: the lock screen has the keyboard, and nothing else is even a
        // candidate. Returning `None` while the lock has no surface yet is
        // deliberate -- for those few milliseconds the keystrokes go nowhere at
        // all, which is the only safe place for them to go.
        if let Some(lock) = &self.lock {
            return (
                lock.surface().map(|s| s.wl_surface().clone()),
                KeyboardOn::Lock,
            );
        }

        // The application switcher has no text input of its own, but the
        // desktop it put away must not continue receiving keystrokes unseen.
        if self.app_switcher.is_some() {
            return (None, KeyboardOn::Switcher);
        }

        let candidates: Vec<Focusable<usize>> = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(index, (surface, _, _))| {
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

        let clicked = self.focused_layer.as_ref().and_then(|want| {
            self.layers
                .iter()
                .position(|(s, _, _)| s.wl_surface() == want)
        });

        match keyboard_focus(&candidates, clicked, self.space.focused()) {
            KeyboardFocus::Layer(index) => {
                let Some((surface, _, _)) = self.layers.get(index) else {
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
            .any(|(l, _, _)| l.wl_surface() == surface)
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

        WindowSurface::Xdg(surface.clone()).set_tiled(true);
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.size = Some(self.space.window(id).map_or_else(
                || (0, 0).into(),
                |w| (w.geometry.w(), w.geometry.h()).into(),
            ));
        });
        // xdg-shell requires the client to ack a configure before its first
        // commit, so a window that never gets one simply never appears.
        surface.send_configure();

        self.refresh_focus();
    }

    /// A client asking to go fullscreen — F11 in a browser, a video player's
    /// full-screen button, a game starting up.
    ///
    /// `output` is ignored: there is one output area, and that is what
    /// fullscreen covers.
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if let Some(id) = self.xdg_window_id(&surface) {
            self.set_fullscreen(id, true);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.xdg_window_id(&surface) {
            self.set_fullscreen(id, false);
        }
    }

    /// A client's own minimize button. Goes to the dock the same way the
    /// gesture and the chord send a window there, so there is one notion of
    /// "put away" whichever side asked.
    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.xdg_window_id(&surface) {
            self.minimize_window(id);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.xdg_window_id(&surface) else {
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
        output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        // The screen the client asked for, or the focused one when it left
        // the choice to us. Resolved to an index now and re-clamped on every
        // refresh, so a panel outlives the screen it was made for.
        let on = output
            .as_ref()
            .and_then(Output::from_resource)
            .and_then(|o| self.output_index(&o.name()))
            .unwrap_or_else(|| self.space.focused_output());
        tracing::debug!(%namespace, ?layer, output = %self.outputs[on.min(self.outputs.len() - 1)].name, "layer surface created");
        // Only record it. Do NOT configure yet: the client sets its anchor,
        // size and exclusive zone *after* creating the surface and before its
        // first commit, so configuring here would compute geometry from empty
        // state and send a bogus size the client has to correct. The commit
        // hook sends the real initial configure a moment later.
        //
        // Rect::ZERO as the "last sent" geometry guarantees that first
        // refresh_layers sees a change and does configure.
        self.layers.push((surface, Rect::ZERO, on));
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        // Let go of the click before the surface does, or the keyboard is left
        // pointing at something that no longer exists — the same class of bug
        // the `set_keyboard_focus` comment describes for the clipboard, and the
        // reason `refresh_layers` below now settles focus as well as geometry.
        if self.focused_layer.as_ref() == Some(surface.wl_surface()) {
            self.focused_layer = None;
        }
        self.layers.retain(|(l, _, _)| l != &surface);
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
        let scale = f64::from(self.scale().advertised);
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
// cursor-shape-v1 can also name a cursor for a tablet tool, so smithay asks
// for this even though tablets are not routed (see docs/protocols.md). The
// defaults ignore the request, which is exactly right until they are.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Huginn {}
delegate_cursor_shape!(Huginn);
impl SessionLockHandler for Huginn {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    /// A client asked to lock the session.
    ///
    /// The protocol says: hide the session, present a cleared frame on every
    /// output, and only then confirm. The confirmation is what lets the client
    /// tell its user the machine is safe, so confirming early would be a lie
    /// with a screenshot's worth of consequences.
    ///
    /// Here the hiding is [`Huginn::begin_lock`], which may have happened
    /// already -- a resume blanks the screen before any client exists, and this
    /// call is that client arriving to claim the blank.
    fn lock(&mut self, confirmation: SessionLocker) {
        self.begin_lock();

        let Some(lock) = self.lock.as_mut() else {
            // Unreachable: `begin_lock` either created it or found it. Dropping
            // the confirmation rather than unwrapping tells the client the lock
            // failed, which is the safe direction to be wrong in -- a client
            // that is told "no" shows an error, where one wrongly told "yes"
            // reports a locked machine that is not.
            return;
        };

        if lock.client.is_some() {
            tracing::warn!("a second client tried to lock an already-locked session");
            return;
        }

        lock.client = Some(ClientLock {
            handle: confirmation.ext_session_lock().clone(),
            surfaces: Vec::new(),
        });
        self.refresh_focus();
        self.queue_redraw();

        // Confirmed here rather than after the next frame. The session stopped
        // being drawn the moment `scene` started returning the lock's contents,
        // which is this instant and not the next vblank: the frame currently on
        // the panel is the one that was there before the lock, and the next one
        // composited cannot contain the desktop. Waiting for a vblank to say so
        // would leave the client believing the machine was unlocked for one
        // refresh longer than it was.
        confirmation.lock();
        tracing::info!("session locked");
    }

    /// The client unlocked. It has already checked a password to get here.
    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.lock = None;
        // The desktop is in the scene again, and the keyboard has to be given
        // back to whatever had it before the lock -- otherwise the session
        // comes back with every keystroke going nowhere.
        self.refresh_focus();
        self.queue_redraw();
    }

    /// The client made a surface to draw the lock screen on.
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        // Sized to its output before anything else. A lock surface has no say
        // in its own size -- the compositor tells it, and until it is told it
        // has nothing to draw into.
        let name = Output::from_resource(&output)
            .map(|o| o.name())
            .unwrap_or_default();
        let area = self
            .output_index(&name)
            .map_or_else(|| self.output_area(), |i| self.outputs[i].rect);
        surface.with_pending_state(|state| {
            state.size = Some((area.w() as u32, area.h() as u32).into());
        });
        surface.send_configure();

        let Some(lock) = self.lock.as_mut() else {
            return;
        };
        let Some(client) = lock.client.as_mut() else {
            return;
        };

        // One surface per output. A second for the same output replaces the
        // first rather than stacking behind it.
        client.surfaces.retain(|(on, _)| *on != name);
        client.surfaces.push((name, surface));
        self.refresh_focus();
        self.queue_redraw();
    }
}

delegate_session_lock!(Huginn);

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
    /// The gesture application switcher is open; no client receives keys.
    Switcher,
    /// The session is locked, and the lock screen has it. Nothing else can.
    Lock,
}

/// Whether an idle session is due to lock.
///
/// Free-standing rather than a method for one reason: a `Huginn` needs a
/// Wayland display to exist, so a rule written inside it is a rule that can
/// only be checked by running a compositor. The three inputs are the whole of
/// what decides this.
fn idle_due(locked: bool, after: Option<std::time::Duration>, idle: std::time::Duration) -> bool {
    // Already locked: it cannot be locked twice, and asking again would start
    // a second lock screen on top of the first.
    if locked {
        return false;
    }
    after.is_some_and(|after| idle >= after)
}

/// How long until [`idle_due`] is worth asking again.
fn idle_wait(
    locked: bool,
    after: Option<std::time::Duration>,
    idle: std::time::Duration,
) -> std::time::Duration {
    /// Long enough to cost nothing, short enough that turning the setting back
    /// on takes effect while somebody is still sitting there. Used for the two
    /// cases with nothing to count down to.
    const NOTHING_TO_COUNT: std::time::Duration = std::time::Duration::from_secs(60);
    /// Never reschedule tighter than this. A zero-length timer that keeps
    /// finding itself due would spin the event loop at full tilt.
    const FLOOR: std::time::Duration = std::time::Duration::from_secs(1);

    if locked {
        return NOTHING_TO_COUNT;
    }
    match after {
        Some(after) => after.saturating_sub(idle).max(FLOOR),
        None => NOTHING_TO_COUNT,
    }
}

/// The session, held.
///
/// # The two-stage lock, and why the first stage exists
///
/// `ext-session-lock-v1` is client-driven: a client asks, the compositor stops
/// showing the session, and only then does the compositor confirm. That is the
/// whole protocol and it is enough for a lock somebody asked for.
///
/// It is not enough for a lock on *resume*. The session has to be hidden the
/// instant the machine comes back, and at that instant no client has asked for
/// anything -- `raven-lock` has not been spawned, let alone connected. So this
/// exists in two stages. [`Huginn::begin_lock`] creates it with
/// `client: None`: the desktop stops being drawn immediately, before there is
/// any client at all. The client then connects, asks, and takes it over.
///
/// The danger in that is a screen nobody can get past -- a blank display and no
/// process able to accept a password is a machine that has to be power-cycled.
/// [`Huginn::abandon_lock_if_unclaimed`] is the way out: if no client has
/// claimed the lock within a few seconds of the blank going up, the blank comes
/// back down. A desktop revealed because the lock screen would not start is
/// bad; a machine bricked by the same failure is worse, and the second is not a
/// security trade -- somebody standing at a wedged blank screen can already
/// hold the power button.
#[derive(Debug, Default)]
pub(crate) struct Lock {
    /// The client's lock object, once one has asked. `None` while this is the
    /// compositor's own pre-emptive blank.
    client: Option<ClientLock>,
    /// How many times the lock screen has been started again after dying
    /// while holding this lock. Bounded by [`Huginn::LOCK_RELAUNCHES`].
    relaunches: u32,
}

impl Lock {
    /// The surface that takes the keyboard while locked: the first one the
    /// client made, if it has made any.
    pub(crate) fn surface(&self) -> Option<&LockSurface> {
        self.client.as_ref()?.surfaces.first().map(|(_, s)| s)
    }

    /// Every lock surface with the name of the output it covers.
    pub(crate) fn surfaces(&self) -> &[(String, LockSurface)] {
        self.client
            .as_ref()
            .map_or(&[], |client| client.surfaces.as_slice())
    }
}

/// A lock a client asked for and the compositor confirmed.
#[derive(Debug)]
pub(crate) struct ClientLock {
    /// The client's `ext_session_lock_v1`. Kept for one question: is the
    /// client still there? A lock screen that crashes takes its connection
    /// with it, the resource dies with the connection, and nothing in the
    /// protocol says so -- see [`Huginn::recover_lost_lock`].
    handle: ExtSessionLockV1,
    /// One surface per output, by output name, as the client makes them.
    ///
    /// Empty between the lock being confirmed and the first lock surface
    /// arriving, which is a handful of milliseconds of blank screen and is
    /// exactly what the protocol asks for. A screen the client never covers
    /// stays blank, which is the safe failure.
    surfaces: Vec<(String, LockSurface)>,
}

/// One screen as the compositor sees it: where it sits in the desktop, what
/// it renders at, and the `wl_output` clients know it by.
#[derive(Debug, Clone)]
pub(crate) struct OutputInfo {
    /// The connector name -- `eDP-1`, `HDMI-A-1` -- which is also the
    /// `wl_output` name.
    pub(crate) name: String,
    /// Where the screen sits, in logical pixels, in the global desktop.
    pub(crate) rect: Rect,
    /// Its density policy. See `huginn_core::scale`.
    pub(crate) scale: OutputScale,
    /// The panel's size in millimetres, `0x0` when it did not say.
    pub(crate) mm: Size,
    /// The advertised output. `None` only for the placeholder a `Huginn` is
    /// built with before any backend has brought a screen up.
    pub(crate) output: Option<Output>,
}

impl OutputInfo {
    /// The placeholder screen before a backend reports a real one.
    pub(crate) fn bare(area: Rect) -> Self {
        Self {
            name: String::new(),
            rect: area,
            scale: OutputScale::for_output(Size::new(area.w(), area.h()), Size::new(0, 0)),
            mm: Size::ZERO,
            output: None,
        }
    }
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
        *states
            .cached_state
            .get::<LayerSurfaceCachedState>()
            .current()
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

/// Where the launch history lives: `$XDG_STATE_HOME/raven/frecency`, or
/// `~/.local/state/raven/frecency` when the variable is unset.
///
/// State rather than cache or config: it is not regenerable from anything
/// else (a cache is), and it is not something the user edits to change the
/// desktop's behaviour (a config is). It is what the basedir specification
/// calls state — "history", by name, in its own list of examples. `None` only
/// when there is no `HOME` either, in which case there is nowhere sensible to
/// put it and the session simply forgets on exit, as it always did.
fn frecency_path() -> Option<std::path::PathBuf> {
    frecency_path_in(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// [`frecency_path`] with the environment as parameters, for testing.
///
/// An empty `$XDG_STATE_HOME` counts as unset, which is what the basedir
/// specification says and what a shell that exported it blank will have done
/// by accident.
fn frecency_path_in(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    let base = match xdg_state_home {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::Path::new(home?).join(".local/state"),
    };
    Some(base.join("raven/frecency"))
}

/// Where the pins live: beside the launch history, as `raven/pins`. State
/// for the reason the history is — see [`frecency_path`] and
/// [`crate::pins`].
fn pins_path() -> Option<std::path::PathBuf> {
    frecency_path().map(|history| history.with_file_name("pins"))
}

/// Where the screen arrangement lives: beside the pins, as `raven/outputs`.
/// State rather than config by the same reasoning as the pins: it is written
/// by the desktop in response to what the person did, not edited to change
/// how the desktop behaves.
fn layout_path() -> Option<std::path::PathBuf> {
    frecency_path().map(|history| history.with_file_name("outputs"))
}

/// The arrangement from the last session, or none. Fail-soft.
fn load_layout() -> Vec<huginn_core::layout::Saved> {
    let Some(file) = layout_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&file) {
        Ok(text) => huginn_core::layout::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::warn!(
                "could not load the output layout from {}: {e}",
                file.display()
            );
            Vec::new()
        }
    }
}

/// Write a state file atomically: a PID-named sibling, then a rename, the
/// way `pins::Pins::save` does.
fn save_text(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The pins from the last session, or none. Fail-soft, as the history is.
fn load_pins() -> crate::pins::Pins {
    let Some(file) = pins_path() else {
        return crate::pins::Pins::new();
    };
    match crate::pins::Pins::load(&file) {
        Ok(pins) => pins,
        Err(e) => {
            tracing::warn!("could not load pins from {}: {e}", file.display());
            crate::pins::Pins::new()
        }
    }
}

/// The launch history from the last session, or an empty one.
///
/// Fail-soft in both directions: a missing file is a fresh install and an
/// unreadable one is a line in the log, and neither stops the compositor. The
/// worst that happens is a launcher that has forgotten what you use.
fn load_frecency() -> raven_desktop::Frecency {
    let Some(file) = frecency_path() else {
        return raven_desktop::Frecency::new();
    };
    match raven_desktop::Frecency::load(&file) {
        Ok(frecency) => frecency,
        Err(e) => {
            tracing::warn!("could not load launch history from {}: {e}", file.display());
            raven_desktop::Frecency::new()
        }
    }
}

#[cfg(test)]
mod frecency_tests {
    use super::frecency_path_in;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn the_history_lives_under_the_xdg_state_directory() {
        assert_eq!(
            frecency_path_in(Some(OsStr::new("/state")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/state/raven/frecency"))
        );
    }

    #[test]
    fn an_unset_or_blank_state_home_falls_back_to_dot_local() {
        let expected = Some(PathBuf::from("/home/u/.local/state/raven/frecency"));
        assert_eq!(
            frecency_path_in(None, Some(OsStr::new("/home/u"))),
            expected
        );
        assert_eq!(
            frecency_path_in(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            expected
        );
    }

    #[test]
    fn with_no_home_at_all_there_is_nowhere_to_save() {
        assert_eq!(frecency_path_in(None, None), None);
    }
}

#[cfg(test)]
mod idle_tests {
    use super::{idle_due, idle_wait};
    use std::time::Duration;

    const AFTER: Option<Duration> = Some(Duration::from_secs(600));

    #[test]
    fn a_session_locks_once_it_has_been_idle_long_enough() {
        assert!(!idle_due(false, AFTER, Duration::from_secs(599)));
        assert!(idle_due(false, AFTER, Duration::from_secs(600)));
        assert!(idle_due(false, AFTER, Duration::from_secs(6000)));
    }

    #[test]
    fn off_never_locks_however_long_it_sits() {
        assert!(!idle_due(false, None, Duration::from_secs(86_400)));
    }

    /// Otherwise every tick past the timeout starts another lock screen on top
    /// of the one already holding the session.
    #[test]
    fn an_already_locked_session_is_never_due() {
        assert!(!idle_due(true, AFTER, Duration::from_secs(6000)));
    }

    #[test]
    fn the_next_check_is_the_time_that_is_left() {
        assert_eq!(
            idle_wait(false, AFTER, Duration::from_secs(60)),
            Duration::from_secs(540)
        );
    }

    /// The reschedule must never be zero, or the event loop spins: past the
    /// timeout, `idle_due` is true and the lock is taken, but the timer is
    /// rearmed before the lock screen has connected.
    #[test]
    fn the_next_check_never_comes_back_instantly() {
        for idle in [600, 601, 100_000] {
            let wait = idle_wait(false, AFTER, Duration::from_secs(idle));
            assert!(
                wait >= Duration::from_secs(1),
                "rescheduled in {wait:?} at {idle}s idle"
            );
        }
    }

    /// With nothing counting down, the only thing being waited for is somebody
    /// changing the setting — so keep looking, but cheaply.
    #[test]
    fn nothing_to_count_still_checks_back() {
        let coarse = Duration::from_secs(60);
        assert_eq!(idle_wait(false, None, Duration::from_secs(5)), coarse);
        assert_eq!(idle_wait(true, AFTER, Duration::from_secs(5)), coarse);
    }
}
