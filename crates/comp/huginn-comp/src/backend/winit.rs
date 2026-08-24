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
            Color32F, Frame, ImportDma, Renderer,
            gles::GlesRenderer,
            utils::draw_render_elements,
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

use huginn_core::{geometry::Rect, workspace::Direction};

use crate::backend::input;
use crate::backend::chord;
use crate::backend::keymap::{Action, help_line, resolve};
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
    output: Output,
    keyboard: KeyboardHandle<Huginn>,
    cursor: Option<Cursor>,
    socket: String,
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
        Err(e) => tracing::warn!(error = %e, "could not identify the render node; clients stay on shm"),
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
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
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

    tracing::info!(socket = %socket, "huginn is up");
    tracing::info!("clients: WAYLAND_DISPLAY={socket} <command>");
    tracing::info!("{}", help_line());

    let mut data = Nested {
        state,
        display,
        backend,
        output,
        keyboard,
        cursor,
        socket,
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
        let size = self.backend.window_size();
        let damage = Rectangle::from_size(size);

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
            let elements = render::elements(renderer, &self.state, self.cursor.as_ref());

            let mut frame = renderer
                .render(&mut framebuffer, size, Transform::Flipped180)
                .map_err(|e| anyhow::anyhow!("starting frame: {e}"))?;
            frame
                .clear(CLEAR, &[damage])
                .map_err(|e| anyhow::anyhow!("clearing frame: {e}"))?;
            draw_render_elements(&mut frame, 1.0, &elements, &[damage])
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
            for (surface, _) in self.state.scene_surfaces() {
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
                self.output.change_current_state(Some(mode), None, None, None);
                // Panels re-anchor and re-reserve first; the window area is
                // whatever is left over.
                self.state
                    .set_output_area(Rect::from_xywh(0, 0, size.w, size.h));
            }
            WinitEvent::Redraw => self.state.queue_redraw(),
            WinitEvent::CloseRequested => self.signal.stop(),
            WinitEvent::Input(InputEvent::Keyboard { event }) => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let key_state = event.state();
                // Read before the filter runs: the closure is handed the compositor
                // state, but working out whose layer this is needs the focus as it
                // stands now, not as the keystroke may leave it.
                let owns_super = self.state.focus_owns_super();
                let action = self
                    .keyboard
                    .input::<Option<Action>, _>(
                        &mut self.state,
                        event.key_code(),
                        key_state,
                        serial,
                        time,
                        |_state, modifiers, handle| {
                            resolve(key_state, modifiers, handle.modified_sym().raw(), owns_super)
                        },
                    )
                    .flatten();
                if let Some(action) = action {
                    self.apply(action, time);
                }
            }
            WinitEvent::Input(event) => input::handle(&mut self.state, event),
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
                state.space.active_workspace_mut().cycle_focus(Direction::Forward);
            }
            Action::FocusPrev => {
                state.space.active_workspace_mut().cycle_focus(Direction::Backward);
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
                // Child processes get the socket through the environment we hand
                // them, not through std::env::set_var — which is unsafe in
                // edition 2024 and therefore unavailable under
                // forbid(unsafe_code).
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
