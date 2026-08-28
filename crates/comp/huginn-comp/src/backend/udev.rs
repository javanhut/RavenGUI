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
        renderer::{ImportDma, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
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
    utils::{DeviceFd, SERIAL_COUNTER, Transform},
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
use crate::backend::input;
use crate::backend::keymap::{Action, Modes, help_line, resolve};
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
    session: LibSeatSession,
    /// The GPU we are driving, as udev identifies it. Every udev event carries
    /// a device id and most of them are about something else.
    device_id: libc::dev_t,
    manager: Manager,
    renderer: GlesRenderer,
    /// The blur, if its shader compiled. `None` means panels do not blur.
    blur: Option<crate::blur::Blur>,
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
        allocator,
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
                data.state.lock_and_launch();
            }
            TimeoutAction::ToDuration(data.state.idle_check_in(now))
        })
        .map_err(|e| anyhow::anyhow!("idle timer: {e}"))?;

    let lock_handle = handle.clone();
    crate::sleep::watch::<Udev, _>(&handle, move |data: &mut Udev| {
        // Locked *before* the display comes back, and that ordering is the
        // whole security of it. `reclaim_display` is what puts the next frame
        // on the panel; blanking the session first means the first thing drawn
        // after a resume is the lock screen and never the desktop. There is no
        // window in which the machine shows what it was doing, because the
        // compositor simply never composites it.
        data.lock_after_resume(&lock_handle);
        data.reclaim_display(true);
    });

    // For the density the state starts with; `relayout` loads it again when
    // the first screen — or a different one — says otherwise.
    let cursor = crate::pointer::Cursor::from_env(state.scale.advertised);

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
        session,
        device_id: node.dev_id(),
        manager,
        renderer,
        screens: HashMap::new(),
        keyboard,
        cursor,
        socket,
        start: Instant::now(),
        signal,
    };

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
fn preferred_mode(connector: &connector::Info) -> Option<smithay::reexports::drm::control::Mode> {
    use smithay::reexports::drm::control::ModeTypeFlags;

    connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
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
    /// Lock the session on the way back from a suspend.
    ///
    /// A laptop that sleeps when the lid closes and wakes showing the desktop
    /// when it opens has a lock screen in name only, so this is not optional
    /// and has no setting; see `theme::LOCK_SCREEN`.
    ///
    /// The timer is the safety catch described on `state::Lock`. The blank goes
    /// up before `raven-lock` has connected -- that is the point of it -- so
    /// something has to take the blank back down if the lock screen never
    /// arrives. Without it, a machine whose `raven-lock` is present but broken
    /// resumes into a black screen with no way past it but the power button.
    fn lock_after_resume(&mut self, handle: &LoopHandle<'static, Udev>) {
        /// How long the lock screen has to claim the session before the
        /// compositor gives up on it. Generous: this covers a cold exec, a
        /// Wayland connection and a socket round-trip to `ravend`, on a machine
        /// that is a few hundred milliseconds out of suspend and still bringing
        /// its disk back.
        const CLAIM_TIMEOUT: Duration = Duration::from_secs(10);

        if self.state.is_locked() {
            // Already held -- the machine was locked when it went to sleep.
            // Nothing to do, and nothing to start: the lock screen suspended
            // along with everything else and is still there.
            return;
        }

        if !self.state.lock_and_launch() {
            return;
        }

        let timer = Timer::from_duration(CLAIM_TIMEOUT);
        if let Err(e) = handle.insert_source(timer, |_, _, data: &mut Udev| {
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

    /// Take the display back and repaint all of it.
    ///
    /// Two things arrive here: a VT switch handing the session back, and a
    /// resume from suspend. They differ in one argument and one assumption.
    ///
    /// `disable_connectors` asks smithay to tear the connectors down before
    /// bringing them back up, which forces a full modeset rather than trusting
    /// the state the device claims to be in. A VT switch does not need it —
    /// we surrendered DRM master cleanly and took it back the same way. A
    /// resume does: nothing surrendered anything, we held master straight
    /// through a firmware transition, and what the device reports afterwards
    /// is not necessarily what is actually programmed into it.
    ///
    /// Every screen is marked dirty either way. Whatever is in the scanout
    /// buffer after either event is not something we put there.
    fn reclaim_display(&mut self, disable_connectors: bool) {
        if let Err(e) = self.manager.activate(disable_connectors) {
            tracing::error!(error = %e, "could not reactivate DRM");
        }
        for screen in self.screens.values_mut() {
            screen.dirty = true;
            // A flip we were waiting on before the interruption will never
            // complete. Left set, it would make every future frame look like
            // one already in flight and nothing would ever be drawn again.
            screen.awaiting_flip = false;
        }
        self.state.queue_redraw();
    }

    /// A DRM device appeared, changed, or went away.
    ///
    /// Almost every event here is about a device we are not driving — udev
    /// reports the whole `drm` subsystem — so the device id is checked first.
    fn on_udev(&mut self, event: &UdevEvent) {
        match *event {
            UdevEvent::Changed { device_id } if device_id == self.device_id => {
                // A connector changed state. Which one, udev does not say.
                tracing::debug!("GPU reported a change; rescanning connectors");
                self.sync_connectors();
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                // Every screen, buffer and framebuffer we hold belongs to a
                // device that no longer exists. There is nothing to fall back
                // to, so hand the session back rather than spin on EIO.
                tracing::error!("the GPU huginn is driving was removed; shutting down");
                self.signal.stop();
            }
            UdevEvent::Added {
                device_id,
                ref path,
            } if device_id != self.device_id => {
                // Multi-GPU needs a DrmOutputManager per device and a way to
                // move buffers between them; until then, say so rather than
                // leave a plugged-in card silently dark.
                tracing::info!(
                    ?path,
                    "another GPU appeared; huginn drives only the primary one"
                );
            }
            _ => {}
        }
    }

    /// Bring `screens` into line with what is actually plugged in.
    ///
    /// Runs once at startup and again on every udev `Changed` event for our
    /// GPU. It has to be a full reconciliation rather than an add or a remove:
    /// the event says only that *something* about the device changed, and a
    /// single change can be both — a KVM switch flipping inputs disconnects one
    /// connector and connects another before we get a chance to look. It can
    /// also be neither: the same connector, still connected, with a different
    /// monitor on the end of it, which is why connectors we already drive are
    /// re-read rather than skipped.
    fn sync_connectors(&mut self) {
        let resources = match self.manager.device().resource_handles() {
            Ok(resources) => resources,
            Err(e) => {
                tracing::warn!(error = %e, "reading DRM resources; keeping the current outputs");
                return;
            }
        };

        // Force-probing is the whole point of the rescan. A connector keeps its
        // handle across a swap, and without a probe the kernel answers from its
        // cache — the old monitor's state and the old monitor's mode list.
        let connected: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|handle| self.manager.device().get_connector(*handle, true).ok())
            .filter(|connector| connector.state() == connector::State::Connected)
            .collect();
        let live: HashSet<connector::Handle> =
            connected.iter().map(connector::Info::handle).collect();

        // --- gone ---
        // Dropping the Screen drops its DrmOutput, which is what releases the
        // CRTC back to the manager for the next connector to claim.
        let dh = self.dh.clone();
        let mut changed = false;
        self.screens.retain(|_, screen| {
            if live.contains(&screen.connector) {
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
            .values()
            .map(|screen| screen.connector)
            .collect();
        for connector in &connected {
            if known.contains(&connector.handle()) {
                changed |= self.refresh_screen(connector);
                continue;
            }
            match self.add_screen(&resources, connector) {
                Ok(()) => changed = true,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "could not bring up a connector")
                }
            }
        }

        if changed {
            self.relayout();
            self.state.queue_redraw();
        }
    }

    /// Give one newly connected connector a CRTC, an output, and a global.
    fn add_screen(
        &mut self,
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
        let Some(crtc) = self.free_crtc(resources, connector) else {
            anyhow::bail!("no free CRTC for {name}");
        };

        let (wl_mode, scale) = describe(connector, &mode);
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

        let dh = self.dh.clone();
        let drm =
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
                .map_err(|e| {
                    // The global went out before the CRTC came up, so take it back
                    // rather than advertise an output that renders nothing.
                    dh.remove_global::<Huginn>(global.clone());
                    anyhow::anyhow!("initialising {name}: {e}")
                })?;

        tracing::info!(
            %name,
            ?crtc,
            width = wl_mode.size.w,
            height = wl_mode.size.h,
            refresh_mhz = wl_mode.refresh,
            "output up"
        );

        self.screens.insert(
            crtc,
            Screen {
                drm,
                output,
                global,
                connector: connector.handle(),
                name,
                mode,
                scale,
                awaiting_flip: false,
                dirty: true,
            },
        );
        Ok(())
    }

    /// Follow a change to the panel behind a connector we already drive.
    ///
    /// A KVM switch, or a monitor swapped while the cable stays in, keeps the
    /// connector handle and changes everything else: the preferred mode, the
    /// physical size, and with it the scale. The rescan force-probed the
    /// connector, so what it reports now is the new panel and not the kernel's
    /// memory of the old one. Returns whether anything changed.
    ///
    /// The mode is switched in place rather than by tearing the screen down:
    /// clients keep the `wl_output` they know and see a mode event on it, the
    /// same as they would from any compositor with a display settings page.
    /// [`DrmOutput::use_mode`] only stages the mode for the next commit, so
    /// marking the screen dirty is what actually performs the modeset.
    fn refresh_screen(&mut self, connector: &connector::Info) -> bool {
        let Some((&crtc, _)) = self
            .screens
            .iter()
            .find(|(_, screen)| screen.connector == connector.handle())
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

        let screen = self
            .screens
            .get_mut(&crtc)
            .expect("looked up by connector just above");
        if mode == screen.mode && scale == screen.scale {
            return false;
        }

        if mode != screen.mode {
            if let Err(e) =
                screen.drm.use_mode(
                    mode,
                    &mut self.renderer,
                    &DrmOutputRenderElements::<
                        GlesRenderer,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                    >::default(),
                )
            {
                tracing::warn!(name = %screen.name, error = %e, "switching mode; keeping the old one");
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

    /// A CRTC this connector can drive that no screen has already taken.
    fn free_crtc(
        &self,
        resources: &ResourceHandles,
        connector: &connector::Info,
    ) -> Option<crtc::Handle> {
        connector
            .encoders()
            .iter()
            .filter_map(|encoder| self.manager.device().get_encoder(*encoder).ok())
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .find(|crtc| !self.screens.contains_key(crtc))
    }

    /// Lay the screens out left to right and tell the core how much room it has.
    ///
    /// In logical pixels throughout: a 4K panel at 2× is 1920 wide here, which
    /// is the unit the desktop is laid out in and the renderer scales back up
    /// from. Using the panel's native size instead would hand the core a
    /// desktop twice as large as the screen, of which clients — told 2× —
    /// would then show a quarter.
    ///
    /// Ordered by connector name rather than by CRTC or discovery order, so
    /// that unplugging the middle of three monitors and plugging it back in
    /// puts it back where it was instead of shuffling the other two.
    fn relayout(&mut self) {
        let mut order: Vec<crtc::Handle> = self.screens.keys().copied().collect();
        order.sort_by(|a, b| self.screens[a].name.cmp(&self.screens[b].name));

        let mut x = 0;
        let mut leftmost = None;
        for crtc in order {
            let screen = &self.screens[&crtc];
            screen
                .output
                .change_current_state(None, None, None, Some((x, 0).into()));
            leftmost.get_or_insert(screen.scale);
            x += screen.scale.logical.w;
        }

        // Single-output for now: huginn-core still models one scale and one
        // usable area, so the leftmost screen defines both. Multi-output needs
        // a per-output area in the core before it means anything here. With
        // nothing connected the last known scale is kept — resizing every
        // client to zero and back is a configure storm that buys nothing while
        // no one can see the screen.
        if let Some(scale) = leftmost
            && scale != self.state.scale
        {
            // `set_output_scale` derives the area from the logical size and
            // reflows the layer surfaces and the windows underneath them.
            // The panels the shell draws are composed for the new density the
            // next time each is refreshed; the cursor bitmap is the one thing
            // loaded once, so it is loaded again here.
            self.state.set_output_scale(scale);
            if self
                .cursor
                .as_ref()
                .is_none_or(|cursor| cursor.density != scale.advertised)
            {
                self.cursor = Cursor::from_env(scale.advertised);
            }
        }
    }

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
        // Advance animations before assembling the scene, so this frame shows
        // where they are now rather than where they were last frame.
        self.state.tick_animations();
        if !self.session.is_active() {
            return;
        }

        // The blur's offscreen passes bind their own framebuffers, so they run
        // before `render_frame` takes the renderer. `None` — no panel open, no
        // shader, no buffer — falls through to the path this backend has always
        // taken, so an ordinary frame costs exactly what it did before.
        let radius = self.state.blur_radius();
        let blurred = if radius > 0.0 {
            let size = self
                .screens
                .get(&crtc)
                .and_then(|screen| screen.output.current_mode())
                .map(|mode| mode.size)
                .unwrap_or_default();
            let (_, behind) =
                render::elements_split(&mut self.renderer, &self.state, self.cursor.as_ref());
            self.blur.as_mut().and_then(|blur| {
                blur.pass(
                    &mut self.renderer,
                    &behind,
                    size,
                    self.state.scale.fractional(),
                    radius,
                )
            })
        } else {
            None
        };

        let mut elements = match &blurred {
            Some(_) => {
                render::elements_split(&mut self.renderer, &self.state, self.cursor.as_ref()).0
            }
            None => render::elements(&mut self.renderer, &self.state, self.cursor.as_ref()),
        };
        // Front to back, so the blurred desktop goes on the end.
        if let Some(element) = blurred {
            elements.push(crate::render::HuginnElement::Blur(element));
        }

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
        let resizing = self.state.resizing;
        let locked = self.state.is_locked();
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
                                resizing,
                                locked,
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
                state
                    .space
                    .active_workspace_mut()
                    .cycle_focus(Direction::Forward);
            }
            Action::FocusPrev => {
                state
                    .space
                    .active_workspace_mut()
                    .cycle_focus(Direction::Backward);
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
            Action::EnterResize => {
                state.resizing = true;
                tracing::debug!("resize mode: arrows resize, Escape or Return leaves");
            }
            Action::Resize(dir) => state.resize_focused(dir),
            Action::LeaveResize => state.resizing = false,
            Action::ToggleCarousel => {
                state.toggle_workspace_carousel();
            }
            Action::OpenSettings => {
                let now = state.uptime();
                state.settings.open(now);
                state.refresh_settings();
            }
            Action::Settings(key) => {
                let now = state.uptime();
                match state.settings.press(key, now) {
                    crate::settings::Outcome::Dismissed | crate::settings::Outcome::Redraw => {
                        state.refresh_settings()
                    }
                    crate::settings::Outcome::Unchanged => {}
                }
            }
            Action::OpenLauncher => state.open_launcher(),
            Action::Launcher(key) => state.launcher_key(key),
            Action::Spawn => {
                state.launch(None, &[state.terminal_command().to_owned()]);
            }
            // Nothing to do if it fails: `lock_and_launch` has already put the
            // desktop back and said why in the log, and there is no message
            // this compositor can put in front of somebody who just pressed it.
            Action::Lock => {
                state.lock_and_launch();
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
