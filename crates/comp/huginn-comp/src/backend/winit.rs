//! Nested backend: the compositor runs inside a window on an existing session.
//!
//! This is the development backend. It needs no seat, no DRM master and no TTY,
//! so it can be driven entirely over ssh — which the udev backend cannot, since
//! logind only grants DRM master to the active session on a seat.
//!
//! # Event loop
//!
//! Everything hangs off one calloop `EventLoop`: the listening socket, the
//! Wayland display's poll fd, and winit's own event source. That structure is
//! not needed for winit alone — a pump loop worked — but the udev backend has
//! to multiplex DRM vblank, libinput and session signals, and retrofitting an
//! event loop underneath a working DRM backend is far worse than putting one in
//! first.
//!
//! Rendering is damage-driven: the loop wakes on a short timeout but only draws
//! when something asked it to. An idle desktop does no GPU work. Animating
//! clients keep themselves going, because every buffer they commit sets the
//! flag that produces the next frame callback.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        egl::EGLDevice,
        input::{Event as _, InputEvent, KeyboardKeyEvent},
        renderer::{
            Color32F, Frame, ImportDma, Renderer, gles::GlesRenderer, utils::draw_render_elements,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::keyboard::{KeyboardHandle, Keysym},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction,
            RegistrationToken,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{Display, protocol::wl_surface::WlSurface},
    },
    utils::{Rectangle, SERIAL_COUNTER, Transform},
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
use smithay::input::pointer::{CursorIcon, CursorImageStatus};

/// Background colour of an empty workspace.
const CLEAR: Color32F = Color32F::new(0.06, 0.06, 0.09, 1.0);

/// How long the loop sleeps when nothing is happening.
///
/// Not a frame budget — the loop wakes this often but only renders when there
/// is something to render. It exists so a missed redraw flag costs one frame of
/// latency rather than freezing the display until the next client commit.
const TICK: Duration = Duration::from_millis(16);

/// Everything the event-loop callbacks touch.
struct Nested {
    state: Huginn,
    display: Display<Huginn>,
    backend: WinitGraphicsBackend<GlesRenderer>,
    /// The blur, if its shader compiled. `None` means panels do not blur and
    /// everything else works exactly as before.
    blur: Option<crate::blur::Blur>,
    output: Output,
    keyboard: KeyboardHandle<Huginn>,
    /// Theme cursors by shape, loaded as clients ask for them. The nested
    /// window is 1x for the life of the process, so density is not a key.
    cursors: HashMap<CursorIcon, Cursor>,
    start: Instant,
    signal: LoopSignal,
    /// The event loop, for arming the lock claim timeout from a keystroke.
    handle: LoopHandle<'static, Nested>,
    /// The screen recording under way, if there is one. See `crate::record`.
    recording: Option<crate::record::Recording>,
    /// The timer that ticks it, so stopping can take the timer out too.
    recording_timer: Option<RegistrationToken>,
    /// The timer that serves clients' captures, while it has anything to
    /// do. See `crate::capture`.
    capture_timer: Option<RegistrationToken>,
}

pub(crate) fn run() -> Result<()> {
    let mut event_loop: EventLoop<Nested> = EventLoop::try_new().context("creating event loop")?;
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    let mut display: Display<Huginn> = Display::new().context("creating wayland display")?;
    let dh = display.handle();

    let (mut backend, winit_source) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("winit backend: {e}"))?;

    let size = backend.window_size();
    let mut state = Huginn::new(&dh, Rect::from_xywh(0, 0, size.w, size.h));
    // The key a surface's imported texture is filed under, for the close
    // animation's snapshots.
    state.set_render_context(backend.renderer().context_id());

    // XWayland. Started here rather than after the backend is up because it is
    // asynchronous either way: this only spawns the server and registers the
    // event source, and the window manager is created later, when XWayland
    // reports ready. Fail-soft -- if the `Xwayland` binary is not installed the
    // compositor runs exactly as before, without X11 clients.
    crate::xwayland::start::<Nested>(&dh, &handle);

    // Watch the application directories, so an application installed during
    // the session reaches the launcher and the dock without a logout. Started
    // here for the same reason XWayland is: it only registers an event source,
    // and everything it does happens later, from the loop.
    crate::appwatch::start::<Nested>(&handle);
    crate::configwatch::start::<Nested>(&handle);
    crate::fileindex::start::<Nested>(&handle, &mut state);
    // BlueZ, for the quick settings row. Same shape: a thread and a wake-up.
    crate::bluetooth::start::<Nested>(&handle, &mut state);
    // org.freedesktop.Notifications, opt-in until cards are drawn. Same shape.
    crate::notifications::start::<Nested>(&handle, &mut state);

    // Tell clients which GPU to allocate on and which formats we can import.
    // Failing here is not fatal — clients simply stay on shm — so every branch
    // warns and carries on rather than aborting startup.
    match EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node())
    {
        Ok(Some(node)) => {
            let formats: Vec<_> = backend.renderer().dmabuf_formats().into_iter().collect();
            state.enable_dmabuf(&dh, node.dev_id(), formats);
        }
        Ok(None) => tracing::warn!("EGL display has no render node; clients stay on shm"),
        Err(e) => {
            tracing::warn!(error = %e, "could not identify the render node; clients stay on shm")
        }
    }

    // A real wl_output makes toolkits behave: GTK and Qt both query scale and
    // mode before they will map a window at the right size.
    let output = Output::new(
        "huginn-winit".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Huginn".to_owned(),
            model: "Winit".to_owned(),
        },
    );
    let mode = Mode {
        size,
        refresh: 60_000,
    };
    // A nested window reports no physical size, so the scale policy falls back
    // to 1x. That is correct here and not a limitation: the host compositor
    // already applied its own scale to the window we were given, and applying
    // a second one on top would double it.
    let scale = OutputScale::for_output(Size::new(size.w, size.h), Size::new(0, 0));
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(advertise(scale)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.set_output_scale("huginn-winit", Some(output.clone()), scale);
    // One output for the life of the process; nothing ever withdraws it.
    let _global = state.add_output(&output, &dh);

    // new_auto picks the first free name, so a second instance does not fail to
    // start just because the first one has huginn-1.
    let socket_source = ListeningSocketSource::new_auto().context("binding wayland socket")?;
    let socket = socket_source.socket_name().to_string_lossy().into_owned();

    let (disconnect, disconnects) =
        calloop::ping::make_ping().context("creating the client-disconnect ping")?;
    handle
        .insert_source(disconnects, |_, _, data: &mut Nested| {
            if data.state.recover_lost_lock() {
                data.arm_claim_timeout();
            }
        })
        .map_err(|e| anyhow::anyhow!("client-disconnect source: {e}"))?;

    handle
        .insert_source(socket_source, move |stream, _, data: &mut Nested| {
            if let Err(e) = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::new(disconnect.clone())))
            {
                tracing::warn!(error = %e, "could not accept a client");
            }
        })
        .map_err(|e| anyhow::anyhow!("wayland socket source: {e}"))?;

    // Level-triggered: if a dispatch leaves requests unread, the loop wakes
    // again immediately rather than stalling until the next client writes.
    let poll_fd = display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .context("cloning the display poll fd")?;
    handle
        .insert_source(
            Generic::new(poll_fd, Interest::READ, CalloopMode::Level),
            |_, _, data: &mut Nested| {
                data.display
                    .dispatch_clients(&mut data.state)
                    .map_err(std::io::Error::other)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("wayland display source: {e}"))?;

    handle
        .insert_source(winit_source, |event, _, data: &mut Nested| {
            data.on_winit(event);
        })
        .map_err(|e| anyhow::anyhow!("winit source: {e}"))?;

    // Cloned rather than fetched per event: get_keyboard borrows the seat out
    // of Huginn, which conflicts with passing Huginn itself to keyboard.input.
    // Loaded once: the nested window is 1× for the life of the process.
    let mut cursors = HashMap::new();
    if let Some(cursor) = crate::pointer::Cursor::from_env(state.scale().advertised) {
        cursors.insert(CursorIcon::Default, cursor);
    }

    let keyboard = state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("adding keyboard")?;

    state.set_socket(socket.clone());
    tracing::info!(socket = %socket, "huginn is up");
    tracing::info!("clients: WAYLAND_DISPLAY={socket} <command>");
    tracing::info!("{}", help_line());

    let blur = crate::blur::Blur::compile(backend.renderer());
    let mut data = Nested {
        blur,
        state,
        display,
        backend,
        output,
        keyboard,
        cursors,
        start: Instant::now(),
        signal,
        handle: handle.clone(),
        recording: None,
        recording_timer: None,
        capture_timer: None,
    };

    // NOTE: no SIGTERM handling. calloop's signal source needs its `signals`
    // feature, which smithay does not enable. Fine for a nested dev backend
    // where Ctrl-C reaches the process directly; the udev backend will need it,
    // because a compositor killed on a TTY has to hand the session back.
    event_loop
        .run(TICK, &mut data, |data| {
            if let Err(e) = data.dispatch_end_of_cycle() {
                tracing::error!("{e:#}");
                data.signal.stop();
            }
        })
        .context("running the event loop")?;

    tracing::info!("huginn is down");
    Ok(())
}

impl Nested {
    /// Lock the session, with the same claim timeout the udev backend gives
    /// it: a lock screen that never claims the blank must not leave a nested
    /// window that can only be closed from outside.
    fn lock_session(&mut self) {
        if self.state.is_locked() || !self.state.lock_and_launch() {
            return;
        }
        self.arm_claim_timeout();
    }

    /// The claim timeout, armed for the first lock screen and for each one
    /// started again after a crash; see `Huginn::recover_lost_lock`.
    fn arm_claim_timeout(&mut self) {
        const CLAIM_TIMEOUT: Duration = Duration::from_secs(10);
        let timer = Timer::from_duration(CLAIM_TIMEOUT);
        if let Err(e) = self.handle.insert_source(timer, |_, _, data: &mut Nested| {
            data.state.abandon_lock_if_unclaimed();
            TimeoutAction::Drop
        }) {
            tracing::error!(error = %e, "cannot arm the lock timeout; unlocking again");
            self.state.abandon_lock_if_unclaimed();
        }
    }

    /// Render if anything asked us to, then flush.
    fn dispatch_end_of_cycle(&mut self) -> Result<()> {
        self.state.refresh();
        if self.state.take_redraw() {
            // render flushes internally, before submit blocks.
            self.render()?;
            // One window, so a frame drawn is a frame of the recorded screen.
            if let Some(recording) = self.recording.as_mut() {
                recording.note_damage();
            }
            // And of every client's capture, for the same reason.
            self.state.captures.note_damage_all();
        } else {
            // Flush even without a frame: a client waiting on a configure it
            // never receives will sit there forever.
            self.display.flush_clients().context("flushing clients")?;
        }
        self.schedule_captures();
        Ok(())
    }

    /// Arm the capture timer if a client's capture has work and it is not
    /// armed, or fire it at once if something has asked for that — a frame
    /// requested, or a source with a frame pending that just changed.
    fn schedule_captures(&mut self) {
        let kick = self.state.captures.take_kick();
        if !kick && (self.capture_timer.is_some() || !self.state.captures.wants_ticks()) {
            return;
        }
        if let Some(token) = self.capture_timer.take() {
            self.handle.remove(token);
        }
        let timer = Timer::immediate();
        match self.handle.insert_source(timer, |_, _, data: &mut Nested| {
            match data.tick_captures() {
                Some(wait) => TimeoutAction::ToDuration(wait),
                None => {
                    data.capture_timer = None;
                    TimeoutAction::Drop
                }
            }
        }) {
            Ok(token) => self.capture_timer = Some(token),
            Err(e) => tracing::error!(error = %e, "cannot arm the capture timer"),
        }
    }

    /// One tick of the clients' captures; see `crate::capture::tick`. The
    /// events it sends go out with the end-of-cycle flush.
    fn tick_captures(&mut self) -> Option<std::time::Duration> {
        let icon = match &self.state.cursor_status {
            CursorImageStatus::Named(icon) => *icon,
            _ => CursorIcon::Default,
        };
        let cursors = &self.cursors;
        // One window at one density: the density is not a key here.
        let cursor = |_density: u32| {
            cursors
                .get(&icon)
                .or_else(|| cursors.get(&CursorIcon::Default))
        };
        crate::capture::tick(self.backend.renderer(), &mut self.state, &cursor)
    }

    fn render(&mut self) -> Result<()> {
        // Advance animations before assembling the scene, so this frame shows
        // where they are now rather than where they were last frame.
        self.state.tick_animations();
        let size = self.backend.window_size();
        let damage = Rectangle::from_size(size);
        let radius = self.state.blur_radius();
        let alpha = self.state.blur_alpha();
        // Always 1 here — see the output setup — but the elements were built
        // against it, so it is the only correct value to draw them with.
        let scale = self.state.scale().fractional();
        let view = self.state.output_area();

        // The blur's offscreen passes have to happen before the output
        // framebuffer is bound — they bind their own — so they run here, on
        // the renderer alone, and hand back one element to draw the result.
        //
        // `None` covers every case that is not "a panel is open and the shader
        // works", and every one of them falls through to the path below
        // unchanged. That is deliberate: the overwhelming majority of frames
        // take exactly the code they took before the blur existed.
        //
        // The scene is split once: `behind` is blurred into the texture, and
        // also drawn sharp beneath the blur, which is cropped to the panel.
        let icon = match &self.state.cursor_status {
            CursorImageStatus::Named(icon) => *icon,
            _ => CursorIcon::Default,
        };
        if !self.cursors.contains_key(&icon)
            && let Some(cursor) = Cursor::named(icon, self.state.scale().advertised)
        {
            self.cursors.insert(icon, cursor);
        }
        let cursor = self
            .cursors
            .get(&icon)
            .or_else(|| self.cursors.get(&CursorIcon::Default));
        let (front, behind) = {
            let renderer = self.backend.renderer();
            render::elements_split(renderer, &self.state, cursor, view, scale)
        };
        let blurred = match self.state.blur_rect() {
            Some(rect) if radius > 0.0 => {
                let renderer = self.backend.renderer();
                self.blur
                    .as_mut()
                    .and_then(|blur| blur.pass(renderer, &behind, size, scale, radius, alpha))
                    .and_then(|element| render::blur_element(element, rect, scale))
            }
            _ => None,
        };

        {
            let (renderer, mut framebuffer) = self
                .backend
                .bind()
                .map_err(|e| anyhow::anyhow!("binding framebuffer: {e}"))?;

            // Validate queued dmabuf imports now that the EGL context is
            // current.
            crate::dmabuf::import_pending(renderer, &mut self.state);

            // Geometry comes from huginn-core; stacking order and the cursor
            // come from render::elements_split, shared with the udev backend.
            // Front to back: the panels, the blurred patch under them, the
            // desktop.
            let mut elements = front;
            elements.extend(blurred);
            elements.extend(behind);

            let mut frame = renderer
                .render(&mut framebuffer, size, Transform::Flipped180)
                .map_err(|e| anyhow::anyhow!("starting frame: {e}"))?;
            frame
                .clear(CLEAR, &[damage])
                .map_err(|e| anyhow::anyhow!("clearing frame: {e}"))?;
            draw_render_elements::<GlesRenderer, _, _>(&mut frame, scale, &elements, &[damage])
                .map_err(|e| anyhow::anyhow!("drawing: {e}"))?;
            // The returned SyncPoint is discarded deliberately: the host
            // compositor we are nested inside does the synchronisation for us.
            // The udev backend will have to honour it.
            let _sync = frame
                .finish()
                .map_err(|e| anyhow::anyhow!("finishing frame: {e}"))?;

            // Layer surfaces need frame callbacks too. A panel that never gets
            // one renders its first frame and then freezes forever.
            let now = self.start.elapsed().as_millis() as u32;
            for (surface, _) in self.state.frame_surfaces() {
                send_frames(&surface, now);
            }
        }

        // Must happen after the framebuffer borrow ends, and after clients are
        // flushed: submit may block, and a client waiting on an unflushed event
        // would stall behind it.
        self.display.flush_clients().context("flushing clients")?;
        self.backend
            .submit(Some(&[damage]))
            .map_err(|e| anyhow::anyhow!("submitting frame: {e}"))?;
        Ok(())
    }

    fn on_winit(&mut self, event: WinitEvent) {
        match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                self.output
                    .change_current_state(Some(mode), None, None, None);
                // Panels re-anchor and re-reserve first; the window area is
                // whatever is left over.
                let scale = OutputScale::for_output(Size::new(size.w, size.h), Size::new(0, 0));
                self.state
                    .set_output_scale("huginn-winit", Some(self.output.clone()), scale);
            }
            WinitEvent::Redraw => self.state.queue_redraw(),
            WinitEvent::CloseRequested => self.signal.stop(),
            WinitEvent::Input(InputEvent::Keyboard { event }) => {
                // Kept in step with the udev backend even though nothing here
                // runs the idle timer: the nested compositor is a development
                // tool, and one that locked itself while somebody was reading
                // the host's screen would be a nuisance rather than a feature.
                // Tracking the activity anyway costs a store and means the two
                // input paths do not differ in what they record.
                self.state.note_activity();
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
                let overview = self.state.overview_open();
                let locked = self.state.is_locked();
                let switcher_open = self.state.app_switcher_open();
                let selecting_region = self.state.region_active();
                let help_open = self.state.help_open();
                let dock_menu_open = self.state.dock_menu_is_open();
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
                                        overview,
                                        locked,
                                        switcher_open,
                                        selecting_region,
                                        help_open,
                                        dock_menu_open,
                                    },
                                )
                            }
                        },
                    )
                    .flatten();
                // Any press that is not an arrow leaves resize mode — the
                // keymap forwarded it, so this is the only place that knows it
                // happened. Presses only: an arrow's *release* also resolves to
                // no action, and clearing on that would end the mode after a
                // single nudge.
                if resizing
                    && key_state == smithay::backend::input::KeyState::Pressed
                    && !matches!(action, Some(Action::Resize(_)))
                {
                    self.state.set_resize_mode(false);
                }
                if let Some(action) = action {
                    self.apply(action, time);
                }
            }
            WinitEvent::Input(event) => {
                self.state.note_activity();
                input::handle(&mut self.state, event);
                // A finished region selection leaves the screen and rectangle
                // here; the capture needs the renderer, which the shared input
                // path does not have.
                if let Some((output, rect)) = self.state.take_pending_capture() {
                    self.capture(output, Some(rect));
                }
            }
            _ => {}
        }
    }

    /// Apply a keybinding.
    fn apply(&mut self, action: Action, time: u32) {
        let state = &mut self.state;
        match action {
            Action::Quit => {
                // Stopped properly, so the file gets its end marker.
                self.stop_recording();
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
            Action::PullFrom(dir) => {
                state.pull_from_output(dir);
            }
            Action::Copy => {
                chord::send_ctrl(&self.keyboard, state, Keysym::c, time);
                return;
            }
            Action::Paste => {
                chord::send_ctrl(&self.keyboard, state, Keysym::v, time);
                return;
            }
            Action::OpenHelp => {
                state.open_help();
                return;
            }
            Action::CloseHelp => {
                state.close_help();
                return;
            }
            Action::CloseDockMenu => {
                state.close_dock_menu();
                return;
            }
            Action::CloseFocused => state.close_focused(),
            Action::ForceCloseFocused => state.force_close_focused(),
            Action::Workspace(i) => state.go_to_workspace(i),
            Action::SendToWorkspace(i) => state.send_focused_to_workspace(i),
            Action::FocusNextOutput => state.focus_next_output(),
            Action::SendToNextOutput => state.send_focused_to_next_output(),
            Action::EnterResize => {
                state.set_resize_mode(true);
                tracing::debug!("resize mode: arrows resize, Escape or Return leaves");
            }
            Action::Resize(dir) => state.resize_focused(dir),
            Action::LeaveResize => state.set_resize_mode(false),
            Action::OverviewMove(dir) => state.overview_move(dir),
            Action::OverviewConfirm => state.overview_confirm(),
            Action::OverviewCancel => state.close_workspace_carousel(),
            Action::ToggleCarousel => {
                state.toggle_workspace_carousel();
            }
            Action::OpenSettings => state.open_settings(),
            Action::Settings(key) => state.settings_key(key),
            Action::OpenFullSettings => state.open_full_settings(),
            Action::OpenStore => state.open_store(),
            Action::DismissNotification => state.dismiss_newest_notification(),
            Action::DismissNotifications => state.dismiss_notifications(),
            Action::OpenLauncher => state.open_launcher(),
            Action::OpenPinned => state.open_pinned(),
            Action::Pinned(key) => state.pinned_key(key),
            Action::Launcher(key) => state.launcher_key(key),
            Action::DismissSwitcher => state.dismiss_app_switcher(),
            Action::AltTab(dir) => state.alt_tab(dir),
            Action::AcceptSwitcher => state.accept_app_switcher(),
            Action::MinimizeFocused => state.minimize_focused(),
            Action::OpenMinimized => state.open_app_switcher(),
            Action::OverviewShift(dir) => state.overview_shift(dir),
            Action::Volume(key) => {
                state.volume_key(key);
                return;
            }
            Action::Screenshot(shot) => {
                self.screenshot(shot);
                return;
            }
            Action::Record => {
                self.toggle_recording();
                return;
            }
            Action::CancelRegion => {
                state.cancel_region();
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

    /// Take a screenshot, or — for a region — arm the interactive selection.
    fn screenshot(&mut self, shot: crate::screenshot::Shot) {
        use crate::screenshot::Shot;
        match shot {
            Shot::Screen => {
                let output = self.state.focused_output_index();
                self.capture(output, None);
            }
            Shot::Window => match self.state.focused_window_rect() {
                Some(rect) => {
                    let output = self.state.output_of_rect(rect);
                    self.capture(output, Some(rect));
                }
                None => tracing::info!("screenshot: no focused window to capture"),
            },
            Shot::Region => {
                self.state.begin_region_select();
            }
        }
    }

    /// Render `output` into a PNG, cropped to `crop` when given, and flash it.
    fn capture(&mut self, output: usize, crop: Option<Rect>) {
        let renderer = self.backend.renderer();
        match crate::screenshot::capture(renderer, &self.state, output, crop) {
            Ok(path) => {
                tracing::info!(path = %path.display(), "screenshot saved");
                self.state.begin_flash(output);
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "screenshot failed"),
        }
    }

    /// Start recording the focused screen, or stop the recording under way.
    fn toggle_recording(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
            return;
        }
        let output = self.state.focused_output_index();
        let recording =
            match crate::record::Recording::start(self.backend.renderer(), &self.state, output) {
                Ok(recording) => recording,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "recording did not start");
                    return;
                }
            };
        let timer = Timer::from_duration(crate::record::INTERVAL);
        let token = match self.handle.insert_source(timer, |_, _, data: &mut Nested| {
            if data.tick_recording() {
                TimeoutAction::ToDuration(crate::record::INTERVAL)
            } else {
                data.recording_timer = None;
                TimeoutAction::Drop
            }
        }) {
            Ok(token) => token,
            Err(e) => {
                tracing::error!(error = %e, "cannot arm the recording timer; not recording");
                if let Err(e) = recording.stop(self.backend.renderer()) {
                    tracing::warn!(error = %format!("{e:#}"), "recording failed");
                }
                return;
            }
        };
        tracing::info!(path = %recording.path().display(), "recording started");
        self.state.set_recording_dot(Some(recording.output()));
        self.recording = Some(recording);
        self.recording_timer = Some(token);
    }

    /// Stop the recording under way, if there is one, and its timer.
    fn stop_recording(&mut self) {
        if let Some(token) = self.recording_timer.take() {
            self.handle.remove(token);
        }
        self.finish_recording();
    }

    /// End the recording and say how it went. Leaves the timer alone, because
    /// this is also called from inside it, and there the timer drops itself.
    fn finish_recording(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        self.state.set_recording_dot(None);
        match recording.stop(self.backend.renderer()) {
            Ok(summary) => tracing::info!(
                path = %summary.path.display(),
                frames = summary.frames,
                dropped = summary.dropped,
                seconds = summary.length.as_secs_f64(),
                "recording saved"
            ),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "recording failed"),
        }
    }

    /// One tick of the recording. Returns whether it is still going.
    fn tick_recording(&mut self) -> bool {
        let Some(recording) = self.recording.as_mut() else {
            return false;
        };
        let icon = match &self.state.cursor_status {
            CursorImageStatus::Named(icon) => *icon,
            _ => CursorIcon::Default,
        };
        let cursor = self
            .cursors
            .get(&icon)
            .or_else(|| self.cursors.get(&CursorIcon::Default));
        match recording.tick(self.backend.renderer(), &self.state, cursor) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "recording stopped");
                self.finish_recording();
                false
            }
        }
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

crate::impl_xwm_handler!(Nested);
