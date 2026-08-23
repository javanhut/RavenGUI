//! The Muninn panel: a `wlr-layer-shell` surface anchored across the top.
//!
//! Software-rendered through `wl_shm` for now. That is the right first step —
//! it proves the protocol path end to end without dragging in a GPU stack, and
//! a 32-pixel bar is not what will make the desktop slow. The iced/wgpu
//! rendering path replaces this drawing code without touching the protocol
//! plumbing around it.
//!
//! Workspace state arrives over `raven_shell_v1`, and clicking a pip sends
//! `activate` back the other way.

use std::num::NonZeroU32;

use raven_protocol::client::{
    raven_shell_manager_v1::{self, RavenShellManagerV1},
    raven_workspace_state_v1::{self, RavenWorkspaceStateV1},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::client::{
        Connection, Dispatch, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};

/// Panel height in logical pixels.
const HEIGHT: u32 = 32;

// Palette. Placeholders until raven-config carries a real theme.
const BG: u32 = 0xFF16_161F;
const BORDER: u32 = 0xFF2A_2A3A;
const PIP_EMPTY: u32 = 0xFF2B_2B3B;
const PIP_OCCUPIED: u32 = 0xFF5A_5A7A;
const PIP_ACTIVE: u32 = 0xFF7A_A2F7;

// Pip geometry. Drawing and hit-testing both derive from these, so a click can
// never land on a different pip than the one under the cursor.
const PIP: u32 = 8;
const PIP_GAP: u32 = 6;
const PIP_LEFT: u32 = 12;
const PIP_STRIDE: u32 = PIP + PIP_GAP;

/// Workspace state as last reported by the compositor.
///
/// The default — every field zero — is what the panel shows before the first
/// event lands. If the compositor does not implement `raven_shell_v1` at all,
/// it stays here and simply draws no pips rather than failing to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Workspaces {
    count: u32,
    active: u32,
    /// Bit N set if workspace N holds at least one window.
    occupied: u32,
}

impl Workspaces {
    fn is_occupied(self, index: u32) -> bool {
        index < 32 && self.occupied & (1 << index) != 0
    }
}

fn pip_x(index: u32) -> u32 {
    PIP_LEFT + index * PIP_STRIDE
}

/// Which pip sits under `x`, if any.
///
/// The hit target is the full stride, not just the painted 8 pixels — the gaps
/// between pips are clickable too, because an 8-pixel target is unkind.
fn pip_at(x: f64, count: u32) -> Option<u32> {
    let origin = PIP_LEFT as f64 - PIP_GAP as f64 / 2.0;
    if x < origin {
        return None;
    }
    let index = ((x - origin) / PIP_STRIDE as f64) as u32;
    (index < count).then_some(index)
}

pub(crate) fn run() -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("no wayland display: {e}. Is WAYLAND_DISPLAY set?"))?;
    let (globals, mut queue) =
        registry_queue_init(&conn).map_err(|e| anyhow::anyhow!("registry init: {e}"))?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor missing: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| {
        anyhow::anyhow!("wlr-layer-shell missing: {e}. Muninn cannot run on a compositor without it")
    })?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm missing: {e}"))?;

    // raven_shell_v1 is optional on purpose. Muninn should still come up under
    // another compositor — degraded, but running — rather than refusing to
    // start. That also makes it possible to test the panel's rendering against
    // a compositor that is not Huginn.
    let workspace_state = match globals.bind::<RavenShellManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(manager) => {
            let state = manager.get_workspace_state(&qh, ());
            tracing::info!("raven_shell_v1 available; observing workspace state");
            Some(state)
        }
        Err(e) => {
            tracing::warn!("raven_shell_v1 unavailable ({e}); workspace pips disabled");
            None
        }
    };

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("muninn"), None);

    // Anchored to three edges: top plus both perpendicular sides. That is the
    // configuration the protocol requires for an exclusive zone to mean
    // anything, and it makes the panel span the output at any width.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, HEIGHT);
    // Reserve our height so tiled windows tile beneath us rather than under us.
    layer.set_exclusive_zone(HEIGHT as i32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);

    // A commit with no buffer attached is what asks for the first configure.
    // Attaching one here instead is the classic way to get a protocol error.
    layer.commit();

    let pool = SlotPool::new(1920 * HEIGHT as usize * 4, &shm)
        .map_err(|e| anyhow::anyhow!("shm pool: {e}"))?;

    let mut panel = Panel {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        layer,
        pointer: None,
        workspace_state,
        workspaces: Workspaces::default(),
        width: 1920,
        height: HEIGHT,
        configured: false,
        exit: false,
    };

    tracing::info!("muninn panel up; waiting for configure");

    while !panel.exit {
        queue
            .blocking_dispatch(&mut panel)
            .map_err(|e| anyhow::anyhow!("dispatch: {e}"))?;
    }
    tracing::info!("panel closed");
    Ok(())
}

struct Panel {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    pointer: Option<wl_pointer::WlPointer>,
    workspace_state: Option<RavenWorkspaceStateV1>,
    workspaces: Workspaces,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
}

impl Panel {
    /// Repaint and commit.
    ///
    /// Deliberately does not request a frame callback. A frame callback would
    /// schedule another draw, and another, pinning the panel to the refresh
    /// rate to redraw pixels that did not change. This panel is event-driven:
    /// it paints on configure and when workspace state changes, and is
    /// otherwise idle.
    fn draw(&mut self) {
        if !self.configured {
            return;
        }
        let (w, h) = (self.width, self.height);
        let stride = w as i32 * 4;

        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
        else {
            tracing::warn!("could not allocate a buffer; skipping this frame");
            return;
        };

        for (index, chunk) in canvas.chunks_exact_mut(4).enumerate() {
            let y = index as u32 / w;
            // A single-pixel bottom rule reads as a panel edge rather than a
            // block of colour floating over the wallpaper.
            let color = if y + 1 == h { BORDER } else { BG };
            chunk.copy_from_slice(&color.to_le_bytes());
        }

        let top = h.saturating_sub(PIP) / 2;
        for i in 0..self.workspaces.count {
            let left = pip_x(i);
            if left + PIP > w {
                break;
            }
            let color = if i == self.workspaces.active {
                PIP_ACTIVE
            } else if self.workspaces.is_occupied(i) {
                PIP_OCCUPIED
            } else {
                PIP_EMPTY
            };
            fill(canvas, w, left, top, PIP, PIP, color);
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if let Err(e) = buffer.attach_to(surface) {
            tracing::warn!(error = %e, "buffer attach failed");
            return;
        }
        self.layer.commit();
    }
}

/// Fill an axis-aligned rectangle, clipped to the canvas.
fn fill(canvas: &mut [u8], stride_px: u32, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let bytes = color.to_le_bytes();
    for row in y..y.saturating_add(h) {
        for col in x..x.saturating_add(w) {
            let offset = ((row * stride_px + col) * 4) as usize;
            if let Some(px) = canvas.get_mut(offset..offset + 4) {
                px.copy_from_slice(&bytes);
            }
        }
    }
}

// ---------------------------------------------------------------- raven_shell

impl Dispatch<RavenShellManagerV1, ()> for Panel {
    fn event(
        _state: &mut Self,
        _proxy: &RavenShellManagerV1,
        _event: raven_shell_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events; it exists only to hand out objects.
    }
}

impl Dispatch<RavenWorkspaceStateV1, ()> for Panel {
    fn event(
        state: &mut Self,
        _proxy: &RavenWorkspaceStateV1,
        event: raven_workspace_state_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let raven_workspace_state_v1::Event::State {
            count,
            active,
            occupied,
        } = event
        else {
            return;
        };

        let next = Workspaces {
            count,
            active,
            occupied,
        };
        if next == state.workspaces {
            return;
        }
        tracing::debug!(count, active, occupied = format!("{occupied:b}"), "workspaces");
        state.workspaces = next;
        state.draw();
    }
}

// ------------------------------------------------------------------ layer shell

impl LayerShellHandler for Panel {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // A zero in either axis means the compositor is leaving that dimension
        // to us, so we keep what we asked for rather than collapsing to nothing.
        self.width = NonZeroU32::new(configure.new_size.0).map_or(self.width, NonZeroU32::get);
        self.height = NonZeroU32::new(configure.new_size.1).map_or(HEIGHT, NonZeroU32::get);
        tracing::info!(width = self.width, height = self.height, "configured");
        self.configured = true;
        self.draw();
    }
}

// -------------------------------------------------------------------- pointer

impl PointerHandler for Panel {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            let PointerEventKind::Press { .. } = event.kind else {
                continue;
            };
            let Some(index) = pip_at(event.position.0, self.workspaces.count) else {
                continue;
            };
            let Some(state) = &self.workspace_state else {
                continue;
            };
            tracing::debug!(index, "pip clicked; requesting workspace switch");
            // Fire and forget. The compositor answers with a state event, which
            // is what actually moves the highlight — the panel never assumes
            // its own request succeeded.
            state.activate(index);
        }
    }
}

impl SeatHandler for Panel {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(e) => tracing::warn!(error = %e, "no pointer; pips will not be clickable"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && let Some(pointer) = self.pointer.take() {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// ------------------------------------------------------------------ plumbing

impl CompositorHandler for Panel {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // Unused: this panel does not request frame callbacks. See Panel::draw.
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Panel {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Panel {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Panel {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Panel);
smithay_client_toolkit::delegate_dispatch2!(Panel);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_hit_testing_matches_where_pips_are_drawn() {
        for i in 0..9 {
            let centre = pip_x(i) as f64 + PIP as f64 / 2.0;
            assert_eq!(pip_at(centre, 9), Some(i), "centre of pip {i}");
        }
    }

    #[test]
    fn clicks_left_of_the_first_pip_hit_nothing() {
        assert_eq!(pip_at(0.0, 9), None);
        assert_eq!(pip_at(3.0, 9), None);
    }

    #[test]
    fn clicks_past_the_last_pip_hit_nothing() {
        assert_eq!(pip_at(pip_x(9) as f64 + 2.0, 9), None);
        assert_eq!(pip_at(1000.0, 9), None);
    }

    #[test]
    fn the_gap_between_two_pips_is_clickable() {
        // An 8px target is unkind, so the gaps belong to their neighbours.
        let between = pip_x(2) as f64 - 2.0;
        assert!(pip_at(between, 9).is_some());
    }

    #[test]
    fn occupancy_bitmask_reads_the_right_bits() {
        let ws = Workspaces { count: 9, active: 0, occupied: 0b1010_0101 };
        for (i, expected) in [true, false, true, false, false, true, false, true].iter().enumerate() {
            assert_eq!(ws.is_occupied(i as u32), *expected, "workspace {i}");
        }
        assert!(!ws.is_occupied(8), "bit 8 is clear");
        assert!(!ws.is_occupied(99), "out of range is never occupied");
    }
}
