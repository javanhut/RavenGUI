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
            EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction, generic::Generic,
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
    cursor: Option<Cursor>,
    start: Instant,
    signal: LoopSignal,
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
    state.set_output_scale(scale);
    // One output for the life of the process; nothing ever withdraws it.
    let _global = state.add_output(&output, &dh);

    // new_auto picks the first free name, so a second instance does not fail to
    // start just because the first one has huginn-1.
    let socket_source = ListeningSocketSource::new_auto().context("binding wayland socket")?;
    let socket = socket_source.socket_name().to_string_lossy().into_owned();

    handle
        .insert_source(socket_source, |stream, _, data: &mut Nested| {
            if let Err(e) = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
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
    let cursor = crate::pointer::Cursor::from_env(state.scale.advertised);

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
        cursor,
        start: Instant::now(),
        signal,
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
    /// Render if anything asked us to, then flush.
    fn dispatch_end_of_cycle(&mut self) -> Result<()> {
        self.state.refresh();
        if self.state.take_redraw() {
            // render flushes internally, before submit blocks.
            self.render()?;
        } else {
            // Flush even without a frame: a client waiting on a configure it
            // never receives will sit there forever.
            self.display.flush_clients().context("flushing clients")?;
        }
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        // Advance animations before assembling the scene, so this frame shows
        // where they are now rather than where they were last frame.
        self.state.tick_animations();
        let size = self.backend.window_size();
        let damage = Rectangle::from_size(size);
        let radius = self.state.blur_radius();
        // Always 1 here — see the output setup — but the elements were built
        // against it, so it is the only correct value to draw them with.
        let scale = self.state.scale.fractional();

        // The blur's offscreen passes have to happen before the output
        // framebuffer is bound — they bind their own — so they run here, on
        // the renderer alone, and hand back one element to draw the result.
        //
        // `None` covers every case that is not "a panel is open and the shader
        // works", and every one of them falls through to the path below
        // unchanged. That is deliberate: the overwhelming majority of frames
        // take exactly the code they took before the blur existed.
        let blurred = if radius > 0.0 {
            let renderer = self.backend.renderer();
            let (_, behind) = render::elements_split(renderer, &self.state, self.cursor.as_ref());
            self.blur
                .as_mut()
                .and_then(|blur| blur.pass(renderer, &behind, size, scale, radius))
        } else {
            None
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
            // come from render::elements, shared with the udev backend.
            let elements = match &blurred {
                // The desktop is already drawn, blurred, in one texture; only
                // what sits above the blur is still to draw.
                Some(_) => render::elements_split(renderer, &self.state, self.cursor.as_ref()).0,
                None => render::elements(renderer, &self.state, self.cursor.as_ref()),
            };

            let mut frame = renderer
                .render(&mut framebuffer, size, Transform::Flipped180)
                .map_err(|e| anyhow::anyhow!("starting frame: {e}"))?;
            frame
                .clear(CLEAR, &[damage])
                .map_err(|e| anyhow::anyhow!("clearing frame: {e}"))?;
            // The blurred desktop goes down first, then everything above it.
            if let Some(element) = &blurred {
                draw_render_elements::<GlesRenderer, _, _>(
                    &mut frame,
                    scale,
                    std::slice::from_ref(element),
                    &[damage],
                )
                .map_err(|e| anyhow::anyhow!("drawing the blurred desktop: {e}"))?;
            }
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
                self.state
                    .set_output_area(Rect::from_xywh(0, 0, size.w, size.h));
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
                // Any press that is not an arrow leaves resize mode — the
                // keymap forwarded it, so this is the only place that knows it
                // happened. Presses only: an arrow's *release* also resolves to
                // no action, and clearing on that would end the mode after a
                // single nudge.
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
            WinitEvent::Input(event) => {
                self.state.note_activity();
                input::handle(&mut self.state, event);
            }
            _ => {}
        }
    }

    /// Apply a keybinding.
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
                    // Ask politely. The client unmaps itself, which arrives back
                    // as toplevel_destroyed; killing it here would lose unsaved
                    // work.
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

crate::impl_xwm_handler!(Nested);
