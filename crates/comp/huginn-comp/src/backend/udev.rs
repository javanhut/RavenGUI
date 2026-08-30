//! Real backend: DRM/KMS on a TTY.
//!
//! Unlike the nested winit backend, this one drives the hardware directly: it
//! takes DRM master through logind, scans out through GBM, and reads input from
//! libinput. That is also why it cannot be started over ssh — logind grants DRM
//! master only to the active session on a seat, and an ssh login has no seat.
//!
//! # Frame pacing
//!
//! Each CRTC owns a [`DrmOutput`]. A frame is rendered, queued for scan-out,
//! and then nothing further happens on that output until the page flip
//! completes and DRM reports a vblank. Queueing a second frame before the first
//! has flipped is how you get `EBUSY` and a stuttering display, so each surface
//! tracks whether it is waiting.
//!
//! # Hotplug
//!
//! [`UdevBackend`] is an event source like any other, and a monitor being
//! plugged in or pulled out arrives as a `Changed` event on the GPU's device
//! node — udev has no notion of a connector, so the only way to learn what
//! actually changed is to re-read the connector list. [`Udev::sync_connectors`]
//! does that and reconciles: connectors that went away lose their screen and
//! their `wl_output` global, new ones get a CRTC and light up, and ones that
//! stayed are re-read in case the panel behind them changed —
//! [`Udev::refresh_screen`] follows a new preferred mode or density without
//! taking the output down.
//!
//! # Coordinates
//!
//! The core lays the desktop out in *logical* pixels and the renderer scales
//! them back up by the output's advertised integer scale. Everything this
//! backend tells the core — the output area, the position of each screen — is
//! therefore in logical units, never in the panel's native resolution.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::EGLDevice,
        input::{Event as _, InputEvent, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind, ExportMem, Frame, ImportDma, Offscreen, Renderer,
            element::{
                Id, Kind, surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
            },
            gles::{GlesRenderer, GlesTexture},
            utils::draw_render_elements,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::all_gpus,
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    input::keyboard::{KeyboardHandle, Keysym},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Device as _, Mode as DrmMode, ResourceHandles, connector, crtc},
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_server::{
            Display, DisplayHandle, backend::GlobalId, protocol::wl_surface::WlSurface,
        },
    },
    utils::{DeviceFd, Physical, Rectangle, SERIAL_COUNTER, Scale, Transform},
    wayland::{
        compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
        socket::ListeningSocketSource,
    },
};

use huginn_core::{
    geometry::{Rect, Size},
    scale::OutputScale,
    workspace::Direction,
};

use crate::backend::advertise;
use crate::backend::chord;
use crate::backend::gpu::{self, Bridge, DumbSurface, Scanout, Secondary};
use crate::backend::input;
use crate::backend::keymap::{Action, Modes, help_line, resolve};
use crate::pointer::Cursor;
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use crate::render;
use crate::render::HuginnElement;
use crate::state::{ClientState, Huginn};

/// Background colour of an empty workspace.
const CLEAR: [f32; 4] = [0.06, 0.06, 0.09, 1.0];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type Manager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type Surface = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

/// A screen is a CRTC on a device; CRTC handles repeat across devices.
type ScreenKey = (libc::dev_t, crtc::Handle);

/// Where a screen's pixels go. See [`crate::backend::gpu`].
enum ScreenScanout {
    /// On the GPU the compositor renders on.
    Primary(Surface),
    /// On another GPU, through a shared buffer.
    Gpu {
        drm: Surface,
        bridge: Option<Bridge>,
    },
    /// On a device with no GPU, through dumb buffers.
    Dumb(DumbSurface),
}

/// One scan-out surface: a CRTC with a connector attached.
struct Screen {
    scanout: ScreenScanout,
    output: Output,
    /// The `wl_output` global this screen advertises. Kept so that unplugging
    /// the monitor can withdraw it — a global left behind is an output clients
    /// still believe in and will happily place surfaces on.
    global: GlobalId,
    /// Which connector this screen is driving. The map is keyed by CRTC, but a
    /// rescan reports connectors, so both directions have to be answerable.
    connector: connector::Handle,
    name: String,
    /// The mode the CRTC is driving. Compared against the connector's preferred
    /// mode on every rescan; a difference means the panel behind the connector
    /// is not the one this screen was brought up for.
    mode: DrmMode,
    /// The scale policy for this panel. `logical` is what the screen measures
    /// in the desktop's coordinates, which is the unit `relayout` works in.
    scale: OutputScale,
    /// The panel's pixels and millimetres, kept so `relayout` can derive the
    /// scale again with or without a saved override.
    physical: Size,
    mm: Size,
    /// Where this screen sits in the desktop, in logical pixels. Decided by
    /// `relayout`, which is also what tells the core.
    rect: Rect,
    /// A frame is queued and we are waiting for its page flip. Queueing another
    /// before the flip completes returns EBUSY.
    awaiting_flip: bool,
    /// Something changed since the last frame we sent.
    dirty: bool,
}

struct Udev {
    state: Huginn,
    display: Display<Huginn>,
    dh: DisplayHandle,
    /// The event loop, for arming timers from inside a callback -- the lock
    /// claim timeout is the one that needs it.
    handle: LoopHandle<'static, Udev>,
    session: LibSeatSession,
    /// The GPU we are driving, as udev identifies it. Every udev event carries
    /// a device id and most of them are about something else.
    device_id: libc::dev_t,
    manager: Manager,
    /// The primary's allocator, for the buffers that bridge to other GPUs.
    allocator: GbmAllocator<DrmDeviceFd>,
    renderer: GlesRenderer,
    /// Every other DRM device on the seat that has connectors.
    secondaries: HashMap<libc::dev_t, Secondary>,
    /// The blur, if its shader compiled. `None` means panels do not blur.
    blur: Option<crate::blur::Blur>,
    screens: HashMap<ScreenKey, Screen>,
    keyboard: KeyboardHandle<Huginn>,
    /// The theme cursors, one bitmap per density and shape in use. A 1x
    /// laptop panel beside a 2x monitor needs both, and the pointer is drawn
    /// on whichever screen it is over with that screen's. Shapes other than
    /// the arrow are loaded the first time a client asks for them.
    cursors: HashMap<(u32, CursorIcon), Cursor>,
    socket: String,
    start: Instant,
    signal: LoopSignal,
}

pub(crate) fn run() -> Result<()> {
    // This is the first thing that fails when you try to run the udev backend
    // over ssh, so it is worth an error that says what to do about it.
    let (session, session_notifier) = LibSeatSession::new().context(
        "could not acquire a seat session. The udev backend needs DRM master, and logind \
         grants that only to the active session on a seat — an ssh login has no seat at all. \
         Run huginn from a TTY (Ctrl+Alt+F2), or use the nested backend with --backend winit",
    )?;
    let seat_name = session.seat();
    tracing::info!(seat = %seat_name, "acquired session");

    let mut event_loop: EventLoop<Udev> = EventLoop::try_new().context("creating event loop")?;
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    let mut display: Display<Huginn> = Display::new().context("creating wayland display")?;
    let dh = display.handle();

    // --- GPU -------------------------------------------------------------
    let gpu_path = primary_gpu(&seat_name)
        .context("scanning for GPUs")?
        .context("no GPU found for this seat")?;
    let node = DrmNode::from_path(&gpu_path).context("opening the GPU as a DRM node")?;
    tracing::info!(path = ?gpu_path, ?node, "primary GPU");

    // The fd comes from the session rather than a plain open: that is what
    // carries DRM master, and what lets logind revoke it on VT switch.
    let mut session_for_open = session.clone();
    let fd = session_for_open
        .open(
            &gpu_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| anyhow::anyhow!("session refused to open {gpu_path:?}: {e}"))?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm_device, drm_notifier) =
        DrmDevice::new(device_fd.clone(), true).context("creating DRM device")?;
    let gbm = GbmDevice::new(device_fd).context("creating GBM device")?;

    // --- renderer: the only unsafe in the project, and it lives elsewhere ---
    let renderer =
        huginn_egl::renderer_for(gbm.clone()).context("initialising the GLES renderer")?;

    let render_formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let manager = DrmOutputManager::new(
        drm_device,
        allocator.clone(),
        GbmFramebufferExporter::new(
            gbm.clone(),
            node.node_with_type(NodeType::Render).and_then(|r| r.ok()),
        ),
        Some(gbm.clone()),
        // Xrgb8888 first: opaque scan-out is the common case and avoids the
        // compositor blending against nothing.
        [Fourcc::Xrgb8888, Fourcc::Argb8888],
        render_formats,
    );

    let mut state = Huginn::new(&dh, Rect::from_xywh(0, 0, 0, 0));

    // XWayland. Started here rather than after the backend is up because it is
    // asynchronous either way: this only spawns the server and registers the
    // event source, and the window manager is created later, when XWayland
    // reports ready. Fail-soft -- if the `Xwayland` binary is not installed the
    // compositor runs exactly as before, without X11 clients.
    crate::xwayland::start::<Udev>(&dh, &handle);

    // Watch the application directories, so an application installed during
    // the session reaches the launcher and the dock without a logout. Started
    // here for the same reason XWayland is: it only registers an event source,
    // and everything it does happens later, from the loop.
    crate::appwatch::start::<Udev>(&handle);
    crate::configwatch::start::<Udev>(&handle);
    crate::fileindex::start::<Udev>(&handle, &mut state);
    // BlueZ, for the quick settings row. Same shape: a thread and a wake-up.
    crate::bluetooth::start::<Udev>(&handle, &mut state);

    match EGLDevice::device_for_display(renderer.egl_context().display())
        .and_then(|device| device.try_get_render_node())
    {
        Ok(Some(render_node)) => {
            let formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
            state.enable_dmabuf(&dh, render_node.dev_id(), formats);
        }
        Ok(None) => tracing::warn!("EGL display has no render node; clients stay on shm"),
        Err(e) => {
            tracing::warn!(error = %e, "could not identify the render node; clients stay on shm")
        }
    }

    // Outputs are not scanned here. The first scan is the same reconciliation
    // a hotplug performs, and it needs the assembled `Udev`, so it happens
    // below once everything it touches exists.

    // --- input -----------------------------------------------------------
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| anyhow::anyhow!("libinput refused seat {seat_name}"))?;
    let input_backend = LibinputInputBackend::new(libinput.clone());

    // --- wayland ---------------------------------------------------------
    let socket_source = ListeningSocketSource::new_auto().context("binding wayland socket")?;
    let socket = socket_source.socket_name().to_string_lossy().into_owned();
    state.set_socket(socket.clone());

    // Every client rings this on its way out, and the loop then asks whether
    // the one that left was the lock screen. See `Huginn::recover_lost_lock`.
    let (disconnect, disconnects) =
        calloop::ping::make_ping().context("creating the client-disconnect ping")?;
    handle
        .insert_source(disconnects, |_, _, data: &mut Udev| {
            if data.state.recover_lost_lock() {
                data.arm_claim_timeout();
            }
        })
        .map_err(|e| anyhow::anyhow!("client-disconnect source: {e}"))?;

    handle
        .insert_source(socket_source, move |stream, _, data: &mut Udev| {
            if let Err(e) = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::new(disconnect.clone())))
            {
                tracing::warn!(error = %e, "could not accept a client");
            }
        })
        .map_err(|e| anyhow::anyhow!("wayland socket source: {e}"))?;

    let poll_fd = display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .context("cloning the display poll fd")?;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, CalloopMode::Level),
            |_, _, data: &mut Udev| {
                data.display
                    .dispatch_clients(&mut data.state)
                    .map_err(std::io::Error::other)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("wayland display source: {e}"))?;

    // --- hardware --------------------------------------------------------
    let primary_id = node.dev_id();
    handle
        .insert_source(
            drm_notifier,
            move |event, meta, data: &mut Udev| match event {
                DrmEvent::VBlank(crtc) => data.on_vblank(primary_id, crtc),
                DrmEvent::Error(e) => {
                    tracing::error!(error = %e, ?meta, "DRM error");
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("DRM source: {e}"))?;

    handle
        .insert_source(input_backend, |event, _, data: &mut Udev| {
            data.on_input(event);
        })
        .map_err(|e| anyhow::anyhow!("libinput source: {e}"))?;

    // --- hotplug ---------------------------------------------------------
    // udev reports only *changes*, which is why the initial scan below is a
    // separate call rather than something this source delivers.
    let udev_monitor = UdevBackend::new(&seat_name)
        .map_err(|e| anyhow::anyhow!("monitoring udev for DRM devices: {e}"))?;
    handle
        .insert_source(udev_monitor, |event, _, data: &mut Udev| {
            data.on_udev(&event);
        })
        .map_err(|e| anyhow::anyhow!("udev source: {e}"))?;

    // VT switching. On pause we must drop DRM master and stop touching
    // devices; on activate we take it back and force a full repaint, because
    // whatever was on screen while we were away is not ours.
    handle
        .insert_source(session_notifier, |event, _, data: &mut Udev| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused; releasing devices");
                data.manager.pause();
                for secondary in data.secondaries.values_mut() {
                    secondary.pause();
                }
                for screen in data.screens.values_mut() {
                    screen.awaiting_flip = false;
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session activated; reclaiming devices");
                // We dropped DRM master on the way out and the connectors are
                // as we left them, so there is no need to tear them down first.
                data.reclaim_display(false);
            }
        })
        .map_err(|e| anyhow::anyhow!("session source: {e}"))?;

    // Suspend. seatd has no equivalent of logind's PrepareForSleep, so this
    // arrives as a change to a file raven-init writes; see `crate::sleep`.
    // Nothing paused the session on the way into the suspend, which is exactly
    // why the recovery has to be the heavier one: we still hold DRM master over
    // a device whose state the firmware has been through, and only a full
    // modeset can be trusted to put a picture back on the panel.
    // The idle lock. Its own timer rather than a check inside the frame loop:
    // an idle session draws no frames at all, so a check that ran per frame
    // would never run once the desktop went quiet -- which is precisely when
    // it is supposed to fire.
    let idle_timer = Timer::from_duration(Duration::from_secs(60));
    handle
        .insert_source(idle_timer, |_, _, data: &mut Udev| {
            let now = Instant::now();
            if data.state.idle_lock_due(now) {
                tracing::info!("idle; locking the session");
                data.lock_session();
            }
            TimeoutAction::ToDuration(data.state.idle_check_in(now))
        })
        .map_err(|e| anyhow::anyhow!("idle timer: {e}"))?;

    crate::sleep::watch::<Udev, _>(&handle, move |data: &mut Udev| {
        // Locked *before* the display comes back, and that ordering is the
        // whole security of it. `reclaim_display` is what puts the next frame
        // on the panel; blanking the session first means the first thing drawn
        // after a resume is the lock screen and never the desktop. There is no
        // window in which the machine shows what it was doing, because the
        // compositor simply never composites it.
        data.lock_session();
        data.reclaim_display(true);
    });

    // For the density the state starts with; `relayout` loads more as screens
    // of other densities appear.
    let mut cursors = HashMap::new();
    if let Some(cursor) = crate::pointer::Cursor::from_env(state.scale().advertised) {
        cursors.insert((state.scale().advertised, CursorIcon::Default), cursor);
    }

    let keyboard = state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("adding keyboard")?;

    let mut renderer = renderer;
    let blur = crate::blur::Blur::compile(&mut renderer);
    let mut data = Udev {
        blur,
        state,
        display,
        dh,
        handle: handle.clone(),
        session,
        device_id: node.dev_id(),
        manager,
        allocator,
        renderer,
        secondaries: HashMap::new(),
        screens: HashMap::new(),
        keyboard,
        cursors,
        socket,
        start: Instant::now(),
        signal,
    };

    // Every other DRM device on the seat: the discrete GPU on a hybrid
    // laptop, a DisplayLink dock already plugged in. Each is opened the way a
    // hotplugged one is, so there is one code path for both.
    match all_gpus(&seat_name) {
        Ok(paths) => {
            for path in paths.iter().filter(|p| **p != gpu_path) {
                data.add_device(path);
            }
        }
        Err(e) => tracing::warn!(error = %e, "listing DRM devices; driving the primary only"),
    }

    // The initial scan. Identical to the one a hotplug triggers, so there is
    // only one code path that can put a monitor on screen.
    data.sync_connectors();
    if data.screens.is_empty() {
        // Not fatal any more. Nothing is visible, but the udev source is live,
        // so the first monitor plugged in will light up.
        tracing::warn!(
            path = ?gpu_path,
            "no connected displays; waiting for one to be plugged in"
        );
    }

    tracing::info!(socket = %data.socket, screens = data.screens.len(), "huginn is up on DRM");
    tracing::info!("clients: WAYLAND_DISPLAY={} <command>", data.socket);
    tracing::info!("{}", help_line());

    // Draw once before entering the loop. calloop only runs the post-dispatch
    // callback after an event, so with nothing connected yet the first frame
    // would otherwise wait for one — indistinguishable from a black screen.
    crate::dmabuf::import_pending(&mut data.renderer, &mut data.state);
    data.render_dirty();

    // A one-second backstop rather than blocking forever: if a redraw flag is
    // ever missed, the cost is a second of staleness instead of a display that
    // never updates again.
    event_loop
        .run(Some(std::time::Duration::from_secs(1)), &mut data, |data| {
            // Before rendering, and outside it: render() returns early when the
            // session is inactive or nothing is dirty, and a notifier skipped
            // for either reason is never answered at all.
            crate::dmabuf::import_pending(&mut data.renderer, &mut data.state);

            // A client applied a layout: arrange again with what it saved.
            if data.state.take_layout_change() {
                data.relayout();
            }
            if data.state.take_redraw() {
                for screen in data.screens.values_mut() {
                    screen.dirty = true;
                }
            }
            data.state.refresh();
            data.render_dirty();
            if let Err(e) = data.display.flush_clients() {
                tracing::warn!(error = %e, "flushing clients");
            }
        })
        .context("running the event loop")?;

    tracing::info!("huginn is down");
    Ok(())
}

/// The mode to drive a connector at.
///
/// The preferred mode is the panel's native one. Falling back to the first
/// available beats refusing to light up the screen at all.
/// The mode to drive a connector at: the panel's native resolution, at the
/// fastest refresh it offers there.
///
/// The kernel flags one mode PREFERRED, which is the native resolution from
/// the EDID -- the right size, since anything else is the monitor's own
/// scaler blurring the picture. It is not always the fastest: a 144 Hz panel
/// commonly flags its 60 Hz mode, so the refresh is chosen here from every
/// mode of that size. With no flag at all the first mode is what the kernel
/// considers best.
fn preferred_mode(connector: &connector::Info) -> Option<smithay::reexports::drm::control::Mode> {
    use smithay::reexports::drm::control::ModeTypeFlags;

    let modes = connector.modes();
    let native = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())?;
    modes
        .iter()
        .filter(|m| m.size() == native.size())
        .max_by_key(|m| refresh_mhz(m))
        .or(Some(native))
        .copied()
}

/// What a connector driven at `mode` looks like to clients: the `wl_output`
/// mode, and the scale policy for the panel's density.
///
/// Shared by bring-up and rescan so the two cannot disagree about a panel.
fn describe(connector: &connector::Info, mode: &DrmMode) -> (Mode, OutputScale) {
    let (w, h) = mode.size();
    let wl_mode = Mode {
        size: (i32::from(w), i32::from(h)).into(),
        refresh: refresh_mhz(mode),
    };
    // Physical size is in millimetres and comes back as u32.
    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    // The integer-scale contract: whatever the panel's density works out to,
    // a client is only ever told a whole number. See `huginn_core::scale`.
    let scale = OutputScale::for_output(
        Size::new(i32::from(w), i32::from(h)),
        Size::new(phys_w as i32, phys_h as i32),
    );
    (wl_mode, scale)
}

/// Put a frame on a screen of the primary GPU.
fn present_primary(
    renderer: &mut GlesRenderer,
    drm: &mut Surface,
    elements: &[HuginnElement],
) -> Result<bool> {
    let result = drm
        .render_frame(renderer, elements, CLEAR, FrameFlags::DEFAULT)
        .map_err(|e| anyhow::anyhow!("rendering: {e}"))?;
    if result.is_empty {
        return Ok(false);
    }
    drm.queue_frame(())
        .map_err(|e| anyhow::anyhow!("queueing: {e}"))?;
    Ok(true)
}

/// Put a frame on a screen of another GPU: render on the primary into the
/// bridge buffer, draw that buffer on the secondary.
#[allow(clippy::too_many_arguments)]
fn present_gpu(
    primary: &mut GlesRenderer,
    allocator: &mut GbmAllocator<DrmDeviceFd>,
    secondary: &mut GlesRenderer,
    drm: &mut Surface,
    bridge: &mut Option<Bridge>,
    elements: &[HuginnElement],
    size: smithay::utils::Size<i32, Physical>,
    scale: f64,
) -> Result<bool> {
    if bridge.as_ref().is_some_and(|b| b.size != size) {
        *bridge = None;
    }
    let bridge = match bridge {
        Some(bridge) => bridge,
        None => bridge.insert(Bridge::new(allocator, secondary, size)?),
    };
    render_into(primary, &mut bridge.dmabuf, size, scale, elements)?;

    // A fresh id each frame: the bridge is redrawn whole, so there is no
    // damage to track, and the compositor's damage tracking takes an unknown
    // element as fully damaged, which it is.
    let element = TextureRenderElement::from_static_texture(
        Id::new(),
        secondary.context_id(),
        (0.0, 0.0),
        bridge.texture.clone(),
        1,
        Transform::Normal,
        Some(1.0),
        None,
        None,
        None,
        Kind::Unspecified,
    );
    let elements = [element];
    let empty = drm
        .render_frame(secondary, &elements, CLEAR, FrameFlags::DEFAULT)
        .map(|result| result.is_empty)
        .map_err(|e| anyhow::anyhow!("rendering on the secondary: {e}"))?;
    if empty {
        return Ok(false);
    }
    drm.queue_frame(())
        .map_err(|e| anyhow::anyhow!("queueing on the secondary: {e}"))?;
    Ok(true)
}

/// Put a frame on a screen of a device with no GPU: render on the primary
/// into a texture, read it back, copy it into the device's dumb buffer.
fn present_dumb(
    primary: &mut GlesRenderer,
    dumb: &mut DumbSurface,
    elements: &[HuginnElement],
    scale: f64,
) -> Result<bool> {
    let size = dumb.size();
    let mut texture = match dumb.texture.take() {
        Some(texture) => texture,
        None => primary
            .create_buffer(gpu::BRIDGE_FORMAT, (size.w, size.h).into())
            .map_err(|e| anyhow::anyhow!("creating the read-back texture: {e}"))?,
    };
    let pixels = {
        let mut framebuffer = primary
            .bind(&mut texture)
            .map_err(|e| anyhow::anyhow!("binding the read-back texture: {e}"))?;
        draw(primary, &mut framebuffer, size, scale, elements)?;
        let mapping = primary
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size((size.w, size.h).into()),
                gpu::BRIDGE_FORMAT,
            )
            .map_err(|e| anyhow::anyhow!("reading the frame back: {e}"))?;
        primary
            .map_texture(&mapping)
            .map_err(|e| anyhow::anyhow!("mapping the read-back: {e}"))?
            .to_vec()
    };
    dumb.texture = Some(texture);
    dumb.present(&pixels)?;
    Ok(true)
}

/// Render `elements` into `target` on the primary and wait for the GPU, so
/// whoever reads the buffer next sees the finished frame.
fn render_into<T>(
    renderer: &mut GlesRenderer,
    target: &mut T,
    size: smithay::utils::Size<i32, Physical>,
    scale: f64,
    elements: &[HuginnElement],
) -> Result<()>
where
    GlesRenderer: Bind<T>,
{
    let mut framebuffer = renderer
        .bind(target)
        .map_err(|e| anyhow::anyhow!("binding the bridge: {e}"))?;
    draw(renderer, &mut framebuffer, size, scale, elements)
}

fn draw(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as smithay::backend::renderer::RendererSuper>::Framebuffer<'_>,
    size: smithay::utils::Size<i32, Physical>,
    scale: f64,
    elements: &[HuginnElement],
) -> Result<()> {
    let damage = [Rectangle::from_size(size)];
    let mut frame = renderer
        .render(framebuffer, size, Transform::Normal)
        .map_err(|e| anyhow::anyhow!("starting the frame: {e}"))?;
    frame
        .clear(CLEAR.into(), &damage)
        .map_err(|e| anyhow::anyhow!("clearing: {e}"))?;
    draw_render_elements::<GlesRenderer, _, _>(&mut frame, Scale::from(scale), elements, &damage)
        .map_err(|e| anyhow::anyhow!("drawing: {e}"))?;
    let sync = frame
        .finish()
        .map_err(|e| anyhow::anyhow!("finishing: {e}"))?;
    // The other device reads this buffer next, and it has no fence to wait
    // on: block here so it never samples a half-drawn frame.
    let _ = sync.wait();
    Ok(())
}

/// Refresh rate in mHz, the unit `wl_output` reports.
///
/// Computed rather than read from `Mode::vrefresh`, which is a legacy field and
/// is frequently zero on modern kernels. The flag adjustments matter on real
/// panels: an interlaced mode scans each field separately, and doublescan
/// repeats every line.
fn refresh_mhz(mode: &smithay::reexports::drm::control::Mode) -> i32 {
    use smithay::reexports::drm::control::ModeFlags;

    let htotal = u64::from(mode.hsync().2);
    let vtotal = u64::from(mode.vsync().2);
    if htotal == 0 || vtotal == 0 {
        return 60_000;
    }
    // clock is in kHz, so this lands in mHz directly.
    let mut refresh = u64::from(mode.clock()) * 1_000_000 / (htotal * vtotal);

    if mode.flags().contains(ModeFlags::INTERLACE) {
        refresh *= 2;
    }
    if mode.flags().contains(ModeFlags::DBLSCAN) {
        refresh /= 2;
    }
    if mode.vscan() > 1 {
        refresh /= u64::from(mode.vscan());
    }
    refresh as i32
}

/// How long the lock screen has to claim the session before the
/// compositor gives up on it. Generous: this covers a cold exec, a
/// Wayland connection and a socket round-trip to `ravend`, on a machine
/// that is a few hundred milliseconds out of suspend and still bringing
/// its disk back.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(10);

impl Udev {
    /// Lock the session: blank it, start the lock screen, and give the lock
    /// screen a bounded time to claim the blank.
    ///
    /// Every way into a lock comes through here -- `Super`+`L`, the idle
    /// timer, and the resume from suspend below -- so every one of them gets
    /// the timeout. A lock screen that starts and then dies before it claims
    /// the session must not leave a blank nobody can get past, and it makes
    /// no difference which path put the blank up.
    fn lock_session(&mut self) {
        if self.state.is_locked() {
            // Already held -- locked before it went to sleep, or `Super`+`L`
            // pressed twice. Nothing to do, and nothing to start: starting
            // another lock screen would put one on top of itself.
            return;
        }

        if !self.state.lock_and_launch() {
            return;
        }
        self.arm_claim_timeout();
    }

    /// Give the lock screen just started `CLAIM_TIMEOUT` to claim the blank.
    ///
    /// Armed for the first lock screen and again for every one started in
    /// its place after a crash: each is a new process that may equally never
    /// arrive.
    fn arm_claim_timeout(&mut self) {
        let timer = Timer::from_duration(CLAIM_TIMEOUT);
        if let Err(e) = self.handle.insert_source(timer, |_, _, data: &mut Udev| {
            data.state.abandon_lock_if_unclaimed();
            TimeoutAction::Drop
        }) {
            // The timer is the way out of a blank screen, so a session that
            // cannot have one must not be blanked on a promise. Take it down
            // now, while there is still something able to.
            tracing::error!(error = %e, "cannot arm the lock timeout; unlocking again");
            self.state.abandon_lock_if_unclaimed();
        }
    }

    fn reclaim_display(&mut self, disable_connectors: bool) {
        if let Err(e) = self.manager.activate(disable_connectors) {
            tracing::error!(error = %e, "could not reactivate DRM");
        }
        for (dev, secondary) in &mut self.secondaries {
            if let Err(e) = secondary.activate(disable_connectors) {
                tracing::error!(device = dev, error = %e, "could not reactivate a secondary DRM device");
            }
        }
        for screen in self.screens.values_mut() {
            screen.dirty = true;
            // A flip we were waiting on before the interruption will never
            // complete. Left set, it would make every future frame look like
            // one already in flight and nothing would ever be drawn again.
            screen.awaiting_flip = false;
            if let ScreenScanout::Dumb(dumb) = &mut screen.scanout {
                dumb.reset();
            }
        }
        self.state.queue_redraw();
    }

    fn on_udev(&mut self, event: &UdevEvent) {
        match *event {
            UdevEvent::Changed { device_id }
                if device_id == self.device_id || self.secondaries.contains_key(&device_id) =>
            {
                // A connector changed state. Which one, udev does not say.
                tracing::debug!(
                    device = device_id,
                    "device reported a change; rescanning connectors"
                );
                if self.sync_device(device_id) {
                    self.relayout();
                    self.state.queue_redraw();
                }
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                // Every buffer we render into belongs to a device that no
                // longer exists. There is nothing to fall back to, so hand
                // the session back rather than spin on EIO.
                tracing::error!("the GPU huginn renders on was removed; shutting down");
                self.signal.stop();
            }
            UdevEvent::Removed { device_id } if self.secondaries.contains_key(&device_id) => {
                self.remove_device(device_id);
            }
            UdevEvent::Added {
                device_id,
                ref path,
            } if device_id != self.device_id && !self.secondaries.contains_key(&device_id) => {
                self.add_device(path);
            }
            _ => {}
        }
    }

    /// Bring up a DRM device other than the primary and scan its connectors.
    ///
    /// Fail-soft: a device that will not open is logged and left alone,
    /// because a dock that will not come up must not take the laptop's own
    /// screen down with it.
    fn add_device(&mut self, path: &std::path::Path) {
        let mut session = self.session.clone();
        let (mut secondary, notifier) = match Secondary::open(&mut session, path) {
            Ok(opened) => opened,
            Err(e) => {
                tracing::warn!(?path, error = %format!("{e:#}"), "could not bring up a DRM device");
                return;
            }
        };
        let dev = secondary.node.dev_id();
        match self
            .handle
            .insert_source(notifier, move |event, meta, data: &mut Udev| match event {
                DrmEvent::VBlank(crtc) => data.on_vblank(dev, crtc),
                DrmEvent::Error(e) => {
                    tracing::error!(device = dev, error = %e, ?meta, "DRM error");
                }
            }) {
            Ok(token) => secondary.token = Some(token),
            Err(e) => {
                tracing::warn!(?path, error = %e, "could not watch a DRM device for vblanks; not driving it");
                return;
            }
        }
        self.secondaries.insert(dev, secondary);
        if self.sync_device(dev) {
            self.relayout();
            self.state.queue_redraw();
        }
    }

    /// A secondary device went away: its screens with it.
    fn remove_device(&mut self, dev: libc::dev_t) {
        let dh = self.dh.clone();
        let mut changed = false;
        self.screens.retain(|key, screen| {
            if key.0 != dev {
                return true;
            }
            tracing::info!(name = %screen.name, "output gone with its device");
            dh.remove_global::<Huginn>(screen.global.clone());
            changed = true;
            false
        });
        if let Some(secondary) = self.secondaries.remove(&dev) {
            if let Some(token) = secondary.token {
                self.handle.remove(token);
            }
            tracing::info!(path = ?secondary.path, "DRM device removed");
        }
        if changed {
            self.relayout();
            self.state.queue_redraw();
        }
    }

    /// The DRM device with id `dev`, primary or secondary.
    fn device(&self, dev: libc::dev_t) -> Option<&DrmDevice> {
        if dev == self.device_id {
            Some(self.manager.device())
        } else {
            self.secondaries.get(&dev).map(Secondary::device)
        }
    }

    fn sync_connectors(&mut self) {
        let mut devices: Vec<libc::dev_t> = self.secondaries.keys().copied().collect();
        devices.insert(0, self.device_id);
        let mut changed = false;
        for dev in devices {
            changed |= self.sync_device(dev);
        }
        if changed {
            self.relayout();
            self.state.queue_redraw();
        }
    }

    /// Reconcile the screens of one device with its connectors. Returns
    /// whether anything changed; the caller lays the desktop out again.
    fn sync_device(&mut self, dev: libc::dev_t) -> bool {
        let Some(device) = self.device(dev) else {
            return false;
        };
        let resources = match device.resource_handles() {
            Ok(resources) => resources,
            Err(e) => {
                tracing::warn!(device = dev, error = %e, "reading DRM resources; keeping the current outputs");
                return false;
            }
        };

        // Force-probing is the whole point of the rescan. A connector keeps its
        // handle across a swap, and without a probe the kernel answers from its
        // cache — the old monitor's state and the old monitor's mode list.
        let connected: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|handle| device.get_connector(*handle, true).ok())
            .filter(|connector| connector.state() == connector::State::Connected)
            .collect();
        let live: HashSet<connector::Handle> =
            connected.iter().map(connector::Info::handle).collect();

        // --- gone ---
        // Dropping the Screen drops its scan-out surface, which is what
        // releases the CRTC for the next connector to claim.
        let dh = self.dh.clone();
        let mut changed = false;
        self.screens.retain(|key, screen| {
            if key.0 != dev || live.contains(&screen.connector) {
                return true;
            }
            tracing::info!(name = %screen.name, "output unplugged");
            dh.remove_global::<Huginn>(screen.global.clone());
            changed = true;
            false
        });

        // --- new, or still here but different ---
        let known: HashSet<connector::Handle> = self
            .screens
            .iter()
            .filter(|(key, _)| key.0 == dev)
            .map(|(_, screen)| screen.connector)
            .collect();
        for connector in &connected {
            if known.contains(&connector.handle()) {
                changed |= self.refresh_screen(dev, connector);
                continue;
            }
            match self.add_screen(dev, &resources, connector) {
                Ok(()) => changed = true,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "could not bring up a connector")
                }
            }
        }
        changed
    }

    fn add_screen(
        &mut self,
        dev: libc::dev_t,
        resources: &ResourceHandles,
        connector: &connector::Info,
    ) -> Result<()> {
        let name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );

        let Some(mode) = preferred_mode(connector) else {
            anyhow::bail!("{name} reports no modes");
        };
        let Some(crtc) = self.free_crtc(dev, resources, connector) else {
            anyhow::bail!("no free CRTC for {name}");
        };

        let (wl_mode, scale) = describe(connector, &mode);
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let mm = Size::new(phys_w as i32, phys_h as i32);
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                // Physical size is in millimetres and comes back as u32.
                size: (mm.w, mm.h).into(),
                subpixel: Subpixel::Unknown,
                make: "Huginn".to_owned(),
                model: name.clone(),
            },
        );
        // Position is left at the origin here; `relayout` places every screen
        // once the whole set is known, and it is also what hands the core its
        // scale and area — this screen may not be the one that defines them.
        output.change_current_state(
            Some(wl_mode),
            Some(Transform::Normal),
            Some(advertise(scale)),
            None,
        );
        output.set_preferred(wl_mode);
        let global = self.state.add_output(&output, &self.dh);

        let scanout =
            if dev == self.device_id {
                self.manager
                    .initialize_output(
                        crtc,
                        mode,
                        &[connector.handle()],
                        &output,
                        None,
                        &mut self.renderer,
                        &DrmOutputRenderElements::<
                            GlesRenderer,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >::default(),
                    )
                    .map(ScreenScanout::Primary)
                    .map_err(|e| anyhow::anyhow!("initialising {name}: {e}"))
            } else {
                match self.secondaries.get_mut(&dev).map(|s| &mut s.scanout) {
                    Some(Scanout::Gpu { manager, renderer }) => manager
                        .initialize_output(
                            crtc,
                            mode,
                            &[connector.handle()],
                            &output,
                            None,
                            &mut **renderer,
                            &DrmOutputRenderElements::<
                                GlesRenderer,
                                TextureRenderElement<GlesTexture>,
                            >::default(),
                        )
                        .map(|drm| ScreenScanout::Gpu { drm, bridge: None })
                        .map_err(|e| anyhow::anyhow!("initialising {name} on its GPU: {e}")),
                    Some(Scanout::Dumb { device, fd }) => {
                        DumbSurface::new(device, fd, crtc, mode, &[connector.handle()])
                            .map(ScreenScanout::Dumb)
                            .with_context(|| format!("initialising {name} with dumb buffers"))
                    }
                    None => Err(anyhow::anyhow!(
                        "{name} is on a device huginn is not driving"
                    )),
                }
            };
        let scanout = match scanout {
            Ok(scanout) => scanout,
            Err(e) => {
                // The global went out before the CRTC came up, so take it back
                // rather than advertise an output that renders nothing.
                self.dh.remove_global::<Huginn>(global);
                return Err(e);
            }
        };

        tracing::info!(
            %name,
            ?crtc,
            device = dev,
            width = wl_mode.size.w,
            height = wl_mode.size.h,
            refresh_mhz = wl_mode.refresh,
            kind = match scanout {
                ScreenScanout::Primary(_) => "primary",
                ScreenScanout::Gpu { .. } => "secondary gpu",
                ScreenScanout::Dumb(_) => "dumb",
            },
            "output up"
        );

        self.screens.insert(
            (dev, crtc),
            Screen {
                scanout,
                output,
                global,
                connector: connector.handle(),
                name,
                mode,
                scale,
                physical: scale.physical,
                mm,
                rect: Rect::from_xywh(0, 0, scale.logical.w, scale.logical.h),
                awaiting_flip: false,
                dirty: true,
            },
        );
        Ok(())
    }

    fn refresh_screen(&mut self, dev: libc::dev_t, connector: &connector::Info) -> bool {
        let Some(&key) = self
            .screens
            .iter()
            .find(|(key, screen)| key.0 == dev && screen.connector == connector.handle())
            .map(|(key, _)| key)
        else {
            return false;
        };
        let Some(mode) = preferred_mode(connector) else {
            // A connected panel with no modes is one mid-way through a swap.
            // Keep driving what we have; the next event will say more.
            tracing::warn!(connector = ?connector.handle(), "connector reports no modes; keeping its current mode");
            return false;
        };
        let (wl_mode, scale) = describe(connector, &mode);
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));

        let Udev {
            screens,
            secondaries,
            renderer,
            ..
        } = self;
        let screen = screens
            .get_mut(&key)
            .expect("looked up by connector just above");
        if mode == screen.mode && scale == screen.scale {
            return false;
        }

        if mode != screen.mode {
            let switched = match &mut screen.scanout {
                ScreenScanout::Primary(drm) => drm
                    .use_mode(
                        mode,
                        renderer,
                        &DrmOutputRenderElements::<
                            GlesRenderer,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                        >::default(),
                    )
                    .map_err(|e| anyhow::anyhow!("{e}")),
                ScreenScanout::Gpu { drm, bridge } => {
                    *bridge = None;
                    match secondaries.get_mut(&dev).map(|s| &mut s.scanout) {
                        Some(Scanout::Gpu {
                            renderer: secondary,
                            ..
                        }) => drm
                            .use_mode(
                                mode,
                                &mut **secondary,
                                &DrmOutputRenderElements::<
                                    GlesRenderer,
                                    TextureRenderElement<GlesTexture>,
                                >::default(),
                            )
                            .map_err(|e| anyhow::anyhow!("{e}")),
                        _ => Err(anyhow::anyhow!("its device is gone")),
                    }
                }
                ScreenScanout::Dumb(dumb) => dumb.use_mode(mode),
            };
            if let Err(e) = switched {
                tracing::warn!(name = %screen.name, error = %format!("{e:#}"), "switching mode; keeping the old one");
                return false;
            }
            tracing::info!(
                name = %screen.name,
                width = wl_mode.size.w,
                height = wl_mode.size.h,
                refresh_mhz = wl_mode.refresh,
                "output mode changed"
            );
        }
        if scale != screen.scale {
            tracing::info!(name = %screen.name, advertised = scale.advertised, "output scale changed");
        }

        screen.mode = mode;
        screen.scale = scale;
        screen.physical = scale.physical;
        screen.mm = Size::new(phys_w as i32, phys_h as i32);
        // The physical size on the `wl_output` global is fixed at creation and
        // stays as the old panel reported it; nothing downstream reads it, the
        // scale policy having already been applied here.
        screen
            .output
            .change_current_state(Some(wl_mode), None, Some(advertise(scale)), None);
        screen.output.set_preferred(wl_mode);
        screen.dirty = true;
        true
    }

    fn free_crtc(
        &self,
        dev: libc::dev_t,
        resources: &ResourceHandles,
        connector: &connector::Info,
    ) -> Option<crtc::Handle> {
        let device = self.device(dev)?;
        connector
            .encoders()
            .iter()
            .filter_map(|encoder| device.get_encoder(*encoder).ok())
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .find(|crtc| !self.screens.contains_key(&(dev, *crtc)))
    }

    fn relayout(&mut self) {
        let saved = self.state.output_layout().to_vec();
        let builtin = |name: &str| {
            name.starts_with("eDP") || name.starts_with("LVDS") || name.starts_with("DSI")
        };

        let mut keys: Vec<ScreenKey> = self.screens.keys().copied().collect();
        keys.sort_by(|a, b| self.screens[a].name.cmp(&self.screens[b].name));

        // A saved scale wins over the one the panel's size implies. Applied
        // before placing, since it changes how much room the screen takes.
        for key in &keys {
            let screen = self.screens.get_mut(key).expect("key from the same map");
            let wanted = saved
                .iter()
                .find(|s| s.name == screen.name)
                .and_then(|s| s.scale)
                .map_or_else(
                    || OutputScale::for_output(screen.physical, screen.mm),
                    |scale| OutputScale::from_effective(screen.physical, scale),
                );
            if wanted != screen.scale {
                tracing::info!(name = %screen.name, advertised = wanted.advertised, effective = wanted.fractional(), "output scale set");
                screen.scale = wanted;
                screen
                    .output
                    .change_current_state(None, None, Some(advertise(wanted)), None);
                screen.dirty = true;
            }
        }

        let candidates: Vec<huginn_core::layout::Candidate> = keys
            .iter()
            .map(|key| {
                let screen = &self.screens[key];
                huginn_core::layout::Candidate {
                    name: screen.name.clone(),
                    size: screen.scale.logical,
                    builtin: builtin(&screen.name),
                }
            })
            .collect();
        let placed = huginn_core::layout::arrange(&candidates, &saved);

        let mut outputs = Vec::with_capacity(keys.len());
        for (key, rect) in keys.iter().zip(placed) {
            let screen = self.screens.get_mut(key).expect("key from the same map");
            if screen.rect != rect {
                screen.dirty = true;
            }
            screen.rect = rect;
            screen
                .output
                .change_current_state(None, None, None, Some((rect.x(), rect.y()).into()));
            outputs.push(crate::state::OutputInfo {
                name: screen.name.clone(),
                rect,
                scale: screen.scale,
                mm: screen.mm,
                output: Some(screen.output.clone()),
            });
        }

        // With nothing connected the last known layout is kept — resizing
        // every client to zero and back is a configure storm that buys nothing
        // while no one can see the screen. `set_outputs` ignores an empty list
        // for the same reason.
        if outputs.is_empty() {
            return;
        }
        // Every density on screen has a cursor loaded for it. Loaded once
        // and kept: a monitor that comes and goes should not re-read the
        // theme each time.
        for output in &outputs {
            let density = output.scale.advertised;
            if !self.cursors.contains_key(&(density, CursorIcon::Default))
                && let Some(cursor) = Cursor::from_env(density)
            {
                self.cursors.insert((cursor.density, CursorIcon::Default), cursor);
            }
        }
        // Reflows the layer surfaces and the windows underneath them on every
        // screen; the panels the shell draws are composed for their screen's
        // density the next time each is refreshed.
        self.state.set_outputs(outputs);
    }

    fn render_dirty(&mut self) {
        let keys: Vec<ScreenKey> = self
            .screens
            .iter()
            .filter(|(_, s)| s.dirty && !s.awaiting_flip)
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            self.render(key);
        }
    }

    fn render(&mut self, key: ScreenKey) {
        // Advance animations before assembling the scene, so this frame shows
        // where they are now rather than where they were last frame.
        self.state.tick_animations();
        if !self.session.is_active() {
            return;
        }

        // This screen's view of the desktop: where it sits and what it
        // renders at. Every screen draws the same scene; this is what makes
        // each draw its own part of it.
        let Some((view, scale, density, size)) = self.screens.get(&key).map(|screen| {
            (
                screen.rect,
                screen.scale.fractional(),
                screen.scale.advertised,
                screen
                    .output
                    .current_mode()
                    .map(|mode| mode.size)
                    .unwrap_or_default(),
            )
        }) else {
            return;
        };
        // The shape a client named through cursor-shape-v1, or the arrow.
        // Loaded on first use and kept; a shape the theme lacks falls back to
        // the arrow rather than to no pointer at all.
        let icon = match &self.state.cursor_status {
            CursorImageStatus::Named(icon) => *icon,
            _ => CursorIcon::Default,
        };
        if !self.cursors.contains_key(&(density, icon))
            && let Some(cursor) = Cursor::named(icon, density)
        {
            self.cursors.insert((density, icon), cursor);
        }
        let cursor = self
            .cursors
            .get(&(density, icon))
            .or_else(|| self.cursors.get(&(density, CursorIcon::Default)));

        // Clients learn which screen they are on -- and so what scale to
        // draw at -- from here, once a frame, before the frame is built.
        self.state.update_surface_outputs();

        // The blur's offscreen passes bind their own framebuffers, so they run
        // before `render_frame` takes the renderer. `None` — no panel open, no
        // shader, no buffer — falls through to the path this backend has always
        // taken, so an ordinary frame costs exactly what it did before.
        //
        // The scene is split once: `behind` is what gets blurred into the
        // texture, and it is also drawn sharp beneath the blur, since the blur
        // is cropped to the panel and the rest of the screen still has to
        // show something.
        let (front, behind) =
            render::elements_split(&mut self.renderer, &self.state, cursor, view, scale);
        let radius = self.state.blur_radius();
        let blurred = match self.state.blur_rect() {
            // The panel is on the focused screen; a blur for it on any other
            // would be a smear over nothing.
            Some(rect) if radius > 0.0 && rect.overlaps(view) => {
                let rect =
                    Rect::from_xywh(rect.x() - view.x(), rect.y() - view.y(), rect.w(), rect.h());
                self.blur
                    .as_mut()
                    .and_then(|blur| blur.pass(&mut self.renderer, &behind, size, scale, radius))
                    .and_then(|element| render::blur_element(element, rect, scale))
            }
            _ => None,
        };

        // Front to back: the panels, then the blurred patch under them, then
        // the desktop.
        let mut elements = front;
        elements.extend(blurred);
        elements.extend(behind);

        let Udev {
            renderer,
            allocator,
            screens,
            secondaries,
            ..
        } = self;
        let Some(screen) = screens.get_mut(&key) else {
            return;
        };

        // Every kind renders the same elements with the primary; they differ
        // only in where the pixels go afterwards. `Ok(true)` is a frame in
        // flight, `Ok(false)` a frame with nothing new in it.
        let presented = match &mut screen.scanout {
            ScreenScanout::Primary(drm) => present_primary(renderer, drm, &elements),
            ScreenScanout::Gpu { drm, bridge } => {
                match secondaries.get_mut(&key.0).map(|s| &mut s.scanout) {
                    Some(Scanout::Gpu {
                        renderer: secondary,
                        ..
                    }) => present_gpu(
                        renderer,
                        allocator,
                        secondary,
                        drm,
                        bridge,
                        &elements,
                        size,
                        scale,
                    ),
                    _ => Err(anyhow::anyhow!("its device is gone")),
                }
            }
            ScreenScanout::Dumb(dumb) => present_dumb(renderer, dumb, &elements, scale),
        };
        match presented {
            Ok(true) => {
                screen.awaiting_flip = true;
                screen.dirty = false;
            }
            // Nothing changed on screen; do not burn a page flip on it.
            Ok(false) => screen.dirty = false,
            Err(e) => {
                tracing::warn!(name = %screen.name, error = %format!("{e:#}"), "presenting frame")
            }
        }

        // Frame callbacks go out even when the frame had no damage, and to
        // windows not yet on screen. A client is allowed to commit without
        // changing any pixels purely to request a callback, and withholding it
        // — because we found nothing to repaint, or because the window has no
        // buffer yet — stalls that client forever.
        let now = self.start.elapsed().as_millis() as u32;
        for (surface, _) in self.state.frame_surfaces() {
            send_frames(&surface, now);
        }
    }

    fn on_vblank(&mut self, dev: libc::dev_t, crtc: crtc::Handle) {
        let key = (dev, crtc);
        if let Some(screen) = self.screens.get_mut(&key) {
            screen.awaiting_flip = false;
            let submitted = match &screen.scanout {
                ScreenScanout::Primary(drm) | ScreenScanout::Gpu { drm, .. } => {
                    drm.frame_submitted().map(|_| ())
                }
                ScreenScanout::Dumb(_) => Ok(()),
            };
            if let Err(e) = submitted {
                tracing::warn!(name = %screen.name, error = %e, "frame_submitted");
            }
        }
        // Only draw again if something actually changed. Rendering on every
        // vblank regardless would rebuild the whole scene 60 times a second on
        // a desktop that is sitting still.
        if self.screens.get(&key).is_some_and(|s| s.dirty) {
            self.render(key);
        }
    }

    fn on_input(&mut self, event: InputEvent<LibinputInputBackend>) {
        // Keyboards are tracked as they come and go so their lock LEDs can
        // follow the xkb state — see `Huginn::keyboard_led_devices`. A device
        // arriving mid-session is set to the session's current state, not
        // trusted to already show it.
        match event {
            InputEvent::DeviceAdded { mut device } => {
                if device.has_capability(DeviceCapability::Keyboard) {
                    device.led_update(self.state.keyboard_led_state.into());
                    self.state.keyboard_led_devices.push(device);
                }
                return;
            }
            InputEvent::DeviceRemoved { device } => {
                self.state.keyboard_led_devices.retain(|d| d != &device);
                return;
            }
            _ => {}
        }
        // Somebody is here. Noted before the event is interpreted, because a
        // keystroke that resolves to no binding and a pointer motion of one
        // pixel are both a person at the machine — the idle lock counts
        // presence, not commands. Device add and remove are excluded above:
        // hardware appearing is not somebody using it.
        self.state.note_activity();

        let InputEvent::Keyboard { event } = event else {
            // Motion, buttons and scroll all go to the shared handler, so the
            // udev and winit backends cannot drift apart.
            input::handle(&mut self.state, event);
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let key_state = event.state();
        // Read before the filter runs: the closure is handed the compositor
        // state, but working out whose layer this is needs the focus as it
        // stands now, not as the keystroke may leave it.
        let owns_super = self.state.focus_owns_super();
        // Read before the filter borrows the state.
        let launcher_open = self.state.launcher.is_open();
        let settings_open = self.state.settings.is_open();
        let pinned_open = self.state.pinned.is_open();
        let resizing = self.state.resizing;
        let locked = self.state.is_locked();
        let switcher_open = self.state.app_switcher_open();
        let action = self
            .keyboard
            .input::<Option<Action>, _>(
                &mut self.state,
                event.key_code(),
                key_state,
                serial,
                time,
                |_state, modifiers, handle| {
                    {
                        let sym = handle.modified_sym();
                        // The character the layout produces, so the
                        // launcher types what the user pressed rather
                        // than what a US keyboard would have.
                        let character = sym.key_char();
                        let launcher = launcher_open.then_some(character);
                        resolve(
                            key_state,
                            modifiers,
                            sym.raw(),
                            Modes {
                                focus_owns_super: owns_super,
                                launcher,
                                settings_open,
                                pinned_open,
                                resizing,
                                locked,
                                switcher_open,
                            },
                        )
                    }
                },
            )
            .flatten();
        if resizing
            && key_state == smithay::backend::input::KeyState::Pressed
            && !matches!(action, Some(Action::Resize(_)))
        {
            self.state.resizing = false;
        }
        if let Some(action) = action {
            self.apply(action, time);
        }
    }

    fn apply(&mut self, action: Action, time: u32) {
        let state = &mut self.state;
        match action {
            Action::Quit => {
                self.signal.stop();
                return;
            }
            Action::FocusNext => {
                state.space.cycle_focus(Direction::Forward);
            }
            Action::FocusPrev => {
                state.space.cycle_focus(Direction::Backward);
            }
            Action::PromoteFocused => {
                state.space.active_workspace_mut().promote_focused();
            }
            Action::Move(dir) => {
                state.space.move_focused(dir);
            }
            Action::Copy => {
                chord::send_ctrl(&self.keyboard, state, Keysym::c, time);
                return;
            }
            Action::Paste => {
                chord::send_ctrl(&self.keyboard, state, Keysym::v, time);
                return;
            }
            Action::ToggleHelp => {
                state.toggle_help();
                return;
            }
            Action::CloseFocused => {
                if let Some(surface) = state.space.focused().and_then(|id| state.surface(id)) {
                    surface.close();
                }
            }
            Action::Workspace(i) => {
                state.space.activate_workspace(i);
            }
            Action::SendToWorkspace(i) => {
                state.space.send_focused_to_workspace(i);
            }
            Action::FocusNextOutput => state.focus_next_output(),
            Action::SendToNextOutput => state.send_focused_to_next_output(),
            Action::EnterResize => {
                state.resizing = true;
                tracing::debug!("resize mode: arrows resize, Escape or Return leaves");
            }
            Action::Resize(dir) => state.resize_focused(dir),
            Action::LeaveResize => state.resizing = false,
            Action::ToggleCarousel => {
                state.toggle_workspace_carousel();
            }
            Action::OpenSettings => state.open_settings(),
            Action::Settings(key) => state.settings_key(key),
            Action::OpenFullSettings => state.open_full_settings(),
            Action::OpenStore => state.open_store(),
            Action::OpenLauncher => state.open_launcher(),
            Action::OpenPinned => state.open_pinned(),
            Action::Pinned(key) => state.pinned_key(key),
            Action::Launcher(key) => state.launcher_key(key),
            Action::DismissSwitcher => state.dismiss_app_switcher(),
            Action::Volume(key) => {
                state.volume_key(key);
                return;
            }
            Action::Spawn => {
                state.launch(None, &[state.terminal_command().to_owned()]);
            }
            // Nothing to do if it fails: `lock_and_launch` has already put the
            // desktop back and said why in the log, and there is no message
            // this compositor can put in front of somebody who just pressed it.
            Action::Lock => {
                self.lock_session();
            }
        }
        self.state.arrange();
        self.state.refresh_focus();
    }
}

/// Release frame callbacks so clients know they may draw again.
fn send_frames(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

crate::impl_xwm_handler!(Udev);
