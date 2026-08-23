//! The Muninn panel: a `wlr-layer-shell` surface anchored across the top.
//!
//! Software-rendered through `wl_shm` for now. That is the right first step —
//! it proves the protocol path end to end without dragging in a GPU stack, and
//! a 32-pixel bar is not what will make the desktop slow. The iced/wgpu
//! rendering path replaces this drawing code without touching the protocol
//! plumbing around it.

use std::num::NonZeroU32;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_output, wl_shm, wl_surface},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
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
const PIP_IDLE: u32 = 0xFF3B_3B54;
const PIP_ACTIVE: u32 = 0xFF7A_A2F7;

const WORKSPACES: u32 = 9;

pub(crate) fn run() -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("no wayland display: {e}. Is WAYLAND_DISPLAY set?"))?;
    let (globals, mut queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("registry init: {e}"))?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor missing: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| {
        anyhow::anyhow!("wlr-layer-shell missing: {e}. Muninn cannot run on a compositor without it")
    })?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm missing: {e}"))?;

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
        shm,
        pool,
        layer,
        width: 1920,
        height: HEIGHT,
        first_configure: true,
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
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    width: u32,
    height: u32,
    first_configure: bool,
    exit: bool,
}

impl Panel {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
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

        // Workspace pips. Placeholder geometry until raven_shell_v1 carries the
        // real workspace state from Huginn — at which point the highlighted pip
        // starts tracking the active workspace instead of being hardcoded.
        let pip = 8u32;
        let gap = 6u32;
        let top = (h.saturating_sub(pip)) / 2;
        for i in 0..WORKSPACES {
            let left = 12 + i * (pip + gap);
            if left + pip > w {
                break;
            }
            let color = if i == 0 { PIP_ACTIVE } else { PIP_IDLE };
            fill(canvas, w, left, top, pip, pip, color);
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        // Ask for the next frame before committing, so the compositor's
        // callback arrives with this frame rather than after a stall.
        surface.frame(qh, FrameCallbackData(surface.clone()));
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

impl LayerShellHandler for Panel {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // A zero in either axis means the compositor is leaving that dimension
        // to us, so we keep what we asked for rather than collapsing to nothing.
        self.width = NonZeroU32::new(configure.new_size.0).map_or(self.width, NonZeroU32::get);
        self.height = NonZeroU32::new(configure.new_size.1).map_or(HEIGHT, NonZeroU32::get);
        tracing::info!(width = self.width, height = self.height, "configured");

        if self.first_configure {
            self.first_configure = false;
            self.draw(qh);
        }
    }
}

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

    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
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
    registry_handlers![OutputState];
}

delegate_registry!(Panel);
smithay_client_toolkit::delegate_dispatch2!(Panel);
