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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

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
            ImportDma,
            element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::primary_gpu,
    },
    input::keyboard::KeyboardHandle,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction, generic::Generic,
        },
        drm::control::{Device as _, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{Display, protocol::wl_surface::WlSurface},
    },
    utils::{DeviceFd, SERIAL_COUNTER, Transform},
    wayland::{
        compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
        socket::ListeningSocketSource,
    },
};

use huginn_core::{geometry::Rect, workspace::Direction};

use crate::backend::input;
use crate::backend::keymap::{Action, HELP, resolve};
use crate::pointer::Cursor;
use crate::render;
use crate::state::{ClientState, Huginn};

/// Background colour of an empty workspace.
const CLEAR: [f32; 4] = [0.06, 0.06, 0.09, 1.0];

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type Manager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type Surface = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

/// One scan-out surface: a CRTC with a connector attached.
struct Screen {
    drm: Surface,
    /// Held, not read: this owns the wl_output global for as long as the screen
    /// exists, and is what a future mode change or hotplug would act on.
    #[allow(dead_code)]
    output: Output,
    name: String,
    /// A frame is queued and we are waiting for its page flip. Queueing another
    /// before the flip completes returns EBUSY.
    awaiting_flip: bool,
    /// Something changed since the last frame we sent.
    dirty: bool,
}

struct Udev {
    state: Huginn,
    display: Display<Huginn>,
    session: LibSeatSession,
    manager: Manager,
    renderer: GlesRenderer,
    screens: HashMap<crtc::Handle, Screen>,
    keyboard: KeyboardHandle<Huginn>,
    cursor: Option<Cursor>,
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
        .open(&gpu_path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)
        .map_err(|e| anyhow::anyhow!("session refused to open {gpu_path:?}: {e}"))?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm_device, drm_notifier) =
        DrmDevice::new(device_fd.clone(), true).context("creating DRM device")?;
    let gbm = GbmDevice::new(device_fd).context("creating GBM device")?;

    // --- renderer: the only unsafe in the project, and it lives elsewhere ---
    let mut renderer =
        huginn_egl::renderer_for(gbm.clone()).context("initialising the GLES renderer")?;

    let render_formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let mut manager = DrmOutputManager::new(
        drm_device,
        allocator,
        GbmFramebufferExporter::new(gbm.clone(), node.node_with_type(NodeType::Render).and_then(|r| r.ok())),
        Some(gbm.clone()),
        // Xrgb8888 first: opaque scan-out is the common case and avoids the
        // compositor blending against nothing.
        [Fourcc::Xrgb8888, Fourcc::Argb8888],
        render_formats,
    );

    let mut state = Huginn::new(&dh, Rect::from_xywh(0, 0, 0, 0));

    match EGLDevice::device_for_display(renderer.egl_context().display())
        .and_then(|device| device.try_get_render_node())
    {
        Ok(Some(render_node)) => {
            let formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
            state.enable_dmabuf(&dh, render_node.dev_id(), formats);
        }
        Ok(None) => tracing::warn!("EGL display has no render node; clients stay on shm"),
        Err(e) => tracing::warn!(error = %e, "could not identify the render node; clients stay on shm"),
    }

    // --- outputs ---------------------------------------------------------
    let screens = scan_connectors(&mut manager, &mut renderer, &mut state, &dh)?;
    if screens.is_empty() {
        anyhow::bail!("no connected displays found on {gpu_path:?}");
    }

    // --- input -----------------------------------------------------------
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| anyhow::anyhow!("libinput refused seat {seat_name}"))?;
    let input_backend = LibinputInputBackend::new(libinput.clone());

    // --- wayland ---------------------------------------------------------
    let socket_source = ListeningSocketSource::new_auto().context("binding wayland socket")?;
    let socket = socket_source.socket_name().to_string_lossy().into_owned();

    handle
        .insert_source(socket_source, |stream, _, data: &mut Udev| {
            if let Err(e) = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
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
    handle
        .insert_source(drm_notifier, |event, meta, data: &mut Udev| match event {
            DrmEvent::VBlank(crtc) => data.on_vblank(crtc),
            DrmEvent::Error(e) => {
                tracing::error!(error = %e, ?meta, "DRM error");
            }
        })
        .map_err(|e| anyhow::anyhow!("DRM source: {e}"))?;

    handle
        .insert_source(input_backend, |event, _, data: &mut Udev| {
            data.on_input(event);
        })
        .map_err(|e| anyhow::anyhow!("libinput source: {e}"))?;

    // VT switching. On pause we must drop DRM master and stop touching
    // devices; on activate we take it back and force a full repaint, because
    // whatever was on screen while we were away is not ours.
    handle
        .insert_source(session_notifier, |event, _, data: &mut Udev| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused; releasing devices");
                data.manager.pause();
                for screen in data.screens.values_mut() {
                    screen.awaiting_flip = false;
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session activated; reclaiming devices");
                if let Err(e) = data.manager.activate(false) {
                    tracing::error!(error = %e, "could not reactivate DRM");
                }
                for screen in data.screens.values_mut() {
                    screen.dirty = true;
                    screen.awaiting_flip = false;
                }
                data.state.queue_redraw();
            }
        })
        .map_err(|e| anyhow::anyhow!("session source: {e}"))?;

    // Loaded once. XCURSOR_THEME/XCURSOR_SIZE are the conventional way users
    // pick a cursor, so honour them rather than inventing our own setting.
    let cursor = crate::pointer::Cursor::load(
        &std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_owned()),
        "default",
        std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24),
    );
    if cursor.is_none() {
        tracing::warn!("no cursor theme found; the pointer will be invisible over the background");
    }

    let keyboard = state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("adding keyboard")?;

    tracing::info!(socket = %socket, screens = screens.len(), "huginn is up on DRM");
    tracing::info!("clients: WAYLAND_DISPLAY={socket} <command>");
    tracing::info!("{HELP}");

    let mut data = Udev {
        state,
        display,
        session,
        manager,
        renderer,
        screens,
        keyboard,
        cursor,
        socket,
        start: Instant::now(),
        signal,
    };
    // Draw once before entering the loop. calloop only runs the post-dispatch
    // callback after an event, so with nothing connected yet the first frame
    // would otherwise wait for one — indistinguishable from a black screen.
    data.render_dirty();

    // A one-second backstop rather than blocking forever: if a redraw flag is
    // ever missed, the cost is a second of staleness instead of a display that
    // never updates again.
    event_loop
        .run(Some(std::time::Duration::from_secs(1)), &mut data, |data| {
            if data.state.take_redraw() {
                for screen in data.screens.values_mut() {
                    screen.dirty = true;
                }
            }
            data.render_dirty();
            if let Err(e) = data.display.flush_clients() {
                tracing::warn!(error = %e, "flushing clients");
            }
        })
        .context("running the event loop")?;

    tracing::info!("huginn is down");
    Ok(())
}

/// Find connected connectors and give each one a CRTC and an output.
///
/// Runs once at startup. Hotplug is not handled yet: plugging a monitor in
/// while Huginn is running will do nothing until it restarts.
fn scan_connectors(
    manager: &mut Manager,
    renderer: &mut GlesRenderer,
    state: &mut Huginn,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
) -> Result<HashMap<crtc::Handle, Screen>> {
    let resources = manager
        .device()
        .resource_handles()
        .context("reading DRM resources")?;

    let mut screens = HashMap::new();
    let mut used_crtcs: Vec<crtc::Handle> = Vec::new();
    // Outputs are laid out left to right in the order connectors are found.
    let mut x = 0;

    for handle in resources.connectors() {
        let Ok(connector) = manager.device().get_connector(*handle, false) else {
            continue;
        };
        if connector.state() != connector::State::Connected {
            continue;
        }
        let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());

        // The preferred mode is the panel's native one. Falling back to the
        // first available beats refusing to light up the screen at all.
        let Some(mode) = connector
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED))
            .or_else(|| connector.modes().first())
            .copied()
        else {
            tracing::warn!(%name, "connector reports no modes; skipping");
            continue;
        };

        // Find a CRTC this connector can drive that nothing else has taken.
        let Some(crtc) = connector
            .encoders()
            .iter()
            .filter_map(|e| manager.device().get_encoder(*e).ok())
            .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
            .find(|c| !used_crtcs.contains(c))
        else {
            tracing::warn!(%name, "no free CRTC for this connector; skipping");
            continue;
        };
        used_crtcs.push(crtc);

        let (w, h) = mode.size();
        let refresh = refresh_mhz(&mode);
        let wl_mode = Mode {
            size: (i32::from(w), i32::from(h)).into(),
            refresh,
        };
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                // Physical size is in millimetres and comes back as u32.
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: Subpixel::Unknown,
                make: "Huginn".to_owned(),
                model: name.clone(),
            },
        );
        output.change_current_state(
            Some(wl_mode),
            Some(Transform::Normal),
            None,
            Some((x, 0).into()),
        );
        output.set_preferred(wl_mode);
        state.add_output(&output, dh);

        let drm = manager
            .initialize_output(
                crtc,
                mode,
                &[connector.handle()],
                &output,
                None,
                renderer,
                &DrmOutputRenderElements::<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>::default(),
            )
            .map_err(|e| anyhow::anyhow!("initialising {name}: {e}"))?;

        tracing::info!(%name, ?crtc, width = w, height = h, refresh_mhz = refresh, "output up");

        // Single-output for now: huginn-core still models one usable area, so
        // the first screen defines it. Multi-output needs a per-output area in
        // the core before it means anything here.
        if screens.is_empty() {
            state.set_output_area(Rect::from_xywh(0, 0, i32::from(w), i32::from(h)));
        }
        x += i32::from(w);

        screens.insert(
            crtc,
            Screen {
                drm,
                output,
                name,
                awaiting_flip: false,
                dirty: true,
            },
        );
    }

    Ok(screens)
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

impl Udev {
    /// Render every screen that has changes and is not already waiting on a flip.
    fn render_dirty(&mut self) {
        let crtcs: Vec<crtc::Handle> = self
            .screens
            .iter()
            .filter(|(_, s)| s.dirty && !s.awaiting_flip)
            .map(|(c, _)| *c)
            .collect();
        for crtc in crtcs {
            self.render(crtc);
        }
    }

    fn render(&mut self, crtc: crtc::Handle) {
        if !self.session.is_active() {
            return;
        }

        let elements = render::elements(&mut self.renderer, &self.state, self.cursor.as_ref());

        let Some(screen) = self.screens.get_mut(&crtc) else {
            return;
        };

        match screen
            .drm
            .render_frame(&mut self.renderer, &elements, CLEAR, FrameFlags::DEFAULT)
        {
            Ok(result) => {
                if result.is_empty {
                    // Nothing changed on screen; do not burn a page flip on it.
                    screen.dirty = false;
                } else {
                    match screen.drm.queue_frame(()) {
                        Ok(()) => {
                            screen.awaiting_flip = true;
                            screen.dirty = false;
                        }
                        Err(e) => tracing::warn!(name = %screen.name, error = %e, "queueing frame"),
                    }
                }
            }
            Err(e) => tracing::warn!(name = %screen.name, error = %e, "rendering frame"),
        }

        // Frame callbacks go out even when the frame had no damage. A client is
        // allowed to commit without changing any pixels purely to request a
        // callback, and withholding it because we found nothing to repaint
        // stalls that client forever.
        let now = self.start.elapsed().as_millis() as u32;
        for (surface, _) in self.state.scene() {
            send_frames(surface, now);
        }
    }

    /// A queued frame reached the screen.
    fn on_vblank(&mut self, crtc: crtc::Handle) {
        if let Some(screen) = self.screens.get_mut(&crtc) {
            screen.awaiting_flip = false;
            if let Err(e) = screen.drm.frame_submitted() {
                tracing::warn!(name = %screen.name, error = %e, "frame_submitted");
            }
        }
        // Only draw again if something actually changed. Rendering on every
        // vblank regardless would rebuild the whole scene 60 times a second on
        // a desktop that is sitting still.
        if self.screens.get(&crtc).is_some_and(|s| s.dirty) {
            self.render(crtc);
        }
    }

    fn on_input(&mut self, event: InputEvent<LibinputInputBackend>) {
        let InputEvent::Keyboard { event } = event else {
            // Motion, buttons and scroll all go to the shared handler, so the
            // udev and winit backends cannot drift apart.
            input::handle(&mut self.state, event);
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();
        let action = self.keyboard.input::<Action, _>(
            &mut self.state,
            event.key_code(),
            event.state(),
            serial,
            time,
            |_state, modifiers, handle| resolve(modifiers, handle.modified_sym().raw()),
        );
        if let Some(action) = action {
            self.apply(action);
        }
    }

    fn apply(&mut self, action: Action) {
        let state = &mut self.state;
        match action {
            Action::Quit => {
                self.signal.stop();
                return;
            }
            Action::FocusNext => {
                state.space.active_workspace_mut().cycle_focus(Direction::Forward);
            }
            Action::FocusPrev => {
                state.space.active_workspace_mut().cycle_focus(Direction::Backward);
            }
            Action::PromoteFocused => {
                state.space.active_workspace_mut().promote_focused();
            }
            Action::CloseFocused => {
                if let Some(surface) = state.space.focused().and_then(|id| state.surface(id)) {
                    surface.send_close();
                }
            }
            Action::Workspace(i) => {
                state.space.activate_workspace(i);
            }
            Action::SendToWorkspace(i) => {
                state.space.send_focused_to_workspace(i);
            }
            Action::Spawn => {
                let cmd = self.state.terminal_command();
                match std::process::Command::new(&cmd)
                    .env("WAYLAND_DISPLAY", &self.socket)
                    .spawn()
                {
                    Ok(_) => tracing::info!(%cmd, "spawned"),
                    Err(e) => tracing::warn!(%cmd, error = %e, "spawn failed"),
                }
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
