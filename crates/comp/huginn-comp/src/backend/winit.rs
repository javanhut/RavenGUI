//! Nested backend: the compositor runs inside a window on an existing session.
//!
//! This is the development backend. It needs no seat, no DRM master and no TTY,
//! so it can be driven entirely over ssh — which the udev backend cannot, since
//! logind only grants DRM master to the active session on a seat.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use smithay::{
    backend::{
        egl::EGLDevice,
        input::{Event as _, InputEvent, KeyboardKeyEvent},
        renderer::{
            Color32F, Frame, Renderer,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            ImportDma,
            gles::GlesRenderer,
            utils::draw_render_elements,
        },
        winit::{self, WinitEvent},
    },
    input::keyboard::{FilterResult, keysyms},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        wayland_server::{Display, ListeningSocket, protocol::wl_surface::WlSurface},
        winit::platform::pump_events::PumpStatus,
    },
    utils::{Rectangle, SERIAL_COUNTER, Transform},
    wayland::{
        compositor::{SurfaceAttributes, TraversalAction, with_surface_tree_downward},
        shell::wlr_layer::Layer,
    },
};

use huginn_core::{geometry::Rect, workspace::Direction};

use crate::state::{ClientState, Huginn};

/// Background colour of an empty workspace.
const CLEAR: Color32F = Color32F::new(0.06, 0.06, 0.09, 1.0);

/// A keybinding resolved to an intent.
///
/// The keyboard filter runs while the keyboard handle is borrowed, so it cannot
/// touch compositor state that the follow-up work needs. Returning an intent
/// and applying it afterwards keeps the borrow checker out of the keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    FocusNext,
    FocusPrev,
    PromoteFocused,
    CloseFocused,
    Workspace(usize),
    SendToWorkspace(usize),
    Spawn,
    Quit,
}

pub(crate) fn run() -> Result<()> {
    let mut display: Display<Huginn> = Display::new().context("creating wayland display")?;
    let dh = display.handle();

    let (mut backend, mut winit_loop) =
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
    state.add_output(&output, &dh);

    // bind_auto picks the first free name, so a second instance does not fail
    // to start just because the first one has wayland-1.
    let listener = ListeningSocket::bind_auto("huginn", 1..32)
        .context("binding wayland socket")?;
    let socket_name = listener
        .socket_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let keyboard = state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("adding keyboard")?;

    tracing::info!(socket = %socket_name, "huginn is up");
    tracing::info!("clients: WAYLAND_DISPLAY={socket_name} <command>");
    tracing::info!(
        "keys: Super+J/K focus · Super+Return promote · Super+Q close · \
         Super+1-9 workspace · Super+Shift+1-9 move · Super+E spawn · Super+Esc quit"
    );

    let start = Instant::now();
    let mut clients = Vec::new();

    loop {
        let mut actions: Vec<Action> = Vec::new();
        let mut resized: Option<_> = None;

        let status = winit_loop.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => resized = Some(size),
            WinitEvent::CloseRequested => actions.push(Action::Quit),
            WinitEvent::Input(InputEvent::Keyboard { event }) => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let action = keyboard.input::<Action, _>(
                    &mut state,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_state, modifiers, handle| {
                        if !modifiers.logo {
                            return FilterResult::Forward;
                        }
                        let sym = handle.modified_sym().raw();
                        let action = match sym {
                            keysyms::KEY_j | keysyms::KEY_J => Action::FocusNext,
                            keysyms::KEY_k | keysyms::KEY_K => Action::FocusPrev,
                            keysyms::KEY_Return => Action::PromoteFocused,
                            keysyms::KEY_q | keysyms::KEY_Q => Action::CloseFocused,
                            keysyms::KEY_e | keysyms::KEY_E => Action::Spawn,
                            keysyms::KEY_Escape => Action::Quit,
                            // Shift+digit gives the punctuation keysyms, which
                            // is how "move to workspace" is told apart from
                            // "switch to workspace" without reading modifiers.
                            keysyms::KEY_1..=keysyms::KEY_9 => {
                                Action::Workspace((sym - keysyms::KEY_1) as usize)
                            }
                            keysyms::KEY_exclam => Action::SendToWorkspace(0),
                            keysyms::KEY_at => Action::SendToWorkspace(1),
                            keysyms::KEY_numbersign => Action::SendToWorkspace(2),
                            keysyms::KEY_dollar => Action::SendToWorkspace(3),
                            keysyms::KEY_percent => Action::SendToWorkspace(4),
                            _ => return FilterResult::Forward,
                        };
                        FilterResult::Intercept(action)
                    },
                );
                if let Some(action) = action {
                    actions.push(action);
                }
            }
            _ => {}
        });

        if let PumpStatus::Exit(_) = status {
            return Ok(());
        }

        if let Some(size) = resized {
            let mode = Mode { size, refresh: 60_000 };
            output.change_current_state(Some(mode), None, None, None);
            // Panels re-anchor and re-reserve first; the window area is
            // whatever is left over.
            state.set_output_area(Rect::from_xywh(0, 0, size.w, size.h));
        }

        for action in actions {
            if apply(&mut state, action, &socket_name) {
                return Ok(());
            }
        }

        let size = backend.window_size();
        let damage = Rectangle::from_size(size);
        {
            let (renderer, mut framebuffer) = backend
                .bind()
                .map_err(|e| anyhow::anyhow!("binding framebuffer: {e}"))?;

            // Validate queued dmabuf imports now that the EGL context is
            // current. Every notifier must be answered one way or the other:
            // a client that gets no reply waits forever for its buffer.
            for (dmabuf, notifier) in state.take_pending_dmabufs() {
                match renderer.import_dmabuf(&dmabuf, None) {
                    Ok(_texture) => {
                        let _ = notifier.successful::<Huginn>();
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "rejected a client dmabuf");
                        notifier.failed();
                    }
                }
            }

            // Geometry comes from huginn-core. Nothing in this loop decides
            // where anything goes.
            //
            // Order is front-to-back: draw_render_elements accumulates opaque
            // regions as it walks the slice and skips whatever is already
            // covered, so the topmost surface must come first. Reversing this
            // does not just reorder the scene, it paints panels underneath the
            // windows they are supposed to sit above.
            let overlay = state.layers_on(Layer::Overlay);
            let top = state.layers_on(Layer::Top);
            let windows = state.render_list();
            let bottom = state.layers_on(Layer::Bottom);
            let background = state.layers_on(Layer::Background);

            let scene: Vec<(&WlSurface, Rect)> = overlay
                .iter()
                .chain(top.iter())
                .map(|(l, r)| (l.wl_surface(), *r))
                .chain(windows.iter().map(|(w, r)| (w.wl_surface(), *r)))
                .chain(
                    bottom
                        .iter()
                        .chain(background.iter())
                        .map(|(l, r)| (l.wl_surface(), *r)),
                )
                .collect();

            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = scene
                .iter()
                .flat_map(|(surface, rect)| {
                    render_elements_from_surface_tree(
                        renderer,
                        surface,
                        (rect.x(), rect.y()),
                        1.0,
                        1.0,
                        Kind::Unspecified,
                    )
                })
                .collect();

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
            let now = start.elapsed().as_millis() as u32;
            for (surface, _) in &scene {
                send_frames(surface, now);
            }

            if let Some(stream) = listener.accept().context("accepting client")? {
                let client = display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .context("inserting client")?;
                clients.push(client);
            }

            display.dispatch_clients(&mut state).context("dispatching")?;
            display.flush_clients().context("flushing")?;
        }

        // Must happen after clients are flushed: submit may block, and a client
        // waiting on an unflushed event would stall behind it.
        backend
            .submit(Some(&[damage]))
            .map_err(|e| anyhow::anyhow!("submitting frame: {e}"))?;
    }
}

/// Apply a keybinding. Returns true if the compositor should exit.
fn apply(state: &mut Huginn, action: Action, socket: &str) -> bool {
    match action {
        Action::Quit => return true,
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
                // Ask politely. The client unmaps itself, which arrives back as
                // toplevel_destroyed; killing it here would lose unsaved work.
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
            let cmd = std::env::var("HUGINN_TERMINAL").unwrap_or_else(|_| "foot".to_owned());
            // Child processes get the socket through the environment we hand
            // them, not through std::env::set_var — which is unsafe in edition
            // 2024 and so unavailable under forbid(unsafe_code).
            match std::process::Command::new(&cmd)
                .env("WAYLAND_DISPLAY", socket)
                .spawn()
            {
                Ok(_) => tracing::info!(%cmd, "spawned"),
                Err(e) => tracing::warn!(%cmd, error = %e, "spawn failed"),
            }
        }
    }
    state.arrange();
    state.refresh_focus();
    false
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
